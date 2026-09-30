// SPDX-License-Identifier: GPL-3.0-or-later

//! HTTP access to the content API and to the software API, and the two
//! requests of the outbox: one note sent, and the operator asked for.
//!
//! Text that the server sends can reach the terminal, a file under the root,
//! or Claude's context. [`printable`] removes its control characters first.

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

/// Largest answer to a note or to `whoami` that this client reads.
const MAX_ANSWER_BYTES: u64 = 64 * 1024;

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
///
/// It carries a sequence, which lets the client refuse a release that was
/// signed before the one it holds. A signature proves who made a release, and
/// not when.
#[derive(Debug, Clone, Deserialize)]
pub struct ContentRelease {
    /// Short name of the release, which is also its archive name.
    pub hash: String,
    /// SHA-256 of the archive, as 64 hexadecimal characters.
    pub sha256: String,
    /// Size of the archive in bytes.
    pub size_bytes: u64,
    /// Order of this release among all releases, higher for a later one.
    ///
    /// A release from a signer older than this field carries none.
    #[serde(default)]
    pub sequence: Option<u64>,
}

/// Reads the latest content release, and checks its signature.
///
/// The check runs over the served bytes before anything parses them, so no
/// JSON canonicalisation rule takes part in the security argument. A release
/// that no compiled-in key accepts stops here, and nothing downloads.
///
/// `reported` holds the headers in which the client reports itself: its
/// platform, its installed content, and the notes waiting. The server records
/// them as reported, and nothing it serves depends on them.
pub fn latest_release(config: &Config, reported: &[(&str, String)]) -> Result<ContentRelease> {
    let url = config.latest_url();
    let body = fetch_text_with(config, &url, crate::config::MAX_MANIFEST_BYTES, reported)?;

    let latest: Latest = serde_json::from_str(&body)
        .with_context(|| format!("{url} did not return a JSON object"))?;

    let (Some(payload), Some(signature)) = (latest.payload, latest.signature) else {
        bail!(
            "{url} returned no signed content release. This brainmaker installs only signed \
             content, and the server published an unsigned release. Publish it again with a \
             signature, or ask whoever runs the server to."
        );
    };

    crate::signature::verify_content(payload.as_bytes(), &signature)
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

/// Reads a URL as text, with a `sync` token.
///
/// The response stops at `limit` bytes.
pub fn fetch_text(config: &Config, url: &str, limit: u64) -> Result<String> {
    fetch_text_with(config, url, limit, &[])
}

/// Reads a URL as text, with a `sync` token and the extra `headers`.
fn fetch_text_with(
    config: &Config,
    url: &str,
    limit: u64,
    headers: &[(&str, String)],
) -> Result<String> {
    let agent = build_agent(TEXT_TIMEOUT);
    let mut request = agent.get(url);
    if let Some(token) = auth::bearer(config, auth::SCOPE_SYNC)? {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    for (name, value) in headers {
        request = request.header(*name, value);
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
/// On every failure after the file was created, the file is removed.
/// Returns the number of bytes written.
pub fn download(config: &Config, url: &str, dest: &Path, limit: u64) -> Result<u64> {
    let agent = build_agent(DOWNLOAD_TIMEOUT);
    let mut request = agent.get(url);
    if let Some(token) = auth::bearer(config, auth::SCOPE_SYNC)? {
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

    // The library stops the read one byte past `limit`, and it reports that as
    // an error. It fails the first read after its own limit is used up, even
    // when the body ends there, so the limit given to it is `limit + 1`. A body
    // of exactly `limit` bytes then succeeds, and a longer body fails. The code
    // below turns that failure into a message about size.
    let mut reader = response.body_mut().with_config().limit(limit + 1).reader();

    let written = match io::copy(&mut reader, &mut writer) {
        Ok(written) => written,
        Err(error) => {
            // Close the file before it is removed, so that no partial body stays.
            drop(writer);
            let _ = std::fs::remove_file(dest);
            if passed_the_limit(&error) {
                bail!("the body of {url} is larger than the limit of {limit} bytes");
            }
            return Err(error)
                .with_context(|| format!("cannot write the download to {}", dest.display()));
        }
    };

    writer
        .into_inner()
        .with_context(|| format!("cannot flush {}", dest.display()))
        .and_then(|file| {
            file.sync_all()
                .with_context(|| format!("cannot sync {}", dest.display()))
        })
        .inspect_err(|_| {
            let _ = std::fs::remove_file(dest);
        })?;

    if written == 0 {
        let _ = std::fs::remove_file(dest);
        bail!("the body of {url} is empty");
    }

    Ok(written)
}

/// What the server answered to one note: the status, and the body as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    pub body: String,
}

/// Sends one note, and returns the server's answer, whatever its status.
///
/// `token` is an `outbox:write` token, or `None` when no credential is
/// configured. The caller checks `name` first: it becomes a path segment. A
/// failure to reach the server is an error; every status is an answer.
pub fn post_note(config: &Config, name: &str, note: &[u8], token: Option<&str>) -> Result<Answer> {
    let url = config.outbox_url(name);
    let agent = build_agent(TEXT_TIMEOUT);
    let mut request = agent
        .post(&url)
        .header("Content-Type", "text/markdown; charset=utf-8")
        .header("Accept", "application/json");
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = request
        .send(note)
        .map_err(describe)
        .with_context(|| format!("cannot send the note to {url}"))?;
    let status = response.status().as_u16();
    // A body that cannot be read leaves the status, which decides what
    // happens to the note.
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_ANSWER_BYTES)
        .read_to_string()
        .unwrap_or_default();
    Ok(Answer { status, body })
}

/// Body of `GET {base}/whoami`. The server sends the client ID and the display
/// name too, and this client reads neither.
#[derive(Debug, Deserialize)]
struct Whoami {
    operator: Option<String>,
}

/// Asks the server which operator this client belongs to, with a `sync`
/// token. `None` means that the server names none: nobody registered the
/// client, or it is retired.
pub fn whoami(config: &Config) -> Result<Option<String>> {
    let url = config.whoami_url();
    let body = fetch_text(config, &url, MAX_ANSWER_BYTES)?;
    let parsed: Whoami = serde_json::from_str(&body)
        .with_context(|| format!("{url} did not return the expected JSON object"))?;
    Ok(parsed.operator)
}

/// Removes every control character from text that the server sent, and every
/// character that changes the direction of the text around it.
///
/// That text can reach the terminal, a file, or Claude's context. A control
/// character there could move the cursor, rewrite a line, or hide the rest.
pub fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

/// True when `error` is the HTTP client's report that the body passed the
/// limit it was given.
fn passed_the_limit(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ureq::Error>())
        .is_some_and(|inner| matches!(inner, ureq::Error::BodyExceedsLimit(_)))
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
            auth::SCOPE_SYNC
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
/// the operator. The result holds no newline and no control character, and it
/// stops at [`MAX_MESSAGE_CHARS`], because it is appended to a one-line error.
pub fn message_from_body(body: &str) -> Option<String> {
    let text = match serde_json::from_str::<ErrorBody>(body) {
        Ok(parsed) => match parsed.error_description {
            Some(description) => format!("{}: {}", parsed.error, description),
            None => parsed.error,
        },
        Err(_) => body.to_string(),
    };

    // The whitespace goes first, so a line break becomes a space rather than
    // nothing, and then every other control character goes.
    let collapsed = printable(&text.split_whitespace().collect::<Vec<_>>().join(" "));
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
    use crate::testutil::{Route, Server, Signer, temp_dir};

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
    fn a_message_holds_no_control_character() {
        let body = "{\"error\": \"bad \\u001b[2Jnote\\u202e txt\"}";
        assert_eq!(message_from_body(body).unwrap(), "bad [2Jnote txt");
        assert_eq!(printable("a\u{7}b\u{2066}c\u{85}d"), "abcd");
        assert_eq!(printable("città — ok"), "città — ok");
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

    /// A server whose `GET /content/latest` route answers with `body`.
    fn serving_latest(body: impl Into<Vec<u8>>) -> Server {
        Server::start(vec![Route::get("/content/latest", body)])
    }

    #[test]
    fn reads_a_signed_release() {
        let signer = Signer::new();
        signer.trust();
        let payload = release("25c60772", &"a".repeat(64), 1152003);
        let server = serving_latest(signer.envelope(&payload));
        let dir = temp_dir("remote-signed");

        let read = latest_release(&Config::for_test(&dir, &server.base()), &[]).unwrap();

        assert_eq!(read.hash, "25c60772");
        assert_eq!(read.sha256, "a".repeat(64));
        assert_eq!(read.size_bytes, 1152003);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_a_release_that_another_key_signed() {
        // The client trusts one key, and a second one signs.
        Signer::new().trust();
        let stranger = Signer::new();
        let payload = release("25c60772", &"a".repeat(64), 1152003);
        let server = serving_latest(stranger.envelope(&payload));
        let dir = temp_dir("remote-stranger");

        let error = latest_release(&Config::for_test(&dir, &server.base()), &[]).unwrap_err();

        let text = format!("{error:#}");
        assert!(
            text.contains("cannot trust the content release"),
            "got {text}"
        );
        assert!(
            text.contains("no signature from a key this binary trusts"),
            "got {text}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_a_release_whose_payload_was_altered() {
        let signer = Signer::new();
        signer.trust();
        let signed: serde_json::Value =
            serde_json::from_str(&signer.envelope(&release("25c60772", &"a".repeat(64), 1152003)))
                .unwrap();
        // A different release that carries the signature of the first one.
        let forged = serde_json::json!({
            "payload": release("25c60772", &"b".repeat(64), 1152003),
            "signature": signed["signature"],
        });
        let server = serving_latest(forged.to_string());
        let dir = temp_dir("remote-altered");

        let error = latest_release(&Config::for_test(&dir, &server.base()), &[]).unwrap_err();

        let text = format!("{error:#}");
        assert!(
            text.contains("cannot trust the content release"),
            "got {text}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_an_unsigned_release() {
        let server = serving_latest(r#"{"hash":"25c60772"}"#);
        let dir = temp_dir("remote-unsigned");

        let error = latest_release(&Config::for_test(&dir, &server.base()), &[]).unwrap_err();

        let text = format!("{error:#}");
        assert!(
            text.contains("returned no signed content release"),
            "got {text}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_a_release_that_claims_too_many_bytes() {
        let signer = Signer::new();
        signer.trust();
        let payload = release("25c60772", &"a".repeat(64), MAX_ARCHIVE_BYTES + 1);
        let server = serving_latest(signer.envelope(&payload));
        let dir = temp_dir("remote-too-large");

        let error = latest_release(&Config::for_test(&dir, &server.base()), &[]).unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("past the limit"), "got {text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_the_server_message_for_a_404() {
        let server = Server::start(vec![Route {
            status: 404,
            ..Route::get(
                "/content/latest",
                r#"{"error": "no content release is published"}"#,
            )
        }]);
        let dir = temp_dir("remote-404");

        let error = latest_release(&Config::for_test(&dir, &server.base()), &[]).unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("HTTP 404"), "got {text}");
        assert!(
            text.contains("no content release is published"),
            "got {text}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_download_writes_the_body_to_the_file() {
        let body: Vec<u8> = (0..1000u32).map(|n| (n % 251) as u8).collect();
        let server = Server::start(vec![Route::get("/f", body.clone())]);
        let dir = temp_dir("remote-download");
        let config = Config::for_test(&dir, &server.base());
        let dest = dir.join("f.bin");

        let written = download(&config, &format!("{}/f", server.base()), &dest, 2000).unwrap();

        assert_eq!(written, 1000);
        assert_eq!(std::fs::read(&dest).unwrap(), body);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_download_past_the_limit_fails_and_leaves_no_usable_file() {
        let server = Server::start(vec![Route::get("/f", vec![7u8; 1000])]);
        let dir = temp_dir("remote-download-limit");
        let config = Config::for_test(&dir, &server.base());
        let dest = dir.join("f.bin");

        let result = download(&config, &format!("{}/f", server.base()), &dest, 999);

        // The message is the subject of `a_download_past_the_limit_names_the_limit`.
        // Here the file must be gone, so that a caller that does not remove it
        // after an error leaves no oversized body behind.
        assert!(
            result.is_err(),
            "a body of 1000 bytes passed a limit of 999"
        );
        assert!(!dest.exists(), "the oversized body stayed in the file");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_download_past_the_limit_names_the_limit() {
        let server = Server::start(vec![Route::get("/f", vec![7u8; 1000])]);
        let dir = temp_dir("remote-download-names-limit");
        let config = Config::for_test(&dir, &server.base());
        let dest = dir.join("f.bin");

        let error = download(&config, &format!("{}/f", server.base()), &dest, 999).unwrap_err();

        let text = format!("{error:#}");
        assert!(
            text.contains("is larger than the limit of 999 bytes"),
            "got {text}"
        );
        assert!(!dest.exists(), "the oversized body stayed in the file");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_download_of_exactly_the_limit_succeeds() {
        let server = Server::start(vec![Route::get("/f", vec![7u8; 1000])]);
        let dir = temp_dir("remote-download-exact-limit");
        let config = Config::for_test(&dir, &server.base());
        let dest = dir.join("f.bin");

        let written = download(&config, &format!("{}/f", server.base()), &dest, 1000).unwrap();

        assert_eq!(written, 1000);
        assert_eq!(std::fs::read(&dest).unwrap(), vec![7u8; 1000]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_download_fails() {
        let server = Server::start(vec![Route::get("/f", Vec::new())]);
        let dir = temp_dir("remote-download-empty");
        let config = Config::for_test(&dir, &server.base());
        let dest = dir.join("f.bin");

        let error = download(&config, &format!("{}/f", server.base()), &dest, 2000).unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("is empty"), "got {text}");
        assert!(!dest.exists(), "the empty body stayed in the file");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
