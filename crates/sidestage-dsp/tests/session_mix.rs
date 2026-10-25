//! A whole session's mix, wired the way the backends and the app do it: several sources, each
//! with its own renderer and preset, voice chat ducking the others, summed and passed through
//! the master bus.

mod common;

use std::sync::Arc;

use common::*;
use sidestage_dsp::{MasterBus, Preset, Settings, SharedParams, Spatializer};

const BLOCK: usize = 480;
/// -1 dBFS.
const CEILING: f32 = 0.891_251;

struct Source {
    sp: Spatializer,
    params: Arc<SharedParams>,
    input: Vec<f32>,
    output: Vec<f32>,
}

impl Source {
    fn new(settings: Settings, input: Vec<f32>) -> Self {
        let (sp, params) = spatializer(settings, FS);
        let output = vec![0.0; input.len()];
        Self { sp, params, input, output }
    }
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
}

/// Renders every source block by block (voice chat sources first; the others duck under their
/// input peak, as the app does), sums them and runs the master bus. Returns the master output,
/// the raw sum's peak, and every source's metered ducking per block.
fn render_session(sources: &mut [Source], master: &mut MasterBus) -> (Vec<f32>, f32, Vec<Vec<f32>>) {
    let len = sources.iter().map(|s| s.input.len()).min().unwrap();
    let mut sum = vec![0.0f32; 2 * BLOCK];
    let mut out = vec![0.0f32; len];
    let mut sum_peak = 0.0f32;
    let mut ducks = vec![Vec::new(); sources.len()];
    for start in (0..len).step_by(2 * BLOCK) {
        let end = (start + 2 * BLOCK).min(len);
        let voice_peak = sources
            .iter()
            .filter(|s| s.sp.settings().voice_chat)
            .map(|s| peak(&s.input[start..end]))
            .fold(0.0f32, f32::max);
        sum.fill(0.0);
        for (k, s) in sources.iter_mut().enumerate() {
            let voice = if s.sp.settings().voice_chat { 0.0 } else { voice_peak };
            s.sp.process_with_voice(&s.input[start..end], &mut s.output[start..end], 0.0, voice);
            for (a, b) in sum.iter_mut().zip(&s.output[start..end]) {
                *a += b;
            }
            ducks[k].push(s.params.meters.take().duck_db);
        }
        sum_peak = sum_peak.max(peak(&sum[..end - start]));
        master.process(&sum[..end - start], &mut out[start..end]);
    }
    (out, sum_peak, ducks)
}

#[test]
fn music_podcast_and_voice_chat_mix_safely() {
    // Music behind (a hot master, +12 dB over the test noise), a podcast on the left, and voice
    // chat on the right that only starts talking after 3 s.
    let secs = 6.0;
    let music = gain_db(&music_like(1, secs), 12.0);
    let podcast = speech_like(2, secs, -14.0);
    let mut voice = speech_like(3, secs, -16.0);
    voice[..2 * 3 * FS as usize].fill(0.0);
    let mut sources = [
        Source::new(Preset::BehindYou.settings(), music),
        Source::new(Preset::PodcastLeft.settings(), podcast),
        Source::new(Preset::VoiceRight.settings(), voice),
    ];
    let mut master = MasterBus::new(FS);
    let (out, sum_peak, ducks) = render_session(&mut sources, &mut master);

    eprintln!("sum peak {:.2} dBFS, master peak {:.2} dBFS", 20.0 * sum_peak.log10(), 20.0 * peak(&out).log10());
    assert!(sum_peak > 1.0, "the sources together go past full scale ({sum_peak})");
    assert!(out.iter().all(|v| v.is_finite()) && peak(&out) <= CEILING * 1.0001);

    let blocks_per_s = FS as usize / BLOCK;
    let quiet = 3 * blocks_per_s;
    let max = |v: &[f32]| v.iter().copied().fold(0.0f32, f32::max);
    // No voice chat yet: nothing ducks (there is no game in this test).
    assert!(ducks.iter().all(|d| max(&d[..quiet]) == 0.0), "ducked before voice chat talked");
    // Voice chat talks: the music turns down by its full depth, the podcast gently; the voice
    // itself never.
    let music_depth = Preset::BehindYou.settings().ducking.depth_db;
    let podcast_depth = Preset::PodcastLeft.settings().ducking.depth_db;
    assert!(max(&ducks[0][quiet..]) > music_depth - 0.5, "music: {}", max(&ducks[0][quiet..]));
    assert!(max(&ducks[1][quiet..]) > podcast_depth - 0.5, "podcast: {}", max(&ducks[1][quiet..]));
    assert!(ducks[2].iter().all(|d| *d == 0.0), "voice chat was ducked");
    let music_out = &sources[0].output;
    let (before, during) = (&music_out[2 * FS as usize..6 * FS as usize], &music_out[8 * FS as usize..]);
    let drop = db(rms(before)) - db(rms(during));
    assert!(drop > 3.0, "the music only got {drop:.1} dB quieter under voice chat");
}

#[test]
fn eight_loud_sources_stay_under_the_ceiling() {
    // Eight sources of the same loud material pushed into their own limiters: the sum reaches
    // about +14 dBFS (+17 at most, if all eight peaked in phase), and the master bus still never
    // passes -1 dBFS.
    let secs = 2.0;
    let loud = gain_db(&music_like(4, secs), 18.0);
    let mut sources: Vec<Source> = Preset::ALL
        .into_iter()
        .take(8)
        .map(|p| Source::new(Settings { volume_db: 12.0, ..p.settings() }, loud.clone()))
        .collect();
    let mut master = MasterBus::new(FS);
    let (out, sum_peak, _) = render_session(&mut sources, &mut master);
    let master_peak = peak(&out);
    eprintln!(
        "8 sources: sum peak {:.1} dBFS, master peak {:.2} dBFS",
        20.0 * sum_peak.log10(),
        20.0 * master_peak.log10()
    );
    assert!(sum_peak > 4.0, "{sum_peak}");
    assert!(out.iter().all(|v| v.is_finite()) && master_peak <= CEILING * 1.0001, "{master_peak}");
    assert!(master_peak > 0.8, "the master limits, it does not mute: {master_peak}");
}
