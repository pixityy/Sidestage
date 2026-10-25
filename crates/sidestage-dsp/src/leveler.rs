//! Voice leveller ([`crate::Settings::leveler`]): evens out loud and quiet talkers on the dry
//! input, before the 3D rendering, so the room and the head model see a steady voice.
//!
//! A stereo-linked feed-forward compressor with automatic make-up gain. The detector is the
//! short-term RMS (6 ms) of the louder channel after a 100 Hz high-pass, so rumble, desk bumps
//! and plosive pops do not drive the gain. The static curve compresses around [`REF_DB`]: a
//! talker there passes at unity, louder ones are turned down and quieter ones up, with ratio,
//! threshold and make-up all growing with the amount (0 is an exact bypass). Attack and release
//! smooth the gain in dB.
//!
//! Lifting quiet input is what makes a leveller pump, so three rules keep noise where it was:
//! below [`FLOOR_DB`] nothing is lifted (breaths, keyboard noise, the residue of Discord's noise
//! suppression); while the input is below [`GATE_DB`] the gain never rises; and it never rises
//! either while the input is within [`NOISE_MARGIN_DB`] of its own noise floor (the lowest
//! detector level of the last [`NOISE_WINDOW_S`]). The last rule is what stops steady noise
//! louder than the gate (an unsuppressed mic, a fan, room tone, a video's music bed) from
//! swelling up by the whole make-up gain in every pause between phrases. In a pause the gain
//! holds, however long the pause: in voice chat the first word of a callout after a silence
//! matters most, and it starts at the level the last phrase ended at instead of re-earning its
//! make-up gain (a new, louder talker is still turned down within the attack time).

use crate::biquad::{Svf2, SvfCoefs};
use crate::smoothing::{Ramp, SUB_BLOCK, approach, db_to_gain, one_pole, smoothstep};

/// Detector level (dBFS, short-term RMS of the louder channel) that passes at unity gain. The
/// detector follows syllable peaks, which sit about 6 dB above the speech's integrated
/// loudness, so this is a talker at about -20 LUFS: between voice chat and normalized podcasts.
const REF_DB: f32 = -19.0;
/// Ratio 1 + 5·amount above a threshold (8 + 16·amount) dB below [`REF_DB`]: 4:1 and up to
/// +13.2 dB of make-up at amount 0.6, 6:1 and up to +20 dB at 1.
const MAX_RATIO_STEP: f32 = 5.0;
const MIN_RANGE_DB: f32 = 8.0;
const MAX_RANGE_STEP_DB: f32 = 16.0;
const KNEE_DB: f32 = 8.0;
/// Nothing at or below this level is lifted; the lift fades in fully by [`FULL_LIFT_DB`].
const FLOOR_DB: f32 = -50.0;
const FULL_LIFT_DB: f32 = -40.0;
/// Below this level the gain never rises (no pumping in pauses).
const GATE_DB: f32 = -45.0;
/// The noise floor is the minimum detector level over this window, kept as the minima of
/// [`NOISE_SEGMENTS`] equal segments (no allocation, a fixed cost per sub-block). Long enough to
/// span the pauses between words, short enough to follow a noise that comes and goes. Digital
/// silence counts too, so on a noise-suppressed source the floor is far down and this rule
/// never gets in the way.
const NOISE_WINDOW_S: f32 = 1.6;
const NOISE_SEGMENTS: usize = 8;
/// The gain only rises while the input is this far above its noise floor: speech, not the
/// noise's own ripple (the short-term level of band-limited noise wanders by several dB).
const NOISE_MARGIN_DB: f32 = 12.0;
/// After a reset the detector rises from zero (and a new renderer fades its input in over
/// 20 ms): levels from that time would pull the floor far down and switch the rule off for a
/// whole window, so they are skipped, and the gain holds meanwhile.
const NOISE_WARMUP_S: f32 = 0.06;
const DETECT_TAU_S: f32 = 0.006;
const DETECT_HP_HZ: f32 = 100.0;
const ATTACK_S: f32 = 0.008;
const RELEASE_S: f32 = 0.25;
/// Glide back to unity when the leveller is switched off.
const OFF_S: f32 = 0.05;
const AMOUNT_TAU_S: f32 = 0.03;
/// Keeps the detector's log finite in digital silence (-200 dBFS).
const SILENCE: f32 = 1e-20;
/// Detector samples below this (-300 dBFS) count as silence: their squares would be denormal.
const TINY: f32 = 1e-15;

