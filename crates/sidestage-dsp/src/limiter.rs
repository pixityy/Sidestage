//! Transparent stereo-linked lookahead peak limiter at -1 dBFS.
//!
//! Per sample, the gain a peak needs is `threshold / peak`. A sliding-window minimum over the
//! lookahead window (plus an optional hold) keeps that gain for as long as the peak is in
//! flight, a one-pole lets it recover afterwards, and a moving average over the lookahead turns
//! the drop into a linear ramp that lands exactly when the (delayed) peak is played. Because
//! every gain value averaged for a sample is at most the gain that sample needs, the output never
//! exceeds the threshold: no overshoot and no clipping distortion, at the cost of the lookahead
//! as latency.
//!
//! The hold keeps the gain flat between the peaks of a sustained overload. Without it the gain
//! recovers a little between every two half-waves of a bass note and dips again at the next one:
//! that ripple is distortion (the per-source limiter rarely works hard enough for it to matter;
//! the master bus, which catches several sources peaking together, can).

use crate::smoothing::db_to_gain;

const THRESHOLD_DB: f32 = -1.0;
const LOOKAHEAD_S: f32 = 0.001;
const RELEASE_S: f32 = 0.08;

pub struct Limiter {
    threshold: f32,
    /// Lookahead L in samples: the length of the gain ramp; the audio is delayed by L - 1.
    window: usize,
    /// Span of the sliding minimum: the lookahead plus the hold (>= `window`).
    span: usize,
    delay: [Box<[f32]>; 2],
    delay_pos: usize,
    /// Monotonic deque (ring) of (sample index, needed gain) for the sliding minimum.
    q_idx: Box<[u64]>,
    q_val: Box<[f32]>,
    q_head: usize,
    q_len: usize,
    t: u64,
    /// How far the held gain is below unity. Recovery shrinks it by `recover` per sample: a
    /// one-pole on the gain itself would stall just below 1 in f32 once each step is under half
    /// an ulp (about -0.001 dB at 48 kHz, more at higher rates), never quite returning to an exact
    /// passthrough.
    depth: f32,
    recover: f32,
    avg_buf: Box<[f32]>,
    avg_pos: usize,
    avg_sum: f64,
}

impl Limiter {
    /// The per-source limiter: 1 ms lookahead, no hold, 80 ms release. Allocates.
    pub fn new(fs: f32) -> Self {
        Self::with_timing(fs, LOOKAHEAD_S, 0.0, RELEASE_S)
    }

    /// A limiter with its own lookahead (= latency), hold and release, in seconds. Allocates.
    pub fn with_timing(fs: f32, lookahead_s: f32, hold_s: f32, release_s: f32) -> Self {
        let window = ((lookahead_s * fs).round() as usize).max(8);
        let span = window + (hold_s.max(0.0) * fs).round() as usize;
        let d = window - 1;
        Self {
            threshold: db_to_gain(THRESHOLD_DB),
            window,
            span,
            delay: [vec![0.0; d].into_boxed_slice(), vec![0.0; d].into_boxed_slice()],
            delay_pos: 0,
            q_idx: vec![0; span + 1].into_boxed_slice(),
            q_val: vec![1.0; span + 1].into_boxed_slice(),
            q_head: 0,
            q_len: 0,
            t: 0,
            depth: 0.0,
            recover: (-1.0 / (release_s.max(1e-3) * fs)).exp(),
            avg_buf: vec![1.0; window].into_boxed_slice(),
            avg_pos: 0,
            avg_sum: window as f64,
        }
    }

    /// Latency added to the processed signal, in samples.
    pub fn latency(&self) -> usize {
        self.window - 1
    }

    /// Pushes the gain needed now and returns the minimum over the last `span` samples.
    #[inline]
    fn window_min(&mut self, g: f32) -> f32 {
        let cap = self.q_idx.len();
        while self.q_len > 0 {
            let mut back = self.q_head + self.q_len - 1;
            if back >= cap {
                back -= cap;
            }
            if self.q_val[back] >= g {
                self.q_len -= 1;
            } else {
                break;
            }
        }
        let mut slot = self.q_head + self.q_len;
        if slot >= cap {
            slot -= cap;
        }
        self.q_val[slot] = g;
        self.q_idx[slot] = self.t;
        self.q_len += 1;
        // The entry just pushed is always inside the span, so the deque never empties here.
        while self.q_len > 1 && self.q_idx[self.q_head] + self.span as u64 <= self.t {
            self.q_head += 1;
            if self.q_head == cap {
                self.q_head = 0;
            }
            self.q_len -= 1;
        }
        self.t += 1;
        self.q_val[self.q_head]
    }

