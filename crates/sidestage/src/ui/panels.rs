//! The panels of the main window and the Diagnostics window.

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, ScrollArea, Sense, Stroke,
    StrokeKind, TextStyle, Ui, pos2, vec2,
};
use sidestage_dsp::{DuckMode, Preset, Room};
use sidestage_platform::{AudioApp, HideMethod, SessionState, Support};

use super::radar::{self, RadarInput};
use super::theme::{self, Palette};
use super::widgets::{self, CardKind};
use super::{SidestageApp, ThemeChoice};
use crate::config::{ChosenApp, same_app};
use crate::engine::{Activity, NoticeKind};
use crate::text;

// ---------------------------------------------------------------------------------------------
// Status

/// What the pill in the top bar says.
struct Status {
    text: String,
    color: Color32,
    tooltip: String,
}

fn status(app: &SidestageApp, p: &Palette) -> Status {
    let s = &app.snap;
    let name = app.config.app.as_ref().map_or("your music app", |a| a.name.as_str()).to_owned();
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
            return mk("Stopping\u{2026}".into(), p.info, format!("Giving {name} back its normal output."));
        }
        _ => {}
    }
    if !app.config.enabled {
        return mk("Off".into(), p.text_faint, format!("Sidestage is off; {name} plays normally."));
    }
    if app.config.app.is_none() {
        return mk("Setup needed".into(), p.warn, "Pick the app that plays your music in the list on the left.".into());
    }
    if s.activity == Activity::Starting {
        return mk("Starting\u{2026}".into(), p.info, format!("Connecting to {name}."));
    }
    if let Some(st) = &s.session {
        return match &st.state {
            SessionState::Running => mk("Running".into(), p.ok, text::status_summary(st)),
            SessionState::WaitingForApp => {
                mk(format!("Waiting for {name}"), p.warn, format!("Sidestage continues as soon as {name} plays again."))
            }
            SessionState::Starting => mk("Starting\u{2026}".into(), p.info, format!("Connecting to {name}.")),
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
    mk(format!("Waiting for {name}"), p.warn, format!("Sidestage starts by itself when {name} plays audio."))
}

/// The line in the status bar.
fn status_line(app: &SidestageApp, p: &Palette) -> (String, Color32) {
    let s = &app.snap;
    let name = app.config.app.as_ref().map_or("your music app", |a| a.name.as_str());
    if app.published.bypass {
        return (format!("Bypass: hearing {name} without Sidestage's processing"), p.warn);
    }
    if let Some(e) = &s.platform_error {
        return (format!("Audio capture is unavailable: {e}"), p.err);
    }
    if !s.ready {
        return ("Starting the audio engine\u{2026}".into(), p.text_dim);
    }
    if !app.config.enabled {
        return (format!("Off. {name} plays normally."), p.text_dim);
    }
    if app.config.app.is_none() {
        return ("Pick the app that plays your music on the left.".into(), p.text_dim);
    }
    match (&s.session, s.activity) {
        (_, Activity::Starting) => (format!("Starting {name}\u{2026}"), p.text_dim),
        (Some(st), _) if st.state == SessionState::Running => {
            let color = if st.original_hidden { p.text_dim } else { p.warn };
            (text::status_summary(st), color)
        }
        (Some(st), _) => (text::state(&st.state, name), p.text_dim),
        (None, _) => match &s.error {
            Some(e) => (format!("Couldn't capture {name}: {e}"), p.err),
            None => (format!("Waiting for {name} to play audio. Sidestage starts by itself."), p.text_dim),
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
                let tip = if on { "Turn Sidestage off (the music plays normally)" } else { "Turn Sidestage on" };
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
    let held = app.published.bypass;
    let (fill, color) = if held { (p.warn.gamma_multiply(0.85), Color32::BLACK) } else { (p.surface_hi, p.text) };
    let response = ui
        .add(
            egui::Button::new(RichText::new("Bypass").color(color))
                .fill(fill)
                .sense(Sense::click_and_drag())
                .min_size(vec2(68.0, 28.0)),
        )
        .on_hover_text("Hold to hear the music without Sidestage's processing (or hold B).");
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
        .on_hover_text("Where the spatialized music plays. Use headphones: binaural audio does not work on speakers.");
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
                        let dot = if app.running() && !app.published.bypass { p.ok } else { color };
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
// Left panel: music source

enum CardAction {
    Retry,
    Diagnostics,
    DismissNotice(u64),
    DismissHint(&'static str),
    EnableSidechain,
    HideOriginal,
}

struct CardSpec {
    kind: CardKind,
    title: String,
    lines: Vec<String>,
    buttons: Vec<(&'static str, CardAction)>,
}

fn cards(app: &SidestageApp) -> Vec<CardSpec> {
    let s = &app.snap;
    let name = app.config.app.as_ref().map_or("your music app", |a| a.name.as_str()).to_owned();
    let mut out = Vec::new();
    if let Some(e) = &s.platform_error {
        out.push(CardSpec {
            kind: CardKind::Error,
            title: "Audio capture is unavailable".into(),
            lines: vec![e.clone()],
            buttons: vec![("Diagnostics", CardAction::Diagnostics)],
        });
    }
    if let Some(Support::Unsupported(why)) = &s.support
        && s.session.is_none()
    {
        out.push(CardSpec {
            kind: CardKind::Error,
            title: "This system isn't supported".into(),
            lines: vec![why.clone()],
            buttons: vec![],
        });
    }
    let session_error = s.session.as_ref().and_then(|st| match &st.state {
        SessionState::Error(e) => Some(e.clone()),
        _ => None,
    });
    if let Some(e) = session_error.or_else(|| s.error.clone()) {
        let mut lines = vec![e];
        if s.retry_pending {
            lines.push("Sidestage will try again in a moment.".into());
        }
        out.push(CardSpec {
            kind: CardKind::Error,
            title: format!("Couldn't capture {name}"),
            lines,
            buttons: vec![("Try again", CardAction::Retry), ("Diagnostics", CardAction::Diagnostics)],
        });
    }
    if let Some(st) = &s.session
        && !st.warnings.is_empty()
    {
        out.push(CardSpec {
            kind: CardKind::Warning,
            title: "Check your audio setup".into(),
            lines: st.warnings.clone(),
            buttons: vec![],
        });
    }
    // Only where the backend offers it: Linux always hides the original (and says so).
    if app.config.enabled
        && app.config.hide == HideMethod::KeepOriginal
        && app.config.app.is_some()
        && s.hide_methods.iter().any(|(m, _)| *m == HideMethod::KeepOriginal)
    {
        out.push(CardSpec {
            kind: CardKind::Warning,
            title: "You'll hear the music twice".into(),
            lines: vec![format!("The original {name} is not hidden, so it plays alongside the spatial version.")],
            buttons: vec![("Hide the original", CardAction::HideOriginal)],
        });
    }
    for n in &s.notices {
        let kind = match n.kind {
            NoticeKind::Info => CardKind::Info,
            NoticeKind::Warning => CardKind::Warning,
            NoticeKind::Error => CardKind::Error,
        };
        out.push(CardSpec {
            kind,
            title: n.title.clone(),
            lines: n.lines.clone(),
            buttons: vec![("Dismiss", CardAction::DismissNotice(n.id))],
        });
    }
    if let Some(Support::NeedsPermission(what)) = &s.support {
        out.push(CardSpec {
            kind: CardKind::Info,
            title: "Allow audio capture".into(),
            lines: vec![
                what.clone(),
                format!("While Sidestage runs, {name} plays only through Sidestage. Quit Sidestage and it plays normally again."),
            ],
            buttons: vec![],
        });
    }
    if !app.config.sidechain && app.config.settings.ducking.enabled {
        out.push(CardSpec {
            kind: CardKind::Info,
            title: "Ducking can't hear the game".into(),
            lines: vec!["Game-level metering is off, so the music won't get quieter when the game is loud.".into()],
            buttons: vec![("Turn it on", CardAction::EnableSidechain)],
        });
    }
    if let Some((id, title, body)) = platform_hint(&name)
        && app.running()
        && app.config.hide == HideMethod::Auto
        && !app.config.dismissed_hints.iter().any(|h| h == id)
    {
        out.push(CardSpec {
            kind: CardKind::Info,
            title: title.into(),
            lines: vec![body],
            buttons: vec![("Got it", CardAction::DismissHint(id))],
        });
    }
    out
}

/// A one-time explanation of how the original is hidden on this OS.
fn platform_hint(name: &str) -> Option<(&'static str, &'static str, String)> {
    if cfg!(windows) {
        Some((
            "windows-mixer",
            "About the Volume mixer",
            format!(
                "{name} shows at 0 in the Volume mixer while Sidestage runs. That's expected: it's how the original is hidden."
            ),
        ))
    } else if cfg!(target_os = "linux") {
        Some((
            "linux-sink",
            "Where your music went",
            format!(
                "While Sidestage runs, {name} plays into a silent Sidestage output. It moves back when Sidestage stops."
            ),
        ))
    } else {
        None
    }
}

fn apply_card_action(app: &mut SidestageApp, action: CardAction) {
    match action {
        CardAction::Retry => app.engine.retry(),
        CardAction::Diagnostics => app.show_diagnostics = true,
        CardAction::DismissNotice(id) => app.engine.dismiss_notice(id),
        CardAction::DismissHint(id) => app.config.dismissed_hints.push(id.to_owned()),
        CardAction::EnableSidechain => app.config.sidechain = true,
        CardAction::HideOriginal => app.config.hide = HideMethod::Auto,
    }
}

pub(super) fn left_panel(app: &mut SidestageApp, ui: &mut Ui, width: f32) {
    let p = theme::palette(ui.ctx());
    egui::Panel::left("music_source")
        .exact_size(width)
        .resizable(false)
        .frame(Frame::new().fill(p.bg).inner_margin(Margin { left: 12, right: 10, top: 12, bottom: 10 }))
        .show(ui, |ui| {
            widgets::section(ui, p, "Music source");
            if app.config.app.is_none() && app.snap.platform_error.is_none() {
                ui.add(
                    egui::Label::new(
                        RichText::new("Click the app that plays your music.")
                            .color(p.accent_hi)
                            .text_style(TextStyle::Small),
                    )
                    .wrap(),
                );
                ui.add_space(2.0);
            }
            ui.add(
                egui::TextEdit::singleline(&mut app.search)
                    .hint_text("Search apps")
                    .desired_width(f32::INFINITY)
                    .margin(Margin::symmetric(8, 5)),
            );
            ui.add_space(6.0);

            // Cards go under the list. Their height is only known after drawing them, so the
            // list is sized with last frame's card height (one frame of lag when cards change).
            let specs = cards(app);
            let cards_id = ui.id().with("cards_height");
            let cards_h = if specs.is_empty() { 0.0 } else { ui.data(|d| d.get_temp::<f32>(cards_id)).unwrap_or(0.0) };
            let max_cards_h = (ui.available_height() * 0.5).max(80.0);
            let gap = if specs.is_empty() { 0.0 } else { ui.spacing().item_spacing.y };
            let list_h = (ui.available_height() - gap - cards_h.min(max_cards_h)).max(90.0);
            // When the list gets shorter (a card appeared, the window shrank), bring the chosen
            // app back into view.
            let list_h_id = ui.id().with("list_height");
            if ui.data(|d| d.get_temp::<f32>(list_h_id)).is_none_or(|h| (h - list_h).abs() > 0.5) {
                ui.data_mut(|d| d.insert_temp(list_h_id, list_h));
                app.reveal_source = true;
            }
            ScrollArea::vertical().id_salt("apps_scroll").max_height(list_h).auto_shrink([false, false]).show(
                ui,
                |ui| {
                    source_list(app, ui, p);
                },
            );
            if specs.is_empty() {
                return;
            }
            let mut action = None;
            let out = ScrollArea::vertical()
                .id_salt("cards_scroll")
                .max_height(max_cards_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add_space(8.0);
                    for spec in specs {
                        let labels: Vec<&str> = spec.buttons.iter().map(|(l, _)| *l).collect();
                        if let Some(i) = widgets::card(ui, p, spec.kind, &spec.title, &spec.lines, &labels) {
                            action = spec.buttons.into_iter().nth(i).map(|(_, a)| a);
                        }
                        ui.add_space(6.0);
                    }
                });
            let measured = out.content_size.y;
            if (measured - cards_h).abs() > 0.5 {
                ui.data_mut(|d| d.insert_temp(cards_id, measured));
                ui.ctx().request_repaint();
            }
            if let Some(a) = action {
                apply_card_action(app, a);
            }
        });
}

fn source_list(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    let query = app.search.trim().to_lowercase();
    let mut apps: Vec<&AudioApp> = app
        .snap
        .apps
        .iter()
        .filter(|a| {
            query.is_empty() || a.name.to_lowercase().contains(&query) || a.detail.to_lowercase().contains(&query)
        })
        .collect();
    apps.sort_by_cached_key(|a| a.name.to_lowercase());

    let chosen = app.config.app.clone();
    // Pending until the chosen row is actually drawn (the app list arrives a moment after
    // startup).
    let reveal = app.reveal_source && chosen.is_some();
    app.reveal_source = reveal;
    let mut pick = None;
    if let Some(c) = &chosen
        && !app.snap.app_present(&c.key)
        && (query.is_empty() || c.name.to_lowercase().contains(&query))
    {
        let response = source_row(ui, p, &c.name, "Not running \u{00b7} starts when it plays", false, 0.0, true, false);
        if reveal {
            response.scroll_to_me(None);
            app.reveal_source = false;
        }
    }
    for a in &apps {
        let selected = chosen.as_ref().is_some_and(|c| same_app(&c.key, &a.key));
        let level = app.meters.apps.get(&a.key).copied().unwrap_or(0.0);
        let detail = if a.detail.is_empty() { "" } else { a.detail.as_str() };
        let response = source_row(ui, p, &a.name, detail, a.playing, level, selected, true);
        if selected && reveal {
            // Only scrolls if the row is not fully visible.
            response.scroll_to_me(None);
            app.reveal_source = false;
        }
        if response.clicked() && !selected {
            pick = Some(ChosenApp { key: a.key.clone(), name: a.name.clone() });
        }
        response.on_hover_text(format!("{}\n{}", a.name, a.key.0));
    }
    if apps.is_empty() {
        ui.add_space(8.0);
        let msg = if !app.snap.ready {
            "Looking for apps\u{2026}".to_owned()
        } else if app.snap.platform_error.is_some() {
            "Apps can't be listed because audio capture is unavailable.".to_owned()
        } else if !query.is_empty() {
            format!("No app matches \u{201c}{}\u{201d}.", app.search.trim())
        } else if let Some(e) = &app.snap.apps_error {
            format!("Couldn't list apps: {e}")
        } else {
            "No apps are playing audio. Start your music and it shows up here.".to_owned()
        };
        ui.add(egui::Label::new(RichText::new(msg).color(p.text_faint).text_style(TextStyle::Small)).wrap());
    }
    if let Some(choice) = pick {
        log::info!("music source: {} ({})", choice.name, choice.key.0);
        app.config.app = Some(choice);
        app.config.enabled = true;
    }
}

#[expect(clippy::too_many_arguments)]
fn source_row(
    ui: &mut Ui,
    p: &Palette,
    name: &str,
    detail: &str,
    playing: bool,
    level: f32,
    selected: bool,
    present: bool,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(vec2(width, 50.0), if present { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let r = CornerRadius::same(8);
        if selected {
            painter.rect_filled(rect, r, p.accent.gamma_multiply(if p.dark { 0.16 } else { 0.10 }));
            painter.rect_stroke(rect, r, Stroke::new(1.0, p.accent.gamma_multiply(0.55)), StrokeKind::Inside);
        } else if response.hovered() {
            painter.rect_filled(rect, r, p.surface_hi.gamma_multiply(if p.dark { 0.8 } else { 1.0 }));
        }
        if response.has_focus() {
            painter.rect_stroke(rect, r, Stroke::new(1.5, p.accent), StrokeKind::Inside);
        }
        let dot = pos2(rect.left() + 14.0, rect.top() + 17.0);
        if playing {
            painter.circle_filled(dot, 4.0, p.ok);
        } else {
            painter.circle_stroke(dot, 3.5, Stroke::new(1.2, p.text_faint));
        }
        let text_left = rect.left() + 28.0;
        let text_w = rect.right() - 10.0 - text_left;
        let alpha = if present { 1.0 } else { 0.6 };
        let name_g = widgets::truncated(ui, name, FontId::proportional(13.5), p.text.gamma_multiply(alpha), text_w);
        painter.galley(pos2(text_left, rect.top() + 8.0), name_g, p.text);
        if !detail.is_empty() {
            let detail_g =
                widgets::truncated(ui, detail, FontId::proportional(11.5), p.text_dim.gamma_multiply(alpha), text_w);
            painter.galley(pos2(text_left, rect.top() + 25.0), detail_g, p.text_dim);
        }
        if present {
            let bar = egui::Rect::from_min_max(
                pos2(text_left, rect.bottom() - 8.0),
                pos2(rect.right() - 10.0, rect.bottom() - 5.0),
            );
            let color = if selected { p.accent } else { p.ok.gamma_multiply(0.8) };
            widgets::level_bar(ui, p, bar, widgets::meter_fraction(level), color);
        }
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, name));
    response
}

// ---------------------------------------------------------------------------------------------
// Right panel: presets, sound, ducking

pub(super) fn right_panel(app: &mut SidestageApp, ui: &mut Ui, width: f32) {
    let p = theme::palette(ui.ctx());
    egui::Panel::right("controls")
        .exact_size(width)
        .resizable(false)
        .frame(Frame::new().fill(p.bg).inner_margin(Margin { left: 14, right: 6, top: 12, bottom: 10 }))
        .show(ui, |ui| {
            ScrollArea::vertical().id_salt("controls_scroll").auto_shrink([false, false]).show(ui, |ui| {
                ui.set_width(ui.available_width() - 10.0);
                presets(app, ui, p);
                ui.add_space(10.0);
                sound(app, ui, p);
                ui.add_space(10.0);
                ducking(app, ui, p);
                ui.add_space(8.0);
                if app.scroll_controls {
                    ui.scroll_to_cursor(Some(Align::Max));
                }
            });
        });
}

fn presets(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    widgets::section(ui, p, "Presets");
    let exact = Preset::matching(&app.config.settings);
    let base = app.config.base_preset;
    let gap = 8.0;
    let card_w = ((ui.available_width() - gap) / 2.0).floor();
    let pad = 9.0;
    let name_font = FontId::proportional(13.5);
    let desc_font = FontId::proportional(11.5);
    // All cards get the height of the tallest one so the grid stays even.
    let desc_h = Preset::ALL
        .iter()
        .map(|pr| {
            ui.painter().layout(pr.description().to_owned(), desc_font.clone(), p.text_dim, card_w - 2.0 * pad).size().y
        })
        .fold(0.0, f32::max);
    let card_h = pad * 2.0 + 17.0 + 3.0 + desc_h;
    let mut clicked = None;
    for row in Preset::ALL.chunks(2) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for preset in row {
                let (rect, response) = ui.allocate_exact_size(vec2(card_w, card_h), Sense::click());
                let active = exact == Some(*preset);
                let edited = !active && exact.is_none() && base == *preset;
                let painter = ui.painter();
                let r = CornerRadius::same(theme::CARD_RADIUS);
                let (fill, stroke) = if active {
                    (p.accent.gamma_multiply(if p.dark { 0.20 } else { 0.12 }), Stroke::new(1.5, p.accent))
                } else if edited {
                    (p.surface, Stroke::new(1.2, p.accent.gamma_multiply(0.6)))
                } else if response.hovered() {
                    (p.surface_hi, Stroke::new(1.0, p.border))
                } else {
                    (p.surface, Stroke::new(1.0, p.border))
                };
                painter.rect(rect, r, fill, stroke, StrokeKind::Inside);
                if response.has_focus() {
                    painter.rect_stroke(rect.expand(2.0), r, Stroke::new(1.5, p.accent_hi), StrokeKind::Outside);
                }
                let name_color = if active { p.text } else { p.text.gamma_multiply(0.92) };
                let name = painter.text(
                    rect.min + vec2(pad, pad),
                    Align2::LEFT_TOP,
                    preset.name(),
                    name_font.clone(),
                    name_color,
                );
                if edited {
                    // "edited" beside the name, or a pencil mark where the card is too narrow.
                    let tag = painter.layout_no_wrap("edited".into(), FontId::proportional(10.5), p.accent_hi);
                    let right = pos2(rect.right() - pad, rect.top() + pad + 2.0);
                    if name.right() + 6.0 + tag.size().x <= right.x {
                        painter.galley(right - vec2(tag.size().x, 0.0), tag, p.accent_hi);
                    } else {
                        pencil(painter, pos2(right.x - 5.0, name.center().y), p.accent_hi);
                    }
                }
                let desc =
                    painter.layout(preset.description().to_owned(), desc_font.clone(), p.text_dim, card_w - 2.0 * pad);
                painter.galley(rect.min + vec2(pad, pad + 20.0), desc, p.text_dim);
                response.widget_info(|| {
                    let name = if edited { format!("{} (edited)", preset.name()) } else { preset.name().to_owned() };
                    egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, active, name)
                });
                let response = if edited {
                    response.on_hover_text(
                        "Your settings started from this preset and were changed. Click to go back to it.",
                    )
                } else {
                    response
                };
                if response.clicked() {
                    clicked = Some(*preset);
                }
            }
        });
        ui.add_space(gap - ui.spacing().item_spacing.y);
    }
    if let Some(preset) = clicked {
        app.config.apply_preset(preset);
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let mut on = app.config.footstep_safe();
        let response = widgets::toggle(ui, p, &mut on, 18.0, "Footstep-safe");
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(RichText::new("Footstep-safe").color(p.text));
            ui.add(
                egui::Label::new(RichText::new("Keeps 2\u{2013}5 kHz clear and ducks harder").color(p.text_dim).text_style(TextStyle::Small))
                    .wrap(),
            );
        });
        if response
            .on_hover_text("Cuts the music around 3 kHz where footsteps live, rolls off highs and bass, and uses Competitive ducking.")
            .changed()
        {
            app.config.set_footstep_safe(on);
        }
    });
}

