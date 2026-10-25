use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

/// Everything the user can change. Angles use the radar's top-down view:
/// azimuth 0 = straight ahead, +90 = right, -90 = left, ±180 = directly behind.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Direction of the music's centre, degrees in -180..=180.
    pub azimuth_deg: f32,
    /// Height, degrees in -40..=80 (positive is up).
    pub elevation_deg: f32,
    /// Distance from the centre of the head, metres in 0.5..=20.
    pub distance_m: f32,
    /// Angle between the virtual left and right speakers, degrees in 0..=120. 0 = mono point
    /// source. The music's left channel always stays on the listener's left side.
    pub width_deg: f32,
    /// Output trim in dB, -40..=12.
    pub volume_db: f32,
    /// "Muffle": low-pass cutoff in Hz, 800..=20000 (20000 = off).
    pub lowpass_hz: f32,
    /// High-pass cutoff in Hz, 20..=400.
    pub highpass_hz: f32,
    /// Low shelf at 150 Hz in dB, -12..=12 ("bass comes through walls").
    pub bass_db: f32,
    /// Static cut around 3.2 kHz where footsteps live, dB in -12..=0.
    pub presence_dip_db: f32,
    /// Room the music plays in.
    pub room: Room,
    /// Reverb level relative to the music in dB, -40..=6. The reverb level does not change with
    /// distance, so moving the music away also makes it sound more "roomy", as in real rooms.
    pub reverb_db: f32,
    pub ducking: Ducking,
    /// Pass the music through untouched (still hidden from its original output).
    pub bypass: bool,
    /// Speech clarity: a broad bell around 2.5 kHz, dB in -6..=6 (voice presets boost it).
    pub clarity_db: f32,
    /// Evens out loud and quiet talkers: 0 = off, 1 = strong levelling, in 0..=1. A gentle
    /// compressor with automatic make-up gain, applied before the 3D rendering.
    pub leveler: f32,
    /// This source is voice chat. Other sources with [`Ducking::under_voice`] turn down while it
    /// is talking.
    pub voice_chat: bool,
}

/// Room character for the reverb.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Room {
    /// Dry: no reflections or reverb.
    Off,
    /// Small room, ~0.35 s.
    #[default]
    Small,
    /// Living room, ~0.5 s, softer furnishings.
    Living,
    /// Large hall / outdoors-ish, ~1.6 s.
    Hall,
    /// Heard through a wall: the reverb is dark (≈1.2 kHz) and the room ~0.7 s.
    NextRoom,
    /// A small speaker on a desk: dense, very short early reflections, ~0.2 s.
    Desk,
}

impl Room {
    pub const ALL: [Room; 6] = [Room::Off, Room::Small, Room::Living, Room::Hall, Room::NextRoom, Room::Desk];

    pub fn name(self) -> &'static str {
        match self {
            Room::Off => "None",
            Room::Small => "Small room",
            Room::Living => "Living room",
            Room::Hall => "Hall",
            Room::NextRoom => "Next room",
            Room::Desk => "Desk",
        }
    }

    fn to_u32(self) -> u32 {
        Room::ALL.iter().position(|r| *r == self).unwrap_or(1) as u32
    }

    fn from_u32(v: u32) -> Room {
        Room::ALL.get(v as usize).copied().unwrap_or_default()
    }
}

/// Turn the music down when the game gets loud (sidechain from "everything except the music").
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ducking {
    pub enabled: bool,
    /// Game level (dBFS, peak envelope) above which ducking starts, -60..=-10.
    pub threshold_db: f32,
    /// Maximum broadband gain reduction in dB, 0..=24.
    pub depth_db: f32,
    /// Extra dynamic cut around 3.2 kHz while ducking, dB in 0..=12 (scaled by how much the
    /// broadband ducker is currently reducing).
    pub band_dip_db: f32,
    /// How fast ducking engages, ms in 1..=200.
    pub attack_ms: f32,
    /// How long it holds after the game gets quiet, ms in 0..=1000.
    pub hold_ms: f32,
    /// How fast the music comes back, ms in 50..=3000.
    pub release_ms: f32,
    /// Also turn this source down while a voice-chat source ([`Settings::voice_chat`]) is
    /// talking, with the same threshold, depth and timing. Works even when `enabled` (ducking
    /// under the game) is off.
    pub under_voice: bool,
}

impl Default for Ducking {
    fn default() -> Self {
        crate::DuckMode::Standard.ducking()
    }
}

impl Default for Settings {
    fn default() -> Self {
        crate::Preset::BehindYou.settings()
    }
}

