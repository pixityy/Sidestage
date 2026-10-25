//! The engine: one background thread that owns the OS backend ([`Platform`]) and the running
//! [`Session`], so the UI thread never waits on a platform call.
//!
//! The UI says what it wants ([`Desired`]: power on/off, which sources, output, hide method,
//! sidechain, the apps that never count as the game) and reads a [`Snapshot`] of what is going
//! on. The engine reconciles the two. It keeps a single session: started as soon as one
//! source's app shows up (the other sources wait inside it), changed live with
//! [`Session::set_sources`] and [`Session::set_sidechain_ignore`] when sources come and go or
//! the ignore list changes, restarted only when the output or the sidechain changes, and always
//! stopped (restoring every original) before the engine exits. Restarting for a source change
//! would un-hide every other app for a moment, so it never does.
//!
//! Sound settings do not go through here: the UI writes them straight to each source's
//! [`SharedParams`], which that source's renderer reads.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sidestage_dsp::SharedParams;
use sidestage_platform::{
    AppKey, AudioApp, HideMethod, OutputDevice, Platform, ProcessorFactory, Session, SessionConfig, SessionState,
    SessionStatus, SourceConfig, SourceId, Support,
};

use crate::config::{ChosenApp, same_app};
use crate::processor::{self, VoiceBus, VoiceSlots};

/// How often the app list is refreshed.
const APPS_EVERY: Duration = Duration::from_secs(1);
/// How often output devices, hide methods and support are refreshed.
#[cfg(not(test))]
const OUTPUTS_EVERY: Duration = Duration::from_secs(5);
#[cfg(test)]
const OUTPUTS_EVERY: Duration = Duration::from_millis(100);
/// How often a running session's status is read for the UI.
const STATUS_EVERY: Duration = Duration::from_millis(100);
/// Automatic retries after a failed start (or a session or source that failed soon after
/// starting): delay before attempt 2, 3, 4. After that the error stays until the user retries or
/// changes something.
#[cfg(not(test))]
const RETRY_DELAYS: [Duration; 3] = [Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(45)];
#[cfg(test)]
const RETRY_DELAYS: [Duration; 3] =
    [Duration::from_millis(300), Duration::from_millis(600), Duration::from_millis(900)];
/// A session (or source) that has been running this long counts as healthy: its next failure
/// starts the retry count from scratch. Before that, one that starts and then fails counts as a
/// failed attempt, so a backend that keeps failing right after starting is not restarted forever.
#[cfg(not(test))]
const HEALTHY_AFTER: Duration = Duration::from_secs(30);
#[cfg(test)]
const HEALTHY_AFTER: Duration = Duration::from_secs(2);
/// An app missing from the list for this long was quit; when it comes back, a failed start gets
/// a fresh set of attempts.
const APP_GONE_RESET: Duration = Duration::from_secs(3);
/// `set_sources` may return before the status shows the change (the contract allows it, and on
/// Windows it does when the session thread is busy). For this long after a failed source was
/// removed and added again, an `Error` for it counts only once the status has shown it in some
/// other state: until then it can still be the old instance's error.
#[cfg(not(test))]
const STALE_ERROR_GRACE: Duration = Duration::from_secs(3);
#[cfg(test)]
const STALE_ERROR_GRACE: Duration = Duration::from_millis(250);

/// Creates the backend on the engine thread (backends may set up per-thread OS state).
pub type PlatformMaker = Box<dyn FnOnce() -> anyhow::Result<Box<dyn Platform>> + Send>;
/// Called after the snapshot changed, e.g. to ask egui for a repaint.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// A source the user switched on.
#[derive(Clone)]
pub struct DesiredSource {
    pub id: SourceId,
    pub app: ChosenApp,
    /// The renderer's settings. One per source for as long as it exists: the backend keeps a
    /// source's renderer while its ID, app and hide method stay, so a new `params` for the same
    /// ID makes the engine remove and re-add the source.
    pub params: Arc<SharedParams>,
}

impl PartialEq for DesiredSource {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.app == other.app && Arc::ptr_eq(&self.params, &other.params)
    }
}

impl std::fmt::Debug for DesiredSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesiredSource").field("id", &self.id).field("app", &self.app).finish()
    }
}

/// What the user asked for.
#[derive(Clone, Debug, PartialEq)]
pub struct Desired {
    pub enabled: bool,
    /// The sources that are switched on, in the order the user added them.
    pub sources: Vec<DesiredSource>,
    pub output: Option<String>,
    pub hide: HideMethod,
    pub sidechain: bool,
    /// Apps whose sound never counts as the game for ducking.
    pub ignore: Vec<AppKey>,
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
    /// Status of the running session (with one entry per source), `None` when there is none.
    pub session: Option<SessionStatus>,
    pub session_since: Option<Instant>,
    /// Why the last start failed or the session ended with an error. Cleared on a new attempt.
    pub error: Option<String>,
    /// True while an automatic retry of a failed start is scheduled.
    pub retry_pending: bool,
    /// Failed sources with an automatic retry scheduled.
    pub source_retries: Vec<SourceId>,
    pub notices: Vec<Notice>,
}

impl Snapshot {
    /// The app is in the app list (also under a newer key of the same app, see [`same_app`]).
    pub fn app_present(&self, key: &AppKey) -> bool {
        self.apps.iter().any(|a| same_app(&a.key, key))
    }

    /// The running session's status of one source.
    pub fn source(&self, id: SourceId) -> Option<&sidestage_platform::SourceStatus> {
        self.session.as_ref()?.sources.iter().find(|s| s.id == id)
    }
}

