//! Per-application audio capture for Sidestage.
//!
//! A session plays any number of *sources* (apps chosen by the user: music behind you, a
//! podcast on the left, voice chat on the right ...) to one output. For every source the OS
//! backend:
//!
//! 1. **Captures** only that app's audio.
//! 2. **Hides the original** so the user does not hear the unprocessed copy as well.
//! 3. Runs the source's own [`Processor`] on it (its binaural renderer, with its own position
//!    and preset).
//!
//! It then sums all sources, runs the session's master [`Processor`] on the sum (a safety
//! limiter) and plays the result to the user's headphones as one stream: one latency, one
//! device to follow, and no clipping when several sources peak together.
//!
//! | OS      | Capture                                   | Hide the original                          | Output            |
//! |---------|-------------------------------------------|--------------------------------------------|-------------------|
//! | Windows | WASAPI process loopback (19041+; 20348+ official) | session volume to -120 dB + makeup gain    | WASAPI render     |
//! | macOS   | Core Audio process tap (14.2+)            | `CATapMutedWhenTapped`                     | same aggregate IOProc |
//! | Linux   | null sink monitor (PulseAudio/PipeWire)   | app streams moved onto the null sink       | Pulse playback stream |
//!
//! The game is never captured, routed or processed. Optionally a backend also measures
//! "everything except Sidestage's sources and Sidestage itself" (the sidechain, in practice
//! the game) so the DSP can duck sources when the game gets loud.
//!
//! Everything in this file is the contract between the backends and the app. Backends live in
//! `windows/`, `macos/` and `linux/` and are selected with [`platform()`].

use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub mod bridge;
pub mod journal;
mod level;

pub use level::{AtomicLevel, peak};

// The Linux and macOS backends keep their OS-independent logic compilable everywhere, so their
// unit tests also run on the other platforms' CI.
#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(any(target_os = "macos", test))]
mod macos;
#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod unsupported;
#[cfg(windows)]
mod windows;

/// Largest number of frames a backend may pass to [`Processor::process`] in one call.
/// Backends split bigger device buffers into chunks of at most this size.
pub const MAX_BLOCK_FRAMES: usize = 4096;

/// Stable identity of an app across restarts: the executable path on Windows, the bundle ID on
/// macOS, the `application.process.binary` (or `application.name`) on Linux.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AppKey(pub String);

impl AppKey {
    /// Whether two keys name the same app. Compare with this rather than `==`: a key can change
    /// when the app updates (on Windows, Store and Squirrel apps such as Spotify and Discord
    /// install every update into a new versioned folder), and backends find a saved app by this
    /// same rule.
    pub fn same_app(&self, other: &AppKey) -> bool {
        #[cfg(windows)]
        return self == other || windows::match_key(&self.0) == windows::match_key(&other.0);
        #[cfg(not(windows))]
        return self == other;
    }
}

/// An app that currently has (or recently had) an audio stream.
#[derive(Clone, Debug)]
pub struct AudioApp {
    pub key: AppKey,
    /// Human-readable name, e.g. "Spotify".
    pub name: String,
    /// Extra detail for the picker, e.g. "Spotify.exe" or the current media title.
    pub detail: String,
    /// Every process ID that belongs to this app (helpers included).
    pub pids: Vec<u32>,
    /// True when the app is producing sound right now (as far as the backend can tell).
    pub playing: bool,
    /// Recent linear peak level 0..=1 when the backend can meter it.
    pub peak: Option<f32>,
}

/// An output device the spatialized music can be played to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputDevice {
    /// Backend-specific ID; pass it back in [`SessionConfig::output`].
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// How the backend keeps the user from hearing the original, unprocessed audio.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum HideMethod {
    /// The backend's recommended method (see the table in the crate docs).
    #[default]
    Auto,
    /// Leave the original audible. Only useful for A/B comparison and debugging.
    KeepOriginal,
    /// Windows only: send the music app to this (unlistened) output device instead of using
    /// the session-volume method. The string is an [`OutputDevice::id`].
    ParkOn(String),
}

/// The app's identifier for one source, unique within a session and stable for as long as the
/// source exists (it is how [`Session::set_sources`] tells kept sources from new ones).
pub type SourceId = u32;

/// One app to capture, hide and render.
#[derive(Clone)]
pub struct SourceConfig {
    pub id: SourceId,
    pub app: AppKey,
    pub hide: HideMethod,
    /// Builds this source's processor (its binaural renderer). See [`ProcessorFactory`].
    pub processor: ProcessorFactory,
}

impl std::fmt::Debug for SourceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceConfig").field("id", &self.id).field("app", &self.app).field("hide", &self.hide).finish()
    }
}

