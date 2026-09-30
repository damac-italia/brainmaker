// SPDX-License-Identifier: GPL-3.0-or-later

//! Wires the synced content into the user's Claude configuration.
//!
//! # Why this exists
//!
//! `sync` puts the shared content in `~/.brainmaker/content`. Claude reads
//! `~/.claude`, and a project's own `.claude` directory only when that project
//! is the one open. The content ships a `.claude` directory of its own, so
//! without a bridge its skills and its session context load for one project
//! and for no other.
//!
//! `link` writes that bridge, and `unlink` removes it. Both are idempotent.
//!
//! # What it writes
//!
//! 1. One symbolic link per shipped skill, under `~/.claude/skills`.
//! 2. A copy of this binary at `<root>/bin/brainmaker`, and a `SessionStart`
//!    hook in `~/.claude/settings.json` that runs it by that full path, first
//!    `sync --quiet --no-update-check` and then `session-context`. The copy
//!    exists because nothing puts the binary on `PATH`, and because the
//!    archive it was unzipped from is meant to be deleted.
//! 3. A marked block in `~/.claude/CLAUDE.md` that names the content
//!    directory.
//! 4. The outbox directory under the root, owner-only, where Claude writes
//!    the notes that `sync` sends.
//! 5. On macOS, a LaunchAgent that runs `self-update` and then `sync` every
//!    hour, through the same program copy. [`crate::schedule`] says why and
//!    when it is left out.
//!
//! Every piece carries a marker, so `unlink` removes what `link` wrote and
//! leaves everything else alone. `link` never overwrites a file it did not
//! write: a skill name that already exists as a real directory is reported and
//! skipped.
//!
//! # The admin's install
//!
//! Before it writes anything, `link` asks the issuer whether the credential
//! may read the outbox and whether it may send notes; see
//! [`crate::auth::grants`]. A credential that reads and cannot send belongs to
//! the admin who collects the notes. For it, `link` writes the program copy and
//! an agent that runs `self-update` alone, and connects nothing to Claude: the
//! briefing is written for operators, and the admin's Claude must not take it
//! as its own. Any piece of the bridge that an earlier run wrote goes. The
//! admin needs no content directory, and gets no outbox.
//!
//! Every other credential gets the bridge, one with both scopes included, and
//! so does a run with no credential. A check that gets no clear answer stops
//! `link` before it changes anything, because a guess could connect the
//! admin's Claude.
//!
//! # What it deliberately leaves out
//!
//! The content also ships `PreToolUse`, `PostToolUse`, `PreCompact` and `Stop`
//! hooks. Those are written for the vault as a project, and at user scope they
//! would run on every tool call in every project. `link` installs the
//! `SessionStart` path only.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::auth;
use crate::config::Config;
use crate::outbox;
use crate::schedule::{self, Agents, Runs};

/// Largest context this emits, in bytes.
///
/// The briefing reaches the model on every session, so an unbounded file would
/// cost every session that many tokens. A file past the cap is cut, and the cut
/// is announced in the text rather than hidden.
const MAX_CONTEXT_BYTES: usize = 64 * 1024;

/// Marks every value this module writes into `settings.json`.
///
/// The hook commands carry it, so `unlink` can find them again after the user
/// has edited the file by hand.
const MARKER: &str = "brainmaker-link";

/// Opens the block this module writes into `CLAUDE.md`.
const BLOCK_START: &str = "<!-- brainmaker-link: start -->";

/// Closes that block.
const BLOCK_END: &str = "<!-- brainmaker-link: end -->";

/// Files the session context carries, in the order it prints them.
///
/// Each one is relative to the content directory. A missing file is skipped,
/// which is what a fresh or a differently shaped content release gives.
///
/// The third field is that file's own cap. Every session pays for this text,
/// so one file that grows without bound must not crowd out the rest: an open
/// thread list reached 34 KiB against a 17 KiB briefing, which spent most of
/// the budget on the least load-bearing file. The briefing gets the largest
/// share because it is the one file meant to be read whole; the other three
/// are working notes, and their head is the part that matters.
const CONTEXT_FILES: [(&str, &str, usize); 4] = [
    ("CLAUDE.md", "The shared briefing", 24 * 1024),
    ("wiki/hot.md", "Recent context", 8 * 1024),
    ("agent-memory/OPEN-THREADS.md", "Open threads", 8 * 1024),
    (
        "agent-memory/PREFERENCES.md",
        "Operator preferences",
        8 * 1024,
    ),
];

/// What one run changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Skills newly linked.
    pub linked: Vec<String>,
    /// Skills already linked to the same target.
    pub unchanged: Vec<String>,
    /// Skills skipped, each with the reason.
    pub skipped: Vec<(String, String)>,
    /// Skills unlinked.
    pub removed: Vec<String>,
    /// True when `settings.json` changed.
    pub settings_changed: bool,
    /// True when `CLAUDE.md` changed.
    pub briefing_changed: bool,
    /// True when the LaunchAgent's property list changed.
    pub agent_changed: bool,
}

impl Report {
    /// True when the run changed a file or a link.
    pub fn changed(&self) -> bool {
        self.settings_changed
            || self.briefing_changed
            || self.agent_changed
            || !self.linked.is_empty()
            || !self.removed.is_empty()
    }
}

/// The Claude configuration directory, `~/.claude`.
pub fn claude_dir() -> Result<PathBuf> {
    let home = std::env::home_dir().context("cannot find the home directory")?;
    Ok(home.join(".claude"))
}

/// Writes the bridge, and the agent when `agents` names where it goes.
///
/// A credential that reads the outbox and cannot send notes gets the admin's
/// install instead; see the module documentation.
pub fn link(
    config: &Config,
    claude: &Path,
    agents: Option<&Agents>,
    log: &dyn Fn(&str),
) -> Result<Report> {
    if collects_without_sending(config)? {
        let program = install_program(config)?;
        let prefix = command_prefix(&program, Some(config.root()))?;
        return link_admin(config, claude, agents, &prefix, log);
    }

    let content = config.content_dir();
    if !content.is_dir() {
        bail!(
            "{} does not exist; run brainmaker sync first",
            content.display()
        );
    }

    let mut report = Report::default();
    outbox::ensure_dir(config)?;
    link_skills(&content, claude, &mut report)?;
    let program = install_program(config)?;
    let prefix = command_prefix(&program, Some(config.root()))?;
    report.settings_changed = write_settings(claude, Some(&prefix))?;
    report.briefing_changed = write_briefing(&content, claude, Some(&prefix))?;
    if let Some(agents) = agents {
        report.agent_changed =
            schedule::install(agents, &prefix, config.root(), Runs::UpdateAndSync, log)?;
    }
    describe(&report, true, log);
    Ok(report)
}

/// True when the issuer lets this credential read the outbox and not send a
/// note: the admin who collects the notes.
///
/// The `outbox:write` request goes out only when `outbox:read` was granted, so
/// an operator's `link` asks the issuer once. Any failure but `invalid_scope`
/// is an error, and `link` then changes nothing.
fn collects_without_sending(config: &Config) -> Result<bool> {
    let check = || -> Result<bool> {
        Ok(auth::grants(config, auth::SCOPE_OUTBOX_READ)?
            && !auth::grants(config, auth::SCOPE_OUTBOX_WRITE)?)
    };
    check().context(
        "cannot learn from the issuer which role this credential holds, so link changed nothing",
    )
}

