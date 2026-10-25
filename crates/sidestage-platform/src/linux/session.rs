//! A running Linux session: one output stream and any number of sources.
//!
//! [`start`] connects, sweeps what earlier runs left behind, and starts the watchdog, the event
//! watch and the output engine on the caller's thread (so failures are reported directly), then
//! hands everything to a supervisor thread, which starts the sources. For every source the
//! supervisor loads a null sink of its own, records its monitor into a drift bridge that the
//! output engine's mixer plays through the source's processor, keeps the app's streams on that
//! sink (and everything else off it), and when the source is removed or the session stops moves
//! the app back and unloads the sink. It also feeds the ducking meters, applies new source
//! lists and ignore lists live, and rebuilds whatever the server pulls away: a source alone
//! when only its sink or capture is affected, everything when the output or the server is.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hasher, RandomState};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, anyhow, bail};
use pulseaudio::protocol::{PulseError, SubscriptionEvent, SubscriptionEventFacility, SubscriptionEventType};

use crate::bridge::{self, BridgeConfig, BridgeStats};
use crate::journal::Journal;
use crate::{
    AppKey, AtomicLevel, HideMethod, Processor, ProcessorFactory, Session, SessionConfig, SessionState, SessionStatus,
    SourceConfig, SourceId, SourceStatus,
};

use super::audio::{AudioEngine, BRIDGE_TARGET_MS, Levels};
use super::capture::Capture;
use super::conn::{CallError, Conn, is_connection_error};
use super::control;
use super::events::EventWatch;
use super::meters::{MeterEngine, Target};
use super::mixer::{MAX_SOURCES, SourceSlot};
use super::modargs::{self, RATE, ServerKind};
use super::routing::{
    self, Action, AppState, FIGHT_THRESHOLD, MoveLimiter, Moved, ROLE_OUTPUT, Route, RoutingJournal, Sink, SinkRecord,
    Stream, World,
};
use super::sources::{self, Identity};
use super::watchdog::Watchdog;
use super::{JOURNAL, owner_alive};

/// Wait after the first of a burst of events before reacting, so one reconcile covers it.
const DEBOUNCE: Duration = Duration::from_millis(60);
/// Reconcile at least this often even without events (a safety net for missed events).
const PERIODIC: Duration = Duration::from_secs(2);
/// Give up reconnecting to a vanished server after this long.
const RECONNECT_FOR: Duration = Duration::from_secs(15);
/// More than this many failures per minute is fatal: of the output engine for the session, of
/// a source's sink or capture for that source.
const MAX_FAILURES_PER_MINUTE: usize = 5;
/// Least time between attempts to (re)start the ducking meters.
const METER_RESTART: Duration = Duration::from_secs(5);
/// Wait before restarting a source whose capture ended.
const CAPTURE_RETRY: Duration = Duration::from_millis(300);
/// Wait before setting a source up again after its setup failed.
const SETUP_RETRY: Duration = Duration::from_secs(1);

/// Messages to the supervisor thread.
pub enum Msg {
    Stop,
    Event(SubscriptionEvent),
    EventsLost(String),
    EngineEnded { generation: u64, reason: String },
    CaptureEnded { source: SourceId, generation: u64, reason: String },
    SetSources(Vec<NewSource>),
    SetIgnore(Vec<AppKey>),
}

/// A source as handed to the supervisor, with the live counters its status entry reads.
pub struct NewSource {
    config: SourceConfig,
    live: Arc<SourceLive>,
}

/// Counters of one source that [`Session::status`] reads live. The output thread only touches
/// `capture_peak`.
#[derive(Default)]
struct SourceLive {
    capture_peak: Arc<AtomicLevel>,
    /// The bridge of the running capture, if any.
    bridge: Mutex<Option<Arc<BridgeStats>>>,
    /// Glitches of the source's earlier captures.
    earlier_dropouts: AtomicU64,
    /// Latency before the bridge (f32 bits).
    capture_latency_ms: AtomicU32,
}

/// One source in the session status, in the order the app configured them.
#[derive(Clone)]
struct SourceEntry {
    id: SourceId,
    app: AppKey,
    hide: HideMethod,
    /// Also the entry's link to the supervisor's source: the same `Arc` on both sides.
    live: Arc<SourceLive>,
    state: SessionState,
    app_name: String,
    original_hidden: bool,
    warnings: Vec<String>,
}

impl SourceEntry {
    fn new(config: &SourceConfig) -> Self {
        Self {
            id: config.id,
            app: config.app.clone(),
            hide: config.hide.clone(),
            live: Arc::default(),
            state: SessionState::Starting,
            app_name: config.app.0.clone(),
            original_hidden: false,
            warnings: Vec::new(),
        }
    }

    fn identity(&self) -> Identity<'_> {
        Identity { id: self.id, app: &self.app, hide: &self.hide }
    }
}

#[derive(Default)]
struct Status {
    state: SessionState,
    output_name: String,
    sidechain_active: bool,
    warnings: Vec<String>,
    sources: Vec<SourceEntry>,
}

struct Shared {
    status: Mutex<Status>,
    levels: Arc<Levels>,
    /// Latency of the output side (f32 bits).
    output_latency_ms: AtomicU32,
}

/// Set while a session exists in this process. There is one journal per user, so only one
/// session may run at a time (it plays any number of sources); this also tells
/// [`super::owner_alive`] whether a journal written by this very process is live or left over.
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
            .map_err(|_| anyhow!("a Sidestage session is already running; change its sources instead"))
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

