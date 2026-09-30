// SPDX-License-Identifier: GPL-3.0-or-later

//! The admin commands: collect the notes, read the fleet, and read the sync
//! log of the fleet.
//!
//! # Who runs them
//!
//! The admin, on a copy of this binary in a root of its own, such as
//! `~/.brainmaker-admin`, that never runs `link`. Every command asks the issuer
//! for a token with the scope `outbox:read`. The server decides what that token
//! may read, so these commands grant nothing by themselves.
//!
//! # What they print
//!
//! The client IDs that the server reports. The admin needs them to tell two
//! machines of one operator apart, and `syncs --client` takes one. A client ID
//! alone authenticates nothing. No command prints the credential of the
//! machine it runs on.
//!
//! # Times
//!
//! The server sends every time in its report time zone, with the offset, such
//! as `2026-09-29T21:00:00+02:00`. These commands check that shape, and print
//! and write each time as the server sent it. They compare two times through
//! the instant that each time and its offset name, so the repeated hour of the
//! autumn clock change stays in order.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::config::Config;
use crate::lock;
use crate::outbox;
use crate::remote;

/// Notes that one page of `pull-outbox` asks for.
const PAGE: u32 = 100;

/// Notes that one run of `pull-outbox` handles at most.
const MAX_NOTES: usize = 1000;

/// Largest page of notes read: a full page of the largest notes, and the JSON
/// around them.
const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Largest fleet view or sync log read.
const MAX_REPORT_BYTES: u64 = 4 * 1024 * 1024;

/// Largest answer to an acknowledgement read.
const MAX_ACK_BYTES: u64 = 64 * 1024;

/// Longest client ID, as the issuer allows it.
const MAX_CLIENT_ID_LEN: usize = 100;

/// Longest operator name and longest flag.
const MAX_NAME_LEN: usize = 32;

/// Prefix of the temporary file that `pull-outbox` writes in the target
/// directory before it links the note into place.
const TEMP_PREFIX: &str = ".brainmaker-pull-";

// ---------------------------------------------------------------- times ---

/// Fails for a time that is not `YYYY-MM-DDTHH:MM:SS+HH:MM`, or `-HH:MM`.
pub fn check_time(value: &str, what: &str) -> Result<()> {
    if instant(value).is_none() {
        bail!(
            "the server sent {what} as {:?}, which is not a time such as \
             2026-09-29T21:00:00+02:00",
            remote::printable(value)
        );
    }
    Ok(())
}

/// Seconds since the Unix epoch at the instant that `value` names, or `None`
/// when `value` does not have the shape the server sends.
///
/// Only the order of two times uses this. The offset comes from the value
/// itself, so no time-zone rule takes part.
fn instant(value: &str) -> Option<i64> {
    let b = value.as_bytes();
    let shape = b.len() == 25
        && [4, 7].iter().all(|&i| b[i] == b'-')
        && b[10] == b'T'
        && [13, 16, 22].iter().all(|&i| b[i] == b':')
        && matches!(b[19], b'+' | b'-')
        && [
            0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18, 20, 21, 23, 24,
        ]
        .iter()
        .all(|&i| b[i].is_ascii_digit());
    if !shape {
        return None;
    }
    let number = |from: usize, to: usize| -> i64 {
        b[from..to]
            .iter()
            .fold(0, |sum, digit| sum * 10 + i64::from(digit - b'0'))
    };
    let (year, month, day) = (number(0, 4), number(5, 7), number(8, 10));
    let (hour, minute, second) = (number(11, 13), number(14, 16), number(17, 19));
    let (offset_hour, offset_minute) = (number(20, 22), number(23, 25));
    let ranges = (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour <= 23
        && minute <= 59
        && second <= 60
        && offset_hour <= 23
        && offset_minute <= 59;
    if !ranges {
        return None;
    }
    let sign = if b[19] == b'+' { 1 } else { -1 };
    let offset = sign * (offset_hour * 3600 + offset_minute * 60);
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// Days from 1970-01-01 to the date, in the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

// ---------------------------------------------------------------- rules ---

/// The issuer's rule for a client ID: 3 to 100 characters of lowercase ASCII
/// letters, digits, `.`, `_`, and `-`. The ID becomes a path segment.
pub fn check_client_id(value: &str) -> Result<()> {
    let ok = (3..=MAX_CLIENT_ID_LEN).contains(&value.len())
        && value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        });
    if !ok {
        bail!(
            "the client ID {:?} is not 3 to {MAX_CLIENT_ID_LEN} characters of lowercase letters, \
             digits, '.', '_', and '-'",
            remote::printable(value)
        );
    }
    Ok(())
}

/// The operator rule: 1 to 32 lowercase ASCII letters, digits, and `-`,
/// starting with a letter or a digit.
pub fn check_operator(value: &str) -> Result<()> {
    if !is_slug(value) {
        bail!(
            "the operator {:?} is not 1 to {MAX_NAME_LEN} lowercase letters, digits, and '-'",
            remote::printable(value)
        );
    }
    Ok(())
}

fn is_slug(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= MAX_NAME_LEN
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Turns an `invalid_scope` answer into the error that names the setting.
fn with_scope_hint(error: anyhow::Error) -> anyhow::Error {
    if auth::is_invalid_scope(&error) {
        return anyhow::anyhow!(
            "this credential cannot read the outbox: the issuer does not grant it the scope {}",
            auth::SCOPE_OUTBOX_READ
        );
    }
    error
}

// ----------------------------------------------------------- pull-outbox ---

/// One page of `GET {base}/admin/outbox`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotesPage {
    notes: Vec<WaitingNote>,
}

/// One note that waits for the admin.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitingNote {
    id: String,
    operator: String,
    client_id: String,
    name: String,
    kind: String,
    domain: String,
    flags: Vec<String>,
    sha256: String,
    size_bytes: u64,
    received_at: String,
    body: String,
}

