//! Small helpers with no Core Audio dependency (unit-tested on every OS).

use std::fmt;

/// A macOS product version such as `14.2.1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OsVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl OsVersion {
    /// First release with Core Audio process taps (`AudioHardwareCreateProcessTap`).
    pub const PROCESS_TAPS: OsVersion = OsVersion::new(14, 2, 0);
    /// First release whose taps resample to the reading device's rate by themselves. On 14.x a
    /// tap whose rate differs from the aggregate's plays at the wrong speed (Chromium's
    /// `kMacCatapSonomaInternalResampling` works around the same bug).
    pub const TAP_RESAMPLING: OsVersion = OsVersion::new(15, 0, 0);

    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self { major, minor, patch }
    }

    /// Parses `kern.osproductversion` output such as `"14.2.1"`, `"15.0"` or `"26"`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.trim().trim_end_matches('\0').split('.');
        let major = parts.next()?.trim().parse().ok()?;
        let mut next = || -> Option<u32> {
            match parts.next() {
                None => Some(0),
                Some(p) => p.trim().parse().ok(),
            }
        };
        let minor = next()?;
        let patch = next()?;
        Some(Self { major, minor, patch })
    }
}

impl fmt::Display for OsVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.patch == 0 {
            write!(f, "{}.{}", self.major, self.minor)
        } else {
            write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
        }
    }
}

/// Formats an `OSStatus` the way Apple documents most of them: as a four-character code when
/// it is one (`'!dev'`), always followed by the number.
pub fn fourcc(status: i32) -> String {
    let bytes = status.to_be_bytes();
    if bytes.iter().all(|b| (0x20..=0x7e).contains(b)) {
        let code: String = bytes.iter().map(|&b| b as char).collect();
        format!("'{code}' ({status})")
    } else {
        status.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_product_versions() {
        assert_eq!(OsVersion::parse("14.2.1"), Some(OsVersion::new(14, 2, 1)));
        assert_eq!(OsVersion::parse("15.0"), Some(OsVersion::new(15, 0, 0)));
        assert_eq!(OsVersion::parse("26\0"), Some(OsVersion::new(26, 0, 0)));
        assert_eq!(OsVersion::parse(" 14.4 \n"), Some(OsVersion::new(14, 4, 0)));
        assert_eq!(OsVersion::parse(""), None);
        assert_eq!(OsVersion::parse("fourteen"), None);
        assert_eq!(OsVersion::parse("14.x"), None);
    }

    #[test]
    fn version_ordering_gates_taps() {
        assert!(OsVersion::new(14, 1, 9) < OsVersion::PROCESS_TAPS);
        assert!(OsVersion::new(14, 2, 0) >= OsVersion::PROCESS_TAPS);
        assert!(OsVersion::new(26, 0, 0) >= OsVersion::TAP_RESAMPLING);
        assert!(OsVersion::new(14, 6, 1) < OsVersion::TAP_RESAMPLING);
        assert_eq!(OsVersion::new(14, 2, 0).to_string(), "14.2");
        assert_eq!(OsVersion::new(14, 2, 1).to_string(), "14.2.1");
    }

    #[test]
    fn formats_status_codes() {
        // kAudioHardwareBadDeviceError
        assert_eq!(fourcc(0x21646576), "'!dev' (560227702)");
        assert_eq!(fourcc(-50), "-50");
        assert_eq!(fourcc(0), "0");
    }
}
