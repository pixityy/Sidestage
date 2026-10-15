//! The room: early reflections plus a feedback-delay-network (FDN) late reverb.
//!
//! Early reflections are taps on a delay line of the dry music, each panned to a lateral or rear
//! direction with a Woodworth ITD and a broadband level difference. The late reverb is an 8-line
//! FDN with a Householder feedback matrix (Jot), fed through series allpass diffusers, with a
//! one-pole loop filter per line that sets separate decay times at DC and Nyquist. Both are
//! low-passed by the room's damping filter (dark through-the-wall sound for `NextRoom`).
//!
//! The wet signal is normalized so that at 0 dB it carries roughly the music's own power:
//! `Settings::reverb_db` then really is "reverb relative to the music", whatever the room size.

use crate::biquad::{Svf2, SvfCoefs};
use crate::delay::DelayLine;
use crate::head;
use crate::params::Room;
use crate::smoothing::{FadeIn, Ramp, SUB_BLOCK, approach, db_to_gain, flush, one_pole};

const FDN_N: usize = 8;
/// Mutually prime FDN delays at 48 kHz (21-41 ms); scaled per room and sample rate.
const FDN_LEN_48K: [f32; FDN_N] = [1031.0, 1153.0, 1279.0, 1409.0, 1523.0, 1663.0, 1789.0, 1951.0];
/// Series allpass diffusers in front of the FDN, `[left, right]` in ms for a room of size 1
/// (scaled with the room). Without them an 8-line FDN has a grainy, sparse onset on music; with
/// fixed long ones a short room would ring for as long as the diffusers do.
const DIFFUSER_MS: [[f32; 4]; 2] = [[4.77, 3.59, 12.73, 9.31], [5.03, 3.31, 11.87, 8.69]];
const DIFFUSER_GAIN: [f32; 4] = [0.65, 0.65, 0.55, 0.55];
/// When the room type changes the wet signal dips out over this time, the room is swapped while
/// silent, and it fades back in. Cheaper than running two rooms, and inaudible next to the dry.
const SWITCH_FADE_S: f32 = 0.03;
/// What enters a reverb memory that was cleared while the music keeps playing ramps up over this
/// time. A cleared memory fed an ongoing signal is the same as one fed a signal that switched on
/// abruptly mid-waveform: every reflection and every FDN round trip would replay that step as a
/// click, 15-170 ms later, long after the output fade-in has finished.
const ONSET_S: f32 = 0.02;
const LEVEL_TAU_S: f32 = 0.03;
/// Added to every recursive write so the FDN and diffusers settle on a tiny normal value
/// (-360 dBFS) instead of decaying through the slow denormal range during silence.
const DENORMAL_GUARD: f32 = 1e-18;
const MAX_TAPS: usize = 24;

/// Static description of a room.
struct Spec {
    /// Decay time at low and mid frequencies, s.
    t60: f32,
    /// Decay time at Nyquist relative to `t60` (air and furnishing absorb treble faster).
    t60_treble: f32,
    /// FDN delay-length scale: smaller rooms have denser, shorter recirculation.
    size: f32,
    /// Gap before the late reverb starts, ms.
    predelay_ms: f32,
    /// Low-pass on the whole wet signal, Hz.
    damp_hz: f32,
    /// Early-reflection and late-reverb power relative to the music at `reverb_db` = 0.
    early_db: f32,
    late_db: f32,
    /// (ms after the direct sound, dB, azimuth in degrees).
    taps: &'static [(f32, f32, f32)],
}

