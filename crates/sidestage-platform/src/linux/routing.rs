//! Routing decisions, independent of the wire protocol: which sink-inputs belong to which
//! source, which streams must move where, where they go back to, how often a move may be
//! retried, which streams count as the game, and what the crash journal records.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CStr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::modargs::is_sidestage_sink;
use crate::{AppKey, SourceId};

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
    pub app_key: Option<AppKey>,
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

    /// Whether this is a stream of `app`. Keys are compared with [`AppKey::same_app`], the rule
    /// the app uses to find a saved app, so sources and the ignore list match the same streams.
    pub fn is_app(&self, app: &AppKey) -> bool {
        !self.is_ours() && self.app_key.as_ref().is_some_and(|k| k.same_app(app))
    }

    /// Name for messages, e.g. "Spotify".
    pub fn label(&self) -> &str {
        self.app_name.as_deref().or(self.app_key.as_ref().map(|k| k.0.as_str())).unwrap_or("An app")
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
pub fn app_key(binary: Option<&str>, name: Option<&str>, pid: Option<u32>) -> Option<AppKey> {
    binary.or(name).map(str::to_owned).or_else(|| pid.map(|p| format!("pid:{p}"))).map(AppKey)
}

/// An entry of the app picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppGroup {
    pub key: AppKey,
    pub name: String,
    pub detail: String,
    pub pids: Vec<u32>,
    pub playing: bool,
}

/// Groups streams into apps, skipping Sidestage's own streams and those of `exclude_pids`
/// (Sidestage and its parents). Apps that are sources right now are included: their streams are
/// listed wherever they play. Playing apps first, then by name.
pub fn group_apps(streams: &[Stream], exclude_pids: &BTreeSet<u32>) -> Vec<AppGroup> {
    let mut groups: BTreeMap<&AppKey, AppGroup> = BTreeMap::new();
    for s in streams {
        if s.is_ours() || s.pid.is_some_and(|p| exclude_pids.contains(&p)) {
            continue;
        }
        let Some(key) = s.app_key.as_ref() else {
            continue;
        };
        let g = groups.entry(key).or_insert_with(|| AppGroup {
            key: key.clone(),
            name: s.app_name.clone().unwrap_or_else(|| key.0.clone()),
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

/// Where a stream leaving a capture sink goes: the server default, unless that is a Sidestage
/// sink (the real device vanished), then `fallback`, then any real sink.
pub fn home_sink<'a>(sinks: &'a [Sink], default: Option<&str>, fallback: Option<&str>) -> Option<&'a Sink> {
    let real = |name: &str| sinks.iter().find(|s| s.name == name && !s.is_capture());
    default.and_then(real).or_else(|| fallback.and_then(real)).or_else(|| sinks.iter().find(|s| !s.is_capture()))
}

/// A stream Sidestage moved onto a capture sink, and how to put it back.
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

/// The crash journal (`linux-routing.json`). Written before every capture sink is loaded,
/// before every batch of moves and after every sink is gone again, so the watchdog or the next
/// launch can always undo them. One session owns it; it lists one record per capture sink.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "JournalFile")]
pub struct RoutingJournal {
    /// Sidestage process that owns the routing.
    pub pid: u32,
    /// Random per-session ID; a watchdog only acts on the session it guards.
    pub session: String,
    pub sinks: Vec<SinkRecord>,
}

/// One capture sink: a source's, or one whose undo has not finished yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkRecord {
    /// Restoring finds the module through the sink's owner or its arguments, not by the index
    /// below, because module indices restart when the server restarts.
    pub sink_name: String,
    #[serde(default)]
    pub module_index: Option<u32>,
    #[serde(default)]
    pub streams: Vec<Moved>,
}

/// What the file may hold: the current layout, or the single-sink layout of Sidestage 0.1,
/// which a crashed 0.1 run can leave for this version's recovery.
#[derive(Deserialize)]
struct JournalFile {
    pid: u32,
    session: String,
    #[serde(default)]
    sinks: Vec<SinkRecord>,
    #[serde(default)]
    sink_name: Option<String>,
    #[serde(default)]
    module_index: Option<u32>,
    #[serde(default)]
    streams: Vec<Moved>,
}

impl From<JournalFile> for RoutingJournal {
    fn from(f: JournalFile) -> Self {
        let mut sinks = f.sinks;
        if let Some(sink_name) = f.sink_name.filter(|n| !n.is_empty())
            && !sinks.iter().any(|s| s.sink_name == sink_name)
        {
            sinks.push(SinkRecord { sink_name, module_index: f.module_index, streams: f.streams });
        }
        Self { pid: f.pid, session: f.session, sinks }
    }
}

/// One source whose capture sink exists, as [`plan`] sees it.
pub struct Route<'a> {
    pub source: SourceId,
    pub app: &'a AppKey,
    /// Index of the source's capture sink.
    pub sink: u32,
    /// The app's streams this source moved (or adopted), by sink-input index.
    pub tracked: &'a BTreeMap<u32, Moved>,
    /// Streams the server refused to move.
    pub unmovable: &'a BTreeSet<u32>,
    /// The sink's monitor is being recorded and played. Streams are only moved onto a sink
    /// someone listens to, so an app is never parked in silence.
    pub listening: bool,
}

