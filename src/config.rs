// SPDX-License-Identifier: GPL-3.0-or-later

//! Paths, endpoint URLs, and credentials for one run.
//!
//! No endpoint is compiled into this binary. Every URL comes from the
//! provisioning file that you ship to an employee. See [`crate::provision`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::auth::{self, TokenCache};
use crate::provision::{self, Settings};
use crate::secretstore;
use crate::url;

/// Environment variable that overrides the provisioned base URL.
pub const API_BASE_ENV: &str = provision::KEY_API_BASE;

/// Environment variable that overrides the provisioned OAuth2 endpoint.
pub const JWT_ENDPOINT_ENV: &str = provision::KEY_JWT_ENDPOINT;

/// Environment variable that overrides the provisioned client identifier.
pub const CLIENT_ID_ENV: &str = provision::KEY_CLIENT_ID;

/// Environment variable that overrides the provisioned client secret.
pub const CLIENT_SECRET_ENV: &str = provision::KEY_CLIENT_SECRET;

/// Default route of the token endpoint, under the OAuth2 endpoint.
pub const DEFAULT_TOKEN_PATH: &str = "oauth2/token";

/// Default route that returns the latest content hash.
pub const DEFAULT_CONTENT_LATEST_PATH: &str = "content/latest";

/// Default route that returns one content archive.
pub const DEFAULT_CONTENT_ARCHIVE_PATH: &str = "content/{hash}.zip";

/// Default route that returns the signed software manifest.
pub const DEFAULT_SOFTWARE_MANIFEST_PATH: &str = "software/brainmaker";

/// Default route that returns one replacement binary.
pub const DEFAULT_SOFTWARE_BINARY_PATH: &str = "software/brainmaker-{version}-{platform}{ext}";

/// Length of a content hash, in characters.
pub const HASH_LEN: usize = 8;

/// Largest archive we accept from the server.
pub const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

/// Largest single file we extract from an archive.
pub const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

/// Largest total payload we extract from one archive. This bounds the damage a
/// zip bomb can do to the disk.
pub const MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

/// Largest software manifest we accept from the server.
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Largest replacement binary we accept from the server.
pub const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;

/// Where the settings for this run came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A provisioning file was found and imported during this run.
    Imported { from: PathBuf, removed: bool },
    /// The sealed store held the settings.
    Stored,
    /// The environment supplied every setting, and no store was needed.
    Environment,
}

