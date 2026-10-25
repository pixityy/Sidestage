//! The top bar, the status bar, the radar in the middle and the Diagnostics window.

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, ScrollArea, Sense, Stroke,
    TextStyle, Ui, vec2,
};
use sidestage_platform::{HideMethod, SessionState, SourceStatus, Support};

use super::radar::{self, Puck, RadarInput};
use super::theme::{self, Palette};
use super::widgets;
use super::{SidestageApp, SourceView, ThemeChoice};
use crate::engine::Activity;
use crate::text;

// ---------------------------------------------------------------------------------------------
// Status

/// What the pill in the top bar says.
struct Status {
    text: String,
    color: Color32,
    tooltip: String,
}

/// "Spotify" for one source, "your apps" for several.
fn waiting_for(app: &SidestageApp) -> String {
    let enabled: Vec<&str> = app.config.sources.iter().filter(|s| s.enabled).map(|s| s.app.name.as_str()).collect();
    match enabled.as_slice() {
        [one] => (*one).to_owned(),
        _ => "your apps".to_owned(),
    }
}

fn status(app: &SidestageApp, p: &Palette) -> Status {
    let s = &app.snap;
    let mk = |text: String, color: Color32, tooltip: String| Status { text, color, tooltip };
    if !s.ready {
        return mk("Loading\u{2026}".into(), p.info, "Starting the audio engine.".into());
    }
    if let Some(e) = &s.platform_error {
        return mk("Error".into(), p.err, e.clone());
    }
    // Linux reports Unsupported while its sound server cannot be reached; a session that is
    // still there (it reconnects by itself) says more than that.
    if let Some(Support::Unsupported(why)) = &s.support
        && s.session.is_none()
    {
        return mk("Not supported".into(), p.err, why.clone());
    }
    match s.activity {
        Activity::Restoring => {
            return mk("Restoring\u{2026}".into(), p.info, "Undoing Sidestage's audio changes.".into());
        }
        Activity::Stopping => {
            return mk("Stopping\u{2026}".into(), p.info, "Giving your apps back their normal output.".into());
        }
        _ => {}
    }
    if !app.config.enabled {
        return mk("Off".into(), p.text_faint, "Sidestage is off; your apps play normally.".into());
    }
    if app.config.sources.is_empty() {
        return mk("Setup needed".into(), p.warn, "Add the apps you want around you in the list on the left.".into());
    }
    if !app.config.sources.iter().any(|s| s.enabled) {
        return mk("All sources off".into(), p.text_faint, "Turn a source on in the list on the left.".into());
    }
    if s.activity == Activity::Starting {
        return mk("Starting\u{2026}".into(), p.info, format!("Connecting to {}.", app.enabled_names()));
    }
    let names = app.enabled_names();
    if let Some(st) = &s.session {
        let any_live = st.sources.iter().any(|x| x.state == SessionState::Running);
        return match &st.state {
            SessionState::Running if any_live => mk("Running".into(), p.ok, text::status_summary(st)),
            SessionState::Running | SessionState::WaitingForApp => mk(
                format!("Waiting for {}", waiting_for(app)),
                p.warn,
                format!("Sidestage continues as soon as {names} play audio."),
            ),
            SessionState::Starting => mk("Starting\u{2026}".into(), p.info, format!("Connecting to {names}.")),
            SessionState::Error(e) => mk("Error".into(), p.err, e.clone()),
            SessionState::Stopped => mk("Stopped".into(), p.text_faint, String::new()),
        };
    }
    if let Some(e) = &s.error {
        return mk("Error".into(), p.err, e.clone());
    }
    if let Some(Support::NeedsPermission(what)) = &s.support {
        return mk("Setup needed".into(), p.warn, what.clone());
    }
    mk(
        format!("Waiting for {}", waiting_for(app)),
        p.warn,
        format!("Sidestage starts by itself when {names} play audio."),
    )
}

