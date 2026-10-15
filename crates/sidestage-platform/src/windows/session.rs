//! A running session: one supervisor thread (MTA) that finds the app, keeps its sessions
//! hushed, runs the capture/render pipeline, follows device changes, re-attaches when the app
//! restarts and publishes the status.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, IAudioSessionManager2, IAudioVolumeDuckNotification, IMMDeviceEnumerator,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::System::Threading::{PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
use windows::core::{HSTRING, PCWSTR, w};

use super::capture::{self, Gain, SidechainLevels, Sink, Target};
use super::com::{Handle, Mta, created_of, open_process, panic_message, process_alive, process_created};
use super::hush::{Change, Hush};
use super::notify::{self, DeviceWatch, Event, SessionWatch};
use super::procs::{Names, ProcessTable, exe_file_name, match_key};
use super::render::{self, RenderInfo, RenderStart};
use super::sessions::{self, Managers, SessionRef};
use super::{MIN_BUILD, SUPPORTED_BUILD};
use crate::bridge::{BridgeConfig, BridgeStats, bridge};
use crate::{HideMethod, ProcessorFactory, Session, SessionConfig, SessionState, SessionStatus};

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
/// Consecutive pipeline failures before the session gives up.
const MAX_FAILURES: u32 = 5;
/// A pipeline that has run this long has recovered; one that stops sooner has failed again.
const STABLE_AFTER: Duration = Duration::from_secs(10);

/// How long `start` waits for the first attach (opening the output and the capture). Usually
/// 50-60 ms; if the audio system is slower than this, `start` returns the session while it is
/// still starting and later problems show up in its status, so the caller (the UI thread) is
/// never blocked for long.
const START_WAIT: Duration = Duration::from_millis(1500);

pub fn start(config: SessionConfig, factory: ProcessorFactory, build: u32) -> anyhow::Result<Box<dyn Session>> {
    let shared = Arc::new(Shared {
        status: Mutex::new(SessionStatus::default()),
        gain: Arc::new(Gain::new()),
        sidechain: Arc::new(SidechainLevels::default()),
    });
    let (tx, rx) = mpsc::channel::<Event>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<anyhow::Result<()>>(1);
    let (done_tx, done_rx) = mpsc::sync_channel::<anyhow::Result<()>>(1);
    let sup_shared = shared.clone();
    let sup_tx = tx.clone();
    let thread = thread::Builder::new()
        .name("sidestage-session".to_owned())
        .spawn(move || {
            // Also shown in the status, in case `start` has stopped waiting.
            let fail = |e: anyhow::Error, shared: &Shared| {
                shared.status().state = SessionState::Error(format!("{e:#}"));
                let _ = ready_tx.send(Err(e));
            };
            let _mta = match Mta::enter() {
                Ok(m) => m,
                Err(e) => return fail(e, &sup_shared),
            };
            let mut sup = match Supervisor::new(config, factory, sup_shared.clone(), sup_tx, rx, build) {
                Ok(s) => s,
                Err(e) => return fail(e, &sup_shared),
            };
            let first = sup.first_attach();
            // Handled like any later failure (retried) in case `start` stopped waiting; if it
            // is still waiting, it reports the error and stops the session.
            if let Err(e) = &first {
                sup.pipeline_failed(format!("{e:#}"));
            }
            let _ = ready_tx.send(first);
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sup.run()));
            if let Err(p) = &run {
                log::error!("session supervisor panicked: {}", panic_message(p));
            }
            let _ = done_tx.send(sup.finish());
        })
        .context("spawning the session thread")?;
    let mut session = WindowsSession { tx, shared, thread: Some(thread), done: done_rx };
    match ready_rx.recv_timeout(START_WAIT) {
        Ok(Ok(())) => Ok(Box::new(session)),
        Err(RecvTimeoutError::Timeout) => {
            log::warn!("the audio system is slow to respond; the session keeps starting");
            Ok(Box::new(session))
        }
        Ok(Err(e)) => {
            // Stops the supervisor if it got as far as running (and restores the app).
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

/// State shared between the supervisor, the audio threads and the session handle.
struct Shared {
    status: Mutex<SessionStatus>,
    gain: Arc<Gain>,
    sidechain: Arc<SidechainLevels>,
}

impl Shared {
    fn status(&self) -> MutexGuard<'_, SessionStatus> {
        self.status.lock().unwrap_or_else(|p| p.into_inner())
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
                Err(anyhow!("the session thread ended without restoring the app"))
            }
            // Leave the stuck thread behind rather than hang the caller; the journal stays
            // dirty, so the guardian or the next start restores the volume.
            Err(RecvTimeoutError::Timeout) => {
                Err(anyhow!("the session did not stop in time; the app's volume is restored when Sidestage exits"))
            }
        }
    }
}

