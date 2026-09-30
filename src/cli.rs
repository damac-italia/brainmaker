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
                   never installs one. Then it asks the server for the
                   operator name, and sends the notes in the outbox.
    status         Print the installed hash, the latest hash, both software
                   versions, the operator, and the notes in the outbox. It
                   changes nothing.
    push           Send the notes in the outbox now
    admin          Collect the notes, and read the fleet and its sync log.
                   For the admin; run brainmaker admin --help
    self-update    Replace this binary with the newest build for this platform
    link           Wire the synced content into ~/.claude, so its skills and
                   its session context load in every project, not only in the
                   content directory. On macOS it also installs a LaunchAgent
                   that runs self-update, then sync, every hour.
    unlink         Remove what link wrote, the LaunchAgent included
    uninstall      Remove brainmaker from this machine: what link wrote, then
                   the content, the sealed settings, and the program copy
                   under the root. It asks first, unless --yes is given.
    session-context
                   Print the SessionStart JSON that the linked hook returns.
                   link registers this; it is not meant to be run by hand.

OPTIONS:
    --force              Download and extract even when the content is up to
                         date. With self-update, reinstall the same version.
    --check              With self-update, report the newer version and install
                         nothing
    --no-update-check    With sync, skip the software version check
    -y, --yes            With uninstall, remove without asking first
    --config <PATH>      Import the provisioning file at PATH
    --keep-config        Do not remove the provisioning file after the import
    --dir <PATH>         Use PATH as the root instead of ~/.brainmaker
    --claude-dir <PATH>  With link, unlink, and uninstall, write to PATH
                         instead of ~/.claude. The LaunchAgent is then left
                         alone, unless --agent-dir names where it goes.
    --agent-dir <PATH>   With link, unlink, and uninstall, write the
                         LaunchAgent to PATH instead of ~/Library/LaunchAgents,
                         and do not load it
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

    Seven more variables name the routes, and each one has a default:

    SWETSI_TOKEN_PATH                  under SWETSI_JWT_ENDPOINT
    BRAINMAKER_CONTENT_LATEST_PATH     under BRAINMAKER_API_BASE
    BRAINMAKER_CONTENT_ARCHIVE_PATH    the same
    BRAINMAKER_SOFTWARE_MANIFEST_PATH  the same
    BRAINMAKER_SOFTWARE_BINARY_PATH    the same
    BRAINMAKER_OUTBOX_PATH             the same
    BRAINMAKER_WHOAMI_PATH             the same

    Each variable overrides the stored value. Supply all three credential
    variables, or none of them. None of them means no Authorization header.

OUTBOX:
    At the end of a session, Claude writes one note into
    ~/.brainmaker/outbox/. sync sends each note that has not changed for a
    minute, and moves it to outbox/sent/<YYYY-MM>/. A note that breaks a rule
    moves to outbox/rejected/, beside a .reason.txt file. The name on a note
    comes from the server, never from the note.

EXIT CODES:
    0    The content is up to date, or the update succeeded. A failure to
         send a note is a notice, and sync still exits 0.
    1    The command failed. push also exits 1 when a note stays in the
         outbox because the server or the issuer did not take it.
";

pub const ADMIN_HELP: &str = "\
brainmaker admin — collect the notes, and read the fleet and its sync log

USAGE:
    brainmaker admin pull-outbox <DIR> [OPTIONS]
    brainmaker admin status [--json] [OPTIONS]
    brainmaker admin syncs <OPERATOR> [--limit N] [--json] [OPTIONS]
    brainmaker admin syncs --client <CLIENT-ID> [--limit N] [--json] [OPTIONS]

Every admin command asks for a token with the scope outbox:read. Keep the
admin copy in a root of its own, such as ~/.brainmaker-admin, with --dir, and
never run link there.

COMMANDS:
    pull-outbox <DIR>
                   Write each note that waits on the server into DIR, as
                   <YYYY-MM-DD>-<operator>-<name>, where the date is the day
                   the server received it. It sets author and review_flags in
                   the frontmatter from the server, and never replaces a file.
                   Then it acknowledges each note that is on disk. DIR must
                   exist.
    status         Print one line per operator: the last sync, the version,
                   the platform, the notes that wait on the laptop, and the
                   notes sent in 7 days. Then one line per client that no
                   operator holds.
    syncs <OPERATOR>
                   Print the syncs of every client of OPERATOR, newest first:
                   the time, the client ID, the status, the IP address, the
                   platform, the version, and the installed content.

