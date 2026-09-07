// SPDX-License-Identifier: GPL-3.0-or-later

//! HTTP access to the content API and to the software API.

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::auth;
use crate::config::{Config, MAX_ARCHIVE_BYTES, validate_hash};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const TEXT_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const USER_AGENT: &str = concat!("brainmaker/", env!("CARGO_PKG_VERSION"));

/// Largest error body this client reads before it gives up on a message.
const MAX_ERROR_BODY: u64 = 4 * 1024;

/// Longest server message this client prints, in characters.
const MAX_MESSAGE_CHARS: usize = 200;

/// Body that both APIs return on a failure.
///
/// Synapsis answers `{"error": "..."}`. An OAuth2 endpoint answers
/// `{"error": "invalid_client"}`, and it may add an `error_description`.
#[derive(Debug, Deserialize)]
struct ErrorBody {
    error: String,
    error_description: Option<String>,
}

/// Body of `GET {base}/content/latest`.
///
/// `payload` and `signature` form the same envelope the software manifest
/// uses, and the signature covers exactly the `payload` bytes. The route also
/// carries a bare `hash`, which a client older than the signing change reads;
/// this client ignores it and takes the hash from the signed payload, so a
/// server cannot point it at one archive while signing another.
#[derive(Debug, Deserialize)]
struct Latest {
    payload: Option<String>,
    signature: Option<String>,
}

/// The signed description of one content release.
///
/// It carries no URL, for the same reason the software manifest carries none:
/// the client derives the download address from its own base URL, so a signed
/// document cannot move the download to another host.
#[derive(Debug, Clone, Deserialize)]
pub struct ContentRelease {
    /// Short name of the release, which is also its archive name.
    pub hash: String,
    /// SHA-256 of the archive, as 64 hexadecimal characters.
    pub sha256: String,
    /// Size of the archive in bytes.
    pub size_bytes: u64,
}

/// Reads the latest content release, and checks its signature.
///
/// The check runs over the served bytes before anything parses them, so no
/// JSON canonicalisation rule takes part in the security argument. A release
/// that no compiled-in key accepts stops here, and nothing downloads.
pub fn latest_release(config: &Config) -> Result<ContentRelease> {
    let url = config.latest_url();
    let body = fetch_text(config, &url, crate::config::MAX_MANIFEST_BYTES)?;

    let latest: Latest = serde_json::from_str(&body)
        .with_context(|| format!("{url} did not return a JSON object"))?;

    let (Some(payload), Some(signature)) = (latest.payload, latest.signature) else {
        bail!(
            "{url} returned no signed content release. This brainmaker installs only signed \
             content, and the server published an unsigned release. Publish it again with a \
             signature, or ask whoever runs the server to."
        );
    };

    crate::signature::verify(payload.as_bytes(), &signature)
        .with_context(|| format!("cannot trust the content release from {url}"))?;

    let release: ContentRelease = serde_json::from_str(&payload)
        .with_context(|| format!("the signed content release from {url} is not usable"))?;

    validate_hash(&release.hash)?;
    crate::digest::checked_sha256(&release.sha256, "the content release")?;
    if release.size_bytes == 0 {
        bail!("the content release claims a size of 0 bytes");
    }
    if release.size_bytes > MAX_ARCHIVE_BYTES {
        bail!(
            "the content release claims {} bytes, past the limit of {MAX_ARCHIVE_BYTES}",
            release.size_bytes
        );
    }

    Ok(release)
}

/// Downloads the archive for `hash` to `dest`.
///
/// The download streams to disk, and it stops at [`MAX_ARCHIVE_BYTES`].
/// Returns the number of bytes written.
pub fn download_archive(config: &Config, hash: &str, dest: &Path) -> Result<u64> {
    validate_hash(hash)?;
    let url = config.archive_url(hash);
    download(config, &url, dest, MAX_ARCHIVE_BYTES)
}

