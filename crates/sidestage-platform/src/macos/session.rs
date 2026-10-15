//! A running session. One control thread owns every Core Audio object: it builds the render
//! engine (which can block on macOS's capture prompt, so never on the UI thread), follows the
//! app's processes, the output device and its sample rate, and rebuilds when they change.
//! Listener blocks only post events to it; the UI reads status through a mutex and atomics.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::autoreleasepool;
use objc2_core_audio::{
    AudioObjectID, kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyNominalSampleRate,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices, kAudioHardwarePropertyProcessObjectList,
    kAudioObjectPropertyScopeGlobal,
};

use super::ca::{self, Listener, SYSTEM_OBJECT};
use super::devices;
use super::engine::{BuildError, RenderEngine, RenderSpec, SidechainEngine};
use super::group::{AppGroup, SelfInfo};
use super::plan::{self, TapScope};
use super::procs::{self, SysLookup};
use super::render::{RenderMeters, SidechainLevel};
use super::sys::OsVersion;
use crate::{HideMethod, ProcessorFactory, Session, SessionConfig, SessionState, SessionStatus};

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

/// Consecutive failed builds before the session gives up.
const MAX_FAILURES: u32 = 6;
/// How often the control thread re-checks processes, devices and warnings without an event.
const POLL: Duration = Duration::from_secs(1);
/// Listeners on the system object: default output, device list, process list.
const SYSTEM_LISTENERS: usize = 3;
/// How long `stop` waits for the control thread before leaving it to finish on its own.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// Wake-ups for the control thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Stop,
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

#[derive(Clone, Debug, Default)]
struct Status {
    state: SessionState,
    app_name: String,
    output_name: String,
    sample_rate: u32,
    original_hidden: bool,
    latency_ms: f32,
    sidechain_active: bool,
    warnings: Vec<String>,
    /// Since when the control thread has been in a call that can wait for the capture prompt.
    prompt_since: Option<Instant>,
}

struct Shared {
    status: Mutex<Status>,
    meters: Arc<RenderMeters>,
    sidechain: Arc<SidechainLevel>,
    stop: AtomicBool,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap_or_else(PoisonError::into_inner));
    }
}

pub struct MacSession {
    shared: Arc<Shared>,
    tx: Sender<Event>,
    done: Receiver<anyhow::Result<()>>,
    thread: Option<JoinHandle<()>>,
}

impl MacSession {
    /// Returns at once; the control thread does all Core Audio work.
    pub fn start(
        config: SessionConfig,
        factory: ProcessorFactory,
        permission: Arc<Permission>,
        me: SelfInfo,
        os: Option<OsVersion>,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared {
            status: Mutex::new(Status { app_name: provisional_name(&config.app.0), ..Status::default() }),
            meters: Arc::new(RenderMeters::default()),
            sidechain: Arc::new(SidechainLevel::default()),
            stop: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread_tx = tx.clone();
        let thread = std::thread::Builder::new()
            .name("sidestage-coreaudio".into())
            .spawn(move || {
                let status = thread_shared.clone();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    Control::new(config, factory, permission, me, os, thread_shared, thread_tx, rx).run()
                }))
                .unwrap_or_else(|_| {
                    // Unwinding dropped the engines, and with the tap the muting.
                    status.update(|s| {
                        s.state = SessionState::Error(
                            "Sidestage's audio thread hit an internal error. Start again; if it keeps \
                             happening, please report it."
                                .to_string(),
                        );
                        s.original_hidden = false;
                    });
                    Err(anyhow::anyhow!("the macOS audio thread panicked"))
                });
                let _ = done_tx.send(result);
            })
            .context("starting the audio control thread")?;
        Ok(Self { shared, tx, done, thread: Some(thread) })
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
        if s.state == SessionState::Starting && s.prompt_since.is_some_and(|t| t.elapsed() >= PROMPT_HINT_AFTER) {
            warnings.insert(0, PROMPT_HINT.to_string());
        }
        SessionStatus {
            state: s.state,
            app_name: s.app_name,
            output_name: s.output_name,
            sample_rate: s.sample_rate,
            original_hidden: s.original_hidden,
            capture_peak: self.shared.meters.capture.take(),
            sidechain_peak: self.shared.sidechain.meter.take(),
            sidechain_active: s.sidechain_active,
            latency_ms: s.latency_ms,
            dropouts: self.shared.meters.dropouts.load(Ordering::Relaxed),
            // Capture and output share one clock.
            drift_ppm: 0.0,
            warnings,
        }
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