/// The answer to an acknowledgement.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Acked {
    acked: u64,
}

/// What one run of `pull-outbox` did.
#[derive(Debug, Default)]
pub struct Pulled {
    /// Each note on disk now, as the path that holds it.
    pub written: Vec<PathBuf>,
    /// Each note that could not be written, as its id and the reason. It stays
    /// on the server, unacknowledged, for the next run.
    pub failed: Vec<(String, String)>,
    /// Notes that the server marked as collected in this run.
    pub acked: u64,
}

/// Writes each note that waits on the server into `dir`, and acknowledges each
/// note that is on disk.
///
/// A note is never written over a file: a name that holds other bytes gets a
/// number. A name that already holds the same bytes counts as written, so a
/// run that stopped before its acknowledgement repeats safely.
pub fn pull_outbox(config: &Config, dir: &Path, log: &dyn Fn(&str)) -> Result<Pulled> {
    if !dir.is_dir() {
        bail!(
            "{} is not a directory; nothing was written. Name the folder that collects the notes.",
            dir.display()
        );
    }
    let lock_path = config.admin_lock_file();
    let Some(_lock) = lock::acquire(&lock_path, Duration::ZERO)? else {
        bail!(
            "another admin command holds {}; run this one when it ends",
            lock_path.display()
        );
    };
    // A run that was stopped cannot remove its temporary file. The lock means
    // that no other run owns one now.
    remove_leftovers(dir);

    let mut pulled = Pulled::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut failed: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let page = fetch_page(config)?;
        if page.is_empty() {
            break;
        }
        let mut on_disk = Vec::new();
        for note in &page {
            seen.insert(note.id.clone());
            match place(note, dir) {
                Ok(path) => {
                    log(&format!(
                        "Wrote {} ({}, {})",
                        path.display(),
                        note.kind,
                        note.domain
                    ));
                    failed.remove(&note.id);
                    pulled.written.push(path);
                    on_disk.push(note.id.clone());
                }
                Err(reason) => {
                    log(&format!(
                        "Cannot write the note {} ({}): {reason}",
                        remote::printable(&note.id),
                        remote::printable(&note.name)
                    ));
                    failed.insert(note.id.clone(), reason);
                }
            }
        }
        if on_disk.is_empty() {
            break;
        }
        pulled.acked += ack(config, &on_disk)?;
        if seen.len() >= MAX_NOTES {
            break;
        }
    }
    pulled.failed = failed.into_iter().collect();
    Ok(pulled)
}