/// What the client-credentials grant needs.
///
/// The struct holds the client secret, so its [`std::fmt::Debug`] output names
/// no value.
#[derive(Clone)]
pub struct Credentials {
    jwt_endpoint: String,
    client_id: String,
    client_secret: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("jwt_endpoint", &self.jwt_endpoint)
            .field("client_id", &"<redacted>")
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl Credentials {
    /// Checks the three values and keeps them.
    ///
    /// The client identifier and the client secret go into an `Authorization`
    /// header, so both must hold printable ASCII only. HTTP Basic separates the
    /// two with a colon, so the identifier must hold no colon.
    pub fn new(jwt_endpoint: String, client_id: String, client_secret: String) -> Result<Self> {
        let jwt_endpoint = jwt_endpoint.trim().to_string();
        url::check_base_url(JWT_ENDPOINT_ENV, &jwt_endpoint)?;

        check_credential_value(CLIENT_ID_ENV, &client_id)?;
        check_credential_value(CLIENT_SECRET_ENV, &client_secret)?;
        if client_id.contains(':') {
            bail!(
                "{CLIENT_ID_ENV} holds a colon. HTTP Basic separates the client identifier \
                 from the client secret with a colon, so an identifier that holds one cannot \
                 be sent."
            );
        }

        Ok(Self {
            jwt_endpoint,
            client_id,
            client_secret,
        })
    }

    /// URL of the token endpoint, for the configured route.
    pub fn token_url(&self, route: &str) -> String {
        join(&self.jwt_endpoint, route)
    }

    /// The `Authorization` header value for the token request.
    pub fn basic_header(&self) -> String {
        let pair = format!("{}:{}", self.client_id, self.client_secret);
        format!("Basic {}", auth::base64(pair.as_bytes()))
    }
}

/// The five routes this client asks for, each relative to a base URL.
///
/// No route name is required in the provisioning file. An absent key takes the
/// generic default above, so a deployment that does not want its route names in
/// a public repository sets all five and the repository learns nothing.
#[derive(Debug, Clone)]
pub struct Routes {
    token: String,
    content_latest: String,
    content_archive: String,
    software_manifest: String,
    software_binary: String,
}

impl Default for Routes {
    fn default() -> Self {
        Self {
            token: DEFAULT_TOKEN_PATH.to_string(),
            content_latest: DEFAULT_CONTENT_LATEST_PATH.to_string(),
            content_archive: DEFAULT_CONTENT_ARCHIVE_PATH.to_string(),
            software_manifest: DEFAULT_SOFTWARE_MANIFEST_PATH.to_string(),
            software_binary: DEFAULT_SOFTWARE_BINARY_PATH.to_string(),
        }
    }
}

impl Routes {
    /// Reads the five routes from one source, and checks each one.
    ///
    /// `value` returns the configured route for a key, or `None` for the
    /// default.
    pub fn load(value: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut routes = Routes::default();
        for key in provision::ROUTE_KEYS {
            let Some(raw) = value(key) else { continue };
            let checked = check_route(key, &raw)?;
            match key {
                provision::KEY_TOKEN_PATH => routes.token = checked,
                provision::KEY_CONTENT_LATEST_PATH => routes.content_latest = checked,
                provision::KEY_CONTENT_ARCHIVE_PATH => routes.content_archive = checked,
                provision::KEY_SOFTWARE_MANIFEST_PATH => routes.software_manifest = checked,
                provision::KEY_SOFTWARE_BINARY_PATH => routes.software_binary = checked,
                _ => {}
            }
        }
        Ok(routes)
    }
}

/// Fails for a route we must not put in a URL, and returns the usable form.
///
/// A route joins a base URL that the TLS rule already accepted. It must
/// therefore stay under that base: an absolute URL would move the request to
/// another host, and a `..` segment would climb out of the base path.
fn check_route(key: &str, value: &str) -> Result<String> {
    let route = value.trim().trim_start_matches('/');

    if route.is_empty() {
        bail!("{key} is empty");
    }
    if value.contains("://") {
        bail!(
            "{key} must be a route under the base URL, not a whole URL, got {value:?}. \
             Put the scheme and the host in {API_BASE_ENV}."
        );
    }
    if route.split('/').any(|segment| segment == "..") {
        bail!("{key} holds a \"..\" segment, which would leave the base URL, got {value:?}");
    }
    if !route.chars().all(|c| matches!(c, '!'..='~')) {
        bail!("{key} holds a space or a character we cannot put in a URL, got {value:?}");
    }

    Ok(route.to_string())
}

/// Joins a base URL and a route.
fn join(base: &str, route: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), route)
}

/// Fails for a credential value that we must not put in a header.
fn check_credential_value(key: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{key} is empty");
    }
    if !value.chars().all(|c| matches!(c, ' '..='~')) {
        bail!("{key} holds a character that brainmaker cannot send in a header");
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Config {
    root: PathBuf,
    base_url: String,
    routes: Routes,
    credentials: Option<Credentials>,
    /// The access token for this run. One `Config` owns one cache, and every
    /// clone of it shares that cache.
    tokens: Arc<TokenCache>,
    source: Source,
}

/// What `Config::load` needs from the command line.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `--dir`, overriding `~/.brainmaker`.
    pub root: Option<PathBuf>,
    /// `--url`, overriding the provisioned base URL.
    pub base_url: Option<String>,
    /// `--config`, naming a provisioning file to import.
    pub config: Option<PathBuf>,
    /// `--keep-config`, leaving the provisioning file in place after import.
    pub keep_config: bool,
}

