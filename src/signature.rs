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
//!
//! # Three documents, three lists
//!
//! The same check reads two more documents, each against a list of its own:
//! a content release against [`CONTENT_KEYS`], and a removal order against
//! [`REMOVAL_KEYS`]. A key signs one kind of document and no other, so each
//! holder has one capability.

use anyhow::{Context, Result, bail};
use ring::signature::{self, UnparsedPublicKey};

use crate::cause::{self, Cause};

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

/// Keys that may sign a content release, newest first.
///
/// A separate list from [`PUBLIC_KEYS`] on purpose. The software key signs what
/// replaces the running executable, and it lives off every server. The content
/// key signs what lands in `content/`, and whoever publishes content holds it —
/// today that is the deploy host. One shared list would let the holder of the
/// content key sign a software manifest, and then publishing content and
/// replacing every binary would be the same capability.
///
/// An empty list means this build installs no content, the same way an empty
/// [`PUBLIC_KEYS`] installs no update.
// Kept one key per line, for the same reason PUBLIC_KEYS is: the release
// workflow finds a key with a grep that anchors to the start of a line.
#[rustfmt::skip]
pub const CONTENT_KEYS: &[&str] = &[
    "382972259033ae361706b404e7c42bad2a017b5a4f63bfc2aeec9374e5213c05",
];

/// Keys that may sign a removal order, newest first.
///
/// A third list, for the reason that the first two are separate. A removal
/// order makes one client remove brainmaker from its machine, and the admin
/// signs one each time that a person leaves. The software key signs once for
/// each release, and the content key lives on the deploy host. With a list of
/// its own, the admin holds a key that can remove a client and can do nothing
/// else, and neither of the other two keys can remove one.
///
/// An empty list means this build obeys no removal order. That is a usable
/// build: a team that wants no removal from a distance leaves the list empty.
/// Generate a pair with:
///
/// ```text
/// cargo run --features sign --bin brainmaker-sign -- keygen removal-signing.key
/// ```
// Kept one key per line, as the two lists above are.
#[rustfmt::skip]
pub const REMOVAL_KEYS: &[&str] = &[
];

/// Number of software signing keys this binary trusts. `status` prints it.
pub fn key_count() -> usize {
    PUBLIC_KEYS.len()
}

/// Number of content signing keys this binary trusts. `status` prints it.
pub fn content_key_count() -> usize {
    CONTENT_KEYS.len()
}

/// Number of removal signing keys this binary trusts. `status` prints it.
pub fn removal_key_count() -> usize {
    REMOVAL_KEYS.len()
}

#[cfg(test)]
thread_local! {
    /// Keys that a test trusts in place of the compiled-in lists.
    ///
    /// A test signs with a key it made, so the client under test must
    /// trust that key. The override lives in the test's own thread and
    /// exists in no other build.
    static TEST_KEYS: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };

    /// Keys that a test trusts for a removal order, in place of
    /// [`REMOVAL_KEYS`]. The slot is separate, so a test can show that a key
    /// for content or for software signs no removal order.
    static TEST_REMOVAL_KEYS: std::cell::RefCell<Option<Vec<String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Makes this thread trust `keys` for the software manifest and for the
/// content release.
#[cfg(test)]
pub fn trust_in_this_test(keys: &[String]) {
    TEST_KEYS.with(|slot| *slot.borrow_mut() = Some(keys.to_vec()));
}

/// Makes this thread trust `keys` for a removal order.
#[cfg(test)]
pub fn trust_for_removal_in_this_test(keys: &[String]) {
    TEST_REMOVAL_KEYS.with(|slot| *slot.borrow_mut() = Some(keys.to_vec()));
}

/// The keys for one check: the list `compiled`, or a test's own keys.
fn trusted(compiled: &[&str]) -> Vec<String> {
    #[cfg(test)]
    if let Some(keys) = TEST_KEYS.with(|slot| slot.borrow().clone()) {
        return keys;
    }
    compiled.iter().map(|key| key.to_string()).collect()
}

/// Checks `signature` against `payload`, using the software keys.
pub fn verify(payload: &[u8], signature: &str) -> Result<()> {
    let keys = trusted(PUBLIC_KEYS);
    let listed: Vec<&str> = keys.iter().map(String::as_str).collect();
    verify_with(&listed, payload, signature).context("the software manifest")
}

/// Checks `signature` against `payload`, using the content keys.
pub fn verify_content(payload: &[u8], signature: &str) -> Result<()> {
    let keys = trusted(CONTENT_KEYS);
    let listed: Vec<&str> = keys.iter().map(String::as_str).collect();
    verify_with(&listed, payload, signature).context("the content release")
}

