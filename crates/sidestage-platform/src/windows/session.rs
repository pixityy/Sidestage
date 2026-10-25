//! A running session: one supervisor thread (MTA) that owns the output and, for every source,
//! its app attachment, hush state and capture. It follows device changes, re-attaches apps that
//! restart, keeps the sidechain's list of game sessions current and publishes the status.
//!
//! Real-time threads: the output's render thread (the mixer, see `render`), one capture thread
//! per attached source and the sidechain's meter thread. Every processor factory call and every
//! change to the OS happens here, on the supervisor thread.
//!
//! A source keeps its bridge and processor while its app restarts (the processor gets silence,
//! so tails ring out); both are rebuilt only when the output is reopened.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IAudioSessionManager2, IAudioVolumeDuckNotification, IMMDeviceEnumerator,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::Threading::{PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
use windows::core::{HSTRING, PCWSTR, w};

use super::capture::{self, Capture, Gain, SidechainLevels, Sink, SpawnError, Target};
use super::com::{Handle, Mta, created_of, open_process, panic_message, process_alive, process_created};
use super::hush::{self, Change, Hush, Ledger};
use super::notify::{self, DeviceWatch, Event, SessionWatch};
use super::procs::{Names, ProcessTable, app_key, exe_file_name, match_key, pid_path};
use super::render::{self, MAX_SOURCES, Output, RenderInfo, Slot};
use super::sessions::{self, Managers, SessionRef};
use super::sidechain::{Root, Sidechain};
use super::{MIN_BUILD, SUPPORTED_BUILD};
use crate::bridge::{BridgeConfig, BridgeProducer, BridgeStats, bridge};
use crate::{
    AppKey, HideMethod, ProcessorFactory, Session, SessionConfig, SessionState, SessionStatus, SourceConfig, SourceId,
    SourceStatus,
};

/// Time for a session-volume change to take effect before the makeup gain is trusted.
const SETTLE: Duration = Duration::from_millis(200);
/// Quiet time after a foreign volume change before re-hushing (the user may still be dragging).
const DEBOUNCE: Duration = Duration::from_millis(150);
/// The same, when the app keeps resetting its own volume.
const DEBOUNCE_FIGHTING: Duration = Duration::from_secs(2);
/// Default-device notifications come in bursts.
const DEVICE_DEBOUNCE: Duration = Duration::from_millis(300);
const SCAN_EVERY: Duration = Duration::from_secs(1);
const RETRY_EVERY: Duration = Duration::from_secs(2);
/// How long the attached instance may go without an audio session before Sidestage looks for
/// another instance of the app that has one.
const SWITCH_AFTER: Duration = Duration::from_secs(2);
/// Consecutive failures (of the output, or of one source) before giving up on it.
const MAX_FAILURES: u32 = 5;
/// A stream that has run this long has recovered; one that stops sooner has failed again.
const STABLE_AFTER: Duration = Duration::from_secs(10);

/// How long `start` waits for the output to open and the first attach of every source. Usually
/// 50-60 ms per source; if the audio system is slower than this, `start` returns the session
/// while it is still starting and later problems show up in its status, so the caller (the UI
/// thread) is never blocked for long.
const START_WAIT: Duration = Duration::from_millis(1500);
/// How long `set_sources` waits for the supervisor to take the new list (removed sources are
/// restored by then; new ones attach right after).
const SET_WAIT: Duration = Duration::from_millis(200);

/// One session per process: the hush journal has a single owner.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Errors for source lists the backend cannot run at all (as opposed to problems with one
/// source, which show in its status).
fn check_sources(sources: &[SourceConfig]) -> anyhow::Result<()> {
    if let Some(e) = SessionConfig::duplicate_error(sources) {
        bail!(e);
    }
    if sources.len() > MAX_SOURCES {
        bail!("Sidestage can play at most {MAX_SOURCES} apps at once");
    }
    if let Some(s) = sources.iter().find(|s| matches!(s.hide, HideMethod::ParkOn(_))) {
        bail!(
            "moving an app to another output is not supported on Windows ({}); use the default hide method",
            exe_file_name(&s.app.0)
        );
    }
    Ok(())
}

pub fn start(config: SessionConfig, build: u32) -> anyhow::Result<Box<dyn Session>> {
    check_sources(&config.sources)?;
    if ACTIVE.swap(true, Ordering::AcqRel) {
        bail!("a Sidestage session is already running; stop it first");
    }
    let shared = Arc::new(Shared {
        status: Mutex::new(SessionStatus::default()),
        sidechain: Arc::new(SidechainLevels::default()),
        gains: Mutex::new(Vec::new()),
    });
    let (tx, rx) = mpsc::channel::<Event>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<anyhow::Result<()>>(1);
    let (done_tx, done_rx) = mpsc::sync_channel::<anyhow::Result<()>>(1);
    let sup_shared = shared.clone();
    let sup_tx = tx.clone();
    let spawned = thread::Builder::new().name("sidestage-session".to_owned()).spawn(move || {
        let result = supervise(config, build, sup_shared, sup_tx, rx, &ready_tx);
        // Before `done`: a caller that starts a new session right after `stop()` must find
        // this one gone.
        ACTIVE.store(false, Ordering::Release);
        let _ = done_tx.send(result);
    });
    let thread = match spawned {
        Ok(t) => t,
        Err(e) => {
            ACTIVE.store(false, Ordering::Release);
            return Err(e).context("spawning the session thread");
        }
    };
    let mut session = WindowsSession { tx, shared, thread: Some(thread), done: done_rx };
    match ready_rx.recv_timeout(START_WAIT) {
        Ok(Ok(())) => Ok(Box::new(session)),
        Err(RecvTimeoutError::Timeout) => {
            log::warn!("the audio system is slow to respond; the session keeps starting");
            Ok(Box::new(session))
        }
        Ok(Err(e)) => {
            // Stops the supervisor if it got as far as running (and restores the apps).
            if let Err(stop) = session.shutdown() {
                log::debug!("stopping the failed session: {stop:#}");
            }
            Err(e)
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = session.shutdown();
            Err(anyhow!("the session thread failed to start"))
        }
    }
}

/// The supervisor thread. Returns the result of stopping (restoring every app).
fn supervise(
    config: SessionConfig,
    build: u32,
    shared: Arc<Shared>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    ready: &SyncSender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    // Also shown in the status, in case `start` has stopped waiting.
    let fail = |e: anyhow::Error| {
        shared.status().state = SessionState::Error(format!("{e:#}"));
        let _ = ready.send(Err(e));
    };
    let _mta = match Mta::enter() {
        Ok(m) => m,
        Err(e) => {
            fail(e);
            return Ok(());
        }
    };
    let sources = config.sources.clone();
    let mut sup = match Supervisor::new(config, build, shared.clone(), tx, rx) {
        Ok(s) => s,
        Err(e) => {
            fail(e);
            return Ok(());
        }
    };
    let first = catch_unwind(AssertUnwindSafe(|| sup.begin(sources)))
        .unwrap_or_else(|p| Err(anyhow!("starting the session panicked: {}", panic_message(&p))));
    // Handled like any later failure (retried) in case `start` stopped waiting; if it is still
    // waiting, it reports the error and stops the session.
    if let Err(e) = &first {
        sup.output_failed(format!("{e:#}"));
    }
    let _ = ready.send(first);
    if let Err(p) = catch_unwind(AssertUnwindSafe(|| sup.run())) {
        log::error!("session supervisor panicked: {}", panic_message(&p));
    }
    let result = catch_unwind(AssertUnwindSafe(|| sup.finish()))
        .unwrap_or_else(|p| Err(anyhow!("stopping the session panicked: {}", panic_message(&p))));
    // Joins every audio thread; an unreleased ledger restores what it can on the way out.
    drop(sup);
    result
}

/// State shared between the supervisor, the audio threads and the session handle.
struct Shared {
    status: Mutex<SessionStatus>,
    sidechain: Arc<SidechainLevels>,
    /// Each source's gain stage, for its capture peak in [`Session::status`].
    gains: Mutex<Vec<(SourceId, Arc<Gain>)>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Shared {
    fn status(&self) -> MutexGuard<'_, SessionStatus> {
        lock(&self.status)
    }
}