pub fn start(config: SessionConfig) -> anyhow::Result<Box<dyn Session>> {
    if let Some(e) = SessionConfig::duplicate_error(&config.sources) {
        bail!(e);
    }
    let (tx, rx) = mpsc::channel();
    let entries: Vec<SourceEntry> = config.sources.iter().map(SourceEntry::new).collect();
    let initial: Vec<NewSource> = config
        .sources
        .iter()
        .zip(&entries)
        .map(|(c, e)| NewSource { config: c.clone(), live: e.live.clone() })
        .collect();
    let shared = Arc::new(Shared {
        status: Mutex::new(Status { sources: entries, ..Status::default() }),
        levels: Arc::default(),
        output_latency_ms: AtomicU32::new(0),
    });
    let mut sup = Supervisor::new(&config, shared.clone(), tx.clone(), rx)?;
    if let Err(e) = sup.establish() {
        if let Err(undo) = sup.teardown() {
            log::error!("pulse: undoing a failed start: {undo:#}");
        }
        return Err(e);
    }
    // The sources are set up on the session thread, first thing; until then their status says
    // Starting. Cannot fail: the receiver is in `sup`.
    let _ = tx.send(Msg::SetSources(initial));
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

    fn send(&self, msg: Msg) -> anyhow::Result<()> {
        self.tx.send(msg).map_err(|_| anyhow!("the session has ended"))
    }
}

impl Session for LinuxSession {
    fn status(&self) -> SessionStatus {
        let s = lock(&self.shared.status);
        let levels = &self.shared.levels;
        let running = s.state == SessionState::Running;
        let output_latency =
            if running { f32::from_bits(self.shared.output_latency_ms.load(Ordering::Relaxed)) } else { 0.0 };
        let sources = s
            .sources
            .iter()
            .map(|e| {
                let live = &e.live;
                let bridge = lock(&live.bridge).clone();
                let capturing = running && matches!(e.state, SessionState::Running | SessionState::WaitingForApp);
                let (fill_ms, drift_ppm, dropouts) =
                    bridge.map_or((0.0, 0.0, 0), |b| (b.fill_ms(), b.correction_ppm(), b.dropouts()));
                SourceStatus {
                    id: e.id,
                    app_name: e.app_name.clone(),
                    state: e.state.clone(),
                    original_hidden: e.original_hidden,
                    capture_peak: live.capture_peak.take(),
                    latency_ms: if capturing {
                        output_latency + f32::from_bits(live.capture_latency_ms.load(Ordering::Relaxed)) + fill_ms
                    } else {
                        0.0
                    },
                    dropouts: live.earlier_dropouts.load(Ordering::Relaxed) + dropouts,
                    drift_ppm,
                    warnings: e.warnings.clone(),
                }
            })
            .collect();
        SessionStatus {
            state: s.state.clone(),
            output_name: s.output_name.clone(),
            sample_rate: RATE,
            sidechain_peak: levels.sidechain.load(),
            sidechain_active: s.sidechain_active,
            latency_ms: output_latency,
            dropouts: levels.server_xruns.load(Ordering::Relaxed),
            warnings: s.warnings.clone(),
            sources,
        }
    }

    fn set_sources(&mut self, sources: Vec<SourceConfig>) -> anyhow::Result<()> {
        if let Some(e) = SessionConfig::duplicate_error(&sources) {
            bail!(e);
        }
        // The status shows the new list at once (kept sources as they are, new ones Starting);
        // the session thread does the work.
        let batch = {
            let mut status = lock(&self.shared.status);
            if matches!(status.state, SessionState::Error(_) | SessionState::Stopped) {
                bail!("the session has ended");
            }
            let entries = sources::carry_over(&status.sources, SourceEntry::identity, &sources, SourceEntry::new);
            let batch = sources
                .into_iter()
                .zip(&entries)
                .map(|(config, e)| NewSource { config, live: e.live.clone() })
                .collect();
            status.sources = entries;
            batch
        };
        self.send(Msg::SetSources(batch))
    }

    fn set_sidechain_ignore(&mut self, apps: Vec<AppKey>) -> anyhow::Result<()> {
        self.send(Msg::SetIgnore(apps))
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

/// A source's capture sink while it exists.
struct CaptureSink {
    name: String,
    index: u32,
    monitor: String,
}

/// A source's running capture; its slot is in the output engine's mixer.
struct Pipeline {
    capture: Capture,
    generation: u64,
    stats: Arc<BridgeStats>,
}

struct Source {
    id: SourceId,
    app: AppKey,
    hide: HideMethod,
    factory: ProcessorFactory,
    live: Arc<SourceLive>,
    /// Name in the journal: the current capture sink, or the one being created; empty while
    /// the source has none.
    sink_name: String,
    module: Option<u32>,
    sink: Option<CaptureSink>,
    pipeline: Option<Pipeline>,
    /// The app's streams this source moved (or adopted), by sink-input index.
    tracked: BTreeMap<u32, Moved>,
    unmovable: BTreeSet<u32>,
    app_name: String,
    /// Warnings by topic, and per-stream warnings by sink-input.
    warnings: BTreeMap<&'static str, String>,
    stream_warnings: BTreeMap<u32, String>,
    failures: Vec<Instant>,
    /// When to set up what is missing (sink, capture).
    retry_at: Option<Instant>,
    /// Given up: shown until the app removes or replaces the source.
    error: Option<String>,
}

impl Source {
    fn identity(&self) -> Identity<'_> {
        Identity { id: self.id, app: &self.app, hide: &self.hide }
    }

    fn route(&self, engine_up: bool) -> Option<Route<'_>> {
        let sink = self.sink.as_ref()?;
        Some(Route {
            source: self.id,
            app: &self.app,
            sink: sink.index,
            tracked: &self.tracked,
            unmovable: &self.unmovable,
            listening: engine_up && self.pipeline.is_some(),
        })
    }

    fn state(&self, engine_up: bool, app: &AppState) -> SessionState {
        if let Some(e) = &self.error {
            SessionState::Error(e.clone())
        } else if !engine_up || self.sink.is_none() || self.pipeline.is_none() {
            SessionState::Starting
        } else if app.streams == 0 {
            SessionState::WaitingForApp
        } else {
            SessionState::Running
        }
    }

    fn record(&self) -> SinkRecord {
        SinkRecord {
            sink_name: self.sink_name.clone(),
            module_index: self.module,
            streams: self.tracked.values().cloned().collect(),
        }
    }
}

