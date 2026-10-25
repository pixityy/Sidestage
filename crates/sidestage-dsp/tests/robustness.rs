//! Hostile inputs and settings, bypass, and output safety.

mod common;

use common::*;
use sidestage_dsp::{DuckMode, Ducking, MasterBus, Preset, Room, Settings};

/// A float that is usually in a wide range but sometimes NaN, infinite or absurd.
fn wild(rng: &mut Rng, lo: f32, hi: f32) -> f32 {
    match rng.next_u64() % 20 {
        0 => f32::NAN,
        1 => f32::INFINITY,
        2 => f32::NEG_INFINITY,
        3 => rng.range(-1e9, 1e9),
        _ => rng.range(lo - 0.25 * (hi - lo), hi + 0.25 * (hi - lo)),
    }
}

fn random_settings(rng: &mut Rng) -> Settings {
    Settings {
        azimuth_deg: wild(rng, -720.0, 720.0),
        elevation_deg: wild(rng, -40.0, 80.0),
        distance_m: wild(rng, 0.5, 20.0),
        width_deg: wild(rng, 0.0, 120.0),
        volume_db: wild(rng, -40.0, 12.0),
        lowpass_hz: wild(rng, 800.0, 20_000.0),
        highpass_hz: wild(rng, 20.0, 400.0),
        bass_db: wild(rng, -12.0, 12.0),
        presence_dip_db: wild(rng, -12.0, 0.0),
        room: Room::ALL[(rng.next_u64() % 6) as usize],
        reverb_db: wild(rng, -40.0, 6.0),
        ducking: Ducking {
            enabled: rng.next_u64().is_multiple_of(2),
            threshold_db: wild(rng, -60.0, -10.0),
            depth_db: wild(rng, 0.0, 24.0),
            band_dip_db: wild(rng, 0.0, 12.0),
            attack_ms: wild(rng, 1.0, 200.0),
            hold_ms: wild(rng, 0.0, 1000.0),
            release_ms: wild(rng, 50.0, 3000.0),
            under_voice: rng.next_u64().is_multiple_of(2),
        },
        bypass: rng.next_u64().is_multiple_of(8),
        clarity_db: wild(rng, -6.0, 6.0),
        leveler: wild(rng, 0.0, 1.0),
        voice_chat: rng.next_u64().is_multiple_of(2),
    }
}

fn hammer(fs: u32, seconds: f32, seed: u64) {
    let mut rng = Rng::new(seed);
    let (mut sp, params) = spatializer(Preset::BehindYou.settings(), fs);
    let total = (seconds * fs as f32) as usize;
    let mut done = 0;
    let mut input = Vec::new();
    let mut output = Vec::new();
    while done < total {
        // Any length, including odd sample counts and empty calls.
        let len = (rng.next_u64() % 2049) as usize;
        input.clear();
        let level = if rng.next_u64().is_multiple_of(4) { 4.0 } else { 0.5 };
        for _ in 0..len {
            let v = match rng.next_u64() % 5000 {
                0 => f32::NAN,
                1 => f32::INFINITY,
                2 => 1e30,
                3 => 1e-40,
                _ => level * rng.white(),
            };
            input.push(v);
        }
        output.clear();
        output.resize(len, f32::NAN);
        if rng.next_u64().is_multiple_of(3) {
            params.set(random_settings(&mut rng));
        }
        let mut level = || match rng.next_u64() % 10 {
            0 => f32::NAN,
            1 => -1.0,
            2 => f32::INFINITY,
            3 => 0.0,
            _ => rng.range(0.0, 1.5),
        };
        let (game, voice) = (level(), level());
        if voice == 0.0 {
            sp.process(&input, &mut output, game);
        } else {
            sp.process_with_voice(&input, &mut output, game, voice);
        }
        for (i, v) in output.iter().enumerate() {
            assert!(v.is_finite() && v.abs() <= 1.0, "fs {fs}, sample {} of a {len}-sample call: {v}", done + i);
        }
        done += len / 2;
    }
    let m = params.meters.take();
    assert!(m.output_peak <= 1.0 && m.input_peak.is_finite() && m.duck_db.is_finite());
}