struct WindowsSession {
    tx: Sender<Event>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    done: Receiver<anyhow::Result<()>>,
}

impl WindowsSession {
    fn shutdown(&mut self) -> anyhow::Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        let _ = self.tx.send(Event::Stop);
        match self.done.recv_timeout(Duration::from_secs(10)) {
            Ok(result) => {
                let _ = thread.join();
                result
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                Err(anyhow!("the session thread ended without restoring the apps"))
            }
            // Leave the stuck thread behind rather than hang the caller; the journal stays
            // dirty, so the guardian or the next start restores the volumes.
            Err(RecvTimeoutError::Timeout) => {
                Err(anyhow!("the session did not stop in time; the apps' volumes are restored when Sidestage exits"))
            }
        }
    }

    fn send(&self, ev: Event) -> anyhow::Result<()> {
        if self.thread.as_ref().is_none_or(JoinHandle::is_finished) {
            bail!("the session has stopped");
        }
        self.tx.send(ev).map_err(|_| anyhow!("the session has stopped"))
    }
}

impl Session for WindowsSession {
    fn status(&self) -> SessionStatus {
        let mut s = self.shared.status().clone();
        s.sidechain_peak = self.shared.sidechain.hold.take();
        let gains = lock(&self.shared.gains);
        for src in &mut s.sources {
            if let Some((_, g)) = gains.iter().find(|(id, _)| *id == src.id) {
                src.capture_peak = g.peak.take();
            }
        }
        s
    }

    fn set_sources(&mut self, sources: Vec<SourceConfig>) -> anyhow::Result<()> {
        check_sources(&sources)?;
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        self.send(Event::SetSources(sources, ack_tx))?;
        match ack_rx.recv_timeout(SET_WAIT) {
            Ok(()) => Ok(()),
            Err(RecvTimeoutError::Timeout) => {
                log::debug!("the session is still applying the new sources");
                Ok(())
            }
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("the session has stopped")),
        }
    }

    fn set_sidechain_ignore(&mut self, apps: Vec<AppKey>) -> anyhow::Result<()> {
        self.send(Event::SetIgnore(apps))
    }

    fn stop(mut self: Box<Self>) -> anyhow::Result<()> {
        self.shutdown()
    }
}

impl Drop for WindowsSession {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            log::error!("stopping the session: {e:#}");
        }
    }
}

/// Sidestage's own process: never a source, never the game.
struct Me {
    pid: u32,
    /// This process and its ancestors, with their creation times: capturing a tree that
    /// contains Sidestage would feed its own output back.
    lineage: HashMap<u32, u64>,
    /// Match keys of their executables.
    lineage_keys: Vec<String>,
}

impl Me {
    fn new() -> anyhow::Result<Me> {
        let pid = std::process::id();
        let mut procs = ProcessTable::snapshot()?;
        let lineage: HashMap<u32, u64> =
            procs.lineage(pid).into_iter().map(|p| (p, procs.created(p).unwrap_or(0))).collect();
        let lineage_keys = lineage.keys().filter_map(|&p| procs.path(p)).map(|p| match_key(&p)).collect();
        Ok(Me { pid, lineage, lineage_keys })
    }

    /// True if the process is Sidestage or one of its ancestors (`created` 0 = unknown, which
    /// counts as one: an ancestor's PID may have been reused since, but never risk feedback).
    fn is_lineage(&self, pid: u32, created: u64) -> bool {
        self.lineage.get(&pid).is_some_and(|&c| c == 0 || created == 0 || c == created)
    }
}

/// The app's process tree a source is attached to.
struct Attached {
    root: u32,
    root_created: u64,
    /// Executable path of the root, in its original case.
    path: String,
    /// Signalled when the root process exits (None if it cannot be opened).
    exited: Option<Handle>,
    /// Processes of the tree with their creation times (a PID alone could have been reused
    /// by an unrelated process since).
    tree: HashMap<u32, u64>,
    /// Session PIDs known not to belong to the tree, with their creation times, and whether
    /// they are another instance of the app.
    foreign: HashMap<(u32, u64), bool>,
}

#[derive(Clone, Copy, PartialEq)]
enum Membership {
    Tree,
    OtherInstance,
    No,
}

impl Attached {
    /// Whether a session's process belongs to the tree (`procs` is snapshotted when needed).
    fn contains(&mut self, pid: u32, created: u64, key: &str, procs: &mut Option<ProcessTable>) -> Membership {
        if created != 0 && self.tree.get(&pid) == Some(&created) {
            return Membership::Tree;
        }
        if let Some(&instance) = self.foreign.get(&(pid, created)) {
            return if instance { Membership::OtherInstance } else { Membership::No };
        }
        let Some(t) = table(procs) else {
            return Membership::No;
        };
        if created != 0 && t.is_in_tree(pid, self.root) {
            self.tree.insert(pid, created);
            return Membership::Tree;
        }
        // Only a process known to be outside the captured tree counts as another instance:
        // restoring a captured one would be amplified by the makeup gain.
        let instance = created != 0 && t.path(pid).is_some_and(|p| match_key(&p) == key);
        self.foreign.insert((pid, created), instance);
        if instance { Membership::OtherInstance } else { Membership::No }
    }
}

/// A process table, taken the first time something needs it.
fn table(t: &mut Option<ProcessTable>) -> Option<&mut ProcessTable> {
    if t.is_none() {
        *t = ProcessTable::snapshot().map_err(|e| log::debug!("listing processes: {e:#}")).ok();
    }
    t.as_mut()
}

/// Finds an app's root process (the one whose tree plays audio, if several are running) and
/// how much audio it has: 2 = an active session, 1 = an idle session, 0 = none.
fn resolve(key: &str, app: &str, procs: &mut ProcessTable, live: &[SessionRef], me: &Me) -> Option<(Attached, u8)> {
    let file = exe_file_name(key).to_lowercase();
    let candidates: Vec<u32> = procs.pids().filter(|p| procs.get(*p).is_some_and(|e| e.exe_name == file)).collect();
    let mut roots: Vec<u32> = Vec::new();
    for pid in candidates {
        if me.is_lineage(pid, procs.created(pid).unwrap_or(0)) || procs.path(pid).is_none_or(|p| match_key(&p) != app) {
            continue;
        }
        let root = procs.root_of(pid);
        if !me.is_lineage(root, procs.created(root).unwrap_or(0)) && !roots.contains(&root) {
            roots.push(root);
        }
    }
    let mut best: Option<(u32, (u8, std::cmp::Reverse<u64>))> = None;
    for &root in &roots {
        let mut score = 0u8;
        for s in live {
            if procs.is_in_tree(s.pid, root) {
                score = score.max(if s.active() { 2 } else { 1 });
            }
        }
        // Among equals, the oldest instance (usually the one the user started first).
        let age = std::cmp::Reverse(procs.created(root).unwrap_or(u64::MAX));
        if best.is_none_or(|(_, b)| (score, age) > b) {
            best = Some((root, (score, age)));
        }
    }
    let (root, (score, _)) = best?;
    let tree: HashMap<u32, u64> = procs.tree(root).into_iter().filter_map(|p| Some((p, procs.created(p)?))).collect();
    let path = procs.path(root).unwrap_or_else(|| key.to_owned());
    let root_created = procs.created(root).unwrap_or(0);
    // Only the process the snapshot saw: its PID may have been reused since.
    let exited = open_process(root, PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION)
        .filter(|h| root_created != 0 && created_of(h) == Some(root_created));
    Some((Attached { root, root_created, path, exited, tree, foreign: HashMap::new() }, score))
}

