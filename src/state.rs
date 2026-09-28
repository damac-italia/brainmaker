// SPDX-License-Identifier: GPL-3.0-or-later

//! The local record of which content version is installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct State {
    /// Hash of the archive that produced the current `content/` directory.
    pub hash: String,
    /// Time of the last successful install, in seconds since the Unix epoch.
    pub updated_at_unix: u64,
    /// Sequence of the installed release, when the release carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
}

impl State {
    pub fn new(hash: impl Into<String>) -> Self {
        Self {
            hash: hash.into(),
            updated_at_unix: now_unix(),
            sequence: None,
        }
    }

    /// The state for a release that carries a sequence.
    pub fn with_sequence(hash: impl Into<String>, sequence: Option<u64>) -> Self {
        Self {
            sequence,
            ..Self::new(hash)
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

/// The temporary file that [`write`] renames over `path`.
pub fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

/// Writes the state file through a temporary file and a rename, so that a
/// crash never leaves a half-written state file.
pub fn write(path: &Path, state: &State) -> Result<()> {
    let parent = path
        .parent()
        .context("the state file path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create the directory {}", parent.display()))?;

    let temp = temporary_path(path);
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

    #[test]
    fn reads_a_state_file_that_an_older_version_wrote() {
        let dir = crate::testutil::temp_dir("state-older");
        let path = dir.join("state.json");
        fs::write(&path, r#"{"hash": "a1b2c3d4", "updated_at_unix": 1}"#).unwrap();

        let state = read(&path).expect("a state file with no sequence still parses");

        assert_eq!(state.hash, "a1b2c3d4");
        assert_eq!(state.sequence, None);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_and_reads_the_sequence() {
        let dir = crate::testutil::temp_dir("state-sequence");
        let path = dir.join("state.json");

        let state = State::with_sequence("a1b2c3d4", Some(1760000000));
        write(&path, &state).unwrap();

        let read_back = read(&path).expect("the state file parses");
        assert_eq!(read_back.sequence, Some(1760000000));
        assert_eq!(read_back, state);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_no_sequence_key_when_there_is_none() {
        let dir = crate::testutil::temp_dir("state-no-sequence");
        let path = dir.join("state.json");

        write(&path, &State::new("a1b2c3d4")).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sequence"), "got {text}");

        fs::remove_dir_all(&dir).unwrap();
    }
}