/// One change [`plan`] wants made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Move one of a source app's streams onto that source's capture sink. Journal it first.
    Capture { source: SourceId, moved: Moved },
    /// The app's stream is already on its source's capture sink (stream-restore or WirePlumber
    /// put it there because an earlier move was saved); remember it so it is moved off on stop.
    Adopt { source: SourceId, moved: Moved },
    /// Move a stream off a capture sink: an app that is not that sink's source (stream-restore
    /// keys routes by media role, so another app with the same role can be routed there), or
    /// Sidestage's own output, which would feed back into the capture.
    Evict { index: u32, to: String },
}

/// Server state a [`plan`] is made from.
pub struct World<'a> {
    pub streams: &'a [Stream],
    pub sinks: &'a [Sink],
    pub default_sink: Option<&'a str>,
    /// Last known real default, used when the default currently points at a Sidestage sink.
    pub fallback_sink: Option<&'a str>,
    /// The output the user chose for Sidestage (`None`: the default). Sidestage's own output
    /// goes back there when something puts it on a capture sink, rather than to the default.
    pub output_sink: Option<&'a str>,
}

/// Decides the moves that bring the server to the wanted state: every source's app on its own
/// capture sink and nothing else on any of them. Idempotent: planning again on the resulting
/// state yields nothing, and `limiter` keeps it from fighting another router.
pub fn plan(world: &World<'_>, routes: &[Route<'_>], limiter: &MoveLimiter, now: Instant) -> Vec<Action> {
    let home = home_sink(world.sinks, world.default_sink, world.fallback_sink);
    let chosen = world.output_sink.and_then(|name| world.sinks.iter().find(|k| k.name == name && !k.is_capture()));
    let own_home = chosen.or(home);
    let mut actions = Vec::new();
    for s in world.streams {
        // On one of this session's capture sinks (leftovers of other runs are swept, not routed).
        let on_ours = routes.iter().any(|r| r.sink == s.sink);
        let owner = routes.iter().find(|r| s.is_app(r.app));
        if let Some(r) = owner {
            if s.sink == r.sink {
                if !r.tracked.contains_key(&s.index)
                    && let Some((original_sink, was_default)) = adopted_origin(r, home)
                {
                    let moved = Moved { index: s.index, app: s.label().to_owned(), original_sink, was_default };
                    actions.push(Action::Adopt { source: r.source, moved });
                }
                continue;
            }
            if !limiter.allow(s.index, now) {
                continue;
            }
            if r.listening && !r.unmovable.contains(&s.index) {
                // A stream moved before keeps its first origin. One that another source's sink
                // received by a saved route was never really there: it inherits like an adopted
                // stream. Otherwise it starts where it plays now.
                let moved = match r.tracked.get(&s.index) {
                    Some(m) => m.clone(),
                    None => {
                        let Some(from) = world.sinks.iter().find(|k| k.index == s.sink) else {
                            continue;
                        };
                        if on_ours {
                            let Some((original_sink, was_default)) = adopted_origin(r, home) else {
                                continue;
                            };
                            Moved { index: s.index, app: s.label().to_owned(), original_sink, was_default }
                        } else if from.is_capture() {
                            continue; // a leftover Sidestage sink being swept
                        } else {
                            Moved {
                                index: s.index,
                                app: s.label().to_owned(),
                                original_sink: from.name.clone(),
                                was_default: world.default_sink == Some(from.name.as_str()),
                            }
                        }
                    }
                };
                actions.push(Action::Capture { source: r.source, moved });
            } else if on_ours && let Some(home) = home {
                // Its own source cannot take it yet, and on another source's sink it would be
                // rendered as that source.
                actions.push(Action::Evict { index: s.index, to: home.name.clone() });
            }
        } else if on_ours
            && limiter.allow(s.index, now)
            && let Some(to) = if s.is_ours() { own_home } else { home }
        {
            // Sidestage's own output here would feed back into the capture (the engine mutes
            // it meanwhile, see `audio`).
            actions.push(Action::Evict { index: s.index, to: to.name.clone() });
        }
    }
    actions
}

