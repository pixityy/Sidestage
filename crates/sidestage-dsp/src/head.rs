//! Spherical-head model (Brown & Duda 1998, IEEE TSAP 6(5) 476-488): per-ear Woodworth delay
//! and a one-pole/one-zero head-shadow filter. Needs no HRTF dataset, so nothing to license.
//!
//! Coordinates: x forward, y right, z up; azimuth 0 = ahead, +90 = right (the radar's view).

use std::f32::consts::{FRAC_PI_2, PI};

pub const HEAD_RADIUS_M: f32 = 0.0875;
pub const SPEED_OF_SOUND: f32 = 343.0;
/// Brown & Duda eq. 5 constants: deepest shadow (alpha 0.1, -20 dB treble) 150° off the ear axis.
const ALPHA_MIN: f32 = 0.1;
const THETA_MIN: f32 = 150.0 * PI / 180.0;
/// Longest per-ear delay the model produces: (a/c)(1 + π/2) ≈ 0.656 ms.
pub const MAX_ITD_S: f32 = HEAD_RADIUS_M / SPEED_OF_SOUND * (1.0 + FRAC_PI_2);

/// Unit vector toward (azimuth, elevation) in degrees.
pub fn direction(az_deg: f32, el_deg: f32) -> [f32; 3] {
    let (sa, ca) = az_deg.to_radians().sin_cos();
    let (se, ce) = el_deg.to_radians().sin_cos();
    [ce * ca, ce * sa, se]
}

/// Angle (radians) between the source direction and each ear's axis, `[left, right]`.
///
/// The ears sit at ±90°. Brown & Duda's KEMAR fit puts them at ±100°, but then a source behind
/// gets ~4.4 dB more treble than one in front, which works against the rear cues; at ±90° the
/// model is front/back symmetric and the explicit rear EQ does the work.
pub fn incidence(dir: [f32; 3]) -> [f32; 2] {
    let y = dir[1].clamp(-1.0, 1.0);
    [(-y).acos(), y.acos()]
}

/// Woodworth & Schlosberg (Brown & Duda eq. 2) shifted by a/c so it is causal: 0 on the ear
/// axis, [`MAX_ITD_S`] diametrically opposite.
pub fn woodworth_delay_s(theta: f32) -> f32 {
    let t = theta.abs().min(PI);
    let ac = HEAD_RADIUS_M / SPEED_OF_SOUND;
    if t < FRAC_PI_2 { ac * (1.0 - t.cos()) } else { ac * (t - FRAC_PI_2 + 1.0) }
}

/// Head-shadow zero (Brown & Duda eq. 5): 2 (+6 dB treble) facing the ear, 0.1 (-20 dB) at 150°,
/// back up to ~0.28 at 180° (the "bright spot" behind a sphere).
pub fn shadow_alpha(theta: f32) -> f32 {
    let t = theta.abs().min(PI);
    (1.0 + 0.5 * ALPHA_MIN) + (1.0 - 0.5 * ALPHA_MIN) * (t / THETA_MIN * PI).cos()
}

/// Azimuths of the virtual speakers playing the music's `[left, right]` channels.
///
/// The music's left channel must stay on the listener's left. In front that means
/// `az - width/2`, behind it means `az + width/2`. Swapping the two at the sides would jump each
/// channel between a front and a rear position, so instead the spread follows `2·cos(az)`
/// clamped to ±1: full width within 60° of straight ahead or behind, narrowing continuously to a
/// point source exactly at the sides, where left/right order has no meaning anyway.
pub fn speaker_azimuths(az_deg: f32, width_deg: f32) -> [f32; 2] {
    let k = (2.0 * az_deg.to_radians().cos()).clamp(-1.0, 1.0);
    let half = 0.5 * width_deg * k;
    [az_deg - half, az_deg + half]
}

/// Brown & Duda eq. 3, H = (1 + jαω/2ω0)/(1 + jω/2ω0), rewritten as H = α + (1 - α)·LP with a
/// fixed one-pole low-pass at c/(πa) ≈ 1.25 kHz. Only α depends on direction, so it can change
/// every sample without clicks (the LP state never sees α).
#[derive(Clone, Copy, Debug)]
pub struct HeadShadow {
    b: f32,
    a1: f32,
    x1: f32,
    y1: f32,
}

impl HeadShadow {
    pub fn new(fs: f32) -> Self {
        let p = 2.0 * SPEED_OF_SOUND / HEAD_RADIUS_M; // pole, rad/s
        let k = p / (p / (2.0 * fs)).tan(); // prewarped bilinear transform
        Self { b: p / (k + p), a1: (p - k) / (k + p), x1: 0.0, y1: 0.0 }
    }

    #[inline]
    pub fn process(&mut self, x: f32, alpha: f32) -> f32 {
        let lp = self.b * (x + self.x1) - self.a1 * self.y1;
        self.x1 = x;
        self.y1 = lp;
        alpha * x + (1.0 - alpha) * lp
    }

    pub fn reset(&mut self) {
        self.x1 = 0.0;
        self.y1 = 0.0;
    }

    pub fn flush(&mut self) {
        crate::smoothing::flush(&mut self.x1);
        crate::smoothing::flush(&mut self.y1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_values() {
        // Ear axis: no delay, full treble boost; opposite: maximal delay.
        assert_eq!(woodworth_delay_s(0.0), 0.0);
        assert!((woodworth_delay_s(PI) - 0.000_655_8).abs() < 1e-6);
        assert!((shadow_alpha(0.0) - 2.0).abs() < 1e-6);
        assert!((shadow_alpha(THETA_MIN) - 0.1).abs() < 1e-5);
        // Front and back are symmetric with ears at ±90°.
        let f = incidence(direction(0.0, 0.0));
        let b = incidence(direction(180.0, 0.0));
        assert!((f[0] - b[0]).abs() < 1e-5 && (f[1] - b[1]).abs() < 1e-5);
        // A source on the right is nearer the right ear.
        let r = incidence(direction(90.0, 0.0));
        assert!(r[1] < 1e-3 && (r[0] - PI).abs() < 1e-3);
    }

    #[test]
    fn music_left_stays_left() {
        for az in (-180..=180).step_by(5) {
            let [l, r] = speaker_azimuths(az as f32, 90.0);
            let yl = direction(l, 0.0)[1];
            let yr = direction(r, 0.0)[1];
            assert!(yl <= yr + 1e-5, "az {az}: left speaker at {l}, right at {r}");
        }
    }
}
