//! Linux backend over the PulseAudio native protocol, which both real PulseAudio and PipeWire's
//! pipewire-pulse speak (pure Rust via the `pulseaudio` crate; no libpulse at build or run time).
//!
//! * **Capture + hide**: a `module-null-sink` with a random per-session name; the music app's
//!   sink-inputs are moved onto it, so the user no longer hears them, and its `.monitor` is
//!   recorded (`no_move`, 10 ms fragments, 48 kHz float stereo).
//! * **Output**: a playback stream to the chosen (or default) sink, fed from the drift bridge
//!   by the [`Processor`](crate::Processor).
//! * **Sidechain**: per-stream peak meters on everything else playing on that sink.
//! * **Undo**: moves persist (stream-restore / WirePlumber); moving a stream back to the
//!   *current* default clears the saved route, other streams go back to their original sink.
//!   A journal, a watchdog process and the startup sweep cover crashes.
//!
//! The protocol-independent parts (`env`, `modargs`, `pcm`, `routing`) are plain Rust with
//! unit tests that run on every OS.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod env;
mod modargs;
mod pcm;
mod routing;

#[cfg(target_os = "linux")]
mod audio;
#[cfg(target_os = "linux")]
mod conn;
#[cfg(target_os = "linux")]
mod control;
#[cfg(target_os = "linux")]
mod events;
#[cfg(target_os = "linux")]
mod meters;
#[cfg(target_os = "linux")]
mod session;
#[cfg(target_os = "linux")]
mod watchdog;

#[cfg(target_os = "linux")]
pub use platform::create;
#[cfg(target_os = "linux")]
pub use watchdog::run_helper;

/// Journal file stem (`linux-routing.json` in the state directory).
#[cfg(target_os = "linux")]
const JOURNAL: &str = "linux-routing";

