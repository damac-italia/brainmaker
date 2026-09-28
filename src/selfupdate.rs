// SPDX-License-Identifier: GPL-3.0-or-later

//! Replacement of the running binary from the software API.
//!
//! # Trust model
//!
//! An Ed25519 signature over the manifest is the trust anchor. The holder of
//! the signing key decides what this binary installs. The API host, the CDN in
//! front of it, and the storage bucket behind it are not trusted: a manifest
//! they alter fails its signature check before anything parses it. See
//! [`crate::signature`].
//!
//! The SHA-256 inside the signed manifest then carries that trust to the
//! binary, because an attacker who cannot forge the manifest cannot choose the
//! checksum either.
//!
//! Two further rules narrow the exposure. The manifest carries no URL at all:
//! [`crate::config::Config::binary_url`] derives the download address from the
//! base URL in the provisioning file, so a manifest cannot move the download to
//! another host, and a published manifest discloses no endpoint. And
//! [`version::is_newer`] installs only a strictly newer version, so a replayed
//! older manifest installs nothing.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::{Config, MAX_BINARY_BYTES, MAX_MANIFEST_BYTES};
use crate::remote;
use crate::signature;
use crate::version;

/// Version of this binary.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a run waits for another run to finish replacing the program.
///
/// The download of a binary is the long step, so this matches the wait of the
/// content install.
const UPDATE_LOCK_WAIT: Duration = Duration::from_secs(30);

/// One platform's build in the manifest.
///
/// The struct carries no URL. The client derives the download address from its
/// own base URL, so a manifest that a release publishes names no host.
#[derive(Debug, Clone, Deserialize)]
pub struct Build {
    pub sha256: String,
}

/// Body of `GET {base}/software/brainmaker`.
///
/// The manifest travels as a string, and the signature covers exactly those
/// bytes. The client checks the bytes before it parses them, so key order,
/// whitespace, and escaping in the manifest change nothing: no JSON
/// canonicalisation rule takes part in the security argument.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    /// The manifest, as the signer wrote it.
    pub payload: String,
    /// The Ed25519 signature over `payload`, as 128 hexadecimal characters.
    pub signature: String,
}

/// The signed payload inside an [`Envelope`].
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub platforms: BTreeMap<String, Build>,
}

/// Result of a version check.
#[derive(Debug, Clone)]
pub enum Check {
    /// The running binary is the newest one the server offers. `build` carries
    /// the published build for this platform when the manifest has one, so
    /// that `--force` can reinstall the same version.
    UpToDate {
        version: String,
        platform: String,
        build: Option<Build>,
    },
    /// The server offers a newer binary for this platform.
    Newer {
        latest: String,
        platform: String,
        build: Build,
    },
    /// The server offers a newer version, but not for this platform.
    NewerElsewhere {
        latest: String,
        platform: String,
        offered: Vec<String>,
    },
}

/// Returns the manifest key for the platform this binary runs on.
///
/// The key is `<os>-<arch>`, for example `darwin-arm64`.
pub fn platform_key() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os}-{arch}")
}

/// Reads the manifest, checks its signature, and compares its version against
/// [`CURRENT_VERSION`].
///
/// The signature check runs before the manifest is parsed, so an untrusted
/// manifest never reaches the version logic or the platform lookup.
pub fn check(config: &Config) -> Result<Check> {
    let url = config.software_url();
    let body = remote::fetch_text(config, &url, MAX_MANIFEST_BYTES)?;

    let envelope: Envelope = serde_json::from_str(&body).with_context(|| {
        format!(
            "{url} did not return the expected JSON object \
             {{\"payload\": \"...\", \"signature\": \"...\"}}"
        )
    })?;

    signature::verify(envelope.payload.as_bytes(), &envelope.signature)
        .with_context(|| format!("cannot trust the software manifest from {url}"))?;

    let manifest: Manifest = serde_json::from_str(&envelope.payload).with_context(|| {
        format!(
            "{url} carries a signed payload that is not the expected JSON object \
             {{\"version\": \"...\", \"platforms\": {{...}}}}"
        )
    })?;

    if !version::validate(&manifest.version) {
        bail!(
            "{url} returned the invalid version string {:?}",
            manifest.version
        );
    }

    let platform = platform_key();

    if !version::is_newer(&manifest.version, CURRENT_VERSION) {
        return Ok(Check::UpToDate {
            build: manifest.platforms.get(&platform).cloned(),
            version: manifest.version,
            platform,
        });
    }

    match manifest.platforms.get(&platform) {
        Some(build) => Ok(Check::Newer {
            latest: manifest.version,
            platform,
            build: build.clone(),
        }),
        None => Ok(Check::NewerElsewhere {
            latest: manifest.version,
            platform,
            offered: manifest.platforms.keys().cloned().collect(),
        }),
    }
}