/// A small pencil ("edited") centred on `c`.
fn pencil(painter: &egui::Painter, c: egui::Pos2, color: Color32) {
    let tip = c + vec2(-4.0, 4.0);
    let end = c + vec2(4.0, -4.0);
    painter.line_segment([tip + vec2(1.5, -1.5), end], Stroke::new(2.4, color));
    painter.line_segment([tip, tip + vec2(1.6, -1.6)], Stroke::new(1.2, color));
}

fn sound(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    widgets::section(ui, p, "Sound");
    let s = &mut app.config.settings;
    widgets::slider_row(
        ui,
        p,
        "Distance",
        "How far away the music is.",
        &mut s.distance_m,
        0.5..=20.0,
        true,
        radar::format_distance,
    );
    widgets::slider_row(
        ui,
        p,
        "Height",
        "Below (-) or above (+) your ears.",
        &mut s.elevation_deg,
        -40.0..=80.0,
        false,
        radar::format_height,
    );
    widgets::slider_row(
        ui,
        p,
        "Width",
        "Angle between the left and right virtual speakers.",
        &mut s.width_deg,
        0.0..=120.0,
        false,
        |v| {
            if v < 0.5 { "Point".into() } else { format!("{v:.0}\u{00b0}") }
        },
    );
    let mut muffle = muffle_amount(s.lowpass_hz);
    if widgets::slider_row(
        ui,
        p,
        "Muffle",
        "Rolls off the highs, like music through a wall.",
        &mut muffle,
        0.0..=1.0,
        false,
        |m| {
            let hz = muffle_cutoff(m);
            if hz >= 19_500.0 { "Off".into() } else { format!("{:.1} kHz", hz / 1000.0) }
        },
    )
    .changed()
    {
        s.lowpass_hz = muffle_cutoff(muffle);
    }
    widgets::slider_row(ui, p, "Bass", "Low shelf at 150 Hz.", &mut s.bass_db, -12.0..=12.0, false, |v| {
        format!("{v:+.1} dB")
    });
    let mut dip = -s.presence_dip_db;
    if widgets::slider_row(
        ui,
        p,
        "Footstep dip",
        "Static cut around 3.2 kHz, where footsteps and reloads live.",
        &mut dip,
        0.0..=12.0,
        false,
        |v| if v < 0.05 { "Off".into() } else { format!("-{v:.1} dB") },
    )
    .changed()
    {
        s.presence_dip_db = -dip;
    }
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(widgets::LABEL_W, ui.spacing().interact_size.y), Sense::hover());
        ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, "Room", FontId::proportional(13.0), p.text_dim);
        egui::ComboBox::from_id_salt("room").width(ui.available_width()).selected_text(s.room.name()).show_ui(
            ui,
            |ui| {
                for room in Room::ALL {
                    ui.selectable_value(&mut s.room, room, room.name());
                }
            },
        );
    });
    widgets::slider_row(
        ui,
        p,
        "Reverb",
        "Level of the room's reverb relative to the music.",
        &mut s.reverb_db,
        -40.0..=6.0,
        false,
        |v| {
            if v <= -39.5 { "Dry".into() } else { format!("{v:+.0} dB") }
        },
    );
    widgets::slider_row(
        ui,
        p,
        "Volume",
        "Sidestage's music volume (use this, not the app's own slider).",
        &mut s.volume_db,
        -40.0..=12.0,
        false,
        |v| format!("{v:+.1} dB"),
    );
}

