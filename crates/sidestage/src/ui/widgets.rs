//! Small custom widgets shared by the panels.

use std::ops::RangeInclusive;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Frame, Margin, Response, RichText, Sense, Stroke, StrokeKind,
    TextStyle, TextWrapMode, Ui, Vec2, WidgetInfo, WidgetType, pos2, vec2,
};

use super::theme::{self, Palette};

/// Small upper-case section heading.
pub fn section(ui: &mut Ui, p: &Palette, text: &str) {
    ui.add_space(4.0);
    ui.label(
        RichText::new(text.to_uppercase())
            .text_style(theme::section_style())
            .color(p.text_faint)
            .extra_letter_spacing(1.2),
    );
    ui.add_space(2.0);
}

/// An iOS-style on/off switch. `label` is its name for screen readers.
pub fn toggle(ui: &mut Ui, p: &Palette, on: &mut bool, size: f32, label: &str) -> Response {
    let desired = vec2(size * 1.8, size);
    let (rect, mut response) = ui.allocate_exact_size(desired, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, ui.is_enabled(), *on, label));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let fill = p.toggle_off.lerp_to_gamma(p.accent, t);
        let fill = if response.hovered() { fill.lerp_to_gamma(p.text, 0.06) } else { fill };
        let r = rect.height() / 2.0;
        ui.painter().rect_filled(rect, CornerRadius::same(r as u8), fill);
        let knob_x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
        ui.painter().circle_filled(pos2(knob_x, rect.center().y), r - 3.0, Color32::WHITE);
        if response.has_focus() {
            ui.painter().rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(r as u8 + 2),
                Stroke::new(1.5, p.accent),
                StrokeKind::Outside,
            );
        }
    }
    response
}

/// A rounded status label with a coloured dot.
pub fn pill(ui: &mut Ui, p: &Palette, text: &str, color: Color32, max_width: f32) -> Response {
    let font = FontId::proportional(12.5);
    let galley = ui.painter().layout(text.to_owned(), font, p.text, f32::INFINITY);
    let text_w = galley.size().x.min(max_width - 30.0);
    let size = vec2(text_w + 30.0, 24.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, format!("Status: {text}")));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_filled(rect, CornerRadius::same(12), color.gamma_multiply(if p.dark { 0.16 } else { 0.12 }));
        painter.rect_stroke(
            rect,
            CornerRadius::same(12),
            Stroke::new(1.0, color.gamma_multiply(0.45)),
            StrokeKind::Inside,
        );
        painter.circle_filled(pos2(rect.left() + 13.0, rect.center().y), 4.0, color);
        let job_galley = truncated(ui, text, FontId::proportional(12.5), p.text, text_w);
        painter.galley(pos2(rect.left() + 22.0, rect.center().y - job_galley.size().y / 2.0), job_galley, p.text);
    }
    response
}

/// Text cut to one line with an ellipsis.
pub fn truncated(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(1.0));
    ui.painter().layout_job(job)
}

/// Map a linear peak to a meter fraction on a -60..0 dBFS scale.
pub fn meter_fraction(linear: f32) -> f32 {
    if linear <= 0.001 { 0.0 } else { ((20.0 * linear.log10() + 60.0) / 60.0).clamp(0.0, 1.0) }
}

/// A thin horizontal level bar in `rect`.
pub fn level_bar(ui: &Ui, p: &Palette, rect: egui::Rect, fraction: f32, color: Color32) {
    let painter = ui.painter();
    let r = CornerRadius::same((rect.height() / 2.0) as u8);
    painter.rect_filled(rect, r, p.meter_bg);
    let f = fraction.clamp(0.0, 1.0);
    if f > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * f).max(rect.height()));
        painter.rect_filled(fill, r, color);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    Info,
    Warning,
    Error,
}

/// A coloured message card with a title, body lines and optional buttons. Returns the index of
/// the clicked button.
pub fn card(
    ui: &mut Ui,
    p: &Palette,
    kind: CardKind,
    title: &str,
    lines: &[String],
    buttons: &[&str],
) -> Option<usize> {
    let color = match kind {
        CardKind::Info => p.info,
        CardKind::Warning => p.warn,
        CardKind::Error => p.err,
    };
    let mut clicked = None;
    Frame::new()
        .fill(color.gamma_multiply(if p.dark { 0.10 } else { 0.07 }))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.40)))
        .corner_radius(CornerRadius::same(theme::CARD_RADIUS))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                let c = rect.center();
                ui.painter().circle_filled(c, 7.5, color);
                let glyph = if kind == CardKind::Info { "i" } else { "!" };
                ui.painter().text(
                    c + vec2(0.0, 0.5),
                    Align2::CENTER_CENTER,
                    glyph,
                    FontId::proportional(11.0),
                    p.stage,
                );
                ui.add(egui::Label::new(RichText::new(title).strong().color(p.text)).wrap());
            });
            for line in lines {
                ui.add(egui::Label::new(RichText::new(line).color(p.text_dim).text_style(TextStyle::Small)).wrap());
            }
            if !buttons.is_empty() {
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    for (i, b) in buttons.iter().enumerate() {
                        let button = if i == 0 {
                            egui::Button::new(RichText::new(*b).color(p.text)).fill(color.gamma_multiply(0.22))
                        } else {
                            egui::Button::new(RichText::new(*b).color(p.text_dim)).frame(false)
                        };
                        if ui.add(button).clicked() {
                            clicked = Some(i);
                        }
                    }
                });
            }
        });
    clicked
}

