// SPDX-License-Identifier: GPL-3.0-or-later

//! The update flow: compare versions, download, extract, and swap.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::archive;
use crate::config::{Config, validate_hash};
use crate::lock;
use crate::remote;
use crate::state::{self, State};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The local content already matches the remote hash.
    UpToDate { hash: String },
    /// The server could not be reached, and the installed content stays in
    /// place. `hash` is the installed one.
    Unreachable { hash: String, error: String },
    /// Another run holds the install lock, and the installed content stays in
    /// place. `hash` is the installed one.
    Busy { hash: String },
    /// The local content now matches the remote hash.
    Updated {
        previous: Option<String>,
        hash: String,
        stats: archive::Stats,
    },
}

/// How long a run waits for another run's install to end.
///
/// The hook allows `sync` 60 seconds in all, and the check of the latest
/// version can take 20 of them.
#[cfg(not(test))]
const LOCK_WAIT: Duration = Duration::from_secs(30);

/// The same wait in a test build, short enough for a test that holds the
/// lock on purpose.
#[cfg(test)]
const LOCK_WAIT: Duration = Duration::from_millis(200);

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
///
/// A release with another hash installs only when its sequence is higher than
/// the installed one, so a server cannot make a machine install a release that
/// was signed before the one it holds. The run then returns an error, and
/// `force` installs the release anyway.
///
/// Two runs never install at once. The function takes the install lock before
/// it downloads, and waits up to [`LOCK_WAIT`] for another run to release it.
/// A run that still finds the lock held keeps the installed content and
/// reports [`Outcome::Busy`]. With nothing installed, or with `force`, it
/// returns an error instead, as it does for a server that cannot be reached.
pub fn sync(config: &Config, force: bool, log: &dyn Fn(&str)) -> Result<Outcome> {
    fs::create_dir_all(config.root())
        .with_context(|| format!("cannot create the directory {}", config.root().display()))?;

    restore_stranded(config);

    let content_dir = config.content_dir();
    let local = state::read(&config.state_file());
    let installed = local.as_ref().map(|s| s.hash.clone());
    let installed_sequence = local.as_ref().and_then(|s| s.sequence);
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

    // A release with another hash must also be a later one. The same hash is
    // the release that is already installed, so it needs no order.
    if !force && installed.as_deref() != Some(remote_hash.as_str()) {
        check_order(installed_sequence, release.sequence)?;
    }

    // One install at a time. `_lock` lives until this function returns.
    let Some(_lock) = lock::acquire(&config.lock_file(), LOCK_WAIT)? else {
        return match installed {
            Some(hash) if content_present && !force => Ok(Outcome::Busy { hash }),
            _ => bail!(
                "another brainmaker run is installing content in {}; \
                 run the command again when it ends",
                config.root().display()
            ),
        };
    };

    // The run that held the lock may have installed this release, or a later
    // one. The first reading of the state is older than the wait, so read it
    // again, and apply the same order to what it holds now.
    let current = state::read(&config.state_file());
    if !force && current.as_ref().is_some_and(|s| s.hash == remote_hash) && content_dir.is_dir() {
        return Ok(Outcome::UpToDate { hash: remote_hash });
    }
    if !force && current.as_ref().map(|s| s.hash.as_str()) != Some(remote_hash.as_str()) {
        check_order(current.and_then(|s| s.sequence), release.sequence)?;
    }

    log(&format!("Downloading content-{remote_hash}.zip"));
    install(config, &release, log).map(|stats| Outcome::Updated {
        previous: installed,
        hash: remote_hash,
        stats,
    })
}

