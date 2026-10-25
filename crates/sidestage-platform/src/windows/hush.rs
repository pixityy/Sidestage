//! Hiding the original ("hush"): each source's app sessions are turned down to -120 dB and its
//! capture is multiplied back up.
//!
//! Process loopback taps the app *after* its session volume, and that volume is an exact linear
//! gain (measured down to 1e-7), so a session at 1e-6 is inaudible while its capture times 1e6
//! is the original signal. Windows persists per-app volume, so the original volume and mute of
//! every session are written to the journal before anything changes; a clean stop, the
//! guardian process or the next start restores them.
//!
//! One journal record holds every app the session hides: the session's [`Ledger`] owns it and
//! each source's [`Hush`] adds and removes its own app's entries.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::Media::Audio::{AudioSessionStateExpired, IAudioSessionEvents, ISimpleAudioVolume};

use super::capture::Gain;
use super::com::{NamedMutex, NamedMutexGuard, process_alive, process_created};
use super::notify::{self, Event, SIDESTAGE_CONTEXT};
use super::procs::{app_key, match_key, pid_path};
use super::sessions::SessionRef;
use super::{guardian, sessions};
use crate::journal::Journal;

pub const JOURNAL_NAME: &str = "windows-hush";

/// The cross-process lock around reading and rewriting the journal at `path`. Sidestage and the
/// guardians of earlier runs both rewrite it; without the lock one could overwrite the other's
/// fresh record. `None` (logged) if the lock cannot be created; callers then go ahead unlocked.
pub fn journal_mutex(path: &Path) -> Option<NamedMutex> {
    NamedMutex::new(&path.to_string_lossy()).map_err(|e| log::warn!("hush journal lock unavailable: {e:#}")).ok()
}

/// Takes the journal lock, waiting a bounded time: a process that holds it is only ever doing
/// a few milliseconds of work, so a timeout means it is stuck and waiting longer would not help.
pub fn lock_journal(m: &Option<NamedMutex>) -> Option<NamedMutexGuard<'_>> {
    let g = m.as_ref()?.lock(Duration::from_secs(2));
    if g.is_none() {
        log::warn!("the hush journal lock is held by a stuck process; going ahead without it");
    }
    g
}

/// Session volumes Sidestage hushes to, quietest first. A level that reads back as 0 (treated
/// as mute by some engine) moves on to the next one.
pub const LEVELS: [f32; 3] = [1e-6, 1e-5, 1e-4];

/// Upper bound for the makeup gain (1 / 1e-7).
const MAX_MAKEUP: f32 = 1e7;

/// A session at or below this volume is considered hushed by Sidestage.
const HUSHED_BELOW: f32 = 1e-3;

// The ladder is quietest first, every level is inaudible, its makeup fits under the bound and
// all levels count as "hushed" when restoring.
const _: () = assert!(LEVELS[0] > 0.0 && LEVELS[0] < LEVELS[1] && LEVELS[1] < LEVELS[2]);
const _: () = assert!(LEVELS[2] <= 1e-4 && LEVELS[2] * 10.0 <= HUSHED_BELOW);
const _: () = assert!(1.0 / LEVELS[0] <= MAX_MAKEUP);

/// Entries not restorable for this long (the output was removed for good, the app is never
/// used again) are dropped instead of being reported at every start.
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The crash-recovery record.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Record {
    pub version: u32,
    /// The Sidestage process responsible for restoring these entries.
    pub owner_pid: u32,
    pub owner_created: u64,
    pub entries: Vec<Entry>,
}

/// One session Sidestage turned down: what to restore and where.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The app's key: lowercase executable path of its root process.
    pub app: String,
    pub app_name: String,
    /// `IAudioSessionControl2::GetSessionIdentifier`, stable per app and endpoint, which is what
    /// Windows persists volume under.
    pub session_id: String,
    /// Executable path of the process that owned the session, lowercase.
    pub exe: String,
    pub endpoint_id: String,
    /// For messages ("Speakers (Realtek(R) Audio)").
    #[serde(default)]
    pub endpoint_name: String,
    /// When the entry was written (Unix seconds); entries older than [`MAX_AGE`] are dropped.
    #[serde(default)]
    pub since: u64,
    /// Volume and mute to restore.
    pub volume: f32,
    pub muted: bool,
    /// The volume Sidestage set; a session found well above it was changed since and is left
    /// alone.
    pub hush_level: f32,
}

/// The process-wide owner of the hush journal. All of this process's reads and writes of the
/// file go through it.
pub struct Store {
    journal: Journal<Record>,
    lock: Mutex<()>,
    /// Shared with other Sidestage processes and guardians.
    shared: Option<NamedMutex>,
    /// A session's [`Ledger`] owns the entries (it may be hiding several apps).
    hiding: AtomicBool,
    /// Entries remain that could not be restored yet (their app is not running).
    pending: AtomicBool,
}

