// SPDX-License-Identifier: GPL-3.0-or-later

//! Removes brainmaker from this machine.
//!
//! # What it removes
//!
//! 1. The bridge that `link` wrote into `~/.claude`, and its LaunchAgent, as
//!    `unlink` removes them. These go first: a hook or an agent left in place
//!    would name a program that is gone, and would then fail on every run.
//! 2. What brainmaker writes under the root: the content, the program copy
//!    that the hook runs, the state file, the sealed settings, the agent's
//!    log, the operator file, the push record, the four lock files, and any
//!    temporary file that a stopped run left behind.
//! 3. The root itself, once nothing else is left in it.
//!
//! # What it leaves
//!
//! The outbox, with every note in it, sent or not: the notes are the
//! operator's work, and a note that was never sent exists nowhere else. The
//! run says how many were never sent. The root then stays too, because the
//! outbox is in it.
//!
//! Everything it did not write. The root goes entry by entry, never as a
//! whole, so a file of yours inside it survives, and so does the directory
//! that holds it. A root reached through a symbolic link loses the link, and
//! the directory that the link names stays.
//!
//! A root that exists but holds neither a state file nor a sealed store is
//! not recognised, and the run then changes nothing at all. That includes the
//! bridge: the hook and the `CLAUDE.md` block carry no mark of the root they
//! belong to, so removing them would break a real install elsewhere. A `--dir`
//! that names a project with a `content` directory of its own removes nothing.
//! A root that does not exist is no such risk, and the bridge still goes.
//!
//! The program that runs `uninstall` stays too, unless it is the copy under
//! the root. It may live where a package manager expects it, so the run names
//! it and you delete it.
//!
//! # Why it reads no settings
//!
//! Every other command loads the sealed settings first. `uninstall` must also
//! work where the store no longer opens, such as after a switch between a
//! release build and a local one, and it must not import a provisioning file
//! only to remove what the import wrote.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::Layout;
use crate::link;
use crate::outbox;
use crate::schedule::{self, Agents};
use crate::secretstore;
use crate::state;

/// Prefix of every temporary file that `link` and `self-update` write beside
/// the program copy.
const TEMPORARY_PREFIX: &str = ".brainmaker";

/// What one run removed, and what it left.
#[derive(Debug, Default)]
pub struct Report {
    /// What came out of the Claude configuration directory.
    pub bridge: link::Report,
    /// Paths removed under the root, in the order they went. The `bin` and
    /// `confidential` directories that held some of them are not listed.
    pub removed: Vec<PathBuf>,
    /// True when the root directory itself is gone.
    pub root_removed: bool,
    /// Names the root still holds, when they kept it in place.
    pub left: Vec<String>,
    /// True when the root exists but carries no mark of brainmaker, so
    /// nothing under it was touched.
    pub unrecognised: bool,
}

/// The question `uninstall` asks before it removes anything.
///
/// `agents` names the LaunchAgent location, when this run removes one.
pub fn question(root: &Path, claude: &Path, agents: Option<&Agents>) -> String {
    let agent = agents
        .map(|agents| format!("  - the hourly LaunchAgent {}\n", agents.plist().display()))
        .unwrap_or_default();
    format!(
        "\
This removes brainmaker from this machine:

  - under {claude}: the skill links, the SessionStart hook, and the
    CLAUDE.md block that link wrote
{agent}  - under {root}: the content, the sealed settings, the state file, the
    agent log, the operator file, and the program copy that the hook runs

Nothing else in either directory changes. The outbox and the notes in it
stay. Changes that you made in the content are lost. The sealed settings hold the client credentials, so a new
install needs a brainmaker.env file from your administrator.

Remove brainmaker? [y/N] ",
        claude = claude.display(),
        root = root.display(),
    )
}