pub struct Leveler {
    fs: f32,
    detect_hp: Svf2,
    hp_coefs: SvfCoefs,
    /// Mean-square envelope of the louder (high-passed) channel.
    env: f32,
    env_coef: f32,
    amount: f32,
    gain_db: f32,
    gain: Ramp,
    noise: NoiseFloor,
}

/// Sliding minimum of the detector level (dB) over [`NOISE_WINDOW_S`], in segments.
struct NoiseFloor {
    /// Minimum of each finished segment; +inf while unknown (since a reset).
    segments: [f32; NOISE_SEGMENTS],
    pos: usize,
    /// Minimum of the segment in progress.
    current: f32,
    frames_left: usize,
    segment_frames: usize,
    warmup_left: usize,
    warmup_frames: usize,
}

impl NoiseFloor {
    fn new(fs: f32) -> Self {
        let segment_frames = ((NOISE_WINDOW_S / NOISE_SEGMENTS as f32 * fs).round() as usize).max(1);
        let warmup_frames = (NOISE_WARMUP_S * fs).round() as usize;
        Self {
            segments: [f32::INFINITY; NOISE_SEGMENTS],
            pos: 0,
            current: f32::INFINITY,
            frames_left: segment_frames,
            segment_frames,
            warmup_left: warmup_frames,
            warmup_frames,
        }
    }

    /// Takes the level of the next `n` frames and returns the floor, this level included (+inf
    /// while warming up). Until a whole window has passed since a reset it is the lowest level
    /// seen since: a source that starts noisy is gated from the start, one that starts with
    /// speech holds its gain until the first pause shows where its noise is.
    #[inline]
    fn update(&mut self, level_db: f32, n: usize) -> f32 {
        if self.warmup_left > 0 {
            self.warmup_left = self.warmup_left.saturating_sub(n);
            return f32::INFINITY;
        }
        self.current = self.current.min(level_db);
        let floor = self.segments.iter().fold(self.current, |m, &s| m.min(s));
        if n >= self.frames_left {
            self.segments[self.pos] = self.current;
            self.pos = (self.pos + 1) % NOISE_SEGMENTS;
            self.current = f32::INFINITY;
            self.frames_left = self.segment_frames;
        } else {
            self.frames_left -= n;
        }
        floor
    }

    fn reset(&mut self) {
        self.segments = [f32::INFINITY; NOISE_SEGMENTS];
        self.current = f32::INFINITY;
        self.frames_left = self.segment_frames;
        self.warmup_left = self.warmup_frames;
    }
}

/// Gain in dB the leveller wants for a detector level, before smoothing.
fn curve(level_db: f32, amount: f32) -> f32 {
    if amount <= 0.0 {
        return 0.0;
    }
    let slope = 1.0 - 1.0 / (1.0 + MAX_RATIO_STEP * amount);
    let threshold = REF_DB - (MIN_RANGE_DB + MAX_RANGE_STEP_DB * amount);
    let reduction = |l: f32| {
        let over = l - threshold;
        let r = if over <= -0.5 * KNEE_DB {
            0.0
        } else if over < 0.5 * KNEE_DB {
            (over + 0.5 * KNEE_DB).powi(2) / (2.0 * KNEE_DB)
        } else {
            over
        };
        slope * r
    };
    let lift = smoothstep((level_db - FLOOR_DB) / (FULL_LIFT_DB - FLOOR_DB));
    reduction(REF_DB) * lift - reduction(level_db)
}

impl Leveler {
    pub fn new(fs: f32) -> Self {
        let hp_coefs = SvfCoefs::highpass(fs, DETECT_HP_HZ, std::f32::consts::FRAC_1_SQRT_2);
        Self {
            fs,
            detect_hp: Svf2::new(hp_coefs),
            hp_coefs,
            env: 0.0,
            env_coef: 1.0 - (-1.0 / (DETECT_TAU_S * fs)).exp(),
            amount: 0.0,
            gain_db: 0.0,
            gain: Ramp::new(1.0),
            noise: NoiseFloor::new(fs),
        }
    }

    /// Current gain in dB (positive = lifted).
    #[cfg(test)]
    pub fn gain_db(&self) -> f32 {
        self.gain_db
    }

    /// Levels one sub-block (at most [`SUB_BLOCK`] frames) in place. `amount` must be sanitized
    /// (0..=1). While off and settled the samples are not touched at all.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], amount: f32) {
        let n = l.len().min(r.len()).min(SUB_BLOCK);
        if n == 0 {
            return;
        }
        approach(&mut self.amount, amount, one_pole(AMOUNT_TAU_S, n, self.fs), 1e-4);
        if self.amount <= 0.0 && self.gain_db == 0.0 {
            // Off: exact bypass. The detector restarts from silence when it is turned on again.
            self.env = 0.0;
            self.detect_hp.reset();
            self.noise.reset();
            return;
        }
        let (l, r) = (&mut l[..n], &mut r[..n]);