/// The origin a stream inherits when it reaches `route`'s sink without being moved there by
/// Sidestage: it was put there by the route a move of a sibling saved (stream-restore and
/// WirePlumber keep one route per app), so without Sidestage it would have followed that
/// sibling's route. An explicit (non-default) origin first: restoring it to the default instead
/// would clear the app's saved route to that output. Without a sibling: the default.
fn adopted_origin(route: &Route<'_>, home: Option<&Sink>) -> Option<(String, bool)> {
    route
        .tracked
        .values()
        .find(|m| !m.was_default)
        .or_else(|| route.tracked.values().next())
        .map(|m| (m.original_sink.clone(), m.was_default))
        .or_else(|| home.map(|h| (h.name.clone(), true)))
}

/// Where each stream on the capture sink goes when its source stops: back to its original sink
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

/// Sink-inputs whose level feeds the ducking sidechain (the game): everything on the sink the
/// user listens to (`output_sink`) except Sidestage's own streams (by role and by `own_pid`)
/// and the streams of `not_game`, the sources' apps plus the user's ignore list.
pub fn meter_targets(streams: &[Stream], output_sink: u32, not_game: &[&AppKey], own_pid: u32) -> Vec<u32> {
    streams
        .iter()
        .filter(|s| {
            s.sink == output_sink && !s.is_ours() && s.pid != Some(own_pid) && !not_game.iter().any(|k| s.is_app(k))
        })
        .map(|s| s.index)
        .collect()
}

/// The game level: the loudest meter that reported within `hold`. Meters of paused streams stop
/// reporting, so their last value must not stick.
pub fn combined_peak(meters: impl IntoIterator<Item = (f32, Instant)>, now: Instant, hold: Duration) -> f32 {
    meters.into_iter().filter(|&(_, at)| now.saturating_duration_since(at) <= hold).fold(0.0, |m, (v, _)| m.max(v))
}

/// How a source app's streams are doing, for its status.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AppState {
    pub streams: usize,
    pub captured: usize,
    pub name: Option<String>,
}

