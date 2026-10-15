//! WASAPI process-loopback capture.
//!
//! A process-loopback client captures one process tree (`INCLUDE`) or everything except one
//! tree (`EXCLUDE`), after the target's session volume. Facts measured on Windows 11 that shape
//! this code:
//! - `GetMixFormat`/`GetDevicePeriod`/`GetCurrentPadding` return `E_NOTIMPL`: we pick the format
//!   (float32 stereo at the output device's rate, converted by the engine).
//! - Packets are always 10 ms and the queue holds only ~3 of them. A late reader loses audio
//!   *silently*, so the capture thread only copies into the bridge and detects lost packets from
//!   gaps in the QPC timestamps (device position is always 0).
//! - The stream never ends: when the target is silent or gone it keeps delivering zeros.
//! - Activation succeeds for any PID, even a dead one; callers validate the target.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use windows::Win32::Foundation::{E_FAIL, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient,
    IAudioClient, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Threading::WaitForMultipleObjects;
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

use super::com::{Handle, Mmcss, Mta};
use crate::bridge::BridgeProducer;
use crate::{AtomicLevel, peak};

/// Which audio a process-loopback client hears.
#[derive(Clone, Copy, Debug)]
pub enum Target {
    /// This process and all its descendants.
    Include(u32),
    /// Everything except this process and its descendants.
    Exclude(u32),
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationDone(mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationDone_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        // Runs on an OS worker thread: only signal, the result is read on our own thread.
        let _ = self.0.try_send(());
        Ok(())
    }
}

/// Activates a process-loopback `IAudioClient`. Call on an MTA thread.
pub fn activate(target: Target) -> anyhow::Result<IAudioClient> {
    let (pid, mode) = match target {
        Target::Include(p) => (p, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
        Target::Exclude(p) => (p, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
    };
    // On the heap: if the activation times out, Windows may still read them when it completes,
    // so they are leaked then rather than freed.
    let mut params = Box::new(AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: mode,
            },
        },
    });
    // The blob points at `params`. PROPVARIANT's Drop calls PropVariantClear, which would try
    // to free that pointer, so it must never be dropped.
    let prop = Box::new(ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&raw mut *params).cast(),
                    },
                },
            }),
        },
    }));
    let (tx, rx) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationDone(tx).into();
    let op = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&**prop as *const PROPVARIANT),
            &handler,
        )
    }
    .context("starting process-loopback activation")?;
    if rx.recv_timeout(Duration::from_secs(5)).is_err() {
        std::mem::forget(params);
        std::mem::forget(prop);
        bail!("process-loopback activation timed out");
    }
    let mut hr = HRESULT(0);
    let mut unk: Option<IUnknown> = None;
    unsafe { op.GetActivateResult(&mut hr, &mut unk) }.context("reading the activation result")?;
    hr.ok().context("process-loopback activation failed (this Windows version may not support it)")?;
    let unk = unk.ok_or_else(|| windows::core::Error::from(E_FAIL)).context("activation returned no client")?;
    unk.cast::<IAudioClient>().context("activation returned no IAudioClient")
}

pub fn float_stereo(rate: u32) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: 2,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// Level control between the capture and the bridge, set from control threads.
///
/// The music app's session runs at -120 dB while hidden, so the capture is multiplied by a large
/// makeup gain. Whenever the real session volume may not match the makeup (while settling,
/// after someone else moved the slider) the gate is closed and the output fades to silence.
/// As a last line of defence, any block that would come out above +12 dBFS is replaced by
/// silence and reported as a fault, so a stray full-level packet can never be multiplied by
/// a million.
#[derive(Debug, Default)]
pub struct Gain {
    makeup: AtomicU32,
    open: AtomicBool,
    fault: AtomicBool,
    /// Post-gain peak hold for the status display.
    pub peak: AtomicLevel,
    /// The same, read by the supervisor's "hears nothing" check.
    pub watch: AtomicLevel,
    /// Lost-packet gaps filled with silence.
    pub gaps: AtomicU64,
    /// Packets received (liveness).
    pub packets: AtomicU64,
}

/// Largest sample the gain stage lets through (+12 dBFS).
const CLAMP: f32 = 4.0;

impl Gain {
    pub fn new() -> Self {
        Self { makeup: AtomicU32::new(1.0f32.to_bits()), ..Default::default() }
    }