/// What to start.
#[derive(Clone)]
pub struct SessionConfig {
    /// Output device ID, or `None` to follow the system default output.
    pub output: Option<String>,
    /// Measure "everything except Sidestage's sources and Sidestage" (the game) for ducking.
    pub sidechain: bool,
    /// Apps whose sound never counts as the game for ducking (a browser playing a video, a
    /// second music player ...). Sources and Sidestage itself never count anyway.
    pub sidechain_ignore: Vec<AppKey>,
    /// The sources to play. May be empty (the session then plays silence, or defers opening
    /// the output until there is a source). No two sources may have the same [`SourceId`] or
    /// name the same app ([`AppKey::same_app`]): an app can only be captured and hidden once.
    pub sources: Vec<SourceConfig>,
    /// Runs on the sum of all sources' outputs before it is played (a safety limiter).
    pub master: ProcessorFactory,
}

impl std::fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionConfig")
            .field("output", &self.output)
            .field("sidechain", &self.sidechain)
            .field("sidechain_ignore", &self.sidechain_ignore)
            .field("sources", &self.sources)
            .finish()
    }
}

impl SessionConfig {
    /// The error [`Platform::start`] and [`Session::set_sources`] return for sources that
    /// repeat an ID or an app, or `None` if the list is valid.
    pub fn duplicate_error(sources: &[SourceConfig]) -> Option<String> {
        for (i, a) in sources.iter().enumerate() {
            for b in &sources[i + 1..] {
                if a.id == b.id {
                    return Some(format!("two sources share the ID {}", a.id));
                }
                if a.app.same_app(&b.app) {
                    return Some(format!("{} is added twice; an app can only be one source", a.app.0));
                }
            }
        }
        None
    }
}

/// Real-time audio processor supplied by the app: a source's binaural renderer, or the
/// session's master bus.
///
/// `process` is called on the backend's real-time audio thread. It must not allocate, lock,
/// block, log or panic. All of a session's processors (every source's, then the master's) are
/// called one after another on the same thread for each block.
pub trait Processor: Send + 'static {
    /// `input` and `output` are interleaved stereo f32 with the same length (`frames * 2`,
    /// `frames <= MAX_BLOCK_FRAMES`). For a source, `input` is the captured app audio: backends
    /// have already undone any level change caused by hiding the original, so it is the app's
    /// audio at the app's own volume (silence while the app is away, so tails can ring out).
    /// For the master, `input` is the sum of every source's `output`.
    ///
    /// `sidechain_peak` is the recent linear peak (0..=1+) of everything except Sidestage's
    /// sources, the ignored apps and Sidestage itself (the game), or 0.0 when the sidechain is
    /// off or unavailable.
    fn process(&mut self, input: &[f32], output: &mut [f32], sidechain_peak: f32);
}

/// Builds a [`Processor`] for a stream at the given sample rate. Backends call it when a source
/// starts and whenever the sample rate changes (they may also call it when the output device
/// changes), never on the real-time thread, so it may allocate.
pub type ProcessorFactory = Arc<dyn Fn(u32) -> Box<dyn Processor> + Send + Sync>;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum SessionState {
    #[default]
    Starting,
    /// Session: the output is playing. Source: capturing and playing.
    Running,
    /// Source only: the app is not running (or has no audio stream yet); the source re-attaches
    /// by itself when it comes back.
    WaitingForApp,
    /// Unrecoverable for this session or source; the message is shown to the user.
    Error(String),
    Stopped,
}

/// Snapshot of a running session for the UI and diagnostics.
#[derive(Clone, Debug, Default)]
pub struct SessionStatus {
    /// The output stream's state (`Starting`, `Running`, `Error` or `Stopped`; `WaitingForApp`
    /// while no source has an app to capture and the output is not open yet).
    pub state: SessionState,
    pub output_name: String,
    pub sample_rate: u32,
    /// Recent linear peak of the sidechain, if enabled.
    pub sidechain_peak: f32,
    /// The backend is measuring the game right now, so a sidechain peak of 0 means the game is
    /// quiet rather than that ducking cannot hear it.
    pub sidechain_active: bool,
    /// Latency the output side adds (device buffers), in milliseconds. Each source's
    /// [`SourceStatus::latency_ms`] already includes it.
    pub latency_ms: f32,
    /// Output underruns since the session started.
    pub dropouts: u64,
    /// Plain-language problems that concern the whole session (e.g. "Spatial sound is on for
    /// your headphones").
    pub warnings: Vec<String>,
    /// One entry per configured source, in the order of [`SessionConfig::sources`].
    pub sources: Vec<SourceStatus>,
}

/// Snapshot of one source.
#[derive(Clone, Debug, Default)]
pub struct SourceStatus {
    pub id: SourceId,
    pub app_name: String,
    pub state: SessionState,
    /// The backend believes the user cannot hear this app's unprocessed original.
    pub original_hidden: bool,
    /// Recent linear peak of the captured audio (after level compensation).
    pub capture_peak: f32,
    /// Estimated latency Sidestage adds to this app's audio, in milliseconds.
    pub latency_ms: f32,
    /// Capture underruns + overruns since the source started.
    pub dropouts: u64,
    /// Current capture-vs-output clock correction in parts per million.
    pub drift_ppm: f32,
    /// Plain-language problems with this source (e.g. "Spotify is muted in the Volume mixer").
    pub warnings: Vec<String>,
}