impl Settings {
    /// Clamp every field into its documented range and replace non-finite values with defaults.
    pub fn sanitized(mut self) -> Self {
        let d = Settings::default();
        fn fix(v: f32, lo: f32, hi: f32, dflt: f32) -> f32 {
            if v.is_finite() { v.clamp(lo, hi) } else { dflt }
        }
        let mut az = if self.azimuth_deg.is_finite() { self.azimuth_deg } else { d.azimuth_deg };
        // Wrap into (-180, 180] so "behind" stays +180.
        az = 180.0 - (180.0 - az).rem_euclid(360.0);
        self.azimuth_deg = az;
        self.elevation_deg = fix(self.elevation_deg, -40.0, 80.0, d.elevation_deg);
        self.distance_m = fix(self.distance_m, 0.5, 20.0, d.distance_m);
        self.width_deg = fix(self.width_deg, 0.0, 120.0, d.width_deg);
        self.volume_db = fix(self.volume_db, -40.0, 12.0, d.volume_db);
        self.lowpass_hz = fix(self.lowpass_hz, 800.0, 20_000.0, d.lowpass_hz);
        self.highpass_hz = fix(self.highpass_hz, 20.0, 400.0, d.highpass_hz);
        self.bass_db = fix(self.bass_db, -12.0, 12.0, d.bass_db);
        self.presence_dip_db = fix(self.presence_dip_db, -12.0, 0.0, d.presence_dip_db);
        self.reverb_db = fix(self.reverb_db, -40.0, 6.0, d.reverb_db);
        let k = &mut self.ducking;
        let dk = d.ducking;
        k.threshold_db = fix(k.threshold_db, -60.0, -10.0, dk.threshold_db);
        k.depth_db = fix(k.depth_db, 0.0, 24.0, dk.depth_db);
        k.band_dip_db = fix(k.band_dip_db, 0.0, 12.0, dk.band_dip_db);
        k.attack_ms = fix(k.attack_ms, 1.0, 200.0, dk.attack_ms);
        k.hold_ms = fix(k.hold_ms, 0.0, 1000.0, dk.hold_ms);
        k.release_ms = fix(k.release_ms, 50.0, 3000.0, dk.release_ms);
        self.clarity_db = fix(self.clarity_db, -6.0, 6.0, d.clarity_db);
        self.leveler = fix(self.leveler, 0.0, 1.0, d.leveler);
        self
    }

    const WORDS: usize = 23;

    fn to_words(self) -> [u32; Self::WORDS] {
        let k = self.ducking;
        [
            self.azimuth_deg.to_bits(),
            self.elevation_deg.to_bits(),
            self.distance_m.to_bits(),
            self.width_deg.to_bits(),
            self.volume_db.to_bits(),
            self.lowpass_hz.to_bits(),
            self.highpass_hz.to_bits(),
            self.bass_db.to_bits(),
            self.presence_dip_db.to_bits(),
            self.room.to_u32(),
            self.reverb_db.to_bits(),
            u32::from(k.enabled),
            k.threshold_db.to_bits(),
            k.depth_db.to_bits(),
            k.band_dip_db.to_bits(),
            k.attack_ms.to_bits(),
            k.hold_ms.to_bits(),
            k.release_ms.to_bits(),
            u32::from(self.bypass),
            u32::from(k.under_voice),
            self.clarity_db.to_bits(),
            self.leveler.to_bits(),
            u32::from(self.voice_chat),
        ]
    }

    fn from_words(w: &[u32; Self::WORDS]) -> Self {
        let f = |i: usize| f32::from_bits(w[i]);
        Settings {
            azimuth_deg: f(0),
            elevation_deg: f(1),
            distance_m: f(2),
            width_deg: f(3),
            volume_db: f(4),
            lowpass_hz: f(5),
            highpass_hz: f(6),
            bass_db: f(7),
            presence_dip_db: f(8),
            room: Room::from_u32(w[9]),
            reverb_db: f(10),
            ducking: Ducking {
                enabled: w[11] != 0,
                threshold_db: f(12),
                depth_db: f(13),
                band_dip_db: f(14),
                attack_ms: f(15),
                hold_ms: f(16),
                release_ms: f(17),
                under_voice: w[19] != 0,
            },
            bypass: w[18] != 0,
            clarity_db: f(20),
            leveler: f(21),
            voice_chat: w[22] != 0,
        }
    }
}

/// Settings going to the audio thread and meters coming back, without locks.
///
/// `Settings` travel through a sequence lock made of atomics: the UI thread is the only writer
/// (callers must not call [`SharedParams::set`] from two threads at once); the audio thread
/// reads with a bounded number of retries and keeps its previous settings if it cannot get a
/// consistent copy, so it never waits.
#[derive(Debug)]
pub struct SharedParams {
    seq: AtomicU32,
    words: [AtomicU32; Settings::WORDS],
    pub meters: Meters,
}

