//! Parameter smoothing and small numeric helpers.
//!
//! Smoothing happens on two levels so that moving a control never produces steps (zipper noise):
//! user-facing parameters follow a one-pole smoother evaluated once per sub-block, and everything
//! derived from them (gains, delays, filter coefficients) moves linearly from sample to sample
//! within the sub-block. The result is continuous at every sample, and the expensive maths
//! (trigonometry, `tan`, `exp`) runs once per sub-block instead of once per sample.

/// Largest number of frames rendered with one set of coefficient targets (1.3 ms at 48 kHz).
pub const SUB_BLOCK: usize = 64;

/// Recursive states smaller than this are flushed to zero once per sub-block. -300 dBFS is far
/// below audibility, and flushing that early means even a heavily damped filter cannot decay
/// into the slow denormal range (< 1.2e-38) within one sub-block. Explicit flushing is used
/// because setting FTZ/DAZ in MXCSR is undefined behaviour in Rust.
pub const FLUSH_LEVEL: f32 = 1e-15;

#[inline]
pub fn flush(x: &mut f32) {
    if x.abs() < FLUSH_LEVEL {
        *x = 0.0;
    }
}

#[inline]
pub fn db_to_gain(db: f32) -> f32 {
    (db * (std::f32::consts::LN_10 / 20.0)).exp()
}

#[inline]
pub fn gain_to_db(gain: f32) -> f32 {
    20.0 * gain.max(1e-12).log10()
}

/// Fraction of the remaining distance a one-pole smoother with time constant `tau_s` covers in
/// `samples` samples.
#[inline]
pub fn one_pole(tau_s: f32, samples: usize, fs: f32) -> f32 {
    if tau_s <= 0.0 {
        return 1.0;
    }
    1.0 - (-(samples as f32) / (tau_s * fs)).exp()
}

/// Moves `value` toward `target` by the fraction `coef`, landing exactly on the target once it
/// is within `eps`. Settled parameters then compare equal, so derived coefficients are not
/// recomputed while nothing changes.
#[inline]
pub fn approach(value: &mut f32, target: f32, coef: f32, eps: f32) {
    let d = target - *value;
    if d.abs() <= eps {
        *value = target;
    } else {
        *value += coef * d;
    }
}

/// Wraps an angle difference in degrees into [-180, 180).
#[inline]
pub fn wrap_deg(d: f32) -> f32 {
    (d + 180.0).rem_euclid(360.0) - 180.0
}

/// Raised-cosine (smoothstep) shape for crossfades: zero slope at both ends.
#[inline]
pub fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A value that moves linearly to a new target over one sub-block, one step per sample.
///
/// Usage per sub-block: [`Ramp::set`] with the sub-block length, [`Ramp::next`] once per
/// sample, then [`Ramp::finish`] so rounding never accumulates across sub-blocks.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ramp {
    value: f32,
    target: f32,
    step: f32,
}

impl Ramp {
    pub const fn new(value: f32) -> Self {
        Self { value, target: value, step: 0.0 }
    }

    /// Value reached at the end of the last sub-block.
    #[inline]
    pub fn value(&self) -> f32 {
        self.value
    }

    #[inline]
    pub fn set(&mut self, target: f32, n: usize) {
        self.target = target;
        self.step = if n == 0 { 0.0 } else { (target - self.value) / n as f32 };
    }

    #[inline]
    pub fn snap(&mut self, value: f32) {
        *self = Self::new(value);
    }

    #[inline]
    pub fn next(&mut self) -> f32 {
        self.value += self.step;
        self.value
    }

    #[inline]
    pub fn finish(&mut self) {
        self.value = self.target;
        self.step = 0.0;
    }

    #[inline]
    pub fn is_moving(&self) -> bool {
        self.step != 0.0
    }
}

/// A gain that restarts at 0 and rises along a [`smoothstep`] to 1 over a fixed time. Used
/// wherever audio memory starts empty while the music is already playing, so the signal it
/// holds begins smoothly instead of abruptly mid-waveform.
#[derive(Clone, Copy, Debug)]
pub struct FadeIn {
    pos: f32,
}

impl FadeIn {
    pub const OPEN: Self = Self { pos: 1.0 };
    pub const CLOSED: Self = Self { pos: 0.0 };

    pub fn restart(&mut self) {
        self.pos = 0.0;
    }

    /// Fills `g` with the gains for the next `g.len()` samples of a fade lasting `samples` in
    /// total; returns false (and leaves `g` alone) once fully open, so the common case costs
    /// nothing.
    #[inline]
    pub fn gains(&mut self, g: &mut [f32], samples: f32) -> bool {
        if self.pos >= 1.0 {
            return false;
        }
        let step = 1.0 / samples.max(1.0);
        for v in g.iter_mut() {
            self.pos = (self.pos + step).min(1.0);
            *v = smoothstep(self.pos);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_in_rises_smoothly_and_then_costs_nothing() {
        let mut f = FadeIn::CLOSED;
        let mut g = [0.0f32; 8];
        assert!(f.gains(&mut g, 16.0));
        assert!(g.windows(2).all(|w| w[1] > w[0]) && g[0] > 0.0 && g[7] < 0.6);
        assert!(f.gains(&mut g, 16.0));
        assert_eq!(g[7], 1.0);
        g.fill(-1.0);
        assert!(!f.gains(&mut g, 16.0));
        assert_eq!(g, [-1.0; 8]);
        f.restart();
        assert!(f.gains(&mut g, 16.0) && g[0] < 0.05);
    }

    #[test]
    fn ramp_lands_on_target() {
        let mut r = Ramp::new(0.0);
        r.set(1.0, 3);
        let v: Vec<f32> = (0..3).map(|_| r.next()).collect();
        r.finish();
        assert!((v[0] - 1.0 / 3.0).abs() < 1e-6 && (v[2] - 1.0).abs() < 1e-6);
        assert_eq!(r.value(), 1.0);
        assert!(!r.is_moving());
    }

    #[test]
    fn wrap_and_approach() {
        assert_eq!(wrap_deg(190.0), -170.0);
        assert_eq!(wrap_deg(-190.0), 170.0);
        let mut v = 0.0;
        for _ in 0..1000 {
            approach(&mut v, 1.0, 0.1, 1e-4);
        }
        assert_eq!(v, 1.0);
        assert!((db_to_gain(-6.0206) - 0.5).abs() < 1e-4);
        assert!((gain_to_db(0.1) + 20.0).abs() < 1e-4);
    }
}