/// The removal keys for one check: [`REMOVAL_KEYS`], or a test's own keys.
fn trusted_for_removal() -> Vec<String> {
    #[cfg(test)]
    if let Some(keys) = TEST_REMOVAL_KEYS.with(|slot| slot.borrow().clone()) {
        return keys;
    }
    REMOVAL_KEYS.iter().map(|key| key.to_string()).collect()
}

/// Checks `signature` against `payload`, using the removal keys.
pub fn verify_removal(payload: &[u8], signature: &str) -> Result<()> {
    let keys = trusted_for_removal();
    let listed: Vec<&str> = keys.iter().map(String::as_str).collect();
    verify_with(&listed, payload, signature).context("the removal order")
}

/// Checks `signature` against `payload`, using `keys`.
///
/// The function returns `Ok` as soon as one key accepts the signature. Every
/// value it reads is public, so the loop leaks nothing through its timing.
fn verify_with(keys: &[&str], payload: &[u8], signature: &str) -> Result<()> {
    if keys.is_empty() {
        return Err(untrusted(
            "this build trusts no signing key for this document, so it cannot check it and \
             will not act on it. Add the public key to PUBLIC_KEYS, CONTENT_KEYS, or \
             REMOVAL_KEYS in src/signature.rs and build again.",
        ));
    }

    let signature_bytes = decode_hex(signature.trim(), SIGNATURE_LEN)
        .context("the document carries a malformed signature")?;

    for (index, key) in keys.iter().enumerate() {
        let key_bytes = decode_hex(key.trim(), PUBLIC_KEY_LEN).with_context(|| {
            format!("compiled-in key {index} in src/signature.rs is not a usable public key")
        })?;

        let public_key = UnparsedPublicKey::new(&signature::ED25519, key_bytes);
        if public_key.verify(payload, &signature_bytes).is_ok() {
            return Ok(());
        }
    }

    Err(untrusted(
        "it carries no signature from a key this binary trusts. \
         Another key signed it, or something altered it after it was signed.",
    ))
}

/// An error for a document that no trusted key signed. It prints `text`, and a
/// report names its cause as a signature.
fn untrusted(text: &str) -> anyhow::Error {
    cause::failed(Cause::Signature, None, text.to_string())
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
        assert!(error.contains("compiled-in key 0"), "got {error}");
    }

    #[test]
    fn the_key_lists_share_no_key() {
        // A key in two lists would join two capabilities. Whoever signs
        // content could sign a software manifest, and replace every binary on
        // every machine. Whoever signs a removal order could publish content.
        let lists = [
            ("software", PUBLIC_KEYS),
            ("content", CONTENT_KEYS),
            ("removal", REMOVAL_KEYS),
        ];
        for (index, (name, list)) in lists.iter().enumerate() {
            for (other_name, other) in &lists[index + 1..] {
                for key in *list {
                    assert!(
                        !other.contains(key),
                        "{key} signs both {name} and {other_name}"
                    );
                }
            }
        }
    }

    #[test]
    fn removal_verification_uses_the_removal_list() {
        let pair = key_pair();
        let public = hex(pair.public_key().as_ref());
        let payload = br#"{"order":"remove"}"#;
        let signature = hex(pair.sign(payload).as_ref());

        // A key that this thread trusts for content and for software signs no
        // removal order.
        trust_in_this_test(std::slice::from_ref(&public));
        verify(payload, &signature).unwrap();
        verify_content(payload, &signature).unwrap();
        trust_for_removal_in_this_test(&[]);
        let error = format!("{:#}", verify_removal(payload, &signature).unwrap_err());
        assert!(error.contains("the removal order"), "got {error}");
        assert!(error.contains("trusts no signing key"), "got {error}");

        // And a key for removal orders signs neither of the other documents.
        let other = hex(key_pair().public_key().as_ref());
        trust_in_this_test(std::slice::from_ref(&other));
        trust_for_removal_in_this_test(std::slice::from_ref(&public));
        verify_removal(payload, &signature).unwrap();
        assert!(verify(payload, &signature).is_err());
        assert!(verify_content(payload, &signature).is_err());
    }

    #[test]
    fn content_verification_uses_the_content_list() {
        // With no content key compiled in, a content release cannot verify,
        // whatever the software list holds.
        if CONTENT_KEYS.is_empty() {
            assert!(verify_content(b"payload", &"a".repeat(128)).is_err());
        }
    }

    #[test]
    fn a_build_with_no_key_installs_no_update() {
        let error = verify_with(&[], b"payload", &"a".repeat(128))
            .unwrap_err()
            .to_string();
        assert!(error.contains("trusts no signing key"), "got {error}");
        assert!(error.contains("CONTENT_KEYS"), "got {error}");
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
