//! Shared-mode, event-driven WASAPI output: pulls the captured music from the bridge, runs the
//! [`Processor`] and writes the result to the headphones.
//!
//! A stream opened on an `IMMDevice` stays on that device, so following the system default is
//! the supervisor's job (it reopens the pipeline when the default changes).

use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use windows::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_NOPERSIST, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    IAudioClient, IAudioRenderClient, IAudioSessionControl, IAudioSessionControl2, IMMDevice, ISpatialAudioClient,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CLSCTX_ALL, CLSCTX_INPROC_SERVER, CoTaskMemFree};
use windows::Win32::System::Threading::WaitForMultipleObjects;
use windows::core::{Interface, w};

use super::capture::SidechainLevels;
use super::com::{Handle, Mmcss, Mta};
use super::sessions;
use crate::bridge::BridgeConsumer;
use crate::{MAX_BLOCK_FRAMES, Processor};

/// What the output stream turned out to be.
#[derive(Clone, Debug)]
pub struct RenderInfo {
    pub device_id: String,
    pub device_name: String,
    pub rate: u32,
    pub channels: u16,
    /// Endpoint buffer length in milliseconds.
    pub buffer_ms: f32,
    /// The engine's processing period (how much each wake-up asks for), in milliseconds.
    pub period_ms: f32,
    /// Windows Sonic / Dolby Atmos / DTS is enabled on the device.
    pub spatial: bool,
}

/// Handed to the render thread once the caller has built the bridge and processor for
/// `RenderInfo::rate`.
pub struct RenderStart {
    pub consumer: BridgeConsumer,
    pub processor: Box<dyn Processor>,
}

pub struct Render {
    pub info: RenderInfo,
    pub start: mpsc::SyncSender<RenderStart>,
    pub thread: JoinHandle<anyhow::Result<()>>,
}

/// Opens the output (`None` = the system default) and returns its format. The thread waits for
/// a [`RenderStart`] before it starts playing; `stop` (a manual-reset event) ends it.
pub fn open(
    device_id: Option<String>,
    stop: Arc<Handle>,
    sidechain: Option<Arc<SidechainLevels>>,
) -> anyhow::Result<Render> {
    let (info_tx, info_rx) = mpsc::sync_channel::<anyhow::Result<RenderInfo>>(1);
    let (start_tx, start_rx) = mpsc::sync_channel::<RenderStart>(1);
    let thread = thread::Builder::new()
        .name("sidestage-render".to_owned())
        .spawn(move || {
            let r = run(device_id, &stop, sidechain, &info_tx, start_rx);
            if let Err(e) = &r {
                let _ = info_tx.try_send(Err(anyhow!("{e:#}")));
            }
            r
        })
        .context("spawning the output thread")?;
    match info_rx.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok(info)) => Ok(Render { info, start: start_tx, thread }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err(anyhow!("the output device did not open in time")),
    }
}

fn find_device(device_id: Option<&str>) -> anyhow::Result<IMMDevice> {
    let en = sessions::enumerator()?;
    match device_id {
        Some(id) => {
            unsafe { en.GetDevice(&windows::core::HSTRING::from(id)) }.context("the chosen output is not available")
        }
        None => sessions::default_render(&en).ok_or_else(|| anyhow!("there is no audio output device")),
    }
}

