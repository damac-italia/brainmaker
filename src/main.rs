// SPDX-License-Identifier: GPL-3.0-or-later
//
// brainmaker — keeps ~/.brainmaker/content in step with the content API.
// Copyright (C) 2026 atom7xyz
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! brainmaker keeps `~/.brainmaker/content/` in step with the content API.

mod admin;
mod archive;
mod auth;
mod cli;
mod config;
mod digest;
mod link;
mod lock;
mod outbox;
mod provision;
mod remote;
mod schedule;
mod secretstore;
mod selfupdate;
mod signature;
mod state;
mod sync;
#[cfg(test)]
mod testutil;
mod uninstall;
mod url;
mod version;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use cli::{Action, Args, Command};
use config::{Config, Layout};

fn main() -> ExitCode {
    let action = match cli::parse(std::env::args().skip(1)) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };

    let args = match action {
        Action::Help => {
            print!("{}", cli::HELP);
            return ExitCode::SUCCESS;
        }
        Action::AdminHelp => {
            print!("{}", cli::ADMIN_HELP);
            return ExitCode::SUCCESS;
        }
        Action::Version => {
            println!("brainmaker {}", selfupdate::CURRENT_VERSION);
            return ExitCode::SUCCESS;
        }
        Action::Run(args) => args,
    };

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<()> {
    let quiet = args.quiet;
    // With --json, standard output holds the document and nothing else, so
    // every other line goes to standard error.
    let json = args.json;
    let log = move |message: &str| {
        if quiet {
        } else if json {
            eprintln!("{message}");
        } else {
            println!("{message}");
        }
    };

    // `uninstall` loads no settings: it must also run where the sealed store
    // no longer opens, and an import would only write what it then removes.
    if args.command == Command::Uninstall {
        return uninstall(args, &log);
    }

    let options = config::Options {
        root: args.dir.clone(),
        base_url: args.url.clone(),
        config: args.config.clone(),
        keep_config: args.keep_config,
    };
    let config = Config::load(&options, &log)?;

    match args.command {
        Command::Status => status(&config),
        Command::Link => {
            let claude = match args.claude_dir.clone() {
                Some(path) => path,
                None => link::claude_dir()?,
            };
            let agents = agents(args)?;
            link::link(&config, &claude, agents.as_ref(), &log)?;
            Ok(())
        }
        Command::Unlink => {
            let claude = match args.claude_dir.clone() {
                Some(path) => path,
                None => link::claude_dir()?,
            };
            let agents = agents(args)?;
            link::unlink(&config, &claude, agents.as_ref(), &log)?;
            Ok(())
        }
        Command::Uninstall => unreachable!("uninstall returns before the settings load"),
        // The hook reads stdout as JSON, so this one prints past --quiet.
        Command::SessionContext => {
            println!("{}", link::session_context(&config)?);
            Ok(())
        }
        Command::SelfUpdate => self_update(&config, args, &log),
        Command::Sync => sync_command(&config, args, &log),
        Command::Push => push(&config, &log),
        Command::AdminPullOutbox => admin_pull_outbox(&config, args, &log),
        Command::AdminStatus => admin_status(&config, args),
        Command::AdminSyncs => admin_syncs(&config, args),
    }
}

/// `admin pull-outbox <DIR>`. It fails when a note could not be written; that
/// note stays on the server for the next run.
fn admin_pull_outbox(config: &Config, args: &Args, log: &dyn Fn(&str)) -> Result<()> {
    let dir = args
        .target
        .as_deref()
        .map(std::path::PathBuf::from)
        .context("admin pull-outbox needs the directory to write the notes into")?;
    let pulled = admin::pull_outbox(config, &dir, log)?;
    log(&format!(
        "Collected {} note(s) into {}, and the server marked {} as collected.",
        pulled.written.len(),
        dir.display(),
        pulled.acked
    ));
    if !pulled.failed.is_empty() {
        bail!(
            "{} note(s) could not be written, and stay on the server for the next run",
            pulled.failed.len()
        );
    }
    Ok(())
}

