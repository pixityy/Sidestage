//! The window: an eframe app around the [`Engine`] and the shared renderer settings.
//!
//! Layout: top bar (power, status, output, bypass, menu), music source list on the left, the
//! radar in the middle, presets and sound/ducking controls on the right, a status line at the
//! bottom and a Diagnostics window on demand.

mod panels;
mod radar;
mod theme;
mod widgets;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Context, ThemePreference, Ui, ViewportCommand};
use sidestage_dsp::{MeterReading, SharedParams};
use sidestage_platform::{AppKey, Platform, SessionState};

use crate::config::{ChosenApp, Config, ConfigStore, MIN_WINDOW, ThemeChoice, same_app};
use crate::demo::{DemoPlatform, Scenario};
use crate::engine::{Desired, Engine, Snapshot};
use crate::screenshot::Screenshot;

/// How the window was asked to start.
pub struct Launch {
    pub demo: bool,
    pub screenshot: Option<Screenshot>,
    pub scenario: Scenario,
    /// Screenshot scenes.
    pub open_diagnostics: bool,
    /// Scroll the controls panel to the bottom (to show Ducking).
    pub scroll_controls: bool,
    pub theme: Option<ThemeChoice>,
    pub waiting: bool,
    /// No music app chosen yet.
    pub no_app: bool,
    pub powered_off: bool,
    /// The music far to the right and raised (checks the radar's edge layout).
    pub edge: bool,
    pub size: Option<[f32; 2]>,
    /// Another Sidestage holds the instance lock: show why instead of capturing.
    pub blocked: bool,
}

impl Launch {
    /// `SIDESTAGE_SCREENSHOT=<file.png>` renders the demo, saves a screenshot and quits.
    /// `SIDESTAGE_SCREENSHOT_SIZE=760x520` sets the window size and
    /// `SIDESTAGE_SCREENSHOT_SCENE=diagnostics,warnings,error,unstable,waiting,setup,off,edge,bottom,light,dark,real`
    /// the state ("real" uses the OS backend instead of the demo). Scenes also work with `--demo`.
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
            },
            open_diagnostics: has("diagnostics"),
            scroll_controls: has("bottom"),
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
        .with_icon(theme::icon());
    let options = eframe::NativeOptions { viewport, centered: true, ..Default::default() };
    log::info!("Sidestage {} starting{}", env!("CARGO_PKG_VERSION"), if launch.demo { " in demo mode" } else { "" });
    eframe::run_native(
        "Sidestage",
        options,
        Box::new(move |cc| Ok(Box::new(SidestageApp::new(&cc.egui_ctx, launch, config, config_path)))),
    )
    .map_err(|e| anyhow::anyhow!("could not open the window: {e}"))
}

/// The demo starts with Spotify chosen and running (or waiting for an app that is not there).
fn demo_config(launch: &Launch) -> Config {
    let app = if launch.waiting {
        ChosenApp { key: AppKey("demo:apple-music".into()), name: "Apple Music".into() }
    } else {
        ChosenApp { key: AppKey("demo:spotify".into()), name: "Spotify".into() }
    };
    let mut config = Config { app: (!launch.no_app).then_some(app), enabled: !launch.powered_off, ..Config::default() };
    if launch.edge {
        config.settings =
            sidestage_dsp::Settings { azimuth_deg: 100.0, distance_m: 20.0, elevation_deg: 30.0, ..config.settings };
    }
    config
}

/// Meter values for display: peak-hold with a steady fall, so 10 Hz backend updates and 30 Hz
/// repaints look smooth.
#[derive(Default)]
struct DisplayMeters {
    music: f32,
    game: f32,
    duck_db: f32,
    /// Last time the sidechain carried any signal (for the "no game audio measured" hint).
    game_seen: Option<Instant>,
    /// Per-app levels for the source list.
    apps: HashMap<AppKey, f32>,
}

