//! Command-line interface: argument parsing and the headless commands (`list`, `outputs`,
//! `presets`, `run`, `restore`). Hand-written to keep the binary small; the grammar is tiny.

use std::time::{Duration, Instant};

use anyhow::Context as _;
use sidestage_dsp::{Preset, PresetKind, Settings, SharedParams};
use sidestage_platform::{
    AppKey, AudioApp, Platform, Session, SessionConfig, SessionState, SourceConfig, SourceId, Support,
};

use crate::config::{ChosenApp, Config, MAX_SOURCES, Source, default_preset_for};
use crate::console::{say, tell};
use crate::demo::{DemoPlatform, Scenario};
use crate::instance::{self, Acquire, InstanceLock};
use crate::processor::{self, VoiceBus, VoiceSlots};
use crate::text::join_names;
use crate::{signal, text};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Gui { demo: bool },
    List,
    Outputs,
    Presets,
    Run(RunArgs),
    Restore,
    Help,
    Version,
}

/// One `<app>[=<preset>]` of `run`.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceArg {
    /// Part of the app's name or key, e.g. "spotify".
    pub query: String,
    pub preset: Option<Preset>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunArgs {
    pub sources: Vec<SourceArg>,
    /// Preset for the sources without their own.
    pub preset: Option<Preset>,
    /// Stop after this many seconds.
    pub seconds: Option<f64>,
}

/// Command-line names of the presets: the name `presets` shows first, then accepted aliases.
const PRESET_NAMES: [(Preset, &str, &[&str]); 11] = [
    (Preset::BehindYou, "behind", &["behind-you"]),
    (Preset::NextRoom, "next-room", &["nextroom"]),
    (Preset::FarAway, "far", &["far-away"]),
    (Preset::Above, "above", &[]),
    (Preset::DeskSpeaker, "desk", &["desk-speaker"]),
    (Preset::Lounge, "lounge", &[]),
    (Preset::VoiceLeft, "voice-left", &["voice-chat-left"]),
    (Preset::VoiceRight, "voice-right", &["voice-chat-right", "voice"]),
    (Preset::OverShoulder, "shoulder", &["over-shoulder", "over-your-shoulder"]),
    (Preset::RadioComms, "radio", &["radio-comms"]),
    (Preset::PodcastLeft, "podcast-left", &["podcast"]),
];

/// The name `run` accepts for a preset.
pub fn preset_cli_name(preset: Preset) -> &'static str {
    PRESET_NAMES.iter().find(|(p, ..)| *p == preset).map_or("", |(_, name, _)| name)
}

fn preset_list(kind: PresetKind) -> String {
    Preset::ALL.iter().filter(|p| p.kind() == kind).map(|p| preset_cli_name(*p)).collect::<Vec<_>>().join(", ")
}

pub fn help() -> String {
    format!(
        "Sidestage {version}
Put the audio of your apps around you in 3D, e.g. music behind you, a podcast on your left and
voice chat on your right, while your game audio stays untouched. Headphones only.

Usage:
  sidestage                       Open the app
  sidestage --demo                Open the app with simulated apps and audio (nothing is captured)
  sidestage list                  List apps that have audio right now
  sidestage outputs               List output devices
  sidestage presets               List the presets
  sidestage run <app>[=<preset>] [<app>[=<preset>] ...] [options]
                                  Spatialize up to {MAX_SOURCES} apps without opening the window
      --preset <name>             Preset for apps without their own (default: your saved
                                  settings for that app, else one that suits it)
      --seconds <n>               Stop after n seconds (default: until Ctrl+C)
  sidestage restore               Undo audio changes left behind if Sidestage was killed
  sidestage --help                Show this help
  sidestage --version             Show the version

<app> is any part of an app's name or key as shown by 'sidestage list', e.g. spotify. Quote
names with spaces: sidestage run \"apple music\" discord=voice-right
Music presets: {music}
Voice presets: {voice}
'run' uses the output device, hide method and ducking settings saved by the app.
Set RUST_LOG=debug for detailed logs, SIDESTAGE_DEMO=1 to try the commands with simulated apps.
SIDESTAGE_CONFIG_DIR and SIDESTAGE_STATE_DIR move the settings folder and the state folder
(crash-recovery records, the lock, the log), e.g. for a test run beside your own Sidestage.",
        version = env!("CARGO_PKG_VERSION"),
        music = preset_list(PresetKind::Music),
        voice = preset_list(PresetKind::Voice),
    )
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let mut args = args.iter().map(String::as_str);
    let Some(first) = args.next() else { return Ok(Command::Gui { demo: false }) };
    let rest: Vec<&str> = args.collect();
    // `--help` anywhere asks for help, except after `--` (`sidestage run -- --help` is an app).
    if rest.iter().take_while(|a| **a != "--").any(|a| matches!(*a, "-h" | "--help")) {
        return Ok(Command::Help);
    }
    let no_more = |command: Command| match rest.first() {
        None => Ok(command),
        Some(extra) => Err(format!("unexpected argument '{extra}'")),
    };
    match first {
        "--demo" => no_more(Command::Gui { demo: true }),
        "-h" | "--help" | "help" => Ok(Command::Help),
        "-V" | "--version" | "version" => no_more(Command::Version),
        "list" => no_more(Command::List),
        "outputs" => no_more(Command::Outputs),
        "presets" => no_more(Command::Presets),
        "restore" => no_more(Command::Restore),
        "run" => parse_run(&rest).map(Command::Run),
        other if other.starts_with('-') => Err(format!("unknown option '{other}'")),
        other => Err(format!("unknown command '{other}'")),
    }
}

