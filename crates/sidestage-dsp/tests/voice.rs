//! Voice chat and spoken word: room and externalisation, tone, side, band-limiting, clarity,
//! the leveller in the renderer, and footstep-safe intelligibility.

mod common;

use common::*;
use sidestage_dsp::{DuckMode, Preset, Room, Settings, make_footstep_safe, render_offline};

/// Render with the ducker off.
fn render(s: Settings, input: &[f32]) -> Vec<f32> {
    render_offline(Settings { ducking: DuckMode::Off.ducking(), ..s }, FS, input, 0.0)
}

/// Frames skipped before measuring (leveller and room settle).
const SETTLE: usize = FS as usize;

/// Interaural coherence: the largest normalized cross-correlation within ±1 ms.
fn iacc(x: &[f32]) -> f64 {
    let (l, r) = split(x);
    let max_lag = FS as usize / 1000;
    let n = l.len() - 2 * max_lag;
    let el: f64 = l[max_lag..max_lag + n].iter().map(|v| (*v as f64).powi(2)).sum();
    let er: f64 = r[max_lag..max_lag + n].iter().map(|v| (*v as f64).powi(2)).sum();
    let mut best = 0.0f64;
    for lag in 0..=2 * max_lag {
        let c: f64 = (0..n).map(|i| l[max_lag + i] as f64 * r[lag + i] as f64).sum();
        best = best.max(c.abs());
    }
    best / (el * er).sqrt()
}

/// The linear parts of a preset (leveller off), measured with speech-shaped noise.
struct Linear {
    /// Direct-to-reverberant ratio, dB.
    drr: f64,
    iacc: f64,
    /// Left minus right ear, dB.
    ild: f64,
    /// Output spectrum relative to the input's in [`BANDS`], each relative to 800-2000 Hz.
    bands: [f64; 5],
}

const BANDS: [(f64, f64); 5] = [(80.0, 250.0), (250.0, 800.0), (800.0, 2000.0), (2000.0, 5000.0), (5000.0, 10_000.0)];

fn linear(s: Settings) -> Linear {
    let lin = Settings { leveler: 0.0, ..s };
    let noise = speech_noise(5, 3.0);
    let skip = FS as usize;
    let with_room = render(lin, &noise);
    let dry = render(Settings { room: Room::Off, ..lin }, &noise);
    let wet: Vec<f32> = with_room.iter().zip(&dry).map(|(a, b)| a - b).collect();
    let drr = db(rms(&dry[skip..])) - db(rms(&wet[skip..]));
    let (l, r) = split(&with_room[skip..]);
    let ild = db(rms(&l)) - db(rms(&r));
    let (il, _) = split(&noise[skip..]);
    let spec_in = power_spectrum(&il, 4096);
    let both: Vec<f64> = power_spectrum(&l, 4096).iter().zip(&power_spectrum(&r, 4096)).map(|(a, b)| a + b).collect();
    let rel = |(lo, hi): (f64, f64)| 10.0 * (band(&both, FS, lo, hi) / band(&spec_in, FS, lo, hi)).log10();
    let mid = rel(BANDS[2]);
    Linear { drr, iacc: iacc(&with_room[skip..]), ild, bands: BANDS.map(|b| rel(b) - mid) }
}

#[test]
fn voice_presets_are_close_clear_and_outside_the_head() {
    for p in Preset::VOICE {
        let m = linear(p.settings());
        eprintln!("{p:?}: DRR {:.1} dB, IACC {:.2}, ILD {:+.1} dB, bands {:+.1?}", m.drr, m.iacc, m.ild, m.bands);
        // A little room: enough reflections to sound outside the head (a dry close voice reads
        // as inside it), little enough to stay close and crisp (Small room music: ~7 dB).
        assert!((6.0..=16.0).contains(&m.drr), "{p:?}: DRR {:.1} dB", m.drr);
        assert!(m.iacc < 0.92, "{p:?}: IACC {:.2}, nearly the same at both ears", m.iacc);
        // Not boomy: the low end below the speech's own, and the consonants at least as present
        // as in the input.
        assert!(m.bands[0] < -1.5 && m.bands[1] < 0.0, "{p:?}: boomy, {:+.1?}", m.bands);
        assert!(m.bands[3] > -1.0, "{p:?}: dull, 2-5 kHz at {:+.1} dB", m.bands[3]);
    }
}

