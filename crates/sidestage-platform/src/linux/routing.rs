//! Routing decisions, independent of the wire protocol: which sink-inputs belong to the music
//! app, which streams must move where, where they go back to, how often a move may be retried,
//! and what the crash journal records.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::modargs::is_sidestage_sink;

/// Stream property that marks Sidestage's own streams. Our streams are recognised by this prop,
/// not by client index: every Sidestage connection is a separate client, and the playback
/// stream's index is not visible to the control connection.
pub const ROLE_KEY: &CStr = c"sidestage.role";
pub const ROLE_CAPTURE: &str = "capture";
pub const ROLE_OUTPUT: &str = "output";
pub const ROLE_METER: &str = "meter";

/// Moves of one stream that keep being undone (WirePlumber or another tool moving it back)
/// produce a warning after this many attempts.
pub const FIGHT_THRESHOLD: u32 = 4;

/// One sink-input (an app's playback stream), reduced to what the decisions need.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stream {
    pub index: u32,
    pub sink: u32,
    /// Grouping key, see [`app_key`].
    pub app_key: Option<String>,
    pub app_name: Option<String>,
    pub media_name: Option<String>,
    pub pid: Option<u32>,
    /// The `sidestage.role` prop when this is one of Sidestage's own streams.
    pub own_role: Option<String>,
    pub corked: bool,
}

impl Stream {
    pub fn is_ours(&self) -> bool {
        self.own_role.is_some()
    }

    pub fn is_app(&self, app: &str) -> bool {
        !self.is_ours() && self.app_key.as_deref() == Some(app)
    }

    /// Name for messages, e.g. "Spotify".
    pub fn label(&self) -> &str {
        self.app_name.as_deref().or(self.app_key.as_deref()).unwrap_or("An app")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sink {
    pub index: u32,
    pub name: String,
    pub description: String,
    pub monitor_source: Option<u32>,
    pub owner_module: Option<u32>,
}

impl Sink {
    /// A Sidestage capture sink (ours or a leftover), never a place to send audio to.
    pub fn is_capture(&self) -> bool {
        is_sidestage_sink(&self.name)
    }

    pub fn label(&self) -> &str {
        if self.description.is_empty() { &self.name } else { &self.description }
    }
}

/// Stable app identity: `application.process.binary`, else `application.name`, else the PID.
/// Arguments must already be trimmed and non-empty.
pub fn app_key(binary: Option<&str>, name: Option<&str>, pid: Option<u32>) -> Option<String> {
    binary.or(name).map(str::to_owned).or_else(|| pid.map(|p| format!("pid:{p}")))
}

/// An entry of the app picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppGroup {
    pub key: String,
    pub name: String,
    pub detail: String,
    pub pids: Vec<u32>,
    pub playing: bool,
}

/// Groups streams into apps, skipping Sidestage's own streams and those of `exclude_pids`
/// (Sidestage and its parents). Playing apps first, then by name.
pub fn group_apps(streams: &[Stream], exclude_pids: &BTreeSet<u32>) -> Vec<AppGroup> {
    let mut groups: BTreeMap<&str, AppGroup> = BTreeMap::new();
    for s in streams {
        if s.is_ours() || s.pid.is_some_and(|p| exclude_pids.contains(&p)) {
            continue;
        }
        let Some(key) = s.app_key.as_deref() else {
            continue;
        };
        let g = groups.entry(key).or_insert_with(|| AppGroup {
            key: key.to_owned(),
            name: s.app_name.clone().unwrap_or_else(|| key.to_owned()),
            detail: String::new(),
            pids: Vec::new(),
            playing: false,
        });
        if let Some(pid) = s.pid
            && !g.pids.contains(&pid)
        {
            g.pids.push(pid);
        }
        let playing = !s.corked;
        // Prefer the title of something that is actually playing.
        if let Some(media) = &s.media_name
            && (g.detail.is_empty() || (playing && !g.playing))
        {
            g.detail.clone_from(media);
        }
        g.playing |= playing;
    }
    let mut apps: Vec<AppGroup> = groups.into_values().collect();
    apps.sort_by(|a, b| b.playing.cmp(&a.playing).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    apps
}

/// Where a stream leaving the capture sink goes: the server default, unless that is a Sidestage
/// sink (the real device vanished), then `fallback`, then any real sink.
pub fn home_sink<'a>(sinks: &'a [Sink], default: Option<&str>, fallback: Option<&str>) -> Option<&'a Sink> {
    let real = |name: &str| sinks.iter().find(|s| s.name == name && !s.is_capture());
    default.and_then(real).or_else(|| fallback.and_then(real)).or_else(|| sinks.iter().find(|s| !s.is_capture()))
}

