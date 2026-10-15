//! Look and feel: colour palettes for dark and light mode, egui styles built from them, and the
//! window icon (drawn at runtime, so there are no image files to ship).

use std::sync::Arc;

use eframe::egui::{
    self, Color32, Context, CornerRadius, FontFamily, FontId, IconData, Margin, Shadow, Stroke, TextStyle, Theme,
    Visuals, style::HandleShape,
};

/// Every colour the app uses. Custom-painted parts (radar, meters, cards) read these directly;
/// standard widgets get them through the egui [`Visuals`] built in [`install`].
///
/// Text colours meet WCAG AA (4.5:1) on `bg`, `stage` and `surface` in both themes, including
/// `text_faint`; `warn`/`err`/`info` are also used for text and meet it on `bg`.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    /// Side panels and bars.
    pub bg: Color32,
    /// The radar stage in the middle.
    pub stage: Color32,
    /// Cards and grouped controls.
    pub surface: Color32,
    /// Hovered cards, input fields, slider rails.
    pub surface_hi: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,
    /// Sidestage violet.
    pub accent: Color32,
    pub accent_hi: Color32,
    /// Accent fill behind `on_accent` text (the plain accent is too light for white text in the
    /// dark theme).
    pub accent_strong: Color32,
    pub on_accent: Color32,
    /// Track of a switch that is off.
    pub toggle_off: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub info: Color32,
    pub meter_bg: Color32,
}

pub const DARK: Palette = Palette {
    dark: true,
    bg: Color32::from_rgb(0x15, 0x16, 0x1C),
    stage: Color32::from_rgb(0x0F, 0x10, 0x15),
    surface: Color32::from_rgb(0x1D, 0x1F, 0x27),
    surface_hi: Color32::from_rgb(0x27, 0x2A, 0x35),
    border: Color32::from_rgb(0x2E, 0x31, 0x3D),
    text: Color32::from_rgb(0xE8, 0xE9, 0xEF),
    text_dim: Color32::from_rgb(0xA0, 0xA4, 0xB4),
    text_faint: Color32::from_rgb(0x80, 0x86, 0x9A),
    accent: Color32::from_rgb(0x8F, 0x7C, 0xFF),
    accent_hi: Color32::from_rgb(0xB3, 0xA6, 0xFF),
    accent_strong: Color32::from_rgb(0x6E, 0x58, 0xF0),
    on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    toggle_off: Color32::from_rgb(0x30, 0x33, 0x3F),
    ok: Color32::from_rgb(0x3D, 0xD6, 0x8C),
    warn: Color32::from_rgb(0xF2, 0xB3, 0x3D),
    err: Color32::from_rgb(0xF3, 0x6A, 0x6A),
    info: Color32::from_rgb(0x6C, 0xB4, 0xFF),
    meter_bg: Color32::from_rgb(0x24, 0x27, 0x31),
};

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: Color32::from_rgb(0xF3, 0xF4, 0xF8),
    stage: Color32::from_rgb(0xFB, 0xFB, 0xFD),
    surface: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    surface_hi: Color32::from_rgb(0xE9, 0xEB, 0xF2),
    border: Color32::from_rgb(0xD9, 0xDC, 0xE6),
    text: Color32::from_rgb(0x1B, 0x1D, 0x26),
    text_dim: Color32::from_rgb(0x4F, 0x55, 0x67),
    text_faint: Color32::from_rgb(0x66, 0x6C, 0x7E),
    accent: Color32::from_rgb(0x5E, 0x48, 0xE8),
    accent_hi: Color32::from_rgb(0x48, 0x32, 0xD0),
    accent_strong: Color32::from_rgb(0x5E, 0x48, 0xE8),
    on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    // Dark enough that the white knob and the track itself both stand out (3:1).
    toggle_off: Color32::from_rgb(0x86, 0x8C, 0x9E),
    ok: Color32::from_rgb(0x1E, 0x9E, 0x5E),
    warn: Color32::from_rgb(0x9A, 0x63, 0x00),
    err: Color32::from_rgb(0xC2, 0x34, 0x34),
    info: Color32::from_rgb(0x1F, 0x68, 0xC8),
    meter_bg: Color32::from_rgb(0xE3, 0xE5, 0xEC),
};

/// The palette of the theme egui is currently showing.
pub fn palette(ctx: &Context) -> &'static Palette {
    match ctx.theme() {
        Theme::Dark => &DARK,
        Theme::Light => &LIGHT,
    }
}

/// Text style for the small upper-case section labels ("MUSIC SOURCE").
pub fn section_style() -> TextStyle {
    TextStyle::Name("Section".into())
}

pub const RADIUS: u8 = 7;
pub const CARD_RADIUS: u8 = 10;

