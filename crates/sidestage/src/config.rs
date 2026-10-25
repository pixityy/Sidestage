//! The user's saved choices: the sources (which apps, each with its own sound settings), the
//! apps that never count as the game for ducking, output device, how the originals are hidden,
//! window size. Stored as JSON in the per-user config directory (or the one `SIDESTAGE_CONFIG_DIR`
//! names, see [`CONFIG_DIR_ENV`]).
//!
//! Loading never fails: a missing file gives defaults. A file with values this version cannot
//! read (damaged, or written by a newer Sidestage) keeps every field that still reads and uses
//! defaults for the rest; the original is kept next to it as `config.json.broken` so nothing the
//! user set up is silently lost. A file from a newer Sidestage is also kept, as
//! `config.json.newer`: the options this version does not know are gone once it saves. A version
//! 0.1 file (one app, one set of settings) becomes one source, and the original is kept as
//! `config.json.v1` for going back to 0.1.
//!
//! Saving happens on a background thread: the write is flushed to disk, which can take a while
//! on a busy drive, and the window must not stall meanwhile.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sidestage_dsp::{Ducking, Preset, PresetKind, Settings, make_footstep_safe};
use sidestage_platform::{AppKey, HideMethod, SourceId};

/// Smallest window that still fits the three columns.
pub const MIN_WINDOW: [f32; 2] = [760.0, 520.0];
pub const DEFAULT_WINDOW: [f32; 2] = [1000.0, 660.0];

/// The config layout this version writes. Files without a version are from Sidestage 0.1.
pub const CONFIG_VERSION: u32 = 2;
/// Most sources one session plays. Each is a capture, a renderer and a puck on the radar; more
/// would crowd the radar long before it strained the CPU.
pub const MAX_SOURCES: usize = 8;
/// Number of radar colours a source can have (see `ui::theme::Palette::pucks`).
pub const SOURCE_COLORS: u8 = 8;
/// Source IDs above this are renumbered on load (see [`Config::sanitized`]). One new ID per
/// added source, so a real config never gets anywhere near it.
const MAX_SANE_ID: SourceId = 1 << 30;

/// How long settings must stay unchanged before they are written (slider drags change them
/// every frame).
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(1500);
/// How long closing the window waits for the last save.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

/// An app the user picked (as a source, or for the ducking ignore list). The name is kept so the
/// UI can say "Waiting for Spotify" while the app is not running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChosenApp {
    pub key: AppKey,
    pub name: String,
}

impl Default for ChosenApp {
    fn default() -> Self {
        Self { key: AppKey(String::new()), name: String::new() }
    }
}

/// Whether two app keys name the same app, compared the way the backend finds a saved app.
///
/// On Windows a key is an executable path, and Store and Squirrel apps (Spotify, Discord) move to
/// a new versioned folder with every update. After an update the app list shows a new key for
/// the app the user picked; an exact comparison would then wait forever for an app that is
/// playing.
pub fn same_app(a: &AppKey, b: &AppKey) -> bool {
    a.same_app(b)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    /// Follow the OS light/dark setting (dark when it is unknown).
    #[default]
    System,
    Dark,
    Light,
}

/// What "Footstep-safe" overrode, so turning it off restores the user's own values.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FootstepUndo {
    pub presence_dip_db: f32,
    pub lowpass_hz: f32,
    pub bass_db: f32,
    pub ducking: Ducking,
}

impl FootstepUndo {
    pub fn capture(s: &Settings) -> Self {
        Self { presence_dip_db: s.presence_dip_db, lowpass_hz: s.lowpass_hz, bass_db: s.bass_db, ducking: s.ducking }
    }

    pub fn apply(self, s: Settings) -> Settings {
        Settings {
            presence_dip_db: self.presence_dip_db,
            lowpass_hz: self.lowpass_hz,
            bass_db: self.bass_db,
            ducking: self.ducking,
            ..s
        }
    }
}

/// One app in the session with its own sound.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    /// Stable for as long as the source exists; never reused for another source (see
    /// [`Config::next_id`]), because the backend keeps a source's renderer while its ID stays.
    pub id: SourceId,
    pub app: ChosenApp,
    /// Off: the app plays normally and the source keeps its settings.
    pub enabled: bool,
    /// Index of its radar colour, `0..SOURCE_COLORS`.
    pub color: u8,
    /// Sound settings (never saved with `bypass` on: that is a hold-to-compare button).
    pub settings: Settings,
    /// Preset the settings started from; double-clicking the radar returns to its position.
    pub base_preset: Preset,
    pub footstep_undo: Option<FootstepUndo>,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            id: 0,
            app: ChosenApp::default(),
            enabled: true,
            color: 0,
            settings: Settings::default(),
            base_preset: Preset::BehindYou,
            footstep_undo: None,
        }
    }
}

impl Source {
    /// Footstep-safe is "on" when applying it again would change nothing.
    pub fn footstep_safe(&self) -> bool {
        make_footstep_safe(self.settings) == self.settings
    }

    pub fn set_footstep_safe(&mut self, on: bool) {
        if on == self.footstep_safe() {
            return;
        }
        if on {
            self.footstep_undo = Some(FootstepUndo::capture(&self.settings));
            self.settings = make_footstep_safe(self.settings);
        } else {
            let undo = self.footstep_undo.take().unwrap_or_else(|| FootstepUndo::capture(&self.base_preset.settings()));
            self.settings = undo.apply(self.settings);
        }
    }

    /// Switch to a preset, keeping Footstep-safe on if it was.
    pub fn apply_preset(&mut self, preset: Preset) {
        let footstep = self.footstep_safe();
        self.base_preset = preset;
        self.settings = preset.settings();
        self.footstep_undo = None;
        if footstep {
            self.set_footstep_safe(true);
        }
    }
}

/// Sound settings from a version 0.1 config that had no app chosen yet. The first music source
/// the user adds gets them, so nothing they tuned is lost.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CarriedSound {
    pub settings: Settings,
    pub base_preset: Preset,
    pub footstep_undo: Option<FootstepUndo>,
}

