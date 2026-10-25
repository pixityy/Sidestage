//! A running session. One control thread owns every Core Audio object: it builds the render
//! engine (which can block on macOS's capture prompt, so never on the UI thread), follows the
//! sources' apps and their processes, the output device and its sample rate, and builds a new
//! engine when the set of taps has to change (see `engine.rs`). `set_sources`,
//! `set_sidechain_ignore` and the listener blocks only post events to it; the UI reads status
//! through a mutex and atomics.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::autoreleasepool;
use objc2_core_audio::{
    AudioObjectID, kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyNominalSampleRate,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices, kAudioHardwarePropertyProcessObjectList,
    kAudioObjectPropertyScopeGlobal,
};

use super::ca::{self, Listener, SYSTEM_OBJECT};
use super::devices;
use super::engine::{BuildError, LaneSpec, RenderEngine, SidechainEngine, StartSpec};
use super::group::{AppGroup, SelfInfo};
use super::plan::{self, TapScope};
use super::procs::{self, SysLookup};
use super::render::{DspCell, Handover, IDLE, SessionMeters, SidechainLevel, SourceMeters, next_generation};
use super::sources::{self, LaneNeed, MAX_FAILURES};
use super::sys::OsVersion;
use crate::{
    AppKey, HideMethod, ProcessorFactory, Session, SessionConfig, SessionState, SessionStatus, SourceConfig, SourceId,
    SourceStatus,
};

/// Shown when macOS refuses audio capture.
pub const PERMISSION_DENIED: &str = "macOS is not letting Sidestage capture audio. Open System Settings > Privacy & \
    Security > Screen & System Audio Recording, allow Sidestage under \"System Audio Recording Only\" (or your terminal \
    app, if you started Sidestage from one), then start again. If it still fails, quit and reopen Sidestage.";

/// Shown when macOS's capture prompt timed out; Core Audio refuses until Sidestage restarts.
pub const PROMPT_TIMED_OUT: &str = "macOS's audio capture question was not answered in time. Quit and reopen Sidestage, \
    start again and choose Allow when macOS asks.";

/// Shown while starting takes long enough that macOS is probably showing its capture prompt.
const PROMPT_HINT: &str = "macOS may be asking whether Sidestage can capture audio. Choose Allow to start.";
const PROMPT_HINT_AFTER: Duration = Duration::from_millis(1500);

/// How often the control thread re-checks processes, devices and warnings without an event.
const POLL: Duration = Duration::from_secs(1);
/// Listeners on the system object: default output, device list, process list.
const SYSTEM_LISTENERS: usize = 3;
/// How long `stop` waits for the control thread before leaving it to finish on its own.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// IO cycles a new engine must have run before it takes over: its taps are being read then, so
/// the kept apps stay muted when the old engine's taps go.
const WARMUP_CYCLES: u64 = 2;
/// How long a new engine may take to run them. Bluetooth outputs are the slowest to start
/// (FineTune waits 300 ms for them before crossfading).
const WARMUP_TIMEOUT: Duration = Duration::from_millis(1500);
/// Polls in a row without a single IO cycle after which the render engine counts as stalled.
const STALLED_POLLS: u32 = 2;
/// Failed rewrites of a tap's process list in a row before the source's tap is rebuilt. One
/// failure is usually a process that exited between the scan and the write.
const TAP_UPDATE_RETRIES: u32 = 3;
/// How long the render engine keeps running once none of its sources' apps has audio processes
/// (their tails have long rung out by then). Then it closes, so the output, a Bluetooth headset
/// especially, can go idle; an app that comes back is tapped by a new build.
const IDLE_CLOSE: Duration = Duration::from_secs(15);

/// Wake-ups for the control thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Stop,
    /// `set_sources` queued a list in [`Requests`].
    SourcesChanged,
    /// `set_sidechain_ignore` left a new list in [`Requests`].
    IgnoreChanged,
    DefaultOutputChanged,
    DevicesChanged,
    ProcessesChanged,
    /// The output device or Sidestage's aggregate died or changed sample rate.
    EngineDeviceChanged,
}

/// What Sidestage knows about the audio-capture permission, shared by the platform and its
/// sessions. There is no public API to ask macOS, so this is learned from starts.
#[derive(Debug, Default)]
pub struct Permission(AtomicU8);

impl Permission {
    const GRANTED: u8 = 1;
    const DENIED: u8 = 2;
    /// The consent prompt went unanswered; Core Audio refuses captures until Sidestage restarts.
    const TIMED_OUT: u8 = 3;

    pub fn denied(&self) -> bool {
        self.0.load(Ordering::Relaxed) == Self::DENIED
    }

    pub fn timed_out(&self) -> bool {
        self.0.load(Ordering::Relaxed) == Self::TIMED_OUT
    }

    fn granted(&self) -> bool {
        self.0.load(Ordering::Relaxed) == Self::GRANTED
    }

    fn set_granted(&self) {
        self.0.store(Self::GRANTED, Ordering::Relaxed);
    }

    fn set_denied(&self) {
        self.0.store(Self::DENIED, Ordering::Relaxed);
    }

    fn set_timed_out(&self) {
        self.0.store(Self::TIMED_OUT, Ordering::Relaxed);
    }
}

/// What the control thread last published.
#[derive(Clone, Debug, Default)]
struct Status {
    state: SessionState,
    output_name: String,
    sample_rate: u32,
    latency_ms: f32,
    sidechain_active: bool,
    warnings: Vec<String>,
    /// Since when the control thread has been in a call that can wait for the capture prompt.
    prompt_since: Option<Instant>,
    sources: Vec<SourceEntry>,
    /// How many `set_sources` lists `sources` reflects.
    lists_applied: u64,
}

/// One source as last published, with its identity so `status` can match it to the list the
/// app asked for most recently.
#[derive(Clone, Debug)]
struct SourceEntry {
    id: SourceId,
    app: AppKey,
    hide: HideMethod,
    app_name: String,
    state: SessionState,
    original_hidden: bool,
    warnings: Vec<String>,
    meters: Arc<SourceMeters>,
}

/// Requests from the app that the control thread has not taken yet.
#[derive(Default)]
struct Requests {
    /// Every `set_sources` list in order. Each one is a change against the one before: the app
    /// restarts a source by removing it in one call and adding it back in the next, which must
    /// not collapse into "unchanged". Applying a list is only bookkeeping, so the taps follow
    /// the net result in one rebuild.
    sources: Vec<Vec<SourceConfig>>,
    /// The latest ignore list (only the newest matters).
    ignore: Option<Vec<AppKey>>,
}

