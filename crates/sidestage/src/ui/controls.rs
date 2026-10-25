//! The right panel: the selected source's presets, sound and ducking, and below them the apps
//! that never turn any source down.

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Margin, RichText, ScrollArea, Sense, Stroke, StrokeKind,
    TextStyle, Ui, pos2, vec2,
};
use sidestage_dsp::{DuckMode, Preset, PresetKind, Room};
use sidestage_platform::{AudioApp, SessionState, SourceId};

use super::radar;
use super::theme::{self, Palette};
use super::widgets;
use super::{SidestageApp, SourceView};
use crate::config::ChosenApp;
use crate::text;

pub(super) fn right_panel(app: &mut SidestageApp, ui: &mut Ui, width: f32) {
    let p = theme::palette(ui.ctx());
    egui::Panel::right("controls")
        .exact_size(width)
        .resizable(false)
        .frame(Frame::new().fill(p.bg).inner_margin(Margin { left: 14, right: 6, top: 12, bottom: 10 }))
        .show(ui, |ui| {
            ScrollArea::vertical().id_salt("controls_scroll").auto_shrink([false, false]).show(ui, |ui| {
                ui.set_width(ui.available_width() - 10.0);
                match app.config.selected_source().map(|s| s.id) {
                    Some(id) => {
                        header(app, ui, p, id);
                        presets(app, ui, p, id);
                        ui.add_space(10.0);
                        sound(app, ui, p, id);
                        ui.add_space(10.0);
                        ducking(app, ui, p, id);
                    }
                    None => {
                        widgets::section(ui, p, "Sound");
                        ui.add(
                            egui::Label::new(
                                RichText::new("Add a source on the left; its presets and sound settings show up here.")
                                    .color(p.text_dim),
                            )
                            .wrap(),
                        );
                    }
                }
                ui.add_space(14.0);
                ignore_list(app, ui, p);
                ui.add_space(8.0);
                if app.scroll_controls {
                    ui.scroll_to_cursor(Some(Align::Max));
                }
            });
        });
}

/// The selected source's name and what it is doing.
fn header(app: &SidestageApp, ui: &mut Ui, p: &Palette, id: SourceId) {
    let Some(source) = app.config.source(id) else { return };
    let name = source.app.name.as_str();
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(14.0, 20.0), Sense::hover());
        ui.painter().circle_filled(r.center(), 6.0, p.puck(source.color));
        let galley = widgets::truncated(ui, name, FontId::proportional(16.5), p.text, ui.available_width());
        let (rect, _) = ui.allocate_exact_size(galley.size(), Sense::hover());
        ui.painter().galley(rect.min, galley, p.text);
    });
    let (line, color) = match app.view(source) {
        SourceView::Playing { hidden: true } => ("Playing in 3D \u{00b7} original hidden".to_owned(), p.text_dim),
        SourceView::Playing { hidden: false } => {
            ("Playing in 3D \u{00b7} the original is audible too".to_owned(), p.warn)
        }
        SourceView::Starting => ("Starting\u{2026}".to_owned(), p.text_dim),
        SourceView::Waiting => (format!("Waiting for {name} to play audio"), p.text_faint),
        SourceView::Off if !app.config.enabled => ("Sidestage is off".to_owned(), p.text_faint),
        SourceView::Off => (format!("Off: {name} plays normally"), p.text_faint),
        SourceView::Failed(_) => (format!("Couldn't capture {name} (see the card on the left)"), p.err),
        SourceView::NotCapturing => ("Not capturing (see the card on the left)".to_owned(), p.text_faint),
    };
    ui.add(egui::Label::new(RichText::new(line).color(color).text_style(TextStyle::Small)).wrap());
    ui.add_space(6.0);
}

