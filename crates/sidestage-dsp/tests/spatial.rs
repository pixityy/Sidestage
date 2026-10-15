//! Direction cues: interaural level and time differences, front/back spectral cues, and
//! zipper-free movement.

mod common;

use common::*;
use sidestage_dsp::{Preset, Settings, render_offline};

/// Right-minus-left level (dB) at 4 kHz and the delay (ms) of the left ear behind the right.
fn ild_itd(azimuth: f32, fs: u32) -> (f64, f64) {
    let s = dry_settings(azimuth, 0.0);

    let tone = sine(4000.0, 0.2, fs as usize, fs);
    let (l, r) = split(&render_offline(s, fs, &interleave(&tone, &tone), 0.0));
    let half = l.len() / 2;
    let ild = db(rms(&r[half..])) - db(rms(&l[half..]));

    let mut rng = Rng::new(7);
    let noise: Vec<f32> = (0..fs as usize).map(|_| 0.1 * rng.white()).collect();
    let (l, r) = split(&render_offline(s, fs, &interleave(&noise, &noise), 0.0));
    let skip = fs as usize / 20;
    let lag = xcorr_lag(&r[skip..], &l[skip..], fs as usize / 500);
    (ild, lag * 1000.0 / fs as f64)
}

#[test]
fn source_on_the_right_is_louder_and_earlier_on_the_right() {
    // Brown & Duda at 4 kHz: alpha 2 at the near ear (+5.7 dB), 0.28 at the far one (-7.9 dB),
    // so 13.6 dB. Woodworth: (a/c)(1 + pi/2) = 0.656 ms. The bands are tight enough to catch a
    // wrong alpha curve (ffmpeg's theta_min of 180 degrees gives ~16 dB) or a wrong ITD.
    for fs in [44_100, 48_000, 96_000] {
        let (ild, itd) = ild_itd(90.0, fs);
        eprintln!("az +90 @ {fs}: ILD {ild:.2} dB, ITD {itd:.3} ms");
        assert!((12.5..=15.0).contains(&ild), "@ {fs}: ILD at 4 kHz {ild:.2} dB");
        assert!((0.63..=0.69).contains(&itd), "@ {fs}: ITD {itd:.3} ms");

        let (ild_m, itd_m) = ild_itd(-90.0, fs);
        assert!((ild + ild_m).abs() < 0.05, "@ {fs}: mirrored ILD {ild_m:.2} dB vs {ild:.2} dB");
        assert!((itd + itd_m).abs() < 0.005, "@ {fs}: mirrored ITD {itd_m:.3} ms vs {itd:.3} ms");
    }
}

struct Bands {
    left_right_db: f64,
    front_band_db: f64,
    back_treble_db: f64,
}

/// Band energies of a render relative to the 1 kHz band (both ears pooled).
fn bands(s: Settings, fs: u32) -> Bands {
    let mut rng = Rng::new(11);
    let frames = 4 * fs as usize;
    let l: Vec<f32> = (0..frames).map(|_| 0.1 * rng.white()).collect();
    let r: Vec<f32> = (0..frames).map(|_| 0.1 * rng.white()).collect();
    let (ol, or) = split(&render_offline(s, fs, &interleave(&l, &r), 0.0));
    let sl = power_spectrum(&ol[fs as usize / 2..], 4096);
    let sr = power_spectrum(&or[fs as usize / 2..], 4096);
    let both: Vec<f64> = sl.iter().zip(&sr).map(|(a, b)| a + b).collect();
    let rel = |lo: f64, hi: f64| 10.0 * (band(&both, fs, lo, hi) / band(&both, fs, 800.0, 1250.0)).log10();
    Bands {
        left_right_db: 10.0 * (sl.iter().sum::<f64>() / sr.iter().sum::<f64>()).log10(),
        front_band_db: rel(3000.0, 6000.0),
        back_treble_db: rel(10_000.0, 14_000.0),
    }
}

#[test]
fn behind_differs_from_front_only_by_the_rear_cues() {
    for fs in [44_100, 48_000, 96_000] {
        let front = bands(dry_settings(0.0, 60.0), fs);
        let back = bands(dry_settings(180.0, 60.0), fs);
        for (name, b) in [("front", &front), ("back", &back)] {
            eprintln!(
                "{name} @ {fs}: L/R {:.2} dB, 3-6k {:.2} dB, 10-14k {:.2} dB",
                b.left_right_db, b.front_band_db, b.back_treble_db
            );
        }
        for (name, b) in [("front", &front), ("back", &back)] {
            assert!(b.left_right_db.abs() < 0.5, "{name} @ {fs}: left/right differ by {:.2} dB", b.left_right_db);
        }
        // Measured: about -4.2 dB and +0.9 dB at every rate.
        assert!(
            back.front_band_db < front.front_band_db - 3.0,
            "3-6 kHz vs 1 kHz @ {fs}: back {:.2} dB, front {:.2} dB",
            back.front_band_db,
            front.front_band_db
        );
        assert!(
            back.back_treble_db > front.back_treble_db + 0.5,
            "10-14 kHz vs 1 kHz @ {fs}: back {:.2} dB, front {:.2} dB",
            back.back_treble_db,
            front.back_treble_db
        );

        // With the room and tone of the default preset the image stays centred.
        let preset = bands(Preset::BehindYou.settings(), fs);
        assert!(
            preset.left_right_db.abs() < 1.0,
            "BehindYou @ {fs}: left/right differ by {:.2} dB",
            preset.left_right_db
        );
    }
}