/// A row of joined buttons (a segmented control). Returns the index clicked this frame.
pub fn segmented(ui: &mut Ui, p: &Palette, labels: &[&str], selected: Option<usize>) -> Option<usize> {
    let height = 28.0;
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let painter = ui.painter().clone();
    painter.rect_filled(rect, CornerRadius::same(8), p.surface_hi);
    // Segments are as wide as their labels plus an equal share of the spare room, so a long
    // label ("Competitive") does not get cut while short ones ("Off") stay compact.
    let font = FontId::proportional(12.5);
    let text_w: Vec<f32> =
        labels.iter().map(|l| painter.layout_no_wrap((*l).to_owned(), font.clone(), p.text).size().x).collect();
    let spare = ((width - text_w.iter().sum::<f32>()) / labels.len() as f32).max(0.0);
    let scale = if spare > 0.0 { 1.0 } else { width / text_w.iter().sum::<f32>().max(1.0) };
    let mut clicked = None;
    let mut x = rect.left();
    for (i, label) in labels.iter().enumerate() {
        let seg_w = text_w[i] * scale + spare;
        let seg = egui::Rect::from_min_size(pos2(x, rect.top()), vec2(seg_w, height));
        x += seg_w;
        let id = ui.id().with(("segment", i));
        let response = ui.interact(seg, id, Sense::click());
        response.widget_info(|| WidgetInfo::selected(WidgetType::SelectableLabel, true, selected == Some(i), *label));
        let is_selected = selected == Some(i);
        let inner = seg.shrink(2.0);
        if is_selected {
            painter.rect_filled(inner, CornerRadius::same(6), p.accent_strong);
        } else if response.hovered() {
            painter.rect_filled(inner, CornerRadius::same(6), p.text.gamma_multiply(0.06));
        }
        if response.has_focus() {
            painter.rect_stroke(inner, CornerRadius::same(6), Stroke::new(1.5, p.accent_hi), StrokeKind::Inside);
        }
        let color = if is_selected { p.on_accent } else { p.text_dim };
        let galley = truncated(ui, label, font.clone(), color, seg_w - 4.0);
        painter.galley(seg.center() - galley.size() / 2.0, galley, color);
        if response.clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

pub const LABEL_W: f32 = 80.0;
pub const VALUE_W: f32 = 54.0;

/// "Label  [------o----]  value" with fixed label and value columns so rows line up.
#[expect(clippy::too_many_arguments)]
pub fn slider_row(
    ui: &mut Ui,
    p: &Palette,
    label: &str,
    tooltip: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    logarithmic: bool,
    format: impl Fn(f32) -> String,
) -> Response {
    let height = ui.spacing().interact_size.y;
    ui.horizontal(|ui| {
        let (label_rect, label_resp) = ui.allocate_exact_size(vec2(LABEL_W, height), Sense::hover());
        let galley = truncated(ui, label, FontId::proportional(13.0), p.text_dim, LABEL_W - 4.0);
        ui.painter().galley(pos2(label_rect.left(), label_rect.center().y - galley.size().y / 2.0), galley, p.text_dim);
        label_resp.on_hover_text(tooltip);
        let slider_w = (ui.available_width() - VALUE_W - ui.spacing().item_spacing.x).max(40.0);
        ui.spacing_mut().slider_width = slider_w;
        // The handle radius follows the slider's height (the larger of the interact height and
        // the body text height); keep both small for a neat handle. No text is shown inside.
        ui.spacing_mut().interact_size.y = 13.0;
        ui.style_mut().text_styles.insert(TextStyle::Body, FontId::proportional(10.0));
        let response = ui
            .add(egui::Slider::new(value, range).logarithmic(logarithmic).show_value(false).trailing_fill(true))
            .on_hover_text(tooltip);
        let (value_rect, _) = ui.allocate_exact_size(vec2(VALUE_W, height), Sense::hover());
        let text = format(*value);
        let galley = truncated(ui, &text, FontId::proportional(12.5), p.text, VALUE_W);
        ui.painter().galley(
            pos2(value_rect.right() - galley.size().x, value_rect.center().y - galley.size().y / 2.0),
            galley,
            p.text,
        );
        response
    })
    .inner
}

/// A three-dot "more" button.
pub fn more_button(ui: &mut Ui, p: &Palette) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    let fill = if response.hovered() { p.surface_hi } else { Color32::TRANSPARENT };
    ui.painter().rect_filled(rect, CornerRadius::same(theme::RADIUS), fill);
    for dx in [-6.0, 0.0, 6.0] {
        ui.painter().circle_filled(rect.center() + vec2(dx, 0.0), 1.8, p.text_dim);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, "More"));
    response
}

/// Text that never wraps (labels in tight rows).
pub fn nowrap(ui: &mut Ui, text: RichText) -> Response {
    ui.add(egui::Label::new(text).wrap_mode(TextWrapMode::Truncate))
}