    pub fn set_makeup(&self, g: f32) {
        self.makeup.store(g.to_bits(), Ordering::Relaxed);
    }

    pub fn makeup(&self) -> f32 {
        f32::from_bits(self.makeup.load(Ordering::Relaxed))
    }

    pub fn open(&self) {
        self.open.store(true, Ordering::Release);
    }

    pub fn close(&self) {
        self.open.store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    /// True (once) if the clamp fired since the last call.
    pub fn take_fault(&self) -> bool {
        self.fault.swap(false, Ordering::AcqRel)
    }
}

/// Per-thread state of the gain stage: fade position 0..=1 and fade speeds.
struct Fader {
    pos: f32,
    up: f32,
    down: f32,
}

impl Fader {
    fn new(rate: u32) -> Self {
        // 20 ms fade-in, 5 ms fade-out.
        Self { pos: 0.0, up: 1.0 / (rate as f32 * 0.020), down: 1.0 / (rate as f32 * 0.005) }
    }

    /// Applies gate, fade and makeup to interleaved stereo in place. Real-time safe.
    #[inline]
    fn apply(&mut self, gain: &Gain, buf: &mut [f32]) {
        let open = gain.open.load(Ordering::Acquire);
        let makeup = gain.makeup();
        if open && self.pos >= 1.0 {
            for s in buf.iter_mut() {
                *s *= makeup;
            }
        } else if !open && self.pos <= 0.0 {
            buf.fill(0.0);
        } else {
            for f in buf.as_chunks_mut::<2>().0 {
                self.pos = if open { (self.pos + self.up).min(1.0) } else { (self.pos - self.down).max(0.0) };
                let g = self.pos * makeup;
                f[0] *= g;
                f[1] *= g;
            }
        }
        let p = peak(buf);
        if p.is_nan() || p > CLAMP {
            buf.fill(0.0);
            self.pos = 0.0;
            gain.open.store(false, Ordering::Release);
            gain.fault.store(true, Ordering::Release);
        } else {
            gain.peak.raise(p);
            gain.watch.raise(p);
        }
    }
}

/// Peak levels of the sidechain ("everything except the music").
#[derive(Debug, Default)]
pub struct SidechainLevels {
    /// Peak of the latest packet; the render thread reads it for the ducker.
    pub now: AtomicLevel,
    /// Peak hold for the status display.
    pub hold: AtomicLevel,
}

/// Where a capture thread delivers its audio.
pub enum Sink {
    /// The music: gain stage, then the bridge.
    Music { producer: BridgeProducer, gain: Arc<Gain> },
    /// The sidechain: peak levels only.
    Sidechain(Arc<SidechainLevels>),
}

/// Starts a capture thread and waits until its client is running. `stop` is a manual-reset
/// event that ends the thread.
pub fn spawn(
    target: Target,
    rate: u32,
    sink: Sink,
    stop: Arc<Handle>,
) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
    let (ready_tx, ready_rx) = mpsc::sync_channel::<anyhow::Result<()>>(1);
    let name = match sink {
        Sink::Music { .. } => "sidestage-capture",
        Sink::Sidechain(_) => "sidestage-sidechain",
    };
    let thread = thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let r = run(target, rate, sink, &stop, &ready_tx);
            if let Err(e) = &r {
                // Also reaches the spawner if we failed before reporting ready.
                let _ = ready_tx.try_send(Err(anyhow!("{e:#}")));
            }
            r
        })
        .context("spawning the capture thread")?;
    match ready_rx.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err(anyhow!("the capture thread did not start in time")),
    }
}

fn run(
    target: Target,
    rate: u32,
    mut sink: Sink,
    stop: &Handle,
    ready: &mpsc::SyncSender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let _mta = Mta::enter()?;
    let client = activate(target)?;
    let fmt = float_stereo(rate);
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, &fmt, None) }
        .context("initializing the process-loopback capture")?;
    let event = Handle::event(false)?;
    unsafe { client.SetEventHandle(event.raw()) }.context("IAudioClient::SetEventHandle")?;
    let cap: IAudioCaptureClient = unsafe { client.GetService() }.context("getting the capture client")?;
    let packet = unsafe { client.GetBufferSize() }.unwrap_or(rate / 100).max(rate / 100) as usize;
    // Room for the largest packet we expect (packets are 10 ms; allow 4x).
    let mut scratch = vec![0f32; packet * 2 * 4];
    let mut fader = Fader::new(rate);
    let _mmcss = Mmcss::pro_audio();
    unsafe { client.Start() }.context("starting the capture")?;
    let _ = ready.try_send(Ok(()));
    let result = capture_loop(&cap, rate, &mut sink, &mut scratch, &mut fader, &event, stop);
    let _ = unsafe { client.Stop() };
    if let Sink::Sidechain(levels) = &sink {
        // A stale level would keep the music ducked.
        levels.now.store(0.0);
    }
    result
}