fn spec(room: Room) -> Option<Spec> {
    Some(match room {
        Room::Off => return None,
        Room::Small => Spec {
            t60: 0.35,
            t60_treble: 0.5,
            size: 0.7,
            predelay_ms: 6.0,
            damp_hz: 7000.0,
            early_db: -3.0,
            late_db: -3.0,
            taps: &[
                (3.1, 0.0, -110.0),
                (4.3, -1.0, 100.0),
                (6.2, -2.0, -60.0),
                (7.4, -2.0, 130.0),
                (9.8, -4.0, 180.0),
                (11.5, -4.0, -140.0),
                (14.0, -6.0, 70.0),
                (17.2, -7.0, -95.0),
            ],
        },
        Room::Living => Spec {
            t60: 0.5,
            t60_treble: 0.4,
            size: 0.85,
            predelay_ms: 9.0,
            damp_hz: 5500.0,
            early_db: -3.5,
            late_db: -2.5,
            taps: &[
                (5.0, 0.0, -100.0),
                (6.6, 0.0, 115.0),
                (8.9, -2.0, -140.0),
                (11.3, -2.0, 65.0),
                (14.1, -4.0, 180.0),
                (17.6, -5.0, -70.0),
                (21.4, -6.0, 125.0),
                (25.3, -7.0, -120.0),
            ],
        },
        Room::Hall => Spec {
            t60: 1.6,
            t60_treble: 0.5,
            size: 1.3,
            predelay_ms: 18.0,
            damp_hz: 6000.0,
            early_db: -6.0,
            late_db: -1.5,
            taps: &[
                (11.0, 0.0, -70.0),
                (15.0, 0.0, 85.0),
                (21.0, -2.0, -130.0),
                (26.0, -2.0, 140.0),
                (33.0, -4.0, 180.0),
                (41.0, -5.0, -100.0),
                (50.0, -6.0, 110.0),
                (59.0, -7.0, -160.0),
            ],
        },
        Room::NextRoom => Spec {
            t60: 0.7,
            t60_treble: 0.5,
            size: 1.0,
            predelay_ms: 8.0,
            damp_hz: 1200.0,
            early_db: -4.0,
            late_db: -2.0,
            taps: &[
                (6.0, 0.0, -120.0),
                (9.0, -1.0, 110.0),
                (13.0, -2.0, 180.0),
                (17.0, -3.0, -80.0),
                (22.0, -4.0, 140.0),
                (28.0, -5.0, -150.0),
            ],
        },
        Room::Desk => Spec {
            t60: 0.2,
            t60_treble: 0.6,
            size: 0.45,
            predelay_ms: 2.0,
            damp_hz: 8000.0,
            early_db: -1.5,
            late_db: -6.0,
            taps: &[
                (0.9, 0.0, -80.0),
                (1.4, -1.0, 95.0),
                (2.1, -1.0, -120.0),
                (2.8, -2.0, 60.0),
                (3.6, -3.0, 150.0),
                (4.5, -3.0, -150.0),
                (5.3, -4.0, 110.0),
                (6.4, -5.0, -60.0),
                (7.5, -6.0, 180.0),
                (8.8, -7.0, 85.0),
            ],
        },
    })
}

/// Nominal decay time of a room (s), `None` for [`Room::Off`].
#[cfg(test)]
pub fn nominal_t60(room: Room) -> Option<f32> {
    spec(room).map(|s| s.t60)
}

fn room_index(room: Room) -> usize {
    Room::ALL.iter().position(|r| *r == room).unwrap_or(0)
}

/// One early-reflection read: a tap on input line `line`, heard at each ear with its own delay
/// (the ITD) and gain (the level difference).
#[derive(Clone, Copy, Debug, Default)]
struct Tap {
    line: usize,
    delay: [usize; 2],
    gain: [f32; 2],
}

/// A room prepared for one sample rate.
struct Config {
    taps: [Tap; MAX_TAPS],
    n_taps: usize,
    lens: [usize; FDN_N],
    g: [f32; FDN_N],
    p: [f32; FDN_N],
    predelay: usize,
    diffusers: [[usize; 4]; 2],
    damp: SvfCoefs,
    late_gain: f32,
}

