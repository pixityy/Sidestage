//! Sidechain ducking: timing, depth, metering, and that the music really gets quieter.

mod common;

use common::*;
use sidestage_dsp::{DuckMode, Preset, Settings};

const BLOCK: usize = 64;

/// Feeds noise in BLOCK-frame calls with the given sidechain level for `ms`, returning the
/// metered reduction after every call and the output RMS of the last half.
fn feed(
    fs: u32,
    sp: &mut sidestage_dsp::Spatializer,
    params: &sidestage_dsp::SharedParams,
    rng: &mut Rng,
    sidechain: f32,
    ms: f32,
) -> (Vec<f32>, f64) {
    let calls = (ms * 1e-3 * fs as f32 / BLOCK as f32).ceil() as usize;
    let mut duck = Vec::with_capacity(calls);
    let mut tail = Vec::new();
    for c in 0..calls {
        let input: Vec<f32> = (0..2 * BLOCK).map(|_| 0.1 * rng.white()).collect();
        let mut out = vec![0.0; input.len()];
        sp.process(&input, &mut out, sidechain);
        let m = params.meters.take();
        assert_eq!(m.sidechain_peak, sidechain);
        duck.push(m.duck_db);
        if c >= calls / 2 {
            tail.extend(out);
        }
    }
    (duck, rms(&tail))
}

#[test]
fn standard_ducking_engages_holds_and_releases() {
    // The timing is in seconds, so it must not depend on the device rate.
    for fs in [44_100, 48_000, 96_000] {
        let base = Settings { room: sidestage_dsp::Room::Off, ..Preset::BehindYou.settings() };
        let d = DuckMode::Standard.ducking();
        let (mut sp, params) = spatializer(Settings { ducking: d, ..base }, fs);
        let mut rng = Rng::new(3);

        let (_, open_rms) = feed(fs, &mut sp, &params, &mut rng, 0.0, 500.0);

        // 0.3 ≈ -10.5 dBFS, far above the -36 dB threshold: full depth within ~5x the attack.
        let (duck, _) = feed(fs, &mut sp, &params, &mut rng, 0.3, 5.0 * d.attack_ms);
        let reached = *duck.last().unwrap();
        assert!(reached > d.depth_db - 0.25, "@ {fs}: after 5x attack: {reached:.2} dB of {} dB", d.depth_db);
        let (duck, ducked_rms) = feed(fs, &mut sp, &params, &mut rng, 0.3, 500.0);
        assert!(duck.iter().all(|v| (v - d.depth_db).abs() < 0.25));
        let drop = db(open_rms) - db(ducked_rms);
        assert!(drop > d.depth_db as f64 - 1.0, "@ {fs}: music only {drop:.1} dB quieter");

        // Game goes quiet: held for hold_ms, then back to ~0 within ~5x the release.
        let (duck, _) = feed(fs, &mut sp, &params, &mut rng, 0.0, 0.8 * d.hold_ms);
        assert!(duck.iter().all(|v| *v > d.depth_db - 0.25), "@ {fs}: released during hold");
        let (duck, _) = feed(fs, &mut sp, &params, &mut rng, 0.0, 0.2 * d.hold_ms + 5.0 * d.release_ms + 5.0);
        let left = *duck.last().unwrap();
        assert!(left < 0.1, "@ {fs}: after hold + 5x release: {left:.3} dB");
        assert!(duck.windows(2).all(|w| w[1] <= w[0] + 1e-6), "@ {fs}: release is monotonic");
    }
}

#[test]
fn ducking_off_never_reduces() {
    let s = Settings { ducking: DuckMode::Off.ducking(), ..Preset::BehindYou.settings() };
    let (mut sp, params) = spatializer(s, FS);
    let mut rng = Rng::new(4);
    for level in [0.0, 0.3, 1.0, 4.0] {
        let (duck, _) = feed(FS, &mut sp, &params, &mut rng, level, 300.0);
        assert!(duck.iter().all(|v| *v == 0.0), "Off ducked at sidechain {level}");
    }
}

#[test]
fn quiet_games_do_not_duck() {
    let d = DuckMode::Standard.ducking();
    let (mut sp, params) = spatializer(Settings { ducking: d, ..Preset::BehindYou.settings() }, FS);
    let mut rng = Rng::new(5);
    // -50 dBFS: well below the -36 dB threshold (and its 3 dB soft knee).
    let (duck, _) = feed(FS, &mut sp, &params, &mut rng, 0.003, 500.0);
    assert!(duck.iter().all(|v| *v == 0.0));
}
