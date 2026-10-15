//! The guardian: a detached copy of this executable
//! (`sidestage --sidestage-helper windows-guardian <pid> <creation time> <journal path>`) that
//! waits for Sidestage to exit, however it exits, and restores the volumes left in the journal.
//!
//! It is started through a launcher that exits at once, so it is not in Sidestage's process
//! tree: Task Manager's "End task" on Sidestage (and "End process tree") ends the whole tree,
//! which would take the guardian down exactly when it is needed.
//!
//! Windows persists per-app session volume, so an app Sidestage hushed would stay silent after
//! a crash. If the app is not running when Sidestage dies, the guardian keeps waiting for it
//! (woken when any app opens audio, polling as a fallback) until the volume is restored, another
//! Sidestage takes the journal over, or a week has passed; the next start's `recover()` covers
//! anything left after that.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use windows::Win32::Media::Audio::IMMDeviceEnumerator;
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

use super::com::{Mta, created_of, open_process, process_alive};
use super::hush::{self, Record, drop_stale, restore};
use super::notify::{DeviceWatch, Event, SessionWatch};
use super::sessions::{self, Managers};
use crate::journal::Journal;

pub const GUARDIAN_ARG: &str = "windows-guardian";
/// Starts the guardian with the same arguments, then exits.
pub const LAUNCH_ARG: &str = "windows-guardian-launch";

/// How long the guardian keeps waiting for an app that is not running.
const GIVE_UP_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);
/// A failed start is retried at the next journal write, but not more often than this.
const RETRY_START: Duration = Duration::from_secs(30);

static STARTED: AtomicBool = AtomicBool::new(false);
static LAST_TRY: Mutex<Option<Instant>> = Mutex::new(None);

/// Starts this process's guardian (once per process; retried later if it could not start).
pub fn ensure(journal: &Path) {
    if STARTED.load(Ordering::Acquire) {
        return;
    }
    let mut last = LAST_TRY.lock().unwrap_or_else(PoisonError::into_inner);
    if STARTED.load(Ordering::Acquire) || last.is_some_and(|t| t.elapsed() < RETRY_START) {
        return;
    }
    *last = Some(Instant::now());
    match spawn(journal) {
        Ok(()) => {
            STARTED.store(true, Ordering::Release);
            log::debug!("started the Windows guardian process");
        }
        Err(e) => log::warn!(
            "could not start the guardian process; if Sidestage crashes, volumes are restored at its next start: {e:#}"
        ),
    }
}

