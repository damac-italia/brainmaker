// SPDX-License-Identifier: GPL-3.0-or-later

//! HTTP access to the content API and to the software API.

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::{Config, MAX_ARCHIVE_BYTES, validate_hash};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TEXT_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const USER_AGENT: &str = concat!("brainmaker/", env!("CARGO_PKG_VERSION"));

/// Body of `GET {base}/content/latest`.
#[derive(Debug, Deserialize)]
struct Latest {
    hash: String,
}

/// Reads the latest content hash from the API.
///
/// The returned hash always passes [`validate_hash`].
pub fn latest_hash(config: &Config) -> Result<String> {
    let url = config.latest_url();
    let body = fetch_text(config, &url, crate::config::MAX_MANIFEST_BYTES)?;

    let latest: Latest = serde_json::from_str(&body).with_context(|| {
        format!("{url} did not return the expected JSON object {{\"hash\": \"...\"}}")
    })?;

    validate_hash(&latest.hash)?;
    Ok(latest.hash)
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
    if let Some(token) = config.token() {
        request = request.header("Authorization", format!("Bearer {token}"));
    }

    let mut response = request
        .call()
        .map_err(describe)
        .with_context(|| format!("cannot read {url}"))?;

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
    if let Some(token) = config.token() {
        request = request.header("Authorization", format!("Bearer {token}"));
    }

    let mut response = request
        .call()
        .map_err(describe)
        .with_context(|| format!("cannot download {url}"))?;

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

fn build_agent(total_timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(total_timeout))
        .user_agent(USER_AGENT)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Turns a ureq error into a message that names the cause.
fn describe(error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::StatusCode(401) | ureq::Error::StatusCode(403) => anyhow::anyhow!(
            "the server rejected the request with HTTP {}; \
             set {} to a valid bearer token",
            status_of(&error),
            crate::config::TOKEN_ENV
        ),
        ureq::Error::StatusCode(404) => {
            anyhow::anyhow!("the server returned HTTP 404 Not Found")
        }
        ureq::Error::StatusCode(code) => anyhow::anyhow!("the server returned HTTP {code}"),
        ureq::Error::Timeout(_) => anyhow::anyhow!("the request timed out"),
        ureq::Error::HostNotFound => anyhow::anyhow!("cannot resolve the host name"),
        other => anyhow::anyhow!(other),
    }
}

fn status_of(error: &ureq::Error) -> u16 {
    match error {
        ureq::Error::StatusCode(code) => *code,
        _ => 0,
    }
}