/// A stream Sidestage moved onto its capture sink, and how to put it back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moved {
    /// Sink-input index (only meaningful while the server that issued it keeps running).
    pub index: u32,
    /// App name for messages.
    pub app: String,
    pub original_sink: String,
    /// It was on the default sink. Such streams go back to the *current* default, which also
    /// clears the route the move saved (PulseAudio 14+ and WirePlumber both treat a move to the
    /// default as "not routed").
    pub was_default: bool,
}

/// The crash journal (`linux-routing.json`). Written before the null sink is loaded and before
/// every batch of moves, so the watchdog or the next launch can always undo them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingJournal {
    /// Sidestage process that owns the routing.
    pub pid: u32,
    /// Random per-session ID; a watchdog only acts on the session it guards.
    pub session: String,
    /// Our capture sink. Restoring finds the module through the sink's owner, not by index,
    /// because module indices restart when the server restarts.
    pub sink_name: String,
    pub module_index: Option<u32>,
    #[serde(default)]
    pub streams: Vec<Moved>,
}

/// One change [`plan`] wants made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Move one of the music app's streams onto the capture sink. Journal it first.
    Capture(Moved),
    /// The app's stream is already on the capture sink (stream-restore or WirePlumber put it
    /// there because our earlier move was saved); remember it so it is moved off on stop.
    Adopt(Moved),
    /// Move a stream off the capture sink: another app routed there, or Sidestage's own output,
    /// which would feed back into the capture.
    Evict { index: u32, to: String },
}

/// Server state a [`plan`] is made from.
pub struct World<'a> {
    pub streams: &'a [Stream],
    pub sinks: &'a [Sink],
    pub capture_sink: u32,
    pub default_sink: Option<&'a str>,
    /// Last known real default, used when the default currently points at a Sidestage sink.
    pub fallback_sink: Option<&'a str>,
}

/// Decides the moves that bring the server to the wanted state. Idempotent: planning again on
/// the resulting state yields nothing, and `limiter` keeps it from fighting another router.
pub fn plan(
    world: &World<'_>,
    app: &str,
    tracked: &BTreeMap<u32, Moved>,
    unmovable: &BTreeSet<u32>,
    limiter: &MoveLimiter,
    now: Instant,
) -> Vec<Action> {
    let home = home_sink(world.sinks, world.default_sink, world.fallback_sink);
    // A stream that arrives on the capture sink by itself was put there by the route our move
    // of a sibling saved (stream-restore and WirePlumber keep one route per app). Without
    // Sidestage it would have followed that sibling's route, so it inherits the sibling's
    // origin, an explicit (non-default) one first: restoring it to the default instead would
    // clear the app's saved route to that output.
    let adopted_origin = tracked
        .values()
        .find(|m| !m.was_default)
        .or_else(|| tracked.values().next())
        .map(|m| (m.original_sink.clone(), m.was_default))
        .or_else(|| home.map(|h| (h.name.clone(), true)));
    let mut actions = Vec::new();
    for s in world.streams {
        let on_capture = s.sink == world.capture_sink;
        if s.is_app(app) {
            if on_capture {
                if !tracked.contains_key(&s.index)
                    && let Some((original_sink, was_default)) = adopted_origin.clone()
                {
                    actions.push(Action::Adopt(Moved {
                        index: s.index,
                        app: s.label().to_owned(),
                        original_sink,
                        was_default,
                    }));
                }
            } else if !unmovable.contains(&s.index) && limiter.allow(s.index, now) {
                // A stream we moved before keeps its first origin; otherwise it starts here.
                let entry = match tracked.get(&s.index) {
                    Some(m) => m.clone(),
                    None => {
                        let Some(from) = world.sinks.iter().find(|k| k.index == s.sink) else {
                            continue;
                        };
                        if from.is_capture() {
                            continue; // another Sidestage sink: a leftover being swept
                        }
                        Moved {
                            index: s.index,
                            app: s.label().to_owned(),
                            original_sink: from.name.clone(),
                            was_default: world.default_sink == Some(from.name.as_str()),
                        }
                    }
                };
                actions.push(Action::Capture(entry));
            }
        } else if on_capture
            && limiter.allow(s.index, now)
            && let Some(home) = home
        {
            actions.push(Action::Evict { index: s.index, to: home.name.clone() });
        }
    }
    actions
}

