//! The two Core Audio assemblies of a session, each a tap + private aggregate + IOProc:
//!
//! * [`RenderEngine`]: the music app's muted tap inside an aggregate clocked by the headphone
//!   output; its IOProc renders the tapped audio to that output in the same cycle.
//! * [`SidechainEngine`]: an unmuted global tap of everything except the music and Sidestage,
//!   in a tap-only aggregate whose IOProc only meters.
//!
//! Field order is teardown order: IOProc (stop, unregister), aggregate, tap. Destroying the tap
//! last is what un-mutes the app.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use objc2_core_audio::{AudioObjectID, CATapMuteBehavior};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp, AudioTimeStampFlags};

use super::ca::{self, Aggregate, IoProc, OSStatus, Tap, TapKind};
use super::devices::{self, Output};
use super::plan::{self, DeviceStream, TapScope};
use super::render::{Buffers, BuffersMut, RenderCore, RenderMeters, SidechainLevel, StereoMap, meter_sidechain};
use crate::ProcessorFactory;

static AGGREGATE_COUNTER: AtomicU32 = AtomicU32::new(0);

const TAP_NAME: &str = "Sidestage music";

/// Why a render engine could not be built.
pub enum BuildError {
    /// The session is stopping.
    Cancelled,
    /// The capture-permission probe failed on a tap of this scope: most likely macOS refused
    /// audio capture, but the probe is only proven on mixdown taps and a process that exits
    /// mid-build may also upset it, so the session decides.
    ProbeFailed {
        error: anyhow::Error,
        scope: TapScope,
    },
    /// macOS's capture prompt went unanswered until it timed out; Core Audio then keeps
    /// failing until Sidestage restarts, so retrying is pointless.
    PromptTimedOut(anyhow::Error),
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for BuildError {
    fn from(e: anyhow::Error) -> Self {
        BuildError::Failed(e)
    }
}

/// What to build.
pub struct RenderSpec<'a> {
    pub objects: &'a [AudioObjectID],
    pub output: &'a Output,
    pub scope: TapScope,
    /// The app currently plays to `output` and nowhere else, so even a mixdown of it runs on
    /// the output's clock.
    pub plays_only_to_output: bool,
    /// Mute the original (`CATapMutedWhenTapped`) or leave it audible.
    pub mute: bool,
    pub factory: &'a ProcessorFactory,
    pub meters: Arc<RenderMeters>,
    pub sidechain: Option<Arc<SidechainLevel>>,
    pub cancel: &'a AtomicBool,
    /// Called with `true` before the calls that can block on macOS's capture prompt and with
    /// `false` after them, so the UI can tell the user to look for the prompt.
    pub prompt_window: &'a dyn Fn(bool),
}

pub struct RenderEngine {
    io: IoProc,
    aggregate: Aggregate,
    tap: Tap,
    pub output: Output,
    /// `nominal_rate` rounded, the rate the processor was built for.
    pub sample_rate: u32,
    /// The aggregate's nominal rate at build time.
    pub nominal_rate: f64,
    /// The output device's nominal rate at build time, if it could be read.
    pub output_rate: Option<f64>,
    /// The scope asked for; `scope` differs when a device-scoped tap was impossible or failed
    /// and the engine fell back to a mixdown.
    pub wanted_scope: TapScope,
    pub scope: TapScope,
    /// The tap's sample rate as last read. A mixdown tap follows the device the app plays to,
    /// so the session refreshes it (it only matters for the macOS 14 rate warning).
    pub tap_rate: f64,
    pub objects: Vec<AudioObjectID>,
    pub muted: bool,
    /// Latency estimate from buffer size and safety offsets, until the IOProc measures it.
    pub fallback_latency_frames: u32,
}