/// The admin's install: the agent that runs `self-update` alone, and no
/// bridge to Claude.
///
/// `prefix` names the program copy, which the caller made. Any skill link,
/// hook entry, or block that an earlier run wrote goes, because each one puts
/// the operator briefing in front of the admin's Claude.
fn link_admin(
    config: &Config,
    claude: &Path,
    agents: Option<&Agents>,
    prefix: &str,
    log: &dyn Fn(&str),
) -> Result<Report> {
    log("This credential collects the notes and sends none, so link connects nothing to Claude.");
    let content = config.content_dir();
    let mut report = Report::default();
    unlink_skills(&content, claude, &mut report)?;
    report.settings_changed = write_settings(claude, None)?;
    report.briefing_changed = write_briefing(&content, claude, None)?;
    if report.changed() {
        describe(&report, false, log);
    }
    if let Some(agents) = agents {
        report.agent_changed =
            schedule::install(agents, prefix, config.root(), Runs::UpdateOnly, log)?;
        if report.agent_changed {
            log(&format!(
                "Wrote the hourly LaunchAgent {}, which runs self-update.",
                schedule::LABEL
            ));
        }
    }
    log(&format!(
        "Collect the notes with: {prefix} admin pull-outbox <DIR>"
    ));
    Ok(report)
}

/// Removes the bridge, and the agent when `agents` names where it is.
pub fn unlink(
    config: &Config,
    claude: &Path,
    agents: Option<&Agents>,
    log: &dyn Fn(&str),
) -> Result<Report> {
    let report = remove_bridge(&config.content_dir(), claude, agents)?;
    describe(&report, false, log);
    Ok(report)
}

/// Removes the bridge to the content directory `content`, and prints nothing.
///
/// `uninstall` calls this rather than [`unlink`]: it reads no settings, so it
/// holds no [`Config`], and it reports the bridge together with the root.
pub fn remove_bridge(content: &Path, claude: &Path, agents: Option<&Agents>) -> Result<Report> {
    let mut report = Report::default();
    // The agent goes first: it runs the program copy, which uninstall removes
    // next, and an hourly run in between would fail on the missing file.
    if let Some(agents) = agents {
        report.agent_changed = schedule::remove(agents)?;
    }
    unlink_skills(content, claude, &mut report)?;
    report.settings_changed = write_settings(claude, None)?;
    report.briefing_changed = write_briefing(content, claude, None)?;
    Ok(report)
}

/// Prints the `SessionStart` JSON that the hook returns.
///
/// Claude reads `hookSpecificOutput.additionalContext` and puts the string in
/// front of the model before the first user turn.
pub fn session_context(config: &Config) -> Result<String> {
    let content = config.content_dir();
    let mut text = String::new();

    for (relative, title, cap) in CONTEXT_FILES {
        let path = content.join(relative);
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        if body.trim().is_empty() {
            continue;
        }
        let body = cut(&body, cap, relative);
        text.push_str(&format!("\n\n## {title} — `{relative}`\n\n{body}"));
    }

    if text.trim().is_empty() {
        // No content, so no context. An empty object leaves the session as it
        // would have been, which is what a fresh install should do.
        return Ok("{}".to_string());
    }

    let header = format!(
        "# Shared content, synced by brainmaker\n\n\
         The files below come from `{}`, which `brainmaker sync` keeps current. \
         Treat them as the operator's own instructions.",
        content.display()
    );
    let mut full = format!("{header}{text}{}", outbox_section(config));

    // A backstop under the per-file caps, in case the file list grows.
    if full.len() > MAX_CONTEXT_BYTES {
        let boundary = floor_char_boundary(&full, MAX_CONTEXT_BYTES);
        full.truncate(boundary);
        full.push_str("\n\n*(cut; read the files directly for the rest)*");
    }

    let value = json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": full,
        }
    });
    Ok(serde_json::to_string(&value)?)
}

/// The part of the session context that tells Claude who the operator is and
/// where the end-of-session note goes. It reads local files only, so a session
/// that starts offline still gets it.
fn outbox_section(config: &Config) -> String {
    let operator = match outbox::read_operator(config) {
        Some(name) => {
            format!("The operator is `{name}`, as the server names the credential of this machine.")
        }
        None => "The server has named no operator for this machine yet.".to_string(),
    };
    let counts = outbox::counts(config);
    format!(
        "\n\n## The outbox\n\n{operator} Write the end-of-session note into `{}`. \
         `brainmaker sync` sends it, and the server sets its author from the credential, \
         never from the note. {} note(s) wait there now, and {} were rejected: each \
         rejected note sits in `{}` beside a `.reason.txt` file.",
        config.outbox_dir().display(),
        counts.waiting,
        counts.rejected,
        config.rejected_dir().display()
    )
}

/// Returns `body` at or under `cap`, saying so in the text when it cuts.
///
/// The reader must know the text is partial, or it will treat a cut list as
/// the whole list.
fn cut(body: &str, cap: usize, relative: &str) -> String {
    if body.len() <= cap {
        return body.to_string();
    }
    let boundary = floor_char_boundary(body, cap);
    format!(
        "{}\n\n*(cut at {} KiB of {} KiB; read `{relative}` for the rest)*",
        &body[..boundary],
        cap / 1024,
        body.len().div_ceil(1024),
    )
}

