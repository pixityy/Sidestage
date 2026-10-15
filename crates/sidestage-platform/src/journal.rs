//! Crash-recovery journals.
//!
//! Before a backend changes anything that outlives the process (an app's session volume on
//! Windows, a loaded null-sink module on Linux), it writes what it is about to change and how to
//! undo it to a small JSON file. A clean stop clears the file. If Sidestage dies, the helper
//! process or the next launch ([`crate::Platform::recover`]) reads it and restores.

use std::fs;
use std::io::Write;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Per-user directory for Sidestage's state files (created on demand).
///
/// Windows: `%LOCALAPPDATA%\Sidestage\data`, macOS: `~/Library/Application Support/Sidestage`,
/// Linux: `$XDG_STATE_HOME/sidestage` (or `~/.local/state/sidestage`).
pub fn state_dir() -> anyhow::Result<PathBuf> {
    let dirs =
        directories::ProjectDirs::from("", "", "Sidestage").context("could not determine the user's home directory")?;
    let dir = dirs.state_dir().unwrap_or_else(|| dirs.data_local_dir()).to_path_buf();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// A typed JSON file in [`state_dir`], written atomically (temp file + rename).
pub struct Journal<T> {
    path: PathBuf,
    _t: PhantomData<fn() -> T>,
}

impl<T: Serialize + DeserializeOwned> Journal<T> {
    /// `name` is a file stem such as `"windows-hush"`; `.json` is appended.
    pub fn open(name: &str) -> anyhow::Result<Self> {
        Ok(Self::at(state_dir()?.join(format!("{name}.json"))))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), _t: PhantomData }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The saved record, or `None` if there is none. A corrupt file is reported as an error but
    /// left in place so the user can inspect it.
    pub fn load(&self) -> anyhow::Result<Option<T>> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map(Some).with_context(|| format!("parsing {}", self.path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    pub fn save(&self, value: &T) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(value)?;
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut f = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path).with_context(|| format!("replacing {}", self.path.display()))
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", self.path.display()))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Rec {
        pid: u32,
        volume: f32,
    }

    #[test]
    fn roundtrip_and_clear() {
        let dir = std::env::temp_dir().join(format!("sidestage-journal-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let j: Journal<Rec> = Journal::at(dir.join("t.json"));
        assert_eq!(j.load().unwrap(), None);
        j.save(&Rec { pid: 7, volume: 0.5 }).unwrap();
        assert_eq!(j.load().unwrap(), Some(Rec { pid: 7, volume: 0.5 }));
        j.clear().unwrap();
        j.clear().unwrap();
        assert_eq!(j.load().unwrap(), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
