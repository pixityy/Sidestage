//! The left panel: the session's sources (or the app picker while one is being added) and the
//! message cards below them.

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, ScrollArea, Sense, Stroke, StrokeKind,
    TextStyle, Ui, Vec2, pos2, vec2,
};
use sidestage_dsp::Preset;
use sidestage_platform::{AudioApp, HideMethod, SessionState, SourceId, Support};

use super::theme::{self, Palette};
use super::widgets::{self, CardKind};
use super::{SidestageApp, SourceView};
use crate::config::{ChosenApp, MAX_SOURCES, Source};
use crate::engine::NoticeKind;

const ROW_H: f32 = 50.0;

pub(super) fn left_panel(app: &mut SidestageApp, ui: &mut Ui, width: f32) {
    let p = theme::palette(ui.ctx());
    egui::Panel::left("sources")
        .exact_size(width)
        .resizable(false)
        .frame(Frame::new().fill(p.bg).inner_margin(Margin { left: 12, right: 10, top: 12, bottom: 10 }))
        .show(ui, |ui| {
            let picking = app.adding || app.config.sources.is_empty();
            if picking {
                picker_header(app, ui, p);
            } else {
                list_header(app, ui, p);
            }

            // Cards go under the list. Their height is only known after drawing them, so the
            // list is sized with last frame's card height (one frame of lag when cards change).
            let specs = cards(app, picking);
            let cards_id = ui.id().with("cards_height");
            let cards_h = if specs.is_empty() { 0.0 } else { ui.data(|d| d.get_temp::<f32>(cards_id)).unwrap_or(0.0) };
            let max_cards_h = (ui.available_height() * 0.5).max(80.0);
            let gap = if specs.is_empty() { 0.0 } else { ui.spacing().item_spacing.y };
            let list_h = (ui.available_height() - gap - cards_h.min(max_cards_h)).max(90.0);
            // When the list gets shorter (a card appeared, the window shrank), bring the selected
            // source back into view.
            let list_h_id = ui.id().with("list_height");
            if ui.data(|d| d.get_temp::<f32>(list_h_id)).is_none_or(|h| (h - list_h).abs() > 0.5) {
                ui.data_mut(|d| d.insert_temp(list_h_id, list_h));
                app.reveal_selected = true;
            }
            ScrollArea::vertical()
                .id_salt(if picking { "picker_scroll" } else { "sources_scroll" })
                .max_height(list_h)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if picking {
                        picker(app, ui, p);
                    } else {
                        source_list(app, ui, p);
                    }
                });
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

// ---------------------------------------------------------------------------------------------
// The sources

fn list_header(app: &SidestageApp, ui: &mut Ui, p: &Palette) {
    ui.horizontal(|ui| {
        widgets::section(ui, p, "Sources");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(format!("{} of {MAX_SOURCES}", app.config.sources.len()))
                    .color(p.text_faint)
                    .text_style(TextStyle::Small),
            );
        });
    });
}

/// What a click in a source row asked for.
enum RowAction {
    Select,
    Enable(bool),
    Remove,
}

fn source_list(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    let mut actions: Vec<(SourceId, RowAction)> = Vec::new();
    let reveal = app.reveal_selected;
    app.reveal_selected = false;
    for source in &app.config.sources {
        let selected = app.config.selected == Some(source.id);
        let view = app.view(source);
        let level = app.meters.source(source.id).level;
        let (response, action) = source_row(ui, p, source, &view, level, selected);
        if selected && reveal {
            // Only scrolls if the row is not fully visible.
            response.scroll_to_me(None);
        }
        if let Some(a) = action {
            actions.push((source.id, a));
        }
    }
    ui.add_space(4.0);
    add_button(app, ui, p);
    for (id, action) in actions {
        match action {
            RowAction::Select => app.config.selected = Some(id),
            RowAction::Enable(on) => {
                if let Some(s) = app.config.source_mut(id) {
                    s.enabled = on;
                    log::info!("source {}: {}", s.app.name, if on { "on" } else { "off" });
                }
                if on {
                    app.config.enabled = true;
                }
            }
            RowAction::Remove => app.remove_source(id),
        }
    }
}

