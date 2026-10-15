//! Second-order filters: a trapezoidal-integrated state-variable filter (Andrew Simper,
//! Cytomic, "SvfLinearTrapOptimised2") that implements every RBJ cookbook response we need.
//!
//! The SVF is used instead of a direct-form biquad because its states are the integrator
//! "voltages" of a stable analog prototype: coefficients can change every sample (they are
//! interpolated linearly across each sub-block) without the transients or instability a
//! modulated direct form can produce.

use std::f32::consts::PI;

use crate::smoothing::{db_to_gain, flush};

/// Coefficients of one SVF: `a*` set the analog prototype (cutoff `g`, damping `k`), `m*` mix the
/// input, band-pass and low-pass outputs into the wanted response.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvfCoefs {
    a1: f32,
    a2: f32,
    a3: f32,
    m0: f32,
    m1: f32,
    m2: f32,
}

/// Prewarped integrator gain for a cutoff in Hz, kept safely below Nyquist so every sample rate
/// from 8 kHz up gets a valid (if band-limited) filter.
#[inline]
pub fn warp(fs: f32, hz: f32) -> f32 {
    (PI * hz.clamp(1.0, 0.45 * fs) / fs).tan()
}

impl SvfCoefs {
    #[inline]
    fn new(g: f32, k: f32, m0: f32, m1: f32, m2: f32) -> Self {
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        Self { a1, a2, a3: g * a2, m0, m1, m2 }
    }

    pub fn lowpass(fs: f32, hz: f32, q: f32) -> Self {
        Self::new(warp(fs, hz), 1.0 / q, 0.0, 0.0, 1.0)
    }

    /// Low-pass blended with the input: `open` = 0 is a plain low-pass, 1 passes everything
    /// untouched. Lets "muffle off" be exactly transparent while staying continuous.
    pub fn lowpass_open(fs: f32, hz: f32, q: f32, open: f32) -> Self {
        let open = open.clamp(0.0, 1.0);
        Self::new(warp(fs, hz), 1.0 / q, open, 0.0, 1.0 - open)
    }

    pub fn highpass(fs: f32, hz: f32, q: f32) -> Self {
        let k = 1.0 / q;
        Self::new(warp(fs, hz), k, 1.0, -k, -1.0)
    }

    /// Peaking EQ with `db` gain at `hz` (0 dB is exactly transparent).
    pub fn bell(fs: f32, hz: f32, q: f32, db: f32) -> Self {
        let a = db_to_gain(0.5 * db);
        let k = 1.0 / (q * a);
        Self::new(warp(fs, hz), k, 1.0, k * (a * a - 1.0), 0.0)
    }

    /// Low shelf: `db` below `hz`, unity above.
    pub fn low_shelf(fs: f32, hz: f32, q: f32, db: f32) -> Self {
        let a = db_to_gain(0.5 * db);
        let k = 1.0 / q;
        Self::new(warp(fs, hz) / a.sqrt(), k, 1.0, k * (a - 1.0), a * a - 1.0)
    }

    /// High shelf: unity below `hz`, `db` above.
    pub fn high_shelf(fs: f32, hz: f32, q: f32, db: f32) -> Self {
        let a = db_to_gain(0.5 * db);
        let k = 1.0 / q;
        Self::new(warp(fs, hz) * a.sqrt(), k, a * a, k * (1.0 - a) * a, 1.0 - a * a)
    }

    #[inline]
    fn step_to(&self, target: &Self, n: usize) -> Self {
        let s = 1.0 / n as f32;
        Self {
            a1: (target.a1 - self.a1) * s,
            a2: (target.a2 - self.a2) * s,
            a3: (target.a3 - self.a3) * s,
            m0: (target.m0 - self.m0) * s,
            m1: (target.m1 - self.m1) * s,
            m2: (target.m2 - self.m2) * s,
        }
    }

    #[inline]
    fn add(&mut self, d: &Self) {
        self.a1 += d.a1;
        self.a2 += d.a2;
        self.a3 += d.a3;
        self.m0 += d.m0;
        self.m1 += d.m1;
        self.m2 += d.m2;
    }

    /// Magnitude response at `hz` (tests and calibration only; not real-time).
    #[cfg(test)]
    pub fn magnitude(&self, fs: f32, hz: f32) -> f32 {
        // RMS ratio after settling: the sample peak of a tone near fs/4 depends on its phase.
        let mut f = Svf::new(*self);
        let w = 2.0 * std::f64::consts::PI * hz as f64 / fs as f64;
        let n = (fs * 0.5) as usize;
        let (mut e_in, mut e_out) = (0.0f64, 0.0f64);
        for i in 0..n {
            let x = (w * i as f64).sin();
            let y = f.tick(x as f32) as f64;
            if i > n / 2 {
                e_in += x * x;
                e_out += y * y;
            }
        }
        (e_out / e_in).sqrt() as f32
    }
}

/// One SVF integration step.
#[inline(always)]
fn tick(c: &SvfCoefs, s: &mut [f32; 2], v0: f32) -> f32 {
    let v3 = v0 - s[1];
    let v1 = c.a1 * s[0] + c.a2 * v3;
    let v2 = s[1] + c.a2 * s[0] + c.a3 * v3;
    s[0] = 2.0 * v1 - s[0];
    s[1] = 2.0 * v2 - s[1];
    c.m0 * v0 + c.m1 * v1 + c.m2 * v2
}

