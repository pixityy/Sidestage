//! The session's master bus: every source's rendered output is summed by the backend and passes
//! through here once before it is played. Each source already limits itself to -1 dBFS, but
//! sources peaking together still add up past full scale (two by up to +5 dB, eight by up to
//! +17 dB); the master limiter catches that.
//!
//! It is the same no-overshoot lookahead limiter each source has, tuned for a mix that can sit
//! over the ceiling for long stretches (music and a loud talker together): a 2 ms gain ramp
//! instead of 1 ms, and a 15 ms hold, so a limited bass note (down to about 33 Hz) gets one flat
//! gain instead of one that recovers a little between its half-waves and adds harmonics.

use crate::limiter::Limiter;

/// Frames processed per internal chunk (stack buffers, no allocation in `process`).
const CHUNK: usize = 256;
const MIN_FS: f32 = 8000.0;
const MAX_FS: f32 = 768_000.0;
const LOOKAHEAD_S: f32 = 0.002;
const HOLD_S: f32 = 0.015;
const RELEASE_S: f32 = 0.12;
/// Eight sources at full scale sum to at most ±8; anything beyond ±64 is a fault and clamped,
/// which also keeps the needed gain far from the denormal range.
const INPUT_LIMIT: f32 = 64.0;

/// Stereo-linked safety limiter for the mix of all sources (-1 dBFS, about 2 ms lookahead).
pub struct MasterBus {
    limiter: Limiter,
}

#[inline]
fn clean(x: f32) -> f32 {
    // Non-finite samples become silence, denormal-sized ones too.
    if x.is_finite() && x.abs() >= 1e-30 { x.clamp(-INPUT_LIMIT, INPUT_LIMIT) } else { 0.0 }
}

impl MasterBus {
    /// Allocates; call off the real-time thread.
    pub fn new(sample_rate: u32) -> Self {
        let fs = (sample_rate as f32).clamp(MIN_FS, MAX_FS);
        Self { limiter: Limiter::with_timing(fs, LOOKAHEAD_S, HOLD_S, RELEASE_S) }
    }

    /// Latency the bus adds, in frames (about 2 ms).
    pub fn latency_frames(&self) -> usize {
        self.limiter.latency()
    }

    /// `input`/`output`: interleaved stereo, any even length (the shorter one wins; the rest of
    /// `output` is zeroed). Non-finite samples become silence. Real-time safe: no allocation,
    /// locks or panics.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        let frames = input.len().min(output.len()) / 2;
        let (out, rest) = output.split_at_mut(frames * 2);
        rest.fill(0.0);
        for (inp, out) in input[..frames * 2].chunks(2 * CHUNK).zip(out.chunks_mut(2 * CHUNK)) {
            let n = inp.len() / 2;
            let (mut l, mut r) = ([0.0f32; CHUNK], [0.0f32; CHUNK]);
            for (i, f) in inp.as_chunks::<2>().0.iter().enumerate() {
                l[i] = clean(f[0]);
                r[i] = clean(f[1]);
            }
            self.limiter.process(&mut l[..n], &mut r[..n]);
            for (i, f) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                f[0] = l[i].clamp(-1.0, 1.0);
                f[1] = r[i].clamp(-1.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 48_000;
    /// -1 dBFS.
    const CEILING: f32 = 0.891_251;

    fn run(bus: &mut MasterBus, input: &[f32], block: usize) -> Vec<f32> {
        let mut out = vec![0.0; input.len()];
        for (i, o) in input.chunks(2 * block).zip(out.chunks_mut(2 * block)) {
            bus.process(i, o);
        }
        out
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    fn tone(hz: f32, amp: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = amp * (i as f32 * hz * std::f32::consts::TAU / FS as f32).sin();
                [v, v]
            })
            .collect()
    }

    #[test]
    fn sum_of_loud_sources_never_clips() {
        let mut bus = MasterBus::new(FS);
        let frames = FS as usize;
        let input: Vec<f32> = (0..frames * 2)
            .map(|i| {
                let t = (i / 2) as f32 / FS as f32;
                // Three full-scale-ish sources summed: well past 0 dBFS.
                0.9 * (t * 220.0 * std::f32::consts::TAU).sin()
                    + 0.9 * (t * 330.0 * std::f32::consts::TAU).sin()
                    + 0.9 * (t * 440.0 * std::f32::consts::TAU).sin()
            })
            .collect();
        let out = run(&mut bus, &input, 480);
        let peak = peak(&out);
        assert!(peak <= CEILING + 1e-4, "peak {peak} must stay at or below -1 dBFS");
        assert!(peak > 0.85, "the limiter should not crush the level: {peak}");
    }

    #[test]
    fn eight_sources_at_full_scale_stay_under_the_ceiling() {
        // The worst case the backend can hand over: eight sources, each already at its own
        // ceiling, all in phase (+17 dB), with sharp onsets and stereo that differs per channel.
        let mut bus = MasterBus::new(FS);
        let mut seed = 7u32;
        let mut input = Vec::new();
        for k in 0..400 {
            let loud = k % 5 < 2;
            for _ in 0..240 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let x = (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                let sum = if loud { 8.0 * x.signum() } else { 0.3 * x };
                input.extend([sum, -0.6 * sum]);
            }
        }
        for block in [1, 64, 480, 4096] {
            let out = run(&mut bus, &input, block);
            let p = peak(&out);
            assert!(p <= CEILING * 1.0001, "{block}-frame blocks: peak {p}");
            assert!(out.iter().all(|v| v.is_finite()));
        }
    }

    #[test]
    fn never_overshoots_at_any_rate_or_call_size() {
        // The hold and lookahead are rounded to whole samples per rate; the no-overshoot
        // argument needs only span >= lookahead, so every rate and any call size must hold the
        // ceiling exactly, with bursts from just over it up to the input clamp.
        let mut seed = 3u32;
        let mut rand = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32
        };
        for fs in [8000u32, 22_050, 44_100, 96_000, 192_000, 768_000] {
            let mut bus = MasterBus::new(fs);
            let frames = fs as usize / 2;
            let mut input = Vec::with_capacity(2 * frames);
            let mut level = 0.5f32;
            for i in 0..frames {
                if i % 97 == 0 {
                    level = [0.3, 0.95, 1.2, 3.0, 8.0, 100.0][(rand() * 6.0) as usize % 6];
                }
                let x = level * (2.0 * rand() - 1.0);
                input.extend([x, -x * rand()]);
            }
            let mut out = vec![0.0f32; input.len()];
            let mut at = 0;
            while at < input.len() {
                let len = (2 * (1 + (rand() * 700.0) as usize) + (rand() * 2.0) as usize).min(input.len() - at);
                bus.process(&input[at..at + len], &mut out[at..at + len]);
                at += len;
            }
            let p = peak(&out);
            assert!(p <= CEILING * 1.000_01, "@ {fs}: peak {p}");
            assert!(p > 0.85, "@ {fs}: {p}");
        }
    }