struct Supervisor {
    output: Option<String>,
    sidechain: bool,
    /// Apps whose sound never counts as the game.
    ignore: Vec<AppKey>,
    master: ProcessorFactory,
    shared: Arc<Shared>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctl: Conn,
    kind: ServerKind,
    journal: Journal<RoutingJournal>,
    session: String,
    sources: Vec<Source>,
    /// Capture sinks of removed or failed sources whose undo could not finish (the server was
    /// unreachable). Kept in the journal and retried until it does.
    orphans: Vec<SinkRecord>,
    limiter: MoveLimiter,
    default_sink: Option<String>,
    last_real_default: Option<String>,
    last_default_fix: Option<Instant>,
    watchdog: Option<Watchdog>,
    events: Option<EventWatch>,
    engine: Option<AudioEngine>,
    /// Generation of the current engine.
    engine_generation: u64,
    /// The engine's command queue was full: restart it.
    engine_stuck: bool,
    meters: Option<MeterEngine>,
    meter_targets: Vec<Target>,
    next_meter_start: Instant,
    /// Numbers engines and captures, so messages from replaced ones are ignored.
    generation: u64,
    engine_retry_at: Option<Instant>,
    /// Engine failures.
    failures: Vec<Instant>,
    lost_since: Option<Instant>,
    next_reconnect: Instant,
    dirty_since: Option<Instant>,
    next_periodic: Instant,
    /// Session-wide warnings by topic.
    warnings: BTreeMap<&'static str, String>,
    failed: bool,
    torn_down: bool,
    /// Dropped last, after `Drop` has restored everything.
    _active: ActiveGuard,
}

impl Supervisor {
    fn new(config: &SessionConfig, shared: Arc<Shared>, tx: Sender<Msg>, rx: Receiver<Msg>) -> anyhow::Result<Self> {
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
            bail!("another Sidestage window is already capturing apps; stop it first");
        }
        // Crash sweep: undo whatever an earlier run left behind before adding our own.
        for line in control::sweep(&mut ctl, previous.as_ref(), &BTreeSet::new())? {
            log::info!("pulse: {line}");
        }
        journal.clear()?;