impl Session for WindowsSession {
    fn status(&self) -> SessionStatus {
        let mut s = self.shared.status().clone();
        s.capture_peak = self.shared.gain.peak.take();
        s.sidechain_peak = self.shared.sidechain.hold.take();
        s
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

/// The app's process tree we are attached to.
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

/// The running audio threads.
struct Pipeline {
    stop: Arc<Handle>,
    render: Option<JoinHandle<anyhow::Result<()>>>,
    capture: Option<JoinHandle<anyhow::Result<()>>>,
    sidechain: Option<JoinHandle<anyhow::Result<()>>>,
    stats: Arc<BridgeStats>,
    info: RenderInfo,
    started: Instant,
}

impl Pipeline {
    fn finished(&self) -> bool {
        [&self.render, &self.capture].iter().any(|t| t.as_ref().is_some_and(|t| t.is_finished()))
    }

    /// If the sidechain thread ended on its own, its error (the music keeps playing).
    fn sidechain_ended(&mut self) -> Option<String> {
        if !self.sidechain.as_ref().is_some_and(|t| t.is_finished()) {
            return None;
        }
        let t = self.sidechain.take()?;
        Some(match t.join() {
            Ok(Ok(())) => "the ducking input stopped".to_owned(),
            Ok(Err(e)) => format!("{e:#}"),
            Err(p) => format!("the ducking thread panicked: {}", panic_message(&p)),
        })
    }

    /// Stops the threads and returns the first error one of them reported.
    fn shutdown(&mut self) -> Option<String> {
        self.stop.set();
        let mut first = None;
        for (what, t) in
            [("output", self.render.take()), ("capture", self.capture.take()), ("ducking", self.sidechain.take())]
        {
            let Some(t) = t else { continue };
            let err = match t.join() {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(format!("{what}: {e:#}")),
                Err(p) => Some(format!("{what} thread panicked: {}", panic_message(&p))),
            };
            if first.is_none() {
                first = err;
            }
        }
        first
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shutdown();
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum Gate {
    Closed,
    Settling(Instant),
    Open,
}

struct Supervisor {
    cfg: SessionConfig,
    key: String,
    /// `key` without a Store app's package version, so the app is found again after an update.
    match_key: String,
    factory: ProcessorFactory,
    shared: Arc<Shared>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    build: u32,
    en: IMMDeviceEnumerator,
    _devices: DeviceWatch,
    managers: Managers,
    watches: Vec<Watch>,
    names: Names,
    app_name: String,
    app: Option<Attached>,
    pipeline: Option<Pipeline>,
    hush: Option<Hush>,
    gate: Gate,
    settle_tries: u32,
    rehush_at: Option<Instant>,
    output_check_at: Option<Instant>,
    devices_changed: bool,
    scan_now: bool,
    last_scan: Instant,
    last_resolve: Instant,
    failures: u32,
    retry_at: Option<Instant>,
    last_error: Option<String>,
    fatal: Option<String>,
    dropouts_base: u64,
    output_fallback: bool,
    sidechain_error: Option<String>,
    ducked: bool,
    duck_does_nothing: bool,
    /// Loudest meter reading of the app's sessions in the last scan (pre-volume).
    app_peak: f32,
    /// Audio sessions of the attached tree in the last scan, and since when there were none.
    app_sessions: usize,
    sessionless_since: Option<Instant>,
    silent_since: Option<Instant>,
    /// Another instance of the app has audio sessions (it plays uncaptured).
    other_instances: bool,
    stopping: bool,
    /// Hushing new sessions failed; do not retry before this.
    adopt_after: Option<Instant>,
}

impl Supervisor {
    fn new(
        cfg: SessionConfig,
        factory: ProcessorFactory,
        shared: Arc<Shared>,
        tx: Sender<Event>,
        rx: Receiver<Event>,
        build: u32,
    ) -> anyhow::Result<Self> {
        let en = sessions::enumerator()?;
        let devices = DeviceWatch::register(&en, tx.clone()).context("watching audio devices")?;

        if let Some(id) = &cfg.output
            && !output_active(&en, id)
        {
            anyhow::bail!("the chosen output device is not available");
        }
        let key = cfg.app.0.to_lowercase();
        let match_key = match_key(&key);
        // Capturing a tree that contains Sidestage would feed its own output back.
        let mut procs = ProcessTable::snapshot()?;
        for pid in procs.lineage(std::process::id()) {
            if procs.path(pid).is_some_and(|p| super::procs::match_key(&p) == match_key) {
                anyhow::bail!(
                    "Sidestage cannot capture itself or the program that started it ({})",
                    exe_file_name(&key)
                );
            }
        }
        let mut names = Names::default();
        let app_name = names.app_name("", &key);
        let now = Instant::now();
        let mut sup = Supervisor {
            key,
            match_key,
            factory,
            shared,
            tx,
            rx,
            build,
            en,
            _devices: devices,
            managers: Managers::default(),
            watches: Vec::new(),
            names,
            app_name,
            app: None,
            pipeline: None,
            hush: None,
            gate: Gate::Closed,
            settle_tries: 0,
            rehush_at: None,
            output_check_at: None,
            devices_changed: false,
            scan_now: false,
            last_scan: now,
            last_resolve: now,
            failures: 0,
            retry_at: None,
            last_error: None,
            fatal: None,
            dropouts_base: 0,
            output_fallback: false,
            sidechain_error: None,
            ducked: false,
            duck_does_nothing: ducking_disabled(),
            app_peak: 0.0,
            app_sessions: 0,
            sessionless_since: None,
            silent_since: None,
            other_instances: false,
            stopping: false,
            adopt_after: None,
            cfg,
        };
        sup.refresh_endpoints();
        sup.app_name = sup.find_app_name();
        if sup.cfg.hide == HideMethod::Auto {
            sup.hush = Some(Hush::begin(&sup.key, &sup.app_name, sup.shared.gain.clone(), sup.tx.clone())?);
        }
        {
            let mut st = sup.shared.status();
            st.state = SessionState::Starting;
            st.app_name = sup.app_name.clone();
        }
        Ok(sup)
    }

    /// The first attach runs before `start` returns so setup errors reach the caller.
    fn first_attach(&mut self) -> anyhow::Result<()> {
        if let Some((att, _)) = self.resolve()? {
            self.attach(att)?;
        } else {
            log::info!("{} is not running; waiting for it", self.app_name);
        }
        self.publish();
        Ok(())
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
            Event::DefaultOutputChanged => {
                if self.cfg.output.is_none() || self.output_fallback {
                    self.output_check_at = Some(Instant::now() + DEVICE_DEBOUNCE);
                }
            }
            Event::DevicesChanged => {
                self.devices_changed = true;
                self.output_check_at = Some(Instant::now() + DEVICE_DEBOUNCE);
            }
            Event::SessionCreated => self.scan_now = true,
            Event::SessionGone { instance } => {
                if let Some(h) = &mut self.hush {
                    h.forget(&instance);
                }
                self.scan_now = true;
            }
            Event::VolumeChanged { instance, volume, muted } => {
                let Some(h) = &mut self.hush else { return };
                match h.on_foreign_change(&instance, volume, muted) {
                    Change::Volume => {
                        // One line per burst (a slider drag sends dozens of changes).
                        if self.rehush_at.is_none() {
                            log::info!(
                                "{}'s volume was changed to {volume:.3} outside Sidestage; adopting it",
                                self.app_name
                            );
                        }
                        self.shared.gain.close();
                        self.gate = Gate::Closed;
                        let wait = if h.fighting() { DEBOUNCE_FIGHTING } else { DEBOUNCE };
                        self.rehush_at = Some(Instant::now() + wait);
                    }
                    Change::Mute => log::info!("{} was {}", self.app_name, if muted { "muted" } else { "unmuted" }),
                    Change::None => {}
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
        if self.fatal.is_some() {
            self.publish();
            return;
        }
        if self.devices_changed {
            self.devices_changed = false;
            self.refresh_endpoints();
            self.scan_now = true;
        }
        if self.output_check_at.is_some_and(|t| now >= t) {
            self.output_check_at = None;
            self.check_output();
        }

        // App lifetime.
        let scan_due = self.scan_now || now.duration_since(self.last_scan) >= SCAN_EVERY;
        let exited = self.app.as_ref().is_some_and(|a| match &a.exited {
            Some(h) => h.is_signalled(),
            None => scan_due && !process_alive(a.root, a.root_created),
        });
        if exited {
            log::info!("{} exited; waiting for it to come back", self.app_name);
            self.detach();
        }
        if self.app.is_none() && now.duration_since(self.last_resolve) >= SCAN_EVERY && self.retry_due(now) {
            self.last_resolve = now;
            match self.resolve() {
                Ok(Some((att, _))) => {
                    if let Err(e) = self.attach(att) {
                        self.pipeline_failed(format!("{e:#}"));
                    }
                }
                Ok(None) => {}
                Err(e) => log::debug!("looking for {}: {e:#}", self.app_name),
            }
        } else if self.app.is_some()
            && self.sessionless_since.is_some_and(|t| now.duration_since(t) >= SWITCH_AFTER)
            && now.duration_since(self.last_resolve) >= SWITCH_AFTER
        {
            self.last_resolve = now;
            self.switch_instance();
        }

        // Pipeline health.
        if self.pipeline.as_ref().is_some_and(Pipeline::finished) {
            let short = self.pipeline.as_ref().is_some_and(|p| p.started.elapsed() < STABLE_AFTER);
            let err = self.stop_pipeline().unwrap_or_else(|| "the audio stream stopped".to_owned());
            log::warn!("audio pipeline stopped: {err}");
            if short {
                // Dying again right after a (re)start counts toward giving up, so a broken
                // device or processor cannot restart forever.
                self.pipeline_failed(err);
            } else {
                self.restart_pipeline(err);
            }
        } else if self.pipeline.is_none() && self.app.is_some() && self.retry_due(now) {
            self.restart_pipeline(String::new());
        }
        if let Some(p) = &mut self.pipeline {
            if self.failures > 0 && p.started.elapsed() >= STABLE_AFTER {
                self.failures = 0;
            }
            if let Some(e) = p.sidechain_ended() {
                log::warn!("ducking input stopped: {e}");
                self.sidechain_error = Some(e);
            }
        }

        // Sessions.
        if self.scan_now || now.duration_since(self.last_scan) >= SCAN_EVERY {
            self.scan();
        }
        if self.shared.gain.take_fault() {
            log::warn!("{}'s captured level jumped unexpectedly; re-hushing", self.app_name);
            // A new session of the app is hushed right away.
            self.scan();
            // Usually someone moved the app's volume and the audio got here before the
            // notification: re-hush after the same quiet time as a notified change, or this
            // would set the volume back while the user is still dragging the slider.
            if let Some(h) = &mut self.hush {
                h.rehush_all();
            }
            self.shared.gain.close();
            self.gate = Gate::Closed;
            self.rehush_at.get_or_insert(now + DEBOUNCE);
        }
        if self.rehush_at.is_some_and(|t| now >= t) {
            self.rehush_at = None;
            self.rehush_and_settle();
        }
        if self.rehush_at.is_none()
            && let Some(h) = &mut self.hush
            && let Err(e) = h.flush()
        {
            log::warn!("{e:#}");
        }
        self.tick_gate(now);
        self.publish();
    }

    fn retry_due(&self, now: Instant) -> bool {
        self.retry_at.is_none_or(|t| now >= t)
    }

    // ---- app -------------------------------------------------------------------------------

    fn find_app_name(&mut self) -> String {
        let live = sessions::scan(&self.managers);
        let display = live
            .iter()
            .find(|s| super::procs::pid_path(s.pid).is_some_and(|p| match_key(&p) == self.match_key))
            .map(SessionRef::display_name)
            .unwrap_or_default();
        // The running executable's version resource, else the configured path's.
        let path = self.app.as_ref().map_or_else(|| self.cfg.app.0.clone(), |a| a.path.clone());
        self.names.app_name(&display, &path)
    }

    /// Finds the app's root process (the one whose tree plays audio, if several are running)
    /// and how much audio it has: 2 = an active session, 1 = an idle session, 0 = none.
    fn resolve(&mut self) -> anyhow::Result<Option<(Attached, u8)>> {
        let mut procs = ProcessTable::snapshot()?;
        let me = std::process::id();
        let blocked: HashSet<u32> = procs.lineage(me).into_iter().collect();
        let file = exe_file_name(&self.key).to_lowercase();
        let candidates: Vec<u32> = procs.pids().filter(|p| procs.get(*p).is_some_and(|e| e.exe_name == file)).collect();
        let mut roots: Vec<u32> = Vec::new();
        for pid in candidates {
            if blocked.contains(&pid) || procs.path(pid).is_none_or(|p| match_key(&p) != self.match_key) {
                continue;
            }
            let root = procs.root_of(pid);
            if !blocked.contains(&root) && !roots.contains(&root) {
                roots.push(root);
            }
        }
        if roots.is_empty() {
            return Ok(None);
        }
        let live = sessions::scan(&self.managers);
        let mut best: Option<(u32, (u8, std::cmp::Reverse<u64>))> = None;
        for &root in &roots {
            let mut score = 0u8;
            for s in &live {
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
        let Some((root, (score, _))) = best else {
            return Ok(None);
        };
        let tree: HashMap<u32, u64> =
            procs.tree(root).into_iter().filter_map(|p| Some((p, procs.created(p)?))).collect();
        let path = procs.path(root).unwrap_or_else(|| self.key.clone());
        let root_created = procs.created(root).unwrap_or(0);
        // Only the process the snapshot saw: its PID may have been reused since.
        let exited = open_process(root, PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION)
            .filter(|h| root_created != 0 && created_of(h) == Some(root_created));
        Ok(Some((Attached { root, root_created, path, exited, tree, foreign: HashMap::new() }, score)))
    }

    /// The attached instance has had no audio session for a while: if another instance of
    /// the app has one, move to that instance.
    fn switch_instance(&mut self) {
        let current = self.app.as_ref().map(|a| a.root);
        match self.resolve() {
            Ok(Some((att, score))) if score > 0 && Some(att.root) != current => {
                log::info!(
                    "{} (pid {}) has no audio; switching to the instance that has (pid {})",
                    self.app_name,
                    current.unwrap_or(0),
                    att.root
                );
                self.detach();
                if let Err(e) = self.attach(att) {
                    self.pipeline_failed(format!("{e:#}"));
                }
            }
            Ok(_) => {}
            Err(e) => log::debug!("looking for {}: {e:#}", self.app_name),
        }
    }

    fn attach(&mut self, att: Attached) -> anyhow::Result<()> {
        log::info!("attaching to {} (pid {})", self.app_name, att.root);
        let root = att.root;
        self.app = Some(att);
        self.app_name = self.find_app_name();
        if let Err(e) = self.start_pipeline(root) {
            self.app = None;
            return Err(e);
        }
        // `failures` is reset once the pipeline has run for a while (see `tick`).
        self.retry_at = None;
        self.last_error = None;
        self.scan();
        self.begin_settle();
        Ok(())
    }

    fn detach(&mut self) {
        self.shared.gain.close();
        self.gate = Gate::Closed;
        self.stop_pipeline();
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

    // ---- pipeline --------------------------------------------------------------------------

    /// The output to open: the chosen one if it is there, else the default.
    fn wanted_output(&mut self) -> Option<String> {
        match &self.cfg.output {
            Some(id) if output_active(&self.en, id) => {
                self.output_fallback = false;
                Some(id.clone())
            }
            Some(_) => {
                self.output_fallback = true;
                None
            }
            None => None,
        }
    }

    fn start_pipeline(&mut self, root: u32) -> anyhow::Result<()> {
        let stop = Arc::new(Handle::event(true)?);
        let device = self.wanted_output();
        let sc = self.cfg.sidechain.then(|| self.shared.sidechain.clone());
        let render::Render { info, start: start_tx, thread: render_thread } =
            render::open(device, stop.clone(), sc.clone())?;
        // One 10 ms capture packet plus one output period plus scheduling margin.
        let target_ms = (20.0 + info.period_ms).max(30.0);
        let (producer, consumer, stats) = bridge(BridgeConfig::new(info.rate, target_ms));
        let processor = (self.factory)(info.rate);
        let mut pipeline = Pipeline {
            stop: stop.clone(),
            render: Some(render_thread),
            capture: None,
            sidechain: None,
            stats,
            info: info.clone(),
            started: Instant::now(),
        };
        let sink = Sink::Music { producer, gain: self.shared.gain.clone() };
        match capture::spawn(Target::Include(root), info.rate, sink, stop.clone()) {
            Ok(t) => pipeline.capture = Some(t),
            Err(e) => {
                // The output thread waits for its start message; hang up first so it exits.
                drop(start_tx);
                pipeline.shutdown();
                return Err(e);
            }
        }
        if start_tx.send(RenderStart { consumer, processor }).is_err() {
            let why = pipeline.shutdown().unwrap_or_default();
            return Err(anyhow!("the output stopped before it started: {why}"));
        }
        self.sidechain_error = None;
        if let Some(levels) = sc {
            match capture::spawn(Target::Exclude(std::process::id()), info.rate, Sink::Sidechain(levels), stop) {
                Ok(t) => pipeline.sidechain = Some(t),
                Err(e) => {
                    log::warn!("ducking sidechain unavailable: {e:#}");
                    self.sidechain_error = Some(format!("{e:#}"));
                }
            }
        }
        log::info!(
            "playing {} on {} ({} Hz, {} ch, {:.0} ms buffer)",
            self.app_name,
            info.device_name,
            info.rate,
            info.channels,
            info.buffer_ms
        );
        {
            let mut st = self.shared.status();
            st.output_name = info.device_name.clone();
            st.sample_rate = info.rate;
        }
        self.pipeline = Some(pipeline);
        Ok(())
    }

    /// Stops the pipeline (if any); returns the first thread error.
    fn stop_pipeline(&mut self) -> Option<String> {
        let mut p = self.pipeline.take()?;
        self.dropouts_base += p.stats.dropouts();
        p.shutdown()
    }

    /// Restarts after a failure or device change; on failure un-hushes and retries later, so the
    /// user is never left without the music.
    fn restart_pipeline(&mut self, why: String) {
        self.shared.gain.close();
        self.gate = Gate::Closed;
        self.stop_pipeline();
        let Some(root) = self.app.as_ref().map(|a| a.root) else {
            return;
        };
        match self.start_pipeline(root) {
            Ok(()) => {
                self.retry_at = None;
                self.last_error = None;
                if let Some(h) = &mut self.hush
                    && h.tracked_count() > 0
                    && let Err(e) = h.apply()
                {
                    log::warn!("re-hushing {}: {e:#}", self.app_name);
                }
                self.begin_settle();
            }
            Err(e) => {
                let msg = if why.is_empty() { format!("{e:#}") } else { format!("{why}; restarting failed: {e:#}") };
                self.pipeline_failed(msg);
            }
        }
    }

    fn pipeline_failed(&mut self, msg: String) {
        log::warn!("audio pipeline failed: {msg}");
        if let Some(h) = &mut self.hush {
            h.unhush_all();
        }
        self.failures += 1;
        self.retry_at = Some(Instant::now() + RETRY_EVERY);
        if self.failures >= MAX_FAILURES {
            self.fatal = Some(format!("Sidestage could not play {}: {msg}", self.app_name));
            if let Some(h) = &mut self.hush
                && let Err(e) = h.finish(sessions::scan(&self.managers))
            {
                log::error!("restoring {}: {e:#}", self.app_name);
            }
        }
        self.last_error = Some(msg);
    }

    /// Reopens the output when the default device changed (or a chosen device came back).
    fn check_output(&mut self) {
        let Some(p) = &self.pipeline else { return };
        let want = match &self.cfg.output {
            Some(id) if output_active(&self.en, id) => id.clone(),
            _ => match sessions::default_render(&self.en).and_then(|d| sessions::device_id(&d).ok()) {
                Some(id) => id,
                None => return,
            },
        };
        if p.info.device_id != want {
            log::info!("output changed; reopening");
            self.restart_pipeline(String::new());
        }
    }

    // ---- sessions --------------------------------------------------------------------------

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

    /// Finds the app's sessions, hushes new ones and restores leftovers of other apps.
    fn scan(&mut self) {
        self.last_scan = Instant::now();
        self.scan_now = false;
        let Some(att) = &mut self.app else { return };
        let live = sessions::scan(&self.managers);
        let peaks = sessions::trusted_peaks(&live);
        let mut table: Option<ProcessTable> = None;
        let (mut mine, mut others) = (Vec::new(), Vec::new());
        // Indices into `others` of sessions that belong to another instance of the app.
        let mut instances = Vec::new();
        let mut app_peak = 0.0f32;
        for (s, peak) in live.into_iter().zip(peaks) {
            let created = process_created(s.pid).unwrap_or(0);
            if created != 0 && att.tree.get(&s.pid) == Some(&created) {
                app_peak = app_peak.max(peak.unwrap_or(0.0));
                mine.push(s);
                continue;
            }
            if let Some(&instance) = att.foreign.get(&(s.pid, created)) {
                if instance {
                    instances.push(others.len());
                }
                others.push(s);
                continue;
            }
            if table.is_none() {
                table = ProcessTable::snapshot().ok();
            }
            let Some(t) = table.as_mut() else {
                others.push(s);
                continue;
            };
            if created != 0 && t.is_in_tree(s.pid, att.root) {
                att.tree.insert(s.pid, created);
                app_peak = app_peak.max(peak.unwrap_or(0.0));
                mine.push(s);
            } else {
                // Only a process known to be outside the captured tree counts as another
                // instance: restoring a captured one would be amplified by the makeup gain.
                let instance = created != 0 && t.path(s.pid).is_some_and(|p| match_key(&p) == self.match_key);
                att.foreign.insert((s.pid, created), instance);
                if instance {
                    instances.push(others.len());
                }
                others.push(s);
            }
        }
        self.app_peak = app_peak;
        self.app_sessions = mine.len();
        // Another instance with audio sessions plays uncaptured (also one started later).
        self.other_instances = !instances.is_empty();
        if mine.is_empty() {
            self.sessionless_since.get_or_insert(self.last_scan);
        } else {
            self.sessionless_since = None;
        }

        let Some(h) = &mut self.hush else { return };
        h.restore_others(&others, &instances);
        let new: Vec<SessionRef> = mine.into_iter().filter(|s| !h.is_tracked(&s.instance_id)).collect();
        if new.is_empty() || self.adopt_after.is_some_and(|t| Instant::now() < t) {
            return;
        }
        // Silence the output before the capture level changes.
        self.shared.gain.close();
        match h.adopt(new) {
            Ok(true) => {
                self.adopt_after = None;
                self.begin_settle();
            }
            Ok(false) => {
                if self.gate == Gate::Open {
                    self.shared.gain.open();
                }
            }
            Err(e) => {
                // The journal could not be written, so nothing was changed: keep playing
                // unhidden and try again later.
                log::error!("could not hide {}: {e:#}", self.app_name);
                self.last_error = Some(format!("{e:#}"));
                self.adopt_after = Some(Instant::now() + RETRY_EVERY * 3);
                if self.gate == Gate::Open {
                    self.shared.gain.open();
                }
            }
        }
    }

    // ---- gate ------------------------------------------------------------------------------

    fn begin_settle(&mut self) {
        self.shared.gain.close();
        self.gate = Gate::Settling(Instant::now() + SETTLE);
        self.settle_tries = 0;
    }

    fn rehush_and_settle(&mut self) {
        if let Some(h) = &mut self.hush
            && let Err(e) = h.apply()
        {
            log::warn!("re-hushing {}: {e:#}", self.app_name);
        }
        self.begin_settle();
    }

    fn tick_gate(&mut self, now: Instant) {
        let Gate::Settling(t) = self.gate else { return };
        if now < t || self.pipeline.is_none() {
            return;
        }
        let makeup = match &mut self.hush {
            Some(h) if h.hidden() => {
                if !h.settled() {
                    self.settle_tries += 1;
                    if self.settle_tries.is_multiple_of(5) {
                        log::warn!("{}'s volume does not stay where Sidestage set it", self.app_name);
                    }
                    h.rehush_all();
                    if let Err(e) = h.apply() {
                        log::warn!("re-hushing {}: {e:#}", self.app_name);
                    }
                    self.gate = Gate::Settling(now + SETTLE);
                    return;
                }
                h.makeup()
            }
            _ => 1.0,
        };
        self.shared.gain.set_makeup(makeup);
        self.shared.gain.open();
        self.gate = Gate::Open;
        log::debug!("{} audible through Sidestage (makeup {makeup:.3e})", self.app_name);
    }

    // ---- status ----------------------------------------------------------------------------

    fn publish(&mut self) {
        let now = Instant::now();
        let gain = &self.shared.gain;
        // Capture silent while the app's meter shows sound: exclusive mode or protected content.
        let heard = gain.watch.take() > 0.0;
        let muted = self.hush.as_ref().is_some_and(Hush::any_muted);
        let turned_down = self.hush.as_ref().is_some_and(|h| h.desired() <= 0.0);
        if self.gate == Gate::Open && self.app_peak > 0.01 && !heard && !muted && !turned_down {
            self.silent_since.get_or_insert(now);
        } else {
            self.silent_since = None;
        }

        let mut warnings = Vec::new();
        let app = &self.app_name;
        if (MIN_BUILD..SUPPORTED_BUILD).contains(&self.build) {
            warnings.push(format!(
                "This Windows version (build {}) is older than the first one that officially supports capturing a single app; it may not work.",
                self.build
            ));
        }
        if let Some(h) = &self.hush {
            if muted {
                warnings.push(format!("{app} is muted in the Volume mixer."));
            } else if h.hidden() && h.desired() <= 0.0 {
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
        if self.ducked && !self.duck_does_nothing {
            warnings.push(
                "Windows communications ducking is lowering other sounds (Sound settings > More sound settings > Communications)."
                    .to_owned(),
            );
        }
        if self.output_fallback {
            warnings.push("The chosen output is not available; playing on the default output.".to_owned());
        }
        if let Some(p) = &self.pipeline {
            if p.info.spatial {
                warnings.push(format!(
                    "Spatial sound (Windows Sonic, Dolby Atmos or DTS) is on for {}; turn it off for the best 3D effect.",
                    p.info.device_name
                ));
            }
            if p.info.channels == 1 || p.info.rate <= 16_000 {
                warnings
                    .push(format!("{} is in headset (hands-free) mode with low sound quality.", p.info.device_name));
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
        if self.cfg.sidechain {
            if let Some(e) = &self.sidechain_error {
                warnings.push(format!("Ducking is unavailable: {e}"));
            } else if self.cfg.hide == HideMethod::KeepOriginal {
                warnings.push("While the original stays audible, ducking also reacts to the music itself.".to_owned());
            }
        }
        if self.fatal.is_none()
            && let Some(e) = &self.last_error
        {
            warnings.push(format!("Problem playing {app}: {e}"));
        }

        let state = if let Some(f) = &self.fatal {
            SessionState::Error(f.clone())
        } else if self.stopping {
            SessionState::Stopped
        } else if self.app.is_none() && self.failures > 0 {
            SessionState::Starting
        } else if self.app.is_none() || self.app_sessions == 0 {
            // Not running, or running without an audio stream yet (it gets one on first play).
            SessionState::WaitingForApp
        } else if self.pipeline.is_some()
            && (self.gate == Gate::Open
                // Briefly re-settling after a change: keep the UI steady.
                || matches!(self.shared.status().state, SessionState::Running))
        {
            SessionState::Running
        } else {
            SessionState::Starting
        };
        let (latency, drift, dropouts) = match &self.pipeline {
            Some(p) => (
                p.stats.fill_ms() + 10.0 + p.info.buffer_ms,
                p.stats.correction_ppm(),
                self.dropouts_base + p.stats.dropouts(),
            ),
            None => (0.0, 0.0, self.dropouts_base),
        };
        let hidden = self.pipeline.is_some() && self.hush.as_ref().is_some_and(Hush::hidden);

        let mut st = self.shared.status();
        st.state = state;
        st.app_name = self.app_name.clone();
        st.original_hidden = hidden;
        st.latency_ms = latency;
        st.drift_ppm = drift;
        st.dropouts = dropouts + gain.gaps.load(std::sync::atomic::Ordering::Relaxed);
        st.warnings = warnings;
        st.sidechain_active =
            self.sidechain_error.is_none() && self.pipeline.as_ref().is_some_and(|p| p.sidechain.is_some());
        if self.pipeline.is_none() {
            st.output_name.clear();
        }
    }

    /// Stops everything and restores the app's volume.
    fn finish(&mut self) -> anyhow::Result<()> {
        self.stopping = true;
        self.shared.gain.close();
        if self.pipeline.is_some() {
            // Let the closed gate fade out and the bridge drain before the output stops.
            thread::sleep(Duration::from_millis(60));
        }
        self.stop_pipeline();
        let result = match self.hush.take() {
            Some(mut h) => h.finish(sessions::scan(&self.managers)),
            None => Ok(()),
        };
        self.watches.clear();
        self.publish();
        result
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
