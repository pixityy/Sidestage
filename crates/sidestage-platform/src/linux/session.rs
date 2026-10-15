//! A running Linux session.
//!
//! [`start`] creates the capture sink, the watchdog, the event watch and the audio engine on
//! the caller's thread (so failures are reported directly), then hands everything to a
//! supervisor thread. The supervisor keeps the music app on the capture sink (and everything
//! else off it), feeds the ducking meters, restarts the engine or recreates the sink when the
//! server pulls them away, and on stop moves the app back and unloads the sink.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hasher, RandomState};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, anyhow};
use pulseaudio::protocol::{PulseError, SubscriptionEvent, SubscriptionEventFacility, SubscriptionEventType};

use crate::bridge::BridgeStats;
use crate::journal::Journal;
use crate::{AppKey, HideMethod, ProcessorFactory, Session, SessionConfig, SessionState, SessionStatus};

use super::audio::{AudioEngine, Levels};
use super::conn::{CallError, Conn, is_connection_error};
use super::control;
use super::events::EventWatch;
use super::meters::{MeterEngine, Target};
use super::modargs::{self, RATE, ServerKind};
use super::routing::{
    self, Action, FIGHT_THRESHOLD, MoveLimiter, Moved, ROLE_OUTPUT, RoutingJournal, Sink, Stream, World,
};
use super::watchdog::Watchdog;
use super::{JOURNAL, owner_alive};

/// Wait after the first of a burst of events before reacting, so one reconcile covers it.
const DEBOUNCE: Duration = Duration::from_millis(60);
/// Reconcile at least this often even without events (a safety net for missed events).
const PERIODIC: Duration = Duration::from_secs(2);
/// Give up reconnecting to a vanished server after this long.
const RECONNECT_FOR: Duration = Duration::from_secs(15);
/// More than this many engine failures or capture-sink losses per minute is fatal.
const MAX_FAILURES_PER_MINUTE: usize = 5;
/// Least time between attempts to (re)start the ducking meters.
const METER_RESTART: Duration = Duration::from_secs(5);

/// Messages to the supervisor thread.
pub enum Msg {
    Stop,
    Event(SubscriptionEvent),
    EventsLost(String),
    EngineEnded { generation: u64, reason: String },
}

#[derive(Clone, Default)]
struct Status {
    state: SessionState,
    app_name: String,
    output_name: String,
    original_hidden: bool,
    sidechain_active: bool,
    warnings: Vec<String>,
}

#[derive(Default)]
struct Shared {
    status: Mutex<Status>,
    levels: Arc<Levels>,
    bridge: Mutex<Option<Arc<BridgeStats>>>,
    base_latency_ms: AtomicU32,
    /// Glitches of engines that have since been replaced.
    earlier_dropouts: AtomicU64,
}

/// Set while a session exists in this process. There is one journal per user, so only one
/// session may run at a time; this also tells [`super::owner_alive`] whether a journal written
/// by this very process is live or left over.
static ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

struct ActiveGuard;

