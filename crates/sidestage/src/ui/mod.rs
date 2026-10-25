//! The window: an eframe app around the [`Engine`] and every source's renderer settings.
//!
//! Layout: top bar (power, status, output, bypass, menu); the session's sources on the left (or
//! the app picker while one is being added); the radar in the middle with one puck per source;
//! the selected source's presets and sound and ducking controls on the right, with the apps that
//! never count as the game below them; a status line at the bottom and a Diagnostics window on
//! demand.

mod controls;
mod panels;
mod radar;
mod sources;
mod theme;
mod widgets;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Context, ThemePreference, Ui, ViewportCommand};
use sidestage_dsp::{MeterReading, Preset, Settings, SharedParams};
use sidestage_platform::{AppKey, Platform, SessionState, SourceId, Support};

use crate::config::{ChosenApp, Config, ConfigStore, MIN_WINDOW, Source, ThemeChoice, same_app};
use crate::demo::{DemoPlatform, Scenario};
use crate::engine::{Activity, Desired, DesiredSource, Engine, Snapshot};
use crate::screenshot::Screenshot;

/// How long the "Removed ... Undo" card stays.
const UNDO_FOR: Duration = Duration::from_secs(12);

/// How the window was asked to start.
pub struct Launch {
    pub demo: bool,
    pub screenshot: Option<Screenshot>,
    pub scenario: Scenario,
    /// Screenshot scenes.
    pub open_diagnostics: bool,
    /// Scroll the controls panel to the bottom (Ducking and the ignore list).
    pub scroll_controls: bool,
    pub theme: Option<ThemeChoice>,
    /// The music source's app is not running.
    pub waiting: bool,
    /// No source added yet (first run).
    pub no_app: bool,
    pub powered_off: bool,
    /// The selected source far to the right and raised (checks the radar's edge layout).
    pub edge: bool,
    /// Select the voice chat source.
    pub voice: bool,
    /// Fill the "never duck for these apps" list.
    pub ignore: bool,
    /// Open the app picker in the left panel.
    pub adding: bool,
    /// A fourth source on the podcast's spot (checks how crowded pucks are shown).
    pub crowd: bool,
    pub size: Option<[f32; 2]>,
    /// Another Sidestage holds the instance lock: show why instead of capturing.
    pub blocked: bool,
}

impl Launch {
    /// `SIDESTAGE_SCREENSHOT=<file.png>` renders the demo, saves a screenshot and quits.
    /// `SIDESTAGE_SCREENSHOT_SIZE=760x520` sets the window size and
    /// `SIDESTAGE_SCREENSHOT_SCENE=diagnostics,warnings,error,unstable,sourceerror,waiting,setup,off,edge,bottom,voice,ignore,add,crowd,light,dark,real`
    /// the state ("real" shows the OS backend's apps and outputs instead of the demo's, through
    /// [`crate::screenshot::Observer`], which never starts a session). Scenes also work with
    /// `--demo`.
    pub fn from_env(demo: bool) -> Self {
        let screenshot = Screenshot::from_env();
        // Scenes only apply to screenshots and the demo, never to a normal run.
        let scenes: Vec<String> = std::env::var("SIDESTAGE_SCREENSHOT_SCENE")
            .ok()
            .filter(|_| demo || screenshot.is_some())
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let has = |s: &str| scenes.iter().any(|x| x == s);
        let size =
            std::env::var("SIDESTAGE_SCREENSHOT_SIZE").ok().filter(|_| demo || screenshot.is_some()).and_then(|v| {
                let (w, h) = v.split_once('x')?;
                Some([w.trim().parse().ok()?, h.trim().parse().ok()?])
            });
        Launch {
            // Screenshots use the demo unless the "real" scene asks for the OS backend.
            demo: demo || (screenshot.is_some() && !has("real")),
            screenshot,
            scenario: Scenario {
                warnings: has("warnings"),
                start_error: has("error"),
                fail_after_start: has("unstable"),
                source_error: has("sourceerror"),
            },
            open_diagnostics: has("diagnostics"),
            scroll_controls: has("bottom") || has("ignore"),
            theme: if has("light") {
                Some(ThemeChoice::Light)
            } else if has("dark") {
                Some(ThemeChoice::Dark)
            } else {
                None
            },
            waiting: has("waiting"),
            no_app: has("setup"),
            powered_off: has("off"),
            edge: has("edge"),
            voice: has("voice"),
            ignore: has("ignore"),
            adding: has("add"),
            crowd: has("crowd"),
            size,
            blocked: false,
        }
    }
}