/// The source's preset as the row shows it: "Behind you", "Behind you (edited)".
fn preset_label(source: &Source) -> String {
    match Preset::matching(&source.settings) {
        Some(p) => p.name().to_owned(),
        None => format!("{} (edited)", source.base_preset.name()),
    }
}

fn source_row(
    ui: &mut Ui,
    p: &Palette,
    source: &Source,
    view: &SourceView,
    level: f32,
    selected: bool,
) -> (egui::Response, Option<RowAction>) {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
    let color = p.puck(source.color);
    let toggle_rect = egui::Rect::from_min_size(pos2(rect.right() - 36.0, rect.top() + 9.0), vec2(26.0, 15.0));
    let x_rect =
        egui::Rect::from_center_size(pos2(toggle_rect.left() - 15.0, toggle_rect.center().y), Vec2::splat(20.0));
    let mut action = None;

    let off = matches!(view, SourceView::Off);
    let detail = match view {
        SourceView::Failed(_) => "Couldn't capture".to_owned(),
        SourceView::Off if !source.enabled => "Off".to_owned(),
        SourceView::Off => preset_label(source),
        SourceView::Waiting => format!("{} \u{00b7} waiting", preset_label(source)),
        SourceView::Starting => "Starting\u{2026}".to_owned(),
        SourceView::Playing { .. } => preset_label(source),
        SourceView::NotCapturing => format!("{} \u{00b7} not capturing", preset_label(source)),
    };
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let r = CornerRadius::same(8);
        if selected {
            painter.rect_filled(rect, r, color.gamma_multiply(if p.dark { 0.14 } else { 0.10 }));
            painter.rect_stroke(rect, r, Stroke::new(1.0, color.gamma_multiply(0.7)), StrokeKind::Inside);
        } else if response.hovered() {
            painter.rect_filled(rect, r, p.surface_hi.gamma_multiply(if p.dark { 0.8 } else { 1.0 }));
        }
        if response.has_focus() {
            painter.rect_stroke(rect, r, Stroke::new(1.5, p.accent), StrokeKind::Inside);
        }
        let dot = pos2(rect.left() + 14.0, rect.top() + 17.0);
        if source.enabled {
            painter.circle_filled(dot, 5.0, color);
        } else {
            painter.circle_stroke(dot, 4.5, Stroke::new(1.5, color.gamma_multiply(0.8)));
        }
        let text_left = rect.left() + 28.0;
        let text_w = x_rect.left() - 2.0 - text_left;
        let alpha = if source.enabled { 1.0 } else { 0.65 };
        let name_g =
            widgets::truncated(ui, &source.app.name, FontId::proportional(13.5), p.text.gamma_multiply(alpha), text_w);
        painter.galley(pos2(text_left, rect.top() + 8.0), name_g, p.text);
        let detail_color = match view {
            SourceView::Failed(_) => p.err,
            SourceView::Off | SourceView::Waiting | SourceView::NotCapturing => p.text_faint,
            _ => p.text_dim,
        };
        let detail_w = rect.right() - 10.0 - text_left;
        let detail_g = widgets::truncated(ui, &detail, FontId::proportional(11.5), detail_color, detail_w);
        painter.galley(pos2(text_left, rect.top() + 25.0), detail_g, detail_color);
        if !off {
            let bar = egui::Rect::from_min_max(
                pos2(text_left, rect.bottom() - 8.0),
                pos2(rect.right() - 10.0, rect.bottom() - 5.0),
            );
            let live = matches!(view, SourceView::Playing { .. });
            widgets::level_bar(ui, p, bar, if live { widgets::meter_fraction(level) } else { 0.0 }, color);
        }
    }
    let name = source.app.name.as_str();
    response.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, name));
    let mut on = source.enabled;
    let toggle = widgets::toggle_at(ui, p, toggle_rect, response.id.with("on"), &mut on, &format!("{name} on"));
    if toggle.on_hover_text(if on { "Turn off: the app plays normally" } else { "Turn on" }).changed() {
        action = Some(RowAction::Enable(on));
    }
    let show_x = selected || response.hovered() || ui.rect_contains_pointer(rect);
    let x = widgets::close_x(ui, p, x_rect, response.id.with("remove"), show_x, &format!("Remove {name}"));
    if x.on_hover_text(format!("Remove {name} (it plays normally again)")).clicked() {
        action = Some(RowAction::Remove);
    }
    if action.is_none() && response.clicked() {
        action = Some(RowAction::Select);
    }
    let state = match view {
        SourceView::Off => "Off: plays normally".to_owned(),
        SourceView::Waiting => format!("Waiting for {name} to play audio"),
        SourceView::Starting => "Starting".to_owned(),
        SourceView::Playing { hidden: true } => "Playing through Sidestage, original hidden".to_owned(),
        SourceView::Playing { hidden: false } => "Playing through Sidestage, original audible".to_owned(),
        SourceView::Failed(e) => format!("Couldn't capture: {e}"),
        SourceView::NotCapturing => format!("Sidestage isn't capturing; {name} plays normally"),
    };
    let response = response.on_hover_text(format!("{name} \u{00b7} {}\n{state}", preset_label(source)));
    (response, action)
}