impl ActiveGuard {
    fn acquire() -> anyhow::Result<Self> {
        ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| anyhow!("a Sidestage session is already running; stop it first"))
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct LinuxSession {
    shared: Arc<Shared>,
    tx: Sender<Msg>,
    supervisor: Option<JoinHandle<anyhow::Result<()>>>,
}

pub fn start(config: SessionConfig, factory: ProcessorFactory) -> anyhow::Result<Box<dyn Session>> {
    let (tx, rx) = mpsc::channel();
    let shared = Arc::new(Shared::default());
    let mut sup = Supervisor::new(config, factory, shared.clone(), tx.clone(), rx)?;
    if let Err(e) = sup.establish() {
        if let Err(undo) = sup.teardown() {
            log::error!("pulse: undoing a failed start: {undo:#}");
        }
        return Err(e);
    }
    // If the thread cannot start, the closure (and the supervisor in it) is dropped, and the
    // supervisor's Drop restores everything.
    let thread = std::thread::Builder::new()
        .name("sidestage-session".into())
        .spawn(move || sup.run())
        .context("starting the session thread")?;
    Ok(Box::new(LinuxSession { shared, tx, supervisor: Some(thread) }))
}

impl LinuxSession {
    fn finish(&mut self) -> anyhow::Result<()> {
        let Some(thread) = self.supervisor.take() else {
            return Ok(());
        };
        let _ = self.tx.send(Msg::Stop);
        thread.join().map_err(|_| anyhow!("the Linux session thread panicked"))?
    }
}

impl Session for LinuxSession {
    fn status(&self) -> SessionStatus {
        let s = lock(&self.shared.status).clone();
        let bridge = lock(&self.shared.bridge).clone();
        let levels = &self.shared.levels;
        let (fill_ms, drift_ppm, bridge_dropouts) =
            bridge.map_or((0.0, 0.0, 0), |b| (b.fill_ms(), b.correction_ppm(), b.dropouts()));
        let running = matches!(s.state, SessionState::Running | SessionState::WaitingForApp);
        SessionStatus {
            state: s.state,
            app_name: s.app_name,
            output_name: s.output_name,
            sample_rate: RATE,
            original_hidden: s.original_hidden,
            capture_peak: levels.capture.take(),
            sidechain_peak: levels.sidechain.load(),
            sidechain_active: s.sidechain_active,
            latency_ms: if running {
                f32::from_bits(self.shared.base_latency_ms.load(Ordering::Relaxed)) + fill_ms
            } else {
                0.0
            },
            dropouts: self.shared.earlier_dropouts.load(Ordering::Relaxed)
                + bridge_dropouts
                + levels.server_xruns.load(Ordering::Relaxed),
            drift_ppm,
            warnings: s.warnings,
        }
    }

    fn stop(mut self: Box<Self>) -> anyhow::Result<()> {
        self.finish()
    }
}

impl Drop for LinuxSession {
    fn drop(&mut self) {
        if let Err(e) = self.finish() {
            log::error!("pulse: stopping the session: {e:#}");
        }
    }
}

/// Our capture sink while it exists.
struct Capture {
    name: String,
    index: u32,
    monitor: String,
}

struct Supervisor {
    app: AppKey,
    output: Option<String>,
    sidechain: bool,
    factory: ProcessorFactory,
    shared: Arc<Shared>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctl: Conn,
    kind: ServerKind,
    journal: Journal<RoutingJournal>,
    session: String,
    /// Name in the journal: the current capture sink, or the one being created.
    sink_name: String,
    module: Option<u32>,
    capture: Option<Capture>,
    /// App streams we moved (or adopted), by sink-input index.
    tracked: BTreeMap<u32, Moved>,
    unmovable: BTreeSet<u32>,
    limiter: MoveLimiter,
    default_sink: Option<String>,
    last_real_default: Option<String>,
    last_default_fix: Option<Instant>,
    watchdog: Option<Watchdog>,
    events: Option<EventWatch>,
    engine: Option<AudioEngine>,
    meters: Option<MeterEngine>,
    meter_targets: Vec<Target>,
    next_meter_start: Instant,
    generation: u64,
    engine_retry_at: Option<Instant>,
    failures: Vec<Instant>,
    lost_since: Option<Instant>,
    next_reconnect: Instant,
    dirty_since: Option<Instant>,
    next_periodic: Instant,
    app_name: String,
    /// Session-wide warnings by topic, and per-stream warnings by sink-input.
    warnings: BTreeMap<&'static str, String>,
    stream_warnings: BTreeMap<u32, String>,
    failed: bool,
    torn_down: bool,
    /// Dropped last, after `Drop` has restored everything.
    _active: ActiveGuard,
}

impl Supervisor {
    fn new(
        config: SessionConfig,
        factory: ProcessorFactory,
        shared: Arc<Shared>,
        tx: Sender<Msg>,
        rx: Receiver<Msg>,
    ) -> anyhow::Result<Self> {
        let active = ActiveGuard::acquire()?;
        let mut ctl = Conn::connect("control")?;
        let journal: Journal<RoutingJournal> = Journal::open(JOURNAL)?;
        let previous = journal.load().unwrap_or_else(|e| {
            log::warn!("pulse: ignoring an unreadable journal: {e:#}");
            None
        });
        if let Some(prev) = &previous
            && prev.pid != std::process::id()
            && owner_alive(prev.pid)
        {
            anyhow::bail!("another Sidestage window is already capturing an app; stop it first");
        }
        // Crash sweep: undo whatever an earlier run left behind before adding our own.
        for line in control::sweep(&mut ctl, previous.as_ref(), None)? {
            log::info!("pulse: {line}");
        }
        journal.clear()?;

        let server = control::server(&mut ctl)?;
        log::info!("pulse: connected to {} at {}", server.kind.label(), ctl.path().display());
        if !server.kind.clears_routes_on_default_move() {
            log::info!("pulse: {} pins streams moved back to the default output", server.kind.label());
        }
        let mut warnings = BTreeMap::new();
        match &config.hide {
            HideMethod::Auto => {}
            HideMethod::KeepOriginal => {
                warnings.insert(
                    "hide",
                    "\u{201c}Keep original\u{201d} is not available on Linux; the original is hidden instead."
                        .to_owned(),
                );
            }
            HideMethod::ParkOn(_) => {
                warnings.insert(
                    "hide",
                    "Parking the app on another output is a Windows option; on Linux the app is moved to Sidestage's hidden capture output instead.".to_owned(),
                );
            }
        }
        let sinks = control::sinks(&mut ctl)?;
        let output = match config.output {
            Some(name) if sinks.iter().any(|s| s.name == name && !s.is_capture()) => Some(name),
            Some(name) => {
                warnings.insert(
                    "output",
                    format!(
                        "The output \u{201c}{name}\u{201d} is not available; playing to the default output instead."
                    ),
                );
                None
            }
            None => None,
        };
        let last_real_default =
            server.default_sink.clone().filter(|d| sinks.iter().any(|s| s.name == *d && !s.is_capture()));
        let now = Instant::now();
        Ok(Self {
            app_name: config.app.0.clone(),
            app: config.app,
            output,
            sidechain: config.sidechain,
            factory,
            shared,
            tx,
            rx,
            ctl,
            kind: server.kind,
            journal,
            session: format!("{:016x}", random_u64()),
            sink_name: String::new(),
            module: None,
            capture: None,
            tracked: BTreeMap::new(),
            unmovable: BTreeSet::new(),
            limiter: MoveLimiter::default(),
            default_sink: server.default_sink,
            last_real_default,
            last_default_fix: None,
            watchdog: None,
            events: None,
            engine: None,
            meters: None,
            meter_targets: Vec::new(),
            next_meter_start: now,
            generation: 0,
            engine_retry_at: None,
            failures: Vec::new(),
            lost_since: None,
            next_reconnect: now,
            dirty_since: None,
            next_periodic: now + PERIODIC,
            warnings,
            stream_warnings: BTreeMap::new(),
            failed: false,
            torn_down: false,
            _active: active,
        })
    }