impl Config {
    fn new(spec: &Spec, fs: f32) -> Self {
        // Early reflections: ITD from the head model, a broadband 0..-8 dB level difference.
        let mut taps = [Tap::default(); MAX_TAPS];
        let mut n_taps = 0;
        for &(ms, db, az) in spec.taps {
            let dir = head::direction(az, 0.0);
            let theta = head::incidence(dir);
            let at_ear = |e: usize| {
                let delay = ((ms * 1e-3 + head::woodworth_delay_s(theta[e])) * fs).round() as usize;
                let gain = db_to_gain(db - 4.0 * (1.0 - theta[e].cos()));
                (delay, gain)
            };
            let (dl, gl) = at_ear(0);
            let (dr, gr) = at_ear(1);
            // Reflections on the left come from the music's left channel, and vice versa;
            // centred ones take half of each.
            let sources: &[(usize, f32)] = if az < -1.0 {
                &[(0, 1.0)]
            } else if az > 1.0 && az < 179.0 {
                &[(1, 1.0)]
            } else {
                &[(0, 0.5), (1, 0.5)]
            };
            for &(line, w) in sources {
                if n_taps < MAX_TAPS {
                    taps[n_taps] = Tap { line, delay: [dl, dr], gain: [gl * w, gr * w] };
                    n_taps += 1;
                }
            }
        }
        // Normalize the reflections' power (per ear, uncorrelated unit-power channels).
        let power: f32 = taps[..n_taps].iter().map(|t| 0.5 * (t.gain[0].powi(2) + t.gain[1].powi(2))).sum();
        let norm = db_to_gain(spec.early_db) / power.max(1e-9).sqrt();
        for t in &mut taps[..n_taps] {
            t.gain[0] *= norm;
            t.gain[1] *= norm;
        }

        let mut lens = [0usize; FDN_N];
        let mut g = [0.0f32; FDN_N];
        let mut p = [0.0f32; FDN_N];
        let mut loop_power = 0.0f32;
        for i in 0..FDN_N {
            lens[i] = nearest_prime((FDN_LEN_48K[i] * spec.size * fs / 48_000.0).round().max(2.0) as usize);
            // Per-line one-pole loop filter g/(1 - p z^-1) with gain r0 at DC and rpi at Nyquist,
            // the round-trip losses for the two decay times (J.O. Smith, PASP).
            let m = lens[i] as f32;
            let r0 = 10f32.powf(-3.0 * m / (fs * spec.t60));
            let rp = 10f32.powf(-3.0 * m / (fs * spec.t60 * spec.t60_treble));
            g[i] = 2.0 * r0 * rp / (r0 + rp);
            p[i] = (r0 - rp) / (r0 + rp);
            // White-noise power gain of one pass through the line.
            loop_power += g[i] * g[i] / (1.0 - p[i] * p[i]) / FDN_N as f32;
        }
        // The Householder matrix is lossless, so with unit-power input on every line the power
        // inside settles at 1/(1 - loop_power) and each output (half the sum of four lines)
        // carries loop_power/(1 - loop_power).
        let late_power = loop_power / (1.0 - loop_power).max(1e-6);
        let late_gain = db_to_gain(spec.late_db) / late_power.max(1e-9).sqrt();

        Self {
            taps,
            n_taps,
            lens,
            g,
            p,
            predelay: (spec.predelay_ms * 1e-3 * fs).round() as usize,
            diffusers: DIFFUSER_MS.map(|ch| ch.map(|ms| ((ms * spec.size * 1e-3 * fs).round() as usize).max(1))),
            damp: SvfCoefs::lowpass(fs, spec.damp_hz, 0.6),
            late_gain,
        }
    }

    fn longest_read(&self) -> usize {
        let taps = self.taps[..self.n_taps].iter().flat_map(|t| t.delay);
        taps.chain([self.predelay]).max().unwrap_or(0)
    }
}

fn nearest_prime(n: usize) -> usize {
    let is_prime = |m: usize| m >= 2 && (2..).take_while(|d| d * d <= m).all(|d| !m.is_multiple_of(d));
    (0..n).flat_map(|d| [n + d, n.saturating_sub(d)]).find(|&m| is_prime(m)).unwrap_or(n.max(2))
}

/// Schroeder allpass: flat magnitude, smears transients into a dense onset.
#[derive(Clone, Debug)]
struct Allpass {
    buf: Box<[f32]>,
    len: usize,
    pos: usize,
    g: f32,
}

impl Allpass {
    fn new(max_len: usize, g: f32) -> Self {
        let len = max_len.max(1);
        Self { buf: vec![0.0; len].into_boxed_slice(), len, pos: 0, g }
    }

    fn configure(&mut self, len: usize) {
        self.len = len.clamp(1, self.buf.len());
        self.clear();
    }

    #[inline]
    fn process(&mut self, x: &mut [f32]) {
        for v in x {
            let d = self.buf[self.pos];
            let w = *v + self.g * d + DENORMAL_GUARD;
            self.buf[self.pos] = w;
            *v = d - self.g * w;
            self.pos += 1;
            if self.pos >= self.len {
                self.pos = 0;
            }
        }
    }

