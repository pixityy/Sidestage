//! Sidechain ducking: timing, depth, metering, that the music really gets quieter, and the two
//! triggers (the game, voice chat) of `process_with_voice`.

mod common;

use common::*;
use sidestage_dsp::{DuckMode, Ducking, Preset, Room, Settings, SharedParams, Spatializer};

const BLOCK: usize = 64;

/// Feeds noise in BLOCK-frame calls with the given sidechain level for `ms`, returning the
/// metered reduction after every call and the output RMS of the last half.
fn feed(
    fs: u32,
    sp: &mut Spatializer,
    params: &SharedParams,
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
        let base = Settings { room: Room::Off, ..Preset::BehindYou.settings() };
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

/// Feeds noise in BLOCK-frame calls of `process_with_voice` for `ms`; returns the metered
/// reduction after the last call and the metered sidechain level of every call.
fn feed_voice(sp: &mut Spatializer, params: &SharedParams, game: f32, voice: f32, ms: f32) -> (f32, Vec<f32>) {
    let mut rng = Rng::new(17);
    let calls = (ms * 1e-3 * FS as f32 / BLOCK as f32).ceil() as usize;
    let mut levels = Vec::with_capacity(calls);
    let mut duck = 0.0;
    for _ in 0..calls {
        let input: Vec<f32> = (0..2 * BLOCK).map(|_| 0.1 * rng.white()).collect();
        let mut out = vec![0.0; input.len()];
        sp.process_with_voice(&input, &mut out, game, voice);
        let m = params.meters.take();
        levels.push(m.sidechain_peak);
        duck = m.duck_db;
    }
    (duck, levels)
}

fn settled_reduction(ducking: Ducking, game: f32, voice: f32) -> f32 {
    let s = Settings { ducking, room: Room::Off, ..Preset::BehindYou.settings() };
    let (mut sp, params) = spatializer(s, FS);
    let (duck, levels) = feed_voice(&mut sp, &params, game, voice, 1000.0);
    // The meter always shows the game itself, never the voice, whether or not it ducks for it.
    assert!(levels.iter().all(|l| *l == game), "metered {:?} for game {game}", levels.last());
    duck
}

#[test]
fn voice_chat_ducking_follows_the_enabled_triggers() {
    let standard = DuckMode::Standard.ducking();
    let game_only = Ducking { enabled: true, under_voice: false, ..standard };
    let voice_only = Ducking { enabled: false, under_voice: true, ..standard };
    let both = Ducking { enabled: true, under_voice: true, ..standard };
    let neither = Ducking { enabled: false, under_voice: false, ..standard };
    // 0.03 ≈ -30.5 dBFS and 0.06 ≈ -24.4 dBFS: both above the -36 dB threshold, both below
    // full depth, so the reduction shows which one the ducker follows.
    let (soft, loud) = (0.03f32, 0.06f32);
    let r_soft = settled_reduction(game_only, soft, 0.0);
    let r_loud = settled_reduction(game_only, loud, 0.0);
    assert!(r_soft > 1.0 && r_loud > r_soft + 2.0 && r_loud < standard.depth_db - 1.0, "{r_soft} {r_loud}");

    // Ducking under the game only: the voice is ignored.
    assert_eq!(settled_reduction(game_only, 0.0, loud), 0.0);
    assert_eq!(settled_reduction(game_only, soft, loud), r_soft);
    // Under voice only: the game is ignored (but still metered).
    assert_eq!(settled_reduction(voice_only, loud, 0.0), 0.0);
    assert_eq!(settled_reduction(voice_only, 0.0, loud), r_loud);
    assert_eq!(settled_reduction(voice_only, loud, soft), r_soft);
    // Both: whichever is louder.
    assert_eq!(settled_reduction(both, soft, loud), r_loud);
    assert_eq!(settled_reduction(both, loud, soft), r_loud);
    assert_eq!(settled_reduction(both, soft, 0.0), r_soft);
    // Neither: nothing, however loud.
    assert_eq!(settled_reduction(neither, 1.0, 1.0), 0.0);
    // Non-finite voice levels count as silence; a negative one as its magnitude.
    assert_eq!(settled_reduction(both, soft, f32::NAN), r_soft);
    assert_eq!(settled_reduction(voice_only, 0.0, f32::INFINITY), 0.0);
    assert_eq!(settled_reduction(voice_only, 0.0, -loud), r_loud);
}

#[test]
fn process_is_process_with_voice_silent() {
    let s = Settings {
        ducking: Ducking { under_voice: true, ..DuckMode::Competitive.ducking() },
        ..Preset::Lounge.settings()
    };
    let (mut a, _) = spatializer(s, FS);
    let (mut b, _) = spatializer(s, FS);
    let input = music_like(3, 2.0);
    let mut rng = Rng::new(9);
    let (mut oa, mut ob) = (vec![0.0; input.len()], vec![0.0; input.len()]);
    for (k, ((i, x), y)) in input.chunks(2 * 480).zip(oa.chunks_mut(2 * 480)).zip(ob.chunks_mut(2 * 480)).enumerate() {
        let game = if (k / 20) % 2 == 0 { 0.4 * rng.uniform() } else { 0.0 };
        a.process(i, x, game);
        b.process_with_voice(i, y, game, 0.0);
    }
    assert!(oa == ob, "process() must be process_with_voice(.., 0.0)");
    assert!(rms(&oa) > 1e-3);
}

#[test]
fn voice_presets_duck_as_documented() {
    let duck = |p: Preset, game: f32, voice: f32| {
        let (mut sp, params) = spatializer(p.settings(), FS);
        feed_voice(&mut sp, &params, game, voice, 1000.0).0
    };
    for p in Preset::VOICE {
        let s = p.settings();
        if s.voice_chat {
            // Voice chat is never turned down: not for the game, not for other voice chat.
            assert_eq!(duck(p, 1.0, 1.0), 0.0, "{p:?}");
        } else {
            // Spoken word ducks gently for the game and under voice chat.
            let game = duck(p, 0.5, 0.0);
            let voice = duck(p, 0.0, 0.5);
            assert!((game - s.ducking.depth_db).abs() < 0.1 && game <= 6.0, "{p:?}: {game} dB for the game");
            assert_eq!(voice, game, "{p:?}: under voice chat");
        }
    }
    for p in Preset::MUSIC {
        assert!(duck(p, 0.0, 0.5) > 3.0, "{p:?}: music turns down while voice chat talks");
    }
}
