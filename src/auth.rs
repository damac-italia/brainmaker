// SPDX-License-Identifier: GPL-3.0-or-later

//! The OAuth2 client-credentials grant that authorises every API request.
//!
//! # Why this exists
//!
//! Earlier versions sent one static token that never expired. A copy of that
//! token, taken from one laptop, stayed valid until somebody noticed and
//! revoked it by hand.
//!
//! The client now holds a client identifier and a client secret, and exchanges
//! them for a token that the server expires after 10 minutes. A stolen token is
//! useful for those 10 minutes. A stolen secret is still valuable, so it lives
//! sealed in the same store as before. See [`crate::secretstore`].
//!
//! # The exchange
//!
//! ```text
//! POST {jwt_endpoint}/{token route}
//! Authorization: Basic base64(client_id:client_secret)
//! Content-Type: application/x-www-form-urlencoded
//!
//! grant_type=client_credentials&scope=sync
//! ```
//!
//! The route defaults to `oauth2/token`, and `SWETSI_TOKEN_PATH` overrides it.
//! [`crate::config::Config::token_url`] builds the whole URL.
//!
//! brainmaker names one scope in each request rather than relying on a
//! default: `sync` to read content and software, `outbox:write` to send a
//! note, and `outbox:read` for the admin commands. A sync token therefore
//! never carries the right to write, and a client whose issuer grants no
//! `outbox:write` still syncs. The issuer answers a scope it does not grant
//! with `invalid_scope`, which [`InvalidScope`] carries.
//!
//! `link` asks for the two outbox scopes once, through [`grants`], to learn
//! whether the credential is the admin's. [`crate::link`] says what it does
//! with the answer.
//!
//! # The cache
//!
//! One run makes several requests, so each token is fetched once and reused.
//! The cache lives in [`TokenCache`], which one [`crate::config::Config`] owns,
//! and it holds one token per scope. It never reaches the disk: a run that
//! ends throws the tokens away.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cause::{self, Cause};
use crate::config::{Config, Credentials};
use crate::remote;

/// The scope that a sync needs. brainmaker asks for it explicitly.
pub const SCOPE_SYNC: &str = "sync";

/// The scope that sending a note needs. brainmaker asks for it only when a
/// note waits in the outbox.
pub const SCOPE_OUTBOX_WRITE: &str = "outbox:write";

/// The scope that the admin commands need: reading the notes, the fleet view,
/// and the sync log. Besides them, only `link` asks for it, to learn the role.
pub const SCOPE_OUTBOX_READ: &str = "outbox:read";

/// Lifetime we assume when the response omits `expires_in`. The server issues a
/// token that lasts 10 minutes.
const DEFAULT_LIFETIME: Duration = Duration::from_secs(600);

/// How long before the server's expiry we stop using a token.
///
/// A request that starts one millisecond before the expiry would otherwise
/// arrive with a token that the server has already rejected.
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);

/// Longest lifetime we accept from the server.
///
/// The value reaches us from the network and is added to a clock reading. One
/// hour is six times what the server issues, and it keeps that sum far from an
/// overflow.
const MAX_LIFETIME: Duration = Duration::from_secs(3600);

/// Longest token we accept. A token longer than this is a server fault.
const MAX_TOKEN_LEN: usize = 8192;

/// Largest token response we read.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

/// The issuer answered that this client may not ask for `scope`.
///
/// A caller finds it with `downcast_ref` on the error that [`bearer`] returns,
/// and treats it as a setting of the issuer rather than as a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidScope {
    pub scope: String,
}

impl std::fmt::Display for InvalidScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the token endpoint does not grant this client the scope {}",
            self.scope
        )
    }
}

impl std::error::Error for InvalidScope {}

/// True when `error` says that the issuer does not grant a scope.
pub fn is_invalid_scope(error: &anyhow::Error) -> bool {
    error.downcast_ref::<InvalidScope>().is_some()
}

/// Body of a successful `POST {jwt_endpoint}/oauth2/token`.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Seconds until the server expires the token.
    expires_in: Option<u64>,
    token_type: Option<String>,
}

/// One token, and the moment it stops being usable.
struct Cached {
    token: String,
    usable_until: Instant,
}

