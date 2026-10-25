//! Shared-mode, event-driven WASAPI output and the session's mixer: every block it pulls each
//! source's captured audio from its bridge, runs the source's [`Processor`], sums the results,
//! runs the master processor on the sum and writes it to the headphones.
//!
//! Sources join and leave while it plays, through a lock-free command queue: the supervisor
//! builds a source's bridge and processor on its own thread and hands over a box. A removed
//! source fades out over a few milliseconds and its box goes back on a second queue, so nothing
//! is allocated or freed on the real-time thread.
//!
//! A stream opened on an `IMMDevice` stays on that device, so following the system default is
//! the supervisor's job (it reopens the output when the default changes).

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
use super::com::{Handle, Mmcss, Mta, panic_message};
use super::sessions;
use crate::bridge::BridgeConsumer;
use crate::{MAX_BLOCK_FRAMES, Processor, ProcessorFactory, SourceId};

/// Most sources one session mixes.
pub const MAX_SOURCES: usize = 64;
/// Capacity of the mixer's slot list, allocated once: every source plus a replaced slot still
/// fading out for each.
const MAX_SLOTS: usize = 2 * MAX_SOURCES;
/// Length of a removed source's fade-out.
const FADE_MS: f32 = 10.0;

/// What the output stream turned out to be.
#[derive(Clone, Debug, Default)]
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

/// One source in the mix: its bridge and processor, built off the real-time thread.
pub struct Slot {
    pub id: SourceId,
    consumer: BridgeConsumer,
    processor: Box<dyn Processor>,
    /// Frames left of the fade-out once the source was removed.
    fade: Option<u32>,
    /// Set when the processor panicked (or the mixer had no room for the slot): it is not
    /// called again and the slot plays silence until the supervisor removes it.
    fault: Arc<AtomicBool>,
    /// The processor's panic payload, kept so it is freed with the slot off the real-time
    /// thread.
    panic: Option<Box<dyn Any + Send>>,
}

impl Slot {
    pub fn new(
        id: SourceId,
        consumer: BridgeConsumer,
        processor: Box<dyn Processor>,
        fault: Arc<AtomicBool>,
    ) -> Box<Slot> {
        Box::new(Slot { id, consumer, processor, fade: None, fault, panic: None })
    }
}

enum Command {
    Add(Box<Slot>),
    Remove(SourceId),
}

/// Calls a processor factory, turning a panic in the app's code into an error so one broken
/// source cannot take the session down.
pub fn build_processor(factory: &ProcessorFactory, rate: u32) -> anyhow::Result<Box<dyn Processor>> {
    catch_unwind(AssertUnwindSafe(|| factory(rate)))
        .map_err(|p| anyhow!("the audio processor could not be created: {}", panic_message(&p)))
}

/// Counters of the output stream.
#[derive(Debug, Default)]
pub struct RenderStats {
    /// Wake-ups that found the device buffer already empty (the engine played silence).
    pub underruns: AtomicU64,
}

/// The running output: its thread, the queues into its mixer and its format.
pub struct Output {
    pub info: RenderInfo,
    pub stats: Arc<RenderStats>,
    pub started: Instant,
    stop: Arc<Handle>,
    thread: Option<JoinHandle<anyhow::Result<()>>>,
    commands: rtrb::Producer<Command>,
    garbage: rtrb::Consumer<Box<Slot>>,
}

impl Output {
    /// Opens the output (`None` = the system default) and starts it with the master processor
    /// built for its rate. `sidechain` is handed to every processor.
    pub fn open(
        device_id: Option<String>,
        sidechain: Option<Arc<SidechainLevels>>,
        master: &ProcessorFactory,
    ) -> anyhow::Result<Output> {
        let stop = Arc::new(Handle::event(true)?);
        let (info_tx, info_rx) = mpsc::sync_channel::<anyhow::Result<RenderInfo>>(1);
        let (start_tx, start_rx) = mpsc::sync_channel::<Box<dyn Processor>>(1);
        let (commands, command_rx) = rtrb::RingBuffer::new(2 * MAX_SLOTS);
        let (garbage_tx, garbage) = rtrb::RingBuffer::new(2 * MAX_SLOTS);
        let stats = Arc::new(RenderStats::default());
        let queues = Queues { commands: command_rx, garbage: garbage_tx, stats: stats.clone() };
        let thread_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("sidestage-render".to_owned())
            .spawn(move || {
                let r = run(device_id, &thread_stop, sidechain, &info_tx, start_rx, queues);
                if let Err(e) = &r {
                    let _ = info_tx.try_send(Err(anyhow!("{e:#}")));
                }
                r
            })
            .context("spawning the output thread")?;
        let mut out = Output {
            info: RenderInfo::default(),
            stats,
            started: Instant::now(),
            stop,
            thread: Some(thread),
            commands,
            garbage,
        };
        out.info = match info_rx.recv_timeout(Duration::from_secs(8)) {
            Ok(Ok(info)) => info,
            Ok(Err(e)) => {
                out.shutdown();
                return Err(e);
            }
            Err(_) => {
                out.shutdown();
                bail!("the output device did not open in time");
            }
        };
        let master = match build_processor(master, out.info.rate) {
            Ok(m) => m,
            Err(e) => {
                out.shutdown();
                return Err(e);
            }
        };
        if start_tx.send(master).is_err() {
            let why = out.shutdown().unwrap_or_default();
            bail!("the output stopped before it started: {why}");
        }
        Ok(out)
    }