    fn clear(&mut self) {
        self.buf.fill(0.0);
        self.pos = 0;
    }
}

/// 8-line FDN with a Householder matrix A = I - (2/N)·11ᵀ: O(N) per sample and lossless, so all
/// decay comes from the loop filters.
struct Fdn {
    lines: [Box<[f32]>; FDN_N],
    len: [usize; FDN_N],
    pos: [usize; FDN_N],
    g: [f32; FDN_N],
    p: [f32; FDN_N],
    lp: [f32; FDN_N],
}

impl Fdn {
    fn new(max_len: [usize; FDN_N]) -> Self {
        Self {
            lines: max_len.map(|l| vec![0.0; l.max(1)].into_boxed_slice()),
            len: max_len.map(|l| l.max(1)),
            pos: [0; FDN_N],
            g: [0.0; FDN_N],
            p: [0.0; FDN_N],
            lp: [0.0; FDN_N],
        }
    }

    fn configure(&mut self, c: &Config) {
        for i in 0..FDN_N {
            self.len[i] = c.lens[i].clamp(1, self.lines[i].len());
        }
        self.g = c.g;
        self.p = c.p;
        self.clear();
    }

    fn clear(&mut self) {
        self.lines.iter_mut().for_each(|l| l.fill(0.0));
        self.pos = [0; FDN_N];
        self.lp = [0.0; FDN_N];
    }

    /// Stereo in, stereo out. Left feeds the even lines and right the odd ones with alternating
    /// signs; the outputs read disjoint line sets, so the two ears get decorrelated tails.
    #[inline]
    fn tick(&mut self, in_l: f32, in_r: f32) -> (f32, f32) {
        let mut s = [0.0f32; FDN_N];
        let mut sum = 0.0;
        for (i, si) in s.iter_mut().enumerate() {
            let v = self.lines[i][self.pos[i]];
            let y = self.g[i] * v + self.p[i] * self.lp[i];
            self.lp[i] = y;
            *si = y;
            sum += y;
        }
        let h = sum * (2.0 / FDN_N as f32);
        let (mut out_l, mut out_r) = (0.0, 0.0);
        for (i, &si) in s.iter().enumerate() {
            let sign = if i & 2 == 0 { 1.0 } else { -1.0 };
            let (input, out) = if i & 1 == 0 { (in_l, &mut out_l) } else { (in_r, &mut out_r) };
            self.lines[i][self.pos[i]] = si - h + sign * input + DENORMAL_GUARD;
            self.pos[i] += 1;
            if self.pos[i] >= self.len[i] {
                self.pos[i] = 0;
            }
            *out += sign * si;
        }
        (0.5 * out_l, 0.5 * out_r)
    }

    fn flush(&mut self) {
        self.lp.iter_mut().for_each(flush);
    }
}

/// Early reflections + late reverb for the current [`Room`], with a click-free room switch.
pub struct Reverb {
    fs: f32,
    configs: [Option<Config>; Room::ALL.len()],
    /// The dry music, kept up to date even while the room is off.
    input: [DelayLine; 2],
    /// Ramps the music written into `input` after [`Reverb::clear`].
    history_onset: FadeIn,
    diffusers: [[Allpass; 4]; 2],
    fdn: Fdn,
    /// Ramps the late reverb's input after its memory was cleared (room swap or reset).
    late_onset: FadeIn,
    damp: Svf2,
    room: Room,
    fade: Ramp,
    level: f32,
    level_ramp: Ramp,
}

impl Reverb {
    /// Allocates every room's buffers for `fs`; call off the real-time thread.
    pub fn new(fs: f32, room: Room, reverb_db: f32) -> Self {
        let configs = Room::ALL.map(|r| spec(r).map(|s| Config::new(&s, fs)));
        let longest = configs.iter().flatten().map(Config::longest_read).max().unwrap_or(0);
        let mut max_len = [1usize; FDN_N];
        for c in configs.iter().flatten() {
            for (m, l) in max_len.iter_mut().zip(c.lens) {
                *m = (*m).max(l);
            }
        }
        let diffusers = std::array::from_fn(|ch| {
            std::array::from_fn(|i| {
                let longest = configs.iter().flatten().map(|c| c.diffusers[ch][i]).max().unwrap_or(1);
                Allpass::new(longest, DIFFUSER_GAIN[i])
            })
        });
        let line = DelayLine::new(longest + SUB_BLOCK + 4);
        let level = db_to_gain(reverb_db);
        let mut r = Self {
            fs,
            configs,
            input: [line.clone(), line],
            history_onset: FadeIn::OPEN,
            diffusers,
            fdn: Fdn::new(max_len),
            late_onset: FadeIn::OPEN,
            damp: Svf2::new(SvfCoefs::lowpass(fs, 8000.0, 0.6)),
            room: Room::Off,
            fade: Ramp::new(0.0),
            level,
            level_ramp: Ramp::new(level),
        };
        r.switch_to(room);
        // Before the first sample there was silence, so the empty memory is already right.
        r.late_onset = FadeIn::OPEN;
        if room != Room::Off {
            r.fade.snap(1.0);
        }
        r
    }