/// The line in the status bar.
fn status_line(app: &SidestageApp, p: &Palette) -> (String, Color32) {
    let s = &app.snap;
    if app.bypassing() {
        return ("Bypass: hearing your apps without Sidestage's processing".into(), p.warn);
    }
    if let Some(e) = &s.platform_error {
        return (format!("Audio capture is unavailable: {e}"), p.err);
    }
    if !s.ready {
        return ("Starting the audio engine\u{2026}".into(), p.text_dim);
    }
    if !app.config.enabled {
        return ("Off. Your apps play normally.".into(), p.text_dim);
    }
    if app.config.sources.is_empty() {
        return ("Add the apps you want around you on the left.".into(), p.text_dim);
    }
    if !app.config.sources.iter().any(|s| s.enabled) {
        return ("Every source is off. Turn one on in the list on the left.".into(), p.text_dim);
    }
    let names = app.enabled_names();
    match (&s.session, s.activity) {
        (_, Activity::Starting) => (format!("Starting {names}\u{2026}"), p.text_dim),
        (Some(st), _) if st.state == SessionState::Running => {
            let audible = st.sources.iter().any(|x| x.state == SessionState::Running && !x.original_hidden);
            (text::status_summary(st), if audible { p.warn } else { p.text_dim })
        }
        (Some(st), _) => (text::state(&st.state, &names), p.text_dim),
        (None, _) => match &s.error {
            Some(e) => (format!("Couldn't start: {e}"), p.err),
            None => (format!("Waiting for {names} to play audio. Sidestage starts by itself."), p.text_dim),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Top bar

pub(super) fn top_bar(app: &mut SidestageApp, ui: &mut Ui) {
    let p = theme::palette(ui.ctx());
    egui::Panel::top("top_bar")
        .frame(Frame::new().fill(p.bg).inner_margin(Margin { left: 14, right: 10, top: 9, bottom: 9 }))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                logo(ui, p);
                ui.add_space(10.0);
                let mut on = app.config.enabled;
                let tip = if on { "Turn Sidestage off (your apps play normally)" } else { "Turn Sidestage on" };
                if widgets::toggle(ui, p, &mut on, 22.0, "Sidestage on").on_hover_text(tip).changed() {
                    app.config.enabled = on;
                }
                ui.add_space(4.0);
                let st = status(app, p);
                let pill_max = (ui.available_width() - 340.0).clamp(110.0, 260.0);
                let pill = widgets::pill(ui, p, &st.text, st.color, pill_max);
                if !st.tooltip.is_empty() {
                    pill.on_hover_text(st.tooltip);
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let more = widgets::more_button(ui, p).on_hover_text("Menu");
                    menu(app, &more);
                    bypass_button(app, ui, p);
                    output_combo(app, ui, p);
                });
            });
        });
}

fn logo(ui: &mut Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
    let c = rect.center();
    let painter = ui.painter();
    painter.circle_stroke(c, 11.0, Stroke::new(1.3, p.accent.gamma_multiply(0.55)));
    painter.circle_stroke(c, 6.5, Stroke::new(1.0, p.accent.gamma_multiply(0.35)));
    painter.circle_filled(c, 3.5, p.text);
    painter.circle_filled(c + vec2(0.0, 8.0), 3.2, p.accent_hi);
    ui.label(RichText::new("Sidestage").size(16.5).color(p.text).strong());
}

fn bypass_button(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    let held = app.bypassing();
    let (fill, color) = if held { (p.warn.gamma_multiply(0.85), Color32::BLACK) } else { (p.surface_hi, p.text) };
    let response = ui
        .add(
            egui::Button::new(RichText::new("Bypass").color(color))
                .fill(fill)
                .sense(Sense::click_and_drag())
                .min_size(vec2(68.0, 28.0)),
        )
        .on_hover_text("Hold to hear your apps without Sidestage's processing (or hold B).");
    if response.is_pointer_button_down_on() {
        app.bypass_held = true;
    }
}

fn output_combo(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    let outputs = &app.snap.outputs;
    let default_name = outputs.iter().find(|o| o.is_default).map(|o| o.name.as_str());
    let selected = match &app.config.output {
        None => match default_name {
            Some(n) => format!("Default: {n}"),
            None => "System default".into(),
        },
        // No list yet (it arrives a moment after startup) or none at all: don't call it missing.
        Some(_) if outputs.is_empty() => "Saved device".into(),
        Some(id) => {
            outputs.iter().find(|o| &o.id == id).map_or_else(|| "Unavailable device".into(), |o| o.name.clone())
        }
    };
    let width = (ui.available_width() - 8.0).clamp(140.0, 230.0);
    let mut choice = app.config.output.clone();
    egui::ComboBox::from_id_salt("output_device")
        .width(width)
        .selected_text(RichText::new(selected).color(p.text))
        .truncate()
        .show_ui(ui, |ui| {
            let label = match default_name {
                Some(n) => format!("System default ({n})"),
                None => "System default".into(),
            };
            ui.selectable_value(&mut choice, None, label);
            for o in outputs {
                ui.selectable_value(&mut choice, Some(o.id.clone()), &o.name);
            }
        })
        .response
        .on_hover_text("Where Sidestage plays your sources. Use headphones: binaural audio does not work on speakers.");
    if choice != app.config.output {
        app.config.output = choice;
    }
}