    /// Everything [`start`] must finish before returning.
    fn establish(&mut self) -> anyhow::Result<()> {
        // The watchdog first, so a crash at any point after the sink is loaded is undone.
        match Watchdog::spawn(self.journal.path(), &self.session) {
            Ok(w) => self.watchdog = Some(w),
            Err(e) => {
                log::warn!("pulse: {e:#}");
                self.warnings.insert(
                    "watchdog",
                    "Crash protection is off: if Sidestage is killed, restart it to get the app's sound back."
                        .to_owned(),
                );
            }
        }
        self.create_capture_sink()?;
        self.events = Some(EventWatch::start(self.tx.clone())?);
        // Output first, then move the app: the music is never parked without a listener.
        self.start_engine()?;
        if self.sidechain {
            self.start_meters();
        }
        self.reconcile()
    }

    fn run(mut self) -> anyhow::Result<()> {
        loop {
            let msg = if self.failed {
                // Nothing left to do but wait for the user to stop the session.
                self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                let wake = if self.lost_since.is_some() {
                    self.next_reconnect
                } else {
                    [self.dirty_since.map(|t| t + DEBOUNCE), self.engine_retry_at]
                        .into_iter()
                        .flatten()
                        .fold(self.next_periodic, Instant::min)
                };
                self.rx.recv_timeout(wake.saturating_duration_since(Instant::now()))
            };
            match msg {
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Msg::Event(event)) => self.on_event(event),
                Ok(Msg::EventsLost(reason)) => self.on_server_lost(&reason),
                Ok(Msg::EngineEnded { generation, reason }) => self.on_engine_ended(generation, &reason),
                Err(RecvTimeoutError::Timeout) => {}
            }
            if self.failed {
                continue;
            }
            match self.tick() {
                Ok(()) => {}
                Err(e) if is_connection_error(&e) => self.on_server_lost(&format!("{e:#}")),
                Err(e) => {
                    log::warn!("pulse: {e:#}");
                    self.warnings.insert("server", format!("Sidestage could not update the audio routing: {e:#}"));
                    let mut status = lock(&self.shared.status);
                    status.warnings = self.warnings.values().chain(self.stream_warnings.values()).cloned().collect();
                }
            }
        }
        let result = self.teardown();
        lock(&self.shared.status).state = SessionState::Stopped;
        result
    }