    /// Swaps the room while the wet signal is faded out. The input history stays valid for any
    /// room (it is the dry music, written even while the room is off), so the new room's early
    /// reflections are already in flight; its late reverb starts empty and its input ramps up.
    fn switch_to(&mut self, room: Room) {
        self.room = room;
        self.clear_tail();
        if let Some(c) = &self.configs[room_index(room)] {
            self.fdn.configure(c);
            for (ch, lens) in self.diffusers.iter_mut().zip(c.diffusers) {
                for (ap, len) in ch.iter_mut().zip(lens) {
                    ap.configure(len);
                }
            }
            self.damp.set_coefs(c.damp);
        }
    }

    fn clear_tail(&mut self) {
        self.diffusers.iter_mut().flatten().for_each(Allpass::clear);
        self.fdn.clear();
        self.damp.reset();
        self.late_onset.restart();
    }

    /// Silences all reverb memory (the configuration stays). The music that follows is let in
    /// gradually, so the reflections and the tail build up without an onset click.
    pub fn clear(&mut self) {
        self.input.iter_mut().for_each(DelayLine::clear);
        self.history_onset.restart();
        self.clear_tail();
    }

    /// Appends one sub-block of dry music to the history; returns where it starts per channel.
    fn write_history(&mut self, x: [&[f32]; 2], n: usize) -> [usize; 2] {
        let mut gate = [0.0f32; SUB_BLOCK];
        if !self.history_onset.gains(&mut gate[..n], ONSET_S * self.fs) {
            return [self.input[0].write(&x[0][..n]), self.input[1].write(&x[1][..n])];
        }
        let mut start = [0usize; 2];
        for ((s, line), x) in start.iter_mut().zip(&mut self.input).zip(x) {
            let mut buf = [0.0f32; SUB_BLOCK];
            for ((b, v), g) in buf.iter_mut().zip(&x[..n]).zip(&gate[..n]) {
                *b = v * g;
            }
            *s = line.write(&buf[..n]);
        }
        start
    }

    /// True when nothing needs to run: room off and fully faded out.
    pub fn is_idle(&self) -> bool {
        self.room == Room::Off && self.fade.value() <= 0.0 && !self.fade.is_moving()
    }

    /// Per sub-block of `n` frames: follow the room choice (dip, swap, fade in) and the level.
    pub fn update(&mut self, target: Room, reverb_db: f32, n: usize) {
        if target != self.room && self.fade.value() <= 0.0 {
            self.switch_to(target);
        }
        let goal = if target == self.room && self.room != Room::Off { 1.0 } else { 0.0 };
        let step = n as f32 / (SWITCH_FADE_S * self.fs);
        let f = self.fade.value();
        let next = if goal > f { (f + step).min(goal) } else { (f - step).max(goal) };
        self.fade.set(next, n);
        approach(&mut self.level, db_to_gain(reverb_db), one_pole(LEVEL_TAU_S, n, self.fs), 1e-6);
        self.level_ramp.set(self.level, n);
    }

