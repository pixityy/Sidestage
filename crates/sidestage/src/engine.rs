//! The engine: one background thread that owns the OS backend ([`Platform`]) and the running
//! [`Session`], so the UI thread never waits on a platform call.
//!
//! The UI says what it wants ([`Desired`]: power on/off, which app, output, hide method,
//! sidechain) and reads a [`Snapshot`] of what is going on. The engine reconciles the two: it
//! starts a session when the chosen app shows up, restarts it when the output or hide method
//! changes, stops it when switched off, and always stops it (restoring the original output)
//! before it exits. Sound settings do not go through here at all: the UI writes them straight to
//! the shared [`SharedParams`] the renderer reads.

use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sidestage_dsp::SharedParams;
use sidestage_platform::{
    AppKey, AudioApp, HideMethod, OutputDevice, Platform, ProcessorFactory, Session, SessionConfig, SessionState,
    SessionStatus, Support,
};

use crate::config::{ChosenApp, same_app};

/// How often the app list is refreshed.
const APPS_EVERY: Duration = Duration::from_secs(1);
/// How often output devices, hide methods and support are refreshed.
#[cfg(not(test))]
const OUTPUTS_EVERY: Duration = Duration::from_secs(5);
#[cfg(test)]
const OUTPUTS_EVERY: Duration = Duration::from_millis(100);
/// How often a running session's status is read for the UI.
const STATUS_EVERY: Duration = Duration::from_millis(100);
/// Automatic retries after a failed start (or a session that failed soon after starting): delay
/// before attempt 2, 3, 4. After that the error stays until the user retries or changes something.
#[cfg(not(test))]
const RETRY_DELAYS: [Duration; 3] = [Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(45)];
#[cfg(test)]
const RETRY_DELAYS: [Duration; 3] =
    [Duration::from_millis(300), Duration::from_millis(600), Duration::from_millis(900)];
/// A session that has been running this long counts as healthy: its next failure starts the
/// retry count from scratch. Before that, a session that starts and then fails counts as a failed
/// attempt, so a backend that keeps failing right after starting is not restarted forever.
#[cfg(not(test))]
const HEALTHY_AFTER: Duration = Duration::from_secs(30);
#[cfg(test)]
const HEALTHY_AFTER: Duration = Duration::from_secs(2);
/// An app missing from the list for this long was quit; when it comes back, a failed start gets
/// a fresh set of attempts.
const APP_GONE_RESET: Duration = Duration::from_secs(3);

/// Creates the backend on the engine thread (backends may set up per-thread OS state).
pub type PlatformMaker = Box<dyn FnOnce() -> anyhow::Result<Box<dyn Platform>> + Send>;
/// Called after the snapshot changed, e.g. to ask egui for a repaint.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// What the user asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct Desired {
    pub enabled: bool,
    pub app: Option<ChosenApp>,
    pub output: Option<String>,
    pub hide: HideMethod,
    pub sidechain: bool,
}

/// Long-running work in progress, for progress indicators.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Activity {
    #[default]
    Idle,
    Starting,
    Stopping,
    Restoring,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warning,
    Error,
}

/// A message for the user that stays until dismissed.
#[derive(Clone, Debug)]
pub struct Notice {
    pub id: u64,
    pub kind: NoticeKind,
    pub title: String,
    pub lines: Vec<String>,
}

/// Everything the UI shows about the backend and the session.
#[derive(Clone, Default)]
pub struct Snapshot {
    /// Changes whenever anything else in the snapshot changed.
    pub version: u64,
    /// The backend has been created (or failed to be).
    pub ready: bool,
    pub platform_name: String,
    /// The backend could not be created at all.
    pub platform_error: Option<String>,
    pub support: Option<Support>,
    pub apps: Vec<AudioApp>,
    pub apps_error: Option<String>,
    pub outputs: Vec<OutputDevice>,
    pub hide_methods: Vec<(HideMethod, String)>,
    pub activity: Activity,
    /// Status of the running session, `None` when there is none.
    pub session: Option<SessionStatus>,
    /// The app the running session captures.
    pub session_app: Option<ChosenApp>,
    pub session_since: Option<Instant>,
    /// Why the last start failed or the session ended with an error. Cleared on a new attempt.
    pub error: Option<String>,
    /// True while an automatic retry of a failed start is scheduled.
    pub retry_pending: bool,
    pub notices: Vec<Notice>,
}

impl Snapshot {
    /// The app is in the app list (also under a newer key of the same app, see [`same_app`]).
    pub fn app_present(&self, key: &AppKey) -> bool {
        self.apps.iter().any(|a| same_app(&a.key, key))
    }
}

