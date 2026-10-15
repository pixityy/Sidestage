//! Command-line interface: argument parsing and the headless commands (`list`, `outputs`, `run`,
//! `restore`). Hand-written to keep the binary small; the grammar is tiny.

use std::time::{Duration, Instant};

use anyhow::Context as _;
use sidestage_dsp::{Preset, SharedParams};
use sidestage_platform::{AudioApp, Platform, SessionConfig, SessionState, Support};

use crate::config::Config;
use crate::console::{say, tell};
use crate::demo::{DemoPlatform, Scenario};
use crate::instance::{self, Acquire, InstanceLock};
use crate::{processor, signal, text};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Gui { demo: bool },
    List,
    Outputs,
    Run(RunArgs),
    Restore,
    Help,
    Version,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunArgs {
    /// Part of the app's name or key, e.g. "spotify".
    pub query: String,
    /// Use this preset instead of the saved settings.
    pub preset: Option<Preset>,
    /// Stop after this many seconds.
    pub seconds: Option<f64>,
}

pub const PRESET_NAMES: &str = "behind | next-room | far | above | desk | lounge";

pub fn help() -> String {
    format!(
        "Sidestage {version}
Put one app's audio (your music) behind you in 3D while your game audio stays untouched.
Headphones only.

Usage:
  sidestage                       Open the app
  sidestage --demo                Open the app with simulated apps and audio (nothing is captured)
  sidestage list                  List apps that have audio right now
  sidestage outputs               List output devices
  sidestage run <app> [options]   Spatialize <app> without opening the window
      --preset <name>             {PRESET_NAMES} (default: your saved settings)
      --seconds <n>               Stop after n seconds (default: until Ctrl+C)
  sidestage restore               Undo audio changes left behind if Sidestage was killed
  sidestage --help                Show this help
  sidestage --version             Show the version

<app> is any part of an app's name or key as shown by 'sidestage list', e.g. spotify.
'run' uses the output device and hide method saved by the app.
Set RUST_LOG=debug for detailed logs, SIDESTAGE_DEMO=1 to try the commands with simulated apps.",
        version = env!("CARGO_PKG_VERSION")
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
        "restore" => no_more(Command::Restore),
        "run" => parse_run(&rest).map(Command::Run),
        other if other.starts_with('-') => Err(format!("unknown option '{other}'")),
        other => Err(format!("unknown command '{other}'")),
    }
}