    /// Hands a source to the mixer; it plays from the next block on.
    pub fn add(&mut self, slot: Box<Slot>) -> anyhow::Result<()> {
        self.commands.push(Command::Add(slot)).map_err(|_| anyhow!("the output is not taking new sources"))
    }

    /// Fades a source out of the mix; its slot comes back through [`Output::collect_garbage`].
    pub fn remove(&mut self, id: SourceId) {
        if self.commands.push(Command::Remove(id)).is_err() {
            log::warn!("the output's command queue is full; source {id} keeps playing until the output restarts");
        }
    }

    /// Frees the slots the mixer is done with (here, not on the real-time thread).
    pub fn collect_garbage(&mut self) {
        while self.garbage.pop().is_ok() {}
    }

    pub fn finished(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| t.is_finished())
    }

    /// Stops the thread; returns its error, if it reported one.
    pub fn shutdown(&mut self) -> Option<String> {
        self.stop.set();
        let t = self.thread.take()?;
        let r = match t.join() {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("output: {e:#}")),
            Err(p) => Some(format!("output thread panicked: {}", panic_message(&p))),
        };
        self.collect_garbage();
        r
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The real-time ends of the output's queues.
struct Queues {
    commands: rtrb::Consumer<Command>,
    garbage: rtrb::Producer<Box<Slot>>,
    stats: Arc<RenderStats>,
}

/// The mix, owned by the render thread. Every method is real-time safe once constructed.
struct Mixer {
    // Slots arrive boxed and leave boxed: unboxing one here would free memory on the real-time
    // thread.
    #[allow(clippy::vec_box)]
    slots: Vec<Box<Slot>>,
    master: Box<dyn Processor>,
    commands: rtrb::Consumer<Command>,
    garbage: rtrb::Producer<Box<Slot>>,
    fade_frames: u32,
    inb: Vec<f32>,
    outb: Vec<f32>,
    sum: Vec<f32>,
    mixed: Vec<f32>,
}

impl Mixer {
    fn new(
        master: Box<dyn Processor>,
        commands: rtrb::Consumer<Command>,
        garbage: rtrb::Producer<Box<Slot>>,
        rate: u32,
        chunk: usize,
    ) -> Self {
        Mixer {
            slots: Vec::with_capacity(MAX_SLOTS),
            master,
            commands,
            garbage,
            fade_frames: ((rate as f32 * FADE_MS / 1000.0) as u32).max(1),
            inb: vec![0.0; chunk * 2],
            outb: vec![0.0; chunk * 2],
            sum: vec![0.0; chunk * 2],
            mixed: vec![0.0; chunk * 2],
        }
    }

    fn chunk(&self) -> usize {
        self.mixed.len() / 2
    }

    /// Applies queued additions and removals.
    fn apply_commands(&mut self) {
        while let Ok(cmd) = self.commands.pop() {
            match cmd {
                Command::Add(slot) => {
                    // Refuse rather than let the vector reallocate here. Flagged as a fault, so
                    // the supervisor retries the source instead of believing it plays.
                    if self.slots.len() < self.slots.capacity() {
                        self.slots.push(slot);
                    } else {
                        slot.fault.store(true, Ordering::Release);
                        give_back(&mut self.garbage, slot);
                    }
                }
                Command::Remove(id) => {
                    for s in self.slots.iter_mut().filter(|s| s.id == id && s.fade.is_none()) {
                        s.fade = Some(self.fade_frames);
                    }
                }
            }
        }
    }

