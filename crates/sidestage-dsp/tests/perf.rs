//! CPU cost: the renderer must be a small fraction of real time, alone and in a full session.

mod common;

use std::time::Instant;

use common::*;
use sidestage_dsp::{DuckMode, Ducking, MasterBus, Preset, Room, Settings};

/// The most expensive settings there are: the longest room, full width, a source that moves
/// and changes tone, both ducking triggers, and the leveller and clarity working.
fn heaviest(base: Settings) -> Settings {
    Settings {
        width_deg: 120.0,
        elevation_deg: 20.0,
        room: Room::Hall,
        reverb_db: 0.0,
        clarity_db: 6.0,
        leveler: 1.0,
        ducking: Ducking { under_voice: true, ..DuckMode::Competitive.ducking() },
        ..base
    }
}

/// 10 s of 48 kHz stereo through `settings` in device-sized (480-frame) calls; returns seconds.
fn time_10s(settings: Settings, move_it: bool) -> f64 {
    let (mut sp, params) = spatializer(settings, FS);
    let mut rng = Rng::new(1);
    let block: Vec<f32> = (0..2 * 480).map(|_| 0.3 * rng.white()).collect();
    let mut out = vec![0.0f32; block.len()];
    let calls = 10 * FS as usize / 480;
    let start = Instant::now();
    for i in 0..calls {
        if move_it && i.is_multiple_of(2) {
            // Keep the source moving so coefficients are recomputed every sub-block.
            let t = i as f32 * 0.7;
            params.set(Settings {
                azimuth_deg: t % 360.0 - 180.0,
                clarity_db: settings.clarity_db - (t * 0.1) % 3.0,
                ..settings
            });
        }
        sp.process_with_voice(&block, &mut out, 0.2, if (i / 50) % 2 == 0 { 0.3 } else { 0.0 });
    }
    let secs = start.elapsed().as_secs_f64();
    std::hint::black_box(&out);
    secs
}

#[test]
#[cfg_attr(debug_assertions, ignore = "timing is only meaningful in release builds")]
fn ten_seconds_render_in_well_under_real_time() {
    let mut cases: Vec<(String, Settings)> = Preset::ALL.iter().map(|p| (format!("{p:?}"), p.settings())).collect();
    cases.push(("heaviest music".into(), heaviest(Preset::Lounge.settings())));
    cases.push(("heaviest voice".into(), heaviest(Preset::VoiceRight.settings())));
    for (name, s) in cases {
        for moving in [false, true] {
            let secs = time_10s(s, moving);
            eprintln!("{name}{}: {:.1} ms for 10 s", if moving { " (moving)" } else { "" }, secs * 1e3);
            assert!(secs < 0.3, "{name}: {secs:.3} s for 10 s of audio");
        }
    }
}

#[test]
#[cfg_attr(debug_assertions, ignore = "timing is only meaningful in release builds")]
fn a_full_session_renders_in_real_time() {
    // Eight sources at their heaviest plus the master bus, on one thread, as the backends run
    // them (measured about 0.63 s for 10 s: 6 % of one core).
    let mut rng = Rng::new(2);
    let block: Vec<f32> = (0..2 * 480).map(|_| 0.3 * rng.white()).collect();
    let mut sources: Vec<_> =
        Preset::ALL.into_iter().take(8).map(|p| spatializer(heaviest(p.settings()), FS)).collect();
    let mut master = MasterBus::new(FS);
    let (mut out, mut sum, mut mixed) =
        (vec![0.0f32; block.len()], vec![0.0f32; block.len()], vec![0.0f32; block.len()]);
    let calls = 10 * FS as usize / 480;
    let start = Instant::now();
    for i in 0..calls {
        sum.fill(0.0);
        for (k, (sp, params)) in sources.iter_mut().enumerate() {
            if (i + k).is_multiple_of(3) {
                params.set(Settings {
                    azimuth_deg: (i as f32 * 0.9 + k as f32 * 40.0) % 360.0 - 180.0,
                    ..*sp.settings()
                });
            }
            sp.process_with_voice(&block, &mut out, 0.2, 0.3);
            sum.iter_mut().zip(&out).for_each(|(a, b)| *a += b);
        }
        master.process(&sum, &mut mixed);
    }
    let secs = start.elapsed().as_secs_f64();
    std::hint::black_box(&mixed);
    eprintln!("8 heaviest sources + master: {:.1} ms for 10 s", secs * 1e3);
    assert!(secs < 2.0, "{secs:.3} s for 10 s of an 8-source session");
}