fn parse_run(args: &[&str]) -> Result<RunArgs, String> {
    let mut words = Vec::new();
    let mut preset = None;
    let mut seconds = None;
    let mut i = 0;
    let mut options_done = false;
    while i < args.len() {
        let arg = args[i];
        i += 1;
        if options_done || !arg.starts_with("--") {
            words.push(arg);
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
            "--preset" => {
                let v = value()?;
                preset = Some(parse_preset(v).ok_or_else(|| format!("unknown preset '{v}' (use {PRESET_NAMES})"))?);
            }
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
    let query = words.join(" ").trim().to_owned();
    if query.is_empty() {
        return Err("'run' needs the name of an app, e.g. 'sidestage run spotify'".into());
    }
    Ok(RunArgs { query, preset, seconds })
}

pub fn parse_preset(name: &str) -> Option<Preset> {
    let n: String = name.trim().to_lowercase().chars().map(|c| if c == '_' || c == ' ' { '-' } else { c }).collect();
    Some(match n.as_str() {
        "behind" | "behind-you" => Preset::BehindYou,
        "next-room" | "nextroom" => Preset::NextRoom,
        "far" | "far-away" => Preset::FarAway,
        "above" => Preset::Above,
        "desk" | "desk-speaker" => Preset::DeskSpeaker,
        "lounge" => Preset::Lounge,
        _ => return None,
    })
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
    if std::env::var_os("SIDESTAGE_DEMO").is_some_and(|v| !v.is_empty() && v != "0") {
        return Ok(Box::new(DemoPlatform::new(Scenario::default(), None)));
    }
    sidestage_platform::platform()
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
    if std::env::var_os("SIDESTAGE_DEMO").is_some_and(|v| !v.is_empty() && v != "0") {
        return Ok(None);
    }
    match instance::acquire()? {
        Acquire::Acquired(lock) => Ok(Some(lock)),
        Acquire::AlreadyRunning => anyhow::bail!(instance::ALREADY_RUNNING),
    }
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

    let config_path = Config::default_path().ok().filter(|p| p.exists());
    let config = config_path.as_deref().map(Config::load).unwrap_or_default();
    let settings = args.preset.map_or(config.settings, Preset::settings);
    let deadline = args.seconds.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let expired = || signal::stop_requested() || deadline.is_some_and(|d| Instant::now() >= d);

    let Some(app) = wait_for_app(platform.as_mut(), &args.query, &expired)? else {
        if signal::stop_requested() {
            say!("Cancelled.");
            return Ok(());
        }
        anyhow::bail!("no app matching '{}' played audio in time; nothing was started", args.query);
    };
    if signal::stop_requested() {
        // Ctrl+C right as the app showed up: don't hide it only to restore it again.
        say!("Cancelled.");
        return Ok(());
    }

    let params = SharedParams::new(settings);
    let session = platform
        .start(
            SessionConfig {
                app: app.key.clone(),
                output: config.output.clone(),
                hide: config.hide.clone(),
                sidechain: config.sidechain,
            },
            processor::factory(params),
        )
        .with_context(|| format!("starting to capture {}", app.name))?;
    let how = match (args.preset, &config_path) {
        (Some(p), _) => format!("the '{}' preset", p.name()),
        (None, Some(_)) => "your saved settings".to_owned(),
        (None, None) => "the default settings".to_owned(),
    };
    say!("Spatializing {} with {how}. Press Ctrl+C to stop.", app.name);

    let mut last_state = None;
    let mut last_print = Instant::now();
    let mut warned: Vec<String> = Vec::new();
    let mut failure = None;
    while !expired() {
        let st = session.status();
        if last_state.as_ref() != Some(&st.state) || last_print.elapsed() >= Duration::from_secs(10) {
            say!("{}", text::status_summary(&st));
            last_print = Instant::now();
            last_state = Some(st.state.clone());
        }
        for w in st.warnings.iter().filter(|w| !warned.contains(w)) {
            say!("warning: {w}");
        }
        warned = st.warnings.clone();
        match st.state {
            SessionState::Error(e) => {
                failure = Some(e);
                break;
            }
            SessionState::Stopped => {
                failure = Some(format!("capturing {} stopped unexpectedly", app.name));
                break;
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if signal::stop_requested() {
        say!("Stopping\u{2026} (press Ctrl+C again to quit at once)");
    }
    let stopped = session.stop();
    match (failure, stopped) {
        (Some(e), stop) => {
            if let Err(se) = stop {
                tell!("error while stopping: {se:#}");
            }
            anyhow::bail!("{e}")
        }
        (None, Err(e)) => Err(e).context("stopping the session"),
        (None, Ok(())) => {
            say!("Stopped. {} plays normally again.", app.name);
            Ok(())
        }
    }
}

/// Poll until an app matching `query` has audio. `None` if interrupted first.
fn wait_for_app(
    platform: &mut dyn Platform,
    query: &str,
    expired: &dyn Fn() -> bool,
) -> anyhow::Result<Option<AudioApp>> {
    let mut announced = false;
    // The first listing must work; later hiccups (the sound server restarting) only delay.
    let mut listed_once = false;
    let mut failing = false;
    loop {
        let mut apps = match platform.list_apps() {
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
        match match_app(&apps, query) {
            AppMatch::Found(i) => return Ok(Some(apps.swap_remove(i))),
            AppMatch::Ambiguous(names) => {
                anyhow::bail!("'{query}' matches several apps: {}. Please be more specific.", names.join(", "))
            }
            AppMatch::None if !announced => {
                let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
                let now = if names.is_empty() { "none".to_owned() } else { names.join(", ") };
                say!(
                    "Waiting for an app matching '{query}' to play audio (apps with audio now: {now}). Ctrl+C to cancel."
                );
                announced = true;
            }
            AppMatch::None => {}
        }
        for _ in 0..10 {
            if expired() {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidestage_platform::AppKey;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse(&[]), Ok(Command::Gui { demo: false }));
        assert_eq!(parse(&args("--demo")), Ok(Command::Gui { demo: true }));
        assert_eq!(parse(&args("list")), Ok(Command::List));
        assert_eq!(parse(&args("outputs")), Ok(Command::Outputs));
        assert_eq!(parse(&args("restore")), Ok(Command::Restore));
        assert_eq!(parse(&args("--help")), Ok(Command::Help));
        assert_eq!(parse(&args("run --help")), Ok(Command::Help));
        assert_eq!(parse(&args("-V")), Ok(Command::Version));
        assert!(parse(&args("list extra")).is_err());
        assert!(parse(&args("frobnicate")).is_err());
        assert!(parse(&args("--frobnicate")).is_err());
    }

    #[test]
    fn parses_run() {
        let run = |s: &str| match parse(&args(s)) {
            Ok(Command::Run(r)) => Ok(r),
            Ok(other) => panic!("{other:?}"),
            Err(e) => Err(e),
        };
        assert_eq!(run("run spotify"), Ok(RunArgs { query: "spotify".into(), preset: None, seconds: None }));
        assert_eq!(
            run("run apple music --preset next-room --seconds 2.5"),
            Ok(RunArgs { query: "apple music".into(), preset: Some(Preset::NextRoom), seconds: Some(2.5) })
        );
        assert_eq!(
            run("run --preset=far --seconds=10 vlc").map(|r| (r.query, r.preset)),
            Ok(("vlc".into(), Some(Preset::FarAway)))
        );
        assert_eq!(run("run -- --weird-name").map(|r| r.query), Ok("--weird-name".into()));
        assert_eq!(run("run -- --help").map(|r| r.query), Ok("--help".into()), "after -- it is an app name");
        assert!(run("run").is_err());
        assert!(run("run spotify --preset").is_err());
        assert!(run("run spotify --preset loud").is_err());
        assert!(run("run spotify --seconds -3").is_err());
        assert!(run("run spotify --seconds abc").is_err());
        assert!(run("run spotify --volume 3").is_err());
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
            ("Behind You", Preset::BehindYou),
            ("desk_speaker", Preset::DeskSpeaker),
        ] {
            assert_eq!(parse_preset(name), Some(p), "{name}");
        }
        assert_eq!(parse_preset("loud"), None);
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
}
