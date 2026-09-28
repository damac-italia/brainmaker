// SPDX-License-Identifier: GPL-3.0-or-later
//
// brainmaker — keeps ~/.brainmaker/content in step with the content API.
// Copyright (C) 2026 atom7xyz
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Key generation and manifest signing for brainmaker.
//!
//! This tool is not part of the shipped binary. It builds only when you ask for
//! the `sign` feature:
//!
//! ```text
//! cargo run --features sign --bin brainmaker-sign -- keygen signing.key
//! cargo run --features sign --bin brainmaker-sign -- sign signing.key dist/manifest.json dist/manifest.signed.json
//! ```
//!
//! The tool uses `ring`, which the crate already depends on, so signing needs
//! no OpenSSL. That matters on macOS, whose system LibreSSL does not sign
//! Ed25519 reliably.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use ring::rand::SystemRandom;
use ring::signature::{self, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Environment variable that carries the PKCS#8 key as hexadecimal.
///
/// Pass `-` in place of the key file to read it. CI uses this form, so the key
/// never touches the runner's disk.
const KEY_ENV: &str = "BRAINMAKER_SIGNING_KEY";

/// Length of a content hash. The client holds the same value as HASH_LEN
/// in src/config.rs, and a test below fails when the two differ.
const HASH_LEN: usize = 8;

/// Largest archive that the client installs. The client holds the same
/// value as MAX_ARCHIVE_BYTES in src/config.rs, and a test below fails
/// when the two differ.
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

const HELP: &str = "\
brainmaker-sign — generate a signing key, and sign what brainmaker installs

USAGE:
    brainmaker-sign keygen <KEY-FILE>
    brainmaker-sign sign <KEY-FILE|-> <MANIFEST-FILE> <ENVELOPE-FILE>
    brainmaker-sign sign-content <KEY-FILE|-> <ARCHIVE> <HASH> <ENVELOPE-FILE> [SEQUENCE]
    brainmaker-sign verify <ENVELOPE-FILE> <PUBLIC-KEY>...

COMMANDS:
    keygen        Write a new PKCS#8 Ed25519 key, and print its public key
    sign          Wrap a software manifest and its signature into the envelope
                  that brainmaker downloads
    sign-content  Digest a content archive and sign the result, giving the
                  envelope that {base}/content/latest returns. The payload
                  carries a sequence, which is the time of signing unless
                  SEQUENCE names one. A client installs only a release with
                  a higher sequence than the one it holds.
    verify        Check an envelope against one or more public keys, the way
                  brainmaker checks it. Run this before you publish.

KEY-FILE:
    A path holding the PKCS#8 key as hexadecimal. Pass - to read the key from
    the environment variable BRAINMAKER_SIGNING_KEY instead.

The printed public key goes into PUBLIC_KEYS in src/signature.rs. Keep the
private key off every machine that serves the API.
";

/// The body that `GET {base}/software/brainmaker` returns.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    payload: String,
    signature: String,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arguments: Vec<&str> = args.iter().map(String::as_str).collect();

    match run(&arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[&str]) -> Result<()> {
    match args {
        [] | ["-h"] | ["--help"] => {
            print!("{HELP}");
            Ok(())
        }
        ["keygen", key_file] => keygen(Path::new(key_file)),
        ["sign", key, manifest, envelope] => sign(key, Path::new(manifest), Path::new(envelope)),
        ["sign-content", key, archive, hash, envelope] => {
            sign_content(key, Path::new(archive), hash, Path::new(envelope), None)
        }
        ["sign-content", key, archive, hash, envelope, sequence] => sign_content(
            key,
            Path::new(archive),
            hash,
            Path::new(envelope),
            Some(sequence),
        ),
        ["verify", envelope, keys @ ..] if !keys.is_empty() => verify(Path::new(envelope), keys),
        _ => bail!("unknown arguments; run brainmaker-sign --help"),
    }
}

/// Writes a new key, and prints the public half.
fn keygen(key_file: &Path) -> Result<()> {
    use std::io::Write;

    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| anyhow::anyhow!("cannot generate a key"))?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|error| anyhow::anyhow!("cannot read the generated key: {error}"))?;

    let mut file = match create_owner_only(key_file) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "{} already exists. Signing with a new key needs a key rotation, \
                 so move the old file aside on purpose.",
                key_file.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot create {}", key_file.display()));
        }
    };
    file.write_all(hex(pkcs8.as_ref()).as_bytes())
        .with_context(|| format!("cannot write {}", key_file.display()))?;

    println!("Wrote the private key to {}.", key_file.display());
    println!();
    println!("Add this line to PUBLIC_KEYS in src/signature.rs:");
    println!("    \"{}\",", hex(pair.public_key().as_ref()));
    println!();
    println!(
        "Then commit it, and keep {} off every server.",
        key_file.display()
    );
    Ok(())
}

