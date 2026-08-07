// SPDX-License-Identifier: GPL-3.0-or-later

//! The provisioning file that you ship to an employee, and its import.
//!
//! The file is `KEY=VALUE` text. brainmaker reads it once, stores it sealed
//! under `~/.brainmaker/confidential/`, and removes the original.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::url;

/// Required. Base of every content and software route, with no trailing slash.
///
/// The keys carry two prefixes, because the two hosts can differ. `SWETSI_`
/// names the authorisation service that issues the token. `BRAINMAKER_` names
/// the service that serves the content and the software.
pub const KEY_API_BASE: &str = "BRAINMAKER_API_BASE";

/// Base of the OAuth2 routes on the authorisation service.
pub const KEY_JWT_ENDPOINT: &str = "SWETSI_JWT_ENDPOINT";

/// Client identifier for the client-credentials grant.
pub const KEY_CLIENT_ID: &str = "SWETSI_CLIENT_ID";

/// Client secret for the client-credentials grant.
pub const KEY_CLIENT_SECRET: &str = "SWETSI_CLIENT_SECRET";

/// The static bearer token that earlier versions read.
///
/// The server now issues a short-lived token instead, so a file that still
/// carries this key is refused rather than silently ignored.
pub const KEY_LEGACY_TOKEN: &str = "SWETSI_TOKEN";

/// The three keys that together configure the client-credentials grant.
///
/// A file supplies all three or none of them.
pub const CREDENTIAL_KEYS: [&str; 3] = [KEY_JWT_ENDPOINT, KEY_CLIENT_ID, KEY_CLIENT_SECRET];

/// Route of the token endpoint, relative to [`KEY_JWT_ENDPOINT`].
pub const KEY_TOKEN_PATH: &str = "SWETSI_TOKEN_PATH";

/// Route that returns the latest content hash, relative to [`KEY_API_BASE`].
pub const KEY_CONTENT_LATEST_PATH: &str = "BRAINMAKER_CONTENT_LATEST_PATH";

/// Route that returns one content archive, relative to [`KEY_API_BASE`].
pub const KEY_CONTENT_ARCHIVE_PATH: &str = "BRAINMAKER_CONTENT_ARCHIVE_PATH";

/// Route that returns the signed software manifest, relative to
/// [`KEY_API_BASE`].
pub const KEY_SOFTWARE_MANIFEST_PATH: &str = "BRAINMAKER_SOFTWARE_MANIFEST_PATH";

/// Route that returns one replacement binary, relative to [`KEY_API_BASE`].
pub const KEY_SOFTWARE_BINARY_PATH: &str = "BRAINMAKER_SOFTWARE_BINARY_PATH";

/// Keys that an earlier version read, each with the key that replaces it.
///
/// A configuration that still carries the old name fails, rather than falling
/// back to a default and reaching the wrong host.
pub const RENAMED_KEYS: [(&str, &str); 1] = [("SWETSI_API_BASE", KEY_API_BASE)];

/// The five route keys, in the order the documents list them.
///
/// Each one is optional. An absent key takes the generic default in
/// [`crate::config`], so a customer who does not want their route names in a
/// public repository sets all five.
pub const ROUTE_KEYS: [&str; 5] = [
    KEY_TOKEN_PATH,
    KEY_CONTENT_LATEST_PATH,
    KEY_CONTENT_ARCHIVE_PATH,
    KEY_SOFTWARE_MANIFEST_PATH,
    KEY_SOFTWARE_BINARY_PATH,
];

/// Names we look for when no path is given.
///
/// Both names carry `brainmaker`. A bare `.env` is deliberately absent: running
/// brainmaker inside an unrelated project would otherwise import that project's
/// `.env` and then delete it.
const FILE_NAMES: [&str; 2] = ["brainmaker.env", ".brainmaker.env"];

/// Environment variable that names the provisioning file.
pub const CONFIG_PATH_ENV: &str = "BRAINMAKER_CONFIG";

/// Largest provisioning file we read.
const MAX_BYTES: u64 = 64 * 1024;

/// The provisioned settings.
///
/// Unknown keys are kept, so a file written for a later version survives a
/// round trip through the sealed store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    values: BTreeMap<String, String>,
}

impl Settings {
    pub fn api_base(&self) -> Option<&str> {
        self.values.get(KEY_API_BASE).map(String::as_str)
    }

    pub fn jwt_endpoint(&self) -> Option<&str> {
        self.value(KEY_JWT_ENDPOINT)
    }