    /// Does whatever is due.
    fn tick(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        if self.lost_since.is_some() {
            return if now >= self.next_reconnect { self.reconnect() } else { Ok(()) };
        }
        if self.engine_retry_at.is_some_and(|t| now >= t) {
            self.engine_retry_at = None;
            if let Err(e) = self.rebuild() {
                log::warn!("pulse: setting up the audio again failed: {e:#}");
                self.note_failure(&format!("{e:#}"));
                if !self.failed {
                    self.engine_retry_at = Some(now + Duration::from_secs(1));
                }
            }
            self.mark_dirty(now);
        }
        if self.dirty_since.is_some_and(|t| now >= t + DEBOUNCE) || now >= self.next_periodic {
            self.reconcile()?;
        }
        Ok(())
    }

    /// Recreates whatever of the capture sink and the engine is missing.
    fn rebuild(&mut self) -> anyhow::Result<()> {
        if self.capture.is_none() {
            self.create_capture_sink()?;
        }
        if self.engine.is_none() {
            self.start_engine()?;
        }
        Ok(())
    }

    fn mark_dirty(&mut self, now: Instant) {
        self.dirty_since.get_or_insert(now);
    }

    fn on_event(&mut self, event: SubscriptionEvent) {
        use SubscriptionEventFacility as F;
        if matches!(event.event_facility, F::SinkInput | F::Sink | F::Server) {
            if event.event_facility == F::Sink
                && event.event_type == SubscriptionEventType::Removed
                && self.capture.as_ref().is_some_and(|c| Some(c.index) == event.index)
            {
                log::warn!("pulse: the capture sink was removed");
            }
            self.mark_dirty(Instant::now());
        }
    }

    fn on_engine_ended(&mut self, generation: u64, reason: &str) {
        if generation != self.generation || self.engine.is_none() {
            return;
        }
        self.stop_engine();
        self.note_failure(reason);
        if !self.failed {
            self.engine_retry_at = Some(Instant::now() + Duration::from_millis(300));
            self.set_state(SessionState::Starting);
        }
    }

    /// Counts a failure; too many in a minute ends the session with `reason`.
    fn note_failure(&mut self, reason: &str) {
        let now = Instant::now();
        self.failures.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        self.failures.push(now);
        if self.failures.len() > MAX_FAILURES_PER_MINUTE {
            self.fail(format!("Sidestage's audio keeps failing: {reason}"));
        }
    }

    /// The events connection died: most likely the sound server restarted or crashed.
    fn on_server_lost(&mut self, reason: &str) {
        if self.failed || self.lost_since.is_some() {
            return;
        }
        log::warn!("pulse: lost the sound server: {reason}");
        self.meters = None;
        self.stop_engine();
        self.events = None;
        let now = Instant::now();
        self.lost_since = Some(now);
        self.next_reconnect = now + Duration::from_millis(500);
        self.set_state(SessionState::Starting);
    }

    /// Tries to resume after losing the server; gives up after [`RECONNECT_FOR`].
    fn reconnect(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        let since = self.lost_since.unwrap_or(now);
        match self.resume() {
            Ok(()) => {
                log::info!("pulse: reconnected to the sound server");
                self.lost_since = None;
            }
            Err(e) => {
                self.meters = None;
                self.stop_engine();
                self.events = None;
                if now.duration_since(since) > RECONNECT_FOR {
                    self.fail(format!("Lost the connection to the sound server: {e:#}"));
                } else {
                    log::debug!("pulse: reconnecting failed, will retry: {e:#}");
                    self.next_reconnect = now + Duration::from_secs(1);
                }
            }
        }
        Ok(())
    }