/// Removes every regular file in `dir` whose name starts with
/// [`TEMP_PREFIX`]. Nothing else writes that prefix.
fn remove_leftovers(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let ours = entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX);
        let file = fs::symlink_metadata(entry.path()).is_ok_and(|meta| meta.is_file());
        if ours && file {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn fetch_page(config: &Config) -> Result<Vec<WaitingNote>> {
    let url = config.admin_outbox_url(PAGE);
    let body = remote::fetch_admin(config, &url, MAX_PAGE_BYTES).map_err(with_scope_hint)?;
    let page: NotesPage = serde_json::from_str(&body)
        .with_context(|| format!("{url} did not return the expected notes"))?;
    Ok(page.notes)
}

fn ack(config: &Config, ids: &[String]) -> Result<u64> {
    let url = config.admin_ack_url();
    let body = serde_json::json!({ "ids": ids }).to_string();
    let answer = remote::post_admin(config, &url, &body, MAX_ACK_BYTES).map_err(with_scope_hint)?;
    let acked: Acked = serde_json::from_str(&answer)
        .with_context(|| format!("{url} did not return the expected answer"))?;
    Ok(acked.acked)
}

/// Checks one note, stamps it, and writes it into `dir`. Returns the path.
///
/// Every value that reaches the file name or the frontmatter is checked again
/// here, whatever the server checked before.
fn place(note: &WaitingNote, dir: &Path) -> Result<PathBuf, String> {
    check_operator(&note.operator).map_err(|e| e.to_string())?;
    check_client_id(&note.client_id).map_err(|e| e.to_string())?;
    outbox::validate_name(&note.name)?;
    check_time(&note.received_at, "received_at").map_err(|e| e.to_string())?;
    if !outbox::KINDS.contains(&note.kind.as_str()) || !is_slug(&note.domain) {
        return Err("the server sent a kind or a domain that no note may carry".to_string());
    }
    if note.size_bytes != note.body.len() as u64 {
        return Err(format!(
            "the text is {} bytes, and the server stored {}",
            note.body.len(),
            note.size_bytes
        ));
    }
    for flag in &note.flags {
        if !is_slug(flag) {
            return Err(format!(
                "the server sent the flag {:?}, which is not a slug",
                remote::printable(flag)
            ));
        }
    }
    let digest = hex(ring::digest::digest(&ring::digest::SHA256, note.body.as_bytes()).as_ref());
    if digest != note.sha256.to_ascii_lowercase() {
        return Err("the text does not match the SHA-256 that the server stored".to_string());
    }

    // The date the server received the note, in its report time zone. The
    // name starts with the operator's own date, so the file name holds two.
    let date = &note.received_at[..10];
    let name = format!("{date}-{}-{}", note.operator, note.name);
    let text = stamp(&note.body, &note.operator, &note.flags)?;
    write_new(dir, &name, text.as_bytes())
}

/// Sets `author` and `review_flags` in the frontmatter from the server's
/// values, whatever the note says.
///
/// A key that is present keeps its place: its line, and the indented and list
/// lines under it, give way to the new line. A key that is absent goes right
/// after the opening `---`. Both keys are always written, so nothing that the
/// operator wrote under them survives. The note's own line end is kept.
pub fn stamp(body: &str, operator: &str, flags: &[String]) -> Result<String, String> {
    let eol = if body.starts_with("---\r\n") {
        "\r\n"
    } else if body.starts_with("---\n") {
        "\n"
    } else {
        return Err("the note does not start with a --- line".to_string());
    };
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    let bare = |line: &str| -> String {
        line.strip_suffix('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .unwrap_or(line)
            .to_string()
    };
    let close = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| bare(line) == "---")
        .map(|(index, _)| index)
        .ok_or_else(|| "the frontmatter has no closing --- line".to_string())?;

    let author = format!("author: {operator}{eol}");
    let review = format!("review_flags: [{}]{eol}", flags.join(", "));
    let keys = [("author", author), ("review_flags", review)];
    let present = |key: &str| lines[1..close].iter().any(|line| is_key(&bare(line), key));

    let mut out = String::with_capacity(body.len() + 64);
    out.push_str(lines[0]);
    for (key, line) in &keys {
        if !present(key) {
            out.push_str(line);
        }
    }
    let mut written: BTreeSet<&str> = BTreeSet::new();
    let mut index = 1;
    while index < close {
        let current = bare(lines[index]);
        let owner = keys.iter().find(|(key, _)| is_key(&current, key));
        let Some((key, line)) = owner else {
            out.push_str(lines[index]);
            index += 1;
            continue;
        };
        if written.insert(key) {
            out.push_str(line);
        }
        // Drop the key line and every line that belongs to it.
        index += 1;
        while index < close && belongs_to_key(&lines[index..close], &bare) {
            index += 1;
        }
    }
    for line in &lines[close..] {
        out.push_str(line);
    }
    Ok(out)
}

/// True when `line` is the key line of `key`: `key:` at column 0, then the
/// end of the line, a space, or a tab.
fn is_key(line: &str, key: &str) -> bool {
    line.strip_prefix(key)
        .and_then(|rest| rest.strip_prefix(':'))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// True when the first of `rest` belongs to the key above it: an indented
/// line, a list item, or a blank line that more of those follow.
fn belongs_to_key(rest: &[&str], bare: &dyn Fn(&str) -> String) -> bool {
    let is_part = |line: &str| {
        line.starts_with([' ', '\t'])
            || line == "-"
            || line.starts_with("- ")
            || line.starts_with("-\t")
    };
    let first = bare(rest[0]);
    if first.trim().is_empty() {
        return rest
            .iter()
            .map(|line| bare(line))
            .find(|line| !line.trim().is_empty())
            .is_some_and(|line| is_part(&line));
    }
    is_part(&first)
}

/// Writes `bytes` under `name` in `dir`, never over another file.
///
/// The bytes go to a temporary file first, which is flushed and then
/// hard-linked to the final name, so the name appears whole or not at all. A
/// name that holds other bytes gets `-2`, `-3`, and so on before `.md`. A name
/// that holds the same bytes is the note, written before.
fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let temp = dir.join(format!(
        "{TEMP_PREFIX}{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(format!("cannot write {}: {error}", temp.display()));
    }

    let stem = name.strip_suffix(".md").unwrap_or(name);
    let mut number = 1;
    let result = loop {
        let candidate = if number == 1 {
            dir.join(name)
        } else {
            dir.join(format!("{stem}-{number}.md"))
        };
        match fs::hard_link(&temp, &candidate) {
            Ok(()) => break Ok(candidate),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let same = fs::symlink_metadata(&candidate).is_ok_and(|meta| meta.is_file())
                    && fs::read(&candidate).is_ok_and(|held| held == bytes);
                if same {
                    break Ok(candidate);
                }
                number += 1;
            }
            Err(error) => break Err(format!("cannot write {}: {error}", candidate.display())),
        }
    };
    let _ = fs::remove_file(&temp);
    result
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// ---------------------------------------------------------------- fleet ---

/// Body of `GET {base}/admin/clients`. Any other shape fails the command, so
/// a change on the server cannot pass unseen.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fleet {
    pub generated_at: String,
    pub bundle: Bundle,
    pub clients: Vec<Client>,
}

/// The content every client should hold.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
}

/// One client, as the server shows it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Client {
    pub client_id: String,
    pub operator: Option<String>,
    /// Part of the shape. The admin's brief carries no display name.
    #[serde(rename = "display_name")]
    _display_name: Option<String>,
    pub retired: bool,
    pub first_seen_at: Option<String>,
    pub last_seen_at: Option<String>,
    pub last_ip: Option<String>,
    /// Part of the shape. `admin syncs` shows the user agent of each sync.
    #[serde(rename = "last_user_agent")]
    _last_user_agent: Option<String>,
    pub platform: Option<String>,
    pub brainmaker_version: Option<String>,
    pub content_version: Option<String>,
    pub outbox_pending: Option<u64>,
    pub last_push_at: Option<String>,
    pub notes_pushed_total: u64,
    pub notes_pushed_7d: u64,
}

/// Reads the fleet view, and checks every value that names a client, an
/// operator, or a time.
pub fn fleet(config: &Config) -> Result<Fleet> {
    let url = config.admin_clients_url();
    let body = remote::fetch_admin(config, &url, MAX_REPORT_BYTES).map_err(with_scope_hint)?;
    let fleet: Fleet = serde_json::from_str(&body)
        .with_context(|| format!("{url} did not return the expected fleet view"))?;
    check_fleet(&fleet)?;
    Ok(fleet)
}