    /// Renders the wet signal of one sub-block (at most [`SUB_BLOCK`] frames) into `out`.
    pub fn process(&mut self, x: [&[f32]; 2], out: [&mut [f32]; 2]) {
        let [out_l, out_r] = out;
        let n = x[0].len().min(x[1].len()).min(out_l.len()).min(out_r.len()).min(SUB_BLOCK);
        let (out_l, out_r) = (&mut out_l[..n], &mut out_r[..n]);
        out_l.fill(0.0);
        out_r.fill(0.0);
        // Two copies per sample keep the history valid while the room is off, so turning a room
        // on does not start its reflections abruptly mid-waveform.
        let start = self.write_history(x, n);
        let idle = self.is_idle();
        let Some(cfg) = self.configs[room_index(self.room)].as_ref().filter(|_| !idle) else {
            self.fade.finish();
            self.level_ramp.finish();
            return;
        };

        for tap in &cfg.taps[..cfg.n_taps] {
            let line = &self.input[tap.line];
            let s = start[tap.line];
            for (i, (ol, or)) in out_l.iter_mut().zip(out_r.iter_mut()).enumerate() {
                let pos = s.wrapping_add(i);
                *ol += tap.gain[0] * line.read(pos, tap.delay[0]);
                *or += tap.gain[1] * line.read(pos, tap.delay[1]);
            }
        }

        let mut gate = [0.0f32; SUB_BLOCK];
        let gated = self.late_onset.gains(&mut gate[..n], ONSET_S * self.fs);
        let mut late = [[0.0f32; SUB_BLOCK]; 2];
        for ((buf, line), (diff, s)) in late.iter_mut().zip(&self.input).zip(self.diffusers.iter_mut().zip(start)) {
            let buf = &mut buf[..n];
            for (i, v) in buf.iter_mut().enumerate() {
                *v = line.read(s.wrapping_add(i), cfg.predelay);
            }
            if gated {
                buf.iter_mut().zip(&gate[..n]).for_each(|(v, g)| *v *= g);
            }
            for ap in diff {
                ap.process(buf);
            }
        }
        let [late_l, late_r] = &late;
        for ((ol, or), (&a, &b)) in out_l.iter_mut().zip(out_r.iter_mut()).zip(late_l.iter().zip(late_r)) {
            let (wl, wr) = self.fdn.tick(a, b);
            *ol += cfg.late_gain * wl;
            *or += cfg.late_gain * wr;
        }

        self.damp.process(out_l, out_r, &cfg.damp);
        for (ol, or) in out_l.iter_mut().zip(out_r.iter_mut()) {
            let g = self.fade.next() * self.level_ramp.next();
            *ol *= g;
            *or *= g;
        }
        self.fade.finish();
        self.level_ramp.finish();
    }

