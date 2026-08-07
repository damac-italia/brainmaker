// SPDX-License-Identifier: GPL-3.0-or-later

//! The local record of which content version is installed.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    /// Hash of the archive that produced the current `content/` directory.
    pub hash: String,
    /// Time of the last successful install, in seconds since the Unix epoch.
    pub updated_at_unix: u64,
}

impl State {
    pub fn new(hash: impl Into<String>) -> Self {
        Self {
            hash: hash.into(),
            updated_at_unix: now_unix(),
        }
    }
}

/// Reads the state file.
///
/// Returns `None` when the file is absent or unreadable. A corrupt state file
/// is not an error, because the caller then reinstalls the content, which
/// repairs the state.
pub fn read(path: &Path) -> Option<State> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes the state file through a temporary file and a rename, so that a
/// crash never leaves a half-written state file.
pub fn write(path: &Path, state: &State) -> Result<()> {
    let parent = path
        .parent()
        .context("the state file path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create the directory {}", parent.display()))?;

    let temp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(state).context("cannot serialize the state")?;
    fs::write(&temp, text).with_context(|| format!("cannot write {}", temp.display()))?;
    fs::rename(&temp, path)
        .with_context(|| format!("cannot rename {} to {}", temp.display(), path.display()))?;
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_then_reads_the_same_state() {
        let dir = std::env::temp_dir().join(format!("brainmaker-state-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");

        let state = State::new("a1b2c3d4");
        write(&path, &state).unwrap();
        assert_eq!(read(&path), Some(state));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn returns_none_for_a_missing_file() {
        assert_eq!(read(Path::new("/nonexistent/state.json")), None);
    }

    #[test]
    fn returns_none_for_a_corrupt_file() {
        let dir = std::env::temp_dir().join(format!(
            "brainmaker-corrupt-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        fs::write(&path, "{ not json").unwrap();

        assert_eq!(read(&path), None);

        fs::remove_dir_all(&dir).unwrap();
    }
}