#[test]
fn voice_presets_sit_on_their_side() {
    for (p, left) in [
        (Preset::VoiceLeft, true),
        (Preset::VoiceRight, false),
        (Preset::OverShoulder, false),
        (Preset::RadioComms, true),
        (Preset::PodcastLeft, true),
    ] {
        let ild = linear(p.settings()).ild;
        assert!(if left { ild > 2.0 } else { ild < -2.0 }, "{p:?}: left ear {ild:+.1} dB vs right");
    }
    let (l, r) = (linear(Preset::VoiceLeft.settings()).ild, linear(Preset::VoiceRight.settings()).ild);
    assert!((l + r).abs() < 0.3, "left and right are mirror images: {l:+.2} vs {r:+.2}");
}

/// Level of `hz` in `x` (Goertzel-style single-bin DFT), linear amplitude.
fn tone_level(x: &[f32], hz: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * hz / FS as f64;
    let (re, im) = x.iter().enumerate().fold((0.0, 0.0), |(re, im), (i, v)| {
        let (s, c) = (w * i as f64).sin_cos();
        (re + *v as f64 * c, im + *v as f64 * s)
    });
    2.0 * (re * re + im * im).sqrt() / x.len() as f64
}

#[test]
fn radio_comms_is_band_limited_not_distorted() {
    let s = Preset::RadioComms.settings();
    let m = linear(s);
    eprintln!("RadioComms bands {:+.1?}", m.bands);
    assert!(m.bands[0] < -9.0 && m.bands[4] < -9.0, "telephone band: {:+.1?}", m.bands);
    assert!(m.bands[1] > -6.0 && m.bands[3] > -3.0, "but the speech band stays: {:+.1?}", m.bands);

    // A 700 Hz tone at a quiet (lifted) and a loud (cut) talker's level, with the leveller
    // working hard: no harmonics from the gain riding the waveform (all of 1.4-3.5 kHz is in the
    // pass band).
    for amp in [0.05f32, 0.3] {
        let worst = worst_harmonic(s, 700.0, amp, 5);
        eprintln!("RadioComms, 700 Hz at {amp}: worst harmonic {worst:.1} dB");
        assert!(worst < -60.0, "amplitude {amp}: harmonic at {worst:.1} dB");
    }
}

/// Renders a `hz` tone at `amp` peak through `s` in 1 s bursts with 40 ms gaps (5 ms fades) and
/// returns the strongest of harmonics 2..=`up_to` relative to the fundamental, in dB, 0.5-0.9 s
/// into the third burst (whole periods of any tone at a multiple of 2.5 Hz). A steady tone looks
/// like steady noise to the leveller, which then only ever holds its gain; the gaps keep it
/// working in both directions.
fn worst_harmonic(s: Settings, hz: f64, amp: f32, up_to: usize) -> f64 {
    let (on, period, ramp) = (FS as usize, FS as usize + FS as usize / 25, FS as usize / 200);
    let tone: Vec<f32> = sine(hz, amp, 4 * period, FS)
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let t = i % period;
            if t < on { v * (t.min(on - t) as f32 / ramp as f32).min(1.0) } else { 0.0 }
        })
        .collect();
    let (l, _) = split(&render(s, &interleave(&tone, &tone)));
    let start = 2 * period + FS as usize / 2;
    let win = &l[start..start + 2 * FS as usize / 5];
    let f = tone_level(win, hz);
    (2..=up_to).map(|k| 20.0 * (tone_level(win, hz * k as f64) / f).log10()).fold(f64::MIN, f64::max)
}

/// Output-over-input power per band for `s` against `base` with the same white noise.
fn response_change(s: Settings, base: Settings, bands: &[(f64, f64)]) -> Vec<f64> {
    let mut rng = Rng::new(41);
    let noise: Vec<f32> = (0..3 * FS as usize).map(|_| 0.05 * rng.white()).collect();
    let input = interleave(&noise, &noise);
    let spectrum = |s: Settings| {
        let (l, r) = split(&render(s, &input)[FS as usize..]);
        let (a, b) = (power_spectrum(&l, 8192), power_spectrum(&r, 8192));
        a.iter().zip(&b).map(|(x, y)| x + y).collect::<Vec<f64>>()
    };
    let (a, b) = (spectrum(s), spectrum(base));
    bands.iter().map(|&(lo, hi)| 10.0 * (band(&a, FS, lo, hi) / band(&b, FS, lo, hi)).log10()).collect()
}

