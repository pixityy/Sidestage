//! The radar: a top-down view of the listener's head where the music is a puck you drag around.
//!
//! Angle = direction (0 straight ahead, up on screen; +90 right). Distance from the head is on a
//! log scale so 0.5-2 m (where most placement happens) gets as much room as 2-20 m.
//!
//! Mouse: drag or click to place (Ctrl/Cmd snaps to 15 degrees), Shift-drag up/down for height,
//! scroll for distance, double-click to return to the preset. Keyboard (after clicking or
//! tabbing to it): arrows move the puck, Shift+arrows in small steps, Page Up/Down change
//! height, Home returns to the preset.

use std::f32::consts::PI;

use eframe::egui::{
    self, Align2, Color32, CursorIcon, EventFilter, FontId, Key, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui,
    Vec2, pos2, vec2,
};
use sidestage_dsp::Settings;

use super::theme::Palette;

pub const MIN_DIST: f32 = 0.5;
pub const MAX_DIST: f32 = 20.0;
pub const RINGS: [f32; 6] = [0.5, 1.0, 2.0, 5.0, 10.0, 20.0];
/// Ctrl-drag snaps the direction to multiples of this.
pub const SNAP_DEG: f32 = 15.0;
/// Half the angle of the "game" wedge in front of the listener.
pub const GAME_WEDGE_HALF_DEG: f32 = 60.0;
/// Ring labels sit along this direction (front-left, where the music rarely goes).
const LABEL_AZ: f32 = -45.0;

/// Screen geometry of the radar: where the head is and which radii the distance range maps to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadarGeom {
    pub center: Pos2,
    /// Radius of the 0.5 m ring.
    pub inner: f32,
    /// Radius of the 20 m ring.
    pub outer: f32,
}

impl RadarGeom {
    /// Largest radar that fits `rect`, leaving room for labels.
    pub fn fit(rect: Rect) -> Self {
        let outer = (rect.width().min(rect.height()) * 0.5 - 22.0).max(60.0);
        let inner = (outer * 0.16).clamp(20.0, 46.0);
        Self { center: rect.center(), inner, outer }
    }

    pub fn head_radius(&self) -> f32 {
        self.inner * 0.62
    }

    /// Ring radius for a distance in metres (clamped to the radar's range).
    pub fn radius_for(&self, dist_m: f32) -> f32 {
        let d = if dist_m.is_finite() { dist_m.clamp(MIN_DIST, MAX_DIST) } else { MIN_DIST };
        let t = (d.ln() - MIN_DIST.ln()) / (MAX_DIST.ln() - MIN_DIST.ln());
        self.inner + t * (self.outer - self.inner)
    }

    /// Distance in metres for a radius on screen (clamped to the radar's range).
    pub fn dist_for(&self, radius: f32) -> f32 {
        let t = ((radius - self.inner) / (self.outer - self.inner)).clamp(0.0, 1.0);
        (MIN_DIST.ln() + t * (MAX_DIST.ln() - MIN_DIST.ln())).exp()
    }

    pub fn pos_of(&self, azimuth_deg: f32, dist_m: f32) -> Pos2 {
        self.center + dir(azimuth_deg) * self.radius_for(dist_m)
    }

    /// (azimuth in (-180, 180], distance in metres) for a point on screen.
    pub fn polar_at(&self, p: Pos2) -> (f32, f32) {
        let v = p - self.center;
        let az = if v.length() < 1e-3 { 180.0 } else { wrap_deg(v.x.atan2(-v.y).to_degrees()) };
        (az, self.dist_for(v.length()))
    }
}

/// Unit vector for an azimuth: 0 = up, +90 = right.
fn dir(azimuth_deg: f32) -> Vec2 {
    let a = azimuth_deg.to_radians();
    vec2(a.sin(), -a.cos())
}

/// Wrap into (-180, 180] (so "behind" is +180, like `Settings::sanitized`).
pub fn wrap_deg(a: f32) -> f32 {
    180.0 - (180.0 - a).rem_euclid(360.0)
}

pub fn snap_deg(a: f32, step: f32) -> f32 {
    wrap_deg((a / step).round() * step)
}

/// Plain-language direction for the readout and screen readers.
pub fn direction_name(azimuth_deg: f32) -> &'static str {
    let a = wrap_deg(azimuth_deg);
    let right = a > 0.0;
    match a.abs() {
        x if x < 15.0 => "In front",
        x if x < 67.5 => {
            if right {
                "Front right"
            } else {
                "Front left"
            }
        }
        x if x < 112.5 => {
            if right {
                "Right"
            } else {
                "Left"
            }
        }
        x if x < 165.0 => {
            if right {
                "Behind right"
            } else {
                "Behind left"
            }
        }
        _ => "Behind you",
    }
}

