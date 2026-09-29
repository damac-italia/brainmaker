// SPDX-License-Identifier: GPL-3.0-or-later

//! The lock that keeps two runs from working in one root at the same time.
//!
//! # Why this exists
//!
//! The `SessionStart` hook and the hourly agent both run `sync`, and nothing
//! orders them. Both use the same temporary paths under the root, and each one
//! clears those paths before it starts. Without a lock, one run deletes the
//! directory that the other run is filling.
//!
//! The lock is an exclusive lock on a file under the root. The operating
//! system drops it when the handle closes, so a run that is killed leaves no
//! stale lock behind.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// How often a waiting run asks for the lock again.
const POLL: Duration = Duration::from_millis(200);

/// A held lock. Dropping the value releases it.
#[derive(Debug)]
pub struct RunLock {
    _file: File,
}

/// Takes the lock at `path`, and waits up to `wait` for another run to
/// release it.
///
/// Returns `None` when another run still holds the lock after `wait`.
pub fn acquire(path: &Path, wait: Duration) -> Result<Option<RunLock>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create the directory {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("cannot open the lock file {}", path.display()))?;

    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(RunLock { _file: file })),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(POLL);
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("cannot lock {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brainmaker-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_second_holder_waits_and_then_gives_up() {
        let dir = temp_dir("lock-held");
        let path = dir.join(".lock");

        let first = acquire(&path, Duration::ZERO).unwrap();
        assert!(first.is_some(), "the first holder takes a free lock");

        // With no wait, the second holder gives up at once.
        let second = acquire(&path, Duration::ZERO).unwrap();
        assert!(second.is_none(), "the lock is held, so nobody else gets it");

        // With a wait, it gives up only after the wait has passed.
        let wait = Duration::from_millis(250);
        let started = Instant::now();
        let third = acquire(&path, wait).unwrap();
        assert!(third.is_none(), "the lock is still held after the wait");
        assert!(
            started.elapsed() >= wait,
            "the run gave up after {:?}, before the {wait:?} wait ended",
            started.elapsed()
        );

        drop(first);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_lock_is_free_again_after_the_holder_drops_it() {
        let dir = temp_dir("lock-dropped");
        let path = dir.join(".lock");

        let first = acquire(&path, Duration::ZERO).unwrap();
        assert!(first.is_some(), "the first holder takes a free lock");
        drop(first);

        let second = acquire(&path, Duration::ZERO).unwrap();
        assert!(second.is_some(), "the lock is free once its holder is gone");

        drop(second);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creates_the_directory_that_holds_the_lock() {
        let dir = temp_dir("lock-directory");
        let path = dir.join("absent").join(".lock");

        let held = acquire(&path, Duration::ZERO).unwrap();

        assert!(held.is_some());
        assert!(path.is_file(), "the lock file exists at {}", path.display());

        drop(held);
        fs::remove_dir_all(&dir).unwrap();
    }
}