fn spawn(journal: &Path) -> anyhow::Result<()> {
    let (pid, created) = hush::me();
    let args = [pid.to_string(), created.to_string(), journal.to_string_lossy().into_owned()];
    let mut launcher = start_detached(LAUNCH_ARG, &args, journal.parent())?;
    // The launcher only starts the guardian; its exit code says whether that worked.
    let t = Instant::now();
    loop {
        if let Some(status) = launcher.try_wait().context("waiting for the guardian launcher")? {
            return if status.success() { Ok(()) } else { Err(anyhow!("the guardian launcher failed ({status})")) };
        }
        if t.elapsed() > Duration::from_secs(2) {
            // A very busy machine; it will most likely get there.
            log::debug!("the guardian launcher is slow to finish");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Starts `<this exe> --sidestage-helper <mode> <args>` with no console, outside our job object
/// when allowed (so it survives a launcher that kills the whole job).
fn start_detached(mode: &str, args: &[String], dir: Option<&Path>) -> anyhow::Result<Child> {
    let exe = std::env::current_exe().context("locating Sidestage's executable")?;
    let mut cmd = Command::new(exe);
    cmd.arg(crate::HELPER_ARG).arg(mode).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(dir) = dir {
        // Do not keep the caller's working directory busy.
        cmd.current_dir(dir);
    }
    // DETACHED_PROCESS already means "no console"; CREATE_NO_WINDOW would be ignored with it.
    let base = DETACHED_PROCESS.0 | CREATE_NEW_PROCESS_GROUP.0;
    // Jobs that forbid breakaway make CreateProcess fail; then start inside the job.
    match cmd.creation_flags(base | CREATE_BREAKAWAY_FROM_JOB.0).spawn() {
        Ok(child) => Ok(child),
        Err(_) => cmd.creation_flags(base).spawn().context("starting the guardian process"),
    }
}

/// Entry point of `--sidestage-helper windows-guardian-launch <pid> <created> <journal path>`.
pub fn launch(args: &[String]) -> i32 {
    let [_, _, path] = args else {
        return 2;
    };
    match start_detached(GUARDIAN_ARG, args, Path::new(path).parent()) {
        // Exiting right away leaves the guardian without a living parent.
        Ok(_) => 0,
        Err(e) => {
            note(&format!("could not start the guardian: {e:#}"));
            1
        }
    }
}

/// Entry point of `--sidestage-helper windows-guardian <pid> <created> <journal path>`.
pub fn run(args: &[String]) -> i32 {
    match guard(args) {
        Ok(()) => 0,
        Err(e) => {
            note(&format!("guardian failed: {e:#}"));
            1
        }
    }
}

fn guard(args: &[String]) -> anyhow::Result<()> {
    let [parent, created, path] = args else {
        bail!("usage: {GUARDIAN_ARG} <parent pid> <parent creation time> <journal path>")
    };
    let parent: u32 = parent.parse().context("parent pid")?;
    let created: u64 = created.parse().context("parent creation time")?;
    let journal = Journal::<Record>::at(path);
    let lock = hush::journal_mutex(Path::new(path));

    wait_for_exit(parent, created);

    let _mta = Mta::enter()?;
    let (tx, rx) = mpsc::channel::<Event>();
    let mut audio = Audio { tx, en: None, _devices: None, managers: Managers::default(), watches: Vec::new() };
    let mut state = State::default();
    let started = Instant::now();
    while started.elapsed() < GIVE_UP_AFTER {
        let step = {
            let _locked = hush::lock_journal(&lock);
            pass(&journal, parent, created, &mut audio, &mut state)
        };
        match step {
            Ok(Next::Exit) => return Ok(()),
            Ok(Next::Wait) => {}
            Err(e) => return Err(e),
        }
        let pause =
            if started.elapsed() < Duration::from_secs(120) { Duration::from_secs(1) } else { Duration::from_secs(10) };
        wait(&rx, pause);
    }
    note("giving up waiting; the next Sidestage start will restore the rest");
    Ok(())
}

/// Blocks until the Sidestage process `pid`, created at `created`, has exited.
fn wait_for_exit(pid: u32, created: u64) {
    match open_process(pid, PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION) {
        Some(h) => {
            // Another creation time: Sidestage died before we got here and its PID was reused.
            if created == 0 || created_of(&h).is_none_or(|c| c == created) {
                unsafe { WaitForSingleObject(h.raw(), INFINITE) };
            }
        }
        // No SYNCHRONIZE access (not expected for our own user's process): poll instead.
        None => {
            while process_alive(pid, created) {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

#[derive(Default)]
struct State {
    announced: bool,
    audio_error: bool,
    io_error: bool,
}

enum Next {
    Exit,
    Wait,
}

/// The audio system as the guardian sees it: session managers of every output, with
/// notifications that wake it when an app opens audio or an output appears.
struct Audio {
    tx: Sender<Event>,
    en: Option<IMMDeviceEnumerator>,
    _devices: Option<DeviceWatch>,
    managers: Managers,
    watches: Vec<SessionWatch>,
}

impl Audio {
    fn refresh(&mut self) -> anyhow::Result<()> {
        let en = match &self.en {
            Some(en) => en.clone(),
            None => {
                let en = sessions::enumerator()?;
                self._devices = DeviceWatch::register(&en, self.tx.clone()).ok();
                self.en = Some(en.clone());
                en
            }
        };
        let (gone, added) = self.managers.refresh(&en)?;
        self.watches.retain(|w| !gone.iter().any(|m| m.endpoint_id == w.endpoint_id));
        for i in added {
            let tx = self.tx.clone();
            if let Ok(w) = SessionWatch::register(&self.managers.list[i], move || {
                let _ = tx.send(Event::SessionCreated);
            }) {
                self.watches.push(w);
            }
        }
        Ok(())
    }
}

/// One look at the journal: restore what can be restored now. Runs under the journal lock.
fn pass(
    journal: &Journal<Record>,
    parent: u32,
    created: u64,
    audio: &mut Audio,
    st: &mut State,
) -> anyhow::Result<Next> {
    let ours =
        |r: &Record| r.owner_pid == parent && (created == 0 || r.owner_created == 0 || r.owner_created == created);
    let rec = match journal.load() {
        Ok(Some(rec)) => rec,
        Ok(None) => return Ok(Next::Exit),
        // Left for the next Sidestage start, which moves a corrupt journal aside.
        Err(e) if e.chain().any(|c| c.is::<serde_json::Error>()) => return Err(e),
        Err(e) => {
            if !std::mem::replace(&mut st.io_error, true) {
                note(&format!("cannot read the journal yet: {e:#}"));
            }
            return Ok(Next::Wait);
        }
    };
    if !ours(&rec) {
        if st.announced {
            note("a newer Sidestage took the journal over");
        }
        return Ok(Next::Exit);
    }
    if rec.entries.is_empty() {
        journal.clear()?;
        return Ok(Next::Exit);
    }
    // Never undo the hiding of a Sidestage that is still running.
    if process_alive(rec.owner_pid, rec.owner_created) {
        return Ok(Next::Wait);
    }
    if !st.announced {
        note(&format!("Sidestage (pid {parent}) exited with {} hushed session(s) left; restoring", rec.entries.len()));
        st.announced = true;
    }
    // The audio service may be restarting (or not up yet after a logon): keep trying.
    if let Err(e) = audio.refresh() {
        if !std::mem::replace(&mut st.audio_error, true) {
            note(&format!("the audio system is not available yet: {e:#}"));
        }
        return Ok(Next::Wait);
    }
    let (entries, dropped) = drop_stale(rec.entries.clone());
    for e in &dropped {
        note(&format!("dropping {} on {}: no longer installed or too old", e.app_name, e.endpoint_name));
    }
    let live = sessions::scan(&audio.managers);
    let (restored, remaining) = restore(entries, &live);
    for e in &restored {
        note(&format!("restored {} to volume {:.3}", e.app_name, e.volume));
    }
    // Only if the lock could not be taken could a new Sidestage have claimed it meanwhile.
    if !journal.load().ok().flatten().is_some_and(|r| ours(&r)) {
        note("a newer Sidestage took the journal over");
        return Ok(Next::Exit);
    }
    if remaining.is_empty() {
        note("all volumes restored");
        journal.clear()?;
        return Ok(Next::Exit);
    }
    if (!restored.is_empty() || !dropped.is_empty())
        && let Err(e) = journal.save(&Record { entries: remaining, ..rec })
    {
        // Restored entries stay listed; restoring them again later is harmless.
        note(&format!("updating the journal: {e:#}"));
    }
    Ok(Next::Wait)
}

/// Sleeps up to `pause`, or until an app opens audio or an output appears.
fn wait(rx: &Receiver<Event>, pause: Duration) {
    match rx.recv_timeout(pause) {
        Ok(_) => {
            // An app opens several streams at once: let the burst arrive, then drain it.
            std::thread::sleep(Duration::from_millis(20));
            while rx.try_recv().is_ok() {}
        }
        Err(RecvTimeoutError::Timeout) => {}
        Err(RecvTimeoutError::Disconnected) => std::thread::sleep(pause),
    }
}

/// Appends a line to the guardian's log next to the journal (the helper has no logger).
fn note(msg: &str) {
    let Ok(dir) = crate::journal::state_dir() else {
        return;
    };
    let path = dir.join("windows-guardian.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 256 * 1024) {
        let _ = std::fs::remove_file(&path);
    }
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{secs}] pid {}: {msg}", std::process::id());
    }
}