        let (mut dl, mut dr) = ([0.0f32; SUB_BLOCK], [0.0f32; SUB_BLOCK]);
        dl[..n].copy_from_slice(l);
        dr[..n].copy_from_slice(r);
        self.detect_hp.process(&mut dl[..n], &mut dr[..n], &self.hp_coefs);
        let mut peak_env = 0.0f32;
        for (a, b) in dl[..n].iter().zip(&dr[..n]) {
            let m = a.abs().max(b.abs());
            let p = if m > TINY { m * m } else { 0.0 };
            self.env += self.env_coef * (p - self.env);
            peak_env = peak_env.max(self.env);
        }
        let level_db = 10.0 * (peak_env + SILENCE).log10();
        let noise_db = self.noise.update(level_db, n);

        let target = curve(level_db, self.amount);
        let dt = n;
        let coef = if self.amount <= 0.0 {
            one_pole(OFF_S, dt, self.fs)
        } else if target < self.gain_db {
            // Louder input: attack, whatever the noise floor (a loud talker is always turned
            // down at once). Below the gate the input has fallen silent: hold, so the next
            // callout starts at the gain the last phrase ended with.
            if level_db >= GATE_DB { one_pole(ATTACK_S, dt, self.fs) } else { 0.0 }
        } else if level_db >= GATE_DB && level_db >= noise_db + NOISE_MARGIN_DB {
            one_pole(RELEASE_S, dt, self.fs)
        } else {
            0.0
        };
        approach(&mut self.gain_db, target, coef, 1e-3);
        if !self.gain_db.is_finite() {
            self.gain_db = 0.0;
        }