struct Shared {
    status: Mutex<Status>,
    requests: Mutex<Requests>,
    /// Output counters, kept across engines.
    session: Arc<SessionMeters>,
    sidechain: Arc<SidechainLevel>,
    permission: Arc<Permission>,
    stop: AtomicBool,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap_or_else(PoisonError::into_inner));
    }

    fn requests(&self) -> MutexGuard<'_, Requests> {
        self.requests.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct MacSession {
    shared: Arc<Shared>,
    tx: Sender<Event>,
    done: Receiver<anyhow::Result<()>>,
    thread: Option<JoinHandle<()>>,
    /// The sources last asked for, in order. `status` lists them so, also before the control
    /// thread has caught up with a change.
    requested: Vec<Requested>,
    /// `set_sources` calls so far.
    lists_sent: u64,
}

/// One source the app asked for.
struct Requested {
    config: SourceConfig,
    /// See [`sources::requested_since`]: until the control thread has applied that call, a
    /// published entry with the same identity (say, the error that made the app restart the
    /// source) is not this source's.
    since: u64,
}

impl MacSession {
    /// Returns at once; the control thread does all Core Audio work. `running` is cleared when
    /// that thread has finished, and with it all of its Core Audio objects.
    pub fn start(
        config: SessionConfig,
        permission: Arc<Permission>,
        me: SelfInfo,
        os: Option<OsVersion>,
        running: Arc<AtomicBool>,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared {
            status: Mutex::new(Status::default()),
            requests: Mutex::new(Requests::default()),
            session: Arc::new(SessionMeters::default()),
            sidechain: Arc::new(SidechainLevel::default()),
            permission,
            stop: AtomicBool::new(false),
        });
        let requested = config.sources.iter().map(|c| Requested { config: c.clone(), since: 0 }).collect();
        let (tx, rx) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread_tx = tx.clone();
        let thread = std::thread::Builder::new()
            .name("sidestage-coreaudio".into())
            .spawn(move || {
                let status = thread_shared.clone();
                let result =
                    catch_unwind(AssertUnwindSafe(|| Control::new(config, me, os, thread_shared, thread_tx, rx).run()))
                        .unwrap_or_else(|_| {
                            // Unwinding dropped the engines, and with the taps the muting.
                            status.update(|s| {
                                s.state = SessionState::Error(
                                    "Sidestage's audio thread hit an internal error. Start again; if it keeps \
                                     happening, please report it."
                                        .to_string(),
                                );
                                for e in &mut s.sources {
                                    e.state = SessionState::Stopped;
                                    e.original_hidden = false;
                                }
                            });
                            Err(anyhow!("the macOS audio thread panicked"))
                        });
                running.store(false, Ordering::Release);
                let _ = done_tx.send(result);
            })
            .context("starting the audio control thread")?;
        Ok(Self { shared, tx, done, thread: Some(thread), requested, lists_sent: 0 })
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Event::Stop);
        match self.done.recv_timeout(STOP_TIMEOUT) {
            Ok(result) => {
                let _ = thread.join();
                result
            }
            Err(RecvTimeoutError::Timeout) => {
                // Almost certainly blocked in Core Audio on an unanswered capture prompt. The
                // thread owns everything it created and cleans up when that call returns.
                log::warn!("the audio thread is still busy; it will clean up when Core Audio returns");
                Ok(())
            }
            Err(RecvTimeoutError::Disconnected) => match thread.join() {
                Ok(()) => Ok(()),
                Err(_) => anyhow::bail!("the macOS audio thread crashed"),
            },
        }
    }
}

impl Session for MacSession {
    fn status(&self) -> SessionStatus {
        let s = self.shared.status.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let mut warnings = s.warnings;
        // The control thread is stuck in Core Audio, so the hint is added here.
        if !self.shared.permission.granted() && s.prompt_since.is_some_and(|t| t.elapsed() >= PROMPT_HINT_AFTER) {
            warnings.insert(0, PROMPT_HINT.to_string());
        }
        let ended = matches!(s.state, SessionState::Error(_) | SessionState::Stopped);
        let sources = self
            .requested
            .iter()
            .map(|Requested { config: r, since }| {
                let entry = s.sources.iter().find(|e| sources::same_source(e.id, &e.app, &e.hide, r));
                (r, entry, s.lists_applied >= *since)
            })
            .map(|(r, entry, applied)| match entry {
                Some(e) if applied => SourceStatus {
                    id: e.id,
                    app_name: e.app_name.clone(),
                    state: e.state.clone(),
                    original_hidden: e.original_hidden,
                    capture_peak: e.meters.capture.take(),
                    // One clock and no bridge: a source adds exactly the output's latency.
                    latency_ms: if e.state == SessionState::Running { s.latency_ms } else { 0.0 },
                    dropouts: e.meters.gaps.load(Ordering::Relaxed),
                    drift_ppm: 0.0,
                    warnings: e.warnings.clone(),
                },
                // Asked for a moment ago (or removed and added back); the control thread has
                // not got to it yet.
                _ => SourceStatus {
                    id: r.id,
                    app_name: entry.map_or_else(|| sources::provisional_name(&r.app.0), |e| e.app_name.clone()),
                    state: if ended { SessionState::Stopped } else { SessionState::Starting },
                    ..SourceStatus::default()
                },
            })
            .collect();
        SessionStatus {
            state: s.state,
            output_name: s.output_name,
            sample_rate: s.sample_rate,
            sidechain_peak: self.shared.sidechain.meter.take(),
            sidechain_active: s.sidechain_active,
            latency_ms: s.latency_ms,
            dropouts: self.shared.session.dropouts.load(Ordering::Relaxed),
            warnings,
            sources,
        }
    }

    fn set_sources(&mut self, sources: Vec<SourceConfig>) -> anyhow::Result<()> {
        if let Some(e) = SessionConfig::duplicate_error(&sources) {
            anyhow::bail!(e);
        }
        self.lists_sent += 1;
        let previous: Vec<(&SourceConfig, u64)> = self.requested.iter().map(|r| (&r.config, r.since)).collect();
        let since = sources::requested_since(&previous, &sources, self.lists_sent);
        self.requested =
            sources.iter().zip(since).map(|(config, since)| Requested { config: config.clone(), since }).collect();
        self.shared.requests().sources.push(sources);
        // A control thread that has ended (stopped or failed) says so in the status.
        let _ = self.tx.send(Event::SourcesChanged);
        Ok(())
    }

    fn set_sidechain_ignore(&mut self, apps: Vec<AppKey>) -> anyhow::Result<()> {
        self.shared.requests().ignore = Some(apps);
        let _ = self.tx.send(Event::IgnoreChanged);
        Ok(())
    }

    fn stop(mut self: Box<Self>) -> anyhow::Result<()> {
        self.shutdown()
    }
}

impl Drop for MacSession {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            log::warn!("stopping the macOS session: {e:#}");
        }
    }
}