impl Config {
    /// Loads the settings for one run.
    ///
    /// The order is:
    ///
    /// 1. Import a provisioning file when one is present, and remove it.
    /// 2. Otherwise read the sealed store.
    /// 3. Apply the environment and the command line on top.
    ///
    /// The function fails when no source supplies a base URL, because this
    /// binary carries no default endpoint.
    pub fn load(options: &Options, log: &dyn Fn(&str)) -> Result<Self> {
        let root = match options.root.clone() {
            Some(path) => absolute(&path)?,
            None => dirs::home_dir()
                .context("cannot locate the home directory")?
                .join(".brainmaker"),
        };

        let store = Self::store_path_for(&root);
        let mut source = Source::Stored;
        let mut settings = Settings::default();

        if let Some(path) = provision::find(options.config.as_deref(), store.is_file())? {
            settings = provision::read(&path)?;
            let sealed = secretstore::seal(settings.to_text().as_bytes())?;
            secretstore::write_owner_only(&store, &sealed)?;
            log(&format!(
                "Imported the configuration from {}",
                path.display()
            ));

            let mut removed = false;
            if !options.keep_config {
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        removed = true;
                        log(&format!("Removed {} after the import.", path.display()));
                    }
                    Err(error) => {
                        // The settings are stored, so the run continues. Say
                        // it plainly, because the file still holds the client
                        // secret.
                        log(&format!(
                            "warning: cannot remove {}: {error}. Delete it yourself.",
                            path.display()
                        ));
                    }
                }
            } else {
                log(&format!(
                    "warning: {} still holds the client secret, because --keep-config was given.",
                    path.display()
                ));
            }

            source = Source::Imported {
                from: path,
                removed,
            };
        } else if store.is_file() {
            let sealed = std::fs::read(&store)
                .with_context(|| format!("cannot read {}", store.display()))?;
            let plaintext = secretstore::open(&sealed)
                .with_context(|| format!("cannot open {}", store.display()))?;
            let text = String::from_utf8(plaintext)
                .context("the stored configuration is not valid UTF-8")?;
            settings = provision::parse(&text).context("the stored configuration is damaged")?;
        }

        // The environment can also carry an old key name, so check the merged
        // set rather than the stored one.
        provision::check_renamed_keys(|key| {
            env_value(key).is_some() || settings.get(key).is_some()
        })?;

        // The environment and the command line override the stored values.
        let mut base_url =
            env_value(API_BASE_ENV).or_else(|| settings.api_base().map(str::to_string));
        let jwt_endpoint =
            env_value(JWT_ENDPOINT_ENV).or_else(|| settings.jwt_endpoint().map(str::to_string));
        let client_id =
            env_value(CLIENT_ID_ENV).or_else(|| settings.client_id().map(str::to_string));
        let client_secret =
            env_value(CLIENT_SECRET_ENV).or_else(|| settings.client_secret().map(str::to_string));
        let legacy_token = env_value(provision::KEY_LEGACY_TOKEN)
            .or_else(|| settings.legacy_token().map(str::to_string));

        if let Some(value) = options.base_url.clone() {
            base_url = Some(value);
        }

        if source == Source::Stored && !store.is_file() {
            if base_url.is_some() {
                source = Source::Environment;
            } else {
                bail!(
                    "brainmaker is not provisioned. Put the brainmaker.env file that your \
                     administrator sent you next to the binary and run it again, or name it \
                     with --config <PATH>."
                );
            }
        }

        let Some(base_url) = base_url else {
            bail!(
                "the configuration has no {API_BASE_ENV}. Ask your administrator for a new \
                 brainmaker.env file."
            );
        };

        let base_url = base_url.trim().trim_end_matches('/').to_string();
        url::check_base_url(API_BASE_ENV, &base_url)?;

        // The environment can complete a stored credential, and it can also
        // leave one half configured. Check the merged set, not the stored one.
        provision::check_credential_set(
            |key| match key {
                JWT_ENDPOINT_ENV => jwt_endpoint.is_some(),
                CLIENT_ID_ENV => client_id.is_some(),
                CLIENT_SECRET_ENV => client_secret.is_some(),
                _ => false,
            },
            legacy_token.as_deref(),
        )?;

        let credentials = match (jwt_endpoint, client_id, client_secret) {
            (Some(endpoint), Some(id), Some(secret)) => {
                Some(Credentials::new(endpoint, id, secret)?)
            }
            _ => None,
        };

        // The environment overrides one route at a time, as it does for the
        // other keys.
        let routes =
            Routes::load(|key| env_value(key).or_else(|| settings.get(key).map(str::to_string)))?;

        Ok(Self {
            root,
            base_url,
            routes,
            credentials,
            tokens: Arc::new(TokenCache::default()),
            source,
        })
    }

    fn store_path_for(root: &Path) -> PathBuf {
        root.join("confidential").join("config.enc")
    }

    /// File that holds the sealed settings.
    pub fn store_path(&self) -> PathBuf {
        Self::store_path_for(&self.root)
    }

    /// Where this run's settings came from.
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// Root directory that holds the content, the state file, and the
    /// temporary directories.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory that holds the extracted content.
    pub fn content_dir(&self) -> PathBuf {
        self.root.join("content")
    }

    /// File that records the installed hash.
    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    /// Directory that receives a fresh extraction before the swap.
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join(".staging")
    }

    /// Directory that holds the previous content between the swap and the
    /// delete.
    pub fn trash_dir(&self) -> PathBuf {
        self.root.join(".trash")
    }

    /// File that receives the downloaded archive.
    pub fn download_file(&self) -> PathBuf {
        self.root.join(".download.zip")
    }

    /// URL that returns the latest content hash as JSON.
    pub fn latest_url(&self) -> String {
        join(&self.base_url, &self.routes.content_latest)
    }

    /// URL of the archive for one hash. Validate the hash first.
    pub fn archive_url(&self, hash: &str) -> String {
        join(
            &self.base_url,
            &self.routes.content_archive.replace("{hash}", hash),
        )
    }

    /// URL that returns the software manifest as JSON.
    pub fn software_url(&self) -> String {
        join(&self.base_url, &self.routes.software_manifest)
    }

    /// URL of the replacement binary for one version and platform.
    ///
    /// The URL is derived from the base URL, so it cannot leave the host that
    /// the provisioning file names. The manifest therefore carries no URL, and
    /// nothing that a release publishes discloses the API host.
    pub fn binary_url(&self, version: &str, platform: &str) -> String {
        let route = self
            .routes
            .software_binary
            .replace("{version}", version)
            .replace("{platform}", platform)
            .replace("{ext}", std::env::consts::EXE_SUFFIX);
        join(&self.base_url, &route)
    }

    /// URL of the token endpoint, or `None` when no credential is configured.
    pub fn token_url(&self) -> Option<String> {
        self.credentials
            .as_ref()
            .map(|credentials| credentials.token_url(&self.routes.token))
    }

    /// Base of every API route, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The client credentials, or `None` when the run is unauthenticated.
    pub fn credentials(&self) -> Option<&Credentials> {
        self.credentials.as_ref()
    }

    /// The access token cache for this run.
    pub fn tokens(&self) -> &TokenCache {
        &self.tokens
    }

    /// Describes the credentials without disclosing them. Never print the
    /// client identifier or the client secret.
    pub fn credentials_summary(&self) -> String {
        match self.credentials.as_ref() {
            Some(credentials) => format!(
                "client-credentials grant, scope {}, client id {} characters, \
                 secret {} characters",
                auth::SCOPE,
                credentials.client_id.chars().count(),
                credentials.client_secret.chars().count()
            ),
            None => "absent".to_string(),
        }
    }

    /// The token endpoint in use, or `<none>`.
    pub fn token_url_summary(&self) -> String {
        self.token_url().unwrap_or_else(|| "<none>".to_string())
    }
}

