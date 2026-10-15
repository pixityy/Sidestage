//! Finding the sound server's socket and auth cookie the way libpulse does, plus small `/proc`
//! parsers. Pure functions over an environment lookup, so they are unit-tested on every OS.

use std::path::PathBuf;

/// Length of the native-protocol auth cookie. Real PulseAudio reads the cookie with
/// `pa_tagstruct_get_arbitrary(.., 256)`, which fails for any other length, and then drops the
/// connection *before* it tries the peer-credential check. So we always send exactly 256 bytes,
/// zero-filled when there is no usable cookie file (as libpulse does); the server then falls
/// back to the uid check. pipewire-pulse ignores the cookie.
pub const COOKIE_LEN: usize = 256;

/// Unix-socket paths to try, best first.
///
/// `$PULSE_SERVER` may list several servers separated by whitespace, each optionally prefixed
/// with `{machine-id}`. When it is set, libpulse uses only that list; we keep its local
/// (`unix:` or absolute-path) entries and refuse network-only values, which we cannot use.
/// Otherwise: `$PULSE_RUNTIME_PATH/native`, `$XDG_RUNTIME_DIR/pulse/native`, the systemd default
/// `/run/user/<uid>/pulse/native` (for when `XDG_RUNTIME_DIR` is unset, e.g. under `sudo -E`),
/// and the system-wide daemon's socket.
pub fn socket_candidates(var: &dyn Fn(&str) -> Option<String>, uid: u32) -> Result<Vec<PathBuf>, String> {
    if let Some(server) = nonempty(var("PULSE_SERVER")) {
        let paths: Vec<String> = server.split_whitespace().filter_map(unix_entry).collect();
        if paths.is_empty() {
            return Err(format!(
                "PULSE_SERVER is set to \"{server}\", which is not a local sound server. Sidestage \
                 needs a local PulseAudio or PipeWire (pipewire-pulse) server."
            ));
        }
        return Ok(dedup(paths));
    }
    let mut paths = Vec::new();
    if let Some(dir) = nonempty(var("PULSE_RUNTIME_PATH")) {
        paths.push(format!("{}/native", dir.trim_end_matches('/')));
    }
    if let Some(dir) = nonempty(var("XDG_RUNTIME_DIR")) {
        paths.push(format!("{}/pulse/native", dir.trim_end_matches('/')));
    }
    paths.push(format!("/run/user/{uid}/pulse/native"));
    paths.push("/var/run/pulse/native".to_owned());
    Ok(dedup(paths))
}

/// Cookie files to try, best first: `$PULSE_COOKIE`, `$XDG_CONFIG_HOME/pulse/cookie` (default
/// `~/.config/pulse/cookie`), then the legacy `~/.pulse-cookie`.
pub fn cookie_candidates(var: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = nonempty(var("PULSE_COOKIE")) {
        paths.push(path);
    }
    let home = nonempty(var("HOME")).map(|h| h.trim_end_matches('/').to_owned());
    let config = nonempty(var("XDG_CONFIG_HOME"))
        .map(|c| c.trim_end_matches('/').to_owned())
        .or_else(|| home.as_ref().map(|h| format!("{h}/.config")));
    if let Some(config) = config {
        paths.push(format!("{config}/pulse/cookie"));
    }
    if let Some(home) = home {
        paths.push(format!("{home}/.pulse-cookie"));
    }
    dedup(paths)
}

/// The cookie to send: the first [`COOKIE_LEN`] bytes of the file, or zeros when there is no
/// file or it is too short to be a valid cookie (libpulse rejects those too).
pub fn auth_cookie(file: Option<&[u8]>) -> Vec<u8> {
    let mut cookie = vec![0; COOKIE_LEN];
    if let Some(bytes) = file.filter(|b| b.len() >= COOKIE_LEN) {
        cookie.copy_from_slice(&bytes[..COOKIE_LEN]);
    }
    cookie
}