/// Creates `path` for writing, readable by its owner alone.
///
/// The file gets its mode when it is created, so no moment exists at which
/// another account can read the key. `create_new` fails when anything is
/// already at the path, a symbolic link included, so a key is never
/// written through a link to another place.
#[cfg(unix)]
fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Signs a manifest and writes the envelope.
fn sign(key: &str, manifest_file: &Path, envelope_file: &Path) -> Result<()> {
    let pair = load_key(key)?;

    let payload = std::fs::read_to_string(manifest_file)
        .with_context(|| format!("cannot read {}", manifest_file.display()))?;

    // Refuse to sign a file that brainmaker could not use. A signature over
    // broken JSON would fail only at the client, after publication.
    check_manifest(&payload)
        .with_context(|| format!("{} is not a usable manifest", manifest_file.display()))?;

    let envelope = Envelope {
        signature: hex(pair.sign(payload.as_bytes()).as_ref()),
        payload,
    };

    let text = serde_json::to_string_pretty(&envelope).context("cannot write the envelope")?;
    std::fs::write(envelope_file, format!("{text}\n"))
        .with_context(|| format!("cannot write {}", envelope_file.display()))?;

    println!(
        "Signed {} into {}.",
        manifest_file.display(),
        envelope_file.display()
    );
    println!("Public key: {}", hex(pair.public_key().as_ref()));
    println!(
        "Serve {} as {{base}}/software/brainmaker.",
        envelope_file.display()
    );
    Ok(())
}

/// Digests `archive` and signs a content release that describes it.
///
/// The signed payload carries the hash, the digest, the size, and a sequence,
/// and no URL. The client derives the download address from its own base URL,
/// so a signed release cannot move the download to another host — the rule the
/// software manifest already follows.
///
/// The sequence orders the releases, and a client installs a release only when
/// its sequence is higher than the one it holds. `sequence` names one. With
/// `None`, the sequence is the time of signing in seconds since the Unix epoch,
/// so a release signed later always carries a higher value.
fn sign_content(
    key: &str,
    archive: &Path,
    hash: &str,
    envelope_file: &Path,
    sequence: Option<&str>,
) -> Result<()> {
    let pair = load_key(key)?;

    check_hash(hash)?;
    let sequence: u64 = match sequence {
        Some(text) => text
            .parse()
            .map_err(|_| anyhow::anyhow!("the sequence {text:?} is not a whole number"))?,
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| anyhow::anyhow!("the clock reads a time before 1970"))?
            .as_secs(),
    };
    let bytes =
        std::fs::read(archive).with_context(|| format!("cannot read {}", archive.display()))?;
    if bytes.is_empty() {
        bail!("{} is empty", archive.display());
    }
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        bail!(
            "{} is {} bytes, past the limit of {MAX_ARCHIVE_BYTES} that every client applies",
            archive.display(),
            bytes.len()
        );
    }

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sha256 = hex(&hasher.finalize());

    // The signature covers these exact bytes, and the client checks them
    // before it parses them. The field order therefore only has to stay put
    // after signing, which serialising once here guarantees.
    let payload = serde_json::to_string(&serde_json::json!({
        "hash": hash,
        "sha256": sha256,
        "size_bytes": bytes.len(),
        "sequence": sequence,
    }))
    .context("cannot write the content release")?;

    let envelope = Envelope {
        signature: hex(pair.sign(payload.as_bytes()).as_ref()),
        payload,
    };

    let text = serde_json::to_string_pretty(&envelope).context("cannot write the envelope")?;
    std::fs::write(envelope_file, format!("{text}\n"))
        .with_context(|| format!("cannot write {}", envelope_file.display()))?;

    println!(
        "Signed {} ({} bytes, sha256 {sha256}, sequence {sequence}) into {}.",
        archive.display(),
        bytes.len(),
        envelope_file.display()
    );
    println!("Public key: {}", hex(pair.public_key().as_ref()));
    println!("Publish its payload and signature with the archive, so that");
    println!("{{base}}/content/latest returns both.");
    Ok(())
}

