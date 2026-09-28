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
        let home = std::env::home_dir().context("cannot find the home directory")?;
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

/// The service unit and the timer unit for a systemd user timer.
///
/// `docs/design/scheduled-agent.md` explains each line. Only the tests call
/// this function, so it carries `cfg(test)`. The build that follows removes
/// that attribute.
///
/// The service runs one `/bin/sh -c` command. Its script is the script of
/// `render` with a redirect in front. systemd has no shell of its own, and the
/// redirect sends both outputs to the log on every systemd version, where
/// `StandardOutput=append:` needs version 240. systemd reads the whole script
/// as one quoted item; see `systemd_quote`.
#[cfg(test)]
fn render_systemd(prefix: &str, log: &Path) -> (String, String) {
    let log = shell_word(&log.display().to_string());
    let script = format!(
        "exec >>{log} 2>&1; date; {prefix} self-update --quiet; {prefix} sync --quiet --no-update-check"
    );
    let service = format!(
        "\
# Written by brainmaker link. brainmaker unlink removes it.
[Unit]
Description=Update brainmaker and the shared content

[Service]
Type=oneshot
ExecStart=/bin/sh -c {script}
TimeoutStartSec=30min
",
        script = systemd_quote(&script),
    );
    let timer = "\
# Written by brainmaker link. brainmaker unlink removes it.
[Unit]
Description=Run brainmaker every hour and after each login

[Timer]
OnCalendar=*-*-* *:00:00
AccuracySec=1s
OnStartupSec=1min

[Install]
WantedBy=timers.target
";
    (service, timer.to_string())
}

