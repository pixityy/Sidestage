//! Source bookkeeping that needs no Core Audio (unit-tested on every OS): matching a new source
//! list against the running one, deciding when the session's aggregate must be rebuilt, and
//! what the UI is told about each source.

use std::time::Instant;

use crate::{AppKey, HideMethod, SessionState, SourceConfig, SourceId};

/// Consecutive failed attempts (of one source, or of the session's builds) before giving up.
pub const MAX_FAILURES: u32 = 6;

/// A source keeps running untouched only if its ID, app and hide method are all unchanged
/// (the [`Session::set_sources`](crate::Session::set_sources) contract).
pub fn unchanged(a: &SourceConfig, b: &SourceConfig) -> bool {
    same_source(a.id, &a.app, &a.hide, b)
}

/// [`unchanged`] for a source known only by its identity (a published status entry).
pub fn same_source(id: SourceId, app: &AppKey, hide: &HideMethod, b: &SourceConfig) -> bool {
    id == b.id && app.same_app(&b.app) && *hide == b.hide
}

/// How a new source list relates to the running one.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// `(old index, new index)` of sources that keep running.
    pub kept: Vec<(usize, usize)>,
    /// New-list indices of sources to start (also an ID whose app or hide method changed).
    pub added: Vec<usize>,
    /// Old-list indices of sources to stop.
    pub removed: Vec<usize>,
}

pub fn diff(old: &[&SourceConfig], new: &[SourceConfig]) -> Diff {
    let mut d = Diff::default();
    for (ni, n) in new.iter().enumerate() {
        // An old source can be kept once; `duplicate_error` rules out a second match anyway.
        match old.iter().enumerate().position(|(oi, o)| unchanged(o, n) && !d.kept.iter().any(|(k, _)| *k == oi)) {
            Some(oi) => d.kept.push((oi, ni)),
            None => d.added.push(ni),
        }
    }
    d.removed = (0..old.len()).filter(|oi| !d.kept.iter().any(|(k, _)| k == oi)).collect();
    d
}

/// For each source of `new`, the `set_sources` call since which it has been asked for without
/// a break: an unchanged source keeps its number from `previous`, any other gets `list` (the
/// number of the call that brings `new`). Until the control thread has applied that call, a
/// published state with the same identity is an earlier instance's: the app restarts a failed
/// source by removing it in one call and adding it back in the next.
pub fn requested_since(previous: &[(&SourceConfig, u64)], new: &[SourceConfig], list: u64) -> Vec<u64> {
    new.iter().map(|n| previous.iter().find(|(p, _)| unchanged(p, n)).map_or(list, |&(_, since)| since)).collect()
}

/// What the planner needs to know about one source.
#[derive(Clone, Copy, Debug)]
pub struct LaneNeed {
    /// The session's internal key for the source (unique for the session's lifetime, unlike a
    /// [`SourceId`], which a changed source keeps).
    pub key: u64,
    /// Its app has audio processes now.
    pub alive: bool,
    /// The session gave up on this source.
    pub failed: bool,
    /// It has a tap in the running engine.
    pub attached: bool,
    /// Its tap should be rebuilt with another scope.
    pub rescope: bool,
    /// Earliest time to try attaching it (backoff after a failed attempt).
    pub ready_at: Instant,
}

/// Taps in the running engine that must go because their source was removed or given up: such
/// a tap keeps its app muted.
pub fn stale_lanes(needs: &[LaneNeed], engine_lanes: &[u64]) -> Vec<u64> {
    engine_lanes.iter().copied().filter(|key| !needs.iter().any(|n| n.key == *key && !n.failed)).collect()
}

/// The running engine's taps as the rebuild decisions see them, from `(key, covers a running
/// process)` per tap. A stale tap whose processes have all exited mutes nothing (a relaunched
/// app gets new processes, which it does not cover), so it is left for the next build rather
/// than costing every other source a switch. Unless no tap serves a source any more: then the
/// engine has nothing left to play and all of them count, so it closes.
pub fn binding_lanes(needs: &[LaneNeed], lanes: &[(u64, bool)]) -> Vec<u64> {
    let current = |key: u64| needs.iter().any(|n| n.key == key && !n.failed);
    let serving = lanes.iter().any(|&(key, _)| current(key));
    lanes.iter().filter(|&&(key, live)| !serving || live || current(key)).map(|&(key, _)| key).collect()
}