impl DisplayMeters {
    fn update(&mut self, reading: MeterReading, snap: &Snapshot, dt: f32) {
        // About 24 dB per second fall.
        let fall = 10f32.powf(-1.2 * dt);
        let status = snap.session.as_ref().filter(|s| s.state == SessionState::Running);
        let music = reading.input_peak.max(status.map_or(0.0, |s| s.capture_peak));
        let game = reading.sidechain_peak.max(status.map_or(0.0, |s| s.sidechain_peak));
        self.music = music.max(self.music * fall);
        self.game = game.max(self.game * fall);
        let duck = if status.is_some() { reading.duck_db.max(0.0) } else { 0.0 };
        self.duck_db += (duck - self.duck_db) * (dt * 20.0).min(1.0);
        if game > 0.001 {
            self.game_seen = Some(Instant::now());
        }
        let live: Option<&AppKey> = snap.session_app.as_ref().map(|a| &a.key).filter(|_| status.is_some());
        for app in &snap.apps {
            let target = if live.is_some_and(|k| same_app(k, &app.key)) { self.music } else { app.peak.unwrap_or(0.0) };
            let v = self.apps.entry(app.key.clone()).or_insert(0.0);
            *v = target.max(*v * fall);
        }
        if self.apps.len() > snap.apps.len() {
            self.apps.retain(|k, _| snap.apps.iter().any(|a| &a.key == k));
        }
    }
}

pub struct SidestageApp {
    engine: Engine,
    params: Arc<SharedParams>,
    config: Config,
    store: ConfigStore,
    /// Settings last published to the renderer (including the transient bypass flag).
    published: sidestage_dsp::Settings,
    sent_desired: Desired,
    snap: Snapshot,
    meters: DisplayMeters,
    last_frame: Instant,
    search: String,
    /// Scroll the chosen app into view in the source list (set when the list's height changes).
    reveal_source: bool,
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
        let params = SharedParams::new(config.settings);
        let desired = desired_of(&config);
        let make_platform: crate::engine::PlatformMaker = if launch.blocked {
            Box::new(|| Err(anyhow::anyhow!(crate::instance::ALREADY_RUNNING)))
        } else if launch.demo {
            let demo_params = Arc::clone(&params);
            let scenario = launch.scenario;
            Box::new(move || Ok(Box::new(DemoPlatform::new(scenario, Some(demo_params))) as Box<dyn Platform>))
        } else {
            Box::new(sidestage_platform::platform)
        };
        let repaint = ctx.clone();
        let engine = Engine::spawn(
            make_platform,
            Arc::clone(&params),
            desired.clone(),
            Arc::new(move || repaint.request_repaint()),
        );
        let store = ConfigStore::new(config_path, &config);
        watch_for_termination(ctx);
        SidestageApp {
            engine,
            published: params.get(),
            params,
            sent_desired: desired,
            store,
            config,
            snap: Snapshot::default(),
            meters: DisplayMeters::default(),
            last_frame: Instant::now(),
            search: String::new(),
            reveal_source: true,
            show_diagnostics: launch.open_diagnostics,
            scroll_controls: launch.scroll_controls,
            bypass_held: false,
            applied_theme: None,
            screenshot: launch.screenshot,
            demo: launch.demo,
            closing: false,
        }
    }

    /// Push settings and session wishes to the renderer and engine when they changed.
    fn sync(&mut self) {
        let effective = sidestage_dsp::Settings { bypass: self.bypass_held, ..self.config.settings };
        if effective != self.published {
            self.params.set(effective);
            self.published = effective;
        }
        let desired = desired_of(&self.config);
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

    fn running(&self) -> bool {
        self.snap.session.as_ref().is_some_and(|s| s.state == SessionState::Running)
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

fn desired_of(config: &Config) -> Desired {
    Desired {
        enabled: config.enabled,
        app: config.app.clone(),
        output: config.output.clone(),
        hide: config.hide.clone(),
        sidechain: config.sidechain,
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
        // renderer bypassed behind a hidden window.
        if self.published.bypass && ctx.input(|i| i.viewport().minimized.unwrap_or(false)) {
            self.bypass_held = false;
            self.sync();
        }
        self.store.update(&self.config, Instant::now());
        if let Some(shot) = &mut self.screenshot {
            shot.tick(ctx);
        }
        // Ctrl+C in the launching terminal, SIGTERM/SIGHUP (logout, `kill`): close normally so
        // the session is stopped and the original output restored before the process ends.
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
        let reading = self.params.meters.take();
        self.meters.update(reading, &self.snap, dt);

        self.bypass_held = false;
        let full_width = ui.available_width();
        panels::top_bar(self, ui);
        panels::status_bar(self, ui);
        panels::left_panel(self, ui, (full_width * 0.23).clamp(206.0, 270.0));
        panels::right_panel(self, ui, (full_width * 0.30).clamp(272.0, 340.0));
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
        let busy = self.running() || self.snap.activity != crate::engine::Activity::Idle || self.screenshot.is_some();
        ui.ctx().request_repaint_after(if busy { Duration::from_millis(33) } else { Duration::from_millis(250) });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.store.flush(&self.config);
        log::info!("window closed; stopping");
        // Stops the session, which restores the music app's original output.
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
