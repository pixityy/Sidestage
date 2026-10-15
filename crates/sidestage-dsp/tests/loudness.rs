//! Presets sound about equally loud, so switching between them is not a volume jump.

mod common;

use common::*;
use sidestage_dsp::{Preset, render_offline};

/// Stereo pink noise at about -20 dBFS RMS with a typical music-like inter-channel correlation
/// (~0.6), 4 s.
fn music_like() -> Vec<f32> {
    let mut rng = Rng::new(9);
    let (mut c, mut l, mut r) = (Pink::new(), Pink::new(), Pink::new());
    let frames = 4 * FS as usize;
    let mut out = Vec::with_capacity(frames * 2);
    for _ in 0..frames {
        let common = c.next(rng.white());
        out.push(0.08 * (common + 0.8 * l.next(rng.white())));
        out.push(0.08 * (common + 0.8 * r.next(rng.white())));
    }
    out
}

#[test]
fn presets_are_loudness_matched() {
    let input = music_like();
    let settle = FS as usize; // skip the first second (reverb build-up)
    let in_rms = db(rms(&input[2 * settle..]));
    let level = |p: Preset| db(rms(&render_offline(p.settings(), FS, &input, 0.0)[2 * settle..])) - in_rms;

    let levels = Preset::ALL.map(|p| (p, level(p)));
    let reference = levels[0].1;
    for (p, l) in levels {
        eprintln!("{p:?}: {l:+.2} dB vs input, {:+.2} dB vs BehindYou", l - reference);
    }
    assert_eq!(levels[0].0, Preset::BehindYou);
    assert!((-4.0..=1.0).contains(&reference), "BehindYou is {reference:+.2} dB vs the input");
    for (p, l) in levels {
        let rel = l - reference;
        let floor = if matches!(p, Preset::NextRoom | Preset::FarAway) { -8.0 } else { -4.0 };
        assert!((floor..=4.0).contains(&rel), "{p:?} is {rel:+.2} dB vs BehindYou");
    }
}
