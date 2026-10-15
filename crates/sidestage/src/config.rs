//! The user's saved choices: sound settings, which app is the music, output device, how the
//! original is hidden, window size. Stored as JSON in the per-user config directory.
//!
//! Loading never fails: a missing file gives defaults. A file with values this version cannot
//! read (damaged, or written by a newer Sidestage) keeps every field that still reads and uses
//! defaults for the rest; the original is kept next to it as `config.json.broken` so nothing the
//! user set up is silently lost.
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
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sidestage_dsp::{Ducking, Preset, Settings, make_footstep_safe};
use sidestage_platform::{AppKey, HideMethod};

/// Smallest window that still fits the three columns.
pub const MIN_WINDOW: [f32; 2] = [760.0, 520.0];
pub const DEFAULT_WINDOW: [f32; 2] = [1000.0, 660.0];

/// How long settings must stay unchanged before they are written (slider drags change them
/// every frame).
pub const SAVE_DEBOUNCE: Duration = Duration::from_millis(1500);
/// How long closing the window waits for the last save.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

/// The app chosen as the music source. The name is kept so the UI can say "Waiting for Spotify"
/// while the app is not running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChosenApp {
    pub key: AppKey,
    pub name: String,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Sound settings (never saved with `bypass` on: that is a hold-to-compare button).
    pub settings: Settings,
    /// Preset the settings started from; double-clicking the radar returns to its position.
    pub base_preset: Preset,
    pub footstep_undo: Option<FootstepUndo>,
    /// The power switch.
    pub enabled: bool,
    pub app: Option<ChosenApp>,
    /// Output device ID, `None` = follow the system default.
    pub output: Option<String>,
    pub hide: HideMethod,
    /// Measure "everything except the music" so ducking can react to the game.
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
            settings: Settings::default(),
            base_preset: Preset::BehindYou,
            footstep_undo: None,
            enabled: true,
            app: None,
            output: None,
            hide: HideMethod::Auto,
            sidechain: true,
            window_size: DEFAULT_WINDOW,
            theme: ThemeChoice::System,
            dismissed_hints: Vec::new(),
        }
    }
}