/// The real-time loop: copy packets into the sink, nothing else. Only WASAPI calls, no
/// allocation, locks or logging.
fn capture_loop(
    cap: &IAudioCaptureClient,
    rate: u32,
    sink: &mut Sink,
    scratch: &mut [f32],
    fader: &mut Fader,
    event: &Handle,
    stop: &Handle,
) -> anyhow::Result<()> {
    let handles = [event.raw(), stop.raw()];
    let max_frames = scratch.len() / 2;
    let mut last_qpc: Option<u64> = None;
    loop {
        let w = unsafe { WaitForMultipleObjects(&handles, false, 1000) };
        if w.0 == WAIT_OBJECT_0.0 + 1 {
            return Ok(());
        }
        if w == WAIT_FAILED {
            bail!("waiting for capture events failed");
        }
        loop {
            let n = unsafe { cap.GetNextPacketSize() }.context("reading the capture queue")?;
            if n == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let (mut frames, mut flags, mut qpc) = (0u32, 0u32, 0u64);
            unsafe { cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc)) }
                .context("reading a capture packet")?;
            let frames_us = frames as usize;
            // A gap in the QPC stamps means the queue overflowed and packets were dropped.
            if let Some(prev) = last_qpc {
                let expected = frames as u64 * 10_000_000 / u64::from(rate);
                let delta = qpc.saturating_sub(prev);
                if delta > expected + expected / 2 && delta < 10_000_000 {
                    let lost = ((delta - expected) * u64::from(rate) / 10_000_000).min(u64::from(rate) / 10) as usize;
                    if let Sink::Music { producer, gain } = sink {
                        producer.push_silence(lost);
                        gain.gaps.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            last_qpc = Some(qpc);
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
            let mut done = 0usize;
            while done < frames_us {
                let take = (frames_us - done).min(max_frames);
                let buf = &mut scratch[..take * 2];
                if silent {
                    buf.fill(0.0);
                } else {
                    // SAFETY: GetBuffer returned `frames` frames of float32 stereo at `data`.
                    let src = unsafe { std::slice::from_raw_parts((data as *const f32).add(done * 2), take * 2) };
                    buf.copy_from_slice(src);
                }
                match sink {
                    Sink::Music { producer, gain } => {
                        fader.apply(gain, buf);
                        producer.push(buf);
                    }
                    Sink::Sidechain(levels) => {
                        let p = if silent { 0.0 } else { peak(buf) };
                        levels.now.store(p);
                        levels.hold.raise(p);
                    }
                }
                done += take;
            }
            unsafe { cap.ReleaseBuffer(frames) }.context("releasing a capture packet")?;
            if let Sink::Music { gain, .. } = sink {
                gain.packets.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fader_gates_fades_and_clamps() {
        let gain = Gain::new();
        let mut f = Fader::new(48_000);
        let mut buf = vec![0.5f32; 960 * 2];
        f.apply(&gain, &mut buf);
        assert!(buf.iter().all(|&s| s == 0.0), "closed gate is silent");

        gain.set_makeup(2.0);
        gain.open();
        let mut buf = vec![0.25f32; 1000 * 2];
        f.apply(&gain, &mut buf);
        assert!(buf[0] < 0.01, "fade starts near zero");
        assert!((buf[buf.len() - 1] - 0.5).abs() < 1e-6, "fully faded in after 20 ms");

        let mut buf = vec![0.25f32; 64];
        f.apply(&gain, &mut buf);
        assert!(buf.iter().all(|&s| (s - 0.5).abs() < 1e-6));
        assert!(!gain.take_fault());

        // A full-level packet multiplied by a big makeup must never get through.
        gain.set_makeup(1e6);
        let mut buf = vec![0.5f32; 64];
        f.apply(&gain, &mut buf);
        assert!(buf.iter().all(|&s| s == 0.0));
        assert!(gain.take_fault());
        assert!(!gain.is_open(), "the clamp closes the gate");
    }
}