/// Display name of an app: a live session's name, else the executable's description.
fn app_name(names: &mut Names, live: &[SessionRef], app: &str, path: &str) -> String {
    let display = live
        .iter()
        .find(|s| pid_path(s.pid).is_some_and(|p| match_key(&p) == app))
        .map(SessionRef::display_name)
        .unwrap_or_default();
    names.app_name(&display, path)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Gate {
    Closed,
    Settling(Instant),
    Open,
}

/// What a source's operations need from the session.
struct Env<'a> {
    output: Option<&'a mut Output>,
    ledger: &'a mut Ledger,
    managers: &'a Managers,
    names: &'a mut Names,
    me: &'a Me,
}

/// One app being captured, hidden and rendered.
struct Source {
    cfg: SourceConfig,
    /// `cfg.app` lowercase, and without a version folder (see [`match_key`]).
    key: String,
    match_key: String,
    name: String,
    gain: Arc<Gain>,
    /// `None` with `HideMethod::KeepOriginal`.
    hush: Option<Hush>,
    app: Option<Attached>,
    /// The capture end of the source's bridge while no capture thread holds it.
    producer: Option<BridgeProducer>,
    capture: Option<Capture>,
    capture_started: Option<Instant>,
    /// The source has a slot (its bridge's output end and its processor) in the output's mix.
    in_mix: bool,
    /// Set by the mixer if the slot's processor panicked.
    slot_fault: Arc<AtomicBool>,
    stats: Option<Arc<BridgeStats>>,
    /// Bridge glitches counted so far, and the bridge's count when the running capture
    /// started: an app that exits leaves its bridge to run dry, which is not a glitch.
    dropouts_base: u64,
    dropouts_mark: Option<u64>,
    gate: Gate,
    settle_tries: u32,
    rehush_at: Option<Instant>,
    /// Shown as running (kept through the brief re-settles after a volume change).
    running: bool,
    failures: u32,
    retry_at: Option<Instant>,
    last_error: Option<String>,
    /// The source gave up (shown as its error; the other sources keep playing).
    fatal: Option<String>,
    last_resolve: Instant,
    /// Loudest meter reading of the app's sessions in the last scan (pre-volume).
    app_peak: f32,
    /// Audio sessions of the attached tree in the last scan, and since when there were none.
    app_sessions: usize,
    sessionless_since: Option<Instant>,
    silent_since: Option<Instant>,
    /// Another instance of the app has audio sessions (it plays uncaptured).
    other_instances: bool,
    /// Hushing new sessions failed; do not retry before this.
    adopt_after: Option<Instant>,
}

impl Source {
    fn new(cfg: SourceConfig, tx: &Sender<Event>, names: &mut Names, live: &[SessionRef], me: &Me) -> Source {
        let key = cfg.app.0.to_lowercase();
        let match_key = match_key(&key);
        let name = app_name(names, live, &match_key, &key);
        let gain = Arc::new(Gain::new());
        let hush = (cfg.hide == HideMethod::Auto)
            .then(|| Hush::new(&key, &name, hush::next_token(), gain.clone(), tx.clone()));
        let fatal = me.lineage_keys.contains(&match_key).then(|| {
            format!("Sidestage cannot capture itself or the program that started it ({})", exe_file_name(&key))
        });
        Source {
            cfg,
            key,
            match_key,
            name,
            gain,
            hush,
            app: None,
            producer: None,
            capture: None,
            capture_started: None,
            in_mix: false,
            slot_fault: Arc::default(),
            stats: None,
            dropouts_base: 0,
            dropouts_mark: None,
            gate: Gate::Closed,
            settle_tries: 0,
            rehush_at: None,
            running: false,
            failures: 0,
            retry_at: None,
            last_error: None,
            fatal,
            last_resolve: Instant::now() - SCAN_EVERY,
            app_peak: 0.0,
            app_sessions: 0,
            sessionless_since: None,
            silent_since: None,
            other_instances: false,
            adopt_after: None,
        }
    }

    /// True if a journal entry's app is this source's.
    fn is_app(&self, app: &str) -> bool {
        app == self.key || match_key(app) == self.match_key
    }

