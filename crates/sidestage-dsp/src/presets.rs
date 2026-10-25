use serde::{Deserialize, Serialize};

use crate::params::{Ducking, Room, Settings};

/// One-click starting points.
///
/// Music presets keep the music out of the frontal area where game sounds usually sit, and away
/// from the 2-5 kHz band where footsteps live. Voice presets are tuned for voice chat (Discord,
/// TeamSpeak ...) and spoken word: close, levelled and in a little room, so every callout is clear
/// and sounds outside your head. Voice chat is never turned down for the game.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Preset {
    BehindYou,
    NextRoom,
    FarAway,
    Above,
    DeskSpeaker,
    Lounge,
    VoiceLeft,
    VoiceRight,
    OverShoulder,
    RadioComms,
    PodcastLeft,
}

/// What a preset is made for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PresetKind {
    Music,
    Voice,
}

impl Preset {
    pub const MUSIC: [Preset; 6] =
        [Preset::BehindYou, Preset::NextRoom, Preset::FarAway, Preset::Above, Preset::DeskSpeaker, Preset::Lounge];
    pub const VOICE: [Preset; 5] =
        [Preset::VoiceLeft, Preset::VoiceRight, Preset::OverShoulder, Preset::RadioComms, Preset::PodcastLeft];
    pub const ALL: [Preset; 11] = [
        Preset::BehindYou,
        Preset::NextRoom,
        Preset::FarAway,
        Preset::Above,
        Preset::DeskSpeaker,
        Preset::Lounge,
        Preset::VoiceLeft,
        Preset::VoiceRight,
        Preset::OverShoulder,
        Preset::RadioComms,
        Preset::PodcastLeft,
    ];

    pub fn kind(self) -> PresetKind {
        if Preset::VOICE.contains(&self) { PresetKind::Voice } else { PresetKind::Music }
    }

