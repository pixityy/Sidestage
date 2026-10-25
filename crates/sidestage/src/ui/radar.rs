//! The radar: a top-down view of the listener's head with one puck per source (the music behind
//! you, a podcast on the left, voice chat on the right ...), each in its own colour.
//!
//! Angle = direction (0 straight ahead, up on screen; +90 right). Distance from the head is on a
//! log scale so 0.5-2 m (where most placement happens) gets as much room as 2-20 m.
//!
//! Everything below acts on the selected source, whose puck is larger and drawn on top.
//! Clicking another puck selects it; dragging one selects and moves it. Mouse: drag or click
//! empty space to place (Ctrl/Cmd snaps to 15 degrees), Shift-drag up/down for height, scroll
//! for distance, double-click to return to the preset. Keyboard (after clicking or tabbing to
//! it): arrows move the puck, Shift+arrows in small steps, Page Up/Down change height, Home
//! returns to the preset.

use eframe::egui::{
    self, Align2, Color32, CursorIcon, EventFilter, FontId, Key, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui,
    Vec2, vec2,
};
use sidestage_dsp::Settings;
use sidestage_platform::SourceId;

use super::theme::{self, Palette};

pub const MIN_DIST: f32 = 0.5;
pub const MAX_DIST: f32 = 20.0;
pub const RINGS: [f32; 6] = [0.5, 1.0, 2.0, 5.0, 10.0, 20.0];
/// Ctrl-drag snaps the direction to multiples of this.
pub const SNAP_DEG: f32 = 15.0;
/// Half the angle of the "game" wedge in front of the listener.
pub const GAME_WEDGE_HALF_DEG: f32 = 60.0;
/// Ring labels sit along this direction (front-left, where the sources rarely go).
const LABEL_AZ: f32 = -45.0;
/// Radius of the selected source's puck, and of the others'.
pub const SELECTED_R: f32 = 13.0;
pub const PUCK_R: f32 = 10.0;
/// Room around a puck that still counts as hitting it.
const HIT_SLACK: f32 = 4.0;

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

/// One source on the radar.
#[derive(Clone, Debug)]
pub struct Puck {
    pub id: SourceId,
    /// Where it is. For the selected source, [`radar`] uses the settings it is given instead.
    pub settings: Settings,
    pub color: Color32,
    /// Its app's name, shown under the puck; the puck shows the first letter.
    pub label: String,
    /// Smoothed input level 0..1, for a subtle glow.
    pub level: f32,
    /// How far it is turned down right now, 0..1 of its ducking depth.
    pub duck: f32,
    /// Drawn faded: the source is off, or its app is not playing through Sidestage right now.
    pub faded: bool,
    /// Marked with a "!": this source could not be captured.
    pub alert: bool,
}

impl Puck {
    /// The letter on the puck.
    fn initial(&self) -> String {
        self.label.chars().find(|c| c.is_alphanumeric()).map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect())
    }
}

/// What the radar shows besides the selected source's settings.
pub struct RadarInput<'a> {
    pub palette: &'a Palette,
    /// Every source to draw, the selected one included.
    pub pucks: &'a [Puck],
    pub selected: Option<SourceId>,
    /// Dim everything (power off, nothing playing).
    pub inactive: bool,
    /// Settings of the selected source's preset, which double-click and Home return to.
    pub preset: Settings,
}

/// What the user did on the radar.
pub struct RadarOutput {
    /// Marked changed when the selected source's settings changed.
    pub response: egui::Response,
    /// Another source's puck was clicked or grabbed: select it.
    pub select: Option<SourceId>,
}

/// What a drag started on, remembered until it ends.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Grab {
    /// A puck. It follows the pointer at the offset it was grabbed with, so it does not jump.
    Puck(SourceId, Vec2),
    /// Empty space: the selected puck jumps to the pointer and follows it.
    Place,
}

/// The puck under `pos`: the selected one when it covers the point (it is drawn on top),
/// otherwise the nearest one within reach; of pucks at the same spot, the one drawn on top (the
/// later one in `pucks`), which is the one the user sees.
pub fn hit_puck(geom: &RadarGeom, pucks: &[Puck], selected: Option<SourceId>, pos: Pos2) -> Option<SourceId> {
    let distance = |q: &Puck| (geom.pos_of(q.settings.azimuth_deg, q.settings.distance_m) - pos).length();
    if let Some(sel) = pucks.iter().find(|q| Some(q.id) == selected)
        && distance(sel) <= SELECTED_R + HIT_SLACK
    {
        return Some(sel.id);
    }
    pucks
        .iter()
        .rev()
        .filter(|q| Some(q.id) != selected)
        .map(|q| (q.id, distance(q)))
        .filter(|(_, d)| *d <= PUCK_R + HIT_SLACK)
        // The first of equally near ones, so the top one in reversed order.
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id)
}

