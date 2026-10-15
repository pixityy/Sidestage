//! One capturing Sidestage at a time.
//!
//! Two instances would both hide the same app and both run `Platform::recover` against each
//! other's live changes. The window app and the `run`/`restore` commands therefore hold an
//! exclusive lock on a file in the state directory. The OS releases it when the process ends,
//! even after a crash, so a stale lock can never block the next start.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use anyhow::Context as _;

/// Held for as long as this process may change the audio setup.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

pub enum Acquire {
    Acquired(InstanceLock),
    /// Another Sidestage holds the lock.
    AlreadyRunning,
}

pub fn acquire() -> anyhow::Result<Acquire> {
    acquire_at(&sidestage_platform::journal::state_dir()?.join("sidestage.lock"))
}

fn acquire_at(path: &Path) -> anyhow::Result<Acquire> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Acquire::Acquired(InstanceLock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(Acquire::AlreadyRunning),
        Err(TryLockError::Error(e)) if e.kind() == std::io::ErrorKind::Unsupported => {
            // File systems without locking: carry on rather than refuse to start.
            log::warn!("{} cannot be locked ({e}); not guarding against a second instance", path.display());
            Ok(Acquire::Acquired(InstanceLock { _file: file }))
        }
        Err(TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

pub const ALREADY_RUNNING: &str =
    "Sidestage is already running (in another window or as 'sidestage run'). Close it first, then try again.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_holder_is_refused_until_the_first_lets_go() {
        let dir = std::env::temp_dir().join(format!("sidestage-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sidestage.lock");
        let Acquire::Acquired(first) = acquire_at(&path).unwrap() else { panic!("first lock must succeed") };
        assert!(matches!(acquire_at(&path).unwrap(), Acquire::AlreadyRunning));
        drop(first);
        assert!(matches!(acquire_at(&path).unwrap(), Acquire::Acquired(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