fn menu(app: &mut SidestageApp, button: &egui::Response) {
    egui::Popup::menu(button).align(egui::RectAlign::BOTTOM_END).show(|ui| {
        ui.set_min_width(200.0);
        if ui.button("Diagnostics\u{2026}").clicked() {
            app.show_diagnostics = true;
            ui.close();
        }
        ui.separator();
        ui.label(RichText::new("Appearance").text_style(TextStyle::Small).weak());
        for (choice, label) in
            [(ThemeChoice::System, "Match system"), (ThemeChoice::Dark, "Dark"), (ThemeChoice::Light, "Light")]
        {
            if ui.radio(app.config.theme == choice, label).clicked() {
                app.config.theme = choice;
            }
        }
        ui.separator();
        ui.label(RichText::new(format!("Sidestage {}", env!("CARGO_PKG_VERSION"))).text_style(TextStyle::Small).weak());
    });
}

// ---------------------------------------------------------------------------------------------
// Status bar

pub(super) fn status_bar(app: &mut SidestageApp, ui: &mut Ui) {
    let p = theme::palette(ui.ctx());
    egui::Panel::bottom("status_bar").frame(Frame::new().fill(p.bg).inner_margin(Margin::symmetric(14, 5))).show(
        ui,
        |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let diag = ui
                        .add(egui::Button::new(RichText::new("Diagnostics").color(p.text_dim).size(12.5)).frame(false));
                    if diag.clicked() {
                        app.show_diagnostics = !app.show_diagnostics;
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let (line, color) = status_line(app, p);
                        let dot = if app.running() && !app.bypassing() && color != p.err { p.ok } else { color };
                        let (r, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 3.5, dot);
                        widgets::nowrap(ui, RichText::new(line).color(color).size(12.5));
                    });
                });
            });
        },
    );
}

// ---------------------------------------------------------------------------------------------
// Centre: the radar