    #[test]
    fn quiet_mix_passes_unchanged_after_latency() {
        let mut bus = MasterBus::new(FS);
        let lat = bus.latency_frames();
        assert!((90..=100).contains(&lat), "about 2 ms: {lat}");
        let input: Vec<f32> = (0..8192).map(|i| if i % 2 == 0 { 0.1 } else { -0.1 }).collect();
        let mut out = vec![0.0; input.len()];
        bus.process(&input, &mut out);
        assert!(out[..lat * 2].iter().all(|v| *v == 0.0));
        assert!(out[lat * 2..].chunks(2).all(|f| f[0] == 0.1 && f[1] == -0.1));
    }

    #[test]
    fn sustained_bass_overload_is_limited_without_distortion() {
        // A 50 Hz note 4 dB over the ceiling for a second. Without the hold the gain recovers
        // between half-waves (3rd harmonic at -42 dB, 5th at -52 dB); with it the gain is flat.
        let frames = FS as usize;
        let input = tone(50.0, 1.4, frames);
        let out = run(&mut MasterBus::new(FS), &input, 480);
        let x: Vec<f32> = out.iter().step_by(2).copied().skip(FS as usize / 2).collect();
        let level = |hz: f32| {
            let w = hz * std::f32::consts::TAU / FS as f32;
            let (re, im) = x.iter().enumerate().fold((0.0f64, 0.0f64), |(re, im), (i, v)| {
                let (s, c) = (w * i as f32).sin_cos();
                (re + (v * c) as f64, im + (v * s) as f64)
            });
            (re * re + im * im).sqrt()
        };
        let fundamental = level(50.0);
        let worst = [100.0, 150.0, 200.0, 250.0].map(|h| 20.0 * (level(h) / fundamental).log10());
        eprintln!("harmonics 2-5: {worst:.1?} dB");
        assert!(worst.iter().all(|&d| d < -60.0), "harmonics {worst:?} dB");
        let p = peak(&x);
        assert!(p <= CEILING * 1.0001 && p > 0.85, "{p}");
    }

    #[test]
    fn hostile_samples_become_silence_and_the_bus_recovers() {
        let mut bus = MasterBus::new(FS);
        let mut input = tone(1000.0, 0.2, 4800);
        for (i, v) in input.iter_mut().enumerate().skip(1000).step_by(97) {
            *v = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30, -f32::MAX, 1e-40][i % 6];
        }
        let out = run(&mut bus, &input, 480);
        assert!(out.iter().all(|v| v.is_finite() && v.abs() <= CEILING * 1.0001));
        // After the faults (clamped to ±64, so 37 dB of reduction) the gain recovers (release
        // 120 ms) all the way: the mix passes bit-exact again.
        let frames = 3 * FS as usize;
        let clean = tone(1000.0, 0.2, frames);
        let out = run(&mut bus, &clean, 480);
        let lat = bus.latency_frames();
        for i in FS as usize / 2..frames {
            let err = (out[2 * i] - clean[2 * (i - lat)]).abs();
            assert!(err <= 0.2 * 0.02, "frame {i}: still {err} off");
        }
        for i in frames - FS as usize / 2..frames {
            assert_eq!(out[2 * i], clean[2 * (i - lat)], "frame {i}");
        }
    }

    #[test]
    fn any_lengths_work() {
        let mut bus = MasterBus::new(0);
        let input = tone(440.0, 0.5, 1000);
        // Odd and empty lengths, and an output longer than the input (the rest is zeroed).
        for len in [0usize, 1, 3, 513, 2000] {
            let mut out = vec![f32::NAN; len + 9];
            bus.process(&input[..len], &mut out);
            assert!(out.iter().all(|v| v.is_finite()), "{len}");
            assert!(out[len / 2 * 2..].iter().all(|v| *v == 0.0), "{len}");
        }
        let mut short = [f32::NAN; 3];
        bus.process(&input, &mut short);
        assert_eq!(short[2], 0.0);
    }
}
