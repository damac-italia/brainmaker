// SPDX-License-Identifier: GPL-3.0-or-later

//! The admin commands: collect the notes, read the fleet, read the sync log
//! of the fleet, and read what each client reports about itself.
//!
//! # Who runs them
//!
//! The admin, on an install like everyone else's, in `~/.brainmaker`. `link`
//! connects nothing to Claude there, because the credential reads the outbox
//! and sends no notes; see [`crate::link`]. Every command asks the issuer for a
//! token with the scope `outbox:read`. The server decides what that token may
//! read, so these commands grant nothing by themselves.
//!
//! # What they print
//!
//! The client IDs that the server reports. The admin needs them to tell two
//! machines of one operator apart, and `syncs --client` takes one. A client ID
//! alone authenticates nothing. No command prints the credential of the
//! machine it runs on.
//!
//! # Why a client sends no note
//!
//! `diagnose` answers that from the admin's machine. It joins what the server
//! saw of a client with the report that the client sends about itself, and
//! names the cause in a line that starts with `finding`. The client decides
//! what it reports, and sends it with its sync: see [`crate::diagnostics`].
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
use crate::cause::{self, Cause};
use crate::config::{Config, validate_hash};
use crate::diagnostics::{Agent, Code, Command, word};
use crate::link::Presence;
use crate::lock;
use crate::outbox::{self, Rule};
use crate::remote;
use crate::selfupdate;
use crate::version;

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

// ---------------------------------------------------------- diagnostics ---

/// The first version of this program that has push. An older client syncs,
/// and sends no note.
const FIRST_VERSION_WITH_PUSH: &str = "0.1.8";

/// How much older than the last sync a report may be, in seconds, before
/// `diagnose` says that the state it shows can be old. A client reports at
/// most twice an hour, so a day is far past any wait.
const STALE_REPORT_SECS: i64 = 24 * 60 * 60;

/// Body of `GET {base}/admin/clients/<client_id>/diagnostics`. Any other
/// shape fails the command, as for the fleet view.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Diagnostics {
    client_id: String,
    report: Option<Report>,
    events: Vec<LoggedEvent>,
}

/// The last report of one client, as the server stored it. The client wrote
/// every value but `received_at`, and nothing verified them. A value is
/// absent when the report held none, or when it broke its rule on the server.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    /// When the server received the report.
    pub received_at: String,
    /// The clock of the client when it read its state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub content: ReportedContent,
    pub link: ReportedLink,
    pub outbox: ReportedOutbox,
    /// Lines of that report that the server did not know, and dropped.
    pub events_dropped: u64,
}

/// The content that a client says it holds.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub present: Option<bool>,
}

/// What a client says `link` wrote on its machine.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedLink {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook: Option<Presence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<Presence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<Agent>,
}

/// The notes that a client says it holds.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedOutbox {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_push_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_push_notes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator: Option<bool>,
}

/// One line of the run log of a client, as the server stored it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LoggedEvent {
    pub id: i64,
    /// The clock of the client when it wrote the line.
    pub at: String,
    pub command: Command,
    pub code: Code,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// What the admin reads of one client: what the server saw, what the client
/// reported, why no note arrives, and the newest lines of the run log.
#[derive(Debug, Serialize)]
pub struct Diagnosis {
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    pub retired: bool,
    /// The last sync that the server saw.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync_at: Option<String>,
    /// The version that the client named at that sync.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brainmaker_version: Option<String>,
    /// The notes of this client that the server holds.
    pub notes_stored: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<Report>,
    /// Each cause that the server and the report show for a client that
    /// sends no note, or that is not current.
    pub findings: Vec<String>,
    /// The run log, newest first.
    pub events: Vec<LoggedEvent>,
}

/// Reads, for each selected client, the report and the newest `limit` lines
/// of the run log, and names what they show.
///
/// The fleet view gives the clients of an operator, and what the server saw
/// of each one. The client sent the rest itself: this command asks no client
/// for anything.
pub fn diagnose(config: &Config, selector: &Selector, limit: u32) -> Result<Vec<Diagnosis>> {
    match selector {
        Selector::Client(client_id) => check_client_id(client_id)?,
        Selector::Operator(operator) => check_operator(operator)?,
    }
    let clients: Vec<Client> = fleet(config)?
        .clients
        .into_iter()
        .filter(|client| match selector {
            Selector::Client(client_id) => &client.client_id == client_id,
            Selector::Operator(operator) => client.operator.as_deref() == Some(operator.as_str()),
        })
        .collect();
    if clients.is_empty() {
        match selector {
            Selector::Client(client_id) => {
                bail!("the server knows no client with the ID {client_id}")
            }
            Selector::Operator(operator) => bail!("the server registers no client to {operator}"),
        }
    }

    clients
        .into_iter()
        .map(|client| {
            let url = config.admin_diagnostics_url(&client.client_id, limit);
            let body = remote::fetch_admin(config, &url, MAX_REPORT_BYTES)
                .map_err(with_scope_hint)
                .map_err(with_route_hint)?;
            let found: Diagnostics = serde_json::from_str(&body)
                .with_context(|| format!("{url} did not return the expected diagnostics"))?;
            check_diagnostics(&found, &client.client_id)?;
            Ok(Diagnosis {
                findings: findings(&client, found.report.as_ref(), &found.events),
                client_id: client.client_id,
                operator: client.operator,
                retired: client.retired,
                last_sync_at: client.last_seen_at,
                brainmaker_version: client.brainmaker_version,
                notes_stored: client.notes_pushed_total,
                report: found.report,
                events: found.events,
            })
        })
        .collect()
}

