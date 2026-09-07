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
//! brainmaker names the scope in the request rather than relying on a default,
//! so a client that the server grants more than one scope still asks for the
//! one scope that a sync needs.
//!
//! # The cache
//!
//! One run makes up to four requests, so the token is fetched once and reused.
//! The cache lives in [`TokenCache`], which one [`crate::config::Config`] owns.
//! It never reaches the disk: a run that ends throws the token away.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::{Config, Credentials};
use crate::remote;

/// The scope that a sync needs. brainmaker asks for it explicitly.
pub const SCOPE: &str = "sync";

/// Lifetime we assume when the response omits `expires_in`. The server issues a
/// token that lasts 10 minutes.
const DEFAULT_LIFETIME: Duration = Duration::from_secs(600);

/// How long before the server's expiry we stop using a token.
///
/// A request that starts one millisecond before the expiry would otherwise
/// arrive with a token that the server has already rejected.
const EXPIRY_MARGIN: Duration = Duration::from_secs(30);

/// Longest token we accept. A token longer than this is a server fault.
const MAX_TOKEN_LEN: usize = 8192;

/// Largest token response we read.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

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

/// The access token for one run.
///
/// The cache holds a secret, so its [`std::fmt::Debug`] output names no value.
#[derive(Default)]
pub struct TokenCache {
    inner: Mutex<Option<Cached>>,
}

impl std::fmt::Debug for TokenCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self.inner.lock() {
            Ok(guard) if guard.is_some() => "held",
            Ok(_) => "empty",
            Err(_) => "poisoned",
        };
        write!(f, "TokenCache({state})")
    }
}

impl TokenCache {
    /// Returns the cached token while it stays usable.
    fn get(&self) -> Option<String> {
        let guard = self.inner.lock().ok()?;
        let cached = guard.as_ref()?;
        if Instant::now() < cached.usable_until {
            return Some(cached.token.clone());
        }
        None
    }

    /// Replaces the cached token.
    fn put(&self, token: &str, lifetime: Duration) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(Cached {
                token: token.to_string(),
                usable_until: Instant::now() + lifetime,
            });
        }
    }
}

/// Returns the bearer token for the next request.
///
/// Returns `Ok(None)` when no credential is configured, which leaves the
/// request without an `Authorization` header.
pub fn bearer(config: &Config) -> Result<Option<String>> {
    let (Some(credentials), Some(url)) = (config.credentials(), config.token_url()) else {
        return Ok(None);
    };

    if let Some(token) = config.tokens().get() {
        return Ok(Some(token));
    }

    let (token, lifetime) = request_token(credentials, &url)
        .with_context(|| format!("cannot get an access token from {url}"))?;
    config.tokens().put(&token, lifetime);
    Ok(Some(token))
}

/// Exchanges the client credentials for a token.
///
/// Returns the token and the time for which this client will use it, which is
/// [`EXPIRY_MARGIN`] shorter than the server's own lifetime.
fn request_token(credentials: &Credentials, url: &str) -> Result<(String, Duration)> {
    let agent = remote::build_agent(remote::TEXT_TIMEOUT);

    let mut response = agent
        .post(url)
        .header("Authorization", credentials.basic_header())
        .header("Accept", "application/json")
        .send_form([("grant_type", "client_credentials"), ("scope", SCOPE)])
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
        return Err(reject(status.as_u16(), &body));
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

    let lifetime = parsed
        .expires_in
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_LIFETIME)
        .saturating_sub(EXPIRY_MARGIN);

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
/// credential the server objected to, so it is appended to the guidance.
fn reject(code: u16, body: &str) -> anyhow::Error {
    let headline = match code {
        400 | 401 => format!(
            "the token endpoint rejected the client credentials with HTTP {code}; \
             check {} and {}, and check that the client may ask for the scope {SCOPE}",
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

    match remote::message_from_body(body) {
        Some(message) => anyhow::anyhow!("{headline}: {message}"),
        None => anyhow::anyhow!("{headline}"),
    }
}

/// Turns a ureq error from the token endpoint into a message that names the
/// cause.
///
/// A status no longer reaches here: the agent takes one as a value, and
/// [`reject`] reports it. What is left is a transport failure.
fn describe(error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::StatusCode(code) => anyhow::anyhow!("the token endpoint returned HTTP {code}"),
        ureq::Error::Timeout(_) => anyhow::anyhow!("the token request timed out"),
        ureq::Error::HostNotFound => anyhow::anyhow!("cannot resolve the token endpoint host name"),
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
        assert_eq!(cache.get(), None);

        cache.put("a-token", Duration::from_secs(60));
        assert_eq!(cache.get().as_deref(), Some("a-token"));
    }

    #[test]
    fn the_cache_drops_a_token_that_expired() {
        let cache = TokenCache::default();
        cache.put("a-token", Duration::from_secs(0));
        assert_eq!(cache.get(), None);
    }

    #[test]
    fn the_cache_debug_output_never_shows_the_token() {
        let cache = TokenCache::default();
        cache.put("super-secret-value", Duration::from_secs(60));
        let shown = format!("{cache:?}");
        assert!(!shown.contains("super-secret-value"), "got {shown}");
        assert_eq!(shown, "TokenCache(held)");
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
}
