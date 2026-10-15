//! CPU cost: the renderer must be a small fraction of real time.

mod common;

use std::time::Instant;

use common::*;
use sidestage_dsp::{Preset, Settings};

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
            params.set(Settings { azimuth_deg: (i as f32 * 0.7) % 360.0 - 180.0, ..settings });
        }
        sp.process(&block, &mut out, 0.2);
    }
    let secs = start.elapsed().as_secs_f64();
    std::hint::black_box(&out);
    secs
}

#[test]
#[cfg_attr(debug_assertions, ignore = "timing is only meaningful in release builds")]
fn ten_seconds_render_in_well_under_real_time() {
    for p in Preset::ALL {
        for moving in [false, true] {
            let secs = time_10s(p.settings(), moving);
            eprintln!("{p:?}{}: {:.1} ms for 10 s", if moving { " (moving)" } else { "" }, secs * 1e3);
            assert!(secs < 0.3, "{p:?}: {secs:.3} s for 10 s of audio");
        }
    }
}