fn add_button(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    let full = app.config.is_full();
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), 34.0), if full { Sense::hover() } else { Sense::click() });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let hot = !full && (response.hovered() || response.has_focus());
        let stroke_color = if hot { p.accent } else { p.border.lerp_to_gamma(p.text_faint, 0.35) };
        painter.rect(
            rect,
            CornerRadius::same(8),
            if hot { p.accent.gamma_multiply(0.08) } else { Color32::TRANSPARENT },
            Stroke::new(1.0, stroke_color),
            StrokeKind::Inside,
        );
        let color = if full {
            p.text_faint
        } else if hot {
            p.text
        } else {
            p.text_dim
        };
        let label = if full { "8 sources is the most".to_owned() } else { "+  Add source".to_owned() };
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, label, FontId::proportional(13.0), color);
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, !full, "Add source"));
    let response = if full {
        response.on_hover_text(format!("Sidestage plays up to {MAX_SOURCES} sources. Remove one to add another."))
    } else {
        response.on_hover_text("Add another app: a podcast, voice chat, a second player \u{2026}")
    };
    if response.clicked() {
        app.adding = true;
        app.search.clear();
    }
}

// ---------------------------------------------------------------------------------------------
// The app picker

fn picker_header(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    let first = app.config.sources.is_empty();
    ui.horizontal(|ui| {
        widgets::section(ui, p, if first { "Add your first source" } else { "Add a source" });
        if !first {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let cancel =
                    ui.add(egui::Button::new(RichText::new("Cancel").color(p.text_dim).size(12.5)).frame(false));
                if cancel.clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    app.adding = false;
                }
            });
        }
    });
    if app.snap.platform_error.is_none() {
        let hint = if first {
            "Click an app to put it around you: your music, a podcast, voice chat \u{2026}"
        } else {
            "Click the app to add. It gets a preset that suits it."
        };
        ui.add(egui::Label::new(RichText::new(hint).color(p.accent_hi).text_style(TextStyle::Small)).wrap());
        ui.add_space(2.0);
    }
    ui.add(
        egui::TextEdit::singleline(&mut app.search)
            .hint_text("Search apps")
            .desired_width(f32::INFINITY)
            .margin(Margin::symmetric(8, 5)),
    );
    ui.add_space(6.0);
}

fn picker(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
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
    let mut pick = None;
    let full = app.config.is_full();
    for a in &apps {
        let used = app.config.source_for(&a.key).is_some();
        let level = app.meters.apps.get(&a.key).copied().unwrap_or(0.0);
        let tag = if used { Some("Added") } else { None };
        let response = app_row(ui, p, &a.name, &a.detail, a.playing, level, tag, !used && !full);
        let response = response.on_hover_text(if used {
            format!("{} is already a source.\n{}", a.name, a.key.0)
        } else {
            format!("{}\n{}", a.name, a.key.0)
        });
        if response.clicked() && !used && !full {
            pick = Some(ChosenApp { key: a.key.clone(), name: a.name.clone() });
        }
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
        app.add_source(choice);
    }
}