enum Command {
    SetDesired(Desired),
    Retry,
    /// Restart one failed source now.
    RetrySource(SourceId),
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
    pub fn spawn(make_platform: PlatformMaker, desired: Desired, waker: Waker) -> Engine {
        let (tx, rx) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let worker_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new().name("sidestage-engine".into()).spawn(move || {
            let _done = DoneGuard(done_tx);
            let panic_shared = Arc::clone(&worker_shared);
            let panic_waker = Arc::clone(&waker);
            let result = std::panic::catch_unwind(AssertUnwindSafe(move || {
                let worker = Worker::new(rx, worker_shared, waker, desired);
                worker.run(make_platform);
            }));
            if result.is_err() {
                // The session (if any) was dropped while unwinding, which restores the originals.
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

    pub fn retry_source(&self, id: SourceId) {
        let _ = self.tx.send(Command::RetrySource(id));
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

/// What makes the backend keep a source's capture and renderer: its ID, app and hide method
/// (as the contract matches them), plus which `SharedParams` its renderer reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceKey {
    pub id: SourceId,
    pub app: AppKey,
    pub hide: HideMethod,
    /// Address of the source's `SharedParams`.
    pub params: usize,
}

/// How a new source list differs from the one the session has.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SourceDiff {
    pub added: Vec<SourceId>,
    pub removed: Vec<SourceId>,
    /// Same ID with another app or hide method: the backend restarts these itself.
    pub restarted: Vec<SourceId>,
    /// Same ID, app and hide method but new settings storage: the backend would keep the old
    /// renderer, so the engine removes and re-adds these.
    pub replaced: Vec<SourceId>,
    /// The sources both lists share are in another order (the status follows the list's order).
    pub reordered: bool,
}

impl SourceDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.restarted.is_empty()
            && self.replaced.is_empty()
            && !self.reordered
    }
}

pub(crate) fn diff(old: &[SourceKey], new: &[SourceKey]) -> SourceDiff {
    let find = |list: &[SourceKey], id: SourceId| list.iter().find(|k| k.id == id).cloned();
    let mut d = SourceDiff::default();
    for n in new {
        match find(old, n.id) {
            None => d.added.push(n.id),
            Some(o) if o.app != n.app || o.hide != n.hide => d.restarted.push(n.id),
            Some(o) if o.params != n.params => d.replaced.push(n.id),
            Some(_) => {}
        }
    }
    d.removed = old.iter().filter(|o| find(new, o.id).is_none()).map(|o| o.id).collect();
    let shared_old: Vec<SourceId> = old.iter().map(|k| k.id).filter(|id| find(new, *id).is_some()).collect();
    let shared_new: Vec<SourceId> = new.iter().map(|k| k.id).filter(|id| find(old, *id).is_some()).collect();
    d.reordered = shared_old != shared_new;
    d
}

/// A source as handed to the session.
#[derive(Clone)]
struct Applied {
    key: SourceKey,
    name: String,
    config: SourceConfig,
}

struct Running {
    key: RunKey,
    session: Box<dyn Session>,
    started: Instant,
    /// The sources and the ignore list the session last got.
    sources: Vec<Applied>,
    ignore: Vec<AppKey>,
}

impl Running {
    fn names(&self) -> String {
        let names: Vec<&str> = self.sources.iter().map(|a| a.name.as_str()).collect();
        if names.is_empty() { "the session".into() } else { names.join(", ") }
    }
}

/// What a session is started with and cannot change while it runs; a change means a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RunKey {
    output: Option<String>,
    sidechain: bool,
}

struct Failure {
    key: RunKey,
    attempts: usize,
    retry_at: Option<Instant>,
    /// Since when none of the sources' apps has been in the app list.
    gone_since: Option<Instant>,
}

/// A source that ended in `Error` inside a running session. It is removed and re-added after
/// the next delay of [`RETRY_DELAYS`]; the other sources keep playing meanwhile.
struct SourceFailure {
    attempts: usize,
    /// The current error has been counted and the source waits for its retry.
    waiting: bool,
    retry_at: Option<Instant>,
    /// When the source was last (re)started, to tell when it has proven healthy.
    retried_at: Instant,
    /// The engine removed and re-added the source at `retried_at`, and the status has not shown
    /// it in another state than `Error` since (see [`STALE_ERROR_GRACE`]).
    unconfirmed: bool,
    /// Since when the source's app has been missing from the app list.
    gone_since: Option<Instant>,
}

impl SourceFailure {
    fn fresh(now: Instant) -> Self {
        Self { attempts: 0, waiting: false, retry_at: None, retried_at: now, unconfirmed: false, gone_since: None }
    }

    /// Whether an `Error` in the status may still be the one from before the last retry.
    fn maybe_stale(&self, now: Instant) -> bool {
        self.unconfirmed && now.duration_since(self.retried_at) < STALE_ERROR_GRACE
    }

