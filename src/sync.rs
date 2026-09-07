// SPDX-License-Identifier: GPL-3.0-or-later

//! The update flow: compare versions, download, extract, and swap.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::archive;
use crate::config::{Config, validate_hash};
use crate::remote;
use crate::state::{self, State};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The local content already matches the remote hash.
    UpToDate { hash: String },
    /// The server could not be reached, and the installed content stays in
    /// place. `hash` is the installed one.
    Unreachable { hash: String, error: String },
    /// The local content now matches the remote hash.
    Updated {
        previous: Option<String>,
        hash: String,
        stats: archive::Stats,
    },
}

/// Brings `content/` to the latest remote version.
///
/// The function downloads and extracts only when the local hash differs from
/// the remote hash, when `content/` is missing, or when `force` is true.
///
/// A server that cannot be reached is not an error while content is
/// installed. The hook runs this at every session start, and a laptop is
/// often offline; the installed content then stays in place and the run
/// reports [`Outcome::Unreachable`]. With nothing installed, or with
/// `force`, the failure is returned, because there is nothing to fall back on.
pub fn sync(config: &Config, force: bool, log: &dyn Fn(&str)) -> Result<Outcome> {
    fs::create_dir_all(config.root())
        .with_context(|| format!("cannot create the directory {}", config.root().display()))?;

    let content_dir = config.content_dir();
    let local = state::read(&config.state_file());
    let installed = local.as_ref().map(|s| s.hash.clone());
    let content_present = content_dir.is_dir();

    log("Checking the latest content version.");
    let release = match remote::latest_release(config) {
        Ok(release) => release,
        Err(error) => {
            return match installed {
                Some(hash) if content_present && !force => Ok(Outcome::Unreachable {
                    hash,
                    error: format!("{error:#}"),
                }),
                _ => Err(error),
            };
        }
    };
    let remote_hash = release.hash.clone();

    if !force && installed.as_deref() == Some(remote_hash.as_str()) && content_present {
        return Ok(Outcome::UpToDate { hash: remote_hash });
    }

    if !force && installed.as_deref() == Some(remote_hash.as_str()) && !content_present {
        log("The state file matches the remote version, but the content directory is missing.");
    }

    log(&format!("Downloading content-{remote_hash}.zip"));
    install(config, &release, log).map(|stats| Outcome::Updated {
        previous: installed,
        hash: remote_hash,
        stats,
    })
}

/// Downloads one release and replaces `content/` with it.
fn install(
    config: &Config,
    release: &remote::ContentRelease,
    log: &dyn Fn(&str),
) -> Result<archive::Stats> {
    let hash = release.hash.as_str();
    validate_hash(hash)?;

    let download = config.download_file();
    let staging = config.staging_dir();
    let trash = config.trash_dir();
    let content = config.content_dir();

    // A previous run may have failed between steps. Clear its leftovers.
    remove_dir_if_present(&staging)?;
    remove_dir_if_present(&trash)?;
    remove_file_if_present(&download)?;

    let result = (|| -> Result<archive::Stats> {
        let bytes = remote::download_archive(config, hash, &download)?;
        log(&format!("Downloaded {bytes} bytes."));

        // Check the archive against the signed release before the extractor
        // reads it. The signature covers this digest, so a server that serves
        // other bytes than it signed stops here, with nothing written.
        if bytes != release.size_bytes {
            bail!(
                "the archive is {bytes} bytes, and the signed content release says {}",
                release.size_bytes
            );
        }
        let actual = crate::digest::sha256_of(&download)?;
        crate::digest::check_matches(&actual, &release.sha256, "the signed content release")?;
        log("The SHA-256 matches the signed content release.");

        let stats = archive::extract(&download, &staging)?;
        log(&format!(
            "Extracted {} files and {} directories.",
            stats.files, stats.directories
        ));
        if stats.skipped > 0 {
            log(&format!(
                "Skipped {} unsafe archive entries.",
                stats.skipped
            ));
        }

        swap(&content, &staging, &trash)?;
        Ok(stats)
    })();

    // Always drop the temporary files, on success and on failure alike.
    let _ = remove_file_if_present(&download);
    let _ = remove_dir_if_present(&staging);
    let _ = remove_dir_if_present(&trash);

    let stats = result?;

    state::write(&config.state_file(), &State::new(hash))
        .context("the content was installed, but the state file could not be written")?;

    Ok(stats)
}

/// Replaces `content` with `staging`.
///
/// The function moves the old directory to `trash` first, so that the window in
/// which `content` does not exist is one rename long. If the second rename
/// fails, the function restores the old directory.
fn swap(content: &Path, staging: &Path, trash: &Path) -> Result<()> {
    let had_content = content.exists();

    if had_content {
        fs::rename(content, trash)
            .with_context(|| format!("cannot move {} to {}", content.display(), trash.display()))?;
    }

    if let Err(error) = fs::rename(staging, content) {
        if had_content {
            // Put the old content back, so that the failure leaves the
            // previous version in place.
            let _ = fs::rename(trash, content);
        }
        return Err(error).with_context(|| {
            format!("cannot move {} to {}", staging.display(), content.display())
        });
    }

    Ok(())
}

fn remove_dir_if_present(path: &Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("cannot remove the directory {}", path.display()))
        }
    }
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("cannot remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
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
    fn swap_replaces_the_old_content() {
        let dir = temp_dir("swap-replace");
        let content = dir.join("content");
        let staging = dir.join(".staging");
        let trash = dir.join(".trash");

        fs::create_dir_all(&content).unwrap();
        fs::write(content.join("old.md"), "old").unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("new.md"), "new").unwrap();

        swap(&content, &staging, &trash).unwrap();

        assert!(content.join("new.md").exists());
        assert!(!content.join("old.md").exists());
        assert!(!staging.exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn swap_creates_the_content_directory_when_it_is_absent() {
        let dir = temp_dir("swap-create");
        let content = dir.join("content");
        let staging = dir.join(".staging");
        let trash = dir.join(".trash");

        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("new.md"), "new").unwrap();

        swap(&content, &staging, &trash).unwrap();

        assert!(content.join("new.md").exists());
        assert!(!trash.exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn swap_restores_the_old_content_when_the_staging_directory_is_absent() {
        let dir = temp_dir("swap-restore");
        let content = dir.join("content");
        let staging = dir.join(".staging");
        let trash = dir.join(".trash");

        fs::create_dir_all(&content).unwrap();
        fs::write(content.join("old.md"), "old").unwrap();

        let error = swap(&content, &staging, &trash);

        assert!(error.is_err());
        assert_eq!(fs::read_to_string(content.join("old.md")).unwrap(), "old");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_helpers_accept_a_missing_path() {
        let dir = temp_dir("remove-missing");
        remove_dir_if_present(&dir.join("absent")).unwrap();
        remove_file_if_present(&dir.join("absent.zip")).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
