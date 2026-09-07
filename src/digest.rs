// SPDX-License-Identifier: GPL-3.0-or-later

//! SHA-256 over a file, and the checked form of a SHA-256 string.
//!
//! Two paths need the same two operations. `self-update` checks a downloaded
//! binary against the signed software manifest, and `sync` checks a downloaded
//! archive against the signed content release. Keeping one implementation
//! means a change to either check reaches both.

use std::fs;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// Length of a SHA-256 written as hexadecimal.
const SHA256_HEX_LEN: usize = 64;

/// Computes the SHA-256 of a file, streaming it.
///
/// The file is read in 64 KiB blocks, so an archive of any size costs the same
/// memory.
pub fn sha256_of(path: &Path) -> Result<String> {
    let file = fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex(&hasher.finalize()))
}

/// Writes bytes as lower-case hexadecimal.
///
/// `sha2` returns an array that carries no `LowerHex`, so the conversion is
/// written out rather than formatted.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Returns `value` in lower case, or fails when it is not a SHA-256.
///
/// `what` names the source in the message, so an operator reading the error
/// knows which document carried the bad value.
pub fn checked_sha256(value: &str, what: &str) -> Result<String> {
    let trimmed = value.trim().to_ascii_lowercase();
    if trimmed.len() != SHA256_HEX_LEN || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "{what} carries the invalid SHA-256 {value:?}; \
             expected {SHA256_HEX_LEN} hexadecimal characters"
        );
    }
    Ok(trimmed)
}

/// Fails when `actual` and `expected` differ.
///
/// Both values are compared in the checked lower-case form, so a server that
/// writes upper-case hexadecimal does not fail a download that is correct.
pub fn check_matches(actual: &str, expected: &str, what: &str) -> Result<()> {
    let actual = checked_sha256(actual, "the downloaded file")?;
    let expected = checked_sha256(expected, what)?;
    if actual != expected {
        bail!("the download does not match {what}: expected {expected}, got {actual}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str, body: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brainmaker-digest-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn digests_a_known_value() {
        let path = temp_file("abc", b"abc");
        assert_eq!(
            sha256_of(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn digests_an_empty_file() {
        let path = temp_file("empty", b"");
        assert_eq!(
            sha256_of(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn digests_a_file_larger_than_one_block() {
        // 64 KiB is the read block, so this crosses it.
        let path = temp_file("big", &vec![7u8; 200 * 1024]);
        let first = sha256_of(&path).unwrap();
        assert_eq!(first.len(), SHA256_HEX_LEN);
        assert_eq!(first, sha256_of(&path).unwrap(), "the digest is stable");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn accepts_a_digest_in_either_case() {
        let upper = "A".repeat(64);
        assert_eq!(checked_sha256(&upper, "x").unwrap(), "a".repeat(64));
        assert_eq!(
            checked_sha256(&format!("  {}  ", "b".repeat(64)), "x").unwrap(),
            "b".repeat(64)
        );
    }

    #[test]
    fn rejects_a_value_that_is_not_a_digest() {
        for value in ["", "abc", &"z".repeat(64), &"a".repeat(63), &"a".repeat(65)] {
            assert!(
                checked_sha256(value, "the manifest").is_err(),
                "{value:?} should not pass"
            );
        }
    }

    #[test]
    fn names_the_source_in_the_message() {
        let error = checked_sha256("nope", "the content release")
            .unwrap_err()
            .to_string();
        assert!(error.contains("the content release"));
    }

    #[test]
    fn matches_two_equal_digests_and_refuses_two_that_differ() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        assert!(check_matches(&a, &a.to_uppercase(), "x").is_ok());
        let error = check_matches(&a, &b, "the content release")
            .unwrap_err()
            .to_string();
        assert!(error.contains("the content release"));
        assert!(error.contains(&b));
    }
}