    /// Mixes `frames` (at most [`Mixer::chunk`]) frames into `mixed[..frames * 2]`.
    fn render(&mut self, frames: usize, sidechain: f32) {
        let n = frames * 2;
        let (inb, outb, sum) = (&mut self.inb[..n], &mut self.outb[..n], &mut self.sum[..n]);
        sum.fill(0.0);
        for slot in &mut self.slots {
            slot.consumer.pull(inb);
            if slot.fault.load(Ordering::Relaxed) {
                // Silent already, so a removed faulty slot leaves at once (it has no fade to
                // run down).
                if slot.fade.is_some() {
                    slot.fade = Some(0);
                }
                continue;
            }
            // A processor that breaks the contract and panics only silences its own source.
            if let Err(p) = catch_unwind(AssertUnwindSafe(|| slot.processor.process(inb, outb, sidechain))) {
                slot.panic = Some(p);
                slot.fault.store(true, Ordering::Release);
                if slot.fade.is_some() {
                    slot.fade = Some(0);
                }
                continue;
            }
            match &mut slot.fade {
                None => {
                    for (s, x) in sum.iter_mut().zip(outb.iter()) {
                        *s += *x;
                    }
                }
                Some(left) => {
                    let total = self.fade_frames as f32;
                    let frames_in = sum.as_chunks_mut::<2>().0.iter_mut().zip(outb.as_chunks::<2>().0);
                    for (k, (s, x)) in frames_in.enumerate() {
                        let g = (*left as f32 - k as f32).max(0.0) / total;
                        s[0] += x[0] * g;
                        s[1] += x[1] * g;
                    }
                    *left = left.saturating_sub(frames as u32);
                }
            }
        }
        let mut i = 0;
        while i < self.slots.len() {
            if self.slots[i].fade == Some(0) {
                let slot = self.slots.swap_remove(i);
                give_back(&mut self.garbage, slot);
            } else {
                i += 1;
            }
        }
        self.master.process(sum, &mut self.mixed[..n], sidechain);
    }
}