/// Both journal locks; the cross-process one is released first.
struct StoreGuard<'a> {
    _shared: Option<NamedMutexGuard<'a>>,
    _local: MutexGuard<'a, ()>,
}

static STORE: OnceLock<Store> = OnceLock::new();

pub fn store() -> anyhow::Result<&'static Store> {
    if let Some(s) = STORE.get() {
        return Ok(s);
    }
    let journal = Journal::open(JOURNAL_NAME)?;
    let shared = journal_mutex(journal.path());
    Ok(STORE.get_or_init(|| Store {
        journal,
        lock: Mutex::new(()),
        shared,
        hiding: AtomicBool::new(false),
        pending: AtomicBool::new(true),
    }))
}

/// This process's identity for the journal: PID and creation time.
pub fn me() -> (u32, u64) {
    static ME: OnceLock<(u32, u64)> = OnceLock::new();
    *ME.get_or_init(|| {
        let pid = std::process::id();
        (pid, process_created(pid).unwrap_or(0))
    })
}

impl Store {
    /// Entries may remain that are not owned by a running session.
    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire) && !self.hiding.load(Ordering::Acquire)
    }

    fn guard(&self) -> StoreGuard<'_> {
        // In-process first, so this process's threads do not queue on the shared mutex.
        let local = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        StoreGuard { _shared: lock_journal(&self.shared), _local: local }
    }

    /// Loads the record. A journal that cannot be parsed is moved aside (kept for inspection)
    /// so it cannot block Sidestage forever.
    fn load(&self) -> anyhow::Result<Option<Record>> {
        match self.journal.load() {
            Err(e) if e.chain().any(|c| c.is::<serde_json::Error>()) => {
                let path = self.journal.path();
                let aside = path.with_extension("corrupt.json");
                log::error!("{e:#}; moving it to {}", aside.display());
                std::fs::rename(path, &aside).with_context(|| format!("moving {} aside", path.display()))?;
                Ok(None)
            }
            r => r,
        }
    }

    /// Loads the entries this process may act on: its own, or a dead process's (then `false`:
    /// not yet owned by us). `None` if another running Sidestage owns the journal.
    fn load_owned(&self) -> anyhow::Result<Option<(Vec<Entry>, bool)>> {
        let Some(rec) = self.load()? else {
            return Ok(Some((Vec::new(), true)));
        };
        let (pid, created) = me();
        let mine = rec.owner_pid == pid && (rec.owner_created == created || rec.owner_created == 0);
        if !mine && process_alive(rec.owner_pid, rec.owner_created) {
            return Ok(None);
        }
        Ok(Some((rec.entries, mine)))
    }

    fn write(&self, entries: &[Entry]) -> anyhow::Result<()> {
        if entries.is_empty() {
            return self.journal.clear();
        }
        // The guardian must exist before anything it would have to undo.
        guardian::ensure(self.journal.path());
        let (owner_pid, owner_created) = me();
        self.journal.save(&Record { version: 1, owner_pid, owner_created, entries: entries.to_vec() })
    }

    /// Claims the journal for a session's ledger. Returns the entries left by earlier runs.
    pub fn begin_hiding(&self) -> anyhow::Result<Vec<Entry>> {
        let _g = self.guard();
        if self.hiding.load(Ordering::Acquire) {
            bail!("an earlier Sidestage session is still hiding apps; stop it first");
        }
        let Some((entries, _)) = self.load_owned()? else {
            bail!("another Sidestage window is already hiding apps; close it first");
        };
        if !entries.is_empty() {
            self.write(&entries)?; // take ownership
        }
        self.hiding.store(true, Ordering::Release);
        Ok(entries)
    }

    /// Saves the ledger's entries (call before every change to the OS).
    pub fn save(&self, entries: &[Entry]) -> anyhow::Result<()> {
        let _g = self.guard();
        self.write(entries)
    }

    /// Releases the journal; `remaining` could not be restored yet.
    pub fn end_hiding(&self, remaining: &[Entry]) -> anyhow::Result<()> {
        let _g = self.guard();
        let r = self.write(remaining);
        self.pending.store(!remaining.is_empty(), Ordering::Release);
        self.hiding.store(false, Ordering::Release);
        r
    }

    /// Restores whatever earlier runs left behind and is restorable now (no hiding session may
    /// be active). Returns plain-language lines for what was restored and what still waits.
    pub fn restore_leftovers(&self, managers: &sessions::Managers, force: bool) -> anyhow::Result<Vec<String>> {
        if self.hiding.load(Ordering::Acquire) || (!force && !self.pending.load(Ordering::Acquire)) {
            return Ok(Vec::new());
        }
        let _g = self.guard();
        if self.hiding.load(Ordering::Acquire) {
            return Ok(Vec::new());
        }
        let Some((entries, owned)) = self.load_owned()? else {
            return Ok(Vec::new());
        };
        if entries.is_empty() {
            self.pending.store(false, Ordering::Release);
            return Ok(Vec::new());
        }
        let (entries, dropped) = drop_stale(entries);
        let live = sessions::scan(managers);
        let (restored, remaining) = restore(entries, &live);
        // Take a dead run's entries over once (our guardian then keeps them after we exit),
        // after that write only when something changed: every write is an fsync.
        if !owned || !restored.is_empty() || !dropped.is_empty() {
            self.write(&remaining)?;
        }
        self.pending.store(!remaining.is_empty(), Ordering::Release);

        let mut lines = Vec::new();
        let mut names: Vec<&str> = restored.iter().map(|e| e.app_name.as_str()).collect();
        names.sort();
        names.dedup();
        for name in names {
            lines.push(format!("{name} was still turned down by an earlier Sidestage run; its normal volume is back."));
        }
        let mut waiting: Vec<(&str, &str)> =
            remaining.iter().map(|e| (e.app_name.as_str(), e.endpoint_name.as_str())).collect();
        waiting.sort();
        waiting.dedup();
        for (name, output) in waiting {
            let output = if output.is_empty() { "one of your outputs" } else { output };
            lines.push(format!(
                "{name} is still turned down on {output} by an earlier Sidestage run; it will be restored the next time {name} plays there."
            ));
        }
        for e in &dropped {
            log::info!("dropped the hush journal entry for {} on {}", e.app_name, e.endpoint_name);
        }
        Ok(lines)
    }
}