fn check_fleet(fleet: &Fleet) -> Result<()> {
    check_time(&fleet.generated_at, "generated_at")?;
    if let Some(time) = &fleet.bundle.published_at {
        check_time(time, "bundle.published_at")?;
    }
    for client in &fleet.clients {
        check_client_id(&client.client_id)?;
        if let Some(operator) = &client.operator {
            check_operator(operator)?;
        }
        for (what, time) in [
            ("first_seen_at", &client.first_seen_at),
            ("last_seen_at", &client.last_seen_at),
            ("last_push_at", &client.last_push_at),
        ] {
            if let Some(time) = time {
                check_time(time, what)?;
            }
        }
    }
    Ok(())
}

/// The fleet in the shape of the admin's brief: one entry per operator, and
/// the clients that no operator holds.
#[derive(Debug, Serialize)]
pub struct TeamState {
    pub generated_at: String,
    pub bundle: Bundle,
    pub operators: BTreeMap<String, OperatorState>,
    pub unregistered: Vec<Unregistered>,
}

/// One operator. A key without a value is left out.
#[derive(Debug, Serialize)]
pub struct OperatorState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brainmaker_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_push_at: Option<String>,
    pub notes_pushed_total: u64,
    pub notes_pushed_7d: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbox_pending: Option<u64>,
    pub clients: Vec<ClientState>,
}

/// One client of an operator.
#[derive(Debug, Serialize)]
pub struct ClientState {
    pub client_id: String,
    pub retired: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brainmaker_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ip: Option<String>,
}

/// One client that no operator holds.
#[derive(Debug, Serialize)]
pub struct Unregistered {
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ip: Option<String>,
}

/// Builds the admin's shape from the fleet view.
///
/// The client ID, the platform, the versions, the last sync, and the notes
/// waiting come from the operator's most recently seen client that is not
/// retired. The note counts and the last push cover all of the operator's
/// clients.
pub fn team_state(fleet: &Fleet) -> TeamState {
    let mut by_operator: BTreeMap<String, Vec<&Client>> = BTreeMap::new();
    let mut unregistered = Vec::new();
    for client in &fleet.clients {
        match &client.operator {
            Some(operator) => by_operator
                .entry(operator.clone())
                .or_default()
                .push(client),
            None => unregistered.push(Unregistered {
                client_id: client.client_id.clone(),
                last_sync_at: client.last_seen_at.clone(),
                last_ip: client.last_ip.clone(),
            }),
        }
    }

    let operators = by_operator
        .into_iter()
        .map(|(operator, clients)| {
            let current = clients
                .iter()
                .filter(|client| !client.retired)
                .max_by_key(|client| {
                    (
                        client.last_seen_at.as_deref().and_then(instant),
                        std::cmp::Reverse(client.client_id.clone()),
                    )
                });
            let last_push_at = clients
                .iter()
                .filter_map(|client| client.last_push_at.as_deref())
                .max_by_key(|time| instant(time))
                .map(str::to_string);
            let state = OperatorState {
                client_id: current.map(|client| client.client_id.clone()),
                platform: current.and_then(|client| client.platform.clone()),
                brainmaker_version: current.and_then(|client| client.brainmaker_version.clone()),
                content_version: current.and_then(|client| client.content_version.clone()),
                last_sync_at: current.and_then(|client| client.last_seen_at.clone()),
                last_push_at,
                notes_pushed_total: clients.iter().map(|client| client.notes_pushed_total).sum(),
                notes_pushed_7d: clients.iter().map(|client| client.notes_pushed_7d).sum(),
                outbox_pending: current.and_then(|client| client.outbox_pending),
                clients: clients
                    .iter()
                    .map(|client| ClientState {
                        client_id: client.client_id.clone(),
                        retired: client.retired,
                        platform: client.platform.clone(),
                        brainmaker_version: client.brainmaker_version.clone(),
                        content_version: client.content_version.clone(),
                        last_sync_at: client.last_seen_at.clone(),
                        last_ip: client.last_ip.clone(),
                    })
                    .collect(),
            };
            (operator, state)
        })
        .collect();

    TeamState {
        generated_at: fleet.generated_at.clone(),
        bundle: fleet.bundle.clone(),
        operators,
        unregistered,
    }
}

/// One line per operator, then one line per client that no operator holds.
/// Every value from the server loses its control characters.
pub fn status_lines(state: &TeamState) -> Vec<String> {
    let value = |text: &Option<String>| {
        text.as_deref()
            .map(remote::printable)
            .unwrap_or_else(|| "-".to_string())
    };
    let mut lines = Vec::new();
    for (operator, entry) in &state.operators {
        lines.push(format!(
            "{operator}  last sync {}  version {}  platform {}  waiting {}  sent in 7 days {}",
            value(&entry.last_sync_at),
            value(&entry.brainmaker_version),
            value(&entry.platform),
            entry
                .outbox_pending
                .map(|count| count.to_string())
                .unwrap_or_else(|| "-".to_string()),
            entry.notes_pushed_7d
        ));
    }
    for client in &state.unregistered {
        lines.push(format!(
            "unregistered  {}  last sync {}  ip {}",
            client.client_id,
            value(&client.last_sync_at),
            value(&client.last_ip)
        ));
    }
    if lines.is_empty() {
        lines.push("The server knows no client.".to_string());
    }
    lines
}

// ---------------------------------------------------------------- syncs ---

/// Body of `GET {base}/admin/clients/<client_id>/syncs`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncPage {
    syncs: Vec<SyncRow>,
}

/// One `GET /content/latest` of one client.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRow {
    pub id: i64,
    pub at: String,
    pub status: u16,
    pub ip: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brainmaker_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outbox_pending: Option<u64>,
}

/// One sync, with the client that made it.
#[derive(Debug, Clone, Serialize)]
pub struct ClientSync {
    pub client_id: String,
    #[serde(flatten)]
    pub row: SyncRow,
}

/// Which syncs to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// Every client of this operator, retired ones included.
    Operator(String),
    /// This one client.
    Client(String),
}

