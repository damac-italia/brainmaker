// SPDX-License-Identifier: GPL-3.0-or-later

//! The provisioning file that you ship to an employee, and its import.
//!
//! The file is `KEY=VALUE` text. brainmaker reads it once, stores it sealed
//! under `~/.brainmaker/confidential/`, and removes the original.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::url;

/// Required. Base of every API route, with no trailing slash.
pub const KEY_API_BASE: &str = "SWETSI_API_BASE";

/// Optional. Sent as `Authorization: Bearer <value>`.
pub const KEY_TOKEN: &str = "SWETSI_TOKEN";

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

    pub fn token(&self) -> Option<&str> {
        self.values
            .get(KEY_TOKEN)
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
        let Some(base) = self.api_base() else {
            bail!("the provisioning file has no {KEY_API_BASE} line");
        };
        if base.trim() != base {
            bail!("{KEY_API_BASE} has leading or trailing whitespace");
        }
        url::check_base_url(KEY_API_BASE, base)
    }
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
            "SWETSI_API_BASE=https://api.example.test/v1/brainmaker\n\
             SWETSI_TOKEN=abc123\n",
        )
        .unwrap();
        assert_eq!(
            settings.api_base(),
            Some("https://api.example.test/v1/brainmaker")
        );
        assert_eq!(settings.token(), Some("abc123"));
        settings.validate().unwrap();
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let settings = parse(
            "# the endpoint\n\
             \n\
             SWETSI_API_BASE=https://api.example.test/v1\n\
             \n\
             # SWETSI_TOKEN=commented-out\n",
        )
        .unwrap();
        assert_eq!(settings.api_base(), Some("https://api.example.test/v1"));
        assert_eq!(settings.token(), None);
    }

    #[test]
    fn accepts_export_and_quotes() {
        let settings = parse(
            "export SWETSI_API_BASE=\"https://api.example.test/v1\"\n\
             SWETSI_TOKEN='quoted token'\n",
        )
        .unwrap();
        assert_eq!(settings.api_base(), Some("https://api.example.test/v1"));
        assert_eq!(settings.token(), Some("quoted token"));
    }

    #[test]
    fn keeps_an_unknown_key_for_a_later_version() {
        let settings = parse(
            "SWETSI_API_BASE=https://api.example.test/v1\n\
             SWETSI_FUTURE_SETTING=42\n",
        )
        .unwrap();
        assert!(settings.to_text().contains("SWETSI_FUTURE_SETTING=42"));
        assert_eq!(parse(&settings.to_text()).unwrap(), settings);
    }

    #[test]
    fn survives_a_round_trip_through_text() {
        let settings =
            parse("SWETSI_API_BASE=https://api.example.test/v1\nSWETSI_TOKEN=abc\n").unwrap();
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
        assert!(parse("SWETSI_TOKEN=abc\n").unwrap().validate().is_err());
        assert!(
            parse("SWETSI_API_BASE=ftp://api.example.test\n")
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(
            parse("SWETSI_API_BASE=api.example.test\n")
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn an_empty_token_counts_as_absent() {
        let settings =
            parse("SWETSI_API_BASE=https://api.example.test/v1\nSWETSI_TOKEN=\n").unwrap();
        assert_eq!(settings.token(), None);
    }

    #[test]
    fn rejects_a_plain_api_base_that_leaves_this_machine() {
        // The token travels on every request, so a plain base URL must not
        // reach the network.
        assert!(
            parse("SWETSI_API_BASE=http://api.example.test/v1\n")
                .unwrap()
                .validate()
                .is_err()
        );
        assert!(
            parse("SWETSI_API_BASE=http://localhost:8080/v1\n")
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