const MUFFLE_MAX_HZ: f32 = 20_000.0;
const MUFFLE_MIN_HZ: f32 = 800.0;

/// Low-pass cutoff to a 0..1 "muffle" amount (0 = off), log-spaced so the slider feels even.
pub fn muffle_amount(lowpass_hz: f32) -> f32 {
    let hz = lowpass_hz.clamp(MUFFLE_MIN_HZ, MUFFLE_MAX_HZ);
    (MUFFLE_MAX_HZ / hz).ln() / (MUFFLE_MAX_HZ / MUFFLE_MIN_HZ).ln()
}

pub fn muffle_cutoff(amount: f32) -> f32 {
    MUFFLE_MAX_HZ * (MUFFLE_MIN_HZ / MUFFLE_MAX_HZ).powf(amount.clamp(0.0, 1.0))
}

fn ducking(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    widgets::section(ui, p, "Ducking");
    let s = &mut app.config.settings;
    let current = DuckMode::matching(&s.ducking);
    let labels: Vec<&str> = DuckMode::ALL.iter().map(|m| m.name()).collect();
    let selected = current.and_then(|m| DuckMode::ALL.iter().position(|x| *x == m));
    if let Some(i) = widgets::segmented(ui, p, &labels, selected) {
        s.ducking = DuckMode::ALL[i].ducking();
    }
    let explain = match current {
        Some(DuckMode::Off) => "The music stays at the same level.".to_owned(),
        Some(_) => "Turns the music down while the game is loud.".to_owned(),
        None => "Custom ducking (see Advanced).".to_owned(),
    };
    ui.label(RichText::new(explain).color(p.text_dim).text_style(TextStyle::Small));
    ui.add_space(4.0);

    // Live meters: game level against the threshold, and how much the music is turned down.
    let d = s.ducking;
    let running = app.snap.session.as_ref().is_some_and(|st| st.state == SessionState::Running);
    let sidechain_active = app.snap.session.as_ref().is_some_and(|st| st.sidechain_active);
    meter_row(
        ui,
        p,
        "Game",
        widgets::meter_fraction(app.meters.game),
        Some(((d.threshold_db + 60.0) / 60.0).clamp(0.0, 1.0)),
        {
            let db = text::to_db(app.meters.game);
            if app.meters.game > 0.001 { format!("{db:.0} dB") } else { "\u{2013}".into() }
        },
        if text::to_db(app.meters.game) > d.threshold_db { p.warn } else { p.info },
    );
    let duck = app.meters.duck_db;
    let full = d.depth_db.max(1.0);
    meter_row(
        ui,
        p,
        "Music down",
        (duck / full).clamp(0.0, 1.0),
        None,
        if duck > 0.05 { format!("-{duck:.1} dB") } else { "0 dB".into() },
        p.warn,
    );

    if !app.config.sidechain {
        ui.label(
            RichText::new("Game-level metering is off (Diagnostics).").color(p.text_faint).text_style(TextStyle::Small),
        );
    } else if running && d.enabled && !sidechain_active {
        ui.add(
            egui::Label::new(
                RichText::new("Ducking can't hear the game right now (see the warnings).")
                    .color(p.warn)
                    .text_style(TextStyle::Small),
            )
            .wrap(),
        );
    } else if running && d.enabled && app.meters.game_seen.is_none_or(|t| t.elapsed().as_secs() > 15) {
        ui.add(
            egui::Label::new(
                RichText::new("No game audio yet. Ducking reacts to everything except the music.")
                    .color(p.text_faint)
                    .text_style(TextStyle::Small),
            )
            .wrap(),
        );
    }

    ui.add_space(2.0);
    egui::CollapsingHeader::new(RichText::new("Advanced").color(p.text_dim)).id_salt("ducking_advanced").show(
        ui,
        |ui| {
            let k = &mut s.ducking;
            let before = *k;
            widgets::slider_row(
                ui,
                p,
                "Threshold",
                "Game level above which ducking starts.",
                &mut k.threshold_db,
                -60.0..=-10.0,
                false,
                |v| format!("{v:.0} dB"),
            );
            widgets::slider_row(
                ui,
                p,
                "Depth",
                "Most the music is turned down.",
                &mut k.depth_db,
                0.0..=24.0,
                false,
                |v| format!("{v:.0} dB"),
            );
            widgets::slider_row(
                ui,
                p,
                "Band dip",
                "Extra cut around 3.2 kHz while ducking.",
                &mut k.band_dip_db,
                0.0..=12.0,
                false,
                |v| format!("{v:.0} dB"),
            );
            widgets::slider_row(
                ui,
                p,
                "Attack",
                "How fast ducking engages.",
                &mut k.attack_ms,
                1.0..=200.0,
                true,
                |v| format!("{v:.0} ms"),
            );
            widgets::slider_row(
                ui,
                p,
                "Hold",
                "How long it stays down after the game gets quiet.",
                &mut k.hold_ms,
                0.0..=1000.0,
                false,
                |v| format!("{v:.0} ms"),
            );
            widgets::slider_row(
                ui,
                p,
                "Release",
                "How fast the music comes back.",
                &mut k.release_ms,
                50.0..=3000.0,
                true,
                |v| format!("{v:.0} ms"),
            );
            if *k != before && !k.enabled {
                // Tweaking the advanced values means the user wants ducking.
                k.enabled = true;
            }
        },
    );
}