/// A readable name before the app has been found: the bundle or file name of the key.
fn provisional_name(key: &str) -> String {
    let last = key.rsplit('/').find(|s| !s.is_empty()).unwrap_or(key);
    last.strip_suffix(".app").unwrap_or(last).to_string()
}

/// The control thread's state. Field order is drop order: listeners go before the engines
/// they watch, the sidechain before the render engine.
struct Control {
    engine_listeners: Vec<Listener>,
    system_listeners: Vec<Listener>,
    sidechain: Option<SidechainEngine>,
    engine: Option<RenderEngine>,
    queue: DispatchRetained<DispatchQueue>,
    config: SessionConfig,
    factory: ProcessorFactory,
    permission: Arc<Permission>,
    shared: Arc<Shared>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    me: SelfInfo,
    os: Option<OsVersion>,
    lookup: SysLookup,
    app: Option<AppGroup>,
    /// The tap kind last asked for, kept across rebuilds for hysteresis.
    scope: Option<TapScope>,
    /// The permission probe failed on a device-scoped tap: use mixdowns (the case the probe is
    /// proven on) for the rest of the session.
    mixdown_only: bool,
    /// Consecutive builds whose permission probe failed.
    probe_failures: u32,
    /// A (re)build is due at `next_attempt`.
    dirty: bool,
    /// Unrecoverable: wait for stop.
    fatal: bool,
    failures: u32,
    next_attempt: Instant,
    next_poll: Instant,
    next_sidechain_attempt: Instant,
    last_error: Option<String>,
    output_note: Option<String>,
    sidechain_note: Option<String>,
    /// Since when the app has been playing without a break, for the silence heuristic.
    playing_since: Option<Instant>,
    /// Problems from closing engines, reported by `stop`.
    teardown_errors: Vec<String>,
}

