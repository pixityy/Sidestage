//! The two Core Audio assemblies of a session:
//!
//! * [`RenderEngine`]: one process tap per source (muted while tapped, unless the source keeps
//!   its original) in a private aggregate clocked by the headphone output. Its one IOProc reads
//!   every tap, runs each source's processor, sums them, runs the master processor and writes
//!   the output, all in the same cycle (`render.rs`).
//! * [`SidechainEngine`]: an unmuted global tap of everything except the sources, the ignored
//!   apps and Sidestage, in a tap-only aggregate whose IOProc only meters.
//!
//! **Changing the sources builds a new render engine; a running one is never re-composed.** An
//! aggregate's tap list is a settable property (`kAudioAggregateDevicePropertyTapList`; Apple's
//! "Capturing system audio with Core Audio taps" sample appends a tap that way), but only on an
//! aggregate whose IO has not started. cpal and FineTune put the taps into the description the
//! aggregate is created with and never change them. Re-composing a running aggregate is a
//! device configuration change, which Core Audio performs with the device's IO stopped, so it
//! would interrupt every source anyway. While IO is stopped nothing reads the kept taps, and
//! `CATapMutedWhenTapped` may let their originals through. And the IO thread would meet the new
//! buffer layout before the control thread could hand it matching lanes; several stereo taps
//! look alike, so one app could play at another's position. FineTune switches devices the same
//! way Sidestage switches sources: it builds a second tap and aggregate for the same process,
//! crossfades, and then destroys the first.
//!
//! So the session builds a complete new engine next to the running one, with fresh taps for
//! every source (a process can be in two taps at once, which FineTune's crossfade relies on). It
//! waits until the new IOProc runs, so the new taps are being read and keep the kept apps muted.
//! Then it switches: the old engine fades out over its next cycle and lets go of the processors,
//! the new one fades in over 5 ms (see `render.rs`). Kept sources keep their processors (reverb
//! tails, smoothing) and their originals stay hidden. What they do lose is a dip where the two
//! engines meet: one cycle fading out, up to one cycle of silence until the new engine's next
//! cycle, 5 ms fading in (10 to 25 ms in all at 512 frames and 48 kHz). The first buffer of the
//! new tap may also repeat or skip a few milliseconds of the app. Building the new engine takes
//! longer (taps, aggregate, IOProc), but the old one keeps playing meanwhile, and a new engine
//! that would gain nothing or lose a playing source (a tap that failed) is dropped unheard. An
//! app's processes coming and going (helpers, a relaunch while the engine runs) never rebuild
//! anything: the tap's process list is rewritten in place (`kAudioTapPropertyDescription`).
//!
//! **Which input buffers belong to which tap.** The IOProc gets one `AudioBuffer` per input
//! stream: the output device's own input streams (a headset microphone) first, then the taps',
//! as the trailing buffers. That is where FineTune, corti and Sidestage's single-tap build find a
//! tap. For several taps, Apple documents only that the order of an aggregate's lists is
//! significant and sets the order of its streams (said of the sub-device list; the tap list is
//! its sibling property). So the engine does not assume its own order. After creating the
//! aggregate it reads the HAL's tap list back and orders the taps by it (`plan::tap_order`).
//! Then the input stream configuration must split into exactly those taps' channel counts in
//! that order, with exactly the device's own input streams in front (`plan::input_layout`), or
//! the build fails. Each cycle, every lane checks the channel counts of its buffers again and
//! reads silence (counted) when they changed. Two taps with the same channel count in swapped
//! order, with the tap list reporting the wrong order too, would not be caught; that needs a
//! check on a Mac.
//!
//! Field order is teardown order: IOProc (stop, unregister), aggregate, taps. Destroying a tap
//! last is what un-mutes its app.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use objc2_core_audio::{
    AudioObjectID, CATapMuteBehavior, kAudioAggregateDevicePropertyTapList, kAudioDevicePropertyStreamConfiguration,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
};