impl SharedParams {
    pub fn new(settings: Settings) -> Arc<Self> {
        let words = settings.sanitized().to_words().map(AtomicU32::new);
        Arc::new(Self { seq: AtomicU32::new(0), words, meters: Meters::default() })
    }

    /// Publish new settings (UI thread). Values are sanitized first.
    pub fn set(&self, settings: Settings) {
        let words = settings.sanitized().to_words();
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        for (a, w) in self.words.iter().zip(words) {
            a.store(w, Ordering::Relaxed);
        }
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Latest settings, waiting as long as needed (non-real-time callers).
    pub fn get(&self) -> Settings {
        loop {
            if let Some(s) = self.try_get() {
                return s;
            }
            std::hint::spin_loop();
        }
    }

    /// Latest settings, or `None` if a write was in progress (real-time callers: keep using the
    /// previous settings and try again next block).
    pub fn try_get(&self) -> Option<Settings> {
        for _ in 0..4 {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                continue;
            }
            let mut w = [0u32; Settings::WORDS];
            for (dst, a) in w.iter_mut().zip(&self.words) {
                *dst = a.load(Ordering::Relaxed);
            }
            std::sync::atomic::fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return Some(Settings::from_words(&w));
            }
        }
        None
    }

    /// Change counter: differs whenever settings were published since the last read.
    pub fn version(&self) -> u32 {
        self.seq.load(Ordering::Acquire)
    }
}

/// Levels written by the audio thread (peak-hold) and read-and-reset by the UI.
#[derive(Debug, Default)]
pub struct Meters {
    input_peak: AtomicU32,
    output_peak: AtomicU32,
    sidechain_peak: AtomicU32,
    /// Current ducking gain reduction in dB (positive number), not peak-held.
    duck_db: AtomicU32,
}

impl Meters {
    /// Audio thread: report the peaks of the block just processed.
    #[inline]
    pub fn report(&self, input_peak: f32, output_peak: f32, sidechain_peak: f32, duck_db: f32) {
        raise(&self.input_peak, input_peak);
        raise(&self.output_peak, output_peak);
        raise(&self.sidechain_peak, sidechain_peak);
        self.duck_db.store(clean(duck_db).to_bits(), Ordering::Relaxed);
    }

    /// UI thread: (input, output, sidechain) linear peaks since the last call, plus the current
    /// ducking reduction in dB.
    pub fn take(&self) -> MeterReading {
        MeterReading {
            input_peak: take(&self.input_peak),
            output_peak: take(&self.output_peak),
            sidechain_peak: take(&self.sidechain_peak),
            duck_db: f32::from_bits(self.duck_db.load(Ordering::Relaxed)),
        }
    }
}

/// See [`Meters::take`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterReading {
    pub input_peak: f32,
    pub output_peak: f32,
    pub sidechain_peak: f32,
    pub duck_db: f32,
}

#[inline]
fn clean(v: f32) -> f32 {
    if v > 0.0 { v.min(f32::MAX) } else { 0.0 }
}

#[inline]
fn raise(a: &AtomicU32, v: f32) {
    a.fetch_max(clean(v).to_bits(), Ordering::Relaxed);
}

#[inline]
fn take(a: &AtomicU32) -> f32 {
    f32::from_bits(a.swap(0, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_roundtrip() {
        for p in crate::Preset::ALL {
            let s = p.settings();
            assert_eq!(Settings::from_words(&s.to_words()), s, "{p:?}");
        }
    }

    #[test]
    fn shared_params_roundtrip_and_sanitize() {
        let sp = SharedParams::new(Settings::default());
        let s = Settings { azimuth_deg: 270.0, distance_m: 1000.0, volume_db: f32::NAN, ..Settings::default() };
        let v0 = sp.version();
        sp.set(s);
        assert_ne!(sp.version(), v0);
        let got = sp.try_get().unwrap();
        assert_eq!(got.azimuth_deg, -90.0);
        assert_eq!(got.distance_m, 20.0);
        assert_eq!(got.volume_db, Settings::default().volume_db);
    }

    #[test]
    fn settings_serde_fills_missing_fields() {
        let s: Settings = serde_json::from_str(r#"{"azimuth_deg": 90.0}"#).unwrap();
        assert_eq!(s.azimuth_deg, 90.0);
        assert_eq!(s.room, Settings::default().room);
    }

    #[test]
    fn meters_peak_hold() {
        let m = Meters::default();
        m.report(0.5, 0.2, 0.0, 3.0);
        m.report(0.1, 0.9, 0.3, 4.0);
        let r = m.take();
        assert_eq!((r.input_peak, r.output_peak, r.sidechain_peak, r.duck_db), (0.5, 0.9, 0.3, 4.0));
        assert_eq!(m.take().input_peak, 0.0);
    }
}