/// Take the single-instance lock unless this is a demo. When another Sidestage holds it, the
/// window still opens (marked `blocked`) and explains why it cannot capture. Returns the lock
/// (keep it until the window has closed) and a problem to log once logging is set up.
pub fn claim_instance(launch: &mut Launch) -> (Option<crate::instance::InstanceLock>, Option<String>) {
    if launch.demo {
        return (None, None);
    }
    match crate::instance::acquire() {
        Ok(crate::instance::Acquire::Acquired(lock)) => (Some(lock), None),
        Ok(crate::instance::Acquire::AlreadyRunning) => {
            launch.blocked = true;
            (None, Some("another Sidestage is running".into()))
        }
        Err(e) => (None, Some(format!("instance lock unavailable: {e:#}"))),
    }
}

pub fn run(launch: Launch) -> anyhow::Result<()> {
    // Demo, screenshot and blocked runs never touch the user's saved settings.
    let config_path = if launch.demo || launch.screenshot.is_some() || launch.blocked {
        None
    } else {
        Config::default_path().map_err(|e| log::warn!("{e:#}")).ok()
    };
    let mut config = match &config_path {
        Some(path) => Config::load(path),
        None if launch.demo => demo_config(&launch),
        None => Config::default(),
    };
    if let Some(theme) = launch.theme {
        config.theme = theme;
    }
    let size = launch.size.unwrap_or(config.window_size);
    let viewport = egui::ViewportBuilder::default()
        .with_title("Sidestage")
        .with_app_id("sidestage")
        .with_inner_size(size)
        .with_min_inner_size(MIN_WINDOW)
        .with_icon(theme::icon())
        // An unattended screenshot run must not take the keyboard from whatever the user is doing.
        .with_active(launch.screenshot.is_none());
    let options = eframe::NativeOptions { viewport, centered: true, ..Default::default() };
    log::info!("Sidestage {} starting{}", env!("CARGO_PKG_VERSION"), if launch.demo { " in demo mode" } else { "" });
    eframe::run_native(
        "Sidestage",
        options,
        Box::new(move |cc| Ok(Box::new(SidestageApp::new(&cc.egui_ctx, launch, config, config_path)))),
    )
    .map_err(|e| anyhow::anyhow!("could not open the window: {e}"))
}

/// The demo scene: Spotify behind you, a podcast on the left and Discord on the right, so the
/// music visibly turns down while Discord talks.
fn demo_config(launch: &Launch) -> Config {
    let mut config = Config { enabled: !launch.powered_off, ..Config::default() };
    if launch.no_app {
        return config;
    }
    let app = |key: &str, name: &str| ChosenApp { key: AppKey(key.into()), name: name.into() };
    let music = if launch.waiting { app("demo:apple-music", "Apple Music") } else { app("demo:spotify", "Spotify") };
    let music = config.add_source(music);
    if let Some(id) = config.add_source(app("demo:pocketcasts", "Pocket Casts"))
        && let Some(podcast) = config.source_mut(id)
    {
        podcast.apply_preset(Preset::PodcastLeft);
    }
    let voice = config.add_source(app("demo:discord", "Discord"));
    config.selected = if launch.voice { voice } else { music };
    if launch.edge
        && let Some(s) = config.selected_source_mut()
    {
        s.settings = Settings { azimuth_deg: 100.0, distance_m: 20.0, elevation_deg: 30.0, ..s.settings };
    }
    // The browser's video would turn every source down all the time: what the ignore list is for.
    config.ducking_ignore = vec![app("demo:firefox", "Firefox")];
    if launch.ignore {
        config.ducking_ignore.push(app("demo:system", "System sounds"));
    }
    if launch.crowd
        && let Some(id) = config.add_source(app("demo:firefox", "Firefox"))
        && let Some(video) = config.source_mut(id)
    {
        video.apply_preset(Preset::PodcastLeft);
        config.selected = music;
        config.ducking_ignore.clear();
    }
    config
}

/// One source's meters for display.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SourceMeter {
    /// Input level, peak-hold with a steady fall.
    pub level: f32,
    /// How far it is turned down right now, dB (smoothed).
    pub duck_db: f32,
}

/// Meter values for display: peak-hold with a steady fall, so 10 Hz backend updates and 30 Hz
/// repaints look smooth.
#[derive(Default)]
struct DisplayMeters {
    sources: HashMap<SourceId, SourceMeter>,
    game: f32,
    /// Last time the sidechain carried any signal (for the "no game audio measured" hint).
    game_seen: Option<Instant>,
    /// Per-app levels for the app pickers.
    apps: HashMap<AppKey, f32>,
}