impl RenderEngine {
    pub fn build(spec: RenderSpec<'_>) -> Result<Self, BuildError> {
        let out = spec.output;
        let mute = if spec.mute { CATapMuteBehavior::MutedWhenTapped } else { CATapMuteBehavior::Unmuted };

        // Where the app's stereo sits on the device: the tap of a device stream has that
        // stream's layout.
        let device_channels = devices::output_channels(out.id)?;
        let device_pair = plan::output_pair(devices::preferred_stereo(out.id), device_channels.iter().sum());

        let device_tap = match spec.scope {
            TapScope::Device => device_tap(out, &device_channels, device_pair, spec.objects, mute),
            TapScope::Mixdown => None,
        };
        let (tap, scope, tapped_stream) = match device_tap {
            Some((tap, stream)) => (tap, TapScope::Device, Some(stream)),
            None => (
                Tap::create(ca::tap_description(&TapKind::StereoMixdown, spec.objects, mute, TAP_NAME))?,
                TapScope::Mixdown,
                None,
            ),
        };
        let format = tap.format()?;
        if !ca::is_float32(&format) || format.mChannelsPerFrame == 0 {
            return Err(anyhow!(
                "the tapped audio has an unsupported format ({} channels, {} bits, flags {:#x})",
                format.mChannelsPerFrame,
                format.mBitsPerChannel,
                format.mFormatFlags
            )
            .into());
        }

        let uid = plan::aggregate_uid("render", std::process::id(), AGGREGATE_COUNTER.fetch_add(1, Ordering::Relaxed));
        let drift = plan::tap_drift_compensation(
            scope == TapScope::Device || spec.plays_only_to_output,
            out.shares_tap_clock(),
        );
        let aggregate = Aggregate::create(&plan::render_aggregate("Sidestage", &uid, &out.uid, tap.uid(), drift))
            .with_context(|| {
                if out.is_aggregate() {
                    format!(
                        "creating Sidestage's device for {} (an aggregate or multi-output device cannot \
                     be nested; choose one of its physical outputs instead)",
                        out.name
                    )
                } else {
                    format!("creating Sidestage's device for {}", out.name)
                }
            })?;
        wait_until_alive(aggregate.id(), spec.cancel)?;

        let aggregate_inputs = devices::input_stream_count(aggregate.id())?;
        let device_inputs = devices::input_stream_count(out.id).inspect_err(|e| log::warn!("{}: {e:#}", out.name)).ok();
        let tap_streams = plan::tap_stream_count(aggregate_inputs, device_inputs)
            .context("the app's audio did not reach Sidestage's device (no tap stream)")?;
        devices::check_output_formats(aggregate.id())?;
        let out_channels: usize = devices::output_channels(aggregate.id())?.iter().sum();
        let (out_left, out_right) = plan::output_pair(devices::preferred_stereo(out.id), out_channels)
            .with_context(|| format!("{} has no output channels", out.name))?;
        // A mixdown is plain stereo; a device-stream tap has the stream's layout.
        let (tap_left, tap_right) = plan::tap_pair(
            format.mChannelsPerFrame as usize,
            tapped_stream.and(device_pair),
            tapped_stream.map_or(0, |s| s.first_channel),
        )
        .context("the tap has no channels")?;

        let nominal_rate = devices::nominal_rate(aggregate.id()).or_else(|_| devices::nominal_rate(out.id))?;
        let sample_rate = nominal_rate.round() as u32;
        if sample_rate == 0 {
            return Err(anyhow!("{} reports no sample rate", out.name).into());
        }
        let map = StereoMap { tap_buffers: tap_streams, tap_left, tap_right, out_left, out_right };
        let core = RenderCore::new(map, (spec.factory)(sample_rate), sample_rate, spec.meters, spec.sidechain);
        if spec.cancel.load(Ordering::Relaxed) {
            return Err(BuildError::Cancelled);
        }

        // The first IOProc on a tap aggregate (or its first start) is where macOS shows the
        // audio-capture prompt; both calls can block until the user answers.
        let asked = Instant::now();
        let prompt = PromptWindow::open(spec.prompt_window);
        let mut io =
            IoProc::create(aggregate.id(), render_io_proc, Box::new(core)).map_err(|e| prompt_failure(e, asked))?;
        if let Some(flags) =
            device_inputs.and_then(|device_inputs| plan::input_stream_usage(aggregate_inputs, device_inputs))
            && let Err(e) = io.set_input_stream_usage(&flags)
        {
            log::warn!("could not turn off {}'s input streams (macOS may ask for the microphone): {e:#}", out.name);
        }
        if spec.cancel.load(Ordering::Relaxed) {
            return Err(BuildError::Cancelled);
        }
        io.start().map_err(|e| prompt_failure(e, asked))?;
        drop(prompt);
        // After the start, unlike Chromium (which probes between create and start): Apple says
        // the prompt comes with the first start, and a probe before the user answers would
        // fail.
        if let Err(error) = tap.probe_permission() {
            return Err(BuildError::ProbeFailed { error, scope });
        }

        // The baseline for the session's rate checks, read now that IO runs.
        let tap_rate = tap.format().map_or(format.mSampleRate, |f| f.mSampleRate);
        let fallback_latency_frames = devices::buffer_frames(aggregate.id())
            + devices::safety_offset(aggregate.id(), true)
            + devices::safety_offset(aggregate.id(), false);
        log::info!(
            "capturing {} process(es) with a {scope:?} tap ({} ch, {} Hz) into {} at {sample_rate} Hz; L/R {tap_left}/{tap_right} -> {out_left}/{out_right}",
            spec.objects.len(),
            format.mChannelsPerFrame,
            format.mSampleRate,
            out.name,
        );
        Ok(Self {
            io,
            aggregate,
            tap,
            output: out.clone(),
            sample_rate,
            nominal_rate,
            output_rate: devices::nominal_rate(out.id).ok(),
            wanted_scope: spec.scope,
            scope,
            tap_rate,
            objects: spec.objects.to_vec(),
            muted: spec.mute,
            fallback_latency_frames,
        })
    }