/// Splits off entries that can never be restored: the executable no longer exists (Windows
/// forgets the volume of an uninstalled or updated app's old path anyway), or the entry has
/// waited longer than [`MAX_AGE`].
pub fn drop_stale(entries: Vec<Entry>) -> (Vec<Entry>, Vec<Entry>) {
    let now = unix_now();
    entries.into_iter().partition(|e| {
        let exe = if e.exe.is_empty() { &e.app } else { &e.exe };
        let fresh = e.since == 0 || now.saturating_sub(e.since) < MAX_AGE.as_secs();
        fresh && !exe_gone(Path::new(exe))
    })
}

/// True only if the executable is known to be gone. A path that cannot be checked (access
/// denied), or one on a drive that is not there right now (an external or network drive), may
/// come back, and dropping its entry would leave the app silent for good.
fn exe_gone(exe: &Path) -> bool {
    let root_present = exe.ancestors().last().is_some_and(|root| root.try_exists().unwrap_or(true));
    root_present && matches!(exe.try_exists(), Ok(false))
}

/// Restores every entry that has a live session. Returns (restored, remaining).
///
/// An entry belongs to the session with its identifier (what Windows saved the volume under).
/// Only if that one is not live does it fall back to a session of the same executable on the
/// same output that no entry claims (the app regrouped its streams under a new identifier):
/// with several sessions per output, a looser match would give one session another's volume.
pub fn restore(entries: Vec<Entry>, live: &[SessionRef]) -> (Vec<Entry>, Vec<Entry>) {
    let mut paths: HashMap<u32, Option<String>> = HashMap::new();
    let keys: Vec<(&str, &str)> = live.iter().map(|s| (s.session_id.as_str(), s.endpoint_id.as_str())).collect();
    let targets = match_sessions(&entries, &keys, |i| {
        let pid = live[i].pid;
        paths.entry(pid).or_insert_with(|| pid_path(pid).map(|p| app_key(&p))).clone()
    });
    let mut restored = Vec::new();
    let mut remaining = Vec::new();
    for (e, t) in entries.into_iter().zip(targets) {
        let mut ok = !t.is_empty();
        for s in t.into_iter().map(|i| &live[i]) {
            if let Err(err) = restore_one(s, &e) {
                log::warn!("restoring {}'s volume: {err:#}", e.app_name);
                ok = false;
            }
        }
        if ok { restored.push(e) } else { remaining.push(e) }
    }
    (restored, remaining)
}

/// For each entry, the indices of the live sessions (`(session id, endpoint id)`) it restores;
/// `exe_of` gives a live session's executable key. See [`restore`].
fn match_sessions(
    entries: &[Entry],
    live: &[(&str, &str)],
    mut exe_of: impl FnMut(usize) -> Option<String>,
) -> Vec<Vec<usize>> {
    let claimed: HashSet<&str> = entries.iter().map(|e| e.session_id.as_str()).collect();
    entries
        .iter()
        .map(|e| {
            let exact: Vec<usize> = (0..live.len()).filter(|&i| live[i].0 == e.session_id).collect();
            if !exact.is_empty() {
                return exact;
            }
            (0..live.len())
                .filter(|&i| {
                    let (sid, ep) = live[i];
                    ep == e.endpoint_id && !claimed.contains(sid) && exe_of(i).as_deref() == Some(e.exe.as_str())
                })
                .collect()
        })
        .collect()
}