impl DisplayMeters {
    fn update(&mut self, readings: &[(SourceId, MeterReading)], snap: &Snapshot, config: &Config, dt: f32) {
        // About 24 dB per second fall.
        let fall = 10f32.powf(-1.2 * dt);
        let session = snap.session.as_ref().filter(|s| s.state == SessionState::Running);
        let mut game = session.map_or(0.0, |s| s.sidechain_peak);
        for (id, reading) in readings {
            let status = session.and_then(|s| s.sources.iter().find(|x| x.id == *id));
            let live = status.filter(|s| s.state == SessionState::Running);
            let level = reading.input_peak.max(live.map_or(0.0, |s| s.capture_peak));
            let m = self.sources.entry(*id).or_default();
            m.level = level.max(m.level * fall);
            // Tails still ring out while the app pauses; a source that is gone is not ducked.
            let duck = if status.is_some() { reading.duck_db.max(0.0) } else { 0.0 };
            m.duck_db += (duck - m.duck_db) * (dt * 20.0).min(1.0);
            game = game.max(reading.sidechain_peak);
        }
        if self.sources.len() > readings.len() {
            self.sources.retain(|id, _| readings.iter().any(|(r, _)| r == id));
        }
        self.game = game.max(self.game * fall);
        if game > 0.001 {
            self.game_seen = Some(Instant::now());
        }
        for app in &snap.apps {
            // A captured app is silent to the backend's own meter; show what Sidestage hears.
            let source = config
                .source_for(&app.key)
                .and_then(|s| session.and_then(|st| st.sources.iter().find(|x| x.id == s.id)));
            let target = match source {
                Some(s) if s.state == SessionState::Running => self.sources.get(&s.id).map_or(0.0, |m| m.level),
                _ => app.peak.unwrap_or(0.0),
            };
            let v = self.apps.entry(app.key.clone()).or_insert(0.0);
            *v = target.max(*v * fall);
        }
        if self.apps.len() > snap.apps.len() {
            self.apps.retain(|k, _| snap.apps.iter().any(|a| &a.key == k));
        }
    }

    fn source(&self, id: SourceId) -> SourceMeter {
        self.sources.get(&id).copied().unwrap_or_default()
    }

    /// The loudest voice-chat source other than `except`: what turns `except` down.
    fn voice(&self, config: &Config, except: SourceId) -> f32 {
        config
            .sources
            .iter()
            .filter(|s| s.enabled && s.settings.voice_chat && s.id != except)
            .map(|s| self.source(s.id).level)
            .fold(0.0, f32::max)
    }
}

/// What one source is doing, for its row, its puck and the right panel.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum SourceView {
    /// Switched off (or Sidestage is off): the app plays normally.
    Off,
    /// Its app is not playing through Sidestage yet.
    Waiting,
    Starting,
    Playing {
        /// The unprocessed original is hidden.
        hidden: bool,
    },
    /// This source could not be captured (the others may be fine).
    Failed(String),
    /// Nothing is captured: the session could not start or capture is unavailable. The app
    /// plays normally meanwhile; the reason is on a card of its own.
    NotCapturing,
}

pub struct SidestageApp {
    engine: Engine,
    config: Config,
    store: ConfigStore,
    /// Renderer settings of every source. Made with the source and kept until it is removed:
    /// the engine and the backend keep a source's renderer for as long as its `SharedParams`.
    params: HashMap<SourceId, Arc<SharedParams>>,
    /// Settings last published per source (including the transient bypass flag).
    published: HashMap<SourceId, Settings>,
    sent_desired: Desired,
    snap: Snapshot,
    meters: DisplayMeters,
    last_frame: Instant,
    /// The left panel shows the app picker instead of the sources.
    adding: bool,
    search: String,
    /// Scroll the selected source's row into view (it was selected on the radar).
    reveal_selected: bool,
    /// The source removed last, for Undo, and when.
    removed: Option<(Source, Instant)>,
    show_diagnostics: bool,
    /// Screenshot mode: scroll the controls to the bottom.
    scroll_controls: bool,
    bypass_held: bool,
    applied_theme: Option<ThemeChoice>,
    screenshot: Option<Screenshot>,
    demo: bool,
    /// A termination signal arrived and the window was asked to close.
    closing: bool,
}