/// An app in the picker: playing dot, name, detail, level, and an "Added" tag for apps that
/// already are a source.
#[expect(clippy::too_many_arguments)]
fn app_row(
    ui: &mut Ui,
    p: &Palette,
    name: &str,
    detail: &str,
    playing: bool,
    level: f32,
    tag: Option<&str>,
    clickable: bool,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(vec2(width, ROW_H), if clickable { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let r = CornerRadius::same(8);
        if clickable && response.hovered() {
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
        let alpha = if clickable { 1.0 } else { 0.6 };
        let text_left = rect.left() + 28.0;
        let tag_g = tag.map(|t| painter.layout_no_wrap(t.to_owned(), FontId::proportional(10.5), p.text_dim));
        let tag_w = tag_g.as_ref().map_or(0.0, |g| g.size().x + 14.0);
        let text_w = rect.right() - 10.0 - tag_w - text_left;
        let name_g = widgets::truncated(ui, name, FontId::proportional(13.5), p.text.gamma_multiply(alpha), text_w);
        painter.galley(pos2(text_left, rect.top() + 8.0), name_g, p.text);
        if let Some(g) = tag_g {
            let pill = egui::Rect::from_min_size(
                pos2(rect.right() - 8.0 - g.size().x - 10.0, rect.top() + 8.0),
                vec2(g.size().x + 10.0, 17.0),
            );
            painter.rect_filled(pill, CornerRadius::same(8), p.surface_hi);
            painter.galley(pill.center() - g.size() / 2.0, g, p.text_dim);
        }
        if !detail.is_empty() {
            let detail_g = widgets::truncated(
                ui,
                detail,
                FontId::proportional(11.5),
                p.text_dim.gamma_multiply(alpha),
                rect.right() - 10.0 - text_left,
            );
            painter.galley(pos2(text_left, rect.top() + 25.0), detail_g, p.text_dim);
        }
        let bar = egui::Rect::from_min_max(
            pos2(text_left, rect.bottom() - 8.0),
            pos2(rect.right() - 10.0, rect.bottom() - 5.0),
        );
        widgets::level_bar(ui, p, bar, widgets::meter_fraction(level), p.ok.gamma_multiply(0.8 * alpha));
    }
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, clickable, name));
    response
}

// ---------------------------------------------------------------------------------------------
// Cards