    pub fn aggregate_id(&self) -> AudioObjectID {
        self.aggregate.id()
    }

    /// The tap's sample rate now.
    pub fn current_tap_rate(&self) -> Option<f64> {
        self.tap.format().ok().map(|f| f.mSampleRate)
    }

    /// Follows the app's helper processes coming and going without a rebuild.
    pub fn set_processes(&mut self, objects: &[AudioObjectID]) -> anyhow::Result<()> {
        self.tap.set_processes(objects)?;
        self.objects = objects.to_vec();
        Ok(())
    }

    /// Tears down in order and reports the first failure (later steps still run: the tap must
    /// go so the app is un-muted).
    pub fn close(mut self) -> anyhow::Result<()> {
        let io = self.io.close();
        let aggregate = self.aggregate.close();
        let tap = self.tap.close();
        io.and(aggregate).and(tap)
    }
}

pub struct SidechainEngine {
    io: IoProc,
    aggregate: Aggregate,
    tap: Tap,
    /// Sidestage's own process object, excluded on every update. Kept rather than looked up
    /// again, so a failed lookup can never let the ducker hear the spatialized music.
    own: AudioObjectID,
}

impl SidechainEngine {
    /// Taps everything except `music` (the music app's process objects) and `own` (Sidestage's
    /// process object; without it the ducker would hear the spatialized music).
    pub fn build(
        music: &[AudioObjectID],
        own: AudioObjectID,
        output_rate: f64,
        level: Arc<SidechainLevel>,
        cancel: &AtomicBool,
    ) -> anyhow::Result<Self> {
        let desc = ca::tap_description(
            &TapKind::GlobalExcluding,
            &plan::sidechain_excluded(music, own),
            CATapMuteBehavior::Unmuted,
            "Sidestage sidechain",
        );
        let tap = Tap::create(desc)?;
        let format = tap.format()?;
        if !ca::is_float32(&format) {
            anyhow::bail!("the sidechain tap has an unsupported format");
        }
        let uid =
            plan::aggregate_uid("sidechain", std::process::id(), AGGREGATE_COUNTER.fetch_add(1, Ordering::Relaxed));
        let aggregate = Aggregate::create(&plan::tap_only_aggregate("Sidestage sidechain", &uid, tap.uid(), true))
            .context("creating the sidechain device")?;
        if let Err(BuildError::Cancelled) = wait_until_alive(aggregate.id(), cancel) {
            anyhow::bail!("the session is stopping");
        }
        // macOS 14 taps do not resample, so run the tap-only device at the output's rate, as
        // Chromium does. Best effort: a meter tolerates a mismatch.
        if let Err(e) = devices::set_nominal_rate(aggregate.id(), output_rate) {
            log::debug!("sidechain device rate: {e:#}");
        }
        let mut io = IoProc::create(aggregate.id(), sidechain_io_proc, Box::new(level))?;
        io.start()?;
        log::info!("sidechain running (excluding {} music process(es) and Sidestage)", music.len());
        Ok(Self { io, aggregate, tap, own })
    }