impl SidestageApp {
    fn new(ctx: &Context, launch: Launch, config: Config, config_path: Option<PathBuf>) -> Self {
        theme::install(ctx);
        let params: HashMap<SourceId, Arc<SharedParams>> =
            config.sources.iter().map(|s| (s.id, SharedParams::new(s.settings))).collect();
        let published = params.iter().map(|(id, p)| (*id, p.get())).collect();
        let desired = desired_of(&config, &params);
        let make_platform: crate::engine::PlatformMaker = if launch.blocked {
            Box::new(|| Err(anyhow::anyhow!(crate::instance::ALREADY_RUNNING)))
        } else if launch.demo {
            let scenario = launch.scenario;
            Box::new(move || Ok(Box::new(DemoPlatform::new(scenario)) as Box<dyn Platform>))
        } else if launch.screenshot.is_some() {
            // The "real" scene: the OS backend's apps and outputs, never a session.
            Box::new(|| Ok(Box::new(crate::screenshot::Observer(sidestage_platform::platform()?)) as Box<dyn Platform>))
        } else {
            Box::new(sidestage_platform::platform)
        };
        let repaint = ctx.clone();
        let engine = Engine::spawn(make_platform, desired.clone(), Arc::new(move || repaint.request_repaint()));
        let store = ConfigStore::new(config_path, &config);
        watch_for_termination(ctx);
        SidestageApp {
            engine,
            params,
            published,
            sent_desired: desired,
            store,
            config,
            snap: Snapshot::default(),
            meters: DisplayMeters::default(),
            last_frame: Instant::now(),
            adding: launch.adding,
            search: String::new(),
            reveal_selected: true,
            removed: None,
            show_diagnostics: launch.open_diagnostics,
            scroll_controls: launch.scroll_controls,
            bypass_held: false,
            applied_theme: None,
            screenshot: launch.screenshot,
            demo: launch.demo,
            closing: false,
        }
    }

    /// Push every source's settings to its renderer and the session wishes to the engine when
    /// they changed.
    fn sync(&mut self) {
        for s in &self.config.sources {
            let params = self.params.entry(s.id).or_insert_with(|| SharedParams::new(s.settings));
            let effective = Settings { bypass: self.bypass_held, ..s.settings };
            if self.published.get(&s.id) != Some(&effective) {
                params.set(effective);
                self.published.insert(s.id, effective);
            }
        }
        if self.params.len() > self.config.sources.len() {
            let config = &self.config;
            self.params.retain(|id, _| config.source(*id).is_some());
            self.published.retain(|id, _| config.source(*id).is_some());
        }
        let desired = desired_of(&self.config, &self.params);
        if desired != self.sent_desired {
            self.engine.set_desired(desired.clone());
            self.sent_desired = desired;
        }
    }

    fn apply_theme(&mut self, ctx: &Context) {
        if self.applied_theme != Some(self.config.theme) {
            ctx.set_theme(match self.config.theme {
                ThemeChoice::System => ThemePreference::System,
                ThemeChoice::Dark => ThemePreference::Dark,
                ThemeChoice::Light => ThemePreference::Light,
            });
            self.applied_theme = Some(self.config.theme);
        }
    }

    /// The session's output is playing.
    fn running(&self) -> bool {
        self.snap.session.as_ref().is_some_and(|s| s.state == SessionState::Running)
    }

    /// Any source is processed without Sidestage right now (the Bypass button is held).
    fn bypassing(&self) -> bool {
        self.published.values().any(|s| s.bypass)
    }

    fn view(&self, source: &Source) -> SourceView {
        if !self.config.enabled || !source.enabled {
            return SourceView::Off;
        }
        match self.snap.source(source.id).map(|st| &st.state) {
            Some(SessionState::Running) => {
                SourceView::Playing { hidden: self.snap.source(source.id).is_some_and(|st| st.original_hidden) }
            }
            Some(SessionState::Starting) => SourceView::Starting,
            Some(SessionState::Error(e)) => SourceView::Failed(e.clone()),
            Some(SessionState::WaitingForApp | SessionState::Stopped) => SourceView::Waiting,
            None if self.snap.activity == Activity::Starting && self.snap.app_present(&source.app.key) => {
                SourceView::Starting
            }
            // "Waiting for Spotify" would be wrong while Spotify plays and Sidestage cannot start.
            None if self.snap.error.is_some()
                || self.snap.platform_error.is_some()
                || matches!(self.snap.support, Some(Support::Unsupported(_))) =>
            {
                SourceView::NotCapturing
            }
            None => SourceView::Waiting,
        }
    }