/// Writes `text` as one double-quoted item of a systemd command line.
///
/// systemd reads `\\` and `\"` inside the quotes, turns `%%` into `%` when it
/// loads the unit, and turns `$$` into `$` when it starts the command. Each of
/// the four characters is doubled, or escaped, so the item reaches the shell
/// as it stands.
#[cfg(test)]
fn systemd_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Writes `text` as one double-quoted shell word.
///
/// This is the rule of `link::shell_quote`. The build makes that function
/// `pub(crate)` and deletes this copy.
#[cfg(test)]
fn shell_word(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if matches!(c, '$' | '`' | '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The program that runs the script of the Windows task.
///
/// Task Scheduler starts one program for each `Exec` action, and it starts no
/// shell of its own. The task therefore starts `cmd.exe` and hands it the
/// script.
#[cfg(test)]
const WINDOWS_SHELL: &str = r"%SystemRoot%\System32\cmd.exe";

/// The task definition that `schtasks.exe /Create /XML` reads.
///
/// `docs/design/scheduled-agent.md` explains each element. Only the tests call
/// this function, so it carries `cfg(test)`. The build that follows removes
/// that attribute.
///
/// `prefix` is the program path and the root, quoted for `cmd.exe`, and not
/// the shell quoting of `link::command_prefix`. `user` is `DOMAIN\name` for the
/// account that runs `link`. The build writes the text as UTF-16 with a byte
/// order mark, because the declaration says UTF-16.
#[cfg(test)]
fn render_task(prefix: &str, log: &Path, user: &str) -> String {
    let script = format!(
        "(echo %date% %time% & {prefix} self-update --quiet & {prefix} sync --quiet --no-update-check) >> \"{}\" 2>&1",
        log.display()
    );
    let arguments = format!("/d /v:off /s /c \"{script}\"");
    let user = escape(user);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<!-- Written by brainmaker link. brainmaker unlink removes it. -->
<Task xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Runs brainmaker self-update, then sync, every hour and after each logon.</Description>
  </RegistrationInfo>
  <Triggers>
    <TimeTrigger>
      <Enabled>true</Enabled>
      <StartBoundary>2026-01-01T00:00:00</StartBoundary>
      <Repetition>
        <Interval>PT1H</Interval>
      </Repetition>
    </TimeTrigger>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
      <Delay>PT1M</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal>
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT30M</ExecutionTimeLimit>
    <Enabled>true</Enabled>
  </Settings>
  <Actions>
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        command = escape(WINDOWS_SHELL),
        arguments = escape(&arguments),
    )
}

/// Returns the tail of `text` that fits in `keep` bytes and starts at the
/// beginning of a line.
///
/// The cut never falls inside a character, and never inside a line: a log
/// that starts with half a line reads as a fault.
///
/// `docs/design/status-and-agent-health.md` explains the rules. When the start
/// lands on the first byte of a line, the function still skips that line, so
/// the result can be one line shorter than `keep` allows. Only the tests call
/// this function, so it carries `cfg(test)`. The build that follows removes
/// that attribute.
#[cfg(test)]
fn tail_of(text: &str, keep: usize) -> &str {
    if text.len() <= keep {
        return text;
    }
    let mut start = text.len() - keep;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    match text[start..].find('\n') {
        Some(offset) => &text[start + offset + 1..],
        None => "",
    }
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

    /// What `link` writes on a Unix system for the root `/opt/bm`.
    const UNIX_PREFIX: &str = "\"/opt/bm/bin/brainmaker\" --dir \"/opt/bm\"";

    /// The same, for a root whose path holds a space.
    const SPACE_PREFIX: &str =
        "\"/home/my user/.brainmaker/bin/brainmaker\" --dir \"/home/my user/.brainmaker\"";

    /// What the build writes on Windows for the root `C:\Users\me\.brainmaker`.
    const WINDOWS_PREFIX: &str =
        r#""C:\Users\me\.brainmaker\bin\brainmaker.exe" --dir "C:\Users\me\.brainmaker""#;

    #[test]
    fn the_systemd_units_run_the_update_and_then_the_sync() {
        let (service, _) = render_systemd(UNIX_PREFIX, Path::new("/opt/bm/agent.log"));

        let update = service.find("self-update --quiet").expect(&service);
        let sync = service
            .find("sync --quiet --no-update-check")
            .expect(&service);
        assert!(update < sync, "{service}");
        // `;` and not `&&`, as in the property list, so a failed self-update
        // still lets sync run.
        assert!(
            service.contains(
                r#"self-update --quiet; \"/opt/bm/bin/brainmaker\" --dir \"/opt/bm\" sync"#
            ),
            "{service}"
        );
        assert!(!service.contains("&&"), "{service}");

        // One command in a one-shot service. The service holds no state that
        // would keep the timer from starting it again an hour later.
        assert_eq!(service.matches("ExecStart=").count(), 1, "{service}");
        assert!(
            service.contains("\nExecStart=/bin/sh -c \"exec >>"),
            "{service}"
        );
        assert!(service.contains("; date; "), "{service}");
        assert!(service.contains("\nType=oneshot\n"), "{service}");
        assert!(!service.contains("RemainAfterExit"), "{service}");
        assert!(!service.contains("[Install]"), "{service}");
        assert!(service.contains("\nTimeoutStartSec=30min\n"), "{service}");
    }

    #[test]
    fn the_timer_fires_at_minute_zero() {
        let (_, timer) = render_systemd(UNIX_PREFIX, Path::new("/opt/bm/agent.log"));

        // Minute 0 of every hour, and the window that systemd allows stays one
        // second wide, so the run starts in minute 0.
        assert!(timer.contains("\nOnCalendar=*-*-* *:00:00\n"), "{timer}");
        assert!(timer.contains("\nAccuracySec=1s\n"), "{timer}");
        // One run after the user manager starts, which is the first login.
        assert!(timer.contains("\nOnStartupSec=1min\n"), "{timer}");
        // The login run makes up a missed hour, so no second one is due.
        assert!(!timer.contains("Persistent"), "{timer}");
        assert!(
            timer.contains("\n[Install]\nWantedBy=timers.target\n"),
            "{timer}"
        );
    }

    #[test]
    fn the_systemd_units_carry_a_path_that_holds_a_space() {
        let (service, _) = render_systemd(
            SPACE_PREFIX,
            Path::new("/home/my user/.brainmaker/agent.log"),
        );
        let exec = service
            .lines()
            .find(|line| line.starts_with("ExecStart="))
            .expect(&service);
        // systemd reads the script as one double-quoted item, so a space
        // cannot split it. The quotes of the shell words are escaped inside it.
        assert_eq!(
            exec,
            r#"ExecStart=/bin/sh -c "exec >>\"/home/my user/.brainmaker/agent.log\" 2>&1; date; \"/home/my user/.brainmaker/bin/brainmaker\" --dir \"/home/my user/.brainmaker\" self-update --quiet; \"/home/my user/.brainmaker/bin/brainmaker\" --dir \"/home/my user/.brainmaker\" sync --quiet --no-update-check""#
        );

        // The four characters that systemd reads are doubled or escaped: the
        // backslash and the double quote of the item, the percent sign of a
        // specifier, and the dollar sign of a variable. The shell has already
        // put a backslash before the dollar sign.
        let (service, _) = render_systemd(
            "\"/srv/50%/\\$HOME/bin/brainmaker\" --dir \"/srv/50%/\\$HOME\"",
            Path::new("/srv/50%/$HOME/agent.log"),
        );
        assert!(
            service
                .contains(r#"\"/srv/50%%/\\$$HOME/bin/brainmaker\" --dir \"/srv/50%%/\\$$HOME\""#),
            "{service}"
        );
        assert!(
            service.contains(r#"exec >>\"/srv/50%%/\\$$HOME/agent.log\" 2>&1"#),
            "{service}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_accepts_the_units() {
        // Like plutil_accepts_the_property_list, this asks the tool of the
        // platform. A system without the tool has nothing to ask.
        let Some(tool) = ["/usr/bin/systemd-analyze", "/bin/systemd-analyze"]
            .into_iter()
            .find(|tool| Path::new(tool).is_file())
        else {
            return;
        };

        let base = temp_dir("systemd");
        let cases = [
            (UNIX_PREFIX, "/opt/bm"),
            (SPACE_PREFIX, "/home/my user/.brainmaker"),
        ];
        for (index, (prefix, root)) in cases.into_iter().enumerate() {
            let dir = base.join(index.to_string());
            fs::create_dir_all(&dir).unwrap();
            let (service, timer) = render_systemd(prefix, &Path::new(root).join("agent.log"));
            fs::write(dir.join("brainmaker.service"), service).unwrap();
            fs::write(dir.join("brainmaker.timer"), timer).unwrap();

            // The tool loads a timer together with the service that it names,
            // and it searches the directory of both files.
            let mut command = std::process::Command::new(tool);
            command
                .args(["verify", "--user"])
                .arg(dir.join("brainmaker.service"))
                .arg(dir.join("brainmaker.timer"));
            // A user manager needs a runtime directory, and a CI runner has none.
            if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
                command.env("XDG_RUNTIME_DIR", &dir);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_task_runs_for_the_user_and_stays_visible() {
        let text = render_task(
            WINDOWS_PREFIX,
            Path::new(r"C:\Users\me\.brainmaker\agent.log"),
            r"PC\me",
        );

        // This account, in its own session, at the least privilege. That is
        // the one logon type that an account without administrator rights can
        // register and run.
        assert!(
            text.contains(
                "<Principal>\n      <UserId>PC\\me</UserId>\n      \
                 <LogonType>InteractiveToken</LogonType>\n      \
                 <RunLevel>LeastPrivilege</RunLevel>\n    </Principal>"
            ),
            "{text}"
        );
        // Every hour from minute 0 with no end, and once after this account
        // logs on. A repetition with no Duration repeats for ever.
        assert!(text.contains("<StartBoundary>2026-01-01T00:00:00</StartBoundary>"));
        assert!(text.contains("<Interval>PT1H</Interval>"));
        assert!(!text.contains("<Duration>"), "{text}");
        assert!(
            text.contains(
                "<LogonTrigger>\n      <Enabled>true</Enabled>\n      \
                 <UserId>PC\\me</UserId>\n      <Delay>PT1M</Delay>"
            ),
            "{text}"
        );
        // A laptop on battery still runs the agent. A run that the machine
        // missed starts late. A run that is still going at the next hour is
        // not started twice, and a hung run ends.
        assert!(text.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(text.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(text.contains("<StartWhenAvailable>true</StartWhenAvailable>"));
        assert!(text.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(text.contains("<ExecutionTimeLimit>PT30M</ExecutionTimeLimit>"));
        // Hidden hides the task in the Task Scheduler window. It does not hide
        // the console window, and the user should see the task.
        assert!(!text.contains("<Hidden>"), "{text}");

        // One action, because the manual does not say what the second of two
        // actions does after the first fails. `&` runs the sync after a failed
        // update, and `&&` would not.
        assert_eq!(text.matches("<Exec>").count(), 1, "{text}");
        assert!(text.contains(r"<Command>%SystemRoot%\System32\cmd.exe</Command>"));
        assert!(
            text.contains(
                r#"self-update --quiet &amp; "C:\Users\me\.brainmaker\bin\brainmaker.exe" --dir "C:\Users\me\.brainmaker" sync"#
            ),
            "{text}"
        );
        assert!(!text.contains("&amp;&amp;"), "{text}");
        assert!(
            text.contains(r#"&gt;&gt; "C:\Users\me\.brainmaker\agent.log" 2&gt;&amp;1"#),
            "{text}"
        );
        assert!(text.starts_with("<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n"));
    }

    #[test]
    fn the_task_escapes_a_path_for_xml() {
        let text = render_task(
            r#""C:\Me & You\bin\brainmaker.exe" --dir "C:\Me & You""#,
            Path::new(r"C:\Me & You\agent.log"),
            r"PC\Me & You",
        );

        assert!(text.contains(r"C:\Me &amp; You\agent.log"), "{text}");
        assert!(text.contains(r"<UserId>PC\Me &amp; You</UserId>"), "{text}");
        assert!(!text.contains("Me & You"), "{text}");

        // No ampersand is left outside an entity, and the arguments hold no
        // angle bracket, because the script redirects with `>>` and `2>&1`.
        let left = text
            .replace("&amp;", "")
            .replace("&gt;", "")
            .replace("&lt;", "");
        assert!(!left.contains('&'), "{text}");
        let arguments = text
            .split("<Arguments>")
            .nth(1)
            .and_then(|rest| rest.split("</Arguments>").next())
            .expect(&text);
        assert!(!arguments.contains(['<', '>']), "{arguments}");
    }

    #[test]
    fn keeps_a_log_that_fits() {
        let log = "one\ntwo\nthree\n";
        assert_eq!(tail_of(log, log.len()), log);
        assert_eq!(tail_of(log, log.len() + 100), log);
        // A log with no line break is a log too, and it fits.
        assert_eq!(tail_of("no break", 8), "no break");
        assert_eq!(tail_of("", 0), "");
        assert_eq!(tail_of("", 10), "");
    }

    #[test]
    fn cuts_a_log_at_the_start_of_a_line() {
        // Twenty lines of ten bytes each.
        let log: String = (0..20).map(|n| format!("run {n:02} ok\n")).collect();
        assert_eq!(log.len(), 200);

        // Whatever the size, the result is the end of the log, it fits, and it
        // starts directly after a line break of the log.
        for keep in 0..=log.len() + 1 {
            let tail = tail_of(&log, keep);
            assert!(log.ends_with(tail), "keep {keep}: {tail:?} is not the end");
            assert!(tail.len() <= keep, "keep {keep}: {tail:?} is too long");
            let start = log.len() - tail.len();
            assert!(
                tail.is_empty() || start == 0 || log.as_bytes()[start - 1] == b'\n',
                "keep {keep}: {tail:?} starts inside a line"
            );
            assert!(
                tail.is_empty() || tail.starts_with("run "),
                "keep {keep}: {tail:?} holds half a line"
            );
        }

        // The first candidate byte is 175, inside line 17, so the cut moves on
        // to the start of line 18.
        assert_eq!(tail_of(&log, 25), "run 18 ok\nrun 19 ok\n");
        // The first candidate byte is 170, the first byte of line 17. The rule
        // still skips that line, so the result is shorter than `keep` allows.
        assert_eq!(tail_of(&log, 30), "run 18 ok\nrun 19 ok\n");
    }

    #[test]
    fn cuts_a_multi_byte_log_without_a_panic() {
        // "é" takes two bytes, so each line takes three. A `keep` of 3n + 2
        // puts the first candidate byte between the two bytes of an "é".
        let log = "é\n".repeat(1000);
        assert_eq!(log.len(), 3000);

        for keep in 0..=log.len() + 1 {
            let tail = tail_of(&log, keep);
            assert!(log.ends_with(tail), "keep {keep}: not the end of the log");
            assert!(tail.len() <= keep, "keep {keep}: {} bytes", tail.len());
            assert!(
                tail.is_empty() || tail.starts_with("é\n"),
                "keep {keep}: the tail starts inside a line"
            );
            assert_eq!(tail.len() % 3, 0, "keep {keep}: half a line");
        }

        // 1001 is odd, and the first candidate byte is 1999, the second byte of
        // an "é". The cut moves on to byte 2000, then to the start of the next
        // line, which is byte 2001.
        let tail = tail_of(&log, 1001);
        assert_eq!(tail.len(), 999);
        assert!(tail.starts_with("é\n"));
    }

    #[test]
    fn returns_nothing_when_the_tail_holds_no_line_break() {
        let log = "x".repeat(5000);
        assert_eq!(tail_of(&log, 100), "");

        // A line break that ends the log leaves nothing after it.
        let log = format!("{}\n", "x".repeat(5000));
        assert_eq!(tail_of(&log, 100), "");
    }
}