    /// Follows the music app's processes; Sidestage itself stays excluded.
    pub fn set_music(&mut self, music: &[AudioObjectID]) -> anyhow::Result<()> {
        self.tap.set_processes(&plan::sidechain_excluded(music, self.own))
    }

    pub fn close(mut self) -> anyhow::Result<()> {
        let io = self.io.close();
        let aggregate = self.aggregate.close();
        let tap = self.tap.close();
        io.and(aggregate).and(tap)
    }
}

/// The music tap on the output stream that carries the device's stereo pair, checked to be
/// what was asked for. `None` (logged) means the caller should use a stereo mixdown instead:
/// the pair spans several streams, the tap could not be made, or it took another stream's
/// format.
fn device_tap(
    out: &Output,
    stream_channels: &[usize],
    pair: Option<(usize, usize)>,
    objects: &[AudioObjectID],
    mute: CATapMuteBehavior,
) -> Option<(Tap, DeviceStream)> {
    let Some(stream) = pair.and_then(|pair| plan::device_tap_stream(stream_channels, pair)) else {
        log::info!(
            "{}'s stereo channels {pair:?} are not in one output stream {stream_channels:?}; using a stereo mixdown",
            out.name
        );
        return None;
    };
    let kind = TapKind::DeviceStream { device_uid: &out.uid, stream: stream.index };
    let tap = match Tap::create(ca::tap_description(&kind, objects, mute, TAP_NAME)) {
        Ok(tap) => tap,
        Err(e) => {
            log::warn!("device-scoped tap on {} failed ({e:#}); using a stereo mixdown", out.name);
            return None;
        }
    };
    let checked = tap.format().and_then(|format| {
        let device_rate = devices::nominal_rate(out.id)?;
        let channels = format.mChannelsPerFrame as usize;
        if plan::device_tap_matches(channels, format.mSampleRate, stream, device_rate) {
            Ok(())
        } else {
            Err(anyhow!(
                "it has {channels} channels at {} Hz, but output stream {} has {} at {device_rate} Hz",
                format.mSampleRate,
                stream.index,
                stream.channels
            ))
        }
    });
    match checked {
        Ok(()) => Some((tap, stream)),
        Err(e) => {
            // Dropping the tap destroys it before the mixdown replaces it.
            log::warn!("device-scoped tap on {} is unusable ({e:#}); using a stereo mixdown", out.name);
            None
        }
    }
}

/// A fresh aggregate can take a moment to come alive (FineTune waits for it). Not fatal: Core
/// Audio reports a dead device again through the IOProc start or the alive listener.
fn wait_until_alive(device: AudioObjectID, cancel: &AtomicBool) -> Result<(), BuildError> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !devices::is_alive(device) {
        if cancel.load(Ordering::Relaxed) {
            return Err(BuildError::Cancelled);
        }
        if Instant::now() >= deadline {
            log::warn!("Sidestage's aggregate device is not alive after 2 s; continuing");
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Brackets the calls that can wait for the capture prompt; closes on every exit path.
struct PromptWindow<'a>(&'a dyn Fn(bool));

impl<'a> PromptWindow<'a> {
    fn open(notify: &'a dyn Fn(bool)) -> Self {
        notify(true);
        Self(notify)
    }
}

impl Drop for PromptWindow<'_> {
    fn drop(&mut self) {
        (self.0)(false);
    }
}

/// Recognizes the one known cause of a very slow failure: an unanswered permission prompt
/// times out after about a minute, and Core Audio then fails until the app restarts.
fn prompt_failure(e: anyhow::Error, asked: Instant) -> BuildError {
    if plan::prompt_timed_out(asked.elapsed()) { BuildError::PromptTimedOut(e) } else { BuildError::Failed(e) }
}

/// An IOProc buffer list, valid for one IO cycle.
struct HalBuffers {
    list: *mut AudioBufferList,
    count: usize,
}

impl HalBuffers {
    /// # Safety
    /// `list` is null or the IOProc's buffer list for the current cycle, used only during it.
    unsafe fn new(list: *mut AudioBufferList) -> Self {
        // SAFETY: caller contract.
        let count = unsafe { list.as_ref() }.map_or(0, |l| l.mNumberBuffers as usize);
        Self { list, count }
    }