/// Says that a server older than the diagnostics route answers 404 too.
fn with_route_hint(error: anyhow::Error) -> anyhow::Error {
    if cause::of(&error).status == Some(404) {
        return error.context(
            "the server has no diagnostics for this client; a server older than the \
             diagnostics route answers 404 too",
        );
    }
    error
}

/// Checks every value of the diagnostics that is not a word of a fixed list:
/// the client, each time, each version, and each content hash.
fn check_diagnostics(found: &Diagnostics, client_id: &str) -> Result<()> {
    if found.client_id != client_id {
        bail!("the server answered with the diagnostics of another client than {client_id}");
    }
    let check_version = |value: &str| -> Result<()> {
        if !version::validate(value) {
            bail!(
                "the server sent the version {:?}, which is not a version",
                remote::printable(value)
            );
        }
        Ok(())
    };
    if let Some(report) = &found.report {
        check_time(&report.received_at, "received_at")?;
        for (what, time) in [
            ("at", &report.at),
            ("content.installed_at", &report.content.installed_at),
            ("outbox.last_push_at", &report.outbox.last_push_at),
        ] {
            if let Some(time) = time {
                check_time(time, what)?;
            }
        }
        if let Some(value) = &report.version {
            check_version(value)?;
        }
        if let Some(platform) = &report.platform
            && !platform
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
        {
            bail!(
                "the server sent the platform {:?}, which is not a platform key",
                remote::printable(platform)
            );
        }
        if let Some(hash) = &report.content.installed_hash {
            validate_hash(hash)?;
        }
    }
    for event in &found.events {
        check_time(&event.at, "at")?;
        if let Some(value) = &event.version {
            check_version(value)?;
        }
        if let Some(hash) = &event.hash {
            validate_hash(hash)?;
        }
    }
    Ok(())
}