/// True when `answer` accepts the question: `y` or `yes`, in any case.
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Removes the bridge in `claude` and the agent in `agents`, then what
/// brainmaker wrote under the root of `layout`, then the root when it is
/// empty. Changes nothing when the root exists but is not recognised.
pub fn uninstall(
    layout: &Layout,
    claude: &Path,
    agents: Option<&Agents>,
    log: &dyn Fn(&str),
) -> Result<Report> {
    let root = layout.root();
    let mut report = Report::default();

    // Decide before the bridge goes: the hook and the CLAUDE.md block do not
    // name their root, so a wrong --dir would otherwise cut off a real install.
    let root_exists = fs::symlink_metadata(root).is_ok();
    if root_exists && !is_recognised(layout) {
        report.unrecognised = true;
        log(&format!(
            "Removed nothing: {} holds no state.json and no sealed \
             confidential/config.enc, the two files that mark a brainmaker root.",
            root.display()
        ));
        return Ok(report);
    }

    // Find the running program before anything goes: once its file is gone,
    // its path no longer resolves.
    let program = std::env::current_exe().ok();
    let runs_the_copy = program
        .as_deref()
        .is_some_and(|exe| link::same_file(exe, &link::installed_program(root)));

    report.bridge = link::remove_bridge(&layout.content_dir(), claude, agents)?;
    if report.bridge.changed() {
        link::describe(&report.bridge, false, log);
    }
    if root_exists {
        remove_root(layout, program.as_deref(), &mut report, log)?;
    }

    if !report.bridge.changed() && report.removed.is_empty() {
        log("Nothing to remove.");
    }
    if let Some(program) = program.filter(|_| !runs_the_copy) {
        log(&format!(
            "The program you ran stays at {}. Delete it yourself.",
            program.display()
        ));
    }
    if report.bridge.settings_changed {
        log("Restart any open Claude session: it keeps the hook it loaded when it started.");
    }
    Ok(report)
}

/// True when the root carries a mark that only brainmaker writes: a state
/// file of the shape `sync` writes, or a store with the sealed header.
fn is_recognised(layout: &Layout) -> bool {
    state::read(&layout.state_file()).is_some() || secretstore::is_sealed(&layout.store_path())
}

/// Removes what brainmaker wrote under the root, then the root when nothing
/// else is left in it.
///
/// The two marks go last. A run that stops part-way then leaves them in
/// place, and the next run still recognises the root and finishes the job.
fn remove_root(
    layout: &Layout,
    program: Option<&Path>,
    report: &mut Report,
    log: &dyn Fn(&str),
) -> Result<()> {
    let root = layout.root();
    let installed = link::installed_program(root);
    let bin = installed
        .parent()
        .context("the program copy has no parent directory")?;
    let state = layout.state_file();
    let store = layout.store_path();
    let confidential = store
        .parent()
        .context("the sealed store has no parent directory")?
        .to_path_buf();

    let operator = layout.operator_file();
    let push_state = layout.push_state_file();
    let mut paths = vec![
        layout.content_dir(),
        layout.staging_dir(),
        layout.trash_dir(),
        layout.download_file(),
        layout.lock_file(),
        layout.update_lock_file(),
        layout.outbox_lock_file(),
        layout.admin_lock_file(),
        schedule::log_path(root),
        outbox::temporary_path(&operator),
        operator,
        outbox::temporary_path(&push_state),
        push_state,
    ];
    paths.extend(program_files(bin, &installed)?);
    paths.extend([
        state::temporary_path(&state),
        state,
        secretstore::temporary_path(&store),
        store,
    ]);

    for path in paths {
        match remove(&path) {
            Ok(true) => {
                log(&format!("Removed {}.", path.display()));
                report.removed.push(path);
            }
            Ok(false) => {}
            // Windows refuses to delete a program while it runs.
            Err(error) if program.is_some_and(|exe| link::same_file(exe, &path)) => {
                log(&format!(
                    "notice: cannot remove {}, which is the program that runs now: {error:#}. \
                     Delete it once this command exits.",
                    path.display()
                ));
            }
            Err(error) => return Err(error),
        }
    }

    // The outbox stays, with every note in it.
    let unsent = outbox::counts_in(&layout.outbox_dir(), &layout.rejected_dir());
    if unsent.waiting + unsent.rejected > 0 {
        log(&format!(
            "Left {} note(s) that were never sent in {}: {} waiting, {} rejected.",
            unsent.waiting + unsent.rejected,
            layout.outbox_dir().display(),
            unsent.waiting,
            unsent.rejected
        ));
    }

    remove_if_empty(bin)?;
    remove_if_empty(&confidential)?;
    let target = fs::read_link(root).ok();
    let left = remove_if_empty(root)?;
    if left.is_empty() {
        report.root_removed = true;
        match target {
            Some(target) => log(&format!(
                "Removed the link {}. The directory it names stays at {}.",
                root.display(),
                target.display()
            )),
            None => log(&format!("Removed {}.", root.display())),
        }
    } else {
        log(&format!(
            "Left {} in place, because it still holds {}.",
            root.display(),
            left.join(", ")
        ));
        report.left = left;
    }
    Ok(())
}

