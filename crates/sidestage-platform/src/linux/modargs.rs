//! Which server we talk to, and the `module-null-sink` arguments it accepts.
//!
//! PulseAudio and pipewire-pulse speak the same protocol but differ in details that matter here:
//! pipewire-pulse rejects any module argument it does not know (`module_args_check` returns
//! `-EINVAL`), and only PulseAudio 15+ knows `norewinds=`. Old PulseAudio rejects unknown keys
//! too. So the arguments are built per server, as a list of attempts from richest to plainest.

/// Every capture sink Sidestage creates (one per source) is named `sidestage_music_<random
/// hex>`. The random suffix matters: PulseAudio's stream-restore and WirePlumber persist every
/// move by sink name, so a fresh name per sink guarantees those saved routes never pull an app
/// into a later Sidestage sink. The prefix is how leftovers from a crash are found (and it stays
/// "music" so leftovers of Sidestage 0.1 are found too).
pub const SINK_PREFIX: &str = "sidestage_music_";

/// Sample rate of the capture sink, the capture stream and the output stream.
pub const RATE: u32 = 48_000;

/// Shown in sound settings for a capture sink, followed by the source's app.
const SINK_DESCRIPTION: &str = "Sidestage Capture";
/// Longest app label put into a sink description.
const LABEL_MAX_CHARS: usize = 40;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerKind {
    /// Real PulseAudio, with its `major.minor` version when it could be parsed.
    PulseAudio { version: Option<(u32, u32)> },
    /// PipeWire's pulse server; `version` is PipeWire's.
    PipeWire { version: String },
}

impl ServerKind {
    /// From `GetServerInfo`: pipewire-pulse reports `server_name` "PulseAudio (on PipeWire x.y.z)".
    pub fn detect(server_name: &str, server_version: &str) -> Self {
        if let Some(i) = server_name.find("PipeWire") {
            let version = server_name[i + "PipeWire".len()..].trim().trim_end_matches(')').trim();
            return Self::PipeWire { version: version.to_owned() };
        }
        Self::PulseAudio { version: parse_version(server_version) }
    }

    /// User-facing, e.g. "PipeWire 1.0.5" or "PulseAudio 16.1".
    pub fn label(&self) -> String {
        match self {
            Self::PipeWire { version } if version.is_empty() => "PipeWire".to_owned(),
            Self::PipeWire { version } => format!("PipeWire {version}"),
            Self::PulseAudio { version: Some((major, minor)) } => format!("PulseAudio {major}.{minor}"),
            Self::PulseAudio { version: None } => "PulseAudio".to_owned(),
        }
    }

    pub fn is_pipewire(&self) -> bool {
        matches!(self, Self::PipeWire { .. })
    }

    /// Moving a stream back to the default sink clears its saved route only on PulseAudio 14+
    /// and pipewire-pulse; older PulseAudio pins it to that device instead.
    pub fn clears_routes_on_default_move(&self) -> bool {
        match self {
            Self::PipeWire { .. } => true,
            Self::PulseAudio { version } => version.is_none_or(|v| v >= (14, 0)),
        }
    }

    /// `norewinds=1` keeps the null sink from rewinding audio the monitor already handed to us
    /// (which would be heard as glitches in the capture). It arrived in PulseAudio 15.
    fn supports_norewinds(&self) -> bool {
        matches!(self, Self::PulseAudio { version: Some(v) } if *v >= (15, 0))
    }
}

/// `major.minor` from a version string such as "16.1", "15.99.1" or "17.0-ubuntu1".
pub fn parse_version(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.trim().split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// A fresh capture-sink name from a random seed.
pub fn sink_name(seed: u64) -> String {
    format!("{SINK_PREFIX}{:012x}", seed & 0xffff_ffff_ffff)
}

pub fn is_sidestage_sink(name: &str) -> bool {
    name.len() > SINK_PREFIX.len() && name.starts_with(SINK_PREFIX)
}

/// Whether a raw tagstruct (part of a control packet) holds a string that names a Sidestage
/// capture sink. The audio thread learns where the server moved its stream this way: decoding
/// the packet would allocate the sink name.
pub fn names_sidestage_sink(tagstruct: &[u8]) -> bool {
    // A string is 't', its bytes, then NUL; the name must go on after the prefix.
    let prefix = SINK_PREFIX.as_bytes();
    tagstruct.windows(prefix.len() + 2).any(|w| w[0] == b't' && &w[1..=prefix.len()] == prefix && w[w.len() - 1] != 0)
}

/// The description of the capture sink for the app `label` (its key or name), e.g. "Sidestage
/// Capture - spotify", so several sources' sinks can be told apart in sound settings.
///
/// The label comes from the app's stream properties and ends up inside a quoted value of the
/// module arguments, so it is reduced to letters, digits, spaces and `.`, `-`, `_`, `+`: a quote,
/// `=` or backslash could otherwise end the value early and smuggle in other arguments (such as
/// another `sink_name=`, which the leftover sweep trusts).
pub fn sink_description(label: &str) -> String {
    let label = label.rsplit('/').next().unwrap_or(label);
    let mut clean = String::new();
    for c in label.chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if (c.is_alphanumeric() || " .-_+".contains(c)) && !(c == ' ' && (clean.is_empty() || clean.ends_with(' '))) {
            clean.push(c);
        }
        if clean.chars().count() >= LABEL_MAX_CHARS {
            break;
        }
    }
    let clean = clean.trim_end();
    if clean.is_empty() { SINK_DESCRIPTION.to_owned() } else { format!("{SINK_DESCRIPTION} - {clean}") }
}