fn restore_one(s: &SessionRef, e: &Entry) -> anyhow::Result<()> {
    let vol = s.volume()?;
    let cur = unsafe { vol.GetMasterVolume() }?;
    // Well above our level: the user or the app has set a volume since; leave it.
    if cur > (e.hush_level * 10.0).max(HUSHED_BELOW) {
        return Ok(());
    }
    let target = e.volume.clamp(0.0, 1.0);
    // Already there (an original volume of about 0): nothing to change or report.
    if (cur - target).abs() <= 1e-7 && unsafe { vol.GetMute() }?.as_bool() == e.muted {
        return Ok(());
    }
    unsafe {
        vol.SetMasterVolume(target, &SIDESTAGE_CONTEXT)?;
        vol.SetMute(e.muted, &SIDESTAGE_CONTEXT)?;
    }
    log::info!("restored {} to volume {:.3}{}", e.app_name, e.volume, if e.muted { " (muted)" } else { "" });
    Ok(())
}

/// Gives a session of another instance of a source's app (not captured) the app's volume back
/// if Windows started it at a hushed level. Only an exact hush level counts: any other volume,
/// and the mute state, are the user's or the app's choice for that instance.
fn restore_instance(s: &SessionRef, e: &Entry) -> anyhow::Result<()> {
    let vol = s.volume()?;
    let cur = unsafe { vol.GetMasterVolume() }?;
    if !LEVELS.iter().any(|&l| (cur - l).abs() <= l * 0.01) {
        return Ok(());
    }
    unsafe { vol.SetMasterVolume(e.volume.clamp(0.0, 1.0), &SIDESTAGE_CONTEXT) }?;
    log::info!("another {} started at the hushed volume; set it to {:.3}", e.app_name, e.volume);
    Ok(())
}

/// The session's share of the journal: the entries of every source's sessions plus whatever
/// earlier runs (or sources removed while their app was closed) left to restore, all written as
/// one record. Lives on the supervisor thread, which makes every change to the OS.
///
/// The journal is claimed the first time a source hides its app and released when the session
/// ends. Dropping an unreleased ledger (the supervisor panicked) still restores what it can.
pub struct Ledger {
    store: Option<&'static Store>,
    pub entries: Vec<Entry>,
    /// In-memory entries changed since the journal was last written.
    dirty: bool,
    released: bool,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new()
    }
}

impl Ledger {
    pub fn new() -> Self {
        Ledger { store: None, entries: Vec::new(), dirty: false, released: false }
    }

    /// Takes the journal over for this session (once): fails if another running Sidestage is
    /// hiding apps. Leftovers of earlier runs become this session's to restore.
    pub fn claim(&mut self) -> anyhow::Result<()> {
        if self.store.is_some() {
            return Ok(());
        }
        let store = store()?;
        self.entries = store.begin_hiding()?;
        self.store = Some(store);
        self.released = false;
        Ok(())
    }

    /// Writes every entry (call before each change to the OS).
    pub fn save(&mut self) -> anyhow::Result<()> {
        let Some(store) = self.store else {
            return Ok(());
        };
        store.save(&self.entries).context("writing the crash-recovery journal")?;
        self.dirty = false;
        Ok(())
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Writes the journal if entries changed since the last write.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        if self.dirty { self.save() } else { Ok(()) }
    }

    /// Restores entries of apps no source is hiding now (an earlier run's leftovers, a source
    /// removed while its app was closed, ...) whose sessions are live.
    pub fn restore_foreign(&mut self, live: &[SessionRef], hidden_by_source: impl Fn(&str) -> bool) {
        if self.entries.is_empty() {
            return;
        }
        let (others, mine): (Vec<Entry>, Vec<Entry>) =
            std::mem::take(&mut self.entries).into_iter().partition(|e| !hidden_by_source(&e.app));
        self.entries = mine;
        if others.is_empty() {
            return;
        }
        let (others, dropped) = drop_stale(others);
        let (restored, remaining) = restore(others, live);
        self.entries.extend(remaining);
        for e in &dropped {
            log::info!("dropped the hush journal entry for {} on {}", e.app_name, e.endpoint_name);
        }
        if (!restored.is_empty() || !dropped.is_empty())
            && let Err(e) = self.save()
        {
            log::warn!("{e:#}");
        }
    }

    /// Restores every entry whose session is live and releases the journal; the rest stay
    /// journaled for the platform worker, the guardian or the next start.
    pub fn release(&mut self, live: &[SessionRef]) -> anyhow::Result<()> {
        let Some(store) = self.store else {
            return Ok(());
        };
        if self.released {
            return Ok(());
        }
        self.released = true;
        let (restored, remaining) = restore(std::mem::take(&mut self.entries), live);
        if !restored.is_empty() {
            log::info!("restored {} leftover session(s)", restored.len());
        }
        let r = store.end_hiding(&remaining).context("updating the crash-recovery journal");
        self.store = None;
        r
    }
}