    /// The switched-on sources' names: "Spotify, Pocket Casts and Discord".
    fn enabled_names(&self) -> String {
        let names: Vec<&str> = self.config.sources.iter().filter(|s| s.enabled).map(|s| s.app.name.as_str()).collect();
        crate::text::join_names(&names)
    }

    /// Add an app as a new, selected source and switch Sidestage on.
    fn add_source(&mut self, app: ChosenApp) {
        let name = app.name.clone();
        let key = app.key.clone();
        match self.config.add_source(app) {
            Some(id) => {
                let preset = self.config.source(id).map_or("", |s| s.base_preset.name());
                log::info!("added source {id}: {name} ({}) as {preset}", key.0);
                // Added again by hand: there is nothing left to undo.
                if self.removed.as_ref().is_some_and(|(s, _)| same_app(&s.app.key, &key)) {
                    self.removed = None;
                }
                self.config.enabled = true;
                self.adding = false;
                self.search.clear();
                self.reveal_selected = true;
            }
            None => log::warn!("{name} could not be added as a source"),
        }
    }

    fn remove_source(&mut self, id: SourceId) {
        if let Some(source) = self.config.remove_source(id) {
            log::info!("removed source {id}: {}", source.app.name);
            self.removed = Some((source, Instant::now()));
        }
    }

    fn undo_remove(&mut self) {
        if let Some((source, _)) = self.removed.take() {
            let name = source.app.name.clone();
            if !self.config.restore_source(source) {
                log::warn!("{name} could not be put back");
            }
        }
    }
}

/// Wake the UI when a termination signal arrives (see `logic`); signal handlers themselves may
/// only set a flag.
fn watch_for_termination(ctx: &Context) {
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new().name("sidestage-signals".into()).spawn(move || {
        while !crate::signal::stop_requested() {
            std::thread::sleep(Duration::from_millis(200));
        }
        ctx.request_repaint();
    });
    if let Err(e) = spawned {
        log::warn!("could not watch for termination signals: {e}");
    }
}

fn desired_of(config: &Config, params: &HashMap<SourceId, Arc<SharedParams>>) -> Desired {
    Desired {
        enabled: config.enabled,
        sources: config
            .sources
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| {
                let params = Arc::clone(params.get(&s.id)?);
                Some(DesiredSource { id: s.id, app: s.app.clone(), params })
            })
            .collect(),
        output: config.output.clone(),
        hide: config.hide.clone(),
        sidechain: config.sidechain,
        ignore: config.ducking_ignore.iter().map(|a| a.key.clone()).collect(),
    }
}

impl eframe::App for SidestageApp {
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.apply_theme(ctx);
        // Remember the window size (in points) for the next launch.
        if let Some(rect) = ctx.input(|i| i.viewport().inner_rect)
            && !ctx.input(|i| i.viewport().maximized.unwrap_or(false) || i.viewport().fullscreen.unwrap_or(false))
            && self.screenshot.is_none()
        {
            let size = [rect.width().round(), rect.height().round()];
            if size[0] >= MIN_WINDOW[0] && size[1] >= MIN_WINDOW[1] {
                self.config.window_size = size;
            }
        }
        // `ui` (which tracks the Bypass button) does not run while minimized: never leave the
        // renderers bypassed behind a hidden window.
        if self.bypassing() && ctx.input(|i| i.viewport().minimized.unwrap_or(false)) {
            self.bypass_held = false;
            self.sync();
        }
        if self.removed.as_ref().is_some_and(|(_, at)| at.elapsed() >= UNDO_FOR) {
            self.removed = None;
        }
        self.store.update(&self.config, Instant::now());
        if let Some(shot) = &mut self.screenshot {
            shot.tick(ctx);
        }
        // Ctrl+C in the launching terminal, SIGTERM/SIGHUP (logout, `kill`): close normally so
        // the session is stopped and the originals restored before the process ends.
        if crate::signal::stop_requested() && !self.closing {
            log::info!("termination requested; closing the window");
            self.closing = true;
            close(ctx);
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            self.store.flush(&self.config);
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32().min(0.25);
        self.last_frame = now;
        self.engine.refresh(&mut self.snap);
        let readings: Vec<(SourceId, MeterReading)> = self
            .config
            .sources
            .iter()
            .filter_map(|s| self.params.get(&s.id).map(|p| (s.id, p.meters.take())))
            .collect();
        self.meters.update(&readings, &self.snap, &self.config, dt);

        self.bypass_held = false;
        let full_width = ui.available_width();
        panels::top_bar(self, ui);
        panels::status_bar(self, ui);
        sources::left_panel(self, ui, (full_width * 0.23).clamp(206.0, 270.0));
        controls::right_panel(self, ui, (full_width * 0.30).clamp(272.0, 340.0));
        panels::center(self, ui);
        if self.show_diagnostics {
            panels::diagnostics(self, ui.ctx());
        }
        // Keyboard bypass: hold B (when not typing). `egui_wants_keyboard_input` would be true
        // whenever any widget has focus, e.g. the radar after every drag.
        if !ui.ctx().text_edit_focused() && ui.input(|i| i.key_down(egui::Key::B)) {
            self.bypass_held = true;
        }
        self.sync();

        // Meters move at ~30 Hz while audio runs; otherwise a slow tick keeps the app list fresh
        // (the engine also wakes us when something changes).
        let busy = self.running() || self.snap.activity != Activity::Idle || self.screenshot.is_some();
        ui.ctx().request_repaint_after(if busy { Duration::from_millis(33) } else { Duration::from_millis(250) });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.store.flush(&self.config);
        log::info!("window closed; stopping");
        // Stops the session, which restores every source's original output.
        self.engine.shutdown(Duration::from_secs(5));
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }
}