fn run(
    device_id: Option<String>,
    stop: &Handle,
    sidechain: Option<Arc<SidechainLevels>>,
    info_tx: &mpsc::SyncSender<anyhow::Result<RenderInfo>>,
    start_rx: mpsc::Receiver<RenderStart>,
) -> anyhow::Result<()> {
    let _mta = Mta::enter()?;
    let device = find_device(device_id.as_deref())?;
    let id = sessions::device_id(&device)?;
    let name = sessions::device_name(&device);
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.context("opening the output device")?;

    let mix = unsafe { client.GetMixFormat() }.context("reading the output format")?;
    // SAFETY: GetMixFormat returns a valid WAVEFORMATEX (or WAVEFORMATEXTENSIBLE) we must free.
    let (rate, channels, mask) = unsafe {
        let f = mix.read_unaligned();
        let mask = if u32::from(f.wFormatTag) == WAVE_FORMAT_EXTENSIBLE && f.cbSize >= 22 {
            (mix as *const WAVEFORMATEXTENSIBLE).read_unaligned().dwChannelMask
        } else {
            0
        };
        CoTaskMemFree(Some(mix as *const _));
        (f.nSamplesPerSec, f.nChannels.max(1), mask)
    };
    let mask = if mask != 0 { mask } else { default_mask(channels) };
    // Always ask for float32 at the mix rate: the engine converts sample formats if needed.
    let block = channels * 4;
    let fmt = WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * u32::from(block),
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
        dwChannelMask: mask,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    };
    // NOPERSIST: our own volume-mixer slider must not be remembered across runs.
    let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_NOPERSIST
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, (&raw const fmt).cast(), None) }
        .context("initializing the output stream")?;
    let event = Handle::event(false)?;
    unsafe { client.SetEventHandle(event.raw()) }.context("IAudioClient::SetEventHandle")?;
    let render: IAudioRenderClient = unsafe { client.GetService() }.context("getting the render client")?;
    let buffer_frames = unsafe { client.GetBufferSize() }.context("reading the output buffer size")?;
    let mut period = 0i64;
    let _ = unsafe { client.GetDevicePeriod(Some(&mut period), None) };
    let period_ms = if period > 0 { period as f32 / 10_000.0 } else { 10.0 };
    configure_session(&client);
    let spatial = spatial_sound_enabled(&device);

    let info = RenderInfo {
        device_id: id,
        device_name: name,
        rate,
        channels,
        buffer_ms: buffer_frames as f32 * 1000.0 / rate as f32,
        period_ms,
        spatial,
    };
    let _ = info_tx.try_send(Ok(info));
    let Ok(RenderStart { mut consumer, mut processor }) = start_rx.recv_timeout(Duration::from_secs(10)) else {
        return Ok(()); // the caller gave up
    };

    let chunk = (buffer_frames as usize).clamp(64, MAX_BLOCK_FRAMES);
    let mut inb = vec![0f32; chunk * 2];
    let mut outb = vec![0f32; chunk * 2];
    // Start with a buffer of silence so the first period cannot underrun.
    unsafe {
        render.GetBuffer(buffer_frames).context("priming the output")?;
        render.ReleaseBuffer(buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32).context("priming the output")?;
    }
    let _mmcss = Mmcss::pro_audio();
    unsafe { client.Start() }.context("starting the output")?;
    let handles = [event.raw(), stop.raw()];
    let ch = channels as usize;
    let result = (|| -> anyhow::Result<()> {
        loop {
            let w = unsafe { WaitForMultipleObjects(&handles, false, 2000) };
            if w.0 == WAIT_OBJECT_0.0 + 1 {
                return Ok(());
            }
            if w == WAIT_FAILED {
                bail!("waiting for output events failed");
            }
            let padding = unsafe { client.GetCurrentPadding() }.context("output device")?;
            let free = buffer_frames.saturating_sub(padding) as usize;
            if free == 0 {
                continue;
            }
            let data = unsafe { render.GetBuffer(free as u32) }.context("output device")? as *mut f32;
            // SAFETY: GetBuffer returned room for `free` frames of `ch` float32 channels.
            let dst = unsafe { std::slice::from_raw_parts_mut(data, free * ch) };
            let mut done = 0;
            while done < free {
                let n = (free - done).min(chunk);
                let (i, o) = (&mut inb[..n * 2], &mut outb[..n * 2]);
                consumer.pull(i);
                let sc = sidechain.as_ref().map_or(0.0, |s| s.now.load());
                processor.process(i, o, sc);
                write_frames(o, &mut dst[done * ch..(done + n) * ch], ch);
                done += n;
            }
            unsafe { render.ReleaseBuffer(free as u32, 0) }.context("output device")?;
        }
    })();
    let _ = unsafe { client.Stop() };
    result
}

/// Copies stereo into the device layout: front left/right, other channels silent, mono gets
/// the average.
#[inline]
fn write_frames(stereo: &[f32], dst: &mut [f32], ch: usize) {
    match ch {
        1 => {
            for (d, s) in dst.iter_mut().zip(stereo.as_chunks::<2>().0) {
                *d = 0.5 * (s[0] + s[1]);
            }
        }
        2 => dst.copy_from_slice(stereo),
        _ => {
            for (d, s) in dst.chunks_exact_mut(ch).zip(stereo.as_chunks::<2>().0) {
                d[0] = s[0];
                d[1] = s[1];
                d[2..].fill(0.0);
            }
        }
    }
}

fn default_mask(channels: u16) -> u32 {
    match channels {
        1 => 0x4,   // front center
        2 => 0x3,   // front left | front right
        4 => 0x33,  // quad
        6 => 0x3f,  // 5.1
        8 => 0x63f, // 7.1
        n => (1u32 << n.min(31)) - 1,
    }
}

/// Names our stream "Sidestage" in the Volume mixer and opts it out of Windows' communications
/// ducking (the music feeding it is ducked already, if at all).
fn configure_session(client: &IAudioClient) {
    let Ok(ctl) = (unsafe { client.GetService::<IAudioSessionControl>() }) else {
        return;
    };
    let _ = unsafe { ctl.SetDisplayName(w!("Sidestage"), std::ptr::null()) };
    if let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() {
        let _ = unsafe { ctl2.SetDuckingPreference(true) };
    }
}

/// True if a spatial sound format (Windows Sonic, Dolby Atmos, DTS) is active on the device; it
/// would spatialize Sidestage's binaural output a second time.
fn spatial_sound_enabled(device: &IMMDevice) -> bool {
    let Ok(sac) = (unsafe { device.Activate::<ISpatialAudioClient>(CLSCTX_INPROC_SERVER, None) }) else {
        return false;
    };
    unsafe { sac.GetMaxDynamicObjectCount() }.is_ok_and(|n| n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_mapping() {
        let st = [0.5, -0.5, 0.25, 0.75];
        let mut mono = [0.0; 2];
        write_frames(&st, &mut mono, 1);
        assert_eq!(mono, [0.0, 0.5]);
        let mut six = [9.0; 12];
        write_frames(&st, &mut six, 6);
        assert_eq!(six, [0.5, -0.5, 0.0, 0.0, 0.0, 0.0, 0.25, 0.75, 0.0, 0.0, 0.0, 0.0]);
    }
}