/// `admin status`, as lines or, with `--json`, as the admin's brief.
fn admin_status(config: &Config, args: &Args) -> Result<()> {
    let state = admin::team_state(&admin::fleet(config)?);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&state)?);
    } else {
        for line in admin::status_lines(&state) {
            println!("{line}");
        }
    }
    Ok(())
}

/// `admin syncs <OPERATOR>` and `admin syncs --client <CLIENT-ID>`.
fn admin_syncs(config: &Config, args: &Args) -> Result<()> {
    let selector = match (&args.client, &args.target) {
        (Some(client), _) => admin::Selector::Client(client.clone()),
        (None, Some(operator)) => admin::Selector::Operator(operator.clone()),
        (None, None) => bail!("admin syncs needs an operator, or --client <CLIENT-ID>"),
    };
    let limit = args.limit.unwrap_or(cli::DEFAULT_SYNCS_LIMIT);
    let rows = admin::syncs(config, &selector, limit)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        println!("The server logs no sync for it.");
    } else {
        for row in &rows {
            println!("{}", admin::sync_line(row));
        }
    }
    Ok(())
}

/// `sync`: the content step, then the outbox steps, then the software check.
///
/// The content result waits until the outbox steps ran, because a failed
/// content step does not stop them. It is returned after them.
fn sync_command(config: &Config, args: &Args, log: &dyn Fn(&str)) -> Result<()> {
    let result = sync::sync(config, args.force, log);
    if let Ok(outcome) = &result {
        report(config, outcome, args.quiet);
    }
    outbox_steps(config, args.quiet, log);
    result?;
    if !args.no_update_check {
        report_software_check(config, args.quiet);
    }
    Ok(())
}

/// The steps of `sync` after the content step: the outbox, `whoami`, and
/// push. Each failure is a notice, and none changes the exit code.
///
/// `whoami` and push run only when this run received a `sync` token, which
/// proves that the issuer answered. Push makes no request when the outbox is
/// empty.
fn outbox_steps(config: &Config, quiet: bool, log: &dyn Fn(&str)) {
    let notice = |message: String| {
        if !quiet {
            eprintln!("notice: {message}");
        }
    };
    // A Mac that linked before the outbox existed never runs link again.
    if let Err(error) = outbox::ensure_dir(config) {
        notice(format!("{error:#}"));
    }
    if !auth::received(config, auth::SCOPE_SYNC) {
        return;
    }
    if let Err(error) = outbox::update_operator(config) {
        notice(format!(
            "cannot ask the server for the operator name: {error:#}"
        ));
    }
    match outbox::push(config, log) {
        Ok(pushed) => {
            if pushed.busy {
                log("Another brainmaker run is sending the notes.");
            }
            if let Some(reason) = pushed.stopped {
                notice(reason);
            }
        }
        Err(error) => notice(format!("cannot send the notes: {error:#}")),
    }
}

/// Sends the notes in the outbox now.
///
/// It fails when a note had to stay, so a person who runs it sees the
/// reason in the exit code as well as in the text.
fn push(config: &Config, log: &dyn Fn(&str)) -> Result<()> {
    let pushed = outbox::push(config, log)?;
    if pushed.busy {
        log("Another brainmaker run is sending the notes, so this one sent none.");
        return Ok(());
    }
    if pushed.settling > 0 {
        log(&format!(
            "{} note(s) changed in the last minute, and wait for the next run.",
            pushed.settling
        ));
    }
    if let Some(reason) = pushed.stopped {
        bail!("{reason}");
    }
    if pushed.sent.is_empty() && pushed.rejected.is_empty() && pushed.settling == 0 {
        log(&format!(
            "No note waits in {}.",
            config.outbox_dir().display()
        ));
    }
    Ok(())
}

/// Where this run writes or removes the LaunchAgent, if anywhere.
fn agents(args: &Args) -> Result<Option<schedule::Agents>> {
    schedule::Agents::resolve(args.agent_dir.as_deref(), args.claude_dir.is_some())
}