/// Reads the newest `limit` syncs of the selected clients, newest first.
pub fn syncs(config: &Config, selector: &Selector, limit: u32) -> Result<Vec<ClientSync>> {
    let client_ids = match selector {
        Selector::Client(client_id) => {
            check_client_id(client_id)?;
            vec![client_id.clone()]
        }
        Selector::Operator(operator) => {
            check_operator(operator)?;
            let ids: Vec<String> = fleet(config)?
                .clients
                .into_iter()
                .filter(|client| client.operator.as_deref() == Some(operator.as_str()))
                .map(|client| client.client_id)
                .collect();
            if ids.is_empty() {
                bail!("the server registers no client to {operator}");
            }
            ids
        }
    };

    let mut rows = Vec::new();
    for client_id in client_ids {
        let url = config.admin_syncs_url(&client_id, limit);
        let body = remote::fetch_admin(config, &url, MAX_REPORT_BYTES).map_err(with_scope_hint)?;
        let page: SyncPage = serde_json::from_str(&body)
            .with_context(|| format!("{url} did not return the expected sync log"))?;
        for row in page.syncs {
            check_time(&row.at, "at")?;
            rows.push(ClientSync {
                client_id: client_id.clone(),
                row,
            });
        }
    }
    rows.sort_by(|a, b| {
        (instant(&b.row.at), &b.client_id, b.row.id).cmp(&(
            instant(&a.row.at),
            &a.client_id,
            a.row.id,
        ))
    });
    rows.truncate(limit as usize);
    Ok(rows)
}