    fn resume(&mut self) -> anyhow::Result<()> {
        let mut ctl = Conn::connect("control")?;
        self.kind = control::server(&mut ctl)?.kind;
        let alive = self.capture.is_some() && control::find_sink(&mut ctl, &self.sink_name)?.is_some();
        self.ctl = ctl;
        if !alive {
            // A restarted server: its stream indices start over, so nothing we knew applies.
            log::info!("pulse: the sound server restarted; setting Sidestage up again");
            self.tracked.clear();
            self.unmovable.clear();
            self.limiter = MoveLimiter::default();
            self.stream_warnings.clear();
            self.capture = None;
            self.create_capture_sink()?;
        }
        self.events = Some(EventWatch::start(self.tx.clone())?);
        self.start_engine()?;
        if self.sidechain && self.meters.is_none() {
            self.start_meters();
        }
        self.reconcile()
    }

    /// Loads a fresh capture sink, journaling its name before the load so a crash in between
    /// still leaves a record the sweep can act on.
    ///
    /// Whatever is left of the previous sink is removed first, while the journal still names
    /// it: a load whose sink never appeared leaves a module that the new name would otherwise
    /// orphan (it would linger in sound settings, and stream-restore could route apps to it).
    fn create_capture_sink(&mut self) -> anyhow::Result<()> {
        if !self.sink_name.is_empty() {
            let tracked: Vec<Moved> = self.tracked.values().cloned().collect();
            for line in control::restore_sink(&mut self.ctl, &self.sink_name, &tracked)? {
                log::info!("pulse: {line}");
            }
        }
        let name = modargs::sink_name(random_u64());
        self.sink_name.clone_from(&name);
        self.module = None;
        self.save_journal()?;
        let module = control::load_capture_sink(&mut self.ctl, &self.kind, &name)?;
        self.module = Some(module);
        self.save_journal()?;
        let sink = match control::wait_for_sink(&mut self.ctl, &name, Duration::from_secs(2)) {
            Ok(sink) => sink,
            Err(e) => {
                if let Err(undo) = control::unload(&mut self.ctl, module) {
                    log::warn!("pulse: {undo:#}");
                }
                return Err(e);
            }
        };
        log::info!("pulse: capture sink {name} is sink #{}", sink.index);
        self.capture = Some(Capture { monitor: format!("{name}.monitor"), name, index: sink.index });
        Ok(())
    }

    fn start_engine(&mut self) -> anyhow::Result<()> {
        let monitor = self.capture.as_ref().map(|c| c.monitor.clone()).context("the capture sink is missing")?;
        let processor = (self.factory)(RATE);
        self.generation += 1;
        let engine = AudioEngine::start(
            &monitor,
            self.output.as_deref(),
            processor,
            self.shared.levels.clone(),
            self.tx.clone(),
            self.generation,
        )?;
        *lock(&self.shared.bridge) = Some(engine.stats.clone());
        self.shared.base_latency_ms.store(engine.base_latency_ms.to_bits(), Ordering::Relaxed);
        if let Some(name) = &self.output {
            if engine.output_fell_back {
                self.warnings.insert(
                    "output",
                    format!(
                        "The output \u{201c}{name}\u{201d} is not available; playing to the default output instead."
                    ),
                );
            } else {
                self.warnings.remove("output");
            }
        }
        self.engine = Some(engine);
        Ok(())
    }

    fn stop_engine(&mut self) {
        if let Some(engine) = self.engine.take() {
            let dropouts = engine.stats.dropouts();
            drop(engine);
            self.shared.earlier_dropouts.fetch_add(dropouts, Ordering::Relaxed);
            *lock(&self.shared.bridge) = None;
        }
    }

    fn start_meters(&mut self) {
        self.next_meter_start = Instant::now() + METER_RESTART;
        match MeterEngine::start(self.shared.levels.clone()) {
            Ok(m) => {
                self.meters = Some(m);
                self.meter_targets.clear();
                self.warnings.remove("meters");
            }
            Err(e) => {
                log::warn!("pulse: ducking meters unavailable: {e:#}");
                self.warnings.insert(
                    "meters",
                    "Ducking is unavailable: Sidestage could not measure the game's level.".to_owned(),
                );
            }
        }
    }