impl Drop for Ledger {
    fn drop(&mut self) {
        if self.store.is_none() || self.released {
            return;
        }
        // Only reached if the session ended without finishing (a panic): put back what is live
        // now; the guardian and the next start handle the rest.
        let mut managers = sessions::Managers::default();
        if let Ok(en) = sessions::enumerator()
            && let Err(e) = managers.refresh(&en)
        {
            log::debug!("listing outputs to restore volumes: {e:#}");
        }
        let live = sessions::scan(&managers);
        if let Err(e) = self.release(&live) {
            log::error!("restoring volumes: {e:#}");
        }
    }
}

/// A session of a source's app that Sidestage has turned down.
struct Tracked {
    session: SessionRef,
    vol: ISimpleAudioVolume,
    events: IAudioSessionEvents,
    /// Muted by the user or the app (Sidestage unmutes when it hushes).
    muted: bool,
    /// Someone changed its volume; set it back to our level.
    rehush: bool,
}

/// What a foreign volume change means for the source.
#[derive(Debug, PartialEq)]
pub enum Change {
    None,
    /// The audible level changed: re-hush after the debounce.
    Volume,
    /// Only the mute state changed.
    Mute,
}

/// Identifies one source's hush state in notifications (a removed source's late events must
/// not reach a newer source of the same app).
pub fn next_token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The hush state of one source: its app's sessions, the level they are held at and the
/// volume the user wants to hear the app at. Its journal entries live in the [`Ledger`].
pub struct Hush {
    app: String,
    match_key: String,
    app_name: String,
    token: u64,
    tracked: HashMap<Arc<str>, Tracked>,
    level: usize,
    /// The volume read back after hushing (f32 bits), shared with the event sinks.
    applied: Arc<AtomicU32>,
    /// The app's volume as the user wants to hear it: the original session volume, then
    /// whatever the user or app sets while hidden.
    desired: Option<f32>,
    gain: Arc<Gain>,
    tx: Sender<Event>,
    /// When Sidestage last set the volume, and foreign changes that came right after it: an
    /// app that resets its own volume every time is "fighting".
    last_apply: Option<Instant>,
    fights: VecDeque<Instant>,
    /// Hushing failed at every level: the original stays audible.
    pub failed: bool,
    finished: bool,
}

impl Hush {
    pub fn new(app: &str, app_name: &str, token: u64, gain: Arc<Gain>, tx: Sender<Event>) -> Self {
        Hush {
            app: app.to_owned(),
            match_key: match_key(app),
            app_name: app_name.to_owned(),
            token,
            tracked: HashMap::new(),
            level: 0,
            applied: Arc::new(AtomicU32::new(LEVELS[0].to_bits())),
            desired: None,
            gain,
            tx,
            last_apply: None,
            fights: VecDeque::new(),
            failed: false,
            finished: false,
        }
    }

    /// Names this hush state in its sessions' notifications.
    pub fn token(&self) -> u64 {
        self.token
    }

    pub fn set_app_name(&mut self, name: &str) {
        name.clone_into(&mut self.app_name);
    }

    /// True if a journal entry is this app's (also one written for an older version of it).
    pub fn owns(&self, e: &Entry) -> bool {
        e.app == self.app || match_key(&e.app) == self.match_key
    }

    pub fn is_tracked(&self, instance: &str) -> bool {
        self.tracked.contains_key(instance)
    }

    pub fn tracked_count(&self) -> usize {
        self.tracked.len()
    }

    pub fn applied(&self) -> f32 {
        f32::from_bits(self.applied.load(Ordering::Relaxed))
    }

    pub fn desired(&self) -> f32 {
        self.desired.unwrap_or(1.0)
    }

    /// Gain that turns the hushed capture back into the app's audio at the user's level.
    pub fn makeup(&self) -> f32 {
        if self.failed || self.tracked.is_empty() {
            return 1.0;
        }
        let applied = self.applied();
        if applied <= 0.0 { 0.0 } else { (self.desired() / applied).min(MAX_MAKEUP) }
    }

    pub fn any_muted(&self) -> bool {
        self.tracked.values().any(|t| t.muted)
    }

    /// True if the app keeps resetting its volume right after Sidestage hushes it. (A user
    /// dragging the mixer slider produces a burst of changes, not a fight.)
    pub fn fighting(&self) -> bool {
        let recent = self.fights.iter().filter(|t| t.elapsed() < Duration::from_secs(30));
        recent.count() >= 3
    }

    pub fn hidden(&self) -> bool {
        !self.failed && !self.tracked.is_empty()
    }