/// True if the journal owner `pid` is a running Sidestage with a live session: this process
/// while it has a session, or another process running Sidestage (executables compared, falling
/// back to the process name when `/proc/<pid>/exe` is not readable).
#[cfg(target_os = "linux")]
fn owner_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return session::is_active();
    }
    // An exited but unreaped Sidestage still has its name; it holds nothing any more.
    if std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| env::exited_from_stat(&s)) {
        return false;
    }
    let file_name =
        |p: &std::path::Path| p.file_name().map(|n| n.to_string_lossy().trim_end_matches(" (deleted)").to_owned());
    match std::fs::read_link(format!("/proc/{pid}/exe")) {
        Ok(exe) => {
            let ours = std::env::current_exe().ok();
            file_name(&exe).is_some() && file_name(&exe) == ours.as_deref().and_then(file_name)
        }
        Err(_) => std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|comm| comm.trim().to_ascii_lowercase().starts_with("sidestage")),
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use anyhow::Context;

    use crate::journal::Journal;
    use crate::{
        AppKey, AudioApp, HideMethod, OutputDevice, Platform, ProcessorFactory, Session, SessionConfig, Support,
    };

    use super::conn::{Conn, is_dead_connection};
    use super::modargs::ServerKind;
    use super::routing::{self, RoutingJournal};
    use super::{JOURNAL, control, env, owner_alive, session};

    /// Reply timeout of the picker connection. Its calls run on the UI thread, which the
    /// `Platform` contract lets a backend block for only ~200 ms; a server that does not answer
    /// within this is hung, and the UI should get an error rather than freeze.
    const PICKER_TIMEOUT: Duration = Duration::from_millis(750);

    pub fn create() -> anyhow::Result<Box<dyn Platform>> {
        Ok(Box::new(LinuxPlatform { ctl: None, kind: None, exclude: ancestry() }))
    }

    struct LinuxPlatform {
        /// Control connection for the picker, opened on first use and reopened after errors.
        ctl: Option<Conn>,
        kind: Option<ServerKind>,
        /// Sidestage and its parent processes: never offered as the music app.
        exclude: BTreeSet<u32>,
    }

    impl LinuxPlatform {
        fn connect(&mut self) -> anyhow::Result<Conn> {
            let mut conn = Conn::connect_with("control", PICKER_TIMEOUT)?;
            if self.kind.is_none() {
                self.kind = control::server(&mut conn).ok().map(|s| s.kind);
            }
            Ok(conn)
        }

        /// Runs `f` on the control connection. A connection that turns out dead (the server
        /// restarted since the last call) is replaced and `f` retried once; after any other
        /// error the connection is dropped and the error returned, so a hung server costs one
        /// timeout, not two.
        fn query<T>(&mut self, f: impl Fn(&mut Conn) -> anyhow::Result<T>) -> anyhow::Result<T> {
            if let Some(conn) = self.ctl.as_mut() {
                match f(conn) {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        self.ctl = None;
                        if !is_dead_connection(&e) {
                            return Err(e);
                        }
                        log::debug!("pulse: retrying on a new connection after: {e:#}");
                    }
                }
            }
            let conn = self.connect()?;
            let result = f(self.ctl.insert(conn));
            if result.is_err() {
                self.ctl = None;
            }
            result
        }
    }

    impl Platform for LinuxPlatform {
        fn name(&self) -> &'static str {
            match &self.kind {
                Some(kind) if kind.is_pipewire() => "Linux · PipeWire (pipewire-pulse) null sink",
                Some(_) => "Linux · PulseAudio null sink",
                None => "Linux · PulseAudio/PipeWire null sink",
            }
        }

        fn support(&self) -> Support {
            match Conn::connect_with("probe", Duration::from_millis(500)) {
                Ok(_) => Support::Supported,
                Err(e) => {
                    Support::Unsupported(format!("Sidestage needs PulseAudio or PipeWire with pipewire-pulse: {e:#}"))
                }
            }
        }

        fn list_apps(&mut self) -> anyhow::Result<Vec<AudioApp>> {
            let streams = self.query(control::streams)?;
            Ok(routing::group_apps(&streams, &self.exclude)
                .into_iter()
                .map(|g| AudioApp {
                    key: AppKey(g.key),
                    name: g.name,
                    detail: g.detail,
                    pids: g.pids,
                    playing: g.playing,
                    peak: None,
                })
                .collect())
        }

        fn list_outputs(&mut self) -> anyhow::Result<Vec<OutputDevice>> {
            let (sinks, default) = self.query(|c| Ok((control::sinks(c)?, control::server(c)?.default_sink)))?;
            Ok(sinks
                .into_iter()
                .filter(|s| !s.is_capture())
                .map(|s| {
                    let is_default = default.as_deref() == Some(s.name.as_str());
                    let name = s.label().to_owned();
                    OutputDevice { id: s.name, name, is_default }
                })
                .collect())
        }

        fn hide_methods(&mut self) -> Vec<(HideMethod, String)> {
            // Keeping the original audible would need a second, per-stream capture path; the
            // A/B comparison is not worth that complexity here. Parking is Windows-only.
            vec![(HideMethod::Auto, "Move the app to Sidestage's hidden capture output".to_owned())]
        }

        fn start(&mut self, config: SessionConfig, processor: ProcessorFactory) -> anyhow::Result<Box<dyn Session>> {
            // The session opens its own connections; drop ours so a dead one is not reused.
            self.ctl = None;
            session::start(config, processor)
        }

        fn recover(&mut self) -> anyhow::Result<Vec<String>> {
            let journal: Journal<RoutingJournal> = Journal::open(JOURNAL)?;
            let record = journal.load().unwrap_or_else(|e| {
                log::warn!("pulse: ignoring an unreadable journal: {e:#}");
                None
            });
            let live = record.as_ref().filter(|r| owner_alive(r.pid));
            // A real round trip, so a stale connection is replaced before the sweep uses it.
            let conn = match self.query(|c| control::server(c).map(drop)) {
                Ok(_) => self.ctl.as_mut().context("no sound server connection")?,
                Err(e) if record.is_some() && live.is_none() => {
                    return Err(e).context(
                        "could not undo what an earlier Sidestage run changed: the sound server is not reachable",
                    );
                }
                Err(e) => {
                    log::info!("pulse: nothing to recover, and no sound server: {e:#}");
                    return Ok(Vec::new());
                }
            };
            let lines =
                control::sweep(conn, record.as_ref().filter(|_| live.is_none()), live.map(|r| r.sink_name.as_str()))?;
            if record.is_some() && live.is_none() {
                journal.clear()?;
            }
            Ok(lines)
        }
    }

    /// This process and its ancestors (stopping before init).
    fn ancestry() -> BTreeSet<u32> {
        let mut pids = BTreeSet::new();
        let mut pid = std::process::id();
        while pids.insert(pid) && pids.len() < 64 {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                break;
            };
            match env::parent_pid_from_stat(&stat) {
                Some(parent) if parent > 1 => pid = parent,
                _ => break,
            }
        }
        pids
    }
}
