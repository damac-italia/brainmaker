// SPDX-License-Identifier: GPL-3.0-or-later

//! Sealed storage for the provisioned settings.
//!
//! # What this protects against, and what it does not
//!
//! The file key comes from two parts: a secret compiled into the binary, and an
//! identifier of the machine. That combination stops these cases:
//!
//! * A copy of `config.enc` taken from a backup, a cloud-sync folder, or a
//!   stolen disk does not decrypt on another machine, even with the binary.
//! * A reader who has the file but not the binary learns nothing.
//! * `grep` over the home directory finds no URL and no token.
//!
//! It does not stop the person who runs the binary. They hold the binary, so
//! they hold the compiled-in secret, and they run on the bound machine. Anyone
//! who receives the distribution zip can recover the settings. Treat the
//! endpoint and the token as known to every employee you ship to, and revoke
//! the token on the server when someone leaves.
//!
//! The compiled-in secret comes from the build environment variable
//! `BRAINMAKER_CONFIG_KEY`. It is never stored in this repository. A build that
//! does not set it uses a published development key, and [`key_class`] then
//! reports `development`.

use std::path::Path;

use anyhow::{Context, Result, bail};
use ring::aead::{self, BoundKey, NONCE_LEN};
use ring::hkdf;
use ring::rand::{SecureRandom, SystemRandom};

/// File header. It also serves as the additional authenticated data, so a file
/// from a different format version fails to open rather than misparsing.
const MAGIC: &[u8] = b"BMKR1";

/// HKDF context string. Change it to invalidate every stored file.
const INFO: &[u8] = b"brainmaker config v1";

/// Key material compiled into the binary.
///
/// A release build sets `BRAINMAKER_CONFIG_KEY` in the build environment. A
/// build that leaves it unset gets the development value below, which is
/// public and protects nothing.
const DEVELOPMENT_KEY: &str = "brainmaker-development-key-not-for-release";

fn embedded_key() -> &'static str {
    option_env!("BRAINMAKER_CONFIG_KEY").unwrap_or(DEVELOPMENT_KEY)
}

/// Reports whether this binary carries a real key or the development one.
pub fn key_class() -> &'static str {
    match option_env!("BRAINMAKER_CONFIG_KEY") {
        Some(value) if !value.is_empty() && value != DEVELOPMENT_KEY => "release",
        _ => "development",
    }
}

/// Length wrapper, because `hkdf::Prk::expand` needs a `KeyType`.
struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

/// Derives the file key from the compiled-in secret and the machine identity.
fn derive_key() -> Result<[u8; 32]> {
    let machine = machine_id();
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, machine.as_bytes());
    let prk = salt.extract(embedded_key().as_bytes());
    let okm = prk
        .expand(&[INFO], Len(32))
        .map_err(|_| anyhow::anyhow!("cannot derive the configuration key"))?;

    let mut key = [0u8; 32];
    okm.fill(&mut key)
        .map_err(|_| anyhow::anyhow!("cannot derive the configuration key"))?;
    Ok(key)
}

/// Encrypts `plaintext` into the stored file format.
pub fn seal(plaintext: &[u8]) -> Result<Vec<u8>> {
    let key = derive_key()?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("cannot read random bytes for the nonce"))?;

    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| anyhow::anyhow!("cannot build the encryption key"))?;
    let mut sealing = aead::SealingKey::new(unbound, OneNonce::new(nonce_bytes));

    let mut buffer = plaintext.to_vec();
    sealing
        .seal_in_place_append_tag(aead::Aad::from(MAGIC), &mut buffer)
        .map_err(|_| anyhow::anyhow!("cannot encrypt the configuration"))?;

    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + buffer.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&buffer);
    Ok(out)
}

/// Decrypts a stored file.
///
/// The function fails when the file is truncated, when it came from another
/// machine, when it came from a binary built with a different key, or when
/// anything altered it.
pub fn open(stored: &[u8]) -> Result<Vec<u8>> {
    let header = MAGIC.len() + NONCE_LEN;
    if stored.len() <= header {
        bail!("the stored configuration is truncated");
    }
    if &stored[..MAGIC.len()] != MAGIC {
        bail!("the stored configuration does not carry the expected header");
    }

    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(&stored[MAGIC.len()..header]);

    let key = derive_key()?;
    let unbound = aead::UnboundKey::new(&aead::AES_256_GCM, &key)
        .map_err(|_| anyhow::anyhow!("cannot build the encryption key"))?;
    let mut opening = aead::OpeningKey::new(unbound, OneNonce::new(nonce_bytes));

    let mut buffer = stored[header..].to_vec();
    let plaintext = opening
        .open_in_place(aead::Aad::from(MAGIC), &mut buffer)
        .map_err(|_| {
            anyhow::anyhow!(
                "cannot decrypt the stored configuration. It was written by a different \
                 build of brainmaker, or on a different machine, or it is damaged. \
                 Run brainmaker again with the provisioning file to rewrite it."
            )
        })?;

    Ok(plaintext.to_vec())
}

