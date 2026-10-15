//! Signal generators and analysis helpers shared by the integration tests.
#![allow(dead_code)]

use std::f64::consts::PI;
use std::sync::Arc;

use sidestage_dsp::{DuckMode, Room, Settings, SharedParams, Spatializer};

pub const FS: u32 = 48_000;

/// Small deterministic PRNG (xorshift64*), so tests need no extra crates.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Uniform in [lo, hi).
    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.uniform()
    }

    /// Unit-variance white noise.
    pub fn white(&mut self) -> f32 {
        (self.uniform() - 0.5) * 12f32.sqrt()
    }
}

/// Pink-ish noise (Paul Kellet's economy filter, about -3 dB/octave), roughly unit variance.
pub struct Pink {
    b: [f32; 3],
}

impl Pink {
    pub fn new() -> Self {
        Self { b: [0.0; 3] }
    }

    pub fn next(&mut self, white: f32) -> f32 {
        self.b[0] = 0.99765 * self.b[0] + white * 0.099_046;
        self.b[1] = 0.96300 * self.b[1] + white * 0.296_516_4;
        self.b[2] = 0.57000 * self.b[2] + white * 1.052_691_3;
        (self.b[0] + self.b[1] + self.b[2] + white * 0.1848) * 0.25
    }
}

/// Settings for measuring the direct path alone: no room, flat tone, no ducking, 1 m.
pub fn dry_settings(azimuth_deg: f32, width_deg: f32) -> Settings {
    Settings {
        azimuth_deg,
        elevation_deg: 0.0,
        distance_m: 1.0,
        width_deg,
        volume_db: -6.0,
        lowpass_hz: 20_000.0,
        highpass_hz: 20.0,
        bass_db: 0.0,
        presence_dip_db: 0.0,
        room: Room::Off,
        reverb_db: -40.0,
        ducking: DuckMode::Off.ducking(),
        bypass: false,
    }
}

pub fn interleave(l: &[f32], r: &[f32]) -> Vec<f32> {
    l.iter().zip(r).flat_map(|(a, b)| [*a, *b]).collect()
}

pub fn split(x: &[f32]) -> (Vec<f32>, Vec<f32>) {
    (x.iter().step_by(2).copied().collect(), x.iter().skip(1).step_by(2).copied().collect())
}

pub fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

pub fn db(x: f64) -> f64 {
    20.0 * x.max(1e-20).log10()
}

/// A renderer with its shared parameters, for tests that change settings while running.
pub fn spatializer(settings: Settings, fs: u32) -> (Spatializer, Arc<SharedParams>) {
    let params = SharedParams::new(settings);
    (Spatializer::new(fs, params.clone()), params)
}

/// In-place radix-2 complex FFT.
pub fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    assert!(n.is_power_of_two() && im.len() == n);
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..len / 2 {
                let (s, c) = (ang * k as f64).sin_cos();
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * c - im[b] * s;
                let ti = re[b] * s + im[b] * c;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
}

/// Averaged power spectrum (Hann-windowed, 50% overlap), `n` bins up to Nyquist.
pub fn power_spectrum(x: &[f32], n: usize) -> Vec<f64> {
    let win: Vec<f64> = (0..n).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos()).collect();
    let mut acc = vec![0.0; n / 2];
    let mut start = 0;
    while start + n <= x.len() {
        let mut re: Vec<f64> = x[start..start + n].iter().zip(&win).map(|(v, w)| *v as f64 * w).collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        for (k, a) in acc.iter_mut().enumerate() {
            *a += re[k] * re[k] + im[k] * im[k];
        }
        start += n / 2;
    }
    acc
}

/// Energy between `lo` and `hi` Hz from a [`power_spectrum`].
pub fn band(spec: &[f64], fs: u32, lo: f64, hi: f64) -> f64 {
    let bin = fs as f64 / (2.0 * spec.len() as f64);
    spec.iter().enumerate().filter(|(k, _)| (*k as f64 * bin) >= lo && (*k as f64 * bin) <= hi).map(|(_, p)| p).sum()
}

