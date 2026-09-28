// SPDX-License-Identifier: GPL-3.0-or-later

//! The macOS LaunchAgent that keeps the content and the binary current between
//! Claude sessions.
//!
//! # Why this exists
//!
//! The `SessionStart` hook syncs only when a session starts, and it installs no
//! binary. A Mac that starts no session for a day keeps a day-old briefing, and
//! a binary stays on its version until someone runs `self-update` by hand. The
//! agent runs `self-update` and then `sync` once an hour, at minute 0, whether
//! or not a session is open.
//!
//! # What it writes
//!
//! `link` writes `~/Library/LaunchAgents/<LABEL>.plist` and loads it with
//! `launchctl`. `unlink` and `uninstall` unload it and remove the file. The
//! agent runs the program copy under the root, the same file the hook runs, so
//! a `self-update` from the agent replaces the file that the next run starts.
//! Both runs are quiet, so the log under the root holds one date line per run
//! and the errors, if any.
//!
//! # Where it does nothing
//!
//! Only macOS has launchd. On any other system `link` writes no agent, and the
//! hook stays the only trigger.
//!
//! The agent belongs to the account, as `~/.claude` does. A `link` that writes
//! into another Claude directory through `--claude-dir` is a test or a second
//! setup, so it writes an agent only where `--agent-dir` names, and never loads
//! it. Without that rule, the installer's dry run would leave an agent that
//! syncs a throwaway root every hour.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The launchd label, and the name of the property list without `.plist`.
pub const LABEL: &str = "it.damac.brainmaker";

/// The file under the root that takes the agent's output.
const LOG_NAME: &str = "agent.log";

/// Where the agent's property list goes, and whether `launchctl` loads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agents {
    dir: PathBuf,
    load: bool,
}

impl Agents {
    /// The agent location for one run.
    ///
    /// `dir` is `--agent-dir`: the list goes there, and nothing loads it.
    /// Without it, the list goes to `~/Library/LaunchAgents` and loads, but
    /// only on macOS and only when `custom_claude` is false, which means no
    /// `--claude-dir` was given. Every other case returns `None`, and no agent
    /// is written or removed.
    pub fn resolve(dir: Option<&Path>, custom_claude: bool) -> Result<Option<Self>> {
        if let Some(dir) = dir {
            return Ok(Some(Self::unloaded(dir)));
        }
        if !cfg!(target_os = "macos") || custom_claude {
            return Ok(None);
        }
        let home = dirs::home_dir().context("cannot find the home directory")?;
        Ok(Some(Self {
            dir: home.join("Library").join("LaunchAgents"),
            load: true,
        }))
    }

    /// A location in `dir` that `launchctl` never loads.
    pub fn unloaded(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            load: false,
        }
    }

    /// The property list: `<dir>/<LABEL>.plist`.
    pub fn plist(&self) -> PathBuf {
        self.dir.join(format!("{LABEL}.plist"))
    }
}

/// The log that the agent writes under `root`.
pub fn log_path(root: &Path) -> PathBuf {
    root.join(LOG_NAME)
}

/// Writes the agent that runs the program named by `prefix`, and loads it.
/// Returns true when the property list changed.
///
/// `prefix` is the [`crate::link::command_prefix`] that the hook also runs. An
/// unchanged list is neither rewritten nor reloaded. A `launchctl` failure is a
/// notice, not an error: the list is in place, and launchd loads it at the next
/// login.
pub fn install(agents: &Agents, prefix: &str, root: &Path, log: &dyn Fn(&str)) -> Result<bool> {
    let path = agents.plist();
    let text = render(prefix, &log_path(root));
    if fs::read_to_string(&path).is_ok_and(|existing| existing == text) {
        return Ok(false);
    }

    fs::create_dir_all(&agents.dir)
        .with_context(|| format!("cannot create {}", agents.dir.display()))?;
    fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;

    if agents.load {
        // A list that is already loaded must go first, or bootstrap refuses
        // it and launchd keeps running the old one.
        let _ = launchctl(&path, "bootout");
        if let Err(error) = launchctl(&path, "bootstrap") {
            log(&format!(
                "notice: cannot load {}: {error:#}. launchd loads it at the next login.",
                path.display()
            ));
        }
    }
    Ok(true)
}

/// Unloads the agent and removes its property list. Returns true when the list
/// was there.
pub fn remove(agents: &Agents) -> Result<bool> {
    let path = agents.plist();
    if fs::symlink_metadata(&path).is_err() {
        return Ok(false);
    }
    if agents.load {
        // A list that was never loaded has nothing to unload.
        let _ = launchctl(&path, "bootout");
    }
    fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
    Ok(true)
}