pub(super) fn center(app: &mut SidestageApp, ui: &mut Ui) {
    let p = theme::palette(ui.ctx());
    egui::CentralPanel::default().frame(Frame::new().fill(p.stage).inner_margin(Margin::same(12))).show(ui, |ui| {
        let rect = ui.available_rect_before_wrap();
        let selected = app.config.selected_source().map(|s| s.id);
        // One puck per switched-on source, plus the selected one even while it is off, so it can
        // still be placed.
        let pucks: Vec<Puck> = app
            .config
            .sources
            .iter()
            .filter(|s| s.enabled || Some(s.id) == selected)
            .map(|s| {
                let m = app.meters.source(s.id);
                let depth = s.settings.ducking.depth_db;
                let view = app.view(s);
                Puck {
                    id: s.id,
                    settings: s.settings,
                    color: p.puck(s.color),
                    label: s.app.name.clone(),
                    level: widgets::meter_fraction(m.level),
                    duck: if depth > 0.0 { (m.duck_db / depth).clamp(0.0, 1.0) } else { 0.0 },
                    faded: !matches!(view, SourceView::Playing { .. }),
                    alert: matches!(view, SourceView::Failed(_)),
                }
            })
            .collect();
        // Lit while any source plays through Sidestage.
        let active = app.running() && app.config.sources.iter().any(|s| matches!(app.view(s), SourceView::Playing { .. }));
        let preset = app.config.selected_source().map_or_else(Default::default, |s| s.base_preset.settings());
        let name = app.config.selected_source().map(|s| (s.app.name.clone(), p.puck(s.color)));
        let input = RadarInput { palette: p, pucks: &pucks, selected, inactive: !active, preset };
        let settings = app.config.selected_source_mut().map(|s| &mut s.settings);
        let out = radar::radar(ui, settings, &input);
        if let Some(id) = out.select {
            app.config.selected = Some(id);
            app.reveal_selected = true;
        }

        // Readout (top left) and hints (bottom left) in the corners the circle leaves free.
        let painter = ui.painter();
        if let (Some(s), Some((name, color))) = (app.config.selected_source().map(|s| s.settings), name) {
            let readout = format!("{} \u{00b7} {}", radar::direction_name(s.azimuth_deg), radar::format_distance(s.distance_m));
            painter.circle_filled(rect.left_top() + vec2(5.0, 10.0), 4.5, color);
            painter.text(rect.left_top() + vec2(15.0, 0.0), Align2::LEFT_TOP, readout, FontId::proportional(16.0), p.text);
            let width =
                if s.width_deg < 0.5 { "point source".to_owned() } else { format!("{:.0}\u{00b0} wide", s.width_deg) };
            let detail = format!("{name} \u{00b7} {width} \u{00b7} height {}", radar::format_height(s.elevation_deg));
            let galley = widgets::truncated(ui, &detail, FontId::proportional(12.0), p.text_dim, rect.width() - 8.0);
            painter.galley(rect.left_top() + vec2(0.0, 22.0), galley, p.text_dim);
            let hint = if out.response.has_focus() {
                "Arrows: move \u{00b7} Shift+arrows: fine steps\nPage Up/Down: height \u{00b7} Home: reset"
            } else if cfg!(target_os = "macos") {
                "Drag to move \u{00b7} Cmd: snap \u{00b7} scroll: distance\nShift-drag: height \u{00b7} double-click: reset"
            } else {
                "Drag to move \u{00b7} Ctrl: snap \u{00b7} scroll: distance\nShift-drag: height \u{00b7} double-click: reset"
            };
            painter.text(rect.left_bottom(), Align2::LEFT_BOTTOM, hint, FontId::proportional(11.0), p.text_faint);
        }
        if app.bypassing() {
            let galley = painter.layout_no_wrap("BYPASS".into(), FontId::proportional(11.5), Color32::BLACK);
            let badge = egui::Rect::from_min_size(
                rect.right_top() - vec2(galley.size().x + 16.0, 0.0),
                galley.size() + vec2(16.0, 8.0),
            );
            painter.rect_filled(badge, CornerRadius::same(6), p.warn);
            painter.galley(badge.min + vec2(8.0, 4.0), galley, Color32::BLACK);
        } else if !active || app.config.sources.is_empty() {
            painter.text(rect.right_top(), Align2::RIGHT_TOP, idle_reason(app), FontId::proportional(12.0), p.text_faint);
        }
    });
}

/// Why the radar is dimmed, for its top-right corner.
fn idle_reason(app: &SidestageApp) -> String {
    let s = &app.snap;
    let all_sources_failed =
        app.config.sources.iter().filter(|x| x.enabled).all(|x| matches!(app.view(x), SourceView::Failed(_)));
    let failed = s.error.is_some()
        || s.session.as_ref().is_some_and(|st| matches!(st.state, SessionState::Error(_)))
        || all_sources_failed;
    if !app.config.enabled {
        "Off".into()
    } else if s.platform_error.is_some() || matches!(s.support, Some(Support::Unsupported(_))) {
        "Capture unavailable".into()
    } else if app.config.sources.is_empty() {
        "No sources yet".into()
    } else if !app.config.sources.iter().any(|s| s.enabled) {
        "All sources off".into()
    } else if failed {
        "Not capturing".into()
    } else {
        format!("Waiting for {}", waiting_for(app))
    }
}

// ---------------------------------------------------------------------------------------------
// Diagnostics

pub(super) fn diagnostics(app: &mut SidestageApp, ctx: &egui::Context) {
    let p = theme::palette(ctx);
    let mut open = app.show_diagnostics;
    egui::Window::new("Diagnostics")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([480.0, 480.0])
        .max_height(ctx.content_rect().height() - 60.0)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| diagnostics_body(app, ui, p));
        });
    app.show_diagnostics = open;
}

/// Width of the key column, the same in every table so they line up.
const DIAG_KEY_W: f32 = 118.0;