#[test]
fn clarity_is_a_broad_bell_at_2_5_khz() {
    let flat = dry_settings(0.0, 0.0);
    let bands = [(150.0, 300.0), (1150.0, 1350.0), (2400.0, 2600.0), (4600.0, 5400.0), (11_000.0, 14_000.0)];
    for db in [6.0f32, 3.0, -6.0] {
        let d = response_change(Settings { clarity_db: db, ..flat }, flat, &bands);
        eprintln!("clarity {db:+}: {d:+.2?}");
        let k = db as f64 / 6.0;
        assert!((d[2] - db as f64).abs() < 0.4 * k.abs(), "{db}: {:+.2} dB at 2.5 kHz", d[2]);
        // Q 0.8: still about half the gain an octave either side, nothing in the bass or treble.
        for edge in [d[1], d[3]] {
            assert!((0.35..=0.7).contains(&(edge / db as f64)), "{db}: {edge:+.2} dB an octave away");
        }
        assert!(d[0].abs() < 0.35 * k.abs() && d[4].abs() < 0.5 * k.abs(), "{db}: {d:+.2?}");
    }
}

#[test]
fn moving_clarity_has_no_zipper_noise() {
    // The UI publishes every 16 ms; clarity sweeps -6 -> +6 dB in 1 s on a 2.5 kHz tone, so
    // the bell's gain changes as fast as it ever will by hand.
    for fs in [44_100, 48_000, 96_000] {
        let base = dry_settings(0.0, 0.0);
        let (mut sp, params) = spatializer(Settings { clarity_db: -6.0, ..base }, fs);
        let frames = fs as usize + fs as usize / 4;
        let tone = sine(2500.0, 0.2, frames, fs);
        let input = interleave(&tone, &tone);
        let mut out = vec![0.0; input.len()];
        let every = (0.016 * fs as f64) as usize;
        for (k, (i, o)) in input.chunks(2 * every).zip(out.chunks_mut(2 * every)).enumerate() {
            let t = (k * every) as f32 / fs as f32;
            params.set(Settings { clarity_db: -6.0 + 12.0 * t.min(1.0), ..base });
            for (bi, bo) in i.chunks(2 * 441).zip(o.chunks_mut(2 * 441)) {
                sp.process(bi, bo, 0.0);
            }
        }
        let (l, _) = split(&out);
        let skip = fs as usize / 20;
        let (curve, at) = worst_curvature_ratio(&l[skip..], 2500.0, fs, fs as usize / 1000);
        eprintln!("clarity sweep @ {fs}: curvature {curve:.3} at {at}");
        assert!(curve < 1.25, "@ {fs}: curvature {curve:.2}x a sine's");
    }
}

#[test]
fn leveller_never_lifts_noise_or_silence() {
    // Hiss, keyboard and breath noise below -50 dBFS, and the near-silence Discord's noise
    // suppression leaves: the leveller at full strength must not lift any of it.
    let mut rng = Rng::new(77);
    for rms_db in [-90.0f64, -70.0, -56.0] {
        let g = 10f64.powf(rms_db / 20.0) as f32;
        let noise: Vec<f32> = (0..2 * 4 * FS as usize).map(|_| g * rng.white()).collect();
        for p in Preset::VOICE {
            let s = Settings { leveler: 1.0, ..p.settings() };
            let lifted = db(rms(&render(s, &noise)[2 * SETTLE..]));
            let plain = db(rms(&render(Settings { leveler: 0.0, ..s }, &noise)[2 * SETTLE..]));
            assert!((lifted - plain).abs() < 0.05, "{p:?}, {rms_db} dBFS noise: {:+.2} dB", lifted - plain);
        }
    }
    // Digital silence stays silent (the room's denormal guard leaves far below -200 dBFS).
    let silence = vec![0.0f32; 2 * 2 * FS as usize];
    for p in Preset::VOICE {
        let s = Settings { leveler: 1.0, ..p.settings() };
        let peak = render(s, &silence).iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < 1e-10, "{p:?}: {peak:e} out of silence");
    }
}

