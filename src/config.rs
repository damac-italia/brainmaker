// SPDX-License-Identifier: GPL-3.0-or-later

//! Paths, endpoint URLs, and credentials for one run.
//!
//! No endpoint is compiled into this binary. Every URL comes from the
//! provisioning file that you ship to an employee. See [`crate::provision`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::provision::{self, Settings};
use crate::secretstore;
use crate::url;

/// Environment variable that overrides the provisioned base URL.
pub const API_BASE_ENV: &str = provision::KEY_API_BASE;

/// Environment variable that overrides the provisioned token.
pub const TOKEN_ENV: &str = provision::KEY_TOKEN;

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

#[derive(Debug, Clone)]
pub struct Config {
    root: PathBuf,
    base_url: String,
    token: Option<String>,
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
            Some(path) => path,
            None => dirs::home_dir()
                .context("cannot locate the home directory")?
                .join(".brainmaker"),
        };

        let store = Self::store_path_for(&root);
        let mut source = Source::Stored;
        let mut settings = Settings::default();

        if let Some(path) = provision::find(options.config.as_deref())? {
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
                        // it plainly, because the file still holds the token.
                        log(&format!(
                            "warning: cannot remove {}: {error}. Delete it yourself.",
                            path.display()
                        ));
                    }
                }
            } else {
                log(&format!(
                    "warning: {} still holds the token, because --keep-config was given.",
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

        // The environment and the command line override the stored values.
        let mut base_url =
            env_value(API_BASE_ENV).or_else(|| settings.api_base().map(str::to_string));
        let mut token = env_value(TOKEN_ENV).or_else(|| settings.token().map(str::to_string));

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

        if let Some(value) = token.as_mut() {
            *value = value.trim().to_string();
            if value.is_empty() {
                token = None;
            }
        }

        Ok(Self {
            root,
            base_url,
            token,
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
        format!("{}/content/latest", self.base_url)
    }

    /// URL of the archive for one hash. Validate the hash first.
    pub fn archive_url(&self, hash: &str) -> String {
        format!("{}/content/{hash}.zip", self.base_url)
    }

    /// URL that returns the software manifest as JSON.
    pub fn software_url(&self) -> String {
        format!("{}/software/brainmaker", self.base_url)
    }

    /// Base of every API route, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Describes the token without disclosing it. Never print the token
    /// itself.
    pub fn token_summary(&self) -> String {
        match self.token.as_deref() {
            Some(value) => format!("present, {} characters", value.chars().count()),
            None => "absent".to_string(),
        }
    }
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
            token: None,
            source: Source::Stored,
        }
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
    fn the_token_summary_never_shows_the_token() {
        let mut config = config_for("https://api.example.test/v1");
        assert_eq!(config.token_summary(), "absent");

        config.token = Some("super-secret-value".to_string());
        let summary = config.token_summary();
        assert!(!summary.contains("super-secret-value"));
        assert_eq!(summary, "present, 18 characters");
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