OPTIONS:
    --json               With status and syncs, print JSON instead of lines
    --limit <N>          With syncs, print at most N rows, from 1 to 1000.
                         Default: 50.
    --client <CLIENT-ID> With syncs, name one client instead of an operator
    --dir, --config, --keep-config, --url, and --quiet work as they do for
    every other command.

These commands print the client IDs that the server reports. No command
prints the credential of this machine.

EXIT CODES:
    0    The command succeeded
    1    The command failed. pull-outbox also exits 1 when a note could not
         be written; that note stays on the server for the next run.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Sync,
    Status,
    SelfUpdate,
    Link,
    Unlink,
    Uninstall,
    SessionContext,
    Push,
    AdminPullOutbox,
    AdminStatus,
    AdminSyncs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Boxed, because `Args` passes the size limit of `clippy::large_enum_variant`
    /// on Windows, where a path takes more bytes.
    Run(Box<Args>),
    Help,
    AdminHelp,
    Version,
}

/// Longest `--limit` that `admin syncs` takes: one page of the server.
pub const MAX_SYNCS_LIMIT: u32 = 1000;

/// Rows that `admin syncs` prints when `--limit` is absent.
pub const DEFAULT_SYNCS_LIMIT: u32 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub command: Command,
    pub force: bool,
    pub quiet: bool,
    /// With `self-update`, report the newer version and install nothing.
    pub check_only: bool,
    /// With `sync`, skip the software version check.
    pub no_update_check: bool,
    /// With `uninstall`, remove without asking first.
    pub yes: bool,
    /// `--config`, naming a provisioning file to import.
    pub config: Option<PathBuf>,
    /// `--keep-config`, leaving the provisioning file in place after import.
    pub keep_config: bool,
    pub dir: Option<PathBuf>,
    pub url: Option<String>,
    /// `--claude-dir`, naming the Claude configuration directory that `link`,
    /// `unlink`, and `uninstall` write to.
    pub claude_dir: Option<PathBuf>,
    /// `--agent-dir`, naming the directory that holds the LaunchAgent that
    /// `link` writes and that `unlink` and `uninstall` remove.
    pub agent_dir: Option<PathBuf>,
    /// With `admin status` and `admin syncs`, print JSON.
    pub json: bool,
    /// With `admin syncs`, the number of rows.
    pub limit: Option<u32>,
    /// With `admin syncs`, the one client to read.
    pub client: Option<String>,
    /// The value after an admin command: the directory of `pull-outbox`, or
    /// the operator of `syncs`.
    pub target: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            command: Command::Sync,
            force: false,
            quiet: false,
            check_only: false,
            no_update_check: false,
            yes: false,
            config: None,
            keep_config: false,
            dir: None,
            url: None,
            claude_dir: None,
            agent_dir: None,
            json: false,
            limit: None,
            client: None,
            target: None,
        }
    }
}