fn parse_run(args: &[&str]) -> Result<RunArgs, String> {
    let mut sources: Vec<SourceArg> = Vec::new();
    let mut preset = None;
    let mut seconds = None;
    let mut i = 0;
    let mut options_done = false;
    while i < args.len() {
        let arg = args[i];
        i += 1;
        if options_done || !arg.starts_with("--") {
            let source = parse_source(arg)?;
            if sources.iter().any(|s| s.query.eq_ignore_ascii_case(&source.query)) {
                return Err(format!("'{}' is given twice; an app can only be one source", source.query));
            }
            sources.push(source);
            continue;
        }
        if arg == "--" {
            options_done = true;
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg, None),
        };
        let mut value = || -> Result<&str, String> {
            if let Some(v) = inline {
                return Ok(v);
            }
            let v = args.get(i).copied().ok_or_else(|| format!("{name} needs a value"))?;
            i += 1;
            Ok(v)
        };
        match name {
            "--preset" => preset = Some(preset_arg(value()?)?),
            "--seconds" => {
                let v = value()?;
                let s: f64 = v.parse().map_err(|_| format!("--seconds expects a number, got '{v}'"))?;
                if !(s.is_finite() && s > 0.0 && s < 1e9) {
                    return Err(format!("--seconds must be a positive number, got '{v}'"));
                }
                seconds = Some(s);
            }
            _ => return Err(format!("unknown option '{name}' for 'run'")),
        }
    }
    if sources.is_empty() {
        return Err("'run' needs the name of an app, e.g. 'sidestage run spotify'".into());
    }
    if sources.len() > MAX_SOURCES {
        return Err(format!("'run' takes at most {MAX_SOURCES} apps, got {}", sources.len()));
    }
    Ok(RunArgs { sources, preset, seconds })
}

/// `spotify`, `discord=voice-right`, `"apple music=far"`.
fn parse_source(arg: &str) -> Result<SourceArg, String> {
    let (query, preset) = match arg.rsplit_once('=') {
        Some((query, preset)) => (query.trim(), Some(preset)),
        None => (arg.trim(), None),
    };
    if query.is_empty() {
        return Err(format!("'{arg}' needs an app before the '='"));
    }
    let preset = preset.map(preset_arg).transpose()?;
    Ok(SourceArg { query: query.to_owned(), preset })
}

fn preset_arg(name: &str) -> Result<Preset, String> {
    parse_preset(name).ok_or_else(|| {
        format!(
            "unknown preset '{name}'. Music presets: {}. Voice presets: {}.",
            preset_list(PresetKind::Music),
            preset_list(PresetKind::Voice)
        )
    })
}

pub fn parse_preset(name: &str) -> Option<Preset> {
    let n: String = name.trim().to_lowercase().chars().map(|c| if c == '_' || c == ' ' { '-' } else { c }).collect();
    PRESET_NAMES.iter().find(|(_, main, aliases)| *main == n || aliases.contains(&n.as_str())).map(|(p, ..)| *p)
}

#[derive(Debug, PartialEq)]
pub enum AppMatch {
    Found(usize),
    None,
    /// Several equally good matches (names).
    Ambiguous(Vec<String>),
}