/// Where each stream on the capture sink goes when Sidestage stops: back to its original sink
/// if it was not on the default and that sink still exists, else to the current default (or,
/// if the default is a Sidestage sink, any real sink).
///
/// Every move is saved per app, and the last move of an app's streams decides its saved route.
/// Moves back to an explicit original therefore come last, so an app the user had routed to a
/// particular output keeps that route even if one of its streams goes to the default.
pub fn plan_restore(
    streams: &[Stream],
    capture_sink: u32,
    sinks: &[Sink],
    tracked: &[Moved],
    default: Option<&str>,
) -> Vec<(u32, String)> {
    let home = home_sink(sinks, default, None);
    let mut moves: Vec<(u32, String, bool)> = streams
        .iter()
        .filter(|s| s.sink == capture_sink)
        .filter_map(|s| {
            let original = tracked
                .iter()
                .find(|m| m.index == s.index && !m.was_default)
                .and_then(|m| sinks.iter().find(|k| k.name == m.original_sink && !k.is_capture()));
            match original {
                Some(k) => Some((s.index, k.name.clone(), true)),
                None => home.map(|k| (s.index, k.name.clone(), false)),
            }
        })
        .collect();
    moves.sort_by_key(|&(_, _, explicit)| explicit);
    moves.into_iter().map(|(index, sink, _)| (index, sink)).collect()
}

/// Per-stream retry backoff. pipewire-pulse acknowledges a move before WirePlumber relinks the
/// stream, and stream-restore or WirePlumber may move a stream right back, so every move of a
/// stream pushes its next allowed move further out (0.5 s, 1 s, 2 s, ... up to 30 s) instead of
/// ping-ponging on every event.
#[derive(Debug, Default)]
pub struct MoveLimiter {
    entries: BTreeMap<u32, Attempts>,
}

#[derive(Debug, Clone, Copy)]
struct Attempts {
    count: u32,
    next: Instant,
}

impl MoveLimiter {
    const FIRST_RETRY: Duration = Duration::from_millis(500);
    const MAX_RETRY: Duration = Duration::from_secs(30);

    pub fn allow(&self, index: u32, now: Instant) -> bool {
        self.entries.get(&index).is_none_or(|a| now >= a.next)
    }

    /// Records a move of `index`; returns how many moves of it have been made.
    pub fn record(&mut self, index: u32, now: Instant) -> u32 {
        let a = self.entries.entry(index).or_insert(Attempts { count: 0, next: now });
        a.count += 1;
        let backoff = Self::FIRST_RETRY.saturating_mul(1 << (a.count - 1).min(16)).min(Self::MAX_RETRY);
        a.next = now + backoff;
        a.count
    }

    /// Forgets streams that no longer exist.
    pub fn retain(&mut self, keep: impl Fn(u32) -> bool) {
        self.entries.retain(|&i, _| keep(i));
    }
}

/// Sink-inputs whose level feeds the ducking sidechain: everything on the sink the user listens
/// to (`output_sink`), except Sidestage's streams and the music app's.
pub fn meter_targets(streams: &[Stream], output_sink: u32, app: &str) -> Vec<u32> {
    streams.iter().filter(|s| s.sink == output_sink && !s.is_ours() && !s.is_app(app)).map(|s| s.index).collect()
}