    /// Journals and hushes the given sessions of the app that are not tracked yet. Returns true
    /// if any was hushed (the capture level changes, so the gate must settle).
    pub fn adopt(&mut self, ledger: &mut Ledger, new: Vec<SessionRef>) -> anyhow::Result<bool> {
        if self.failed {
            return Ok(false);
        }
        let mut fresh: Vec<(SessionRef, ISimpleAudioVolume, f32, bool)> = Vec::new();
        for s in new {
            if self.tracked.contains_key(s.instance_id.as_str())
                || fresh.iter().any(|f| f.0.instance_id == s.instance_id)
            {
                continue;
            }
            let vol = match s.volume() {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("cannot control a session of {}: {e}", self.app_name);
                    continue;
                }
            };
            let (Ok(cur), Ok(muted)) = (unsafe { vol.GetMasterVolume() }, unsafe { vol.GetMute() }) else {
                continue;
            };
            fresh.push((s, vol, cur, muted.as_bool()));
        }
        if fresh.is_empty() {
            return Ok(false);
        }
        ledger.claim()?;

        // Journal first. A session already near our level was hushed by an earlier run whose
        // entry is gone; give it the app's level (or full volume) back on restore. A session
        // that still has an entry (an earlier run crashed, or this app was a source before)
        // keeps it: it holds the real original volume.
        let fallback = self.desired.unwrap_or(1.0);
        for (s, _, cur, muted) in &fresh {
            if ledger.entries.iter().any(|e| e.session_id == s.session_id) {
                continue;
            }
            let volume = if *cur > 0.0 && *cur <= HUSHED_BELOW { fallback } else { *cur };
            let exe = pid_path(s.pid).map(|p| app_key(&p)).unwrap_or_default();
            ledger.entries.push(Entry {
                app: self.app.clone(),
                app_name: self.app_name.clone(),
                session_id: s.session_id.clone(),
                exe,
                endpoint_id: s.endpoint_id.clone(),
                endpoint_name: s.endpoint_name.clone(),
                since: unix_now(),
                volume,
                muted: *muted,
                hush_level: LEVELS[LEVELS.len() - 1],
            });
        }
        if self.desired.is_none() {
            let primary = fresh.iter().find(|f| f.0.active()).unwrap_or(&fresh[0]);
            let entry = ledger.entries.iter().find(|e| e.session_id == primary.0.session_id);
            self.desired = Some(entry.map_or(1.0, |e| e.volume));
        }
        ledger.save()?;
        log::info!("hiding {} session(s) of {} (volume to restore: {:.3})", fresh.len(), self.app_name, self.desired());