enum Command {
    SetDesired(Desired),
    Retry,
    /// Stop the session and run `Platform::recover`.
    Restore,
    Shutdown,
}

pub struct Engine {
    tx: Sender<Command>,
    shared: Arc<Mutex<Snapshot>>,
    done: Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn spawn(make_platform: PlatformMaker, params: Arc<SharedParams>, desired: Desired, waker: Waker) -> Engine {
        let (tx, rx) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let worker_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new().name("sidestage-engine".into()).spawn(move || {
            let _done = DoneGuard(done_tx);
            let panic_shared = Arc::clone(&worker_shared);
            let panic_waker = Arc::clone(&waker);
            let result = std::panic::catch_unwind(AssertUnwindSafe(move || {
                let worker = Worker::new(rx, worker_shared, waker, crate::processor::factory(params), desired);
                worker.run(make_platform);
            }));
            if result.is_err() {
                // The session (if any) was dropped while unwinding, which restores the original.
                log::error!("the engine thread panicked");
                let mut s = panic_shared.lock().unwrap_or_else(PoisonError::into_inner);
                // Also when it panicked while creating the backend, before `ready` was set.
                s.ready = true;
                s.platform_error = Some("Sidestage hit an internal error and stopped. Please restart it.".into());
                s.session = None;
                s.activity = Activity::Idle;
                s.version = s.version.wrapping_add(1);
                drop(s);
                panic_waker();
            }
        });
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                log::error!("could not start the engine thread: {e}");
                let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
                s.ready = true;
                s.platform_error = Some(format!("Could not start the audio engine: {e}"));
                s.version = s.version.wrapping_add(1);
                None
            }
        };
        Engine { tx, shared, done, thread }
    }

    /// Read the current snapshot without copying it.
    pub fn with_snapshot<R>(&self, f: impl FnOnce(&Snapshot) -> R) -> R {
        f(&self.shared.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Copy the snapshot into `into` if it changed since `into` was taken.
    pub fn refresh(&self, into: &mut Snapshot) {
        self.with_snapshot(|s| {
            if s.version != into.version {
                into.clone_from(s);
            }
        });
    }

    pub fn set_desired(&self, desired: Desired) {
        let _ = self.tx.send(Command::SetDesired(desired));
    }

    pub fn retry(&self) {
        let _ = self.tx.send(Command::Retry);
    }

    pub fn restore(&self) {
        let _ = self.tx.send(Command::Restore);
    }

    pub fn dismiss_notice(&self, id: u64) {
        let mut s = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        s.notices.retain(|n| n.id != id);
        s.version = s.version.wrapping_add(1);
    }

    /// Stop the session (restoring the original output) and end the engine thread, waiting at
    /// most `timeout`. Safe to call more than once.
    pub fn shutdown(&mut self, timeout: Duration) {
        let Some(thread) = self.thread.take() else { return };
        let _ = self.tx.send(Command::Shutdown);
        match self.done.recv_timeout(timeout) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                if thread.join().is_err() {
                    log::error!("the engine thread panicked while shutting down");
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // The crash guardian restores the original output if we exit before the backend
                // finished stopping.
                log::error!("the audio engine did not stop within {timeout:?}; exiting anyway");
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown(Duration::from_secs(5));
    }
}

/// Signals "engine thread finished" even when it unwinds.
struct DoneGuard(Sender<()>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

struct Running {
    key: RunKey,
    app: ChosenApp,
    session: Box<dyn Session>,
    started: Instant,
}

/// Everything a session is started with; a change means a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RunKey {
    app: AppKey,
    output: Option<String>,
    hide: HideMethod,
    sidechain: bool,
}

struct Failure {
    key: RunKey,
    attempts: usize,
    retry_at: Option<Instant>,
    /// Since when the app has been missing from the app list.
    gone_since: Option<Instant>,
}

struct Worker {
    rx: Receiver<Command>,
    shared: Arc<Mutex<Snapshot>>,
    waker: Waker,
    factory: ProcessorFactory,
    desired: Desired,
    platform: Option<Box<dyn Platform>>,
    support: Option<Support>,
    apps: Vec<AudioApp>,
    /// IDs of the last output list, to notice a device being plugged in.
    output_ids: Vec<String>,
    session: Option<Running>,
    failure: Option<Failure>,
    next_apps: Instant,
    next_outputs: Instant,
    next_status: Instant,
    next_notice: u64,
}

impl Worker {
    fn new(
        rx: Receiver<Command>,
        shared: Arc<Mutex<Snapshot>>,
        waker: Waker,
        factory: ProcessorFactory,
        desired: Desired,
    ) -> Self {
        let now = Instant::now();
        Worker {
            rx,
            shared,
            waker,
            factory,
            desired,
            platform: None,
            support: None,
            apps: Vec::new(),
            output_ids: Vec::new(),
            session: None,
            failure: None,
            next_apps: now,
            next_outputs: now,
            next_status: now,
            next_notice: 1,
        }
    }

    fn update(&self, f: impl FnOnce(&mut Snapshot)) {
        let mut s = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut s);
        s.version = s.version.wrapping_add(1);
        drop(s);
        (self.waker)();
    }

    fn notice(&mut self, kind: NoticeKind, title: impl Into<String>, lines: Vec<String>) {
        let notice = Notice { id: self.next_notice, kind, title: title.into(), lines };
        self.next_notice += 1;
        self.update(|s| s.notices.push(notice));
    }

    fn run(mut self, make_platform: PlatformMaker) {
        self.init(make_platform);
        loop {
            // Apply everything queued before doing slow platform work. This also runs before the
            // first tick: a window closed while the backend was being set up must not start a
            // session (and hide the app) only to stop it again.
            loop {
                match self.rx.try_recv() {
                    Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => {
                        self.stop_session();
                        return;
                    }
                    Ok(command) => self.handle(command),
                    Err(TryRecvError::Empty) => break,
                }
            }
            // The first tick starts right away if the chosen app is already playing.
            self.tick();
            let wait = self.next_deadline().saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(wait) {
                Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(command) => self.handle(command),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        self.stop_session();
    }

    fn init(&mut self, make_platform: PlatformMaker) {
        let mut platform = match make_platform() {
            Ok(p) => p,
            Err(e) => {
                log::error!("audio backend unavailable: {e:#}");
                self.update(|s| {
                    s.ready = true;
                    s.platform_error = Some(format!("{e:#}"));
                });
                return;
            }
        };
        let name = platform.name().to_owned();
        let support = platform.support();
        log::info!("backend: {name}; support: {support:?}");

        // Undo leftovers of a crashed run before anything else touches the audio setup.
        let recovered = platform.recover();
        self.support = Some(support.clone());
        self.platform = Some(platform);
        self.update(|s| {
            s.ready = true;
            s.platform_name = name;
            s.support = Some(support);
        });
        match recovered {
            Ok(lines) if !lines.is_empty() => {
                for line in &lines {
                    log::info!("recovered: {line}");
                }
                // Each line says what was put back, or what is put back once its app plays again.
                self.notice(NoticeKind::Info, "Undoing changes left by the last run", lines);
            }
            Ok(_) => {}
            Err(e) => {
                log::warn!("recover failed: {e:#}");
                self.notice(
                    NoticeKind::Warning,
                    "Couldn't check for changes left by the last run",
                    vec![format!("{e:#}"), "Use Restore in Diagnostics to try again.".into()],
                );
            }
        }
        self.refresh_outputs();
        self.refresh_apps();
    }

    fn next_deadline(&self) -> Instant {
        if self.platform.is_none() {
            return Instant::now() + Duration::from_secs(3600);
        }
        let mut t = self.next_apps.min(self.next_outputs);
        if self.session.is_some() {
            t = t.min(self.next_status);
        } else if let Some(f) = &self.failure
            && let Some(retry) = f.retry_at
            && self.may_start()
            && self.app_listed(&f.key.app)
        {
            // While the app is missing, the app-list poll wakes us instead. The floor keeps an
            // overdue retry that cannot start from turning into a busy loop.
            t = t.min(retry.max(Instant::now() + Duration::from_millis(20)));
        }
        t
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::SetDesired(desired) => {
                if desired != self.desired {
                    self.desired = desired;
                    self.failure = None;
                    self.update(|s| {
                        s.error = None;
                        s.retry_pending = false;
                    });
                }
            }
            Command::Retry => {
                self.failure = None;
                if self.session_failed().is_some() {
                    self.stop_session();
                }
                self.update(|s| {
                    s.error = None;
                    s.retry_pending = false;
                });
                // Pick up an app that just appeared without waiting for the next poll.
                self.next_apps = Instant::now();
            }
            Command::Restore => self.restore(),
            Command::Shutdown => {}
        }
    }

    fn tick(&mut self) {
        if self.platform.is_none() {
            return;
        }
        let now = Instant::now();
        if now >= self.next_outputs {
            self.refresh_outputs();
        }
        if now >= self.next_apps {
            self.refresh_apps();
        }
        self.reconcile();
        if self.session.is_some() && Instant::now() >= self.next_status {
            self.poll_status();
        }
    }

    fn app_listed(&self, key: &AppKey) -> bool {
        self.apps.iter().any(|a| same_app(&a.key, key))
    }

    /// Whether a new session may be started. Linux reports `Unsupported` while the sound server
    /// cannot be reached (it is restarting, or not up yet at login): nothing new is started
    /// then, but a running session is left alone, as it reconnects by itself.
    fn may_start(&self) -> bool {
        !matches!(self.support, Some(Support::Unsupported(_)))
    }

    fn refresh_apps(&mut self) {
        let Some(platform) = self.platform.as_mut() else { return };
        let result = platform.list_apps();
        self.next_apps = Instant::now() + APPS_EVERY;
        match result {
            Ok(apps) => {
                self.apps = apps.clone();
                self.update(|s| {
                    s.apps = apps;
                    s.apps_error = None;
                });
            }
            Err(e) => {
                log::debug!("list_apps failed: {e:#}");
                self.update(|s| s.apps_error = Some(format!("{e:#}")));
            }
        }
    }

    /// Outputs, hide methods and support. Support can change while running: macOS reports
    /// `NeedsPermission` once the user has denied audio capture, Linux `Unsupported` while the
    /// sound server cannot be reached.
    fn refresh_outputs(&mut self) {
        let Some(platform) = self.platform.as_mut() else { return };
        let outputs = platform.list_outputs();
        let hide_methods = platform.hide_methods();
        let support = platform.support();
        self.next_outputs = Instant::now() + OUTPUTS_EVERY;
        if self.support.as_ref() != Some(&support) {
            log::info!("support changed: {support:?}");
            self.support = Some(support.clone());
        }
        match outputs {
            Ok(outputs) => {
                let appeared = outputs.iter().any(|o| !self.output_ids.contains(&o.id));
                self.output_ids = outputs.iter().map(|o| o.id.clone()).collect();
                if appeared && self.session.is_none() && self.failure.take().is_some() {
                    // The device a failed start was missing (headphones plugged back in) may be
                    // there now: try again with a fresh set of attempts.
                    log::info!("an output device appeared; trying again");
                    self.update(|s| {
                        s.error = None;
                        s.retry_pending = false;
                    });
                }
                self.update(|s| {
                    s.outputs = outputs;
                    s.hide_methods = hide_methods;
                    s.support = Some(support);
                });
            }
            Err(e) => {
                log::debug!("list_outputs failed: {e:#}");
                self.update(|s| {
                    s.hide_methods = hide_methods;
                    s.support = Some(support);
                });
            }
        }
    }

    fn wanted(&self) -> Option<(RunKey, ChosenApp)> {
        if !self.desired.enabled {
            return None;
        }
        let app = self.desired.app.clone()?;
        let key = RunKey {
            app: app.key.clone(),
            output: self.desired.output.clone(),
            hide: self.desired.hide.clone(),
            sidechain: self.desired.sidechain,
        };
        Some((key, app))
    }

    /// The running session's error, if it has failed.
    fn session_failed(&self) -> Option<String> {
        let running = self.session.as_ref()?;
        let state = self.shared.lock().unwrap_or_else(PoisonError::into_inner).session.as_ref()?.state.clone();
        match state {
            SessionState::Error(message) => Some(message),
            SessionState::Stopped => Some(format!("Capturing {} stopped unexpectedly.", running.app.name)),
            _ => None,
        }
    }

    fn reconcile(&mut self) {
        let wanted = self.wanted();

        if let Some(running) = &self.session
            && wanted.as_ref().map(|(k, _)| k) != Some(&running.key)
        {
            self.stop_session();
        }

        if let Some(message) = self.session_failed() {
            let key = self.session.as_ref().map(|r| r.key.clone());
            log::warn!("session failed: {message}");
            self.stop_session();
            if let Some(key) = key {
                self.record_failure(key, message);
            }
        }

        let Some((key, app)) = wanted else {
            // Nothing should run: a scheduled retry no longer applies.
            if self.failure.take().is_some() {
                self.update(|s| {
                    s.error = None;
                    s.retry_pending = false;
                });
            }
            return;
        };
        if self.session.is_some() || !self.may_start() {
            return;
        }
        // macOS reports NeedsPermission once the user declined audio capture. Starting again
        // cannot work until they allow it in System Settings, so only "Try again" retries.
        if matches!(self.support, Some(Support::NeedsPermission(_)))
            && let Some(f) = &mut self.failure
            && f.retry_at.take().is_some()
        {
            self.update(|s| s.retry_pending = false);
        }
        if !self.app_listed(&key.app) {
            // Waiting for the app. Quitting and restarting it gets a fresh attempt; a list that
            // only flickers (streams coming and going) keeps the retry count.
            if let Some(f) = &mut self.failure {
                let gone = *f.gone_since.get_or_insert_with(Instant::now);
                if gone.elapsed() >= APP_GONE_RESET {
                    self.failure = None;
                    self.update(|s| {
                        s.error = None;
                        s.retry_pending = false;
                    });
                }
            }
            return;
        }
        if let Some(f) = &mut self.failure {
            f.gone_since = None;
        }
        if let Some(failure) = &self.failure
            && failure.key == key
        {
            match failure.retry_at {
                Some(t) if Instant::now() >= t => {}
                _ => return,
            }
        }
        self.start(key, app);
    }

    fn start(&mut self, key: RunKey, app: ChosenApp) {
        if self.platform.is_none() {
            return;
        }
        log::info!(
            "starting: {} ({}), output {:?}, hide {:?}, sidechain {}",
            app.name,
            key.app.0,
            key.output,
            key.hide,
            key.sidechain
        );
        self.update(|s| {
            s.activity = Activity::Starting;
            s.error = None;
            s.retry_pending = false;
        });
        let config = SessionConfig {
            app: key.app.clone(),
            output: key.output.clone(),
            hide: key.hide.clone(),
            sidechain: key.sidechain,
        };
        let factory = Arc::clone(&self.factory);
        let Some(platform) = self.platform.as_mut() else { return };
        let result = platform.start(config, factory);
        match result {
            Ok(session) => {
                let status = session.status();
                log::info!("session started: {:?}", status.state);
                // Keep counting attempts until the session has proven healthy (see HEALTHY_AFTER);
                // only the scheduled retry is used up.
                if let Some(f) = &mut self.failure {
                    f.retry_at = None;
                }
                self.session = Some(Running { key, app: app.clone(), session, started: Instant::now() });
                self.next_status = Instant::now() + STATUS_EVERY;
                self.update(|s| {
                    s.activity = Activity::Idle;
                    s.session = Some(status);
                    s.session_app = Some(app);
                    s.session_since = Some(Instant::now());
                });
            }
            Err(e) => {
                let message = format!("{e:#}");
                log::warn!("could not start capturing {}: {message}", app.name);
                self.update(|s| s.activity = Activity::Idle);
                self.record_failure(key, message);
            }
        }
    }

    fn record_failure(&mut self, key: RunKey, message: String) {
        let attempts = match &self.failure {
            Some(f) if f.key == key => f.attempts + 1,
            _ => 1,
        };
        let retry_at = RETRY_DELAYS.get(attempts - 1).map(|d| Instant::now() + *d);
        self.failure = Some(Failure { key, attempts, retry_at, gone_since: None });
        // A failure can change what the backend reports (a denied permission, a vanished device).
        self.next_outputs = Instant::now();
        self.update(|s| {
            s.error = Some(message);
            s.retry_pending = retry_at.is_some();
        });
    }

    fn poll_status(&mut self) {
        let Some(running) = &self.session else { return };
        let status = running.session.status();
        self.next_status = Instant::now() + STATUS_EVERY;
        if self.failure.is_some()
            && matches!(status.state, SessionState::Running | SessionState::WaitingForApp)
            && running.started.elapsed() >= HEALTHY_AFTER
        {
            self.failure = None;
        }
        self.update(|s| s.session = Some(status));
    }

    fn stop_session(&mut self) {
        let Some(running) = self.session.take() else { return };
        log::info!("stopping: {}", running.app.name);
        self.update(|s| s.activity = Activity::Stopping);
        let result = running.session.stop();
        self.update(|s| {
            s.activity = Activity::Idle;
            s.session = None;
            s.session_app = None;
            s.session_since = None;
        });
        if let Err(e) = result {
            log::error!("stopping {} failed: {e:#}", running.app.name);
            self.notice(
                NoticeKind::Error,
                format!("{} may not have been restored", running.app.name),
                vec![format!("{e:#}"), "Use Restore in Diagnostics to try again.".into()],
            );
        }
    }

    fn restore(&mut self) {
        self.stop_session();
        if self.platform.is_none() {
            return;
        }
        self.update(|s| s.activity = Activity::Restoring);
        let Some(platform) = self.platform.as_mut() else { return };
        let result = platform.recover();
        self.update(|s| s.activity = Activity::Idle);
        match result {
            Ok(lines) if lines.is_empty() => {
                self.notice(
                    NoticeKind::Info,
                    "Nothing to restore",
                    vec!["Your audio setup has no changes left by Sidestage.".into()],
                );
            }
            Ok(lines) => self.notice(NoticeKind::Info, "Restore finished", lines),
            Err(e) => self.notice(NoticeKind::Error, "Restore failed", vec![format!("{e:#}")]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::{DemoPlatform, Scenario};
    use sidestage_dsp::Settings;

    fn wait_for(engine: &Engine, what: &str, f: impl Fn(&Snapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if engine.with_snapshot(&f) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn starts_when_app_present_and_stops_when_disabled() {
        let params = SharedParams::new(Settings::default());
        let demo_params = Arc::clone(&params);
        let app = ChosenApp { key: AppKey("demo:spotify".into()), name: "Spotify".into() };
        let desired =
            Desired { enabled: true, app: Some(app.clone()), output: None, hide: HideMethod::Auto, sidechain: true };
        let mut engine = Engine::spawn(
            Box::new(move || {
                Ok(Box::new(DemoPlatform::new(Scenario::default(), Some(demo_params))) as Box<dyn Platform>)
            }),
            params,
            desired.clone(),
            Arc::new(|| {}),
        );
        wait_for(&engine, "session", |s| s.session.as_ref().is_some_and(|st| st.state == SessionState::Running));
        engine.set_desired(Desired { enabled: false, ..desired.clone() });
        wait_for(&engine, "stop", |s| s.session.is_none());
        engine.set_desired(Desired {
            app: Some(ChosenApp { key: AppKey("nope".into()), name: "Nope".into() }),
            ..desired
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(engine.with_snapshot(|s| s.session.is_none()), "must wait for an app that is not running");
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn failed_start_is_reported_and_recovered_lines_become_a_notice() {
        let scenario = Scenario { warnings: true, start_error: true, ..Scenario::default() };
        let app = ChosenApp { key: AppKey("demo:spotify".into()), name: "Spotify".into() };
        let desired = Desired { enabled: true, app: Some(app), output: None, hide: HideMethod::Auto, sidechain: false };
        let mut engine = Engine::spawn(
            Box::new(move || Ok(Box::new(DemoPlatform::new(scenario, None)) as Box<dyn Platform>)),
            SharedParams::new(Settings::default()),
            desired,
            Arc::new(|| {}),
        );
        wait_for(&engine, "start error", |s| s.error.is_some() && s.retry_pending && s.activity == Activity::Idle);
        let id = engine.with_snapshot(|s| {
            assert!(s.session.is_none());
            assert_eq!(s.notices.len(), 1, "recover() lines are shown once");
            assert_eq!(s.notices[0].kind, NoticeKind::Info);
            s.notices[0].id
        });
        engine.dismiss_notice(id);
        assert!(engine.with_snapshot(|s| s.notices.is_empty()));
        // Retry clears the error right away; the demo then fails again.
        engine.retry();
        wait_for(&engine, "second failure", |s| s.error.is_some());
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn session_failing_right_after_start_is_not_restarted_forever() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let starts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&starts);
        let scenario = Scenario { fail_after_start: true, ..Scenario::default() };
        let app = ChosenApp { key: AppKey("demo:spotify".into()), name: "Spotify".into() };
        let desired = Desired { enabled: true, app: Some(app), output: None, hide: HideMethod::Auto, sidechain: false };
        let mut engine = Engine::spawn(
            Box::new(move || {
                Ok(Box::new(DemoPlatform::new(scenario, None).with_start_counter(counter)) as Box<dyn Platform>)
            }),
            SharedParams::new(Settings::default()),
            desired,
            Arc::new(|| {}),
        );
        // Every start succeeds and then fails: one attempt plus the scheduled retries, then stop.
        wait_for(&engine, "retries to run out", |s| {
            s.error.is_some() && !s.retry_pending && s.session.is_none() && s.activity == Activity::Idle
        });
        std::thread::sleep(RETRY_DELAYS[RETRY_DELAYS.len() - 1] * 2);
        assert_eq!(starts.load(Ordering::Relaxed), 1 + RETRY_DELAYS.len());
        assert!(engine.with_snapshot(|s| s.session.is_none() && s.error.is_some()));
        // The user asking again gets a fresh attempt.
        engine.retry();
        wait_for(&engine, "manual retry", |_| starts.load(Ordering::Relaxed) == 2 + RETRY_DELAYS.len());
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn missing_backend_is_an_error_not_a_hang() {
        let desired = Desired { enabled: true, app: None, output: None, hide: HideMethod::Auto, sidechain: true };
        let mut engine = Engine::spawn(
            Box::new(|| Err(anyhow::anyhow!("no backend here"))),
            SharedParams::new(Settings::default()),
            desired,
            Arc::new(|| {}),
        );
        wait_for(&engine, "ready", |s| s.ready);
        assert_eq!(engine.with_snapshot(|s| s.platform_error.clone()), Some("no backend here".into()));
        let started = Instant::now();
        engine.shutdown(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1), "an idle engine stops at once");
    }

    #[test]
    fn panicking_backend_is_reported() {
        let desired = Desired { enabled: true, app: None, output: None, hide: HideMethod::Auto, sidechain: true };
        let mut engine = Engine::spawn(
            Box::new(|| panic!("backend exploded (expected in this test)")),
            SharedParams::new(Settings::default()),
            desired,
            Arc::new(|| {}),
        );
        wait_for(&engine, "error", |s| s.ready && s.platform_error.is_some());
        engine.shutdown(Duration::from_secs(5));
    }

    /// A backend the test steers while the engine runs.
    #[derive(Default)]
    struct Script {
        /// `None` = supported.
        support: Option<Support>,
        apps: Vec<AppKey>,
        outputs: Vec<String>,
        /// What running sessions report.
        state: SessionState,
        fail_start: bool,
        /// Keys passed to `start`, one per call.
        started: Vec<AppKey>,
        stops: usize,
    }

    type Steer = Arc<Mutex<Script>>;

    fn script(s: &Steer) -> std::sync::MutexGuard<'_, Script> {
        s.lock().unwrap_or_else(PoisonError::into_inner)
    }

    struct ScriptPlatform(Steer);
    struct ScriptSession(Steer);

    impl Platform for ScriptPlatform {
        fn name(&self) -> &'static str {
            "script"
        }
        fn support(&self) -> Support {
            script(&self.0).support.clone().unwrap_or(Support::Supported)
        }
        fn list_apps(&mut self) -> anyhow::Result<Vec<AudioApp>> {
            let app = |key: &AppKey| AudioApp {
                key: key.clone(),
                name: "Music".into(),
                detail: String::new(),
                pids: vec![1],
                playing: true,
                peak: None,
            };
            Ok(script(&self.0).apps.iter().map(app).collect())
        }
        fn list_outputs(&mut self) -> anyhow::Result<Vec<OutputDevice>> {
            let device = |id: &String| OutputDevice { id: id.clone(), name: id.clone(), is_default: false };
            Ok(script(&self.0).outputs.iter().map(device).collect())
        }
        fn hide_methods(&mut self) -> Vec<(HideMethod, String)> {
            vec![(HideMethod::Auto, "Auto".into())]
        }
        fn start(&mut self, config: SessionConfig, _: ProcessorFactory) -> anyhow::Result<Box<dyn Session>> {
            let mut s = script(&self.0);
            s.started.push(config.app);
            anyhow::ensure!(!s.fail_start, "scripted start failure");
            Ok(Box::new(ScriptSession(Arc::clone(&self.0))))
        }
        fn recover(&mut self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    impl Session for ScriptSession {
        fn status(&self) -> SessionStatus {
            SessionStatus { state: script(&self.0).state.clone(), ..SessionStatus::default() }
        }
        fn stop(self: Box<Self>) -> anyhow::Result<()> {
            script(&self.0).stops += 1;
            Ok(())
        }
    }

    fn scripted(s: Script, desired: Desired, setup_time: Duration) -> (Engine, Steer) {
        let steer = Arc::new(Mutex::new(s));
        let backend = Arc::clone(&steer);
        let engine = Engine::spawn(
            Box::new(move || {
                std::thread::sleep(setup_time);
                Ok(Box::new(ScriptPlatform(backend)) as Box<dyn Platform>)
            }),
            SharedParams::new(Settings::default()),
            desired,
            Arc::new(|| {}),
        );
        (engine, steer)
    }

    fn wants(key: &str, output: Option<&str>) -> Desired {
        Desired {
            enabled: true,
            app: Some(ChosenApp { key: AppKey(key.into()), name: "Music".into() }),
            output: output.map(Into::into),
            hide: HideMethod::Auto,
            sidechain: false,
        }
    }

    fn running(s: &Snapshot) -> bool {
        s.session.as_ref().is_some_and(|st| st.state == SessionState::Running)
    }

    #[test]
    fn closing_during_backend_setup_starts_nothing() {
        let s = Script { apps: vec![AppKey("music".into())], state: SessionState::Running, ..Script::default() };
        let (mut engine, steer) = scripted(s, wants("music", None), Duration::from_millis(300));
        engine.shutdown(Duration::from_secs(5));
        assert!(script(&steer).started.is_empty(), "a session was started after the window closed");
    }

    #[test]
    fn unreachable_sound_server_keeps_the_running_session() {
        let s = Script { apps: vec![AppKey("music".into())], state: SessionState::Running, ..Script::default() };
        let (mut engine, steer) = scripted(s, wants("music", None), Duration::ZERO);
        wait_for(&engine, "session", running);
        // Linux reports Unsupported while pipewire-pulse restarts; its session reconnects.
        script(&steer).support = Some(Support::Unsupported("no sound server".into()));
        wait_for(&engine, "support change", |s| matches!(s.support, Some(Support::Unsupported(_))));
        std::thread::sleep(OUTPUTS_EVERY * 3);
        assert_eq!(script(&steer).stops, 0, "the session must not be stopped");
        assert!(engine.with_snapshot(running));
        // A failure meanwhile is not retried until the backend can run again.
        script(&steer).state = SessionState::Error("lost the server".into());
        wait_for(&engine, "failure", |s| s.session.is_none() && s.error.is_some());
        std::thread::sleep(RETRY_DELAYS[0] * 2);
        assert_eq!(script(&steer).started.len(), 1);
        script(&steer).support = None;
        script(&steer).state = SessionState::Running;
        wait_for(&engine, "restart", running);
        assert_eq!(script(&steer).started.len(), 2);
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn denied_permission_is_only_retried_by_the_user() {
        let s = Script {
            apps: vec![AppKey("music".into())],
            fail_start: true,
            support: Some(Support::NeedsPermission("allow capture".into())),
            ..Script::default()
        };
        let (mut engine, steer) = scripted(s, wants("music", None), Duration::ZERO);
        wait_for(&engine, "failure", |s| s.error.is_some() && !s.retry_pending);
        std::thread::sleep(RETRY_DELAYS[0] * 3);
        assert_eq!(script(&steer).started.len(), 1, "no automatic retry while permission is missing");
        engine.retry();
        wait_for(&engine, "manual retry", |_| script(&steer).started.len() == 2);
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn plugging_in_an_output_gives_a_failed_start_another_go() {
        let s = Script {
            apps: vec![AppKey("music".into())],
            outputs: vec!["speakers".into()],
            fail_start: true,
            state: SessionState::Running,
            ..Script::default()
        };
        let (mut engine, steer) = scripted(s, wants("music", Some("headphones")), Duration::ZERO);
        wait_for(&engine, "retries to run out", |s| {
            s.error.is_some() && !s.retry_pending && s.activity == Activity::Idle
        });
        let attempts = script(&steer).started.len();
        assert_eq!(attempts, 1 + RETRY_DELAYS.len());
        {
            let mut s = script(&steer);
            s.outputs.push("headphones".into());
            s.fail_start = false;
        }
        wait_for(&engine, "start on the new device", running);
        assert_eq!(script(&steer).started.len(), attempts + 1);
        engine.shutdown(Duration::from_secs(5));
    }

    #[cfg(windows)]
    #[test]
    fn store_app_update_still_finds_the_saved_app() {
        let saved = r"c:\program files\windowsapps\spotifyab.spotifymusic_1.298.301.0_x64__zpdnekdrzrea0\spotify.exe";
        let updated = r"c:\program files\windowsapps\spotifyab.spotifymusic_1.304.258.0_x64__zpdnekdrzrea0\spotify.exe";
        let s = Script { apps: vec![AppKey(updated.into())], state: SessionState::Running, ..Script::default() };
        let (mut engine, steer) = scripted(s, wants(saved, None), Duration::ZERO);
        wait_for(&engine, "session", running);
        assert!(engine.with_snapshot(|s| s.app_present(&AppKey(saved.into()))));
        // The saved key goes to the backend, which matches it without the version.
        assert_eq!(script(&steer).started, vec![AppKey(saved.into())]);
        engine.shutdown(Duration::from_secs(5));
    }
}