/// Names each cause that the server and the report show for a client that
/// sends no note, or that is not current.
///
/// The four causes that the server alone cannot see come first to mind: the
/// client is too old, `link` never connected Claude, every note broke a rule
/// on the machine, or no note was written at all.
fn findings(client: &Client, report: Option<&Report>, events: &[LoggedEvent]) -> Vec<String> {
    let mut found = Vec::new();
    if client.retired {
        found.push(
            "The registry marks this client as retired, so the server refuses its notes."
                .to_string(),
        );
    } else if client.operator.is_none() {
        found.push(
            "The registry names no operator for this client, so the server refuses its notes. \
             Register the client."
                .to_string(),
        );
    }

    let Some(report) = report else {
        found.push(match (&client.last_seen_at, &client.brainmaker_version) {
            (None, _) => "The server never saw this client sync.".to_string(),
            (Some(_), Some(named)) if version::is_newer(FIRST_VERSION_WITH_PUSH, named) => format!(
                "This client runs brainmaker {}. That version has no push, which came with \
                 {FIRST_VERSION_WITH_PUSH}, so it sends no note and no report. Update it.",
                remote::printable(named)
            ),
            (Some(_), named) => format!(
                "The server holds no report from this client. Its last sync named {}, and a \
                 version that sends reports sends one with its sync. So this client is too \
                 old for reports, or its reports do not arrive. This brainmaker is {}.",
                named
                    .as_deref()
                    .map(|named| format!("brainmaker {}", remote::printable(named)))
                    .unwrap_or_else(|| "no version".to_string()),
                selfupdate::CURRENT_VERSION
            ),
        });
        return found;
    };

    // A report that the syncs left behind describes the machine as it was.
    let behind = client
        .last_seen_at
        .as_deref()
        .and_then(instant)
        .zip(instant(&report.received_at))
        .is_some_and(|(synced, received)| synced.saturating_sub(received) > STALE_REPORT_SECS);
    if behind {
        found.push(format!(
            "The last report is from {}, and the client synced after it, at {}. Its newer \
             reports did not arrive, so the state below can be old.",
            report.received_at,
            client.last_seen_at.as_deref().unwrap_or("-")
        ));
    }

    match report.link.hook {
        Some(Presence::Absent) => found.push(
            "The SessionStart hook of brainmaker is not in the Claude settings of that \
             machine. No session there reads the briefing or learns of the outbox, so Claude \
             writes no note. Run link on that machine. The admin's own install has no hook \
             on purpose."
                .to_string(),
        ),
        Some(Presence::Unknown) => found.push(
            "brainmaker could not read the Claude settings of that machine, so the report \
             does not say whether the SessionStart hook is there."
                .to_string(),
        ),
        Some(Presence::Present) | None => {}
    }

    let waiting = report.outbox.waiting.unwrap_or(0);
    let rejected = report.outbox.rejected.unwrap_or(0);
    let sent = report.outbox.sent.unwrap_or(0);
    let none_arrived = sent == 0 && client.notes_pushed_total == 0;
    if rejected > 0 {
        let mut text = format!(
            "{rejected} note(s) broke a rule on that machine and moved to rejected/{}.",
            if none_arrived {
                ", and none was sent"
            } else {
                ""
            }
        );
        let rules = broken_rules(events);
        if !rules.is_empty() {
            text.push_str(&format!(
                " The run log names the rule: {}.",
                rules.join(", ")
            ));
        }
        found.push(text);
    }
    if waiting > 0 {
        let mut text = format!("{waiting} note(s) wait in the outbox of that machine.");
        if let Some(reason) = last_push(events) {
            text.push(' ');
            text.push_str(&reason);
        }
        found.push(text);
    }
    if waiting == 0 && rejected == 0 && none_arrived && report.link.hook != Some(Presence::Absent) {
        found.push(
            "No note was ever written on that machine: its outbox holds none, none was \
             rejected, and none was sent."
                .to_string(),
        );
    }

    if let Some(runs) = &report.version
        && version::is_newer(selfupdate::CURRENT_VERSION, runs)
    {
        let mut text = format!(
            "That machine runs brainmaker {}, and this one runs {}.",
            remote::printable(runs),
            selfupdate::CURRENT_VERSION
        );
        if matches!(report.link.agent, Some(Agent::Absent | Agent::None)) {
            text.push_str(
                " It has no hourly agent, so it updates only when someone runs self-update.",
            );
        }
        found.push(text);
    }
    if report.events_dropped > 0 {
        found.push(format!(
            "The server did not know {} line(s) of the last report and dropped them. The \
             server is older than that client.",
            report.events_dropped
        ));
    }
    found
}

/// Each rule that the run log names for a rejected note, with its count:
/// `kind (2)`.
fn broken_rules(events: &[LoggedEvent]) -> Vec<String> {
    let mut by_rule: BTreeMap<Rule, u64> = BTreeMap::new();
    for event in events {
        if let (Code::PushRejected, Some(rule)) = (event.code, event.rule) {
            let count = by_rule.entry(rule).or_default();
            *count = count.saturating_add(event.count.unwrap_or(1));
        }
    }
    by_rule
        .into_iter()
        .map(|(rule, count)| format!("{} ({count})", word(&rule)))
        .collect()
}

/// What the newest push line of the run log says about notes that wait.
fn last_push(events: &[LoggedEvent]) -> Option<String> {
    let event = events.iter().find(|event| {
        matches!(
            event.code,
            Code::PushStopped | Code::PushSettling | Code::PushSent | Code::PushFailed
        )
    })?;
    match event.code {
        Code::PushStopped => Some(format!(
            "The last push stopped: {}.",
            match (event.cause, event.status) {
                (Some(Cause::Scope), _) =>
                    "the issuer does not grant this client the scope outbox:write".to_string(),
                (Some(Cause::Credentials), _) =>
                    "the issuer refused the client credentials".to_string(),
                (Some(Cause::Unreachable), _) =>
                    "the server or the issuer did not answer".to_string(),
                (Some(Cause::Http), Some(403)) =>
                    "the server refused the note with HTTP 403, as it does for a client with no \
                     operator and for a retired one"
                        .to_string(),
                (Some(Cause::Http), Some(404)) =>
                    "the server answered HTTP 404, so it has no outbox route".to_string(),
                (Some(Cause::Http), Some(429)) =>
                    "the server limits the notes of this client (HTTP 429)".to_string(),
                (Some(Cause::Http), Some(status)) => format!("the server answered HTTP {status}"),
                _ => "the run log names no cause".to_string(),
            }
        )),
        Code::PushSettling => Some(
            "They changed less than a minute before the last run, so push left them for the \
             next run."
                .to_string(),
        ),
        Code::PushFailed => Some("The last push failed on that machine.".to_string()),
        _ => None,
    }
}