/// When the engine must be rebuilt for its sources, if at all. A stale tap must go at once. A
/// source whose app has processes but no tap gets one once its backoff has passed. A source
/// whose app went away keeps its tap: it captures silence, lets the processor's tail ring out,
/// and the app's processes are put back into it when it returns, all without a rebuild.
pub fn rebuild_due(needs: &[LaneNeed], engine_lanes: &[u64], now: Instant) -> Option<Instant> {
    if !stale_lanes(needs, engine_lanes).is_empty() {
        return Some(now);
    }
    needs
        .iter()
        .filter(|n| n.alive && !n.failed)
        .filter_map(|n| match (n.attached, n.rescope) {
            (true, true) => Some(now),
            (true, false) => None,
            (false, _) => Some(n.ready_at),
        })
        .min()
}

/// The sources the next engine taps, in session order. A source whose app went away is left
/// out: its tap would only capture silence, and it waits for its app without one.
pub fn lanes_for_build(needs: &[LaneNeed], now: Instant) -> Vec<u64> {
    needs.iter().filter(|n| n.alive && !n.failed && (n.attached || n.ready_at <= now)).map(|n| n.key).collect()
}

/// Whether a freshly prepared engine should replace the running one.
#[derive(Debug, PartialEq, Eq)]
pub enum Switch {
    Go,
    /// Keep the running engine: the new one taps the same sources (only a source being added
    /// failed), so switching would only cost the others a dip.
    NothingGained,
    /// Keep the running engine: these sources play in it, but their new taps failed, and the
    /// new engine would leave them audible unprocessed.
    WouldDrop(Vec<u64>),
}

/// Judges a build that nothing forces (no device or rate change, no tap that must go): it is
/// only worth a switch if it adds or re-scopes a source without losing one. `wanted` are the
/// sources the build was for, `running` the running engine's and `built` the new engine's.
pub fn switch_verdict(wanted: &[u64], running: &[u64], built: &[u64], rescoping: bool) -> Switch {
    let lost: Vec<u64> = wanted.iter().copied().filter(|k| running.contains(k) && !built.contains(k)).collect();
    if !lost.is_empty() {
        Switch::WouldDrop(lost)
    } else if !rescoping && built.iter().all(|k| running.contains(k)) {
        Switch::NothingGained
    } else {
        Switch::Go
    }
}

/// What the UI is told about one source. `session_over` is set once the session gave up (its
/// error is the session's) or stopped.
pub fn source_state(session_over: bool, gave_up: Option<&str>, alive: bool, attached: bool) -> SessionState {
    match (session_over, gave_up) {
        (true, _) => SessionState::Stopped,
        (false, Some(message)) => SessionState::Error(message.to_string()),
        (false, None) if !alive => SessionState::WaitingForApp,
        (false, None) if attached => SessionState::Running,
        (false, None) => SessionState::Starting,
    }
}

/// What the UI is told about the session's output. With no source to tap, the output is not
/// opened at all (the contract allows deferring it) and the session waits for an app.
pub fn session_state(fatal: Option<&str>, engine_running: bool, any_startable: bool) -> SessionState {
    match fatal {
        Some(message) => SessionState::Error(message.to_string()),
        None if engine_running => SessionState::Running,
        None if any_startable => SessionState::Starting,
        None => SessionState::WaitingForApp,
    }
}