/// Sweeps the azimuth once around the head in 1 s, publishing new settings every 16 ms like the
/// UI does, and returns the worst (step, curvature) ratios of both ears.
fn sweep(base: Settings, block_frames: usize, fs: u32) -> (f64, f64) {
    let (mut sp, params) = spatializer(base, fs);
    let frames = fs as usize + fs as usize / 4;
    let input = {
        let tone = sine(1000.0, 0.25, frames, fs);
        interleave(&tone, &tone)
    };
    let mut out = vec![0.0; input.len()];
    let update_every = (0.016 * fs as f64) as usize;
    let mut next_update = 0;
    let mut pos = 0;
    while pos < frames {
        if pos >= next_update {
            let t = (pos as f32 / fs as f32).min(1.0);
            params.set(Settings { azimuth_deg: -180.0 + 360.0 * t, ..base });
            next_update += update_every;
        }
        let n = block_frames.min(frames - pos);
        sp.process(&input[pos * 2..(pos + n) * 2], &mut out[pos * 2..(pos + n) * 2], 0.0);
        pos += n;
    }
    let (l, r) = split(&out);
    // Skip the first 50 ms (the start fade and the first sweep step).
    let skip = fs as usize / 20;
    let win = fs as usize / 1000;
    let [step, curve] = [worst_step_ratio, worst_curvature_ratio]
        .map(|f| f(&l[skip..], 1000.0, fs, win).0.max(f(&r[skip..], 1000.0, fs, win).0));
    (step, curve)
}

#[test]
fn sweeping_the_azimuth_has_no_zipper_noise() {
    let quiet = |s: Settings| Settings { ducking: sidestage_dsp::DuckMode::Off.ducking(), ..s };
    for fs in [44_100, 48_000, 96_000] {
        for (name, s) in [
            ("BehindYou", quiet(Preset::BehindYou.settings())),
            ("Lounge", quiet(Preset::Lounge.settings())),
            ("dry point source", dry_settings(0.0, 0.0)),
        ] {
            for block in [64, 441, 1024] {
                let (step, curve) = sweep(s, block, fs);
                eprintln!("{name} @ {fs}, {block}: step {step:.3}, curvature {curve:.3}");
                assert!(step < 1.5, "{name} @ {fs}, {block}-frame blocks: step {step:.2}x the sine's max slope");
                // Smooth: <= 1.03. Delay and head shadow stepping once per sub-block instead of
                // gliding (a zipper the step ratio alone misses): >= 1.5.
                assert!(curve < 1.25, "{name} @ {fs}, {block}-frame blocks: curvature {curve:.2}x a sine's");
            }
        }
    }
}

/// Interaural energy difference (dB, listener's left ear minus right ear) when only one of the
/// music's channels plays.
fn channel_side(azimuth: f32, width: f32, left_channel: bool) -> f64 {
    let mut rng = Rng::new(21);
    let noise: Vec<f32> = (0..FS as usize / 2).map(|_| 0.1 * rng.white()).collect();
    let silence = vec![0.0; noise.len()];
    let input = if left_channel { interleave(&noise, &silence) } else { interleave(&silence, &noise) };
    let (l, r) = split(&render_offline(dry_settings(azimuth, width), FS, &input, 0.0));
    db(rms(&l)) - db(rms(&r))
}

#[test]
fn music_left_channel_stays_left_of_the_right_channel() {
    // In front, behind, and off to one side (where the whole image is on that side, but the
    // music's left channel must still be the one further left).
    for az in [0.0, 30.0, -45.0, 135.0, 180.0, -150.0] {
        let left = channel_side(az, 90.0, true);
        let right = channel_side(az, 90.0, false);
        assert!(left > right + 3.0, "az {az}: left channel at {left:.1} dB, right channel at {right:.1} dB");
    }
}

/// The same through every preset, room reflections and tone included, at common device rates.
#[test]
fn presets_keep_the_music_left_channel_on_the_left() {
    for fs in [44_100, 48_000, 96_000] {
        for p in Preset::ALL {
            let s = Settings { ducking: sidestage_dsp::DuckMode::Off.ducking(), ..p.settings() };
            let side = |left_channel: bool| {
                let mut rng = Rng::new(21);
                let noise: Vec<f32> = (0..fs as usize).map(|_| 0.1 * rng.white()).collect();
                let silence = vec![0.0; noise.len()];
                let input = if left_channel { interleave(&noise, &silence) } else { interleave(&silence, &noise) };
                let (l, r) = split(&render_offline(s, fs, &input, 0.0));
                db(rms(&l[fs as usize / 4..])) - db(rms(&r[fs as usize / 4..]))
            };
            let (left, right) = (side(true), side(false));
            // Width 0 (NextRoom, DeskSpeaker) leaves only the reflections to tell them apart.
            let margin = if s.width_deg > 0.0 { 3.0 } else { 0.3 };
            assert!(
                left > right + margin,
                "{p:?} @ {fs}: left channel at {left:.2} dB, right channel at {right:.2} dB"
            );
        }
    }
}

#[test]
fn elevation_adds_the_above_cue() {
    let level = |el: f32| {
        let mut rng = Rng::new(23);
        let noise: Vec<f32> = (0..2 * FS as usize).map(|_| 0.1 * rng.white()).collect();
        let s = Settings { elevation_deg: el, ..dry_settings(0.0, 0.0) };
        let (l, _) = split(&render_offline(s, FS, &interleave(&noise, &noise), 0.0));
        let spec = power_spectrum(&l, 4096);
        10.0 * (band(&spec, FS, 6500.0, 8500.0) / band(&spec, FS, 800.0, 1250.0)).log10()
    };
    let (flat, up) = (level(0.0), level(70.0));
    assert!(up > flat + 1.5, "7-8 kHz vs 1 kHz: {up:.2} dB at 70°, {flat:.2} dB at 0°");
}
