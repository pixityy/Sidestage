//! Presets sound about equally loud, so switching between them (or mixing several sources) is
//! not a volume jump: music presets measured with music-like pink noise, voice presets with
//! speech-like noise at the levels voice chat and spoken-word apps usually deliver.

mod common;

use common::*;
use sidestage_dsp::{DuckMode, Preset, PresetKind, Settings, render_offline};

/// Frames skipped before measuring (reverb build-up, leveller settling).
const SETTLE: usize = FS as usize;

#[test]
fn music_presets_are_loudness_matched() {
    let input = music_like(9, 4.0);
    let in_rms = db(rms(&input[2 * SETTLE..]));
    let level = |p: Preset| db(rms(&render_offline(p.settings(), FS, &input, 0.0)[2 * SETTLE..])) - in_rms;

    let levels = Preset::MUSIC.map(|p| (p, level(p)));
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

/// Output minus input integrated loudness (LU), ducking off.
fn lufs_gain(s: Settings, input: &[f32]) -> f64 {
    let s = Settings { ducking: DuckMode::Off.ducking(), ..s };
    lufs(&render_offline(s, FS, input, 0.0)[2 * SETTLE..], FS) - lufs(&input[2 * SETTLE..], FS)
}

/// The speech levels a voice preset is matched at, LUFS. Voice chat: [`SPEECH_LUFS`], the
/// leveller's reference. Podcasts and videos are mostly loudness-normalized to -14 to -16 LUFS
/// and some are quieter, so spoken word sits between that and voice chat.
fn matched_levels(p: Preset) -> &'static [f64] {
    if p.settings().voice_chat { &[SPEECH_LUFS] } else { &[-15.0, SPEECH_LUFS] }
}

#[test]
fn voice_presets_are_as_loud_as_the_music() {
    // Same loudness in, about the same loudness out: a voice next to you plays at the level of
    // the music behind you. (Away from these levels the leveller holds the voice near this
    // level, while music follows its input; see the next test.)
    let reference = lufs_gain(Preset::BehindYou.settings(), &music_like(9, 6.0));
    let speech = speech_like(31, 12.0, SPEECH_LUFS);
    for p in Preset::ALL.into_iter().filter(|p| p.kind() == PresetKind::Voice) {
        let s = p.settings();
        for &l in matched_levels(p) {
            let g = lufs_gain(s, &gain_db(&speech, l - SPEECH_LUFS)) - reference;
            eprintln!("{p:?}: {g:+.2} LU vs the music, speech at {l} LUFS");
            let limit = if s.voice_chat { 1.0 } else { 2.0 };
            assert!(g.abs() <= limit, "{p:?}: {g:+.2} LU vs the music for speech at {l} LUFS");
        }
    }
}

#[test]
fn voice_presets_even_out_loud_and_quiet_talkers() {
    // Two talkers 12 dB apart come out within 5 dB of each other (leveller 0.6 and up), and a
    // talker anywhere in the usual range lands within a few dB of the others.
    let loud = speech_like(31, 12.0, SPEECH_LUFS);
    for p in Preset::VOICE {
        let s = Settings { ducking: DuckMode::Off.ducking(), ..p.settings() };
        let out = |db: f64| {
            let x = gain_db(&loud, db);
            lufs(&render_offline(s, FS, &x, 0.0)[2 * SETTLE..], FS)
        };
        let levels = [6.0, 0.0, -6.0, -12.0].map(out);
        let spread_12 = levels[1] - levels[3];
        eprintln!("{p:?} (leveller {}): -14/-20/-26/-32 LUFS in -> {levels:.1?} LUFS out", s.leveler);
        let limit = if s.leveler >= 0.6 { 5.0 } else { 6.5 };
        assert!(spread_12 <= limit, "{p:?}: 12 dB apart in, {spread_12:.1} dB apart out");
        assert!(spread_12 > 1.0, "{p:?}: a leveller, not a squasher: {spread_12:.1} dB");
        let range = levels[0] - levels[3];
        assert!(levels.windows(2).all(|w| w[0] > w[1]) && range < 18.0 * 0.6, "{p:?}: {levels:.1?}");
    }
}