/// The game level: the loudest meter that reported within `hold`. Meters of paused streams stop
/// reporting, so their last value must not stick.
pub fn combined_peak(meters: impl IntoIterator<Item = (f32, Instant)>, now: Instant, hold: Duration) -> f32 {
    meters.into_iter().filter(|&(_, at)| now.saturating_duration_since(at) <= hold).fold(0.0, |m, (v, _)| m.max(v))
}

/// How the music app's streams are doing, for the session status.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AppState {
    pub streams: usize,
    pub captured: usize,
    pub name: Option<String>,
}

pub fn app_state(streams: &[Stream], capture_sink: u32, app: &str) -> AppState {
    let mut state = AppState::default();
    for s in streams.iter().filter(|s| s.is_app(app)) {
        state.streams += 1;
        if s.sink == capture_sink {
            state.captured += 1;
        }
        if state.name.is_none() {
            state.name = s.app_name.clone();
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u32 = 50;

    fn sink(index: u32, name: &str) -> Sink {
        Sink {
            index,
            name: name.into(),
            description: format!("{name} desc"),
            monitor_source: Some(index + 100),
            owner_module: Some(index + 200),
        }
    }

    fn sinks() -> Vec<Sink> {
        vec![sink(1, "speakers"), sink(2, "headset"), sink(CAP, "sidestage_music_abc")]
    }

    fn app_stream(index: u32, sink: u32, key: &str) -> Stream {
        Stream {
            index,
            sink,
            app_key: Some(key.into()),
            app_name: Some(key.to_uppercase()),
            media_name: Some(format!("{key} media {index}")),
            pid: Some(index * 10),
            own_role: None,
            corked: false,
        }
    }

    fn ours(index: u32, sink: u32) -> Stream {
        Stream {
            index,
            sink,
            own_role: Some(ROLE_OUTPUT.into()),
            app_key: Some("sidestage".into()),
            ..Default::default()
        }
    }

    fn world<'a>(streams: &'a [Stream], sinks: &'a [Sink], default: &'a str) -> World<'a> {
        World { streams, sinks, capture_sink: CAP, default_sink: Some(default), fallback_sink: None }
    }

    #[test]
    fn app_keys_fall_back_in_order() {
        assert_eq!(app_key(Some("spotify"), Some("Spotify"), Some(3)).as_deref(), Some("spotify"));
        assert_eq!(app_key(None, Some("Spotify"), Some(3)).as_deref(), Some("Spotify"));
        assert_eq!(app_key(None, None, Some(3)).as_deref(), Some("pid:3"));
        assert_eq!(app_key(None, None, None), None);
    }

    #[test]
    fn groups_apps_and_skips_own_streams() {
        let mut paused = app_stream(4, 1, "firefox");
        paused.corked = true;
        let mut tab = app_stream(5, 1, "firefox");
        tab.pid = Some(40); // same process as `paused`
        let streams = vec![
            paused,
            tab,
            app_stream(6, CAP, "spotify"),
            ours(7, 1),
            app_stream(8, 1, "konsole"),
            Stream { index: 9, sink: 1, ..Default::default() },
        ];
        let exclude = BTreeSet::from([80]);
        let apps = group_apps(&streams, &exclude);
        let keys: Vec<&str> = apps.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, ["firefox", "spotify"], "playing first, by name; own, parents and keyless skipped");
        let ff = &apps[0];
        assert!(ff.playing);
        assert_eq!(ff.pids, [40]);
        assert_eq!(ff.detail, "firefox media 5", "detail comes from the playing stream");
        assert_eq!(ff.name, "FIREFOX");
    }

    #[test]
    fn captures_app_and_evicts_others() {
        let s = sinks();
        let streams = vec![
            app_stream(1, 1, "spotify"),
            app_stream(2, 2, "spotify"),
            app_stream(3, CAP, "game"),
            ours(4, CAP),
            app_stream(5, 1, "game"),
        ];
        let actions = plan(
            &world(&streams, &s, "speakers"),
            "spotify",
            &BTreeMap::new(),
            &BTreeSet::new(),
            &MoveLimiter::default(),
            Instant::now(),
        );
        assert_eq!(
            actions,
            [
                Action::Capture(Moved {
                    index: 1,
                    app: "SPOTIFY".into(),
                    original_sink: "speakers".into(),
                    was_default: true
                }),
                Action::Capture(Moved {
                    index: 2,
                    app: "SPOTIFY".into(),
                    original_sink: "headset".into(),
                    was_default: false
                }),
                Action::Evict { index: 3, to: "speakers".into() },
                Action::Evict { index: 4, to: "speakers".into() },
            ]
        );
    }

    #[test]
    fn plan_is_idempotent_and_adopts_restored_streams() {
        let s = sinks();
        let streams = vec![app_stream(1, CAP, "spotify"), app_stream(2, CAP, "spotify"), ours(3, 1)];
        let mut tracked = BTreeMap::new();
        tracked
            .insert(1, Moved { index: 1, app: "SPOTIFY".into(), original_sink: "headset".into(), was_default: false });
        let actions = plan(
            &world(&streams, &s, "speakers"),
            "spotify",
            &tracked,
            &BTreeSet::new(),
            &MoveLimiter::default(),
            Instant::now(),
        );
        assert_eq!(
            actions,
            [Action::Adopt(Moved {
                index: 2,
                app: "SPOTIFY".into(),
                original_sink: "headset".into(),
                was_default: false
            })],
            "stream 2 was put on the capture sink by the route stream 1's move saved; it \
             inherits stream 1's origin and only it needs recording"
        );

        let nothing_tracked = plan(
            &world(&streams, &s, "speakers"),
            "spotify",
            &BTreeMap::new(),
            &BTreeSet::new(),
            &MoveLimiter::default(),
            Instant::now(),
        );
        assert_eq!(
            nothing_tracked[0],
            Action::Adopt(Moved {
                index: 1,
                app: "SPOTIFY".into(),
                original_sink: "speakers".into(),
                was_default: true
            }),
            "without a sibling to inherit from, adopted streams belong to the default"
        );
    }

    #[test]
    fn recaptures_keep_first_origin_and_respect_limits() {
        let s = sinks();
        let streams = vec![app_stream(1, 1, "spotify"), app_stream(2, 2, "spotify")];
        let mut tracked = BTreeMap::new();
        tracked
            .insert(1, Moved { index: 1, app: "SPOTIFY".into(), original_sink: "headset".into(), was_default: false });
        let now = Instant::now();
        let mut limiter = MoveLimiter::default();
        limiter.record(1, now);
        let unmovable = BTreeSet::from([2]);
        let w = world(&streams, &s, "speakers");
        assert!(plan(&w, "spotify", &tracked, &unmovable, &limiter, now).is_empty(), "debounced and unmovable");
        let later = now + Duration::from_millis(600);
        assert_eq!(
            plan(&w, "spotify", &tracked, &unmovable, &limiter, later),
            [Action::Capture(tracked[&1].clone())],
            "moved back by someone else: try again, still remembering the first origin"
        );
    }

    #[test]
    fn default_on_capture_sink_falls_back_to_real_sink() {
        let s = sinks();
        let streams = vec![ours(4, CAP)];
        let w = World {
            streams: &streams,
            sinks: &s,
            capture_sink: CAP,
            default_sink: Some("sidestage_music_abc"),
            fallback_sink: Some("headset"),
        };
        let actions = plan(&w, "spotify", &BTreeMap::new(), &BTreeSet::new(), &MoveLimiter::default(), Instant::now());
        assert_eq!(actions, [Action::Evict { index: 4, to: "headset".into() }]);
        assert_eq!(home_sink(&s, Some("gone"), None).map(|k| k.name.as_str()), Some("speakers"));
        assert_eq!(home_sink(&s[2..], None, None), None, "only Sidestage sinks: nowhere to go");
    }

    #[test]
    fn restore_goes_to_original_or_current_default() {
        let s = sinks();
        let streams = vec![
            app_stream(1, CAP, "spotify"),
            app_stream(2, CAP, "spotify"),
            app_stream(3, CAP, "spotify"),
            app_stream(4, CAP, "other"),
            app_stream(5, 1, "spotify"),
        ];
        let tracked = vec![
            Moved { index: 1, app: "S".into(), original_sink: "headset".into(), was_default: false },
            Moved { index: 2, app: "S".into(), original_sink: "speakers".into(), was_default: true },
            Moved { index: 3, app: "S".into(), original_sink: "unplugged".into(), was_default: false },
        ];
        let moves = plan_restore(&streams, CAP, &s, &tracked, Some("headset"));
        assert_eq!(
            moves,
            [(2, "headset".into()), (3, "headset".into()), (4, "headset".into()), (1, "headset".into())],
            "default-origin streams follow the *current* default; missing originals fall back \
             to it; explicit origins go last"
        );
        let moves = plan_restore(&streams, CAP, &s, &tracked, Some("speakers"));
        assert_eq!(
            moves,
            [(2, "speakers".into()), (3, "speakers".into()), (4, "speakers".into()), (1, "headset".into())],
            "non-default origin restored even if the default changed, and last, so the app's \
             saved route ends up pointing at it"
        );
    }

    #[test]
    fn limiter_backs_off_exponentially() {
        let mut l = MoveLimiter::default();
        let t0 = Instant::now();
        assert!(l.allow(7, t0));
        assert_eq!(l.record(7, t0), 1);
        assert!(!l.allow(7, t0 + Duration::from_millis(499)));
        assert!(l.allow(7, t0 + Duration::from_millis(500)));
        assert_eq!(l.record(7, t0), 2);
        assert!(!l.allow(7, t0 + Duration::from_millis(999)));
        assert!(l.allow(7, t0 + Duration::from_secs(1)));
        for _ in 0..30 {
            l.record(7, t0);
        }
        assert!(l.allow(7, t0 + Duration::from_secs(30)), "capped at 30 s");
        l.retain(|i| i != 7);
        assert!(l.allow(7, t0));
    }

    #[test]
    fn meters_cover_everything_else_on_the_output() {
        let streams = vec![
            app_stream(1, 1, "game"),
            app_stream(2, 1, "spotify"),
            ours(3, 1),
            app_stream(4, 2, "voip"),
            app_stream(5, CAP, "spotify"),
        ];
        assert_eq!(meter_targets(&streams, 1, "spotify"), [1]);
    }

    #[test]
    fn combined_peak_ignores_stale_meters() {
        let old = Instant::now();
        let now = old + Duration::from_millis(200);
        let hold = Duration::from_millis(150);
        assert_eq!(combined_peak([(0.3, now), (0.9, old), (0.5, now)], now, hold), 0.5);
        assert_eq!(combined_peak([(0.9, old)], old + hold, hold), 0.9);
        assert_eq!(combined_peak([], now, hold), 0.0);
    }

    #[test]
    fn app_state_counts_captured_streams() {
        let streams = vec![app_stream(1, CAP, "spotify"), app_stream(2, 1, "spotify"), app_stream(3, 1, "game")];
        assert_eq!(
            app_state(&streams, CAP, "spotify"),
            AppState { streams: 2, captured: 1, name: Some("SPOTIFY".into()) }
        );
    }

    #[test]
    fn journal_roundtrips_and_tolerates_missing_streams() {
        let j = RoutingJournal {
            pid: 1,
            session: "s".into(),
            sink_name: "sidestage_music_abc".into(),
            module_index: Some(3),
            streams: vec![Moved {
                index: 1,
                app: "Spotify".into(),
                original_sink: "speakers".into(),
                was_default: true,
            }],
        };
        let text = serde_json::to_string(&j).unwrap();
        assert_eq!(serde_json::from_str::<RoutingJournal>(&text).unwrap(), j);
        let minimal: RoutingJournal =
            serde_json::from_str(r#"{"pid":1,"session":"s","sink_name":"x","module_index":null}"#).unwrap();
        assert!(minimal.streams.is_empty());
    }
}