pub fn format_distance(m: f32) -> String {
    if m < 10.0 { format!("{m:.1} m") } else { format!("{m:.0} m") }
}

/// "0°" at ear level (not "+0°"), otherwise signed: "+15°", "-20°".
pub fn format_height(deg: f32) -> String {
    let r = deg.round();
    if r == 0.0 { "0\u{00b0}".into() } else { format!("{r:+.0}\u{00b0}") }
}

/// What the radar shows besides the settings.
pub struct RadarInput<'a> {
    pub palette: &'a Palette,
    /// Current ducking gain reduction (dB, positive) and the configured depth.
    pub duck_db: f32,
    pub duck_depth_db: f32,
    /// Smoothed output level 0..1, for a subtle glow.
    pub level: f32,
    /// Dim everything (power off / not running).
    pub inactive: bool,
    /// Settings of the preset that double-click returns to.
    pub preset: Settings,
}

/// Draw the radar in all of `ui`'s available space and handle input. The response is marked
/// changed when `settings` changed.
pub fn radar(ui: &mut Ui, settings: &mut Settings, input: &RadarInput<'_>) -> egui::Response {
    let p = input.palette;
    let mut response = ui.allocate_response(ui.available_size(), Sense::click_and_drag());
    // Clipped to the panel rather than to the allocated rect: the puck's glow and ducking pulse
    // may reach into the panel's margin when the music sits on the outer ring.
    let painter = ui.painter().clone();
    let rect = response.rect;
    let geom = RadarGeom::fit(rect);
    let before = *settings;
    let id = response.id;

    // ---- input -------------------------------------------------------------------------
    let (modifiers, scroll, time) = ui.input(|i| (i.modifiers, i.smooth_scroll_delta.y, i.time));
    if response.drag_started() || response.clicked() {
        response.request_focus();
    }
    if response.dragged() && modifiers.shift {
        // Shift-drag: height.
        settings.elevation_deg = (settings.elevation_deg - response.drag_delta().y * 0.5).clamp(-40.0, 80.0);
    } else if (response.dragged() || response.clicked())
        && let Some(pos) = response.interact_pointer_pos()
    {
        let (mut az, dist) = geom.polar_at(pos);
        if modifiers.command || modifiers.ctrl {
            az = snap_deg(az, SNAP_DEG);
        }
        settings.azimuth_deg = az;
        settings.distance_m = dist;
    }
    if response.hovered() && scroll != 0.0 {
        // Scroll up pushes the music away.
        settings.distance_m = (settings.distance_m * (scroll * 0.004).exp()).clamp(MIN_DIST, MAX_DIST);
        ui.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
    }
    let mut reset = response.double_clicked();
    if response.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() },
            );
        });
        reset |= keyboard(ui, &geom, settings);
    }
    if reset {
        settings.azimuth_deg = input.preset.azimuth_deg;
        settings.distance_m = input.preset.distance_m;
        settings.elevation_deg = input.preset.elevation_deg;
        settings.width_deg = input.preset.width_deg;
    }
    if response.dragged() {
        ui.ctx().set_cursor_icon(if modifiers.shift { CursorIcon::ResizeVertical } else { CursorIcon::Grabbing });
    } else if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
    }
    if *settings != before {
        *settings = settings.sanitized();
    }

    // ---- drawing -----------------------------------------------------------------------
    let dim = if input.inactive { 0.45 } else { 1.0 };
    let c = geom.center;
    let ring_color = p.text.gamma_multiply(if p.dark { 0.10 } else { 0.14 });
    let label_color = p.text_faint;

    // Game wedge in front: the area the music stays away from.
    let wedge_fill = p.info.gamma_multiply(if p.dark { 0.07 } else { 0.06 });
    let steps = 24;
    let mut fan = vec![c];
    for i in 0..=steps {
        let a = -GAME_WEDGE_HALF_DEG + 2.0 * GAME_WEDGE_HALF_DEG * i as f32 / steps as f32;
        fan.push(c + dir(a) * geom.outer);
    }
    painter.add(Shape::convex_polygon(fan, wedge_fill, Stroke::NONE));
    for a in [-GAME_WEDGE_HALF_DEG, GAME_WEDGE_HALF_DEG] {
        painter.line_segment(
            [c + dir(a) * geom.inner, c + dir(a) * geom.outer],
            Stroke::new(1.0, p.info.gamma_multiply(0.18)),
        );
    }
    painter.text(
        c + dir(0.0) * (geom.outer - 16.0),
        Align2::CENTER_CENTER,
        "GAME",
        FontId::proportional(10.5),
        p.info.gamma_multiply(if p.dark { 0.75 } else { 0.9 }),
    );

    // Distance rings with labels along the front-left diagonal.
    for (i, d) in RINGS.iter().enumerate() {
        let r = geom.radius_for(*d);
        let stroke = if i == RINGS.len() - 1 {
            Stroke::new(1.2, ring_color.gamma_multiply(1.6))
        } else {
            Stroke::new(1.0, ring_color)
        };
        painter.circle_stroke(c, r, stroke);
    }
    // Outermost first; on a small radar, labels that would collide with one already placed (or
    // with the head) are skipped.
    let head_box = Rect::from_center_size(c, Vec2::splat(geom.head_radius() * 2.7));
    let mut placed: Vec<Rect> = Vec::new();
    for d in RINGS.iter().rev() {
        let r = geom.radius_for(*d);
        let label = if *d < 1.0 { format!("{d} m") } else { format!("{d:.0} m") };
        let pos = c + dir(LABEL_AZ) * r;
        let galley = painter.layout_no_wrap(label, FontId::proportional(10.5), label_color);
        let bg = Rect::from_center_size(pos, galley.size() + vec2(6.0, 2.0));
        if placed.iter().any(|q| q.expand(1.0).intersects(bg)) || head_box.intersects(bg) {
            continue;
        }
        placed.push(bg);
        painter.rect_filled(bg, 3, p.stage);
        painter.galley(bg.min + vec2(3.0, 1.0), galley, label_color);
    }

    // Line from the head to the music.
    let puck = geom.pos_of(settings.azimuth_deg, settings.distance_m);
    let puck_r = geom.radius_for(settings.distance_m);
    painter.line_segment(
        [c + (puck - c).normalized() * geom.head_radius(), puck],
        Stroke::new(1.0, p.accent.gamma_multiply(0.35 * dim)),
    );

    // Spread arc: where the left and right virtual speakers are.
    if settings.width_deg > 0.5 {
        let half = settings.width_deg * 0.5;
        let a0 = settings.azimuth_deg - half;
        let a1 = settings.azimuth_deg + half;
        let n = ((settings.width_deg / 3.0).ceil() as usize).max(2);
        let pts: Vec<Pos2> = (0..=n).map(|i| c + dir(a0 + (a1 - a0) * i as f32 / n as f32) * puck_r).collect();
        painter.add(Shape::line(pts, Stroke::new(3.0, p.accent.gamma_multiply(0.45 * dim))));
        // The music's left channel stays on the listener's left.
        let (left, right) = if dir(a0).x <= dir(a1).x { (a0, a1) } else { (a1, a0) };
        for (a, label) in [(left, "L"), (right, "R")] {
            let pt = c + dir(a) * puck_r;
            painter.circle(pt, 4.5, p.stage, Stroke::new(1.5, p.accent.gamma_multiply(0.9 * dim)));
            let out = c + dir(a) * (puck_r + 13.0);
            painter.text(out, Align2::CENTER_CENTER, label, FontId::proportional(10.5), p.text_dim.gamma_multiply(dim));
        }
    }

    // Head: ears, nose, face.
    let hr = geom.head_radius();
    let head_fill = if p.dark { p.surface_hi } else { p.surface };
    let head_stroke = Stroke::new(1.5, p.text_dim.gamma_multiply(0.8));
    for side in [-1.0, 1.0] {
        let ear = Rect::from_center_size(c + vec2(side * hr * 0.98, hr * 0.05), vec2(hr * 0.32, hr * 0.62));
        let corner = egui::CornerRadius::same((hr * 0.16).round().clamp(0.0, 255.0) as u8);
        painter.rect(ear, corner, head_fill, head_stroke, StrokeKind::Middle);
    }
    let nose = vec![c + vec2(-hr * 0.26, -hr * 0.9), c + vec2(0.0, -hr * 1.28), c + vec2(hr * 0.26, -hr * 0.9)];
    painter.add(Shape::convex_polygon(nose, head_fill, head_stroke));
    painter.circle(c, hr, head_fill, head_stroke);

    // Puck: glow with the music level, dims and pulses while ducking.
    let duck = if input.duck_depth_db > 0.0 { (input.duck_db / input.duck_depth_db).clamp(0.0, 1.0) } else { 0.0 };
    let alpha = dim * (1.0 - 0.45 * duck);
    let base_r = 13.0;
    let glow = (input.level.clamp(0.0, 1.0) * 10.0 + 4.0) * dim;
    painter.circle_filled(puck, base_r + glow, p.accent.gamma_multiply(0.12 * alpha));
    painter.circle_filled(puck, base_r + glow * 0.5, p.accent.gamma_multiply(0.18 * alpha));
    if duck > 0.02 {
        let phase = (time as f32 * 2.2).fract();
        let ring_r = base_r + 4.0 + phase * 16.0;
        painter.circle_stroke(puck, ring_r, Stroke::new(2.0, p.warn.gamma_multiply(duck * (1.0 - phase) * dim)));
    }
    let focus = response.has_focus();
    painter.circle(
        puck,
        base_r,
        p.accent.gamma_multiply(alpha),
        Stroke::new(if focus { 2.5 } else { 1.5 }, if focus { p.text } else { p.accent_hi.gamma_multiply(alpha) }),
    );
    music_note(&painter, puck, base_r * 0.9, p.on_accent.gamma_multiply(alpha.max(0.6)));

    // Height badge next to the puck: up-right of it, mirrored where that would leave the radar.
    if settings.elevation_deg.round() != 0.0 {
        let up = settings.elevation_deg > 0.0;
        let galley = painter.layout_no_wrap(format_height(settings.elevation_deg), FontId::proportional(11.0), p.text);
        let size = vec2(galley.size().x + 18.0, 18.0);
        let gap = base_r + 8.0;
        let mut badge = Rect::from_min_size(puck + vec2(gap, -base_r - 11.0), size);
        if badge.right() > rect.right() {
            badge = badge.translate(vec2(-2.0 * gap - size.x, 0.0));
        }
        if badge.top() < rect.top() {
            badge = badge.translate(vec2(0.0, 2.0 * base_r + 4.0));
        }
        painter.rect(badge, 9, p.surface, Stroke::new(1.0, p.border), StrokeKind::Inside);
        let tri_c = badge.left_center() + vec2(8.0, 0.0);
        let s = if up { -1.0 } else { 1.0 };
        painter.add(Shape::convex_polygon(
            vec![tri_c + vec2(-3.5, -s * 2.5), tri_c + vec2(3.5, -s * 2.5), tri_c + vec2(0.0, s * 3.5)],
            p.accent_hi,
            Stroke::NONE,
        ));
        painter.galley(badge.left_center() + vec2(14.0, -galley.size().y / 2.0), galley, p.text);
    }

    // Focus outline for keyboard users.
    if focus {
        painter.circle_stroke(c, geom.outer + 6.0, Stroke::new(1.5, p.accent.gamma_multiply(0.5)));
    }

    if *settings != before {
        response.mark_changed();
    }
    let summary = format!(
        "{}, {}, height {}",
        direction_name(settings.azimuth_deg),
        format_distance(settings.distance_m),
        format_height(settings.elevation_deg)
    );
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, format!("Music position: {summary}")));
    response
}