/// The lines that `diagnose` prints for one client. Every value from the
/// server loses its control characters.
pub fn diagnosis_lines(diagnosis: &Diagnosis) -> Vec<String> {
    let value = |text: &Option<String>| {
        text.as_deref()
            .map(remote::printable)
            .unwrap_or_else(|| "-".to_string())
    };
    let number = |count: Option<u64>| {
        count
            .map(|count| count.to_string())
            .unwrap_or_else(|| "-".to_string())
    };
    fn named<T: Serialize>(value: &Option<T>) -> String {
        value.as_ref().map(word).unwrap_or_else(|| "-".to_string())
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "{}  {}{}",
        diagnosis.operator.as_deref().unwrap_or("unregistered"),
        diagnosis.client_id,
        if diagnosis.retired { "  retired" } else { "" }
    ));
    lines.push(format!(
        "  server    last sync {}  version {}  notes stored {}",
        value(&diagnosis.last_sync_at),
        value(&diagnosis.brainmaker_version),
        diagnosis.notes_stored
    ));
    match &diagnosis.report {
        None => lines.push("  report    none".to_string()),
        Some(report) => {
            lines.push(format!(
                "  report    received {}  read on the client at {}",
                report.received_at,
                value(&report.at)
            ));
            lines.push(format!(
                "  software  {}  {}",
                value(&report.version),
                value(&report.platform)
            ));
            lines.push(format!(
                "  content   {}  installed {}  {}",
                value(&report.content.installed_hash),
                value(&report.content.installed_at),
                match report.content.present {
                    Some(true) => "present",
                    Some(false) => "missing",
                    None => "-",
                }
            ));
            lines.push(format!(
                "  link      hook {}  block {}  skills {}  agent {}",
                named(&report.link.hook),
                named(&report.link.block),
                number(report.link.skills),
                named(&report.link.agent)
            ));
            lines.push(format!(
                "  outbox    waiting {}  rejected {}  sent {}  last push {}  operator {}",
                number(report.outbox.waiting),
                number(report.outbox.rejected),
                number(report.outbox.sent),
                value(&report.outbox.last_push_at),
                match report.outbox.operator {
                    Some(true) => "named",
                    Some(false) => "not named",
                    None => "-",
                }
            ));
        }
    }
    if diagnosis.findings.is_empty() {
        lines.push("  finding   none".to_string());
    }
    for finding in &diagnosis.findings {
        lines.push(format!("  finding   {finding}"));
    }
    for event in &diagnosis.events {
        lines.push(format!("  log       {}", event_line(event)));
    }
    lines
}