/// A running session. Dropping it must also stop it and restore every source's original output;
/// [`Session::stop`] is the explicit path that reports errors.
pub trait Session: Send {
    fn status(&self) -> SessionStatus;
    /// Replace the session's sources while it runs, returning within about 200 ms (progress
    /// shows in [`Session::status`]).
    ///
    /// Sources are matched by [`SourceId`]. A source whose ID, app and hide method are
    /// unchanged keeps running untouched: no unhiding, its processor is not rebuilt (the app
    /// changes its settings through its own shared parameters), and no gap (on macOS a dip of a
    /// few tens of milliseconds while the set of captured apps changes). A new ID is started; a
    /// missing ID is stopped and its original restored. A changed app or hide method counts as
    /// remove + add. Errors are returned for invalid lists ([`SessionConfig::duplicate_error`]);
    /// problems with one source appear in its [`SourceStatus`] and never affect the others.
    fn set_sources(&mut self, sources: Vec<SourceConfig>) -> anyhow::Result<()>;
    /// Replace [`SessionConfig::sidechain_ignore`] while the session runs (matched with
    /// [`AppKey::same_app`]; apps that are not running are remembered for when they start).
    fn set_sidechain_ignore(&mut self, apps: Vec<AppKey>) -> anyhow::Result<()>;
    fn stop(self: Box<Self>) -> anyhow::Result<()>;
}

/// Whether this machine can run Sidestage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Support {
    Supported,
    /// Supported once the user grants a permission (macOS audio capture); the string says which.
    NeedsPermission(String),
    /// Not supported; the string explains why (e.g. "Windows 10 build 19041 or newer is required").
    Unsupported(String),
}

/// One OS backend. Methods may be called from the UI thread (which on Windows is a COM STA
/// thread owned by winit); backends do their OS work on their own threads where needed and
/// must not block the caller for more than ~200 ms.
pub trait Platform: Send {
    /// Short description, e.g. "Windows · WASAPI process loopback".
    fn name(&self) -> &'static str;
    fn support(&self) -> Support;
    /// Apps that currently have audio streams (including apps that are sources right now),
    /// excluding Sidestage itself and its parents, to avoid feedback loops. Cheap enough to call
    /// about once per second.
    fn list_apps(&mut self) -> anyhow::Result<Vec<AudioApp>>;
    fn list_outputs(&mut self) -> anyhow::Result<Vec<OutputDevice>>;
    /// Hide methods this backend offers, with a user-facing label, recommended first.
    fn hide_methods(&mut self) -> Vec<(HideMethod, String)>;
    /// Start the one session of this platform (the app keeps a single session and changes its
    /// sources with [`Session::set_sources`]; backends may refuse a second concurrent session).
    fn start(&mut self, config: SessionConfig) -> anyhow::Result<Box<dyn Session>>;
    /// Undo anything a previous Sidestage run left behind (crashed while apps were hidden).
    /// Returns one plain-language line per thing that was restored. Call once at startup.
    fn recover(&mut self) -> anyhow::Result<Vec<String>>;
}

/// The backend for the current OS.
pub fn platform() -> anyhow::Result<Box<dyn Platform>> {
    #[cfg(windows)]
    return windows::create();
    #[cfg(target_os = "macos")]
    return macos::create();
    #[cfg(target_os = "linux")]
    return linux::create();
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    return unsupported::create();
}

/// Backends restore the user's audio from a small helper process if Sidestage dies (the
/// "guardian" / "watchdog"). The helper is this same executable started with special
/// arguments. Call this first thing in `main`; if it returns `Some(code)`, exit with it.
pub fn run_helper_if_requested() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some(HELPER_ARG) {
        return None;
    }
    let rest = &args[1..];
    #[cfg(windows)]
    return Some(windows::run_helper(rest));
    #[cfg(target_os = "macos")]
    return Some(macos::run_helper(rest));
    #[cfg(target_os = "linux")]
    return Some(linux::run_helper(rest));
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = rest;
        return Some(2);
    }
}

/// First argument that marks a helper-process launch: `sidestage --sidestage-helper <...>`.
pub const HELPER_ARG: &str = "--sidestage-helper";

#[cfg(test)]
mod tests {
    use super::*;

    struct Silence;
    impl Processor for Silence {
        fn process(&mut self, _input: &[f32], output: &mut [f32], _sidechain_peak: f32) {
            output.fill(0.0);
        }
    }

    fn source(id: SourceId, app: &str) -> SourceConfig {
        SourceConfig {
            id,
            app: AppKey(app.into()),
            hide: HideMethod::Auto,
            processor: Arc::new(|_| Box::new(Silence) as Box<dyn Processor>),
        }
    }

    #[test]
    fn duplicate_sources_are_rejected() {
        assert_eq!(SessionConfig::duplicate_error(&[]), None);
        assert_eq!(SessionConfig::duplicate_error(&[source(1, "spotify"), source(2, "discord")]), None);
        assert!(
            SessionConfig::duplicate_error(&[source(1, "spotify"), source(1, "discord")]).unwrap().contains("ID 1")
        );
        assert!(
            SessionConfig::duplicate_error(&[source(1, "spotify"), source(2, "spotify")]).unwrap().contains("twice")
        );
    }
}
