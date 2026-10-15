//! Windows backend: WASAPI process loopback (capture), session volume (hide the original) and
//! shared-mode WASAPI (output).
//!
//! Threads: the caller's thread (winit's STA) never touches COM, and waits for the backend's
//! threads only briefly (a slow audio system gets the previous app list, or a session that is
//! still starting). List and recovery requests run on a platform worker in the MTA, which also
//! puts back volumes left by an earlier run the moment their app opens audio again; each
//! session runs a supervisor thread (MTA) that owns the session volumes and notifications,
//! plus real-time capture/render threads (MTA + MMCSS).
//!
//! Files: `com` (apartments, handles), `procs` (process tree, names), `sessions` (endpoints,
//! sessions, app list), `capture` (process loopback, gain stage), `render` (output), `hush`
//! (session-volume hiding and its journal), `guardian` (crash restore helper), `notify` (COM
//! notification sinks and registrations), `session` (the supervisor).
//!
//! Not offered: `HideMethod::ParkOn`. Routing an app to another output needs the undocumented
//! `IAudioPolicyConfig`, and resetting the route does not move streams the app opened while
//! parked (measured on build 26200), so after Sidestage exits the app could keep playing to the
//! unlistened output until the user restarts it. Session-volume hiding needs no spare output
//! and is fully restorable.

mod capture;
mod com;
mod guardian;
mod hush;
mod notify;
mod procs;
pub(crate) use procs::match_key;
mod render;
mod session;
mod sessions;

use std::collections::HashSet;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::anyhow;
use windows::Win32::Media::Audio::IMMDeviceEnumerator;

use crate::{AudioApp, HideMethod, OutputDevice, Platform, ProcessorFactory, Session, SessionConfig, Support};
use com::{Poster, Worker};
use notify::SessionWatch;
use procs::Names;
use sessions::Managers;

/// Builds that are officially supported by process loopback (Windows 11 and Server 2022).
const SUPPORTED_BUILD: u32 = 20348;
/// Oldest build where process loopback is known to work at all (Windows 10 2004, as OBS uses).
const MIN_BUILD: u32 = 19041;

/// How long the caller's thread may wait for the audio worker when it has nothing to fall back
/// on (the first app list, recovery).
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
/// How long the UI thread waits for a fresh app or output list (usually 15-25 ms) before it
/// gets the previous one; the fresh list is kept for the next call.
const LIST_WAIT: Duration = Duration::from_millis(150);

pub fn create() -> anyhow::Result<Box<dyn Platform>> {
    let build = com::os_build();
    if (MIN_BUILD..SUPPORTED_BUILD).contains(&build) {
        log::warn!("Windows build {build}: per-app audio capture is best effort before build {SUPPORTED_BUILD}");
    }
    let worker = Worker::spawn(
        "sidestage-audio",
        Duration::from_secs(2),
        || {
            Ok(Inventory {
                en: sessions::enumerator()?,
                managers: Managers::default(),
                names: Names::default(),
                poster: None,
                watches: Vec::new(),
            })
        },
        Inventory::tick,
    )?;
    let poster = worker.poster();
    worker.call(CALL_TIMEOUT, move |inv| inv.poster = Some(poster))?;
    Ok(Box::new(WindowsPlatform { build, worker, apps: Arc::default(), outputs: Arc::default() }))
}

/// Entry point for `sidestage --sidestage-helper <...>` on Windows.
pub fn run_helper(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some(guardian::GUARDIAN_ARG) => guardian::run(&args[1..]),
        Some(guardian::LAUNCH_ARG) => guardian::launch(&args[1..]),
        _ => 2,
    }
}

/// COM state of the platform worker.
struct Inventory {
    en: IMMDeviceEnumerator,
    managers: Managers,
    names: Names,
    /// Lets new-session notifications queue a restore check on this worker.
    poster: Option<Poster<Inventory>>,
    /// New-session notifications per endpoint: a leftover volume is restored the moment its
    /// app opens audio again, not at the next tick.
    watches: Vec<SessionWatch>,
}

impl Inventory {
    fn refresh(&mut self) -> anyhow::Result<()> {
        self.managers.refresh(&self.en)?;
        let live: HashSet<&str> = self.managers.list.iter().map(|m| m.endpoint_id.as_str()).collect();
        self.watches.retain(|w| live.contains(w.endpoint_id.as_str()));
        let Some(poster) = &self.poster else {
            return Ok(());
        };
        for m in &self.managers.list {
            if self.watches.iter().any(|w| w.endpoint_id == m.endpoint_id) {
                continue;
            }
            let p = poster.clone();
            match SessionWatch::register(m, move || p.post(Inventory::tick)) {
                Ok(w) => self.watches.push(w),
                Err(e) => log::debug!("watching sessions on {}: {e}", m.endpoint_name),
            }
        }
        Ok(())
    }

    /// Restores volumes left behind by a stopped session or an earlier run as soon as their app
    /// plays again (no-op while a session is hiding an app or nothing is pending).
    fn tick(&mut self) {
        let Ok(store) = hush::store() else { return };
        if !store.has_pending() {
            return;
        }
        let r = self.refresh().and_then(|()| store.restore_leftovers(&self.managers, false));
        match r {
            Ok(lines) => lines.iter().for_each(|l| log::debug!("{l}")),
            Err(e) => log::debug!("restoring leftover volumes: {e:#}"),
        }
    }
}

struct WindowsPlatform {
    build: u32,
    worker: Worker<Inventory>,
    apps: Arc<Mutex<Cached<Vec<AudioApp>>>>,
    outputs: Arc<Mutex<Cached<Vec<OutputDevice>>>>,
}