/// One line of the run log: the time, the command, what it did, and then each
/// value that the line holds.
fn event_line(event: &LoggedEvent) -> String {
    let mut line = format!(
        "{}  {}  {}",
        event.at,
        word(&event.command),
        word(&event.code)
    );
    if let Some(cause) = &event.cause {
        line.push_str(&format!("  {}", word(cause)));
    }
    if let Some(status) = event.status {
        line.push_str(&format!("  HTTP {status}"));
    }
    if let Some(rule) = &event.rule {
        line.push_str(&format!("  rule {}", word(rule)));
    }
    if let Some(count) = event.count {
        line.push_str(&format!("  count {count}"));
    }
    if let Some(hash) = &event.hash {
        line.push_str(&format!("  {}", remote::printable(hash)));
    }
    if let Some(version) = &event.version {
        line.push_str(&format!("  {}", remote::printable(version)));
    }
    line
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

    // ------------------------------------------------------ diagnostics ---

    use serde_json::json;

    const GABRIELE: &str = "brainmaker-sync-gabriele";

    /// The fleet row of one client of gabriele: the version that it named at
    /// its last sync, and the notes of it that the server holds.
    fn seen(version: Option<&str>, notes: u64) -> Client {
        let mut row = client(
            GABRIELE,
            Some("gabriele"),
            false,
            Some("2026-10-05T10:00:03+02:00"),
        );
        row["brainmaker_version"] = json!(version);
        row["notes_pushed_total"] = json!(notes);
        serde_json::from_value(row).unwrap()
    }

    /// The report of a laptop that works, as the server answers it.
    fn healthy() -> serde_json::Value {
        json!({
            "received_at": "2026-10-05T10:00:04+02:00",
            "at": "2026-10-05T10:00:02+02:00",
            "version": selfupdate::CURRENT_VERSION,
            "platform": "darwin-arm64",
            "content": {
                "installed_hash": "a377aa94",
                "installed_at": "2026-10-01T09:00:00+02:00",
                "present": true
            },
            "link": { "hook": "present", "block": "present", "skills": 12, "agent": "present" },
            "outbox": {
                "waiting": 0, "rejected": 0, "sent": 3,
                "last_push_at": "2026-10-04T18:12:40+02:00", "last_push_notes": 1,
                "operator": true
            },
            "events_dropped": 0
        })
    }

    /// A report whose outbox holds these counts.
    fn with_notes(waiting: u64, rejected: u64, sent: u64) -> serde_json::Value {
        let mut report = healthy();
        report["outbox"]["waiting"] = json!(waiting);
        report["outbox"]["rejected"] = json!(rejected);
        report["outbox"]["sent"] = json!(sent);
        report
    }

    /// One line of a run log, as the server answers it.
    fn logged(id: i64, command: &str, code: &str) -> serde_json::Value {
        json!({
            "id": id, "at": "2026-10-05T10:00:02+02:00", "command": command, "code": code,
            "cause": null, "status": null, "rule": null, "count": null, "hash": null,
            "version": null
        })
    }

    fn with(
        mut value: serde_json::Value,
        key: &str,
        field: serde_json::Value,
    ) -> serde_json::Value {
        value[key] = field;
        value
    }

    /// The findings for one client, its report, and its run log.
    fn found(
        client: &Client,
        report: Option<serde_json::Value>,
        events: Vec<serde_json::Value>,
    ) -> Vec<String> {
        let report: Option<Report> = report.map(|report| serde_json::from_value(report).unwrap());
        let events: Vec<LoggedEvent> = events
            .into_iter()
            .map(|event| serde_json::from_value(event).unwrap())
            .collect();
        findings(client, report.as_ref(), &events)
    }

    // The four causes of the operators who synced and sent no note. Each one
    // was local to the machine, and the admin could not see it.

    #[test]
    fn the_diagnosis_names_a_client_that_is_too_old_to_send_notes() {
        // A client older than this report sends none, so the version comes
        // from what the server saw at its last sync.
        let named = found(&seen(Some("0.1.7"), 0), None, Vec::new());
        assert_eq!(named.len(), 1, "{named:?}");
        assert!(named[0].contains("runs brainmaker 0.1.7"), "{named:?}");
        assert!(named[0].contains("has no push"), "{named:?}");
        assert!(named[0].contains("came with 0.1.8"), "{named:?}");

        // 0.1.8 has push and no report: the finding says what is known.
        let named = found(&seen(Some("0.1.8"), 0), None, Vec::new());
        assert_eq!(named.len(), 1, "{named:?}");
        assert!(named[0].contains("holds no report"), "{named:?}");
        assert!(named[0].contains("named brainmaker 0.1.8"), "{named:?}");
        assert!(!named[0].contains("has no push"), "{named:?}");
    }

    #[test]
    fn the_diagnosis_names_a_missing_link_to_claude() {
        let mut report = with_notes(0, 0, 0);
        report["link"] =
            json!({ "hook": "absent", "block": "absent", "skills": 0, "agent": "present" });
        let events = vec![logged(1, "sync", "content.up_to_date")];

        let named = found(
            &seen(Some(selfupdate::CURRENT_VERSION), 0),
            Some(report),
            events,
        );

        assert_eq!(named.len(), 1, "{named:?}");
        assert!(named[0].contains("SessionStart hook"), "{named:?}");
        assert!(
            named[0].contains("is not in the Claude settings"),
            "{named:?}"
        );
        assert!(named[0].contains("Run link"), "{named:?}");
    }

    #[test]
    fn the_diagnosis_says_when_the_report_is_older_than_the_syncs() {
        // The client synced on 5 October, and its last report is of 1 October.
        let mut old = healthy();
        old["received_at"] = json!("2026-10-01T09:00:00+02:00");
        let named = found(
            &seen(Some(selfupdate::CURRENT_VERSION), 3),
            Some(old),
            Vec::new(),
        );
        assert_eq!(named.len(), 1, "{named:?}");
        assert!(
            named[0].contains("is from 2026-10-01T09:00:00+02:00"),
            "{named:?}"
        );
        assert!(named[0].contains("can be old"), "{named:?}");

        // A report of the same day is not behind.
        let mut fresh = healthy();
        fresh["received_at"] = json!("2026-10-04T12:00:00+02:00");
        let named = found(
            &seen(Some(selfupdate::CURRENT_VERSION), 3),
            Some(fresh),
            Vec::new(),
        );
        assert!(named.is_empty(), "{named:?}");
    }

    #[test]
    fn the_diagnosis_names_notes_that_all_broke_a_rule() {
        let events = vec![
            with(
                with(logged(3, "sync", "push.rejected"), "rule", json!("kind")),
                "count",
                json!(1),
            ),
            logged(2, "sync", "content.up_to_date"),
            with(
                with(
                    logged(1, "sync", "push.rejected"),
                    "rule",
                    json!("frontmatter"),
                ),
                "count",
                json!(5),
            ),
            with(
                with(logged(0, "push", "push.rejected"), "rule", json!("kind")),
                "count",
                json!(8),
            ),
        ];

        let named = found(
            &seen(Some(selfupdate::CURRENT_VERSION), 0),
            Some(with_notes(0, 14, 0)),
            events,
        );

        assert_eq!(named.len(), 1, "{named:?}");
        assert!(named[0].contains("14 note(s) broke a rule"), "{named:?}");
        assert!(named[0].contains("moved to rejected/"), "{named:?}");
        assert!(named[0].contains("none was sent"), "{named:?}");
        assert!(named[0].contains("frontmatter (5), kind (9)"), "{named:?}");
    }

    #[test]
    fn the_diagnosis_names_an_outbox_that_never_held_a_note() {
        let named = found(
            &seen(Some(selfupdate::CURRENT_VERSION), 0),
            Some(with_notes(0, 0, 0)),
            vec![logged(1, "sync", "content.up_to_date")],
        );
        assert_eq!(named.len(), 1, "{named:?}");
        assert!(named[0].contains("No note was ever written"), "{named:?}");
    }

    #[test]
    fn a_client_whose_notes_arrive_has_no_finding() {
        let named = found(
            &seen(Some(selfupdate::CURRENT_VERSION), 3),
            Some(healthy()),
            Vec::new(),
        );
        assert!(named.is_empty(), "{named:?}");
        // The server holds its notes, though the laptop keeps no sent copy.
        let named = found(
            &seen(Some(selfupdate::CURRENT_VERSION), 3),
            Some(with_notes(0, 0, 0)),
            Vec::new(),
        );
        assert!(named.is_empty(), "{named:?}");
    }

    #[test]
    fn the_diagnosis_names_why_notes_wait() {
        let stopped = |cause: &str, status: serde_json::Value| {
            with(
                with(logged(2, "sync", "push.stopped"), "cause", json!(cause)),
                "status",
                status,
            )
        };
        for (event, needle) in [
            (
                stopped("scope", json!(null)),
                "does not grant this client the scope outbox:write",
            ),
            (stopped("http", json!(403)), "HTTP 403"),
            (stopped("http", json!(404)), "no outbox route"),
            (stopped("http", json!(429)), "limits the notes"),
            (stopped("http", json!(502)), "HTTP 502"),
            (stopped("unreachable", json!(null)), "did not answer"),
            (
                stopped("credentials", json!(401)),
                "refused the client credentials",
            ),
            (
                with(logged(2, "sync", "push.settling"), "count", json!(2)),
                "less than a minute",
            ),
            (logged(2, "push", "push.failed"), "failed on that machine"),
        ] {
            // An older line of another push does not hide the newest one.
            let events = vec![event, stopped("http", json!(500))];
            let named = found(
                &seen(Some(selfupdate::CURRENT_VERSION), 3),
                Some(with_notes(2, 0, 3)),
                events,
            );
            assert_eq!(named.len(), 1, "{named:?}");
            assert!(
                named[0].starts_with("2 note(s) wait in the outbox"),
                "{named:?}"
            );
            assert!(named[0].contains(needle), "{needle}: {named:?}");
        }
    }

    #[test]
    fn the_diagnosis_names_what_the_registry_and_the_versions_show() {
        let mut unregistered = client(GABRIELE, None, false, Some("2026-10-05T10:00:03+02:00"));
        unregistered["notes_pushed_total"] = json!(3);
        let unregistered: Client = serde_json::from_value(unregistered).unwrap();
        let named = found(&unregistered, Some(healthy()), Vec::new());
        assert_eq!(named.len(), 1, "{named:?}");
        assert!(named[0].contains("names no operator"), "{named:?}");

        let retired: Client =
            serde_json::from_value(client(GABRIELE, Some("gabriele"), true, None)).unwrap();
        let named = found(&retired, None, Vec::new());
        assert!(named[0].contains("retired"), "{named:?}");
        assert!(named[1].contains("never saw this client sync"), "{named:?}");

        // A client that reports an older version, with no agent to update it.
        let mut old = healthy();
        old["version"] = json!("0.0.1");
        old["link"]["agent"] = json!("none");
        old["events_dropped"] = json!(2);
        let named = found(&seen(Some("0.0.1"), 3), Some(old), Vec::new());
        assert_eq!(named.len(), 2, "{named:?}");
        assert!(named[0].contains("runs brainmaker 0.0.1"), "{named:?}");
        assert!(named[0].contains("no hourly agent"), "{named:?}");
        assert!(named[1].contains("did not know 2 line(s)"), "{named:?}");

        let mut unreadable = healthy();
        unreadable["link"]["hook"] = json!("unknown");
        let named = found(&seen(None, 3), Some(unreadable), Vec::new());
        assert!(
            named[0].contains("could not read the Claude settings"),
            "{named:?}"
        );
    }

    fn diagnostics_body(
        client_id: &str,
        report: Option<serde_json::Value>,
        events: Vec<serde_json::Value>,
    ) -> String {
        json!({ "client_id": client_id, "report": report, "events": events }).to_string()
    }

    #[test]
    fn diagnose_reads_every_client_of_an_operator_and_prints_what_it_found() {
        let mut absent = with_notes(0, 0, 0);
        absent["link"] =
            json!({ "hook": "absent", "block": "absent", "skills": 0, "agent": "present" });
        let events = vec![
            with(
                logged(8, "sync", "content.up_to_date"),
                "hash",
                json!("a377aa94"),
            ),
            with(
                with(
                    logged(7, "sync", "content.unreachable"),
                    "cause",
                    json!("http"),
                ),
                "status",
                json!(503),
            ),
            with(
                logged(6, "self-update", "update.installed"),
                "version",
                json!("0.1.9"),
            ),
            with(
                with(logged(5, "push", "push.rejected"), "rule", json!("kind")),
                "count",
                json!(2),
            ),
        ];
        let mut first = client(
            "brainmaker-sync-a",
            Some("gabriele"),
            false,
            Some("2026-10-05T10:00:03+02:00"),
        );
        first["brainmaker_version"] = json!(selfupdate::CURRENT_VERSION);
        first["notes_pushed_total"] = json!(0);
        let mut second = client(
            "brainmaker-sync-b",
            Some("gabriele"),
            true,
            Some("2026-09-01T10:00:03+02:00"),
        );
        second["brainmaker_version"] = json!("0.1.7");
        let server = Server::start(vec![
            Route::token("outbox:read", r#"{"access_token":"read-token"}"#),
            Route::get(
                "/admin/clients",
                fleet_body(&[
                    first,
                    second,
                    client("brainmaker-sync-c", Some("anna"), false, None),
                ]),
            ),
            Route::get(
                "/admin/clients/brainmaker-sync-a/diagnostics?limit=20",
                diagnostics_body("brainmaker-sync-a", Some(absent), events),
            ),
            Route::get(
                "/admin/clients/brainmaker-sync-b/diagnostics?limit=20",
                diagnostics_body("brainmaker-sync-b", None, Vec::new()),
            ),
        ]);
        let dir = temp_dir("admin-diagnose");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let diagnoses = diagnose(&config, &Selector::Operator("gabriele".into()), 20).unwrap();

        assert_eq!(diagnoses.len(), 2);
        let received = server.received();
        assert_eq!(received[0].form("scope").as_deref(), Some("outbox:read"));
        assert!(
            received[1..]
                .iter()
                .all(|request| request.header("authorization") == Some("Bearer read-token"))
        );
        let version = selfupdate::CURRENT_VERSION;
        assert_eq!(
            diagnosis_lines(&diagnoses[0]),
            [
                "gabriele  brainmaker-sync-a".to_string(),
                format!(
                    "  server    last sync 2026-10-05T10:00:03+02:00  version {version}  notes \
                     stored 0"
                ),
                "  report    received 2026-10-05T10:00:04+02:00  read on the client at \
                 2026-10-05T10:00:02+02:00"
                    .to_string(),
                format!("  software  {version}  darwin-arm64"),
                "  content   a377aa94  installed 2026-10-01T09:00:00+02:00  present".to_string(),
                "  link      hook absent  block absent  skills 0  agent present".to_string(),
                "  outbox    waiting 0  rejected 0  sent 0  last push 2026-10-04T18:12:40+02:00  \
                 operator named"
                    .to_string(),
                "  finding   The SessionStart hook of brainmaker is not in the Claude settings \
                 of that machine. No session there reads the briefing or learns of the outbox, \
                 so Claude writes no note. Run link on that machine. The admin's own install \
                 has no hook on purpose."
                    .to_string(),
                "  log       2026-10-05T10:00:02+02:00  sync  content.up_to_date  a377aa94"
                    .to_string(),
                "  log       2026-10-05T10:00:02+02:00  sync  content.unreachable  http  HTTP 503"
                    .to_string(),
                "  log       2026-10-05T10:00:02+02:00  self-update  update.installed  0.1.9"
                    .to_string(),
                "  log       2026-10-05T10:00:02+02:00  push  push.rejected  rule kind  count 2"
                    .to_string(),
            ]
        );
        let lines = diagnosis_lines(&diagnoses[1]);
        assert_eq!(lines[0], "gabriele  brainmaker-sync-b  retired");
        assert_eq!(
            lines[1],
            "  server    last sync 2026-09-01T10:00:03+02:00  version 0.1.7  notes stored 2"
        );
        assert_eq!(lines[2], "  report    none");
        assert!(
            lines[3].starts_with("  finding   The registry marks"),
            "{lines:?}"
        );
        assert!(lines[4].contains("has no push"), "{lines:?}");
        assert_eq!(lines.len(), 5);

        // The JSON form: a key without a value is left out.
        let json = serde_json::to_value(&diagnoses).unwrap();
        assert_eq!(json[0]["client_id"], "brainmaker-sync-a");
        assert_eq!(json[0]["report"]["link"]["hook"], "absent");
        assert_eq!(json[0]["findings"].as_array().unwrap().len(), 1);
        assert_eq!(json[0]["events"][1]["status"], 503);
        assert!(json[0]["events"][0].get("cause").is_none());
        assert!(json[1].get("report").is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diagnose_of_one_client_reads_that_client_alone() {
        let server = Server::start(vec![
            Route::token("outbox:read", r#"{"access_token":"r"}"#),
            Route::get(
                "/admin/clients",
                fleet_body(&[
                    client("brainmaker-sync-a", Some("gabriele"), false, None),
                    client("brainmaker-sync-old", None, false, None),
                ]),
            ),
            Route::get(
                "/admin/clients/brainmaker-sync-old/diagnostics?limit=5",
                diagnostics_body("brainmaker-sync-old", Some(healthy()), Vec::new()),
            ),
        ]);
        let dir = temp_dir("admin-diagnose-client");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let diagnoses =
            diagnose(&config, &Selector::Client("brainmaker-sync-old".into()), 5).unwrap();

        assert_eq!(diagnoses.len(), 1);
        assert_eq!(
            diagnosis_lines(&diagnoses[0])[0],
            "unregistered  brainmaker-sync-old"
        );
        let asked: Vec<String> = server.received().into_iter().map(|r| r.path).collect();
        assert_eq!(
            asked,
            [
                "/oauth2/token",
                "/admin/clients",
                "/admin/clients/brainmaker-sync-old/diagnostics?limit=5"
            ]
        );
        // A client that the fleet does not list, and an ID that is none.
        let error =
            diagnose(&config, &Selector::Client("brainmaker-sync-x".into()), 5).unwrap_err();
        assert!(
            format!("{error:#}").contains("knows no client with the ID brainmaker-sync-x"),
            "{error:#}"
        );
        assert!(diagnose(&config, &Selector::Client("../admin".into()), 5).is_err());
        let error = diagnose(&config, &Selector::Operator("nobody".into()), 5).unwrap_err();
        assert!(
            format!("{error:#}").contains("no client to nobody"),
            "{error:#}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_server_without_the_diagnostics_route_gets_a_clear_error() {
        // The test server answers 404 for a route that it does not have, as a
        // server older than this route does.
        let server = fleet_server(fleet_body(&[client(
            "brainmaker-sync-a",
            Some("gabriele"),
            false,
            None,
        )]));
        let dir = temp_dir("admin-diagnose-old-server");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());

        let error = diagnose(&config, &Selector::Operator("gabriele".into()), 20).unwrap_err();

        let text = format!("{error:#}");
        assert!(text.contains("older than the diagnostics route"), "{text}");
        assert!(text.contains("HTTP 404"), "{text}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diagnostics_of_another_shape_are_refused() {
        let mut extra = healthy();
        extra["error_text"] = json!("cannot read the URL");
        let mut bad_time = healthy();
        bad_time["received_at"] = json!("yesterday");
        let mut bad_hash = healthy();
        bad_hash["content"]["installed_hash"] = json!("../../etc");
        let mut bad_version = healthy();
        bad_version["version"] = json!("0.1.9\u{1b}[2J");
        let mut bad_word = healthy();
        bad_word["link"]["hook"] = json!("maybe");
        let mut bad_platform = healthy();
        bad_platform["platform"] = json!("darwin arm64\u{7}");
        let a = "brainmaker-sync-a";
        for body in [
            diagnostics_body(a, Some(extra), Vec::new()),
            diagnostics_body(a, Some(bad_time), Vec::new()),
            diagnostics_body(a, Some(bad_hash), Vec::new()),
            diagnostics_body(a, Some(bad_version), Vec::new()),
            diagnostics_body(a, Some(bad_word), Vec::new()),
            diagnostics_body(a, Some(bad_platform), Vec::new()),
            // A line with a word that this program does not know.
            diagnostics_body(a, None, vec![logged(1, "sync", "content.replaced")]),
            diagnostics_body(a, None, vec![logged(1, "rm -rf", "content.updated")]),
            diagnostics_body(
                a,
                None,
                vec![with(
                    logged(1, "sync", "content.updated"),
                    "at",
                    json!("now"),
                )],
            ),
            diagnostics_body(
                a,
                None,
                vec![with(
                    logged(1, "sync", "content.updated"),
                    "text",
                    json!("x"),
                )],
            ),
            // The diagnostics of another client than the one that was asked.
            diagnostics_body("brainmaker-sync-b", None, Vec::new()),
        ] {
            let server = Server::start(vec![
                Route::token("outbox:read", r#"{"access_token":"r"}"#),
                Route::get(
                    "/admin/clients",
                    fleet_body(&[client(a, Some("gabriele"), false, None)]),
                ),
                Route::get(
                    "/admin/clients/brainmaker-sync-a/diagnostics?limit=20",
                    body.clone(),
                ),
            ]);
            let dir = temp_dir("admin-diagnose-shape");
            let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());
            assert!(
                diagnose(&config, &Selector::Client(a.into()), 20).is_err(),
                "accepted {body}"
            );
            fs::remove_dir_all(&dir).unwrap();
        }
    }
}