impl Build {
    /// Rejects a checksum that is not 64 hexadecimal characters.
    fn checksum(&self) -> Result<String> {
        crate::digest::checked_sha256(&self.sha256, "the manifest")
    }
}

/// Downloads `build` and puts it in place of the running binary.
///
/// The function verifies the SHA-256 and runs the new binary with `--version`
/// before it swaps. Returns the path it replaced.
///
/// The function holds the update lock until it returns, so two runs never
/// replace the program at once. It waits up to `UPDATE_LOCK_WAIT` for that
/// lock, and returns an error when another run still holds it.
///
/// When `link` has installed a copy under the root and the running binary is
/// another file, that copy is replaced too. The hook runs the copy, and an
/// update that reached only the file the user happened to run would leave
/// every session on the old version.
pub fn apply(config: &Config, latest: &str, build: &Build, log: &dyn Fn(&str)) -> Result<PathBuf> {
    let expected_sum = build.checksum()?;

    // One update at a time. Two runs would each remove the other's backup.
    let Some(_lock) = crate::lock::acquire(&config.update_lock_file(), UPDATE_LOCK_WAIT)? else {
        bail!(
            "another brainmaker run is replacing the program under {}; \
             run the command again when it ends",
            config.root().display()
        );
    };

    let url = config.binary_url(latest, &platform_key());

    let exe = current_exe()?;
    let directory = exe
        .parent()
        .context("the executable path has no parent directory")?;

    // The staged file must sit on the same filesystem as the executable,
    // because the swap is a rename. It also needs the platform's executable
    // suffix, because `verify_runs` runs it, and Windows runs only a file
    // named `.exe`. EXE_SUFFIX is empty on Unix.
    let suffix = std::env::consts::EXE_SUFFIX;
    let staged = directory.join(format!(".brainmaker-update-{}{suffix}", std::process::id()));
    let backup = directory.join(format!(".brainmaker-old{suffix}"));

    check_writable(directory, &exe)?;

    let result = (|| -> Result<()> {
        log(&format!("Downloading {url}"));
        let bytes = remote::download(config, &url, &staged, MAX_BINARY_BYTES)?;
        log(&format!("Downloaded {bytes} bytes."));

        let actual_sum = crate::digest::sha256_of(&staged)?;
        crate::digest::check_matches(&actual_sum, &expected_sum, "the manifest")?;
        log("The SHA-256 matches the manifest.");

        set_executable(&staged)?;
        verify_runs(&staged, latest)?;
        log(&format!("The new binary reports version {latest}."));

        swap(&exe, &staged, &backup)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    let _ = fs::remove_file(&backup);
    result?;

    let installed = crate::link::installed_program(config.root());
    if installed.is_file() && !crate::link::same_file(&exe, &installed) {
        crate::link::copy_program(&exe, &installed)?;
        log(&format!(
            "Replaced {} too, which the SessionStart hook runs.",
            installed.display()
        ));
    }

    Ok(exe)
}

/// The command that installs the update, as the reader can type it.
///
/// A bare `brainmaker self-update` fails after the archive is deleted: nothing
/// puts the binary on `PATH`. So the hint names the running binary's own path.
pub fn self_update_command() -> String {
    match std::env::current_exe() {
        Ok(exe) => format!("{} self-update", exe.display()),
        Err(_) => "brainmaker self-update".to_string(),
    }
}

/// Puts `staged` at `exe`.
///
/// The function moves the current binary to `backup` first. A running process
/// keeps its open image, so this is safe while brainmaker itself runs. If
/// the second rename fails, the function restores the old binary.
fn swap(exe: &Path, staged: &Path, backup: &Path) -> Result<()> {
    let _ = fs::remove_file(backup);

    fs::rename(exe, backup)
        .with_context(|| format!("cannot move {} to {}", exe.display(), backup.display()))?;

    if let Err(error) = fs::rename(staged, exe) {
        let _ = fs::rename(backup, exe);
        return Err(error)
            .with_context(|| format!("cannot move {} to {}", staged.display(), exe.display()));
    }

    Ok(())
}

/// Resolves the path of the running binary, and follows symbolic links.
fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate the running executable")?;
    // Follow a symbolic link, so that the swap replaces the real file rather
    // than the link.
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

/// Fails early when we cannot write the executable or its directory.
fn check_writable(directory: &Path, exe: &Path) -> Result<()> {
    let probe = directory.join(format!(".brainmaker-probe-{}", std::process::id()));
    fs::write(&probe, b"").map_err(|error| {
        anyhow::anyhow!(
            "cannot write in {}: {error}. Reinstall brainmaker in a directory you own, \
             or run the update with an account that can write {}",
            directory.display(),
            exe.display()
        )
    })?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("cannot make {} executable", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Runs the staged binary with `--version` and checks what it reports.
///
/// This catches a build for the wrong architecture, a truncated file, and a
/// manifest whose version does not match the binary it points at. The whole
/// line is compared, because `0.1.1` is a part of `0.1.10`. The staged file
/// already passed the checksum, and it is the file we are about to make the
/// user's binary, so running it adds no new trust.
fn verify_runs(staged: &Path, expected_version: &str) -> Result<()> {
    let output = std::process::Command::new(staged)
        .arg("--version")
        .output()
        .with_context(|| {
            format!(
                "cannot run {} --version; the download may be built for another platform",
                staged.display()
            )
        })?;

    if !output.status.success() {
        bail!(
            "{} --version exited with {}",
            staged.display(),
            output.status
        );
    }

    let reported = String::from_utf8_lossy(&output.stdout);
    let reported = reported.trim();
    if !reports_version(reported, expected_version) {
        bail!(
            "the manifest says version {expected_version}, but the download reports {reported:?}"
        );
    }

    Ok(())
}

/// True when `output` is the line that `brainmaker --version` prints for
/// `expected_version`.
///
/// The line is `brainmaker <version>`, and nothing else may differ. White
/// space around it, such as the `\r` of a Windows line end, does not count.
fn reports_version(output: &str, expected_version: &str) -> bool {
    output.trim() == format!("brainmaker {expected_version}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{self, Route, Server, Signer};

    fn build() -> Build {
        Build {
            sha256: "0".repeat(64),
        }
    }

    #[test]
    fn builds_a_platform_key() {
        let key = platform_key();
        assert!(key.contains('-'), "got {key}");
        assert!(!key.contains("macos"), "got {key}");
        assert!(!key.contains("aarch64"), "got {key}");
    }

    #[test]
    fn the_manifest_type_carries_no_url() {
        // The download address is derived from the base URL, so a published
        // manifest names no host. This test fails if a `url` field returns.
        let source = include_str!("selfupdate.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("selfupdate.rs has a non-test section");
        let build_struct = production
            .split("pub struct Build {")
            .nth(1)
            .and_then(|rest| rest.split('}').next())
            .expect("selfupdate.rs declares Build");
        assert!(
            !build_struct.contains("url"),
            "Build must carry no URL, got {build_struct:?}"
        );
    }

    #[test]
    fn accepts_only_a_well_formed_checksum() {
        let mut b = build();
        b.sha256 = "A".repeat(64);
        assert_eq!(b.checksum().unwrap(), "a".repeat(64));

        b.sha256 = "abc".to_string();
        assert!(b.checksum().is_err());

        b.sha256 = "z".repeat(64);
        assert!(b.checksum().is_err());

        b.sha256 = String::new();
        assert!(b.checksum().is_err());
    }

    #[test]
    fn parses_a_manifest() {
        let text = r#"{
            "version": "0.2.0",
            "platforms": {
                "darwin-arm64": {
                    "sha256": "9f2c0000000000000000000000000000000000000000000000000000000000ab"
                }
            }
        }"#;
        let manifest: Manifest = serde_json::from_str(text).unwrap();
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.platforms.len(), 1);
        assert!(manifest.platforms.contains_key("darwin-arm64"));
    }

    #[test]
    fn parses_an_envelope_and_the_manifest_inside_it() {
        // The payload is a string, so the signature covers the bytes the server
        // served and no canonicalisation rule takes part.
        let text = r#"{
            "payload": "{\"version\":\"0.2.0\",\"platforms\":{}}",
            "signature": "ab12"
        }"#;
        let envelope: Envelope = serde_json::from_str(text).unwrap();
        assert_eq!(envelope.signature, "ab12");

        let manifest: Manifest = serde_json::from_str(&envelope.payload).unwrap();
        assert_eq!(manifest.version, "0.2.0");
    }

    #[test]
    fn swap_replaces_the_binary_and_restores_it_on_failure() {
        let dir = std::env::temp_dir().join(format!(
            "brainmaker-swap-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let exe = dir.join("brainmaker");
        let staged = dir.join("staged");
        let backup = dir.join("backup");

        fs::write(&exe, b"old").unwrap();
        fs::write(&staged, b"new").unwrap();
        swap(&exe, &staged, &backup).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");

        // A missing staged file must leave the binary in place.
        let missing = dir.join("absent");
        assert!(swap(&exe, &missing, &backup).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"new");

        fs::remove_dir_all(&dir).unwrap();
    }

    /// A software manifest for `version`, with one build for each of `platforms`.
    fn manifest(version: &str, platforms: &[&str]) -> String {
        let builds: serde_json::Map<String, serde_json::Value> = platforms
            .iter()
            .map(|platform| {
                (
                    platform.to_string(),
                    serde_json::json!({ "sha256": "c".repeat(64) }),
                )
            })
            .collect();
        serde_json::json!({ "version": version, "platforms": builds }).to_string()
    }

    /// Runs `check` against a server that publishes `payload`, signed by `signer`.
    fn check_against(signer: &Signer, payload: &str) -> Result<Check> {
        let server = Server::start(vec![Route::get(
            "/software/brainmaker",
            signer.envelope(payload),
        )]);
        let dir = testutil::temp_dir("update-check");
        let result = check(&Config::for_test(&dir, &server.base()));
        fs::remove_dir_all(&dir).unwrap();
        result
    }

    #[test]
    fn offers_a_newer_version_for_this_platform() {
        let signer = Signer::new();
        signer.trust();
        let platform = platform_key();
        let payload = manifest("999.0.0", &[platform.as_str()]);

        match check_against(&signer, &payload).unwrap() {
            Check::Newer {
                latest,
                platform: offered_for,
                build,
            } => {
                assert_eq!(latest, "999.0.0");
                assert_eq!(offered_for, platform);
                assert_eq!(build.sha256, "c".repeat(64));
            }
            other => panic!("expected a newer version, got {other:?}"),
        }
    }

    #[test]
    fn offers_nothing_for_the_running_version() {
        let signer = Signer::new();
        signer.trust();
        let platform = platform_key();
        let payload = manifest(CURRENT_VERSION, &[platform.as_str()]);

        match check_against(&signer, &payload).unwrap() {
            Check::UpToDate { version, build, .. } => {
                assert_eq!(version, CURRENT_VERSION);
                assert!(build.is_some(), "the manifest lists this platform");
            }
            other => panic!("expected the running version to stand, got {other:?}"),
        }
    }

    #[test]
    fn offers_nothing_for_an_older_version() {
        // A replayed manifest from an earlier release, which carries a valid
        // signature, must not move the binary back.
        let signer = Signer::new();
        signer.trust();
        let platform = platform_key();
        let payload = manifest("0.0.1", &[platform.as_str()]);

        match check_against(&signer, &payload).unwrap() {
            Check::UpToDate { version, .. } => assert_eq!(version, "0.0.1"),
            other => panic!("expected no update, got {other:?}"),
        }
    }

    #[test]
    fn names_the_platforms_when_this_one_is_absent() {
        let signer = Signer::new();
        signer.trust();
        let payload = manifest("999.0.0", &["plan9-mips"]);

        match check_against(&signer, &payload).unwrap() {
            Check::NewerElsewhere {
                latest,
                platform,
                offered,
            } => {
                assert_eq!(latest, "999.0.0");
                assert_eq!(platform, platform_key());
                assert_eq!(offered, vec!["plan9-mips".to_string()]);
            }
            other => panic!("expected a version for other platforms, got {other:?}"),
        }
    }

    #[test]
    fn refuses_a_manifest_that_the_content_key_signed() {
        // The harness makes the client trust one key for both documents, so it
        // cannot give the content key a role of its own. This test covers the
        // rule one level down: a manifest that a key outside the trusted set
        // signed is refused. `the_two_key_lists_share_no_key` covers the rule
        // that the content key is not in the software list.
        let signer = Signer::new();
        Signer::new().trust();
        let platform = platform_key();
        let payload = manifest("999.0.0", &[platform.as_str()]);

        let error = check_against(&signer, &payload).unwrap_err();

        let text = format!("{error:#}");
        assert!(
            text.contains("cannot trust the software manifest"),
            "got {text}"
        );
    }

    #[test]
    fn refuses_a_version_with_a_forbidden_character() {
        let signer = Signer::new();
        signer.trust();
        let platform = platform_key();
        let payload = manifest("1.0.0/../x", &[platform.as_str()]);

        let error = check_against(&signer, &payload).unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("invalid version string"), "got {text}");
    }

    #[test]
    fn accepts_the_exact_version_line() {
        assert!(reports_version("brainmaker 0.1.1", "0.1.1"));
        assert!(reports_version("brainmaker 0.1.1\n", "0.1.1"));
        // A Windows line end.
        assert!(reports_version("brainmaker 0.1.1\r\n", "0.1.1"));
    }

    #[test]
    fn refuses_a_version_that_only_starts_the_same() {
        // `0.1.1` is a part of `0.1.10`, which a plain substring check misses.
        assert!(!reports_version("brainmaker 0.1.10", "0.1.1"));
        assert!(!reports_version("brainmaker 0.1.10\n", "0.1.1"));
        assert!(!reports_version("brainmaker 0.1.1", "0.1.10"));
    }

    #[test]
    fn refuses_another_program_name() {
        assert!(!reports_version("other 0.1.1", "0.1.1"));
    }
}