    fn retry_due(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|t| now >= t)
    }

    fn token(&self) -> Option<u64> {
        self.hush.as_ref().map(Hush::token)
    }

    // ---- lifetime ------------------------------------------------------------------------

    /// Whether this tick needs the process table and session list for this source.
    fn wants_lookup(&self, now: Instant) -> bool {
        if self.fatal.is_some() || !self.retry_due(now) {
            return false;
        }
        match &self.app {
            None => now.duration_since(self.last_resolve) >= SCAN_EVERY,
            Some(_) => {
                self.sessionless_since.is_some_and(|t| now.duration_since(t) >= SWITCH_AFTER)
                    && now.duration_since(self.last_resolve) >= SWITCH_AFTER
            }
        }
    }

    /// App lifetime and capture health. Returns true if the source (re)attached, so the
    /// session list should be scanned now.
    fn tick(&mut self, env: &mut Env, now: Instant, procs: &mut Option<ProcessTable>, live: &[SessionRef]) -> bool {
        if self.fatal.is_some() {
            return false;
        }
        let exited = self.app.as_ref().is_some_and(|a| match &a.exited {
            Some(h) => h.is_signalled(),
            None => !process_alive(a.root, a.root_created),
        });
        if exited {
            log::info!("{} exited; waiting for it to come back", self.name);
            self.detach();
        }
        if env.output.is_none() {
            return false;
        }
        let mut attached = false;
        if self.wants_lookup(now) {
            self.last_resolve = now;
            let current = self.app.as_ref().map(|a| a.root);
            if let Some(t) = procs.as_mut()
                && let Some((att, score)) = resolve(&self.key, &self.match_key, t, live, env.me)
                // An attached instance is only swapped for another one that has audio.
                && (current.is_none() || (score > 0 && Some(att.root) != current))
            {
                if let Some(old) = current {
                    log::info!(
                        "{} (pid {old}) has no audio; switching to the instance that has (pid {})",
                        self.name,
                        att.root
                    );
                    self.detach();
                }
                match self.attach(env, att) {
                    Ok(()) => attached = true,
                    Err(e) => self.failed(env, format!("{e:#}")),
                }
            }
        }

        if self.in_mix && self.slot_fault.load(Ordering::Acquire) {
            // The mixer stopped calling it; a new one is built when the source retries.
            if let Some(o) = env.output.as_deref_mut() {
                self.leave_mix(o);
            }
            self.failed(env, "the audio processor stopped working".to_owned());
        }
        if self.capture.as_ref().is_some_and(Capture::finished) {
            let short = self.capture_started.is_some_and(|t| t.elapsed() < STABLE_AFTER);
            let err = self.stop_capture().unwrap_or_else(|| "the capture stopped".to_owned());
            log::warn!("capturing {} stopped: {err}", self.name);
            if short {
                // Dying again right after a (re)start counts toward giving up, so a broken
                // app or device cannot restart forever.
                self.failed(env, err);
            } else {
                self.restart_capture(env, err);
            }
        } else if self.capture.is_none() && self.app.is_some() && self.retry_due(now) {
            self.restart_capture(env, String::new());
        }
        if self.failures > 0 && self.capture_started.is_some_and(|t| t.elapsed() >= STABLE_AFTER) {
            self.failures = 0;
        }
        attached
    }

    fn attach(&mut self, env: &mut Env, att: Attached) -> anyhow::Result<()> {
        log::info!("attaching to {} (pid {})", self.name, att.root);
        let Some(output) = env.output.as_deref_mut() else {
            bail!("the output is not open");
        };
        let root = att.root;
        self.name = app_name(env.names, &sessions::scan(env.managers), &self.match_key, &att.path);
        if let Some(h) = &mut self.hush {
            h.set_app_name(&self.name);
        }
        self.app = Some(att);
        if let Err(e) = self.ensure_slot(output).and_then(|()| self.start_capture(root, output.info.rate)) {
            self.app = None;
            return Err(e);
        }
        // `failures` is reset once the capture has run for a while (see `tick`).
        self.retry_at = None;
        self.last_error = None;
        self.begin_settle();
        Ok(())
    }

    fn detach(&mut self) {
        self.close();
        self.stop_capture();
        if let Some(h) = &mut self.hush {
            h.forget_all();
        }
        self.app = None;
        self.app_peak = 0.0;
        self.app_sessions = 0;
        self.other_instances = false;
        self.sessionless_since = None;
        // Look again right away: another instance may still be running.
        self.last_resolve = Instant::now() - SCAN_EVERY;
    }

    /// Makes sure the source has a slot in the mix with a bridge whose capture end is here or
    /// in the capture thread. Builds the processor (here, never on the real-time thread).
    fn ensure_slot(&mut self, output: &mut Output) -> anyhow::Result<()> {
        if self.in_mix && (self.producer.is_some() || self.capture.is_some()) {
            return Ok(());
        }
        // The bridge's capture end was lost with a capture thread that never came back.
        self.leave_mix(output);
        let info = &output.info;
        // One 10 ms capture packet plus one output period plus scheduling margin.
        let target_ms = (20.0 + info.period_ms).max(30.0);
        let (producer, consumer, stats) = bridge(BridgeConfig::new(info.rate, target_ms));
        let processor = render::build_processor(&self.cfg.processor, info.rate)?;
        self.slot_fault = Arc::default();
        output.add(Slot::new(self.cfg.id, consumer, processor, self.slot_fault.clone()))?;
        self.producer = Some(producer);
        self.stats = Some(stats);
        self.in_mix = true;
        Ok(())
    }

    /// Fades the source out of the mix (its processor is dropped with its slot).
    fn leave_mix(&mut self, output: &mut Output) {
        if self.in_mix {
            output.remove(self.cfg.id);
        }
        self.forget_bridge();
    }

    /// Stops the capture and drops the bridge (it belonged to a mix the source left, or to an
    /// output that closed).
    fn forget_bridge(&mut self) {
        self.stop_capture();
        self.in_mix = false;
        self.producer = None;
        self.stats = None;
    }

    fn start_capture(&mut self, root: u32, rate: u32) -> anyhow::Result<()> {
        let producer = self.producer.take().context("the source has no bridge")?;
        match capture::spawn(Target::Include(root), rate, Sink::Source { producer, gain: self.gain.clone() }) {
            Ok(c) => {
                self.capture = Some(c);
                self.capture_started = Some(Instant::now());
                self.dropouts_mark = self.stats.as_ref().map(|s| s.dropouts());
                Ok(())
            }
            Err(SpawnError { error, sink }) => {
                if let Some(Sink::Source { producer, .. }) = sink {
                    self.producer = Some(producer);
                }
                Err(error)
            }
        }
    }

    /// Stops the capture thread (if any) and takes the bridge's capture end back. Returns the
    /// thread's error.
    fn stop_capture(&mut self) -> Option<String> {
        let c = self.capture.take()?;
        if let (Some(stats), Some(mark)) = (&self.stats, self.dropouts_mark.take()) {
            self.dropouts_base += stats.dropouts().saturating_sub(mark);
        }
        let (err, sink) = c.stop();
        if let Some(Sink::Source { producer, .. }) = sink {
            self.producer = Some(producer);
        }
        err
    }

    fn restart_capture(&mut self, env: &mut Env, why: String) {
        self.close();
        let (Some(root), Some(output)) = (self.app.as_ref().map(|a| a.root), env.output.as_deref_mut()) else {
            return;
        };
        match self.ensure_slot(output).and_then(|()| self.start_capture(root, output.info.rate)) {
            Ok(()) => {
                self.retry_at = None;
                self.last_error = None;
                if let Some(h) = &mut self.hush
                    && h.tracked_count() > 0
                    && let Err(e) = h.apply(env.ledger)
                {
                    log::warn!("re-hushing {}: {e:#}", self.name);
                }
                self.begin_settle();
            }
            Err(e) => {
                let msg = if why.is_empty() { format!("{e:#}") } else { format!("{why}; restarting failed: {e:#}") };
                self.failed(env, msg);
            }
        }
    }

    /// The capture could not run: the original is made audible again and the capture is
    /// retried later, so the user is never left without the app. Gives up after
    /// [`MAX_FAILURES`] in a row; the other sources are not affected.
    fn failed(&mut self, env: &mut Env, msg: String) {
        log::warn!("{}: {msg}", self.name);
        // Stopped before any volume changes: the makeup gain must never meet the full level.
        self.stop_capture();
        self.unhush(env.ledger);
        self.failures += 1;
        self.retry_at = Some(Instant::now() + RETRY_EVERY);
        if self.failures >= MAX_FAILURES {
            self.fatal = Some(format!("Sidestage could not play {}: {msg}", self.name));
            if let Err(e) = self.finish(env.ledger, &sessions::scan(env.managers)) {
                log::error!("restoring {}: {e:#}", self.name);
            }
            if let Some(o) = env.output.as_deref_mut() {
                self.leave_mix(o);
            }
        }
        self.last_error = Some(msg);
    }

    /// Makes the original audible again (its capture is not playing).
    fn unhush(&mut self, ledger: &Ledger) {
        self.close();
        if let Some(h) = &mut self.hush {
            h.unhush_all(ledger);
        }
    }

    /// Stops the capture and restores the app's volumes; entries of sessions that are not live
    /// stay journaled until the app plays again.
    fn finish(&mut self, ledger: &mut Ledger, live: &[SessionRef]) -> anyhow::Result<()> {
        self.close();
        self.stop_capture();
        match &mut self.hush {
            Some(h) => h.finish(ledger, live),
            None => Ok(()),
        }
    }

    // ---- sessions ------------------------------------------------------------------------

    /// Takes a session scan: `mine` are the attached tree's sessions and `instances` other
    /// instances of the app (both indices into `live`). Hushes new sessions.
    fn scanned(
        &mut self,
        ledger: &mut Ledger,
        live: &[SessionRef],
        peaks: &[Option<f32>],
        mine: &[usize],
        instances: &[usize],
    ) {
        let now = Instant::now();
        self.app_peak = mine.iter().map(|&i| peaks[i].unwrap_or(0.0)).fold(0.0, f32::max);
        self.app_sessions = mine.len();
        // Another instance with audio sessions plays uncaptured (also one started later).
        self.other_instances = !instances.is_empty();
        if mine.is_empty() {
            self.sessionless_since.get_or_insert(now);
        } else {
            self.sessionless_since = None;
        }
        let Some(h) = &mut self.hush else { return };
        h.restore_instances(ledger, live, instances);
        let new: Vec<SessionRef> =
            mine.iter().map(|&i| &live[i]).filter(|s| !h.is_tracked(&s.instance_id)).cloned().collect();
        // Never hushed while nothing plays it.
        if new.is_empty() || self.capture.is_none() || self.adopt_after.is_some_and(|t| now < t) {
            return;
        }
        // Silence the output before the capture level changes.
        self.gain.close();
        match h.adopt(ledger, new) {
            Ok(true) => {
                self.adopt_after = None;
                self.begin_settle();
            }
            Ok(false) => {
                if self.gate == Gate::Open {
                    self.gain.open();
                }
            }
            Err(e) => {
                // The journal could not be written, so nothing was changed: keep playing
                // unhidden and try again later.
                log::error!("could not hide {}: {e:#}", self.name);
                self.last_error = Some(format!("{e:#}"));
                self.adopt_after = Some(now + RETRY_EVERY * 3);
                if self.gate == Gate::Open {
                    self.gain.open();
                }
            }
        }
    }

    /// Someone other than Sidestage changed one of the app's hushed sessions.
    fn volume_changed(&mut self, ledger: &mut Ledger, instance: &str, volume: f32, muted: bool) {
        let Some(h) = &mut self.hush else { return };
        match h.on_foreign_change(ledger, instance, volume, muted) {
            Change::Volume => {
                // One line per burst (a slider drag sends dozens of changes).
                if self.rehush_at.is_none() {
                    log::info!("{}'s volume was changed to {volume:.3} outside Sidestage; adopting it", self.name);
                }
                self.gain.close();
                self.gate = Gate::Closed;
                let wait = if h.fighting() { DEBOUNCE_FIGHTING } else { DEBOUNCE };
                self.rehush_at = Some(Instant::now() + wait);
            }
            Change::Mute => log::info!("{} was {}", self.name, if muted { "muted" } else { "unmuted" }),
            Change::None => {}
        }
    }

    // ---- gate ----------------------------------------------------------------------------

    fn close(&mut self) {
        self.gain.close();
        self.gate = Gate::Closed;
    }

    fn begin_settle(&mut self) {
        self.gain.close();
        self.gate = Gate::Settling(Instant::now() + SETTLE);
        self.settle_tries = 0;
    }

    /// Re-hushes after a capture fault or a debounced foreign change, and opens the gate once
    /// the volumes have settled.
    fn tick_gate(&mut self, ledger: &mut Ledger, now: Instant) {
        if self.gain.take_fault() {
            log::warn!("{}'s captured level jumped unexpectedly; re-hushing", self.name);
            // Usually someone moved the app's volume and the audio got here before the
            // notification: re-hush after the same quiet time as a notified change, or this
            // would set the volume back while the user is still dragging the slider.
            if let Some(h) = &mut self.hush {
                h.rehush_all();
            }
            self.close();
            self.rehush_at.get_or_insert(now + DEBOUNCE);
        }
        if self.rehush_at.is_some_and(|t| now >= t) {
            self.rehush_at = None;
            if let Some(h) = &mut self.hush
                && let Err(e) = h.apply(ledger)
            {
                log::warn!("re-hushing {}: {e:#}", self.name);
            }
            self.begin_settle();
        }
        let Gate::Settling(t) = self.gate else { return };
        if now < t || self.capture.is_none() {
            return;
        }
        let makeup = match &mut self.hush {
            Some(h) if h.hidden() => {
                if !h.settled() {
                    self.settle_tries += 1;
                    if self.settle_tries.is_multiple_of(5) {
                        log::warn!("{}'s volume does not stay where Sidestage set it", self.name);
                    }
                    h.rehush_all();
                    if let Err(e) = h.apply(ledger) {
                        log::warn!("re-hushing {}: {e:#}", self.name);
                    }
                    self.gate = Gate::Settling(now + SETTLE);
                    return;
                }
                h.makeup()
            }
            _ => 1.0,
        };
        self.gain.set_makeup(makeup);
        self.gain.open();
        self.gate = Gate::Open;
        self.running = true;
        log::debug!("{} audible through Sidestage (makeup {makeup:.3e})", self.name);
    }

    // ---- status --------------------------------------------------------------------------

    fn status(&mut self, output: Option<&RenderInfo>, session_down: bool, stopping: bool) -> SourceStatus {
        let now = Instant::now();
        // Capture silent while the app's meter shows sound: exclusive mode or protected content.
        let heard = self.gain.watch.take() > 0.0;
        let muted = self.hush.as_ref().is_some_and(Hush::any_muted);
        let turned_down = self.hush.as_ref().is_some_and(|h| h.hidden() && h.desired() <= 0.0);
        if self.gate == Gate::Open && self.app_peak > 0.01 && !heard && !muted && !turned_down {
            self.silent_since.get_or_insert(now);
        } else {
            self.silent_since = None;
        }

        let app = &self.name;
        let mut warnings = Vec::new();
        if let Some(h) = &self.hush {
            if muted {
                warnings.push(format!("{app} is muted in the Volume mixer."));
            } else if turned_down {
                warnings.push(format!("{app} is turned all the way down in the Volume mixer."));
            }
            if h.failed {
                warnings.push(format!(
                    "{app} cannot be turned down without muting it, so its original sound stays audible."
                ));
            }
            if h.fighting() {
                warnings.push(format!(
                    "{app} keeps changing its own volume; you may hear its original sound now and then."
                ));
            }
        }
        if self.silent_since.is_some_and(|t| now.duration_since(t) > Duration::from_secs(3)) {
            warnings.push(format!(
                "Sidestage hears nothing from {app} although it is playing (exclusive mode or protected content?)."
            ));
        }
        if self.other_instances {
            warnings.push(format!("More than one {app} is running; only one of them is captured."));
        }
        if self.fatal.is_none()
            && let Some(e) = &self.last_error
        {
            warnings.push(format!("Problem playing {app}: {e}"));
        }

        let playing = output.is_some() && self.capture.is_some();
        if !playing || self.app.is_none() {
            self.running = false;
        }
        let state = if let Some(f) = &self.fatal {
            SessionState::Error(f.clone())
        } else if stopping || session_down {
            SessionState::Stopped
        } else if self.app.is_none() && self.failures > 0 {
            SessionState::Starting
        } else if self.app.is_none() || self.app_sessions == 0 {
            // Not running, or running without an audio stream yet (it gets one on first play).
            SessionState::WaitingForApp
        } else if playing && (self.gate == Gate::Open || self.running) {
            SessionState::Running
        } else {
            SessionState::Starting
        };
        let (fill, drift, counting) = match &self.stats {
            Some(s) => {
                (s.fill_ms(), s.correction_ppm(), self.dropouts_mark.map_or(0, |m| s.dropouts().saturating_sub(m)))
            }
            None => (0.0, 0.0, 0),
        };
        SourceStatus {
            id: self.cfg.id,
            app_name: self.name.clone(),
            state,
            original_hidden: playing && self.hush.as_ref().is_some_and(Hush::hidden),
            // Filled in by `Session::status` (peak since the last call).
            capture_peak: 0.0,
            // One 10 ms capture packet, the bridge and the output buffer.
            latency_ms: if playing { fill + 10.0 + output.map_or(0.0, |o| o.buffer_ms) } else { 0.0 },
            dropouts: self.dropouts_base + counting + self.gain.gaps.load(Ordering::Relaxed),
            drift_ppm: if playing { drift } else { 0.0 },
            warnings,
        }
    }
}