/// Speech over a steady noise floor (an unsuppressed mic, room tone): [`speech_like`] at
/// `speech_lufs` plus mono white noise at `floor_db` dBFS RMS. Also returns, per frame, whether
/// the speech is in a pause (`Some(false)`), talking (`Some(true)`) or in between (`None`), from
/// the clean speech's 50 ms RMS.
fn speech_over_noise(seed: u64, speech_lufs: f64, floor_db: f64) -> (Vec<f32>, Vec<Option<bool>>) {
    let clean = speech_like(seed, 12.0, speech_lufs);
    let (mono, _) = split(&clean);
    let w = FS as usize / 20;
    let mut energy = vec![0.0f64; mono.len() + 1];
    for (i, v) in mono.iter().enumerate() {
        energy[i + 1] = energy[i] + (*v as f64).powi(2);
    }
    let state: Vec<Option<bool>> = (0..mono.len())
        .map(|i| {
            let (lo, hi) = (i.saturating_sub(w / 2), (i + w / 2).min(mono.len()));
            let r = db(((energy[hi] - energy[lo]) / (hi - lo) as f64).sqrt());
            if r < -60.0 {
                Some(false)
            } else if r > speech_lufs - 10.0 {
                Some(true)
            } else {
                None
            }
        })
        .collect();
    let mut rng = Rng::new(seed + 100);
    let g = 10f64.powf(floor_db / 20.0) as f32;
    let noisy = clean
        .chunks(2)
        .flat_map(|f| {
            let n = g * rng.white();
            [f[0] + n, f[1] + n]
        })
        .collect();
    (noisy, state)
}

#[test]
fn leveller_does_not_swell_steady_noise_in_pauses() {
    // Noise 15-30 dB under the voice is louder than the leveller's absolute gate; without the
    // noise-floor rule it rose by up to +16 dB in every pause (the whole make-up gain).
    for p in [Preset::VoiceLeft, Preset::RadioComms, Preset::PodcastLeft] {
        let s = Settings { room: Room::Off, ..p.settings() };
        for (speech, floor) in [(-20.0, -46.0), (-20.0, -40.0), (-20.0, -34.0), (-26.0, -40.0)] {
            let (x, state) = speech_over_noise(8, speech, floor);
            let (on, off) = (render(s, &x), render(Settings { leveler: 0.0, ..s }, &x));
            let gain = |want: bool| {
                let pick = |y: &[f32]| -> Vec<f32> {
                    let (l, _) = split(y);
                    l.iter()
                        .zip(&state)
                        .skip(2 * SETTLE)
                        .filter(|(_, st)| **st == Some(want))
                        .map(|(v, _)| *v)
                        .collect()
                };
                db(rms(&pick(&on))) - db(rms(&pick(&off)))
            };
            let (talk, pause) = (gain(true), gain(false));
            eprintln!(
                "{p:?}, speech {speech} LUFS over {floor} dBFS noise: talking {talk:+.1} dB, pauses {pause:+.1} dB"
            );
            // The pauses keep the gain the speech had (a lifted talker's noise comes up with it),
            // nothing more.
            assert!(
                pause < talk + 1.0,
                "{p:?} {speech}/{floor}: the noise swells by {pause:+.1} dB (talking {talk:+.1})"
            );
        }
    }
    // Over a quiet floor a quiet talker is still lifted.
    let (x, state) = speech_over_noise(8, -32.0, -55.0);
    let s = Settings { room: Room::Off, ..Preset::VoiceLeft.settings() };
    let (on, off) = (render(s, &x), render(Settings { leveler: 0.0, ..s }, &x));
    let talking = |y: &[f32]| -> Vec<f32> {
        let (l, _) = split(y);
        l.iter().zip(&state).skip(2 * SETTLE).filter(|(_, st)| **st == Some(true)).map(|(v, _)| *v).collect()
    };
    let lift = db(rms(&talking(&on))) - db(rms(&talking(&off)));
    assert!(lift > 5.0, "a -32 LUFS talker only lifted by {lift:+.1} dB");
}