/// Takes the value of `flag` from the arguments.
///
/// A value that starts with a hyphen is refused: it is the next flag, and the
/// value that the user meant is missing. `given` says whether the flag has
/// appeared before; a second appearance is refused, because only one of the
/// two values could take effect.
fn value_of(
    flag: &str,
    noun: &str,
    given: bool,
    iter: &mut impl Iterator<Item = String>,
) -> Result<String> {
    if given {
        bail!("{flag} is given twice; give it once");
    }
    match iter.next() {
        Some(value) if value.starts_with('-') => {
            // Only a path can be written out with ./ in front of its hyphen.
            if noun == "a path" {
                bail!(
                    "{flag} needs {noun}, and {value:?} is another option. \
                     Write a path that starts with a hyphen as ./{value}"
                );
            }
            bail!("{flag} needs {noun}, and {value:?} is another option")
        }
        Some(value) => Ok(value),
        None => bail!("{flag} needs {noun}"),
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
    // True after `admin`, until its subcommand arrives.
    let mut admin_pending = false;
    let mut admin_seen = false;
    let mut iter = raw.into_iter().map(Into::into);

    while let Some(item) = iter.next() {
        match item.as_str() {
            "-h" | "--help" if admin_seen => return Ok(Action::AdminHelp),
            "-h" | "--help" => return Ok(Action::Help),
            "-V" | "--version" => return Ok(Action::Version),
            "--force" => args.force = true,
            "--check" => args.check_only = true,
            "--no-update-check" => args.no_update_check = true,
            "-y" | "--yes" => args.yes = true,
            "--keep-config" => args.keep_config = true,
            "-q" | "--quiet" => args.quiet = true,
            "--config" => {
                let value = value_of("--config", "a path", args.config.is_some(), &mut iter)?;
                args.config = Some(PathBuf::from(value));
            }
            "--dir" => {
                let value = value_of("--dir", "a path", args.dir.is_some(), &mut iter)?;
                args.dir = Some(PathBuf::from(value));
            }
            "--claude-dir" => {
                let value = value_of(
                    "--claude-dir",
                    "a path",
                    args.claude_dir.is_some(),
                    &mut iter,
                )?;
                args.claude_dir = Some(PathBuf::from(value));
            }
            "--agent-dir" => {
                let value = value_of("--agent-dir", "a path", args.agent_dir.is_some(), &mut iter)?;
                args.agent_dir = Some(PathBuf::from(value));
            }
            "--url" => {
                let value = value_of("--url", "a URL", args.url.is_some(), &mut iter)?;
                args.url = Some(value);
            }
            "--json" => args.json = true,
            "--limit" => {
                let value = value_of("--limit", "a number", args.limit.is_some(), &mut iter)?;
                match value.parse::<u32>() {
                    Ok(limit) if (1..=MAX_SYNCS_LIMIT).contains(&limit) => {
                        args.limit = Some(limit);
                    }
                    _ => bail!(
                        "--limit takes a whole number from 1 to {MAX_SYNCS_LIMIT}, got {value:?}"
                    ),
                }
            }
            "--client" => {
                let value = value_of("--client", "a client ID", args.client.is_some(), &mut iter)?;
                args.client = Some(value);
            }
            "admin" if !command_seen => {
                admin_pending = true;
                admin_seen = true;
                command_seen = true;
            }
            "pull-outbox" if admin_pending => {
                args.command = Command::AdminPullOutbox;
                admin_pending = false;
            }
            "status" if admin_pending => {
                args.command = Command::AdminStatus;
                admin_pending = false;
            }
            "syncs" if admin_pending => {
                args.command = Command::AdminSyncs;
                admin_pending = false;
            }
            other if admin_pending => bail!(
                "admin takes pull-outbox, status, or syncs, not {other:?}; run brainmaker admin \
                 --help"
            ),
            other
                if matches!(args.command, Command::AdminPullOutbox | Command::AdminSyncs)
                    && args.target.is_none()
                    && !other.starts_with('-') =>
            {
                args.target = Some(other.to_string());
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
            "link" if !command_seen => {
                args.command = Command::Link;
                command_seen = true;
            }
            "unlink" if !command_seen => {
                args.command = Command::Unlink;
                command_seen = true;
            }
            "uninstall" if !command_seen => {
                args.command = Command::Uninstall;
                command_seen = true;
            }
            "session-context" if !command_seen => {
                args.command = Command::SessionContext;
                command_seen = true;
            }
            "push" if !command_seen => {
                args.command = Command::Push;
                command_seen = true;
            }
            other => bail!("unknown argument {other:?}; run brainmaker --help"),
        }
    }

    if admin_pending {
        bail!("admin needs pull-outbox, status, or syncs; run brainmaker admin --help");
    }
    check_admin_target(&args)?;
    check_options(&args)?;

    Ok(Action::Run(Box::new(args)))
}

/// Fails for an admin command whose value is missing, or given twice over.
fn check_admin_target(args: &Args) -> Result<()> {
    match args.command {
        Command::AdminPullOutbox if args.target.is_none() => {
            bail!("admin pull-outbox needs the directory to write the notes into")
        }
        Command::AdminSyncs if args.target.is_some() == args.client.is_some() => {
            bail!("admin syncs needs an operator, or --client <CLIENT-ID>, and not both")
        }
        _ => Ok(()),
    }
}

/// Fails for an option that has no effect with the command.
///
/// An option that is accepted and ignored lets the user believe it took
/// effect. The table is the "Applies to" column of the option table in
/// docs/API.md.
fn check_options(args: &Args) -> Result<()> {
    use Command::*;

    // The commands that load settings. uninstall does not, so a settings flag
    // would do nothing there.
    let all_but_uninstall: &[Command] = &[
        Sync,
        Status,
        SelfUpdate,
        Link,
        Unlink,
        SessionContext,
        Push,
        AdminPullOutbox,
        AdminStatus,
        AdminSyncs,
    ];

    let given: [(&str, bool, &[Command]); 12] = [
        ("--json", args.json, &[AdminStatus, AdminSyncs]),
        ("--limit", args.limit.is_some(), &[AdminSyncs]),
        ("--client", args.client.is_some(), &[AdminSyncs]),
        ("--force", args.force, &[Sync, SelfUpdate]),
        ("--check", args.check_only, &[SelfUpdate]),
        ("--no-update-check", args.no_update_check, &[Sync]),
        ("--yes", args.yes, &[Uninstall]),
        ("--config", args.config.is_some(), all_but_uninstall),
        ("--keep-config", args.keep_config, all_but_uninstall),
        ("--url", args.url.is_some(), all_but_uninstall),
        (
            "--claude-dir",
            args.claude_dir.is_some(),
            &[Link, Unlink, Uninstall],
        ),
        (
            "--agent-dir",
            args.agent_dir.is_some(),
            &[Link, Unlink, Uninstall],
        ),
    ];

    for (flag, is_given, commands) in given {
        if is_given && !commands.contains(&args.command) {
            if args.command == Uninstall {
                bail!("{flag} has no effect with uninstall, which loads no settings");
            }
            bail!(
                "{flag} has no effect with {}; run brainmaker --help",
                name_of(args.command)
            );
        }
    }
    Ok(())
}

/// The word that the user types for the command.
fn name_of(command: Command) -> &'static str {
    match command {
        Command::Sync => "sync",
        Command::Status => "status",
        Command::SelfUpdate => "self-update",
        Command::Link => "link",
        Command::Unlink => "unlink",
        Command::Uninstall => "uninstall",
        Command::SessionContext => "session-context",
        Command::Push => "push",
        Command::AdminPullOutbox => "admin pull-outbox",
        Command::AdminStatus => "admin status",
        Command::AdminSyncs => "admin syncs",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(items: &[&str]) -> Args {
        match parse(items.iter().copied()).unwrap() {
            Action::Run(args) => *args,
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
    fn parses_the_uninstall_command_and_the_yes_flag() {
        let args = run(&["uninstall", "--claude-dir", "/tmp/claude"]);
        assert_eq!(args.command, Command::Uninstall);
        assert!(!args.yes, "uninstall asks unless --yes is given");
        assert_eq!(args.claude_dir, Some(PathBuf::from("/tmp/claude")));
        assert_eq!(args.agent_dir, None);

        assert!(run(&["uninstall", "--yes"]).yes);
        assert!(run(&["-y", "uninstall"]).yes);
    }

    #[test]
    fn parses_the_push_command_with_the_settings_flags() {
        let args = run(&[
            "push",
            "--quiet",
            "--dir",
            "/tmp/root",
            "--url",
            "https://x/y",
        ]);
        assert_eq!(args.command, Command::Push);
        assert!(args.quiet);
        assert_eq!(args.dir, Some(PathBuf::from("/tmp/root")));
    }

    #[test]
    fn refuses_a_flag_that_push_does_not_use() {
        for flag in ["--force", "--check", "--no-update-check", "--yes"] {
            let error = parse(["push", flag]).unwrap_err().to_string();
            assert!(error.contains("has no effect with push"), "{flag}: {error}");
        }
    }

    #[test]
    fn parses_the_three_admin_commands() {
        let args = run(&["--dir", "/tmp/admin", "admin", "pull-outbox", "inbox"]);
        assert_eq!(args.command, Command::AdminPullOutbox);
        assert_eq!(args.target.as_deref(), Some("inbox"));

        let args = run(&["admin", "status", "--json"]);
        assert_eq!(args.command, Command::AdminStatus);
        assert!(args.json);

        let args = run(&["admin", "syncs", "gabriele", "--limit", "10", "--json"]);
        assert_eq!(args.command, Command::AdminSyncs);
        assert_eq!(args.target.as_deref(), Some("gabriele"));
        assert_eq!(args.limit, Some(10));

        let args = run(&["admin", "syncs", "--client", "brainmaker-sync-old"]);
        assert_eq!(args.client.as_deref(), Some("brainmaker-sync-old"));
        assert_eq!(args.target, None);
    }

    #[test]
    fn an_admin_command_needs_its_value_and_no_extra() {
        for (items, needle) in [
            (&["admin"][..], "needs pull-outbox, status, or syncs"),
            (&["admin", "stats"][..], "not \"stats\""),
            (&["admin", "pull-outbox"][..], "needs the directory"),
            (&["admin", "syncs"][..], "needs an operator"),
            (
                &["admin", "syncs", "gabriele", "--client", "c1c"][..],
                "and not both",
            ),
            (&["admin", "status", "extra"][..], "unknown argument"),
            (&["admin", "pull-outbox", "a", "b"][..], "unknown argument"),
        ] {
            let error = parse(items.iter().copied()).unwrap_err().to_string();
            assert!(error.contains(needle), "{items:?}: {error}");
        }
    }

    #[test]
    fn an_admin_option_fails_where_it_has_no_effect() {
        for (items, needle) in [
            (
                &["status", "--json"][..],
                "--json has no effect with status",
            ),
            (
                &["admin", "pull-outbox", "x", "--json"][..],
                "--json has no effect with admin pull-outbox",
            ),
            (
                &["admin", "status", "--limit", "5"][..],
                "--limit has no effect with admin status",
            ),
            (
                &["admin", "status", "--client", "c1c"][..],
                "--client has no effect with admin status",
            ),
            (
                &["admin", "syncs", "g", "--limit", "0"][..],
                "from 1 to 1000",
            ),
            (
                &["admin", "syncs", "g", "--limit", "1001"][..],
                "from 1 to 1000",
            ),
            (&["admin", "status", "--force"][..], "--force has no effect"),
        ] {
            let error = parse(items.iter().copied()).unwrap_err().to_string();
            assert!(error.contains(needle), "{items:?}: {error}");
        }
        // The settings flags work with every admin command.
        assert!(parse(["admin", "status", "--config", "/tmp/admin.env"]).is_ok());
    }

    #[test]
    fn admin_help_holds_the_details_and_the_main_help_one_line() {
        assert_eq!(parse(["admin", "--help"]).unwrap(), Action::AdminHelp);
        assert_eq!(
            parse(["admin", "syncs", "--help"]).unwrap(),
            Action::AdminHelp
        );
        assert_eq!(parse(["--help", "admin"]).unwrap(), Action::Help);
        let entries = HELP
            .lines()
            .filter(|line| line.starts_with("    admin "))
            .count();
        assert_eq!(entries, 1, "one entry in the main help");
        assert!(HELP.contains("run brainmaker admin --help"));
        assert!(
            !HELP.contains("pull-outbox"),
            "the details live in admin --help"
        );
        assert!(ADMIN_HELP.contains("pull-outbox <DIR>"));
        assert!(ADMIN_HELP.contains("outbox:read"));
        for text in [HELP, ADMIN_HELP] {
            assert!(!text.contains("http://") && !text.contains("https://"));
        }
    }

    #[test]
    fn the_help_names_the_push_command_and_the_new_routes() {
        assert!(HELP.contains("\n    push "), "the command list names push");
        assert!(HELP.contains("BRAINMAKER_OUTBOX_PATH"));
        assert!(HELP.contains("BRAINMAKER_WHOAMI_PATH"));
    }

    #[test]
    fn refuses_a_settings_flag_with_uninstall() {
        for extra in [
            &["--config", "/tmp/brainmaker.env"][..],
            &["--keep-config"][..],
            &["--url", "https://x/y"][..],
        ] {
            let items: Vec<&str> = ["uninstall"].iter().chain(extra).copied().collect();
            let error = parse(items.iter().copied()).unwrap_err().to_string();
            assert!(error.contains("has no effect with uninstall"), "{error}");
        }
        // The same flags stay valid with every other command.
        assert!(parse(["unlink", "--url", "https://x/y"]).is_ok());
    }

    #[test]
    fn parses_the_agent_dir_flag() {
        let args = run(&["link", "--agent-dir", "/tmp/agents"]);
        assert_eq!(args.command, Command::Link);
        assert_eq!(args.agent_dir, Some(PathBuf::from("/tmp/agents")));
        assert!(parse(["link", "--agent-dir"]).is_err());
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

    #[test]
    fn refuses_an_option_as_the_value_of_another() {
        // Before, this set the root to a directory named "--yes" and dropped
        // the --yes that the user meant.
        let error = parse(["uninstall", "--dir", "--yes"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--dir needs a path"), "{error}");

        let error = parse(["--url", "-q"]).unwrap_err().to_string();
        assert!(error.contains("--url needs a URL"), "{error}");
        assert!(!error.contains("./"), "{error}");

        let error = parse(["link", "--claude-dir", "--agent-dir", "/x"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("--claude-dir needs a path"), "{error}");
    }

    #[test]
    fn accepts_a_relative_path_that_starts_with_a_hyphen_when_it_is_written_out() {
        let args = run(&["--dir", "./-odd"]);
        assert_eq!(args.dir, Some(PathBuf::from("./-odd")));
    }

    #[test]
    fn refuses_a_value_option_that_is_given_twice() {
        // Each line uses a command that the option applies to, so the repeat is
        // the only reason to refuse it.
        for items in [
            &["sync", "--config", "/a", "--config", "/b"][..],
            &["sync", "--dir", "/a", "--dir", "/b"][..],
            &["link", "--claude-dir", "/a", "--claude-dir", "/b"][..],
            &["link", "--agent-dir", "/a", "--agent-dir", "/b"][..],
            &["sync", "--url", "https://x/a", "--url", "https://x/b"][..],
        ] {
            let error = parse(items.iter().copied()).unwrap_err().to_string();
            assert!(error.contains("is given twice"), "{items:?}: {error}");
            assert!(error.contains(items[1]), "{items:?}: {error}");
        }
    }

    #[test]
    fn accepts_a_switch_that_is_given_twice() {
        // A repeated switch has one meaning, so it stays valid.
        assert!(run(&["sync", "--quiet", "-q"]).quiet);
    }

    #[test]
    fn refuses_an_option_that_has_no_effect_with_the_command() {
        for (items, flag, command) in [
            (&["sync", "--check"][..], "--check", "sync"),
            (&["status", "--force"][..], "--force", "status"),
            (
                &["self-update", "--no-update-check"][..],
                "--no-update-check",
                "self-update",
            ),
            (&["link", "--yes"][..], "--yes", "link"),
            (&["sync", "--claude-dir", "/x"][..], "--claude-dir", "sync"),
            (
                &["status", "--agent-dir", "/x"][..],
                "--agent-dir",
                "status",
            ),
        ] {
            let error = parse(items.iter().copied()).unwrap_err().to_string();
            let expected = format!("{flag} has no effect with {command}");
            assert!(error.contains(&expected), "{items:?}: {error}");
        }
    }

    #[test]
    fn the_default_command_takes_the_sync_options() {
        assert!(run(&["--force"]).force);
        assert!(run(&["--no-update-check"]).no_update_check);

        let error = parse(["--check"]).unwrap_err().to_string();
        assert!(error.contains("--check has no effect with sync"), "{error}");
    }

    #[test]
    fn accepts_every_command_line_that_brainmaker_writes() {
        // The SessionStart hook, the LaunchAgent, and the unlink line in the
        // CLAUDE.md block run these lines on every installed machine (see
        // src/link.rs and src/schedule.rs). Each one is shown without the
        // program name, and /opt/bm is the root.
        let args = run(&["--dir", "/opt/bm", "sync", "--quiet", "--no-update-check"]);
        assert_eq!(args.command, Command::Sync);
        assert!(args.quiet);
        assert!(args.no_update_check);
        assert_eq!(args.dir, Some(PathBuf::from("/opt/bm")));

        let args = run(&["--dir", "/opt/bm", "session-context"]);
        assert_eq!(args.command, Command::SessionContext);
        assert_eq!(args.dir, Some(PathBuf::from("/opt/bm")));

        let args = run(&["--dir", "/opt/bm", "self-update", "--quiet"]);
        assert_eq!(args.command, Command::SelfUpdate);
        assert!(args.quiet);
        assert_eq!(args.dir, Some(PathBuf::from("/opt/bm")));

        let args = run(&["--dir", "/opt/bm", "unlink"]);
        assert_eq!(args.command, Command::Unlink);
        assert_eq!(args.dir, Some(PathBuf::from("/opt/bm")));
    }

    #[test]
    fn help_and_version_win_over_every_other_argument() {
        // The option check runs after the loop, so a help or version request
        // returns before it can refuse an option that has no effect.
        assert_eq!(parse(["sync", "--check", "--help"]).unwrap(), Action::Help);
        assert_eq!(
            parse(["sync", "--check", "--version"]).unwrap(),
            Action::Version
        );
    }
}