/// Close the window from code (screenshot mode).
pub fn close(ctx: &Context) {
    ctx.send_viewport_cmd(ViewportCommand::Close);
}

/// The window icon as an image (screenshot mode exports it for review).
pub fn icon_image() -> egui::ColorImage {
    let icon = theme::icon();
    egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> Launch {
        Launch {
            demo: true,
            screenshot: None,
            scenario: Scenario::default(),
            open_diagnostics: false,
            scroll_controls: false,
            theme: None,
            waiting: false,
            no_app: false,
            powered_off: false,
            edge: false,
            voice: false,
            ignore: false,
            adding: false,
            crowd: false,
            size: None,
            blocked: false,
        }
    }

    #[test]
    fn demo_scene_has_music_a_podcast_and_voice_chat() {
        let c = demo_config(&launch());
        let presets: Vec<(&str, Preset)> = c.sources.iter().map(|s| (s.app.name.as_str(), s.base_preset)).collect();
        assert_eq!(
            presets,
            [("Spotify", Preset::BehindYou), ("Pocket Casts", Preset::PodcastLeft), ("Discord", Preset::VoiceRight)]
        );
        assert_eq!(c.selected_source().map(|s| s.app.name.as_str()), Some("Spotify"));
        assert!(c.sources[2].settings.voice_chat && c.sources[0].settings.ducking.under_voice);
        let voice = demo_config(&Launch { voice: true, ..launch() });
        assert_eq!(voice.selected_source().map(|s| s.app.name.as_str()), Some("Discord"));
        assert!(demo_config(&Launch { no_app: true, ..launch() }).sources.is_empty());
        assert_eq!(demo_config(&Launch { ignore: true, ..launch() }).ducking_ignore.len(), 2);
    }

    #[test]
    fn only_switched_on_sources_are_desired() {
        let mut c = demo_config(&launch());
        c.sources[1].enabled = false;
        let params: HashMap<SourceId, Arc<SharedParams>> =
            c.sources.iter().map(|s| (s.id, SharedParams::new(s.settings))).collect();
        c.ducking_ignore.push(ChosenApp { key: AppKey("demo:system".into()), name: "System sounds".into() });
        let d = desired_of(&c, &params);
        let ids: Vec<SourceId> = d.sources.iter().map(|s| s.id).collect();
        assert_eq!(ids, [c.sources[0].id, c.sources[2].id]);
        assert_eq!(d.ignore, vec![AppKey("demo:firefox".into()), AppKey("demo:system".into())]);
        // The same storage gives an equal wish, so nothing is sent to the engine again.
        assert_eq!(desired_of(&c, &params), d);
    }

    #[test]
    fn voice_level_comes_from_the_other_voice_sources() {
        let c = demo_config(&launch());
        let (music, podcast, discord) = (c.sources[0].id, c.sources[1].id, c.sources[2].id);
        let mut m = DisplayMeters::default();
        m.sources.insert(discord, SourceMeter { level: 0.4, duck_db: 0.0 });
        m.sources.insert(podcast, SourceMeter { level: 0.9, duck_db: 0.0 });
        assert_eq!(m.voice(&c, music), 0.4, "the podcast is not voice chat");
        assert_eq!(m.voice(&c, discord), 0.0, "voice chat never turns itself down");
    }
}