/// `module-null-sink` argument strings to try in order, richest first. Each later attempt drops
/// something an older or stricter server might reject: first `norewinds=`, then the quoted
/// `sink_properties=` (its nested quoting is parsed by both servers' code, but a plain fallback
/// costs nothing). `label` names the source's app, see [`sink_description`].
pub fn null_sink_args(kind: &ServerKind, sink_name: &str, label: &str) -> Vec<String> {
    let base =
        format!("sink_name={sink_name} format=float32le rate={RATE} channels=2 channel_map=front-left,front-right");
    let description = sink_description(label);
    let props = format!("sink_properties=\"device.description='{description}' node.description='{description}'\"");
    let mut attempts = Vec::with_capacity(3);
    if kind.supports_norewinds() {
        attempts.push(format!("{base} {props} norewinds=1"));
    }
    attempts.push(format!("{base} {props}"));
    attempts.push(base);
    attempts
}

/// The Sidestage capture sink a `module-null-sink` argument string created, if it created one.
/// This finds our modules even when their sink is not listed (it never appeared, or the server
/// does not report sink owners), so a crash between loading and use still gets cleaned up.
pub fn sidestage_sink_in_args(args: &str) -> Option<&str> {
    sink_name_in_args(args).filter(|name| is_sidestage_sink(name))
}

/// Whether the `module-null-sink` with arguments `args` and index `module` created the sink
/// `sink_name`, whose owner module the server reports as `owner`. The arguments decide when they
/// name a sink: a module that names another sink is never ours, whatever the reported owner says
/// (pipewire-pulse may report a PipeWire module there), because unloading it would remove another
/// app's virtual output. The owner only counts for a module whose arguments name no sink.
pub fn created_sink(args: &str, module: u32, sink_name: &str, owner: Option<u32>) -> bool {
    match sink_name_in_args(args) {
        Some(name) => name == sink_name,
        None => owner == Some(module),
    }
}