/// Arrow keys and Page Up/Down; returns true for Home (back to the preset).
fn keyboard(ui: &Ui, geom: &RadarGeom, settings: &mut Settings) -> bool {
    let (shift, keys) = ui.input(|i| {
        let pressed = |k: Key| i.key_pressed(k);
        (
            i.modifiers.shift,
            [
                pressed(Key::ArrowLeft),
                pressed(Key::ArrowRight),
                pressed(Key::ArrowUp),
                pressed(Key::ArrowDown),
                pressed(Key::PageUp),
                pressed(Key::PageDown),
                pressed(Key::Home),
            ],
        )
    });
    let [left, right, up, down, page_up, page_down, home] = keys;
    let step = if shift { 0.01 } else { 0.04 } * geom.outer;
    let mut delta = Vec2::ZERO;
    if left {
        delta.x -= step;
    }
    if right {
        delta.x += step;
    }
    if up {
        delta.y -= step;
    }
    if down {
        delta.y += step;
    }
    if delta != Vec2::ZERO {
        let p = geom.pos_of(settings.azimuth_deg, settings.distance_m) + delta;
        let (az, dist) = geom.polar_at(p);
        settings.azimuth_deg = az;
        settings.distance_m = dist;
    }
    if page_up {
        settings.elevation_deg = (settings.elevation_deg + 5.0).min(80.0);
    }
    if page_down {
        settings.elevation_deg = (settings.elevation_deg - 5.0).max(-40.0);
    }
    home
}

