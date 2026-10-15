//! Per-application audio capture for Sidestage.
//!
//! Each OS backend does three jobs for one chosen app (the "music app"):
//!
//! 1. **Capture** only that app's audio.
//! 2. **Hide the original** so the user does not hear the unprocessed copy as well.
//! 3. **Play** the spatialized result to the user's headphones, calling a [`Processor`] on the
//!    real-time thread to turn captured stereo into binaural stereo.
//!
//! | OS      | Capture                                   | Hide the original                          | Output            |
//! |---------|-------------------------------------------|--------------------------------------------|-------------------|
//! | Windows | WASAPI process loopback (19041+; 20348+ official) | session volume to -120 dB + makeup gain    | WASAPI render     |
//! | macOS   | Core Audio process tap (14.2+)            | `CATapMutedWhenTapped`                     | same aggregate IOProc |
//! | Linux   | null sink monitor (PulseAudio/PipeWire)   | app streams moved onto the null sink       | Pulse playback stream |
//!
//! The game is never captured, routed or processed. Optionally a backend also measures
//! "everything except the music" (the sidechain) so the DSP can duck the music when the
//! game gets loud.
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

/// What to start.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub app: AppKey,
    /// Output device ID, or `None` to follow the system default output.
    pub output: Option<String>,
    pub hide: HideMethod,
    /// Measure "everything except the music app and Sidestage" for ducking.
    pub sidechain: bool,
}

/// Real-time audio processor supplied by the app (the binaural renderer).
///
/// `process` is called on the backend's real-time audio thread. It must not allocate, lock,
/// block, log or panic.
pub trait Processor: Send + 'static {
    /// `input` is the captured music and `output` the buffer to fill, both interleaved stereo
    /// f32 with the same length (`frames * 2`, `frames <= MAX_BLOCK_FRAMES`). Backends have
    /// already undone any level change caused by hiding the original, so `input` is the app's
    /// audio at the app's own volume.
    ///
    /// `sidechain_peak` is the recent linear peak (0..=1+) of "everything except the music",
    /// or 0.0 when the sidechain is off or unavailable.
    fn process(&mut self, input: &[f32], output: &mut [f32], sidechain_peak: f32);
}

/// Builds a [`Processor`] for a stream at the given sample rate. Backends call it whenever they
/// (re)open their output stream, never on the real-time thread, so it may allocate.
pub type ProcessorFactory = Arc<dyn Fn(u32) -> Box<dyn Processor> + Send + Sync>;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum SessionState {
    #[default]
    Starting,
    /// Capturing and playing.
    Running,
    /// The app is not running (or has no audio stream yet); the session re-attaches by itself
    /// when it comes back.
    WaitingForApp,
    /// Unrecoverable for this session; the message is shown to the user.
    Error(String),
    Stopped,
}

/// Snapshot of a running session for the UI and diagnostics.
#[derive(Clone, Debug, Default)]
pub struct SessionStatus {
    pub state: SessionState,
    pub app_name: String,
    pub output_name: String,
    pub sample_rate: u32,
    /// The backend believes the user cannot hear the unprocessed original.
    pub original_hidden: bool,
    /// Recent linear peak of the captured music (after level compensation).
    pub capture_peak: f32,
    /// Recent linear peak of the sidechain, if enabled.
    pub sidechain_peak: f32,
    /// The backend is measuring "everything except the music" right now, so a sidechain peak
    /// of 0 means the game is quiet rather than that ducking cannot hear it.
    pub sidechain_active: bool,
    /// Estimated latency Sidestage adds on top of the app's own, in milliseconds.
    pub latency_ms: f32,
    /// Buffer underruns + overruns since the session started.
    pub dropouts: u64,
    /// Current capture-vs-output clock correction in parts per million.
    pub drift_ppm: f32,
    /// Plain-language problems to show the user (e.g. "Spotify is muted in the Volume mixer").
    pub warnings: Vec<String>,
}

/// A running capture-and-play session. Dropping it must also stop it and restore the original
/// output; [`Session::stop`] is the explicit path that reports errors.
pub trait Session: Send {
    fn status(&self) -> SessionStatus;
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
    /// Apps that currently have audio streams, excluding Sidestage itself (and its parents, to
    /// avoid feedback loops). Cheap enough to call about once per second.
    fn list_apps(&mut self) -> anyhow::Result<Vec<AudioApp>>;
    fn list_outputs(&mut self) -> anyhow::Result<Vec<OutputDevice>>;
    /// Hide methods this backend offers, with a user-facing label, recommended first.
    fn hide_methods(&mut self) -> Vec<(HideMethod, String)>;
    fn start(&mut self, config: SessionConfig, processor: ProcessorFactory) -> anyhow::Result<Box<dyn Session>>;
    /// Undo anything a previous Sidestage run left behind (crashed while an app was hidden).
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
