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
        clarity_db: 0.0,
        leveler: 0.0,
        voice_chat: false,
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

/// Stereo pink noise at about -20 dBFS RMS with a typical music-like inter-channel correlation
/// (~0.6), `seconds` long at [`FS`].
pub fn music_like(seed: u64, seconds: f32) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let (mut c, mut l, mut r) = (Pink::new(), Pink::new(), Pink::new());
    let frames = (seconds * FS as f32) as usize;
    let mut out = Vec::with_capacity(frames * 2);
    for _ in 0..frames {
        let common = c.next(rng.white());
        out.push(0.08 * (common + 0.8 * l.next(rng.white())));
        out.push(0.08 * (common + 0.8 * r.next(rng.white())));
    }
    out
}

/// Direct-form biquad in f64 (RBJ cookbook), for building test signals.
#[derive(Clone, Copy)]
pub struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    pub fn new(b: [f64; 3], a: [f64; 3]) -> Self {
        Self { b: b.map(|v| v / a[0]), a: [a[1] / a[0], a[2] / a[0]], z: [0.0; 2] }
    }

    fn rbj(hz: f64, q: f64, fs: u32) -> (f64, f64) {
        let w = 2.0 * PI * hz / fs as f64;
        (w.cos(), w.sin() / (2.0 * q))
    }

    pub fn highpass(hz: f64, q: f64, fs: u32) -> Self {
        let (c, al) = Self::rbj(hz, q, fs);
        Self::new([(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0], [1.0 + al, -2.0 * c, 1.0 - al])
    }

    pub fn lowpass(hz: f64, q: f64, fs: u32) -> Self {
        let (c, al) = Self::rbj(hz, q, fs);
        Self::new([(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0], [1.0 + al, -2.0 * c, 1.0 - al])
    }

    pub fn tick(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// ITU-R BS.1770 K-weighting (48 kHz coefficients from the standard).
fn k_weight(x: &[f32]) -> Vec<f64> {
    let mut shelf = Biquad::new(
        [1.535_124_859_586_97, -2.691_696_189_406_38, 1.198_392_810_852_85],
        [1.0, -1.690_659_293_182_41, 0.732_480_774_215_85],
    );
    let mut rlb = Biquad::new([1.0, -2.0, 1.0], [1.0, -1.990_047_454_833_98, 0.990_072_250_366_21]);
    x.iter().map(|&v| rlb.tick(shelf.tick(v as f64))).collect()
}

/// Integrated loudness (LUFS) of interleaved stereo at 48 kHz: BS.1770 K-weighting, 400 ms
/// blocks with 75 % overlap, absolute (-70) and relative (-10 LU) gates, so pauses in speech do
/// not count.
pub fn lufs(x: &[f32], fs: u32) -> f64 {
    assert_eq!(fs, 48_000, "the K-weighting coefficients are for 48 kHz");
    let (l, r) = split(x);
    let (kl, kr) = (k_weight(&l), k_weight(&r));
    let block = (0.4 * fs as f64) as usize;
    let mut z = Vec::new();
    let mut start = 0;
    while start + block <= kl.len() {
        let e: f64 = kl[start..start + block].iter().chain(&kr[start..start + block]).map(|v| v * v).sum();
        z.push(e / block as f64);
        start += block / 4;
    }
    let loud = |ms: f64| -0.691 + 10.0 * ms.max(1e-30).log10();
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    let abs: Vec<f64> = z.iter().copied().filter(|&m| loud(m) > -70.0).collect();
    let rel = loud(mean(&abs)) - 10.0;
    let gated: Vec<f64> = abs.into_iter().filter(|&m| loud(m) > rel).collect();
    loud(mean(&gated))
}

/// Typical integrated loudness of speech arriving from voice chat or a video, LUFS. Voice chat
/// usually lands around -18 to -24 LUFS and loudness-normalized podcasts and videos around -14 to
/// -16; the leveller pulls both toward its reference.
pub const SPEECH_LUFS: f64 = -20.0;

/// Speech-like test signal at [`FS`]: noise with the long-term spectrum of speech (100 Hz
/// high-pass, about -6 dB/octave above 600 Hz and -12 above 2.5 kHz), in 120-260 ms syllables
/// with ±3 dB of level variation, 4-10 to a phrase, phrases separated by 0.3-0.8 s pauses. A
/// -70 dBFS noise floor runs underneath (what Discord's noise suppression leaves). Mono on both
/// channels like voice chat, normalized to `lufs` integrated loudness.
pub fn speech_like(seed: u64, seconds: f32, lufs_target: f64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let frames = (seconds * FS as f32) as usize;
    let mut env = vec![0.0f32; frames];
    let mut t = (0.05 * FS as f32) as usize;
    'outer: loop {
        let syllables = 4 + (rng.next_u64() % 7) as usize;
        for _ in 0..syllables {
            let len = (rng.range(0.12, 0.26) * FS as f32) as usize;
            let amp = 10f32.powf(rng.range(-3.0, 3.0) / 20.0);
            for i in 0..len {
                if t + i >= frames {
                    break 'outer;
                }
                env[t + i] = amp * (std::f32::consts::PI * i as f32 / len as f32).sin();
            }
            t += len + (rng.range(0.02, 0.06) * FS as f32) as usize;
        }
        t += (rng.range(0.3, 0.8) * FS as f32) as usize;
        if t >= frames {
            break;
        }
    }
    let mut hp = Biquad::highpass(100.0, std::f64::consts::FRAC_1_SQRT_2, FS);
    let (mut lp1, mut lp2) = (0.0f64, 0.0f64);
    let a1 = 1.0 - (-2.0 * PI * 600.0 / FS as f64).exp();
    let a2 = 1.0 - (-2.0 * PI * 2500.0 / FS as f64).exp();
    let mono: Vec<f32> = env
        .iter()
        .map(|&e| {
            let x = hp.tick(rng.white() as f64);
            lp1 += a1 * (x - lp1);
            lp2 += a2 * (lp1 - lp2);
            (lp2 * e as f64) as f32
        })
        .collect();
    let stereo = interleave(&mono, &mono);
    let gain = 10f64.powf((lufs_target - lufs(&stereo, FS)) / 20.0) as f32;
    let floor = 10f32.powf(-70.0 / 20.0);
    stereo
        .chunks(2)
        .flat_map(|f| {
            let n = floor * rng.white();
            [f[0] * gain + n, f[1] * gain + n]
        })
        .collect()
}

/// Speech-shaped continuous noise (the spectrum of [`speech_like`] without the syllables), mono
/// on both channels, about -20 dBFS RMS.
pub fn speech_noise(seed: u64, seconds: f32) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let frames = (seconds * FS as f32) as usize;
    let mut hp = Biquad::highpass(100.0, std::f64::consts::FRAC_1_SQRT_2, FS);
    let (mut lp1, mut lp2) = (0.0f64, 0.0f64);
    let a1 = 1.0 - (-2.0 * PI * 600.0 / FS as f64).exp();
    let a2 = 1.0 - (-2.0 * PI * 2500.0 / FS as f64).exp();
    let mono: Vec<f32> = (0..frames)
        .map(|_| {
            let x = hp.tick(rng.white() as f64);
            lp1 += a1 * (x - lp1);
            lp2 += a2 * (lp1 - lp2);
            lp2 as f32
        })
        .collect();
    let g = 0.1 / rms(&mono) as f32;
    let mono: Vec<f32> = mono.iter().map(|v| v * g).collect();
    interleave(&mono, &mono)
}

/// `x` scaled by `db`.
pub fn gain_db(x: &[f32], db: f64) -> Vec<f32> {
    let g = 10f64.powf(db / 20.0) as f32;
    x.iter().map(|v| v * g).collect()
}