/// Set up fonts sizes, spacing and both colour schemes.
pub fn install(ctx: &Context) {
    for (theme, p) in [(Theme::Dark, &DARK), (Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |style| {
            style.visuals = visuals(p);
            style.text_styles = [
                (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
                (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
                (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
                (TextStyle::Heading, FontId::new(17.0, FontFamily::Proportional)),
                (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
                (section_style(), FontId::new(11.0, FontFamily::Proportional)),
            ]
            .into();
            let s = &mut style.spacing;
            s.item_spacing = egui::vec2(8.0, 6.0);
            s.button_padding = egui::vec2(10.0, 4.0);
            s.interact_size = egui::vec2(36.0, 24.0);
            s.slider_rail_height = 4.0;
            s.combo_height = 320.0;
            s.window_margin = Margin::same(14);
            s.menu_margin = Margin::same(6);
            s.scroll = egui::style::ScrollStyle::thin();
            style.animation_time = 0.12;
        });
    }
}

fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    let r = CornerRadius::same(RADIUS);
    v.override_text_color = None;
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(CARD_RADIUS + 2);
    v.window_shadow = Shadow {
        offset: [0, 10],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(if p.dark { 140 } else { 40 }),
    };
    v.popup_shadow =
        Shadow { offset: [0, 6], blur: 16, spread: 0, color: Color32::from_black_alpha(if p.dark { 120 } else { 32 }) };
    v.menu_corner_radius = CornerRadius::same(8);
    v.extreme_bg_color = if p.dark { p.stage } else { p.surface };
    v.text_edit_bg_color = Some(if p.dark { p.stage } else { p.surface });
    v.faint_bg_color = p.surface;
    v.code_bg_color = p.surface_hi;
    v.hyperlink_color = p.accent_hi;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.err;
    v.selection.bg_fill = p.accent.gamma_multiply(if p.dark { 0.55 } else { 0.35 });
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.slider_trailing_fill = true;
    v.handle_shape = HandleShape::Circle;
    v.striped = false;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.surface;
    w.noninteractive.weak_bg_fill = p.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, p.text_dim);
    w.noninteractive.corner_radius = r;

    w.inactive.bg_fill = p.surface_hi;
    w.inactive.weak_bg_fill = p.surface_hi;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, p.text);
    w.inactive.corner_radius = r;
    w.inactive.expansion = 0.0;

    let hover_fill = p.surface_hi.lerp_to_gamma(p.text, 0.08);
    w.hovered.bg_fill = hover_fill;
    w.hovered.weak_bg_fill = hover_fill;
    w.hovered.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.7));
    w.hovered.fg_stroke = Stroke::new(1.5, p.text);
    w.hovered.corner_radius = r;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = p.accent;
    w.active.weak_bg_fill = p.accent.gamma_multiply(0.35);
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(2.0, p.text);
    w.active.corner_radius = r;
    w.active.expansion = 0.0;

    w.open.bg_fill = p.surface_hi;
    w.open.weak_bg_fill = p.surface_hi;
    w.open.bg_stroke = Stroke::new(1.0, p.accent.gamma_multiply(0.7));
    w.open.fg_stroke = Stroke::new(1.0, p.text);
    w.open.corner_radius = r;
    v
}

/// The window icon: a violet tile with a top-down head and the music puck behind it, like the
/// radar. Rendered with simple signed-distance shapes at runtime.
pub fn icon() -> Arc<IconData> {
    const N: usize = 128;
    let mut rgba = vec![0u8; N * N * 4];
    let top: [u8; 3] = [0xA0, 0x8C, 0xFF];
    let bottom: [u8; 3] = [0x5B, 0x3E, 0xE0];
    for y in 0..N {
        for x in 0..N {
            // Sample at the pixel centre in a -1..1 square.
            let px = (x as f32 + 0.5) / N as f32 * 2.0 - 1.0;
            let py = (y as f32 + 0.5) / N as f32 * 2.0 - 1.0;
            let aa = 2.0 / N as f32;
            // Rounded square tile.
            let q = (px.abs() - 0.78).max(0.0).hypot((py.abs() - 0.78).max(0.0)) - 0.2;
            let tile = coverage(q, aa);
            if tile <= 0.0 {
                continue;
            }
            let t = (py + 1.0) / 2.0;
            let mut c = [0.0f32; 3];
            for i in 0..3 {
                c[i] = f32::from(top[i]) * (1.0 - t) + f32::from(bottom[i]) * t;
            }
            // Distance rings (faint), head with nose, and the puck behind it.
            let r = px.hypot(py + 0.12);
            let ring = (coverage((r - 0.52).abs() - 0.025, aa) + coverage((r - 0.78).abs() - 0.02, aa) * 0.6) * 0.35;
            let head = coverage(r - 0.22, aa);
            let nose = coverage(px.abs() * 1.6 + (py + 0.12 + 0.27) - 0.06, aa) * coverage(-(py + 0.12 + 0.18), aa);
            let puck = coverage(px.hypot(py - 0.62) - 0.15, aa);
            let white = (ring + head + nose).min(1.0);
            for ch in &mut c {
                *ch += (255.0 - *ch) * white;
            }
            // The puck is a warm "stage light" dot so it reads at 16 px.
            let puck_col: [u8; 3] = [0xFF, 0xD3, 0x6E];
            for (ch, pc) in c.iter_mut().zip(puck_col) {
                *ch += (f32::from(pc) - *ch) * puck;
            }
            let o = (y * N + x) * 4;
            rgba[o] = c[0].round() as u8;
            rgba[o + 1] = c[1].round() as u8;
            rgba[o + 2] = c[2].round() as u8;
            rgba[o + 3] = (tile * 255.0).round() as u8;
        }
    }
    Arc::new(IconData { rgba, width: N as u32, height: N as u32 })
}

/// Anti-aliased inside-ness of a signed distance (negative = inside).
fn coverage(distance: f32, aa: f32) -> f32 {
    (0.5 - distance / aa).clamp(0.0, 1.0)
}