/// Parent PID from the contents of `/proc/<pid>/stat`. The command name in parentheses may
/// itself contain spaces and parentheses, so fields are counted from the *last* `)`.
pub fn parent_pid_from_stat(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

/// True if `/proc/<pid>/stat` describes a process that has exited but was not reaped yet
/// (zombie or dead): its name and PID linger, but it holds nothing any more.
pub fn exited_from_stat(stat: &str) -> bool {
    stat.rfind(')')
        .and_then(|i| stat[i + 1..].split_whitespace().next())
        .is_some_and(|state| matches!(state, "Z" | "X" | "x"))
}

/// One entry of a PulseAudio server string, if it names a local socket.
fn unix_entry(entry: &str) -> Option<String> {
    let entry = match entry.strip_prefix('{') {
        Some(rest) => rest.split_once('}')?.1,
        None => entry,
    };
    match entry.strip_prefix("unix:") {
        Some(path) => (!path.is_empty()).then(|| path.to_owned()),
        None => entry.starts_with('/').then(|| entry.to_owned()),
    }
}

fn nonempty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

fn dedup(paths: Vec<String>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for p in paths {
        let p = PathBuf::from(p);
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    fn strs(paths: &[PathBuf]) -> Vec<String> {
        paths.iter().map(|p| p.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn default_socket_search_order() {
        let var = env(&[("XDG_RUNTIME_DIR", "/run/user/1000/"), ("PULSE_RUNTIME_PATH", "/tmp/pa")]);
        let got = strs(&socket_candidates(&var, 1000).unwrap());
        assert_eq!(
            got,
            ["/tmp/pa/native", "/run/user/1000/pulse/native", "/var/run/pulse/native"],
            "duplicates removed, PULSE_RUNTIME_PATH is the runtime dir itself"
        );
        let got = strs(&socket_candidates(&env(&[]), 42).unwrap());
        assert_eq!(got, ["/run/user/42/pulse/native", "/var/run/pulse/native"]);
    }

    #[test]
    fn pulse_server_list_is_exclusive() {
        let var =
            env(&[("PULSE_SERVER", "{abc}unix:/x/native tcp:host:4713 /y/native unix:"), ("XDG_RUNTIME_DIR", "/r")]);
        assert_eq!(strs(&socket_candidates(&var, 1).unwrap()), ["/x/native", "/y/native"]);
        let remote = env(&[("PULSE_SERVER", "tcp:192.168.1.2")]);
        let err = socket_candidates(&remote, 1).unwrap_err();
        assert!(err.contains("tcp:192.168.1.2"), "{err}");
        let blank = env(&[("PULSE_SERVER", "  "), ("XDG_RUNTIME_DIR", "/r")]);
        assert_eq!(strs(&socket_candidates(&blank, 1).unwrap())[0], "/r/pulse/native");
    }

    #[test]
    fn cookie_locations() {
        let var = env(&[("HOME", "/home/u/"), ("PULSE_COOKIE", "/c")]);
        assert_eq!(strs(&cookie_candidates(&var)), ["/c", "/home/u/.config/pulse/cookie", "/home/u/.pulse-cookie"]);
        let var = env(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/cfg")]);
        assert_eq!(strs(&cookie_candidates(&var)), ["/cfg/pulse/cookie", "/home/u/.pulse-cookie"]);
        assert!(cookie_candidates(&env(&[])).is_empty());
    }

    #[test]
    fn cookie_is_always_256_bytes() {
        assert_eq!(auth_cookie(None), vec![0; 256]);
        assert_eq!(auth_cookie(Some(&[7; 10])), vec![0; 256], "short files are invalid cookies");
        let long: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let c = auth_cookie(Some(&long));
        assert_eq!(c.len(), 256);
        assert_eq!(c[..], long[..256]);
    }

    #[test]
    fn parses_ppid_with_awkward_command_names() {
        assert_eq!(parent_pid_from_stat("1234 (bash) S 1200 1234 1234 0"), Some(1200));
        assert_eq!(parent_pid_from_stat("77 (my (weird) app) R 5 77 77"), Some(5));
        assert_eq!(parent_pid_from_stat("garbage"), None);
    }

    #[test]
    fn detects_exited_processes() {
        assert!(exited_from_stat("77 (sidestage) Z 5 77 77"));
        assert!(exited_from_stat("77 (a) b) X 5"));
        assert!(!exited_from_stat("77 (sidestage) S 5 77 77"));
        assert!(!exited_from_stat("77 (Z) R 5"));
        assert!(!exited_from_stat("garbage"));
    }
}
