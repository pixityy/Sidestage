//! Sidechain ducker: turns the music down while the game is loud.
//!
//! Only a level arrives (the backend's recent linear peak of "everything except the music",
//! sampled once per `process` call), so there is no K-weighting prefilter: the level drives a
//! 2:1 soft-knee gain computer directly. Attack, hold and release then smooth the *reduction*
//! in dB rather than the level, which makes the release time independent of how loud the game
//! was (a level envelope released toward silence would recover faster after loud bursts).

use crate::params::Ducking;
use crate::smoothing::gain_to_db;

/// Ratio above the threshold, and the soft knee around it.
const RATIO: f32 = 2.0;
const KNEE_DB: f32 = 6.0;

#[derive(Clone, Copy, Debug, Default)]
pub struct Ducker {
    reduction_db: f32,
    hold_left_s: f32,
}

/// Wanted reduction in dB (positive) for a sidechain level, before smoothing.
fn static_curve(level_db: f32, d: &Ducking) -> f32 {
    let over = level_db - d.threshold_db;
    let slope = 1.0 - 1.0 / RATIO;
    let r = if over <= -0.5 * KNEE_DB {
        0.0
    } else if over < 0.5 * KNEE_DB {
        slope * (over + 0.5 * KNEE_DB).powi(2) / (2.0 * KNEE_DB)
    } else {
        slope * over
    };
    r.min(d.depth_db).max(0.0)
}

impl Ducker {
    /// Current gain reduction in dB (positive).
    pub fn reduction_db(&self) -> f32 {
        self.reduction_db
    }

    /// Advances by `dt_s` seconds with sidechain peak `level` (linear) and returns the
    /// reduction to apply. Disabled ducking releases to 0 at the release rate, so switching it
    /// off mid-duck does not jump.
    pub fn update(&mut self, level: f32, d: &Ducking, dt_s: f32) -> f32 {
        let target = if d.enabled { static_curve(gain_to_db(level), d) } else { 0.0 };
        let coef = |ms: f32| 1.0 - (-dt_s / (ms.max(0.1) * 1e-3)).exp();
        if target >= self.reduction_db && target > 0.0 {
            // Still at least as loud as what we are ducking for: attack and re-arm the hold.
            self.reduction_db += (target - self.reduction_db) * coef(d.attack_ms);
            self.hold_left_s = d.hold_ms * 1e-3;
        } else if self.hold_left_s > 0.0 {
            self.hold_left_s -= dt_s;
        } else {
            self.reduction_db += (target - self.reduction_db) * coef(d.release_ms);
        }
        if (self.reduction_db - target).abs() < 1e-4 || !self.reduction_db.is_finite() {
            self.reduction_db = if target.is_finite() { target } else { 0.0 };
        }
        self.reduction_db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DuckMode;

    #[test]
    fn curve_is_two_to_one_with_a_soft_knee() {
        let d = DuckMode::Standard.ducking();
        assert_eq!(static_curve(-60.0, &d), 0.0);
        assert!((static_curve(d.threshold_db + 10.0, &d) - 5.0).abs() < 1e-4);
        assert!(static_curve(d.threshold_db, &d) > 0.0 && static_curve(d.threshold_db, &d) < 1.0);
        assert_eq!(static_curve(0.0, &d), d.depth_db);
    }

    #[test]
    fn attack_hold_release() {
        let d = DuckMode::Standard.ducking();
        let dt = 0.001;
        let mut k = Ducker::default();
        for _ in 0..(5.0 * d.attack_ms) as usize {
            k.update(0.3, &d, dt);
        }
        assert!(k.reduction_db() > d.depth_db - 0.1, "{}", k.reduction_db());
        for _ in 0..(d.hold_ms * 0.9) as usize {
            k.update(0.0, &d, dt);
        }
        assert!(k.reduction_db() > d.depth_db - 0.1, "hold: {}", k.reduction_db());
        for _ in 0..(d.hold_ms * 0.1 + 5.0 * d.release_ms) as usize + 2 {
            k.update(0.0, &d, dt);
        }
        assert!(k.reduction_db() < 0.1, "release: {}", k.reduction_db());
    }
}