impl Default for CarriedSound {
    fn default() -> Self {
        Self { settings: Settings::default(), base_preset: Preset::BehindYou, footstep_undo: None }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// [`CONFIG_VERSION`] when written by this version.
    pub version: u32,
    /// The power switch.
    pub enabled: bool,
    /// In the order the user added them.
    pub sources: Vec<Source>,
    /// The source the right panel and the radar edit.
    pub selected: Option<SourceId>,
    /// The ID the next new source gets (always above every ID in use).
    pub next_id: SourceId,
    /// Apps whose sound never turns the sources down (a browser video, a second music player):
    /// they do not count as the game for ducking.
    pub ducking_ignore: Vec<ChosenApp>,
    pub carried_sound: Option<CarriedSound>,
    /// Output device ID, `None` = follow the system default.
    pub output: Option<String>,
    pub hide: HideMethod,
    /// Measure "everything except the sources" so ducking can react to the game.
    pub sidechain: bool,
    /// Inner window size in points.
    pub window_size: [f32; 2],
    pub theme: ThemeChoice,
    /// One-time hint cards the user closed.
    pub dismissed_hints: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            enabled: true,
            sources: Vec::new(),
            selected: None,
            next_id: 1,
            ducking_ignore: Vec::new(),
            carried_sound: None,
            output: None,
            hide: HideMethod::Auto,
            sidechain: true,
            window_size: DEFAULT_WINDOW,
            theme: ThemeChoice::System,
            dismissed_hints: Vec::new(),
        }
    }
}

/// Environment variable that moves the config directory (where `config.json` and its backups
/// live) elsewhere, so test runs and portable setups never read or change the user's saved
/// settings. Its companion `SIDESTAGE_STATE_DIR` ([`sidestage_platform::journal::STATE_DIR_ENV`])
/// moves the state directory: crash-recovery journals, the instance lock and the log. A test run
/// that must not touch a Sidestage the user is running sets both.
pub const CONFIG_DIR_ENV: &str = "SIDESTAGE_CONFIG_DIR";

impl Config {
    /// `<config dir>/config.json`, e.g. `%APPDATA%\Sidestage\config\config.json` on Windows, or
    /// `$SIDESTAGE_CONFIG_DIR/config.json` when [`CONFIG_DIR_ENV`] is set.
    pub fn default_path() -> anyhow::Result<PathBuf> {
        Ok(config_dir(std::env::var_os(CONFIG_DIR_ENV))?.join("config.json"))
    }

    /// Clamp everything into range, drop transient state and repair what a damaged file or a
    /// hand edit could have left inconsistent (repeated apps or IDs, a selection that points
    /// nowhere).
    pub fn sanitized(mut self) -> Self {
        self.version = CONFIG_VERSION;
        for (v, (min, default)) in self.window_size.iter_mut().zip(MIN_WINDOW.iter().zip(DEFAULT_WINDOW)) {
            *v = if v.is_finite() { v.clamp(*min, 8192.0) } else { default };
        }
        if self.output.as_ref().is_some_and(String::is_empty) {
            self.output = None;
        }

        let mut sources: Vec<Source> = Vec::with_capacity(self.sources.len().min(MAX_SOURCES));
        for mut s in std::mem::take(&mut self.sources) {
            if s.app.key.0.is_empty() {
                log::warn!("dropping a saved source without an app");
                continue;
            }
            if sources.iter().any(|k| same_app(&k.app.key, &s.app.key)) {
                log::warn!("{} is saved as a source twice; keeping the first", s.app.name);
                continue;
            }
            if sources.len() == MAX_SOURCES {
                log::warn!("more than {MAX_SOURCES} saved sources; dropping {}", s.app.name);
                continue;
            }
            s.settings = Settings { bypass: false, ..s.settings.sanitized() };
            if s.app.name.trim().is_empty() {
                s.app.name = name_from_key(&s.app.key);
            }
            sources.push(s);
        }
        // IDs only need to be unique; a hand-edited file with huge ones would run out of new
        // IDs (and hand out duplicates) after a few additions, so those start over from 1.
        if sources.iter().map(|s| s.id).chain([self.next_id]).any(|id| id > MAX_SANE_ID) {
            log::warn!("renumbering the saved sources (their IDs were out of range)");
            for (s, id) in sources.iter_mut().zip(1..) {
                s.id = id;
            }
            self.next_id = 1;
        }
        let max_id = sources.iter().map(|s| s.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(max_id.saturating_add(1)).max(1);
        for i in 0..sources.len() {
            let (before, rest) = sources.split_at_mut(i);
            let s = &mut rest[0];
            if s.id == 0 || before.iter().any(|b| b.id == s.id) {
                s.id = self.next_id;
                self.next_id = self.next_id.saturating_add(1);
            }
            if s.color >= SOURCE_COLORS || before.iter().any(|b| b.color == s.color) {
                s.color = first_free_color(before);
            }
        }
        self.sources = sources;
        if !self.selected.is_some_and(|id| self.source(id).is_some()) {
            self.selected = self.sources.first().map(|s| s.id);
        }

        let mut ignore: Vec<ChosenApp> = Vec::new();
        for mut a in std::mem::take(&mut self.ducking_ignore) {
            if !a.key.0.is_empty() && !ignore.iter().any(|b| same_app(&b.key, &a.key)) {
                if a.name.trim().is_empty() {
                    a.name = name_from_key(&a.key);
                }
                ignore.push(a);
            }
        }
        self.ducking_ignore = ignore;

        if let Some(c) = &mut self.carried_sound {
            c.settings = Settings { bypass: false, ..c.settings.sanitized() };
        }
        if self.carried_sound == Some(CarriedSound::default()) {
            self.carried_sound = None;
        }
        self
    }

    /// Load from `path`; any problem is logged and gives defaults for what could not be read.
    pub fn load(path: &Path) -> Config {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Config::default(),
            Err(e) => {
                log::warn!("could not read {}: {e}; using defaults", path.display());
                return Config::default();
            }
        };
        let mut value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(e) => {
                log::warn!("{} is not valid JSON ({e}); using defaults", path.display());
                keep_copy(path, "json.broken");
                return Config::default();
            }
        };
        if is_v1(&value) {
            log::info!("upgrading the settings from Sidestage 0.1");
            keep_copy(path, "json.v1");
            value = migrate_v1(&value);
        } else if let Some(newer) = value.get("version").and_then(Value::as_u64).filter(|v| *v > CONFIG_VERSION.into())
        {
            // Its options this version does not know would be gone with the next save.
            log::warn!("{} was written by a newer Sidestage (layout {newer})", path.display());
            keep_copy(path, "json.newer");
        }
        match serde_json::from_value::<Config>(value.clone()) {
            Ok(config) => config.sanitized(),
            Err(e) => {
                let (config, skipped) = recover_fields(&value);
                log::warn!(
                    "{} has values this version cannot read ({e}); using defaults for: {}",
                    path.display(),
                    skipped.join(", ")
                );
                keep_copy(path, "json.broken");
                config.sanitized()
            }
        }
    }

    /// Write atomically (temp file + rename), creating the directory if needed.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(&self.clone().sanitized())?;
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&bytes).with_context(|| format!("writing {}", tmp.display()))?;
            f.sync_all().with_context(|| format!("flushing {}", tmp.display()))?;
        }
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }

    pub fn source(&self, id: SourceId) -> Option<&Source> {
        self.sources.iter().find(|s| s.id == id)
    }

    pub fn source_mut(&mut self, id: SourceId) -> Option<&mut Source> {
        self.sources.iter_mut().find(|s| s.id == id)
    }

    pub fn selected_source(&self) -> Option<&Source> {
        self.selected.and_then(|id| self.source(id))
    }

    pub fn selected_source_mut(&mut self) -> Option<&mut Source> {
        let id = self.selected?;
        self.source_mut(id)
    }

    /// The source that captures this app, if any.
    pub fn source_for(&self, key: &AppKey) -> Option<&Source> {
        self.sources.iter().find(|s| same_app(&s.app.key, key))
    }

    pub fn is_ignored(&self, key: &AppKey) -> bool {
        self.ducking_ignore.iter().any(|a| same_app(&a.key, key))
    }

    pub fn is_full(&self) -> bool {
        self.sources.len() >= MAX_SOURCES
    }

    /// Add `app` as a new, selected source with the preset that suits it (see
    /// [`default_preset_for`]). `None` when the session is full or the app is already a source.
    pub fn add_source(&mut self, app: ChosenApp) -> Option<SourceId> {
        if self.is_full() || app.key.0.is_empty() || self.source_for(&app.key).is_some() {
            return None;
        }
        let mut source = Source { id: self.take_id(), color: first_free_color(&self.sources), ..Source::default() };
        let preset = default_preset_for(&app, &self.sources);
        match self.carried_sound.take() {
            Some(c) if preset.kind() == PresetKind::Music => {
                source.settings = c.settings;
                source.base_preset = c.base_preset;
                source.footstep_undo = c.footstep_undo;
            }
            carried => {
                self.carried_sound = carried;
                source.apply_preset(preset);
            }
        }
        source.app = app;
        let id = source.id;
        self.sources.push(source);
        self.selected = Some(id);
        Some(id)
    }

    /// Remove a source (its app then plays normally again). Returns it, for undo.
    pub fn remove_source(&mut self, id: SourceId) -> Option<Source> {
        let index = self.sources.iter().position(|s| s.id == id)?;
        let removed = self.sources.remove(index);
        if self.selected == Some(id) {
            let next = index.min(self.sources.len().saturating_sub(1));
            self.selected = self.sources.get(next).map(|s| s.id);
        }
        Some(removed)
    }

    /// Put a removed source back (undo). False when it no longer fits.
    pub fn restore_source(&mut self, mut source: Source) -> bool {
        if self.is_full() || self.source_for(&source.app.key).is_some() {
            return false;
        }
        if self.source(source.id).is_some() {
            source.id = self.take_id();
        }
        if self.sources.iter().any(|s| s.color == source.color) {
            source.color = first_free_color(&self.sources);
        }
        self.selected = Some(source.id);
        self.sources.push(source);
        true
    }

    fn take_id(&mut self) -> SourceId {
        let max_id = self.sources.iter().map(|s| s.id).max().unwrap_or(0);
        let id = self.next_id.max(max_id.saturating_add(1)).max(1);
        self.next_id = id.saturating_add(1);
        id
    }
}

