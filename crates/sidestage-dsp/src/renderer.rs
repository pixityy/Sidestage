//! The real-time renderer: wires the leveller, voices, room, ducker and limiter together.
//!
//! ```text
//! music ─► leveller ─┬─► voice (2 virtual speakers ─► ITD + head shadow ─► rear/elevation cues
//!                    │          ─► distance) + room wet
//!                    │      ─► tone (HPF, muffle LPF, bass shelf, presence dip, clarity bell)
//!                    └─► room (early reflections + FDN late reverb, constant send)
//!   ─► ducker (dynamic 3.2 kHz dip + broadband gain) ─► volume ─► limiter ─► bypass crossfade
//! ```
//!
//! Audio is processed in sub-blocks of at most [`SUB_BLOCK`] frames: control values (smoothed
//! parameters, filter coefficient targets, ducking) update once per sub-block and glide per
//! sample within it, so any block length works and nothing steps.

use std::sync::Arc;

use crate::biquad::{Svf2, SvfCoefs};
use crate::ducker::Ducker;
use crate::leveler::Leveler;
use crate::limiter::Limiter;
use crate::params::{Settings, SharedParams};
use crate::reverb::Reverb;
use crate::smoothing::{FadeIn, Ramp, SUB_BLOCK, approach, db_to_gain, one_pole, smoothstep};
use crate::voice::Voice;

/// Crossfade between the old and the new rendering after a far settings jump.
const VOICE_XFADE_S: f32 = 0.05;
/// Crossfade between processed and untouched audio when bypass is toggled.
const BYPASS_XFADE_S: f32 = 0.02;
/// A renderer is usually created while the music is already playing (session start, device or
/// sample-rate change), so its input fades in over this time. Starting abruptly mid-waveform
/// would click, and every room reflection would replay that click up to ~200 ms later.
const START_FADE_S: f32 = 0.02;
const VOLUME_TAU_S: f32 = 0.02;
/// Level a typical stereo mix loses in the binaural rendering: the two channels only partly add
/// up at each ear (they arrive with different delays and head shadow), and the rear EQ takes
/// some presence out. With it, `volume_db` = 0 at the default distance plays about as loud as
/// the original.
const MAKEUP_DB: f32 = 3.0;
/// Dynamic cut where footsteps live, deepening with the ducking reduction.
const DUCK_DIP_HZ: f32 = 3200.0;
const DUCK_DIP_Q: f32 = 0.7;
/// Input samples beyond ±16 (+24 dBFS) are treated as a fault and clamped.
const INPUT_LIMIT: f32 = 16.0;
/// Internal sample-rate range; any device rate is accepted, the maths is clamped to this.
const MIN_FS: f32 = 8000.0;
const MAX_FS: f32 = 768_000.0;

/// The real-time binaural renderer.
pub struct Spatializer {
    sample_rate: u32,
    fs: f32,
    params: Arc<SharedParams>,
    settings: Settings,
    /// Evens out the input level before anything else hears it ([`Settings::leveler`]).
    leveler: Leveler,
    /// Two renderings of the direct sound, so far jumps crossfade instead of sweeping.
    voices: [Voice; 2],
    active: usize,
    /// Progress (0..1) of a crossfade from `voices[active]` to the other voice.
    xfade: Option<f32>,
    xfade_mix: Ramp,
    reverb: Reverb,
    ducker: Ducker,
    duck_eq: Svf2,
    volume: f32,
    out_gain: Ramp,
    limiter: Limiter,
    bypass_pos: f32,
    bypass_mix: Ramp,
    /// Fully bypassed: the renderer's state has been cleared and nothing but the copy runs.
    idle: bool,
    /// Fades the input in after creation (and after a fault reset).
    start: FadeIn,
}

#[inline]
fn clean_input(x: f32) -> f32 {
    // Non-finite samples become silence, denormal-sized ones too (they would slow the filters).
    if x.is_finite() && x.abs() >= 1e-30 { x.clamp(-INPUT_LIMIT, INPUT_LIMIT) } else { 0.0 }
}