    pub fn flush(&mut self) {
        self.fdn.flush();
        self.damp.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wet impulse response of `room` (left input only), `secs` long.
    fn impulse_response(room: Room, fs: f32, secs: f32) -> Vec<[f32; 2]> {
        let mut r = Reverb::new(fs, room, 0.0);
        let frames = (secs * fs) as usize;
        let mut out = Vec::with_capacity(frames);
        let mut first = true;
        let mut done = 0;
        while done < frames {
            let n = SUB_BLOCK.min(frames - done);
            let mut l = [0.0f32; SUB_BLOCK];
            let r_in = [0.0f32; SUB_BLOCK];
            if first {
                l[0] = 1.0;
                first = false;
            }
            let (mut ol, mut or) = ([0.0f32; SUB_BLOCK], [0.0f32; SUB_BLOCK]);
            r.update(room, 0.0, n);
            r.process([&l[..n], &r_in[..n]], [&mut ol[..n], &mut or[..n]]);
            out.extend((0..n).map(|i| [ol[i], or[i]]));
            done += n;
        }
        out
    }

    /// Broadband T60 from Schroeder backward integration, fitted between -5 and -25 dB.
    fn measure_t60(ir: &[[f32; 2]], fs: f32) -> f32 {
        let e: Vec<f64> = ir.iter().map(|s| (s[0] as f64).powi(2) + (s[1] as f64).powi(2)).collect();
        let mut edc = vec![0.0f64; e.len()];
        let mut acc = 0.0;
        for i in (0..e.len()).rev() {
            acc += e[i];
            edc[i] = acc;
        }
        let total = edc[0];
        let db = |i: usize| 10.0 * (edc[i] / total).log10();
        let t5 = (0..e.len()).find(|&i| db(i) <= -5.0).unwrap();
        let t25 = (0..e.len()).find(|&i| db(i) <= -25.0).unwrap();
        // Least-squares slope of the EDC (dB per sample) between the two points.
        let (mut sx, mut sy, mut sxx, mut sxy, mut k) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for i in t5..=t25 {
            let (x, y) = (i as f64, db(i));
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
            k += 1.0;
        }
        let slope = (k * sxy - sx * sy) / (k * sxx - sx * sx);
        (-60.0 / slope / fs as f64) as f32
    }

    #[test]
    fn every_room_decays_at_its_nominal_rate() {
        for fs in [44_100.0, 48_000.0, 96_000.0] {
            for room in Room::ALL {
                let Some(t60) = nominal_t60(room) else { continue };
                let ir = impulse_response(room, fs, 6.0 * t60 + 0.1);
                let peak = ir.iter().map(|s| s[0].abs().max(s[1].abs())).fold(0.0, f32::max);
                assert!(peak > 1e-3, "{room:?} produced no reverb");
                let tail_from = (5.0 * t60 * fs) as usize;
                let tail = ir[tail_from..].iter().map(|s| s[0].abs().max(s[1].abs())).fold(0.0, f32::max);
                let tail_db = 20.0 * (tail / peak).log10();
                assert!(tail_db < -60.0, "{room:?} @ {fs}: tail {tail_db:.1} dB after 5*T60");
                let measured = measure_t60(&ir, fs);
                assert!((measured / t60 - 1.0).abs() < 0.15, "{room:?} @ {fs}: T60 {measured:.3} s, nominal {t60} s");
            }
        }
    }

    #[test]
    fn wet_power_follows_the_spec() {
        // White noise in at unit power: the wet power should land within a few dB of the
        // room's early + late budget (minus what the damping low-pass removes).
        let fs = 48_000.0;
        for room in Room::ALL {
            let Some(s) = spec(room) else { continue };
            let mut r = Reverb::new(fs, room, 0.0);
            let mut seed = 0x1234_5678u32;
            let mut noise = || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as f32 / u32::MAX as f32 - 0.5) * 12f32.sqrt()
            };
            let (mut acc, mut count) = (0.0f64, 0usize);
            for b in 0..(fs as usize * 4 / SUB_BLOCK) {
                let l: [f32; SUB_BLOCK] = std::array::from_fn(|_| noise());
                let rr: [f32; SUB_BLOCK] = std::array::from_fn(|_| noise());
                let (mut ol, mut or) = ([0.0f32; SUB_BLOCK], [0.0f32; SUB_BLOCK]);
                r.update(room, 0.0, SUB_BLOCK);
                r.process([&l, &rr], [&mut ol, &mut or]);
                if b * SUB_BLOCK > fs as usize * 2 {
                    acc += ol.iter().chain(&or).map(|v| (*v as f64).powi(2)).sum::<f64>();
                    count += 2 * SUB_BLOCK;
                }
            }
            let got_db = 10.0 * (acc / count as f64).log10() as f32;
            let budget = 10.0 * (db_to_gain(2.0 * s.early_db) + db_to_gain(2.0 * s.late_db)).log10();
            assert!(got_db < budget + 3.0, "{room:?}: wet {got_db:.1} dB, budget {budget:.1} dB");
            if s.damp_hz > 5000.0 {
                assert!(got_db > budget - 6.0, "{room:?}: wet {got_db:.1} dB, budget {budget:.1} dB");
            }
        }
    }

    #[test]
    fn room_switch_is_smooth_and_off_is_silent() {
        let fs = 48_000.0;
        let mut r = Reverb::new(fs, Room::Hall, 0.0);
        let x = [0.5f32; SUB_BLOCK];
        let mut prev = 0.0f32;
        let rooms = [Room::Hall, Room::Desk, Room::Off, Room::NextRoom, Room::Small];
        for (step, room) in rooms.into_iter().enumerate() {
            for _ in 0..200 {
                let (mut ol, mut or) = ([0.0f32; SUB_BLOCK], [0.0f32; SUB_BLOCK]);
                r.update(room, 0.0, SUB_BLOCK);
                r.process([&x, &x], [&mut ol, &mut or]);
                for v in ol {
                    assert!(v.is_finite());
                    // A constant input gives a smooth wet signal; switches must not step it.
                    assert!((v - prev).abs() < 0.05, "step {step}: jump {prev} -> {v}");
                    prev = v;
                }
            }
            if room == Room::Off {
                assert!(r.is_idle());
                assert_eq!(prev, 0.0);
            }
        }
    }
}
