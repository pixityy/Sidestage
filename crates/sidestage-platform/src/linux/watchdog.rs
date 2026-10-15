//! The watchdog: a copy of Sidestage that restores the user's audio if Sidestage dies.
//!
//! The capture sink belongs to the sound server, not to our connection, so it survives a crash
//! and the music app would stay silent. The watchdog holds the read end of a pipe whose only
//! write end is in the Sidestage process; when that process exits for any reason (even
//! SIGKILL) the kernel closes it, the watchdog reads EOF and undoes what the journal records.
//! On a clean stop the session restores and clears the journal first, so the watchdog finds
//! nothing to do.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

use anyhow::Context;

use super::conn::Conn;
use super::control;
use super::routing::RoutingJournal;
use crate::journal::Journal;

/// `sidestage --sidestage-helper linux-watchdog <journal path> <session id>`.
const KIND: &str = "linux-watchdog";

pub struct Watchdog {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl Watchdog {
    pub fn spawn(journal: &Path, session: &str) -> anyhow::Result<Self> {
        // /proc/self/exe still works when the binary was replaced by an update meanwhile.
        let mut child = Command::new("/proc/self/exe")
            .arg0("sidestage")
            .arg(crate::HELPER_ARG)
            .arg(KIND)
            .arg(journal)
            .arg(session)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Own process group: Ctrl+C in the terminal that started Sidestage must not kill
            // the watchdog along with it.
            .process_group(0)
            .spawn()
            .context("starting the crash-recovery helper")?;
        let stdin = child.stdin.take();
        log::info!("pulse: watchdog running as pid {}", child.id());
        Ok(Self { child: Some(child), stdin })
    }
}

impl Drop for Watchdog {
    /// Closes the pipe (the helper then checks the journal and exits) and reaps the helper in
    /// the background so it does not linger as a zombie.
    fn drop(&mut self) {
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let reaper = std::thread::Builder::new().name("sidestage-reaper".into()).spawn(move || {
                let _ = child.wait();
            });
            if let Err(e) = reaper {
                log::debug!("pulse: could not start the watchdog reaper: {e}");
            }
        }
    }
}

/// Entry point of the helper process (`linux::run_helper`).
pub fn run_helper(args: &[String]) -> i32 {
    let [kind, journal, session] = args else {
        log::error!("linux helper: unexpected arguments {args:?}");
        return 2;
    };
    if kind != KIND {
        log::error!("linux helper: unknown helper {kind:?}");
        return 2;
    }
    // SAFETY: setting SIG_IGN for these signals is async-signal-safe and has no preconditions.
    // A closing terminal or Ctrl+C must not stop the watchdog before it has done its job, and
    // neither must SIGTERM: `pkill sidestage` or a logout signals both processes at once, and
    // the watchdog is what gives the music app its sound back when Sidestage dies that way. It
    // still exits on its own as soon as Sidestage is gone and the restore is done.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }

    // Block until Sidestage exits (or closes the pipe on a clean stop).
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 64];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }

    let journal: Journal<RoutingJournal> = Journal::at(journal);
    let record = match journal.load() {
        Ok(Some(record)) if record.session == *session => record,
        Ok(_) => return 0, // clean stop, or a newer session owns the journal
        Err(e) => {
            log::error!("linux helper: {e:#}");
            return 1;
        }
    };
    // The sound server may be restarting at the same moment (e.g. at logout); retry briefly.
    for attempt in 1..=10 {
        let result = Conn::connect("recovery")
            .and_then(|mut c| control::restore_sink(&mut c, &record.sink_name, &record.streams));
        match result {
            Ok(lines) => {
                for line in lines {
                    log::info!("linux helper: {line}");
                }
                if journal.load().ok().flatten().is_some_and(|r| r.session == *session)
                    && let Err(e) = journal.clear()
                {
                    log::error!("linux helper: {e:#}");
                }
                return 0;
            }
            Err(e) => {
                log::warn!("linux helper: restore attempt {attempt} failed: {e:#}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    1
}
