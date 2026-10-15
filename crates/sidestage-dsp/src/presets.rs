use serde::{Deserialize, Serialize};

use crate::params::{Ducking, Room, Settings};

/// One-click starting points. Every preset keeps the music out of the frontal area where game
/// sounds usually sit, and away from the 2-5 kHz band where footsteps live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Preset {
    BehindYou,
    NextRoom,
    FarAway,
    Above,
    DeskSpeaker,
    Lounge,
}

impl Preset {
    pub const ALL: [Preset; 6] =
        [Preset::BehindYou, Preset::NextRoom, Preset::FarAway, Preset::Above, Preset::DeskSpeaker, Preset::Lounge];

    pub fn name(self) -> &'static str {
        match self {
            Preset::BehindYou => "Behind you",
            Preset::NextRoom => "Next room",
            Preset::FarAway => "Far away",
            Preset::Above => "Above",
            Preset::DeskSpeaker => "Desk speaker",
            Preset::Lounge => "Lounge",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Preset::BehindYou => "Close behind your head, in a small room. The default.",
            Preset::NextRoom => "Muffled, like a party through the wall.",
            Preset::FarAway => "Distant and airy, barely there.",
            Preset::Above => "Overhead and slightly behind.",
            Preset::DeskSpeaker => "A little speaker on your desk, front-right.",
            Preset::Lounge => "Wide and relaxed behind you. For casual games.",
        }
    }

    pub fn settings(self) -> Settings {
        let base = Settings {
            azimuth_deg: 180.0,
            elevation_deg: 0.0,
            distance_m: 1.5,
            width_deg: 60.0,
            volume_db: 0.0,
            // Keep 10-15 kHz: that band is part of what makes the music read as "behind".
            lowpass_hz: 18_000.0,
            highpass_hz: 30.0,
            bass_db: 0.0,
            presence_dip_db: -3.0,
            room: Room::Small,
            reverb_db: -12.0,
            ducking: DuckMode::Standard.ducking(),
            bypass: false,
        };
        match self {
            Preset::BehindYou => base,
            Preset::NextRoom => Settings {
                azimuth_deg: 150.0,
                distance_m: 5.0,
                width_deg: 0.0,
                volume_db: -1.0,
                lowpass_hz: 1_400.0,
                bass_db: 3.0,
                presence_dip_db: 0.0,
                room: Room::NextRoom,
                reverb_db: -6.0,
                ducking: DuckMode::Gentle.ducking(),
                ..base
            },
            Preset::FarAway => Settings {
                elevation_deg: 5.0,
                distance_m: 15.0,
                width_deg: 10.0,
                volume_db: 0.0,
                lowpass_hz: 6_000.0,
                presence_dip_db: -4.0,
                room: Room::Hall,
                reverb_db: -10.0,
                ..base
            },
            Preset::Above => {
                Settings { elevation_deg: 60.0, distance_m: 2.0, width_deg: 30.0, room: Room::Small, ..base }
            }
            Preset::DeskSpeaker => Settings {
                azimuth_deg: 35.0,
                elevation_deg: -20.0,
                distance_m: 0.7,
                width_deg: 0.0,
                volume_db: -1.5,
                lowpass_hz: 8_000.0,
                highpass_hz: 180.0,
                presence_dip_db: -2.0,
                room: Room::Desk,
                reverb_db: -16.0,
                ducking: DuckMode::Gentle.ducking(),
                ..base
            },
            Preset::Lounge => Settings {
                elevation_deg: 10.0,
                distance_m: 2.5,
                width_deg: 90.0,
                volume_db: 1.0,
                lowpass_hz: 16_000.0,
                presence_dip_db: 0.0,
                room: Room::Living,
                reverb_db: -10.0,
                ducking: DuckMode::Off.ducking(),
                ..base
            },
        }
    }

    /// The preset these settings were made from, if they still match it exactly.
    pub fn matching(settings: &Settings) -> Option<Preset> {
        Preset::ALL.into_iter().find(|p| p.settings() == *settings)
    }
}

/// Ducking strength presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DuckMode {
    Off,
    Gentle,
    Standard,
    Competitive,
}

impl DuckMode {
    pub const ALL: [DuckMode; 4] = [DuckMode::Off, DuckMode::Gentle, DuckMode::Standard, DuckMode::Competitive];

    pub fn name(self) -> &'static str {
        match self {
            DuckMode::Off => "Off",
            DuckMode::Gentle => "Gentle",
            DuckMode::Standard => "Standard",
            DuckMode::Competitive => "Competitive",
        }
    }

    pub fn ducking(self) -> Ducking {
        let (enabled, threshold_db, depth_db, band_dip_db, attack_ms, hold_ms, release_ms) = match self {
            DuckMode::Off => (false, -36.0, 8.0, 6.0, 20.0, 150.0, 600.0),
            DuckMode::Gentle => (true, -36.0, 4.0, 3.0, 40.0, 100.0, 900.0),
            DuckMode::Standard => (true, -36.0, 8.0, 6.0, 20.0, 150.0, 600.0),
            DuckMode::Competitive => (true, -42.0, 12.0, 9.0, 10.0, 250.0, 450.0),
        };
        Ducking { enabled, threshold_db, depth_db, band_dip_db, attack_ms, hold_ms, release_ms }
    }

    /// The mode these ducking settings match exactly, if any.
    pub fn matching(d: &Ducking) -> Option<DuckMode> {
        if !d.enabled {
            return Some(DuckMode::Off);
        }
        DuckMode::ALL.into_iter().find(|m| m.ducking() == *d)
    }
}

/// "Footstep-safe": carve the music out of the band competitive players listen to, and duck
/// hard when the game is loud. Applied on top of any preset.
pub fn make_footstep_safe(mut s: Settings) -> Settings {
    s.presence_dip_db = s.presence_dip_db.min(-6.0);
    s.lowpass_hz = s.lowpass_hz.min(8_000.0);
    s.bass_db = s.bass_db.min(-3.0);
    s.ducking = DuckMode::Competitive.ducking();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_in_range_and_distinct() {
        for p in Preset::ALL {
            let s = p.settings();
            assert_eq!(s.sanitized(), s, "{p:?} must already be within documented ranges");
            assert_eq!(Preset::matching(&s), Some(p));
        }
    }

    #[test]
    fn duck_modes_roundtrip() {
        for m in DuckMode::ALL {
            assert_eq!(DuckMode::matching(&m.ducking()), Some(m));
        }
    }
}