/// Lag (samples, fractional) at which `b` best matches `a` delayed: positive when `b` lags `a`.
pub fn xcorr_lag(a: &[f32], b: &[f32], max_lag: usize) -> f64 {
    let n = a.len().min(b.len());
    let c = |lag: isize| -> f64 {
        let mut s = 0.0;
        for i in max_lag..n - max_lag {
            s += a[i] as f64 * b[(i as isize + lag) as usize] as f64;
        }
        s
    };
    let lags: Vec<isize> = (-(max_lag as isize)..=max_lag as isize).collect();
    let vals: Vec<f64> = lags.iter().map(|&l| c(l)).collect();
    let k = (0..vals.len()).max_by(|&i, &j| vals[i].total_cmp(&vals[j])).unwrap();
    // Parabolic interpolation around the peak.
    let frac = if k > 0 && k + 1 < vals.len() {
        let (y0, y1, y2) = (vals[k - 1], vals[k], vals[k + 1]);
        0.5 * (y0 - y2) / (y0 - 2.0 * y1 + y2)
    } else {
        0.0
    };
    lags[k] as f64 + frac
}

/// Processes `input` (interleaved) through `sp` in blocks of `block` frames.
pub fn run(sp: &mut Spatializer, input: &[f32], block: usize, sidechain: f32) -> Vec<f32> {
    let mut out = vec![0.0; input.len()];
    for (i, o) in input.chunks(block * 2).zip(out.chunks_mut(block * 2)) {
        sp.process(i, o, sidechain);
    }
    out
}

/// Largest sample-to-sample step relative to the steepest slope a sine of frequency `hz` with
/// the local amplitude (peak within ±`window` samples) can have. A smooth signal stays at or
/// below about 1; clicks and zipper steps stand out far above it.
pub fn worst_step_ratio(x: &[f32], hz: f64, fs: u32, window: usize) -> (f64, usize) {
    let w = 2.0 * PI * hz / fs as f64;
    let mut worst = (0.0, 0);
    for i in window + 1..x.len().saturating_sub(window) {
        let amp = x[i - window..=i + window].iter().fold(0.0f32, |m, v| m.max(v.abs())) as f64;
        if amp < 1e-4 {
            continue;
        }
        let step = (x[i] - x[i - 1]).abs() as f64;
        let ratio = step / (amp * w);
        if ratio > worst.0 {
            worst = (ratio, i);
        }
    }
    worst
}

/// Largest second difference relative to what a sine of frequency `hz` with the local amplitude
/// can have (`amp·w²`). Far more sensitive than [`worst_step_ratio`]: a step of `δ` shows up
/// as `δ / (amp·w²)` instead of `δ / (amp·w)`, so it catches zipper steps 20-40 dB below the
/// signal that the step ratio cannot see.
pub fn worst_curvature_ratio(x: &[f32], hz: f64, fs: u32, window: usize) -> (f64, usize) {
    let w = 2.0 * PI * hz / fs as f64;
    let mut worst = (0.0, 0);
    for i in window + 2..x.len().saturating_sub(window) {
        let amp = x[i - window..=i + window].iter().fold(0.0f32, |m, v| m.max(v.abs())) as f64;
        if amp < 1e-4 {
            continue;
        }
        let d2 = (x[i] as f64 - 2.0 * x[i - 1] as f64 + x[i - 2] as f64).abs();
        let ratio = d2 / (amp * w * w);
        if ratio > worst.0 {
            worst = (ratio, i);
        }
    }
    worst
}

/// A sine of `hz` at `amp`, `frames` long.
pub fn sine(hz: f64, amp: f32, frames: usize, fs: u32) -> Vec<f32> {
    let w = 2.0 * PI * hz / fs as f64;
    (0..frames).map(|i| amp * (w * i as f64).sin() as f32).collect()
}

/// Plays `input` (interleaved) in `block`-frame calls, publishing `to` once `at` frames have
/// been processed.
pub fn run_with_change(from: Settings, to: Settings, fs: u32, input: &[f32], at: usize, block: usize) -> Vec<f32> {
    let (mut sp, params) = spatializer(from, fs);
    let mut out = vec![0.0; input.len()];
    let mut changed = false;
    for (k, (i, o)) in input.chunks(block * 2).zip(out.chunks_mut(block * 2)).enumerate() {
        if !changed && k * block >= at {
            params.set(to);
            changed = true;
        }
        sp.process(i, o, 0.0);
    }
    out
}