fn meter_row(ui: &mut Ui, p: &Palette, label: &str, fraction: f32, marker: Option<f32>, value: String, color: Color32) {
    let h = 22.0;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(widgets::LABEL_W, h), Sense::hover());
        ui.painter().text(rect.left_center(), Align2::LEFT_CENTER, label, FontId::proportional(13.0), p.text_dim);
        let bar_w = (ui.available_width() - widgets::VALUE_W - ui.spacing().item_spacing.x).max(30.0);
        let (bar_rect, _) = ui.allocate_exact_size(vec2(bar_w, h), Sense::hover());
        let bar = egui::Rect::from_center_size(bar_rect.center(), vec2(bar_w, 6.0));
        widgets::level_bar(ui, p, bar, fraction, color);
        if let Some(m) = marker {
            let x = bar.left() + bar.width() * m;
            ui.painter()
                .line_segment([pos2(x, bar.top() - 4.0), pos2(x, bar.bottom() + 4.0)], Stroke::new(1.5, p.text_dim));
        }
        let (value_rect, _) = ui.allocate_exact_size(vec2(widgets::VALUE_W, h), Sense::hover());
        ui.painter().text(value_rect.right_center(), Align2::RIGHT_CENTER, value, FontId::proportional(12.5), p.text);
    });
}

