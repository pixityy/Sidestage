//! Every room is stable and decays at about its nominal rate, measured through the whole
//! renderer (direct sound included).

mod common;

use common::*;
use sidestage_dsp::{DuckMode, Room, Settings};

/// Nominal T60 per room, from the `Room` docs.
const ROOMS: [(Room, f32); 5] =
    [(Room::Small, 0.35), (Room::Living, 0.5), (Room::Hall, 1.6), (Room::NextRoom, 0.7), (Room::Desk, 0.2)];

/// Energy of both ears per frame for an impulse played once the renderer's start fade is over;
/// index 0 is the impulse.
fn impulse_response(room: Room, secs: f32, fs: u32) -> Vec<f32> {
    let s = Settings {
        room,
        reverb_db: 0.0,
        distance_m: 20.0,
        ducking: DuckMode::Off.ducking(),
        ..dry_settings(180.0, 60.0)
    };
    let lead = fs as usize / 20;
    let frames = lead + (secs * fs as f32) as usize;
    let mut input = vec![0.0f32; frames * 2];
    input[2 * lead] = 0.5;
    input[2 * lead + 1] = 0.5;
    let (mut sp, _) = spatializer(s, fs);
    let out = run(&mut sp, &input, 480, 0.0);
    out[2 * lead..].chunks(2).map(|f| f[0] * f[0] + f[1] * f[1]).collect()
}

/// Broadband T60 from Schroeder backward integration, least-squares fit from -5 to -25 dB.
fn t60(energy: &[f32], fs: u32) -> f64 {
    let mut edc = vec![0.0f64; energy.len()];
    let mut acc = 0.0;
    for i in (0..energy.len()).rev() {
        acc += energy[i] as f64;
        edc[i] = acc;
    }
    let level = |i: usize| 10.0 * (edc[i] / edc[0]).log10();
    let a = (0..edc.len()).find(|&i| level(i) <= -5.0).unwrap();
    let b = (0..edc.len()).find(|&i| level(i) <= -25.0).unwrap();
    let n = (b - a + 1) as f64;
    let (sx, sy) = (a..=b).fold((0.0, 0.0), |(sx, sy), i| (sx + i as f64, sy + level(i)));
    let (mx, my) = (sx / n, sy / n);
    let (num, den) = (a..=b).fold((0.0, 0.0), |(num, den), i| {
        let dx = i as f64 - mx;
        (num + dx * (level(i) - my), den + dx * dx)
    });
    -60.0 / (num / den) / fs as f64
}

#[test]
fn every_room_decays_and_matches_its_t60() {
    for fs in [44_100, 48_000, 96_000] {
        for (room, nominal) in ROOMS {
            let e = impulse_response(room, 6.0 * nominal + 0.2, fs);
            let peak = e.iter().copied().fold(0.0f32, f32::max);
            let tail_start = (5.0 * nominal * fs as f32) as usize;
            let tail = e[tail_start..].iter().copied().fold(0.0f32, f32::max);
            let tail_db = 10.0 * (tail / peak).log10();
            assert!(tail_db < -60.0, "{room:?} @ {fs}: {tail_db:.1} dB after 5*T60");
            let measured = t60(&e, fs);
            eprintln!("{room:?} @ {fs}: T60 {measured:.3} s (nominal {nominal}), tail {tail_db:.0} dB");
            assert!(
                (measured / nominal as f64 - 1.0).abs() <= 0.15,
                "{room:?} @ {fs}: T60 {measured:.3} s, nominal {nominal} s"
            );
        }
    }
}

#[test]
fn room_off_is_dry() {
    let e = impulse_response(Room::Off, 0.5, FS);
    let peak = e.iter().copied().fold(0.0f32, f32::max);
    // After the first 20 ms (head model, limiter latency) only the 20 Hz high-pass's own
    // ringing remains, far below anything a room would add.
    let late = e[FS as usize / 50..].iter().copied().fold(0.0f32, f32::max);
    let late_db = 10.0 * (late / peak).log10();
    assert!(late_db < -60.0, "{late_db:.1} dB");
}

/// Turning a room on, switching rooms and leaving bypass must not click. A cleared reverb fed
/// music that is already playing behaves like one fed a signal that switched on abruptly: every
/// reflection and FDN round trip replays that step, 15-170 ms after the change, long after the
/// wet fade-in is over. A low tone (gentle slope, so small steps stand out) with the direct sound
/// far away and the room loud makes such steps easy to see.
#[test]
fn room_changes_do_not_click() {
    use sidestage_dsp::Preset;
    let base =
        Settings { distance_m: 20.0, reverb_db: 0.0, ducking: DuckMode::Off.ducking(), ..Preset::BehindYou.settings() };
    let with = |room: Room, bypass: bool| Settings { room, bypass, ..base };
    let cases = [
        ("Off -> Small", with(Room::Off, false), with(Room::Small, false)),
        ("Off -> Hall", with(Room::Off, false), with(Room::Hall, false)),
        ("Small -> Hall", with(Room::Small, false), with(Room::Hall, false)),
        ("Hall -> Desk", with(Room::Hall, false), with(Room::Desk, false)),
        ("leaving bypass", with(Room::Hall, true), with(Room::Hall, false)),
    ];
    for fs in [44_100, 48_000, 96_000] {
        let tone = sine(55.0, 0.2, 3 * fs as usize, fs);
        let input = interleave(&tone, &tone);
        for (name, from, to) in cases {
            let at = fs as usize;
            let (l, r) = split(&run_with_change(from, to, fs, &input, at, 480));
            let period = (fs / 55) as usize;
            for ch in [&l, &r] {
                let (worst, i) = worst_step_ratio(&ch[at - fs as usize / 2..], 55.0, fs, period);
                let ms = (i as f64 - fs as f64 / 2.0) * 1e3 / fs as f64;
                // Before the onset ramps: up to 16x (a loud tick 30-110 ms after the change).
                assert!(worst < 1.35, "{name} @ {fs}: step {worst:.2}x the tone's max slope, {ms:.0} ms after");
            }
        }
    }
}
