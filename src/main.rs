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

mod archive;
mod auth;
mod cli;
mod config;
mod link;
mod provision;
mod remote;
mod secretstore;
mod selfupdate;
mod signature;
mod state;
mod sync;
mod url;
mod version;

use std::process::ExitCode;

use anyhow::{Result, bail};

use cli::{Action, Args, Command};
use config::Config;

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
    let log = move |message: &str| {
        if !quiet {
            println!("{message}");
        }
    };

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
            link::link(&config, &claude, &log)?;
            Ok(())
        }
        Command::Unlink => {
            let claude = match args.claude_dir.clone() {
                Some(path) => path,
                None => link::claude_dir()?,
            };
            link::unlink(&config, &claude, &log)?;
            Ok(())
        }
        // The hook reads stdout as JSON, so this one prints past --quiet.
        Command::SessionContext => {
            println!("{}", link::session_context(&config)?);
            Ok(())
        }
        Command::SelfUpdate => self_update(&config, args, &log),
        Command::Sync => {
            let outcome = sync::sync(&config, args.force, &log)?;
            report(&config, &outcome, quiet);
            if !args.no_update_check {
                report_software_check(&config, quiet);
            }
            Ok(())
        }
    }
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
    println!("signing   {} trusted key(s)", signature::key_count());
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

    let latest = remote::latest_hash(config)?;
    println!("latest    {latest}");
    println!(
        "state     {}",
        match installed.as_ref() {
            Some(s) if s.hash == latest && content_dir.is_dir() => "up to date",
            Some(_) => "stale",
            None => "not installed",
        }
    );

    println!("software  {}", selfupdate::CURRENT_VERSION);
    println!("platform  {}", selfupdate::platform_key());
    match selfupdate::check(config) {
        Ok(selfupdate::Check::UpToDate { version, .. }) => {
            println!("published {version}");
            println!("update    none");
        }
        Ok(selfupdate::Check::Newer { latest, .. }) => {
            println!("published {latest}");
            println!("update    available; run brainmaker self-update");
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
                    "A newer brainmaker is published: {latest} (this binary is {}). \
                     Run brainmaker self-update.",
                    selfupdate::CURRENT_VERSION
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