/// Notification registrations on one endpoint, undone on drop.
struct Watch {
    endpoint_id: String,
    _sessions: Option<SessionWatch>,
    ducks: Option<(IAudioSessionManager2, IAudioVolumeDuckNotification)>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some((mgr, ducks)) = &self.ducks {
            let _ = unsafe { mgr.UnregisterDuckNotification(ducks) };
        }
    }
}

/// The output stream and how it is doing.
struct Out {
    /// The device the user chose (`None` = follow the default).
    chosen: Option<String>,
    master: ProcessorFactory,
    output: Option<Output>,
    /// The chosen device is missing; playing on the default.
    fallback: bool,
    check_at: Option<Instant>,
    failures: u32,
    retry_at: Option<Instant>,
    last_error: Option<String>,
    fatal: Option<String>,
    /// Underruns of outputs that were closed.
    dropouts_base: u64,
}

/// Who a session's process is, for the sidechain (cached per process).
#[derive(Clone, Default)]
struct Owner {
    /// Sidestage itself or a process it started.
    mine: bool,
    /// The app's root process (the highest ancestor with the same executable).
    root: Root,
    /// The root is Sidestage's ancestor: capturing its tree would capture Sidestage.
    ancestor: bool,
    /// Match key of the app the session belongs to (its root's executable).
    app: Option<String>,
}

struct Supervisor {
    shared: Arc<Shared>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    build: u32,
    en: IMMDeviceEnumerator,
    _devices: DeviceWatch,
    managers: Managers,
    watches: Vec<Watch>,
    names: Names,
    me: Me,
    out: Out,
    sources: Vec<Source>,
    ledger: Ledger,
    sidechain: Option<Sidechain>,
    /// Match keys of `SessionConfig::sidechain_ignore`.
    ignore: Vec<String>,
    owners: HashMap<(u32, u64), Owner>,
    ducked: bool,
    duck_does_nothing: bool,
    devices_changed: bool,
    scan_now: bool,
    last_scan: Instant,
    stopping: bool,
}