        self.gain.set(db_to_gain(self.gain_db), n);
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            let g = self.gain.next();
            *a *= g;
            *b *= g;
        }
        self.gain.finish();
    }

    /// Forgets the level and returns to unity gain.
    pub fn reset(&mut self) {
        self.detect_hp.reset();
        self.env = 0.0;
        self.gain_db = 0.0;
        self.gain.snap(1.0);
        self.noise.reset();
    }

    pub fn flush(&mut self) {
        self.detect_hp.flush();
        if self.env < 1e-20 {
            self.env = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = 48_000.0;

    /// Syllable-like bursts: 300 ms of signal, 40 ms of silence.
    fn talking(frame: usize, fs: f32) -> bool {
        (frame as f32 / fs) % 0.34 < 0.3
    }

    /// Small deterministic white noise in [-1, 1).
    fn white(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (*seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
    }

    fn feed(lev: &mut Leveler, db: f32, secs: f32, amount: f32) -> f32 {
        feed_at(FS, lev, db, secs, amount)
    }

    /// Feeds `secs` of a 1 kHz tone at `db` dBFS peak (stereo, identical channels) at `fs`, in
    /// syllable-like bursts ([`talking`]); returns the output peak relative to the input peak, in
    /// dB, of the last sub-block that had signal.
    fn feed_at(fs: f32, lev: &mut Leveler, db: f32, secs: f32, amount: f32) -> f32 {
        let amp = db_to_gain(db);
        let blocks = (secs * fs / SUB_BLOCK as f32) as usize;
        let mut phase = 0usize;
        let mut ratio = 0.0;
        for _ in 0..blocks {
            let mut l = [0.0f32; SUB_BLOCK];
            for v in l.iter_mut() {
                let on = if talking(phase, fs) { amp } else { 0.0 };
                *v = on * (phase as f32 * std::f32::consts::TAU * 1000.0 / fs).sin();
                phase += 1;
            }
            let mut r = l;
            let input = l;
            lev.process(&mut l, &mut r, amount);
            let pin = input.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            let pout = l.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert_eq!(l, r, "stereo-linked");
            if pin > 0.0 {
                ratio = 20.0 * (pout / pin).log10();
            }
        }
        ratio
    }

    #[test]
    fn amount_zero_is_bit_exact() {
        let mut lev = Leveler::new(FS);
        let mut l: [f32; SUB_BLOCK] = std::array::from_fn(|i| (i as f32 * 0.37).sin() * 0.01);
        let mut r: [f32; SUB_BLOCK] = std::array::from_fn(|i| (i as f32 * 0.11).cos() * 0.9);
        let (l0, r0) = (l, r);
        for _ in 0..100 {
            lev.process(&mut l, &mut r, 0.0);
        }
        assert_eq!((l, r), (l0, r0));
    }

    #[test]
    fn curve_levels_toward_the_reference_and_spares_the_floor() {
        for amount in [0.2f32, 0.6, 1.0] {
            assert!(curve(REF_DB, amount).abs() < 1e-4, "{amount}: the reference passes at unity");
            assert!(curve(REF_DB - 12.0, amount) > 0.0 && curve(REF_DB + 12.0, amount) < 0.0);
            assert_eq!(curve(FLOOR_DB, amount), 0.0, "{amount}: nothing at the floor is lifted");
            assert_eq!(curve(-90.0, amount), 0.0);
        }
        // 12 dB between two talkers shrinks to well under 5 dB at amount 0.6.
        let a = 0.6;
        let spread = 12.0 + curve(REF_DB, a) - curve(REF_DB - 12.0, a);
        assert!(spread < 4.5, "{spread}");
        // More amount, more levelling, monotonically.
        let mut last = 12.0;
        for k in 1..=10 {
            let a = k as f32 / 10.0;
            let s = 12.0 + curve(REF_DB, a) - curve(REF_DB - 12.0, a);
            assert!(s <= last + 1e-4, "{a}: {s} > {last}");
            last = s;
        }
        assert_eq!(curve(-30.0, 0.0), 0.0);
    }

    #[test]
    fn quiet_input_is_lifted_loud_input_cut() {
        let mut lev = Leveler::new(FS);
        // A sine's RMS is 3 dB under its peak.
        let quiet = feed(&mut lev, REF_DB - 9.0, 2.0, 0.6);
        assert!(quiet > 4.0, "quiet tone lifted by {quiet:.2} dB");
        let loud = feed(&mut lev, REF_DB + 15.0, 1.0, 0.6);
        assert!(loud < -4.0, "loud tone cut by {loud:.2} dB");
    }

    #[test]
    fn the_same_at_every_sample_rate() {
        // Time constants are in seconds and the curve in dB, so a device rate change does not
        // change how a voice is levelled.
        let at = |fs: f32| {
            let mut lev = Leveler::new(fs);
            let quiet = feed_at(fs, &mut lev, REF_DB - 9.0, 2.0, 0.6);
            let loud = feed_at(fs, &mut lev, REF_DB + 15.0, 1.0, 0.6);
            (quiet, loud)
        };
        let (q48, l48) = at(48_000.0);
        for fs in [22_050.0, 44_100.0, 96_000.0, 192_000.0] {
            let (q, l) = at(fs);
            assert!((q - q48).abs() < 0.3 && (l - l48).abs() < 0.3, "@ {fs}: {q:.2}/{l:.2} vs {q48:.2}/{l48:.2}");
        }
    }

    #[test]
    fn noise_floor_is_never_lifted_and_pauses_hold() {
        let mut lev = Leveler::new(FS);
        // Low-level hiss alone: no lift, ever.
        let g = feed(&mut lev, -55.0, 3.0, 1.0);
        assert!(g.abs() < 0.01, "{g}");
        assert!(lev.gain_db().abs() < 1e-3);

        // A quiet talker gets lifted; in the pause that follows the gain does not rise further.
        let mut lev = Leveler::new(FS);
        feed(&mut lev, -30.0, 2.0, 1.0);
        let speaking = lev.gain_db();
        assert!(speaking > 3.0);
        feed(&mut lev, -70.0, 0.3, 1.0);
        let paused = lev.gain_db();
        // (The detector needs ~15 ms to fall through the gate, during which the release may add
        // a fraction of a dB.)
        assert!(paused <= speaking + 0.3, "pause raised the gain: {speaking:.2} -> {paused:.2}");
        assert!(paused > 0.8 * speaking, "a short pause keeps the lift: {speaking:.2} -> {paused:.2}");
        // A long silence keeps it too: the next callout must not start quieter than the last.
        feed(&mut lev, -90.0, 10.0, 1.0);
        assert!((lev.gain_db() - paused).abs() < 0.1, "{paused:.2} -> {:.2}", lev.gain_db());
        // And a louder talker after the silence is turned down within the attack time.
        let loud = feed(&mut lev, REF_DB + 12.0, 0.1, 1.0);
        assert!(loud < 0.0, "{loud:+.2} dB");
    }

    /// Runs `secs` of tone bursts at `talker` dBFS peak, or silence if `None`, over steady white
    /// noise at `noise` dBFS RMS; returns the gain at the end.
    fn over_noise(lev: &mut Leveler, talker: Option<f32>, noise: f32, secs: f32, seed: &mut u32) -> f32 {
        let amp = talker.map_or(0.0, db_to_gain);
        let floor = db_to_gain(noise) * 3f32.sqrt();
        let mut phase = 0usize;
        for _ in 0..(secs * FS / SUB_BLOCK as f32) as usize {
            let mut l = [0.0f32; SUB_BLOCK];
            let mut r = [0.0f32; SUB_BLOCK];
            for (a, b) in l.iter_mut().zip(r.iter_mut()) {
                let on = if talking(phase, FS) { amp } else { 0.0 };
                let s = on * (phase as f32 * std::f32::consts::TAU * 1000.0 / FS).sin();
                *a = s + floor * white(seed);
                *b = s + floor * white(seed);
                phase += 1;
            }
            lev.process(&mut l, &mut r, 1.0);
        }
        lev.gain_db()
    }

    #[test]
    fn steady_noise_above_the_gate_does_not_swell_in_pauses() {
        // An unsuppressed mic or room tone 25-35 dB under the voice: louder than the gate, so
        // only the noise-floor rule keeps it from rising by most of the make-up gain (up to
        // +17 dB here) in every pause.
        for noise in [-44.0f32, -40.0, -34.0] {
            let mut seed = 5;
            let mut lev = Leveler::new(FS);
            let speaking = over_noise(&mut lev, Some(REF_DB + 3.0), noise, 3.0, &mut seed);
            let paused = over_noise(&mut lev, None, noise, 2.0, &mut seed);
            assert!(paused <= speaking + 0.5, "{noise} dBFS noise: {speaking:+.2} -> {paused:+.2} dB in the pause");
        }
        // Noise alone, from the start: never lifted.
        let mut seed = 9;
        let mut lev = Leveler::new(FS);
        let g = over_noise(&mut lev, None, -38.0, 3.0, &mut seed);
        assert!(g.abs() < 0.5, "{g:+.2} dB");
        // A quiet talker well clear of the noise is still lifted.
        let quiet = over_noise(&mut lev, Some(REF_DB - 9.0), -60.0, 3.0, &mut seed);
        assert!(quiet > 4.0, "quiet talker only at {quiet:+.2} dB");
    }

    #[test]
    fn switching_off_returns_to_exact_unity() {
        let mut lev = Leveler::new(FS);
        feed(&mut lev, -35.0, 2.0, 0.8);
        assert!(lev.gain_db() > 3.0);
        feed(&mut lev, -35.0, 1.0, 0.0);
        assert_eq!(lev.gain_db(), 0.0);
        let mut l = [0.123f32; SUB_BLOCK];
        let mut r = [-0.5f32; SUB_BLOCK];
        lev.process(&mut l, &mut r, 0.0);
        assert_eq!((l[SUB_BLOCK - 1], r[0]), (0.123, -0.5));
    }

    #[test]
    fn gain_moves_smoothly() {
        // 20 dB level jumps with syllable gaps, so the gain both falls and rises: it glides, it
        // never steps by more than a fraction of a dB per sample. The right channel carries a
        // small DC (removed by the detector's high-pass), so it shows the applied gain sample by
        // sample.
        let mut lev = Leveler::new(FS);
        let dc = 1e-3f32;
        let (mut last, mut worst, mut phase) = (1.0f32, 0.0f32, 0usize);
        let (mut lowest, mut highest) = (f32::MAX, f32::MIN);
        for k in 0..2000 {
            let amp = if (k / 300) % 2 == 0 { 0.02 } else { 0.2 };
            let mut l = [0.0f32; SUB_BLOCK];
            for v in l.iter_mut() {
                let on = if talking(phase, FS) { amp } else { 0.0 };
                *v = on * (phase as f32 * std::f32::consts::TAU * 1000.0 / FS).sin();
                phase += 1;
            }
            let mut r = [dc; SUB_BLOCK];
            lev.process(&mut l, &mut r, 1.0);
            for v in r {
                let g = v / dc;
                worst = worst.max((20.0 * (g / last).log10()).abs());
                last = g;
            }
            if k > 600 {
                lowest = lowest.min(lev.gain_db());
                highest = highest.max(lev.gain_db());
            }
        }
        assert!(highest - lowest > 10.0, "the gain moved {lowest:+.1}..{highest:+.1} dB");
        assert!(worst < 0.1, "{worst} dB in one sample");
    }
}