#[test]
fn leveller_does_not_distort_low_voices() {
    // Gain riding is distortion when the gain follows the waveform; a speech fundamental at
    // 100-150 Hz is where a 6 ms detector ripples most. Quiet (lifted) and loud (cut) tones with
    // the room off: the harmonics the leveller adds stay far down (a 10x faster detector and
    // attack put them at -48 dB).
    for p in [Preset::VoiceLeft, Preset::PodcastLeft] {
        let s = Settings { room: Room::Off, ..p.settings() };
        for hz in [100.0f64, 150.0] {
            for amp in [0.02f32, 0.3] {
                let worst = worst_harmonic(s, hz, amp, 6);
                eprintln!("{p:?}, {hz} Hz at {amp}: worst harmonic {worst:.1} dB");
                assert!(worst < -60.0, "{p:?}, {hz} Hz at {amp}: harmonic at {worst:.1} dB");
            }
        }
    }
}

#[test]
fn footstep_safe_voice_chat_stays_intelligible() {
    // Octave bands with their speech-intelligibility importance (ANSI S3.5-1997, octave-band
    // procedure). Footsteps live around 2-5 kHz, right where speech carries most of it, so
    // voice chat gets a lighter version of footstep-safe than music does.
    let octaves =
        [(177.0, 354.0), (354.0, 707.0), (707.0, 1414.0), (1414.0, 2828.0), (2828.0, 5657.0), (5657.0, 11_314.0)];
    let importance = [0.0617, 0.1671, 0.2373, 0.2648, 0.2142, 0.0549];
    let weighted = |d: &[f64]| d.iter().zip(importance).map(|(v, w)| v * w).sum::<f64>();
    for p in Preset::VOICE.into_iter().filter(|p| p.settings().voice_chat) {
        let s = Settings { leveler: 0.0, ..p.settings() };
        let d = response_change(make_footstep_safe(s), s, &octaves);
        // What the music version would do to the same voice.
        let as_music = Settings { voice_chat: false, ..s };
        let m = response_change(make_footstep_safe(as_music), as_music, &octaves);
        let (w, wm) = (weighted(&d), weighted(&m));
        eprintln!("{p:?}: footstep-safe {d:+.1?} dB, weighted {w:+.2} dB (music rules {wm:+.2} dB)");
        assert!(w > -1.5, "{p:?}: speech loses {w:+.2} dB (importance-weighted)");
        assert!(d[..5].iter().all(|v| *v > -3.0), "{p:?}: {d:+.1?}");
        // The music rules cut deeper, and they would also duck the voice by 12 dB for the game.
        assert!(wm < w - 1.0, "{p:?}: the voice version is lighter ({w:+.2} vs {wm:+.2} dB)");
        assert_eq!(make_footstep_safe(s).ducking, s.ducking);
    }
}

/// Integrated loudness after the settling time.
fn out_lufs(s: Settings, input: &[f32]) -> f64 {
    lufs(&render(s, input)[2 * SETTLE..], FS)
}