impl Supervisor {
    fn new(
        cfg: SessionConfig,
        build: u32,
        shared: Arc<Shared>,
        tx: Sender<Event>,
        rx: Receiver<Event>,
    ) -> anyhow::Result<Self> {
        let en = sessions::enumerator()?;
        let devices = DeviceWatch::register(&en, tx.clone()).context("watching audio devices")?;
        if let Some(id) = &cfg.output
            && !output_active(&en, id)
        {
            bail!("the chosen output device is not available");
        }
        let sidechain = cfg.sidechain.then(|| Sidechain::new(shared.sidechain.clone()));
        let mut sup = Supervisor {
            shared,
            tx,
            rx,
            build,
            en,
            _devices: devices,
            managers: Managers::default(),
            watches: Vec::new(),
            names: Names::default(),
            me: Me::new()?,
            out: Out {
                chosen: cfg.output,
                master: cfg.master,
                output: None,
                fallback: false,
                check_at: None,
                failures: 0,
                retry_at: None,
                last_error: None,
                fatal: None,
                dropouts_base: 0,
            },
            sources: Vec::new(),
            ledger: Ledger::new(),
            sidechain,
            ignore: Vec::new(),
            owners: HashMap::new(),
            ducked: false,
            duck_does_nothing: ducking_disabled(),
            devices_changed: false,
            scan_now: false,
            last_scan: Instant::now(),
            stopping: false,
        };
        sup.refresh_endpoints();
        sup.set_ignore(&cfg.sidechain_ignore);
        sup.shared.status().state = SessionState::Starting;
        Ok(sup)
    }

    /// Opens the output and attaches every source whose app is running. Runs before `start`
    /// returns, so an output that cannot be opened is reported to the caller.
    fn begin(&mut self, sources: Vec<SourceConfig>) -> anyhow::Result<()> {
        let opened = self.open_output();
        self.install(sources);
        self.lifecycle(Instant::now());
        self.scan();
        self.publish();
        opened
    }