/// Rejects a hash that brainmaker's `validate_hash` would reject.
///
/// Eight alphanumeric ASCII characters, which is what synapsis derives from
/// the first 8 characters of the archive digest.
fn check_hash(hash: &str) -> Result<()> {
    if hash.len() != HASH_LEN || !hash.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("the hash {hash:?} is not 8 alphanumeric ASCII characters");
    }
    Ok(())
}

/// Checks an envelope against `keys`, the way brainmaker checks it.
///
/// Run this before you publish. It catches a manifest signed with a key that no
/// released binary trusts, which would otherwise stop every self-update.
fn verify(envelope_file: &Path, keys: &[&str]) -> Result<()> {
    let text = std::fs::read_to_string(envelope_file)
        .with_context(|| format!("cannot read {}", envelope_file.display()))?;
    let envelope: Envelope = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a signed envelope", envelope_file.display()))?;

    let signature_bytes = decode_hex(envelope.signature.trim())
        .context("the envelope carries a malformed signature")?;
    if signature_bytes.len() != 64 {
        bail!(
            "the signature is {} bytes, and an Ed25519 signature is 64",
            signature_bytes.len()
        );
    }

    for (index, key) in keys.iter().enumerate() {
        let key_bytes = decode_hex(key.trim())
            .with_context(|| format!("public key {index} is not hexadecimal"))?;
        if key_bytes.len() != 32 {
            bail!(
                "public key {index} is {} bytes, and an Ed25519 public key is 32",
                key_bytes.len()
            );
        }

        let public_key = UnparsedPublicKey::new(&signature::ED25519, key_bytes);
        if public_key
            .verify(envelope.payload.as_bytes(), &signature_bytes)
            .is_ok()
        {
            let kind = check_payload(&envelope.payload)?;
            println!(
                "{} verifies against public key {index} ({key}).",
                envelope_file.display()
            );
            println!("The signed payload is {kind}.");
            return Ok(());
        }
    }

    bail!(
        "{} verifies against none of the {} public key(s) given. \
         Every brainmaker that carries those keys would refuse this envelope.",
        envelope_file.display(),
        keys.len()
    )
}

/// Reads the key from a file, or from [`KEY_ENV`] when `key` is `-`.
fn load_key(key: &str) -> Result<Ed25519KeyPair> {
    let text = if key == "-" {
        std::env::var(KEY_ENV).with_context(|| format!("{KEY_ENV} is not set"))?
    } else {
        std::fs::read_to_string(key).with_context(|| format!("cannot read {key}"))?
    };

    let bytes = decode_hex(text.trim()).context("the signing key is not hexadecimal")?;
    Ed25519KeyPair::from_pkcs8(&bytes)
        .map_err(|error| anyhow::anyhow!("the signing key is not a PKCS#8 Ed25519 key: {error}"))
}

/// Names the shape of a signed payload, or fails when it is neither shape.
///
/// One envelope carries a software manifest, and another carries a content
/// release. Both reach the same route pattern and the same signature check, so
/// `verify` accepts either and says which it read. Guessing wrong would let a
/// content release pass as a manifest.
fn check_payload(text: &str) -> Result<&'static str> {
    let value: serde_json::Value =
        serde_json::from_str(text).context("the signed payload is not valid JSON")?;

    if value.get("platforms").is_some() {
        check_manifest(text).context("the signed payload is not a usable software manifest")?;
        return Ok("a software manifest");
    }
    if value.get("sha256").is_some() {
        check_content(text).context("the signed payload is not a usable content release")?;
        return Ok("a content release");
    }
    bail!(
        "the signed payload is neither a software manifest, which carries \"platforms\", \
         nor a content release, which carries \"sha256\""
    )
}

/// Fails for a content release that brainmaker could not use.
fn check_content(text: &str) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(text).context("the content release is not valid JSON")?;

    let hash = value
        .get("hash")
        .and_then(serde_json::Value::as_str)
        .context("the content release has no \"hash\" string")?;
    check_hash(hash)?;

    let sha256 = value
        .get("sha256")
        .and_then(serde_json::Value::as_str)
        .context("the content release has no \"sha256\" string")?;
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("the content release carries the invalid SHA-256 {sha256:?}");
    }

    let size = value
        .get("size_bytes")
        .and_then(serde_json::Value::as_u64)
        .context("the content release has no \"size_bytes\" number")?;
    if size == 0 {
        bail!("the content release claims a size of 0 bytes");
    }
    if size > MAX_ARCHIVE_BYTES {
        bail!(
            "the content release claims {size} bytes, past the limit of \
             {MAX_ARCHIVE_BYTES} that every client applies"
        );
    }

    // The sequence is optional, because a client accepts a release from a
    // signer that wrote none. When it is there, it must be a whole number,
    // because a client refuses a payload with a sequence that it cannot read.
    if value
        .get("sequence")
        .is_some_and(|sequence| sequence.as_u64().is_none())
    {
        bail!("the content release carries a \"sequence\" that is not a whole number");
    }

    // The client derives the download address from its own base URL, so a URL
    // here would be a URL the client ignores and an operator trusts.
    if value.get("url").is_some() {
        bail!("the content release carries a \"url\", and brainmaker derives the address itself");
    }
    Ok(())
}

