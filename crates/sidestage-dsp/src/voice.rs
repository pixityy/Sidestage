//! One rendering of the music: two virtual speakers through the spherical-head model, rear and
//! elevation cues, distance, and the user's tone controls (applied to direct sound plus room).
//!
//! The renderer owns two voices so that a far jump in settings (a preset change, a click across
//! the radar) can crossfade between the old and the new rendering instead of audibly sweeping
//! through every position in between.

use std::f32::consts::{FRAC_1_SQRT_2, LN_2};

use crate::biquad::{Svf, Svf2, SvfCoefs};
use crate::delay::DelayLine;
use crate::head::{self, HeadShadow};
use crate::params::Settings;
use crate::smoothing::{Ramp, SUB_BLOCK, approach, one_pole, wrap_deg};

/// Common latency of every ear path in samples; keeps the Lagrange taps causal.
const BASE_DELAY: f32 = 2.0;
/// Each speaker contributes half, so a mono source (or width 0) keeps unity gain in the bass,
/// where both speakers add up coherently at each ear.
const SPEAKER_GAIN: f32 = 0.5;
/// Parameter smoothing. The UI sends updates at ~60 Hz; 30 ms bridges the steps between them.
const PARAM_TAU_S: f32 = 0.03;

/// Rear cues from Blauert's directional bands (So et al. 2011, Human Factors): 2.9-5.8 kHz reads
/// as "front", 0.72-1.8 kHz and 10.3-14.9 kHz as "back". Each is scaled by how far behind the
/// speaker is. The back treble band is boosted, not cut: removing 10-15 kHz pulls sound forward.
const FRONT_BAND: Band = Band { hz: 4200.0, q: 1.2, db: -5.0 };
const BACK_MID: Band = Band { hz: 1150.0, q: 1.0, db: 1.5 };
const BACK_TREBLE: Band = Band { hz: 12_400.0, q: 1.4, db: 3.0 };
/// Elevation: a gentle treble lift around 7.5 kHz reads as "above", a darker top as "below".
const ABOVE: Band = Band { hz: 7500.0, q: 1.4, db: 3.0 };
const BELOW: Band = Band { hz: 5000.0, q: FRAC_1_SQRT_2, db: -4.0 };
/// Distance "air": a high shelf losing this much per doubling beyond 1 m. Real air absorption
/// is only a couple of dB at 8 kHz over 20 m; this is a gentle, musical exaggeration that still
/// leaves the 10-15 kHz "behind" band audible at room distances.
const AIR: Band = Band { hz: 10_000.0, q: 0.6, db: -1.2 };

const BASS_HZ: f32 = 150.0;
const PRESENCE_HZ: f32 = 3200.0;
/// Speech clarity: a broad bell over the band that carries most of speech intelligibility
/// (consonants, 1.2-5 kHz at -3 dB for this Q).
const CLARITY_HZ: f32 = 2500.0;
const CLARITY_Q: f32 = 0.8;
/// "Muffle" fades from a plain low-pass to fully open between these cutoffs, so 20 kHz is
/// exactly transparent at every sample rate.
const MUFFLE_OPEN_FROM_HZ: f32 = 16_000.0;
const MUFFLE_OPEN_AT_HZ: f32 = 20_000.0;

/// Settings change farther than this in one go crossfade to a fresh rendering instead of gliding.
const FAR_ANGLE_COS: f32 = 0.643; // 50°
const FAR_DISTANCE_LN: f32 = LN_2; // a doubling or halving
const FAR_WIDTH_DEG: f32 = 50.0;
const FAR_LOWPASS_LN: f32 = 0.92; // x2.5
const FAR_HIGHPASS_LN: f32 = 1.1; // x3
const FAR_TONE_DB: f32 = 6.0;

const CUES: usize = 5;
const TONES: usize = 5;

#[derive(Clone, Copy, Debug)]
struct Band {
    hz: f32,
    q: f32,
    db: f32,
}

/// The parameters a voice smooths. Distance and cutoffs live in the log domain so they glide
/// evenly in perceived terms.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Params {
    /// Unwrapped: it follows the shortest way round, so it may leave -180..180 for a while.
    az: f32,
    el: f32,
    dist_ln: f32,
    width: f32,
    hp_ln: f32,
    lp_ln: f32,
    bass_db: f32,
    presence_db: f32,
    clarity_db: f32,
}