// ---------------------------------------------------------------------------------------------
// Centre: the radar

pub(super) fn center(app: &mut SidestageApp, ui: &mut Ui) {
    let p = theme::palette(ui.ctx());
    egui::CentralPanel::default().frame(Frame::new().fill(p.stage).inner_margin(Margin::same(12))).show(ui, |ui| {
        let rect = ui.available_rect_before_wrap();
        let active = app.config.enabled && app.running();
        let s = &mut app.config.settings;
        let input = RadarInput {
            palette: p,
            duck_db: app.meters.duck_db,
            duck_depth_db: s.ducking.depth_db,
            level: widgets::meter_fraction(app.meters.music),
            inactive: !active,
            preset: app.config.base_preset.settings(),
        };
        let response = radar::radar(ui, s, &input);

        // Readout (top left) and hints (bottom left) in the corners the circle leaves free.
        let painter = ui.painter();
        let readout =
            format!("{} \u{00b7} {}", radar::direction_name(s.azimuth_deg), radar::format_distance(s.distance_m));
        painter.text(rect.left_top(), Align2::LEFT_TOP, readout, FontId::proportional(16.0), p.text);
        let width =
            if s.width_deg < 0.5 { "point source".to_owned() } else { format!("{:.0}\u{00b0} wide", s.width_deg) };
        let detail = format!("{width} \u{00b7} height {}", radar::format_height(s.elevation_deg));
        painter.text(
            rect.left_top() + vec2(0.0, 22.0),
            Align2::LEFT_TOP,
            detail,
            FontId::proportional(12.0),
            p.text_dim,
        );
        let hint = if response.has_focus() {
            "Arrows: move \u{00b7} Shift+arrows: fine steps\nPage Up/Down: height \u{00b7} Home: reset"
        } else if cfg!(target_os = "macos") {
            "Drag to move \u{00b7} Cmd: snap \u{00b7} scroll: distance\nShift-drag: height \u{00b7} double-click: reset"
        } else {
            "Drag to move \u{00b7} Ctrl: snap \u{00b7} scroll: distance\nShift-drag: height \u{00b7} double-click: reset"
        };
        painter.text(rect.left_bottom(), Align2::LEFT_BOTTOM, hint, FontId::proportional(11.0), p.text_faint);
        if app.published.bypass {
            let galley = painter.layout_no_wrap("BYPASS".into(), FontId::proportional(11.5), Color32::BLACK);
            let badge = egui::Rect::from_min_size(
                rect.right_top() - vec2(galley.size().x + 16.0, 0.0),
                galley.size() + vec2(16.0, 8.0),
            );
            painter.rect_filled(badge, CornerRadius::same(6), p.warn);
            painter.galley(badge.min + vec2(8.0, 4.0), galley, Color32::BLACK);
        } else if !active {
            painter.text(rect.right_top(), Align2::RIGHT_TOP, idle_reason(app), FontId::proportional(12.0), p.text_faint);
        }
    });
}