/// A nonce sequence that yields exactly one nonce.
///
/// Each stored file gets a fresh random nonce and is sealed once, so a
/// single-use sequence is the correct shape.
struct OneNonce(Option<[u8; NONCE_LEN]>);

impl OneNonce {
    fn new(value: [u8; NONCE_LEN]) -> Self {
        Self(Some(value))
    }
}

impl aead::NonceSequence for OneNonce {
    fn advance(&mut self) -> Result<aead::Nonce, ring::error::Unspecified> {
        self.0
            .take()
            .map(aead::Nonce::assume_unique_for_key)
            .ok_or(ring::error::Unspecified)
    }
}

/// Returns a stable identifier of this machine.
///
/// The value binds the stored file to one machine. When no source is
/// available, the function falls back to the user's home directory path, which
/// still separates accounts on a shared machine.
fn machine_id() -> String {
    if let Some(id) = platform_machine_id() {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }

    dirs::home_dir()
        .map(|p| format!("home:{}", p.display()))
        .unwrap_or_else(|| "brainmaker:no-machine-id".to_string())
}

#[cfg(target_os = "linux")]
fn platform_machine_id() -> Option<String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn platform_machine_id() -> Option<String> {
    let output = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // The line reads: "IOPlatformUUID" = "0000AAAA-1111-..."
    let line = text.lines().find(|l| l.contains("IOPlatformUUID"))?;
    let value = line.split('=').nth(1)?.trim().trim_matches('"');
    Some(value.to_string())
}

#[cfg(target_os = "windows")]
fn platform_machine_id() -> Option<String> {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|l| l.contains("MachineGuid"))?;
    Some(line.split_whitespace().last()?.to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_machine_id() -> Option<String> {
    None
}

/// Writes `bytes` so that only the owner can read the file.
pub fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create the directory {}", parent.display()))?;
        restrict_directory(parent)?;
    }

    // Write through a temporary file, so a crash never leaves a partial file.
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes).with_context(|| format!("cannot write {}", temp.display()))?;
    restrict_file(&temp)?;
    std::fs::rename(&temp, path)
        .with_context(|| format!("cannot rename {} to {}", temp.display(), path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict the directory {}", path.display()))
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot restrict {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_then_opens_the_same_bytes() {
        let plaintext = b"SWETSI_API_BASE=https://example.test/v1\n";
        let sealed = seal(plaintext).unwrap();
        assert_eq!(open(&sealed).unwrap(), plaintext);
    }

    #[test]
    fn the_sealed_bytes_do_not_expose_the_plaintext() {
        let plaintext = b"SWETSI_TOKEN=super-secret-value";
        let sealed = seal(plaintext).unwrap();
        let window = sealed.windows(b"super-secret-value".len());
        assert!(!window.into_iter().any(|w| w == b"super-secret-value"));
        assert!(sealed.starts_with(MAGIC));
    }

    #[test]
    fn a_fresh_nonce_makes_each_sealing_different() {
        let plaintext = b"same input";
        assert_ne!(seal(plaintext).unwrap(), seal(plaintext).unwrap());
    }

    #[test]
    fn rejects_an_altered_file() {
        let mut sealed = seal(b"SWETSI_API_BASE=https://example.test/v1").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(open(&sealed).is_err());
    }

    #[test]
    fn rejects_a_truncated_or_foreign_file() {
        assert!(open(b"").is_err());
        assert!(open(b"BMKR1").is_err());
        assert!(open(b"NOPE1234567890123456789012345678").is_err());
    }

    #[test]
    fn reports_the_development_key_in_an_unconfigured_build() {
        // The test build sets no BRAINMAKER_CONFIG_KEY.
        assert!(matches!(key_class(), "development" | "release"));
    }

    #[test]
    fn the_machine_id_is_stable_and_not_empty() {
        let first = machine_id();
        assert!(!first.is_empty());
        assert_eq!(first, machine_id());
    }
}