/// Fails when the manifest is not the shape brainmaker reads.
fn check_manifest(text: &str) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(text).context("the manifest is not valid JSON")?;

    let version = value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .context("the manifest has no \"version\" string")?;
    if !usable_version(version) {
        bail!(
            "the manifest version {version:?} is not 1 to 64 characters of ASCII letters, \
             digits, dots, hyphens, and plus signs, so every client would refuse it"
        );
    }

    let platforms = value
        .get("platforms")
        .and_then(serde_json::Value::as_object)
        .context("the manifest has no \"platforms\" object")?;
    if platforms.is_empty() {
        bail!("the manifest names no platform");
    }

    // A build carries a checksum and nothing else. The client derives the
    // download address from its own base URL, so a manifest names no URL.
    for (key, build) in platforms {
        let sum = build
            .get("sha256")
            .and_then(serde_json::Value::as_str)
            .with_context(|| format!("the platform {key} has no \"sha256\" string"))?;
        if sum.len() != 64 || !sum.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("the platform {key} carries the invalid SHA-256 {sum:?}");
        }
    }

    Ok(())
}

/// True for a version string that the client accepts.
///
/// The same rule as `version::validate` in src/version.rs. A test below
/// fails when the two disagree.
fn usable_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+')
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn decode_hex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        bail!("the value has an odd number of characters");
    }

    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let mut byte = 0u8;
        for half in pair {
            let value = match half {
                b'0'..=b'9' => half - b'0',
                b'a'..=b'f' => half - b'a' + 10,
                b'A'..=b'F' => half - b'A' + 10,
                other => bail!("{:?} is not a hexadecimal character", *other as char),
            };
            byte = byte << 4 | value;
        }
        out.push(byte);
    }
    Ok(out)
}