#[test]
fn random_settings_and_hostile_input_stay_finite_and_bounded() {
    hammer(48_000, 30.0, 1);
    hammer(22_050, 5.0, 2);
    hammer(192_000, 3.0, 3);
}

#[test]
fn unusual_sample_rates_work() {
    for fs in [0, 8000, 11_025, 22_050, 44_100, 88_200, 176_400, 192_000, 384_000] {
        let mut rng = Rng::new(fs as u64 + 1);
        let input: Vec<f32> = (0..20_000).map(|_| 0.3 * rng.white()).collect();
        let out = sidestage_dsp::render_offline(Preset::FarAway.settings(), fs, &input, 0.0);
        assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 1.0), "fs {fs}");
        assert!(rms(&out[20_000 / 2..]) > 1e-3, "fs {fs}: silent");
    }
}

#[test]
fn bypass_is_exact_after_the_crossfade_and_toggles_cleanly() {
    let base = Settings { ducking: DuckMode::Off.ducking(), ..Preset::BehindYou.settings() };
    let (mut sp, params) = spatializer(base, FS);
    let mut rng = Rng::new(5);

    // Noise: once the crossfade is over, bypass is a bit-exact copy.
    let noise: Vec<f32> = (0..FS as usize).map(|_| 0.3 * rng.white()).collect();
    let input = interleave(&noise, &noise.iter().map(|v| -0.5 * v).collect::<Vec<_>>());
    run(&mut sp, &input[..FS as usize / 2], 256, 0.0);
    params.set(Settings { bypass: true, ..base });
    let fade = 2 * (FS as usize / 20); // 50 ms of interleaved samples
    let out = run(&mut sp, &input, 256, 0.0);
    for (i, (o, x)) in out.iter().zip(&input).enumerate().skip(fade) {
        assert!((o - x).abs() <= 1e-6, "sample {i}: {o} vs {x}");
    }

    // A tone through several toggles: the crossfades must not click.
    let w = 2.0 * std::f64::consts::PI * 1000.0 / FS as f64;
    let tone: Vec<f32> = (0..3 * FS as usize).map(|i| 0.25 * (w * i as f64).sin() as f32).collect();
    let input = interleave(&tone, &tone);
    let mut out = vec![0.0; input.len()];
    for (k, (i, o)) in input.chunks(2 * 4800).zip(out.chunks_mut(2 * 4800)).enumerate() {
        params.set(Settings { bypass: k.is_multiple_of(2), ..base });
        sp.process(i, o, 0.0);
    }
    let (l, r) = split(&out);
    for ch in [&l, &r] {
        let (worst, at) = worst_step_ratio(&ch[4800..], 1000.0, FS, 48);
        assert!(worst < 1.5, "bypass toggle: step {worst:.2}x the sine's max slope at {at}");
    }
}

#[test]
fn preset_changes_crossfade_without_clicks() {
    let quiet = |p: Preset| Settings { ducking: DuckMode::Off.ducking(), ..p.settings() };
    let (mut sp, params) = spatializer(quiet(Preset::BehindYou), FS);
    let w = 2.0 * std::f64::consts::PI * 1000.0 / FS as f64;
    let tone: Vec<f32> = (0..12 * FS as usize).map(|i| 0.2 * (w * i as f64).sin() as f32).collect();
    let input = interleave(&tone, &tone);
    let mut out = vec![0.0; input.len()];
    // Between music and voice presets too: the leveller and clarity switch in and out.
    let order = [
        Preset::DeskSpeaker,
        Preset::VoiceLeft,
        Preset::Above,
        Preset::RadioComms,
        Preset::NextRoom,
        Preset::OverShoulder,
        Preset::Lounge,
        Preset::PodcastLeft,
        Preset::FarAway,
        Preset::VoiceRight,
        Preset::BehindYou,
    ];
    for (k, (i, o)) in input.chunks(2 * FS as usize).zip(out.chunks_mut(2 * FS as usize)).enumerate() {
        if k > 0 {
            params.set(quiet(order[(k - 1) % order.len()]));
        }
        for (bi, bo) in i.chunks(2 * 512).zip(o.chunks_mut(2 * 512)) {
            sp.process(bi, bo, 0.0);
        }
    }
    let (l, r) = split(&out);
    for ch in [&l, &r] {
        let (worst, at) = worst_step_ratio(&ch[4800..], 1000.0, FS, 48);
        assert!(worst < 1.5, "preset change: step {worst:.2}x the sine's max slope at {at}");
    }
}