    pub fn client_id(&self) -> Option<&str> {
        self.value(KEY_CLIENT_ID)
    }

    pub fn client_secret(&self) -> Option<&str> {
        self.value(KEY_CLIENT_SECRET)
    }

    /// The static token that versions before 0.2.0 read. Only the credential
    /// check reads it, and only to name the reason a stale file fails.
    pub fn legacy_token(&self) -> Option<&str> {
        self.value(KEY_LEGACY_TOKEN)
    }

    /// Any key by name. An empty value counts as absent.
    ///
    /// The route keys and the renamed-key check read this, because both work
    /// over a list of key names rather than over named fields.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.value(key)
    }

    /// Reads one key. An empty value counts as absent.
    fn value(&self, key: &str) -> Option<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// Renders the settings back to `KEY=VALUE` text for the sealed store.
    ///
    /// Every key survives, including one this version does not read, so a file
    /// written for a later version round-trips through the sealed store.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for (key, value) in &self.values {
            out.push_str(key);
            out.push('=');
            out.push_str(value);
            out.push('\n');
        }
        out
    }

    /// Fails when a required key is absent or unusable.
    pub fn validate(&self) -> Result<()> {
        check_renamed_keys(|key| self.value(key).is_some())?;

        let Some(base) = self.api_base() else {
            bail!("the provisioning file has no {KEY_API_BASE} line");
        };
        if base.trim() != base {
            bail!("{KEY_API_BASE} has leading or trailing whitespace");
        }
        url::check_base_url(KEY_API_BASE, base)?;

        if let Some(endpoint) = self.jwt_endpoint() {
            if endpoint.trim() != endpoint {
                bail!("{KEY_JWT_ENDPOINT} has leading or trailing whitespace");
            }
            url::check_base_url(KEY_JWT_ENDPOINT, endpoint)?;
        }

        check_credential_set(
            |key| self.value(key).is_some(),
            self.value(KEY_LEGACY_TOKEN),
        )
    }
}

/// Fails when a source still carries a key under its old name.
///
/// `present` reports whether one key holds a non-empty value.
pub fn check_renamed_keys(present: impl Fn(&str) -> bool) -> Result<()> {
    for (old, new) in RENAMED_KEYS {
        if present(old) && !present(new) {
            bail!(
                "{old} is now called {new}. Rename the key, or ask your administrator for a new brainmaker.env file."
            );
        }
    }
    Ok(())
}

/// Fails when the credential keys are half configured.
///
/// `present` reports whether one key holds a non-empty value. `legacy` holds
/// the value of [`KEY_LEGACY_TOKEN`], when a source still supplies it.
///
/// All three keys, or none of them, is usable. None of them means brainmaker
/// sends no `Authorization` header, which suits a local test server.
pub fn check_credential_set(present: impl Fn(&str) -> bool, legacy: Option<&str>) -> Result<()> {
    let missing: Vec<&str> = CREDENTIAL_KEYS
        .into_iter()
        .filter(|key| !present(key))
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    if missing.len() == CREDENTIAL_KEYS.len() {
        if legacy.is_some() {
            bail!(
                "{KEY_LEGACY_TOKEN} is no longer read. The server now issues a short-lived \
                 token, so the configuration needs {}. Ask your administrator for a new \
                 brainmaker.env file.",
                CREDENTIAL_KEYS.join(", ")
            );
        }
        return Ok(());
    }

    bail!(
        "the configuration has {} but not {}. Supply all three keys, or none of them.",
        CREDENTIAL_KEYS
            .into_iter()
            .filter(|key| present(key))
            .collect::<Vec<_>>()
            .join(", "),
        missing.join(", ")
    )
}

/// Parses `KEY=VALUE` text.
///
/// The parser skips blank lines and lines that start with `#`. It accepts an
/// optional `export ` prefix, and it strips one pair of matching single or
/// double quotes from a value.
pub fn parse(text: &str) -> Result<Settings> {
    let mut values = BTreeMap::new();

    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();

        let Some((key, value)) = line.split_once('=') else {
            bail!("line {} is not KEY=VALUE: {raw:?}", index + 1);
        };

        let key = key.trim();
        if key.is_empty() {
            bail!("line {} has an empty key", index + 1);
        }
        if !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        {
            bail!("line {} has the invalid key {key:?}", index + 1);
        }

        values.insert(key.to_string(), unquote(value.trim()).to_string());
    }

    if values.is_empty() {
        bail!("the provisioning file holds no KEY=VALUE line");
    }

    Ok(Settings { values })
}