impl Params {
    fn from_settings(s: &Settings) -> Self {
        Self {
            az: s.azimuth_deg,
            el: s.elevation_deg,
            dist_ln: s.distance_m.max(0.01).ln(),
            width: s.width_deg,
            hp_ln: s.highpass_hz.max(1.0).ln(),
            lp_ln: s.lowpass_hz.max(1.0).ln(),
            bass_db: s.bass_db,
            presence_db: s.presence_dip_db,
            clarity_db: s.clarity_db,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct EarPath {
    delay: Ramp,
    delay_target: f32,
    alpha: Ramp,
    alpha_target: f32,
    shadow: HeadShadow,
}

#[derive(Clone, Debug)]
struct Speaker {
    cues: [Svf; CUES],
    cue_targets: [SvfCoefs; CUES],
    line: DelayLine,
    /// `[left ear, right ear]`.
    ears: [EarPath; 2],
}

/// Direct path and tone for one set of (smoothed) settings.
pub struct Voice {
    fs: f32,
    /// 1 when the 10-15 kHz back band fits below Nyquist, fading to 0 at low sample rates.
    back_treble_scale: f32,
    p: Params,
    /// The parameters the current coefficient targets were computed from.
    computed: Params,
    /// `[music left, music right]`.
    speakers: [Speaker; 2],
    air: Svf2,
    air_target: SvfCoefs,
    gain: Ramp,
    gain_target: f32,
    /// High-pass, muffle low-pass, bass shelf, presence dip, clarity.
    tone: [Svf2; TONES],
    tone_targets: [SvfCoefs; TONES],
}

impl Voice {
    /// Allocates the delay lines; call off the real-time thread.
    pub fn new(fs: f32, settings: &Settings) -> Self {
        let line_len = BASE_DELAY as usize + (head::MAX_ITD_S * fs).ceil() as usize + 4 + SUB_BLOCK;
        let flat = SvfCoefs::bell(fs, 1000.0, 1.0, 0.0);
        let ear = EarPath {
            delay: Ramp::new(BASE_DELAY),
            delay_target: BASE_DELAY,
            alpha: Ramp::new(1.0),
            alpha_target: 1.0,
            shadow: HeadShadow::new(fs),
        };
        let speaker = Speaker {
            cues: [Svf::new(flat); CUES],
            cue_targets: [flat; CUES],
            line: DelayLine::new(line_len),
            ears: [ear; 2],
        };
        let p = Params::from_settings(settings);
        let mut v = Self {
            fs,
            back_treble_scale: ((0.45 * fs - 10_300.0) / (BACK_TREBLE.hz - 10_300.0)).clamp(0.0, 1.0),
            p,
            computed: p,
            speakers: [speaker.clone(), speaker],
            air: Svf2::new(flat),
            air_target: flat,
            gain: Ramp::new(0.0),
            gain_target: 0.0,
            tone: [Svf2::new(flat); TONES],
            tone_targets: [flat; TONES],
        };
        v.snap(settings);
        v
    }

    /// Jumps straight to `settings` (no gliding). Only call while this voice is silent or
    /// faded out.
    pub fn snap(&mut self, settings: &Settings) {
        self.p = Params::from_settings(settings);
        self.compute_targets();
        for spk in &mut self.speakers {
            for (f, t) in spk.cues.iter_mut().zip(&spk.cue_targets) {
                f.set_coefs(*t);
            }
            for ear in &mut spk.ears {
                ear.delay.snap(ear.delay_target);
                ear.alpha.snap(ear.alpha_target);
            }
        }
        self.air.set_coefs(self.air_target);
        self.gain.snap(self.gain_target);
        for (f, t) in self.tone.iter_mut().zip(&self.tone_targets) {
            f.set_coefs(*t);
        }
    }

    /// Clears all audio state (delay lines and filter memories).
    pub fn reset(&mut self) {
        for spk in &mut self.speakers {
            spk.cues.iter_mut().for_each(Svf::reset);
            spk.line.clear();
            for ear in &mut spk.ears {
                ear.shadow.reset();
            }
        }
        self.air.reset();
        self.tone.iter_mut().for_each(Svf2::reset);
    }

    /// Takes over another voice's complete state, so a crossfade starts from warm filters and
    /// delay lines instead of silence. No allocation: both voices have the same sizes.
    pub fn copy_from(&mut self, other: &Voice) {
        self.p = other.p;
        self.computed = other.computed;
        for (dst, src) in self.speakers.iter_mut().zip(&other.speakers) {
            dst.cues = src.cues;
            dst.cue_targets = src.cue_targets;
            dst.line.copy_from(&src.line);
            dst.ears = src.ears;
        }
        self.air = other.air;
        self.air_target = other.air_target;
        self.gain = other.gain;
        self.gain_target = other.gain_target;
        self.tone = other.tone;
        self.tone_targets = other.tone_targets;
    }

    /// True when `settings` are so far from where this voice currently is that gliding there
    /// would be an audible sweep.
    pub fn is_far_from(&self, settings: &Settings) -> bool {
        let t = Params::from_settings(settings);
        let p = &self.p;
        let a = head::direction(p.az, p.el);
        let b = head::direction(t.az, t.el);
        let cos = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        cos < FAR_ANGLE_COS
            || (t.dist_ln - p.dist_ln).abs() > FAR_DISTANCE_LN
            || (t.width - p.width).abs() > FAR_WIDTH_DEG
            || (t.lp_ln - p.lp_ln).abs() > FAR_LOWPASS_LN
            || (t.hp_ln - p.hp_ln).abs() > FAR_HIGHPASS_LN
            || (t.bass_db - p.bass_db).abs() > FAR_TONE_DB
            || (t.presence_db - p.presence_db).abs() > FAR_TONE_DB
            || (t.clarity_db - p.clarity_db).abs() > FAR_TONE_DB
    }

    /// Moves the smoothed parameters one sub-block of `n` frames toward `settings` and updates
    /// the coefficient targets the next [`Voice::process`] glides to.
    pub fn update(&mut self, settings: &Settings, n: usize) {
        let t = Params::from_settings(settings);
        let c = one_pole(PARAM_TAU_S, n, self.fs);
        let p = &mut self.p;
        let az_target = p.az + wrap_deg(t.az - p.az);
        approach(&mut p.az, az_target, c, 0.01);
        if p.az.abs() > 360.0 {
            // Keep the unwrapped angle bounded; geometry only uses sin/cos of it.
            p.az = wrap_deg(p.az);
        }
        approach(&mut p.el, t.el, c, 0.01);
        approach(&mut p.width, t.width, c, 0.01);
        approach(&mut p.dist_ln, t.dist_ln, c, 1e-4);
        approach(&mut p.hp_ln, t.hp_ln, c, 1e-4);
        approach(&mut p.lp_ln, t.lp_ln, c, 1e-4);
        approach(&mut p.bass_db, t.bass_db, c, 1e-3);
        approach(&mut p.presence_db, t.presence_db, c, 1e-3);
        approach(&mut p.clarity_db, t.clarity_db, c, 1e-3);
        if self.p != self.computed {
            self.compute_targets();
        }
    }

    fn compute_targets(&mut self) {
        let fs = self.fs;
        let p = self.p;
        let azimuths = head::speaker_azimuths(p.az, p.width);
        for (spk, az) in self.speakers.iter_mut().zip(azimuths) {
            let dir = head::direction(az, p.el);
            let behind = (-dir[0]).max(0.0);
            let up = dir[2].max(0.0);
            let down = (-dir[2]).max(0.0);
            let bell = |b: Band, amount: f32| SvfCoefs::bell(fs, b.hz, b.q, b.db * amount);
            spk.cue_targets = [
                bell(FRONT_BAND, behind),
                bell(BACK_MID, behind),
                bell(BACK_TREBLE, behind * self.back_treble_scale),
                bell(ABOVE, up),
                SvfCoefs::high_shelf(fs, BELOW.hz, BELOW.q, BELOW.db * down),
            ];
            for (ear, theta) in spk.ears.iter_mut().zip(head::incidence(dir)) {
                ear.delay_target = BASE_DELAY + head::woodworth_delay_s(theta) * fs;
                ear.alpha_target = head::shadow_alpha(theta);
            }
        }
        // Distance: -3 dB per doubling (musical, gentler than the physical -6 dB), referenced to
        // 1 m. The room send does not change with distance, so the direct-to-reverberant ratio
        // falls as the music moves away.
        let dist = p.dist_ln.exp();
        self.gain_target = SPEAKER_GAIN / dist.sqrt();
        let doublings = p.dist_ln.max(0.0) / LN_2;
        self.air_target = SvfCoefs::high_shelf(fs, AIR.hz, AIR.q, AIR.db * doublings);

        let lp_hz = p.lp_ln.exp();
        let open = (lp_hz - MUFFLE_OPEN_FROM_HZ) / (MUFFLE_OPEN_AT_HZ - MUFFLE_OPEN_FROM_HZ);
        self.tone_targets = [
            SvfCoefs::highpass(fs, p.hp_ln.exp(), FRAC_1_SQRT_2),
            SvfCoefs::lowpass_open(fs, lp_hz, FRAC_1_SQRT_2, open),
            SvfCoefs::low_shelf(fs, BASS_HZ, FRAC_1_SQRT_2, p.bass_db),
            SvfCoefs::bell(fs, PRESENCE_HZ, 1.0, p.presence_db),
            SvfCoefs::bell(fs, CLARITY_HZ, CLARITY_Q, p.clarity_db),
        ];
        self.computed = p;
    }

    /// Renders one sub-block (at most [`SUB_BLOCK`] frames): `x` is the music, `wet` the room
    /// signal to add before the tone controls, `out` receives the result.
    pub fn process(&mut self, x: [&[f32]; 2], wet: [&[f32]; 2], out: [&mut [f32]; 2]) {
        let lens = [x[0].len(), x[1].len(), wet[0].len(), wet[1].len(), out[0].len(), out[1].len()];
        let n = lens.into_iter().fold(SUB_BLOCK, usize::min);
        let mut bus = [[0.0f32; SUB_BLOCK]; 2];
        for (spk, input) in self.speakers.iter_mut().zip(x) {
            let mut buf = [0.0f32; SUB_BLOCK];
            let buf = &mut buf[..n];
            buf.copy_from_slice(&input[..n]);
            for (f, t) in spk.cues.iter_mut().zip(&spk.cue_targets) {
                f.process(buf, t);
            }
            let start = spk.line.write(buf);
            for (ear, ear_bus) in spk.ears.iter_mut().zip(&mut bus) {
                ear.delay.set(ear.delay_target, n);
                ear.alpha.set(ear.alpha_target, n);
                for (i, b) in ear_bus[..n].iter_mut().enumerate() {
                    let d = ear.delay.next();
                    let a = ear.alpha.next();
                    let s = spk.line.read_lagrange3(start.wrapping_add(i), d);
                    *b += ear.shadow.process(s, a);
                }
                ear.delay.finish();
                ear.alpha.finish();
            }
        }

        let [bus_l, bus_r] = &mut bus;
        self.air.process(&mut bus_l[..n], &mut bus_r[..n], &self.air_target);

        let [out_l, out_r] = out;
        let (out_l, out_r) = (&mut out_l[..n], &mut out_r[..n]);
        self.gain.set(self.gain_target, n);
        let outs = out_l.iter_mut().zip(out_r.iter_mut());
        let ins = bus_l.iter().zip(bus_r.iter()).zip(wet[0].iter().zip(wet[1]));
        for ((ol, or), ((bl, br), (wl, wr))) in outs.zip(ins) {
            let g = self.gain.next();
            *ol = bl * g + wl;
            *or = br * g + wr;
        }
        self.gain.finish();
        for (f, t) in self.tone.iter_mut().zip(&self.tone_targets) {
            f.process(out_l, out_r, t);
        }
    }

    /// Flushes tiny recursive states (once per sub-block).
    pub fn flush(&mut self) {
        for spk in &mut self.speakers {
            spk.cues.iter_mut().for_each(Svf::flush);
            for ear in &mut spk.ears {
                ear.shadow.flush();
            }
        }
        self.air.flush();
        self.tone.iter_mut().for_each(Svf2::flush);
    }
}