enum CardAction {
    Retry,
    RetrySource(SourceId),
    TurnOff(SourceId),
    Diagnostics,
    DismissNotice(u64),
    DismissHint(&'static str),
    EnableSidechain,
    HideOriginal,
    Undo,
    ForgetRemoved,
}

struct CardSpec {
    kind: CardKind,
    title: String,
    lines: Vec<String>,
    buttons: Vec<(&'static str, CardAction)>,
}

/// The cards to show. While an app is being picked, one-time hints wait so the app list has
/// room.
fn cards(app: &SidestageApp, picking: bool) -> Vec<CardSpec> {
    let s = &app.snap;
    let names = app.enabled_names();
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
            title: "Couldn't start Sidestage".into(),
            lines,
            buttons: vec![("Try again", CardAction::Retry), ("Diagnostics", CardAction::Diagnostics)],
        });
    }
    // One card per failed source; the others keep playing.
    for source in app.config.sources.iter().filter(|x| x.enabled) {
        if let SourceView::Failed(e) = app.view(source) {
            let mut lines = vec![e];
            if s.source_retries.contains(&source.id) {
                lines.push("Sidestage will try again in a moment. Your other sources keep playing.".into());
            }
            out.push(CardSpec {
                kind: CardKind::Error,
                title: format!("Couldn't capture {}", source.app.name),
                lines,
                buttons: vec![
                    ("Try again", CardAction::RetrySource(source.id)),
                    ("Turn off", CardAction::TurnOff(source.id)),
                ],
            });
        }
    }
    if let Some(st) = &s.session {
        if !st.warnings.is_empty() {
            out.push(CardSpec {
                kind: CardKind::Warning,
                title: "Check your audio setup".into(),
                lines: st.warnings.clone(),
                buttons: vec![],
            });
        }
        for source in st.sources.iter().filter(|x| !x.warnings.is_empty()) {
            // The name the user picked; the backend's can be a process name.
            let name = app.config.source(source.id).map_or(source.app_name.as_str(), |s| s.app.name.as_str());
            out.push(CardSpec {
                kind: CardKind::Warning,
                title: format!("Check {name}"),
                lines: source.warnings.clone(),
                buttons: vec![],
            });
        }
    }
    // Only where the backend offers it: Linux always hides the originals (and says so).
    if app.config.enabled
        && app.config.hide == HideMethod::KeepOriginal
        && app.config.sources.iter().any(|x| x.enabled)
        && s.hide_methods.iter().any(|(m, _)| *m == HideMethod::KeepOriginal)
    {
        out.push(CardSpec {
            kind: CardKind::Warning,
            title: "You'll hear your apps twice".into(),
            lines: vec![format!("The originals of {names} are not hidden, so they play alongside the 3D versions.")],
            buttons: vec![("Hide the originals", CardAction::HideOriginal)],
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
                "While Sidestage runs, your sources play only through Sidestage. Quit Sidestage and they play \
                 normally again."
                    .into(),
            ],
            buttons: vec![],
        });
    }
    if !app.config.sidechain && app.config.sources.iter().any(|x| x.enabled && x.settings.ducking.enabled) {
        out.push(CardSpec {
            kind: CardKind::Info,
            title: "Ducking can't hear the game".into(),
            lines: vec!["Game-level metering is off, so your sources won't get quieter when the game is loud.".into()],
            buttons: vec![("Turn it on", CardAction::EnableSidechain)],
        });
    }
    if !picking
        && let Some((id, title, body)) = platform_hint(&names)
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
    // Also after removing the last source, when the picker opens by itself; only a picker the
    // user opened hides it. An app that is a source again has nothing to undo.
    if let Some((removed, _)) = &app.removed
        && !app.adding
        && app.config.source_for(&removed.app.key).is_none()
    {
        let name = &removed.app.name;
        let (line, buttons) = if app.config.is_full() {
            (
                format!("{name} plays normally again. Remove another source to put it back."),
                vec![("Dismiss", CardAction::ForgetRemoved)],
            )
        } else {
            (
                format!("{name} plays normally again."),
                vec![("Undo", CardAction::Undo), ("Dismiss", CardAction::ForgetRemoved)],
            )
        };
        out.push(CardSpec { kind: CardKind::Info, title: format!("Removed {name}"), lines: vec![line], buttons });
    }
    out
}

/// A one-time explanation of how the originals are hidden on this OS.
fn platform_hint(names: &str) -> Option<(&'static str, &'static str, String)> {
    if cfg!(windows) {
        Some((
            "windows-mixer",
            "About the Volume mixer",
            format!(
                "{names} show at 0 in the Volume mixer while Sidestage runs. That's expected: it's how the originals \
                 are hidden."
            ),
        ))
    } else if cfg!(target_os = "linux") {
        Some((
            "linux-sink",
            "Where your apps went",
            format!(
                "While Sidestage runs, {names} play into a silent Sidestage output. They move back when Sidestage \
                 stops."
            ),
        ))
    } else {
        None
    }
}