/// Removes one pair of matching quotes.
fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Reads and parses a provisioning file.
pub fn read(path: &Path) -> Result<Settings> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    if size > MAX_BYTES {
        bail!(
            "{} is {size} bytes, larger than the limit of {MAX_BYTES} bytes",
            path.display()
        );
    }

    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let settings = parse(&text).with_context(|| format!("cannot parse {}", path.display()))?;
    settings
        .validate()
        .with_context(|| format!("{} is not a usable provisioning file", path.display()))?;
    Ok(settings)
}

/// Finds a provisioning file to import.
///
/// The search order is:
///
/// 1. `explicit`, from `--config`. A missing file there is an error, not a
///    silent skip.
/// 2. `$BRAINMAKER_CONFIG`.
/// 3. `brainmaker.env`, then `.brainmaker.env`, next to the running binary.
/// 4. `brainmaker.env`, then `.brainmaker.env`, in the working directory.
pub fn find(explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(path) = explicit {
        if !path.is_file() {
            bail!("--config names {}, which is not a file", path.display());
        }
        return Ok(Some(path.to_path_buf()));
    }

    if let Ok(value) = std::env::var(CONFIG_PATH_ENV) {
        let path = PathBuf::from(&value);
        if !path.is_file() {
            bail!("{CONFIG_PATH_ENV} names {value}, which is not a file");
        }
        return Ok(Some(path));
    }

    let mut directories: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        directories.push(parent.to_path_buf());
    }
    if let Ok(cwd) = std::env::current_dir()
        && !directories.contains(&cwd)
    {
        directories.push(cwd);
    }

    for directory in directories {
        for name in FILE_NAMES {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Ok(Some(candidate));
            }
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plain_file() {
        let settings = parse(
            "BRAINMAKER_API_BASE=https://api.example.test/v1/brainmaker\n\
             SWETSI_JWT_ENDPOINT=https://api.example.test/swetsi/v1/\n\
             SWETSI_CLIENT_ID=the-client-id\n\
             SWETSI_CLIENT_SECRET=abc123\n",
        )
        .unwrap();
        assert_eq!(
            settings.api_base(),
            Some("https://api.example.test/v1/brainmaker")
        );
        assert_eq!(
            settings.jwt_endpoint(),
            Some("https://api.example.test/swetsi/v1/")
        );
        assert_eq!(settings.client_id(), Some("the-client-id"));
        assert_eq!(settings.client_secret(), Some("abc123"));
        settings.validate().unwrap();
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let settings = parse(
            "# the endpoint\n\
             \n\
             BRAINMAKER_API_BASE=https://api.example.test/v1\n\
             \n\
             # SWETSI_CLIENT_SECRET=commented-out\n",
        )
        .unwrap();
        assert_eq!(settings.api_base(), Some("https://api.example.test/v1"));
        assert_eq!(settings.client_secret(), None);
    }

    #[test]
    fn accepts_export_and_quotes() {
        let settings = parse(
            "export BRAINMAKER_API_BASE=\"https://api.example.test/v1\"\n\
             SWETSI_CLIENT_SECRET='quoted secret'\n",
        )
        .unwrap();
        assert_eq!(settings.api_base(), Some("https://api.example.test/v1"));
        assert_eq!(settings.client_secret(), Some("quoted secret"));
    }

    #[test]
    fn keeps_an_unknown_key_for_a_later_version() {
        let settings = parse(
            "BRAINMAKER_API_BASE=https://api.example.test/v1\n\
             SWETSI_FUTURE_SETTING=42\n",
        )
        .unwrap();
        assert!(settings.to_text().contains("SWETSI_FUTURE_SETTING=42"));
        assert_eq!(parse(&settings.to_text()).unwrap(), settings);
    }

    #[test]
    fn survives_a_round_trip_through_text() {
        let settings =
            parse("BRAINMAKER_API_BASE=https://api.example.test/v1\nSWETSI_CLIENT_SECRET=abc\n")
                .unwrap();
        assert_eq!(parse(&settings.to_text()).unwrap(), settings);
    }

    #[test]
    fn rejects_a_malformed_line() {
        assert!(parse("this line has no equals sign\n").is_err());
        assert!(parse("=novalue\n").is_err());
        assert!(parse("BAD KEY=value\n").is_err());
        assert!(parse("").is_err());
        assert!(parse("# only a comment\n").is_err());
    }

    #[test]
    fn rejects_a_file_without_a_usable_api_base() {
        assert!(
            parse("SWETSI_CLIENT_SECRET=abc\n")
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(
            parse("BRAINMAKER_API_BASE=ftp://api.example.test\n")
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(
            parse("BRAINMAKER_API_BASE=api.example.test\n")
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn an_empty_client_secret_counts_as_absent() {
        let settings =
            parse("BRAINMAKER_API_BASE=https://api.example.test/v1\nSWETSI_CLIENT_SECRET=\n")
                .unwrap();
        assert_eq!(settings.client_secret(), None);
    }

    #[test]
    fn accepts_a_file_with_no_credential_key() {
        // A local test server needs no credential, so an unauthenticated
        // configuration stays usable.
        parse("BRAINMAKER_API_BASE=http://localhost:8080/v1\n")
            .unwrap()
            .validate()
            .unwrap();
    }

    #[test]
    fn rejects_a_half_configured_credential() {
        let settings = parse(
            "BRAINMAKER_API_BASE=https://api.example.test/v1\n\
             SWETSI_CLIENT_ID=the-client-id\n",
        )
        .unwrap();
        let error = settings.validate().unwrap_err();
        let message = format!("{error}");
        assert!(message.contains("SWETSI_CLIENT_SECRET"), "got {message}");
        assert!(message.contains("SWETSI_JWT_ENDPOINT"), "got {message}");
    }

    #[test]
    fn rejects_a_key_under_its_old_name() {
        // A silent fall-back would leave the client with no base URL, or with
        // one that names the wrong service.
        let settings = parse("SWETSI_API_BASE=https://api.example.test/v1\n").unwrap();
        let error = settings.validate().unwrap_err();
        assert!(
            format!("{error}").contains("SWETSI_API_BASE is now called BRAINMAKER_API_BASE"),
            "got {error}"
        );
    }

    #[test]
    fn the_two_prefixes_split_the_two_services() {
        // SWETSI_ names the authorisation service. BRAINMAKER_ names the
        // service that serves the content and the software.
        for key in [
            KEY_JWT_ENDPOINT,
            KEY_CLIENT_ID,
            KEY_CLIENT_SECRET,
            KEY_TOKEN_PATH,
        ] {
            assert!(key.starts_with("SWETSI_"), "got {key}");
        }
        for key in [
            KEY_API_BASE,
            KEY_CONTENT_LATEST_PATH,
            KEY_CONTENT_ARCHIVE_PATH,
            KEY_SOFTWARE_MANIFEST_PATH,
            KEY_SOFTWARE_BINARY_PATH,
        ] {
            assert!(key.starts_with("BRAINMAKER_"), "got {key}");
        }
    }

    #[test]
    fn rejects_the_legacy_token_key() {
        // A file that predates the client-credentials grant must fail loudly.
        // Silent acceptance would send no Authorization header and produce an
        // HTTP 401 that names the wrong cause.
        let settings = parse(
            "BRAINMAKER_API_BASE=https://api.example.test/v1\n\
             SWETSI_TOKEN=an-old-static-token\n",
        )
        .unwrap();
        let error = settings.validate().unwrap_err();
        assert!(
            format!("{error}").contains("SWETSI_TOKEN is no longer read"),
            "got {error}"
        );
    }

    #[test]
    fn rejects_a_plain_jwt_endpoint_that_leaves_this_machine() {
        // The client secret travels to this endpoint, so the TLS rule applies
        // to it as it applies to the base URL.
        let settings = parse(
            "BRAINMAKER_API_BASE=https://api.example.test/v1\n\
             SWETSI_JWT_ENDPOINT=http://api.example.test/swetsi/v1/\n\
             SWETSI_CLIENT_ID=the-client-id\n\
             SWETSI_CLIENT_SECRET=abc123\n",
        )
        .unwrap();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn rejects_a_plain_api_base_that_leaves_this_machine() {
        // The token travels on every request, so a plain base URL must not
        // reach the network.
        assert!(
            parse("BRAINMAKER_API_BASE=http://api.example.test/v1\n")
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(
            parse("BRAINMAKER_API_BASE=http://localhost:8080/v1\n")
                .unwrap()
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn no_search_name_is_a_bare_dot_env() {
        // A bare ".env" would import and then delete an unrelated project's
        // file when brainmaker runs in that project's directory.
        assert!(!FILE_NAMES.contains(&".env"));
        assert!(FILE_NAMES.iter().all(|name| name.contains("brainmaker")));
    }

    #[test]
    fn find_rejects_an_explicit_path_that_does_not_exist() {
        let missing = Path::new("/nonexistent/brainmaker.env");
        assert!(find(Some(missing)).is_err());
    }
}