// The client's own rules, compiled into the tests alone, so that the tests
// below can compare the signer against them.
#[cfg(test)]
#[path = "../src/version.rs"]
mod client_version;

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "brainmaker-sign-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// A software manifest with one platform, at `version`.
    fn manifest(version: &str) -> String {
        serde_json::json!({
            "version": version,
            "platforms": { "darwin-arm64": { "sha256": DIGEST } },
        })
        .to_string()
    }

    /// A content release that claims `size` bytes.
    fn release(size: u64) -> String {
        serde_json::json!({
            "hash": "abcd1234",
            "sha256": DIGEST,
            "size_bytes": size,
        })
        .to_string()
    }

    #[test]
    fn the_version_rule_is_the_client_rule() {
        let nines_64 = "9".repeat(64);
        let nines_65 = "9".repeat(65);
        let samples = [
            "0.2.0",
            "1.0.0-rc.1+build9",
            "",
            "0.2.0; rm -rf /",
            "0.2.0\n",
            "v1",
            "1 0",
            "1.0.0/../x",
            nines_64.as_str(),
            nines_65.as_str(),
            "1.0.0é",
        ];
        for sample in samples {
            assert_eq!(
                usable_version(sample),
                client_version::validate(sample),
                "the signer and the client disagree about {sample:?}"
            );
        }
    }

    #[test]
    fn the_limits_are_the_client_limits() {
        let config = include_str!("../src/config.rs");
        assert!(
            config.contains("pub const HASH_LEN: usize = 8;"),
            "src/config.rs changed HASH_LEN, so change HASH_LEN in the signer"
        );
        assert!(
            config.contains("pub const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;"),
            "src/config.rs changed MAX_ARCHIVE_BYTES, so change MAX_ARCHIVE_BYTES in the signer"
        );
        assert_eq!(HASH_LEN, 8);
        assert_eq!(MAX_ARCHIVE_BYTES, 512 * 1024 * 1024);
    }

    #[test]
    fn refuses_a_manifest_version_that_the_client_refuses() {
        let error = check_manifest(&manifest("1.0.0 beta")).unwrap_err();
        assert!(
            error.to_string().contains("every client would refuse it"),
            "{error:#}"
        );
    }

    #[test]
    fn accepts_a_usable_manifest() {
        check_manifest(&manifest("0.2.0")).unwrap();
    }

    #[test]
    fn refuses_a_manifest_with_no_platform() {
        let text = serde_json::json!({ "version": "0.2.0", "platforms": {} }).to_string();
        let error = check_manifest(&text).unwrap_err();
        assert!(error.to_string().contains("names no platform"), "{error:#}");
    }

    #[test]
    fn refuses_a_platform_with_a_malformed_digest() {
        let samples = [
            String::from("abc"),
            "g".repeat(64),
            "a".repeat(63),
            "a".repeat(65),
        ];
        for sum in &samples {
            let text = serde_json::json!({
                "version": "0.2.0",
                "platforms": { "darwin-arm64": { "sha256": sum } },
            })
            .to_string();
            assert!(
                check_manifest(&text).is_err(),
                "accepted the digest {sum:?}"
            );
        }
    }

    #[test]
    fn refuses_a_content_release_past_the_client_limit() {
        check_content(&release(MAX_ARCHIVE_BYTES)).unwrap();

        let error = check_content(&release(MAX_ARCHIVE_BYTES + 1)).unwrap_err();
        assert!(
            error.to_string().contains("every client applies"),
            "{error:#}"
        );
    }

    #[test]
    fn refuses_a_content_release_that_carries_a_url() {
        let text = serde_json::json!({
            "hash": "abcd1234",
            "sha256": DIGEST,
            "size_bytes": 1024,
            "url": "https://example.invalid/content.zip",
        })
        .to_string();
        let error = check_content(&text).unwrap_err();
        assert!(error.to_string().contains("\"url\""), "{error:#}");
    }

    #[test]
    fn names_the_kind_of_a_payload() {
        assert_eq!(
            check_payload(&manifest("0.2.0")).unwrap(),
            "a software manifest"
        );
        assert_eq!(check_payload(&release(1024)).unwrap(), "a content release");
        assert!(check_payload("{\"a\":1}").is_err());
    }

    #[test]
    fn decodes_hexadecimal() {
        assert_eq!(decode_hex("00ff10AB").unwrap(), [0x00, 0xff, 0x10, 0xab]);
        assert!(decode_hex("0").is_err());
        assert!(decode_hex("0g").is_err());
    }

    #[test]
    fn a_key_signs_and_the_envelope_verifies() {
        let base = temp_dir("sign");
        let key_path = base.join("test.key");
        let manifest_path = base.join("manifest.json");
        let envelope_path = base.join("manifest.signed.json");
        let key = key_path.to_str().unwrap();

        keygen(&key_path).unwrap();
        std::fs::write(&manifest_path, manifest("0.2.0")).unwrap();
        sign(key, &manifest_path, &envelope_path).unwrap();

        let pair = load_key(key).unwrap();
        let public_hex = hex(pair.public_key().as_ref());
        verify(&envelope_path, &[public_hex.as_str()]).unwrap();

        // The public key of a key that never signed the envelope must fail.
        let other_pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let other = Ed25519KeyPair::from_pkcs8(other_pkcs8.as_ref()).unwrap();
        let other_hex = hex(other.public_key().as_ref());
        assert!(verify(&envelope_path, &[other_hex.as_str()]).is_err());

        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn keygen_creates_a_file_that_only_its_owner_reads() {
        use std::os::unix::fs::PermissionsExt;

        let base = temp_dir("mode");
        let key_path = base.join("test.key");

        keygen(&key_path).unwrap();

        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the key file has mode {mode:o}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn keygen_refuses_a_path_that_is_taken() {
        let base = temp_dir("taken");
        let key_path = base.join("test.key");
        std::fs::write(&key_path, "an earlier key").unwrap();

        let error = keygen(&key_path).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(&key_path).unwrap(),
            "an earlier key"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn keygen_refuses_a_symbolic_link() {
        let base = temp_dir("link");
        let elsewhere = base.join("elsewhere");
        let key_path = base.join("test.key");
        std::os::unix::fs::symlink(&elsewhere, &key_path).unwrap();

        assert!(keygen(&key_path).is_err());
        assert!(
            !elsewhere.exists(),
            "keygen wrote the key through the symbolic link"
        );
        std::fs::remove_dir_all(&base).ok();
    }
}