/// A readable name before the app has been found: the bundle or file name of the key.
pub fn provisional_name(key: &str) -> String {
    let last = key.rsplit('/').find(|s| !s.is_empty()).unwrap_or(key);
    last.strip_suffix(".app").unwrap_or(last).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Processor;
    use std::sync::Arc;
    use std::time::Duration;

    struct Silence;
    impl Processor for Silence {
        fn process(&mut self, _: &[f32], output: &mut [f32], _: f32) {
            output.fill(0.0);
        }
    }

    fn source(id: u32, app: &str, hide: HideMethod) -> SourceConfig {
        SourceConfig { id, app: AppKey(app.into()), hide, processor: Arc::new(|_| Box::new(Silence)) }
    }

    #[test]
    fn diff_keeps_unchanged_sources_only() {
        let old = [
            source(1, "com.spotify.client", HideMethod::Auto),
            source(2, "com.hnc.Discord", HideMethod::Auto),
            source(3, "com.apple.podcasts", HideMethod::Auto),
        ];
        let new = [
            // Same ID, other app: remove + add.
            source(1, "com.apple.Music", HideMethod::Auto),
            // Unchanged, moved in the list.
            source(3, "com.apple.podcasts", HideMethod::Auto),
            // Same app, other hide method: remove + add.
            source(2, "com.hnc.Discord", HideMethod::KeepOriginal),
            source(4, "org.videolan.vlc", HideMethod::Auto),
        ];
        let old_refs: Vec<&SourceConfig> = old.iter().collect();
        let d = diff(&old_refs, &new);
        assert_eq!(d.kept, vec![(2, 1)]);
        assert_eq!(d.added, vec![0, 2, 3]);
        assert_eq!(d.removed, vec![0, 1]);
        assert_eq!(diff(&[], &new).added.len(), 4);
        assert_eq!(diff(&old_refs, &[]).removed, vec![0, 1, 2]);
        // An unchanged list keeps everything, in order.
        let same = diff(&old_refs, &old);
        assert_eq!(same.kept, vec![(0, 0), (1, 1), (2, 2)]);
        assert!(same.added.is_empty() && same.removed.is_empty());
        assert!(same_source(3, &AppKey("com.apple.podcasts".into()), &HideMethod::Auto, &new[1]));
        assert!(!same_source(2, &AppKey("com.hnc.Discord".into()), &HideMethod::Auto, &new[2]));
    }

    #[test]
    fn an_old_source_is_kept_at_most_once() {
        // Not a valid list (`duplicate_error` refuses it), but the diff must still not hand one
        // running source to two new ones.
        let old = [source(1, "com.spotify.client", HideMethod::Auto)];
        let new =
            [source(1, "com.spotify.client", HideMethod::Auto), source(1, "com.spotify.client", HideMethod::Auto)];
        let d = diff(&old.iter().collect::<Vec<_>>(), &new);
        assert_eq!(d.kept, vec![(0, 0)]);
        assert_eq!(d.added, vec![1]);
    }

    #[test]
    fn a_restarted_source_is_new_from_the_call_that_adds_it_back() {
        let spotify = source(1, "com.spotify.client", HideMethod::Auto);
        let discord = source(2, "com.hnc.Discord", HideMethod::Auto);
        let started = [(&spotify, 0), (&discord, 0)];
        // Call 1 removes Discord, call 2 adds it back unchanged.
        let first = [spotify.clone()];
        assert_eq!(requested_since(&started, &first, 1), vec![0]);
        let after_first = [(&first[0], 0)];
        let second = [spotify.clone(), discord.clone()];
        assert_eq!(requested_since(&after_first, &second, 2), vec![0, 2]);
        // Unchanged lists keep every number; a changed hide method is a new source.
        assert_eq!(requested_since(&started, &[spotify.clone(), discord.clone()], 3), vec![0, 0]);
        let kept = source(2, "com.hnc.Discord", HideMethod::KeepOriginal);
        assert_eq!(requested_since(&started, &[kept], 4), vec![4]);
    }

    fn need(key: u64, alive: bool, attached: bool, ready_at: Instant) -> LaneNeed {
        LaneNeed { key, alive, failed: false, attached, rescope: false, ready_at }
    }

    #[test]
    fn rebuilds_only_for_tap_changes() {
        let now = Instant::now();
        // Everything attached and playing: nothing to do.
        let steady = [need(1, true, true, now), need(2, true, true, now)];
        assert_eq!(rebuild_due(&steady, &[1, 2], now), None);
        // An app that quit keeps its (silent) tap.
        let quit = [need(1, true, true, now), need(2, false, true, now)];
        assert_eq!(rebuild_due(&quit, &[1, 2], now), None);
        // A source whose app is not running waits without a rebuild.
        let waiting = [need(1, true, true, now), need(3, false, false, now)];
        assert_eq!(rebuild_due(&waiting, &[1], now), None);
        // Its app starts: tap it.
        let started = [need(1, true, true, now), need(3, true, false, now)];
        assert_eq!(rebuild_due(&started, &[1], now), Some(now));
        assert_eq!(lanes_for_build(&started, now), vec![1, 3]);
        // A removed source's tap must go, even if its app is not running.
        assert_eq!(rebuild_due(&[need(1, true, true, now)], &[1, 2], now), Some(now));
        assert_eq!(stale_lanes(&[need(1, true, true, now)], &[1, 2]), vec![2]);
        // A failed source's tap too.
        let mut failed = need(2, true, true, now);
        failed.failed = true;
        assert_eq!(rebuild_due(&[need(1, true, true, now), failed], &[1, 2], now), Some(now));
        assert_eq!(lanes_for_build(&[need(1, true, true, now), failed], now), vec![1]);
        assert_eq!(stale_lanes(&[need(1, true, true, now), failed], &[1, 2]), vec![2]);
        // A scope change.
        let mut rescope = need(1, true, true, now);
        rescope.rescope = true;
        assert_eq!(rebuild_due(&[rescope], &[1], now), Some(now));
        // The last source removed: the engine goes, with nothing to build.
        assert_eq!(rebuild_due(&[], &[1], now), Some(now));
        assert!(lanes_for_build(&[], now).is_empty());
        // No engine and nothing to tap: nothing to do.
        assert_eq!(rebuild_due(&[], &[], now), None);
    }

    #[test]
    fn a_removed_source_whose_app_quit_waits_for_the_next_build() {
        let now = Instant::now();
        let playing = [need(1, true, true, now)];
        // Source 2 was removed while its app still runs: its tap mutes it, so it must go now.
        let lanes = binding_lanes(&playing, &[(1, true), (2, true)]);
        assert_eq!(lanes, vec![1, 2]);
        assert_eq!(rebuild_due(&playing, &lanes, now), Some(now));
        // Its app had quit: nothing to un-mute, so source 1 is not interrupted for it.
        let lanes = binding_lanes(&playing, &[(1, true), (2, false)]);
        assert_eq!(lanes, vec![1]);
        assert_eq!(rebuild_due(&playing, &lanes, now), None);
        // A current source's tap counts whether or not its app runs.
        assert_eq!(binding_lanes(&[need(1, false, true, now)], &[(1, false), (2, false)]), vec![1]);
        // Every source removed: the engine has nothing left to play and closes.
        assert_eq!(binding_lanes(&[], &[(2, false)]), vec![2]);
        assert_eq!(rebuild_due(&[], &[2], now), Some(now));
    }

    #[test]
    fn backoff_delays_attaching_a_source() {
        let now = Instant::now();
        let later = now + Duration::from_secs(2);
        let needs = [need(1, true, true, now), need(2, true, false, later)];
        assert_eq!(rebuild_due(&needs, &[1], now), Some(later));
        assert_eq!(lanes_for_build(&needs, now), vec![1], "not yet");
        assert_eq!(lanes_for_build(&needs, later), vec![1, 2]);
        // Nothing attached yet, two sources becoming ready at different times.
        let needs = [need(1, true, false, later), need(2, true, false, now)];
        assert_eq!(rebuild_due(&needs, &[], now), Some(now));
        assert_eq!(lanes_for_build(&needs, now), vec![2]);
        // Dead apps never schedule anything.
        assert_eq!(rebuild_due(&[need(1, false, false, now)], &[], now), None);
        assert!(lanes_for_build(&[need(1, false, true, now)], now).is_empty(), "nothing to tap");
    }

    #[test]
    fn a_failed_addition_does_not_disturb_the_playing_sources() {
        // A and B play; C is added.
        assert_eq!(switch_verdict(&[1, 2, 3], &[1, 2], &[1, 2, 3], false), Switch::Go);
        // C's tap failed: the new engine would be the old one again.
        assert_eq!(switch_verdict(&[1, 2, 3], &[1, 2], &[1, 2], false), Switch::NothingGained);
        // B's new tap failed: switching would leave B unprocessed.
        assert_eq!(switch_verdict(&[1, 2, 3], &[1, 2], &[1, 3], false), Switch::WouldDrop(vec![2]));
        // A re-scoped tap is the point of the build.
        assert_eq!(switch_verdict(&[1, 2], &[1, 2], &[1, 2], true), Switch::Go);
        assert_eq!(switch_verdict(&[1, 2], &[1, 2], &[2], true), Switch::WouldDrop(vec![1]));
        // A source whose app quit is left out on purpose; that is not a loss.
        assert_eq!(switch_verdict(&[1, 3], &[1, 2], &[1, 3], false), Switch::Go);
        assert_eq!(switch_verdict(&[1], &[1, 2], &[1], false), Switch::NothingGained);
    }

    #[test]
    fn states_for_the_ui() {
        use SessionState::*;
        assert_eq!(source_state(false, None, true, true), Running);
        assert_eq!(source_state(false, None, true, false), Starting);
        assert_eq!(source_state(false, None, false, true), WaitingForApp, "its app quit; the tap waits");
        assert_eq!(source_state(false, None, false, false), WaitingForApp);
        assert_eq!(source_state(false, Some("no"), true, false), Error("no".into()));
        assert_eq!(source_state(true, Some("no"), true, true), Stopped, "the session's error says it all");
        assert_eq!(session_state(Some("denied"), true, true), Error("denied".into()));
        assert_eq!(session_state(None, true, false), Running);
        assert_eq!(session_state(None, false, true), Starting);
        assert_eq!(session_state(None, false, false), WaitingForApp);
    }

    #[test]
    fn provisional_names() {
        assert_eq!(provisional_name("com.spotify.client"), "com.spotify.client");
        assert_eq!(provisional_name("/Applications/VLC.app"), "VLC");
        assert_eq!(provisional_name("/opt/homebrew/bin/mpv"), "mpv");
        assert_eq!(provisional_name(""), "");
    }
}