    /// Brings the server to the wanted state and publishes the status.
    fn reconcile(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        self.dirty_since = None;
        self.next_periodic = now + PERIODIC;
        // After a teardown (a failure inside this same tick can cause one) nothing may move the
        // app again: no stop or Drop would undo it, since teardown runs only once.
        if self.failed || self.torn_down || self.capture.is_none() {
            return Ok(());
        }
        let server = control::server(&mut self.ctl)?;
        let sinks = control::sinks(&mut self.ctl)?;
        let Some(capture_index) = sinks
            .iter()
            .find(|s| Some(s.name.as_str()) == self.capture.as_ref().map(|c| c.name.as_str()))
            .map(|s| s.index)
        else {
            return self.capture_sink_lost();
        };
        if let Some(c) = self.capture.as_mut() {
            c.index = capture_index;
        }
        self.track_default(server.default_sink, &sinks, now);
        if self.sidechain && !self.meters.as_ref().is_some_and(MeterEngine::is_running) && now >= self.next_meter_start
        {
            log::info!("pulse: (re)starting the ducking meters");
            self.meters = None;
            self.start_meters();
        }

        let mut streams = control::streams(&mut self.ctl)?;
        self.forget_gone(&streams);
        let world = World {
            streams: &streams,
            sinks: &sinks,
            capture_sink: capture_index,
            default_sink: self.default_sink.as_deref(),
            fallback_sink: self.last_real_default.as_deref(),
        };
        let actions = routing::plan(&world, &self.app.0, &self.tracked, &self.unmovable, &self.limiter, now);
        if !actions.is_empty() {
            self.apply(actions, &streams, now)?;
            streams = control::streams(&mut self.ctl)?;
        }
        self.warnings.remove("server");
        self.publish(&streams, &sinks, capture_index);
        Ok(())
    }

    fn apply(&mut self, actions: Vec<Action>, streams: &[Stream], now: Instant) -> anyhow::Result<()> {
        // Journal every stream before it is touched. The in-memory record only changes once the
        // journal holds it: otherwise, after a failed write, the next attempt would find nothing
        // new to journal and move the streams unrecorded.
        let mut tracked = self.tracked.clone();
        let mut changed = false;
        for action in &actions {
            if let Action::Capture(m) | Action::Adopt(m) = action {
                changed |= tracked.insert(m.index, m.clone()).as_ref() != Some(m);
            }
        }
        if changed {
            self.write_journal(&tracked)?;
        }
        self.tracked = tracked;
        let sink_name = self.sink_name.clone();
        let mut refused = false;
        for action in actions {
            match action {
                Action::Adopt(m) => log::info!("pulse: {} (#{}) is already on the capture sink", m.app, m.index),
                Action::Capture(m) => {
                    let attempts = self.limiter.record(m.index, now);
                    match control::move_stream(&mut self.ctl, m.index, &sink_name) {
                        Ok(()) => {
                            log::info!("pulse: moved {} (#{}) from {} to {sink_name}", m.app, m.index, m.original_sink);
                            if attempts >= FIGHT_THRESHOLD {
                                self.warnings.insert(
                                    "fight",
                                    format!(
                                        "Something keeps moving {} back to its old output (another audio tool?). Sidestage keeps moving it back, more slowly each time.",
                                        m.app
                                    ),
                                );
                            }
                        }
                        Err(CallError::Server(PulseError::NoEntity)) => {
                            // The stream ended between listing and moving.
                            log::debug!("pulse: {} (#{}) is gone", m.app, m.index);
                            self.tracked.remove(&m.index);
                            refused = true;
                        }
                        Err(CallError::Server(code)) => {
                            // DONT_MOVE or passthrough streams (PulseAudio), node.dont-reconnect
                            // streams (pipewire-pulse).
                            log::warn!("pulse: cannot move {} (#{}): {code}", m.app, m.index);
                            self.unmovable.insert(m.index);
                            self.tracked.remove(&m.index);
                            refused = true;
                            let title = streams
                                .iter()
                                .find(|s| s.index == m.index)
                                .and_then(|s| s.media_name.as_deref())
                                .map(|t| format!(" \u{201c}{t}\u{201d}"))
                                .unwrap_or_default();
                            self.stream_warnings.insert(
                                m.index,
                                format!(
                                    "{}'s stream{title} could not be moved to Sidestage (the app does not allow it), so you hear it unprocessed.",
                                    m.app
                                ),
                            );
                        }
                        Err(e) => return Err(e).context("moving the app to Sidestage"),
                    }
                }
                Action::Evict { index, to } => {
                    self.limiter.record(index, now);
                    let who = streams.iter().find(|s| s.index == index).map_or("A stream", Stream::label).to_owned();
                    match control::move_stream(&mut self.ctl, index, &to) {
                        Ok(()) => {
                            log::info!("pulse: moved {who} (#{index}) off the capture sink to {to}")
                        }
                        Err(CallError::Server(code)) => {
                            log::warn!("pulse: cannot move {who} (#{index}) off the capture sink: {code}")
                        }
                        Err(e) => {
                            return Err(e).context("moving a stream off Sidestage's capture output");
                        }
                    }
                }
            }
        }
        if refused {
            self.save_journal()?;
        }
        Ok(())
    }