fn status(config: &Config) -> Result<()> {
    let installed = state::read(&config.state_file());
    let content_dir = config.content_dir();

    println!("root      {}", config.root().display());
    println!("content   {}", content_dir.display());
    println!("store     {}", config.store_path().display());
    println!(
        "settings  {}",
        match config.source() {
            config::Source::Imported { from, removed } => format!(
                "imported from {} ({})",
                from.display(),
                if *removed {
                    "the file was removed"
                } else {
                    "the file is still there"
                }
            ),
            config::Source::Stored => "read from the sealed store".to_string(),
            config::Source::Environment => "read from the environment".to_string(),
        }
    );
    println!("api base  {}", config.base_url());
    println!("token url {}", config.token_url_summary());
    println!("auth      {}", config.credentials_summary());
    println!("key       {}", secretstore::key_class());
    println!("binding   {}", secretstore::binding().name());
    // Both counts on one line. A second row would need its own label, and
    // "content" already names the content directory above, while any longer
    // label would not fit the column every other value starts at.
    println!(
        "signing   {} trusted software key(s), {} trusted content key(s)",
        signature::key_count(),
        signature::content_key_count()
    );
    println!(
        "installed {}",
        installed
            .as_ref()
            .map(|s| s.hash.as_str())
            .unwrap_or("<none>")
    );
    println!(
        "present   {}",
        if content_dir.is_dir() { "yes" } else { "no" }
    );

    // `status` changes nothing, so an unreachable server is a value here, not
    // an exit. The software rows below take the same view.
    match remote::latest_release(config, &outbox::report_headers(config)) {
        Ok(release) => {
            let latest = release.hash;
            println!("latest    {latest}");
            println!(
                "state     {}",
                match installed.as_ref() {
                    Some(s) if s.hash == latest && content_dir.is_dir() => "up to date",
                    Some(_) => "stale",
                    None => "not installed",
                }
            );
        }
        Err(error) => {
            println!("latest    <unknown>");
            println!("state     cannot check: {error:#}");
        }
    }

    println!("software  {}", selfupdate::CURRENT_VERSION);
    println!("platform  {}", selfupdate::platform_key());
    match selfupdate::check(config) {
        Ok(selfupdate::Check::UpToDate { version, .. }) => {
            println!("published {version}");
            println!("update    none");
        }
        Ok(selfupdate::Check::Newer { latest, .. }) => {
            println!("published {latest}");
            println!(
                "update    available; run {}",
                selfupdate::self_update_command()
            );
        }
        Ok(selfupdate::Check::NewerElsewhere {
            latest, offered, ..
        }) => {
            println!("published {latest}");
            println!(
                "update    not built for this platform; the manifest offers {}",
                offered.join(", ")
            );
        }
        Err(error) => {
            println!("published <unknown>");
            println!("update    cannot check: {error:#}");
        }
    }

    // From the files alone: the operator that the last sync wrote, and the
    // notes on this machine.
    println!(
        "operator  {}",
        outbox::read_operator(config).unwrap_or_else(|| "<none>".to_string())
    );
    let counts = outbox::counts(config);
    println!(
        "outbox    {} waiting, {} rejected",
        counts.waiting, counts.rejected
    );
    println!(
        "pushed    {}",
        outbox::read_push_state(config)
            .map(|state| age(state.last_push_at_unix))
            .unwrap_or_else(|| "<none>".to_string())
    );

    Ok(())
}

/// The age of a moment, in the largest whole unit: `2 days ago`.
fn age(at_unix: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    age_between(at_unix, now)
}