/// Reads a URL as text.
///
/// The response stops at `limit` bytes.
pub fn fetch_text(config: &Config, url: &str, limit: u64) -> Result<String> {
    let agent = build_agent(TEXT_TIMEOUT);
    let mut request = agent.get(url);
    if let Some(token) = auth::bearer(config)? {
        request = request.header("Authorization", format!("Bearer {token}"));
    }

    let mut response = request
        .call()
        .map_err(describe)
        .with_context(|| format!("cannot read {url}"))?;

    check_status(&mut response).with_context(|| format!("cannot read {url}"))?;

    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_string()
        .with_context(|| format!("cannot read the response body from {url}"))
}

/// Streams a URL to `dest`.
///
/// The download stops at `limit` bytes, and it fails when the body is larger.
/// Returns the number of bytes written.
pub fn download(config: &Config, url: &str, dest: &Path, limit: u64) -> Result<u64> {
    let agent = build_agent(DOWNLOAD_TIMEOUT);
    let mut request = agent.get(url);
    if let Some(token) = auth::bearer(config)? {
        request = request.header("Authorization", format!("Bearer {token}"));
    }

    let mut response = request
        .call()
        .map_err(describe)
        .with_context(|| format!("cannot download {url}"))?;

    // Before the file is created, so that an error body never lands in `dest`.
    check_status(&mut response).with_context(|| format!("cannot download {url}"))?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create the directory {}", parent.display()))?;
    }

    let file = File::create(dest).with_context(|| format!("cannot create {}", dest.display()))?;
    let mut writer = BufWriter::new(file);

    // Read one byte past the limit so that an oversized body fails instead of
    // being silently truncated.
    let mut reader = response.body_mut().with_config().limit(limit + 1).reader();

    let written = io::copy(&mut reader, &mut writer)
        .with_context(|| format!("cannot write the download to {}", dest.display()))?;

    writer
        .into_inner()
        .with_context(|| format!("cannot flush {}", dest.display()))?
        .sync_all()
        .with_context(|| format!("cannot sync {}", dest.display()))?;

    if written > limit {
        let _ = std::fs::remove_file(dest);
        bail!("the body of {url} is larger than the limit of {limit} bytes");
    }
    if written == 0 {
        let _ = std::fs::remove_file(dest);
        bail!("the body of {url} is empty");
    }

    Ok(written)
}

/// Builds an agent with this client's timeouts and user agent.
pub fn build_agent(total_timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(total_timeout))
        .user_agent(USER_AGENT)
        // Hand a non-2xx response back as a value rather than an error. ureq
        // drops the body when it makes the status an error, and that body is
        // the only place the server says why. See `check_status`, and see
        // `auth::request_token`, which reads the same agent's failures.
        .http_status_as_error(false)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Fails when the response carries a status outside 2xx.
///
/// The message names the status and, when the server sent one, the server's
/// own explanation. A 404 from the content route then reads
/// `no content release is published` rather than the bare status, which tells
/// an operator that the server is reachable and simply empty.
fn check_status(response: &mut ureq::http::Response<ureq::Body>) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }

    let code = status.as_u16();
    let headline = match code {
        401 | 403 => format!(
            "the server rejected the request with HTTP {code}; check that {} is configured, and \
             that the client may read this route with the scope {}",
            crate::config::CLIENT_ID_ENV,
            auth::SCOPE
        ),
        404 => "the server returned HTTP 404 Not Found".to_string(),
        _ => format!("the server returned HTTP {code}"),
    };

    match server_message(response) {
        Some(message) => bail!("{headline}: {message}"),
        None => bail!("{headline}"),
    }
}

/// Reads the message a failed response carries, if it carries one.
fn server_message(response: &mut ureq::http::Response<ureq::Body>) -> Option<String> {
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_ERROR_BODY)
        .read_to_string()
        .ok()?;
    message_from_body(&body)
}

/// Pulls a one-line message out of an error body.
///
/// A body that parses as [`ErrorBody`] yields its fields. Anything else is
/// returned as it stands, so a proxy's plain-text or HTML page still reaches
/// the operator. The result holds no newline and stops at
/// [`MAX_MESSAGE_CHARS`], because it is appended to a one-line error.
pub fn message_from_body(body: &str) -> Option<String> {
    let text = match serde_json::from_str::<ErrorBody>(body) {
        Ok(parsed) => match parsed.error_description {
            Some(description) => format!("{}: {}", parsed.error, description),
            None => parsed.error,
        },
        Err(_) => body.to_string(),
    };

    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }

    if collapsed.chars().count() > MAX_MESSAGE_CHARS {
        let head: String = collapsed.chars().take(MAX_MESSAGE_CHARS).collect();
        return Some(format!("{head}..."));
    }
    Some(collapsed)
}

