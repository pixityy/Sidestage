//! Formatting shared by the window and the command line.

use sidestage_platform::{SessionState, SessionStatus};

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

pub fn state(state: &SessionState, app: &str) -> String {
    match state {
        SessionState::Starting => format!("Starting {app}\u{2026}"),
        SessionState::Running => format!("Capturing {app}"),
        SessionState::WaitingForApp => format!("Waiting for {app}"),
        SessionState::Error(e) => format!("Error: {e}"),
        SessionState::Stopped => "Stopped".into(),
    }
}

/// One line for the status bar / terminal:
/// "Capturing Spotify · 48 kHz · +23 ms · 0 dropouts · Original hidden ✔".
pub fn status_summary(st: &SessionStatus) -> String {
    let mut parts = vec![state(&st.state, &st.app_name)];
    if st.state == SessionState::Running {
        parts.push(sample_rate(st.sample_rate));
        parts.push(format!("+{:.0} ms", st.latency_ms.max(0.0)));
        parts.push(match st.dropouts {
            1 => "1 dropout".to_owned(),
            n => format!("{n} dropouts"),
        });
        parts.push(if st.original_hidden { "Original hidden \u{2714}".into() } else { "Original audible".into() });
    }
    parts.join(" \u{00b7} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_matches_status_bar_format() {
        let st = SessionStatus {
            state: SessionState::Running,
            app_name: "Spotify".into(),
            sample_rate: 48_000,
            latency_ms: 23.4,
            original_hidden: true,
            ..Default::default()
        };
        assert_eq!(
            status_summary(&st),
            "Capturing Spotify \u{b7} 48 kHz \u{b7} +23 ms \u{b7} 0 dropouts \u{b7} Original hidden \u{2714}"
        );
        assert_eq!(sample_rate(44_100), "44.1 kHz");
        assert_eq!(to_db(1.0), 0.0);
        assert_eq!(to_db(0.0), -120.0);
    }
}