    /// Retry at once (the user asked, or the app was quit and restarted).
    fn retry_now(&mut self, now: Instant) {
        self.attempts = 0;
        self.waiting = true;
        self.retry_at = Some(now);
    }
}

struct Worker {
    rx: Receiver<Command>,
    shared: Arc<Mutex<Snapshot>>,
    waker: Waker,
    desired: Desired,
    platform: Option<Box<dyn Platform>>,
    support: Option<Support>,
    apps: Vec<AudioApp>,
    /// IDs of the last output list, to notice a device being plugged in.
    output_ids: Vec<String>,
    session: Option<Running>,
    failure: Option<Failure>,
    source_failures: HashMap<SourceId, SourceFailure>,
    /// Voice-chat levels the sources' renderers share, and who writes where.
    voice: Arc<VoiceBus>,
    slots: VoiceSlots,
    master: ProcessorFactory,
    next_apps: Instant,
    next_outputs: Instant,
    next_status: Instant,
    next_notice: u64,
}

impl Worker {
    fn new(rx: Receiver<Command>, shared: Arc<Mutex<Snapshot>>, waker: Waker, desired: Desired) -> Self {
        let now = Instant::now();
        Worker {
            rx,
            shared,
            waker,
            desired,
            platform: None,
            support: None,
            apps: Vec::new(),
            output_ids: Vec::new(),
            session: None,
            failure: None,
            source_failures: HashMap::new(),
            voice: VoiceBus::new(),
            slots: VoiceSlots::default(),
            master: processor::master_factory(),
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
            // session (and hide the apps) only to stop it again.
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
            // The first tick starts right away if a source's app is already playing.
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
            // Source retries are checked on every status poll.
            t = t.min(self.next_status);
        } else if let Some(f) = &self.failure
            && let Some(retry) = f.retry_at
            && self.may_start()
            && self.any_source_listed()
        {
            // While the apps are missing, the app-list poll wakes us instead. The floor keeps an
            // overdue retry that cannot start from turning into a busy loop.
            t = t.min(retry.max(Instant::now() + Duration::from_millis(20)));
        }
        t
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::SetDesired(desired) => {
                if desired != self.desired {
                    // A source that is gone, or now names another app, starts over.
                    self.source_failures.retain(|id, _| {
                        let app = |d: &Desired| d.sources.iter().find(|s| s.id == *id).map(|s| s.app.key.clone());
                        app(&desired).is_some() && app(&desired) == app(&self.desired)
                    });
                    // Voice slots are freed once the session has dropped the source (see
                    // `update_session`), not here: a source switched off and on again before the
                    // next tick never left the session and still writes to its slot.
                    self.desired = desired;
                    self.failure = None;
                    self.update(|s| {
                        s.error = None;
                        s.retry_pending = false;
                    });
                    self.publish_source_retries();
                }
            }
            Command::Retry => {
                self.failure = None;
                if self.session_failed().is_some() {
                    self.stop_session();
                }
                let now = Instant::now();
                for f in self.source_failures.values_mut().filter(|f| f.waiting) {
                    f.retry_now(now);
                }
                self.update(|s| {
                    s.error = None;
                    s.retry_pending = false;
                });
                // Pick up an app that just appeared without waiting for the next poll.
                self.next_apps = Instant::now();
            }
            Command::RetrySource(id) => {
                let now = Instant::now();
                self.source_failures.entry(id).or_insert_with(|| SourceFailure::fresh(now)).retry_now(now);
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

    fn any_source_listed(&self) -> bool {
        self.desired.sources.iter().any(|s| self.app_listed(&s.app.key))
    }

    /// Whether a new session may be started (or a running one changed). Linux reports
    /// `Unsupported` while the sound server cannot be reached (it is restarting, or not up yet
    /// at login): nothing new is started then, but a running session is left alone, as it
    /// reconnects by itself.
    fn may_start(&self) -> bool {
        !matches!(self.support, Some(Support::Unsupported(_)))
    }

    /// macOS reports `NeedsPermission` once the user declined audio capture. Starting again
    /// cannot work until they allow it in System Settings, so only "Try again" retries.
    fn permission_missing(&self) -> bool {
        matches!(self.support, Some(Support::NeedsPermission(_)))
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

    fn run_key(&self) -> RunKey {
        RunKey { output: self.desired.output.clone(), sidechain: self.desired.sidechain }
    }

    /// The running session's error, if it has failed.
    fn session_failed(&self) -> Option<String> {
        let running = self.session.as_ref()?;
        let state = self.shared.lock().unwrap_or_else(PoisonError::into_inner).session.as_ref()?.state.clone();
        match state {
            SessionState::Error(message) => Some(message),
            SessionState::Stopped => Some(format!("Playing {} stopped unexpectedly.", running.names())),
            _ => None,
        }
    }

    fn reconcile(&mut self) {
        let wanted = self.desired.enabled && !self.desired.sources.is_empty();
        let key = self.run_key();

        if let Some(running) = &self.session
            && (!wanted || running.key != key)
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

        if self.permission_missing() {
            let mut cancelled = false;
            if let Some(f) = &mut self.failure
                && f.retry_at.take().is_some()
            {
                cancelled = true;
                self.update(|s| s.retry_pending = false);
            }
            for f in self.source_failures.values_mut().filter(|f| f.attempts > 0) {
                cancelled |= f.retry_at.take().is_some();
            }
            if cancelled {
                self.publish_source_retries();
            }
        }

        if !wanted {
            // Nothing should run: a scheduled retry no longer applies.
            if self.failure.take().is_some() {
                self.update(|s| {
                    s.error = None;
                    s.retry_pending = false;
                });
            }
            if !self.source_failures.is_empty() {
                self.source_failures.clear();
                self.publish_source_retries();
            }
            return;
        }
        if self.session.is_some() {
            self.update_session();
            return;
        }
        if !self.may_start() {
            return;
        }
        if !self.any_source_listed() {
            // Waiting for the apps. Quitting and restarting one gets a fresh attempt; a list
            // that only flickers (streams coming and going) keeps the retry count.
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
        self.start(key);
    }

    fn source_key(&self, s: &DesiredSource) -> SourceKey {
        SourceKey {
            id: s.id,
            app: s.app.key.clone(),
            hide: self.desired.hide.clone(),
            params: Arc::as_ptr(&s.params) as usize,
        }
    }

    /// What to hand the session for every switched-on source.
    fn wanted_sources(&mut self) -> Vec<Applied> {
        let sources = self.desired.sources.clone();
        sources
            .iter()
            .map(|s| {
                let slot = self.slots.assign(s.id);
                if slot.is_none() {
                    log::warn!("no voice slot left for {}; it will not turn others down", s.app.name);
                }
                let config = SourceConfig {
                    id: s.id,
                    app: s.app.key.clone(),
                    hide: self.desired.hide.clone(),
                    processor: processor::source_factory(Arc::clone(&s.params), Arc::clone(&self.voice), slot),
                };
                Applied { key: self.source_key(s), name: s.app.name.clone(), config }
            })
            .collect()
    }

    /// Bring the running session's sources and ignore list up to date, without restarting it.
    fn update_session(&mut self) {
        // While the sound server is unreachable the session reconnects by itself; changes wait.
        if !self.may_start() {
            return;
        }
        let now = Instant::now();
        self.track_failed_sources(now);
        let Some(running) = &self.session else { return };

        // Compared by key first: the renderers' factories are only built when something changed.
        let old: Vec<SourceKey> = running.sources.iter().map(|a| a.key.clone()).collect();
        let new: Vec<SourceKey> = self.desired.sources.iter().map(|s| self.source_key(s)).collect();
        let changes = diff(&old, &new);
        let due: Vec<SourceId> = self
            .source_failures
            .iter()
            .filter(|(id, f)| f.waiting && f.retry_at.is_some_and(|t| now >= t) && old.iter().any(|k| k.id == **id))
            .map(|(id, _)| *id)
            .filter(|id| new.iter().any(|k| k.id == *id))
            .collect();
        let rebuild: Vec<SourceId> = changes.replaced.iter().chain(&due).copied().collect();

        let mut result = Ok(());
        if !changes.is_empty() || !rebuild.is_empty() {
            let wanted = self.wanted_sources();
            let Some(running) = &mut self.session else { return };
            log::info!(
                "sources: added {:?}, removed {:?}, restarted {:?}, rebuilt {:?}",
                changes.added,
                changes.removed,
                changes.restarted,
                rebuild
            );
            if !rebuild.is_empty() {
                // Remove first, so the backend starts these again with their current renderer.
                let interim: Vec<SourceConfig> =
                    wanted.iter().filter(|a| !rebuild.contains(&a.key.id)).map(|a| a.config.clone()).collect();
                result = running.session.set_sources(interim);
            }
            if result.is_ok() {
                result = running.session.set_sources(wanted.iter().map(|a| a.config.clone()).collect());
            }
            running.sources = wanted;
            // The removed sources' renderers are gone (or going); their slots go to the back
            // of the line.
            self.slots.retain(|id| running.sources.iter().any(|a| a.key.id == id));
            for id in &due {
                if let Some(f) = self.source_failures.get_mut(id) {
                    f.waiting = false;
                    f.retry_at = None;
                    f.retried_at = now;
                    f.unconfirmed = true;
                }
            }
            // The next status shows the new list.
            self.next_status = now;
        }
        let Some(running) = &mut self.session else { return };
        if result.is_ok() && running.ignore != self.desired.ignore {
            log::info!("ducking ignores {} app(s)", self.desired.ignore.len());
            running.ignore = self.desired.ignore.clone();
            result = running.session.set_sidechain_ignore(running.ignore.clone());
        }
        let key = running.key.clone();
        if !due.is_empty() {
            self.publish_source_retries();
        }
        if let Err(e) = result {
            // Only an invalid list or a dead session is refused; start over.
            let message = format!("{e:#}");
            log::warn!("could not update the running session: {message}");
            self.stop_session();
            self.record_failure(key, message);
        }
    }

    /// A failed source whose app has been gone for a while gets a fresh start when it is back.
    fn track_failed_sources(&mut self, now: Instant) {
        let listed: Vec<SourceId> =
            self.desired.sources.iter().filter(|s| self.app_listed(&s.app.key)).map(|s| s.id).collect();
        let mut changed = false;
        for (id, f) in self.source_failures.iter_mut().filter(|(_, f)| f.waiting) {
            if listed.contains(id) {
                f.gone_since = None;
                continue;
            }
            let gone = *f.gone_since.get_or_insert(now);
            if now.duration_since(gone) >= APP_GONE_RESET && f.attempts > 0 {
                // Re-added now, it waits inside the session and attaches when the app is back.
                f.retry_now(now);
                changed = true;
            }
        }
        if changed {
            self.publish_source_retries();
        }
    }

    fn start(&mut self, key: RunKey) {
        if self.platform.is_none() {
            return;
        }
        // No session, so no renderer of a removed source is left writing to its slot.
        self.slots.retain(|id| self.desired.sources.iter().any(|s| s.id == id));
        let sources = self.wanted_sources();
        let names: Vec<&str> = sources.iter().map(|a| a.name.as_str()).collect();
        log::info!(
            "starting: {} source(s) ({}), output {:?}, hide {:?}, sidechain {}, {} ignored app(s)",
            sources.len(),
            names.join(", "),
            key.output,
            self.desired.hide,
            key.sidechain,
            self.desired.ignore.len()
        );
        self.update(|s| {
            s.activity = Activity::Starting;
            s.error = None;
            s.retry_pending = false;
        });
        let config = SessionConfig {
            output: key.output.clone(),
            sidechain: key.sidechain,
            sidechain_ignore: self.desired.ignore.clone(),
            sources: sources.iter().map(|a| a.config.clone()).collect(),
            master: Arc::clone(&self.master),
        };
        let ignore = self.desired.ignore.clone();
        let Some(platform) = self.platform.as_mut() else { return };
        let result = platform.start(config);
        match result {
            Ok(session) => {
                let status = session.status();
                log::info!("session started: {:?}", status.state);
                // Keep counting attempts until the session has proven healthy (see HEALTHY_AFTER);
                // only the scheduled retry is used up.
                if let Some(f) = &mut self.failure {
                    f.retry_at = None;
                }
                // Every source was started afresh, in a session with no old status to lag behind.
                let now = Instant::now();
                for f in self.source_failures.values_mut() {
                    f.waiting = false;
                    f.retry_at = None;
                    f.retried_at = now;
                    f.unconfirmed = false;
                }
                self.session = Some(Running { key, session, started: now, sources, ignore });
                self.next_status = now + STATUS_EVERY;
                self.update(|s| {
                    s.activity = Activity::Idle;
                    s.session = Some(status);
                    s.session_since = Some(now);
                });
                self.publish_source_retries();
            }
            Err(e) => {
                let message = format!("{e:#}");
                log::warn!("could not start: {message}");
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
        let now = Instant::now();
        self.next_status = now + STATUS_EVERY;
        if self.failure.is_some()
            && matches!(status.state, SessionState::Running | SessionState::WaitingForApp)
            && running.started.elapsed() >= HEALTHY_AFTER
        {
            self.failure = None;
        }
        let applied: Vec<SourceId> = running.sources.iter().map(|a| a.key.id).collect();
        let permission_missing = self.permission_missing();
        let mut scheduled = false;
        for source in status.sources.iter().filter(|s| applied.contains(&s.id)) {
            if !matches!(source.state, SessionState::Error(_))
                && let Some(f) = self.source_failures.get_mut(&source.id)
            {
                f.unconfirmed = false;
            }
            match &source.state {
                SessionState::Error(message) => {
                    let f = self.source_failures.entry(source.id).or_insert_with(|| SourceFailure::fresh(now));
                    if f.waiting || f.maybe_stale(now) {
                        continue;
                    }
                    f.attempts += 1;
                    f.waiting = true;
                    f.retry_at =
                        if permission_missing { None } else { RETRY_DELAYS.get(f.attempts - 1).map(|d| now + *d) };
                    log::warn!(
                        "{} failed (attempt {}): {message}; {}",
                        source.app_name,
                        f.attempts,
                        if f.retry_at.is_some() { "trying again soon" } else { "not retrying by itself" }
                    );
                    scheduled = true;
                }
                // Running long enough after its last retry: the next failure starts afresh.
                SessionState::Running
                    if self
                        .source_failures
                        .get(&source.id)
                        .is_some_and(|f| !f.waiting && now.duration_since(f.retried_at) >= HEALTHY_AFTER) =>
                {
                    self.source_failures.remove(&source.id);
                }
                _ => {}
            }
        }
        self.update(|s| s.session = Some(status));
        if scheduled {
            self.publish_source_retries();
        }
    }

    fn publish_source_retries(&self) {
        let mut ids: Vec<SourceId> =
            self.source_failures.iter().filter(|(_, f)| f.waiting && f.retry_at.is_some()).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        self.update(|s| s.source_retries = ids);
    }

    fn stop_session(&mut self) {
        let Some(running) = self.session.take() else { return };
        let names = running.names();
        log::info!("stopping: {names}");
        self.update(|s| s.activity = Activity::Stopping);
        let result = running.session.stop();
        self.update(|s| {
            s.activity = Activity::Idle;
            s.session = None;
            s.session_since = None;
        });
        if let Err(e) = result {
            log::error!("stopping {names} failed: {e:#}");
            self.notice(
                NoticeKind::Error,
                format!("{names} may not have been restored"),
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
    use sidestage_platform::SourceStatus;

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

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}");
    }

    fn source(id: SourceId, key: &str) -> DesiredSource {
        DesiredSource {
            id,
            app: ChosenApp { key: AppKey(key.into()), name: key.into() },
            params: SharedParams::new(Settings::default()),
        }
    }

    fn wants(sources: Vec<DesiredSource>) -> Desired {
        Desired { enabled: true, sources, output: None, hide: HideMethod::Auto, sidechain: false, ignore: Vec::new() }
    }

    fn running(s: &Snapshot) -> bool {
        s.session.as_ref().is_some_and(|st| st.state == SessionState::Running)
    }

    #[test]
    fn diffs_source_lists() {
        let key = |id, app: &str, hide: HideMethod, params| SourceKey { id, app: AppKey(app.into()), hide, params };
        let a = key(1, "spotify", HideMethod::Auto, 10);
        let b = key(2, "discord", HideMethod::Auto, 20);
        let c = key(3, "podcasts", HideMethod::Auto, 30);
        assert!(diff(&[a.clone(), b.clone()], &[a.clone(), b.clone()]).is_empty());
        let d = diff(&[a.clone(), b.clone()], &[a.clone(), c.clone()]);
        assert_eq!((d.added, d.removed), (vec![3], vec![2]));
        assert!(!d.reordered);
        let d = diff(&[a.clone(), b.clone()], &[b.clone(), a.clone()]);
        assert!(d.reordered && d.added.is_empty() && d.removed.is_empty());
        let d = diff(std::slice::from_ref(&a), &[key(1, "vlc", HideMethod::Auto, 10)]);
        assert_eq!(d.restarted, vec![1], "another app under the same ID");
        let d = diff(std::slice::from_ref(&a), &[key(1, "spotify", HideMethod::KeepOriginal, 10)]);
        assert_eq!(d.restarted, vec![1], "another hide method");
        let d = diff(std::slice::from_ref(&a), &[key(1, "spotify", HideMethod::Auto, 11)]);
        assert_eq!(d.replaced, vec![1], "new settings storage needs a rebuild");
        assert!(diff(&[], &[]).is_empty());
        assert_eq!(diff(&[a, b], &[]).removed, vec![1, 2]);
    }

    #[test]
    fn demo_session_starts_with_sources_and_stops_when_disabled() {
        let desired = Desired {
            sidechain: true,
            ..wants(vec![source(1, "demo:spotify"), source(2, "demo:discord"), source(3, "demo:not-running")])
        };
        let mut engine = Engine::spawn(
            Box::new(|| Ok(Box::new(DemoPlatform::new(Scenario::default())) as Box<dyn Platform>)),
            desired.clone(),
            Arc::new(|| {}),
        );
        wait_for(&engine, "session", |s| {
            running(s)
                && s.source(1).is_some_and(|x| x.state == SessionState::Running)
                && s.source(3).is_some_and(|x| x.state == SessionState::WaitingForApp)
        });
        engine.set_desired(Desired { enabled: false, ..desired.clone() });
        wait_for(&engine, "stop", |s| s.session.is_none());
        engine.set_desired(wants(vec![source(4, "nope")]));
        std::thread::sleep(Duration::from_millis(300));
        assert!(engine.with_snapshot(|s| s.session.is_none()), "must wait for an app that is not running");
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn failed_start_is_reported_and_recovered_lines_become_a_notice() {
        let scenario = Scenario { warnings: true, start_error: true, ..Scenario::default() };
        let mut engine = Engine::spawn(
            Box::new(move || Ok(Box::new(DemoPlatform::new(scenario)) as Box<dyn Platform>)),
            wants(vec![source(1, "demo:spotify")]),
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
        let mut engine = Engine::spawn(
            Box::new(
                move || Ok(Box::new(DemoPlatform::new(scenario).with_start_counter(counter)) as Box<dyn Platform>),
            ),
            wants(vec![source(1, "demo:spotify")]),
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
        let mut engine =
            Engine::spawn(Box::new(|| Err(anyhow::anyhow!("no backend here"))), wants(Vec::new()), Arc::new(|| {}));
        wait_for(&engine, "ready", |s| s.ready);
        assert_eq!(engine.with_snapshot(|s| s.platform_error.clone()), Some("no backend here".into()));
        let started = Instant::now();
        engine.shutdown(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1), "an idle engine stops at once");
    }

    #[test]
    fn panicking_backend_is_reported() {
        let mut engine = Engine::spawn(
            Box::new(|| panic!("backend exploded (expected in this test)")),
            wants(Vec::new()),
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
        /// What a source reports; `Running` when not listed.
        source_states: HashMap<SourceId, SessionState>,
        /// When `set_sources` adds this source again, it works from then on, but the status
        /// keeps showing its old error for this long (a backend still applying the list).
        lagging_readd: Option<(SourceId, Duration)>,
        /// Until when the status shows the old error of `lagging_readd`'s source.
        stale_until: Option<Instant>,
        fail_start: bool,
        /// Source apps passed to `start`, one list per call.
        started: Vec<Vec<AppKey>>,
        started_ignore: Vec<Vec<AppKey>>,
        /// `(id, hide)` of the sources of every `set_sources` call.
        set_calls: Vec<Vec<(SourceId, HideMethod)>>,
        ignore_calls: Vec<Vec<AppKey>>,
        stops: usize,
        /// The running session's sources.
        current: Vec<SourceConfig>,
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
                name: "App".into(),
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
        fn start(&mut self, config: SessionConfig) -> anyhow::Result<Box<dyn Session>> {
            let mut s = script(&self.0);
            s.started.push(config.sources.iter().map(|c| c.app.clone()).collect());
            s.started_ignore.push(config.sidechain_ignore.clone());
            anyhow::ensure!(!s.fail_start, "scripted start failure");
            s.current = config.sources;
            Ok(Box::new(ScriptSession(Arc::clone(&self.0))))
        }
        fn recover(&mut self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    impl Session for ScriptSession {
        fn status(&self) -> SessionStatus {
            let s = script(&self.0);
            let stale = s.stale_until.filter(|t| Instant::now() < *t).and(s.lagging_readd.map(|(id, _)| id));
            let sources = s
                .current
                .iter()
                .map(|c| SourceStatus {
                    id: c.id,
                    app_name: c.app.0.clone(),
                    state: if stale == Some(c.id) {
                        SessionState::Error("the old instance's error".into())
                    } else {
                        s.source_states.get(&c.id).cloned().unwrap_or(SessionState::Running)
                    },
                    ..SourceStatus::default()
                })
                .collect();
            SessionStatus { state: s.state.clone(), sources, ..SessionStatus::default() }
        }
        fn set_sources(&mut self, sources: Vec<SourceConfig>) -> anyhow::Result<()> {
            if let Some(e) = SessionConfig::duplicate_error(&sources) {
                anyhow::bail!(e);
            }
            let mut s = script(&self.0);
            s.set_calls.push(sources.iter().map(|c| (c.id, c.hide.clone())).collect());
            if let Some((id, lag)) = s.lagging_readd
                && sources.iter().any(|c| c.id == id)
                && !s.current.iter().any(|c| c.id == id)
            {
                s.source_states.remove(&id);
                s.stale_until = Some(Instant::now() + lag);
            }
            s.current = sources;
            Ok(())
        }
        fn set_sidechain_ignore(&mut self, apps: Vec<AppKey>) -> anyhow::Result<()> {
            script(&self.0).ignore_calls.push(apps);
            Ok(())
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
            desired,
            Arc::new(|| {}),
        );
        (engine, steer)
    }

    fn keys(list: &[&str]) -> Vec<AppKey> {
        list.iter().map(|k| AppKey((*k).into())).collect()
    }

    fn ids(call: &[(SourceId, HideMethod)]) -> Vec<SourceId> {
        call.iter().map(|(id, _)| *id).collect()
    }

    #[test]
    fn closing_during_backend_setup_starts_nothing() {
        let s = Script { apps: keys(&["music"]), state: SessionState::Running, ..Script::default() };
        let (mut engine, steer) = scripted(s, wants(vec![source(1, "music")]), Duration::from_millis(300));
        engine.shutdown(Duration::from_secs(5));
        assert!(script(&steer).started.is_empty(), "a session was started after the window closed");
    }

    #[test]
    fn starts_when_any_source_app_is_listed_and_changes_sources_live() {
        let s = Script { state: SessionState::Running, ..Script::default() };
        let music = source(1, "music");
        let voice = source(2, "voice");
        let mut desired = wants(vec![music.clone(), voice.clone()]);
        let (mut engine, steer) = scripted(s, desired.clone(), Duration::ZERO);
        std::thread::sleep(APPS_EVERY / 2);
        assert!(script(&steer).started.is_empty(), "no source's app is running yet");

        // One app shows up: the session starts with both sources, the other one waits inside.
        script(&steer).apps = keys(&["voice"]);
        wait_for(&engine, "session", running);
        assert_eq!(script(&steer).started, vec![keys(&["music", "voice"])]);

        // Adding, switching off and re-adding sources never restarts the session.
        let podcast = source(3, "podcast");
        desired.sources.push(podcast.clone());
        engine.set_desired(desired.clone());
        wait_until("add", || script(&steer).set_calls.last().is_some_and(|c| ids(c) == [1, 2, 3]));
        desired.sources.retain(|s| s.id != 2);
        engine.set_desired(desired.clone());
        wait_until("remove", || script(&steer).set_calls.last().is_some_and(|c| ids(c) == [1, 3]));
        wait_for(&engine, "status follows", |s| {
            s.session.as_ref().is_some_and(|st| st.sources.iter().map(|x| x.id).collect::<Vec<_>>() == [1, 3])
        });

        // The ignore list and the hide method change live too.
        desired.ignore = keys(&["browser"]);
        engine.set_desired(desired.clone());
        wait_until("ignore", || script(&steer).ignore_calls == vec![keys(&["browser"])]);
        desired.hide = HideMethod::KeepOriginal;
        engine.set_desired(desired.clone());
        wait_until("hide", || {
            script(&steer).set_calls.last().is_some_and(|c| c.iter().all(|(_, h)| *h == HideMethod::KeepOriginal))
        });
        {
            let s = script(&steer);
            assert_eq!((s.started.len(), s.stops), (1, 0), "nothing above restarted the session");
        }
        std::thread::sleep(STATUS_EVERY * 3);
        let calls = script(&steer).set_calls.len();
        assert_eq!(calls, 3, "nothing changed, nothing sent: {:?}", script(&steer).set_calls);

        // A new output needs a new session, with everything the old one had. (The voice app is
        // no longer a source, so another source's app must be running for it to start.)
        script(&steer).apps.push(AppKey("music".into()));
        desired.output = Some("headphones".into());
        engine.set_desired(desired.clone());
        wait_until("restart", || script(&steer).started.len() == 2);
        {
            let s = script(&steer);
            assert_eq!(s.stops, 1);
            assert_eq!(s.started[1], keys(&["music", "podcast"]));
            assert_eq!(s.started_ignore[1], keys(&["browser"]));
        }

        // No source left: the session stops and restores everything.
        desired.sources.clear();
        engine.set_desired(desired);
        wait_for(&engine, "stop", |s| s.session.is_none());
        assert_eq!(script(&steer).stops, 2);
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn new_settings_storage_rebuilds_only_that_source() {
        let s = Script { apps: keys(&["music", "voice"]), state: SessionState::Running, ..Script::default() };
        let mut desired = wants(vec![source(1, "music"), source(2, "voice")]);
        let (mut engine, steer) = scripted(s, desired.clone(), Duration::ZERO);
        wait_for(&engine, "session", running);
        desired.sources[1] = source(2, "voice");
        engine.set_desired(desired);
        wait_until("rebuild", || script(&steer).set_calls.len() == 2);
        let calls: Vec<Vec<SourceId>> = script(&steer).set_calls.iter().map(|c| ids(c)).collect();
        assert_eq!(calls, vec![vec![1], vec![1, 2]], "removed, then added again");
        assert_eq!(script(&steer).stops, 0);
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn voice_slots_follow_what_the_session_plays() {
        let steer: Steer = Arc::new(Mutex::new(Script {
            apps: keys(&["music", "voice"]),
            state: SessionState::Running,
            ..Script::default()
        }));
        let backend = Arc::clone(&steer);
        let (_tx, rx) = mpsc::channel();
        let (music, voice) = (source(1, "music"), source(2, "voice"));
        let both = wants(vec![music.clone(), voice.clone()]);
        let mut w = Worker::new(rx, Arc::new(Mutex::new(Snapshot::default())), Arc::new(|| {}), both.clone());
        w.init(Box::new(move || Ok(Box::new(ScriptPlatform(backend)) as Box<dyn Platform>)));
        w.reconcile();
        assert!(w.session.is_some());
        let slot = w.slots.slot_of(2);
        assert!(slot.is_some());

        // Switched off and on again before the engine got to it: the session never dropped the
        // source, so its renderer still writes to its slot, which must stay its own.
        w.handle(Command::SetDesired(wants(vec![music.clone()])));
        w.handle(Command::SetDesired(both.clone()));
        w.reconcile();
        assert!(script(&steer).set_calls.is_empty(), "nothing changed for the session");
        assert_eq!(w.slots.slot_of(2), slot);

        // Really removed: the slot is freed once the session has dropped the source, and a new
        // source does not get it straight away.
        w.handle(Command::SetDesired(wants(vec![music.clone()])));
        w.reconcile();
        assert_eq!(script(&steer).set_calls.len(), 1);
        assert_eq!(w.slots.slot_of(2), None);
        w.handle(Command::SetDesired(wants(vec![music, source(3, "podcast")])));
        w.reconcile();
        assert!(w.slots.slot_of(3).is_some() && w.slots.slot_of(3) != slot);
        w.stop_session();
    }

    #[test]
    fn failed_source_is_retried_with_backoff_without_touching_the_others() {
        let s = Script { apps: keys(&["music", "voice"]), state: SessionState::Running, ..Script::default() };
        let (mut engine, steer) = scripted(s, wants(vec![source(1, "music"), source(2, "voice")]), Duration::ZERO);
        wait_for(&engine, "session", running);
        script(&steer).source_states.insert(2, SessionState::Error("protected audio".into()));
        wait_for(&engine, "retry scheduled", |s| s.source_retries == [2]);
        // Each retry removes and re-adds only the failed source; the scripted source keeps
        // failing, so after the last scheduled retry it stays failed.
        wait_until("every retry", || script(&steer).set_calls.len() == 2 * RETRY_DELAYS.len());
        wait_for(&engine, "retries to run out", |s| s.source_retries.is_empty());
        std::thread::sleep(RETRY_DELAYS[RETRY_DELAYS.len() - 1] + STALE_ERROR_GRACE * 2);
        let calls: Vec<Vec<SourceId>> = script(&steer).set_calls.iter().map(|c| ids(c)).collect();
        let expected: Vec<Vec<SourceId>> = (0..RETRY_DELAYS.len()).flat_map(|_| [vec![1], vec![1, 2]]).collect();
        assert_eq!(calls, expected, "each retry removes and re-adds only the failed source");
        let (starts, stops) = {
            let s = script(&steer);
            (s.started.len(), s.stops)
        };
        assert_eq!((starts, stops), (1, 0), "the session itself was never restarted");

        // The user asks again (and it works this time): one more attempt, which stays.
        script(&steer).source_states.clear();
        engine.retry_source(2);
        wait_until("manual retry", || script(&steer).set_calls.len() == 2 * RETRY_DELAYS.len() + 2);
        std::thread::sleep(RETRY_DELAYS[0] * 2);
        assert_eq!(script(&steer).set_calls.len(), 2 * RETRY_DELAYS.len() + 2);
        assert!(engine.with_snapshot(|s| s.source_retries.is_empty() && s.error.is_none()));
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn a_status_that_lags_behind_a_retry_does_not_count_as_another_failure() {
        let s = Script {
            apps: keys(&["music", "voice"]),
            state: SessionState::Running,
            // Longer than a status poll, shorter than the grace.
            lagging_readd: Some((2, STATUS_EVERY * 3 / 2)),
            ..Script::default()
        };
        let (mut engine, steer) = scripted(s, wants(vec![source(1, "music"), source(2, "voice")]), Duration::ZERO);
        wait_for(&engine, "session", running);
        script(&steer).source_states.insert(2, SessionState::Error("protected audio".into()));
        wait_for(&engine, "retry scheduled", |s| s.source_retries == [2]);
        // The retry fixes it, but the status shows the old error for a few polls more.
        wait_until("retry", || script(&steer).set_calls.len() == 2);
        wait_for(&engine, "the stale error was seen", |s| {
            s.source(2).is_some_and(|x| matches!(x.state, SessionState::Error(_)))
        });
        wait_for(&engine, "the source runs", |s| s.source(2).is_some_and(|x| x.state == SessionState::Running));
        std::thread::sleep(RETRY_DELAYS[1] + STALE_ERROR_GRACE * 2);
        assert_eq!(script(&steer).set_calls.len(), 2, "a working source was restarted again");
        assert!(engine.with_snapshot(|s| s.source_retries.is_empty()));

        // An error that outlasts the grace is a real one: it is retried.
        {
            let mut s = script(&steer);
            s.lagging_readd = None;
            s.source_states.insert(2, SessionState::Error("protected audio".into()));
        }
        wait_until("next retry", || script(&steer).set_calls.len() == 4);
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn unreachable_sound_server_keeps_the_running_session() {
        let s = Script { apps: keys(&["music"]), state: SessionState::Running, ..Script::default() };
        let mut desired = wants(vec![source(1, "music")]);
        let (mut engine, steer) = scripted(s, desired.clone(), Duration::ZERO);
        wait_for(&engine, "session", running);
        // Linux reports Unsupported while pipewire-pulse restarts; its session reconnects.
        script(&steer).support = Some(Support::Unsupported("no sound server".into()));
        wait_for(&engine, "support change", |s| matches!(s.support, Some(Support::Unsupported(_))));
        // Source changes wait until the server is back.
        desired.sources.push(source(2, "voice"));
        engine.set_desired(desired);
        std::thread::sleep(OUTPUTS_EVERY * 3);
        assert_eq!(script(&steer).stops, 0, "the session must not be stopped");
        assert!(script(&steer).set_calls.is_empty());
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
        assert_eq!(script(&steer).started[1], keys(&["music", "voice"]));
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn denied_permission_is_only_retried_by_the_user() {
        let s = Script {
            apps: keys(&["music"]),
            fail_start: true,
            support: Some(Support::NeedsPermission("allow capture".into())),
            ..Script::default()
        };
        let (mut engine, steer) = scripted(s, wants(vec![source(1, "music")]), Duration::ZERO);
        wait_for(&engine, "failure", |s| s.error.is_some() && !s.retry_pending);
        std::thread::sleep(RETRY_DELAYS[0] * 3);
        assert_eq!(script(&steer).started.len(), 1, "no automatic retry while permission is missing");
        engine.retry();
        wait_until("manual retry", || script(&steer).started.len() == 2);
        engine.shutdown(Duration::from_secs(5));
    }

    #[test]
    fn plugging_in_an_output_gives_a_failed_start_another_go() {
        let s = Script {
            apps: keys(&["music"]),
            outputs: vec!["speakers".into()],
            fail_start: true,
            state: SessionState::Running,
            ..Script::default()
        };
        let desired = Desired { output: Some("headphones".into()), ..wants(vec![source(1, "music")]) };
        let (mut engine, steer) = scripted(s, desired, Duration::ZERO);
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
        let s = Script { apps: keys(&[updated]), state: SessionState::Running, ..Script::default() };
        let (mut engine, steer) = scripted(s, wants(vec![source(1, saved)]), Duration::ZERO);
        wait_for(&engine, "session", running);
        assert!(engine.with_snapshot(|s| s.app_present(&AppKey(saved.into()))));
        // The saved key goes to the backend, which matches it without the version.
        assert_eq!(script(&steer).started, vec![keys(&[saved])]);
        engine.shutdown(Duration::from_secs(5));
    }
}