/// The config directory: `overridden` (the value of [`CONFIG_DIR_ENV`]) when it is set and not
/// empty, else the per-user default. Not created here; saving creates it.
fn config_dir(overridden: Option<std::ffi::OsString>) -> anyhow::Result<PathBuf> {
    if let Some(dir) = overridden.filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let dirs =
        directories::ProjectDirs::from("", "", "Sidestage").context("could not determine the user's home directory")?;
    Ok(dirs.config_dir().to_path_buf())
}

/// A readable name for an app whose saved name was lost: its key's file name without the
/// extension ("Spotify" for `C:\...\Spotify.exe`, "com.spotify.client" for a bundle ID).
fn name_from_key(key: &AppKey) -> String {
    let file = key.0.rsplit(['\\', '/']).next().unwrap_or_default();
    let split = file.len().saturating_sub(4);
    let stem = match (file.get(..split), file.get(split..)) {
        (Some(stem), Some(ext)) if ext.eq_ignore_ascii_case(".exe") => stem,
        _ => file,
    };
    if stem.trim().is_empty() { key.0.clone() } else { stem.to_owned() }
}

fn first_free_color(sources: &[Source]) -> u8 {
    (0..SOURCE_COLORS).find(|c| !sources.iter().any(|s| s.color == *c)).unwrap_or(0)
}

/// Whether the app is a voice-chat app (Discord, TeamSpeak ...), judged by the words of its name
/// and of its key's file name, so "Microsoft Teams" and `ms-teams.exe` count but "Steam" does not.
pub fn is_voice_chat_app(app: &ChosenApp) -> bool {
    const EXACT: [&str; 16] = [
        "mumble",
        "ventrilo",
        "guilded",
        "zoom",
        "teams",
        "msteams",
        "skype",
        "webex",
        "slack",
        "vesktop",
        "legcord",
        "armcord",
        "whatsapp",
        "telegram",
        "jitsi",
        "ts3client",
    ];
    const PREFIX: [&str; 3] = ["discord", "teamspeak", "ts3client"];
    let file = app.key.0.rsplit(['\\', '/']).next().unwrap_or_default();
    let text = format!("{} {}", app.name, file).to_lowercase();
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .any(|w| EXACT.contains(&w) || PREFIX.iter().any(|p| w.starts_with(p)))
}

/// The preset a newly added source starts with. Voice chat goes to the right, or to the next
/// voice position (left, over the shoulder, radio front-left) where no other source is yet, so
/// two talkers never land on the same spot; anything else goes behind you, or to the first
/// music preset no other source uses yet.
pub fn default_preset_for(app: &ChosenApp, others: &[Source]) -> Preset {
    /// A source this close in direction, and within this distance ratio, takes a voice position.
    const NEAR_DEG: f32 = 30.0;
    const NEAR_RATIO: f32 = 2.5;
    if is_voice_chat_app(app) {
        let taken = |p: Preset| {
            let spot = p.settings();
            others.iter().any(|s| {
                let d = (s.settings.azimuth_deg - spot.azimuth_deg).rem_euclid(360.0);
                let ratio = s.settings.distance_m.max(0.1) / spot.distance_m.max(0.1);
                d.min(360.0 - d) < NEAR_DEG && ratio.max(1.0 / ratio) < NEAR_RATIO
            })
        };
        return [Preset::VoiceRight, Preset::VoiceLeft, Preset::OverShoulder, Preset::RadioComms]
            .into_iter()
            .find(|p| !taken(*p))
            .unwrap_or(Preset::VoiceRight);
    }
    Preset::MUSIC.into_iter().find(|p| !others.iter().any(|s| s.base_preset == *p)).unwrap_or(Preset::BehindYou)
}