    fn run(&mut self) {
        loop {
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Event::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(ev) => self.handle(ev),
                Err(RecvTimeoutError::Timeout) => {}
            }
            // Drain bursts before doing periodic work.
            while let Ok(ev) = self.rx.try_recv() {
                if matches!(ev, Event::Stop) {
                    return;
                }
                self.handle(ev);
            }
            self.tick();
        }
    }

    fn handle(&mut self, ev: Event) {
        match ev {
            Event::Stop => {}
            Event::SetSources(list, ack) => {
                self.install(list);
                self.publish();
                let _ = ack.try_send(());
                // New sources attach right away, after the caller has its answer.
                if self.out.fatal.is_none() {
                    self.lifecycle(Instant::now());
                    self.scan();
                }
                self.publish();
            }
            Event::SetIgnore(apps) => {
                self.set_ignore(&apps);
                self.scan_now = true;
            }
            Event::DefaultOutputChanged => {
                if self.out.chosen.is_none() || self.out.fallback {
                    self.out.check_at = Some(Instant::now() + DEVICE_DEBOUNCE);
                }
            }
            Event::DevicesChanged => {
                self.devices_changed = true;
                self.out.check_at = Some(Instant::now() + DEVICE_DEBOUNCE);
            }
            Event::SessionCreated => self.scan_now = true,
            Event::SessionGone { hush, instance } => {
                if let Some(h) = self.sources.iter_mut().find(|s| s.token() == Some(hush)).and_then(|s| s.hush.as_mut())
                {
                    h.forget(&instance);
                }
                self.scan_now = true;
            }
            Event::VolumeChanged { hush, instance, volume, muted } => {
                // A late event of a removed source finds nothing.
                if let Some(s) = self.sources.iter_mut().find(|s| s.token() == Some(hush)) {
                    s.volume_changed(&mut self.ledger, &instance, volume, muted);
                }
            }
            Event::Ducking(on) => {
                self.ducked = on;
                if on {
                    self.duck_does_nothing = ducking_disabled();
                }
            }
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.out.fatal.is_some() {
            self.publish();
            return;
        }
        if self.devices_changed {
            self.devices_changed = false;
            self.refresh_endpoints();
            self.scan_now = true;
        }
        if self.out.check_at.is_some_and(|t| now >= t) {
            self.out.check_at = None;
            self.check_output();
        }
        self.output_health(now);
        if let Some(o) = &mut self.out.output {
            o.collect_garbage();
        }
        self.lifecycle(now);
        if self.scan_now || now.duration_since(self.last_scan) >= SCAN_EVERY {
            self.scan();
        }
        for s in self.sources.iter_mut().filter(|s| s.fatal.is_none()) {
            s.tick_gate(&mut self.ledger, now);
        }
        if let Some(sc) = &mut self.sidechain {
            sc.tick();
        }
        // Volume changes are written in one go once the user stops dragging.
        if self.sources.iter().all(|s| s.rehush_at.is_none())
            && let Err(e) = self.ledger.flush()
        {
            log::warn!("{e:#}");
        }
        self.publish();
    }

    /// Splits off what a source's operations need.
    fn split(&mut self) -> (&mut [Source], Env<'_>) {
        (
            &mut self.sources,
            Env {
                output: self.out.output.as_mut(),
                ledger: &mut self.ledger,
                managers: &self.managers,
                names: &mut self.names,
                me: &self.me,
            },
        )
    }

    // ---- sources -------------------------------------------------------------------------

    /// Replaces the source list. Kept sources are not touched; removed ones are stopped and
    /// restored before new ones are created (a new source may hide the app a removed one has
    /// just given back).
    fn install(&mut self, list: Vec<SourceConfig>) {
        let mut old = std::mem::take(&mut self.sources);
        let mut kept: Vec<Option<Source>> = Vec::with_capacity(list.len());
        let mut added = Vec::new();
        for cfg in list {
            let same = |s: &Source| s.cfg.id == cfg.id && s.cfg.hide == cfg.hide && s.cfg.app.same_app(&cfg.app);
            match old.iter().position(same) {
                Some(i) => {
                    let mut s = old.swap_remove(i);
                    // Its processor keeps running; the new factory is used when the output
                    // is reopened.
                    s.cfg.processor = cfg.processor;
                    kept.push(Some(s));
                }
                None => {
                    kept.push(None);
                    added.push(cfg);
                }
            }
        }
        if !old.is_empty() {
            let live = sessions::scan(&self.managers);
            for mut s in old {
                log::info!("removing {}", s.name);
                if let Some(o) = &mut self.out.output {
                    // The mixer fades it out; the capture stops before the volume comes back.
                    s.leave_mix(o);
                }
                if let Err(e) = s.finish(&mut self.ledger, &live) {
                    log::error!("restoring {}: {e:#}", s.name);
                }
            }
        }
        let live = if added.is_empty() { Vec::new() } else { sessions::scan(&self.managers) };
        let mut added = added.into_iter();
        let mut sources = Vec::with_capacity(kept.len());
        for s in kept {
            let s = match s {
                Some(s) => s,
                None => {
                    let Some(cfg) = added.next() else { continue };
                    let s = Source::new(cfg, &self.tx, &mut self.names, &live, &self.me);
                    log::info!("adding {}", s.name);
                    s
                }
            };
            sources.push(s);
        }
        self.sources = sources;
        *lock(&self.shared.gains) = self.sources.iter().map(|s| (s.cfg.id, s.gain.clone())).collect();
        self.scan_now = true;
    }

    /// App lifetimes and capture health of every source.
    fn lifecycle(&mut self, now: Instant) {
        let lookup = self.out.output.is_some() && self.sources.iter().any(|s| s.wants_lookup(now));
        let mut procs = None;
        let live = if lookup {
            procs = ProcessTable::snapshot().map_err(|e| log::debug!("listing processes: {e:#}")).ok();
            sessions::scan(&self.managers)
        } else {
            Vec::new()
        };
        let mut attached = false;
        let (sources, mut env) = self.split();
        for s in sources {
            attached |= s.tick(&mut env, now, &mut procs, &live);
        }
        if attached {
            self.scan_now = true;
        }
    }

    fn set_ignore(&mut self, apps: &[AppKey]) {
        self.ignore = apps.iter().map(|a| match_key(&a.0)).collect();
    }

    // ---- output --------------------------------------------------------------------------

    /// The output to open: the chosen one if it is there, else the default.
    fn wanted_output(&mut self) -> Option<String> {
        match &self.out.chosen {
            Some(id) if output_active(&self.en, id) => {
                self.out.fallback = false;
                Some(id.clone())
            }
            Some(_) => {
                self.out.fallback = true;
                None
            }
            None => None,
        }
    }

    fn open_output(&mut self) -> anyhow::Result<()> {
        let device = self.wanted_output();
        let levels = self.sidechain.as_ref().map(|s| s.levels().clone());
        let output = Output::open(device, levels, &self.out.master)?;
        let info = &output.info;
        log::info!(
            "output: {} ({} Hz, {} ch, {:.0} ms buffer, {:.1} ms period)",
            info.device_name,
            info.rate,
            info.channels,
            info.buffer_ms,
            info.period_ms
        );
        self.out.output = Some(output);
        self.out.retry_at = None;
        self.out.last_error = None;
        // Sources that were attached join the new output with new bridges and processors.
        let (sources, mut env) = self.split();
        for s in sources.iter_mut().filter(|s| s.app.is_some() && s.fatal.is_none()) {
            s.restart_capture(&mut env, String::new());
        }
        self.scan_now = true;
        Ok(())
    }

    /// Stops every capture and the output; the bridges belonged to the output's mixer.
    fn close_output(&mut self) {
        for s in &mut self.sources {
            s.close();
            s.forget_bridge();
        }
        if let Some(mut o) = self.out.output.take() {
            o.shutdown();
            self.out.dropouts_base += o.stats.underruns.load(Ordering::Relaxed);
        }
    }

    fn output_health(&mut self, now: Instant) {
        let Some(o) = &mut self.out.output else {
            if self.out.retry_at.is_none_or(|t| now >= t) {
                self.restart_output(String::new());
            }
            return;
        };
        if self.out.failures > 0 && o.started.elapsed() >= STABLE_AFTER {
            self.out.failures = 0;
        }
        if !o.finished() {
            return;
        }
        let short = o.started.elapsed() < STABLE_AFTER;
        let err = o.shutdown().unwrap_or_else(|| "the output stopped".to_owned());
        log::warn!("output stopped: {err}");
        if short {
            // Dying again right after a (re)start counts toward giving up.
            self.output_failed(err);
        } else {
            self.restart_output(err);
        }
    }

    /// Reopens the output after a failure or a device change; if that fails, every app is made
    /// audible again and the output is retried later.
    fn restart_output(&mut self, why: String) {
        self.close_output();
        if let Err(e) = self.open_output() {
            let msg = if why.is_empty() { format!("{e:#}") } else { format!("{why}; reopening failed: {e:#}") };
            self.output_failed(msg);
        }
    }

    fn output_failed(&mut self, msg: String) {
        log::warn!("output failed: {msg}");
        // Captures stop before any volume changes.
        self.close_output();
        for s in &mut self.sources {
            s.unhush(&self.ledger);
        }
        self.out.failures += 1;
        self.out.retry_at = Some(Instant::now() + RETRY_EVERY);
        if self.out.failures >= MAX_FAILURES {
            self.out.fatal = Some(format!("Sidestage could not play to the output: {msg}"));
            let live = sessions::scan(&self.managers);
            for s in &mut self.sources {
                if let Err(e) = s.finish(&mut self.ledger, &live) {
                    log::error!("restoring {}: {e:#}", s.name);
                }
            }
        }
        self.out.last_error = Some(msg);
    }

    /// Reopens the output when the default device changed (or a chosen device came back).
    fn check_output(&mut self) {
        let Some(o) = &self.out.output else { return };
        let want = match &self.out.chosen {
            Some(id) if output_active(&self.en, id) => id.clone(),
            _ => match sessions::default_render(&self.en).and_then(|d| sessions::device_id(&d).ok()) {
                Some(id) => id,
                None => return,
            },
        };
        if o.info.device_id != want {
            log::info!("output changed; reopening");
            self.restart_output(String::new());
        }
    }

    // ---- sessions ------------------------------------------------------------------------

    fn refresh_endpoints(&mut self) {
        let (gone, added) = match self.managers.refresh(&self.en) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("listing audio outputs: {e:#}");
                return;
            }
        };
        let gone_ids: HashSet<String> = gone.into_iter().map(|m| m.endpoint_id).collect();
        self.watches.retain(|w| !gone_ids.contains(&w.endpoint_id));
        for i in added {
            let m = &self.managers.list[i];
            let tx = self.tx.clone();
            let sessions = SessionWatch::register(m, move || {
                let _ = tx.send(Event::SessionCreated);
            })
            .map_err(|e| log::debug!("watching sessions on {}: {e}", m.endpoint_name))
            .ok();
            let ducks = notify::duck_events(self.tx.clone());
            // A null session ID asks for every ducking notification on the endpoint.
            let ducks = match unsafe { m.mgr.RegisterDuckNotification(PCWSTR::null(), &ducks) } {
                Ok(()) => Some((m.mgr.clone(), ducks)),
                Err(e) => {
                    log::debug!("watching ducking on {}: {e}", m.endpoint_name);
                    None
                }
            };
            self.watches.push(Watch { endpoint_id: m.endpoint_id.clone(), _sessions: sessions, ducks });
        }
    }

    /// Who a session's process is, cached by PID and creation time.
    fn owner(&mut self, pid: u32, created: u64, procs: &mut Option<ProcessTable>) -> Owner {
        if created != 0
            && let Some(o) = self.owners.get(&(pid, created))
        {
            return o.clone();
        }
        let Some(t) = table(procs) else {
            return Owner::default();
        };
        let mine = pid == self.me.pid || t.is_in_tree(pid, self.me.pid);
        let root = t.root_of(pid);
        let root_created = t.created(root).unwrap_or(0);
        let o = Owner {
            mine,
            root: (root, root_created),
            ancestor: self.me.is_lineage(root, root_created),
            app: t.path(root).map(|p| match_key(&app_key(&p))),
        };
        if created != 0 {
            self.owners.insert((pid, created), o.clone());
        }
        o
    }

    /// Hands every session to its source (hushing new ones), restores leftovers of apps that
    /// are not sources, and gives the sidechain the sessions that count as the game.
    fn scan(&mut self) {
        self.last_scan = Instant::now();
        self.scan_now = false;
        let live = sessions::scan(&self.managers);
        let peaks = sessions::trusted_peaks(&live);
        let created: Vec<u64> = live.iter().map(|s| process_created(s.pid).unwrap_or(0)).collect();
        let mut procs: Option<ProcessTable> = None;

        let mut member: Vec<Vec<Membership>> = Vec::with_capacity(self.sources.len());
        for s in &mut self.sources {
            let row = match &mut s.app {
                Some(att) if s.fatal.is_none() => {
                    (0..live.len()).map(|j| att.contains(live[j].pid, created[j], &s.match_key, &mut procs)).collect()
                }
                _ => vec![Membership::No; live.len()],
            };
            member.push(row);
        }
        let owners: Vec<Owner> = (0..live.len()).map(|j| self.owner(live[j].pid, created[j], &mut procs)).collect();
        let seen: HashSet<(u32, u64)> = live.iter().zip(&created).map(|(s, &c)| (s.pid, c)).collect();
        self.owners.retain(|k, _| seen.contains(k));
        // Each session belongs to at most one source: the one whose app it is, else the one
        // whose process tree it is in (a helper with another executable).
        let claimed: Vec<Option<usize>> = (0..live.len())
            .map(|j| {
                let in_tree = |i: usize| member[i][j] == Membership::Tree;
                let app = owners[j].app.as_deref();
                (0..self.sources.len())
                    .find(|&i| in_tree(i) && app == Some(self.sources[i].match_key.as_str()))
                    .or_else(|| (0..self.sources.len()).find(|&i| in_tree(i)))
            })
            .collect();
        for (i, s) in self.sources.iter_mut().enumerate() {
            if s.app.is_none() || s.fatal.is_some() {
                continue;
            }
            let mine: Vec<usize> = (0..live.len()).filter(|&j| claimed[j] == Some(i)).collect();
            let instances: Vec<usize> = (0..live.len())
                .filter(|&j| claimed[j].is_none() && member[i][j] == Membership::OtherInstance)
                .collect();
            s.scanned(&mut self.ledger, &live, &peaks, &mine, &instances);
        }

        // Leftovers of apps no source is hiding (earlier runs, removed sources whose app was
        // closed, a source that gave up, a source that keeps its original audible) are restored
        // when their app plays again.
        let hiding = |s: &Source| s.hush.is_some() && s.fatal.is_none();
        let free: Vec<SessionRef> = (0..live.len())
            .filter(|&j| claimed[j].is_none_or(|i| !hiding(&self.sources[i])))
            .map(|j| live[j].clone())
            .collect();
        let sources = &self.sources;
        self.ledger.restore_foreign(&free, |app| sources.iter().any(|s| hiding(s) && s.is_app(app)));

        self.update_sidechain(&live, &owners, &claimed);
    }

    /// The game: the apps of every session except the sources', the ignored apps and
    /// Sidestage's own process tree (the system sounds session is never listed). An ancestor of
    /// Sidestage cannot be listened to without hearing Sidestage, so it does not count.
    fn update_sidechain(&mut self, live: &[SessionRef], owners: &[Owner], claimed: &[Option<usize>]) {
        let Some(sc) = &mut self.sidechain else { return };
        let mut roots: Vec<Root> = Vec::new();
        for j in 0..live.len() {
            let o = &owners[j];
            let app = o.app.as_deref().unwrap_or("");
            let counts = claimed[j].is_none()
                && !o.mine
                && !o.ancestor
                && o.root.1 != 0
                && !self.sources.iter().any(|s| s.match_key == app)
                && !self.ignore.iter().any(|k| k == app);
            if counts && !roots.contains(&o.root) {
                roots.push(o.root);
            }
        }
        sc.listen(&roots);
    }

    // ---- status --------------------------------------------------------------------------

    fn publish(&mut self) {
        let mut warnings = Vec::new();
        if (MIN_BUILD..SUPPORTED_BUILD).contains(&self.build) {
            warnings.push(format!(
                "This Windows version (build {}) is older than the first one that officially supports capturing a single app; it may not work.",
                self.build
            ));
        }
        if self.ducked && !self.duck_does_nothing {
            warnings.push(
                "Windows communications ducking is lowering other sounds (Sound settings > More sound settings > Communications)."
                    .to_owned(),
            );
        }
        if self.out.fallback {
            warnings.push("The chosen output is not available; playing on the default output.".to_owned());
        }
        let info = self.out.output.as_ref().map(|o| o.info.clone());
        if let Some(i) = &info {
            if i.spatial {
                warnings.push(format!(
                    "Spatial sound (Windows Sonic, Dolby Atmos or DTS) is on for {}; turn it off for the best 3D effect.",
                    i.device_name
                ));
            }
            if i.channels == 1 || i.rate <= 16_000 {
                warnings.push(format!("{} is in headset (hands-free) mode with low sound quality.", i.device_name));
            }
        }
        if let Some(sc) = &self.sidechain
            && sc.unheard() > 0
        {
            warnings
                .push(format!("Ducking cannot hear {} of your apps, so they do not lower the sources.", sc.unheard()));
        }
        if self.out.fatal.is_none()
            && let Some(e) = &self.out.last_error
        {
            warnings.push(format!("Problem with the output: {e}"));
        }

        let down = self.out.fatal.is_some();
        let stopping = self.stopping;
        let sources: Vec<SourceStatus> =
            self.sources.iter_mut().map(|s| s.status(info.as_ref(), down, stopping)).collect();
        let state = if let Some(f) = &self.out.fatal {
            SessionState::Error(f.clone())
        } else if self.stopping {
            SessionState::Stopped
        } else if self.out.output.is_some() {
            SessionState::Running
        } else {
            SessionState::Starting
        };
        let sidechain_active = self.out.output.is_some() && self.sidechain.is_some();
        let dropouts =
            self.out.dropouts_base + self.out.output.as_ref().map_or(0, |o| o.stats.underruns.load(Ordering::Relaxed));

        let mut st = self.shared.status();
        st.state = state;
        st.output_name = info.as_ref().map(|i| i.device_name.clone()).unwrap_or_default();
        st.sample_rate = info.as_ref().map_or(0, |i| i.rate);
        st.sidechain_active = sidechain_active;
        st.latency_ms = info.as_ref().map_or(0.0, |i| i.buffer_ms);
        st.dropouts = dropouts;
        st.warnings = warnings;
        st.sources = sources;
    }

    /// Stops everything and restores every app's volume.
    fn finish(&mut self) -> anyhow::Result<()> {
        self.stopping = true;
        for s in &mut self.sources {
            s.close();
        }
        if self.sources.iter().any(|s| s.capture.is_some()) {
            // Let the closed gates fade out and the bridges drain before the output stops.
            thread::sleep(Duration::from_millis(60));
        }
        self.close_output();
        // No ducking input outlives the session.
        self.sidechain = None;
        let live = sessions::scan(&self.managers);
        let mut result = Ok(());
        for s in &mut self.sources {
            if let Err(e) = s.finish(&mut self.ledger, &live) {
                log::error!("restoring {}: {e:#}", s.name);
                if result.is_ok() {
                    result = Err(e);
                }
            }
        }
        let released = self.ledger.release(&live);
        self.watches.clear();
        self.publish();
        result.and(released)
    }
}