/// Draw the radar in all of `ui`'s available space and handle input. `settings` are the selected
/// source's (`None` when there is no source); the mouse and keyboard edit them.
pub fn radar(ui: &mut Ui, mut settings: Option<&mut Settings>, input: &RadarInput<'_>) -> RadarOutput {
    let p = input.palette;
    let mut response = ui.allocate_response(ui.available_size(), Sense::click_and_drag());
    // Clipped to the panel rather than to the allocated rect: a puck's glow and ducking pulse
    // may reach into the panel's margin when it sits on the outer ring.
    let painter = ui.painter().clone();
    let rect = response.rect;
    let geom = RadarGeom::fit(rect);
    let before = settings.as_deref().copied();
    let id = response.id;
    let grab_id = id.with("grab");
    let mut select = None;

    // Hit-testing and drawing see the selected source where its settings put it this frame.
    let mut pucks: Vec<Puck> = input.pucks.to_vec();
    let sync_selected = |pucks: &mut [Puck], s: Option<&Settings>| {
        if let (Some(s), Some(sel)) = (s, pucks.iter_mut().find(|q| Some(q.id) == input.selected)) {
            sel.settings = *s;
        }
    };
    sync_selected(&mut pucks, settings.as_deref());

    // ---- input -------------------------------------------------------------------------
    let (modifiers, scroll, time, press_origin, hover) =
        ui.input(|i| (i.modifiers, i.smooth_scroll_delta.y, i.time, i.pointer.press_origin(), i.pointer.hover_pos()));
    let snap = modifiers.command || modifiers.ctrl;
    if response.drag_started() || response.clicked() {
        response.request_focus();
    }
    if response.drag_started()
        && let Some(origin) = press_origin
    {
        let grab = match hit_puck(&geom, &pucks, input.selected, origin) {
            Some(hit) => {
                if Some(hit) != input.selected {
                    select = Some(hit);
                }
                let at = pucks.iter().find(|q| q.id == hit).map_or(origin, |q| puck_pos(&geom, q));
                Grab::Puck(hit, at - origin)
            }
            None => Grab::Place,
        };
        ui.data_mut(|d| d.insert_temp(grab_id, grab));
    }
    let grab = if response.dragged() { ui.data(|d| d.get_temp::<Grab>(grab_id)) } else { None };
    if response.drag_stopped() {
        ui.data_mut(|d| d.remove::<Grab>(grab_id));
    }
    if let Some(s) = settings.as_deref_mut() {
        // A puck grabbed to select it only moves from the next frame on, once it is selected.
        let offset = match grab {
            Some(Grab::Puck(hit, offset)) if Some(hit) == input.selected => Some(offset),
            Some(Grab::Place) => Some(Vec2::ZERO),
            _ => None,
        };
        if let Some(offset) = offset {
            if modifiers.shift {
                // Shift-drag: height.
                s.elevation_deg = (s.elevation_deg - response.drag_delta().y * 0.5).clamp(-40.0, 80.0);
            } else if let Some(pos) = response.interact_pointer_pos() {
                place(s, &geom, pos + offset, snap);
            }
        }
    }
    if response.clicked()
        && let Some(pos) = response.interact_pointer_pos()
    {
        match hit_puck(&geom, &pucks, input.selected, pos) {
            Some(hit) if Some(hit) != input.selected => select = Some(hit),
            // Clicking the selected puck leaves it where it is.
            Some(_) => {}
            None => {
                if let Some(s) = settings.as_deref_mut() {
                    place(s, &geom, pos, snap);
                }
            }
        }
    }
    if let Some(s) = settings.as_deref_mut() {
        if response.hovered() && scroll != 0.0 {
            // Scroll up pushes the source away.
            s.distance_m = (s.distance_m * (scroll * 0.004).exp()).clamp(MIN_DIST, MAX_DIST);
            ui.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
        }
        // The first click of a double-click on another puck selects it; the second resets it.
        // egui counts a quick third click as a triple click; that resets too.
        let mut reset = (response.double_clicked() || response.triple_clicked()) && select.is_none();
        if response.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() },
                );
            });
            reset |= keyboard(ui, &geom, s);
        }
        if reset {
            s.azimuth_deg = input.preset.azimuth_deg;
            s.distance_m = input.preset.distance_m;
            s.elevation_deg = input.preset.elevation_deg;
            s.width_deg = input.preset.width_deg;
        }
        if Some(*s) != before {
            *s = s.sanitized();
        }
    }
    sync_selected(&mut pucks, settings.as_deref());
    let hovered_puck = hover.filter(|_| response.hovered()).and_then(|h| hit_puck(&geom, &pucks, input.selected, h));
    if response.dragged() {
        ui.ctx().set_cursor_icon(if modifiers.shift { CursorIcon::ResizeVertical } else { CursorIcon::Grabbing });
    } else if let Some(h) = hovered_puck {
        ui.ctx().set_cursor_icon(if Some(h) == input.selected { CursorIcon::Grab } else { CursorIcon::PointingHand });
    } else if response.hovered() && settings.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
    }

    // ---- drawing -----------------------------------------------------------------------
    let dim = if input.inactive { 0.45 } else { 1.0 };
    draw_stage(&painter, &geom, p);
    let selected = pucks.iter().find(|q| Some(q.id) == input.selected);
    if let Some(sel) = selected {
        draw_spread(&painter, &geom, p, sel, dim);
    }
    draw_head(&painter, &geom, p);
    let focus = response.has_focus();
    // The others first, so the selected puck is drawn on top.
    for q in pucks.iter().filter(|q| Some(q.id) != input.selected) {
        let look = Look { selected: false, focus: false, hovered: Some(q.id) == hovered_puck, dim };
        draw_puck(&painter, &geom, p, q, look, time);
    }
    if let Some(sel) = selected {
        draw_puck(&painter, &geom, p, sel, Look { selected: true, focus, hovered: false, dim }, time);
    }
    // Names and the height badge go on top of every puck, placed so they cover none of them.
    let badge = selected.and_then(|sel| height_badge(&painter, &geom, rect, p, sel, &pucks));
    let unnamed = draw_labels(&painter, &geom, rect, p, &pucks, input.selected, badge.as_ref().map(|b| b.0), dim);
    if let Some((badge, galley, up)) = badge {
        draw_height_badge(&painter, p, badge, galley, up);
    }
    // Focus outline for keyboard users.
    if focus {
        painter.circle_stroke(geom.center, geom.outer + 6.0, Stroke::new(1.5, p.accent.gamma_multiply(0.5)));
    }
    // A puck whose name found no free spot (pucks crowded together) names itself on hover.
    if let Some(q) = hovered_puck.filter(|h| unnamed.contains(h)).and_then(|h| pucks.iter().find(|q| q.id == h))
        && !response.dragged()
    {
        response = response.on_hover_text_at_pointer(q.label.clone());
    }

    if settings.as_deref().copied() != before {
        response.mark_changed();
    }
    let summary = match (settings.as_deref(), selected) {
        (Some(s), Some(sel)) => format!(
            "{} position: {}, {}, height {}",
            sel.label,
            direction_name(s.azimuth_deg),
            format_distance(s.distance_m),
            format_height(s.elevation_deg)
        ),
        _ => "No source to place".to_owned(),
    };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, &summary));
    RadarOutput { response, select }
}