/// A small eighth note.
fn music_note(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let head_c = center + vec2(-size * 0.22, size * 0.32);
    let rx = size * 0.3;
    let ry = size * 0.22;
    let tilt = -0.35f32;
    let pts: Vec<Pos2> = (0..16)
        .map(|i| {
            let t = i as f32 / 16.0 * 2.0 * PI;
            let (x, y) = (rx * t.cos(), ry * t.sin());
            head_c + vec2(x * tilt.cos() - y * tilt.sin(), x * tilt.sin() + y * tilt.cos())
        })
        .collect();
    painter.add(Shape::convex_polygon(pts, color, Stroke::NONE));
    let stem_x = head_c.x + rx * 0.92;
    let stem_top = center.y - size * 0.55;
    painter.line_segment([pos2(stem_x, head_c.y - ry * 0.3), pos2(stem_x, stem_top)], Stroke::new(size * 0.12, color));
    painter.add(Shape::line(
        vec![
            pos2(stem_x, stem_top),
            pos2(stem_x + size * 0.3, stem_top + size * 0.25),
            pos2(stem_x + size * 0.28, stem_top + size * 0.52),
        ],
        Stroke::new(size * 0.12, color),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom() -> RadarGeom {
        RadarGeom::fit(Rect::from_min_size(pos2(100.0, 50.0), vec2(460.0, 420.0)))
    }

    #[test]
    fn rings_are_monotonic_and_span_the_radar() {
        let g = geom();
        assert!((g.radius_for(MIN_DIST) - g.inner).abs() < 1e-3);
        assert!((g.radius_for(MAX_DIST) - g.outer).abs() < 1e-3);
        for w in RINGS.windows(2) {
            assert!(g.radius_for(w[0]) < g.radius_for(w[1]));
        }
        assert!(g.head_radius() < g.inner);
        // Out-of-range distances clamp instead of leaving the radar.
        assert_eq!(g.radius_for(100.0), g.outer);
        assert_eq!(g.radius_for(0.0), g.inner);
        assert_eq!(g.radius_for(f32::NAN), g.inner);
    }

    #[test]
    fn screen_round_trip() {
        let g = geom();
        for az_i in -25..=26 {
            let az = wrap_deg(az_i as f32 * 7.0);
            for d in [0.5, 0.7, 1.0, 1.5, 3.0, 7.0, 12.0, 20.0] {
                let p = g.pos_of(az, d);
                let (az2, d2) = g.polar_at(p);
                let daz = wrap_deg(az2 - az).abs();
                assert!(daz < 0.01, "az {az} -> {az2}");
                assert!((d2 - d).abs() / d < 1e-3, "dist {d} -> {d2}");
            }
        }
    }

    #[test]
    fn screen_directions() {
        let g = geom();
        let c = g.center;
        assert!(g.pos_of(0.0, 1.0).y < c.y, "front is up");
        assert!(g.pos_of(90.0, 1.0).x > c.x, "+90 is right");
        assert!(g.pos_of(-90.0, 1.0).x < c.x, "-90 is left");
        assert!(g.pos_of(180.0, 1.0).y > c.y, "behind is down");
        assert_eq!(g.polar_at(c + vec2(0.0, 50.0)).0, 180.0);
        // Points inside the head map to the nearest distance; beyond the radar to the farthest.
        assert_eq!(g.polar_at(c + vec2(1.0, 1.0)).1, MIN_DIST);
        assert!((g.polar_at(c + vec2(5000.0, 0.0)).1 - MAX_DIST).abs() < 1e-3);
    }

    #[test]
    fn wrap_and_snap() {
        assert_eq!(wrap_deg(180.0), 180.0);
        assert_eq!(wrap_deg(-180.0), 180.0);
        assert_eq!(wrap_deg(270.0), -90.0);
        assert_eq!(wrap_deg(-190.0), 170.0);
        assert_eq!(snap_deg(7.4, 15.0), 0.0);
        assert_eq!(snap_deg(8.0, 15.0), 15.0);
        assert_eq!(snap_deg(-173.0, 15.0), 180.0);
        assert_eq!(snap_deg(-97.0, 15.0), -90.0);
    }

    #[test]
    fn names() {
        assert_eq!(direction_name(180.0), "Behind you");
        assert_eq!(direction_name(-170.0), "Behind you");
        assert_eq!(direction_name(150.0), "Behind right");
        assert_eq!(direction_name(-90.0), "Left");
        assert_eq!(direction_name(35.0), "Front right");
        assert_eq!(direction_name(0.0), "In front");
        assert_eq!(format_distance(1.46), "1.5 m");
        assert_eq!(format_distance(15.2), "15 m");
        assert_eq!(format_height(0.3), "0\u{00b0}");
        assert_eq!(format_height(-0.4), "0\u{00b0}");
        assert_eq!(format_height(15.0), "+15\u{00b0}");
        assert_eq!(format_height(-20.0), "-20\u{00b0}");
    }
}
