// SPDX-License-Identifier: GPL-3.0-or-later

//! Signature check for the software manifest.
//!
//! # Why this exists
//!
//! `self-update` replaces the running executable. Without a signature, the TLS
//! connection to the API host is the only trust anchor: whoever controls that
//! host controls both the manifest and the SHA-256 inside it, so the checksum
//! constrains them not at all.
//!
//! An Ed25519 signature over the manifest moves the trust anchor to the holder
//! of the signing key. The API host, the CDN in front of it, and the storage
//! bucket behind it all leave the trusted set.
//!
//! # What is signed
//!
//! The signature covers the exact manifest bytes that the server serves, before
//! anything parses them. No JSON canonicalisation rule takes part in the
//! security argument. See [`crate::selfupdate::Envelope`].
//!
//! # The public key
//!
//! The key is public, so it lives in this repository, unlike the configuration
//! key. A build with no key in [`PUBLIC_KEYS`] refuses every manifest, which
//! fails closed.

use anyhow::{Context, Result, bail};
use ring::signature::{self, UnparsedPublicKey};

/// Length of an Ed25519 public key, in bytes.
const PUBLIC_KEY_LEN: usize = 32;

/// Length of an Ed25519 signature, in bytes.
const SIGNATURE_LEN: usize = 64;

/// Public keys that this binary accepts, newest first.
///
/// Each entry is 64 hexadecimal characters, which is one raw Ed25519 public
/// key. Generate a pair with:
///
/// ```text
/// cargo run --features sign --bin brainmaker-sign -- keygen signing.key
/// ```
///
/// Paste the printed key here, commit it, and release. To rotate a key, put the
/// new key first and keep the old one until every client carries a binary that
/// holds the new one. A manifest verifies when any key in this list accepts it.
///
/// An empty list means this build trusts no key and installs no update.
// Kept one key per line: the release workflow checks for a key with a grep that
// anchors to the start of a line, so that a commented-out placeholder cannot
// satisfy it. See "Check that a manifest signing key is compiled in" in
// .github/workflows/release.yml.
#[rustfmt::skip]
pub const PUBLIC_KEYS: &[&str] = &[
    "ce8e1071de31dc8df324a296bdcca1beebabfc17f9f2b2d9a1f874b7bc45a2e7",
];

/// Number of signing keys this binary trusts. `status` prints it.
pub fn key_count() -> usize {
    PUBLIC_KEYS.len()
}

/// Checks `signature` against `payload`, using the compiled-in keys.
pub fn verify(payload: &[u8], signature: &str) -> Result<()> {
    verify_with(PUBLIC_KEYS, payload, signature)
}

/// Checks `signature` against `payload`, using `keys`.
///
/// The function returns `Ok` as soon as one key accepts the signature. Every
/// value it reads is public, so the loop leaks nothing through its timing.
fn verify_with(keys: &[&str], payload: &[u8], signature: &str) -> Result<()> {
    if keys.is_empty() {
        bail!(
            "this build trusts no manifest signing key, so it cannot check the software \
             manifest and will install no update. Add the public key to PUBLIC_KEYS in \
             src/signature.rs and build again."
        );
    }

    let signature_bytes = decode_hex(signature.trim(), SIGNATURE_LEN)
        .context("the software manifest carries a malformed signature")?;

    for (index, key) in keys.iter().enumerate() {
        let key_bytes = decode_hex(key.trim(), PUBLIC_KEY_LEN).with_context(|| {
            format!("PUBLIC_KEYS entry {index} in src/signature.rs is not a usable public key")
        })?;

        let public_key = UnparsedPublicKey::new(&signature::ED25519, key_bytes);
        if public_key.verify(payload, &signature_bytes).is_ok() {
            return Ok(());
        }
    }

    bail!(
        "the software manifest carries no signature from a key this binary trusts. \
         Another key signed it, or something altered it after it was signed."
    )
}

/// Decodes a hexadecimal string into exactly `expected` bytes.
fn decode_hex(text: &str, expected: usize) -> Result<Vec<u8>> {
    if text.len() != expected * 2 {
        bail!(
            "expected {} hexadecimal characters, got {}",
            expected * 2,
            text.len()
        );
    }

    let mut out = Vec::with_capacity(expected);
    for pair in text.as_bytes().chunks(2) {
        out.push(digit(pair[0])? << 4 | digit(pair[1])?);
    }
    Ok(out)
}