fn apply_card_action(app: &mut SidestageApp, action: CardAction) {
    match action {
        CardAction::Retry => app.engine.retry(),
        CardAction::RetrySource(id) => app.engine.retry_source(id),
        CardAction::TurnOff(id) => {
            if let Some(s) = app.config.source_mut(id) {
                s.enabled = false;
            }
        }
        CardAction::Diagnostics => app.show_diagnostics = true,
        CardAction::DismissNotice(id) => app.engine.dismiss_notice(id),
        CardAction::DismissHint(id) => app.config.dismissed_hints.push(id.to_owned()),
        CardAction::EnableSidechain => app.config.sidechain = true,
        CardAction::HideOriginal => app.config.hide = HideMethod::Auto,
        CardAction::Undo => app.undo_remove(),
        CardAction::ForgetRemoved => app.removed = None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use eframe::egui;

    use super::*;
    use crate::ui::{Launch, demo_config};

    fn app() -> SidestageApp {
        let launch = Launch {
            demo: true,
            screenshot: None,
            scenario: crate::demo::Scenario::default(),
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
        };
        let config = demo_config(&launch);
        SidestageApp::new(&egui::Context::default(), launch, config, None)
    }

    fn titles(app: &SidestageApp, picking: bool) -> Vec<String> {
        cards(app, picking).into_iter().map(|c| c.title).collect()
    }

    #[test]
    fn editing_and_switching_sources_only_tells_the_engine_what_changed() {
        let mut app = app();
        let ids: Vec<SourceId> = app.config.sources.iter().map(|s| s.id).collect();
        let before = app.sent_desired.clone();

        // Sound changes go to the renderer, never to the engine (no set_sources storm).
        for i in 0..20 {
            app.config.sources[0].settings.distance_m = 1.0 + i as f32 * 0.1;
            app.config.sources[2].settings.volume_db = -(i as f32);
            app.sync();
        }
        assert!(app.sent_desired == before, "a slider drag changed the session's sources");
        assert!((app.params[&ids[0]].get().distance_m - 2.9).abs() < 1e-4, "the renderer got the last value");

        // Switching one off drops only that one; the others keep their renderer settings
        // storage, which is what keeps them running untouched.
        app.config.source_mut(ids[1]).unwrap().enabled = false;
        app.sync();
        let now: Vec<SourceId> = app.sent_desired.sources.iter().map(|s| s.id).collect();
        assert_eq!(now, [ids[0], ids[2]]);
        for s in &app.sent_desired.sources {
            let old = before.sources.iter().find(|o| o.id == s.id).unwrap();
            assert!(Arc::ptr_eq(&old.params, &s.params));
        }
        // Back on: the same storage again.
        app.config.source_mut(ids[1]).unwrap().enabled = true;
        app.sync();
        assert!(app.sent_desired == before);
    }

    #[test]
    fn removing_even_the_last_source_can_be_undone() {
        let mut app = app();
        let ids: Vec<SourceId> = app.config.sources.iter().map(|s| s.id).collect();
        for id in &ids {
            app.remove_source(*id);
        }
        app.sync();
        assert!(app.config.sources.is_empty() && app.sent_desired.sources.is_empty());
        // The picker opens by itself now; the Undo card still shows.
        assert!(titles(&app, true).contains(&"Removed Discord".to_owned()), "{:?}", titles(&app, true));
        // ... but not while the user is picking an app on purpose.
        app.adding = true;
        assert!(!titles(&app, true).iter().any(|t| t.starts_with("Removed")));
        app.adding = false;

        apply_card_action(&mut app, CardAction::Undo);
        app.sync();
        assert_eq!(app.config.sources.iter().map(|s| s.id).collect::<Vec<_>>(), [ids[2]]);
        assert_eq!(app.config.selected, Some(ids[2]));
        assert_eq!(app.sent_desired.sources.iter().map(|s| s.id).collect::<Vec<_>>(), [ids[2]]);
        assert!(app.removed.is_none() && !titles(&app, false).iter().any(|t| t.starts_with("Removed")));

        // Removed, then added again from the picker: nothing left to undo.
        app.remove_source(ids[2]);
        let discord = ChosenApp { key: sidestage_platform::AppKey("demo:discord".into()), name: "Discord".into() };
        app.add_source(discord);
        assert!(app.removed.is_none());
        assert!(app.config.sources.iter().all(|s| s.id != ids[2]), "a new source gets a new ID");
    }

    #[test]
    fn turning_off_a_failed_source_from_its_card_keeps_the_others() {
        let mut app = app();
        let ids: Vec<SourceId> = app.config.sources.iter().map(|s| s.id).collect();
        apply_card_action(&mut app, CardAction::TurnOff(ids[2]));
        app.sync();
        assert!(!app.config.source(ids[2]).unwrap().enabled);
        assert_eq!(app.sent_desired.sources.iter().map(|s| s.id).collect::<Vec<_>>(), [ids[0], ids[1]]);
        assert!(app.sent_desired.enabled);
    }
}