/// One source as the control thread tracks it.
struct Source {
    /// Unique for the session's lifetime; a changed source gets a new one (see
    /// [`LaneNeed::key`]).
    key: u64,
    config: SourceConfig,
    name: String,
    /// Shared with every engine that renders the source, so they survive rebuilds.
    meters: Arc<SourceMeters>,
    /// The processor, built for `dsp.rate()`, handed from engine to engine.
    dsp: Option<Arc<DspCell>>,
    /// The app with its audio processes, or `None` while it has none.
    group: Option<AppGroup>,
    /// The tap kind last asked for, kept across rebuilds for hysteresis.
    scope: Option<TapScope>,
    /// Its tap should be rebuilt (another scope, or its process list cannot be updated).
    rescope: bool,
    /// Consecutive failed attempts to tap it, and when to try next.
    failures: u32,
    ready_at: Instant,
    error: Option<String>,
    /// Given up after too many failures; shown as its error until the app re-adds it.
    gave_up: Option<String>,
    tap_update_failures: u32,
    /// Since when its app has been playing without a break, for the silence heuristic.
    playing_since: Option<Instant>,
}

impl Source {
    fn new(key: u64, config: SourceConfig, now: Instant) -> Self {
        Self {
            key,
            name: sources::provisional_name(&config.app.0),
            config,
            meters: Arc::new(SourceMeters::default()),
            dsp: None,
            group: None,
            scope: None,
            rescope: false,
            failures: 0,
            ready_at: now,
            error: None,
            gave_up: None,
            tap_update_failures: 0,
            playing_since: None,
        }
    }

    /// `ParkOn` is a Windows method; macOS mutes the original instead (with a warning).
    fn mute(&self) -> bool {
        !matches!(self.config.hide, HideMethod::KeepOriginal)
    }
}

/// What one lane of the next engine is built from.
struct LanePlan {
    key: u64,
    name: String,
    objects: Vec<AudioObjectID>,
    scope: TapScope,
    plays_only_to_output: bool,
    mute: bool,
}

/// The control thread's state. Field order is drop order: listeners go before the engines
/// they watch, the sidechain before the render engine.
struct Control {
    engine_listeners: Vec<Listener>,
    system_listeners: Vec<Listener>,
    sidechain: Option<SidechainEngine>,
    engine: Option<RenderEngine>,
    queue: DispatchRetained<DispatchQueue>,
    output_pref: Option<String>,
    sidechain_on: bool,
    ignore: Vec<AppKey>,
    sources: Vec<Source>,
    master_factory: ProcessorFactory,
    /// The master processor, built for `master.rate()`, handed from engine to engine.
    master: Option<Arc<DspCell>>,
    handover: Arc<Handover>,
    generation: u32,
    next_key: u64,
    shared: Arc<Shared>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    me: SelfInfo,
    os: Option<OsVersion>,
    lookup: SysLookup,
    /// Every app from the last process scan, for the sidechain's ignore list.
    groups: Vec<AppGroup>,
    /// The permission probe failed on a device-scoped tap: use mixdowns (the case the probe is
    /// proven on) for the rest of the session.
    mixdown_only: bool,
    /// Consecutive builds whose permission probe failed.
    probe_failures: u32,
    /// A rebuild for the output's sake (device, rate, default output) is due at `next_attempt`.
    dirty: bool,
    /// Unrecoverable: the session's error, shown until it is stopped.
    fatal: Option<String>,
    /// Consecutive failed builds of the session's device, and when to try next.
    failures: u32,
    next_attempt: Instant,
    next_poll: Instant,
    next_sidechain_attempt: Instant,
    last_error: Option<String>,
    output_note: Option<String>,
    sidechain_note: Option<String>,
    /// The render engine's IO cycle count at the last poll, and how many polls it stood still.
    last_cycles: u64,
    stalled_polls: u32,
    layout_errors_seen: u64,
    /// Since when no source of the render engine has had an app with audio processes.
    idle_since: Option<Instant>,
    /// Problems from closing engines, reported by `stop`.
    teardown_errors: Vec<String>,
    /// `set_sources` lists applied so far, published with the sources' states.
    lists_applied: u64,
}

impl Control {
    fn new(
        config: SessionConfig,
        me: SelfInfo,
        os: Option<OsVersion>,
        shared: Arc<Shared>,
        tx: Sender<Event>,
        rx: Receiver<Event>,
    ) -> Self {
        let now = Instant::now();
        let SessionConfig { output, sidechain, sidechain_ignore, sources, master } = config;
        let count = sources.len() as u64;
        let sources = sources.into_iter().zip(0..).map(|(config, key)| Source::new(key, config, now)).collect();
        Self {
            engine_listeners: Vec::new(),
            system_listeners: Vec::new(),
            sidechain: None,
            engine: None,
            queue: ca::listener_queue(),
            output_pref: output,
            sidechain_on: sidechain,
            ignore: sidechain_ignore,
            sources,
            master_factory: master,
            master: None,
            handover: Arc::new(Handover::default()),
            generation: IDLE,
            next_key: count,
            shared,
            tx,
            rx,
            me,
            os,
            lookup: SysLookup::default(),
            groups: Vec::new(),
            mixdown_only: false,
            probe_failures: 0,
            dirty: false,
            fatal: None,
            failures: 0,
            next_attempt: now,
            next_poll: now + POLL,
            next_sidechain_attempt: now,
            last_error: None,
            output_note: None,
            sidechain_note: None,
            last_cycles: 0,
            stalled_polls: 0,
            layout_errors_seen: 0,
            idle_since: None,
            teardown_errors: Vec::new(),
            lists_applied: 0,
        }
    }