fn digit(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        other => bail!("{:?} is not a hexadecimal character", other as char),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Returns a fresh key pair, its public key in hex, and a signer.
    fn key_pair() -> Ed25519KeyPair {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    #[test]
    fn accepts_a_signature_from_a_trusted_key() {
        let pair = key_pair();
        let public = hex(pair.public_key().as_ref());
        let payload = br#"{"version":"0.2.0","platforms":{}}"#;
        let signature = hex(pair.sign(payload).as_ref());

        verify_with(&[&public], payload, &signature).unwrap();
    }

    #[test]
    fn accepts_an_uppercase_signature_and_key() {
        let pair = key_pair();
        let public = hex(pair.public_key().as_ref()).to_uppercase();
        let payload = b"payload";
        let signature = hex(pair.sign(payload).as_ref()).to_uppercase();

        verify_with(&[&public], payload, &signature).unwrap();
    }

    #[test]
    fn accepts_a_signature_from_any_listed_key() {
        // This is what makes a key rotation possible: the old key stays in the
        // list until every client carries a binary that holds the new one.
        let old = key_pair();
        let new = key_pair();
        let payload = b"payload";

        let keys = [
            hex(new.public_key().as_ref()),
            hex(old.public_key().as_ref()),
        ];
        let listed: Vec<&str> = keys.iter().map(String::as_str).collect();

        verify_with(&listed, payload, &hex(old.sign(payload).as_ref())).unwrap();
        verify_with(&listed, payload, &hex(new.sign(payload).as_ref())).unwrap();
    }

    #[test]
    fn rejects_a_signature_from_another_key() {
        let signer = key_pair();
        let other = key_pair();
        let payload = b"payload";
        let signature = hex(signer.sign(payload).as_ref());

        let error = verify_with(&[&hex(other.public_key().as_ref())], payload, &signature)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("no signature from a key this binary trusts"),
            "got {error}"
        );
    }

    #[test]
    fn rejects_an_altered_payload() {
        let pair = key_pair();
        let public = hex(pair.public_key().as_ref());
        let signature = hex(pair.sign(b"the signed payload").as_ref());

        assert!(verify_with(&[&public], b"the altered payload", &signature).is_err());
    }

    #[test]
    fn rejects_an_altered_signature() {
        let pair = key_pair();
        let public = hex(pair.public_key().as_ref());
        let payload = b"payload";

        let mut signature: Vec<u8> = pair.sign(payload).as_ref().to_vec();
        signature[0] ^= 0xff;

        assert!(verify_with(&[&public], payload, &hex(&signature)).is_err());
    }

    #[test]
    fn rejects_a_malformed_signature() {
        let public = hex(key_pair().public_key().as_ref());
        assert!(verify_with(&[&public], b"payload", "").is_err());
        assert!(verify_with(&[&public], b"payload", &"z".repeat(128)).is_err());
        assert!(verify_with(&[&public], b"payload", &"a".repeat(127)).is_err());
    }

    #[test]
    fn rejects_a_malformed_compiled_in_key() {
        let error = verify_with(&["not a key"], b"payload", &"a".repeat(128))
            .unwrap_err()
            .to_string();
        assert!(error.contains("PUBLIC_KEYS entry 0"), "got {error}");
    }

    #[test]
    fn a_build_with_no_key_installs_no_update() {
        let error = verify_with(&[], b"payload", &"a".repeat(128))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("trusts no manifest signing key"),
            "got {error}"
        );
    }

    #[test]
    fn decodes_hexadecimal_in_either_case() {
        assert_eq!(
            decode_hex("00ff10AB", 4).unwrap(),
            vec![0x00, 0xff, 0x10, 0xab]
        );
        assert!(decode_hex("0", 1).is_err());
        assert!(decode_hex("0g", 1).is_err());
        // A multibyte character must not pass the length check.
        assert!(decode_hex("é", 1).is_err());
    }
}