fn row(ui: &mut Ui, p: &Palette, k: &str, v: String) {
    ui.label(RichText::new(k).color(p.text_dim));
    ui.add(egui::Label::new(RichText::new(v).color(p.text)).wrap());
    ui.end_row();
}

fn state_text(state: &SessionState) -> String {
    match state {
        SessionState::Starting => "Starting".to_owned(),
        SessionState::Running => "Running".to_owned(),
        SessionState::WaitingForApp => "Waiting for the app".to_owned(),
        SessionState::Error(e) => format!("Error: {e}"),
        SessionState::Stopped => "Stopped".to_owned(),
    }
}

fn level_text(linear: f32) -> String {
    if linear > 1e-6 { format!("{:.1} dBFS", text::to_db(linear)) } else { "Silent".to_owned() }
}

fn diagnostics_body(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    let s = &app.snap;
    widgets::section(ui, p, "Session");
    egui::Grid::new("diag_session").num_columns(2).spacing([16.0, 5.0]).min_col_width(DIAG_KEY_W).show(
        ui,
        |ui| match &s.session {
            Some(st) => {
                row(ui, p, "State", state_text(&st.state));
                row(ui, p, "Output", st.output_name.clone());
                row(ui, p, "Sample rate", text::sample_rate(st.sample_rate));
                let sidechain = if st.sidechain_active {
                    level_text(st.sidechain_peak)
                } else if app.config.sidechain {
                    "Not measuring right now".to_owned()
                } else {
                    "Off".to_owned()
                };
                row(ui, p, "Game level", sidechain);
                let ignored: Vec<&str> = app.config.ducking_ignore.iter().map(|a| a.name.as_str()).collect();
                row(
                    ui,
                    p,
                    "Never ducks for",
                    if ignored.is_empty() { "\u{2013}".into() } else { text::join_names(&ignored) },
                );
                row(ui, p, "Output latency", format!("{:.1} ms", st.latency_ms));
                row(ui, p, "Output dropouts", st.dropouts.to_string());
                if let Some(since) = s.session_since {
                    row(ui, p, "Running for", format_duration(since.elapsed().as_secs()));
                }
                row(ui, p, "Warnings", if st.warnings.is_empty() { "None".into() } else { st.warnings.join("\n") });
            }
            None => {
                let why = match (&s.error, app.config.enabled) {
                    (Some(e), _) => format!("Not running: {e}"),
                    (None, false) => "Off".into(),
                    (None, true) => "Not running".into(),
                };
                row(ui, p, "State", why);
            }
        },
    );

    for (i, source) in app.config.sources.iter().enumerate() {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.5, p.puck(source.color));
            widgets::section(ui, p, &format!("Source: {}", source.app.name));
        });
        let status: Option<&SourceStatus> = s.source(source.id);
        egui::Grid::new(("diag_source", i)).num_columns(2).spacing([16.0, 5.0]).min_col_width(DIAG_KEY_W).show(
            ui,
            |ui| {
                let preset = match sidestage_dsp::Preset::matching(&source.settings) {
                    Some(p) => p.name().to_owned(),
                    None => format!("{} (edited)", source.base_preset.name()),
                };
                row(ui, p, "Preset", preset);
                match (status, app.config.enabled && source.enabled) {
                    (Some(st), true) => {
                        row(ui, p, "State", state_text(&st.state));
                        row(ui, p, "Original hidden", if st.original_hidden { "Yes".into() } else { "No".into() });
                        row(ui, p, "Capture peak", level_text(st.capture_peak));
                        row(ui, p, "Turned down", format!("-{:.1} dB", app.meters.source(source.id).duck_db));
                        row(ui, p, "Added latency", format!("{:.1} ms", st.latency_ms));
                        row(ui, p, "Dropouts", st.dropouts.to_string());
                        row(ui, p, "Clock drift", format!("{:+.1} ppm", st.drift_ppm));
                        if !st.warnings.is_empty() {
                            row(ui, p, "Warnings", st.warnings.join("\n"));
                        }
                    }
                    (None, true) => row(ui, p, "State", "Not started".into()),
                    (_, false) => row(ui, p, "State", "Off: the app plays normally".into()),
                }
                row(ui, p, "App key", source.app.key.0.clone());
            },
        );
    }

    ui.add_space(8.0);
    widgets::section(ui, p, "Backend");
    egui::Grid::new("diag_backend").num_columns(2).spacing([16.0, 5.0]).min_col_width(DIAG_KEY_W).show(ui, |ui| {
        row(ui, p, "Backend", if s.platform_name.is_empty() { "\u{2013}".into() } else { s.platform_name.clone() });
        let support = match &s.support {
            None => s.platform_error.clone().unwrap_or_else(|| "\u{2013}".into()),
            Some(Support::Supported) => "Supported".into(),
            Some(Support::NeedsPermission(w)) => format!("Needs permission: {w}"),
            Some(Support::Unsupported(w)) => format!("Unsupported: {w}"),
        };
        row(ui, p, "Support", support);
        row(ui, p, "Apps with audio", s.apps.len().to_string());
        row(ui, p, "Output devices", s.outputs.len().to_string());
        if let Some(e) = &s.apps_error {
            row(ui, p, "App list error", e.clone());
        }
        if app.demo {
            row(ui, p, "Mode", "Demo (nothing is captured)".into());
        }
    });

    ui.add_space(8.0);
    widgets::section(ui, p, "Hide the originals");
    let mut methods = s.hide_methods.clone();
    if !methods.iter().any(|(m, _)| *m == app.config.hide) {
        let label = match &app.config.hide {
            HideMethod::Auto => "Automatic".to_owned(),
            HideMethod::KeepOriginal => "Keep the originals audible".to_owned(),
            HideMethod::ParkOn(id) => {
                let device = s.outputs.iter().find(|o| &o.id == id).map_or(id.as_str(), |o| o.name.as_str());
                format!("Send the originals to {device} (not offered right now)")
            }
        };
        methods.push((app.config.hide.clone(), label));
    }
    for (method, label) in methods {
        let selected = app.config.hide == method;
        if ui.radio(selected, label).clicked() && !selected {
            app.config.hide = method;
        }
    }
    ui.add_space(2.0);
    ui.checkbox(&mut app.config.sidechain, "Measure the game level for ducking").on_hover_text(
        "Listens to everything except your sources, the apps that never duck and Sidestage (the sidechain).",
    );
    ui.add(
        egui::Label::new(
            RichText::new(
                "Changing how the originals are hidden restarts every source; changing the game measurement \
                 restarts the session.",
            )
            .color(p.text_faint)
            .text_style(TextStyle::Small),
        )
        .wrap(),
    );

    ui.add_space(8.0);
    widgets::section(ui, p, "Restore");
    ui.add(
        egui::Label::new(
            RichText::new(
                "If an app stays silent or on the wrong output after Sidestage crashed, this stops Sidestage and \
                 undoes every audio change it made.",
            )
            .color(p.text_dim),
        )
        .wrap(),
    );
    let busy = app.snap.activity == Activity::Restoring;
    if ui.add_enabled(!busy, egui::Button::new("Stop and restore original audio")).clicked() {
        app.config.enabled = false;
        // Send the power-off first so the engine does not restart the session right away.
        app.sync();
        app.engine.restore();
    }

    ui.add_space(8.0);
    widgets::section(ui, p, "Files");
    let config = app.store.path().map_or_else(|| "Not saved (demo)".to_owned(), |p| p.display().to_string());
    let log = crate::logging::written_log()
        .map_or_else(|| "Not written for this run".to_owned(), |p| p.display().to_string());
    egui::Grid::new("diag_files").num_columns(2).spacing([16.0, 5.0]).min_col_width(DIAG_KEY_W).show(ui, |ui| {
        ui.label(RichText::new("Settings").color(p.text_dim));
        ui.add(egui::Label::new(RichText::new(config).monospace().size(11.5)).wrap());
        ui.end_row();
        ui.label(RichText::new("Log").color(p.text_dim));
        ui.add(egui::Label::new(RichText::new(log).monospace().size(11.5)).wrap());
        ui.end_row();
    });
    ui.add_space(4.0);
    ui.label(
        RichText::new(format!("Sidestage {}", env!("CARGO_PKG_VERSION")))
            .color(p.text_faint)
            .text_style(TextStyle::Small),
    );
}

fn format_duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min {} s", s / 60, s % 60),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(format_duration(5), "5 s");
        assert_eq!(format_duration(125), "2 min 5 s");
        assert_eq!(format_duration(7300), "2 h 1 min");
    }
}