        for (s, vol, _, _) in fresh {
            let instance: Arc<str> = Arc::from(s.instance_id.as_str());
            let events = notify::hushed_events(
                self.token,
                instance.clone(),
                self.tx.clone(),
                self.gain.clone(),
                self.applied.clone(),
            );
            if let Err(e) = unsafe { s.ctl.RegisterAudioSessionNotification(&events) } {
                log::warn!("cannot watch a session of {} for volume changes: {e}", self.app_name);
            }
            self.tracked.insert(instance, Tracked { session: s, vol, events, muted: false, rehush: true });
        }
        self.apply(ledger)?;
        Ok(true)
    }

    /// Sets every tracked session that needs it to the current level and reads the level back.
    /// Moves to a louder level if the engine reports 0 (treats it as mute).
    pub fn apply(&mut self, ledger: &mut Ledger) -> anyhow::Result<()> {
        // What to restore must be on disk before the volume changes.
        ledger.flush()?;
        self.last_apply = Some(Instant::now());
        while !self.failed {
            let level = LEVELS[self.level];
            let mut readback: Option<f32> = None;
            let mut zero = false;
            for t in self.tracked.values_mut() {
                if !t.rehush {
                    continue;
                }
                let mut r = match set_level(&t.vol, level, !t.muted) {
                    Ok(r) => r,
                    Err(e) => {
                        // Usually the session just went away; its end is handled elsewhere.
                        log::debug!("hushing a session of {}: {e}", self.app_name);
                        t.rehush = false;
                        continue;
                    }
                };
                if r <= 0.0 {
                    r = set_level(&t.vol, level, !t.muted).unwrap_or(0.0);
                }
                if r <= 0.0 {
                    zero = true;
                    break;
                }
                t.rehush = false;
                readback = Some(readback.map_or(r, |x: f32| x.max(r)));
            }
            if zero {
                self.level += 1;
                if self.level >= LEVELS.len() {
                    self.failed = true;
                    log::warn!("{} cannot be turned down without muting it; keeping it audible", self.app_name);
                    self.unhush_all(ledger);
                    break;
                }
                log::info!("volume {level} reads back as 0; trying {}", LEVELS[self.level]);
                for t in self.tracked.values_mut() {
                    t.rehush = true;
                }
                continue;
            }
            if let Some(r) = readback {
                self.applied.store(r.to_bits(), Ordering::Relaxed);
            }
            break;
        }
        Ok(())
    }

    /// True if every tracked session is at the applied level (the gate may open).
    pub fn settled(&self) -> bool {
        let applied = self.applied();
        self.tracked.values().all(|t| {
            !t.rehush && unsafe { t.vol.GetMasterVolume() }.map_or(true, |v| (v - applied).abs() <= applied * 0.01)
        })
    }

    /// Marks every session for re-hushing (after a capture fault).
    pub fn rehush_all(&mut self) {
        for t in self.tracked.values_mut() {
            t.rehush = true;
        }
    }

    /// Someone other than Sidestage changed a tracked session's volume or mute.
    pub fn on_foreign_change(&mut self, ledger: &mut Ledger, instance: &str, volume: f32, muted: bool) -> Change {
        let applied = self.applied();
        let Some(t) = self.tracked.get_mut(instance) else {
            return Change::None;
        };
        let mut change = Change::None;
        let session_id = t.session.session_id.clone();
        if muted != t.muted {
            t.muted = muted;
            change = Change::Mute;
        }
        if (volume - applied).abs() > applied * 0.01 {
            t.rehush = true;
            self.desired = Some(volume.clamp(0.0, 1.0));
            let now = Instant::now();
            // An app that fights resets its volume right after each hush; count that once
            // per hush (a single change arrives as several notifications).
            if self.last_apply.take().is_some_and(|t| now.duration_since(t) < Duration::from_millis(300)) {
                self.fights.push_back(now);
            }
            while self.fights.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(30)) {
                self.fights.pop_front();
            }
            change = Change::Volume;
        }
        if change != Change::None {
            for e in ledger.entries.iter_mut().filter(|e| e.session_id == session_id) {
                if change == Change::Volume {
                    e.volume = volume.clamp(0.0, 1.0);
                }
                e.muted = muted;
            }
            // Written before the next re-hush, or by the supervisor shortly: a slider drag
            // sends dozens of changes and each write is an fsync.
            ledger.mark_dirty();
        }
        change
    }

    /// Stops tracking a session that ended (its journal entry stays until it is restored).
    pub fn forget(&mut self, instance: &str) {
        if let Some(t) = self.tracked.remove(instance) {
            let _ = unsafe { t.session.ctl.UnregisterAudioSessionNotification(&t.events) };
        }
    }

    /// Stops tracking every session (the app exited).
    pub fn forget_all(&mut self) {
        let keys: Vec<Arc<str>> = self.tracked.keys().cloned().collect();
        for k in keys {
            self.forget(&k);
        }
    }

    /// Another instance of the app is not captured, so a session of it that started at the
    /// hushed volume Windows saved for the app (the volume of the last session that closed)
    /// gets the app's normal volume back; the entry stays, the attached instance still needs
    /// it. `instances` index `live`.
    pub fn restore_instances(&self, ledger: &Ledger, live: &[SessionRef], instances: &[usize]) {
        for s in instances.iter().filter_map(|&i| live.get(i)) {
            if let Some(e) = ledger.entries.iter().find(|e| self.owns(e) && e.session_id == s.session_id)
                && let Err(err) = restore_instance(s, e)
            {
                log::debug!("restoring another {}: {err:#}", self.app_name);
            }
        }
    }

    /// Makes the tracked sessions audible again at their journaled level (the pipeline failed,
    /// or hushing does not work for this app); they are re-hushed when playback resumes.
    pub fn unhush_all(&mut self, ledger: &Ledger) {
        let desired = self.desired();
        for t in self.tracked.values_mut() {
            let volume =
                ledger.entries.iter().find(|e| e.session_id == t.session.session_id).map_or(desired, |e| e.volume);
            unsafe {
                let _ = t.vol.SetMasterVolume(volume, &SIDESTAGE_CONTEXT);
            }
            t.rehush = true;
        }
    }

    /// Restores every live session of the app and drops their entries; entries whose session
    /// is not live stay in the ledger until the app plays again (see
    /// [`Ledger::restore_foreign`]). `others` are the other live sessions (every output, from
    /// [`sessions::scan`]): the app may have sessions there that are not tracked.
    pub fn finish(&mut self, ledger: &mut Ledger, others: &[SessionRef]) -> anyhow::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let mut live: Vec<SessionRef> = self
            .tracked
            .drain()
            .map(|(_, t)| {
                let _ = unsafe { t.session.ctl.UnregisterAudioSessionNotification(&t.events) };
                t.session
            })
            // An expired session (its app just exited) cannot carry the volume back; its entry
            // stays journaled until the app plays again.
            .filter(|s| unsafe { s.ctl.GetState() }.is_ok_and(|st| st != AudioSessionStateExpired))
            .collect();
        let known: HashSet<String> = live.iter().map(|s| s.instance_id.clone()).collect();
        live.extend(others.iter().filter(|s| !known.contains(&s.instance_id)).cloned());
        let (mine, rest): (Vec<Entry>, Vec<Entry>) =
            std::mem::take(&mut ledger.entries).into_iter().partition(|e| self.owns(e));
        ledger.entries = rest;
        if mine.is_empty() {
            return Ok(());
        }
        let (restored, remaining) = restore(mine, &live);
        if !restored.is_empty() {
            log::info!("restored {} session(s) of {}", restored.len(), self.app_name);
        }
        if !remaining.is_empty() {
            log::info!(
                "{} of {}'s sessions are not live; they will be restored when it plays again",
                remaining.len(),
                self.app_name
            );
        }
        ledger.entries.extend(remaining);
        ledger.save()
    }
}