impl Control {
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: SessionConfig,
        factory: ProcessorFactory,
        permission: Arc<Permission>,
        me: SelfInfo,
        os: Option<OsVersion>,
        shared: Arc<Shared>,
        tx: Sender<Event>,
        rx: Receiver<Event>,
    ) -> Self {
        let now = Instant::now();
        Self {
            engine_listeners: Vec::new(),
            system_listeners: Vec::new(),
            sidechain: None,
            engine: None,
            queue: ca::listener_queue(),
            config,
            factory,
            permission,
            shared,
            tx,
            rx,
            me,
            os,
            lookup: SysLookup::default(),
            app: None,
            scope: None,
            mixdown_only: false,
            probe_failures: 0,
            dirty: true,
            fatal: false,
            failures: 0,
            next_attempt: now,
            next_poll: now + POLL,
            next_sidechain_attempt: now,
            last_error: None,
            output_note: None,
            sidechain_note: None,
            playing_since: None,
            teardown_errors: Vec::new(),
        }
    }

    fn run(mut self) -> anyhow::Result<()> {
        self.listen_system();
        'run: loop {
            if self.shared.stop.load(Ordering::Relaxed) {
                break;
            }
            let now = Instant::now();
            if self.dirty && !self.fatal && now >= self.next_attempt {
                autoreleasepool(|_| self.rebuild());
            }
            if now >= self.next_poll {
                self.next_poll = now + POLL;
                autoreleasepool(|_| self.poll());
            }
            let wake = if self.dirty && !self.fatal { self.next_attempt.min(self.next_poll) } else { self.next_poll };
            let timeout = wake.saturating_duration_since(Instant::now()).max(Duration::from_millis(10));
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
            s.original_hidden = false;
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
        if self.fatal {
            return;
        }
        match event {
            Event::Stop => {}
            Event::DefaultOutputChanged => {
                if self.follows_default() && self.default_output_moved() {
                    log::info!("default output changed");
                    self.request_rebuild();
                }
            }
            Event::DevicesChanged => {
                if let Some(uid) = self.config.output.as_deref() {
                    let connected = devices::device_for_uid(uid).is_some();
                    let using_it = self.output_note.is_none() && self.engine.is_some();
                    let substituting = self.output_note.is_some();
                    if (connected && substituting) || (!connected && using_it) {
                        log::info!("chosen output {}", if connected { "reconnected" } else { "disconnected" });
                        self.request_rebuild();
                    }
                }
            }
            Event::ProcessesChanged => self.follow_processes(),
            Event::EngineDeviceChanged => self.check_engine_devices(),
        }
    }

    /// Playing to the system default output: none was chosen, or the chosen one is missing.
    fn follows_default(&self) -> bool {
        self.config.output.is_none() || self.output_note.is_some()
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

    fn find_app(&mut self) -> anyhow::Result<Option<AppGroup>> {
        let groups = procs::app_groups(&self.me, &mut self.lookup)?;
        Ok(groups.into_iter().find(|g| g.key == self.config.app.0 && !g.objects.is_empty()))
    }

    fn set_waiting(&mut self) {
        self.app = None;
        // Waiting is not a failure: earlier failed attempts must not shorten the next start (a
        // probe that failed as the app quit says nothing about the permission either).
        self.failures = 0;
        self.probe_failures = 0;
        self.last_error = None;
        self.shared.update(|s| {
            s.state = SessionState::WaitingForApp;
            s.original_hidden = false;
            s.latency_ms = 0.0;
        });
    }

    /// Keeps the tap's process list equal to the app's process objects.
    fn follow_processes(&mut self) {
        if self.engine.is_none() {
            // Waiting for the app (or retrying): let the rebuild look again.
            if !self.dirty {
                self.request_rebuild();
            }
            return;
        }
        let group = match self.find_app() {
            Ok(g) => g,
            Err(e) => {
                log::warn!("{e:#}");
                return;
            }
        };
        let Some(group) = group else {
            log::info!("{} has no audio processes any more; waiting for it", self.config.app.0);
            self.drop_engines();
            self.set_waiting();
            return;
        };
        let Some(engine) = self.engine.as_mut() else {
            return;
        };
        if group.objects != engine.objects {
            log::info!("{} now has {} audio process(es); updating the taps", group.name, group.objects.len());
            if let Err(e) = engine.set_processes(&group.objects) {
                log::warn!("{e:#}; rebuilding");
                self.request_rebuild();
            }
            if let Some(sidechain) = self.sidechain.as_mut()
                && let Err(e) = sidechain.set_music(&group.objects)
            {
                log::warn!("{e:#}; rebuilding the sidechain");
                self.close_sidechain();
            }
        }
        self.app = Some(group);
    }

    /// Rebuilds when the engine's devices die or change rate. A running aggregate cannot be
    /// re-rated in place (its IOProc goes silent, FineTune found), and the processor was built
    /// for the old rate.
    fn check_engine_devices(&mut self) {
        let follows_default = self.follows_default();
        let Some(engine) = &mut self.engine else {
            return;
        };
        // A mixdown tap follows the device the app plays to. A rebuild cannot fix a rate it
        // does not match (macOS 14 does not resample taps; 15 does), so the new rate only
        // updates the warning. A device-scoped tap changes rate with its device, below.
        if let Some(rate) = engine.current_tap_rate()
            && plan::rate_changed(engine.tap_rate, rate)
        {
            log::info!("the app's audio switched from {} Hz to {rate} Hz", engine.tap_rate);
            engine.tap_rate = rate;
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

    fn rebuild(&mut self) {
        self.dirty = false;
        self.drop_engines();
        let group = match self.find_app() {
            Ok(g) => g,
            Err(e) => return self.fail(e),
        };
        let Some(group) = group else {
            return self.set_waiting();
        };
        let (output, note) = match devices::resolve(self.config.output.as_deref()) {
            Ok(r) => r,
            Err(e) => return self.fail(e.context("finding the output device")),
        };
        self.output_note = note;
        let (to_output, elsewhere) = procs::device_usage(&group.objects, output.id);
        let is_default = devices::default_output().is_ok_and(|id| id == output.id);
        let scope = if self.mixdown_only {
            TapScope::Mixdown
        } else {
            plan::choose_scope(to_output, elsewhere, self.scope, is_default)
        };
        let mute = !matches!(self.config.hide, HideMethod::KeepOriginal);
        self.shared.update(|s| {
            s.state = SessionState::Starting;
            s.app_name = group.name.clone();
            s.output_name = output.name.clone();
        });

        let shared = self.shared.clone();
        let prompt_window = move |open: bool| {
            shared.update(|s| s.prompt_since = open.then(Instant::now));
        };
        let spec = RenderSpec {
            prompt_window: &prompt_window,
            objects: &group.objects,
            output: &output,
            scope,
            plays_only_to_output: to_output && !elsewhere,
            mute,
            factory: &self.factory,
            meters: self.shared.meters.clone(),
            sidechain: self.config.sidechain.then(|| self.shared.sidechain.clone()),
            cancel: &self.shared.stop,
        };
        match RenderEngine::build(spec) {
            Ok(engine) => {
                self.permission.set_granted();
                self.failures = 0;
                self.probe_failures = 0;
                self.last_error = None;
                self.scope = Some(engine.wanted_scope);
                self.listen_engine(engine.aggregate_id(), engine.output.id);
                self.shared.update(|s| {
                    s.state = SessionState::Running;
                    s.sample_rate = engine.sample_rate;
                    s.original_hidden = engine.muted;
                });
                self.engine = Some(engine);
                self.app = Some(group);
                self.try_sidechain();
                self.update_status();
            }
            Err(BuildError::Cancelled) => {}
            Err(BuildError::ProbeFailed { error, scope }) => self.probe_failed(error, scope),
            Err(BuildError::PromptTimedOut(e)) => {
                log::warn!("the audio capture prompt timed out: {e:#}");
                // Reported through support() too, so the app does not retry into a Core Audio that
                // refuses until Sidestage restarts.
                self.permission.set_timed_out();
                self.give_up(PROMPT_TIMED_OUT.to_string());
            }
            Err(BuildError::Failed(e)) => self.fail(e),
        }
    }

    fn probe_failed(&mut self, error: anyhow::Error, scope: TapScope) {
        self.probe_failures += 1;
        log::warn!(
            "audio capture permission probe failed on a {scope:?} tap (attempt {}): {error:#}",
            self.probe_failures
        );
        if scope == TapScope::Device {
            // The probe is only proven on mixdown taps.
            self.mixdown_only = true;
        }
        if plan::probe_failure_is_denial(self.probe_failures, self.permission.granted()) {
            self.permission.set_denied();
            self.give_up(PERMISSION_DENIED.to_string());
        } else {
            self.fail(error.context("macOS did not confirm audio capture"));
        }
    }

    /// Ends the session's attempts with `message` for the user; the original stays audible.
    fn give_up(&mut self, message: String) {
        self.fatal = true;
        self.dirty = false;
        self.shared.update(|s| {
            s.state = SessionState::Error(message);
            s.original_hidden = false;
        });
    }

    fn fail(&mut self, e: anyhow::Error) {
        self.failures += 1;
        let message = format!("{e:#}");
        log::warn!("starting capture failed (attempt {}): {message}", self.failures);
        if self.failures >= MAX_FAILURES {
            self.give_up(format!("Sidestage could not start: {message}"));
        } else {
            self.dirty = true;
            self.next_attempt = Instant::now() + plan::retry_delay(self.failures);
            self.last_error = Some(message);
            self.shared.update(|s| s.state = SessionState::Starting);
            self.update_status();
        }
    }

    fn try_sidechain(&mut self) {
        if !self.config.sidechain || self.sidechain.is_some() || Instant::now() < self.next_sidechain_attempt {
            return;
        }
        let (Some(engine), Some(app)) = (&self.engine, &self.app) else {
            return;
        };
        // Without excluding itself the ducker would hear the spatialized music. Sidestage is a
        // HAL client once its render device runs, so this resolves right after a build.
        let Some(own) = procs::process_object_for_pid(self.me.pid) else {
            self.sidechain_note = Some("Ducking starts in a moment.".to_string());
            self.next_sidechain_attempt = Instant::now() + Duration::from_secs(2);
            return;
        };
        match SidechainEngine::build(
            &app.objects,
            own,
            f64::from(engine.sample_rate),
            self.shared.sidechain.clone(),
            &self.shared.stop,
        ) {
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

    fn drop_engines(&mut self) {
        self.engine_listeners.clear();
        self.close_sidechain();
        if let Some(engine) = self.engine.take() {
            if let Err(e) = engine.close() {
                log::warn!("{e:#}");
                self.teardown_errors.push(format!("{e:#}"));
            }
            self.shared.update(|s| s.original_hidden = false);
        }
    }

    /// Once a second: missed notifications, tap scope, sidechain retries, status.
    fn poll(&mut self) {
        if self.fatal {
            return;
        }
        if self.engine.is_some() {
            self.follow_processes();
            self.check_engine_devices();
        } else if !self.dirty && self.system_listeners.len() < SYSTEM_LISTENERS {
            // Waiting for the app without a process-list listener: look again every second.
            self.request_rebuild();
        }
        if let (Some(engine), Some(app), false) = (&self.engine, &self.app, self.mixdown_only) {
            let (to_output, elsewhere) = procs::device_usage(&app.objects, engine.output.id);
            let wanted = plan::choose_scope(to_output, elsewhere, Some(engine.wanted_scope), false);
            if wanted != engine.wanted_scope {
                log::info!(
                    "{} now plays {}; switching to a {wanted:?} tap",
                    app.name,
                    if to_output { "to the output" } else { "elsewhere" }
                );
                self.scope = Some(wanted);
                self.request_rebuild();
            }
        }
        self.try_sidechain();
        self.update_status();
    }

    fn update_status(&mut self) {
        let mut warnings = Vec::new();
        if let Some(note) = &self.output_note {
            warnings.push(note.clone());
        }
        if let HideMethod::ParkOn(_) = self.config.hide {
            warnings.push(
                "Moving the app to another output is a Windows feature; macOS mutes the original instead.".to_string(),
            );
        }
        if let Some(error) = &self.last_error
            && self.engine.is_none()
        {
            warnings.push(format!("Retrying: {error}"));
        }
        let mut latency_ms = 0.0;
        if let Some(engine) = &self.engine {
            let rate = f64::from(engine.sample_rate.max(1));
            let app_name = self.app.as_ref().map_or("The app", |a| a.name.as_str());
            if self.config.sidechain
                && let Some(note) = &self.sidechain_note
            {
                warnings.push(note.clone());
            }
            let silent = self.shared.meters.silent_frames.load(Ordering::Relaxed) as f64 / rate;
            let playing = self.app.as_ref().is_some_and(|a| a.objects.iter().any(|&o| procs::running_output(o)));
            let now = Instant::now();
            self.playing_since = if playing { self.playing_since.or(Some(now)) } else { None };
            let playing_secs = self.playing_since.map_or(0.0, |t| now.duration_since(t).as_secs_f64());
            if plan::looks_like_denied_capture(playing_secs, silent) {
                warnings.push(format!(
                    "Sidestage receives only silence from {app_name} although it is playing. If you hear nothing, \
                     allow Sidestage in System Settings > Privacy & Security > Screen & System Audio Recording."
                ));
            }
            if engine.scope == TapScope::Mixdown
                && self.os.is_some_and(|v| v < OsVersion::TAP_RESAMPLING)
                && plan::rate_changed(engine.nominal_rate, engine.tap_rate)
            {
                warnings.push(format!(
                    "macOS 14 cannot convert {app_name}'s audio from {} Hz to {} Hz, so it may play at the wrong speed. \
                     Play {app_name} to {} or update to macOS 15.",
                    engine.tap_rate, engine.sample_rate, engine.output.name
                ));
            }
            let measured = self.shared.meters.latency_frames.load(Ordering::Relaxed);
            let frames = if measured > 0 { measured } else { engine.fallback_latency_frames };
            latency_ms = (f64::from(frames) * 1000.0 / rate) as f32;
        }
        let sidechain_active = self.engine.is_some() && self.sidechain.is_some();
        self.shared.update(|s| {
            s.warnings = warnings;
            s.latency_ms = latency_ms;
            s.sidechain_active = sidechain_active;
        });
    }
}