    fn run(mut self) -> anyhow::Result<()> {
        self.listen_system();
        autoreleasepool(|_| {
            self.scan_logged();
            self.publish();
        });
        'run: loop {
            if self.shared.stop.load(Ordering::Relaxed) {
                break;
            }
            let now = Instant::now();
            if self.rebuild_due(now).is_some_and(|due| now >= due) {
                autoreleasepool(|_| self.rebuild());
            }
            if now >= self.next_poll {
                self.next_poll = now + POLL;
                autoreleasepool(|_| self.poll());
            }
            let now = Instant::now();
            let wake = self.rebuild_due(now).map_or(self.next_poll, |due| due.min(self.next_poll));
            let timeout = wake.saturating_duration_since(now).max(Duration::from_millis(10));
            match self.rx.recv_timeout(timeout) {
                Ok(first) => {
                    // A burst (a browser opening tabs changes the process list many times) is
                    // handled once per kind of event.
                    let mut events = vec![first];
                    for event in self.rx.try_iter() {
                        if !events.contains(&event) {
                            events.push(event);
                        }
                    }
                    if events.contains(&Event::Stop) {
                        break 'run;
                    }
                    for event in events {
                        autoreleasepool(|_| self.handle(event));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        self.drop_engines();
        self.system_listeners.clear();
        self.shared.update(|s| {
            s.state = SessionState::Stopped;
            for e in &mut s.sources {
                e.state = SessionState::Stopped;
                e.original_hidden = false;
            }
        });
        match self.teardown_errors.first() {
            None => Ok(()),
            Some(first) => anyhow::bail!("stopping Sidestage's audio: {first}"),
        }
    }

    fn listen(&self, object: AudioObjectID, selector: u32, event: Event) -> Option<Listener> {
        let tx = self.tx.clone();
        match Listener::add(object, selector, kAudioObjectPropertyScopeGlobal, &self.queue, move || {
            let _ = tx.send(event);
        }) {
            Ok(listener) => Some(listener),
            Err(e) => {
                // The one-second poll still notices the change, just later.
                log::warn!("{e:#}");
                None
            }
        }
    }

    fn listen_system(&mut self) {
        let listeners: [(u32, Event); SYSTEM_LISTENERS] = [
            (kAudioHardwarePropertyDefaultOutputDevice, Event::DefaultOutputChanged),
            (kAudioHardwarePropertyDevices, Event::DevicesChanged),
            (kAudioHardwarePropertyProcessObjectList, Event::ProcessesChanged),
        ];
        for (selector, event) in listeners {
            if let Some(l) = self.listen(SYSTEM_OBJECT, selector, event) {
                self.system_listeners.push(l);
            }
        }
    }

    fn listen_engine(&mut self, aggregate: AudioObjectID, output: AudioObjectID) {
        for object in [aggregate, output] {
            for selector in [kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyNominalSampleRate] {
                if let Some(l) = self.listen(object, selector, Event::EngineDeviceChanged) {
                    self.engine_listeners.push(l);
                }
            }
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Stop => {}
            Event::SourcesChanged => self.apply_sources(),
            Event::IgnoreChanged => self.apply_ignore(),
            _ if self.fatal.is_some() => {}
            Event::DefaultOutputChanged => {
                if self.follows_default() && self.default_output_moved() {
                    log::info!("default output changed");
                    self.request_rebuild();
                }
            }
            Event::DevicesChanged => {
                if let Some(uid) = self.output_pref.as_deref() {
                    let connected = devices::device_for_uid(uid).is_some();
                    let using_it = self.output_note.is_none() && self.engine.is_some();
                    let substituting = self.output_note.is_some();
                    if (connected && substituting) || (!connected && using_it) {
                        log::info!("chosen output {}", if connected { "reconnected" } else { "disconnected" });
                        self.request_rebuild();
                    }
                }
            }
            Event::ProcessesChanged => {
                self.scan_logged();
                self.publish();
            }
            Event::EngineDeviceChanged => self.check_engine_devices(),
        }
    }

    /// Applies the queued source lists in order and looks for the new sources' apps.
    fn apply_sources(&mut self) {
        let lists = std::mem::take(&mut self.shared.requests().sources);
        if lists.is_empty() {
            return;
        }
        let mut changed = false;
        self.lists_applied += lists.len() as u64;
        for list in lists {
            changed |= self.apply_source_list(list);
        }
        if changed {
            // A changed list deserves a prompt attempt, also after earlier failures: a removed
            // app must be heard again soon.
            self.failures = 0;
            self.next_attempt = Instant::now();
            self.last_error = None;
        }
        self.scan_logged();
        self.publish();
    }

    /// Unchanged sources keep their key, processor, meters and tap; the rest are new. Removed
    /// sources' taps go with the next engine, which is due at once (see
    /// [`sources::rebuild_due`]). Returns whether anything was added or removed.
    fn apply_source_list(&mut self, list: Vec<SourceConfig>) -> bool {
        let old: Vec<&SourceConfig> = self.sources.iter().map(|s| &s.config).collect();
        let diff = sources::diff(&old, &list);
        let mut previous: Vec<Option<Source>> = std::mem::take(&mut self.sources).into_iter().map(Some).collect();
        for &index in &diff.removed {
            if let Some(s) = previous.get(index).and_then(Option::as_ref) {
                log::info!("removing source {} ({})", s.config.id, s.name);
            }
        }
        let now = Instant::now();
        for (index, config) in list.into_iter().enumerate() {
            let kept = diff.kept.iter().find(|(_, n)| *n == index).and_then(|&(o, _)| previous.get_mut(o)?.take());
            match kept {
                Some(mut s) => {
                    // The new factory only matters if the processor ever has to be rebuilt for
                    // another sample rate.
                    s.config = config;
                    self.sources.push(s);
                }
                None => {
                    log::info!("adding source {} ({})", config.id, config.app.0);
                    let key = self.next_key;
                    self.next_key += 1;
                    self.sources.push(Source::new(key, config, now));
                }
            }
        }
        !diff.added.is_empty() || !diff.removed.is_empty()
    }

    fn apply_ignore(&mut self) {
        let Some(apps) = self.shared.requests().ignore.take() else {
            return;
        };
        log::info!("ducking now ignores {} app(s)", apps.len());
        self.ignore = apps;
        self.update_sidechain();
    }

    /// Playing to the system default output: none was chosen, or the chosen one is missing.
    fn follows_default(&self) -> bool {
        self.output_pref.is_none() || self.output_note.is_some()
    }

    fn default_output_moved(&self) -> bool {
        plan::default_output_moved(self.engine.as_ref().map(|e| e.output.id), devices::default_output().ok())
    }

    fn request_rebuild(&mut self) {
        if !self.dirty && self.failures == 0 {
            self.next_attempt = Instant::now();
        }
        self.dirty = true;
    }

    fn attached(&self, key: u64) -> bool {
        self.engine.as_ref().is_some_and(|e| e.lane(key).is_some())
    }

    fn needs(&self) -> Vec<LaneNeed> {
        self.sources
            .iter()
            .map(|s| LaneNeed {
                key: s.key,
                alive: s.group.is_some(),
                failed: s.gave_up.is_some(),
                attached: self.attached(s.key),
                rescope: s.rescope,
                ready_at: s.ready_at,
            })
            .collect()
    }

    /// The running engine's taps that the rebuild decisions reckon with
    /// ([`sources::binding_lanes`]).
    fn engine_lanes(&self, needs: &[LaneNeed]) -> Vec<u64> {
        let Some(engine) = &self.engine else {
            return Vec::new();
        };
        let running = |o: &AudioObjectID| self.groups.iter().any(|g| g.objects.contains(o));
        let lanes: Vec<(u64, bool)> = engine.lanes().iter().map(|l| (l.key, l.objects.iter().any(running))).collect();
        sources::binding_lanes(needs, &lanes)
    }

    /// Whether the running engine taps a source that was removed or given up on, and whose app
    /// it therefore keeps muted.
    fn engine_is_stale(&self) -> bool {
        let needs = self.needs();
        !sources::stale_lanes(&needs, &self.engine_lanes(&needs)).is_empty()
    }

    /// When the next engine should be built, if one is needed.
    fn rebuild_due(&self, now: Instant) -> Option<Instant> {
        if self.fatal.is_some() {
            return None;
        }
        let needs = self.needs();
        let mut due = sources::rebuild_due(&needs, &self.engine_lanes(&needs), now);
        if self.dirty {
            due = Some(due.map_or(now, |d| d.min(now)));
        }
        due.map(|d| d.max(self.next_attempt))
    }

    fn scan_logged(&mut self) {
        if let Err(e) = self.scan() {
            log::warn!("{e:#}");
        }
    }

    /// Finds every source's app and its processes, keeps the taps' process lists equal to them
    /// and updates the sidechain's exclusions.
    fn scan(&mut self) -> anyhow::Result<()> {
        let groups = procs::app_groups(&self.me, &mut self.lookup)?;
        for s in &mut self.sources {
            let group =
                groups.iter().find(|g| !g.objects.is_empty() && s.config.app.same_app(&AppKey(g.key.clone()))).cloned();
            match (&s.group, &group) {
                (None, Some(g)) => log::info!("{} has {} audio process(es)", g.name, g.objects.len()),
                (Some(_), None) => {
                    log::info!("{} has no audio processes any more; waiting for it", s.name);
                    // Waiting is not a failure: earlier failed attempts must not shorten the
                    // next start (a probe that failed as the app quit says nothing about the
                    // permission either).
                    s.failures = 0;
                    s.error = None;
                    self.probe_failures = 0;
                }
                _ => {}
            }
            if let Some(g) = &group {
                s.name = g.name.clone();
            }
            s.group = group;
        }
        self.groups = groups;
        self.follow_taps();
        self.update_sidechain();
        Ok(())
    }

    /// Keeps each tap's process list equal to its app's process objects, without a rebuild.
    /// A tap whose app went away keeps its old list: it captures silence until the app comes
    /// back and its new processes are written in.
    fn follow_taps(&mut self) {
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        for s in &mut self.sources {
            let Some(group) = &s.group else {
                continue;
            };
            if !engine.lane(s.key).is_some_and(|lane| lane.objects != group.objects) {
                continue;
            }
            log::info!("{} now has {} audio process(es); updating its tap", group.name, group.objects.len());
            match engine.set_processes(s.key, &group.objects) {
                Ok(()) => s.tap_update_failures = 0,
                Err(e) => {
                    s.tap_update_failures += 1;
                    log::warn!("{e:#} (attempt {})", s.tap_update_failures);
                    if s.tap_update_failures >= TAP_UPDATE_RETRIES {
                        s.tap_update_failures = 0;
                        s.rescope = true;
                    }
                }
            }
        }
    }

    /// The process objects the sidechain must not hear besides Sidestage itself: every
    /// source's app (whatever its state) and every ignored app that is running.
    fn sidechain_apps(&self) -> Vec<AudioObjectID> {
        let mut apps: Vec<AudioObjectID> =
            self.sources.iter().filter_map(|s| s.group.as_ref()).flat_map(|g| g.objects.iter().copied()).collect();
        for key in &self.ignore {
            for g in self.groups.iter().filter(|g| key.same_app(&AppKey(g.key.clone()))) {
                apps.extend_from_slice(&g.objects);
            }
        }
        apps
    }

    fn update_sidechain(&mut self) {
        let apps = self.sidechain_apps();
        let Some(sidechain) = self.sidechain.as_mut() else {
            return;
        };
        if let Err(e) = sidechain.set_apps(&apps) {
            log::warn!("{e:#}; rebuilding the sidechain");
            self.close_sidechain();
            self.next_sidechain_attempt = Instant::now();
        }
    }

    /// Rebuilds when the engine's devices die or change rate. A running aggregate cannot be
    /// re-rated in place (its IOProc goes silent, FineTune found), and the processors were
    /// built for the old rate.
    fn check_engine_devices(&mut self) {
        let follows_default = self.follows_default();
        let Some(engine) = &mut self.engine else {
            return;
        };
        // A mixdown tap follows the device its app plays to. A rebuild cannot fix a rate it
        // does not match (macOS 14 does not resample taps; 15 does), so the new rate only
        // updates the warning. A device-scoped tap changes rate with its device, below.
        for lane in engine.lanes_mut() {
            if let Some(rate) = lane.current_tap_rate()
                && plan::rate_changed(lane.tap_rate, rate)
            {
                log::info!("a source's audio switched from {} Hz to {rate} Hz", lane.tap_rate);
                lane.tap_rate = rate;
            }
        }
        // Each rate is compared with its own value at build time, so a device that settles on
        // a rate other than the aggregate's cannot cause a rebuild loop.
        let reason = if !devices::is_alive(engine.output.id) {
            Some(format!("{} went away", engine.output.name))
        } else if !devices::is_alive(engine.aggregate_id()) {
            Some("Sidestage's device stopped".to_string())
        } else if let Ok(rate) = devices::nominal_rate(engine.aggregate_id())
            && plan::rate_changed(engine.nominal_rate, rate)
        {
            Some(format!("Sidestage's device switched to {rate} Hz"))
        } else if let Some(built) = engine.output_rate
            && let Ok(rate) = devices::nominal_rate(engine.output.id)
            && plan::rate_changed(built, rate)
        {
            Some(format!("{} switched to {rate} Hz", engine.output.name))
        } else if follows_default && plan::default_output_moved(Some(engine.output.id), devices::default_output().ok())
        {
            Some("the default output changed".to_string())
        } else {
            None
        };
        if let Some(reason) = reason {
            log::info!("{reason}; rebuilding");
            self.request_rebuild();
        }
    }

    /// Notices an engine whose IO stopped (or never started) and taps whose streams changed.
    fn check_io(&mut self) {
        let Some(engine) = &self.engine else {
            return;
        };
        let cycles = engine.meters.cycles.load(Ordering::Relaxed);
        let layout_errors = engine.meters.layout_errors.load(Ordering::Relaxed);
        self.stalled_polls = if cycles == self.last_cycles { self.stalled_polls + 1 } else { 0 };
        self.last_cycles = cycles;
        if layout_errors > self.layout_errors_seen {
            self.layout_errors_seen = layout_errors;
            log::warn!("the taps' audio no longer arrives the way Sidestage's device was built for; rebuilding");
            self.request_rebuild();
        }
        if self.stalled_polls >= STALLED_POLLS {
            // Its taps are not being read, so its apps are most likely audible already; a new
            // engine is built from scratch.
            self.stalled_polls = 0;
            log::warn!("Sidestage's device has stopped running; closing it");
            self.drop_engines();
            self.fail(anyhow!("Sidestage's device stopped running"), &[]);
        }
    }

    /// Switches a source's tap between device scope and mixdown when its app moves between
    /// the output and other devices.
    fn check_scopes(&mut self) {
        if self.mixdown_only {
            return;
        }
        let Some(engine) = &self.engine else {
            return;
        };
        for s in &mut self.sources {
            let (Some(group), Some(lane)) = (&s.group, engine.lane(s.key)) else {
                continue;
            };
            if s.rescope {
                continue;
            }
            let (to_output, elsewhere) = procs::device_usage(&group.objects, engine.output.id);
            let wanted = plan::choose_scope(to_output, elsewhere, Some(lane.wanted_scope), false);
            if wanted != lane.wanted_scope {
                log::info!(
                    "{} now plays {}; switching to a {wanted:?} tap",
                    group.name,
                    if to_output { "to the output" } else { "elsewhere" }
                );
                s.scope = Some(wanted);
                s.rescope = true;
            }
        }
    }

    /// Builds the engine the sources need now and switches to it (see `engine.rs`). Unless the
    /// running engine has to go anyway, a new one that gains nothing or loses a playing source
    /// is dropped before it makes a sound ([`sources::switch_verdict`]).
    fn rebuild(&mut self) {
        // The running engine must go whatever the new one ends up with: the output or its rate
        // changed (or a build is being retried), or it taps a source that must be heard again.
        let forced = self.dirty || self.engine_is_stale();
        self.dirty = false;
        if let Err(e) = self.scan() {
            self.fail(e.context("listing audio apps"), &[]);
            return self.publish();
        }
        let now = Instant::now();
        let keys = sources::lanes_for_build(&self.needs(), now);
        // This attempt answers every pending scope change; the poll asks again if it fails.
        let rescoped: Vec<u64> =
            self.sources.iter().filter(|s| s.rescope && keys.contains(&s.key)).map(|s| s.key).collect();
        for s in self.sources.iter_mut().filter(|s| keys.contains(&s.key)) {
            s.rescope = false;
        }
        if keys.is_empty() {
            if self.engine.is_some() {
                log::info!("no source to play; closing Sidestage's device");
            }
            self.drop_engines();
            return self.publish();
        }
        let (output, note) = match devices::resolve(self.output_pref.as_deref()) {
            Ok(r) => r,
            Err(e) => {
                self.fail(e.context("finding the output device"), &keys);
                return self.publish();
            }
        };
        let previous_note = std::mem::replace(&mut self.output_note, note);
        let is_default = devices::default_output().is_ok_and(|id| id == output.id);
        let plans: Vec<LanePlan> = keys
            .iter()
            .filter_map(|key| self.sources.iter().find(|s| s.key == *key))
            .filter_map(|s| {
                let group = s.group.as_ref()?;
                let (to_output, elsewhere) = procs::device_usage(&group.objects, output.id);
                let scope = if self.mixdown_only {
                    TapScope::Mixdown
                } else {
                    plan::choose_scope(to_output, elsewhere, s.scope, is_default)
                };
                Some(LanePlan {
                    key: s.key,
                    name: s.name.clone(),
                    objects: group.objects.clone(),
                    scope,
                    plays_only_to_output: to_output && !elsewhere,
                    mute: s.mute(),
                })
            })
            .collect();
        let specs: Vec<LaneSpec<'_>> = plans
            .iter()
            .map(|p| LaneSpec {
                key: p.key,
                name: &p.name,
                objects: &p.objects,
                scope: p.scope,
                plays_only_to_output: p.plays_only_to_output,
                mute: p.mute,
            })
            .collect();
        log::info!(
            "building Sidestage's device for {} with {} source(s): {}",
            output.name,
            specs.len(),
            plans.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        );

        let mut failed = Vec::new();
        let prepared = RenderEngine::prepare(&specs, &output, &self.shared.stop, &mut failed);
        for (key, e) in failed {
            self.source_failed(key, e);
        }
        let prepared = match prepared {
            Ok(p) => p,
            Err(e) => return self.build_failed(e, &keys),
        };
        // A source that could not be tapped must not cost the others anything: no dip from a
        // switch that gains nothing, and no playing source left unprocessed because its new
        // tap failed. The prepared engine has no IOProc yet, so dropping it is silent.
        let verdict = match &self.engine {
            Some(engine) if !forced => {
                sources::switch_verdict(&keys, &engine.lane_keys(), &prepared.keys(), !rescoped.is_empty())
            }
            _ => sources::Switch::Go,
        };
        match verdict {
            sources::Switch::Go => {}
            sources::Switch::NothingGained => {
                log::info!("no source could be added; Sidestage's device keeps playing as it was");
                drop(prepared);
                self.output_note = previous_note;
                return self.publish();
            }
            sources::Switch::WouldDrop(lost) => {
                let worst = self.sources.iter().filter(|s| lost.contains(&s.key)).map(|s| s.failures).max();
                let worst = worst.unwrap_or(MAX_FAILURES);
                if worst < MAX_FAILURES {
                    // Most likely a process that exited mid-build: try again after a while.
                    log::warn!(
                        "{} playing source(s) could not be tapped again; keeping the running device",
                        lost.len()
                    );
                    drop(prepared);
                    self.output_note = previous_note;
                    self.next_attempt = Instant::now() + plan::retry_delay(worst.max(1));
                    for s in self.sources.iter_mut().filter(|s| rescoped.contains(&s.key)) {
                        s.rescope = true;
                    }
                    return self.publish();
                }
                // Their taps keep failing: switch without them. They are then tried again (and
                // given up on) like any source that has no tap.
                log::warn!("{} source(s) could not be tapped {MAX_FAILURES} times; switching without them", lost.len());
            }
        }

        // Processors for the device's rate. A kept source keeps its own (and with it its
        // reverb tail and smoothing state) unless the rate changed.
        let rate = prepared.sample_rate;
        let mut dsp = Vec::new();
        for key in prepared.keys() {
            let Some(s) = self.sources.iter().find(|s| s.key == key) else {
                return self.build_failed(anyhow!("a source vanished during the build").into(), &keys);
            };
            let cell = match &s.dsp {
                Some(cell) if cell.rate() == rate => cell.clone(),
                _ => Arc::new(DspCell::new((s.config.processor)(rate), rate)),
            };
            dsp.push((key, cell, s.meters.clone()));
        }
        let master = match &self.master {
            Some(master) if master.rate() == rate => master.clone(),
            _ => Arc::new(DspCell::new((self.master_factory)(rate), rate)),
        };

        self.generation = next_generation(self.generation);
        let shared = self.shared.clone();
        let prompt_window = move |open: bool| {
            shared.update(|s| s.prompt_since = open.then(Instant::now));
        };
        let spec = StartSpec {
            generation: self.generation,
            handover: self.handover.clone(),
            dsp: dsp.iter().map(|(_, cell, meters)| (cell.clone(), meters.clone())).collect(),
            master: master.clone(),
            session: self.shared.session.clone(),
            sidechain: self.sidechain_on.then(|| self.shared.sidechain.clone()),
            activate: self.engine.is_none(),
            cancel: &self.shared.stop,
            prompt_window: &prompt_window,
        };
        let new = match prepared.start(spec) {
            Ok(engine) => engine,
            Err(e) => return self.build_failed(e, &keys),
        };

        // Only switch to an engine whose IO runs: until then its taps are not read, and the
        // kept apps would be heard as soon as the old engine's taps go.
        let running = new.wait_for_io(WARMUP_CYCLES, WARMUP_TIMEOUT, &self.shared.stop);
        if self.shared.stop.load(Ordering::Relaxed) {
            // `run` tears everything down.
            if self.engine.is_none() {
                self.engine = Some(new);
            } else {
                self.close_engine(new);
            }
            return;
        }
        match self.engine.take() {
            Some(old) if !running => {
                self.engine = Some(old);
                self.close_engine(new);
                return self.build_failed(anyhow!("the new device for {} did not start", output.name).into(), &keys);
            }
            Some(old) => {
                self.engine_listeners.clear();
                self.switch(old, &new);
            }
            None if !running => log::warn!("Sidestage's device for {} has not started running yet", output.name),
            None => {}
        }

        self.permission().set_granted();
        self.failures = 0;
        self.probe_failures = 0;
        self.last_error = None;
        self.master = Some(master);
        for (key, cell, _) in dsp {
            let wanted = new.lane(key).map(|lane| lane.wanted_scope);
            if let Some(s) = self.sources.iter_mut().find(|s| s.key == key) {
                s.dsp = Some(cell);
                s.failures = 0;
                s.error = None;
                s.tap_update_failures = 0;
                s.scope = wanted.or(s.scope);
            }
        }
        if self.sidechain.as_ref().is_some_and(|sc| sc.rate != new.sample_rate) {
            // Built for the old rate; `try_sidechain` builds it again.
            self.close_sidechain();
        }
        self.listen_engine(new.aggregate_id(), new.output.id);
        self.last_cycles = 0;
        self.stalled_polls = 0;
        self.layout_errors_seen = 0;
        self.engine = Some(new);
        self.update_sidechain();
        self.try_sidechain();
        self.publish();
    }

    /// Hands rendering from `old` to `new` (see `render.rs`): the old engine fades out over its
    /// next cycle and lets go of the processors, then the new one takes over and fades in.
    /// Only then are the old engine's taps destroyed; the new engine's keep the apps muted.
    fn switch(&mut self, old: RenderEngine, new: &RenderEngine) {
        self.handover.pause();
        if devices::is_alive(old.aggregate_id()) {
            let deadline = Instant::now() + plan::retire_timeout(old.buffer_frames, old.sample_rate);
            while !self.handover.has_retired(old.generation) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            if !self.handover.has_retired(old.generation) {
                // Taking a processor is atomic, so the engines can never run one at once; the
                // new one just skips a source whose processor the old one still holds.
                log::debug!("the previous engine did not let go in time");
            }
        }
        self.handover.activate(new.generation);
        log::info!("switched to the new device");
        self.close_engine(old);
    }

    fn permission(&self) -> &Permission {
        &self.shared.permission
    }

    fn build_failed(&mut self, e: BuildError, keys: &[u64]) {
        match e {
            BuildError::Cancelled => return,
            // Each source's failure is on record; a stale tap must still go.
            BuildError::NothingToTap => self.drop_stale_engine(),
            BuildError::ProbeFailed { error, device_scoped } => self.probe_failed(error, device_scoped, keys),
            BuildError::PromptTimedOut(e) => {
                log::warn!("the audio capture prompt timed out: {e:#}");
                // Reported through support() too, so the app does not retry into a Core Audio
                // that refuses until Sidestage restarts.
                self.permission().set_timed_out();
                self.give_up(PROMPT_TIMED_OUT.to_string());
            }
            BuildError::Failed(e) => self.fail(e, keys),
        }
        self.publish();
    }

    fn probe_failed(&mut self, error: anyhow::Error, device_scoped: bool, keys: &[u64]) {
        self.probe_failures += 1;
        log::warn!("audio capture permission probe failed (attempt {}): {error:#}", self.probe_failures);
        if device_scoped {
            // The probe is only proven on mixdown taps.
            self.mixdown_only = true;
        }
        if plan::probe_failure_is_denial(self.probe_failures, self.permission().granted()) {
            self.permission().set_denied();
            self.give_up(PERMISSION_DENIED.to_string());
        } else {
            self.fail(error.context("macOS did not confirm audio capture"), keys);
        }
    }

    /// Ends the session's attempts with `message` for the user; every original is audible
    /// again.
    fn give_up(&mut self, message: String) {
        log::warn!("giving up: {message}");
        self.fatal = Some(message);
        self.dirty = false;
        self.drop_engines();
    }

    /// A failed build of the session's device (`keys` are the sources it was for).
    fn fail(&mut self, e: anyhow::Error, keys: &[u64]) {
        self.failures += 1;
        let message = format!("{e:#}");
        log::warn!("building Sidestage's device failed (attempt {}): {message}", self.failures);
        if let Some(engine) = &self.engine
            && (!devices::is_alive(engine.output.id) || !devices::is_alive(engine.aggregate_id()))
        {
            // Nothing to keep playing: its device is gone (and with it the reading that keeps
            // its apps muted), so the status must not claim it still runs.
            log::info!("closing Sidestage's device for {}, which is gone", engine.output.name);
            self.drop_engines();
        }
        if self.failures >= MAX_FAILURES {
            let Some(engine) = &self.engine else {
                return self.give_up(format!("Sidestage could not start: {message}"));
            };
            // The sources that play are fine; give up on the ones that could not be added.
            let playing = engine.lane_keys();
            for s in self.sources.iter_mut().filter(|s| keys.contains(&s.key) && !playing.contains(&s.key)) {
                s.gave_up = Some(format!("Sidestage could not add {}: {message}", s.name));
            }
            self.failures = 0;
            self.last_error = None;
        } else {
            self.dirty = true;
            self.next_attempt = Instant::now() + plan::retry_delay(self.failures);
            self.last_error = Some(message);
        }
        self.drop_stale_engine();
    }

    /// A source that could not be tapped. It is tried again after a while, and given up after
    /// [`MAX_FAILURES`] attempts in a row, but never while it still plays from the running
    /// engine (`rebuild` then switches without it first).
    fn source_failed(&mut self, key: u64, e: anyhow::Error) {
        let playing = self.attached(key);
        let Some(s) = self.sources.iter_mut().find(|s| s.key == key) else {
            return;
        };
        s.failures += 1;
        let message = format!("{e:#}");
        if s.failures >= MAX_FAILURES && !playing {
            s.gave_up = Some(format!("Sidestage could not capture {}: {message}", s.name));
        } else {
            s.ready_at = Instant::now() + plan::retry_delay(s.failures);
            s.error = Some(message);
        }
    }

    /// After a failed build: if the running engine still taps a removed or given-up source,
    /// close it, so that app is heard again now rather than after the next successful build.
    fn drop_stale_engine(&mut self) {
        if !self.engine_is_stale() {
            return;
        }
        log::warn!("closing Sidestage's device so removed sources are heard again; the others follow when it rebuilds");
        self.drop_engines();
    }

    fn try_sidechain(&mut self) {
        if !self.sidechain_on || self.sidechain.is_some() || Instant::now() < self.next_sidechain_attempt {
            return;
        }
        let Some(rate) = self.engine.as_ref().map(|e| e.sample_rate) else {
            return;
        };
        // Without excluding itself the ducker would hear the sources. Sidestage is a HAL client
        // once its render device runs, so this resolves right after a build.
        let Some(own) = procs::process_object_for_pid(self.me.pid) else {
            self.sidechain_note = Some("Ducking starts in a moment.".to_string());
            self.next_sidechain_attempt = Instant::now() + Duration::from_secs(2);
            return;
        };
        let apps = self.sidechain_apps();
        match SidechainEngine::build(&apps, own, rate, self.shared.sidechain.clone(), &self.shared.stop) {
            Ok(sidechain) => {
                self.sidechain = Some(sidechain);
                self.sidechain_note = None;
            }
            Err(e) => {
                log::warn!("sidechain: {e:#}");
                self.sidechain_note = Some(format!("Ducking is unavailable: {e:#}"));
                self.next_sidechain_attempt = Instant::now() + Duration::from_secs(10);
            }
        }
    }

    fn close_sidechain(&mut self) {
        if let Some(sidechain) = self.sidechain.take()
            && let Err(e) = sidechain.close()
        {
            log::warn!("{e:#}");
            self.teardown_errors.push(format!("{e:#}"));
        }
    }

    fn close_engine(&mut self, engine: RenderEngine) {
        if let Err(e) = engine.close() {
            log::warn!("{e:#}");
            self.teardown_errors.push(format!("{e:#}"));
        }
    }

    /// Closes everything that captures; every original is audible again.
    fn drop_engines(&mut self) {
        self.engine_listeners.clear();
        self.close_sidechain();
        if let Some(engine) = self.engine.take() {
            self.close_engine(engine);
        }
    }

    /// Once a second: missed notifications, stalls, tap scopes, sidechain retries, status.
    fn poll(&mut self) {
        if self.fatal.is_some() {
            return;
        }
        self.scan_logged();
        self.check_engine_devices();
        self.check_io();
        self.check_scopes();
        self.check_idle(Instant::now());
        self.try_sidechain();
        self.publish();
    }

    /// Closes the render engine after [`IDLE_CLOSE`] without any of its sources' apps.
    fn check_idle(&mut self, now: Instant) {
        let Some(engine) = &self.engine else {
            self.idle_since = None;
            return;
        };
        if self.sources.iter().any(|s| s.group.is_some() && engine.lane(s.key).is_some()) {
            self.idle_since = None;
            return;
        }
        let since = *self.idle_since.get_or_insert(now);
        if now.duration_since(since) >= IDLE_CLOSE {
            log::info!("none of the sources' apps has audio any more; closing Sidestage's device");
            self.idle_since = None;
            self.drop_engines();
        }
    }

    /// Publishes the session's and every source's state for `status`.
    fn publish(&mut self) {
        let now = Instant::now();
        let over = self.fatal.is_some();
        let engine = self.engine.as_ref();
        let rate = f64::from(engine.map_or(0, |e| e.sample_rate).max(1));
        let mut entries = Vec::with_capacity(self.sources.len());
        for s in &mut self.sources {
            let lane = engine.and_then(|e| e.lane(s.key));
            let alive = s.group.is_some();
            let state = sources::source_state(over, s.gave_up.as_deref(), alive, lane.is_some());
            let mut warnings = Vec::new();
            if let HideMethod::ParkOn(_) = s.config.hide {
                warnings.push(
                    "Moving the app to another output is a Windows feature; macOS mutes the original instead."
                        .to_string(),
                );
            }
            if state == SessionState::Starting
                && let Some(error) = &s.error
            {
                warnings.push(format!("Retrying: {error}"));
            }
            let playing = state == SessionState::Running
                && s.group.as_ref().is_some_and(|g| g.objects.iter().any(|&o| procs::running_output(o)));
            s.playing_since = if playing { s.playing_since.or(Some(now)) } else { None };
            if let (Some(engine), Some(lane), true) = (engine, lane, state == SessionState::Running) {
                let silent = s.meters.silent_frames.load(Ordering::Relaxed) as f64 / rate;
                let playing_secs = s.playing_since.map_or(0.0, |t| now.duration_since(t).as_secs_f64());
                if plan::looks_like_denied_capture(playing_secs, silent) {
                    warnings.push(format!(
                        "Sidestage receives only silence from {} although it is playing. If you hear nothing, allow \
                         Sidestage in System Settings > Privacy & Security > Screen & System Audio Recording.",
                        s.name
                    ));
                }
                if lane.scope == TapScope::Mixdown
                    && self.os.is_some_and(|v| v < OsVersion::TAP_RESAMPLING)
                    && plan::rate_changed(engine.nominal_rate, lane.tap_rate)
                {
                    warnings.push(format!(
                        "macOS 14 cannot convert {}'s audio from {} Hz to {} Hz, so it may play at the wrong speed. \
                         Play {} to {} or update to macOS 15.",
                        s.name, lane.tap_rate, engine.sample_rate, s.name, engine.output.name
                    ));
                }
            }
            entries.push(SourceEntry {
                id: s.config.id,
                app: s.config.app.clone(),
                hide: s.config.hide.clone(),
                app_name: s.name.clone(),
                original_hidden: state == SessionState::Running && lane.is_some_and(|l| l.muted),
                state,
                warnings,
                meters: s.meters.clone(),
            });
        }

        let mut warnings = Vec::new();
        if let Some(note) = &self.output_note {
            warnings.push(note.clone());
        }
        if self.sidechain_on
            && engine.is_some()
            && let Some(note) = &self.sidechain_note
        {
            warnings.push(note.clone());
        }
        if !over && let Some(error) = &self.last_error {
            warnings.push(format!("Retrying: {error}"));
        }
        let latency_ms = engine.map_or(0.0, |e| {
            let measured = self.shared.session.latency_frames.load(Ordering::Relaxed);
            let frames = if measured > 0 { measured } else { e.fallback_latency_frames };
            (f64::from(frames) * 1000.0 / rate) as f32
        });
        let any_startable = self.sources.iter().any(|s| s.group.is_some() && s.gave_up.is_none());
        let state = sources::session_state(self.fatal.as_deref(), engine.is_some(), any_startable);
        let output_name = engine.map(|e| e.output.name.clone()).unwrap_or_default();
        let sample_rate = engine.map_or(0, |e| e.sample_rate);
        let sidechain_active = engine.is_some() && self.sidechain.is_some();
        let lists_applied = self.lists_applied;
        self.shared.update(|s| {
            s.state = state;
            s.output_name = output_name;
            s.sample_rate = sample_rate;
            s.latency_ms = latency_ms;
            s.sidechain_active = sidechain_active;
            s.warnings = warnings;
            s.sources = entries;
            s.lists_applied = lists_applied;
        });
    }
}