impl Drop for Hush {
    fn drop(&mut self) {
        // Volumes are put back by `finish` or, failing that, by the ledger; only the
        // notification registrations end here.
        self.forget_all();
    }
}

/// Sets a session's volume with Sidestage's event context and reads it back.
fn set_level(vol: &ISimpleAudioVolume, level: f32, unmute: bool) -> windows::core::Result<f32> {
    unsafe {
        if unmute {
            vol.SetMute(false, &SIDESTAGE_CONTEXT)?;
        }
        vol.SetMasterVolume(level, &SIDESTAGE_CONTEXT)?;
        vol.GetMasterVolume()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(exe: &str, since: u64) -> Entry {
        Entry {
            app: exe.to_owned(),
            app_name: "App".to_owned(),
            session_id: "sid".to_owned(),
            exe: exe.to_owned(),
            endpoint_id: "ep".to_owned(),
            endpoint_name: "Speakers".to_owned(),
            since,
            volume: 0.8,
            muted: false,
            hush_level: LEVELS[LEVELS.len() - 1],
        }
    }

    #[test]
    fn stale_entries_are_dropped() {
        let me = std::env::current_exe().unwrap().to_string_lossy().to_lowercase();
        let old = unix_now() - MAX_AGE.as_secs() - 10;
        let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        let mut entries = vec![
            entry(&me, unix_now()),
            entry(&me, 0),
            entry(&me, old),
            entry(&format!(r"{system_drive}\no\such\app.exe"), unix_now()),
        ];
        // An app on a drive that is not connected right now is kept for when it comes back.
        let absent = ('D'..='Z').rev().map(|d| format!(r"{d}:\")).find(|root| !Path::new(root).exists());
        if let Some(root) = &absent {
            entries.push(entry(&format!(r"{root}games\player.exe"), unix_now()));
        }
        let (keep, drop) = drop_stale(entries);
        assert_eq!(keep.len(), 2 + usize::from(absent.is_some()));
        assert_eq!(drop.len(), 2);
    }

    #[test]
    fn entries_restore_their_own_session_first() {
        let mut a = entry(r"c:\app\player.exe", 0);
        a.session_id = "ep1|player%b{A}".to_owned();
        a.endpoint_id = "ep1".to_owned();
        let mut b = a.clone();
        b.session_id = "ep1|player%b{B}".to_owned();
        let mut c = a.clone();
        c.session_id = "ep1|player%b{C}".to_owned();
        let live = [
            ("ep1|player%b{B}", "ep1"),
            ("ep1|player%b{A}", "ep1"),
            ("ep1|player%b{D}", "ep1"),
            ("ep2|player%b{A}", "ep2"),
            ("ep1|other%b{A}", "ep1"),
        ];
        let exe = |i: usize| Some(if i == 4 { r"c:\other.exe" } else { r"c:\app\player.exe" }.to_owned());
        let t = match_sessions(&[a, b, c], &live, exe);
        assert_eq!(t[0], vec![1], "A restores the session it was saved for");
        assert_eq!(t[1], vec![0], "B too, not A's");
        // C's session is gone: it falls back to the app's unclaimed session on its output only.
        assert_eq!(t[2], vec![2]);
    }

    #[test]
    fn the_journal_lock_is_shared_and_exclusive() {
        let key = format!("sidestage-test-{}", std::process::id());
        let m = NamedMutex::new(&key).unwrap();
        let held = m.lock(Duration::from_secs(1)).unwrap();
        let other = std::thread::spawn(move || {
            let m2 = NamedMutex::new(&key).unwrap();
            let busy = m2.lock(Duration::from_millis(50)).is_none();
            (busy, m2)
        });
        let (busy, m2) = other.join().unwrap();
        assert!(busy, "a second opener waits while the first holds it");
        drop(held);
        assert!(m2.lock(Duration::from_millis(500)).is_some());
    }

    #[test]
    fn journal_reads_entries_without_newer_fields() {
        let json = r#"{"version":1,"owner_pid":4,"owner_created":5,"entries":[{"app":"a","app_name":"A",
            "session_id":"s","exe":"a","endpoint_id":"e","volume":0.5,"muted":true,"hush_level":0.0001}]}"#;
        let rec: Record = serde_json::from_str(json).unwrap();
        assert_eq!(rec.entries[0].since, 0);
        assert!(rec.entries[0].endpoint_name.is_empty());
        assert!(rec.entries[0].muted);
    }
}