fn presets(app: &mut SidestageApp, ui: &mut Ui, p: &Palette, id: SourceId) {
    let Some(source) = app.config.source(id) else { return };
    widgets::section(ui, p, "Presets");
    let exact = Preset::matching(&source.settings);
    let base = source.base_preset;
    // Which group is shown is remembered per source for this run; it starts at the group of the
    // source's preset.
    let tab_id = ui.id().with(("preset_group", id));
    let mut group = ui.data(|d| d.get_temp::<PresetKind>(tab_id)).unwrap_or(base.kind());
    let selected = Some(if group == PresetKind::Music { 0 } else { 1 });
    if let Some(i) = widgets::segmented(ui, p, "preset_group", &["Music", "Voice"], selected) {
        group = if i == 0 { PresetKind::Music } else { PresetKind::Voice };
        ui.data_mut(|d| d.insert_temp(tab_id, group));
    }
    ui.add_space(6.0);
    let list: &[Preset] = if group == PresetKind::Music { &Preset::MUSIC } else { &Preset::VOICE };

    let gap = 8.0;
    let card_w = ((ui.available_width() - gap) / 2.0).floor();
    let pad = 9.0;
    let text_w = card_w - 2.0 * pad;
    let name_font = FontId::proportional(13.5);
    let desc_font = FontId::proportional(11.5);
    let is_edited = |preset: Preset| exact.is_none() && base == preset;
    // An edited card keeps room for its pencil mark at the end of the name.
    let name_w = |preset: Preset| if is_edited(preset) { text_w - 14.0 } else { text_w };
    let measure = ui.painter().clone();
    let layout = |text: &str, font: &FontId, color: Color32, width: f32| {
        measure.layout(text.to_owned(), font.clone(), color, width)
    };
    let mut clicked = None;
    for row in list.chunks(2) {
        // Names and descriptions wrap; both cards of a row get the taller one's height.
        let content_h = row
            .iter()
            .map(|pr| {
                layout(pr.name(), &name_font, p.text, name_w(*pr)).size().y
                    + 3.0
                    + layout(pr.description(), &desc_font, p.text_dim, text_w).size().y
            })
            .fold(0.0, f32::max);
        let card_h = content_h + 2.0 * pad;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for preset in row {
                let (rect, response) = ui.allocate_exact_size(vec2(card_w, card_h), Sense::click());
                let active = exact == Some(*preset);
                let edited = is_edited(*preset);
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
                let name = painter.layout(preset.name().to_owned(), name_font.clone(), name_color, name_w(*preset));
                let name_rect = egui::Rect::from_min_size(rect.min + vec2(pad, pad), name.size());
                let one_line = name.rows.len() <= 1;
                painter.galley(name_rect.min, name, name_color);
                if edited {
                    // "edited" beside a one-line name, or a pencil mark where there is no room.
                    let tag = painter.layout_no_wrap("edited".into(), FontId::proportional(10.5), p.accent_hi);
                    let right = pos2(rect.right() - pad, rect.top() + pad + 2.0);
                    if one_line && name_rect.right() + 6.0 + tag.size().x <= right.x {
                        painter.galley(right - vec2(tag.size().x, 0.0), tag, p.accent_hi);
                    } else {
                        pencil(painter, pos2(right.x - 5.0, rect.top() + pad + 9.0), p.accent_hi);
                    }
                }
                let desc = painter.layout(preset.description().to_owned(), desc_font.clone(), p.text_dim, text_w);
                painter.galley(pos2(rect.left() + pad, name_rect.bottom() + 3.0), desc, p.text_dim);
                response.widget_info(|| {
                    let name = if edited { format!("{} (edited)", preset.name()) } else { preset.name().to_owned() };
                    egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, active, name)
                });
                let response = if edited {
                    response.on_hover_text(
                        "This source's settings started from this preset and were changed. Click to go back to it.",
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
    let Some(source) = app.config.source_mut(id) else { return };
    if let Some(preset) = clicked {
        source.apply_preset(preset);
    }

    ui.add_space(6.0);
    let mut footstep = source.footstep_safe();
    let (subtitle, tooltip) = if source.settings.voice_chat {
        (
            "A lighter cut around 3 kHz, so speech stays clear",
            "Cuts this source a little around 3 kHz where footsteps live and rolls off the highs. Voice chat \
             is never turned down for the game.",
        )
    } else {
        (
            "Keeps 2\u{2013}5 kHz clear and ducks harder",
            "Cuts this source around 3 kHz where footsteps live, rolls off highs and bass, and uses Competitive \
             ducking.",
        )
    };
    if widgets::toggle_row(ui, p, &mut footstep, "Footstep-safe", subtitle, tooltip) {
        source.set_footstep_safe(footstep);
    }
    let mut voice = source.settings.voice_chat;
    if widgets::toggle_row(
        ui,
        p,
        &mut voice,
        "Voice chat",
        "Your other sources turn down while it talks",
        "Marks this source as voice chat (Discord, TeamSpeak \u{2026}). Sources with \u{201c}Turn down while voice \
         chat talks\u{201d} get quieter whenever it is talking.",
    ) {
        source.settings.voice_chat = voice;
    }
}

/// A small pencil ("edited") centred on `c`.
fn pencil(painter: &egui::Painter, c: egui::Pos2, color: Color32) {
    let tip = c + vec2(-4.0, 4.0);
    let end = c + vec2(4.0, -4.0);
    painter.line_segment([tip + vec2(1.5, -1.5), end], Stroke::new(2.4, color));
    painter.line_segment([tip, tip + vec2(1.6, -1.6)], Stroke::new(1.2, color));
}

fn sound(app: &mut SidestageApp, ui: &mut Ui, p: &Palette, id: SourceId) {
    let Some(source) = app.config.source_mut(id) else { return };
    widgets::section(ui, p, "Sound");
    let s = &mut source.settings;
    widgets::slider_row(
        ui,
        p,
        "Distance",
        "How far away this source is.",
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
        "Rolls off the highs, like sound through a wall.",
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
    widgets::slider_row(
        ui,
        p,
        "Clarity",
        "Speech clarity: a broad lift (or cut) around 2.5 kHz. Voice presets raise it.",
        &mut s.clarity_db,
        -6.0..=6.0,
        false,
        |v| if v.abs() < 0.05 { "Off".into() } else { format!("{v:+.1} dB") },
    );
    widgets::slider_row(
        ui,
        p,
        "Leveler",
        "Evens out loud and quiet talkers, before the 3D rendering. Voice presets turn it on.",
        &mut s.leveler,
        0.0..=1.0,
        false,
        |v| if v < 0.005 { "Off".into() } else { format!("{:.0}%", v * 100.0) },
    );
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
        "Level of the room's reverb relative to this source.",
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
        "This source's volume in Sidestage (use this, not the app's own slider).",
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

fn ducking(app: &mut SidestageApp, ui: &mut Ui, p: &Palette, id: SourceId) {
    // What the meters and hints need, read before the source is borrowed for editing.
    let voices: Vec<&str> = app
        .config
        .sources
        .iter()
        .filter(|s| s.enabled && s.settings.voice_chat && s.id != id)
        .map(|s| s.app.name.as_str())
        .collect();
    let voices = (!voices.is_empty()).then(|| text::join_names(&voices));
    let voice_level = app.meters.voice(&app.config, id);
    let game_level = app.meters.game;
    let game_seen = app.meters.game_seen;
    let duck = app.meters.source(id).duck_db;
    let running = app.snap.session.as_ref().is_some_and(|st| st.state == SessionState::Running);
    let sidechain_active = app.snap.session.as_ref().is_some_and(|st| st.sidechain_active);
    let sidechain = app.config.sidechain;
    let Some(source) = app.config.source_mut(id) else { return };
    let is_voice = source.settings.voice_chat;
    let s = &mut source.settings;

    widgets::section(ui, p, "Ducking");
    let current = DuckMode::matching(&s.ducking);
    let labels: Vec<&str> = DuckMode::ALL.iter().map(|m| m.name()).collect();
    let selected = current.and_then(|m| DuckMode::ALL.iter().position(|x| *x == m));
    if let Some(i) = widgets::segmented(ui, p, "duck_mode", &labels, selected) {
        s.ducking = DuckMode::ALL[i].apply(s.ducking);
    }
    let explain = match current {
        Some(DuckMode::Off) => "Stays at the same level when the game is loud.",
        Some(_) => "Turns down while the game is loud.",
        None => "Custom ducking (see Advanced).",
    };
    ui.label(RichText::new(explain).color(p.text_dim).text_style(TextStyle::Small));
    ui.add_space(4.0);
    let subtitle = match &voices {
        Some(names) => format!("When {names} talks"),
        None if is_voice => "When another voice chat source talks".to_owned(),
        None => "Turn on \u{201c}Voice chat\u{201d} for your chat app first".to_owned(),
    };
    widgets::toggle_row(
        ui,
        p,
        &mut s.ducking.under_voice,
        "Turn down while voice chat talks",
        &subtitle,
        "Uses the same strength as ducking for the game, and works even when that is off.",
    );
    ui.add_space(4.0);

    // Live meters: what triggers ducking against the threshold, and how far this source is down.
    let d = s.ducking;
    let marker = Some(((d.threshold_db + 60.0) / 60.0).clamp(0.0, 1.0));
    let db_text = |level: f32| if level > 0.001 { format!("{:.0} dB", text::to_db(level)) } else { "\u{2013}".into() };
    let over = |level: f32| text::to_db(level) > d.threshold_db;
    meter_row(
        ui,
        p,
        "Game",
        widgets::meter_fraction(game_level),
        marker,
        db_text(game_level),
        if d.enabled && over(game_level) { p.warn } else { p.info },
    );
    if voices.is_some() {
        meter_row(
            ui,
            p,
            "Voice",
            widgets::meter_fraction(voice_level),
            marker,
            db_text(voice_level),
            if d.under_voice && over(voice_level) { p.warn } else { p.info },
        );
    }
    let full = d.depth_db.max(1.0);
    meter_row(
        ui,
        p,
        "Turned down",
        (duck / full).clamp(0.0, 1.0),
        None,
        if duck > 0.05 { format!("-{duck:.1} dB") } else { "0 dB".into() },
        p.warn,
    );

    let note = |ui: &mut Ui, text: &str, color: Color32| {
        ui.add(egui::Label::new(RichText::new(text).color(color).text_style(TextStyle::Small)).wrap());
    };
    if !sidechain {
        note(ui, "Game-level metering is off (Diagnostics).", p.text_faint);
    } else if running && d.enabled && !sidechain_active {
        note(ui, "Ducking can't hear the game right now (see the warnings).", p.warn);
    } else if running && d.enabled && game_seen.is_none_or(|t| t.elapsed().as_secs() > 15) {
        note(ui, "No game audio yet. Ducking reacts to everything except your sources.", p.text_faint);
    }

    ui.add_space(2.0);
    egui::CollapsingHeader::new(RichText::new("Advanced").color(p.text_dim)).id_salt(("ducking_advanced", id)).show(
        ui,
        |ui| {
            let k = &mut s.ducking;
            let before = *k;
            widgets::slider_row(
                ui,
                p,
                "Threshold",
                "Level above which ducking starts.",
                &mut k.threshold_db,
                -60.0..=-10.0,
                false,
                |v| format!("{v:.0} dB"),
            );
            widgets::slider_row(
                ui,
                p,
                "Depth",
                "Most this source is turned down.",
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
                "How long it stays down after it gets quiet.",
                &mut k.hold_ms,
                0.0..=1000.0,
                false,
                |v| format!("{v:.0} ms"),
            );
            widgets::slider_row(
                ui,
                p,
                "Release",
                "How fast this source comes back.",
                &mut k.release_ms,
                50.0..=3000.0,
                true,
                |v| format!("{v:.0} ms"),
            );
            if *k != before && !k.enabled && !k.under_voice {
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

/// The apps that never count as the game, for every source.
fn ignore_list(app: &mut SidestageApp, ui: &mut Ui, p: &Palette) {
    ui.separator();
    widgets::section(ui, p, "Never duck for these apps");
    ui.add(
        egui::Label::new(
            RichText::new(
                "Their sounds never turn your sources down, like a video playing in your browser. For all sources.",
            )
            .color(p.text_dim)
            .text_style(TextStyle::Small),
        )
        .wrap(),
    );
    ui.add_space(4.0);
    let mut remove = None;
    let mut add = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        for (i, a) in app.config.ducking_ignore.iter().enumerate() {
            let running = app.snap.app_present(&a.key);
            let tip =
                if running { a.key.0.clone() } else { format!("{} (not running right now)\n{}", a.name, a.key.0) };
            if widgets::chip(ui, p, &a.name, &tip) {
                remove = Some(i);
            }
        }
        let button = ui
            .add(
                egui::Button::new(RichText::new("+ Add app").color(p.text_dim).size(12.5))
                    .corner_radius(CornerRadius::same(13))
                    .min_size(vec2(0.0, 26.0)),
            )
            .on_hover_text("Pick an app whose sound should never turn your sources down.");
        egui::Popup::menu(&button).width(240.0).show(|ui| {
            ui.set_min_width(220.0);
            let mut apps: Vec<&AudioApp> = app.snap.apps.iter().collect();
            apps.sort_by_cached_key(|a| a.name.to_lowercase());
            let mut any = false;
            ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for a in apps {
                    any = true;
                    // A switched-off source's app plays normally and so does count as the game.
                    let source = app.config.source_for(&a.key).is_some_and(|s| s.enabled);
                    let ignored = app.config.is_ignored(&a.key);
                    let label = match (source, ignored) {
                        (true, _) => format!("{} \u{00b7} a source", a.name),
                        (_, true) => format!("{} \u{00b7} added", a.name),
                        _ => a.name.clone(),
                    };
                    let response = ui.add_enabled(!source && !ignored, egui::Button::new(label));
                    let response = if source {
                        response.on_disabled_hover_text("Sources never turn each other down for the game.")
                    } else {
                        response
                    };
                    if response.clicked() {
                        add = Some(ChosenApp { key: a.key.clone(), name: a.name.clone() });
                        ui.close();
                    }
                }
            });
            if !any {
                ui.label(RichText::new("No apps are playing audio right now.").color(p.text_faint));
            }
        });
    });
    if let Some(i) = remove {
        let gone = app.config.ducking_ignore.remove(i);
        log::info!("ducking listens to {} again", gone.name);
    }
    if let Some(a) = add {
        log::info!("ducking ignores {} ({})", a.name, a.key.0);
        app.config.ducking_ignore.push(a);
    }
    if !app.config.sidechain {
        ui.add(
            egui::Label::new(
                RichText::new("Game-level metering is off, so nothing turns your sources down for the game.")
                    .color(p.text_faint)
                    .text_style(TextStyle::Small),
            )
            .wrap(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muffle_mapping_round_trips() {
        assert!((muffle_amount(20_000.0)).abs() < 1e-6);
        assert!((muffle_amount(800.0) - 1.0).abs() < 1e-6);
        for hz in [800.0, 1400.0, 3400.0, 6000.0, 8000.0, 12_000.0, 14_000.0, 20_000.0] {
            assert!((muffle_cutoff(muffle_amount(hz)) - hz).abs() / hz < 1e-4, "{hz}");
        }
    }
}