/// `capture_sink` is the source's sink index, or `None` while it has none.
pub fn app_state(streams: &[Stream], capture_sink: Option<u32>, app: &AppKey) -> AppState {
    let mut state = AppState::default();
    for s in streams.iter().filter(|s| s.is_app(app)) {
        state.streams += 1;
        if Some(s.sink) == capture_sink {
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
    const CAP2: u32 = 51;

    fn key(k: &str) -> AppKey {
        AppKey(k.into())
    }

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
        vec![
            sink(1, "speakers"),
            sink(2, "headset"),
            sink(CAP, "sidestage_music_abc"),
            sink(CAP2, "sidestage_music_def"),
        ]
    }

    fn app_stream(index: u32, sink: u32, key: &str) -> Stream {
        Stream {
            index,
            sink,
            app_key: Some(AppKey(key.into())),
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
            app_key: Some(AppKey("sidestage".into())),
            ..Default::default()
        }
    }

    fn world<'a>(streams: &'a [Stream], sinks: &'a [Sink], default: &'a str) -> World<'a> {
        World { streams, sinks, default_sink: Some(default), fallback_sink: None, output_sink: None }
    }

    fn moved(index: u32, app: &str, original: &str, was_default: bool) -> Moved {
        Moved { index, app: app.into(), original_sink: original.into(), was_default }
    }

    /// Owns what the routes borrow.
    struct Src {
        id: SourceId,
        app: AppKey,
        sink: u32,
        tracked: BTreeMap<u32, Moved>,
        unmovable: BTreeSet<u32>,
        listening: bool,
    }

    fn src(id: SourceId, app: &str, sink: u32) -> Src {
        Src { id, app: key(app), sink, tracked: BTreeMap::new(), unmovable: BTreeSet::new(), listening: true }
    }

    fn routes(srcs: &[Src]) -> Vec<Route<'_>> {
        srcs.iter()
            .map(|s| Route {
                source: s.id,
                app: &s.app,
                sink: s.sink,
                tracked: &s.tracked,
                unmovable: &s.unmovable,
                listening: s.listening,
            })
            .collect()
    }

    fn plan_now(w: &World<'_>, srcs: &[Src]) -> Vec<Action> {
        plan(w, &routes(srcs), &MoveLimiter::default(), Instant::now())
    }

    #[test]
    fn app_keys_fall_back_in_order() {
        assert_eq!(app_key(Some("spotify"), Some("Spotify"), Some(3)), Some(key("spotify")));
        assert_eq!(app_key(None, Some("Spotify"), Some(3)), Some(key("Spotify")));
        assert_eq!(app_key(None, None, Some(3)), Some(key("pid:3")));
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
        let keys: Vec<&str> = apps.iter().map(|a| a.key.0.as_str()).collect();
        assert_eq!(keys, ["firefox", "spotify"], "playing first, by name; own, parents and keyless skipped");
        let ff = &apps[0];
        assert!(ff.playing);
        assert_eq!(ff.pids, [40]);
        assert_eq!(ff.detail, "firefox media 5", "detail comes from the playing stream");
        assert_eq!(ff.name, "FIREFOX");
    }

    #[test]
    fn captures_each_app_onto_its_own_sink_and_evicts_others() {
        let s = sinks();
        let streams = vec![
            app_stream(1, 1, "spotify"),
            app_stream(2, 2, "spotify"),
            app_stream(3, CAP, "game"),
            ours(4, CAP),
            app_stream(5, 1, "game"),
            app_stream(6, 2, "discord"),
            ours(7, CAP2),
        ];
        let srcs = [src(1, "spotify", CAP), src(2, "discord", CAP2)];
        assert_eq!(
            plan_now(&world(&streams, &s, "speakers"), &srcs),
            [
                Action::Capture { source: 1, moved: moved(1, "SPOTIFY", "speakers", true) },
                Action::Capture { source: 1, moved: moved(2, "SPOTIFY", "headset", false) },
                Action::Evict { index: 3, to: "speakers".into() },
                Action::Evict { index: 4, to: "speakers".into() },
                Action::Capture { source: 2, moved: moved(6, "DISCORD", "headset", false) },
                Action::Evict { index: 7, to: "speakers".into() },
            ],
            "the game stays where it is unless it lands on a capture sink; Sidestage's output \
             is never on one"
        );
    }

    #[test]
    fn stream_on_another_sources_sink_moves_to_its_own() {
        // stream-restore keys routes by media role: discord's stream can inherit the route
        // spotify's move saved and land on spotify's sink.
        let s = sinks();
        let streams = vec![app_stream(1, CAP, "spotify"), app_stream(2, CAP, "discord")];
        let mut spotify = src(1, "spotify", CAP);
        spotify.tracked.insert(1, moved(1, "SPOTIFY", "headset", false));
        let mut discord = src(2, "discord", CAP2);
        discord.tracked.insert(9, moved(9, "DISCORD", "speakers", true));
        let actions = plan_now(&world(&streams, &s, "speakers"), &[spotify, discord]);
        assert_eq!(
            actions,
            [Action::Capture { source: 2, moved: moved(2, "DISCORD", "speakers", true) }],
            "it inherits its own sibling's origin, not spotify's"
        );

        let mut discord = src(2, "discord", CAP2);
        discord.listening = false;
        let actions = plan_now(&world(&streams, &s, "speakers"), &[src(1, "spotify", CAP), discord]);
        assert_eq!(
            actions,
            [
                Action::Adopt { source: 1, moved: moved(1, "SPOTIFY", "speakers", true) },
                Action::Evict { index: 2, to: "speakers".into() },
            ],
            "while its own source is not listening it must not be rendered as spotify"
        );
    }

    #[test]
    fn only_listening_sources_take_streams() {
        let s = sinks();
        let streams = vec![app_stream(1, 1, "spotify"), app_stream(2, CAP, "spotify")];
        let mut spotify = src(1, "spotify", CAP);
        spotify.listening = false;
        assert_eq!(
            plan_now(&world(&streams, &s, "speakers"), &[spotify]),
            [Action::Adopt { source: 1, moved: moved(2, "SPOTIFY", "speakers", true) }],
            "a stream already on the sink is recorded, a new one waits for the capture"
        );
    }

    #[test]
    fn sources_without_a_sink_and_leftover_sinks_are_left_alone() {
        let s = sinks();
        // CAP2 is not one of this session's sinks: a leftover being swept.
        let streams = vec![app_stream(1, CAP2, "spotify"), app_stream(2, CAP2, "game"), app_stream(3, 1, "discord")];
        assert_eq!(
            plan_now(&world(&streams, &s, "speakers"), &[src(1, "spotify", CAP)]),
            [],
            "discord's source has no sink yet, and nothing is taken from a foreign sink"
        );
    }

    #[test]
    fn an_app_whose_source_has_no_sink_is_not_rendered_as_another_source() {
        // Discord's source is still setting up its sink (or failed); stream-restore put its new
        // stream on spotify's sink, where it would come out at spotify's position.
        let s = sinks();
        let streams = vec![app_stream(1, CAP, "spotify"), app_stream(2, CAP, "discord")];
        let mut spotify = src(1, "spotify", CAP);
        spotify.tracked.insert(1, moved(1, "SPOTIFY", "speakers", true));
        assert_eq!(
            plan_now(&world(&streams, &s, "headset"), &[spotify]),
            [Action::Evict { index: 2, to: "headset".into() }],
            "it goes to the current default until its own source can take it"
        );
    }

    #[test]
    fn three_sources_settle_in_one_plan() {
        // Music behind you, a podcast on the left, voice chat on the right, and the game.
        let mut s = sinks();
        const CAP3: u32 = 52;
        s.push(sink(CAP3, "sidestage_music_ghi"));
        let mut streams = vec![
            app_stream(1, 1, "spotify"),
            app_stream(2, 1, "podcast"),
            app_stream(3, 2, "discord"),
            app_stream(4, 1, "game"),
            ours(5, 1),
        ];
        let mut srcs = [src(1, "spotify", CAP), src(2, "podcast", CAP2), src(3, "discord", CAP3)];
        let actions = plan_now(&world(&streams, &s, "speakers"), &srcs);
        assert_eq!(
            actions,
            [
                Action::Capture { source: 1, moved: moved(1, "SPOTIFY", "speakers", true) },
                Action::Capture { source: 2, moved: moved(2, "PODCAST", "speakers", true) },
                Action::Capture { source: 3, moved: moved(3, "DISCORD", "headset", false) },
            ],
            "each app onto its own sink; the game and Sidestage's output stay put"
        );
        // Apply the moves the way the session does, then plan again.
        for a in actions {
            let Action::Capture { source, moved } = a else { unreachable!() };
            let r = srcs.iter_mut().find(|r| r.id == source).unwrap();
            streams.iter_mut().find(|st| st.index == moved.index).unwrap().sink = r.sink;
            r.tracked.insert(moved.index, moved);
        }
        assert!(plan_now(&world(&streams, &s, "speakers"), &srcs).is_empty(), "settled");
        for (r, expected) in srcs.iter().zip([CAP, CAP2, CAP3]) {
            let state = app_state(&streams, Some(r.sink), &r.app);
            assert_eq!((r.sink, state.streams, state.captured), (expected, 1, 1));
        }
        let (game_sink, own_sink) = (streams[3].sink, streams[4].sink);
        assert_eq!((game_sink, own_sink), (1, 1));
    }

    #[test]
    fn plan_is_idempotent_and_adopts_restored_streams() {
        let s = sinks();
        let streams = vec![app_stream(1, CAP, "spotify"), app_stream(2, CAP, "spotify"), ours(3, 1)];
        let mut spotify = src(1, "spotify", CAP);
        spotify.tracked.insert(1, moved(1, "SPOTIFY", "headset", false));
        assert_eq!(
            plan_now(&world(&streams, &s, "speakers"), &[spotify]),
            [Action::Adopt { source: 1, moved: moved(2, "SPOTIFY", "headset", false) }],
            "stream 2 was put on the capture sink by the route stream 1's move saved; it \
             inherits stream 1's origin and only it needs recording"
        );

        let nothing_tracked = plan_now(&world(&streams, &s, "speakers"), &[src(1, "spotify", CAP)]);
        assert_eq!(
            nothing_tracked[0],
            Action::Adopt { source: 1, moved: moved(1, "SPOTIFY", "speakers", true) },
            "without a sibling to inherit from, adopted streams belong to the default"
        );

        let mut settled = src(1, "spotify", CAP);
        settled.tracked.insert(1, moved(1, "SPOTIFY", "headset", false));
        settled.tracked.insert(2, moved(2, "SPOTIFY", "headset", false));
        assert!(plan_now(&world(&streams, &s, "speakers"), &[settled]).is_empty());
    }

    #[test]
    fn recaptures_keep_first_origin_and_respect_limits() {
        let s = sinks();
        let streams = vec![app_stream(1, 1, "spotify"), app_stream(2, 2, "spotify")];
        let mut spotify = src(1, "spotify", CAP);
        spotify.tracked.insert(1, moved(1, "SPOTIFY", "headset", false));
        spotify.unmovable.insert(2);
        let now = Instant::now();
        let mut limiter = MoveLimiter::default();
        limiter.record(1, now);
        let w = world(&streams, &s, "speakers");
        let r = routes(std::slice::from_ref(&spotify));
        assert!(plan(&w, &r, &limiter, now).is_empty(), "debounced and unmovable");
        let later = now + Duration::from_millis(600);
        assert_eq!(
            plan(&w, &r, &limiter, later),
            [Action::Capture { source: 1, moved: spotify.tracked[&1].clone() }],
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
            default_sink: Some("sidestage_music_abc"),
            fallback_sink: Some("headset"),
            output_sink: None,
        };
        assert_eq!(plan_now(&w, &[src(1, "spotify", CAP)]), [Action::Evict { index: 4, to: "headset".into() }]);
        assert_eq!(home_sink(&s, Some("gone"), None).map(|k| k.name.as_str()), Some("speakers"));
        assert_eq!(home_sink(&s[2..], None, None), None, "only Sidestage sinks: nowhere to go");
    }

    #[test]
    fn own_output_goes_back_to_the_chosen_output() {
        let s = sinks();
        let streams = vec![ours(4, CAP2), app_stream(5, CAP2, "game")];
        let srcs = [src(1, "spotify", CAP), src(2, "discord", CAP2)];
        let chosen = |output_sink| World { output_sink, ..world(&streams, &s, "speakers") };
        assert_eq!(
            plan_now(&chosen(Some("headset")), &srcs),
            [Action::Evict { index: 4, to: "headset".into() }, Action::Evict { index: 5, to: "speakers".into() }],
            "Sidestage's output returns to the output the user chose; other apps to the default"
        );
        for missing in [Some("unplugged"), Some("sidestage_music_abc"), None] {
            assert_eq!(
                plan_now(&chosen(missing), &srcs)[0],
                Action::Evict { index: 4, to: "speakers".into() },
                "without a usable chosen output: the default ({missing:?})"
            );
        }
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
            app_stream(6, CAP2, "discord"),
        ];
        let tracked =
            vec![moved(1, "S", "headset", false), moved(2, "S", "speakers", true), moved(3, "S", "unplugged", false)];
        let moves = plan_restore(&streams, CAP, &s, &tracked, Some("headset"));
        assert_eq!(
            moves,
            [(2, "headset".into()), (3, "headset".into()), (4, "headset".into()), (1, "headset".into())],
            "default-origin streams follow the *current* default; missing originals fall back \
             to it; explicit origins go last; other sources' sinks are not touched"
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
        let mut own_pid = app_stream(7, 1, "sidestage-ui");
        own_pid.pid = Some(4242);
        let streams = vec![
            app_stream(1, 1, "game"),
            app_stream(2, 1, "spotify"),
            ours(3, 1),
            app_stream(4, 2, "voip"),
            app_stream(5, CAP, "spotify"),
            app_stream(6, 1, "firefox"),
            own_pid,
            app_stream(8, 1, "discord"),
        ];
        let (spotify, discord, firefox) = (key("spotify"), key("discord"), key("firefox"));
        assert_eq!(meter_targets(&streams, 1, &[&spotify], 4242), [1, 6, 8]);
        assert_eq!(
            meter_targets(&streams, 1, &[&spotify, &discord, &firefox], 4242),
            [1],
            "sources and ignored apps never count as the game, nor do Sidestage's streams"
        );
        assert_eq!(meter_targets(&streams, 2, &[], 4242), [4], "only the sink the user listens to");
        // Keys match by `AppKey::same_app`: exactly on Linux (Windows, where these tests also
        // run, ignores case).
        let spotify_cased = key("Spotify");
        let expected: &[u32] = if spotify_cased.same_app(&spotify) { &[1, 6, 8] } else { &[1, 2, 6, 8] };
        assert_eq!(meter_targets(&streams, 1, &[&spotify_cased], 4242), expected);
        #[cfg(target_os = "linux")]
        assert_eq!(expected, [1, 2, 6, 8], "keys match exactly on Linux");
    }

    #[test]
    fn ignored_apps_match_by_the_same_key_as_the_picker() {
        // An app without `application.process.binary` is keyed (and listed in the picker) by
        // its `application.name`; ignoring that key must silence its meter.
        let mut by_name = app_stream(1, 1, "unused");
        by_name.app_key = app_key(None, Some("Rhythmbox"), Some(10));
        by_name.app_name = Some("Rhythmbox".into());
        let mut by_pid = app_stream(2, 1, "unused");
        by_pid.app_key = app_key(None, None, Some(20));
        by_pid.app_name = None;
        let streams = vec![by_name, by_pid, app_stream(3, 1, "game")];
        let listed: Vec<AppKey> = group_apps(&streams, &BTreeSet::new()).into_iter().map(|g| g.key).collect();
        assert_eq!(listed, [key("game"), key("pid:20"), key("Rhythmbox")]);
        let ignore: Vec<&AppKey> = listed.iter().filter(|k| k.0 != "game").collect();
        assert_eq!(meter_targets(&streams, 1, &ignore, 4242), [3], "only the game is measured");
        assert_eq!(meter_targets(&streams, 1, &[], 4242), [1, 2, 3], "and every app counts until ignored");
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
            app_state(&streams, Some(CAP), &key("spotify")),
            AppState { streams: 2, captured: 1, name: Some("SPOTIFY".into()) }
        );
        assert_eq!(app_state(&streams, None, &key("spotify")).captured, 0);
        assert_eq!(app_state(&streams, Some(CAP), &key("discord")), AppState::default());
    }

    #[test]
    fn journal_roundtrips_and_reads_the_single_sink_layout() {
        let j = RoutingJournal {
            pid: 1,
            session: "s".into(),
            sinks: vec![
                SinkRecord {
                    sink_name: "sidestage_music_abc".into(),
                    module_index: Some(3),
                    streams: vec![moved(1, "Spotify", "speakers", true)],
                },
                SinkRecord { sink_name: "sidestage_music_def".into(), module_index: None, streams: vec![] },
            ],
        };
        let text = serde_json::to_string(&j).unwrap();
        assert_eq!(serde_json::from_str::<RoutingJournal>(&text).unwrap(), j);

        let v01: RoutingJournal = serde_json::from_str(
            r#"{"pid":1,"session":"s","sink_name":"sidestage_music_abc","module_index":3,
                "streams":[{"index":1,"app":"Spotify","original_sink":"speakers","was_default":true}]}"#,
        )
        .unwrap();
        assert_eq!(v01, RoutingJournal { sinks: j.sinks[..1].to_vec(), ..j.clone() }, "a 0.1 journal is still undone");
        let minimal: RoutingJournal =
            serde_json::from_str(r#"{"pid":1,"session":"s","sink_name":"","module_index":null}"#).unwrap();
        assert!(minimal.sinks.is_empty(), "0.1 wrote an empty name before its first sink");
        let empty: RoutingJournal = serde_json::from_str(r#"{"pid":1,"session":"s"}"#).unwrap();
        assert!(empty.sinks.is_empty());
    }
}