/// Copy the config file next to itself before it is overwritten: `config.json.broken` and
/// `config.json.newer` (replaced each time) or `config.json.v1` (written once: later copies would
/// already be version 2).
fn keep_copy(path: &Path, extension: &str) {
    let backup = path.with_extension(extension);
    let once = extension == "json.v1";
    if once && backup.exists() {
        return;
    }
    match fs::copy(path, &backup) {
        Ok(_) if once => log::info!("the 0.1 settings file is kept as {}", backup.display()),
        Ok(_) => log::warn!("the original file is kept as {}", backup.display()),
        Err(e) => log::warn!("could not back up {}: {e}", path.display()),
    }
}

/// A Sidestage 0.1 file: no version and no sources.
fn is_v1(file: &Value) -> bool {
    file.is_object() && file.get("version").is_none() && file.get("sources").is_none()
}

/// Rewrite a 0.1 config (`app` + `settings` + `base_preset` + `footstep_undo` at the top level)
/// as version 2 with that app as its only source. Values are moved, not parsed, so the normal
/// per-field recovery still applies to them afterwards.
fn migrate_v1(file: &Value) -> Value {
    let Some(old) = file.as_object() else { return file.clone() };
    let mut new = old.clone();
    let mut sound = Map::new();
    for field in ["settings", "base_preset", "footstep_undo"] {
        if let Some(v) = new.remove(field) {
            sound.insert(field.to_owned(), v);
        }
    }
    let app = new.remove("app").filter(|a| a.get("key").and_then(Value::as_str).is_some_and(|k| !k.is_empty()));
    match app {
        Some(app) => {
            let mut source = sound;
            source.insert("id".into(), 1.into());
            source.insert("app".into(), app);
            source.insert("enabled".into(), true.into());
            source.insert("color".into(), 0.into());
            new.insert("sources".into(), Value::Array(vec![Value::Object(source)]));
            new.insert("selected".into(), 1.into());
            new.insert("next_id".into(), 2.into());
        }
        None if !sound.is_empty() => {
            new.insert("carried_sound".into(), Value::Object(sound));
        }
        None => {}
    }
    new.insert("version".into(), CONFIG_VERSION.into());
    Value::Object(new)
}

/// Start from the defaults and take over the file's values one field at a time, descending into
/// nested objects (`settings`, `settings.ducking`) and into each source, so one value this
/// version cannot read (a damaged number, an option added by a newer version) only resets that
/// value, and one damaged source does not take the others with it. Returns the config and the
/// JSON pointers of the values left at their defaults.
fn recover_fields(file: &Value) -> (Config, Vec<String>) {
    let mut skipped = Vec::new();
    let Some(fields) = file.as_object() else { return (Config::default(), vec!["/".into()]) };
    let mut rest = fields.clone();
    let sources = rest.remove("sources");
    let ignore = rest.remove("ducking_ignore");
    let mut config: Config = recover(&Value::Object(rest), "", &mut skipped);
    if let Some(items) = sources {
        match items.as_array() {
            Some(items) => {
                config.sources = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| recover::<Source>(item, &format!("/sources/{i}"), &mut skipped))
                    .collect();
            }
            None => skipped.push("/sources".into()),
        }
    }
    if let Some(items) = ignore {
        match items.as_array() {
            Some(items) => {
                for (i, item) in items.iter().enumerate() {
                    match serde_json::from_value::<ChosenApp>(item.clone()) {
                        Ok(app) => config.ducking_ignore.push(app),
                        Err(_) => skipped.push(format!("/ducking_ignore/{i}")),
                    }
                }
            }
            None => skipped.push("/ducking_ignore".into()),
        }
    }
    (config, skipped)
}

/// `T::default()` with every value of `file` that reads as part of a `T` taken over.
/// `at` is the JSON pointer of `file` in the whole document, for the list of skipped values.
fn recover<T: Default + Serialize + DeserializeOwned>(file: &Value, at: &str, skipped: &mut Vec<String>) -> T {
    let Ok(mut merged) = serde_json::to_value(T::default()) else {
        skipped.push(if at.is_empty() { "/".into() } else { at.into() });
        return T::default();
    };
    if file.is_object() {
        overlay::<T>(&mut merged, "", file, at, skipped);
    } else {
        skipped.push(if at.is_empty() { "/".into() } else { at.into() });
    }
    serde_json::from_value(merged).unwrap_or_else(|_| {
        skipped.push(if at.is_empty() { "/".into() } else { at.into() });
        T::default()
    })
}

fn overlay<T: DeserializeOwned>(root: &mut Value, at: &str, file: &Value, prefix: &str, skipped: &mut Vec<String>) {
    let Some(fields) = file.as_object() else { return };
    for (key, value) in fields {
        let pointer = format!("{at}/{}", key.replace('~', "~0").replace('/', "~1"));
        // Fields this version does not know are ignored, as serde does.
        let Some(slot) = root.pointer(&pointer) else { continue };
        if slot.is_object() && value.is_object() {
            overlay::<T>(root, &pointer, value, prefix, skipped);
            continue;
        }
        let Some(slot) = root.pointer_mut(&pointer) else { continue };
        let old = std::mem::replace(slot, value.clone());
        if serde_json::from_value::<T>(root.clone()).is_err() {
            if let Some(slot) = root.pointer_mut(&pointer) {
                *slot = old;
            }
            skipped.push(format!("{prefix}{pointer}"));
        }
    }
}

/// Saves a [`Config`] some time after it stops changing, and on demand, on a background thread.
/// `path: None` keeps the config in memory only (demo and screenshot mode).
pub struct ConfigStore {
    path: Option<PathBuf>,
    saved: Config,
    /// The unsaved config last seen by [`Self::update`] and when it was first seen.
    pending: Option<(Config, Instant)>,
    /// Started with the first save.
    writer: Option<Writer>,
}

/// The background thread that writes the file. Only the newest queued config is written.
struct Writer {
    tx: Option<Sender<Config>>,
    /// One message per queued config that has been dealt with (written or superseded).
    done: Receiver<()>,
    queued: usize,
    thread: Option<JoinHandle<()>>,
}