/// A renderer is usually created while the music is already playing (session start, device or
/// sample-rate change). Starting mid-waveform must not click, and the room must not replay the
/// start as clicks ~40-200 ms later (before the start fade: up to 7.6x with FarAway).
#[test]
fn starting_while_music_plays_does_not_click() {
    for fs in [44_100u32, 48_000, 96_000] {
        // A cosine starts at its peak: the worst possible place to begin.
        let w = 2.0 * std::f64::consts::PI * 55.0 / fs as f64;
        let tone: Vec<f32> = (0..fs as usize).map(|i| 0.25 * (w * i as f64).cos() as f32).collect();
        let input = interleave(&tone, &tone);
        for p in Preset::ALL {
            for bypass in [false, true] {
                let s = Settings { ducking: DuckMode::Off.ducking(), bypass, ..p.settings() };
                let (mut sp, _) = spatializer(s, fs);
                let (l, r) = split(&run(&mut sp, &input, 480, 0.0));
                for ch in [&l, &r] {
                    let (worst, at) = worst_step_ratio(ch, 55.0, fs, (fs / 55) as usize);
                    let ms = at as f64 * 1e3 / fs as f64;
                    assert!(
                        worst < 1.35,
                        "{p:?} bypass {bypass} @ {fs}: step {worst:.2}x the tone's slope at {ms:.0} ms"
                    );
                }
            }
        }
    }
}

#[test]
fn master_bus_survives_hostile_mixes() {
    let mut rng = Rng::new(11);
    let mut bus = MasterBus::new(48_000);
    let mut input = Vec::new();
    let mut output = Vec::new();
    for _ in 0..3000 {
        let len = (rng.next_u64() % 2049) as usize;
        let level = [0.1, 1.0, 8.0, 100.0][(rng.next_u64() % 4) as usize];
        input.clear();
        for _ in 0..len {
            input.push(match rng.next_u64() % 3000 {
                0 => f32::NAN,
                1 => f32::NEG_INFINITY,
                2 => f32::MAX,
                3 => 1e-40,
                _ => level * rng.white(),
            });
        }
        output.clear();
        output.resize(len + (rng.next_u64() % 3) as usize, f32::NAN);
        bus.process(&input, &mut output);
        assert!(output.iter().all(|v| v.is_finite() && v.abs() <= 0.891_3), "a {len}-sample call");
    }
}

#[test]
fn v0_1_settings_load_with_the_new_fields_defaulted() {
    // A config saved by 0.1 has no clarity, leveller, voice chat or under-voice fields: it keeps
    // sounding exactly as before, and its music now turns down while voice chat talks.
    let old = serde_json::json!({
        "azimuth_deg": 150.0, "elevation_deg": 0.0, "distance_m": 2.0, "width_deg": 60.0,
        "volume_db": -3.0, "lowpass_hz": 18000.0, "highpass_hz": 30.0, "bass_db": 0.0,
        "presence_dip_db": -3.0, "room": "Small", "reverb_db": -12.0, "bypass": false,
        "ducking": { "enabled": true, "threshold_db": -42.0, "depth_db": 12.0, "band_dip_db": 9.0,
                     "attack_ms": 10.0, "hold_ms": 250.0, "release_ms": 450.0 }
    });
    let s: Settings = serde_json::from_value(old).unwrap();
    assert_eq!((s.clarity_db, s.leveler, s.voice_chat), (0.0, 0.0, false));
    assert!(s.ducking.under_voice);
    assert_eq!(DuckMode::matching(&s.ducking), Some(DuckMode::Competitive));
    assert_eq!(s.sanitized(), s);
}