use super::ca::{self, Aggregate, IoProc, OSStatus, Tap, TapKind};
use super::devices::{self, Output};
use super::plan::{self, DeviceStream, SubTap, TapScope, TapStreams};
use super::render::{
    Buffers, BuffersMut, CoreSetup, DspCell, EngineMeters, Handover, LaneMap, LaneSetup, RenderCore, SessionMeters,
    SidechainLevel, SourceMeters, meter_sidechain,
};

static AGGREGATE_COUNTER: AtomicU32 = AtomicU32::new(0);

const TAP_NAME: &str = "Sidestage source";
/// How long a fresh aggregate may take until its input streams show every tap.
const LAYOUT_WAIT: Duration = Duration::from_millis(500);

/// Why a render engine could not be built.
pub enum BuildError {
    /// The session is stopping.
    Cancelled,
    /// None of the sources could be tapped (each failure is reported for its source).
    NothingToTap,
    /// The capture-permission probe failed on every tap: most likely macOS refused audio
    /// capture, but the probe is only proven on mixdown taps and a process that exits mid-build
    /// may also upset it, so the session decides.
    ProbeFailed {
        error: anyhow::Error,
        /// A failed tap was device-scoped.
        device_scoped: bool,
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

/// One source to tap.
pub struct LaneSpec<'a> {
    /// The session's key for the source.
    pub key: u64,
    /// The app's name, for logs.
    pub name: &'a str,
    pub objects: &'a [AudioObjectID],
    pub scope: TapScope,
    /// The app currently plays to the output and nowhere else, so even a mixdown of it runs on
    /// the output's clock.
    pub plays_only_to_output: bool,
    /// Mute the original (`CATapMutedWhenTapped`) or leave it audible.
    pub mute: bool,
}

struct PreparedLane {
    key: u64,
    tap: Tap,
    format: AudioStreamBasicDescription,
    /// The output stream a device-scoped tap covers.
    tapped_stream: Option<DeviceStream>,
    wanted_scope: TapScope,
    scope: TapScope,
    objects: Vec<AudioObjectID>,
    muted: bool,
    plays_only_to_output: bool,
    map: Option<LaneMap>,
}

/// The taps and the aggregate of an engine whose IOProc does not exist yet. Its sample rate is
/// known, so the session can build processors for it first.
pub struct Prepared {
    aggregate: Aggregate,
    lanes: Vec<PreparedLane>,
    pub output: Output,
    /// `nominal_rate` rounded: the rate the processors must be built for.
    pub sample_rate: u32,
    nominal_rate: f64,
    aggregate_inputs: usize,
    device_inputs: Option<usize>,
    out_left: usize,
    out_right: usize,
}

/// What [`Prepared::start`] needs besides the taps.
pub struct StartSpec<'a> {
    pub generation: u32,
    pub handover: Arc<Handover>,
    /// Each lane's processor and meters, in [`Prepared::keys`] order.
    pub dsp: Vec<(Arc<DspCell>, Arc<SourceMeters>)>,
    pub master: Arc<DspCell>,
    pub session: Arc<SessionMeters>,
    pub sidechain: Option<Arc<SidechainLevel>>,
    /// No other engine is playing: render from the first cycle rather than stand by until the
    /// session switches over.
    pub activate: bool,
    pub cancel: &'a AtomicBool,
    /// Called with `true` before the calls that can block on macOS's capture prompt and with
    /// `false` after them, so the UI can tell the user to look for the prompt.
    pub prompt_window: &'a dyn Fn(bool),
}

/// One source's tap in a running engine.
pub struct EngineLane {
    pub key: u64,
    tap: Tap,
    /// The process objects the tap covers now.
    pub objects: Vec<AudioObjectID>,
    /// The scope asked for; `scope` differs when a device-scoped tap was impossible or failed
    /// and the lane fell back to a mixdown.
    pub wanted_scope: TapScope,
    pub scope: TapScope,
    /// The tap's sample rate as last read. A mixdown tap follows the device the app plays to,
    /// so the session refreshes it (it only matters for the macOS 14 rate warning).
    pub tap_rate: f64,
    pub muted: bool,
}