#[test]
#[ignore = "tuning aid: cargo test -p sidestage-dsp --release --test voice -- --ignored --nocapture"]
fn tuning_report() {
    let music = music_like(9, 6.0);
    let behind = out_lufs(Preset::BehindYou.settings(), &music) - lufs(&music[2 * SETTLE..], FS);
    eprintln!("BehindYou on music: {behind:+.2} LU vs input");
    let base = Settings { room: Room::Off, volume_db: -12.0, ..Preset::VoiceLeft.settings() };
    for amount in [0.2f32, 0.4, 0.6, 0.85, 1.0] {
        let s = Settings { leveler: amount, ..base };
        let off = Settings { leveler: 0.0, ..base };
        let mut line = format!("leveler {amount:.2}: spread");
        let loud = speech_like(31, 12.0, SPEECH_LUFS);
        let loud_out = out_lufs(s, &loud);
        for down in [6.0, 12.0, 18.0] {
            line += &format!(" {down:.0}->{:.1}", loud_out - out_lufs(s, &gain_db(&loud, -down)));
        }
        line += "; net gain at";
        for lvl in [-14.0, -20.0, -26.0, -32.0] {
            let x = speech_like(31, 12.0, lvl);
            line += &format!(" {lvl:.0}:{:+.1}", out_lufs(s, &x) - out_lufs(off, &x));
        }
        eprintln!("{line}");
    }
    if std::env::var("ROOM_GRID").is_ok() {
        for (az, dist) in [(-70.0f32, 1.0f32), (140.0, 0.7), (-90.0, 1.5), (-40.0, 1.0)] {
            for room in [Room::Desk, Room::Small, Room::Living] {
                let mut line = format!("az {az} {dist} m {room:?}:");
                for rv in [-8.0f32, -12.0, -16.0, -20.0] {
                    let s = Settings {
                        azimuth_deg: az,
                        distance_m: dist,
                        room,
                        reverb_db: rv,
                        ..Preset::VoiceLeft.settings()
                    };
                    let r = linear(s);
                    line += &format!(" {rv:.0}: DRR {:.1} IACC {:.2} lo {:+.1} |", r.drr, r.iacc, r.bands[0]);
                }
                eprintln!("{line}");
            }
        }
    }
    eprintln!("preset        gain@-20 gain@-15 spread  peak@-14  DRR   IACC  ILD    80-250 250-800 2-5k  5-10k");
    for p in Preset::VOICE.into_iter().chain([Preset::BehindYou, Preset::DeskSpeaker]) {
        let s = p.settings();
        let loud = speech_like(31, 12.0, SPEECH_LUFS);
        let at = |lvl: f64| {
            let x = speech_like(31, 12.0, lvl);
            out_lufs(s, &x) - lufs(&x[2 * SETTLE..], FS) - behind
        };
        let spread = out_lufs(s, &loud) - out_lufs(s, &gain_db(&loud, -12.0));
        let hot = speech_like(31, 12.0, -14.0);
        let peak = db(render(s, &hot).iter().fold(0.0f32, |m, v| m.max(v.abs())) as f64);
        let r = linear(s);
        eprintln!(
            "{:<13} {:+6.1}   {:+6.1}  {:5.1}  {:+6.1}   {:5.1} {:5.2} {:+5.1}  {:+5.1}  {:+5.1}  {:+5.1}  {:+5.1}",
            format!("{p:?}"),
            at(SPEECH_LUFS),
            at(-15.0),
            spread,
            peak,
            r.drr,
            r.iacc,
            r.ild,
            r.bands[0],
            r.bands[1],
            r.bands[3],
            r.bands[4]
        );
    }
}

/// Voice chat lives on callouts after silences: the first word after a pause must start at the
/// level the last phrase ended at, not re-earn the leveller's make-up gain (it used to start
/// 3.5-4.5 dB quieter after 4-10 s of silence).
#[test]
fn callouts_after_long_silence_start_at_full_level() {
    let fs = 48_000usize;
    let rms_db =
        |x: &[f32]| 10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).max(1e-24).log10();
    for gap_s in [4.0f64, 10.0] {
        // 2.4 s of syllable-like noise bursts (150 ms on, 100 ms off), the gap, then the same again.
        let period = 2.4 + gap_s;
        let n = (fs as f64 * period * 2.0) as usize;
        let mut seed = 11u32;
        let input: Vec<f32> = (0..n)
            .flat_map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let w = ((seed >> 8) as f32 / 16_777_216.0 - 0.5) * 0.2;
                let t = (i as f64 / fs as f64) % period;
                let s = if t < 2.4 && (i % 12_000) < 7_200 { w } else { 0.0 };
                [s, s]
            })
            .collect();
        let out = render_offline(Preset::VoiceRight.settings(), fs as u32, &input, 0.0);
        let window = |start_s: f64| {
            let s = (start_s * fs as f64) as usize * 2;
            rms_db(&out[s..s + (0.3 * fs as f64) as usize * 2])
        };
        let end_of_first = window(2.1);
        let start_of_second = window(period);
        assert!(
            start_of_second > end_of_first - 1.5,
            "after {gap_s} s of silence the callout starts at {start_of_second:.1} dB, the last one ended at {end_of_first:.1} dB"
        );
    }
}