/// The access tokens for one run, one per scope.
///
/// The cache holds secrets, so its [`std::fmt::Debug`] output names no value.
#[derive(Default)]
pub struct TokenCache {
    inner: Mutex<BTreeMap<String, Cached>>,
}

impl std::fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self.inner.lock() {
            Ok(guard) if guard.is_empty() => "empty".to_string(),
            Ok(guard) => format!(
                "held for {}",
                guard.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
            Err(_) => "poisoned".to_string(),
        };
        write!(f, "TokenCache({state})")
    }
}

impl TokenCache {
    /// Returns the cached token for `scope` while it stays usable.
    fn get(&self, scope: &str) -> Option<String> {
        let guard = self.inner.lock().ok()?;
        let cached = guard.get(scope)?;
        if Instant::now() < cached.usable_until {
            return Some(cached.token.clone());
        }
        None
    }

    /// Replaces the cached token for `scope`.
    ///
    /// A lifetime that the clock cannot hold stores nothing, so the next
    /// request asks for a new token.
    fn put(&self, scope: &str, token: &str, lifetime: Duration) {
        let Some(usable_until) = Instant::now().checked_add(lifetime) else {
            return;
        };
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(
                scope.to_string(),
                Cached {
                    token: token.to_string(),
                    usable_until,
                },
            );
        }
    }

    /// True when this run received a token for `scope`, usable or not.
    fn received(&self, scope: &str) -> bool {
        self.inner
            .lock()
            .is_ok_and(|guard| guard.contains_key(scope))
    }
}

/// Returns the bearer token for `scope`, for the next request.
///
/// Returns `Ok(None)` when no credential is configured, which leaves the
/// request without an `Authorization` header. An issuer that does not grant
/// the scope gives an error that holds [`InvalidScope`].
pub fn bearer(config: &Config, scope: &str) -> Result<Option<String>> {
    let (Some(credentials), Some(url)) = (config.credentials(), config.token_url()) else {
        return Ok(None);
    };

    if let Some(token) = config.tokens().get(scope) {
        return Ok(Some(token));
    }

    let (token, lifetime) = request_token(credentials, &url, scope)
        .with_context(|| format!("cannot get an access token from {url}"))?;
    config.tokens().put(scope, &token, lifetime);
    Ok(Some(token))
}

/// True when this run received a token for `scope` from the issuer.
///
/// A sync token proves that the issuer answered this run, so the steps after
/// the content step ask this before they make a request of their own.
pub fn received(config: &Config, scope: &str) -> bool {
    config.tokens().received(scope)
}