/// Find the app the user means: exact name/key first, then name prefix, then name substring,
/// then key substring. Among equally good matches, a single playing app wins.
pub fn match_app(apps: &[AudioApp], query: &str) -> AppMatch {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return AppMatch::None;
    }
    let rank = |a: &AudioApp| -> Option<u8> {
        let name = a.name.to_lowercase();
        let key = a.key.0.to_lowercase();
        if name == q || key == q {
            Some(0)
        } else if name.starts_with(&q) {
            Some(1)
        } else if name.contains(&q) {
            Some(2)
        } else if key.contains(&q) {
            Some(3)
        } else {
            None
        }
    };
    let ranked: Vec<(usize, u8)> = apps.iter().enumerate().filter_map(|(i, a)| rank(a).map(|r| (i, r))).collect();
    let Some(best) = ranked.iter().map(|(_, r)| *r).min() else { return AppMatch::None };
    let candidates: Vec<usize> = ranked.iter().filter(|(_, r)| *r == best).map(|(i, _)| *i).collect();
    if let [only] = candidates.as_slice() {
        return AppMatch::Found(*only);
    }
    let playing: Vec<usize> = candidates.iter().copied().filter(|i| apps[*i].playing).collect();
    if let [only] = playing.as_slice() {
        return AppMatch::Found(*only);
    }
    AppMatch::Ambiguous(candidates.iter().map(|i| apps[*i].name.clone()).collect())
}

/// What matching the waiting queries against the app list gave.
#[derive(Debug, Default)]
struct Resolved {
    /// Newly found sources as (index into the queries, app).
    found: Vec<(usize, AudioApp)>,
    /// Queries that cannot become a source, with why: several apps match, or the app it finds
    /// is already one (`run apple music` without quotes: an app can only be one source).
    problems: Vec<(usize, String)>,
}

/// Match the queries in `waiting` (indices into `sources`) against `apps`, given the sources
/// `found` so far. Every query that found its app, or cannot ever be a source, leaves `waiting`.
fn resolve(
    apps: &[AudioApp],
    sources: &[SourceArg],
    waiting: &mut Vec<usize>,
    found: &[(usize, AudioApp)],
) -> Resolved {
    let mut out = Resolved::default();
    for &i in waiting.iter() {
        let query = &sources[i].query;
        match match_app(apps, query) {
            AppMatch::Found(j) => {
                let app = &apps[j];
                if let Some((k, _)) = found.iter().chain(&out.found).find(|(_, a)| a.key.same_app(&app.key)) {
                    let (a, b) = if *k < i { (&sources[*k].query, query) } else { (query, &sources[*k].query) };
                    let problem = format!(
                        "'{a}' and '{b}' both match {}, and an app can only be one source. If '{a} {b}' is one \
                         name, quote it: sidestage run \"{a} {b}\"",
                        app.name
                    );
                    out.problems.push((i, problem));
                    continue;
                }
                out.found.push((i, app.clone()));
            }
            AppMatch::Ambiguous(names) => {
                let problem = format!("'{query}' matches several apps: {}. Please be more specific.", names.join(", "));
                out.problems.push((i, problem));
            }
            AppMatch::None => {}
        }
    }
    waiting.retain(|i| !out.found.iter().any(|(j, _)| j == i) && !out.problems.iter().any(|(j, _)| j == i));
    out
}