impl Config {
    /// `<config dir>/config.json`, e.g. `%APPDATA%\Sidestage\config\config.json` on Windows.
    pub fn default_path() -> anyhow::Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("", "", "Sidestage")
            .context("could not determine the user's home directory")?;
        Ok(dirs.config_dir().join("config.json"))
    }

    /// Clamp everything into range; drop transient state.
    pub fn sanitized(mut self) -> Self {
        self.settings = Settings { bypass: false, ..self.settings.sanitized() };
        for (v, (min, default)) in self.window_size.iter_mut().zip(MIN_WINDOW.iter().zip(DEFAULT_WINDOW)) {
            *v = if v.is_finite() { v.clamp(*min, 8192.0) } else { default };
        }
        if self.app.as_ref().is_some_and(|a| a.key.0.is_empty()) {
            self.app = None;
        }
        if self.output.as_ref().is_some_and(String::is_empty) {
            self.output = None;
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
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(e) => {
                log::warn!("{} is not valid JSON ({e}); using defaults", path.display());
                keep_broken(path);
                return Config::default();
            }
        };
        match serde_json::from_value::<Config>(value.clone()) {
            Ok(config) => config.sanitized(),
            Err(e) => {
                let (config, skipped) = recover_fields(&value);
                log::warn!(
                    "{} has values this version cannot read ({e}); using defaults for: {}",
                    path.display(),
                    skipped.join(", ")
                );
                keep_broken(path);
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

/// Copy an unreadable config to `config.json.broken` before it is overwritten.
fn keep_broken(path: &Path) {
    let backup = path.with_extension("json.broken");
    match fs::copy(path, &backup) {
        Ok(_) => log::warn!("the original file is kept as {}", backup.display()),
        Err(e) => log::warn!("could not back up {}: {e}", path.display()),
    }
}

/// Start from the defaults and take over the file's values one field at a time, descending into
/// nested objects (`settings`, `settings.ducking`), so one value this version cannot read (a
/// damaged number, an option added by a newer version) only resets that value. Returns the
/// config and the JSON pointers of the values left at their defaults.
fn recover_fields(file: &Value) -> (Config, Vec<String>) {
    let Ok(mut merged) = serde_json::to_value(Config::default()) else {
        return (Config::default(), vec!["/".into()]);
    };
    let mut skipped = Vec::new();
    if file.is_object() {
        overlay(&mut merged, "", file, &mut skipped);
    } else {
        skipped.push("/".into());
    }
    match serde_json::from_value(merged) {
        Ok(config) => (config, skipped),
        Err(_) => (Config::default(), vec!["/".into()]),
    }
}

fn overlay(root: &mut Value, at: &str, file: &Value, skipped: &mut Vec<String>) {
    let Some(fields) = file.as_object() else { return };
    for (key, value) in fields {
        let pointer = format!("{at}/{}", key.replace('~', "~0").replace('/', "~1"));
        // Fields this version does not know are ignored, as serde does.
        let Some(slot) = root.pointer(&pointer) else { continue };
        if slot.is_object() && value.is_object() {
            overlay(root, &pointer, value, skipped);
            continue;
        }
        let Some(slot) = root.pointer_mut(&pointer) else { continue };
        let old = std::mem::replace(slot, value.clone());
        if serde_json::from_value::<Config>(root.clone()).is_err() {
            if let Some(slot) = root.pointer_mut(&pointer) {
                *slot = old;
            }
            skipped.push(pointer);
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

    fn sample() -> Config {
        let mut c = Config {
            app: Some(ChosenApp {
                key: AppKey(r"C:\Program Files\Spotify\Spotify.exe".into()),
                name: "Spotify".into(),
            }),
            output: Some("{0.0.0.00000000}.{abc}".into()),
            hide: HideMethod::ParkOn("{0.0.0.00000000}.{def}".into()),
            sidechain: false,
            enabled: false,
            theme: ThemeChoice::Light,
            window_size: [1200.0, 800.0],
            ..Config::default()
        };
        c.apply_preset(Preset::NextRoom);
        c.settings.volume_db = -7.5;
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
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(Config::load(&dir.join("config.json")), Config::default());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested").join("config.json");
        let c = sample();
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path), c);
        assert!(!path.with_extension("json.tmp").exists(), "temp file must be renamed away");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bypass_is_never_saved() {
        let dir = temp_dir("bypass");
        let path = dir.join("config.json");
        let mut c = Config::default();
        c.settings.bypass = true;
        c.save(&path).unwrap();
        assert!(!Config::load(&path).settings.bypass);
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
        fs::write(&path, br#"{"settings": {"distance_m": 500.0}, "window_size": [10.0, 99999.0], "output": ""}"#)
            .unwrap();
        let c = Config::load(&path);
        assert_eq!(c.settings.distance_m, 20.0);
        assert_eq!(c.settings.room, Settings::default().room, "missing fields get defaults");
        assert_eq!(c.window_size, [MIN_WINDOW[0], 8192.0]);
        assert_eq!(c.output, None);
        assert!(c.enabled && c.sidechain);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn store_debounces_and_flushes() {
        let dir = temp_dir("store");
        let path = dir.join("config.json");
        let c0 = Config::default();
        let mut store = ConfigStore::new(Some(path.clone()), &c0);
        let t0 = Instant::now();
        assert!(!store.update(&c0, t0), "unchanged config is not written");

        let mut c1 = c0.clone();
        c1.settings.distance_m = 3.0;
        assert!(!store.update(&c1, t0));
        // Still changing (a slider drag): the timer restarts.
        let mut c2 = c1.clone();
        c2.settings.distance_m = 4.0;
        assert!(!store.update(&c2, t0 + SAVE_DEBOUNCE));
        assert!(!path.exists());
        assert!(!store.update(&c2, t0 + SAVE_DEBOUNCE + Duration::from_millis(100)));
        assert!(store.update(&c2, t0 + SAVE_DEBOUNCE * 2 + Duration::from_millis(1)));
        assert!(!store.flush(&c2), "already saved; flush only waits for it");
        assert_eq!(Config::load(&path), c2);

        let mut c3 = c2.clone();
        c3.enabled = false;
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
            "settings": {"distance_m": 3.0, "room": "Garage", "ducking": {"threshold_db": -40.0, "depth_db": "lots"}},
            "hide": {"SomethingNew": 1},
            "enabled": false,
            "app": {"key": "C:\\Apps\\X.exe", "name": "X"},
            "future_option": true
        }"#;
        fs::write(&path, text).unwrap();
        let c = Config::load(&path);
        let d = Config::default();
        assert_eq!(c.settings.distance_m, 3.0);
        assert_eq!(c.settings.room, d.settings.room);
        assert_eq!(c.settings.ducking.threshold_db, -40.0);
        assert_eq!(c.settings.ducking.depth_db, d.settings.ducking.depth_db);
        assert_eq!(c.hide, HideMethod::Auto);
        assert!(!c.enabled);
        assert_eq!(c.app.as_ref().map(|a| a.name.as_str()), Some("X"));
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

    #[test]
    fn footstep_safe_toggles_and_survives_presets() {
        let mut c = Config::default();
        let original = c.settings;
        assert!(!c.footstep_safe());
        c.set_footstep_safe(true);
        assert!(c.footstep_safe());
        assert_eq!(c.settings.ducking, DuckMode::Competitive.ducking());
        c.set_footstep_safe(false);
        assert!(!c.footstep_safe());
        assert_eq!(c.settings, original, "turning it off restores the user's values");

        c.set_footstep_safe(true);
        c.apply_preset(Preset::Lounge);
        assert_eq!(c.base_preset, Preset::Lounge);
        assert!(c.footstep_safe(), "switching presets keeps footstep-safe on");
        c.set_footstep_safe(false);
        assert_eq!(c.settings, Preset::Lounge.settings());
    }
}