/// The property list for an agent that runs `prefix` hourly and writes to
/// `log`.
///
/// The two commands run through `/bin/sh`, so `sync` runs when `self-update`
/// fails: an update that cannot reach the server must not hold the content
/// back. `sync` skips its own software check, which `self-update` just made.
/// `RunAtLoad` makes a run at every login, and launchd makes up one missed run
/// when the Mac wakes from sleep.
fn render(prefix: &str, log: &Path) -> String {
    let script =
        format!("date; {prefix} self-update --quiet; {prefix} sync --quiet --no-update-check");
    let log = escape(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by brainmaker link. brainmaker unlink removes it. -->
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/sh</string>
		<string>-c</string>
		<string>{script}</string>
	</array>
	<key>StartCalendarInterval</key>
	<dict>
		<key>Minute</key>
		<integer>0</integer>
	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>ProcessType</key>
	<string>Background</string>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        script = escape(&script),
    )
}

/// Escapes the three characters that XML text cannot hold as they are.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Runs `launchctl <verb> gui/<uid> <plist>` for the account that owns the
/// property list, which is the account this program runs as, since it just
/// wrote the file or found it in its own home directory.
#[cfg(unix)]
fn launchctl(plist: &Path, verb: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let uid = fs::metadata(plist)
        .with_context(|| format!("cannot read {}", plist.display()))?
        .uid();
    let output = std::process::Command::new("/bin/launchctl")
        .arg(verb)
        .arg(format!("gui/{uid}"))
        .arg(plist)
        .output()
        .context("cannot run /bin/launchctl")?;
    if !output.status.success() {
        anyhow::bail!(
            "launchctl {verb} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn launchctl(_plist: &Path, _verb: &str) -> Result<()> {
    anyhow::bail!("launchctl exists only on macOS")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "brainmaker-schedule-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn quiet(_: &str) {}

    #[test]
    fn writes_the_agent_once_and_removes_it_again() {
        let base = temp_dir("install");
        let agents = Agents::unloaded(&base.join("LaunchAgents"));
        let root = base.join("root");

        assert!(install(&agents, "\"/opt/bm/bin/brainmaker\"", &root, &quiet).unwrap());
        assert!(agents.plist().is_file());
        assert!(
            !install(&agents, "\"/opt/bm/bin/brainmaker\"", &root, &quiet).unwrap(),
            "second run is a no-op"
        );
        assert!(
            install(&agents, "\"/other/brainmaker\"", &root, &quiet).unwrap(),
            "a new program path rewrites the list"
        );

        assert!(remove(&agents).unwrap());
        assert!(!agents.plist().exists());
        assert!(!remove(&agents).unwrap(), "a second removal finds nothing");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_agent_updates_then_syncs_even_when_the_update_fails() {
        let text = render(
            "\"/opt/bm/bin/brainmaker\" --dir \"/opt/bm\"",
            Path::new("/opt/bm/agent.log"),
        );
        // `;` and not `&&`, so a failed self-update still lets sync run.
        assert!(
            text.contains(
                "date; \"/opt/bm/bin/brainmaker\" --dir \"/opt/bm\" self-update --quiet; \
                 \"/opt/bm/bin/brainmaker\" --dir \"/opt/bm\" sync --quiet --no-update-check"
            ),
            "{text}"
        );
        assert!(!text.contains("&&"), "{text}");
        assert!(text.contains(&format!("<string>{LABEL}</string>")));
        assert!(text.contains("<key>Minute</key>\n\t\t<integer>0</integer>"));
        assert!(text.contains("<string>/opt/bm/agent.log</string>"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plutil_accepts_the_property_list() {
        let base = temp_dir("plutil");
        let agents = Agents::unloaded(&base);
        install(
            &agents,
            "\"/Me & You/bin/brainmaker\" --dir \"/Me & You\"",
            Path::new("/Me & You"),
            &quiet,
        )
        .unwrap();

        let output = std::process::Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(agents.plist())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn escapes_a_path_that_holds_an_ampersand() {
        let text = render("\"/Me & You/brainmaker\"", Path::new("/Me & You/agent.log"));
        assert!(text.contains("/Me &amp; You/agent.log"), "{text}");
        assert!(!text.contains("Me & You"), "{text}");
    }

    #[test]
    fn a_custom_claude_directory_gets_no_account_agent() {
        // The installer's dry run passes --claude-dir. An agent in the real
        // LaunchAgents directory would then sync a throwaway root every hour.
        assert_eq!(Agents::resolve(None, true).unwrap(), None);

        let named = Agents::resolve(Some(Path::new("/tmp/agents")), true)
            .unwrap()
            .unwrap();
        assert_eq!(named, Agents::unloaded(Path::new("/tmp/agents")));
        assert!(!named.load, "an agent in a named directory never loads");
    }

    #[test]
    fn only_macos_gets_an_account_agent() {
        let resolved = Agents::resolve(None, false).unwrap();
        if cfg!(target_os = "macos") {
            let agents = resolved.unwrap();
            assert!(agents.load);
            assert!(
                agents
                    .plist()
                    .ends_with(format!("Library/LaunchAgents/{LABEL}.plist"))
            );
        } else {
            assert_eq!(resolved, None);
        }
    }
}