/// Mono SVF whose coefficients glide to a new target over each processed buffer.
#[derive(Clone, Copy, Debug)]
pub struct Svf {
    c: SvfCoefs,
    s: [f32; 2],
}

impl Svf {
    pub fn new(c: SvfCoefs) -> Self {
        Self { c, s: [0.0; 2] }
    }

    /// Jump to new coefficients (only while the output is faded out or before first use).
    pub fn set_coefs(&mut self, c: SvfCoefs) {
        self.c = c;
    }

    #[cfg(test)]
    fn tick(&mut self, x: f32) -> f32 {
        tick(&self.c, &mut self.s, x)
    }

    /// Filters `buf` in place while moving the coefficients linearly to `target`.
    #[inline]
    pub fn process(&mut self, buf: &mut [f32], target: &SvfCoefs) {
        if buf.is_empty() {
            return;
        }
        if self.c == *target {
            let c = self.c;
            for x in buf.iter_mut() {
                *x = tick(&c, &mut self.s, *x);
            }
        } else {
            let d = self.c.step_to(target, buf.len());
            let mut c = self.c;
            for x in buf.iter_mut() {
                c.add(&d);
                *x = tick(&c, &mut self.s, *x);
            }
            self.c = *target;
        }
    }

    pub fn reset(&mut self) {
        self.s = [0.0; 2];
    }

    pub fn flush(&mut self) {
        self.s.iter_mut().for_each(flush);
    }
}

/// Two channels (a stereo bus) sharing one set of gliding coefficients.
#[derive(Clone, Copy, Debug)]
pub struct Svf2 {
    c: SvfCoefs,
    s: [[f32; 2]; 2],
}

impl Svf2 {
    pub fn new(c: SvfCoefs) -> Self {
        Self { c, s: [[0.0; 2]; 2] }
    }

    pub fn set_coefs(&mut self, c: SvfCoefs) {
        self.c = c;
    }

    /// Filters both channels in place while moving the coefficients linearly to `target`.
    #[inline]
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], target: &SvfCoefs) {
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        let [sl, sr] = &mut self.s;
        if self.c == *target {
            let c = self.c;
            for (a, b) in l[..n].iter_mut().zip(&mut r[..n]) {
                *a = tick(&c, sl, *a);
                *b = tick(&c, sr, *b);
            }
        } else {
            let d = self.c.step_to(target, n);
            let mut c = self.c;
            for (a, b) in l[..n].iter_mut().zip(&mut r[..n]) {
                c.add(&d);
                *a = tick(&c, sl, *a);
                *b = tick(&c, sr, *b);
            }
            self.c = *target;
        }
    }

    pub fn reset(&mut self) {
        self.s = [[0.0; 2]; 2];
    }

    pub fn flush(&mut self) {
        self.s.iter_mut().flatten().for_each(flush);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(x: f32) -> f32 {
        20.0 * x.log10()
    }

    #[test]
    fn responses_match_the_cookbook() {
        let fs = 48_000.0;
        let lp = SvfCoefs::lowpass(fs, 1000.0, std::f32::consts::FRAC_1_SQRT_2);
        assert!((db(lp.magnitude(fs, 100.0))).abs() < 0.1);
        assert!((db(lp.magnitude(fs, 1000.0)) + 3.0).abs() < 0.2);
        assert!(db(lp.magnitude(fs, 8000.0)) < -30.0);

        let hp = SvfCoefs::highpass(fs, 100.0, std::f32::consts::FRAC_1_SQRT_2);
        assert!((db(hp.magnitude(fs, 100.0)) + 3.0).abs() < 0.2);
        assert!(db(hp.magnitude(fs, 5000.0)).abs() < 0.1);

        let bell = SvfCoefs::bell(fs, 3200.0, 1.0, -6.0);
        assert!((db(bell.magnitude(fs, 3200.0)) + 6.0).abs() < 0.15);
        assert!(db(bell.magnitude(fs, 200.0)).abs() < 0.2);

        let ls = SvfCoefs::low_shelf(fs, 150.0, std::f32::consts::FRAC_1_SQRT_2, 6.0);
        assert!((db(ls.magnitude(fs, 20.0)) - 6.0).abs() < 0.3);
        assert!(db(ls.magnitude(fs, 4000.0)).abs() < 0.1);

        let hs = SvfCoefs::high_shelf(fs, 4000.0, std::f32::consts::FRAC_1_SQRT_2, -6.0);
        assert!((db(hs.magnitude(fs, 4000.0)) + 3.0).abs() < 0.3);
        assert!((db(hs.magnitude(fs, 16000.0)) + 6.0).abs() < 0.3);
        assert!(db(hs.magnitude(fs, 200.0)).abs() < 0.1);

        let open = SvfCoefs::lowpass_open(fs, 1000.0, 0.7, 1.0);
        assert!(db(open.magnitude(fs, 10_000.0)).abs() < 1e-3);
    }

    #[test]
    fn gliding_coefficients_stay_stable() {
        let fs = 22_050.0;
        let mut f = Svf2::new(SvfCoefs::lowpass(fs, 20.0, 0.5));
        let mut l = [1.0f32; 64];
        let mut r = [-1.0f32; 64];
        for i in 0..2000usize {
            let hz = if i.is_multiple_of(2) { 20.0 } else { 10_000.0 };
            f.process(&mut l, &mut r, &SvfCoefs::lowpass(fs, hz, 8.0));
            assert!(l.iter().chain(&r).all(|x| x.is_finite() && x.abs() < 100.0));
            l.fill(1.0);
            r.fill(-1.0);
        }
    }
}