/// The last good answer to a UI request, and whether a request is still running.
struct Cached<T> {
    last: Option<T>,
    busy: bool,
}

impl<T> Default for Cached<T> {
    fn default() -> Self {
        Cached { last: None, busy: false }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Clears `busy` when the request ends, also if it panics or never runs.
struct NotBusy<T>(Arc<Mutex<Cached<T>>>);

impl<T> Drop for NotBusy<T> {
    fn drop(&mut self) {
        lock(&self.0).busy = false;
    }
}

/// Runs `job` on the worker for the UI thread without blocking it for long: if the audio system
/// is slow (busy, or the audio service restarting), the caller gets the previous answer and the
/// fresh one is kept for the next call. At most one request per kind is queued.
fn cached_call<S: 'static, T: Clone + Send + 'static>(
    worker: &Worker<S>,
    slot: &Arc<Mutex<Cached<T>>>,
    job: impl FnOnce(&mut S) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let slow = || anyhow!("the Windows audio system did not answer in time");
    let wait = {
        let mut s = lock(slot);
        if s.busy {
            return s.last.clone().ok_or_else(slow);
        }
        s.busy = true;
        if s.last.is_some() { LIST_WAIT } else { CALL_TIMEOUT }
    };
    let (tx, rx) = mpsc::sync_channel(1);
    let (done, keep) = (NotBusy(slot.clone()), slot.clone());
    worker.poster().post(move |inv| {
        let r = job(inv);
        if let Ok(v) = &r {
            lock(&keep).last = Some(v.clone());
        }
        drop(done);
        let _ = tx.send(r);
    });
    match rx.recv_timeout(wait) {
        Ok(r) => r,
        Err(RecvTimeoutError::Timeout) => lock(slot).last.clone().ok_or_else(slow),
        Err(RecvTimeoutError::Disconnected) => Err(anyhow!("the audio worker failed while handling a request")),
    }
}

impl Platform for WindowsPlatform {
    fn name(&self) -> &'static str {
        "Windows · WASAPI process loopback"
    }

    fn support(&self) -> Support {
        if self.build != 0 && self.build < MIN_BUILD {
            Support::Unsupported(format!(
                "Sidestage needs Windows 10 version 2004 (build {MIN_BUILD}) or newer; this PC runs build {}.",
                self.build
            ))
        } else {
            Support::Supported
        }
    }

    fn list_apps(&mut self) -> anyhow::Result<Vec<AudioApp>> {
        cached_call(&self.worker, &self.apps, |inv| {
            inv.refresh()?;
            sessions::collect_apps(&inv.managers, &mut inv.names)
        })
    }

    fn list_outputs(&mut self) -> anyhow::Result<Vec<OutputDevice>> {
        cached_call(&self.worker, &self.outputs, |inv| sessions::list_outputs(&inv.en))
    }

    fn hide_methods(&mut self) -> Vec<(HideMethod, String)> {
        vec![
            (HideMethod::Auto, "Turn the original down (recommended)".to_owned()),
            (HideMethod::KeepOriginal, "Keep the original audible (for comparison)".to_owned()),
        ]
    }

    fn start(&mut self, config: SessionConfig, processor: ProcessorFactory) -> anyhow::Result<Box<dyn Session>> {
        if let Support::Unsupported(why) = self.support() {
            anyhow::bail!(why);
        }
        if let HideMethod::ParkOn(_) = config.hide {
            anyhow::bail!("moving an app to another output is not supported on Windows; use the default hide method");
        }
        session::start(config, processor, self.build)
    }

    fn recover(&mut self) -> anyhow::Result<Vec<String>> {
        self.worker.call(CALL_TIMEOUT, |inv| {
            inv.refresh()?;
            let lines = hush::store()?.restore_leftovers(&inv.managers, true)?;
            // Also logged: if the caller stopped waiting, this is the only trace of them.
            lines.iter().for_each(|l| log::info!("{l}"));
            Ok(lines)
        })?
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn slow_lists_fall_back_to_the_previous_answer() {
        let w = Worker::spawn("test-cache", Duration::from_secs(60), || Ok(0u32), |_| {}).unwrap();
        let slot: Arc<Mutex<Cached<u32>>> = Arc::default();
        let slow = |v: u32| {
            move |n: &mut u32| {
                std::thread::sleep(Duration::from_millis(400));
                *n = v;
                Ok(v)
            }
        };
        // Nothing to fall back on yet: the first call waits for the answer.
        assert_eq!(cached_call(&w, &slot, slow(1)).unwrap(), 1);
        let t = Instant::now();
        assert_eq!(cached_call(&w, &slot, slow(2)).unwrap(), 1, "previous answer");
        assert!(t.elapsed() < Duration::from_millis(300), "the caller is not held up");
        // Still busy: no second request is queued.
        assert_eq!(cached_call(&w, &slot, slow(3)).unwrap(), 1);
        std::thread::sleep(Duration::from_millis(450));
        assert_eq!(
            cached_call(&w, &slot, |n: &mut u32| Ok(*n * 10)).unwrap(),
            20,
            "the slow answer was kept, and the skipped request never ran"
        );
        assert_eq!(cached_call(&w, &slot, |_: &mut u32| Ok(7)).unwrap(), 7);
        let err = cached_call(&w, &slot, |_: &mut u32| -> anyhow::Result<u32> { anyhow::bail!("boom") });
        assert!(err.is_err(), "errors are reported, not hidden by the cache");
        assert_eq!(cached_call(&w, &slot, |_: &mut u32| Ok(8)).unwrap(), 8);
    }
}