    /// Limits a stereo sub-block in place. Inputs must be finite.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        // Re-sum the moving average exactly once per block so rounding cannot drift.
        self.avg_sum = self.avg_buf.iter().map(|&v| v as f64).sum();
        let inv = 1.0 / self.window as f64;
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let peak = a.abs().max(b.abs());
            let need = if peak > self.threshold { self.threshold / peak } else { 1.0 };
            let wmin = self.window_min(need);
            self.depth = (self.depth * self.recover).max(1.0 - wmin);
            if self.depth < 1e-9 {
                self.depth = 0.0;
            }
            let held = 1.0 - self.depth;
            let old = std::mem::replace(&mut self.avg_buf[self.avg_pos], held);
            self.avg_pos += 1;
            if self.avg_pos == self.window {
                self.avg_pos = 0;
            }
            self.avg_sum += (held - old) as f64;
            let gain = (self.avg_sum * inv) as f32;

            let [dl, dr] = &mut self.delay;
            let out_l = std::mem::replace(&mut dl[self.delay_pos], *a);
            let out_r = std::mem::replace(&mut dr[self.delay_pos], *b);
            self.delay_pos += 1;
            if self.delay_pos == dl.len() {
                self.delay_pos = 0;
            }
            *a = out_l * gain;
            *b = out_r * gain;
        }
    }

    pub fn reset(&mut self) {
        self.delay.iter_mut().for_each(|d| d.fill(0.0));
        self.delay_pos = 0;
        self.q_head = 0;
        self.q_len = 0;
        self.depth = 0.0;
        self.avg_buf.fill(1.0);
        self.avg_pos = 0;
        self.avg_sum = self.window as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_exceeds_threshold_and_is_transparent_below() {
        let fs = 48_000.0;
        let mut lim = Limiter::new(fs);
        let thr = db_to_gain(THRESHOLD_DB);
        let mut seed = 1u32;
        let mut max_out = 0.0f32;
        for block in 0..2000 {
            let loud = block % 7 < 3;
            let mut l = [0.0f32; 64];
            let mut r = [0.0f32; 64];
            for (a, b) in l.iter_mut().zip(r.iter_mut()) {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let x = (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                *a = x * if loud { 6.0 } else { 0.5 };
                *b = -*a * 0.7;
            }
            lim.process(&mut l, &mut r);
            max_out = l.iter().chain(&r).fold(max_out, |m, v| m.max(v.abs()));
        }
        assert!(max_out <= thr * 1.0001, "{max_out}");

        // Quiet signal after recovery: exact passthrough (delayed).
        let mut lim = Limiter::new(fs);
        let d = lim.latency();
        let input: Vec<f32> = (0..512).map(|i| 0.5 * (i as f32 * 0.05).sin()).collect();
        let mut l = input.clone();
        let mut r = input.clone();
        lim.process(&mut l, &mut r);
        for i in d..512 {
            assert!((l[i] - input[i - d]).abs() < 1e-6);
        }
    }

    #[test]
    fn recovers_to_an_exact_passthrough_after_limiting() {
        // At high rates the release step per sample is tiny; a gain one-pole in f32 used to
        // stall about -0.004 dB short of unity at 192 kHz and stay there.
        for fs in [44_100.0, 192_000.0, 768_000.0] {
            let mut lim = Limiter::new(fs);
            let mut l = [4.0f32; 64];
            let mut r = [-4.0f32; 64];
            lim.process(&mut l, &mut r);
            let quiet = 0.25f32;
            let mut out = 0.0;
            for _ in 0..(2.0 * fs) as usize / 64 {
                let (mut l, mut r) = ([quiet; 64], [quiet; 64]);
                lim.process(&mut l, &mut r);
                out = l[63];
            }
            assert_eq!(out, quiet, "@ {fs}");
        }
    }

    #[test]
    fn hold_keeps_the_gain_down_and_never_overshoots() {
        let fs = 48_000.0;
        let mut lim = Limiter::with_timing(fs, 0.002, 0.01, 0.1);
        let d = lim.latency();
        assert_eq!(d, 95);
        // One loud burst, then quiet: the gain stays at the burst's level for the hold.
        let mut l = vec![0.1f32; 19_200];
        l[1000..1010].fill(2.0);
        let mut r = l.clone();
        for (a, b) in l.chunks_mut(64).zip(r.chunks_mut(64)) {
            lim.process(a, b);
        }
        let thr = db_to_gain(THRESHOLD_DB);
        assert!(l.iter().all(|v| v.abs() <= thr * 1.0001));
        let g = |i: usize| l[i + d] / 0.1;
        let burst = thr / 2.0;
        assert!((g(1015) - burst).abs() < 1e-3 && (g(1000 + 480) - burst).abs() < 1e-3, "held");
        // Then it releases (100 ms): about 220 ms after the hold it is back within 0.6 dB.
        assert!(g(1700) > burst + 1e-3 && g(12_000) > 0.93, "released: {} {}", g(1700), g(12_000));
    }
}