impl EngineLane {
    /// The tap's sample rate now.
    pub fn current_tap_rate(&self) -> Option<f64> {
        self.tap.format().ok().map(|f| f.mSampleRate)
    }
}

pub struct RenderEngine {
    io: IoProc,
    aggregate: Aggregate,
    lanes: Vec<EngineLane>,
    pub output: Output,
    pub generation: u32,
    /// `nominal_rate` rounded, the rate the processors were built for.
    pub sample_rate: u32,
    /// The aggregate's nominal rate at build time.
    pub nominal_rate: f64,
    /// The output device's nominal rate at build time, if it could be read.
    pub output_rate: Option<f64>,
    /// IO buffer size, for the switch's timing.
    pub buffer_frames: u32,
    /// Latency estimate from buffer size and safety offsets, until the IOProc measures it.
    pub fallback_latency_frames: u32,
    pub meters: Arc<EngineMeters>,
}

impl RenderEngine {
    /// Creates every source's tap and the aggregate holding them, and works out where each
    /// tap's audio arrives (see the module docs). A source whose tap cannot be made is left out
    /// and reported in `failed`; the others go ahead.
    pub fn prepare(
        specs: &[LaneSpec<'_>],
        out: &Output,
        cancel: &AtomicBool,
        failed: &mut Vec<(u64, anyhow::Error)>,
    ) -> Result<Prepared, BuildError> {
        // Where the app's stereo sits on the device: the tap of a device stream has that
        // stream's layout.
        let device_channels = devices::output_channels(out.id)?;
        let device_pair = plan::output_pair(devices::preferred_stereo(out.id), device_channels.iter().sum());
        // Unreadable: assume a microphone that could take the tap's place (mixdowns only).
        let input_channels = devices::input_channels(out.id)
            .inspect_err(|e| log::warn!("{}: {e:#}", out.name))
            .unwrap_or_else(|_| device_channels.clone());
        let layout = DeviceLayout { outputs: &device_channels, inputs: &input_channels, pair: device_pair };

        let mut lanes = Vec::with_capacity(specs.len());
        for spec in specs {
            if cancel.load(Ordering::Relaxed) {
                return Err(BuildError::Cancelled);
            }
            match prepare_lane(spec, out, &layout) {
                Ok(lane) => lanes.push(lane),
                Err(e) => {
                    log::warn!("could not tap {}: {e:#}", spec.name);
                    failed.push((spec.key, e));
                }
            }
        }
        if lanes.is_empty() {
            return Err(BuildError::NothingToTap);
        }

        let uid = plan::aggregate_uid("render", std::process::id(), AGGREGATE_COUNTER.fetch_add(1, Ordering::Relaxed));
        let sub_taps: Vec<SubTap<'_>> = lanes
            .iter()
            .map(|l| SubTap {
                uid: l.tap.uid(),
                drift: plan::tap_drift_compensation(
                    l.scope == TapScope::Device || l.plays_only_to_output,
                    out.shares_tap_clock(),
                ),
            })
            .collect();
        let description = plan::render_aggregate("Sidestage", &uid, &out.uid, &sub_taps);
        let aggregate = Aggregate::create(&description).with_context(|| {
            if out.is_aggregate() {
                format!(
                    "creating Sidestage's device for {} (an aggregate or multi-output device cannot be nested; \
                     choose one of its physical outputs instead)",
                    out.name
                )
            } else {
                format!("creating Sidestage's device for {}", out.name)
            }
        })?;
        wait_until_alive(aggregate.id(), cancel)?;
        order_like_the_hal(aggregate.id(), &mut lanes);

        let device_inputs = devices::input_stream_count(out.id).inspect_err(|e| log::warn!("{}: {e:#}", out.name)).ok();
        let tap_channels: Vec<usize> = lanes.iter().map(|l| l.format.mChannelsPerFrame as usize).collect();
        let (aggregate_inputs, layout) = wait_for_layout(aggregate.id(), device_inputs, &tap_channels, cancel)?;
        devices::check_output_formats(aggregate.id())?;
        let out_channels: usize = devices::output_channels(aggregate.id())?.iter().sum();
        let (out_left, out_right) = plan::output_pair(devices::preferred_stereo(out.id), out_channels)
            .with_context(|| format!("{} has no output channels", out.name))?;
        for (lane, streams) in lanes.iter_mut().zip(layout) {
            // A mixdown is plain stereo; a device-stream tap has the stream's layout.
            let (left, right) = plan::tap_pair(
                lane.format.mChannelsPerFrame as usize,
                lane.tapped_stream.and(device_pair),
                lane.tapped_stream.map_or(0, |s| s.first_channel),
            )
            .context("a tap has no channels")?;
            lane.map = Some(LaneMap { from_end: streams.from_end, channels: streams.channels.into(), left, right });
        }

        let nominal_rate = devices::nominal_rate(aggregate.id()).or_else(|_| devices::nominal_rate(out.id))?;
        let sample_rate = nominal_rate.round() as u32;
        if sample_rate == 0 {
            return Err(anyhow!("{} reports no sample rate", out.name).into());
        }
        Ok(Prepared {
            aggregate,
            lanes,
            output: out.clone(),
            sample_rate,
            nominal_rate,
            aggregate_inputs,
            device_inputs,
            out_left,
            out_right,
        })
    }

    pub fn aggregate_id(&self) -> AudioObjectID {
        self.aggregate.id()
    }

    pub fn lanes(&self) -> &[EngineLane] {
        &self.lanes
    }

    pub fn lanes_mut(&mut self) -> &mut [EngineLane] {
        &mut self.lanes
    }

    pub fn lane(&self, key: u64) -> Option<&EngineLane> {
        self.lanes.iter().find(|l| l.key == key)
    }

    pub fn lane_keys(&self) -> Vec<u64> {
        self.lanes.iter().map(|l| l.key).collect()
    }

    /// Follows a source's processes (helpers coming and going, the app relaunching) without a
    /// rebuild.
    pub fn set_processes(&mut self, key: u64, objects: &[AudioObjectID]) -> anyhow::Result<()> {
        let lane = self.lanes.iter_mut().find(|l| l.key == key).context("no such tap")?;
        lane.tap.set_processes(objects)?;
        lane.objects = objects.to_vec();
        Ok(())
    }

    /// Waits until the IOProc has run `cycles` IO cycles (rendering or standing by). False when
    /// it does not within `timeout`, or the session stops.
    pub fn wait_for_io(&self, cycles: u64, timeout: Duration, cancel: &AtomicBool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.meters.cycles.load(Ordering::Relaxed) >= cycles {
                return true;
            }
            if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Tears down in order and reports the first failure (later steps still run: every tap must
    /// go so its app is un-muted).
    pub fn close(mut self) -> anyhow::Result<()> {
        let io = self.io.close();
        let aggregate = self.aggregate.close();
        let mut taps = Ok(());
        for lane in &mut self.lanes {
            let closed = lane.tap.close();
            if taps.is_ok() {
                taps = closed;
            }
        }
        io.and(aggregate).and(taps)
    }
}

impl Prepared {
    /// The sources that got a tap, in the engine's lane order.
    pub fn keys(&self) -> Vec<u64> {
        self.lanes.iter().map(|l| l.key).collect()
    }

    /// Registers and starts the IOProc and confirms capture. The engine stands by (its taps are
    /// read, so their apps are muted, but it plays nothing) until the session activates its
    /// generation, unless `spec.activate` does that right away.
    pub fn start(self, spec: StartSpec<'_>) -> Result<RenderEngine, BuildError> {
        if spec.dsp.len() != self.lanes.len() {
            return Err(anyhow!("{} processors for {} taps", spec.dsp.len(), self.lanes.len()).into());
        }
        let meters = Arc::new(EngineMeters::default());
        let mut lanes = Vec::with_capacity(self.lanes.len());
        for (lane, (dsp, source_meters)) in self.lanes.iter().zip(spec.dsp) {
            let map = lane.map.clone().context("a tap's streams were not found")?;
            lanes.push(LaneSetup { map, dsp, meters: source_meters });
        }
        let core = RenderCore::new(CoreSetup {
            generation: spec.generation,
            handover: spec.handover.clone(),
            lanes,
            master: spec.master,
            out_left: self.out_left,
            out_right: self.out_right,
            sample_rate: self.sample_rate,
            session: spec.session,
            engine: meters.clone(),
            sidechain: spec.sidechain,
        });
        if spec.cancel.load(Ordering::Relaxed) {
            return Err(BuildError::Cancelled);
        }

        // The first IOProc on a tap aggregate (or its first start) is where macOS shows the
        // audio-capture prompt; both calls can block until the user answers.
        let asked = Instant::now();
        let prompt = PromptWindow::open(spec.prompt_window);
        let mut io = IoProc::create(self.aggregate.id(), render_io_proc, Box::new(core))
            .map_err(|e| prompt_failure(e, asked))?;
        if let Some(flags) =
            self.device_inputs.and_then(|device_inputs| plan::input_stream_usage(self.aggregate_inputs, device_inputs))
            && let Err(e) = io.set_input_stream_usage(&flags)
        {
            log::warn!(
                "could not turn off {}'s input streams (macOS may ask for the microphone): {e:#}",
                self.output.name
            );
        }
        if spec.cancel.load(Ordering::Relaxed) {
            return Err(BuildError::Cancelled);
        }
        if spec.activate {
            spec.handover.activate(spec.generation);
        }
        io.start().map_err(|e| prompt_failure(e, asked))?;
        drop(prompt);

        // After the start, unlike Chromium (which probes between create and start): Apple says
        // the prompt comes with the first start, and a probe before the user answers would
        // fail. One tap that passes proves the permission; a tap that fails next to one that
        // passes most likely lost its process during the build, and following the processes
        // repairs it.
        let mut passed = false;
        let mut first_error = None;
        let mut device_scoped = false;
        for lane in &self.lanes {
            match lane.tap.probe_permission() {
                Ok(()) => passed = true,
                Err(e) => {
                    log::warn!("audio capture probe failed on a {:?} tap: {e:#}", lane.scope);
                    device_scoped |= lane.scope == TapScope::Device;
                    first_error.get_or_insert(e);
                }
            }
        }
        if !passed {
            let error = first_error.unwrap_or_else(|| anyhow!("no tap to probe"));
            return Err(BuildError::ProbeFailed { error, device_scoped });
        }

        let fallback_latency_frames = devices::buffer_frames(self.aggregate.id())
            + devices::safety_offset(self.aggregate.id(), true)
            + devices::safety_offset(self.aggregate.id(), false);
        let buffer_frames = devices::buffer_frames(self.aggregate.id());
        let output_rate = devices::nominal_rate(self.output.id).ok();
        for lane in &self.lanes {
            log::info!(
                "tapping {} process(es) with a {:?} tap ({} ch, {} Hz); app L/R on tap channels {:?}",
                lane.objects.len(),
                lane.scope,
                lane.format.mChannelsPerFrame,
                lane.format.mSampleRate,
                lane.map.as_ref().map(|m| (m.left, m.right)),
            );
        }
        log::info!(
            "rendering {} source(s) to {} at {} Hz, L/R {}/{}",
            self.lanes.len(),
            self.output.name,
            self.sample_rate,
            self.out_left,
            self.out_right
        );
        let lanes = self
            .lanes
            .into_iter()
            .map(|l| EngineLane {
                key: l.key,
                // The baseline for the session's rate checks, read now that IO runs.
                tap_rate: l.tap.format().map_or(l.format.mSampleRate, |f| f.mSampleRate),
                tap: l.tap,
                objects: l.objects,
                wanted_scope: l.wanted_scope,
                scope: l.scope,
                muted: l.muted,
            })
            .collect();
        Ok(RenderEngine {
            io,
            aggregate: self.aggregate,
            lanes,
            output: self.output,
            generation: spec.generation,
            sample_rate: self.sample_rate,
            nominal_rate: self.nominal_rate,
            output_rate,
            buffer_frames,
            fallback_latency_frames,
            meters,
        })
    }
}

/// The output device's streams, as far as device-scoped taps are concerned.
struct DeviceLayout<'a> {
    /// Channels of each output stream.
    outputs: &'a [usize],
    /// Channels of each input stream.
    inputs: &'a [usize],
    /// The device's stereo pair, 0-based device-wide channels.
    pair: Option<(usize, usize)>,
}

/// One source's tap: device-scoped when asked for and possible, else a stereo mixdown.
fn prepare_lane(spec: &LaneSpec<'_>, out: &Output, layout: &DeviceLayout<'_>) -> anyhow::Result<PreparedLane> {
    let mute = if spec.mute { CATapMuteBehavior::MutedWhenTapped } else { CATapMuteBehavior::Unmuted };
    let device_tap = match spec.scope {
        TapScope::Device => device_tap(out, layout, spec.objects, mute),
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
        anyhow::bail!(
            "the tapped audio has an unsupported format ({} channels, {} bits, flags {:#x})",
            format.mChannelsPerFrame,
            format.mBitsPerChannel,
            format.mFormatFlags
        );
    }
    Ok(PreparedLane {
        key: spec.key,
        tap,
        format,
        tapped_stream,
        wanted_scope: spec.scope,
        scope,
        objects: spec.objects.to_vec(),
        muted: spec.mute,
        plays_only_to_output: spec.plays_only_to_output,
        map: None,
    })
}

/// Puts the lanes in the order the aggregate reports its taps in. Unreadable or inconsistent
/// reports keep Sidestage's order, which the layout check then has to confirm.
fn order_like_the_hal(aggregate: AudioObjectID, lanes: &mut Vec<PreparedLane>) {
    let ours: Vec<String> = lanes.iter().map(|l| l.tap.uid().to_string()).collect();
    let reported = match ca::get_string_array(aggregate, kAudioAggregateDevicePropertyTapList, GLOBAL) {
        Ok(reported) => reported,
        Err(e) => {
            log::warn!("could not read the order of Sidestage's taps: {e:#}");
            return;
        }
    };
    let Some(order) = plan::tap_order(&ours, &reported) else {
        log::warn!("Sidestage's device lists the taps {reported:?}, expected {ours:?}; keeping the requested order");
        return;
    };
    if order.iter().enumerate().any(|(i, &o)| i != o) {
        log::info!("Sidestage's device orders its taps {order:?}");
        let mut slots: Vec<Option<PreparedLane>> = std::mem::take(lanes).into_iter().map(Some).collect();
        lanes.extend(order.iter().filter_map(|&i| slots.get_mut(i).and_then(Option::take)));
    }
}

const GLOBAL: u32 = kAudioObjectPropertyScopeGlobal;

/// The aggregate's input stream count and where each tap's streams sit. A fresh aggregate may
/// take a moment to show every tap's stream, so a layout that does not fit yet is read again
/// for a little while before the build fails.
fn wait_for_layout(
    aggregate: AudioObjectID,
    device_inputs: Option<usize>,
    tap_channels: &[usize],
    cancel: &AtomicBool,
) -> Result<(usize, Vec<TapStreams>), BuildError> {
    let deadline = Instant::now() + LAYOUT_WAIT;
    loop {
        let streams =
            ca::stream_channels(aggregate, kAudioDevicePropertyStreamConfiguration, kAudioObjectPropertyScopeInput)
                .context("reading Sidestage's input streams")?;
        match plan::input_layout(&streams, device_inputs, tap_channels) {
            Ok(layout) => return Ok((streams.len(), layout)),
            Err(why) if Instant::now() >= deadline => {
                return Err(anyhow!("the apps' audio did not reach Sidestage's device as expected: {why}").into());
            }
            Err(_) => {}
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(BuildError::Cancelled);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub struct SidechainEngine {
    io: IoProc,
    aggregate: Aggregate,
    tap: Tap,
    /// Sidestage's own process object, excluded on every update. Kept rather than looked up
    /// again, so a failed lookup can never let the ducker hear Sidestage's own output.
    own: AudioObjectID,
    /// The exclusion list the tap has now.
    excluded: Vec<AudioObjectID>,
    /// The device rate it was built for (the render engine's).
    pub rate: u32,
}

impl SidechainEngine {
    /// Taps everything except `apps` (the process objects of the sources and the ignored apps)
    /// and `own` (Sidestage's process object; without it the ducker would hear the sources).
    pub fn build(
        apps: &[AudioObjectID],
        own: AudioObjectID,
        rate: u32,
        level: Arc<SidechainLevel>,
        cancel: &AtomicBool,
    ) -> anyhow::Result<Self> {
        let excluded = plan::sidechain_excluded(apps.iter().copied(), own);
        let desc = ca::tap_description(
            &TapKind::GlobalExcluding,
            &excluded,
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
        if let Err(e) = devices::set_nominal_rate(aggregate.id(), f64::from(rate)) {
            log::debug!("sidechain device rate: {e:#}");
        }
        let mut io = IoProc::create(aggregate.id(), sidechain_io_proc, Box::new(level))?;
        io.start()?;
        log::info!("sidechain running (excluding {} process(es), Sidestage included)", excluded.len());
        Ok(Self { io, aggregate, tap, own, excluded, rate })
    }

    /// Follows the sources' and ignored apps' processes; Sidestage itself stays excluded. Only
    /// writes the tap when the list changed.
    pub fn set_apps(&mut self, apps: &[AudioObjectID]) -> anyhow::Result<()> {
        let excluded = plan::sidechain_excluded(apps.iter().copied(), self.own);
        if excluded != self.excluded {
            self.tap.set_processes(&excluded)?;
            log::debug!("sidechain now excludes {} process(es)", excluded.len());
            self.excluded = excluded;
        }
        Ok(())
    }

    pub fn close(mut self) -> anyhow::Result<()> {
        let io = self.io.close();
        let aggregate = self.aggregate.close();
        let tap = self.tap.close();
        io.and(aggregate).and(tap)
    }
}

/// A tap of the output stream that carries the device's stereo pair, checked to be what was
/// asked for. `None` (logged) means the caller should use a stereo mixdown instead: the pair
/// spans several streams, a tap on the wrong stream could not be told apart, the tap could not
/// be made, or it took another stream's format.
fn device_tap(
    out: &Output,
    layout: &DeviceLayout<'_>,
    objects: &[AudioObjectID],
    mute: CATapMuteBehavior,
) -> Option<(Tap, DeviceStream)> {
    let (outputs, pair) = (layout.outputs, layout.pair);
    let Some(stream) = pair.and_then(|pair| plan::device_tap_stream(outputs, pair)) else {
        log::info!(
            "{}'s stereo channels {pair:?} are not in one output stream {outputs:?}; using a stereo mixdown",
            out.name
        );
        return None;
    };
    if !plan::device_tap_verifiable(stream, outputs, layout.inputs) {
        log::info!(
            "{} has input streams {:?} like its output stream {}; using a stereo mixdown",
            out.name,
            layout.inputs,
            stream.index
        );
        return None;
    }
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
/// the process aborts (and macOS un-mutes every app as its tap dies) rather than unwinding
/// into Core Audio's IO thread. `catch_unwind` is not real-time safe, so it is not used.
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