    /// Follows the default sink. If the server made our capture sink the default (the real
    /// device vanished), apps and our own output would feed back into it: switch back.
    fn track_default(&mut self, default: Option<String>, sinks: &[Sink], now: Instant) {
        let capture_is_default =
            default.as_deref().is_some_and(|d| sinks.iter().any(|s| s.name == d && s.is_capture()));
        if !capture_is_default {
            if let Some(d) = &default
                && sinks.iter().any(|s| s.name == *d)
            {
                self.last_real_default = Some(d.clone());
            }
            self.default_sink = default;
            return;
        }
        self.default_sink = default;
        if self.last_default_fix.is_some_and(|t| now.duration_since(t) < Duration::from_secs(5)) {
            return;
        }
        let Some(real) = routing::home_sink(sinks, None, self.last_real_default.as_deref()) else {
            return;
        };
        self.last_default_fix = Some(now);
        match control::set_default_sink(&mut self.ctl, &real.name) {
            Ok(()) => {
                log::warn!("pulse: the default output had become the capture sink; set it back to {}", real.name);
                self.default_sink = Some(real.name.clone());
                self.warnings.insert(
                    "default",
                    format!(
                        "Your system made Sidestage's capture output the default; Sidestage switched it back to {}.",
                        real.label()
                    ),
                );
            }
            Err(e) => log::warn!("pulse: could not reset the default output: {e}"),
        }
    }

    /// Forgets streams that no longer exist (their saved routes point at a sink name that is
    /// never used again, so they are harmless).
    fn forget_gone(&mut self, streams: &[Stream]) {
        let alive: BTreeSet<u32> = streams.iter().map(|s| s.index).collect();
        let before = self.tracked.len();
        self.tracked.retain(|i, _| alive.contains(i));
        self.unmovable.retain(|i| alive.contains(i));
        self.stream_warnings.retain(|i, _| alive.contains(i));
        self.limiter.retain(|i| alive.contains(&i));
        if self.tracked.len() != before
            && let Err(e) = self.save_journal()
        {
            log::warn!("pulse: {e:#}");
        }
    }

    fn publish(&mut self, streams: &[Stream], sinks: &[Sink], capture_index: u32) {
        let app = routing::app_state(streams, capture_index, &self.app.0);
        if let Some(name) = app.name {
            self.app_name = name;
        }
        // By the index the server gave our output stream, or else by its role (there is only one
        // Sidestage session per user): if the two index lists ever disagree, ducking and the
        // output name must still work.
        let output_sink = self
            .engine
            .as_ref()
            .and_then(|e| {
                streams
                    .iter()
                    .find(|s| s.index == e.output_stream)
                    .or_else(|| streams.iter().find(|s| s.own_role.as_deref() == Some(ROLE_OUTPUT)))
            })
            .and_then(|o| sinks.iter().find(|k| k.index == o.sink));
        if let (Some(meters), Some(sink)) = (&self.meters, output_sink)
            && let Some(monitor) = sink.monitor_source
        {
            let targets: Vec<Target> =
                routing::meter_targets(streams, sink.index, &self.app.0).into_iter().map(|i| (i, monitor)).collect();
            if targets != self.meter_targets {
                meters.set_targets(targets.clone());
                self.meter_targets = targets;
            }
        }
        let state = if self.engine.is_none() {
            SessionState::Starting
        } else if app.streams == 0 {
            SessionState::WaitingForApp
        } else {
            SessionState::Running
        };
        let mut status = lock(&self.shared.status);
        if self.failed {
            return;
        }
        status.state = state;
        status.app_name.clone_from(&self.app_name);
        if let Some(sink) = output_sink
            && status.output_name != sink.label()
        {
            log::info!("pulse: playing to {} ({})", sink.label(), sink.name);
            status.output_name = sink.label().to_owned();
        }
        status.original_hidden = app.captured == app.streams;
        status.sidechain_active = self.meters.as_ref().is_some_and(MeterEngine::is_running);
        status.warnings = self.warnings.values().chain(self.stream_warnings.values()).cloned().collect();
    }