/// The age of `at_unix` at `now_unix`, in the largest whole unit of seconds,
/// minutes, hours, or days. A moment after `now_unix` reads as `0 seconds
/// ago`.
fn age_between(at_unix: u64, now_unix: u64) -> String {
    let seconds = now_unix.saturating_sub(at_unix);
    let (count, unit) = match seconds {
        0..60 => (seconds, "second"),
        60..3600 => (seconds / 60, "minute"),
        3600..86400 => (seconds / 3600, "hour"),
        _ => (seconds / 86400, "day"),
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{plural} ago")
}

/// Removes brainmaker from this machine, once the user says yes.
///
/// The question prints past `--quiet`, because an answer needs its question.
/// With no terminal to ask on, only `--yes` lets the command run.
fn uninstall(args: &Args, log: &dyn Fn(&str)) -> Result<()> {
    let layout = Layout::resolve(args.dir.as_deref())?;
    let claude = match args.claude_dir.clone() {
        Some(path) => path,
        None => link::claude_dir()?,
    };
    let agents = agents(args)?;

    if !args.yes {
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            bail!(
                "uninstall asks before it removes anything, and stdin is not a terminal; \
                 pass --yes to remove without asking"
            );
        }
        print!(
            "{}",
            uninstall::question(layout.root(), &claude, agents.as_ref())
        );
        std::io::stdout()
            .flush()
            .context("cannot print the question")?;
        let mut answer = String::new();
        stdin
            .read_line(&mut answer)
            .context("cannot read the answer")?;
        if !uninstall::is_yes(&answer) {
            log("Nothing was removed.");
            return Ok(());
        }
    }

    uninstall::uninstall(&layout, &claude, agents.as_ref(), log)?;
    Ok(())
}

fn self_update(config: &Config, args: &Args, log: &dyn Fn(&str)) -> Result<()> {
    log("Checking the published software version.");
    let check = selfupdate::check(config)?;

    match check {
        selfupdate::Check::UpToDate {
            version,
            platform,
            build,
        } => {
            log(&format!(
                "brainmaker {version} is the newest published build."
            ));
            if !args.force {
                return Ok(());
            }
            if args.check_only {
                log("--check was given, so nothing was installed.");
                return Ok(());
            }
            let Some(build) = build else {
                bail!(
                    "--force cannot reinstall {version}, \
                     because the manifest has no build for {platform}"
                );
            };
            log(&format!("--force was given, so {version} is reinstalled."));
            let replaced = selfupdate::apply(config, &version, &build, log)?;
            log(&format!(
                "Replaced {} with version {version}.",
                replaced.display()
            ));
            Ok(())
        }
        selfupdate::Check::NewerElsewhere {
            latest,
            platform,
            offered,
        } => {
            bail!(
                "version {latest} is published, but the manifest has no build for {platform}; \
                 it offers {}",
                offered.join(", ")
            );
        }
        selfupdate::Check::Newer {
            latest,
            platform,
            build,
        } => {
            log(&format!(
                "Version {latest} is published for {platform}. This binary is {}.",
                selfupdate::CURRENT_VERSION
            ));

            if args.check_only {
                log("--check was given, so nothing was installed.");
                return Ok(());
            }

            let replaced = selfupdate::apply(config, &latest, &build, log)?;
            log(&format!(
                "Replaced {} with version {latest}.",
                replaced.display()
            ));
            Ok(())
        }
    }
}

/// Reports a newer binary after a sync. A failed check never fails the sync,
/// because the content is already in place.
fn report_software_check(config: &Config, quiet: bool) {
    match selfupdate::check(config) {
        Ok(selfupdate::Check::Newer { latest, .. }) => {
            if !quiet {
                println!(
                    "A newer brainmaker is published: {latest} (this binary is {}). Run {}.",
                    selfupdate::CURRENT_VERSION,
                    selfupdate::self_update_command()
                );
            }
        }
        Ok(_) => {}
        Err(error) => {
            // A notice, not an error. Quiet suppresses it, and the exit code
            // stays 0.
            if !quiet {
                eprintln!("notice: cannot check for a software update: {error:#}");
            }
        }
    }
}