/// One line for one sync: the time, the client, the status, the address, the
/// platform, the version, and the installed content.
pub fn sync_line(sync: &ClientSync) -> String {
    let value = |text: &Option<String>| {
        text.as_deref()
            .map(remote::printable)
            .unwrap_or_else(|| "-".to_string())
    };
    format!(
        "{}  {}  {}  {}  {}  {}  {}",
        sync.row.at,
        sync.client_id,
        sync.row.status,
        remote::printable(&sync.row.ip),
        value(&sync.row.platform),
        value(&sync.row.brainmaker_version),
        value(&sync.row.content_version)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Route, Server, temp_dir};

    fn quiet(_: &str) {}

    // ------------------------------------------------------------ times ---

    #[test]
    fn a_time_reads_as_the_instant_its_offset_names() {
        assert_eq!(instant("1970-01-01T00:00:00+00:00"), Some(0));
        assert_eq!(instant("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(instant("2000-03-01T00:00:00+00:00"), Some(951_868_800));
        // The repeated hour of the autumn change in Rome.
        let summer = instant("2026-10-25T02:30:00+02:00").unwrap();
        let winter = instant("2026-10-25T02:10:00+01:00").unwrap();
        assert!(summer < winter, "02:30 summer time comes first");
        for bad in [
            "2026-09-29 21:00:00+02:00",
            "2026-09-29T21:00:00Z",
            "2026-13-29T21:00:00+02:00",
            "2026-09-29T24:00:00+02:00",
            "2026-09-29T21:00:00+02",
            "",
        ] {
            assert_eq!(instant(bad), None, "{bad}");
        }
    }

    // ------------------------------------------------------------ stamp ---

    #[test]
    fn stamp_replaces_both_keys_and_what_the_operator_wrote_under_them() {
        let body = "---\nkind: fact\nauthor: matteo\n  - pretends\nreview_flags:\n- none\n\ndomain: damac\n---\n\n# Note\nauthor: in the body stays\n";
        let stamped = stamp(body, "gabriele", &["iban".to_string()]).unwrap();
        assert_eq!(
            stamped,
            "---\nkind: fact\nauthor: gabriele\nreview_flags: [iban]\n\ndomain: damac\n---\n\n# Note\nauthor: in the body stays\n"
        );
    }

    #[test]
    fn stamp_inserts_a_missing_key_after_the_opening_line() {
        let body = "---\r\nkind: fact\r\ndomain: damac\r\n---\r\nBody\r\n";
        let stamped = stamp(body, "gabriele", &[]).unwrap();
        assert_eq!(
            stamped,
            "---\r\nauthor: gabriele\r\nreview_flags: []\r\nkind: fact\r\ndomain: damac\r\n---\r\nBody\r\n"
        );
    }

    #[test]
    fn stamp_keeps_the_place_of_a_present_key() {
        let body = "---\nkind: fact\nreview_flags: []\ndomain: damac\n---\n";
        let stamped = stamp(body, "anna", &["iban".into(), "codice-fiscale".into()]).unwrap();
        assert_eq!(
            stamped,
            "---\nauthor: anna\nkind: fact\nreview_flags: [iban, codice-fiscale]\ndomain: damac\n---\n"
        );
    }

    #[test]
    fn stamp_refuses_a_note_without_a_frontmatter() {
        assert!(stamp("# no frontmatter\n", "anna", &[]).is_err());
        assert!(stamp("---\nkind: fact\n", "anna", &[]).is_err());
    }

    // ------------------------------------------------------- pull-outbox ---

    const BODY: &str = "---\nkind: fact\ndomain: damac\nauthor: matteo\n---\n\n# Lezione\n";

    fn sha(text: &str) -> String {
        hex(ring::digest::digest(&ring::digest::SHA256, text.as_bytes()).as_ref())
    }

    fn note(id: &str, name: &str, received_at: &str, flags: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "operator": "gabriele",
            "client_id": "brainmaker-sync-gabriele",
            "name": name,
            "kind": "fact",
            "domain": "damac",
            "flags": flags,
            "sha256": sha(BODY),
            "size_bytes": BODY.len(),
            "received_at": received_at,
            "body": BODY,
        })
    }

    fn page(notes: &[serde_json::Value]) -> String {
        serde_json::json!({ "notes": notes }).to_string()
    }

    fn admin_server(pages: Vec<String>, ack: &str) -> Server {
        let mut outbox = Route::get("/admin/outbox?limit=100", pages[0].clone());
        for next in &pages[1..] {
            outbox = outbox.then(200, next.clone());
        }
        Server::start(vec![
            Route::token("outbox:read", r#"{"access_token":"read-token"}"#),
            outbox,
            Route::post("/admin/outbox/ack", ack.to_string()),
        ])
    }

    fn admin_config(server: &Server, tag: &str) -> (PathBuf, Config, PathBuf) {
        let dir = temp_dir(tag);
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());
        let inbox = dir.join("inbox");
        fs::create_dir_all(&inbox).unwrap();
        (dir, config, inbox)
    }

    #[test]
    fn pull_outbox_writes_each_note_with_two_dates_and_acks_it() {
        let server = admin_server(
            vec![
                page(&[
                    note(
                        "n1",
                        "2026-09-29-lezioni-firma.md",
                        "2026-09-30T01:30:00+02:00",
                        &["iban"],
                    ),
                    note(
                        "n2",
                        "2026-09-29-altro.md",
                        "2026-09-29T18:00:00+02:00",
                        &[],
                    ),
                ]),
                page(&[]),
            ],
            r#"{"acked":2}"#,
        );
        let (dir, config, inbox) = admin_config(&server, "admin-pull");

        let pulled = pull_outbox(&config, &inbox, &quiet).unwrap();

        let first = inbox.join("2026-09-30-gabriele-2026-09-29-lezioni-firma.md");
        let second = inbox.join("2026-09-29-gabriele-2026-09-29-altro.md");
        assert_eq!(pulled.written, vec![first.clone(), second.clone()]);
        assert!(pulled.failed.is_empty());
        assert_eq!(pulled.acked, 2);
        let text = fs::read_to_string(&first).unwrap();
        assert!(text.contains("\nauthor: gabriele\n"), "{text}");
        assert!(text.contains("\nreview_flags: [iban]\n"), "{text}");
        assert!(!text.contains("matteo"), "{text}");
        assert!(
            fs::read_to_string(&second)
                .unwrap()
                .contains("review_flags: []")
        );

        let received = server.received();
        let ack = received
            .iter()
            .find(|r| r.path == "/admin/outbox/ack")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&ack.body).unwrap();
        assert_eq!(body, serde_json::json!({ "ids": ["n1", "n2"] }));
        assert_eq!(ack.header("authorization"), Some("Bearer read-token"));
        assert_eq!(received[0].form("scope").as_deref(), Some("outbox:read"));
        // No temporary file stays behind.
        let names: Vec<String> = fs::read_dir(&inbox)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.starts_with('.')), "{names:?}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pull_outbox_never_replaces_a_file() {
        let server = admin_server(
            vec![
                page(&[note(
                    "n1",
                    "2026-09-29-a.md",
                    "2026-09-29T18:00:00+02:00",
                    &[],
                )]),
                page(&[]),
            ],
            r#"{"acked":1}"#,
        );
        let (dir, config, inbox) = admin_config(&server, "admin-numbered");
        let taken = inbox.join("2026-09-29-gabriele-2026-09-29-a.md");
        fs::write(&taken, "someone else's file").unwrap();

        let pulled = pull_outbox(&config, &inbox, &quiet).unwrap();

        assert_eq!(
            pulled.written,
            vec![inbox.join("2026-09-29-gabriele-2026-09-29-a-2.md")]
        );
        assert_eq!(fs::read_to_string(&taken).unwrap(), "someone else's file");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_note_already_on_disk_counts_as_written() {
        let server = admin_server(
            vec![
                page(&[note(
                    "n1",
                    "2026-09-29-a.md",
                    "2026-09-29T18:00:00+02:00",
                    &[],
                )]),
                page(&[]),
            ],
            r#"{"acked":1}"#,
        );
        let (dir, config, inbox) = admin_config(&server, "admin-identical");
        let path = inbox.join("2026-09-29-gabriele-2026-09-29-a.md");
        fs::write(&path, stamp(BODY, "gabriele", &[]).unwrap()).unwrap();

        let pulled = pull_outbox(&config, &inbox, &quiet).unwrap();

        assert_eq!(pulled.written, vec![path]);
        assert_eq!(fs::read_dir(&inbox).unwrap().count(), 1);
        assert_eq!(pulled.acked, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_note_that_cannot_be_written_stays_unacked() {
        // The same length, other bytes: only the digest tells.
        let mut altered = note("n2", "2026-09-29-b.md", "2026-09-29T18:00:00+02:00", &[]);
        altered["body"] = serde_json::json!(BODY.replace("Lezione", "Lezionx"));
        let bad_name = note("n3", "../x.md", "2026-09-29T18:00:00+02:00", &[]);
        let mut short = note("n4", "2026-09-29-d.md", "2026-09-29T18:00:00+02:00", &[]);
        short["size_bytes"] = serde_json::json!(1);
        let server = admin_server(
            vec![
                page(&[
                    note("n1", "2026-09-29-a.md", "2026-09-29T18:00:00+02:00", &[]),
                    altered.clone(),
                    bad_name.clone(),
                    short.clone(),
                ]),
                // The failures come back, and bring nothing to disk.
                page(&[altered, bad_name, short]),
            ],
            r#"{"acked":1}"#,
        );
        let (dir, config, inbox) = admin_config(&server, "admin-failed");

        let pulled = pull_outbox(&config, &inbox, &quiet).unwrap();

        assert_eq!(pulled.written.len(), 1);
        let failed: Vec<&str> = pulled.failed.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(failed, ["n2", "n3", "n4"]);
        assert!(
            pulled.failed[0].1.contains("SHA-256"),
            "{:?}",
            pulled.failed
        );
        assert!(
            pulled.failed[2].1.contains("the server stored 1"),
            "{:?}",
            pulled.failed
        );
        let acks: Vec<serde_json::Value> = server
            .received()
            .iter()
            .filter(|r| r.path == "/admin/outbox/ack")
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(acks, vec![serde_json::json!({ "ids": ["n1"] })]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pull_outbox_removes_what_a_stopped_run_left() {
        let server = admin_server(vec![page(&[])], r#"{"acked":0}"#);
        let (dir, config, inbox) = admin_config(&server, "admin-leftover");
        fs::write(inbox.join(".brainmaker-pull-42-0"), "half a note").unwrap();
        fs::write(inbox.join(".other"), "not ours").unwrap();

        pull_outbox(&config, &inbox, &quiet).unwrap();

        assert!(!inbox.join(".brainmaker-pull-42-0").exists());
        assert!(inbox.join(".other").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pull_outbox_refuses_a_directory_that_does_not_exist() {
        let server = admin_server(vec![page(&[])], r#"{"acked":0}"#);
        let (dir, config, inbox) = admin_config(&server, "admin-no-dir");
        let missing = inbox.join("absent");

        let error = pull_outbox(&config, &missing, &quiet).unwrap_err();

        assert!(
            format!("{error:#}").contains("not a directory"),
            "{error:#}"
        );
        assert!(server.received().is_empty(), "nothing was asked");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pull_outbox_waits_for_no_other_admin_run() {
        let server = admin_server(vec![page(&[])], r#"{"acked":0}"#);
        let (dir, config, inbox) = admin_config(&server, "admin-lock");
        let held = lock::acquire(&config.admin_lock_file(), Duration::ZERO)
            .unwrap()
            .unwrap();

        let error = pull_outbox(&config, &inbox, &quiet).unwrap_err();

        assert!(
            format!("{error:#}").contains("another admin command"),
            "{error:#}"
        );
        drop(held);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_credential_without_the_read_scope_gets_a_clear_error() {
        let server = Server::start(vec![
            Route::token("outbox:read", r#"{"error":"invalid_scope"}"#).status(400),
        ]);
        let (dir, config, inbox) = admin_config(&server, "admin-scope");

        let error = pull_outbox(&config, &inbox, &quiet).unwrap_err();

        let text = format!("{error:#}");
        assert!(
            text.contains("this credential cannot read the outbox"),
            "{text}"
        );
        assert!(text.contains("outbox:read"), "{text}");
        fs::remove_dir_all(&dir).unwrap();
    }

    // ------------------------------------------------------------ fleet ---

    fn client(
        client_id: &str,
        operator: Option<&str>,
        retired: bool,
        last_seen_at: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "client_id": client_id,
            "operator": operator,
            "display_name": operator,
            "retired": retired,
            "first_seen_at": last_seen_at,
            "last_seen_at": last_seen_at,
            "last_ip": "203.0.113.7",
            "last_user_agent": "brainmaker/0.1.8",
            "platform": "darwin-arm64",
            "brainmaker_version": format!("v-{client_id}"),
            "content_version": "a377aa94",
            "outbox_pending": 1,
            "last_push_at": last_seen_at,
            "notes_pushed_total": 2,
            "notes_pushed_7d": 1
        })
    }

    fn fleet_body(clients: &[serde_json::Value]) -> String {
        serde_json::json!({
            "generated_at": "2026-09-29T21:00:00+02:00",
            "bundle": { "version": "a377aa94", "published_at": "2026-09-29T18:00:00+02:00", "files": 641 },
            "clients": clients,
        })
        .to_string()
    }

    fn fleet_server(body: String) -> Server {
        Server::start(vec![
            Route::token("outbox:read", r#"{"access_token":"r"}"#),
            Route::get("/admin/clients", body),
        ])
    }

    #[test]
    fn the_team_state_takes_the_newest_live_client_and_sums_the_notes() {
        let server = fleet_server(fleet_body(&[
            client(
                "brainmaker-sync-gabriele",
                Some("gabriele"),
                false,
                Some("2026-10-25T02:30:00+02:00"),
            ),
            // Seen later by the instant, though its text sorts first.
            client(
                "brainmaker-sync-gabriele-2",
                Some("gabriele"),
                false,
                Some("2026-10-25T02:10:00+01:00"),
            ),
            // Seen last of all, but retired.
            client(
                "brainmaker-sync-gabriele-old",
                Some("gabriele"),
                true,
                Some("2026-10-26T09:00:00+01:00"),
            ),
            client(
                "brainmaker-sync-old",
                None,
                false,
                Some("2026-09-20T09:00:00+02:00"),
            ),
        ]));
        let dir = temp_dir("admin-team");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let state = team_state(&fleet(&config).unwrap());
        let json = serde_json::to_value(&state).unwrap();

        let gabriele = &json["operators"]["gabriele"];
        assert_eq!(gabriele["client_id"], "brainmaker-sync-gabriele-2");
        assert_eq!(
            gabriele["brainmaker_version"],
            "v-brainmaker-sync-gabriele-2"
        );
        assert_eq!(gabriele["last_sync_at"], "2026-10-25T02:10:00+01:00");
        assert_eq!(
            gabriele["last_push_at"], "2026-10-26T09:00:00+01:00",
            "all clients"
        );
        assert_eq!(gabriele["notes_pushed_total"], 6);
        assert_eq!(gabriele["notes_pushed_7d"], 3);
        assert_eq!(gabriele["outbox_pending"], 1);
        assert_eq!(gabriele["clients"].as_array().unwrap().len(), 3);
        assert_eq!(gabriele["clients"][2]["retired"], true);
        assert!(gabriele.get("access_enabled").is_none());
        assert_eq!(
            json["unregistered"],
            serde_json::json!([{
                "client_id": "brainmaker-sync-old",
                "last_sync_at": "2026-09-20T09:00:00+02:00",
                "last_ip": "203.0.113.7"
            }])
        );
        assert_eq!(json["bundle"]["files"], 641);
        assert_eq!(json["generated_at"], "2026-09-29T21:00:00+02:00");

        let lines = status_lines(&state);
        assert_eq!(
            lines[0],
            "gabriele  last sync 2026-10-25T02:10:00+01:00  version v-brainmaker-sync-gabriele-2  \
             platform darwin-arm64  waiting 1  sent in 7 days 3"
        );
        assert_eq!(
            lines[1],
            "unregistered  brainmaker-sync-old  last sync 2026-09-20T09:00:00+02:00  ip 203.0.113.7"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_without_a_value_is_left_out() {
        let mut never = client("brainmaker-sync-anna", Some("anna"), false, None);
        for key in [
            "platform",
            "brainmaker_version",
            "content_version",
            "outbox_pending",
            "last_ip",
        ] {
            never[key] = serde_json::Value::Null;
        }
        let mut body: serde_json::Value = serde_json::from_str(&fleet_body(&[never])).unwrap();
        body["bundle"] =
            serde_json::json!({ "version": null, "published_at": null, "files": null });
        let server = fleet_server(body.to_string());
        let dir = temp_dir("admin-left-out");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let json = serde_json::to_value(team_state(&fleet(&config).unwrap())).unwrap();

        assert_eq!(json["bundle"], serde_json::json!({}));
        let anna = json["operators"]["anna"].as_object().unwrap();
        let keys: Vec<&str> = anna.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "client_id",
                "notes_pushed_total",
                "notes_pushed_7d",
                "clients"
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_fleet_of_another_shape_is_refused() {
        let mut extra: serde_json::Value = serde_json::from_str(&fleet_body(&[])).unwrap();
        extra["access_enabled"] = serde_json::json!(true);
        let bad_time = fleet_body(&[client(
            "brainmaker-sync-g",
            Some("g"),
            false,
            Some("yesterday"),
        )]);
        let bad_id = fleet_body(&[client("Bad Client", Some("g"), false, None)]);
        for body in [extra.to_string(), bad_time, bad_id] {
            let server = fleet_server(body);
            let dir = temp_dir("admin-shape");
            let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());
            assert!(fleet(&config).is_err());
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    // ------------------------------------------------------------ syncs ---

    fn row(id: i64, at: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "at": at, "status": 200, "ip": "203.0.113.7",
            "user_agent": "brainmaker/0.1.8", "platform": "darwin-arm64",
            "brainmaker_version": "0.1.8", "content_version": null, "outbox_pending": 0
        })
    }

    #[test]
    fn syncs_merge_the_clients_of_an_operator_newest_first() {
        let server = Server::start(vec![
            Route::token("outbox:read", r#"{"access_token":"r"}"#),
            Route::get(
                "/admin/clients",
                fleet_body(&[
                    client("brainmaker-sync-a", Some("gabriele"), false, None),
                    client("brainmaker-sync-b", Some("gabriele"), true, None),
                    client("brainmaker-sync-c", Some("anna"), false, None),
                ]),
            ),
            Route::get(
                "/admin/clients/brainmaker-sync-a/syncs?limit=3",
                serde_json::json!({ "syncs": [
                    row(9, "2026-10-25T02:10:00+01:00"),
                    row(4, "2026-09-29T08:00:00+02:00"),
                ] })
                .to_string(),
            ),
            Route::get(
                "/admin/clients/brainmaker-sync-b/syncs?limit=3",
                serde_json::json!({ "syncs": [
                    row(8, "2026-10-25T02:30:00+02:00"),
                    row(1, "2026-01-15T13:00:00+01:00"),
                ] })
                .to_string(),
            ),
        ]);
        let dir = temp_dir("admin-syncs");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let rows = syncs(&config, &Selector::Operator("gabriele".into()), 3).unwrap();

        let order: Vec<(&str, i64)> = rows
            .iter()
            .map(|sync| (sync.client_id.as_str(), sync.row.id))
            .collect();
        assert_eq!(
            order,
            [
                ("brainmaker-sync-a", 9),
                ("brainmaker-sync-b", 8),
                ("brainmaker-sync-a", 4)
            ]
        );
        assert_eq!(
            sync_line(&rows[0]),
            "2026-10-25T02:10:00+01:00  brainmaker-sync-a  200  203.0.113.7  darwin-arm64  0.1.8  -"
        );
        let json = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(json["client_id"], "brainmaker-sync-a");
        assert_eq!(json["at"], "2026-10-25T02:10:00+01:00");
        assert!(json.get("content_version").is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn syncs_of_one_client_skip_the_fleet() {
        let server = Server::start(vec![
            Route::token("outbox:read", r#"{"access_token":"r"}"#),
            Route::get(
                "/admin/clients/brainmaker-sync-old/syncs?limit=50",
                serde_json::json!({ "syncs": [row(1, "2026-09-20T09:00:00+02:00")] }).to_string(),
            ),
        ]);
        let dir = temp_dir("admin-syncs-client");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let rows = syncs(&config, &Selector::Client("brainmaker-sync-old".into()), 50).unwrap();

        assert_eq!(rows.len(), 1);
        assert!(!server.received().iter().any(|r| r.path == "/admin/clients"));
        assert!(syncs(&config, &Selector::Client("../admin".into()), 50).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn syncs_of_an_operator_with_no_client_fail() {
        let server = fleet_server(fleet_body(&[]));
        let dir = temp_dir("admin-syncs-none");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let error = syncs(&config, &Selector::Operator("nobody".into()), 50).unwrap_err();

        assert!(
            format!("{error:#}").contains("no client to nobody"),
            "{error:#}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
