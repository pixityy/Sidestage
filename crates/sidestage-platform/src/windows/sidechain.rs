//! The ducking sidechain: how loud "the game" is, meaning every app except Sidestage's sources,
//! the apps the user told it to ignore and Sidestage itself.
//!
//! Each app that counts gets its own process-loopback capture (`INCLUDE` its process tree)
//! that only measures peaks; the level is the loudest app's latest 10 ms packet. The supervisor
//! decides at every session scan which apps count and hands their roots to [`Sidechain::listen`].
//!
//! Why not something cheaper (measured on build 26200):
//! - v0.1's single `EXCLUDE` capture of Sidestage's own tree cannot leave out other apps.
//! - Session peak meters (`IAudioMeterInformation`) do not tell what an app plays: a process
//!   that runs a process-loopback capture gets a session on the captured app's endpoint, and
//!   its meter shows the captured audio. A probe that played silence while capturing a 0.01
//!   test tone read exactly 0.0100 on the tone's endpoint, and a running Sidestage read the
//!   same as every app its `EXCLUDE` capture heard. Counting such an app (a screen recorder, a
//!   streaming voice chat) would feed Sidestage's own output, or an ignored app, into ducking.
//!
//! Process loopback hears only what a tree renders, after its session volume and mute, so
//! hushed, muted and turned-down apps count at the level the user hears them. The system sounds
//! session is not an app and is never listened to.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::capture::{self, Capture, SIDECHAIN_SLOTS, SidechainLevels, Sink, SpawnError, Target};

/// The sidechain only needs peaks, so its captures run at one fixed rate whatever the output
/// does (the engine converts).
const RATE: u32 = 48_000;
/// A listener that stops is restarted after this times its failure count...
const RETRY_BASE: Duration = Duration::from_secs(2);
/// ...and given up after this many failures in a row.
const MAX_FAILURES: u32 = 5;
/// A listener that has run this long has recovered.
const STABLE_AFTER: Duration = Duration::from_secs(10);

/// An app process tree, by root PID and creation time (a PID alone may be reused).
pub type Root = (u32, u64);

/// One app the sidechain listens to.
struct Listener {
    root: Root,
    slot: usize,
    capture: Option<Capture>,
    started: Instant,
    failures: u32,
    retry_at: Option<Instant>,
}

/// The listening captures. Lives on the supervisor thread.
pub struct Sidechain {
    levels: Arc<SidechainLevels>,
    apps: Vec<Listener>,
    free: Vec<usize>,
    /// More apps count than there are slots (logged when it starts).
    full: bool,
}

impl Sidechain {
    pub fn new(levels: Arc<SidechainLevels>) -> Sidechain {
        Sidechain { levels, apps: Vec::new(), free: (0..SIDECHAIN_SLOTS).rev().collect(), full: false }
    }

    pub fn levels(&self) -> &Arc<SidechainLevels> {
        &self.levels
    }

    /// Listens to exactly these app trees from now on: new ones get a capture, gone ones are
    /// stopped. Does not wait for new captures to start.
    pub fn listen(&mut self, roots: &[Root]) {
        let mut i = 0;
        while i < self.apps.len() {
            if roots.contains(&self.apps[i].root) {
                i += 1;
                continue;
            }
            let gone = self.apps.swap_remove(i);
            self.release(gone);
        }
        let mut full = false;
        for &root in roots {
            if self.apps.iter().any(|a| a.root == root) {
                continue;
            }
            let Some(slot) = self.free.pop() else {
                full = true;
                break;
            };
            let mut l = Listener { root, slot, capture: None, started: Instant::now(), failures: 0, retry_at: None };
            start(&self.levels, &mut l);
            self.apps.push(l);
        }
        // Recomputed at every call: the warning goes away once fewer apps count.
        if full && !self.full {
            log::warn!("ducking listens to at most {SIDECHAIN_SLOTS} apps; the others are not heard");
        }
        self.full = full;
    }

    /// Restarts listeners that stopped (an app whose audio cannot be captured is given up on
    /// after a few tries).
    pub fn tick(&mut self) {
        let now = Instant::now();
        for l in &mut self.apps {
            if l.capture.as_ref().is_some_and(Capture::finished) {
                let err = l.capture.take().and_then(|c| c.stop().0);
                self.levels.clear(l.slot);
                if l.started.elapsed() >= STABLE_AFTER {
                    l.failures = 0;
                }
                let why = err.as_deref().unwrap_or("the capture stopped");
                failed(l, why, now);
            } else if l.capture.is_none() && l.retry_at.is_some_and(|t| now >= t) {
                start(&self.levels, l);
            }
        }
    }

    /// Apps that count but cannot be heard (given up on, or no slot left).
    pub fn unheard(&self) -> usize {
        self.apps.iter().filter(|l| l.capture.is_none() && l.retry_at.is_none()).count() + usize::from(self.full)
    }

    fn release(&mut self, l: Listener) {
        if let Some(c) = l.capture {
            c.stop();
        }
        self.levels.clear(l.slot);
        self.free.push(l.slot);
    }
}

fn start(levels: &Arc<SidechainLevels>, l: &mut Listener) {
    l.retry_at = None;
    l.started = Instant::now();
    let sink = Sink::Sidechain { levels: levels.clone(), slot: l.slot };
    match capture::spawn_unwaited(Target::Include(l.root.0), RATE, sink) {
        Ok(c) => l.capture = Some(c),
        Err(SpawnError { error, .. }) => failed(l, &format!("{error:#}"), Instant::now()),
    }
}

/// Schedules a retry, or gives up after [`MAX_FAILURES`] in a row.
fn failed(l: &mut Listener, why: &str, now: Instant) {
    l.failures += 1;
    log::debug!("ducking stopped hearing pid {}: {why}", l.root.0);
    if l.failures >= MAX_FAILURES {
        log::warn!("ducking cannot hear the app with pid {} ({why}); giving up on it", l.root.0);
        l.retry_at = None;
    } else {
        l.retry_at = Some(now + RETRY_BASE * l.failures);
    }
}

impl Drop for Sidechain {
    fn drop(&mut self) {
        for l in std::mem::take(&mut self.apps) {
            self.release(l);
        }
        // No ducking level outlives the sidechain.
        for slot in 0..SIDECHAIN_SLOTS {
            self.levels.clear(slot);
        }
    }
}
