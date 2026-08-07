// SPDX-License-Identifier: GPL-3.0-or-later

//! Command-line parsing.

use std::path::PathBuf;

use anyhow::{Result, bail};

pub const HELP: &str = "\
brainmaker — keep the local content in step with the server

USAGE:
    brainmaker [COMMAND] [OPTIONS]

COMMANDS:
    sync           Update the content when the server has a newer version
                   (default). It also reports a newer brainmaker, but it
                   never installs one.
    status         Print the installed hash, the latest hash, and both
                   software versions. It changes nothing.
    self-update    Replace this binary with the newest build for this platform

OPTIONS:
    --force              Download and extract even when the content is up to
                         date. With self-update, reinstall the same version.
    --check              With self-update, report the newer version and install
                         nothing
    --no-update-check    With sync, skip the software version check
    --config <PATH>      Import the provisioning file at PATH
    --keep-config        Do not remove the provisioning file after the import
    --dir <PATH>         Use PATH as the root instead of ~/.brainmaker
    --url <URL>          Use URL as the API base
    -q, --quiet          Print errors only
    -h, --help           Print this help text
    -V, --version        Print the version

FIRST RUN:
    brainmaker carries no endpoint. Put the brainmaker.env file that your
    administrator sent you next to the binary and run brainmaker. It reads the
    file once, stores the settings sealed under
    ~/.brainmaker/confidential/, and removes the file.

ENVIRONMENT:
    The keys carry two prefixes, because the two hosts can differ. SWETSI_
    names the service that issues the token. BRAINMAKER_ names the service
    that serves the content and the software.

    BRAINMAKER_API_BASE   Base of every content and software route.
    SWETSI_JWT_ENDPOINT   Base of the OAuth2 routes. brainmaker asks it for an
                          access token before every run.
    SWETSI_CLIENT_ID      Client identifier for the token request.
    SWETSI_CLIENT_SECRET  Client secret for the token request.
    BRAINMAKER_CONFIG     Path of the provisioning file to import.

    Five more variables name the routes, and each one has a default:

    SWETSI_TOKEN_PATH                  under SWETSI_JWT_ENDPOINT
    BRAINMAKER_CONTENT_LATEST_PATH     under BRAINMAKER_API_BASE
    BRAINMAKER_CONTENT_ARCHIVE_PATH    the same
    BRAINMAKER_SOFTWARE_MANIFEST_PATH  the same
    BRAINMAKER_SOFTWARE_BINARY_PATH    the same

    Each variable overrides the stored value. Supply all three credential
    variables, or none of them. None of them means no Authorization header.

EXIT CODES:
    0    The content is up to date, or the update succeeded
    1    The command failed
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Sync,
    Status,
    SelfUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Run(Args),
    Help,
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub command: Command,
    pub force: bool,
    pub quiet: bool,
    /// With `self-update`, report the newer version and install nothing.
    pub check_only: bool,
    /// With `sync`, skip the software version check.
    pub no_update_check: bool,
    /// `--config`, naming a provisioning file to import.
    pub config: Option<PathBuf>,
    /// `--keep-config`, leaving the provisioning file in place after import.
    pub keep_config: bool,
    pub dir: Option<PathBuf>,
    pub url: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            command: Command::Sync,
            force: false,
            quiet: false,
            check_only: false,
            no_update_check: false,
            config: None,
            keep_config: false,
            dir: None,
            url: None,
        }
    }
}

/// Parses the arguments that follow the program name.
pub fn parse<I, S>(raw: I) -> Result<Action>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args = Args::default();
    let mut command_seen = false;
    let mut iter = raw.into_iter().map(Into::into).peekable();

    while let Some(item) = iter.next() {
        match item.as_str() {
            "-h" | "--help" => return Ok(Action::Help),
            "-V" | "--version" => return Ok(Action::Version),
            "--force" => args.force = true,
            "--check" => args.check_only = true,
            "--no-update-check" => args.no_update_check = true,
            "--keep-config" => args.keep_config = true,
            "-q" | "--quiet" => args.quiet = true,
            "--config" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--config needs a path"))?;
                args.config = Some(PathBuf::from(value));
            }
            "--dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--dir needs a path"))?;
                args.dir = Some(PathBuf::from(value));
            }
            "--url" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--url needs a URL"))?;
                args.url = Some(value);
            }
            "sync" if !command_seen => {
                args.command = Command::Sync;
                command_seen = true;
            }
            "status" if !command_seen => {
                args.command = Command::Status;
                command_seen = true;
            }
            "self-update" if !command_seen => {
                args.command = Command::SelfUpdate;
                command_seen = true;
            }
            other => bail!("unknown argument {other:?}; run brainmaker --help"),
        }
    }

    Ok(Action::Run(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(items: &[&str]) -> Args {
        match parse(items.iter().copied()).unwrap() {
            Action::Run(args) => args,
            other => panic!("expected Action::Run, got {other:?}"),
        }
    }

    #[test]
    fn defaults_to_the_sync_command() {
        let args = run(&[]);
        assert_eq!(args.command, Command::Sync);
        assert!(!args.force);
        assert!(!args.quiet);
    }

    #[test]
    fn parses_the_status_command_and_the_flags() {
        let args = run(&[
            "status",
            "--quiet",
            "--dir",
            "/tmp/root",
            "--url",
            "https://x/y",
        ]);
        assert_eq!(args.command, Command::Status);
        assert!(args.quiet);
        assert_eq!(args.dir, Some(PathBuf::from("/tmp/root")));
        assert_eq!(args.url.as_deref(), Some("https://x/y"));
    }

    #[test]
    fn parses_the_force_flag() {
        assert!(run(&["sync", "--force"]).force);
    }

    #[test]
    fn parses_the_self_update_command_and_its_flags() {
        let args = run(&["self-update", "--check"]);
        assert_eq!(args.command, Command::SelfUpdate);
        assert!(args.check_only);
        assert!(!args.no_update_check);
    }

    #[test]
    fn parses_the_no_update_check_flag() {
        let args = run(&["sync", "--no-update-check"]);
        assert_eq!(args.command, Command::Sync);
        assert!(args.no_update_check);
    }

    #[test]
    fn parses_the_provisioning_flags() {
        let args = run(&["sync", "--config", "/tmp/brainmaker.env", "--keep-config"]);
        assert_eq!(args.config, Some(PathBuf::from("/tmp/brainmaker.env")));
        assert!(args.keep_config);
    }

    #[test]
    fn rejects_config_without_its_value() {
        assert!(parse(["--config"]).is_err());
    }

    #[test]
    fn the_help_text_names_no_endpoint() {
        // The help text ships to every employee, so it must disclose no URL.
        assert!(!HELP.contains("https://"));
        assert!(!HELP.contains("http://"));
    }

    #[test]
    fn returns_help_and_version() {
        assert_eq!(parse(["--help"]).unwrap(), Action::Help);
        assert_eq!(parse(["-V"]).unwrap(), Action::Version);
    }

    #[test]
    fn rejects_an_unknown_argument() {
        assert!(parse(["--nope"]).is_err());
        assert!(parse(["sync", "status"]).is_err());
    }

    #[test]
    fn rejects_an_option_without_its_value() {
        assert!(parse(["--dir"]).is_err());
        assert!(parse(["--url"]).is_err());
    }
}