    pub fn name(self) -> &'static str {
        match self {
            Preset::BehindYou => "Behind you",
            Preset::NextRoom => "Next room",
            Preset::FarAway => "Far away",
            Preset::Above => "Above",
            Preset::DeskSpeaker => "Desk speaker",
            Preset::Lounge => "Lounge",
            Preset::VoiceLeft => "Voice chat left",
            Preset::VoiceRight => "Voice chat right",
            Preset::OverShoulder => "Over your shoulder",
            Preset::RadioComms => "Radio comms",
            Preset::PodcastLeft => "Podcast left",
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
            Preset::VoiceLeft => "Your squad close on your left, clear and levelled. For Discord.",
            Preset::VoiceRight => "Your squad close on your right, clear and levelled. For Discord.",
            Preset::OverShoulder => "A teammate leaning in behind your right shoulder.",
            Preset::RadioComms => "Tight walkie-talkie comms, front-left.",
            Preset::PodcastLeft => "A podcast or video playing on your left, in the room.",
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
            clarity_db: 0.0,
            leveler: 0.0,
            voice_chat: false,
        };
        // Voice chat: close and to the side, in a little room (enough reflections to sound
        // outside the head, little enough to stay crisp), levelled, with a clarity lift and the
        // boom of close headset mics trimmed. Never ducked for the game; other sources (the
        // music) turn down while it talks. A mono voice beside you at 1 m reaches the near ear
        // much louder than a stereo mix behind you, so the volume compensates.
        let voice = Settings {
            azimuth_deg: -70.0,
            elevation_deg: 0.0,
            distance_m: 1.0,
            width_deg: 0.0,
            volume_db: -8.0,
            lowpass_hz: 12_000.0,
            highpass_hz: 100.0,
            bass_db: -2.0,
            presence_dip_db: 0.0,
            room: Room::Small,
            reverb_db: -11.0,
            ducking: Ducking { under_voice: false, ..DuckMode::Off.ducking() },
            bypass: false,
            clarity_db: 3.0,
            leveler: 0.6,
            voice_chat: true,
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
            Preset::VoiceLeft => voice,
            Preset::VoiceRight => Settings { azimuth_deg: 70.0, ..voice },
            // Behind, the rear cues take presence out; more clarity puts it back.
            Preset::OverShoulder => Settings {
                azimuth_deg: 140.0,
                elevation_deg: 10.0,
                distance_m: 0.7,
                volume_db: -9.0,
                clarity_db: 4.5,
                ..voice
            },
            // Telephone band with steep-ish skirts (high-pass plus a bass cut), the boxy
            // reflections of a small speaker, and firm levelling.
            Preset::RadioComms => Settings {
                azimuth_deg: -40.0,
                volume_db: -6.0,
                highpass_hz: 300.0,
                lowpass_hz: 3_200.0,
                bass_db: -6.0,
                room: Room::Desk,
                reverb_db: -14.0,
                clarity_db: 4.0,
                leveler: 0.85,
                ..voice
            },
            // Spoken word from a video or podcast app: farther away and roomier, gently
            // levelled; it ducks for the game and under voice chat like music does. Such apps
            // are usually loudness-normalized (-14 to -16 LUFS), louder than voice chat, so the
            // level sits between the two.
            Preset::PodcastLeft => Settings {
                azimuth_deg: -90.0,
                distance_m: 1.5,
                volume_db: -5.0,
                highpass_hz: 80.0,
                lowpass_hz: 14_000.0,
                bass_db: 0.0,
                room: Room::Living,
                reverb_db: -10.0,
                clarity_db: 2.0,
                leveler: 0.4,
                ducking: DuckMode::Gentle.ducking(),
                voice_chat: false,
                ..voice
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
        Ducking { enabled, threshold_db, depth_db, band_dip_db, attack_ms, hold_ms, release_ms, under_voice: true }
    }

    /// `d` switched to this mode, keeping its [`Ducking::under_voice`] choice.
    pub fn apply(self, d: Ducking) -> Ducking {
        Ducking { under_voice: d.under_voice, ..self.ducking() }
    }

    /// The mode these ducking settings match exactly, if any ([`Ducking::under_voice`] is not
    /// part of the mode).
    pub fn matching(d: &Ducking) -> Option<DuckMode> {
        if !d.enabled {
            return Some(DuckMode::Off);
        }
        DuckMode::ALL.into_iter().find(|m| m.apply(*d) == *d)
    }
}

/// "Footstep-safe": carve the music out of the band competitive players listen to, and duck
/// hard when the game is loud. Applied on top of any preset.
///
/// Voice chat keeps its intelligibility: a lighter dip, and it is never ducked for the game.
pub fn make_footstep_safe(mut s: Settings) -> Settings {
    if s.voice_chat {
        s.presence_dip_db = s.presence_dip_db.min(-3.0);
        s.lowpass_hz = s.lowpass_hz.min(10_000.0);
        return s;
    }
    s.presence_dip_db = s.presence_dip_db.min(-6.0);
    s.lowpass_hz = s.lowpass_hz.min(8_000.0);
    s.bass_db = s.bass_db.min(-3.0);
    s.ducking = DuckMode::Competitive.apply(s.ducking);
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
            let no_voice = Ducking { under_voice: false, ..m.ducking() };
            assert_eq!(DuckMode::matching(&no_voice), Some(m), "under_voice is not part of the mode");
            assert!(!m.apply(no_voice).under_voice);
        }
    }

    #[test]
    fn preset_kinds() {
        assert_eq!(Preset::MUSIC.len() + Preset::VOICE.len(), Preset::ALL.len());
        for p in Preset::ALL {
            let voice = p.kind() == PresetKind::Voice;
            assert_eq!(Preset::VOICE.contains(&p), voice);
            if p.settings().voice_chat {
                assert!(voice, "{p:?}: voice chat presets are voice presets");
                assert!(!p.settings().ducking.enabled, "{p:?}: voice chat is never ducked for the game");
            }
        }
        assert!(Preset::VoiceLeft.settings().voice_chat && !Preset::BehindYou.settings().voice_chat);
        for p in Preset::MUSIC {
            assert!(p.settings().ducking.under_voice, "{p:?}: music turns down for voice chat");
        }
        for p in Preset::VOICE {
            let s = p.settings();
            assert!(s.leveler > 0.0 && s.clarity_db > 0.0, "{p:?} is levelled and clear");
            if s.voice_chat {
                assert!(!s.ducking.under_voice, "{p:?}: voice chat does not duck under itself");
            } else {
                assert!(s.ducking.enabled && s.ducking.under_voice, "{p:?}: spoken word ducks for both");
            }
        }
        let voice_chat = Preset::VOICE.into_iter().filter(|p| p.settings().voice_chat).count();
        assert_eq!(voice_chat, 4, "everything but the podcast preset is voice chat");
    }

    #[test]
    fn footstep_safe_is_idempotent_and_spares_voice_chat() {
        for p in Preset::ALL {
            let s = p.settings();
            let safe = make_footstep_safe(s);
            // The app shows "footstep-safe" as on when applying it changes nothing.
            assert_eq!(make_footstep_safe(safe), safe, "{p:?}");
            assert_eq!(safe.sanitized(), safe, "{p:?}");
            assert!(safe.presence_dip_db <= -3.0, "{p:?}");
            if s.voice_chat {
                assert_eq!(safe.ducking, s.ducking, "{p:?}: voice chat still never ducks");
                // A light dip, and the consonants up to 10 kHz stay (radio comms are narrower anyway).
                assert!(safe.presence_dip_db >= -3.0, "{p:?}");
                assert_eq!(safe.lowpass_hz, s.lowpass_hz.min(10_000.0), "{p:?}");
                assert_eq!((safe.clarity_db, safe.leveler, safe.bass_db), (s.clarity_db, s.leveler, s.bass_db));
            } else {
                assert_eq!(DuckMode::matching(&safe.ducking), Some(DuckMode::Competitive), "{p:?}");
                assert_eq!(safe.ducking.under_voice, s.ducking.under_voice, "{p:?}");
                assert!(safe.presence_dip_db <= -6.0 && safe.lowpass_hz <= 8_000.0, "{p:?}");
            }
        }
    }
}