/// True when the issuer gives this client a token for `scope`.
///
/// An `invalid_scope` answer is false, and so is a run with no credential,
/// which sends its requests with no token at all. Any other failure is an
/// error, because it says nothing about what the issuer grants.
pub fn grants(config: &Config, scope: &str) -> Result<bool> {
    match bearer(config, scope) {
        Ok(token) => Ok(token.is_some()),
        Err(error) if is_invalid_scope(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Returns the time for which this client uses a token.
///
/// `expires_in` is the server's own value, in seconds. The result is never
/// longer than [`MAX_LIFETIME`], and it is [`EXPIRY_MARGIN`] shorter than the
/// bounded value.
fn usable_lifetime(expires_in: Option<u64>) -> Duration {
    expires_in
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_LIFETIME)
        .min(MAX_LIFETIME)
        .saturating_sub(EXPIRY_MARGIN)
}

/// Exchanges the client credentials for a token.
///
/// Returns the token and the time for which this client will use it. That time
/// comes from [`usable_lifetime`]: the server's value, cut to at most
/// [`MAX_LIFETIME`], less [`EXPIRY_MARGIN`].
fn request_token(credentials: &Credentials, url: &str, scope: &str) -> Result<(String, Duration)> {
    let agent = remote::build_agent(remote::TEXT_TIMEOUT);

    let mut response = agent
        .post(url)
        .header("Authorization", credentials.basic_header())
        .header("Accept", "application/json")
        .send_form([("grant_type", "client_credentials"), ("scope", scope)])
        .map_err(describe)?;

    // Read the body before the status is judged: an OAuth2 failure carries its
    // reason in the body, and the agent hands a non-2xx response over intact.
    let status = response.status();
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .context("cannot read the token response body")?;

    if !status.is_success() {
        return Err(reject(status.as_u16(), &body, scope));
    }

    let parsed: TokenResponse = serde_json::from_str(&body)
        .context("the token endpoint did not return the expected JSON object")?;

    if let Some(kind) = parsed.token_type.as_deref()
        && !kind.eq_ignore_ascii_case("bearer")
    {
        bail!("the token endpoint returned the token type {kind:?}, and brainmaker sends Bearer");
    }

    let token = parsed.access_token;
    check_token(&token)?;

    let lifetime = usable_lifetime(parsed.expires_in);

    Ok((token, lifetime))
}

/// Fails for a token that we must not put in a header.
///
/// The token reaches us from the network and goes straight into an
/// `Authorization` header, so a carriage return or a line feed inside it would
/// let the server write further headers.
fn check_token(token: &str) -> Result<()> {
    if token.is_empty() {
        bail!("the token endpoint returned an empty access_token");
    }
    if token.len() > MAX_TOKEN_LEN {
        bail!(
            "the token endpoint returned an access_token of {} bytes, \
             larger than the limit of {MAX_TOKEN_LEN} bytes",
            token.len()
        );
    }
    if !token.chars().all(|c| matches!(c, ' '..='~')) {
        bail!("the token endpoint returned an access_token with a character we cannot send");
    }
    Ok(())
}

/// Turns a non-2xx token response into a message that names the cause.
///
/// An OAuth2 endpoint answers a failure as `{"error": "invalid_client"}`, and
/// it may add an `error_description`. That body says which half of the
/// credential the server objected to, so it is appended to the guidance. An
/// `invalid_scope` answer becomes [`InvalidScope`], which a caller can find.
fn reject(code: u16, body: &str, scope: &str) -> anyhow::Error {
    if (code == 400 || code == 401) && oauth_error(body).as_deref() == Some("invalid_scope") {
        return anyhow::Error::new(InvalidScope {
            scope: scope.to_string(),
        });
    }
    let headline = match code {
        400 | 401 => format!(
            "the token endpoint rejected the client credentials with HTTP {code}; \
             check {} and {}, and check that the client may ask for the scope {scope}",
            crate::config::CLIENT_ID_ENV,
            crate::config::CLIENT_SECRET_ENV
        ),
        404 => format!(
            "the token endpoint returned HTTP 404 Not Found; check {} and {}",
            crate::config::JWT_ENDPOINT_ENV,
            crate::provision::KEY_TOKEN_PATH
        ),
        _ => format!("the token endpoint returned HTTP {code}"),
    };

    // A report names the cause as a word: the issuer refused the credentials,
    // or it answered some other status.
    let cause = match code {
        400 | 401 => Cause::Credentials,
        _ => Cause::Http,
    };
    let text = match remote::message_from_body(body) {
        Some(message) => format!("{headline}: {message}"),
        None => headline,
    };
    cause::failed(cause, Some(code), text)
}

/// The `error` code of an OAuth2 error body, such as `invalid_scope`.
fn oauth_error(body: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct OAuthError {
        error: String,
    }
    serde_json::from_str::<OAuthError>(body)
        .ok()
        .map(|parsed| parsed.error)
}

/// Turns a ureq error from the token endpoint into a message that names the
/// cause.
///
/// A status no longer reaches here: the agent takes one as a value, and
/// [`reject`] reports it. What is left is a transport failure.
fn describe(error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::StatusCode(code) => anyhow::anyhow!("the token endpoint returned HTTP {code}"),
        ureq::Error::Timeout(_) => remote::unanswered("the token request timed out"),
        ureq::Error::HostNotFound => {
            remote::unanswered("cannot resolve the token endpoint host name")
        }
        other => anyhow::anyhow!(other),
    }
}

/// Encodes bytes as standard base64, with padding.
///
/// HTTP Basic needs this one encoding and nothing else, so brainmaker carries
/// these 20 lines instead of a dependency.
pub fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Route, Server, temp_dir};

    #[test]
    fn encodes_the_base64_test_vectors() {
        // RFC 4648, section 10.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn encodes_bytes_that_are_not_ascii() {
        assert_eq!(base64(&[0xff, 0xef, 0xbf]), "/++/");
        assert_eq!(base64(&[0x00, 0x00, 0x00]), "AAAA");
    }

    #[test]
    fn accepts_a_plain_token() {
        check_token("abc.def-ghi_jkl=").unwrap();
    }

    #[test]
    fn rejects_a_token_that_can_write_a_header() {
        assert!(check_token("abc\r\nX-Injected: 1").is_err());
        assert!(check_token("abc\ndef").is_err());
        assert!(check_token("abc\tdef").is_err());
        assert!(check_token("").is_err());
        assert!(check_token(&"a".repeat(MAX_TOKEN_LEN + 1)).is_err());
    }

    #[test]
    fn the_cache_returns_a_token_that_is_still_usable() {
        let cache = TokenCache::default();
        assert_eq!(cache.get(SCOPE_SYNC), None);

        cache.put(SCOPE_SYNC, "a-token", Duration::from_secs(60));
        assert_eq!(cache.get(SCOPE_SYNC).as_deref(), Some("a-token"));
    }

    #[test]
    fn the_cache_keeps_one_token_per_scope() {
        let cache = TokenCache::default();
        cache.put(SCOPE_SYNC, "sync-token", Duration::from_secs(60));
        assert_eq!(cache.get(SCOPE_OUTBOX_WRITE), None);
        assert!(!cache.received(SCOPE_OUTBOX_WRITE));

        cache.put(SCOPE_OUTBOX_WRITE, "write-token", Duration::from_secs(60));
        assert_eq!(cache.get(SCOPE_SYNC).as_deref(), Some("sync-token"));
        assert_eq!(
            cache.get(SCOPE_OUTBOX_WRITE).as_deref(),
            Some("write-token")
        );
    }

    #[test]
    fn the_cache_drops_a_token_that_expired() {
        let cache = TokenCache::default();
        cache.put(SCOPE_SYNC, "a-token", Duration::from_secs(0));
        assert_eq!(cache.get(SCOPE_SYNC), None);
        // The run still received it, so the issuer answered.
        assert!(cache.received(SCOPE_SYNC));
    }

    #[test]
    fn the_cache_debug_output_never_shows_the_token() {
        let cache = TokenCache::default();
        cache.put(SCOPE_SYNC, "super-secret-value", Duration::from_secs(60));
        let shown = format!("{cache:?}");
        assert!(!shown.contains("super-secret-value"), "got {shown}");
        assert_eq!(shown, "TokenCache(held for sync)");
    }

    #[test]
    fn bounds_the_lifetime_that_the_server_names() {
        assert_eq!(
            usable_lifetime(Some(u64::MAX)),
            MAX_LIFETIME - EXPIRY_MARGIN
        );
        assert_eq!(
            usable_lifetime(Some(600)),
            Duration::from_secs(600) - EXPIRY_MARGIN
        );
        assert_eq!(usable_lifetime(None), DEFAULT_LIFETIME - EXPIRY_MARGIN);
        // A lifetime shorter than the margin is zero, not negative.
        assert_eq!(usable_lifetime(Some(5)), Duration::ZERO);
    }

    #[test]
    fn a_lifetime_the_clock_cannot_hold_caches_nothing() {
        let cache = TokenCache::default();
        cache.put(SCOPE_SYNC, "token", Duration::MAX);
        assert!(cache.get(SCOPE_SYNC).is_none());
    }

    #[test]
    fn no_endpoint_is_compiled_into_this_module() {
        // The distributed binary must disclose no customer endpoint.
        let source = include_str!("auth.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("auth.rs has a non-test section");

        for scheme in ["https://", "http://"] {
            for (index, _) in production.match_indices(scheme) {
                let next = production[index + scheme.len()..].chars().next();
                assert!(
                    !next.is_some_and(|c| c.is_ascii_alphanumeric()),
                    "auth.rs must hold no URL with a host outside its tests"
                );
            }
        }
    }

    /// A server whose token route answers with `status` and `body`.
    fn serving_token(status: u16, body: &str) -> Server {
        Server::start(vec![Route {
            status,
            ..Route::post("/oauth2/token", body)
        }])
    }

    #[test]
    fn exchanges_the_credentials_for_a_token() {
        let server = serving_token(
            200,
            r#"{"access_token":"abc","expires_in":600,"token_type":"Bearer"}"#,
        );
        let dir = temp_dir("auth-exchange");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let token = bearer(&config, SCOPE_SYNC).unwrap();

        assert_eq!(token, Some("abc".to_string()));
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let (method, path, authorization) = &requests[0];
        assert_eq!(method, "POST");
        assert_eq!(path, "/oauth2/token");
        assert!(authorization.starts_with("Basic "), "got {authorization}");
        // The base64 of "the-client-id:the-client-secret", which the system
        // base64 tool computed, so that the wire format does not rest on this
        // module's own encoder.
        assert_eq!(
            authorization,
            "Basic dGhlLWNsaWVudC1pZDp0aGUtY2xpZW50LXNlY3JldA=="
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn asks_for_one_token_per_run() {
        let server = serving_token(200, r#"{"access_token":"abc","expires_in":600}"#);
        let dir = temp_dir("auth-cache");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        assert_eq!(
            bearer(&config, SCOPE_SYNC).unwrap(),
            Some("abc".to_string())
        );
        assert_eq!(
            bearer(&config, SCOPE_SYNC).unwrap(),
            Some("abc".to_string())
        );

        assert_eq!(server.requests().len(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_a_token_type_that_is_not_bearer() {
        let server = serving_token(
            200,
            r#"{"access_token":"abc","expires_in":600,"token_type":"mac"}"#,
        );
        let dir = temp_dir("auth-token-type");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let error = bearer(&config, SCOPE_SYNC).unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("token type"), "got {text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_a_token_that_holds_a_line_break() {
        // The JSON escapes decode to a carriage return and a line feed, which
        // would let the server write a header of its own.
        let server = serving_token(200, r#"{"access_token":"a\r\nX-Injected: 1"}"#);
        let dir = temp_dir("auth-line-break");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let error = bearer(&config, SCOPE_SYNC).unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("cannot send"), "got {text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_the_reason_that_the_token_endpoint_gives() {
        let server = serving_token(
            401,
            r#"{"error":"invalid_client","error_description":"unknown client"}"#,
        );
        let dir = temp_dir("auth-rejected");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let error = bearer(&config, SCOPE_SYNC).unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("invalid_client"), "got {text}");
        assert!(text.contains("unknown client"), "got {text}");
        assert!(!text.contains("the-client-secret"), "got {text}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sends_no_header_when_no_credential_is_configured() {
        let server = Server::start(vec![Route::get("/x", "hello")]);
        let dir = temp_dir("auth-none");
        let config = Config::for_test(&dir, &server.base());

        assert_eq!(bearer(&config, SCOPE_SYNC).unwrap(), None);
        remote::fetch_text(&config, &format!("{}/x", server.base()), 1024).unwrap();

        // One request, the one for the route, with no Authorization header.
        assert_eq!(
            server.requests(),
            vec![("GET".to_string(), "/x".to_string(), String::new())]
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn grants_a_scope_that_the_issuer_answers_with_a_token() {
        let server = Server::start(vec![
            Route::token(
                SCOPE_OUTBOX_READ,
                r#"{"access_token":"r","expires_in":600}"#,
            ),
            Route::token(SCOPE_OUTBOX_WRITE, r#"{"error":"invalid_scope"}"#).status(400),
        ]);
        let dir = temp_dir("auth-grants");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        assert!(grants(&config, SCOPE_OUTBOX_READ).unwrap());
        assert!(!grants(&config, SCOPE_OUTBOX_WRITE).unwrap());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_credential_grants_nothing_and_asks_nobody() {
        let server = Server::start(vec![]);
        let dir = temp_dir("auth-grants-none");
        let config = Config::for_test(&dir, &server.base());

        assert!(!grants(&config, SCOPE_OUTBOX_READ).unwrap());
        assert!(server.requests().is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_token_request_is_an_error_and_not_a_refusal() {
        let server = serving_token(500, "the issuer is down");
        let dir = temp_dir("auth-grants-down");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        assert!(grants(&config, SCOPE_OUTBOX_READ).is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