    /// Our capture sink vanished (someone unloaded it): the server has already moved the app's
    /// streams elsewhere; make a new sink, and the next reconcile captures them again (their
    /// tracked origins are kept).
    fn capture_sink_lost(&mut self) -> anyhow::Result<()> {
        log::warn!("pulse: capture sink {} disappeared", self.sink_name);
        self.stop_engine();
        self.capture = None;
        self.note_failure("the capture output keeps disappearing");
        if !self.failed {
            self.set_state(SessionState::Starting);
            self.engine_retry_at = Some(Instant::now());
        }
        Ok(())
    }

    /// Ends the session with an error the user sees. Undoes the routing right away so the
    /// app's sound comes back without waiting for the user to press stop.
    fn fail(&mut self, message: String) {
        if self.failed {
            return; // the first failure is the one worth showing
        }
        log::error!("pulse: {message}");
        self.failed = true;
        if let Err(e) = self.teardown() {
            log::error!("pulse: {e:#}");
        }
        lock(&self.shared.status).state = SessionState::Error(message);
    }

    fn set_state(&self, state: SessionState) {
        if !self.failed {
            lock(&self.shared.status).state = state;
        }
    }

    fn save_journal(&self) -> anyhow::Result<()> {
        self.write_journal(&self.tracked)
    }

    fn write_journal(&self, tracked: &BTreeMap<u32, Moved>) -> anyhow::Result<()> {
        self.journal
            .save(&RoutingJournal {
                pid: std::process::id(),
                session: self.session.clone(),
                sink_name: self.sink_name.clone(),
                module_index: self.module,
                streams: tracked.values().cloned().collect(),
            })
            .context("writing the crash-recovery journal")
    }

    /// Stops the threads, moves every stream off the capture sink, unloads it and clears the
    /// journal. Runs once; later calls return Ok.
    fn teardown(&mut self) -> anyhow::Result<()> {
        if std::mem::replace(&mut self.torn_down, true) {
            return Ok(());
        }
        self.meters = None;
        self.stop_engine();
        self.events = None;
        let result = self.restore();
        match &result {
            Ok(()) => {
                if let Err(e) = self.journal.clear() {
                    log::error!("pulse: {e:#}");
                }
            }
            Err(e) => log::error!(
                "pulse: restoring the apps' outputs failed; the watchdog or the next start will retry: {e:#}"
            ),
        }
        // Closing the pipe lets the watchdog exit (or retry, if the journal is still there).
        self.watchdog = None;
        self.shared.levels.capture.store(0.0);
        self.shared.levels.sidechain.store(0.0);
        result
    }

    fn restore(&mut self) -> anyhow::Result<()> {
        if self.sink_name.is_empty() {
            return Ok(());
        }
        let tracked: Vec<Moved> = self.tracked.values().cloned().collect();
        let lines = match control::restore_sink(&mut self.ctl, &self.sink_name, &tracked) {
            Ok(lines) => lines,
            Err(e) if is_connection_error(&e) => {
                log::warn!("pulse: retrying the restore on a new connection: {e:#}");
                self.ctl = Conn::connect("control")?;
                control::restore_sink(&mut self.ctl, &self.sink_name, &tracked)?
            }
            Err(e) => return Err(e),
        };
        for line in lines {
            log::info!("pulse: {line}");
        }
        Ok(())
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if let Err(e) = self.teardown() {
            log::error!("pulse: {e:#}");
        }
    }
}

/// Randomness for sink names and session IDs without a dependency: std's `RandomState` is
/// seeded from the OS RNG; mixing in the time and PID keeps values distinct across processes.
fn random_u64() -> u64 {
    let mut h = RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    if let Ok(t) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        h.write_u128(t.as_nanos());
    }
    h.finish()
}