/// Largest index at or below `limit` that starts a character.
///
/// `String::truncate` panics inside a multi-byte character, and the briefing is
/// prose that may hold any of them.
fn floor_char_boundary(text: &str, limit: usize) -> usize {
    let mut index = limit.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Puts the running binary where the hook can still find it, and returns that
/// path.
///
/// The hook cannot name the binary by its bare name: nothing puts `brainmaker`
/// on `PATH`, and a shell then answers `brainmaker: command not found` at every
/// session start. Naming the running executable's own path is not enough
/// either, because that path is wherever the archive was unzipped — a Downloads
/// folder that the install instructions tell the reader to delete.
///
/// So the binary is copied under the brainmaker root, which is the one
/// directory that outlives the archive, and the hook names that copy. When the
/// running binary already is that copy, nothing is written, so `self-update`
/// keeps working on the file the hook runs.
fn install_program(config: &Config) -> Result<PathBuf> {
    let target = installed_program(config.root());
    let current = std::env::current_exe().context("cannot find this program's own path")?;
    copy_program(&current, &target)?;
    Ok(target)
}

/// The path the hook runs: `<root>/bin/brainmaker`.
pub fn installed_program(root: &Path) -> PathBuf {
    root.join("bin").join(program_name())
}

/// Copies the binary at `from` to `to`, unless both name one file.
///
/// The identity check goes through [`same_file`], not through the two path
/// strings. `link` may run through a symbolic link to the installed copy, or
/// by a path that spells the root differently, and a string comparison then
/// removed the very file it was about to copy: the copy failed with "No such
/// file or directory", and the installed binary was gone.
///
/// The copy lands beside `to` and is renamed over it. A rename never writes
/// into a file that is being executed, which fails with ETXTBSY on some
/// systems, and a running old copy keeps its own inode.
pub fn copy_program(from: &Path, to: &Path) -> Result<()> {
    if same_file(from, to) {
        return Ok(());
    }

    let parent = to
        .parent()
        .with_context(|| format!("{} has no parent directory", to.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;

    let staged = parent.join(format!(".{}-{}.new", program_name(), std::process::id()));
    let result = (|| -> Result<()> {
        fs::copy(from, &staged)
            .with_context(|| format!("cannot copy {} to {}", from.display(), staged.display()))?;
        set_executable(&staged)?;
        fs::rename(&staged, to)
            .with_context(|| format!("cannot move {} to {}", staged.display(), to.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

/// True when both paths name one existing file, whatever links or spellings
/// lie on the way to it.
pub fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The file name the copied binary takes.
fn program_name() -> &'static str {
    if cfg!(windows) {
        "brainmaker.exe"
    } else {
        "brainmaker"
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut mode = fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .permissions();
    mode.set_mode(0o755);
    fs::set_permissions(path, mode)
        .with_context(|| format!("cannot set the mode on {}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Links every shipped skill into `~/.claude/skills`.
fn link_skills(content: &Path, claude: &Path, report: &mut Report) -> Result<()> {
    let source_root = content.join(".claude").join("skills");
    if !source_root.is_dir() {
        return Ok(());
    }
    let target_root = claude.join("skills");
    fs::create_dir_all(&target_root)
        .with_context(|| format!("cannot create {}", target_root.display()))?;

    for (name, source) in shipped_skills(&source_root)? {
        let target = target_root.join(&name);
        match fs::symlink_metadata(&target) {
            Err(_) => {
                make_symlink(&source, &target)?;
                report.linked.push(name);
            }
            Ok(meta) if meta.is_symlink() => {
                let current = fs::read_link(&target).unwrap_or_default();
                if current == source {
                    report.unchanged.push(name);
                } else if current.starts_with(content) {
                    // Ours, but pointing at an older layout. Replace it.
                    remove_link(&target)
                        .with_context(|| format!("cannot remove {}", target.display()))?;
                    make_symlink(&source, &target)?;
                    report.linked.push(name);
                } else {
                    report.skipped.push((
                        name,
                        format!("a link to {} is already there", current.display()),
                    ));
                }
            }
            Ok(_) => report.skipped.push((
                name,
                "a directory of that name is already there".to_string(),
            )),
        }
    }
    Ok(())
}

/// Removes the links this module made.
fn unlink_skills(content: &Path, claude: &Path, report: &mut Report) -> Result<()> {
    let target_root = claude.join("skills");
    if !target_root.is_dir() {
        return Ok(());
    }

    let entries = fs::read_dir(&target_root)
        .with_context(|| format!("cannot read {}", target_root.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_symlink() {
            continue;
        }
        // Only a link into the content directory is ours.
        if fs::read_link(&path).is_ok_and(|target| target.starts_with(content)) {
            remove_link(&path).with_context(|| format!("cannot remove {}", path.display()))?;
            report
                .removed
                .push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// Removes the symbolic link `path`, and not what it names.
pub(crate) fn remove_link(path: &Path) -> std::io::Result<()> {
    let removed = fs::remove_file(path);
    // Windows keeps a link to a directory as a directory entry, which only
    // remove_dir takes away. It removes the link, never the target.
    #[cfg(windows)]
    {
        if removed.is_err() {
            return fs::remove_dir(path);
        }
    }
    removed
}

/// Every directory under `root` that holds a `SKILL.md`, by name.
fn shipped_skills(root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut found = BTreeMap::new();
    let entries = fs::read_dir(root).with_context(|| format!("cannot read {}", root.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && path.join("SKILL.md").is_file() {
            found.insert(entry.file_name().to_string_lossy().into_owned(), path);
        }
    }
    Ok(found)
}

#[cfg(unix)]
fn make_symlink(source: &Path, target: &Path) -> Result<()> {
    std::os::unix::fs::symlink(source, target)
        .with_context(|| format!("cannot link {} to {}", target.display(), source.display()))
}

#[cfg(windows)]
fn make_symlink(source: &Path, target: &Path) -> Result<()> {
    std::os::windows::fs::symlink_dir(source, target).with_context(|| {
        format!(
            "cannot link {} to {}. Windows grants this to an administrator, or to any user \
             with Developer Mode enabled",
            target.display(),
            source.display()
        )
    })
}

/// Writes `text` as one double-quoted shell word.
///
/// Inside double quotes a shell gives a meaning to four characters: `$`, the
/// backtick, `"`, and `\`. Each one gets a backslash in front. Every other
/// character is written as it stands, a non-ASCII one included, because the
/// shell reads the bytes and not an escape.
///
/// A control character is refused. No quoting carries a line break through
/// both a shell and a property list, and a path that holds one is a mistake.
fn shell_quote(text: &str) -> Result<String> {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if c.is_control() {
            bail!("the path {text:?} holds a control character, which a hook command cannot carry");
        }
        if matches!(c, '$' | '`' | '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}

/// The start of every command that names the installed binary: the full path,
/// quoted, and the root it must use.
///
/// The path is quoted because nothing puts this binary on `PATH`, so the
/// command must carry the whole path, and a home directory may hold a space.
/// The root is named because without it the command takes the default root,
/// which is the wrong one whenever `link` ran with `--dir`.
///
/// The quoting follows the shell's rules; see `shell_quote`.
pub fn command_prefix(program: &Path, root: Option<&Path>) -> Result<String> {
    let exe = shell_quote(&program.display().to_string())?;
    match root {
        Some(path) => Ok(format!(
            "{exe} --dir {}",
            shell_quote(&path.display().to_string())?
        )),
        None => Ok(exe),
    }
}

/// Reads the text of `path`, or `None` when no file is there.
///
/// Any other failure is an error. A file that exists but cannot be read must
/// never count as empty: the caller would then write over text it never saw.
fn read_if_present(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| {
            format!(
                "cannot read {} as text, so it stays as it is",
                path.display()
            )
        }),
    }
}

/// Follows the symbolic links at `path` to the first path that is not one.
///
/// The file at the end may be missing. A link to a file that does not exist
/// yet still names where that file goes, and `fs::canonicalize` fails on it.
/// A chain that never ends, such as a loop of links, is an error.
fn link_target(path: &Path) -> Result<PathBuf> {
    const MAX_LINKS: usize = 40;

    let mut current = path.to_path_buf();
    let mut followed = 0;
    while fs::symlink_metadata(&current).is_ok_and(|meta| meta.file_type().is_symlink()) {
        followed += 1;
        if followed > MAX_LINKS {
            bail!(
                "{} passes through more than {MAX_LINKS} symbolic links",
                path.display()
            );
        }
        let link = fs::read_link(&current)
            .with_context(|| format!("cannot read the link {}", current.display()))?;
        current = match current.parent() {
            Some(parent) => parent.join(link),
            None => link,
        };
    }
    Ok(current)
}

/// Replaces `path` with `text` through a temporary file and a rename, so that
/// a run that stops part-way leaves the previous file whole.
///
/// When `path` is a symbolic link, the file it names is replaced and the link
/// stays, which is what a plain write did. The mode of an existing file is
/// kept.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    // Replace the file a link names, so that the link itself stays.
    let target = link_target(path)?;
    let parent = target
        .parent()
        .with_context(|| format!("{} has no parent directory", target.display()))?;
    let name = target
        .file_name()
        .with_context(|| format!("{} has no file name", target.display()))?
        .to_string_lossy();
    let staged = parent.join(format!(".{name}.brainmaker-{}.tmp", std::process::id()));

    let result = (|| -> Result<()> {
        fs::write(&staged, text).with_context(|| format!("cannot write {}", staged.display()))?;
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(&staged, meta.permissions())
                .with_context(|| format!("cannot set the mode on {}", staged.display()))?;
        }
        fs::rename(&staged, &target)
            .with_context(|| format!("cannot move {} to {}", staged.display(), target.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

/// Adds or removes the `SessionStart` hook. Returns true when the file changed.
///
/// `prefix` is the [`command_prefix`] to install, or `None` to remove the hook.
fn write_settings(claude: &Path, prefix: Option<&str>) -> Result<bool> {
    let path = claude.join("settings.json");
    let mut root: Map<String, Value> = match read_if_present(&path)? {
        Some(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not a JSON object", path.display()))?,
        _ => Map::new(),
    };
    let before = root.clone();

    // Drop any hook entry this module wrote before, so a second run neither
    // duplicates the hook nor keeps a command from an older version. Single
    // entries go, so a command the user added to the same group stays. A group
    // goes only when this run removed its last entry: a group that was already
    // empty belongs to the user.
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("the \"hooks\" value in {} is not an object", path.display());
    };
    if let Some(Value::Array(groups)) = hooks.get_mut("SessionStart") {
        groups.retain_mut(|group| {
            let Some(Value::Array(entries)) = group.get_mut("hooks") else {
                return true;
            };
            let held = entries.len();
            entries.retain(|entry| !is_ours(entry));
            let removed_any = entries.len() < held;
            !(removed_any && entries.is_empty())
        });
        if groups.is_empty() {
            hooks.remove("SessionStart");
        }
    }

    if let Some(prefix) = prefix {
        let group = json!({
            "hooks": [
                {
                    "type": "command",
                    "command": format!("{prefix} sync --quiet --no-update-check # {MARKER}"),
                    "timeout": 60
                },
                {
                    "type": "command",
                    "command": format!("{prefix} session-context # {MARKER}"),
                    "timeout": 15
                }
            ]
        });
        match hooks.entry("SessionStart") {
            serde_json::map::Entry::Occupied(mut slot) => {
                if let Some(list) = slot.get_mut().as_array_mut() {
                    list.push(group);
                } else {
                    bail!("\"hooks.SessionStart\" in {} is not a list", path.display());
                }
            }
            serde_json::map::Entry::Vacant(slot) => {
                slot.insert(json!([group]));
            }
        }
    }

    if root
        .get("hooks")
        .is_some_and(|value| value.as_object().is_some_and(|object| object.is_empty()))
    {
        root.remove("hooks");
    }

    if root == before {
        return Ok(false);
    }

    fs::create_dir_all(claude).with_context(|| format!("cannot create {}", claude.display()))?;
    let text = serde_json::to_string_pretty(&Value::Object(root))?;
    write_atomic(&path, &format!("{text}\n"))?;
    Ok(true)
}

/// True when this module wrote the hook entry.
///
/// Every command that `link` writes ends with the marker as a shell comment.
/// A command that only mentions the marker somewhere else is the user's own.
fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.trim_end().ends_with(&format!("# {MARKER}")))
}

/// Adds or removes the marked block in `CLAUDE.md`. Returns true on a change.
///
/// `prefix` is the [`command_prefix`] the block tells the reader to run, or
/// `None` to remove the block. The block names the full command, because a
/// bare `brainmaker unlink` fails: nothing puts the binary on `PATH`.
fn write_briefing(content: &Path, claude: &Path, prefix: Option<&str>) -> Result<bool> {
    let path = claude.join("CLAUDE.md");
    let existing = read_if_present(&path)?.unwrap_or_default();
    let stripped = strip_block(&existing, &path)?;

    let updated = if let Some(prefix) = prefix {
        let block = format!(
            "{BLOCK_START}\n\
             # Shared content\n\n\
             `brainmaker` keeps `{}` in step with the team's server. It holds the shared \
             briefing, the wiki, and the agent memory. Read `CLAUDE.md` there before any \
             non-trivial action, and follow the `[[wiki/...]]` pointers it gives rather than \
             loading the whole directory.\n\n\
             Run `{prefix} unlink` to remove this block and the linked skills.\n\
             {BLOCK_END}",
            content.display()
        );
        if stripped.trim().is_empty() {
            format!("{block}\n")
        } else {
            format!("{}\n\n{block}\n", stripped.trim_end())
        }
    } else if stripped.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n", stripped.trim_end())
    };

    if updated == existing {
        return Ok(false);
    }

    fs::create_dir_all(claude).with_context(|| format!("cannot create {}", claude.display()))?;
    if updated.is_empty() && path.is_file() {
        // The file held nothing but our block, so leave no empty file behind.
        fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
        return Ok(true);
    }
    write_atomic(&path, &updated)?;
    Ok(true)
}

/// Returns `text` without the marked block, if it holds one.
///
/// The text must hold no marker, or one start marker followed by one end
/// marker. Any other shape is an error and removes nothing: a guess at which
/// markers belong together would delete text that the user wrote.
fn strip_block(text: &str, path: &Path) -> Result<String> {
    let starts = text.matches(BLOCK_START).count();
    let ends = text.matches(BLOCK_END).count();
    match (starts, ends, text.find(BLOCK_START), text.find(BLOCK_END)) {
        (0, 0, _, _) => Ok(text.to_string()),
        (1, 1, Some(start), Some(end)) if start < end => {
            let after = end + BLOCK_END.len();
            Ok(format!("{}{}", &text[..start], &text[after..]))
        }
        _ => bail!(
            "{} holds a damaged brainmaker block: {starts} start marker(s) and {ends} end \
             marker(s). Remove the lines that hold \"{MARKER}\" from the file by hand, then \
             run the command again",
            path.display()
        ),
    }
}

/// Prints what the run changed.
///
/// `installing` picks the verb, so an unlink does not report that it wrote the
/// very things it removed.
pub fn describe(report: &Report, installing: bool, log: &dyn Fn(&str)) {
    let verb = if installing { "Wrote" } else { "Removed" };
    for name in &report.linked {
        log(&format!("Linked the skill {name}."));
    }
    for name in &report.removed {
        log(&format!("Unlinked the skill {name}."));
    }
    if !report.unchanged.is_empty() {
        log(&format!(
            "{} skill(s) were already linked.",
            report.unchanged.len()
        ));
    }
    for (name, reason) in &report.skipped {
        log(&format!("Skipped the skill {name}: {reason}."));
    }
    if report.settings_changed {
        log(&format!("{verb} the SessionStart hook in settings.json."));
    }
    if report.briefing_changed {
        log(&format!("{verb} the shared-content block in CLAUDE.md."));
    }
    if report.agent_changed {
        log(&format!(
            "{verb} the hourly LaunchAgent {}.",
            schedule::LABEL
        ));
    }
    if !report.changed() {
        log("Nothing to change.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Route, Server};

    #[test]
    fn the_session_context_names_the_operator_and_the_outbox() {
        let base = temp_dir("context-outbox");
        let config = Config::for_test(&base, "http://127.0.0.1:9");
        write(&config.content_dir().join("CLAUDE.md"), "# Briefing\n");
        write(&config.operator_file(), "gabriele\n");
        write(&config.outbox_dir().join("2026-09-30-a.md"), "note");
        write(&config.rejected_dir().join("2026-09-29-b.md"), "note");

        let json: Value = serde_json::from_str(&session_context(&config).unwrap()).unwrap();
        let text = json["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();

        assert!(text.contains("# Briefing"), "{text}");
        assert!(text.contains("The operator is `gabriele`"), "{text}");
        assert!(
            text.contains(&config.outbox_dir().display().to_string()),
            "{text}"
        );
        assert!(
            text.contains("1 note(s) wait there now, and 1 were rejected"),
            "{text}"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_session_context_says_when_no_operator_is_named() {
        let base = temp_dir("context-no-operator");
        let config = Config::for_test(&base, "http://127.0.0.1:9");
        write(&config.content_dir().join("CLAUDE.md"), "# Briefing\n");
        // A file that someone edited into another shape names nobody.
        write(&config.operator_file(), "Gabriele Rossi\n");

        let context = session_context(&config).unwrap();

        assert!(context.contains("named no operator"), "{context}");
        assert!(!context.contains("Gabriele Rossi"), "{context}");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_session_with_no_content_gets_no_context_at_all() {
        let base = temp_dir("context-empty");
        let config = Config::for_test(&base, "http://127.0.0.1:9");
        write(&config.operator_file(), "gabriele\n");
        assert_eq!(session_context(&config).unwrap(), "{}");
        fs::remove_dir_all(&base).ok();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "brainmaker-link-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn content_with_skills(root: &Path, names: &[&str]) {
        for name in names {
            write(
                &root
                    .join(".claude")
                    .join("skills")
                    .join(name)
                    .join("SKILL.md"),
                "---\nname: x\n---\n",
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn copying_through_a_link_to_the_target_leaves_the_target_alone() {
        // `link` run through a symbolic link to the installed copy used to
        // remove that copy and then fail to copy it onto itself.
        let base = temp_dir("copy-self");
        let target = base.join("bin").join("brainmaker");
        write(&target, "installed\n");
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();

        copy_program(&alias, &target).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "installed\n");
        assert!(same_file(&alias, &target));
        assert!(!same_file(&base.join("absent"), &target));
        fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn copies_a_binary_from_elsewhere_and_makes_it_executable() {
        use std::os::unix::fs::PermissionsExt;

        let base = temp_dir("copy-new");
        let source = base.join("Downloads").join("brainmaker");
        write(&source, "new\n");
        let target = base.join("root").join("bin").join("brainmaker");
        write(&target, "old\n");

        copy_program(&source, &target).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "got {mode:o}");
        // No staging file is left beside the target.
        let leftovers: Vec<_> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["brainmaker".to_string()]);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_command_prefix_quotes_the_path_and_names_the_root() {
        assert_eq!(
            command_prefix(Path::new("/opt/bm/bin/brainmaker"), None).unwrap(),
            "\"/opt/bm/bin/brainmaker\""
        );
        assert_eq!(
            command_prefix(
                Path::new("/My Apps/brainmaker"),
                Some(Path::new("/My Root"))
            )
            .unwrap(),
            "\"/My Apps/brainmaker\" --dir \"/My Root\""
        );
    }

    #[test]
    fn escapes_the_characters_that_a_shell_expands() {
        assert_eq!(shell_quote("/a$b").unwrap(), "\"/a\\$b\"");
        assert_eq!(shell_quote("/a`b").unwrap(), "\"/a\\`b\"");
        assert_eq!(shell_quote("/a\"b").unwrap(), "\"/a\\\"b\"");
        assert_eq!(shell_quote("/a\\b").unwrap(), "\"/a\\\\b\"");
    }

    #[test]
    fn writes_a_non_ascii_path_as_it_stands() {
        // macOS stores a file name in decomposed form: `e`, then a combining
        // accent. The `{:?}` formatter wrote that accent as the text `\u{301}`,
        // which a shell does not read.
        let quoted = shell_quote("/Users/rene\u{301}/root").unwrap();
        assert!(quoted.contains('\u{301}'), "{quoted}");
        assert!(!quoted.contains("\\u{"), "{quoted}");
        assert_eq!(quoted, "\"/Users/rene\u{301}/root\"");
    }

    #[test]
    fn keeps_a_single_quote_and_a_space() {
        assert_eq!(shell_quote("/My App's/x").unwrap(), "\"/My App's/x\"");
    }

    #[test]
    fn refuses_a_path_that_holds_a_line_break() {
        assert!(shell_quote("/a\nb").is_err());
        assert!(command_prefix(Path::new("/a\nb"), None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_shell_reads_the_quoted_path_back() {
        // The shell that runs the hook is the judge of the quoting, so run one.
        // The backtick path would run `echo x` if the quoting let it through.
        for path in [
            "/a b",
            "/a$HOME",
            "/a`echo x`b",
            "/a\"b",
            "/a\\b",
            "/rene\u{301}",
        ] {
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("printf %s {}", shell_quote(path).unwrap()))
                .output()
                .unwrap();
            assert_eq!(
                output.stdout,
                path.as_bytes(),
                "the shell read {path:?} back as {:?}, stderr {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn the_briefing_block_names_a_command_that_can_run() {
        // A bare `brainmaker unlink` fails: nothing puts the binary on PATH.
        let base = temp_dir("briefing-command");
        let content = base.join("content");
        let claude = base.join("claude");
        let prefix = command_prefix(
            Path::new("/opt/bm/bin/brainmaker"),
            Some(Path::new("/opt/bm")),
        )
        .unwrap();

        write_briefing(&content, &claude, Some(&prefix)).unwrap();
        let text = fs::read_to_string(claude.join("CLAUDE.md")).unwrap();
        assert!(
            text.contains("Run `\"/opt/bm/bin/brainmaker\" --dir \"/opt/bm\" unlink`"),
            "{text}"
        );
        assert!(!text.contains("Run `brainmaker unlink`"), "{text}");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn finds_only_a_directory_that_holds_a_skill_file() {
        let base = temp_dir("shipped");
        content_with_skills(&base, &["query", "ingest"]);
        // A directory with no SKILL.md is not a skill.
        fs::create_dir_all(base.join(".claude").join("skills").join("notes")).unwrap();

        let found = shipped_skills(&base.join(".claude").join("skills")).unwrap();
        assert_eq!(
            found.keys().cloned().collect::<Vec<_>>(),
            vec!["ingest".to_string(), "query".to_string()]
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn links_every_skill_and_says_so_only_once() {
        let base = temp_dir("links");
        let content = base.join("content");
        let claude = base.join("claude");
        content_with_skills(&content, &["query", "today"]);

        let mut first = Report::default();
        link_skills(&content, &claude, &mut first).unwrap();
        assert_eq!(first.linked, vec!["query".to_string(), "today".to_string()]);
        assert!(
            claude
                .join("skills")
                .join("query")
                .join("SKILL.md")
                .is_file()
        );

        // A second run changes nothing.
        let mut second = Report::default();
        link_skills(&content, &claude, &mut second).unwrap();
        assert!(second.linked.is_empty());
        assert_eq!(second.unchanged.len(), 2);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn refuses_to_replace_a_skill_the_user_owns() {
        let base = temp_dir("owned");
        let content = base.join("content");
        let claude = base.join("claude");
        content_with_skills(&content, &["query"]);
        write(
            &claude.join("skills").join("query").join("SKILL.md"),
            "mine\n",
        );

        let mut report = Report::default();
        link_skills(&content, &claude, &mut report).unwrap();
        assert!(report.linked.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(
            fs::read_to_string(claude.join("skills").join("query").join("SKILL.md")).unwrap(),
            "mine\n"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn unlink_removes_our_links_and_leaves_the_rest() {
        let base = temp_dir("unlink");
        let content = base.join("content");
        let claude = base.join("claude");
        content_with_skills(&content, &["query"]);
        write(
            &claude.join("skills").join("mine").join("SKILL.md"),
            "mine\n",
        );

        let mut linked = Report::default();
        link_skills(&content, &claude, &mut linked).unwrap();
        let mut removed = Report::default();
        unlink_skills(&content, &claude, &mut removed).unwrap();

        assert_eq!(removed.removed, vec!["query".to_string()]);
        assert!(
            claude
                .join("skills")
                .join("mine")
                .join("SKILL.md")
                .is_file()
        );
        assert!(!claude.join("skills").join("query").exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn adds_the_hook_once_and_removes_it_again() {
        let base = temp_dir("settings");
        let claude = base.join("claude");
        fs::create_dir_all(&claude).unwrap();
        write(
            &claude.join("settings.json"),
            r#"{"model": "opus", "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "echo mine"}]}]}}"#,
        );

        assert!(
            write_settings(
                &claude,
                Some(&command_prefix(Path::new("/opt/bm/bin/brainmaker"), None).unwrap())
            )
            .unwrap()
        );
        assert!(
            !write_settings(
                &claude,
                Some(&command_prefix(Path::new("/opt/bm/bin/brainmaker"), None).unwrap())
            )
            .unwrap(),
            "second run is a no-op"
        );

        let text = fs::read_to_string(claude.join("settings.json")).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let groups = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "the user's own group survives");
        assert_eq!(value["model"], "opus", "unrelated settings survive");
        assert_eq!(text.matches(MARKER).count(), 2);

        assert!(write_settings(&claude, None).unwrap());
        let value: Value =
            serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(value["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert_eq!(value["model"], "opus");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn unlink_keeps_a_command_the_user_added_to_our_group() {
        // `unlink` used to drop the whole group, so a command the user had put
        // beside ours went with it.
        let base = temp_dir("settings-shared-group");
        let claude = base.join("claude");
        let path = claude.join("settings.json");
        let prefix = command_prefix(Path::new("/opt/bm/bin/brainmaker"), None).unwrap();
        assert!(write_settings(&claude, Some(&prefix)).unwrap());

        let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value
            .pointer_mut("/hooks/SessionStart/0/hooks")
            .and_then(Value::as_array_mut)
            .unwrap()
            .push(json!({"type": "command", "command": "echo mine"}));
        write(&path, &serde_json::to_string_pretty(&value).unwrap());

        assert!(write_settings(&claude, None).unwrap());

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("echo mine"), "{text}");
        assert!(!text.contains(MARKER), "{text}");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn keeps_a_command_that_only_mentions_the_marker() {
        // A command that names the marker anywhere but at the end is the
        // user's own, and `unlink` used to drop its group.
        let base = temp_dir("settings-mention");
        let claude = base.join("claude");
        let path = claude.join("settings.json");
        let original = r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "grep brainmaker-link ~/.claude/settings.json"}]}]}}"#;
        write(&path, original);

        assert!(!write_settings(&claude, None).unwrap());

        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_hook_names_the_binary_by_its_full_path() {
        // A bare name fails: nothing puts this binary on PATH, and a shell
        // answers "brainmaker: command not found" at every session start.
        let base = temp_dir("hook-path");
        let claude = base.join("claude");
        let program = base.join("bin").join("brainmaker");

        write_settings(&claude, Some(&command_prefix(&program, None).unwrap())).unwrap();
        let text = fs::read_to_string(claude.join("settings.json")).unwrap();

        // Every command starts with the full path, quoted for the shell. The
        // text of the file cannot be searched for the path itself: on Windows
        // the shell quoting and then JSON each double a backslash.
        let prefix = command_prefix(&program, None).unwrap();
        assert!(prefix.contains("hook-path"), "{prefix}");
        let settings: serde_json::Value = serde_json::from_str(&text).unwrap();
        let hooks = settings["hooks"]["SessionStart"][0]["hooks"]
            .as_array()
            .unwrap();
        assert!(!hooks.is_empty(), "{text}");
        for hook in hooks {
            let command = hook["command"].as_str().unwrap();
            assert!(command.starts_with(&format!("{prefix} ")), "{command}");
        }
        assert!(
            !text.contains("\"brainmaker sync"),
            "the command must not start with a bare name: {text}"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_hook_quotes_a_path_that_holds_a_space() {
        let base = temp_dir("hook-space");
        let claude = base.join("claude");
        let program = base.join("My Apps").join("brainmaker");

        write_settings(&claude, Some(&command_prefix(&program, None).unwrap())).unwrap();
        let value: Value =
            serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap())
                .unwrap();
        let command = value["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.starts_with('"'), "unquoted path in {command}");
        assert!(command.contains("My Apps"));
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn writes_the_hook_into_a_file_that_does_not_exist_yet() {
        let base = temp_dir("fresh-settings");
        let claude = base.join("claude");

        assert!(
            write_settings(
                &claude,
                Some(&command_prefix(Path::new("/opt/bm/bin/brainmaker"), None).unwrap())
            )
            .unwrap()
        );
        let value: Value =
            serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(value["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn keeps_the_text_the_user_wrote_around_the_block() {
        let base = temp_dir("briefing");
        let content = base.join("content");
        let claude = base.join("claude");
        fs::create_dir_all(&claude).unwrap();
        write(&claude.join("CLAUDE.md"), "# Mine\n\nKeep this.\n");

        assert!(write_briefing(&content, &claude, Some("\"/opt/bm/bin/brainmaker\"")).unwrap());
        let text = fs::read_to_string(claude.join("CLAUDE.md")).unwrap();
        assert!(text.starts_with("# Mine\n\nKeep this."));
        assert!(text.contains(BLOCK_START) && text.contains(BLOCK_END));

        assert!(
            !write_briefing(&content, &claude, Some("\"/opt/bm/bin/brainmaker\"")).unwrap(),
            "idempotent"
        );

        assert!(write_briefing(&content, &claude, None).unwrap());
        assert_eq!(
            fs::read_to_string(claude.join("CLAUDE.md")).unwrap(),
            "# Mine\n\nKeep this.\n"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn removes_a_briefing_file_that_held_only_our_block() {
        let base = temp_dir("briefing-only");
        let content = base.join("content");
        let claude = base.join("claude");

        assert!(write_briefing(&content, &claude, Some("\"/opt/bm/bin/brainmaker\"")).unwrap());
        assert!(claude.join("CLAUDE.md").is_file());
        assert!(write_briefing(&content, &claude, None).unwrap());
        assert!(!claude.join("CLAUDE.md").exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn refuses_a_briefing_that_is_not_text() {
        // One invalid byte used to read as an empty file, and `link` then wrote
        // its block over everything the user had kept in `CLAUDE.md`.
        let base = temp_dir("briefing-not-text");
        let content = base.join("content");
        let claude = base.join("claude");
        fs::create_dir_all(&claude).unwrap();
        let bytes: &[u8] = b"# Mine\n\xff\xfe\n";
        fs::write(claude.join("CLAUDE.md"), bytes).unwrap();

        let result = write_briefing(&content, &claude, Some("\"/opt/bm/bin/brainmaker\""));

        assert!(
            result.is_err(),
            "a briefing that cannot be read is an error"
        );
        assert_eq!(fs::read(claude.join("CLAUDE.md")).unwrap(), bytes);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn keeps_the_key_order_of_the_settings() {
        // Without `preserve_order` every object came back sorted by key, which
        // turned a hand-ordered file into one large diff.
        let base = temp_dir("settings-order");
        let claude = base.join("claude");
        fs::create_dir_all(&claude).unwrap();
        write(
            &claude.join("settings.json"),
            r#"{"zeta": 1, "model": "opus", "alpha": {"b": 1, "a": 2}}"#,
        );

        assert!(
            write_settings(
                &claude,
                Some(&command_prefix(Path::new("/opt/bm/bin/brainmaker"), None).unwrap())
            )
            .unwrap()
        );

        let text = fs::read_to_string(claude.join("settings.json")).unwrap();
        let at = |key: &str| {
            text.find(key)
                .unwrap_or_else(|| panic!("{key} is missing from {text}"))
        };
        assert!(
            at("\"zeta\"") < at("\"model\""),
            "top level reordered: {text}"
        );
        assert!(
            at("\"model\"") < at("\"alpha\""),
            "top level reordered: {text}"
        );
        assert!(at("\"b\"") < at("\"a\""), "nested keys reordered: {text}");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn leaves_no_temporary_file_beside_the_settings() {
        let base = temp_dir("settings-temp");
        let claude = base.join("claude");

        assert!(
            write_settings(
                &claude,
                Some(&command_prefix(Path::new("/opt/bm/bin/brainmaker"), None).unwrap())
            )
            .unwrap()
        );

        let names: Vec<String> = fs::read_dir(&claude)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.contains(&"settings.json".to_string()),
            "got {names:?}"
        );
        assert!(
            names.iter().all(|name| !name.contains(".brainmaker-")),
            "a temporary file was left behind: {names:?}"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_a_link_and_keeps_the_link() {
        // A dotfiles manager keeps the real file elsewhere and links it in.
        // Renaming over the link would swap it for a regular file.
        let base = temp_dir("settings-link");
        let claude = base.join("claude");
        let real = base.join("dotfiles").join("settings.json");
        write(&real, r#"{"model": "opus"}"#);
        fs::create_dir_all(&claude).unwrap();
        let link = claude.join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(
            write_settings(
                &claude,
                Some(&command_prefix(Path::new("/opt/bm/bin/brainmaker"), None).unwrap())
            )
            .unwrap()
        );

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link was replaced by a regular file"
        );
        let text = fs::read_to_string(&real).unwrap();
        assert!(
            text.contains("brainmaker-link"),
            "the file behind the link was not written: {text}"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_a_dangling_link_and_keeps_the_link() {
        // The file behind the link is not there yet, and a plain write would
        // have created it. The target is relative, as a dotfiles script makes it.
        let base = temp_dir("settings-dangling");
        let claude = base.join("claude");
        let dotfiles = base.join("dotfiles");
        fs::create_dir_all(&claude).unwrap();
        fs::create_dir_all(&dotfiles).unwrap();
        let link = claude.join("settings.json");
        std::os::unix::fs::symlink("../dotfiles/settings.json", &link).unwrap();

        assert!(
            write_settings(
                &claude,
                Some(&command_prefix(Path::new("/opt/bm/bin/brainmaker"), None).unwrap())
            )
            .unwrap()
        );

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link was replaced by a regular file"
        );
        let real = dotfiles.join("settings.json");
        assert!(real.is_file(), "the file behind the link was not created");
        let text = fs::read_to_string(&real).unwrap();
        assert!(
            text.contains("brainmaker-link"),
            "the file behind the link was not written: {text}"
        );
        fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn keeps_the_mode_of_the_settings() {
        use std::os::unix::fs::PermissionsExt;

        let base = temp_dir("settings-mode");
        let claude = base.join("claude");
        let path = claude.join("settings.json");
        write(&path, r#"{"model": "opus"}"#);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(
            write_settings(
                &claude,
                Some(&command_prefix(Path::new("/opt/bm/bin/brainmaker"), None).unwrap())
            )
            .unwrap()
        );

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn strips_a_block_and_leaves_text_that_holds_none() {
        let path = Path::new("CLAUDE.md");
        assert_eq!(strip_block("plain", path).unwrap(), "plain");
        let text = format!("a\n{BLOCK_START}\nx\n{BLOCK_END}\nb");
        assert_eq!(strip_block(&text, path).unwrap(), "a\n\nb");
    }

    #[test]
    fn refuses_a_start_marker_with_no_end() {
        let text = format!("mine\n{BLOCK_START}\nx\n");

        let error = strip_block(&text, Path::new("CLAUDE.md")).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("1 start marker(s) and 0 end marker(s)"),
            "{error}"
        );
    }

    #[test]
    fn refuses_an_end_marker_before_the_start() {
        let text = format!("{BLOCK_END}\nmine\n{BLOCK_START}\n");

        let error = strip_block(&text, Path::new("CLAUDE.md")).unwrap_err();

        assert!(
            error.to_string().contains("damaged brainmaker block"),
            "{error}"
        );
    }

    #[test]
    fn refuses_two_blocks() {
        let text = format!("{BLOCK_START}\na\n{BLOCK_END}\nmine\n{BLOCK_START}\nb\n{BLOCK_END}\n");

        let error = strip_block(&text, Path::new("CLAUDE.md")).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("2 start marker(s) and 2 end marker(s)"),
            "{error}"
        );
    }

    #[test]
    fn a_damaged_block_leaves_the_briefing_as_it_is() {
        // With the end line deleted by hand, the next run appended a second
        // block, and the run after that paired the first start marker with the
        // new end marker and deleted the text between them.
        let base = temp_dir("briefing-damaged");
        let content = base.join("content");
        let claude = base.join("claude");
        let original = format!("# Mine\n\nKeep this.\n\n{BLOCK_START}\nleft over\n\nAlso mine.\n");
        write(&claude.join("CLAUDE.md"), &original);

        let result = write_briefing(&content, &claude, Some("\"/opt/bm/bin/brainmaker\""));

        assert!(result.is_err(), "a damaged block is an error");
        assert_eq!(
            fs::read_to_string(claude.join("CLAUDE.md")).unwrap(),
            original
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn keeps_a_file_that_fits_under_its_cap() {
        assert_eq!(cut("short", 1024, "x.md"), "short");
    }

    #[test]
    fn cuts_a_file_past_its_cap_and_says_so() {
        let body = "x".repeat(3000);
        let out = cut(&body, 1024, "agent-memory/OPEN-THREADS.md");
        assert!(out.starts_with(&"x".repeat(1024)));
        assert!(out.contains("cut at 1 KiB of 3 KiB"));
        assert!(out.contains("agent-memory/OPEN-THREADS.md"));
    }

    #[test]
    fn cuts_a_multi_byte_file_without_panicking() {
        // 1024 lands inside a two-byte character, so a naive truncate panics.
        let body = "é".repeat(2000);
        let out = cut(&body, 1025, "x.md");
        assert!(out.contains("cut at 1 KiB"));
    }

    #[test]
    fn every_per_file_cap_fits_inside_the_overall_cap() {
        let total: usize = CONTEXT_FILES.iter().map(|(_, _, cap)| cap).sum();
        assert!(
            total <= MAX_CONTEXT_BYTES,
            "the per-file caps total {total}, past the overall {MAX_CONTEXT_BYTES}"
        );
    }

    #[test]
    fn cuts_only_on_a_character_boundary() {
        let text = "é".repeat(100);
        // 5 is inside the third character, whose bytes are 4 and 5.
        assert_eq!(floor_char_boundary(&text, 5), 4);
        assert_eq!(floor_char_boundary(&text, 4), 4);
        assert_eq!(floor_char_boundary("abc", 99), 3);
    }

    // ------------------------------------------------------------ roles ---

    const PREFIX: &str = "\"/opt/bm/bin/brainmaker\" --dir \"/opt/bm\"";

    fn quiet(_: &str) {}

    /// A token endpoint that grants `outbox:read` and `outbox:write`, or
    /// answers `invalid_scope`, as `read` and `write` say.
    fn issuer(read: bool, write: bool) -> Server {
        let answer = |scope: &str, granted: bool| {
            if granted {
                Route::token(scope, r#"{"access_token":"t","expires_in":600}"#)
            } else {
                Route::token(scope, r#"{"error":"invalid_scope"}"#).status(400)
            }
        };
        Server::start(vec![
            answer(auth::SCOPE_OUTBOX_READ, read),
            answer(auth::SCOPE_OUTBOX_WRITE, write),
        ])
    }

    fn scopes_asked(server: &Server) -> Vec<String> {
        server
            .received()
            .iter()
            .filter_map(|request| request.form("scope"))
            .collect()
    }

    #[test]
    fn a_credential_that_reads_and_cannot_send_is_the_admin() {
        let server = issuer(true, false);
        let base = temp_dir("role-admin");
        let config = Config::for_test_with_credentials(&base, &server.base(), &server.base());

        assert!(collects_without_sending(&config).unwrap());
        assert_eq!(scopes_asked(&server), ["outbox:read", "outbox:write"]);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn an_operator_credential_is_asked_once_and_gets_the_bridge() {
        let server = issuer(false, true);
        let base = temp_dir("role-operator");
        let config = Config::for_test_with_credentials(&base, &server.base(), &server.base());

        assert!(!collects_without_sending(&config).unwrap());
        assert_eq!(scopes_asked(&server), ["outbox:read"]);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_credential_with_both_scopes_gets_the_bridge() {
        let server = issuer(true, true);
        let base = temp_dir("role-both");
        let config = Config::for_test_with_credentials(&base, &server.base(), &server.base());

        assert!(!collects_without_sending(&config).unwrap());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_run_with_no_credential_gets_the_bridge_and_asks_nobody() {
        let server = issuer(true, false);
        let base = temp_dir("role-none");
        let config = Config::for_test(&base, &server.base());

        assert!(!collects_without_sending(&config).unwrap());
        assert!(scopes_asked(&server).is_empty());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_role_check_with_no_answer_changes_nothing() {
        let server = Server::start(vec![
            Route::token(auth::SCOPE_OUTBOX_READ, "the issuer is down").status(500),
        ]);
        let base = temp_dir("role-down");
        let config = Config::for_test_with_credentials(&base, &server.base(), &server.base());
        let claude = base.join("claude");
        let agents = Agents::unloaded(&base.join("LaunchAgents"));

        let error = link(&config, &claude, Some(&agents), &quiet).unwrap_err();

        assert!(
            format!("{error:#}").contains("so link changed nothing"),
            "{error:#}"
        );
        assert!(!claude.exists());
        assert!(!agents.plist().exists());
        assert!(!installed_program(config.root()).exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_admin_install_removes_the_bridge_and_writes_an_update_agent() {
        let base = temp_dir("admin-install");
        let config = Config::for_test(&base, "http://127.0.0.1:9");
        let content = config.content_dir();
        let claude = base.join("claude");
        let agents = Agents::unloaded(&base.join("LaunchAgents"));
        // An earlier link, with an operator's credential, wrote the bridge.
        content_with_skills(&content, &["query"]);
        link_skills(&content, &claude, &mut Report::default()).unwrap();
        write_settings(&claude, Some(PREFIX)).unwrap();
        write_briefing(&content, &claude, Some(PREFIX)).unwrap();

        let report = link_admin(&config, &claude, Some(&agents), PREFIX, &quiet).unwrap();

        assert_eq!(report.removed, vec!["query".to_string()]);
        assert!(report.settings_changed && report.briefing_changed && report.agent_changed);
        assert!(fs::symlink_metadata(claude.join("skills").join("query")).is_err());
        let settings = fs::read_to_string(claude.join("settings.json")).unwrap();
        assert!(!settings.contains(MARKER), "{settings}");
        assert!(!claude.join("CLAUDE.md").exists());
        let agent = fs::read_to_string(agents.plist()).unwrap();
        assert!(
            agent.contains(&format!("{PREFIX} self-update --quiet")),
            "{agent}"
        );
        assert!(!agent.contains(" sync "), "{agent}");
        assert!(!config.outbox_dir().exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_admin_install_needs_no_content_and_touches_no_claude_file() {
        let base = temp_dir("admin-empty");
        let config = Config::for_test(&base, "http://127.0.0.1:9");
        let claude = base.join("claude");
        let agents = Agents::unloaded(&base.join("LaunchAgents"));

        let report = link_admin(&config, &claude, Some(&agents), PREFIX, &quiet).unwrap();

        assert!(report.agent_changed);
        assert!(!report.settings_changed && !report.briefing_changed);
        assert!(!claude.exists());
        assert!(!config.content_dir().exists());
        let again = link_admin(&config, &claude, Some(&agents), PREFIX, &quiet).unwrap();
        assert!(!again.changed(), "a second run changes nothing");
        fs::remove_dir_all(&base).ok();
    }
}