/// Why the radar is dimmed, for its top-right corner.
fn idle_reason(app: &SidestageApp) -> String {
    let s = &app.snap;
    let failed = s.error.is_some() || s.session.as_ref().is_some_and(|st| matches!(st.state, SessionState::Error(_)));
    if !app.config.enabled {
        "Off".into()
    } else if s.platform_error.is_some() || matches!(s.support, Some(Support::Unsupported(_))) {
        "Capture unavailable".into()
    } else if failed {
        "Not capturing".into()
    } else if let Some(a) = &app.config.app {
        format!("Waiting for {}", a.name)
    } else {
        "No music source chosen".into()
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
        .default_size([460.0, 460.0])
        .max_height(ctx.content_rect().height() - 60.0)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .show(ctx, |ui| {
            ScrollArea::vertical().auto_shrink([false, true]).show(ui, |ui| diagnostics_body(app, ui, p));
        });
    app.show_diagnostics = open;
}

fn diagnostics_body(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    let s = &app.snap;
    widgets::section(ui, p, "Session");
    egui::Grid::new("diag_session").num_columns(2).spacing([16.0, 5.0]).show(ui, |ui| {
        let row = |ui: &mut Ui, k: &str, v: String| {
            ui.label(RichText::new(k).color(p.text_dim));
            ui.add(egui::Label::new(RichText::new(v).color(p.text)).wrap());
            ui.end_row();
        };
        match &s.session {
            Some(st) => {
                let state = match &st.state {
                    SessionState::Starting => "Starting".to_owned(),
                    SessionState::Running => "Running".to_owned(),
                    SessionState::WaitingForApp => "Waiting for the app".to_owned(),
                    SessionState::Error(e) => format!("Error: {e}"),
                    SessionState::Stopped => "Stopped".to_owned(),
                };
                row(ui, "State", state);
                row(ui, "App", st.app_name.clone());
                row(ui, "Output", st.output_name.clone());
                row(ui, "Sample rate", text::sample_rate(st.sample_rate));
                row(ui, "Original hidden", if st.original_hidden { "Yes".into() } else { "No".into() });
                row(ui, "Capture peak", format!("{:.1} dBFS", text::to_db(st.capture_peak)));
                let sidechain = if st.sidechain_active {
                    format!("{:.1} dBFS", text::to_db(st.sidechain_peak))
                } else if app.config.sidechain {
                    "Not measuring right now".to_owned()
                } else {
                    "Off".to_owned()
                };
                row(ui, "Sidechain peak", sidechain);
                row(ui, "Ducking", format!("-{:.1} dB", app.meters.duck_db));
                row(ui, "Added latency", format!("{:.1} ms", st.latency_ms));
                row(ui, "Dropouts", st.dropouts.to_string());
                row(ui, "Clock drift", format!("{:+.1} ppm", st.drift_ppm));
                if let Some(since) = s.session_since {
                    row(ui, "Running for", format_duration(since.elapsed().as_secs()));
                }
                row(ui, "Warnings", if st.warnings.is_empty() { "None".into() } else { st.warnings.join("\n") });
            }
            None => {
                let why = match (&s.error, app.config.enabled) {
                    (Some(e), _) => format!("Not running: {e}"),
                    (None, false) => "Off".into(),
                    (None, true) => "Not running".into(),
                };
                row(ui, "State", why);
            }
        }
    });

    ui.add_space(8.0);
    widgets::section(ui, p, "Backend");
    egui::Grid::new("diag_backend").num_columns(2).spacing([16.0, 5.0]).show(ui, |ui| {
        let row = |ui: &mut Ui, k: &str, v: String| {
            ui.label(RichText::new(k).color(p.text_dim));
            ui.add(egui::Label::new(RichText::new(v).color(p.text)).wrap());
            ui.end_row();
        };
        row(ui, "Backend", if s.platform_name.is_empty() { "\u{2013}".into() } else { s.platform_name.clone() });
        let support = match &s.support {
            None => s.platform_error.clone().unwrap_or_else(|| "\u{2013}".into()),
            Some(Support::Supported) => "Supported".into(),
            Some(Support::NeedsPermission(w)) => format!("Needs permission: {w}"),
            Some(Support::Unsupported(w)) => format!("Unsupported: {w}"),
        };
        row(ui, "Support", support);
        row(ui, "Apps with audio", s.apps.len().to_string());
        row(ui, "Output devices", s.outputs.len().to_string());
        if let Some(e) = &s.apps_error {
            row(ui, "App list error", e.clone());
        }
        if app.demo {
            row(ui, "Mode", "Demo (nothing is captured)".into());
        }
    });

    ui.add_space(8.0);
    widgets::section(ui, p, "Hide the original");
    let mut methods = s.hide_methods.clone();
    if !methods.iter().any(|(m, _)| *m == app.config.hide) {
        let label = match &app.config.hide {
            HideMethod::Auto => "Automatic".to_owned(),
            HideMethod::KeepOriginal => "Keep the original audible".to_owned(),
            HideMethod::ParkOn(id) => {
                let device = s.outputs.iter().find(|o| &o.id == id).map_or(id.as_str(), |o| o.name.as_str());
                format!("Send the original to {device} (not offered right now)")
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
    ui.checkbox(&mut app.config.sidechain, "Measure the game level for ducking")
        .on_hover_text("Listens to everything except the music app and Sidestage (the sidechain).");
    ui.label(RichText::new("Changing these restarts the capture.").color(p.text_faint).text_style(TextStyle::Small));

    ui.add_space(8.0);
    widgets::section(ui, p, "Restore");
    ui.add(
        egui::Label::new(
            RichText::new(
                "If the music app stays silent or on the wrong output after Sidestage crashed, this stops Sidestage and \
                 undoes every audio change it made.",
            )
            .color(p.text_dim),
        )
        .wrap(),
    );
    let busy = s.activity == Activity::Restoring;
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
    egui::Grid::new("diag_files").num_columns(2).spacing([16.0, 5.0]).show(ui, |ui| {
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
    fn muffle_mapping_round_trips() {
        assert!((muffle_amount(20_000.0)).abs() < 1e-6);
        assert!((muffle_amount(800.0) - 1.0).abs() < 1e-6);
        for hz in [800.0, 1400.0, 6000.0, 8000.0, 14_000.0, 20_000.0] {
            assert!((muffle_cutoff(muffle_amount(hz)) - hz).abs() / hz < 1e-4, "{hz}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(5), "5 s");
        assert_eq!(format_duration(125), "2 min 5 s");
        assert_eq!(format_duration(7300), "2 h 1 min");
    }
}