/// The `sink_name=` of a module argument string (our names never need quoting).
fn sink_name_in_args(args: &str) -> Option<&str> {
    args.split_whitespace().find_map(|a| a.strip_prefix("sink_name="))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_servers() {
        let pw = ServerKind::detect("PulseAudio (on PipeWire 1.0.5)", "15.0.0");
        assert_eq!(pw, ServerKind::PipeWire { version: "1.0.5".into() });
        assert_eq!(pw.label(), "PipeWire 1.0.5");
        assert!(pw.is_pipewire() && pw.clears_routes_on_default_move());
        let pa = ServerKind::detect("pulseaudio", "16.1");
        assert_eq!(pa, ServerKind::PulseAudio { version: Some((16, 1)) });
        assert_eq!(pa.label(), "PulseAudio 16.1");
        let old = ServerKind::detect("pulseaudio", "13.99.1");
        assert!(!old.clears_routes_on_default_move());
        assert!(ServerKind::detect("pulseaudio", "").clears_routes_on_default_move());
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("16.1"), Some((16, 1)));
        assert_eq!(parse_version("15.99.1"), Some((15, 99)));
        assert_eq!(parse_version("17.0-ubuntu1"), Some((17, 0)));
        assert_eq!(parse_version("14"), Some((14, 0)));
        assert_eq!(parse_version("x"), None);
    }

    #[test]
    fn module_args_per_server() {
        let name = sink_name(0xdead_beef_1234_5678);
        assert_eq!(name, "sidestage_music_beef12345678");
        assert!(is_sidestage_sink(&name) && !is_sidestage_sink(SINK_PREFIX) && !is_sidestage_sink("alsa_output.pci"));

        let pw = null_sink_args(&ServerKind::PipeWire { version: "1.2".into() }, &name, "spotify");
        assert_eq!(pw.len(), 2);
        assert!(
            pw.iter().all(|a| !a.contains("norewinds") && !a.contains("formats=")),
            "pipewire-pulse rejects unknown keys"
        );
        assert!(pw[0].contains("sink_properties=\"device.description='Sidestage Capture - spotify'"));
        assert!(pw[1].starts_with(&format!("sink_name={name} ")) && !pw[1].contains("sink_properties"));

        let pa16 = null_sink_args(&ServerKind::PulseAudio { version: Some((16, 1)) }, &name, "spotify");
        assert_eq!(pa16.len(), 3);
        assert!(pa16[0].ends_with(" norewinds=1") && !pa16[1].contains("norewinds"));

        let pa14 = null_sink_args(&ServerKind::PulseAudio { version: Some((14, 2)) }, &name, "spotify");
        assert!(pa14.iter().all(|a| !a.contains("norewinds")));
        for args in pa16.iter().chain(&pw) {
            assert!(args.contains("format=float32le rate=48000 channels=2 channel_map=front-left,front-right"));
            assert_eq!(sidestage_sink_in_args(args), Some(name.as_str()));
        }
    }

    #[test]
    fn descriptions_name_the_app_and_cannot_break_the_arguments() {
        assert_eq!(sink_description("spotify"), "Sidestage Capture - spotify");
        assert_eq!(sink_description("/usr/lib/firefox/firefox"), "Sidestage Capture - firefox");
        assert_eq!(sink_description("Müsik  Player	2"), "Sidestage Capture - Müsik Player 2");
        assert_eq!(sink_description(""), "Sidestage Capture");
        assert_eq!(sink_description("'\"=\\"), "Sidestage Capture");
        assert_eq!(sink_description(&"x".repeat(100)), format!("Sidestage Capture - {}", "x".repeat(40)));
        let hostile = "a' sink_name=sidestage_music_evil b=\"c";
        let description = sink_description(hostile);
        assert!(!description.contains(['\'', '"', '=', '\\']), "{description}");
        let args = null_sink_args(&ServerKind::PipeWire { version: "1.2".into() }, "sidestage_music_abc", hostile);
        for a in &args {
            assert_eq!(sidestage_sink_in_args(a), Some("sidestage_music_abc"));
            assert_eq!(a.matches("sink_name=").count(), 1, "{a}");
        }
    }

    #[test]
    fn spots_our_sinks_in_raw_packets() {
        // The tail of a PlaybackStreamMoved: sink index, sink name, suspended flag.
        let moved = |name: &str| {
            let mut b = vec![b'L', 0, 0, 0, 7, b't'];
            b.extend_from_slice(name.as_bytes());
            b.extend_from_slice(&[0, b'0']);
            b
        };
        assert!(names_sidestage_sink(&moved("sidestage_music_beef12345678")));
        assert!(!names_sidestage_sink(&moved("alsa_output.usb-headset")));
        assert!(!names_sidestage_sink(&moved("my_sidestage_music_x")), "only a name that starts with it");
        assert!(!names_sidestage_sink(&moved("sidestage_music_")), "the bare prefix is no sink");
        assert!(!names_sidestage_sink(b"tsidestage_music"), "a cut-off packet");
        assert!(!names_sidestage_sink(&[]));
    }

    #[test]
    fn finds_our_modules_by_their_arguments() {
        assert_eq!(
            sidestage_sink_in_args("format=float32le sink_name=sidestage_music_abcd2 rate=48000"),
            Some("sidestage_music_abcd2"),
            "exact name, wherever it appears"
        );
        assert_eq!(sidestage_sink_in_args("rate=48000"), None);
        assert_eq!(
            sidestage_sink_in_args("sink_name=easyeffects_sink"),
            None,
            "other apps' null sinks are never touched"
        );
        assert_eq!(sidestage_sink_in_args("sink_name=sidestage_music_"), None);
        assert_eq!(
            sidestage_sink_in_args("sink_properties=sink_name=sidestage_music_x"),
            None,
            "only a real sink_name= key counts"
        );
    }

    #[test]
    fn module_ownership_trusts_arguments_over_reported_owner() {
        let ours = "sidestage_music_abc";
        let args = format!("sink_name={ours} rate=48000");
        assert!(created_sink(&args, 7, ours, None), "found by its arguments");
        assert!(created_sink(&args, 7, ours, Some(99)), "a wrong owner does not matter");
        assert!(
            !created_sink("sink_name=easyeffects_sink", 7, ours, Some(7)),
            "a module naming another sink is never unloaded, even if reported as the owner"
        );
        assert!(created_sink("", 7, ours, Some(7)), "no sink named: the owner decides");
        assert!(!created_sink("rate=48000", 7, ours, Some(8)));
        assert!(!created_sink("", 7, ours, None));
    }
}