/// Turns a ureq error into a message that names the cause.
///
/// A status no longer reaches here: the agent takes one as a value, and
/// [`check_status`] reports it. What is left is a transport failure.
fn describe(error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::StatusCode(code) => anyhow::anyhow!("the server returned HTTP {code}"),
        ureq::Error::Timeout(_) => anyhow::anyhow!("the request timed out"),
        ureq::Error::HostNotFound => anyhow::anyhow!("cannot resolve the host name"),
        other => anyhow::anyhow!(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A release the client would accept, as JSON.
    fn release(hash: &str, sha256: &str, size: u64) -> String {
        format!(r#"{{"hash":"{hash}","sha256":"{sha256}","size_bytes":{size}}}"#)
    }

    #[test]
    fn reads_a_well_formed_content_release() {
        let text = release("25c60772", &"a".repeat(64), 1152003);
        let parsed: ContentRelease = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.hash, "25c60772");
        assert_eq!(parsed.size_bytes, 1152003);
    }

    #[test]
    fn a_release_carries_no_url() {
        // The client derives the address from its own base URL. An extra field
        // is ignored rather than trusted, which this pins.
        let text = r#"{"hash":"25c60772","sha256":"aa","size_bytes":1,"url":"http://evil"}"#;
        let parsed: ContentRelease = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.hash, "25c60772");
    }

    #[test]
    fn the_latest_body_needs_both_envelope_fields() {
        let both: Latest =
            serde_json::from_str(r#"{"hash":"25c60772","payload":"{}","signature":"ab"}"#).unwrap();
        assert!(both.payload.is_some() && both.signature.is_some());

        // The shape an older server returns. Both fields are absent, and
        // `latest_release` refuses it rather than installing unsigned content.
        let old: Latest = serde_json::from_str(r#"{"hash":"25c60772"}"#).unwrap();
        assert!(old.payload.is_none() && old.signature.is_none());
    }

    #[test]
    fn reads_the_error_field_that_synapsis_returns() {
        let body = r#"{"error": "no content release is published"}"#;
        assert_eq!(
            message_from_body(body).unwrap(),
            "no content release is published"
        );
    }

    #[test]
    fn joins_the_two_fields_that_an_oauth2_endpoint_returns() {
        let body = r#"{"error": "invalid_client", "error_description": "unknown client"}"#;
        assert_eq!(
            message_from_body(body).unwrap(),
            "invalid_client: unknown client"
        );
    }

    #[test]
    fn returns_a_body_that_is_not_the_expected_json() {
        let body = "<html><body>502 Bad Gateway</body></html>";
        assert_eq!(message_from_body(body).unwrap(), body);
    }

    #[test]
    fn collapses_a_body_onto_one_line() {
        let body = "502 Bad Gateway\n\nnginx\n";
        assert_eq!(message_from_body(body).unwrap(), "502 Bad Gateway nginx");
    }

    #[test]
    fn returns_nothing_for_a_body_that_holds_no_text() {
        assert!(message_from_body("").is_none());
        assert!(message_from_body("   \n\t ").is_none());
        assert!(message_from_body(r#"{"error": "  "}"#).is_none());
    }

    #[test]
    fn stops_a_long_body_at_the_limit() {
        let body = "x".repeat(MAX_MESSAGE_CHARS * 2);
        let message = message_from_body(&body).unwrap();
        assert_eq!(message.chars().count(), MAX_MESSAGE_CHARS + 3);
        assert!(message.ends_with("..."));
    }

    #[test]
    fn counts_characters_rather_than_bytes_when_it_truncates() {
        let body = "é".repeat(MAX_MESSAGE_CHARS + 10);
        let message = message_from_body(&body).unwrap();
        assert_eq!(message.chars().count(), MAX_MESSAGE_CHARS + 3);
    }
}