/// True if the endpoint exists and is active.
fn output_active(en: &IMMDeviceEnumerator, id: &str) -> bool {
    unsafe { en.GetDevice(&HSTRING::from(id)) }
        .and_then(|d| unsafe { d.GetState() })
        .is_ok_and(|s| s == DEVICE_STATE_ACTIVE)
}

/// True if the user set Windows to "Do nothing" when it detects communications activity.
fn ducking_disabled() -> bool {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Multimedia\\Audio"),
            w!("UserDuckingPreference"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    r == ERROR_SUCCESS && value == 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Processor;

    struct Silence;
    impl Processor for Silence {
        fn process(&mut self, _input: &[f32], output: &mut [f32], _sidechain_peak: f32) {
            output.fill(0.0);
        }
    }

    fn source(id: SourceId, app: &str, hide: HideMethod) -> SourceConfig {
        SourceConfig {
            id,
            app: AppKey(app.into()),
            hide,
            processor: Arc::new(|_| Box::new(Silence) as Box<dyn Processor>),
        }
    }

    #[test]
    fn invalid_source_lists_are_refused() {
        assert!(check_sources(&[]).is_ok());
        let ok =
            [source(1, r"c:\a\music.exe", HideMethod::Auto), source(2, r"c:\b\voice.exe", HideMethod::KeepOriginal)];
        assert!(check_sources(&ok).is_ok());
        let dup = [source(1, r"c:\a\music.exe", HideMethod::Auto), source(2, r"C:\A\Music.exe", HideMethod::Auto)];
        assert!(check_sources(&dup).unwrap_err().to_string().contains("twice"));
        let squirrel = [
            source(1, r"c:\users\u\appdata\local\discord\app-1.0.9\discord.exe", HideMethod::Auto),
            source(2, r"c:\users\u\appdata\local\discord\app-1.0.10\discord.exe", HideMethod::Auto),
        ];
        assert!(check_sources(&squirrel).is_err(), "two versions of one app are one app");
        let park = [source(1, r"c:\a\music.exe", HideMethod::ParkOn("x".into()))];
        assert!(check_sources(&park).unwrap_err().to_string().contains("not supported"));
        let many: Vec<SourceConfig> =
            (0..=MAX_SOURCES as u32).map(|i| source(i, &format!(r"c:\app{i}.exe"), HideMethod::Auto)).collect();
        assert!(check_sources(&many).is_err());
    }
}
