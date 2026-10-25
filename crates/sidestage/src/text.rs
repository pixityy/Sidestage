//! Formatting shared by the window and the command line.

use sidestage_platform::{SessionState, SessionStatus, SourceStatus};

/// Linear amplitude to dBFS, with silence at -120.
pub fn to_db(linear: f32) -> f32 {
    if linear > 1e-6 { 20.0 * linear.log10() } else { -120.0 }
}

/// "48 kHz", "44.1 kHz".
pub fn sample_rate(hz: u32) -> String {
    if hz == 0 {
        return "? kHz".into();
    }
    let khz = f64::from(hz) / 1000.0;
    if (khz - khz.round()).abs() < 0.05 { format!("{khz:.0} kHz") } else { format!("{khz:.1} kHz") }
}

/// What one source is doing, e.g. "Capturing Spotify".
pub fn state(state: &SessionState, app: &str) -> String {
    match state {
        SessionState::Starting => format!("Starting {app}\u{2026}"),
        SessionState::Running => format!("Capturing {app}"),
        SessionState::WaitingForApp => format!("Waiting for {app}"),
        SessionState::Error(e) => format!("Error: {e}"),
        SessionState::Stopped => "Stopped".into(),
    }
}

/// One line per source for the terminal: "Discord: capturing".
pub fn source_line(s: &SourceStatus) -> String {
    let what = match &s.state {
        SessionState::Starting => "starting\u{2026}".to_owned(),
        SessionState::Running if s.original_hidden => "capturing, original hidden".to_owned(),
        SessionState::Running => "capturing, original audible".to_owned(),
        SessionState::WaitingForApp => "waiting for the app to play audio".to_owned(),
        SessionState::Error(e) => format!("failed: {e}"),
        SessionState::Stopped => "stopped".to_owned(),
    };
    format!("{}: {what}", s.app_name)
}

/// "Spotify", "Spotify and Discord", "Spotify, Pocket Casts and Discord".
pub fn join_names(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

/// One line for the status bar / terminal:
/// "3 sources · 48 kHz · +62 ms · 0 dropouts · Originals hidden ✔".
pub fn status_summary(st: &SessionStatus) -> String {
    match &st.state {
        SessionState::Running => {}
        SessionState::Starting => return "Starting\u{2026}".into(),
        SessionState::WaitingForApp => return "Waiting for the apps to play audio".into(),
        SessionState::Error(e) => return format!("Error: {e}"),
        SessionState::Stopped => return "Stopped".into(),
    }
    let live: Vec<&SourceStatus> = st.sources.iter().filter(|s| s.state == SessionState::Running).collect();
    let total = st.sources.len();
    let mut parts = vec![if live.len() == total {
        count(total, "source", "sources")
    } else {
        format!("{} of {} capturing", live.len(), count(total, "source", "sources"))
    }];
    parts.push(sample_rate(st.sample_rate));
    // The slowest source is what the user notices.
    let latency = live.iter().map(|s| s.latency_ms).fold(st.latency_ms, f32::max);
    parts.push(format!("+{:.0} ms", latency.max(0.0)));
    let dropouts = st.dropouts + st.sources.iter().map(|s| s.dropouts).sum::<u64>();
    parts.push(count(dropouts as usize, "dropout", "dropouts"));
    let audible: Vec<&str> = live.iter().filter(|s| !s.original_hidden).map(|s| s.app_name.as_str()).collect();
    match (live.len(), audible.as_slice()) {
        (0, _) => {}
        (1, []) => parts.push("Original hidden \u{2714}".into()),
        (_, []) => parts.push("Originals hidden \u{2714}".into()),
        (_, [one]) => parts.push(format!("{one} audible")),
        (_, many) => parts.push(format!("{} originals audible", many.len())),
    }
    parts.join(" \u{00b7} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: u32, name: &str, state: SessionState, hidden: bool) -> SourceStatus {
        SourceStatus {
            id,
            app_name: name.into(),
            state,
            original_hidden: hidden,
            latency_ms: 61.6 - id as f32,
            dropouts: u64::from(id == 2),
            ..Default::default()
        }
    }

    #[test]
    fn summary_matches_status_bar_format() {
        let mut st = SessionStatus {
            state: SessionState::Running,
            sample_rate: 48_000,
            latency_ms: 14.0,
            sources: vec![
                source(0, "Spotify", SessionState::Running, true),
                source(1, "Pocket Casts", SessionState::Running, true),
                source(3, "Discord", SessionState::Running, true),
            ],
            ..Default::default()
        };
        assert_eq!(
            status_summary(&st),
            "3 sources \u{b7} 48 kHz \u{b7} +62 ms \u{b7} 0 dropouts \u{b7} Originals hidden \u{2714}"
        );
        st.sources[2].state = SessionState::WaitingForApp;
        st.sources.push(source(2, "VLC", SessionState::Running, false));
        assert_eq!(
            status_summary(&st),
            "3 of 4 sources capturing \u{b7} 48 kHz \u{b7} +62 ms \u{b7} 1 dropout \u{b7} VLC audible"
        );
        st.sources.truncate(1);
        assert_eq!(
            status_summary(&st),
            "1 source \u{b7} 48 kHz \u{b7} +62 ms \u{b7} 0 dropouts \u{b7} Original hidden \u{2714}"
        );
        st.sources.clear();
        assert_eq!(status_summary(&st), "0 sources \u{b7} 48 kHz \u{b7} +14 ms \u{b7} 0 dropouts");
        st.state = SessionState::Error("gone".into());
        assert_eq!(status_summary(&st), "Error: gone");

        assert_eq!(
            source_line(&source(1, "Discord", SessionState::Running, true)),
            "Discord: capturing, original hidden"
        );
        assert_eq!(
            source_line(&source(1, "Discord", SessionState::WaitingForApp, false)),
            "Discord: waiting for the app to play audio"
        );
        assert_eq!(sample_rate(44_100), "44.1 kHz");
        assert_eq!(join_names(&[]), "");
        assert_eq!(join_names(&["Spotify"]), "Spotify");
        assert_eq!(join_names(&["Spotify", "Discord"]), "Spotify and Discord");
        assert_eq!(join_names(&["Spotify", "Pocket Casts", "Discord"]), "Spotify, Pocket Casts and Discord");
        assert_eq!(to_db(1.0), 0.0);
        assert_eq!(to_db(0.0), -120.0);
    }
}