        let server = control::server(&mut ctl)?;
        log::info!("pulse: connected to {} at {}", server.kind.label(), ctl.path().display());
        if !server.kind.clears_routes_on_default_move() {
            log::info!("pulse: {} pins streams moved back to the default output", server.kind.label());
        }
        let mut warnings = BTreeMap::new();
        let sinks = control::sinks(&mut ctl)?;
        let output = match &config.output {
            Some(name) if sinks.iter().any(|s| s.name == *name && !s.is_capture()) => Some(name.clone()),
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
            output,
            sidechain: config.sidechain,
            ignore: config.sidechain_ignore.clone(),
            master: config.master.clone(),
            shared,
            tx,
            rx,
            ctl,
            kind: server.kind,
            journal,
            session: format!("{:016x}", random_u64()),
            sources: Vec::new(),
            orphans: Vec::new(),
            limiter: MoveLimiter::default(),
            default_sink: server.default_sink,
            last_real_default,
            last_default_fix: None,
            watchdog: None,
            events: None,
            engine: None,
            engine_generation: 0,
            engine_stuck: false,
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
            failed: false,
            torn_down: false,
            _active: active,
        })
    }

    /// Everything [`start`] must finish before returning.
    fn establish(&mut self) -> anyhow::Result<()> {
        // The watchdog first, so a crash at any point after a sink is loaded is undone.
        match Watchdog::spawn(self.journal.path(), &self.session) {
            Ok(w) => self.watchdog = Some(w),
            Err(e) => {
                log::warn!("pulse: {e:#}");
                self.warnings.insert(
                    "watchdog",
                    "Crash protection is off: if Sidestage is killed, restart it to get the apps' sound back."
                        .to_owned(),
                );
            }
        }
        // The journal names this session's owner from the start, so a second Sidestage process
        // is refused even before the first sink exists.
        self.save_journal()?;
        self.events = Some(EventWatch::start(self.tx.clone())?);
        // Output first, then the sources: an app is never parked without a listener.
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
                        .chain(self.sources.iter().map(|s| s.retry_at))
                        .flatten()
                        .fold(self.next_periodic, Instant::min)
                };
                self.rx.recv_timeout(wake.saturating_duration_since(Instant::now()))
            };
            match msg {
                Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(Msg::SetSources(_)) if self.failed => {
                    // A list sent as the session failed: its new entries ended with it.
                    mark_sources_stopped(&mut lock(&self.shared.status));
                    continue;
                }
                Ok(_) if self.failed => continue,
                Ok(Msg::Event(event)) => self.on_event(event),
                Ok(Msg::EventsLost(reason)) => self.on_server_lost(&reason),
                Ok(Msg::EngineEnded { generation, reason }) => self.on_engine_ended(generation, &reason),
                Ok(Msg::CaptureEnded { source, generation, reason }) => {
                    self.on_capture_ended(source, generation, &reason)
                }
                Ok(Msg::SetSources(list)) => self.set_sources(list),
                Ok(Msg::SetIgnore(apps)) => {
                    log::info!("pulse: {} app(s) never count as the game", apps.len());
                    self.ignore = apps;
                    self.reconcile_soon();
                }
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
                    lock(&self.shared.status).warnings = self.warnings.values().cloned().collect();
                }
            }
        }
        let result = self.teardown();
        let mut status = lock(&self.shared.status);
        status.state = SessionState::Stopped;
        mark_sources_stopped(&mut status);
        result
    }

    /// Does whatever is due.
    fn tick(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        if self.lost_since.is_some() {
            return if now >= self.next_reconnect { self.reconnect() } else { Ok(()) };
        }
        if let Some(engine) = self.engine.as_mut() {
            engine.mixer.collect();
        }
        if std::mem::take(&mut self.engine_stuck) {
            let generation = self.engine_generation;
            self.on_engine_ended(generation, "the audio engine stopped taking new sources");
        }
        if self.engine_retry_at.is_some_and(|t| now >= t) {
            self.engine_retry_at = None;
            // A reconnect may have started one meanwhile.
            if self.engine.is_none() {
                match self.start_engine() {
                    Ok(()) => self.setup_sources(None)?,
                    Err(e) if is_connection_error(&e) => return Err(e),
                    Err(e) => {
                        log::warn!("pulse: starting the output again failed: {e:#}");
                        self.note_failure(&format!("{e:#}"));
                        if !self.failed {
                            self.engine_retry_at = Some(now + Duration::from_secs(1));
                        }
                    }
                }
                self.mark_dirty(now);
            }
        }
        // A failure above may have ended the session (and undone everything).
        if self.failed {
            return Ok(());
        }
        self.setup_sources(Some(now))?;
        let periodic = now >= self.next_periodic;
        if periodic {
            self.retry_orphans()?;
        }
        if self.dirty_since.is_some_and(|t| now >= t + DEBOUNCE) || periodic {
            self.reconcile()?;
        }
        Ok(())
    }

    fn mark_dirty(&mut self, now: Instant) {
        self.dirty_since.get_or_insert(now);
    }

    /// Reconcile on this wake-up, without the event debounce (a change the app asked for).
    fn reconcile_soon(&mut self) {
        let now = Instant::now();
        self.dirty_since = Some(now.checked_sub(DEBOUNCE).unwrap_or(now));
    }

    fn on_event(&mut self, event: SubscriptionEvent) {
        use SubscriptionEventFacility as F;
        if matches!(event.event_facility, F::SinkInput | F::Sink | F::Server) {
            if event.event_facility == F::Sink
                && event.event_type == SubscriptionEventType::Removed
                && let Some(src) =
                    self.sources.iter().find(|s| s.sink.as_ref().is_some_and(|c| Some(c.index) == event.index))
            {
                log::warn!("pulse: the capture sink of source {} ({}) was removed", src.id, src.app.0);
            }
            self.mark_dirty(Instant::now());
        }
    }

    // --- Sources ---------------------------------------------------------------------------

    /// Applies a new source list: stops the sources it drops or changes (first, so a re-added
    /// app is free to be captured again), keeps the others untouched, and queues the new ones
    /// for setup, which follows on this same wake-up.
    fn set_sources(&mut self, list: Vec<NewSource>) {
        let diff = {
            let current: Vec<Identity<'_>> = self.sources.iter().map(Source::identity).collect();
            let wanted: Vec<Identity<'_>> = list.iter().map(|n| Identity::of(&n.config)).collect();
            sources::diff(&current, &wanted)
        };
        for id in diff.remove {
            if let Some(i) = self.sources.iter().position(|s| s.id == id) {
                log::info!("pulse: removing source {id} ({})", self.sources[i].app.0);
                self.remove_source(i);
            }
        }
        let mut list: Vec<Option<NewSource>> = list.into_iter().map(Some).collect();
        for i in diff.add {
            if let Some(new) = list[i].take() {
                self.add_source(new);
            }
        }
        self.reconcile_soon();
    }

    fn add_source(&mut self, new: NewSource) {
        let NewSource { config, live } = new;
        log::info!("pulse: adding source {} ({})", config.id, config.app.0);
        let mut warnings = BTreeMap::new();
        if let Some(w) = sources::hide_warning(&config.hide) {
            warnings.insert("hide", w.to_owned());
        }
        let error = (self.sources.len() >= MAX_SOURCES)
            .then(|| format!("Sidestage can play at most {MAX_SOURCES} apps at once; remove one to add this one."));
        self.sources.push(Source {
            id: config.id,
            app_name: config.app.0.clone(),
            app: config.app,
            hide: config.hide,
            factory: config.processor,
            live,
            sink_name: String::new(),
            module: None,
            sink: None,
            pipeline: None,
            tracked: BTreeMap::new(),
            unmovable: BTreeSet::new(),
            warnings,
            stream_warnings: BTreeMap::new(),
            failures: Vec::new(),
            retry_at: error.is_none().then(Instant::now),
            error,
        });
    }

    /// Stops a source and gives its app its own output back.
    fn remove_source(&mut self, i: usize) {
        self.stop_pipeline(i);
        self.release_sink(i);
        self.sources.remove(i);
    }

    /// Sets up what is missing of each source: every source, or with `due`, those whose retry
    /// time has come. A source that fails is retried later or, after too many failures, given
    /// up, without affecting the others; only a connection failure is returned.
    fn setup_sources(&mut self, due: Option<Instant>) -> anyhow::Result<()> {
        // After a teardown nothing may load a sink again: no stop or Drop would remove it.
        if self.failed || self.torn_down {
            return Ok(());
        }
        for i in 0..self.sources.len() {
            let src = &mut self.sources[i];
            if src.error.is_some() || due.is_some_and(|now| src.retry_at.is_none_or(|t| now < t)) {
                continue;
            }
            src.retry_at = None;
            if let Err(e) = self.setup_source(i) {
                if is_connection_error(&e) {
                    self.sources[i].retry_at = Some(Instant::now());
                    return Err(e);
                }
                if control::is_module_loading_denied(&e) {
                    let message = format!("Sidestage cannot capture {}: {e:#}.", self.sources[i].app_name);
                    self.fail_source(i, message);
                } else {
                    self.count_source_failure(i, &format!("{e:#}"), SETUP_RETRY);
                }
            }
            self.mark_dirty(Instant::now());
        }
        Ok(())
    }

    fn setup_source(&mut self, i: usize) -> anyhow::Result<()> {
        if self.sources[i].sink.is_none() {
            self.create_capture_sink(i)?;
        }
        if self.sources[i].pipeline.is_none() && self.engine.is_some() {
            self.start_pipeline(i)?;
        }
        self.sources[i].warnings.remove("setup");
        Ok(())
    }

    /// Loads a fresh capture sink for source `i`, journaling its name before the load so a
    /// crash in between still leaves a record the sweep can act on.
    ///
    /// Whatever is left of the source's previous sink is removed first, while the journal still
    /// names it: a load whose sink never appeared leaves a module that the new name would
    /// otherwise orphan (it would linger in sound settings, and stream-restore could route apps
    /// to it).
    fn create_capture_sink(&mut self, i: usize) -> anyhow::Result<()> {
        if !self.sources[i].sink_name.is_empty() {
            let old = self.sources[i].record();
            for line in control::restore_sink(&mut self.ctl, &old.sink_name, &old.streams)? {
                log::info!("pulse: {line}");
            }
        }
        let name = modargs::sink_name(random_u64());
        let src = &mut self.sources[i];
        src.sink_name.clone_from(&name);
        src.module = None;
        let label = src.app_name.clone();
        self.save_journal()?;
        let module = control::load_capture_sink(&mut self.ctl, &self.kind, &name, &label)?;
        self.sources[i].module = Some(module);
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
        log::info!("pulse: capture sink {name} is sink #{} (source {})", sink.index, self.sources[i].id);
        self.sources[i].sink = Some(CaptureSink { monitor: format!("{name}.monitor"), name, index: sink.index });
        Ok(())
    }

    /// Starts recording source `i`'s sink and adds it to the output engine's mix.
    fn start_pipeline(&mut self, i: usize) -> anyhow::Result<()> {
        let Some(engine) = self.engine.as_mut() else {
            return Ok(());
        };
        let src = &mut self.sources[i];
        let monitor = src.sink.as_ref().map(|c| c.monitor.clone()).context("the capture output is missing")?;
        // Off the real-time thread, as the contract requires.
        let processor = build_processor(&src.factory)?;
        let (producer, consumer, stats) = bridge::bridge(BridgeConfig::new(RATE, BRIDGE_TARGET_MS));
        self.generation += 1;
        let crashed = Arc::new(AtomicBool::new(false));
        // The capture has a connection of its own, so its failure (a refused or timed-out
        // connection, e.g. at PulseAudio's client limit) is this source's alone: flattened, it
        // no longer reads as a lost server. A server that really went away is noticed by the
        // event connection.
        let capture = Capture::start(&monitor, producer, crashed.clone(), self.tx.clone(), src.id, self.generation)
            .map_err(|e| anyhow!("{e:#}"))?;
        let slot = SourceSlot::new(src.id, consumer, processor, src.live.capture_peak.clone(), crashed);
        if engine.mixer.add(slot).is_err() {
            self.engine_stuck = true;
            bail!("the audio engine is not taking new sources");
        }
        *lock(&src.live.bridge) = Some(stats.clone());
        src.live.capture_latency_ms.store(capture.latency_ms.to_bits(), Ordering::Relaxed);
        src.pipeline = Some(Pipeline { capture, generation: self.generation, stats });
        Ok(())
    }

    /// Takes source `i` out of the mix and stops its capture.
    fn stop_pipeline(&mut self, i: usize) {
        let src = &mut self.sources[i];
        let Some(pipeline) = src.pipeline.take() else {
            return;
        };
        if let Some(engine) = self.engine.as_mut()
            && !engine.mixer.remove(src.id)
        {
            // Practically impossible (the queue holds every source twice over); a restart of
            // the engine drops the slot instead.
            self.engine_stuck = true;
        }
        drop(pipeline.capture);
        src.live.earlier_dropouts.fetch_add(pipeline.stats.dropouts(), Ordering::Relaxed);
        *lock(&src.live.bridge) = None;
        src.live.capture_latency_ms.store(0, Ordering::Relaxed);
    }

    /// Moves source `i`'s app back to its own output and unloads its capture sink. If that
    /// cannot finish now, the sink stays in the journal as an orphan and is retried.
    fn release_sink(&mut self, i: usize) {
        let src = &mut self.sources[i];
        if src.sink_name.is_empty() {
            return;
        }
        let record = src.record();
        src.sink_name.clear();
        src.module = None;
        src.sink = None;
        src.tracked.clear();
        src.unmovable.clear();
        src.stream_warnings.clear();
        // The journal still lists the record until the restore is done.
        match control::restore_sink(&mut self.ctl, &record.sink_name, &record.streams) {
            Ok(lines) => {
                for line in lines {
                    log::info!("pulse: {line}");
                }
            }
            Err(e) => {
                log::warn!("pulse: undoing capture sink {} failed, will retry: {e:#}", record.sink_name);
                self.orphans.push(record);
            }
        }
        if let Err(e) = self.save_journal() {
            log::warn!("pulse: {e:#}");
        }
    }

    /// Counts a failure of source `i`; too many in a minute give it up with `reason`, otherwise
    /// it is set up again after `retry_after`.
    fn count_source_failure(&mut self, i: usize, reason: &str, retry_after: Duration) {
        let now = Instant::now();
        let src = &mut self.sources[i];
        log::warn!("pulse: source {} ({}): {reason}", src.id, src.app.0);
        src.failures.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        src.failures.push(now);
        if src.failures.len() > MAX_FAILURES_PER_MINUTE {
            let message = format!("Sidestage's capture of {} keeps failing: {reason}", src.app_name);
            self.fail_source(i, message);
        } else {
            src.retry_at = Some(now + retry_after);
            src.warnings.insert("setup", format!("Setting up {} again after a problem: {reason}", src.app_name));
        }
    }

    /// Gives up source `i`: its app gets its own output back and the error stays in its status.
    /// The other sources keep playing.
    fn fail_source(&mut self, i: usize, message: String) {
        log::error!("pulse: {message}");
        self.stop_pipeline(i);
        self.release_sink(i);
        let src = &mut self.sources[i];
        src.warnings.remove("setup");
        src.retry_at = None;
        src.error = Some(message);
    }

    fn on_capture_ended(&mut self, source: SourceId, generation: u64, reason: &str) {
        let Some(i) = self
            .sources
            .iter()
            .position(|s| s.id == source && s.pipeline.as_ref().is_some_and(|p| p.generation == generation))
        else {
            return;
        };
        self.stop_pipeline(i);
        self.count_source_failure(i, reason, CAPTURE_RETRY);
        self.mark_dirty(Instant::now());
    }

    /// Source `i`'s capture sink vanished (someone unloaded it): the server has already moved
    /// the app's streams elsewhere. A new sink is made, and the next reconcile captures them
    /// again (their tracked origins are kept).
    fn capture_sink_lost(&mut self, i: usize) {
        log::warn!("pulse: capture sink {} of source {} disappeared", self.sources[i].sink_name, self.sources[i].id);
        // Its capture stream usually ended a moment earlier (killed with the sink) and that was
        // counted already: one loss is one failure.
        let counted = self.sources[i].pipeline.is_none() && self.sources[i].retry_at.is_some();
        self.stop_pipeline(i);
        self.sources[i].sink = None;
        if counted {
            self.sources[i].retry_at = Some(Instant::now());
        } else {
            self.count_source_failure(i, "its capture output keeps disappearing", Duration::ZERO);
        }
    }

    /// Retries undoing the sinks of removed sources.
    fn retry_orphans(&mut self) -> anyhow::Result<()> {
        if self.orphans.is_empty() {
            return Ok(());
        }
        let before = self.orphans.len();
        let mut left = Vec::new();
        let mut result = Ok(());
        for record in std::mem::take(&mut self.orphans) {
            if result.is_err() {
                left.push(record);
                continue;
            }
            match control::restore_sink(&mut self.ctl, &record.sink_name, &record.streams) {
                Ok(lines) => lines.into_iter().for_each(|l| log::info!("pulse: {l}")),
                Err(e) => {
                    left.push(record);
                    if is_connection_error(&e) {
                        result = Err(e);
                    } else {
                        log::warn!("pulse: {e:#}");
                    }
                }
            }
        }
        let changed = left.len() != before;
        self.orphans = left;
        if changed && let Err(e) = self.save_journal() {
            log::warn!("pulse: {e:#}");
        }
        result
    }

    // --- Output engine and meters -----------------------------------------------------------

    /// Starts a fresh output engine with no sources in its mix; [`Self::setup_sources`] then
    /// starts their captures again. A running engine is stopped first, so no source is left
    /// believing it plays in an engine that is gone.
    fn start_engine(&mut self) -> anyhow::Result<()> {
        self.stop_engine();
        let master = build_processor(&self.master)?;
        self.generation += 1;
        let engine = AudioEngine::start(
            self.output.as_deref(),
            master,
            self.shared.levels.clone(),
            self.tx.clone(),
            self.generation,
        )?;
        self.engine_generation = self.generation;
        self.shared.output_latency_ms.store(engine.latency_ms.to_bits(), Ordering::Relaxed);
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

    /// Stops the engine and every source's capture (their slots die with the engine).
    fn stop_engine(&mut self) {
        let Some(engine) = self.engine.take() else {
            return;
        };
        for i in 0..self.sources.len() {
            self.stop_pipeline(i);
        }
        drop(engine);
    }

    fn on_engine_ended(&mut self, generation: u64, reason: &str) {
        if generation != self.engine_generation || self.engine.is_none() {
            return;
        }
        self.stop_engine();
        self.note_failure(reason);
        if !self.failed {
            self.engine_retry_at = Some(Instant::now() + Duration::from_millis(300));
            self.set_state(SessionState::Starting);
        }
    }

    /// Counts an engine failure; too many in a minute end the session with `reason`.
    fn note_failure(&mut self, reason: &str) {
        let now = Instant::now();
        self.failures.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        self.failures.push(now);
        if self.failures.len() > MAX_FAILURES_PER_MINUTE {
            self.fail(format!("Sidestage's audio keeps failing: {reason}"));
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

    // --- Server loss ------------------------------------------------------------------------

    /// The events connection died: most likely the sound server restarted or crashed.
    fn on_server_lost(&mut self, reason: &str) {
        if self.failed || self.lost_since.is_some() {
            return;
        }
        log::warn!("pulse: lost the sound server: {reason}");
        self.meters = None;
        self.stop_engine();
        self.events = None;
        // Resuming starts the engine (and every source's capture) itself.
        self.engine_retry_at = None;
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
        let sinks = control::sinks(&mut ctl)?;
        self.ctl = ctl;
        // Stream indices restart with a restarted server.
        self.limiter = MoveLimiter::default();
        for src in &mut self.sources {
            let alive = src.sink.as_ref().is_some_and(|c| sinks.iter().any(|s| s.name == c.name));
            if !alive {
                // Gone with a restarted server (or unloaded meanwhile): nothing we knew about
                // the app's streams applies any more.
                src.sink = None;
                src.tracked.clear();
                src.unmovable.clear();
                src.stream_warnings.clear();
            }
        }
        self.events = Some(EventWatch::start(self.tx.clone())?);
        self.start_engine()?;
        self.setup_sources(None)?;
        if self.sidechain && self.meters.is_none() {
            self.start_meters();
        }
        self.retry_orphans()?;
        self.reconcile()
    }

    // --- Routing ----------------------------------------------------------------------------

    /// Brings the server to the wanted state and publishes the status.
    fn reconcile(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        self.dirty_since = None;
        self.next_periodic = now + PERIODIC;
        // After a teardown (a failure inside this same tick can cause one) nothing may move an
        // app again: no stop or Drop would undo it, since teardown runs only once.
        if self.failed || self.torn_down {
            return Ok(());
        }
        let server = control::server(&mut self.ctl)?;
        let sinks = control::sinks(&mut self.ctl)?;
        for i in 0..self.sources.len() {
            let Some(name) = self.sources[i].sink.as_ref().map(|c| c.name.clone()) else {
                continue;
            };
            match sinks.iter().find(|s| s.name == name) {
                Some(found) => {
                    if let Some(c) = self.sources[i].sink.as_mut() {
                        c.index = found.index;
                    }
                }
                None => self.capture_sink_lost(i),
            }
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
        let actions = {
            let engine_up = self.engine.is_some();
            let routes: Vec<Route<'_>> = self.sources.iter().filter_map(|s| s.route(engine_up)).collect();
            let world = World {
                streams: &streams,
                sinks: &sinks,
                default_sink: self.default_sink.as_deref(),
                fallback_sink: self.last_real_default.as_deref(),
                output_sink: self.output.as_deref(),
            };
            routing::plan(&world, &routes, &self.limiter, now)
        };
        if !actions.is_empty() {
            self.apply(actions, &streams, now)?;
            streams = control::streams(&mut self.ctl)?;
        }
        self.warnings.remove("server");
        self.publish(&streams, &sinks);
        Ok(())
    }

    fn apply(&mut self, actions: Vec<Action>, streams: &[Stream], now: Instant) -> anyhow::Result<()> {
        // Journal every stream before it is touched. The in-memory record only changes once the
        // journal holds it: otherwise, after a failed write, the next attempt would find nothing
        // new to journal and move the streams unrecorded.
        let pending: Vec<(SourceId, Moved)> = actions
            .iter()
            .filter_map(|a| match a {
                Action::Capture { source, moved } | Action::Adopt { source, moved } => {
                    let src = self.sources.iter().find(|s| s.id == *source)?;
                    (src.tracked.get(&moved.index) != Some(moved)).then(|| (*source, moved.clone()))
                }
                Action::Evict { .. } => None,
            })
            .collect();
        if !pending.is_empty() {
            self.write_journal(&pending)?;
            for (id, m) in pending {
                if let Some(src) = self.sources.iter_mut().find(|s| s.id == id) {
                    src.tracked.insert(m.index, m);
                }
            }
        }
        let mut refused = false;
        for action in actions {
            match action {
                Action::Adopt { source, moved: m } => {
                    log::info!("pulse: {} (#{}) is already on the capture sink of source {source}", m.app, m.index)
                }
                Action::Capture { source, moved: m } => {
                    let Some(i) = self.sources.iter().position(|s| s.id == source) else {
                        continue;
                    };
                    let Some(sink_name) = self.sources[i].sink.as_ref().map(|c| c.name.clone()) else {
                        continue;
                    };
                    let attempts = self.limiter.record(m.index, now);
                    match control::move_stream(&mut self.ctl, m.index, &sink_name) {
                        Ok(()) => {
                            log::info!("pulse: moved {} (#{}) from {} to {sink_name}", m.app, m.index, m.original_sink);
                            if attempts >= FIGHT_THRESHOLD {
                                self.sources[i].warnings.insert(
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
                            self.sources[i].tracked.remove(&m.index);
                            refused = true;
                        }
                        Err(CallError::Server(code)) => {
                            // DONT_MOVE or passthrough streams (PulseAudio), node.dont-reconnect
                            // streams (pipewire-pulse).
                            log::warn!("pulse: cannot move {} (#{}): {code}", m.app, m.index);
                            let src = &mut self.sources[i];
                            src.unmovable.insert(m.index);
                            src.tracked.remove(&m.index);
                            refused = true;
                            let title = streams
                                .iter()
                                .find(|s| s.index == m.index)
                                .and_then(|s| s.media_name.as_deref())
                                .map(|t| format!(" \u{201c}{t}\u{201d}"))
                                .unwrap_or_default();
                            src.stream_warnings.insert(
                                m.index,
                                format!(
                                    "{}'s stream{title} could not be moved to Sidestage (the app does not allow it), so you hear it unprocessed.",
                                    m.app
                                ),
                            );
                        }
                        Err(e) => return Err(e).context("moving an app to Sidestage"),
                    }
                }
                Action::Evict { index, to } => {
                    self.limiter.record(index, now);
                    let who = streams.iter().find(|s| s.index == index).map_or("A stream", Stream::label).to_owned();
                    match control::move_stream(&mut self.ctl, index, &to) {
                        Ok(()) => log::info!("pulse: moved {who} (#{index}) off a capture sink to {to}"),
                        Err(CallError::Server(code)) => {
                            log::warn!("pulse: cannot move {who} (#{index}) off a capture sink: {code}")
                        }
                        Err(e) => return Err(e).context("moving a stream off Sidestage's capture outputs"),
                    }
                }
            }
        }
        if refused {
            self.save_journal()?;
        }
        Ok(())
    }

    /// Follows the default sink. If the server made one of the capture sinks the default (the
    /// real device vanished), apps and our own output would feed back into it: switch back.
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
                log::warn!("pulse: the default output had become a capture sink; set it back to {}", real.name);
                self.default_sink = Some(real.name.clone());
                self.warnings.insert(
                    "default",
                    format!(
                        "Your system made a Sidestage capture output the default; Sidestage switched it back to {}.",
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
        let mut changed = false;
        for src in &mut self.sources {
            let before = src.tracked.len();
            src.tracked.retain(|i, _| alive.contains(i));
            src.unmovable.retain(|i| alive.contains(i));
            src.stream_warnings.retain(|i, _| alive.contains(i));
            changed |= src.tracked.len() != before;
        }
        self.limiter.retain(|i| alive.contains(&i));
        if changed && let Err(e) = self.save_journal() {
            log::warn!("pulse: {e:#}");
        }
    }

    fn publish(&mut self, streams: &[Stream], sinks: &[Sink]) {
        let engine_up = self.engine.is_some();
        let mut states = Vec::with_capacity(self.sources.len());
        for src in &mut self.sources {
            let app = routing::app_state(streams, src.sink.as_ref().map(|c| c.index), &src.app);
            if let Some(name) = &app.name {
                src.app_name.clone_from(name);
            }
            // A source that gave up has handed its app back: the original is heard.
            states.push((src.state(engine_up, &app), src.error.is_none() && app.captured == app.streams));
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
        if let Some(sink) = output_sink {
            // The engine mutes itself as soon as the server says it moved the stream; this
            // covers a server that does not say so (and unmutes after the move back).
            let into_capture = sink.is_capture();
            self.shared.levels.output_muted.store(into_capture, Ordering::Relaxed);
            if into_capture && routing::home_sink(sinks, None, None).is_none() {
                self.warnings.insert(
                    "feedback",
                    "No sound output is available, so Sidestage is muted until one is back.".to_owned(),
                );
            } else {
                self.warnings.remove("feedback");
            }
        }
        if let (Some(meters), Some(sink)) = (&self.meters, output_sink)
            && let Some(monitor) = sink.monitor_source
        {
            // The game is everything else: not the sources (whatever their state), not the
            // ignored apps, not Sidestage.
            let not_game: Vec<&AppKey> = self.sources.iter().map(|s| &s.app).chain(&self.ignore).collect();
            let targets: Vec<Target> = routing::meter_targets(streams, sink.index, &not_game, std::process::id())
                .into_iter()
                .map(|i| (i, monitor))
                .collect();
            if targets != self.meter_targets {
                meters.set_targets(targets.clone());
                self.meter_targets = targets;
            }
        }
        let mut status = lock(&self.shared.status);
        if self.failed {
            return;
        }
        status.state = if engine_up { SessionState::Running } else { SessionState::Starting };
        if let Some(sink) = output_sink
            && status.output_name != sink.label()
        {
            log::info!("pulse: playing to {} ({})", sink.label(), sink.name);
            status.output_name = sink.label().to_owned();
        }
        status.sidechain_active = self.meters.as_ref().is_some_and(MeterEngine::is_running);
        status.warnings = self.warnings.values().cloned().collect();
        for entry in &mut status.sources {
            let Some((src, (state, hidden))) =
                self.sources.iter().zip(&states).find(|(s, _)| Arc::ptr_eq(&s.live, &entry.live))
            else {
                continue; // queued, not applied yet
            };
            entry.state = state.clone();
            entry.original_hidden = *hidden;
            entry.app_name.clone_from(&src.app_name);
            entry.warnings = src.warnings.values().chain(src.stream_warnings.values()).cloned().collect();
        }
    }

    // --- Failure, journal, teardown ---------------------------------------------------------

    /// Ends the session with an error the user sees. Undoes the routing right away so the
    /// apps' sound comes back without waiting for the user to press stop.
    fn fail(&mut self, message: String) {
        if self.failed {
            return; // the first failure is the one worth showing
        }
        log::error!("pulse: {message}");
        self.failed = true;
        if let Err(e) = self.teardown() {
            log::error!("pulse: {e:#}");
        }
        let mut status = lock(&self.shared.status);
        status.state = SessionState::Error(message);
        mark_sources_stopped(&mut status);
    }

    fn set_state(&self, state: SessionState) {
        if !self.failed {
            lock(&self.shared.status).state = state;
        }
    }

    fn save_journal(&self) -> anyhow::Result<()> {
        self.write_journal(&[])
    }

    /// Writes every source's sink and the orphans, with `pending` moves added to their
    /// sources' streams.
    fn write_journal(&self, pending: &[(SourceId, Moved)]) -> anyhow::Result<()> {
        let mut sinks: Vec<SinkRecord> = self
            .sources
            .iter()
            .filter(|s| !s.sink_name.is_empty())
            .map(|s| {
                let mut record = s.record();
                for (_, m) in pending.iter().filter(|(id, _)| *id == s.id) {
                    record.streams.retain(|r| r.index != m.index);
                    record.streams.push(m.clone());
                }
                record
            })
            .collect();
        sinks.extend(self.orphans.iter().cloned());
        self.journal
            .save(&RoutingJournal { pid: std::process::id(), session: self.session.clone(), sinks })
            .context("writing the crash-recovery journal")
    }

    /// Stops the threads, moves every stream off every capture sink, unloads them and clears
    /// the journal. Runs once; later calls return Ok.
    fn teardown(&mut self) -> anyhow::Result<()> {
        if std::mem::replace(&mut self.torn_down, true) {
            return Ok(());
        }
        self.meters = None;
        self.stop_engine();
        self.events = None;
        let result = self.restore_all();
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
        self.shared.levels.sidechain.store(0.0);
        result
    }

    /// Undoes every source's sink and the orphans. What cannot be undone stays in the journal.
    fn restore_all(&mut self) -> anyhow::Result<()> {
        let mut records = std::mem::take(&mut self.orphans);
        for src in &mut self.sources {
            if !src.sink_name.is_empty() {
                records.push(src.record());
                src.sink_name.clear();
                src.module = None;
                src.sink = None;
                src.tracked.clear();
            }
        }
        let mut first_error: Option<anyhow::Error> = None;
        let mut reconnected = false;
        for record in records {
            if first_error.as_ref().is_some_and(is_connection_error) {
                // The server is unreachable: do not wait for it once per sink.
                self.orphans.push(record);
                continue;
            }
            let mut result = control::restore_sink(&mut self.ctl, &record.sink_name, &record.streams);
            if let Err(e) = &result
                && is_connection_error(e)
                && !reconnected
            {
                reconnected = true;
                log::warn!("pulse: retrying the restore on a new connection: {e:#}");
                result = Conn::connect("control").and_then(|ctl| {
                    self.ctl = ctl;
                    control::restore_sink(&mut self.ctl, &record.sink_name, &record.streams)
                });
            }
            match result {
                Ok(lines) => lines.into_iter().for_each(|l| log::info!("pulse: {l}")),
                Err(e) => {
                    log::warn!("pulse: undoing capture sink {}: {e:#}", record.sink_name);
                    self.orphans.push(record);
                    first_error.get_or_insert(e);
                }
            }
        }
        match first_error {
            None => Ok(()),
            Some(e) => {
                if let Err(j) = self.save_journal() {
                    log::error!("pulse: {j:#}");
                }
                Err(e)
            }
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if let Err(e) = self.teardown() {
            log::error!("pulse: {e:#}");
        }
    }
}

/// Runs a processor factory (the app's code) on the session thread. One that panics fails only
/// what it was building: one source (retried, then given up), or the output engine (retried,
/// then the session), instead of taking the session thread and every other source with it.
fn build_processor(factory: &ProcessorFactory) -> anyhow::Result<Box<dyn Processor>> {
    panic::catch_unwind(AssertUnwindSafe(|| factory(RATE)))
        .map_err(|_| anyhow!("Sidestage's audio processor could not be created (it crashed)"))
}

/// After the session ended, every source has stopped with it (their apps play normally again).
fn mark_sources_stopped(status: &mut Status) {
    for entry in &mut status.sources {
        if !matches!(entry.state, SessionState::Error(_)) {
            entry.state = SessionState::Stopped;
        }
        entry.original_hidden = false;
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