fn report(config: &Config, outcome: &sync::Outcome, quiet: bool) {
    if quiet {
        return;
    }
    match outcome {
        sync::Outcome::UpToDate { hash } => {
            println!("The content is up to date at {hash}.");
        }
        // A notice, not an error: the installed content is in place and the
        // exit code stays 0. Quiet suppresses it, as it does the software one.
        sync::Outcome::Unreachable { hash, error } => {
            eprintln!("notice: cannot check the latest content version: {error}");
            eprintln!("notice: the installed content {hash} stays in place.");
        }
        // A notice, for the same reason: another run is installing, and the
        // content on disk stays usable. The exit code stays 0.
        sync::Outcome::Busy { hash } => {
            eprintln!("notice: another brainmaker run is installing content.");
            eprintln!("notice: the installed content {hash} stays in place.");
        }
        sync::Outcome::Updated {
            previous,
            hash,
            stats,
        } => {
            let from = previous.as_deref().unwrap_or("<none>");
            println!(
                "Updated the content from {from} to {hash}: {} files, {} bytes, in {}.",
                stats.files,
                stats.bytes,
                config.content_dir().display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::testutil::{Route, Server, Signer, temp_dir, zip_of};

    const NOTE: &str = "---\nkind: fact\ndomain: damac\n---\n\n# Lezione\n";

    fn quiet(_: &str) {}

    fn args() -> Args {
        Args {
            quiet: true,
            no_update_check: true,
            ..Args::default()
        }
    }

    /// Writes a note that changed two minutes ago.
    fn settled_note(config: &Config, name: &str) {
        fs::create_dir_all(config.outbox_dir()).unwrap();
        let path = config.outbox_dir().join(name);
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(NOTE.as_bytes()).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
    }

    /// The routes of a server that publishes one signed release.
    fn release(signer: &Signer) -> Vec<Route> {
        let archive = zip_of(&[("CLAUDE.md", b"# briefing")]);
        let payload = serde_json::json!({
            "hash": "a1b2c3d4",
            "sha256": crate::testutil::sha256_hex(&archive),
            "size_bytes": archive.len(),
        });
        vec![
            Route::get("/content/latest", signer.envelope(&payload.to_string())),
            Route::get("/content/a1b2c3d4.zip", archive),
        ]
    }

    fn paths(server: &Server) -> Vec<String> {
        server.received().into_iter().map(|r| r.path).collect()
    }

    #[test]
    fn sync_creates_the_outbox_asks_whoami_and_pushes() {
        let signer = Signer::new();
        signer.trust();
        let mut routes = vec![
            Route::token("sync", r#"{"access_token":"s"}"#),
            Route::token("outbox:write", r#"{"access_token":"w"}"#),
            Route::get("/whoami", r#"{"client_id":"c","operator":"gabriele","display_name":"G"}"#),
            Route::post(
                "/outbox/2026-09-30-a.md",
                r#"{"id":"x","sha256":"y","operator":"gabriele","received_at":"2026-09-30T10:00:00+02:00","duplicate":false}"#,
            )
            .status(201),
        ];
        routes.extend(release(&signer));
        let server = Server::start(routes);
        let dir = temp_dir("main-sync-outbox");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());
        settled_note(&config, "2026-09-30-a.md");

        sync_command(&config, &args(), &quiet).unwrap();

        assert!(config.content_dir().join("CLAUDE.md").is_file());
        assert_eq!(
            fs::read_to_string(config.operator_file()).unwrap(),
            "gabriele\n"
        );
        assert!(
            config
                .sent_dir()
                .join("2026-09")
                .join("2026-09-30-a.md")
                .is_file()
        );
        let seen = paths(&server);
        let latest = seen.iter().position(|p| p == "/content/latest").unwrap();
        let whoami = seen.iter().position(|p| p == "/whoami").unwrap();
        let note = seen
            .iter()
            .position(|p| p == "/outbox/2026-09-30-a.md")
            .unwrap();
        assert!(latest < whoami && whoami < note, "got {seen:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sync_sends_the_report_headers_with_the_content_request() {
        let signer = Signer::new();
        signer.trust();
        let server = Server::start(release(&signer));
        let dir = temp_dir("main-sync-headers");
        let config = Config::for_test(&dir, &server.base());
        settled_note(&config, "2026-09-30-a.md");

        sync_command(&config, &args(), &quiet).unwrap();

        let latest = server
            .received()
            .into_iter()
            .find(|r| r.path == "/content/latest")
            .unwrap();
        assert_eq!(
            latest.header("brainmaker-platform"),
            Some(selfupdate::platform_key().as_str())
        );
        assert_eq!(latest.header("brainmaker-content"), Some("none"));
        assert_eq!(latest.header("brainmaker-outbox"), Some("1"));
        let agent = latest.header("user-agent").unwrap();
        assert_eq!(agent, format!("brainmaker/{}", selfupdate::CURRENT_VERSION));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sync_creates_a_missing_outbox_with_no_credential() {
        let signer = Signer::new();
        signer.trust();
        let server = Server::start(release(&signer));
        let dir = temp_dir("main-sync-mkdir");
        let config = Config::for_test(&dir, &server.base());

        sync_command(&config, &args(), &quiet).unwrap();

        assert!(config.outbox_dir().is_dir());
        // No token, so no whoami and no push.
        assert!(!paths(&server).iter().any(|p| p == "/whoami"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_token_request_skips_whoami_and_push() {
        let signer = Signer::new();
        signer.trust();
        let mut routes = vec![
            Route::token("sync", r#"{"error":"invalid_client"}"#).status(401),
            Route::token("outbox:write", r#"{"access_token":"w"}"#),
            Route::get("/whoami", r#"{"operator":"gabriele"}"#),
        ];
        routes.extend(release(&signer));
        let server = Server::start(routes);
        let dir = temp_dir("main-sync-no-token");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());
        settled_note(&config, "2026-09-30-a.md");

        // Nothing is installed, so the content failure is the result.
        assert!(sync_command(&config, &args(), &quiet).is_err());

        let seen = paths(&server);
        assert_eq!(seen, vec!["/oauth2/token"], "got {seen:?}");
        assert_eq!(outbox::counts(&config).waiting, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn push_still_runs_after_a_failed_content_step() {
        let server = Server::start(vec![
            Route::token("sync", r#"{"access_token":"s"}"#),
            Route::token("outbox:write", r#"{"access_token":"w"}"#),
            Route::get(
                "/content/latest",
                r#"{"error":"no content release is published"}"#,
            )
            .status(404),
            Route::get(
                "/whoami",
                r#"{"client_id":"c","operator":null,"display_name":null}"#,
            ),
            Route::post(
                "/outbox/2026-09-30-a.md",
                r#"{"received_at":"2026-09-30T10:00:00+02:00"}"#,
            )
            .status(201),
        ]);
        let dir = temp_dir("main-sync-content-failed");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());
        settled_note(&config, "2026-09-30-a.md");
        fs::write(config.operator_file(), "gabriele\n").unwrap();

        let error = sync_command(&config, &args(), &quiet).unwrap_err();

        assert!(format!("{error:#}").contains("404"), "got {error:#}");
        assert!(!config.operator_file().exists(), "null deletes the file");
        assert_eq!(outbox::counts(&config).waiting, 0, "the note was sent");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_age_reads_in_its_largest_whole_unit() {
        assert_eq!(age_between(100, 100), "0 seconds ago");
        assert_eq!(age_between(100, 101), "1 second ago");
        assert_eq!(age_between(0, 59), "59 seconds ago");
        assert_eq!(age_between(0, 60), "1 minute ago");
        assert_eq!(age_between(0, 3599), "59 minutes ago");
        assert_eq!(age_between(0, 7199), "1 hour ago");
        assert_eq!(age_between(0, 7200), "2 hours ago");
        assert_eq!(age_between(0, 2 * 86400 + 5), "2 days ago");
        assert_eq!(age_between(200, 100), "0 seconds ago");
    }
}