/// Makes a `--dir` path absolute.
///
/// Every path this module derives from the root ends up somewhere that outlives
/// the working directory: the skill links under `~/.claude/skills`, and the
/// `--dir` in the `SessionStart` hook. A relative root would put a relative
/// path into both, and both would then break from any other directory.
fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path)
        .with_context(|| format!("cannot make {} an absolute path", path.display()))
}

fn env_value(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => None,
    }
}

/// Rejects any hash that we must not put in a URL or in a path.
///
/// The hash reaches us from the network, and we interpolate it into both a URL
/// and a file name. We therefore accept 8 ASCII alphanumeric characters and
/// nothing else. That excludes `/`, `.`, `..`, `%`, and `?`.
pub fn validate_hash(hash: &str) -> Result<()> {
    if hash.chars().count() != HASH_LEN || !hash.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!(
            "the server returned the invalid content hash {hash:?}; \
             expected {HASH_LEN} ASCII alphanumeric characters"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_for(base_url: &str) -> Config {
        Config {
            root: PathBuf::from("/tmp/root"),
            base_url: base_url.to_string(),
            routes: Routes::default(),
            credentials: None,
            tokens: Arc::new(TokenCache::default()),
            source: Source::Stored,
        }
    }

    fn credentials_for(endpoint: &str) -> Credentials {
        Credentials::new(
            endpoint.to_string(),
            "the-client-id".to_string(),
            "super-secret-value".to_string(),
        )
        .unwrap()
    }

    #[test]
    fn a_relative_root_becomes_absolute() {
        let root = absolute(Path::new("some/root")).unwrap();
        assert!(root.is_absolute(), "got {}", root.display());
        assert!(root.ends_with("some/root"), "got {}", root.display());
        // An absolute root is kept as it is.
        assert_eq!(
            absolute(Path::new("/tmp/root")).unwrap(),
            PathBuf::from("/tmp/root")
        );
    }

    #[test]
    fn accepts_an_eight_character_alphanumeric_hash() {
        assert!(validate_hash("a1b2c3d4").is_ok());
        assert!(validate_hash("DEADBEEF").is_ok());
    }

    #[test]
    fn rejects_a_hash_of_the_wrong_length() {
        assert!(validate_hash("a1b2c3d").is_err());
        assert!(validate_hash("a1b2c3d4e").is_err());
        assert!(validate_hash("").is_err());
    }

    #[test]
    fn rejects_a_hash_that_can_escape_a_path_or_a_url() {
        assert!(validate_hash("../../etc").is_err());
        assert!(validate_hash("a1b2c3d/").is_err());
        assert!(validate_hash("a1b2%2e2e").is_err());
        assert!(validate_hash("a1b2c3d.").is_err());
    }

    #[test]
    fn builds_the_expected_urls() {
        let config = config_for("https://api.example.test/v1/brainmaker");
        assert_eq!(
            config.latest_url(),
            "https://api.example.test/v1/brainmaker/content/latest"
        );
        assert_eq!(
            config.archive_url("a1b2c3d4"),
            "https://api.example.test/v1/brainmaker/content/a1b2c3d4.zip"
        );
        assert_eq!(
            config.software_url(),
            "https://api.example.test/v1/brainmaker/software/brainmaker"
        );
    }

    #[test]
    fn the_store_lives_under_confidential() {
        let config = config_for("https://api.example.test/v1");
        assert!(config.store_path().ends_with("confidential/config.enc"));
    }

    #[test]
    fn the_credentials_summary_never_shows_the_secret() {
        let mut config = config_for("https://api.example.test/v1");
        assert_eq!(config.credentials_summary(), "absent");

        config.credentials = Some(credentials_for("https://api.example.test/swetsi/v1/"));
        let summary = config.credentials_summary();
        assert!(!summary.contains("super-secret-value"), "got {summary}");
        assert!(!summary.contains("the-client-id"), "got {summary}");
        assert!(summary.contains("secret 18 characters"), "got {summary}");
    }

    #[test]
    fn the_credentials_debug_output_never_shows_the_secret() {
        let credentials = credentials_for("https://api.example.test/swetsi/v1/");
        let shown = format!("{credentials:?}");
        assert!(!shown.contains("super-secret-value"), "got {shown}");
        assert!(!shown.contains("the-client-id"), "got {shown}");
    }

    #[test]
    fn appends_the_token_route_to_the_endpoint() {
        // The endpoint arrives with or without a trailing slash, and both give
        // one token URL.
        assert_eq!(
            credentials_for("https://api.example.test/swetsi/v1/").token_url(DEFAULT_TOKEN_PATH),
            "https://api.example.test/swetsi/v1/oauth2/token"
        );
        assert_eq!(
            credentials_for("https://api.example.test/swetsi/v1").token_url(DEFAULT_TOKEN_PATH),
            "https://api.example.test/swetsi/v1/oauth2/token"
        );
    }

    #[test]
    fn a_configured_route_replaces_the_default() {
        let mut config = config_for("https://api.example.test/v1/brainmaker");
        config.routes = Routes::load(|key| match key {
            provision::KEY_CONTENT_LATEST_PATH => Some("a/b/current".to_string()),
            provision::KEY_CONTENT_ARCHIVE_PATH => Some("/a/b/{hash}.bin".to_string()),
            provision::KEY_SOFTWARE_MANIFEST_PATH => Some("c/manifest".to_string()),
            provision::KEY_SOFTWARE_BINARY_PATH => {
                Some("c/bm-{version}-{platform}{ext}".to_string())
            }
            _ => None,
        })
        .unwrap();

        assert_eq!(
            config.latest_url(),
            "https://api.example.test/v1/brainmaker/a/b/current"
        );
        // A leading slash on the route does not double the separator.
        assert_eq!(
            config.archive_url("a1b2c3d4"),
            "https://api.example.test/v1/brainmaker/a/b/a1b2c3d4.bin"
        );
        assert_eq!(
            config.software_url(),
            "https://api.example.test/v1/brainmaker/c/manifest"
        );
        assert_eq!(
            config.binary_url("0.2.0", "linux-x86_64"),
            format!(
                "https://api.example.test/v1/brainmaker/c/bm-0.2.0-linux-x86_64{}",
                std::env::consts::EXE_SUFFIX
            )
        );
    }

    #[test]
    fn the_derived_binary_url_stays_under_the_base_url() {
        let config = config_for("https://api.example.test/v1/brainmaker");
        assert_eq!(
            config.binary_url("0.2.0", "darwin-arm64"),
            format!(
                "https://api.example.test/v1/brainmaker/software/brainmaker-0.2.0-darwin-arm64{}",
                std::env::consts::EXE_SUFFIX
            )
        );
    }

    #[test]
    fn rejects_a_route_that_leaves_the_base_url() {
        // A whole URL would move the request to another host.
        assert!(check_route("KEY", "https://evil.example/x").is_err());
        // A parent segment would climb out of the base path.
        assert!(check_route("KEY", "content/../../admin").is_err());
        assert!(check_route("KEY", "../admin").is_err());
        // A space cannot go into a URL.
        assert!(check_route("KEY", "content/two words").is_err());
        assert!(check_route("KEY", "").is_err());
        assert!(check_route("KEY", "   ").is_err());
    }

    #[test]
    fn accepts_a_route_and_strips_its_leading_slash() {
        assert_eq!(
            check_route("KEY", "/content/latest").unwrap(),
            "content/latest"
        );
        assert_eq!(
            check_route("KEY", " content/latest ").unwrap(),
            "content/latest"
        );
        // A segment that merely contains two dots is not a parent segment.
        assert_eq!(
            check_route("KEY", "content/v1..2/x").unwrap(),
            "content/v1..2/x"
        );
    }

    #[test]
    fn builds_the_basic_header() {
        let credentials = Credentials::new(
            "https://api.example.test/swetsi/v1/".to_string(),
            "id".to_string(),
            "secret".to_string(),
        )
        .unwrap();
        // base64("id:secret")
        assert_eq!(credentials.basic_header(), "Basic aWQ6c2VjcmV0");
    }

    #[test]
    fn rejects_a_credential_that_cannot_be_sent() {
        let endpoint = "https://api.example.test/swetsi/v1/".to_string();
        // HTTP Basic separates the two values with a colon.
        assert!(
            Credentials::new(
                endpoint.clone(),
                "id:with:colon".to_string(),
                "s".to_string()
            )
            .is_err()
        );
        // A line feed would let the value write a further header.
        assert!(
            Credentials::new(endpoint.clone(), "id".to_string(), "a\r\nb".to_string()).is_err()
        );
        assert!(Credentials::new(endpoint, "id".to_string(), String::new()).is_err());
    }

    #[test]
    fn rejects_a_plain_jwt_endpoint_that_leaves_this_machine() {
        assert!(
            Credentials::new(
                "http://api.example.test/swetsi/v1/".to_string(),
                "id".to_string(),
                "secret".to_string(),
            )
            .is_err()
        );
        assert!(
            Credentials::new(
                "http://localhost:8080/swetsi/v1/".to_string(),
                "id".to_string(),
                "secret".to_string(),
            )
            .is_ok()
        );
    }

    #[test]
    fn no_endpoint_is_compiled_into_this_module() {
        // The distributed binary must disclose no customer endpoint. This test
        // fails if someone reintroduces a compiled-in default.
        let source = include_str!("config.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("config.rs has a non-test section");

        // A bare "https://" inside an error message is fine. A scheme followed
        // by a host character is an endpoint, and must not be here.
        for scheme in ["https://", "http://"] {
            for (index, _) in production.match_indices(scheme) {
                let next = production[index + scheme.len()..].chars().next();
                assert!(
                    !next.is_some_and(|c| c.is_ascii_alphanumeric()),
                    "config.rs must hold no URL with a host outside its tests"
                );
            }
        }

        assert!(
            !production.contains("DEFAULT_BASE_URL"),
            "config.rs must not reintroduce a default base URL"
        );
    }
}