fn puck_pos(geom: &RadarGeom, q: &Puck) -> Pos2 {
    geom.pos_of(q.settings.azimuth_deg, q.settings.distance_m)
}

/// Move a source to a point on screen, optionally snapping its direction.
fn place(s: &mut Settings, geom: &RadarGeom, pos: Pos2, snap: bool) {
    let (mut az, dist) = geom.polar_at(pos);
    if snap {
        az = snap_deg(az, SNAP_DEG);
    }
    s.azimuth_deg = az;
    s.distance_m = dist;
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

/// The game wedge and the distance rings with their labels.
fn draw_stage(painter: &egui::Painter, geom: &RadarGeom, p: &Palette) {
    let c = geom.center;
    let ring_color = p.text.gamma_multiply(if p.dark { 0.10 } else { 0.14 });
    let label_color = p.text_faint;

    // Game wedge in front: the area the sources stay away from.
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
}

/// The selected source's line from the head and its spread arc (where its left and right
/// virtual speakers are).
fn draw_spread(painter: &egui::Painter, geom: &RadarGeom, p: &Palette, q: &Puck, dim: f32) {
    let c = geom.center;
    let s = &q.settings;
    let alpha = dim * if q.faded { 0.55 } else { 1.0 };
    let pos = puck_pos(geom, q);
    let r = geom.radius_for(s.distance_m);
    painter.line_segment(
        [c + (pos - c).normalized() * geom.head_radius(), pos],
        Stroke::new(1.0, q.color.gamma_multiply(0.4 * alpha)),
    );
    if s.width_deg <= 0.5 {
        return;
    }
    let half = s.width_deg * 0.5;
    let a0 = s.azimuth_deg - half;
    let a1 = s.azimuth_deg + half;
    let n = ((s.width_deg / 3.0).ceil() as usize).max(2);
    let pts: Vec<Pos2> = (0..=n).map(|i| c + dir(a0 + (a1 - a0) * i as f32 / n as f32) * r).collect();
    painter.add(Shape::line(pts, Stroke::new(3.0, q.color.gamma_multiply(0.45 * alpha))));
    // The source's left channel stays on the listener's left.
    let (left, right) = if dir(a0).x <= dir(a1).x { (a0, a1) } else { (a1, a0) };
    for (a, label) in [(left, "L"), (right, "R")] {
        painter.circle(c + dir(a) * r, 4.5, p.stage, Stroke::new(1.5, q.color.gamma_multiply(0.9 * alpha)));
        let out = c + dir(a) * (r + 13.0);
        painter.text(out, Align2::CENTER_CENTER, label, FontId::proportional(10.5), p.text_dim.gamma_multiply(dim));
    }
}

/// The listener: ears, nose, face.
fn draw_head(painter: &egui::Painter, geom: &RadarGeom, p: &Palette) {
    let c = geom.center;
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
}

#[derive(Clone, Copy)]
struct Look {
    selected: bool,
    /// The radar has keyboard focus (drawn on the selected puck).
    focus: bool,
    hovered: bool,
    /// The whole radar's brightness.
    dim: f32,
}

/// One puck: a glow with its level, a pulse while it is turned down, its initial and, when it
/// could not be captured, a "!" mark. Its name is drawn by [`draw_labels`].
fn draw_puck(painter: &egui::Painter, geom: &RadarGeom, p: &Palette, q: &Puck, look: Look, time: f64) {
    let pos = puck_pos(geom, q);
    let dim = look.dim;
    let r = if look.selected { SELECTED_R } else { PUCK_R };
    let faded = if q.faded { 0.5 } else { 1.0 };
    let emphasis = if look.selected || look.hovered { 1.0 } else { 0.85 };
    let alpha = dim * faded * emphasis * (1.0 - 0.35 * q.duck);
    let glow = (q.level.clamp(0.0, 1.0) * if look.selected { 10.0 } else { 7.0 } + 3.0) * dim * faded;
    painter.circle_filled(pos, r + glow, q.color.gamma_multiply(0.12 * alpha));
    painter.circle_filled(pos, r + glow * 0.5, q.color.gamma_multiply(0.18 * alpha));
    if q.duck > 0.02 {
        let phase = (time as f32 * 2.2).fract();
        let ring_r = r + 4.0 + phase * if look.selected { 16.0 } else { 12.0 };
        painter.circle_stroke(pos, ring_r, Stroke::new(2.0, p.warn.gamma_multiply(q.duck * (1.0 - phase) * dim)));
    }
    let stroke = if look.focus {
        Stroke::new(2.5, p.text)
    } else if look.selected {
        Stroke::new(2.0, p.text.gamma_multiply(0.85 * dim))
    } else if look.hovered {
        Stroke::new(1.5, p.text.gamma_multiply(0.7 * dim))
    } else {
        Stroke::new(1.0, p.stage)
    };
    // Opaque, blended towards the stage, so the initial can be picked to stand out on what is
    // actually drawn (a dimmed puck is much lighter in the light theme, darker in the dark one).
    let fill = p.stage.lerp_to_gamma(q.color, alpha.clamp(0.0, 1.0));
    painter.circle(pos, r, fill, stroke);
    let ink = theme::text_on(fill);
    let initial_size = if look.selected { 13.0 } else { 11.0 };
    painter.text(pos + vec2(0.0, 0.5), Align2::CENTER_CENTER, q.initial(), FontId::proportional(initial_size), ink);

    if q.alert {
        let c = pos + vec2(r * 0.75, -r * 0.75);
        painter.circle(c, 6.0, p.err.gamma_multiply(dim), Stroke::new(1.5, p.stage));
        painter.text(c + vec2(0.0, 0.5), Align2::CENTER_CENTER, "!", FontId::proportional(9.5), p.stage);
    }
}

/// Where a puck's name goes, best first: below it, above it, right of it, left of it.
fn label_spots(pos: Pos2, r: f32, size: Vec2) -> [Pos2; 4] {
    [
        pos + vec2(-size.x / 2.0, r + 4.0),
        pos + vec2(-size.x / 2.0, -r - 4.0 - size.y),
        pos + vec2(r + 6.0, -size.y / 2.0),
        pos + vec2(-r - 6.0 - size.x, -size.y / 2.0),
    ]
}

/// Every puck's name, each at the first spot (see [`label_spots`]) where it covers no puck, no
/// other name, the listener's head and the height badge, the selected source's first. A name
/// that fits nowhere is left out (the radar shows it on hover); the selected source's is always
/// drawn. Returns the sources left unnamed.
#[expect(clippy::too_many_arguments)]
fn draw_labels(
    painter: &egui::Painter,
    geom: &RadarGeom,
    rect: Rect,
    p: &Palette,
    pucks: &[Puck],
    selected: Option<SourceId>,
    badge: Option<Rect>,
    dim: f32,
) -> Vec<SourceId> {
    let radius = |q: &Puck| if Some(q.id) == selected { SELECTED_R } else { PUCK_R };
    let mut taken: Vec<Rect> = pucks
        .iter()
        .map(|q| Rect::from_center_size(puck_pos(geom, q), Vec2::splat(2.0 * radius(q) + 2.0)))
        .chain(std::iter::once(Rect::from_center_size(geom.center, Vec2::splat(geom.head_radius() * 2.4))))
        .chain(badge)
        .collect();
    let mut unnamed = Vec::new();
    let order = pucks.iter().filter(|q| Some(q.id) == selected).chain(pucks.iter().filter(|q| Some(q.id) != selected));
    for q in order {
        let is_selected = Some(q.id) == selected;
        let font = FontId::proportional(if is_selected { 11.5 } else { 10.5 });
        let color = if is_selected { p.text.gamma_multiply(dim) } else { p.text_dim.gamma_multiply(dim) };
        let name: String = if q.label.chars().count() > 16 {
            q.label.chars().take(15).chain(std::iter::once('\u{2026}')).collect()
        } else {
            q.label.clone()
        };
        let galley = painter.layout_no_wrap(name, font, color);
        let size = galley.size();
        let inside = |at: Pos2| {
            let x = at.x.clamp(rect.left(), (rect.right() - size.x).max(rect.left()));
            let r = Rect::from_min_size(egui::pos2(x, at.y), size).expand2(vec2(3.0, 0.0));
            (r.top() >= rect.top() && r.bottom() <= rect.bottom()).then_some(r)
        };
        let spots: Vec<Rect> = label_spots(puck_pos(geom, q), radius(q), size).into_iter().filter_map(inside).collect();
        let free = spots.iter().find(|s| !taken.iter().any(|t| t.intersects(**s))).copied();
        let Some(spot) = free.or_else(|| spots.first().copied().filter(|_| is_selected)) else {
            unnamed.push(q.id);
            continue;
        };
        taken.push(spot);
        painter.rect_filled(spot, 3, p.stage.gamma_multiply(0.75));
        painter.galley(spot.min + vec2(3.0, 0.0), galley, color);
    }
    unnamed
}

/// The selected puck's height badge (rectangle, text, pointing up), when it is above or below
/// ear level: up-right of the puck, or the first of up-left, down-right and down-left that stays
/// on the radar and off the other pucks.
fn height_badge(
    painter: &egui::Painter,
    geom: &RadarGeom,
    rect: Rect,
    p: &Palette,
    q: &Puck,
    pucks: &[Puck],
) -> Option<(Rect, std::sync::Arc<egui::Galley>, bool)> {
    let s = &q.settings;
    if s.elevation_deg.round() == 0.0 {
        return None;
    }
    let pos = puck_pos(geom, q);
    let r = SELECTED_R;
    let galley = painter.layout_no_wrap(format_height(s.elevation_deg), FontId::proportional(11.0), p.text);
    let size = vec2(galley.size().x + 18.0, 18.0);
    let gap = r + 8.0;
    let spots =
        [vec2(gap, -r - 11.0), vec2(-gap - size.x, -r - 11.0), vec2(gap, r - 7.0), vec2(-gap - size.x, r - 7.0)]
            .map(|offset| Rect::from_min_size(pos + offset, size));
    let on_radar: Vec<Rect> = spots.into_iter().filter(|b| rect.contains_rect(*b)).collect();
    let others: Vec<Rect> = pucks
        .iter()
        .filter(|o| o.id != q.id)
        .map(|o| Rect::from_center_size(puck_pos(geom, o), Vec2::splat(2.0 * PUCK_R + 2.0)))
        .collect();
    let badge = on_radar
        .iter()
        .find(|b| !others.iter().any(|o| o.intersects(**b)))
        .or(on_radar.first())
        .copied()
        .unwrap_or(spots[0]);
    Some((badge, galley, s.elevation_deg > 0.0))
}

fn draw_height_badge(
    painter: &egui::Painter,
    p: &Palette,
    badge: Rect,
    galley: std::sync::Arc<egui::Galley>,
    up: bool,
) {
    painter.rect(badge, 9, p.surface, Stroke::new(1.0, p.border), StrokeKind::Inside);
    let tri_c = badge.left_center() + vec2(8.0, 0.0);
    let sign = if up { -1.0 } else { 1.0 };
    painter.add(Shape::convex_polygon(
        vec![tri_c + vec2(-3.5, -sign * 2.5), tri_c + vec2(3.5, -sign * 2.5), tri_c + vec2(0.0, sign * 3.5)],
        p.accent_hi,
        Stroke::NONE,
    ));
    painter.galley(badge.left_center() + vec2(14.0, -galley.size().y / 2.0), galley, p.text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::pos2;

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

    fn puck(id: SourceId, azimuth_deg: f32, distance_m: f32) -> Puck {
        Puck {
            id,
            settings: Settings { azimuth_deg, distance_m, ..Settings::default() },
            color: Color32::WHITE,
            label: format!("App {id}"),
            level: 0.0,
            duck: 0.0,
            faded: false,
            alert: false,
        }
    }

    #[test]
    fn hit_testing_finds_the_right_puck() {
        let g = geom();
        let at = |q: &Puck| puck_pos(&g, q);
        let pucks = [puck(1, 180.0, 1.5), puck(2, -90.0, 1.5), puck(3, 75.0, 1.0)];
        let [behind, left, right] = &pucks;
        assert_eq!(hit_puck(&g, &pucks, Some(1), at(left)), Some(2));
        assert_eq!(hit_puck(&g, &pucks, Some(1), at(right) + vec2(PUCK_R, 0.0)), Some(3), "the rim counts");
        assert_eq!(hit_puck(&g, &pucks, Some(1), at(right) + vec2(PUCK_R + HIT_SLACK + 1.0, 0.0)), None);
        let rim = at(behind) + vec2(0.0, SELECTED_R + 2.0);
        assert_eq!(hit_puck(&g, &pucks, Some(1), rim), Some(1), "the selected puck is bigger");
        assert_eq!(hit_puck(&g, &pucks, Some(2), rim), None, "the same spot misses it when it is not selected");
        assert_eq!(hit_puck(&g, &pucks, Some(1), g.center), None, "the head is not a puck");
        assert_eq!(hit_puck(&g, &[], None, at(left)), None);

        // Overlapping pucks: the selected one is on top, otherwise the nearest one wins.
        let close = [puck(1, 90.0, 2.0), puck(2, 92.0, 2.0), puck(3, 94.0, 2.0)];
        let [a, b, c] = &close;
        assert!((at(a) - at(b)).length() < PUCK_R, "the test needs overlapping pucks");
        assert_eq!(hit_puck(&g, &close, None, at(a)), Some(1));
        assert_eq!(hit_puck(&g, &close, None, at(b)), Some(2));
        assert_eq!(hit_puck(&g, &close, Some(3), at(b)), Some(3), "the selected puck covers its neighbours");
        let beyond = at(a) - (at(c) - at(a)) * 2.0;
        assert!((beyond - at(c)).length() > SELECTED_R + HIT_SLACK);
        assert_eq!(hit_puck(&g, &close, Some(3), beyond), Some(1), "out of the selected puck's reach");

        // Pucks on the very same spot: the one drawn last (on top) is the one clicked.
        let stacked = [puck(1, 180.0, 1.5), puck(2, -75.0, 1.0), puck(3, -75.0, 1.0)];
        assert_eq!(hit_puck(&g, &stacked, Some(1), at(&stacked[1])), Some(3));
        assert_eq!(hit_puck(&g, &stacked, Some(3), at(&stacked[1])), Some(3));
        assert_eq!(hit_puck(&g, &stacked, Some(2), at(&stacked[1])), Some(2));
    }

    #[test]
    fn height_badge_stays_on_the_radar_and_off_other_pucks() {
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(460.0, 420.0));
        let g = RadarGeom::fit(rect);
        let raised =
            |az, d| Puck { settings: Settings { elevation_deg: 30.0, ..puck(1, az, d).settings }, ..puck(1, az, d) };
        let mut badges = Vec::new();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter().clone();
            let badge = |q: &Puck, all: &[Puck]| height_badge(&painter, &g, rect, &theme::DARK, q, all).map(|b| b.0);
            // Free: up-right of the puck.
            let alone = raised(-90.0, 1.5);
            badges.push((puck_pos(&g, &alone), badge(&alone, std::slice::from_ref(&alone))));
            // At the right edge, with a neighbour up-left of it: down-left.
            let edge = raised(90.0, 20.0);
            let at = puck_pos(&g, &edge);
            let (az, d) = g.polar_at(at + vec2(-30.0, -24.0));
            let pucks = [edge.clone(), puck(2, az, d)];
            badges.push((at, badge(&edge, &pucks)));
            assert_eq!(badge(&puck(1, 0.0, 1.0), &pucks), None, "no badge at ear level");
        });
        output.textures_delta.clear();
        let [(p0, Some(b0)), (p1, Some(b1))] = badges.as_slice() else { panic!("{badges:?}") };
        assert!(b0.left() > p0.x && b0.bottom() < p0.y, "up-right: {b0:?} of {p0:?}");
        assert!(rect.contains_rect(*b1) && b1.right() < p1.x && b1.top() > p1.y - SELECTED_R, "{b1:?} of {p1:?}");
    }

    #[test]
    fn crowded_names_move_aside_or_wait_for_hover() {
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(460.0, 420.0));
        let g = RadarGeom::fit(rect);
        let mut unnamed = Vec::new();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.painter().clone();
            // Two apart, three stacked on one spot.
            let pucks = [
                puck(1, 180.0, 1.5),
                puck(2, 90.0, 5.0),
                puck(3, -75.0, 1.0),
                puck(4, -75.0, 1.0),
                puck(5, -75.0, 1.0),
            ];
            unnamed = draw_labels(&painter, &g, rect, &theme::DARK, &pucks, Some(5), None, 1.0);
        });
        // Nothing is rendered, so the font texture is never uploaded.
        output.textures_delta.clear();
        // The selected one and the free ones are named; the rest of the stack has no free spot
        // and is named on hover.
        assert!(!unnamed.contains(&5) && !unnamed.contains(&1) && !unnamed.contains(&2), "{unnamed:?}");
        assert!(!unnamed.is_empty() && unnamed.iter().all(|id| [3, 4].contains(id)), "{unnamed:?}");
    }

    /// The radar in a headless egui context, fed synthetic pointer events frame by frame. Like
    /// the app, it applies a selection the radar asks for before the next frame.
    struct Harness {
        ctx: egui::Context,
        time: f64,
        sources: Vec<(SourceId, Settings)>,
        selected: SourceId,
        rect: Rect,
        /// Every source the radar asked to select.
        selections: Vec<SourceId>,
    }

    impl Harness {
        fn new(sources: Vec<(SourceId, Settings)>, selected: SourceId) -> Self {
            let mut h = Self {
                ctx: egui::Context::default(),
                time: 0.0,
                sources,
                selected,
                rect: Rect::NOTHING,
                selections: Vec::new(),
            };
            h.frame(Vec::new());
            h
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            self.time += 1.0 / 60.0;
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(640.0, 520.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let pucks: Vec<Puck> =
                self.sources.iter().map(|(id, s)| Puck { settings: *s, ..puck(*id, 0.0, 1.0) }).collect();
            let selected = self.selected;
            let (sources, rect, picked) = (&mut self.sources, &mut self.rect, &mut None);
            let mut output = self.ctx.run_ui(raw, |ui| {
                let settings = sources.iter_mut().find(|(id, _)| *id == selected).map(|(_, s)| s);
                let input = RadarInput {
                    palette: &theme::DARK,
                    pucks: &pucks,
                    selected: Some(selected),
                    inactive: false,
                    preset: Settings::default(),
                };
                let out = radar(ui, settings, &input);
                *rect = out.response.rect;
                if out.select.is_some() {
                    *picked = out.select;
                }
            });
            // Nothing is rendered, so the font texture is never uploaded.
            output.textures_delta.clear();
            if let Some(id) = *picked {
                self.selections.push(id);
                self.selected = id;
            }
        }

        fn geom(&self) -> RadarGeom {
            RadarGeom::fit(self.rect)
        }

        fn settings(&self, id: SourceId) -> Settings {
            self.sources.iter().find(|(i, _)| *i == id).map(|(_, s)| *s).unwrap()
        }

        fn at(&self, id: SourceId) -> Pos2 {
            let s = self.settings(id);
            self.geom().pos_of(s.azimuth_deg, s.distance_m)
        }

        fn button(pos: Pos2, pressed: bool) -> egui::Event {
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }
        }

        fn click(&mut self, pos: Pos2) {
            self.frame(vec![egui::Event::PointerMoved(pos), Self::button(pos, true)]);
            self.frame(vec![Self::button(pos, false)]);
        }

        /// Press at `from`, move to `to` in small steps, release.
        fn drag(&mut self, from: Pos2, to: Pos2) {
            self.frame(vec![egui::Event::PointerMoved(from), Self::button(from, true)]);
            for i in 1..=12 {
                self.frame(vec![egui::Event::PointerMoved(from + (to - from) * (i as f32 / 12.0))]);
            }
            self.frame(vec![Self::button(to, false)]);
        }
    }

    #[test]
    fn clicking_and_dragging_select_and_move_the_right_source() {
        let behind = Settings::default();
        let voice = Settings { azimuth_deg: -90.0, distance_m: 1.5, ..Settings::default() };
        let mut h = Harness::new(vec![(1, behind), (2, voice)], 1);

        // Clicking another source's puck selects it and moves nothing.
        let at2 = h.at(2);
        h.click(at2 + vec2(3.0, -2.0));
        assert_eq!((h.selections.as_slice(), h.selected), (&[2][..], 2));
        assert_eq!((h.settings(1), h.settings(2)), (behind, voice));

        // Clicking empty space places the selected source there.
        let target = h.geom().pos_of(-120.0, 3.0);
        h.click(target);
        let moved = h.settings(2);
        assert!((wrap_deg(moved.azimuth_deg + 120.0)).abs() < 0.5, "{}", moved.azimuth_deg);
        assert!((moved.distance_m - 3.0).abs() / 3.0 < 0.02, "{}", moved.distance_m);
        assert_eq!(h.settings(1), behind, "the other source stays put");
        assert_eq!(h.selections, [2], "placing selects nothing");

        // Dragging an unselected puck selects it and moves it by the drag, without jumping to
        // the pointer.
        let start = h.at(1);
        let grab = start + vec2(4.0, 3.0);
        h.drag(grab, grab + vec2(90.0, -30.0));
        assert_eq!(h.selected, 1);
        let end = h.at(1);
        assert!((end - (start + vec2(90.0, -30.0))).length() < 1.0, "{start:?} -> {end:?}");
        assert_eq!(h.settings(2), moved, "dragging one puck leaves the others alone");

        // A double-click on the selected puck returns it to its preset.
        h.time += 1.0;
        let at1 = h.at(1);
        h.click(at1);
        h.click(at1);
        let reset = h.settings(1);
        assert_eq!((reset.azimuth_deg, reset.distance_m), (behind.azimuth_deg, behind.distance_m));

        // A double-click on another puck selects it with the first click and resets it with
        // the second.
        h.time += 1.0;
        let at2 = h.at(2);
        h.click(at2);
        h.click(at2);
        assert_eq!(h.selected, 2);
        assert_eq!(h.settings(2).azimuth_deg, behind.azimuth_deg);
        assert_eq!(h.settings(1), reset);
    }

    #[test]
    fn puck_initials() {
        let mut q = puck(1, 0.0, 1.0);
        assert_eq!(q.initial(), "A");
        q.label = "  ".into();
        assert_eq!(q.initial(), "?");
        q.label = "\u{00e9}cho".into();
        assert_eq!(q.initial(), "\u{00c9}");
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