/// The program copy in `bin`, and every temporary file that `link` and
/// `self-update` leave beside it.
fn program_files(bin: &Path, program: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(bin) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", bin.display()));
        }
    };

    let mut found = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("cannot read {}", bin.display()))?
            .path();
        let temporary = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(TEMPORARY_PREFIX));
        if temporary || path == program {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

/// Removes `path` when it exists: a directory with everything in it, or a
/// file. A symbolic link goes, and what it names stays. Returns true when
/// something was removed.
fn remove(path: &Path) -> Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", path.display()));
        }
    };
    if meta.is_symlink() {
        link::remove_link(path)
    } else if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("cannot remove {}", path.display()))?;
    Ok(true)
}

/// Removes the directory `path` when it is empty. Returns the names it still
/// holds otherwise, sorted. A missing directory holds nothing. A directory
/// reached through a symbolic link loses the link, and the directory that the
/// link names stays.
fn remove_if_empty(path: &Path) -> Result<Vec<String>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", path.display()));
        }
    };

    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read {}", path.display()))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    if names.is_empty() {
        let is_link = fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink());
        if is_link {
            link::remove_link(path)
        } else {
            fs::remove_dir(path)
        }
        .with_context(|| format!("cannot remove {}", path.display()))?;
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "brainmaker-uninstall-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn write(path: &Path, body: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn quiet(_: &str) {}

    /// Seals settings the way an import does.
    fn seal_store(layout: &Layout) {
        let sealed =
            secretstore::seal(b"BRAINMAKER_API_BASE=https://api.example.test/v1\n").unwrap();
        secretstore::write_owner_only(&layout.store_path(), &sealed).unwrap();
    }

    /// A root as `sync` and `link` leave it: content with one skill, the state
    /// file, the sealed store, and the program copy.
    fn installed(root: &Path) -> Layout {
        let layout = Layout::resolve(Some(root)).unwrap();
        write(
            &layout
                .content_dir()
                .join(".claude")
                .join("skills")
                .join("query")
                .join("SKILL.md"),
            b"---\nname: query\n---\n",
        );
        state::write(&layout.state_file(), &state::State::new("a1b2c3d4")).unwrap();
        seal_store(&layout);
        write(&link::installed_program(root), b"program\n");
        layout
    }

    #[cfg(unix)]
    #[test]
    fn removes_the_bridge_and_everything_under_the_root() {
        let base = temp_dir("all");
        let root = base.join("root");
        let claude = base.join("claude");
        let layout = installed(&root);
        // The bridge as link writes it, beside a hook and a note of the user's.
        fs::create_dir_all(claude.join("skills")).unwrap();
        std::os::unix::fs::symlink(
            layout
                .content_dir()
                .join(".claude")
                .join("skills")
                .join("query"),
            claude.join("skills").join("query"),
        )
        .unwrap();
        write(
            &claude.join("settings.json"),
            br#"{"hooks": {"SessionStart": [
                {"hooks": [{"type": "command", "command": "echo mine"}]},
                {"hooks": [{"type": "command", "command": "\"/x/bin/brainmaker\" session-context # brainmaker-link"}]}
            ]}}"#,
        );
        write(
            &claude.join("CLAUDE.md"),
            b"# Mine\n\n<!-- brainmaker-link: start -->\nShared.\n<!-- brainmaker-link: end -->\n",
        );

        let lines = RefCell::new(Vec::new());
        let log = |line: &str| lines.borrow_mut().push(line.to_string());
        let report = uninstall(&layout, &claude, None, &log).unwrap();

        assert!(report.root_removed, "left {:?}", report.left);
        assert!(!root.exists());
        assert_eq!(report.bridge.removed, vec!["query".to_string()]);
        assert!(fs::symlink_metadata(claude.join("skills").join("query")).is_err());
        let settings = fs::read_to_string(claude.join("settings.json")).unwrap();
        assert!(settings.contains("echo mine"), "{settings}");
        assert!(!settings.contains("brainmaker-link"), "{settings}");
        assert_eq!(
            fs::read_to_string(claude.join("CLAUDE.md")).unwrap(),
            "# Mine\n"
        );
        // An open session keeps the hook it loaded, so the run says so.
        assert!(
            lines
                .borrow()
                .iter()
                .any(|line| line.starts_with("Restart any open Claude session")),
            "{:?}",
            lines.borrow()
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn touches_nothing_under_a_root_that_carries_no_mark() {
        // A --dir that names some project must not cost it its content.
        let base = temp_dir("foreign");
        let root = base.join("site");
        let claude = base.join("claude");
        write(&root.join("content").join("post.md"), b"mine\n");
        write(&root.join("state.json"), br#"{"version": 3}"#);
        write(
            &root.join("confidential").join("config.enc"),
            b"another tool's store",
        );
        let layout = Layout::resolve(Some(&root)).unwrap();

        let report = uninstall(&layout, &claude, None, &quiet).unwrap();

        assert!(report.unrecognised);
        assert!(report.removed.is_empty());
        assert!(root.join("content").join("post.md").is_file());
        assert!(root.join("state.json").is_file());
        assert!(root.join("confidential").join("config.enc").is_file());
        assert!(!claude.exists(), "no Claude directory is created");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn keeps_the_bridge_when_the_root_carries_no_mark() {
        // The hook and the CLAUDE.md block do not name their root. A wrong
        // --dir must not cut off the real install that they belong to.
        let base = temp_dir("foreign-bridge");
        let root = base.join("site");
        let claude = base.join("claude");
        write(&root.join("content").join("post.md"), b"mine\n");
        let settings = br#"{"hooks": {"SessionStart": [
            {"hooks": [{"type": "command", "command": "\"/x/bin/brainmaker\" session-context # brainmaker-link"}]}
        ]}}"#;
        let briefing =
            b"# Mine\n\n<!-- brainmaker-link: start -->\nShared.\n<!-- brainmaker-link: end -->\n";
        write(&claude.join("settings.json"), settings);
        write(&claude.join("CLAUDE.md"), briefing);
        let layout = Layout::resolve(Some(&root)).unwrap();

        let report = uninstall(&layout, &claude, None, &quiet).unwrap();

        assert!(report.unrecognised);
        assert!(!report.bridge.changed(), "{:?}", report.bridge);
        assert_eq!(fs::read(claude.join("settings.json")).unwrap(), settings);
        assert_eq!(fs::read(claude.join("CLAUDE.md")).unwrap(), briefing);
        assert!(root.join("content").join("post.md").is_file());
        fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn removes_a_linked_root_but_not_the_directory_it_names() {
        let base = temp_dir("root-link");
        let real = base.join("real");
        let root = base.join("root");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &root).unwrap();
        let layout = installed(&root);

        let lines = RefCell::new(Vec::new());
        let log = |line: &str| lines.borrow_mut().push(line.to_string());
        let report = uninstall(&layout, &base.join("claude"), None, &log).unwrap();

        assert!(report.root_removed, "left {:?}", report.left);
        assert!(fs::symlink_metadata(&root).is_err(), "the link is gone");
        assert!(real.is_dir(), "the directory it names stays");
        assert_eq!(fs::read_dir(&real).unwrap().count(), 0, "and is empty");
        assert!(
            lines
                .borrow()
                .iter()
                .any(|line| line.starts_with("Removed the link ")),
            "{:?}",
            lines.borrow()
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn keeps_what_brainmaker_did_not_write_and_the_root_that_holds_it() {
        let base = temp_dir("mixed");
        let root = base.join("root");
        let layout = installed(&root);
        write(&root.join("notes.md"), b"mine\n");
        write(&root.join("bin").join("other-tool"), b"mine\n");
        write(&root.join("confidential").join("other.key"), b"mine\n");

        let report = uninstall(&layout, &base.join("claude"), None, &quiet).unwrap();

        assert!(!report.root_removed);
        assert_eq!(report.left, vec!["bin", "confidential", "notes.md"]);
        assert!(root.join("notes.md").is_file());
        assert!(root.join("bin").join("other-tool").is_file());
        assert!(root.join("confidential").join("other.key").is_file());
        assert!(!layout.content_dir().exists());
        assert!(!layout.state_file().exists());
        assert!(!layout.store_path().exists());
        assert!(!link::installed_program(&root).exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn removes_the_temporary_files_a_stopped_run_left() {
        let base = temp_dir("leftovers");
        let root = base.join("root");
        let layout = installed(&root);
        write(&layout.staging_dir().join("CLAUDE.md"), b"half\n");
        write(&layout.trash_dir().join("CLAUDE.md"), b"old\n");
        write(&layout.download_file(), b"zip\n");
        // The lock files that sync and self-update leave between runs, under
        // the names that docs/API.md gives.
        write(&root.join(".lock"), b"");
        write(&root.join(".update.lock"), b"");
        write(&root.join(".outbox.lock"), b"");
        write(&root.join(".admin.lock"), b"");
        write(&root.join("operator"), b"gabriele\n");
        write(&root.join("operator.tmp"), b"gabriele\n");
        write(&root.join("push.json"), b"{}\n");
        write(&root.join("push.json.tmp"), b"{}\n");
        write(&state::temporary_path(&layout.state_file()), b"{}\n");
        write(&secretstore::temporary_path(&layout.store_path()), b"x\n");
        // What link and self-update stage beside the program copy.
        for name in [
            ".brainmaker-42.new",
            ".brainmaker-update-42",
            ".brainmaker-probe-42",
            ".brainmaker-old",
        ] {
            write(&root.join("bin").join(name), b"\n");
        }

        let report = uninstall(&layout, &base.join("claude"), None, &quiet).unwrap();

        assert!(report.root_removed, "left {:?}", report.left);
        assert!(!root.join(".lock").exists());
        assert!(!root.join(".update.lock").exists());
        assert!(!root.exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn keeps_the_outbox_and_counts_the_notes_never_sent() {
        let base = temp_dir("outbox");
        let root = base.join("root");
        let layout = installed(&root);
        write(&layout.outbox_dir().join("2026-09-30-a.md"), b"waiting\n");
        write(&layout.rejected_dir().join("2026-09-29-b.md"), b"refused\n");
        write(
            &layout.rejected_dir().join("2026-09-29-b.md.reason.txt"),
            b"why\n",
        );
        let sent = layout.sent_dir().join("2026-09").join("2026-09-28-c.md");
        write(&sent, b"sent\n");
        write(&layout.operator_file(), b"gabriele\n");
        write(&layout.push_state_file(), b"{}\n");

        let lines = RefCell::new(Vec::new());
        let log = |line: &str| lines.borrow_mut().push(line.to_string());
        let report = uninstall(&layout, &base.join("claude"), None, &log).unwrap();

        assert!(!report.root_removed);
        assert_eq!(report.left, vec!["outbox".to_string()]);
        assert!(layout.outbox_dir().join("2026-09-30-a.md").is_file());
        assert!(layout.rejected_dir().join("2026-09-29-b.md").is_file());
        assert!(sent.is_file());
        assert!(!layout.operator_file().exists());
        assert!(!layout.push_state_file().exists());
        assert!(
            lines.borrow().iter().any(|line| line
                .starts_with("Left 2 note(s) that were never sent")
                && line.ends_with("1 waiting, 1 rejected.")),
            "{:?}",
            lines.borrow()
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn recognises_a_root_by_its_sealed_store_alone() {
        // An import writes the store before the first sync installs anything.
        let base = temp_dir("store-only");
        let root = base.join("root");
        let layout = Layout::resolve(Some(&root)).unwrap();
        seal_store(&layout);

        let lines = RefCell::new(Vec::new());
        let log = |line: &str| lines.borrow_mut().push(line.to_string());
        let report = uninstall(&layout, &base.join("claude"), None, &log).unwrap();

        assert!(report.root_removed);
        assert_eq!(report.removed, vec![layout.store_path()]);
        // The test binary is not the copy under the root, so it stays, and
        // the run names it.
        assert!(
            lines
                .borrow()
                .iter()
                .any(|line| line.starts_with("The program you ran stays at ")),
            "{:?}",
            lines.borrow()
        );
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_second_run_finds_nothing_to_remove() {
        let base = temp_dir("twice");
        let root = base.join("root");
        let claude = base.join("claude");
        let layout = installed(&root);
        uninstall(&layout, &claude, None, &quiet).unwrap();

        let lines = RefCell::new(Vec::new());
        let log = |line: &str| lines.borrow_mut().push(line.to_string());
        let second = uninstall(&layout, &claude, None, &log).unwrap();

        assert!(!second.bridge.changed());
        assert!(second.removed.is_empty());
        assert!(!second.root_removed && !second.unrecognised);
        assert!(
            lines
                .borrow()
                .iter()
                .any(|line| line == "Nothing to remove."),
            "{:?}",
            lines.borrow()
        );
        fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn removes_a_linked_content_directory_but_not_what_it_names() {
        let base = temp_dir("content-link");
        let root = base.join("root");
        let layout = installed(&root);
        let vault = base.join("vault");
        write(&vault.join("note.md"), b"mine\n");
        fs::remove_dir_all(layout.content_dir()).unwrap();
        std::os::unix::fs::symlink(&vault, layout.content_dir()).unwrap();

        uninstall(&layout, &base.join("claude"), None, &quiet).unwrap();

        assert!(!root.exists());
        assert_eq!(fs::read_to_string(vault.join("note.md")).unwrap(), "mine\n");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn only_y_or_yes_accepts_the_question() {
        for answer in ["y\n", "Y\n", "yes\n", " YES \r\n"] {
            assert!(is_yes(answer), "{answer:?}");
        }
        for answer in ["", "\n", "n\n", "no\n", "yess\n", "y es\n", "sure\n"] {
            assert!(!is_yes(answer), "{answer:?}");
        }
    }

    #[test]
    fn the_question_names_both_directories_and_defaults_to_no() {
        let text = question(Path::new("/opt/bm"), Path::new("/home/me/.claude"), None);
        assert!(text.contains("under /opt/bm:"), "{text}");
        assert!(text.contains("under /home/me/.claude:"), "{text}");
        assert!(!text.contains("LaunchAgent"), "{text}");
        assert!(text.ends_with("[y/N] "), "{text}");
    }

    #[test]
    fn the_question_names_the_agent_when_the_run_removes_one() {
        let agents = Agents::unloaded(Path::new("/home/me/Library/LaunchAgents"));
        let text = question(
            Path::new("/opt/bm"),
            Path::new("/home/me/.claude"),
            Some(&agents),
        );
        // The separator before the file name is the platform's own.
        let plist =
            Path::new("/home/me/Library/LaunchAgents").join(format!("{}.plist", schedule::LABEL));
        assert!(
            text.contains(&format!("the hourly LaunchAgent {}", plist.display())),
            "{text}"
        );
    }

    #[test]
    fn removes_the_agent_and_its_log() {
        let base = temp_dir("agent");
        let root = base.join("root");
        let layout = installed(&root);
        let agents = Agents::unloaded(&base.join("LaunchAgents"));
        schedule::install(
            &agents,
            "\"/x/bin/brainmaker\"",
            &root,
            schedule::Runs::UpdateAndSync,
            &quiet,
        )
        .unwrap();
        write(
            &schedule::log_path(&root),
            b"Mon Sep 28 17:00:00 CEST 2026\n",
        );

        let report = uninstall(&layout, &base.join("claude"), Some(&agents), &quiet).unwrap();

        assert!(report.bridge.agent_changed);
        assert!(!agents.plist().exists());
        assert!(report.root_removed, "left {:?}", report.left);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn keeps_the_agent_under_a_root_that_carries_no_mark() {
        // The agent, like the hook, may belong to a real install elsewhere.
        let base = temp_dir("agent-foreign");
        let root = base.join("site");
        write(&root.join("content").join("post.md"), b"mine\n");
        let layout = Layout::resolve(Some(&root)).unwrap();
        let agents = Agents::unloaded(&base.join("LaunchAgents"));
        schedule::install(
            &agents,
            "\"/x/bin/brainmaker\"",
            &base,
            schedule::Runs::UpdateAndSync,
            &quiet,
        )
        .unwrap();

        let report = uninstall(&layout, &base.join("claude"), Some(&agents), &quiet).unwrap();

        assert!(report.unrecognised);
        assert!(agents.plist().is_file());
        fs::remove_dir_all(&base).ok();
    }
}
