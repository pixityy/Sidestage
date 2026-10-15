//! Logging setup. `RUST_LOG` overrides the defaults (Sidestage's crates at `info`, everything
//! else at `warn`).
//!
//! The window app also writes its log to `sidestage.log` in the state directory, because a
//! Windows GUI-subsystem process has no stderr anyone can see. The previous run's log is kept as
//! `sidestage.prev.log` for after a crash.

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use log::LevelFilter;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// stderr and the log file.
    Gui,
    /// stderr only.
    Cli,
}

/// Set once this process writes the log file.
static WRITING_FILE: AtomicBool = AtomicBool::new(false);

pub fn init(mode: Mode) {
    let mut builder = env_logger::Builder::new();
    builder.filter_level(LevelFilter::Warn).filter_module("sidestage", LevelFilter::Info);
    if let Ok(filters) = std::env::var("RUST_LOG") {
        builder.parse_filters(&filters);
    }
    if mode == Mode::Gui
        && let Some(file) = open_log_file()
    {
        builder.target(env_logger::Target::Pipe(Box::new(Tee { file })));
        // A file is not a terminal; keep it free of colour codes.
        builder.write_style(env_logger::WriteStyle::Never);
        WRITING_FILE.store(true, Ordering::Relaxed);
    }
    // Only fails if a logger is already set (tests), which is fine.
    let _ = builder.try_init();
}

/// The log file this process writes, if any (demo, screenshot and CLI runs only log to stderr).
pub fn written_log() -> Option<PathBuf> {
    if WRITING_FILE.load(Ordering::Relaxed) { log_path() } else { None }
}

/// Path of the log file, if the state directory is available. Looked up once: the Diagnostics
/// window shows it every frame, and the lookup creates the directory.
fn log_path() -> Option<PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| sidestage_platform::journal::state_dir().ok().map(|d| d.join("sidestage.log"))).clone()
}

fn open_log_file() -> Option<File> {
    let path = log_path()?;
    let _ = std::fs::rename(&path, path.with_extension("prev.log"));
    File::create(&path).ok()
}

/// Writes every log line to the file and, best effort, to stderr.
struct Tee {
    file: File,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        self.file.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        self.file.flush()
    }
}