impl Writer {
    fn spawn(path: PathBuf) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<Config>();
        let (done_tx, done) = mpsc::channel();
        let thread = std::thread::Builder::new().name("sidestage-config".into()).spawn(move || {
            while let Ok(mut config) = rx.recv() {
                let mut handled = 1;
                while let Ok(newer) = rx.try_recv() {
                    config = newer;
                    handled += 1;
                }
                match config.save(&path) {
                    Ok(()) => log::debug!("saved {}", path.display()),
                    Err(e) => log::warn!("could not save settings: {e:#}"),
                }
                for _ in 0..handled {
                    let _ = done_tx.send(());
                }
            }
        })?;
        Ok(Self { tx: Some(tx), done, queued: 0, thread: Some(thread) })
    }

    /// Wait until everything queued so far is on disk. False on timeout.
    fn wait(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while self.queued > 0 {
            match self.done.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(()) => self.queued -= 1,
                Err(RecvTimeoutError::Timeout) => return false,
                Err(RecvTimeoutError::Disconnected) => self.queued = 0,
            }
        }
        true
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Let the thread finish the last save, but never hang the exit on a stuck drive.
        if !self.wait(FLUSH_TIMEOUT) {
            log::warn!("the settings file is still being written; not waiting any longer");
            return;
        }
        self.tx = None;
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            log::error!("the settings writer panicked");
        }
    }
}

