//! Decisions about taps and aggregate devices that need no Core Audio (unit-tested on every OS).

use std::time::Duration;

/// UID prefix of every aggregate device Sidestage creates. Private aggregates are visible to
/// their creator, so device lists skip these.
pub const AGGREGATE_UID_PREFIX: &str = "com.pixityy.sidestage.aggregate.";

/// A unique aggregate UID for this process and build counter.
pub fn aggregate_uid(kind: &str, pid: u32, counter: u32) -> String {
    format!("{AGGREGATE_UID_PREFIX}{kind}.{pid}.{counter}")
}

/// A property-list value, the shape of Core Audio's aggregate-device descriptions.
#[derive(Clone, Debug, PartialEq)]
pub enum Plist {
    Str(String),
    /// A CFBoolean. The header documents some of these keys as CFNumber 0/1; the HAL takes
    /// both, and booleans are what Apple's sample, Chromium, cpal and FineTune all pass
    /// (including for `tapautostart`, whose type is undocumented).
    Bool(bool),
    Array(Vec<Plist>),
    Dict(Vec<(&'static str, Plist)>),
}

impl Plist {
    fn str(s: &str) -> Self {
        Plist::Str(s.to_string())
    }

    /// Value for `key` in a dictionary.
    #[cfg(test)]
    pub fn get(&self, key: &str) -> Option<&Plist> {
        match self {
            Plist::Dict(entries) => entries.iter().find(|(k, _)| *k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// Dictionary keys from `AudioHardware.h` (`kAudioAggregateDevice*Key`, `kAudioSubDevice*Key`,
/// `kAudioSubTap*Key`). A macOS-only test checks them against objc2-core-audio's constants.
pub mod keys {
    pub const NAME: &str = "name";
    pub const UID: &str = "uid";
    pub const MAIN_SUB_DEVICE: &str = "master";
    pub const CLOCK_DEVICE: &str = "clock";
    pub const IS_PRIVATE: &str = "private";
    pub const IS_STACKED: &str = "stacked";
    pub const TAP_AUTO_START: &str = "tapautostart";
    pub const SUB_DEVICE_LIST: &str = "subdevices";
    pub const TAP_LIST: &str = "taps";
    pub const SUB_DEVICE_UID: &str = "uid";
    pub const SUB_DEVICE_DRIFT: &str = "drift";
    pub const SUB_TAP_UID: &str = "uid";
    pub const SUB_TAP_DRIFT: &str = "drift";
}

/// The music aggregate: the headphone output is the main and clock sub-device, the tap is its
/// input. Private, so it is invisible to other apps and dies with Sidestage. `tapautostart`
/// is on because with it off the tap can deliver silence until something else starts it
/// (cpal 0.17 fixed silent loopback capture this way).
pub fn render_aggregate(name: &str, uid: &str, output_uid: &str, tap_uid: &str, tap_drift: bool) -> Plist {
    Plist::Dict(vec![
        (keys::NAME, Plist::str(name)),
        (keys::UID, Plist::str(uid)),
        (keys::MAIN_SUB_DEVICE, Plist::str(output_uid)),
        (keys::CLOCK_DEVICE, Plist::str(output_uid)),
        (keys::IS_PRIVATE, Plist::Bool(true)),
        (keys::IS_STACKED, Plist::Bool(false)),
        (keys::TAP_AUTO_START, Plist::Bool(true)),
        (
            keys::SUB_DEVICE_LIST,
            Plist::Array(vec![Plist::Dict(vec![
                (keys::SUB_DEVICE_UID, Plist::str(output_uid)),
                // The clock device never needs drift compensation against itself.
                (keys::SUB_DEVICE_DRIFT, Plist::Bool(false)),
            ])]),
        ),
        (keys::TAP_LIST, Plist::Array(vec![sub_tap(tap_uid, tap_drift)])),
    ])
}

/// The sidechain aggregate: only a tap, no sub-device; its IOProc just meters.
pub fn tap_only_aggregate(name: &str, uid: &str, tap_uid: &str, tap_drift: bool) -> Plist {
    Plist::Dict(vec![
        (keys::NAME, Plist::str(name)),
        (keys::UID, Plist::str(uid)),
        (keys::IS_PRIVATE, Plist::Bool(true)),
        (keys::TAP_AUTO_START, Plist::Bool(true)),
        (keys::TAP_LIST, Plist::Array(vec![sub_tap(tap_uid, tap_drift)])),
    ])
}

fn sub_tap(tap_uid: &str, drift: bool) -> Plist {
    Plist::Dict(vec![(keys::SUB_TAP_UID, Plist::str(tap_uid)), (keys::SUB_TAP_DRIFT, Plist::Bool(drift))])
}

/// Drift compensation for the tap in the render aggregate. A tap scoped to the clock device
/// already runs on that clock, and so does a mixdown of an app that plays only to that device
/// (the HAL mixes a process's audio in its device's IO cycle). Compensating such a tap anyway
/// makes the HAL insert or drop a sample every so often (an audible tick). FineTune measured
/// the same artefact for any tap when the output is Bluetooth (or virtual, or of unknown
/// transport): tap and output follow one clock.
pub fn tap_drift_compensation(tap_on_output_clock: bool, output_shares_tap_clock: bool) -> bool {
    !tap_on_output_clock && !output_shares_tap_clock
}

/// Input streams of the render aggregate that belong to the tap: the ones after the output
/// device's own. `None` when there is no tap stream. A tap is one stream, which is also the
/// assumption when the device's input streams could not be counted.
pub fn tap_stream_count(aggregate_inputs: usize, device_inputs: Option<usize>) -> Option<usize> {
    match device_inputs {
        Some(device) => aggregate_inputs.checked_sub(device).filter(|&n| n > 0),
        None => (aggregate_inputs > 0).then_some(1),
    }
}

/// Per-stream "in use" flags for `kAudioDevicePropertyIOProcStreamUsage` on the render
/// aggregate's input side. The output device's own input streams (a headset or interface
/// microphone) come first and are turned off, so macOS neither powers a microphone (and asks
/// for microphone permission) nor switches a Bluetooth headset to its call profile; the
/// trailing tap streams stay on. `None` when there is nothing to turn off.
pub fn input_stream_usage(aggregate_inputs: usize, device_inputs: usize) -> Option<Vec<u32>> {
    if device_inputs == 0 || aggregate_inputs <= device_inputs {
        return None;
    }
    Some((0..aggregate_inputs).map(|i| u32::from(i >= device_inputs)).collect())
}

/// The variable-length C struct `AudioHardwareIOProcStreamUsage`
/// (`{ void *mIOProc; UInt32 mNumberStreams; UInt32 mStreamIsOn[n]; }`) in pointer-aligned
/// storage, ready for `AudioObjectSetPropertyData`.
pub struct StreamUsage {
    words: Vec<u64>,
    len: usize,
}

impl StreamUsage {
    pub fn new(ioproc: usize, flags: &[u32]) -> Self {
        let mut bytes = Vec::with_capacity(size_of::<usize>() + 4 + 4 * flags.len());
        bytes.extend_from_slice(&ioproc.to_ne_bytes());
        bytes.extend_from_slice(&(flags.len() as u32).to_ne_bytes());
        for f in flags {
            bytes.extend_from_slice(&f.to_ne_bytes());
        }
        let len = bytes.len();
        bytes.resize(len.div_ceil(8) * 8, 0);
        let words = bytes.as_chunks::<8>().0.iter().map(|w| u64::from_ne_bytes(*w)).collect();
        Self { words, len }
    }

    /// Start of the struct; valid while `self` lives.
    pub fn as_ptr(&self) -> *const u64 {
        self.words.as_ptr()
    }

    /// Size of the struct in bytes.
    pub fn byte_len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    fn bytes(&self) -> Vec<u8> {
        self.words.iter().flat_map(|w| w.to_ne_bytes()).take(self.len).collect()
    }
}

/// 0-based output channels for the rendered left and right. `preferred` is the device's
/// `kAudioDevicePropertyPreferredChannelsForStereo` (1-based); a missing or invalid pair falls
/// back to the first two channels, and a mono device gets both sides on its only channel.
pub fn output_pair(preferred: Option<[u32; 2]>, channels: usize) -> Option<(usize, usize)> {
    match channels {
        0 => None,
        1 => Some((0, 0)),
        _ => {
            let valid = |c: u32| c >= 1 && (c as usize) <= channels;
            match preferred {
                Some([l, r]) if valid(l) && valid(r) && l != r => Some((l as usize - 1, r as usize - 1)),
                _ => Some((0, 1)),
            }
        }
    }
}

/// Channels of the tap stream that hold the app's left and right. A stereo mixdown is plain
/// stereo (Core Audio duplicates mono sources); a tap scoped to a device stream has that
/// stream's layout, in which the app's stereo sits on the device's preferred stereo pair.
/// `device_pair` is that pair in device-global 0-based channels and `stream_first_channel` is
/// where the tapped stream starts.
pub fn tap_pair(
    tap_channels: usize,
    device_pair: Option<(usize, usize)>,
    stream_first_channel: usize,
) -> Option<(usize, usize)> {
    match tap_channels {
        0 => None,
        1 => Some((0, 0)),
        _ => {
            let local = |c: usize| c.checked_sub(stream_first_channel).filter(|&c| c < tap_channels);
            match device_pair.map(|(l, r)| (local(l), local(r))) {
                Some((Some(l), Some(r))) if l != r => Some((l, r)),
                _ => Some((0, 1)),
            }
        }
    }
}

/// Which kind of tap to build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapScope {
    /// `initWithProcesses:andDeviceUID:withStream:` on the output device: the tap takes the
    /// device stream's exact format, so there is no rate mismatch (macOS 14 does not resample
    /// taps) and nothing to resample on 15+. It only sees what the app sends to that device.
    Device,
    /// `initStereoMixdownOfProcesses:`: catches the app on whatever device it plays to.
    Mixdown,
}

/// Picks the tap kind from what the app currently plays to. An idle app keeps the current
/// choice, so pausing and resuming the music never rebuilds anything; with no choice yet, an
/// idle app is assumed to play to the system default output, like nearly every app does.
pub fn choose_scope(
    plays_to_output: bool,
    plays_elsewhere: bool,
    current: Option<TapScope>,
    output_is_default: bool,
) -> TapScope {
    if plays_to_output {
        TapScope::Device
    } else if plays_elsewhere {
        TapScope::Mixdown
    } else if let Some(current) = current {
        current
    } else if output_is_default {
        TapScope::Device
    } else {
        TapScope::Mixdown
    }
}

/// The stream holding device-wide channel `channel`, and that stream's first channel, given
/// the channel count of each stream in order.
pub fn stream_for_channel(stream_channels: &[usize], channel: usize) -> Option<(usize, usize)> {
    let mut first = 0;
    for (index, &count) in stream_channels.iter().enumerate() {
        if channel < first + count {
            return Some((index, first));
        }
        first += count;
    }
    None
}

/// The output stream a device-scoped tap must cover to capture (and mute) the app's stereo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceStream {
    /// Index among the device's output streams.
    pub index: usize,
    /// Its first device-wide channel.
    pub first_channel: usize,
    pub channels: usize,
}

/// The stream holding both channels of the device's stereo pair, or `None` when they live in
/// different streams (devices that expose one mono stream per channel). A device-scoped tap
/// covers one stream, so it would capture and mute only one side there: the other side of the
/// original would stay audible and Sidestage would play mono.
pub fn device_tap_stream(stream_channels: &[usize], pair: (usize, usize)) -> Option<DeviceStream> {
    let (index, first_channel) = stream_for_channel(stream_channels, pair.0)?;
    let (right_index, _) = stream_for_channel(stream_channels, pair.1)?;
    let channels = *stream_channels.get(index)?;
    (index == right_index).then_some(DeviceStream { index, first_channel, channels })
}

/// Whether a device-scoped tap took the format of the stream it was asked for, which the
/// header promises. Apple does not say whether the stream index counts output streams only
/// (Chromium and cpal pass 0 for any device) or all of the device's streams (FineTune skips the
/// input streams); a tap that landed on a different stream than intended is caught here when
/// the formats differ, and then replaced by a mixdown.
pub fn device_tap_matches(tap_channels: usize, tap_rate: f64, stream: DeviceStream, device_rate: f64) -> bool {
    tap_channels == stream.channels && !rate_changed(device_rate, tap_rate)
}

/// Sample rates differ by more than rounding noise.
pub fn rate_changed(was: f64, now: f64) -> bool {
    (was - now).abs() >= 0.5
}

/// The sidechain's exclusion list: the music app and Sidestage itself.
pub fn sidechain_excluded(music: &[u32], own: u32) -> Vec<u32> {
    let mut excluded = music.to_vec();
    if !excluded.contains(&own) {
        excluded.push(own);
    }
    excluded
}

/// A capture call that failed after this long was waiting for macOS's prompt, which gives up
/// after about a minute (Chromium restarts its audio process after 59 s).
pub fn prompt_timed_out(waited: Duration) -> bool {
    waited >= Duration::from_secs(45)
}

/// Whether a failed capture-permission probe means that macOS refuses audio capture. The probe
/// is Chromium's and only proven on mixdown taps, and a process that exits while the tap is
/// built may also upset it. So one failure only leads to another attempt (on a mixdown), and
/// after capture has worked in this process a failure is an ordinary, retried error.
pub fn probe_failure_is_denial(probe_failures: u32, granted_earlier: bool) -> bool {
    probe_failures >= 2 && !granted_earlier
}

/// Whether a session that follows the system default output (or stands in with it for a
/// missing chosen device) must move: macOS also reports default-output changes that land on the
/// same device, and rebuilding for those would only cause a gap. A default that cannot be read
/// does not count; if the device went away, its alive check rebuilds.
pub fn default_output_moved(playing_to: Option<u32>, default: Option<u32>) -> bool {
    match (playing_to, default) {
        (None, _) => true,
        (Some(current), Some(default)) => current != default,
        (Some(_), None) => false,
    }
}

/// Seconds of exact digital silence from the tap, while the app has been playing all along,
/// after which Sidestage suspects that macOS is withholding the audio (denied capture
/// permission delivers zeros and still mutes the app).
pub const SUSPICIOUS_SILENCE_SECS: f64 = 6.0;

/// Both durations count up to now; requiring the app to have played for the whole window keeps
/// the silence left over from a pause from raising the alarm when playback resumes.
pub fn looks_like_denied_capture(playing_secs: f64, silent_secs: f64) -> bool {
    playing_secs >= SUSPICIOUS_SILENCE_SECS && silent_secs >= SUSPICIOUS_SILENCE_SECS
}

/// Wait before rebuilding after `failures` consecutive failed attempts.
pub fn retry_delay(failures: u32) -> Duration {
    let ms = 500u64.saturating_mul(1 << failures.saturating_sub(1).min(4));
    Duration::from_millis(ms.min(5000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_aggregate_is_clocked_by_the_output() {
        let d = render_aggregate("Sidestage", "agg-1", "BuiltInSpeakerDevice", "TAP-UUID", false);
        assert_eq!(d.get("master"), Some(&Plist::Str("BuiltInSpeakerDevice".into())));
        assert_eq!(d.get("clock"), Some(&Plist::Str("BuiltInSpeakerDevice".into())));
        assert_eq!(d.get("private"), Some(&Plist::Bool(true)));
        assert_eq!(d.get("stacked"), Some(&Plist::Bool(false)));
        assert_eq!(d.get("tapautostart"), Some(&Plist::Bool(true)));
        let Some(Plist::Array(subs)) = d.get("subdevices") else { panic!("subdevices") };
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].get("uid"), Some(&Plist::Str("BuiltInSpeakerDevice".into())));
        let Some(Plist::Array(taps)) = d.get("taps") else { panic!("taps") };
        assert_eq!(taps[0].get("uid"), Some(&Plist::Str("TAP-UUID".into())));
        assert_eq!(taps[0].get("drift"), Some(&Plist::Bool(false)));
    }

    #[test]
    fn tap_only_aggregate_has_no_sub_devices() {
        let d = tap_only_aggregate("Sidestage sidechain", "agg-2", "T", true);
        assert_eq!(d.get("subdevices"), None);
        assert_eq!(d.get("master"), None);
        let Some(Plist::Array(taps)) = d.get("taps") else { panic!("taps") };
        assert_eq!(taps[0].get("drift"), Some(&Plist::Bool(true)));
        assert!(aggregate_uid("render", 42, 7).starts_with(AGGREGATE_UID_PREFIX));
        assert_ne!(aggregate_uid("render", 42, 7), aggregate_uid("render", 42, 8));
    }

    #[test]
    fn drift_only_for_mixdown_on_independent_clocks() {
        assert!(!tap_drift_compensation(true, false));
        assert!(!tap_drift_compensation(true, true));
        assert!(!tap_drift_compensation(false, true));
        assert!(tap_drift_compensation(false, false));
    }

    #[test]
    fn stream_usage_keeps_only_tap_streams() {
        assert_eq!(input_stream_usage(1, 0), None, "plain output device: nothing to disable");
        assert_eq!(input_stream_usage(3, 2), Some(vec![0, 0, 1]));
        assert_eq!(input_stream_usage(2, 2), None, "no tap stream: never disable everything");
        let usage = StreamUsage::new(0x1122_3344_5566_7788, &[0, 0, 1]);
        let b = usage.bytes();
        let p = size_of::<usize>();
        assert_eq!(usage.byte_len(), p + 4 + 12);
        assert_eq!(&b[..p], &0x1122_3344_5566_7788usize.to_ne_bytes()[..]);
        assert_eq!(&b[p..p + 4], &3u32.to_ne_bytes());
        assert_eq!(&b[p + 4..p + 8], &0u32.to_ne_bytes());
        assert_eq!(&b[p + 12..p + 16], &1u32.to_ne_bytes());
        assert_eq!(usage.as_ptr() as usize % 8, 0);
    }

    #[test]
    fn output_pairs() {
        assert_eq!(output_pair(None, 0), None);
        assert_eq!(output_pair(Some([1, 2]), 1), Some((0, 0)));
        assert_eq!(output_pair(Some([1, 2]), 2), Some((0, 1)));
        assert_eq!(output_pair(Some([3, 4]), 8), Some((2, 3)));
        assert_eq!(output_pair(Some([9, 10]), 8), Some((0, 1)));
        assert_eq!(output_pair(Some([0, 1]), 2), Some((0, 1)));
        assert_eq!(output_pair(Some([2, 2]), 2), Some((0, 1)));
        assert_eq!(output_pair(None, 6), Some((0, 1)));
    }

    #[test]
    fn tap_pairs() {
        assert_eq!(tap_pair(0, None, 0), None);
        assert_eq!(tap_pair(1, Some((0, 1)), 0), Some((0, 0)));
        assert_eq!(tap_pair(2, None, 0), Some((0, 1)));
        // 8-channel interface stream whose preferred stereo pair is 3/4.
        assert_eq!(tap_pair(8, Some((2, 3)), 0), Some((2, 3)));
        // Device with two stereo streams; the tapped second stream starts at channel 2.
        assert_eq!(tap_pair(2, Some((2, 3)), 2), Some((0, 1)));
        // Pair outside the tapped stream: use its first two channels.
        assert_eq!(tap_pair(2, Some((4, 5)), 0), Some((0, 1)));
    }

    #[test]
    fn scope_follows_the_app_with_hysteresis() {
        assert_eq!(choose_scope(true, false, None, false), TapScope::Device);
        assert_eq!(choose_scope(true, true, Some(TapScope::Mixdown), false), TapScope::Device);
        assert_eq!(choose_scope(false, true, Some(TapScope::Device), true), TapScope::Mixdown);
        assert_eq!(choose_scope(false, false, None, true), TapScope::Device);
        assert_eq!(choose_scope(false, false, None, false), TapScope::Mixdown);
        assert_eq!(choose_scope(false, false, Some(TapScope::Device), false), TapScope::Device);
        assert_eq!(choose_scope(false, false, Some(TapScope::Mixdown), true), TapScope::Mixdown);
    }

    #[test]
    fn finds_the_stream_of_a_channel() {
        assert_eq!(stream_for_channel(&[2], 0), Some((0, 0)));
        assert_eq!(stream_for_channel(&[2, 2], 2), Some((1, 2)));
        assert_eq!(stream_for_channel(&[1, 1, 1, 1], 3), Some((3, 3)));
        assert_eq!(stream_for_channel(&[2], 2), None);
        assert_eq!(stream_for_channel(&[], 0), None);
        assert_eq!(stream_for_channel(&[0, 2], 0), Some((1, 0)), "empty streams are skipped");
    }

    #[test]
    fn device_tap_needs_the_whole_pair_in_one_stream() {
        let stereo = DeviceStream { index: 0, first_channel: 0, channels: 2 };
        assert_eq!(device_tap_stream(&[2], (0, 1)), Some(stereo));
        // Second stereo stream of a 2x2 device.
        assert_eq!(device_tap_stream(&[2, 2], (2, 3)), Some(DeviceStream { index: 1, first_channel: 2, channels: 2 }));
        // Mono device: the fold-down pair (0, 0).
        assert_eq!(device_tap_stream(&[1], (0, 0)), Some(DeviceStream { index: 0, first_channel: 0, channels: 1 }));
        // One mono stream per channel: a one-stream tap would miss (and not mute) the right.
        assert_eq!(device_tap_stream(&[1, 1, 1, 1], (0, 1)), None);
        // Pair straddling two stereo streams.
        assert_eq!(device_tap_stream(&[2, 2], (1, 2)), None);
        assert_eq!(device_tap_stream(&[2], (0, 5)), None);
        assert_eq!(device_tap_stream(&[], (0, 1)), None);
    }

    #[test]
    fn device_tap_format_is_checked() {
        let stream = DeviceStream { index: 0, first_channel: 0, channels: 2 };
        assert!(device_tap_matches(2, 48_000.0, stream, 48_000.0));
        assert!(device_tap_matches(2, 48_000.2, stream, 48_000.0));
        // A headset's mono microphone stream instead of its stereo output.
        assert!(!device_tap_matches(1, 48_000.0, stream, 48_000.0));
        // Same channel count, wrong clock (an input stream at another rate).
        assert!(!device_tap_matches(2, 16_000.0, stream, 48_000.0));
        assert!(rate_changed(44_100.0, 48_000.0));
        assert!(!rate_changed(48_000.0, 48_000.0));
    }

    #[test]
    fn tap_streams_follow_the_device_inputs() {
        assert_eq!(tap_stream_count(1, Some(0)), Some(1), "plain output device");
        assert_eq!(tap_stream_count(3, Some(2)), Some(1), "headset with two mic streams");
        assert_eq!(tap_stream_count(2, Some(2)), None, "the tap stream is missing");
        assert_eq!(tap_stream_count(1, Some(4)), None, "inconsistent counts");
        assert_eq!(tap_stream_count(0, None), None);
        assert_eq!(tap_stream_count(3, None), Some(1), "unknown device inputs: only the last stream");
        // The usage flags turn off exactly what precedes the tap streams.
        let flags = input_stream_usage(3, 2).unwrap_or_default();
        assert_eq!(flags.iter().filter(|&&f| f == 1).count(), tap_stream_count(3, Some(2)).unwrap_or(0));
    }

    #[test]
    fn sidechain_always_excludes_sidestage() {
        assert_eq!(sidechain_excluded(&[10, 11], 99), vec![10, 11, 99]);
        assert_eq!(sidechain_excluded(&[], 99), vec![99]);
        assert_eq!(sidechain_excluded(&[99, 10], 99), vec![99, 10]);
    }

    #[test]
    fn probe_failures_and_prompt_timeouts() {
        assert!(!probe_failure_is_denial(1, false), "first failure: try again");
        assert!(probe_failure_is_denial(2, false), "second failure: denied");
        assert!(probe_failure_is_denial(5, false));
        assert!(!probe_failure_is_denial(2, true), "capture worked before in this process");
        assert!(!prompt_timed_out(Duration::from_secs(3)));
        assert!(prompt_timed_out(Duration::from_secs(60)));
    }

    #[test]
    fn default_output_moves() {
        assert!(default_output_moved(None, Some(5)), "nothing running yet");
        assert!(default_output_moved(None, None));
        assert!(default_output_moved(Some(5), Some(6)));
        assert!(!default_output_moved(Some(5), None), "unreadable default: left to the alive checks");
        assert!(!default_output_moved(Some(5), Some(5)), "a notification that lands on the same device");
    }

    #[test]
    fn silence_heuristic_and_backoff() {
        assert!(!looks_like_denied_capture(0.0, 100.0), "not playing");
        assert!(!looks_like_denied_capture(100.0, 1.0), "real audio arrives");
        assert!(!looks_like_denied_capture(1.0, 100.0), "just resumed after a pause");
        assert!(looks_like_denied_capture(SUSPICIOUS_SILENCE_SECS, SUSPICIOUS_SILENCE_SECS));
        assert_eq!(retry_delay(1), Duration::from_millis(500));
        assert_eq!(retry_delay(2), Duration::from_millis(1000));
        assert_eq!(retry_delay(4), Duration::from_millis(4000));
        assert_eq!(retry_delay(30), Duration::from_millis(5000));
    }
}
