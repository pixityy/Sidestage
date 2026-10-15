//! Which server we talk to, and the `module-null-sink` arguments it accepts.
//!
//! PulseAudio and pipewire-pulse speak the same protocol but differ in details that matter here:
//! pipewire-pulse rejects any module argument it does not know (`module_args_check` returns
//! `-EINVAL`), and only PulseAudio 15+ knows `norewinds=`. Old PulseAudio rejects unknown keys
//! too. So the arguments are built per server, as a list of attempts from richest to plainest.

/// Every capture sink Sidestage creates is named `sidestage_music_<random hex>`. The random
/// suffix matters: PulseAudio's stream-restore and WirePlumber persist every move by sink name,
/// so a fresh name per session guarantees those saved routes never pull an app into a later
/// Sidestage sink. The prefix is how leftovers from a crash are found.
pub const SINK_PREFIX: &str = "sidestage_music_";

/// Sample rate of the capture sink, the capture stream and the output stream.
pub const RATE: u32 = 48_000;

/// Shown in sound settings for the capture sink.
const SINK_DESCRIPTION: &str = "Sidestage Music Capture";

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

/// `module-null-sink` argument strings to try in order, richest first. Each later attempt drops
/// something an older or stricter server might reject: first `norewinds=`, then the quoted
/// `sink_properties=` (its nested quoting is parsed by both servers' code, but a plain fallback
/// costs nothing).
pub fn null_sink_args(kind: &ServerKind, sink_name: &str) -> Vec<String> {
    let base =
        format!("sink_name={sink_name} format=float32le rate={RATE} channels=2 channel_map=front-left,front-right");
    let props =
        format!("sink_properties=\"device.description='{SINK_DESCRIPTION}' node.description='{SINK_DESCRIPTION}'\"");
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

        let pw = null_sink_args(&ServerKind::PipeWire { version: "1.2".into() }, &name);
        assert_eq!(pw.len(), 2);
        assert!(
            pw.iter().all(|a| !a.contains("norewinds") && !a.contains("formats=")),
            "pipewire-pulse rejects unknown keys"
        );
        assert!(pw[0].contains("sink_properties=\"device.description='Sidestage Music Capture'"));
        assert!(pw[1].starts_with(&format!("sink_name={name} ")) && !pw[1].contains("sink_properties"));

        let pa16 = null_sink_args(&ServerKind::PulseAudio { version: Some((16, 1)) }, &name);
        assert_eq!(pa16.len(), 3);
        assert!(pa16[0].ends_with(" norewinds=1") && !pa16[1].contains("norewinds"));

        let pa14 = null_sink_args(&ServerKind::PulseAudio { version: Some((14, 2)) }, &name);
        assert!(pa14.iter().all(|a| !a.contains("norewinds")));
        for args in pa16.iter().chain(&pw) {
            assert!(args.contains("format=float32le rate=48000 channels=2 channel_map=front-left,front-right"));
            assert_eq!(sidestage_sink_in_args(args), Some(name.as_str()));
        }
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