impl ConfigStore {
    pub fn new(path: Option<PathBuf>, loaded: &Config) -> Self {
        Self { path, saved: loaded.clone(), pending: None, writer: None }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Call regularly with the current config; saves once it has been unchanged for
    /// [`SAVE_DEBOUNCE`], so a long slider drag is written once, at the end. Never blocks.
    /// Returns true when it started a save.
    pub fn update(&mut self, current: &Config, now: Instant) -> bool {
        if *current == self.saved {
            self.pending = None;
            return false;
        }
        match &self.pending {
            Some((pending, since)) if pending == current => {
                if now.saturating_duration_since(*since) >= SAVE_DEBOUNCE {
                    return self.save(current);
                }
            }
            _ => self.pending = Some((current.clone(), now)),
        }
        false
    }

    /// Save now if anything changed, and wait (at most [`FLUSH_TIMEOUT`]) until every save is on
    /// disk. For closing the window. Returns true when it saved something new.
    pub fn flush(&mut self, current: &Config) -> bool {
        let saved = self.save(current);
        if let Some(w) = &mut self.writer
            && !w.wait(FLUSH_TIMEOUT)
        {
            log::warn!("saving settings is taking long; continuing without waiting");
        }
        saved
    }

    /// Queue a save of `current` if it differs from the last one.
    fn save(&mut self, current: &Config) -> bool {
        self.pending = None;
        if *current == self.saved {
            return false;
        }
        self.saved = current.clone();
        let Some(path) = &self.path else { return false };
        if self.writer.is_none() {
            match Writer::spawn(path.clone()) {
                Ok(w) => self.writer = Some(w),
                Err(e) => log::warn!("no background saver ({e}); saving on this thread"),
            }
        }
        if let Some(w) = &mut self.writer
            && w.tx.as_ref().is_some_and(|tx| tx.send(current.clone()).is_ok())
        {
            w.queued += 1;
            return true;
        }
        match current.save(path) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("could not save settings: {e:#}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidestage_dsp::DuckMode;

    /// A fresh directory per test (tests run in parallel).
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sidestage-config-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn app(key: &str, name: &str) -> ChosenApp {
        ChosenApp { key: AppKey(key.into()), name: name.into() }
    }

    fn sample() -> Config {
        let mut c = Config {
            output: Some("{0.0.0.00000000}.{abc}".into()),
            hide: HideMethod::ParkOn("{0.0.0.00000000}.{def}".into()),
            sidechain: false,
            enabled: false,
            theme: ThemeChoice::Light,
            window_size: [1200.0, 800.0],
            ..Config::default()
        };
        let spotify = c.add_source(app(r"C:\Program Files\Spotify\Spotify.exe", "Spotify")).unwrap();
        c.source_mut(spotify).unwrap().apply_preset(Preset::NextRoom);
        c.source_mut(spotify).unwrap().settings.volume_db = -7.5;
        let discord = c.add_source(app(r"C:\Users\me\AppData\Local\Discord\app-1.0.9\Discord.exe", "Discord")).unwrap();
        c.source_mut(discord).unwrap().enabled = false;
        c.selected = Some(spotify);
        c.ducking_ignore.push(app(r"C:\Program Files\Mozilla Firefox\firefox.exe", "Firefox"));
        c.dismissed_hints.push("windows-mixer".into());
        c
    }

    #[test]
    fn store_app_keys_survive_updates() {
        let old = r"C:\Program Files\WindowsApps\SpotifyAB.SpotifyMusic_1.298.301.0_x64__zpdnekdrzrea0\Spotify.exe";
        let new = r"c:\program files\windowsapps\spotifyab.spotifymusic_1.304.258.0_x64__zpdnekdrzrea0\spotify.exe";
        let other = r"c:\program files\windowsapps\otherco.player_1.0.0.0_x64__abc\spotify.exe";
        let discord_old = r"c:\users\me\appdata\local\discord\app-1.0.9261\discord.exe";
        let discord_new = r"c:\users\me\appdata\local\discord\app-1.0.9302\discord.exe";

        let key = |s: &str| AppKey(s.into());
        assert!(same_app(&key("demo:spotify"), &key("demo:spotify")));
        assert!(!same_app(&key("spotify"), &key("firefox")));
        assert!(!same_app(&key(other), &key(new)));
        assert_eq!(same_app(&key(old), &key(new)), cfg!(windows), "only Windows keys are paths");
        assert_eq!(same_app(&key(discord_old), &key(discord_new)), cfg!(windows));

        // An updated app is still recognised as the saved source.
        let mut c = Config::default();
        c.add_source(app(old, "Spotify")).unwrap();
        assert_eq!(c.source_for(&key(new)).is_some(), cfg!(windows));
        assert_eq!(c.add_source(app(new, "Spotify")).is_none(), cfg!(windows), "no second source for one app");
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(Config::load(&dir.join("config.json")), Config::default());
    }

    #[test]
    fn config_dir_can_be_moved() {
        // The variable's value is passed in: setting it would race with the other tests.
        let dir = temp_dir("moved");
        assert_eq!(config_dir(Some(dir.clone().into_os_string())).unwrap(), dir);
        let default = config_dir(None).unwrap();
        assert_ne!(default, dir);
        assert_eq!(config_dir(Some(std::ffi::OsString::new())).unwrap(), default, "empty means unset");
        assert!(default.to_string_lossy().to_lowercase().contains("sidestage"), "{}", default.display());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested").join("config.json");
        let c = sample();
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path), c);
        assert!(!path.with_extension("json.tmp").exists(), "temp file must be renamed away");
        assert!(!path.with_extension("json.v1").exists(), "a version 2 file is not migrated");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bypass_is_never_saved() {
        let dir = temp_dir("bypass");
        let path = dir.join("config.json");
        let mut c = Config::default();
        let id = c.add_source(app("demo:spotify", "Spotify")).unwrap();
        c.source_mut(id).unwrap().settings.bypass = true;
        c.save(&path).unwrap();
        assert!(!Config::load(&path).sources[0].settings.bypass);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn broken_file_gives_defaults_and_is_kept() {
        let dir = temp_dir("broken");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, b"{ this is not json").unwrap();
        assert_eq!(Config::load(&path), Config::default());
        assert_eq!(fs::read(path.with_extension("json.broken")).unwrap(), b"{ this is not json");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn partial_and_out_of_range_values_are_fixed() {
        let dir = temp_dir("partial");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(
            &path,
            br#"{"version": 2, "sources": [{"id": 4, "app": {"key": "a", "name": "A"}, "settings": {"distance_m": 500.0}}],
                "window_size": [10.0, 99999.0], "output": ""}"#,
        )
        .unwrap();
        let c = Config::load(&path);
        let s = &c.sources[0];
        assert_eq!(s.settings.distance_m, 20.0);
        assert_eq!(s.settings.room, Settings::default().room, "missing fields get defaults");
        assert!(s.enabled);
        assert_eq!(c.selected, Some(4), "a selection that points nowhere picks the first source");
        assert_eq!(c.next_id, 5, "new IDs are above every saved one");
        assert_eq!(c.window_size, [MIN_WINDOW[0], 8192.0]);
        assert_eq!(c.output, None);
        assert!(c.enabled && c.sidechain);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn repeated_apps_ids_and_colors_are_repaired() {
        let mut c = Config { next_id: 1, ..Config::default() };
        let mk = |id, key: &str, color| Source { id, app: app(key, key), color, ..Source::default() };
        c.sources = vec![mk(3, "a", 1), mk(3, "b", 1), mk(0, "c", 99), mk(7, "a", 2), mk(8, "", 3)];
        let c = c.sanitized();
        let ids: Vec<_> = c.sources.iter().map(|s| s.id).collect();
        let colors: Vec<_> = c.sources.iter().map(|s| s.color).collect();
        let keys: Vec<_> = c.sources.iter().map(|s| s.app.key.0.as_str()).collect();
        assert_eq!(keys, ["a", "b", "c"], "the repeated app and the one without a key are dropped");
        assert_eq!(ids, [3, 4, 5]);
        assert_eq!(colors, [1, 0, 2]);
        assert_eq!(c.next_id, 6);
    }

    #[test]
    fn huge_ids_and_lost_names_are_repaired() {
        let mk = |id, key: &str, name: &str| Source { id, app: app(key, name), ..Source::default() };
        let mut c = Config { next_id: SourceId::MAX, ..Config::default() };
        c.sources = vec![mk(SourceId::MAX, r"C:\Apps\Player.EXE", ""), mk(SourceId::MAX - 1, "com.vendor.chat", " ")];
        c.ducking_ignore = vec![app("/usr/bin/firefox", "")];
        let mut c = c.sanitized();
        assert_eq!(c.sources.iter().map(|s| s.id).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(c.next_id, 3);
        let names: Vec<&str> = c.sources.iter().map(|s| s.app.name.as_str()).collect();
        assert_eq!(names, ["Player", "com.vendor.chat"]);
        assert_eq!(c.ducking_ignore[0].name, "firefox");
        // New sources keep getting IDs of their own.
        let a = c.add_source(app("a", "A")).unwrap();
        let b = c.add_source(app("b", "B")).unwrap();
        assert!(a != b && a > 2 && b > 2);
    }

    #[test]
    fn file_from_a_newer_version_is_kept() {
        let dir = temp_dir("newer");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let text = r#"{"version": 9, "enabled": false, "sources": [], "hologram_mode": true}"#;
        fs::write(&path, text).unwrap();
        let c = Config::load(&path);
        assert!(!c.enabled, "what still reads is used");
        assert_eq!(fs::read_to_string(path.with_extension("json.newer")).unwrap(), text);
        assert!(!path.with_extension("json.broken").exists() && !path.with_extension("json.v1").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn too_many_sources_are_capped() {
        let mut c = Config::default();
        for i in 0..MAX_SOURCES {
            assert!(c.add_source(app(&format!("app{i}"), "App")).is_some());
        }
        assert!(c.is_full());
        assert_eq!(c.add_source(app("one-more", "App")), None);
        let colors: std::collections::HashSet<u8> = c.sources.iter().map(|s| s.color).collect();
        assert_eq!(colors.len(), MAX_SOURCES, "every source has its own colour");
    }

    #[test]
    fn store_debounces_and_flushes() {
        let dir = temp_dir("store");
        let path = dir.join("config.json");
        let c0 = sample();
        let mut store = ConfigStore::new(Some(path.clone()), &c0);
        let t0 = Instant::now();
        assert!(!store.update(&c0, t0), "unchanged config is not written");

        let mut c1 = c0.clone();
        c1.sources[0].settings.distance_m = 3.0;
        assert!(!store.update(&c1, t0));
        // Still changing (a slider drag): the timer restarts.
        let mut c2 = c1.clone();
        c2.sources[0].settings.distance_m = 4.0;
        assert!(!store.update(&c2, t0 + SAVE_DEBOUNCE));
        assert!(!path.exists());
        assert!(!store.update(&c2, t0 + SAVE_DEBOUNCE + Duration::from_millis(100)));
        assert!(store.update(&c2, t0 + SAVE_DEBOUNCE * 2 + Duration::from_millis(1)));
        assert!(!store.flush(&c2), "already saved; flush only waits for it");
        assert_eq!(Config::load(&path), c2);

        let mut c3 = c2.clone();
        c3.enabled = true;
        assert!(store.flush(&c3), "flush writes immediately");
        assert_eq!(Config::load(&path), c3);
        assert!(!store.flush(&c3), "nothing new to write");
        drop(store);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_values_only_reset_themselves() {
        let dir = temp_dir("lenient");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let text = br#"{
            "version": 2,
            "sources": [
                {"id": 1, "app": {"key": "C:\\Apps\\X.exe", "name": "X"},
                 "settings": {"distance_m": 3.0, "room": "Garage", "ducking": {"threshold_db": -40.0, "depth_db": "lots"}},
                 "base_preset": "SomethingNew"},
                "not a source",
                {"id": 2, "app": {"key": "C:\\Apps\\Y.exe", "name": "Y"}, "enabled": false, "color": "red"}
            ],
            "ducking_ignore": [{"key": "C:\\Apps\\Z.exe", "name": "Z"}, 42],
            "hide": {"SomethingNew": 1},
            "enabled": false,
            "future_option": true
        }"#;
        fs::write(&path, text).unwrap();
        let c = Config::load(&path);
        let d = Settings::default();
        assert_eq!(c.sources.len(), 2, "the unreadable source is dropped, the others kept");
        let x = &c.sources[0];
        assert_eq!(x.app.name, "X");
        assert_eq!(x.settings.distance_m, 3.0);
        assert_eq!(x.settings.room, d.room);
        assert_eq!(x.settings.ducking.threshold_db, -40.0);
        assert_eq!(x.settings.ducking.depth_db, d.ducking.depth_db);
        assert_eq!(x.base_preset, Preset::BehindYou);
        let y = &c.sources[1];
        assert_eq!((y.app.name.as_str(), y.enabled), ("Y", false));
        assert_ne!(y.color, x.color);
        assert_eq!(c.ducking_ignore, vec![app(r"C:\Apps\Z.exe", "Z")]);
        assert_eq!(c.hide, HideMethod::Auto);
        assert!(!c.enabled);
        assert_eq!(fs::read(path.with_extension("json.broken")).unwrap(), text, "the original is kept");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn non_object_json_gives_defaults() {
        let dir = temp_dir("array");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, b"[1, 2, 3]").unwrap();
        assert_eq!(Config::load(&path), Config::default());
        assert!(path.with_extension("json.broken").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn memory_only_store_never_writes() {
        let mut store = ConfigStore::new(None, &Config::default());
        assert!(!store.flush(&Config { enabled: false, ..Config::default() }));
    }

    /// A config file as Sidestage 0.1 wrote it.
    const V1: &str = r#"{
      "settings": {
        "azimuth_deg": 150.0, "elevation_deg": 0.0, "distance_m": 5.0, "width_deg": 0.0, "volume_db": -7.5,
        "lowpass_hz": 1400.0, "highpass_hz": 30.0, "bass_db": -3.0, "presence_dip_db": -6.0, "room": "NextRoom",
        "reverb_db": -6.0,
        "ducking": {"enabled": true, "threshold_db": -42.0, "depth_db": 12.0, "band_dip_db": 9.0,
                    "attack_ms": 10.0, "hold_ms": 250.0, "release_ms": 450.0},
        "bypass": false
      },
      "base_preset": "NextRoom",
      "footstep_undo": {"presence_dip_db": 0.0, "lowpass_hz": 1400.0, "bass_db": 3.0,
                        "ducking": {"enabled": true, "threshold_db": -36.0, "depth_db": 4.0, "band_dip_db": 3.0,
                                    "attack_ms": 40.0, "hold_ms": 100.0, "release_ms": 900.0}},
      "enabled": false,
      "app": {"key": "C:\\Users\\me\\AppData\\Roaming\\Spotify\\Spotify.exe", "name": "Spotify"},
      "output": "{0.0.0.00000000}.{abc}",
      "hide": "KeepOriginal",
      "sidechain": false,
      "window_size": [1100.0, 700.0],
      "theme": "Light",
      "dismissed_hints": ["windows-mixer"]
    }"#;

    #[test]
    fn v1_config_becomes_one_source() {
        let dir = temp_dir("v1");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, V1).unwrap();
        let c = Config::load(&path);
        assert_eq!(c.version, CONFIG_VERSION);
        assert_eq!(c.sources.len(), 1);
        let s = &c.sources[0];
        assert_eq!(s.app, app(r"C:\Users\me\AppData\Roaming\Spotify\Spotify.exe", "Spotify"));
        assert!(s.enabled, "the power switch stays global; the source itself is on");
        assert_eq!(s.base_preset, Preset::NextRoom);
        assert_eq!(s.settings.azimuth_deg, 150.0);
        assert_eq!(s.settings.volume_db, -7.5);
        assert_eq!(s.settings.ducking.depth_db, 12.0);
        assert!(s.settings.ducking.under_voice, "0.1 ducking gains the new field's default");
        assert_eq!(s.settings.clarity_db, Preset::BehindYou.settings().clarity_db);
        assert!(!s.settings.voice_chat);
        assert!(s.footstep_safe());
        let undo = s.footstep_undo.expect("footstep undo is carried over");
        assert_eq!((undo.bass_db, undo.ducking.depth_db), (3.0, 4.0));
        assert_eq!(c.selected, Some(s.id));
        assert!(c.next_id > s.id);
        assert!(!c.enabled);
        assert_eq!(c.output.as_deref(), Some("{0.0.0.00000000}.{abc}"));
        assert_eq!(c.hide, HideMethod::KeepOriginal);
        assert!(!c.sidechain);
        assert_eq!(c.window_size, [1100.0, 700.0]);
        assert_eq!(c.theme, ThemeChoice::Light);
        assert_eq!(c.dismissed_hints, vec!["windows-mixer".to_owned()]);
        assert_eq!(c.carried_sound, None);
        // Turning Footstep-safe off brings back the 0.1 values exactly.
        let mut s = s.clone();
        s.set_footstep_safe(false);
        assert_eq!((s.settings.bass_db, s.settings.presence_dip_db, s.settings.lowpass_hz), (3.0, 0.0, 1400.0));

        assert_eq!(fs::read_to_string(path.with_extension("json.v1")).unwrap(), V1, "0.1's file is kept");
        assert!(!path.with_extension("json.broken").exists(), "a clean 0.1 file is not broken");
        // Saved again, it loads as version 2 without migrating twice.
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path), c);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn v1_config_without_an_app_keeps_its_sound_for_the_first_music_source() {
        let dir = temp_dir("v1-noapp");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        fs::write(&path, r#"{"settings": {"distance_m": 7.0}, "base_preset": "Lounge", "app": null}"#).unwrap();
        let mut c = Config::load(&path);
        assert!(c.sources.is_empty());
        assert_eq!(c.carried_sound.map(|s| (s.settings.distance_m, s.base_preset)), Some((7.0, Preset::Lounge)));
        // Voice chat does not take the music's settings ...
        let discord = c.add_source(app("demo:discord", "Discord")).unwrap();
        assert_eq!(c.source(discord).unwrap().base_preset, Preset::VoiceRight);
        // ... the first music source does.
        let spotify = c.add_source(app("demo:spotify", "Spotify")).unwrap();
        assert_eq!(c.source(spotify).unwrap().settings.distance_m, 7.0);
        assert_eq!(c.source(spotify).unwrap().base_preset, Preset::Lounge);
        assert_eq!(c.carried_sound, None);
        fs::remove_dir_all(&dir).unwrap();

        // A 0.1 file with untouched settings carries nothing.
        assert_eq!(migrate_v1(&serde_json::json!({"enabled": true})).get("carried_sound"), None);
        let defaults = serde_json::to_value(CarriedSound::default()).unwrap();
        let migrated: Config = serde_json::from_value(migrate_v1(&defaults)).unwrap();
        assert_eq!(migrated.sanitized().carried_sound, None);
    }

    #[test]
    fn v1_config_with_damaged_values_is_migrated_field_by_field() {
        let dir = temp_dir("v1-broken");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let text =
            r#"{"settings": {"distance_m": 3.0, "room": "Garage"}, "app": {"key": "x", "name": "X"}, "theme": 5}"#;
        fs::write(&path, text).unwrap();
        let c = Config::load(&path);
        assert_eq!(c.sources.len(), 1);
        assert_eq!(c.sources[0].settings.distance_m, 3.0);
        assert_eq!(c.sources[0].settings.room, Settings::default().room);
        assert_eq!(c.theme, ThemeChoice::System);
        assert!(path.with_extension("json.v1").exists() && path.with_extension("json.broken").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn footstep_safe_toggles_and_survives_presets() {
        let mut s = Source::default();
        let original = s.settings;
        assert!(!s.footstep_safe());
        s.set_footstep_safe(true);
        assert!(s.footstep_safe());
        assert_eq!(s.settings.ducking, DuckMode::Competitive.ducking());
        s.set_footstep_safe(false);
        assert!(!s.footstep_safe());
        assert_eq!(s.settings, original, "turning it off restores the user's values");

        s.set_footstep_safe(true);
        s.apply_preset(Preset::Lounge);
        assert_eq!(s.base_preset, Preset::Lounge);
        assert!(s.footstep_safe(), "switching presets keeps footstep-safe on");
        s.set_footstep_safe(false);
        assert_eq!(s.settings, Preset::Lounge.settings());
    }

    #[test]
    fn voice_chat_apps_are_recognised() {
        for (key, name) in [
            (r"C:\Users\me\AppData\Local\Discord\app-1.0.9302\Discord.exe", "Discord"),
            (r"C:\Users\me\AppData\Local\DiscordPTB\app-1.0\DiscordPTB.exe", "Discord PTB"),
            (r"C:\Program Files\TeamSpeak 3 Client\ts3client_win64.exe", "TeamSpeak 3"),
            (r"C:\Program Files\Mumble\mumble.exe", "Mumble"),
            ("com.microsoft.teams2", "Microsoft Teams"),
            (r"C:\Program Files\WindowsApps\MSTeams_24.0_x64__8w\ms-teams.exe", "ms-teams"),
            ("us.zoom.xos", "zoom.us"),
            ("demo:discord", "Discord"),
            ("/usr/bin/skypeforlinux", "Skype"),
        ] {
            assert!(is_voice_chat_app(&app(key, name)), "{name}");
        }
        for (key, name) in [
            (r"C:\Program Files (x86)\Steam\steam.exe", "Steam"),
            (r"C:\Program Files (x86)\Steam\steamapps\common\Signalis\SIGNALIS.exe", "SIGNALIS"),
            ("demo:spotify", "Spotify"),
            (r"C:\Program Files\Mozilla Firefox\firefox.exe", "Firefox"),
        ] {
            assert!(!is_voice_chat_app(&app(key, name)), "{name}");
        }
    }

    #[test]
    fn new_sources_get_a_preset_that_fits() {
        let mut c = Config::default();
        let first = c.add_source(app("demo:spotify", "Spotify")).unwrap();
        assert_eq!(c.source(first).unwrap().base_preset, Preset::BehindYou);
        assert_eq!(c.selected, Some(first), "a new source is selected");
        let discord = c.add_source(app("demo:discord", "Discord")).unwrap();
        assert_eq!(c.source(discord).unwrap().base_preset, Preset::VoiceRight);
        assert!(c.source(discord).unwrap().settings.voice_chat);
        let teamspeak = c.add_source(app("ts3client_win64.exe", "TeamSpeak")).unwrap();
        assert_eq!(c.source(teamspeak).unwrap().base_preset, Preset::VoiceLeft, "the right is taken");
        let browser = c.add_source(app("demo:firefox", "Firefox")).unwrap();
        assert_eq!(c.source(browser).unwrap().base_preset, Preset::NextRoom, "first music preset not in use");
        assert_eq!(c.add_source(app("demo:firefox", "Firefox")), None, "an app is one source");

        // A third talker gets a spot of its own instead of landing on one already used.
        let mumble = c.add_source(app("mumble.exe", "Mumble")).unwrap();
        assert_eq!(c.source(mumble).unwrap().base_preset, Preset::OverShoulder);
        // A podcast on the left takes the left for voice chat too.
        let others = [
            Source { settings: Preset::PodcastLeft.settings(), ..Source::default() },
            Source { settings: Preset::VoiceRight.settings(), ..Source::default() },
        ];
        assert_eq!(default_preset_for(&app("demo:discord", "Discord"), &others), Preset::OverShoulder);

        // The right is free again once the voice source there moved.
        let others = [Source { settings: Preset::VoiceLeft.settings(), ..Source::default() }];
        assert_eq!(default_preset_for(&app("demo:discord", "Discord"), &others), Preset::VoiceRight);
        let all_music: Vec<Source> =
            Preset::MUSIC.iter().map(|p| Source { base_preset: *p, ..Source::default() }).collect();
        assert_eq!(default_preset_for(&app("x", "Player"), &all_music), Preset::BehindYou);
    }

    #[test]
    fn removing_and_undoing_keeps_ids_unique() {
        let mut c = Config::default();
        let a = c.add_source(app("a", "A")).unwrap();
        let b = c.add_source(app("b", "B")).unwrap();
        c.selected = Some(b);
        let removed = c.remove_source(b).unwrap();
        assert_eq!(c.selected, Some(a), "the selection moves to a neighbour");
        let d = c.add_source(app("d", "D")).unwrap();
        assert!(d > b, "a removed source's ID is never handed to a new one");
        assert!(c.restore_source(removed.clone()));
        assert_eq!(c.selected, Some(b));
        assert!(!c.restore_source(removed), "restoring twice does nothing");
        c.remove_source(a);
        c.remove_source(b);
        c.remove_source(d);
        assert_eq!(c.selected, None);
    }
}
