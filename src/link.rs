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
//! 2. A `SessionStart` hook in `~/.claude/settings.json`, which runs
//!    `brainmaker sync --quiet --no-update-check` and then
//!    `brainmaker session-context`.
//! 3. A marked block in `~/.claude/CLAUDE.md` that names the content
//!    directory.
//!
//! Every piece carries a marker, so `unlink` removes what `link` wrote and
//! leaves everything else alone. `link` never overwrites a file it did not
//! write: a skill name that already exists as a real directory is reported and
//! skipped.
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

use crate::config::Config;

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
}

/// The Claude configuration directory, `~/.claude`.
pub fn claude_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot find the home directory")?;
    Ok(home.join(".claude"))
}

/// Writes the bridge.
pub fn link(config: &Config, claude: &Path, log: &dyn Fn(&str)) -> Result<Report> {
    let content = config.content_dir();
    if !content.is_dir() {
        bail!(
            "{} does not exist; run brainmaker sync first",
            content.display()
        );
    }

    let mut report = Report::default();
    link_skills(&content, claude, &mut report)?;
    report.settings_changed = write_settings(claude, true)?;
    report.briefing_changed = write_briefing(&content, claude, true)?;
    describe(&report, true, log);
    Ok(report)
}

/// Removes the bridge.
pub fn unlink(config: &Config, claude: &Path, log: &dyn Fn(&str)) -> Result<Report> {
    let content = config.content_dir();
    let mut report = Report::default();
    unlink_skills(&content, claude, &mut report)?;
    report.settings_changed = write_settings(claude, false)?;
    report.briefing_changed = write_briefing(&content, claude, false)?;
    describe(&report, false, log);
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
    let mut full = format!("{header}{text}");

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
                    fs::remove_file(&target)
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
            fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
            report
                .removed
                .push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(())
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

/// Adds or removes the `SessionStart` hook. Returns true when the file changed.
fn write_settings(claude: &Path, install: bool) -> Result<bool> {
    let path = claude.join("settings.json");
    let mut root: Map<String, Value> = if path.is_file() {
        let text =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        if text.trim().is_empty() {
            Map::new()
        } else {
            serde_json::from_str(&text)
                .with_context(|| format!("{} is not a JSON object", path.display()))?
        }
    } else {
        Map::new()
    };
    let before = root.clone();

    // Drop any matcher group this module wrote before, so a second run neither
    // duplicates the hook nor keeps a command from an older version.
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("the \"hooks\" value in {} is not an object", path.display());
    };
    if let Some(Value::Array(groups)) = hooks.get_mut("SessionStart") {
        groups.retain(|group| !is_ours(group));
        if groups.is_empty() {
            hooks.remove("SessionStart");
        }
    }

    if install {
        let group = json!({
            "hooks": [
                {
                    "type": "command",
                    "command": format!("brainmaker sync --quiet --no-update-check # {MARKER}"),
                    "timeout": 60
                },
                {
                    "type": "command",
                    "command": format!("brainmaker session-context # {MARKER}"),
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
    fs::write(&path, format!("{text}\n"))
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}

/// True when this module wrote the matcher group.
fn is_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|list| {
            list.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(MARKER))
            })
        })
}

/// Adds or removes the marked block in `CLAUDE.md`. Returns true on a change.
fn write_briefing(content: &Path, claude: &Path, install: bool) -> Result<bool> {
    let path = claude.join("CLAUDE.md");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let stripped = strip_block(&existing);

    let updated = if install {
        let block = format!(
            "{BLOCK_START}\n\
             # Shared content\n\n\
             `brainmaker` keeps `{}` in step with the team's server. It holds the shared \
             briefing, the wiki, and the agent memory. Read `CLAUDE.md` there before any \
             non-trivial action, and follow the `[[wiki/...]]` pointers it gives rather than \
             loading the whole directory.\n\n\
             Run `brainmaker unlink` to remove this block and the linked skills.\n\
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
    fs::write(&path, updated).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}

/// Returns `text` without the marked block, if it holds one.
fn strip_block(text: &str) -> String {
    let (Some(start), Some(end)) = (text.find(BLOCK_START), text.find(BLOCK_END)) else {
        return text.to_string();
    };
    if end < start {
        return text.to_string();
    }
    let after = end + BLOCK_END.len();
    format!("{}{}", &text[..start], &text[after..])
}

/// Prints what the run changed.
///
/// `installing` picks the verb, so an unlink does not report that it wrote the
/// very things it removed.
fn describe(report: &Report, installing: bool, log: &dyn Fn(&str)) {
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
    if !report.settings_changed
        && !report.briefing_changed
        && report.linked.is_empty()
        && report.removed.is_empty()
    {
        log("Nothing to change.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert!(write_settings(&claude, true).unwrap());
        assert!(
            !write_settings(&claude, true).unwrap(),
            "second run is a no-op"
        );

        let text = fs::read_to_string(claude.join("settings.json")).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let groups = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "the user's own group survives");
        assert_eq!(value["model"], "opus", "unrelated settings survive");
        assert_eq!(text.matches(MARKER).count(), 2);

        assert!(write_settings(&claude, false).unwrap());
        let value: Value =
            serde_json::from_str(&fs::read_to_string(claude.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(value["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        assert_eq!(value["model"], "opus");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn writes_the_hook_into_a_file_that_does_not_exist_yet() {
        let base = temp_dir("fresh-settings");
        let claude = base.join("claude");

        assert!(write_settings(&claude, true).unwrap());
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

        assert!(write_briefing(&content, &claude, true).unwrap());
        let text = fs::read_to_string(claude.join("CLAUDE.md")).unwrap();
        assert!(text.starts_with("# Mine\n\nKeep this."));
        assert!(text.contains(BLOCK_START) && text.contains(BLOCK_END));

        assert!(
            !write_briefing(&content, &claude, true).unwrap(),
            "idempotent"
        );

        assert!(write_briefing(&content, &claude, false).unwrap());
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

        assert!(write_briefing(&content, &claude, true).unwrap());
        assert!(claude.join("CLAUDE.md").is_file());
        assert!(write_briefing(&content, &claude, false).unwrap());
        assert!(!claude.join("CLAUDE.md").exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn strips_a_block_and_leaves_text_that_holds_none() {
        assert_eq!(strip_block("plain"), "plain");
        let text = format!("a\n{BLOCK_START}\nx\n{BLOCK_END}\nb");
        assert_eq!(strip_block(&text), "a\n\nb");
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
}