#[inline]
fn guard_output(x: f32) -> f32 {
    if x.is_finite() { x.clamp(-1.0, 1.0) } else { 0.0 }
}

impl Spatializer {
    /// Allocates all buffers; call off the real-time thread. The input fades in over the first
    /// 20 ms, since a renderer usually starts while the music is already playing.
    pub fn new(sample_rate: u32, params: Arc<SharedParams>) -> Self {
        let fs = (sample_rate as f32).clamp(MIN_FS, MAX_FS);
        let settings = params.get().sanitized();
        let bypass = if settings.bypass { 1.0 } else { 0.0 };
        let volume = db_to_gain(settings.volume_db + MAKEUP_DB);
        let mut reverb = Reverb::new(fs, settings.room, settings.reverb_db);
        if settings.bypass {
            // Starting idle: the music will already be playing when rendering starts, so the
            // room must let it in gradually, exactly as after a reset.
            reverb.clear();
        }
        Self {
            sample_rate,
            fs,
            leveler: Leveler::new(fs),
            voices: [Voice::new(fs, &settings), Voice::new(fs, &settings)],
            active: 0,
            xfade: None,
            xfade_mix: Ramp::new(0.0),
            reverb,
            ducker: Ducker::default(),
            duck_eq: Svf2::new(SvfCoefs::bell(fs, DUCK_DIP_HZ, DUCK_DIP_Q, 0.0)),
            volume,
            out_gain: Ramp::new(volume),
            limiter: Limiter::new(fs),
            bypass_pos: bypass,
            bypass_mix: Ramp::new(bypass),
            idle: settings.bypass,
            start: FadeIn::CLOSED,
            params,
            settings,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The settings the last block was rendered with (for the app's real-time wrapper, e.g. to
    /// see whether this source is voice chat).
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Render one block. `input`/`output`: interleaved stereo, equal length, any even size.
    /// `sidechain_peak`: recent linear peak of the game (0 if unavailable).
    /// Real-time safe: no allocation, locks or panics.
    pub fn process(&mut self, input: &[f32], output: &mut [f32], sidechain_peak: f32) {
        self.process_with_voice(input, output, sidechain_peak, 0.0);
    }

    /// [`Spatializer::process`] with a second ducking trigger: `voice_peak` is the recent linear
    /// peak of the session's voice-chat sources ([`Settings::voice_chat`]), 0 if none. The ducker
    /// follows the game when [`crate::Ducking::enabled`] and the voice when
    /// [`crate::Ducking::under_voice`], whichever is louder. The sidechain meter always shows
    /// `game_peak`, whether or not this source ducks for it.
    pub fn process_with_voice(&mut self, input: &[f32], output: &mut [f32], game_peak: f32, voice_peak: f32) {
        if let Some(s) = self.params.try_get() {
            self.settings = s.sanitized();
        }
        let frames = input.len().min(output.len()) / 2;
        let (output, rest) = output.split_at_mut(frames * 2);
        rest.fill(0.0);
        let clean = |p: f32| if p.is_finite() { p.abs().min(INPUT_LIMIT) } else { 0.0 };
        let d = self.settings.ducking;
        // The meter shows the game level even while ducking under it is off.
        let game_level = clean(game_peak);
        let game = if d.enabled { game_level } else { 0.0 };
        let voice = if d.under_voice { clean(voice_peak) } else { 0.0 };
        let sidechain = game.max(voice);

        let (mut in_peak, mut out_peak) = (0.0f32, 0.0f32);
        for (inp, out) in input[..frames * 2].chunks(2 * SUB_BLOCK).zip(output.chunks_mut(2 * SUB_BLOCK)) {
            let (i, o) = self.sub_block(inp, out, sidechain);
            in_peak = in_peak.max(i);
            out_peak = out_peak.max(o);
        }
        self.params.meters.report(in_peak, out_peak, game_level, self.ducker.reduction_db());
    }

    /// One sub-block of at most [`SUB_BLOCK`] interleaved frames. Returns (input, output) peaks.
    fn sub_block(&mut self, input: &[f32], output: &mut [f32], sidechain: f32) -> (f32, f32) {
        let n = (input.len() / 2).min(SUB_BLOCK);
        let s = self.settings;
        let mut x = [[0.0f32; SUB_BLOCK]; 2];
        let mut in_peak = 0.0f32;
        for (i, &[l, r]) in input.as_chunks::<2>().0.iter().take(n).enumerate() {
            let (l, r) = (clean_input(l), clean_input(r));
            x[0][i] = l;
            x[1][i] = r;
            in_peak = in_peak.max(l.abs()).max(r.abs());
        }
        let mut fade = [0.0f32; SUB_BLOCK];
        if self.start.gains(&mut fade[..n], START_FADE_S * self.fs) {
            for ch in &mut x {
                ch.iter_mut().zip(&fade[..n]).for_each(|(v, g)| *v *= g);
            }
        }

        // The ducker keeps tracking while bypassed so its meter stays live.
        let reduction = self.ducker.update(sidechain, &s.ducking, n as f32 / self.fs);

        let step = n as f32 / (BYPASS_XFADE_S * self.fs);
        self.bypass_pos = if s.bypass { (self.bypass_pos + step).min(1.0) } else { (self.bypass_pos - step).max(0.0) };
        if self.idle && self.bypass_pos < 1.0 {
            // Leaving full bypass: rendering restarts from the cleared state, already at the
            // current settings, while the crossfade brings it in from silence.
            self.idle = false;
            for v in &mut self.voices {
                v.snap(&s);
            }
        }
        self.bypass_mix.set(smoothstep(self.bypass_pos), n);

        let mut y = [[0.0f32; SUB_BLOCK]; 2];
        if !self.idle {
            self.render(&x, &mut y, n, reduction);
        }

        let mut out_peak = 0.0f32;
        for (i, frame) in output.as_chunks_mut::<2>().0.iter_mut().take(n).enumerate() {
            let m = self.bypass_mix.next();
            let l = guard_output((1.0 - m) * y[0][i] + m * x[0][i].clamp(-1.0, 1.0));
            let r = guard_output((1.0 - m) * y[1][i] + m * x[1][i].clamp(-1.0, 1.0));
            frame[0] = l;
            frame[1] = r;
            out_peak = out_peak.max(l.abs()).max(r.abs());
        }
        self.bypass_mix.finish();

        if !self.idle && self.bypass_pos >= 1.0 {
            // Fully bypassed: clear everything so un-bypassing starts fresh, then stop rendering.
            self.reset_state();
            self.idle = true;
        }
        (in_peak, out_peak)
    }

    fn render(&mut self, x: &[[f32; SUB_BLOCK]; 2], y: &mut [[f32; SUB_BLOCK]; 2], n: usize, reduction: f32) {
        let s = self.settings;
        // The leveller works on a copy: the bypass crossfade needs the untouched input.
        let mut lev = *x;
        {
            let [ll, lr] = &mut lev;
            self.leveler.process(&mut ll[..n], &mut lr[..n], s.leveler);
        }
        let xin = [&lev[0][..n], &lev[1][..n]];

        let mut wet = [[0.0f32; SUB_BLOCK]; 2];
        self.reverb.update(s.room, s.reverb_db, n);
        {
            let [wl, wr] = &mut wet;
            self.reverb.process(xin, [&mut wl[..n], &mut wr[..n]]);
        }
        let wet_in = [&wet[0][..n], &wet[1][..n]];

        if self.xfade.is_none() && self.voices[self.active].is_far_from(&s) {
            let [v0, v1] = &mut self.voices;
            let (from, to) = if self.active == 0 { (&*v0, v1) } else { (&*v1, v0) };
            to.copy_from(from);
            to.snap(&s);
            self.xfade = Some(0.0);
            self.xfade_mix.snap(0.0);
        }

        let [yl, yr] = y;
        let (yl, yr) = (&mut yl[..n], &mut yr[..n]);
        match self.xfade {
            None => {
                let v = &mut self.voices[self.active];
                v.update(&s, n);
                v.process(xin, wet_in, [&mut *yl, &mut *yr]);
            }
            Some(pos) => {
                let next = (pos + n as f32 / (VOICE_XFADE_S * self.fs)).min(1.0);
                self.xfade_mix.set(smoothstep(next), n);
                let incoming = 1 - self.active;
                // The outgoing voice stays where it was; the incoming one follows the settings.
                let mut old = [[0.0f32; SUB_BLOCK]; 2];
                {
                    let [ol, or] = &mut old;
                    self.voices[self.active].process(xin, wet_in, [&mut ol[..n], &mut or[..n]]);
                }
                let v = &mut self.voices[incoming];
                v.update(&s, n);
                v.process(xin, wet_in, [&mut *yl, &mut *yr]);
                for ((a, b), (oa, ob)) in yl.iter_mut().zip(yr.iter_mut()).zip(old[0].iter().zip(&old[1])) {
                    let w = self.xfade_mix.next();
                    *a = oa + w * (*a - oa);
                    *b = ob + w * (*b - ob);
                }
                self.xfade_mix.finish();
                if next >= 1.0 {
                    self.active = incoming;
                    self.xfade = None;
                } else {
                    self.xfade = Some(next);
                }
            }
        }

        // Ducking: a presence cut that deepens with the reduction, then the broadband gain.
        let k = &s.ducking;
        let dip = if k.depth_db > 0.01 { -k.band_dip_db * (reduction / k.depth_db).min(1.0) } else { 0.0 };
        self.duck_eq.process(yl, yr, &SvfCoefs::bell(self.fs, DUCK_DIP_HZ, DUCK_DIP_Q, dip));
        approach(&mut self.volume, db_to_gain(s.volume_db + MAKEUP_DB), one_pole(VOLUME_TAU_S, n, self.fs), 1e-7);
        self.out_gain.set(self.volume * db_to_gain(-reduction), n);
        let mut healthy = true;
        for (a, b) in yl.iter_mut().zip(yr.iter_mut()) {
            let g = self.out_gain.next();
            *a *= g;
            *b *= g;
            if !(a.is_finite() && b.is_finite()) {
                healthy = false;
                *a = 0.0;
                *b = 0.0;
            }
        }
        self.out_gain.finish();
        self.limiter.process(yl, yr);

        if healthy {
            self.flush();
        } else {
            // Cannot happen with sanitized inputs, but if a state ever went non-finite it would
            // stay that way forever: start over instead of going silent for good.
            self.reset_state();
            self.start.restart();
        }
    }

    /// Clears all audio memory and jumps to the current settings.
    fn reset_state(&mut self) {
        for v in &mut self.voices {
            v.reset();
            v.snap(&self.settings);
        }
        self.xfade = None;
        self.xfade_mix.snap(0.0);
        self.leveler.reset();
        self.reverb.clear();
        self.duck_eq.reset();
        self.limiter.reset();
    }

    fn flush(&mut self) {
        self.leveler.flush();
        self.voices[self.active].flush();
        if self.xfade.is_some() {
            self.voices[1 - self.active].flush();
        }
        self.reverb.flush();
        self.duck_eq.flush();
    }
}

/// Render a whole interleaved stereo buffer offline with fixed settings (tests, tuning, CLI).
pub fn render_offline(settings: Settings, sample_rate: u32, input: &[f32], sidechain_peak: f32) -> Vec<f32> {
    let mut sp = Spatializer::new(sample_rate, SharedParams::new(settings));
    let mut out = vec![0.0; input.len()];
    for (i, o) in input.chunks(1024).zip(out.chunks_mut(1024)) {
        sp.process(i, o, sidechain_peak);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spatializer_can_move_to_the_audio_thread() {
        fn assert_send<T: Send + 'static>() {}
        assert_send::<Spatializer>();
    }
}