/// Fails for a release that is not newer than the installed one.
///
/// The signature proves who made a release, and not when. Without an
/// order, a server could serve any release that was ever signed, and the
/// client would install it. `installed` and `offered` are the two
/// sequences; the hashes are already known to differ.
fn check_order(installed: Option<u64>, offered: Option<u64>) -> Result<()> {
    match (installed, offered) {
        (None, _) => Ok(()),
        (Some(held), Some(new)) if new > held => Ok(()),
        (Some(held), Some(new)) => bail!(
            "the server offers a content release with the sequence {new}, and the \
             installed one has the sequence {held}. brainmaker installs only a newer \
             release. If this rollback is intended, run sync --force"
        ),
        (Some(held), None) => bail!(
            "the server offers a content release with no sequence, and the installed \
             one has the sequence {held}. Sign the release with a current \
             brainmaker-sign. If this rollback is intended, run sync --force"
        ),
    }
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

    state::write(
        &config.state_file(),
        &State::with_sequence(hash, release.sequence),
    )
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

/// Puts the previous content back when a stopped run left it in `.trash`.
///
/// A run that stops between the two renames of [`swap`] leaves no `content`
/// and the old one in `.trash`. The state file is written after the swap, so
/// it still names that old content.
fn restore_stranded(config: &Config) {
    let content = config.content_dir();
    let trash = config.trash_dir();
    if content.exists() || !trash.is_dir() {
        return;
    }
    // Only when no run is in its own swap right now.
    if let Ok(Some(_lock)) = lock::acquire(&config.lock_file(), Duration::ZERO) {
        let _ = fs::rename(&trash, &content);
    }
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
    use std::time::Instant;

    use super::*;
    use crate::testutil::{self, Route, Server, Signer};

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

    #[test]
    fn restores_the_content_that_a_stopped_run_left_in_the_trash() {
        let dir = temp_dir("restore-stranded");
        let config = Config::for_test(&dir, "https://api.example.test/v1");
        fs::create_dir_all(config.trash_dir()).unwrap();
        fs::write(config.trash_dir().join("old.md"), "old").unwrap();

        restore_stranded(&config);

        assert_eq!(
            fs::read_to_string(config.content_dir().join("old.md")).unwrap(),
            "old"
        );
        assert!(!config.trash_dir().exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn leaves_the_trash_alone_while_content_exists() {
        let dir = temp_dir("restore-content-present");
        let config = Config::for_test(&dir, "https://api.example.test/v1");
        fs::create_dir_all(config.content_dir()).unwrap();
        fs::write(config.content_dir().join("new.md"), "new").unwrap();
        fs::create_dir_all(config.trash_dir()).unwrap();
        fs::write(config.trash_dir().join("old.md"), "old").unwrap();

        restore_stranded(&config);

        assert_eq!(
            fs::read_to_string(config.content_dir().join("new.md")).unwrap(),
            "new"
        );
        assert!(!config.content_dir().join("old.md").exists());
        assert_eq!(
            fs::read_to_string(config.trash_dir().join("old.md")).unwrap(),
            "old"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn leaves_the_trash_alone_while_another_run_holds_the_lock() {
        // A run between the two renames of `swap` has no content either, and
        // it holds the lock. What it left in `.trash` is not stranded.
        let dir = temp_dir("restore-lock-held");
        let config = Config::for_test(&dir, "https://api.example.test/v1");
        fs::create_dir_all(config.trash_dir()).unwrap();
        fs::write(config.trash_dir().join("old.md"), "old").unwrap();
        // The other run, as the operating system sees it: an open handle with
        // the exclusive lock on the lock file.
        let other_run = fs::File::create(config.lock_file()).unwrap();
        other_run.try_lock().unwrap();

        restore_stranded(&config);

        assert!(!config.content_dir().exists());
        assert_eq!(
            fs::read_to_string(config.trash_dir().join("old.md")).unwrap(),
            "old"
        );

        drop(other_run);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn installs_the_first_sequenced_release() {
        // Nothing that is installed carries a sequence, so nothing orders the
        // first release that does.
        check_order(None, Some(1760000000)).unwrap();
    }

    #[test]
    fn installs_a_higher_sequence() {
        check_order(Some(100), Some(101)).unwrap();
    }

    #[test]
    fn refuses_an_equal_sequence() {
        let error = check_order(Some(100), Some(100)).unwrap_err();

        let text = error.to_string();
        assert!(text.contains("installs only a newer release"), "got {text}");
        assert!(text.contains("sync --force"), "got {text}");
    }

    #[test]
    fn refuses_a_lower_sequence() {
        let error = check_order(Some(200), Some(100)).unwrap_err();

        let text = error.to_string();
        assert!(text.contains("with the sequence 100"), "got {text}");
        assert!(text.contains("has the sequence 200"), "got {text}");
        assert!(text.contains("installs only a newer release"), "got {text}");
    }

    #[test]
    fn refuses_no_sequence_after_a_sequence() {
        let error = check_order(Some(100), None).unwrap_err();

        let text = error.to_string();
        assert!(text.contains("with no sequence"), "got {text}");
        assert!(text.contains("has the sequence 100"), "got {text}");
        assert!(text.contains("sync --force"), "got {text}");
    }

    #[test]
    fn installs_an_unsequenced_release_on_an_unsequenced_state() {
        check_order(None, None).unwrap();
    }

    /// Ignores the progress messages.
    fn quiet(_: &str) {}

    /// The SHA-256 of `bytes`, from the function that checks a download.
    fn digest_of(bytes: &[u8]) -> String {
        let dir = testutil::temp_dir("sync-digest");
        let path = dir.join("bytes");
        fs::write(&path, bytes).unwrap();
        let digest = crate::digest::sha256_of(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        digest
    }

    /// The routes of a server that publishes one release.
    ///
    /// The signed release names `described` by its digest and claims `size`
    /// bytes, and the archive route serves `served`. The three differ in the
    /// tests that publish a release that cannot be trusted. The release
    /// carries `sequence` when it is given, and no sequence key otherwise, as
    /// a release from an older signer does.
    fn routes(
        signer: &Signer,
        hash: &str,
        described: &[u8],
        size: u64,
        served: &[u8],
        sequence: Option<u64>,
    ) -> Vec<Route> {
        let mut payload = serde_json::json!({
            "hash": hash,
            "sha256": digest_of(described),
            "size_bytes": size,
        });
        if let Some(sequence) = sequence {
            payload["sequence"] = sequence.into();
        }
        vec![
            Route::get("/content/latest", signer.envelope(&payload.to_string())),
            Route::get(&format!("/content/{hash}.zip"), served),
        ]
    }

    /// The routes of a server that publishes `archive` as it stands.
    fn published(signer: &Signer, hash: &str, archive: &[u8]) -> Vec<Route> {
        routes(signer, hash, archive, archive.len() as u64, archive, None)
    }

    /// The routes of a server that publishes `archive` as it stands, in a
    /// release that carries `sequence`.
    fn sequenced(signer: &Signer, hash: &str, archive: &[u8], sequence: u64) -> Vec<Route> {
        routes(
            signer,
            hash,
            archive,
            archive.len() as u64,
            archive,
            Some(sequence),
        )
    }

    /// How many times `server` was asked for the archive of `hash`.
    fn downloads(server: &Server, hash: &str) -> usize {
        let path = format!("/content/{hash}.zip");
        server
            .requests()
            .iter()
            .filter(|(_, requested, _)| *requested == path)
            .count()
    }

    #[test]
    fn installs_a_signed_release() {
        let signer = Signer::new();
        signer.trust();
        let archive = testutil::zip_of(&[("notes.md", b"# release one")]);
        let server = Server::start(published(&signer, "a1b2c3d4", &archive));
        let dir = testutil::temp_dir("sync-install");
        let config = Config::for_test(&dir, &server.base());

        let outcome = sync(&config, false, &quiet).unwrap();

        match outcome {
            Outcome::Updated {
                previous,
                hash,
                stats,
            } => {
                assert_eq!(previous, None);
                assert_eq!(hash, "a1b2c3d4");
                assert_eq!(stats.files, 1);
            }
            other => panic!("expected an install, got {other:?}"),
        }
        assert_eq!(
            fs::read_to_string(config.content_dir().join("notes.md")).unwrap(),
            "# release one"
        );
        assert_eq!(state::read(&config.state_file()).unwrap().hash, "a1b2c3d4");
        for leftover in [
            config.staging_dir(),
            config.trash_dir(),
            config.download_file(),
        ] {
            assert!(!leftover.exists(), "{} was left behind", leftover.display());
        }

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reports_up_to_date_and_downloads_nothing() {
        let signer = Signer::new();
        signer.trust();
        let archive = testutil::zip_of(&[("notes.md", b"# release one")]);
        let server = Server::start(published(&signer, "a1b2c3d4", &archive));
        let dir = testutil::temp_dir("sync-up-to-date");
        let config = Config::for_test(&dir, &server.base());

        sync(&config, false, &quiet).unwrap();
        let outcome = sync(&config, false, &quiet).unwrap();

        assert_eq!(
            outcome,
            Outcome::UpToDate {
                hash: "a1b2c3d4".to_string()
            }
        );
        // The first run downloaded the archive, and the second one did not.
        assert_eq!(downloads(&server, "a1b2c3d4"), 1);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn force_installs_again() {
        let signer = Signer::new();
        signer.trust();
        let archive = testutil::zip_of(&[("notes.md", b"# release one")]);
        let server = Server::start(published(&signer, "a1b2c3d4", &archive));
        let dir = testutil::temp_dir("sync-force");
        let config = Config::for_test(&dir, &server.base());

        sync(&config, false, &quiet).unwrap();
        let outcome = sync(&config, true, &quiet).unwrap();

        match outcome {
            Outcome::Updated { previous, hash, .. } => {
                assert_eq!(previous, Some("a1b2c3d4".to_string()));
                assert_eq!(hash, "a1b2c3d4");
            }
            other => panic!("expected a second install, got {other:?}"),
        }
        assert_eq!(downloads(&server, "a1b2c3d4"), 2);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_the_content_when_the_server_cannot_be_reached() {
        let signer = Signer::new();
        signer.trust();
        let archive = testutil::zip_of(&[("notes.md", b"# release one")]);
        let server = Server::start(published(&signer, "a1b2c3d4", &archive));
        let dir = testutil::temp_dir("sync-offline");
        let config = Config::for_test(&dir, &server.base());
        sync(&config, false, &quiet).unwrap();

        let offline = Config::for_test(&dir, &testutil::closed_port_base());
        let outcome = sync(&offline, false, &quiet).unwrap();

        match outcome {
            Outcome::Unreachable { hash, error } => {
                assert_eq!(hash, "a1b2c3d4");
                assert!(!error.is_empty(), "the outcome names no cause");
            }
            other => panic!("expected the installed content to stay, got {other:?}"),
        }
        assert!(config.content_dir().join("notes.md").is_file());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fails_when_nothing_is_installed_and_the_server_cannot_be_reached() {
        let dir = testutil::temp_dir("sync-offline-empty");
        let config = Config::for_test(&dir, &testutil::closed_port_base());

        let result = sync(&config, false, &quiet);

        assert!(result.is_err(), "got {result:?}");
        assert!(!config.content_dir().exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fails_with_force_when_the_server_cannot_be_reached() {
        let signer = Signer::new();
        signer.trust();
        let archive = testutil::zip_of(&[("notes.md", b"# release one")]);
        let server = Server::start(published(&signer, "a1b2c3d4", &archive));
        let dir = testutil::temp_dir("sync-offline-force");
        let config = Config::for_test(&dir, &server.base());
        sync(&config, false, &quiet).unwrap();

        let offline = Config::for_test(&dir, &testutil::closed_port_base());
        let result = sync(&offline, true, &quiet);

        assert!(result.is_err(), "got {result:?}");
        assert!(config.content_dir().join("notes.md").is_file());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_an_archive_that_differs_from_the_signed_digest() {
        let signer = Signer::new();
        signer.trust();
        let signed = testutil::zip_of(&[("notes.md", b"signed bytes")]);
        let served = testutil::zip_of(&[("notes.md", b"forged bytes")]);
        assert_eq!(
            signed.len(),
            served.len(),
            "the two archives must differ in content alone"
        );
        let server = Server::start(routes(
            &signer,
            "a1b2c3d4",
            &signed,
            signed.len() as u64,
            &served,
            None,
        ));
        let dir = testutil::temp_dir("sync-forged");
        let config = Config::for_test(&dir, &server.base());

        let error = sync(&config, false, &quiet).unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("does not match"), "got {text}");
        assert!(!config.content_dir().exists());
        assert!(!config.state_file().exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_an_archive_of_another_size() {
        let signer = Signer::new();
        signer.trust();
        let archive = testutil::zip_of(&[("notes.md", b"# release one")]);
        let server = Server::start(routes(
            &signer,
            "a1b2c3d4",
            &archive,
            archive.len() as u64 + 1,
            &archive,
            None,
        ));
        let dir = testutil::temp_dir("sync-size");
        let config = Config::for_test(&dir, &server.base());

        let error = sync(&config, false, &quiet).unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("signed content release says"), "got {text}");
        assert!(!config.content_dir().exists());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_install_keeps_the_previous_content() {
        let signer = Signer::new();
        signer.trust();
        let first = testutil::zip_of(&[("one.md", b"release one")]);
        let second = testutil::zip_of(&[("two.md", b"release two")]);
        let old_server = Server::start(published(&signer, "a1b2c3d4", &first));
        // The second release names the digest of other bytes than it serves.
        let new_server = Server::start(routes(
            &signer,
            "e5f6a7b8",
            b"other bytes",
            second.len() as u64,
            &second,
            None,
        ));
        let dir = testutil::temp_dir("sync-failed-install");
        let config = Config::for_test(&dir, &old_server.base());
        sync(&config, false, &quiet).unwrap();

        let newer = Config::for_test(&dir, &new_server.base());
        let result = sync(&newer, false, &quiet);

        assert!(result.is_err(), "got {result:?}");
        assert_eq!(
            fs::read_to_string(config.content_dir().join("one.md")).unwrap(),
            "release one"
        );
        assert!(!config.content_dir().join("two.md").exists());
        assert_eq!(state::read(&config.state_file()).unwrap().hash, "a1b2c3d4");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn installs_again_when_the_content_directory_is_missing() {
        let signer = Signer::new();
        signer.trust();
        let archive = testutil::zip_of(&[("notes.md", b"# release one")]);
        let server = Server::start(published(&signer, "a1b2c3d4", &archive));
        let dir = testutil::temp_dir("sync-missing-content");
        let config = Config::for_test(&dir, &server.base());
        sync(&config, false, &quiet).unwrap();
        fs::remove_dir_all(config.content_dir()).unwrap();

        let outcome = sync(&config, false, &quiet).unwrap();

        match outcome {
            Outcome::Updated { previous, hash, .. } => {
                assert_eq!(previous, Some("a1b2c3d4".to_string()));
                assert_eq!(hash, "a1b2c3d4");
            }
            other => panic!("expected a new install, got {other:?}"),
        }
        assert!(config.content_dir().join("notes.md").is_file());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_the_content_while_another_run_holds_the_lock() {
        let signer = Signer::new();
        signer.trust();
        let first = testutil::zip_of(&[("one.md", b"release one")]);
        let second = testutil::zip_of(&[("two.md", b"release two")]);
        let old_server = Server::start(published(&signer, "a1b2c3d4", &first));
        let new_server = Server::start(published(&signer, "e5f6a7b8", &second));
        let dir = testutil::temp_dir("sync-busy");
        let config = Config::for_test(&dir, &old_server.base());
        sync(&config, false, &quiet).unwrap();

        // The other run, as the operating system sees it: an exclusive lock on
        // the lock file. The server now publishes the second release.
        let newer = Config::for_test(&dir, &new_server.base());
        // Another test in this binary can spawn a child process that holds a copy of the lock file
        // until it runs `exec`, so the test waits for the lock and does not demand it at once.
        let held = lock::acquire(&config.lock_file(), Duration::from_secs(5))
            .unwrap()
            .expect("nothing else holds the lock");
        let outcome = sync(&newer, false, &quiet).unwrap();

        assert_eq!(
            outcome,
            Outcome::Busy {
                hash: "a1b2c3d4".to_string()
            }
        );
        assert!(config.content_dir().join("one.md").is_file());
        assert_eq!(downloads(&new_server, "e5f6a7b8"), 0);

        // With the lock free again, the same run installs the second release.
        drop(held);
        let outcome = sync(&newer, false, &quiet).unwrap();

        match outcome {
            Outcome::Updated { previous, hash, .. } => {
                assert_eq!(previous, Some("a1b2c3d4".to_string()));
                assert_eq!(hash, "e5f6a7b8");
            }
            other => panic!("expected the second release, got {other:?}"),
        }
        assert!(config.content_dir().join("two.md").is_file());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fails_while_another_run_holds_the_lock_and_nothing_is_installed() {
        let signer = Signer::new();
        signer.trust();
        let archive = testutil::zip_of(&[("notes.md", b"# release one")]);
        let server = Server::start(published(&signer, "a1b2c3d4", &archive));
        let dir = testutil::temp_dir("sync-busy-empty");
        let config = Config::for_test(&dir, &server.base());
        let held = lock::acquire(&config.lock_file(), Duration::ZERO)
            .unwrap()
            .expect("nothing else holds the lock");

        let error = sync(&config, false, &quiet).unwrap_err();

        let text = format!("{error:#}");
        assert!(
            text.contains("another brainmaker run is installing content"),
            "got {text}"
        );
        assert!(!config.content_dir().exists());
        assert_eq!(downloads(&server, "a1b2c3d4"), 0);

        drop(held);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn records_the_sequence_it_installed() {
        let signer = Signer::new();
        signer.trust();
        let archive = testutil::zip_of(&[("notes.md", b"# release one")]);
        let server = Server::start(sequenced(&signer, "a1b2c3d4", &archive, 5));
        let dir = testutil::temp_dir("sync-records-sequence");
        let config = Config::for_test(&dir, &server.base());

        sync(&config, false, &quiet).unwrap();

        let kept = state::read(&config.state_file()).unwrap();
        assert_eq!(kept.hash, "a1b2c3d4");
        assert_eq!(kept.sequence, Some(5));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_a_replayed_release() {
        let signer = Signer::new();
        signer.trust();
        // The signer made release A first, with the sequence 100, and release
        // B later, with the sequence 200. A machine that holds B is offered A.
        let release_a = testutil::zip_of(&[("a.md", b"release a")]);
        let release_b = testutil::zip_of(&[("b.md", b"release b")]);
        let server_b = Server::start(sequenced(&signer, "e5f6a7b8", &release_b, 200));
        let server_a = Server::start(sequenced(&signer, "a1b2c3d4", &release_a, 100));
        let dir = testutil::temp_dir("sync-replay");
        let config = Config::for_test(&dir, &server_b.base());
        sync(&config, false, &quiet).unwrap();

        let replayed = Config::for_test(&dir, &server_a.base());
        let error = sync(&replayed, false, &quiet).unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("installs only a newer release"), "got {text}");
        assert!(config.content_dir().join("b.md").is_file());
        assert!(!config.content_dir().join("a.md").exists());
        let kept = state::read(&config.state_file()).unwrap();
        assert_eq!(kept.hash, "e5f6a7b8");
        assert_eq!(kept.sequence, Some(200));
        assert_eq!(downloads(&server_a, "a1b2c3d4"), 0);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn force_installs_an_older_release() {
        let signer = Signer::new();
        signer.trust();
        let release_a = testutil::zip_of(&[("a.md", b"release a")]);
        let release_b = testutil::zip_of(&[("b.md", b"release b")]);
        let server_b = Server::start(sequenced(&signer, "e5f6a7b8", &release_b, 200));
        let server_a = Server::start(sequenced(&signer, "a1b2c3d4", &release_a, 100));
        let dir = testutil::temp_dir("sync-replay-force");
        let config = Config::for_test(&dir, &server_b.base());
        sync(&config, false, &quiet).unwrap();

        let older = Config::for_test(&dir, &server_a.base());
        let outcome = sync(&older, true, &quiet).unwrap();

        match outcome {
            Outcome::Updated { previous, hash, .. } => {
                assert_eq!(previous, Some("e5f6a7b8".to_string()));
                assert_eq!(hash, "a1b2c3d4");
            }
            other => panic!("expected the older release to install, got {other:?}"),
        }
        assert!(config.content_dir().join("a.md").is_file());
        assert!(!config.content_dir().join("b.md").exists());
        let kept = state::read(&config.state_file()).unwrap();
        assert_eq!(kept.hash, "a1b2c3d4");
        assert_eq!(kept.sequence, Some(100));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_same_hash_is_up_to_date_whatever_its_sequence() {
        let signer = Signer::new();
        signer.trust();
        let archive = testutil::zip_of(&[("notes.md", b"# release one")]);
        // The signer signed one archive twice, and the server now serves the
        // earlier signature.
        let later = Server::start(sequenced(&signer, "a1b2c3d4", &archive, 200));
        let earlier = Server::start(sequenced(&signer, "a1b2c3d4", &archive, 100));
        let dir = testutil::temp_dir("sync-same-hash");
        let config = Config::for_test(&dir, &later.base());
        sync(&config, false, &quiet).unwrap();

        let replayed = Config::for_test(&dir, &earlier.base());
        let outcome = sync(&replayed, false, &quiet).unwrap();

        assert_eq!(
            outcome,
            Outcome::UpToDate {
                hash: "a1b2c3d4".to_string()
            }
        );
        assert_eq!(downloads(&earlier, "a1b2c3d4"), 0);
        assert_eq!(
            state::read(&config.state_file()).unwrap().sequence,
            Some(200)
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn installs_again_when_the_content_is_missing_whatever_the_sequence() {
        let signer = Signer::new();
        signer.trust();
        let archive = testutil::zip_of(&[("notes.md", b"# release one")]);
        let server = Server::start(sequenced(&signer, "a1b2c3d4", &archive, 200));
        let dir = testutil::temp_dir("sync-missing-sequenced");
        let config = Config::for_test(&dir, &server.base());
        sync(&config, false, &quiet).unwrap();
        fs::remove_dir_all(config.content_dir()).unwrap();

        // The server offers the release that is installed, with the sequence
        // that is installed, which is not a higher one.
        let outcome = sync(&config, false, &quiet).unwrap();

        match outcome {
            Outcome::Updated { previous, hash, .. } => {
                assert_eq!(previous, Some("a1b2c3d4".to_string()));
                assert_eq!(hash, "a1b2c3d4");
            }
            other => panic!("expected the content to be installed again, got {other:?}"),
        }
        assert!(config.content_dir().join("notes.md").is_file());
        assert_eq!(
            state::read(&config.state_file()).unwrap().sequence,
            Some(200)
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checks_the_order_again_after_the_lock_is_taken() {
        let signer = Signer::new();
        signer.trust();
        let first = testutil::zip_of(&[("one.md", b"release one")]);
        let second = testutil::zip_of(&[("two.md", b"release two")]);
        let old_server = Server::start(sequenced(&signer, "a1b2c3d4", &first, 100));
        let new_server = Server::start(sequenced(&signer, "e5f6a7b8", &second, 200));
        let dir = testutil::temp_dir("sync-order-after-lock");
        let config = Config::for_test(&dir, &old_server.base());
        sync(&config, false, &quiet).unwrap();

        // Another run holds the lock while this run waits with the second
        // release in hand. That run installs a third release, with the
        // sequence 300, and then lets go of the lock.
        let newer = Config::for_test(&dir, &new_server.base());
        let held = lock::acquire(&config.lock_file(), Duration::from_secs(5))
            .unwrap()
            .expect("nothing else holds the lock");
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                // The waiting run has read its state before it asks for the
                // release, so the state can change once the release is asked for.
                let deadline = Instant::now() + Duration::from_secs(5);
                while !new_server
                    .requests()
                    .iter()
                    .any(|(_, path, _)| path == "/content/latest")
                {
                    assert!(
                        Instant::now() < deadline,
                        "the waiting run never asked for the release"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                let third = State::with_sequence("c9d0e1f2", Some(300));
                state::write(&config.state_file(), &third).unwrap();
                drop(held);
            });
            sync(&newer, false, &quiet)
        });

        let error = result.unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("installs only a newer release"), "got {text}");
        assert!(text.contains("has the sequence 300"), "got {text}");
        assert!(config.content_dir().join("one.md").is_file());
        assert!(!config.content_dir().join("two.md").exists());
        assert_eq!(downloads(&new_server, "e5f6a7b8"), 0);
        let kept = state::read(&config.state_file()).unwrap();
        assert_eq!(kept.hash, "c9d0e1f2");
        assert_eq!(kept.sequence, Some(300));

        fs::remove_dir_all(&dir).unwrap();
    }
}