    fn buffer(&self, i: usize) -> Option<&AudioBuffer> {
        if i >= self.count {
            return None;
        }
        // SAFETY: `mBuffers` is a C flexible array with `mNumberBuffers` entries.
        unsafe { (&raw const (*self.list).mBuffers).cast::<AudioBuffer>().add(i).as_ref() }
    }

    fn data(&self, i: usize) -> Option<(*mut f32, usize)> {
        let b = self.buffer(i)?;
        let data = b.mData.cast::<f32>();
        if data.is_null() || !data.is_aligned() {
            return None;
        }
        Some((data, b.mDataByteSize as usize / size_of::<f32>()))
    }
}

impl Buffers for HalBuffers {
    fn count(&self) -> usize {
        self.count
    }

    fn channels(&self, i: usize) -> usize {
        self.buffer(i).map_or(0, |b| b.mNumberChannels as usize)
    }

    fn samples(&self, i: usize) -> Option<&[f32]> {
        let (data, len) = self.data(i)?;
        // SAFETY: the HAL's buffer for this cycle, `len` floats long.
        Some(unsafe { std::slice::from_raw_parts(data, len) })
    }
}

impl BuffersMut for HalBuffers {
    fn samples_mut(&mut self, i: usize) -> Option<&mut [f32]> {
        let (data, len) = self.data(i)?;
        // SAFETY: as above; output buffers are ours to write, and `&mut self` keeps this the
        // only live slice.
        Some(unsafe { std::slice::from_raw_parts_mut(data, len) })
    }
}

/// # Safety
/// `t` is null or a valid timestamp.
unsafe fn sample_time(t: *const AudioTimeStamp) -> Option<f64> {
    // SAFETY: caller contract.
    let t = unsafe { t.as_ref() }?;
    t.mFlags.contains(AudioTimeStampFlags::SampleTimeValid).then_some(t.mSampleTime)
}

unsafe extern "C-unwind" fn render_io_proc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    output_time: *const AudioTimeStamp,
    context: *mut c_void,
) -> OSStatus {
    // SAFETY: Core Audio passes this cycle's buffers and timestamps and the registered context.
    unsafe { render_cycle(input, input_time, output, output_time, context) };
    0
}

/// `extern "C"` on purpose: if the render path ever panicked despite being written not to,
/// the process aborts (and macOS un-mutes the app as its tap dies) rather than unwinding into
/// Core Audio's IO thread. `catch_unwind` is not real-time safe, so it is not used.
unsafe extern "C" fn render_cycle(
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    output_time: *const AudioTimeStamp,
    context: *mut c_void,
) {
    // SAFETY: the context is the RenderCore registered with this IOProc; only this IO thread
    // uses it while the IOProc is registered.
    let Some(core) = (unsafe { context.cast::<RenderCore>().as_mut() }) else {
        return;
    };
    // SAFETY: this cycle's buffer lists; the input list is only read.
    let (input, mut output) = unsafe { (HalBuffers::new(input.cast_mut()), HalBuffers::new(output)) };
    // SAFETY: this cycle's timestamps.
    let (input_time, output_time) = unsafe { (sample_time(input_time), sample_time(output_time)) };
    core.render(&input, &mut output, input_time, output_time);
}

unsafe extern "C-unwind" fn sidechain_io_proc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    _input_time: *const AudioTimeStamp,
    _output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    context: *mut c_void,
) -> OSStatus {
    // SAFETY: as for `render_io_proc`.
    unsafe { sidechain_cycle(input, context) };
    0
}

/// `extern "C"` for the same reason as [`render_cycle`].
unsafe extern "C" fn sidechain_cycle(input: *const AudioBufferList, context: *mut c_void) {
    // SAFETY: the context is the `Arc<SidechainLevel>` registered with this IOProc.
    let Some(level) = (unsafe { context.cast::<Arc<SidechainLevel>>().as_ref() }) else {
        return;
    };
    // SAFETY: this cycle's input list, only read.
    let input = unsafe { HalBuffers::new(input.cast_mut()) };
    meter_sidechain(level, &input);
}