/// Run a non-GUI command; returns the process exit code.
pub fn execute(command: Command) -> i32 {
    let result = match command {
        Command::Help => {
            say!("{}", help());
            Ok(())
        }
        Command::Version => {
            say!("sidestage {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::List => list(),
        Command::Outputs => outputs(),
        Command::Presets => {
            presets();
            Ok(())
        }
        Command::Restore => restore(),
        Command::Run(args) => run(&args),
        Command::Gui { .. } => Ok(()),
    };
    signal::cleanup_done();
    match result {
        Ok(()) => 0,
        Err(e) => {
            log::debug!("command failed: {e:?}");
            tell!("error: {e:#}");
            1
        }
    }
}

/// The OS backend, or the simulated one when `SIDESTAGE_DEMO=1` (for trying the commands
/// without capturing anything).
fn any_platform() -> anyhow::Result<Box<dyn Platform>> {
    if demo_mode() {
        return Ok(Box::new(DemoPlatform::new(Scenario::default())));
    }
    sidestage_platform::platform()
}

fn demo_mode() -> bool {
    std::env::var_os("SIDESTAGE_DEMO").is_some_and(|v| !v.is_empty() && v != "0")
}

fn platform() -> anyhow::Result<Box<dyn Platform>> {
    let platform = any_platform()?;
    if let Support::Unsupported(why) = platform.support() {
        anyhow::bail!("{why}");
    }
    Ok(platform)
}

fn list() -> anyhow::Result<()> {
    let mut apps = platform()?.list_apps()?;
    if apps.is_empty() {
        say!("No apps are playing audio right now.");
        return Ok(());
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    let width = apps.iter().map(|a| a.name.chars().count()).max().unwrap_or(0).min(32);
    say!("Apps with audio (* = playing now):");
    for a in &apps {
        let mark = if a.playing { '*' } else { ' ' };
        say!("{mark} {:<width$}  {}", a.name, a.detail);
        say!("  {:<width$}  key: {}", "", a.key.0);
    }
    Ok(())
}

fn outputs() -> anyhow::Result<()> {
    let outputs = platform()?.list_outputs()?;
    if outputs.is_empty() {
        say!("No output devices found.");
    }
    for o in outputs {
        let default = if o.is_default { "  (default)" } else { "" };
        say!("{}{default}\n    id: {}", o.name, o.id);
    }
    Ok(())
}

fn presets() {
    let width = PRESET_NAMES.iter().map(|(_, n, _)| n.len()).max().unwrap_or(0);
    let name_width = Preset::ALL.iter().map(|p| p.name().len()).max().unwrap_or(0);
    for (kind, title) in [(PresetKind::Music, "Music presets"), (PresetKind::Voice, "Voice presets")] {
        say!("{title}:");
        for p in Preset::ALL.iter().filter(|p| p.kind() == kind) {
            say!("  {:<width$}  {:<name_width$}  {}", preset_cli_name(*p), p.name(), p.description());
        }
        say!("");
    }
    say!("Use them as 'sidestage run <app>=<preset>', e.g. sidestage run spotify discord=voice-right");
}

fn restore() -> anyhow::Result<()> {
    let _lock = lock()?;
    let lines = any_platform()?.recover()?;
    if lines.is_empty() {
        say!("Nothing to restore: Sidestage left no changes behind.");
    }
    // Each line is a sentence of its own: what was put back, or (Windows) what is put back the
    // next time its app plays.
    for line in lines {
        say!("{line}");
    }
    Ok(())
}

/// Refuse to touch the audio setup while another Sidestage is doing so.
fn lock() -> anyhow::Result<Option<InstanceLock>> {
    if demo_mode() {
        return Ok(None);
    }
    match instance::acquire()? {
        Acquire::Acquired(lock) => Ok(Some(lock)),
        Acquire::AlreadyRunning => anyhow::bail!(instance::ALREADY_RUNNING),
    }
}

/// A source `run` plays.
struct RunSource {
    id: SourceId,
    app: AudioApp,
    /// "Voice chat right", "your saved settings".
    how: String,
    settings: Settings,
    config: SourceConfig,
}

/// Everything `run` needs to add a source to the session.
struct Sources<'a> {
    args: &'a RunArgs,
    saved: &'a Config,
    hide: sidestage_platform::HideMethod,
    voice: std::sync::Arc<VoiceBus>,
    slots: VoiceSlots,
    list: Vec<RunSource>,
}

impl Sources<'_> {
    fn add(&mut self, index: usize, app: AudioApp) {
        let arg = &self.args.sources[index];
        let chosen = ChosenApp { key: app.key.clone(), name: app.name.clone() };
        let (settings, how) = match (arg.preset.or(self.args.preset), self.saved.source_for(&app.key)) {
            (Some(p), _) => (p.settings(), p.name().to_owned()),
            (None, Some(saved)) => (saved.settings, "your saved settings".to_owned()),
            (None, None) => {
                // The same choice the window makes for a newly added app.
                let others: Vec<Source> = self
                    .list
                    .iter()
                    .map(|s| Source { settings: s.settings, base_preset: base_of(&s.settings), ..Source::default() })
                    .collect();
                let p = default_preset_for(&chosen, &others);
                (p.settings(), p.name().to_owned())
            }
        };
        let id = index as SourceId + 1;
        let params = SharedParams::new(settings);
        let slot = self.slots.assign(id);
        let config = SourceConfig {
            id,
            app: app.key.clone(),
            hide: self.hide.clone(),
            processor: processor::source_factory(params, std::sync::Arc::clone(&self.voice), slot),
        };
        self.list.push(RunSource { id, app, how, settings, config });
    }

    fn configs(&self) -> Vec<SourceConfig> {
        self.list.iter().map(|s| s.config.clone()).collect()
    }

    fn names(&self) -> String {
        join_names(&self.list.iter().map(|s| s.app.name.as_str()).collect::<Vec<_>>())
    }
}

fn base_of(settings: &Settings) -> Preset {
    Preset::matching(settings).unwrap_or(Preset::BehindYou)
}

fn run(args: &RunArgs) -> anyhow::Result<()> {
    signal::install().context("installing the Ctrl+C handler")?;
    let _lock = lock()?;
    let mut platform = platform()?;
    if let Support::NeedsPermission(what) = platform.support() {
        say!("Note: {what}");
    }
    match platform.recover() {
        Ok(lines) if !lines.is_empty() => {
            say!("Undoing changes left by an earlier Sidestage run:");
            lines.iter().for_each(|l| say!("  {l}"));
        }
        Ok(_) => {}
        Err(e) => tell!("warning: could not check for leftovers of a previous run: {e:#}"),
    }

    // The simulated apps have nothing to do with the user's saved setup (and reading a 0.1 file
    // would leave its backup copy behind).
    let config_path = Config::default_path().ok().filter(|p| p.exists() && !demo_mode());
    let saved = config_path.as_deref().map(Config::load).unwrap_or_default();
    let deadline = args.seconds.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let expired = || signal::stop_requested() || deadline.is_some_and(|d| Instant::now() >= d);

    let mut waiting: Vec<usize> = (0..args.sources.len()).collect();
    let first = wait_for_apps(platform.as_mut(), args, &mut waiting, &expired)?;
    if first.is_empty() {
        if signal::stop_requested() {
            say!("Cancelled.");
            return Ok(());
        }
        let queries: Vec<&str> = args.sources.iter().map(|s| s.query.as_str()).collect();
        anyhow::bail!("no app matching {} played audio in time; nothing was started", quoted(&queries));
    }
    if signal::stop_requested() {
        // Ctrl+C right as the app showed up: don't hide it only to restore it again.
        say!("Cancelled.");
        return Ok(());
    }

    let mut sources = Sources {
        args,
        saved: &saved,
        hide: saved.hide.clone(),
        voice: VoiceBus::new(),
        slots: VoiceSlots::default(),
        list: Vec::new(),
    };
    for (i, app) in first {
        sources.add(i, app);
    }
    let ignore: Vec<AppKey> = saved.ducking_ignore.iter().map(|a| a.key.clone()).collect();
    let mut session = platform
        .start(SessionConfig {
            output: saved.output.clone(),
            sidechain: saved.sidechain,
            sidechain_ignore: ignore,
            sources: sources.configs(),
            master: processor::master_factory(),
        })
        .with_context(|| format!("starting to capture {}", sources.names()))?;
    let described: Vec<String> = sources.list.iter().map(|s| format!("{} ({})", s.app.name, s.how)).collect();
    say!(
        "Spatializing {}. Press Ctrl+C to stop.",
        join_names(&described.iter().map(String::as_str).collect::<Vec<_>>())
    );
    announce_waiting(args, &waiting);

    let outcome = supervise(session.as_mut(), platform.as_mut(), &mut sources, &mut waiting, &expired);
    if signal::stop_requested() {
        say!("Stopping\u{2026} (press Ctrl+C again to quit at once)");
    }
    let names = sources.names();
    let stopped = session.stop();
    match (outcome, stopped) {
        (Err(e), stop) => {
            if let Err(se) = stop {
                tell!("error while stopping: {se:#}");
            }
            Err(e)
        }
        (Ok(()), Err(e)) => Err(e).context("stopping the session"),
        (Ok(()), Ok(())) => {
            let verb = if sources.list.len() == 1 { "plays" } else { "play" };
            say!("Stopped. {names} {verb} normally again.");
            Ok(())
        }
    }
}

/// Print status changes until the time is up, adding sources whose apps show up late. An error
/// ends it when the session fails or every source has failed.
fn supervise(
    session: &mut dyn Session,
    platform: &mut dyn Platform,
    sources: &mut Sources<'_>,
    waiting: &mut Vec<usize>,
    expired: &dyn Fn() -> bool,
) -> anyhow::Result<()> {
    // Printed when a state changes (latency jitter alone does not count) and every 10 s.
    let mut last_states: Option<(SessionState, Vec<(SourceId, SessionState)>)> = None;
    let mut last_print = Instant::now();
    let mut warned: Vec<String> = Vec::new();
    let mut next_list = Instant::now() + Duration::from_secs(1);
    while !expired() {
        let st = session.status();
        let states = (st.state.clone(), st.sources.iter().map(|s| (s.id, s.state.clone())).collect::<Vec<_>>());
        if last_states.as_ref() != Some(&states) || last_print.elapsed() >= Duration::from_secs(10) {
            say!("{}", text::status_summary(&st));
            last_print = Instant::now();
        }
        for s in &st.sources {
            if !last_states.as_ref().is_some_and(|(_, old)| old.contains(&(s.id, s.state.clone()))) {
                say!("  {}", text::source_line(s));
            }
        }
        last_states = Some(states);
        let warnings: Vec<String> =
            st.warnings.iter().chain(st.sources.iter().flat_map(|s| &s.warnings)).cloned().collect();
        for w in warnings.iter().filter(|w| !warned.contains(w)) {
            say!("warning: {w}");
        }
        warned = warnings;
        match &st.state {
            SessionState::Error(e) => anyhow::bail!("{e}"),
            SessionState::Stopped => anyhow::bail!("playing {} stopped unexpectedly", sources.names()),
            _ => {}
        }
        let errors: Vec<String> = st
            .sources
            .iter()
            .filter_map(|s| match &s.state {
                SessionState::Error(e) => Some(format!("{}: {e}", s.app_name)),
                _ => None,
            })
            .collect();
        if !st.sources.is_empty() && errors.len() == st.sources.len() && waiting.is_empty() {
            anyhow::bail!("{}", errors.join("; "));
        }
        if !waiting.is_empty() && Instant::now() >= next_list {
            next_list = Instant::now() + Duration::from_secs(1);
            match platform.list_apps() {
                Ok(apps) => {
                    let found: Vec<(usize, AudioApp)> =
                        sources.list.iter().map(|s| (s.id as usize - 1, s.app.clone())).collect();
                    let Resolved { found: newly, problems } = resolve(&apps, &sources.args.sources, waiting, &found);
                    // The session is running: a query that went wrong must not end it (and give
                    // every other app back its original output).
                    for (_, problem) in problems {
                        say!("warning: {problem} It is not added; the others keep playing.");
                    }
                    if !newly.is_empty() {
                        let mut added = Vec::new();
                        for (i, app) in newly {
                            added.push(app.name.clone());
                            sources.add(i, app);
                        }
                        session.set_sources(sources.configs()).context("adding a source")?;
                        let start = sources.list.len() - added.len();
                        for s in &sources.list[start..] {
                            say!("Added {} ({}).", s.app.name, s.how);
                        }
                    }
                }
                Err(e) => log::debug!("listing apps failed: {e:#}; still trying"),
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn quoted(queries: &[&str]) -> String {
    let q: Vec<String> = queries.iter().map(|q| format!("'{q}'")).collect();
    let refs: Vec<&str> = q.iter().map(String::as_str).collect();
    join_names(&refs).replace(" and ", " or ")
}

fn announce_waiting(args: &RunArgs, waiting: &[usize]) {
    if !waiting.is_empty() {
        let queries: Vec<&str> = waiting.iter().map(|i| args.sources[*i].query.as_str()).collect();
        say!("Waiting for an app matching {} to play audio; it is added when it does.", quoted(&queries));
    }
}

/// Poll until at least one query matches an app with audio. Returns the matches (index into
/// `args.sources`, app); empty if interrupted first.
fn wait_for_apps(
    platform: &mut dyn Platform,
    args: &RunArgs,
    waiting: &mut Vec<usize>,
    expired: &dyn Fn() -> bool,
) -> anyhow::Result<Vec<(usize, AudioApp)>> {
    let mut announced = false;
    // The first listing must work; later hiccups (the sound server restarting) only delay.
    let mut listed_once = false;
    let mut failing = false;
    loop {
        let apps = match platform.list_apps() {
            Ok(apps) => {
                failing = false;
                apps
            }
            Err(e) if listed_once => {
                if !failing {
                    log::warn!("listing apps failed: {e:#}; still trying");
                }
                failing = true;
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        listed_once = true;
        let Resolved { found, problems } = resolve(&apps, &args.sources, waiting, &[]);
        // Nothing runs yet: better to stop and let the user fix the command line.
        if let Some((_, problem)) = problems.into_iter().next() {
            anyhow::bail!("{problem}");
        }
        if !found.is_empty() {
            return Ok(found);
        }
        if !announced {
            let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
            let now = if names.is_empty() { "none".to_owned() } else { names.join(", ") };
            let queries: Vec<&str> = args.sources.iter().map(|s| s.query.as_str()).collect();
            say!(
                "Waiting for an app matching {} to play audio (apps with audio now: {now}). Ctrl+C to cancel.",
                quoted(&queries)
            );
            announced = true;
        }
        for _ in 0..10 {
            if expired() {
                return Ok(Vec::new());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    fn run(s: &str) -> Result<RunArgs, String> {
        match parse(&args(s)) {
            Ok(Command::Run(r)) => Ok(r),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e),
        }
    }

    fn src(query: &str, preset: Option<Preset>) -> SourceArg {
        SourceArg { query: query.into(), preset }
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse(&[]), Ok(Command::Gui { demo: false }));
        assert_eq!(parse(&args("--demo")), Ok(Command::Gui { demo: true }));
        assert_eq!(parse(&args("list")), Ok(Command::List));
        assert_eq!(parse(&args("outputs")), Ok(Command::Outputs));
        assert_eq!(parse(&args("presets")), Ok(Command::Presets));
        assert_eq!(parse(&args("restore")), Ok(Command::Restore));
        assert_eq!(parse(&args("--help")), Ok(Command::Help));
        assert_eq!(parse(&args("run --help")), Ok(Command::Help));
        assert_eq!(parse(&args("-V")), Ok(Command::Version));
        assert!(parse(&args("list extra")).is_err());
        assert!(parse(&args("presets all")).is_err());
        assert!(parse(&args("frobnicate")).is_err());
        assert!(parse(&args("--frobnicate")).is_err());
    }

    #[test]
    fn parses_run_with_one_source() {
        assert_eq!(
            run("run spotify"),
            Ok(RunArgs { sources: vec![src("spotify", None)], preset: None, seconds: None })
        );
        assert_eq!(
            run("run vlc --preset next-room --seconds 2.5"),
            Ok(RunArgs { sources: vec![src("vlc", None)], preset: Some(Preset::NextRoom), seconds: Some(2.5) })
        );
        assert_eq!(
            run("run --preset=far --seconds=10 vlc").map(|r| (r.sources, r.preset)),
            Ok((vec![src("vlc", None)], Some(Preset::FarAway)))
        );
        let after_dashes = |s: &str| run(s).map(|r| r.sources[0].query.clone());
        assert_eq!(after_dashes("run -- --weird-name"), Ok("--weird-name".into()));
        assert_eq!(after_dashes("run -- --help"), Ok("--help".into()), "after -- it is an app name");
        assert!(run("run").is_err());
        assert!(run("run spotify --preset").is_err());
        assert!(run("run spotify --preset loud").is_err());
        assert!(run("run spotify --seconds -3").is_err());
        assert!(run("run spotify --seconds abc").is_err());
        assert!(run("run spotify --volume 3").is_err());
    }

    #[test]
    fn parses_run_with_several_sources() {
        assert_eq!(
            run("run spotify pocket=podcast-left discord=voice-right --preset far"),
            Ok(RunArgs {
                sources: vec![
                    src("spotify", None),
                    src("pocket", Some(Preset::PodcastLeft)),
                    src("discord", Some(Preset::VoiceRight)),
                ],
                preset: Some(Preset::FarAway),
                seconds: None,
            })
        );
        // A quoted name with a space is one argument, so one source.
        let quoted = vec!["run".to_owned(), "apple music=shoulder".to_owned(), "teamspeak=radio".to_owned()];
        match parse(&quoted) {
            Ok(Command::Run(r)) => {
                assert_eq!(
                    r.sources,
                    vec![src("apple music", Some(Preset::OverShoulder)), src("teamspeak", Some(Preset::RadioComms))]
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(run("run discord=").is_err(), "empty preset");
        assert!(run("run =voice-left").is_err(), "no app");
        assert!(run("run discord=loud").unwrap_err().contains("voice-left"), "the error lists the voice presets");
        assert!(run("run spotify Spotify").is_err(), "the same app twice");
        assert!(run("run a b c d e f g h").is_ok());
        assert!(run("run a b c d e f g h i").is_err(), "at most {MAX_SOURCES} sources");
    }

    #[test]
    fn preset_names() {
        for (name, p) in [
            ("behind", Preset::BehindYou),
            ("next-room", Preset::NextRoom),
            ("far", Preset::FarAway),
            ("above", Preset::Above),
            ("desk", Preset::DeskSpeaker),
            ("lounge", Preset::Lounge),
            ("voice-left", Preset::VoiceLeft),
            ("voice-right", Preset::VoiceRight),
            ("shoulder", Preset::OverShoulder),
            ("radio", Preset::RadioComms),
            ("podcast-left", Preset::PodcastLeft),
            ("Behind You", Preset::BehindYou),
            ("desk_speaker", Preset::DeskSpeaker),
            ("Over your shoulder", Preset::OverShoulder),
            ("podcast", Preset::PodcastLeft),
        ] {
            assert_eq!(parse_preset(name), Some(p), "{name}");
        }
        assert_eq!(parse_preset("loud"), None);
        for p in Preset::ALL {
            assert_eq!(parse_preset(preset_cli_name(p)), Some(p), "every preset has a command-line name");
        }
        assert!(help().contains("voice-right") && help().contains("podcast-left"));
    }

    fn app(name: &str, key: &str, playing: bool) -> AudioApp {
        AudioApp {
            key: AppKey(key.into()),
            name: name.into(),
            detail: String::new(),
            pids: vec![1],
            playing,
            peak: None,
        }
    }

    #[test]
    fn matches_apps() {
        let apps = vec![
            app("Spotify", r"C:\Users\me\AppData\Roaming\Spotify\Spotify.exe", true),
            app("Firefox", r"C:\Program Files\Mozilla Firefox\firefox.exe", true),
            app("Firefox Developer Edition", r"C:\Program Files\Firefox Dev\firefox.exe", false),
            app("Microsoft Edge", r"C:\Program Files\Edge\msedge.exe", true),
        ];
        assert_eq!(match_app(&apps, "spotify"), AppMatch::Found(0));
        assert_eq!(match_app(&apps, "SPOT"), AppMatch::Found(0));
        assert_eq!(match_app(&apps, "firefox"), AppMatch::Found(1), "exact name beats prefix");
        assert_eq!(match_app(&apps, "edge"), AppMatch::Found(3), "name substring");
        assert_eq!(match_app(&apps, "msedge"), AppMatch::Found(3), "key substring");
        assert_eq!(match_app(&apps, "fire"), AppMatch::Found(1), "the playing one of two prefixes");
        assert_eq!(match_app(&apps, "vlc"), AppMatch::None);
        assert_eq!(match_app(&apps, "  "), AppMatch::None);
        let both_playing = vec![app("Chrome", "a", true), app("Chrome", "b", true)];
        assert_eq!(match_app(&both_playing, "chrome"), AppMatch::Ambiguous(vec!["Chrome".into(), "Chrome".into()]));
    }

    #[test]
    fn resolves_several_sources_as_their_apps_appear() {
        let sources = vec![src("spotify", None), src("discord", Some(Preset::VoiceRight)), src("vlc", None)];
        let mut waiting = vec![0, 1, 2];
        let apps = vec![app("Spotify", "spotify.exe", true), app("Steam", "steam.exe", true)];
        let found = resolve(&apps, &sources, &mut waiting, &[]).found;
        assert_eq!(found.iter().map(|(i, a)| (*i, a.name.as_str())).collect::<Vec<_>>(), vec![(0, "Spotify")]);
        assert_eq!(waiting, vec![1, 2]);
        let apps = vec![app("Spotify", "spotify.exe", true), app("Discord", "discord.exe", true)];
        let later = resolve(&apps, &sources, &mut waiting, &found);
        assert_eq!(later.found.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![1]);
        assert!(later.problems.is_empty());
        assert_eq!(waiting, vec![2]);

        // `run apple music` without quotes: both words find Apple Music. The first one gets it,
        // the second is a problem and stops waiting.
        let words = vec![src("apple", None), src("music", None)];
        let apps = vec![app("Apple Music", "com.apple.Music", true)];
        let mut waiting = vec![0, 1];
        let r = resolve(&apps, &words, &mut waiting, &[]);
        assert_eq!(r.found.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0]);
        let [(1, problem)] = r.problems.as_slice() else { panic!("{:?}", r.problems) };
        assert!(problem.contains("both match Apple Music") && problem.contains("\"apple music\""), "{problem}");
        assert!(waiting.is_empty());

        // A late query that finds an app already playing, or several apps, does not stop the
        // others (this is what `run` checks while the session plays).
        let mut waiting = vec![1, 2];
        let chrome = [app("Spotify", "spotify.exe", true), app("Chrome", "a", true), app("Chrome", "b", true)];
        let late = vec![src("spotify", None), src("spot", None), src("chrome", None)];
        let r = resolve(&chrome, &late, &mut waiting, &[(0, chrome[0].clone())]);
        assert!(r.found.is_empty() && waiting.is_empty());
        let problems: Vec<&str> = r.problems.iter().map(|(_, p)| p.as_str()).collect();
        assert!(problems[0].contains("both match Spotify") && problems[1].contains("several apps"), "{problems:?}");
    }

    #[test]
    fn queries_read_naturally() {
        assert_eq!(quoted(&["spotify"]), "'spotify'");
        assert_eq!(quoted(&["spotify", "discord"]), "'spotify' or 'discord'");
        assert_eq!(quoted(&["a", "b", "c"]), "'a', 'b' or 'c'");
    }
}