/// Returns a slot to the supervisor for freeing. The queue holds twice the most slots that can
/// exist, so it is never full in practice; if it were, the slot is leaked rather than freed on
/// the real-time thread.
fn give_back(garbage: &mut rtrb::Producer<Box<Slot>>, slot: Box<Slot>) {
    if let Err(rtrb::PushError::Full(slot)) = garbage.push(slot) {
        std::mem::forget(slot);
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
    start_rx: mpsc::Receiver<Box<dyn Processor>>,
    queues: Queues,
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
    let Ok(master) = start_rx.recv_timeout(Duration::from_secs(10)) else {
        return Ok(()); // the caller gave up
    };

    let chunk = (buffer_frames as usize).clamp(64, MAX_BLOCK_FRAMES);
    let Queues { commands, garbage, stats } = queues;
    let mut mixer = Mixer::new(master, commands, garbage, rate, chunk);
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
            mixer.apply_commands();
            let padding = unsafe { client.GetCurrentPadding() }.context("output device")?;
            if padding == 0 {
                // Everything we gave the engine was played before we refilled: it filled the
                // gap with silence.
                stats.underruns.fetch_add(1, Ordering::Relaxed);
            }
            let free = buffer_frames.saturating_sub(padding) as usize;
            if free == 0 {
                continue;
            }
            let data = unsafe { render.GetBuffer(free as u32) }.context("output device")? as *mut f32;
            // SAFETY: GetBuffer returned room for `free` frames of `ch` float32 channels.
            let dst = unsafe { std::slice::from_raw_parts_mut(data, free * ch) };
            let mut done = 0;
            while done < free {
                let n = (free - done).min(mixer.chunk());
                let sc = sidechain.as_ref().map_or(0.0, |s| s.now());
                mixer.render(n, sc);
                write_frames(&mixer.mixed[..n * 2], &mut dst[done * ch..(done + n) * ch], ch);
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
/// ducking (the sources feeding it are ducked already, if at all).
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
    use crate::bridge::{BridgeConfig, BridgeProducer, bridge};

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

    /// Multiplies its input by a gain.
    struct Gain(f32);
    impl Processor for Gain {
        fn process(&mut self, input: &[f32], output: &mut [f32], _sidechain_peak: f32) {
            for (o, i) in output.iter_mut().zip(input) {
                *o = i * self.0;
            }
        }
    }

    fn source(id: SourceId, gain: f32) -> (Box<Slot>, BridgeProducer) {
        let (producer, consumer, _) = bridge(BridgeConfig::new(48_000, 1.0));
        (Slot::new(id, consumer, Box::new(Gain(gain)), Arc::default()), producer)
    }

    #[test]
    fn mixes_sources_and_fades_removed_ones() {
        let (mut cmd, cmd_rx) = rtrb::RingBuffer::new(2 * MAX_SOURCES);
        let (garbage_tx, mut garbage) = rtrb::RingBuffer::new(2 * MAX_SOURCES);
        let mut m = Mixer::new(Box::new(Gain(0.5)), cmd_rx, garbage_tx, 48_000, 480);
        let (a, mut pa) = source(1, 1.0);
        let (b, mut pb) = source(2, 2.0);
        assert!(cmd.push(Command::Add(a)).is_ok() && cmd.push(Command::Add(b)).is_ok());
        m.apply_commands();
        let feed = |p: &mut BridgeProducer, v: f32| p.push(&vec![v; 4800 * 2]);
        feed(&mut pa, 0.1);
        feed(&mut pb, 0.2);
        // The bridges' interpolators start from silence.
        m.render(480, 0.0);
        m.render(480, 0.0);
        // master(0.5) * (0.1 * 1 + 0.2 * 2)
        assert!(m.mixed[..960].iter().all(|&s| (s - 0.25).abs() < 1e-6), "{}", m.mixed[0]);

        assert!(cmd.push(Command::Remove(2)).is_ok());
        m.apply_commands();
        m.render(240, 0.0);
        let (first, last) = (m.mixed[0], m.mixed[479]);
        assert!((first - 0.25).abs() < 1e-3, "the fade starts at full level: {first}");
        assert!(last < first && last > 0.06, "fading: {last}");
        m.render(240, 0.0);
        assert!(m.mixed[479] < 0.051, "faded out: {}", m.mixed[479]);
        assert_eq!(m.slots.len(), 1, "the faded slot left the mix");
        assert!(garbage.pop().is_ok_and(|s| s.id == 2), "the removed slot comes back for freeing");
        m.render(240, 0.0);
        assert!((m.mixed[479] - 0.05).abs() < 1e-6, "only source 1 remains: {}", m.mixed[479]);
    }

    /// Panics on the second block.
    struct Broken(u32);
    impl Processor for Broken {
        fn process(&mut self, input: &[f32], output: &mut [f32], _sidechain_peak: f32) {
            self.0 += 1;
            assert!(self.0 < 2, "test: a processor that breaks the contract");
            output.copy_from_slice(input);
        }
    }

    #[test]
    fn a_panicking_processor_only_silences_its_source() {
        let (mut cmd, cmd_rx) = rtrb::RingBuffer::new(2 * MAX_SOURCES);
        let (garbage_tx, mut garbage) = rtrb::RingBuffer::new(2 * MAX_SOURCES);
        let mut m = Mixer::new(Box::new(Gain(1.0)), cmd_rx, garbage_tx, 48_000, 480);
        let (good, mut pg) = source(1, 1.0);
        let (pb_tx, consumer, _) = bridge(BridgeConfig::new(48_000, 1.0));
        let mut pb = pb_tx;
        let fault = Arc::new(AtomicBool::new(false));
        let bad = Slot::new(2, consumer, Box::new(Broken(0)), fault.clone());
        assert!(cmd.push(Command::Add(good)).is_ok() && cmd.push(Command::Add(bad)).is_ok());
        m.apply_commands();
        pg.push(&vec![0.1; 4800 * 2]);
        pb.push(&vec![0.2; 4800 * 2]);
        m.render(480, 0.0);
        m.render(480, 0.0);
        assert!(fault.load(Ordering::Acquire), "the panic is reported");
        m.render(480, 0.0);
        assert!(m.mixed[..960].iter().all(|&s| (s - 0.1).abs() < 1e-6), "only the good source plays: {}", m.mixed[0]);

        // The supervisor removes the broken source: it leaves at once (with its panic payload,
        // to be freed off the real-time thread), the good one plays on.
        assert!(cmd.push(Command::Remove(2)).is_ok());
        m.apply_commands();
        m.render(480, 0.0);
        assert_eq!(m.slots.len(), 1, "the faulty slot left the mix");
        assert!(garbage.pop().is_ok_and(|s| s.id == 2 && s.panic.is_some()));
        assert!(m.mixed[..960].iter().all(|&s| (s - 0.1).abs() < 1e-6));
    }

    #[test]
    fn a_full_mixer_refuses_instead_of_growing() {
        let (mut cmd, cmd_rx) = rtrb::RingBuffer::new(2 * MAX_SLOTS);
        let (garbage_tx, mut garbage) = rtrb::RingBuffer::new(2 * MAX_SLOTS);
        let mut m = Mixer::new(Box::new(Gain(1.0)), cmd_rx, garbage_tx, 48_000, 64);
        let mut producers = Vec::new();
        let mut faults = Vec::new();
        for id in 0..=MAX_SLOTS as u32 {
            let (s, p) = source(id, 1.0);
            producers.push(p);
            faults.push(s.fault.clone());
            assert!(cmd.push(Command::Add(s)).is_ok());
        }
        m.apply_commands();
        assert_eq!(m.slots.len(), MAX_SLOTS);
        assert_eq!(m.slots.capacity(), MAX_SLOTS, "no reallocation");
        assert!(garbage.pop().is_ok_and(|s| s.id == MAX_SLOTS as u32));
        assert!(faults[MAX_SLOTS].load(Ordering::Acquire), "a refused slot is reported, not silently dropped");
        assert!(!faults[0].load(Ordering::Acquire));
    }
}
