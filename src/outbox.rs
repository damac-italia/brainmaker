// SPDX-License-Identifier: GPL-3.0-or-later

//! The outbox: the notes that the operator's Claude writes, and `push`, which
//! sends them to the server.
//!
//! # Why this exists
//!
//! At the end of a session, the operator's Claude writes one note into
//! `~/.brainmaker/outbox/`. The admin collects the notes from the server. The
//! name on a note comes from the server, from the credential that sent it, so
//! nothing here names the operator: the server answers that through `whoami`,
//! and this module writes the answer to the `operator` file for Claude to read.
//!
//! # A new trust boundary
//!
//! Push is the first path from the disk to the network. A symbolic link in the
//! outbox could otherwise send any file the user can read. So push sends only a
//! regular file, never through a link, with a name that matches the note rule,
//! at most [`MAX_NOTE_BYTES`], in UTF-8, with a frontmatter that passes the
//! same rules the server applies. A file that changed in the last 60 seconds
//! waits for the next run, so a note that Claude is still writing never leaves
//! half written.
//!
//! # What happens to a file
//!
//! | Answer | The file |
//! |---|---|
//! | 201 or 200 | moves to `sent/<YYYY-MM>/`, the month the server received it |
//! | 400 or 413, or a broken rule here | moves to `rejected/`, beside `<name>.reason.txt` |
//! | anything else, or no answer | stays, and the run stops |
//!
//! A run holds `.outbox.lock`, so two runs never send one note twice at once,
//! and the server answers a note it already holds as a duplicate, so a run that
//! was stopped half way loses nothing.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::config::{Config, MAX_NOTE_BYTES};
use crate::lock;
use crate::remote;

/// The values `kind` may take.
pub const KINDS: [&str; 4] = ["fact", "decision", "anomaly", "question"];

/// How long a file must stay unchanged before push sends it.
pub const SETTLE_TIME: Duration = Duration::from_secs(60);

/// Longest part of a note name after its date, before `.md`.
const MAX_SLUG_LEN: usize = 100;

/// Longest `domain`, and longest operator name.
const MAX_NAME_LEN: usize = 32;

/// The UTF-8 byte-order mark, which a note may not start with.
const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Name of the header that reports the platform.
pub const HEADER_PLATFORM: &str = "Brainmaker-Platform";

/// Name of the header that reports the installed content.
pub const HEADER_CONTENT: &str = "Brainmaker-Content";

/// Name of the header that reports the notes waiting.
pub const HEADER_OUTBOX: &str = "Brainmaker-Outbox";

// -------------------------------------------------------------- the rules ---

/// A note name is `YYYY-MM-DD-<slug>.md`, where the slug is lowercase ASCII
/// letters, digits, and `-`, starting with a letter or a digit. The server
/// applies the same rule, and the name becomes a path on the admin's machine.
pub fn validate_name(name: &str) -> Result<(), String> {
    let bad = || {
        format!(
            "the note name {name:?} is not YYYY-MM-DD-<slug>.md, where the slug is 1 to \
             {MAX_SLUG_LEN} lowercase letters, digits, and '-', starting with a letter or a digit"
        )
    };
    let stem = name.strip_suffix(".md").ok_or_else(bad)?;
    let bytes = stem.as_bytes();
    if bytes.len() < 12 {
        return Err(bad());
    }
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    let date_ok = digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10)
        && bytes[10] == b'-';
    let slug = &bytes[11..];
    let slug_ok = slug.len() <= MAX_SLUG_LEN
        && (slug[0].is_ascii_lowercase() || slug[0].is_ascii_digit())
        && slug
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
    if date_ok && slug_ok {
        Ok(())
    } else {
        Err(bad())
    }
}

/// What the frontmatter of a note says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub kind: String,
    pub domain: String,
}

/// Checks a note body, and reads its frontmatter. The size cap is the
/// caller's. The error names the rule that the body breaks.
pub fn check_note(body: &[u8]) -> Result<Note, String> {
    if body.is_empty() {
        return Err("the note is empty".to_string());
    }
    if body.starts_with(BOM) {
        return Err("the note starts with a byte-order mark".to_string());
    }
    let text = std::str::from_utf8(body).map_err(|_| "the note is not valid UTF-8".to_string())?;
    if text.contains('\0') {
        return Err("the note holds a NUL character".to_string());
    }
    parse_frontmatter(text)
}

/// Reads the frontmatter: `---` and a line end, `key: value` lines, and a line
/// that is exactly `---`.
///
/// Only the keys a note needs are read. Indented lines and list items belong
/// to the key above them, and blank lines and comments carry nothing.
fn parse_frontmatter(text: &str) -> Result<Note, String> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or_else(|| "the note does not start with a --- line".to_string())?;

    let mut seen: Vec<&str> = Vec::new();
    let mut kind = None;
    let mut domain = None;
    let mut closed = false;
    for (index, raw) in rest.split('\n').enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line == "---" {
            closed = true;
            break;
        }
        match classify(line) {
            Line::Ignored => continue,
            Line::Other => {
                // Line 1 is the opening ---, so the first line here is line 2.
                return Err(format!(
                    "line {} of the frontmatter is not `key: value`, an indented line, a list \
                     item, or a comment",
                    index + 2
                ));
            }
            Line::Key(key, value) => {
                if seen.contains(&key) {
                    return Err(format!("the frontmatter repeats the key {key}"));
                }
                seen.push(key);
                match key {
                    "kind" => kind = Some(read_value(key, value)?),
                    "domain" => domain = Some(read_value(key, value)?),
                    // Nothing reads the author for attribution, and the
                    // server applies the same value rule to it.
                    "author" => {
                        read_value(key, value)?;
                    }
                    _ => {}
                }
            }
        }
    }
    if !closed {
        return Err("the frontmatter has no closing --- line".to_string());
    }

    let kind = kind.ok_or_else(|| "the frontmatter has no kind".to_string())?;
    if !KINDS.contains(&kind.as_str()) {
        return Err(format!(
            "the kind {kind:?} is not one of {}",
            KINDS.join(", ")
        ));
    }
    let domain = domain.ok_or_else(|| "the frontmatter has no domain".to_string())?;
    if !is_slug(&domain, MAX_NAME_LEN) {
        return Err(format!(
            "the domain {domain:?} is not 1 to {MAX_NAME_LEN} lowercase letters, digits, and \
             '-', starting with a letter or a digit"
        ));
    }
    Ok(Note { kind, domain })
}

/// What one frontmatter line is.
enum Line<'a> {
    /// A `key: value` line at column 0, with the text after the colon.
    Key(&'a str, &'a str),
    /// A blank line, a comment, an indented line, or a list item.
    Ignored,
    /// Anything else.
    Other,
}

fn classify(line: &str) -> Line<'_> {
    if line.trim().is_empty()
        || line.starts_with([' ', '\t', '#'])
        || line == "-"
        || line.starts_with("- ")
        || line.starts_with("-\t")
    {
        return Line::Ignored;
    }
    let Some((key, rest)) = line.split_once(':') else {
        return Line::Other;
    };
    let key_ok = key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    // YAML needs a space after the colon of a mapping key.
    if key_ok && (rest.is_empty() || rest.starts_with([' ', '\t'])) {
        Line::Key(key, rest)
    } else {
        Line::Other
    }
}

/// The value after a key's colon: trimmed, up to its closing quote when it is
/// quoted, and up to ` #` when it is not.
fn read_value(key: &str, rest: &str) -> Result<String, String> {
    let trimmed = rest.trim_start();
    if let Some(quote) = trimmed.chars().next().filter(|c| matches!(c, '"' | '\'')) {
        let inner = &trimmed[1..];
        return match inner.find(quote) {
            Some(end) => Ok(inner[..end].to_string()),
            None => Err(format!(
                "the value of {key} opens a quote and does not close it"
            )),
        };
    }
    // `rest` still starts with the space after the colon, so a value that is
    // only a comment reads as empty.
    let bytes = rest.as_bytes();
    let end = (1..bytes.len())
        .find(|&i| bytes[i] == b'#' && matches!(bytes[i - 1], b' ' | b'\t'))
        .unwrap_or(bytes.len());
    Ok(rest[..end].trim().to_string())
}

/// True when `value` is 1 to `max` lowercase ASCII letters, digits, and `-`,
/// starting with a letter or a digit.
fn is_slug(value: &str, max: usize) -> bool {
    let mut chars = value.chars();
    value.len() <= max
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ----------------------------------------------------------- the folders ---

/// Creates `outbox/` with mode `0700` when it is missing.
///
/// `sync` calls this too, because a Mac that linked before the outbox existed
/// never runs `link` again.
pub fn ensure_dir(config: &Config) -> Result<()> {
    let dir = config.outbox_dir();
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&dir)
        .with_context(|| format!("cannot create the directory {}", dir.display()))
}

/// How many notes wait, and how many were rejected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub waiting: usize,
    pub rejected: usize,
}

/// Counts the notes from the files alone, with no request.
pub fn counts(config: &Config) -> Counts {
    counts_in(&config.outbox_dir(), &config.rejected_dir())
}

/// Counts the notes in an outbox and in its rejected directory. `uninstall`
/// calls this, because it holds a layout and no settings.
pub fn counts_in(outbox: &Path, rejected: &Path) -> Counts {
    Counts {
        waiting: notes_in(outbox).len(),
        rejected: notes_in(rejected).len(),
    }
}

/// The `.md` entries directly in `dir` that are not directories and whose
/// names do not start with a dot, sorted by name. A missing directory holds
/// none.
fn notes_in(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if name.starts_with('.') || !name.ends_with(".md") {
                return None;
            }
            // symlink_metadata, so a link to a directory is still a note
            // candidate, and push refuses it.
            let meta = fs::symlink_metadata(entry.path()).ok()?;
            if meta.is_dir() {
                return None;
            }
            Some((name, entry.path()))
        })
        .collect();
    found.sort();
    found
}

/// The headers in which `content/latest` reports this client: its platform,
/// its installed content, and the notes waiting on it.
pub fn report_headers(config: &Config) -> Vec<(&'static str, String)> {
    let installed = crate::state::read(&config.state_file())
        .filter(|_| config.content_dir().is_dir())
        .map(|state| state.hash)
        .unwrap_or_else(|| "none".to_string());
    vec![
        (HEADER_PLATFORM, crate::selfupdate::platform_key()),
        (HEADER_CONTENT, installed),
        (HEADER_OUTBOX, counts(config).waiting.to_string()),
    ]
}

// ---------------------------------------------------------- the operator ---

/// What `whoami` changed in the `operator` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operator {
    /// The file names this operator.
    Named(String),
    /// The server names no operator, so the file is gone.
    Unnamed,
}

/// Asks the server which operator this client belongs to, and writes the
/// answer to the `operator` file: the name and one line feed, nothing else.
///
/// An answer of `null` deletes the file. Any failure leaves the file as it is,
/// so a server older than `whoami` changes nothing.
pub fn update_operator(config: &Config) -> Result<Operator> {
    let path = config.operator_file();
    match remote::whoami(config)? {
        Some(name) => {
            if !is_slug(&name, MAX_NAME_LEN) {
                anyhow::bail!(
                    "the server names the operator {:?}, which is not 1 to {MAX_NAME_LEN} \
                     lowercase letters, digits, and '-'",
                    remote::printable(&name)
                );
            }
            let text = format!("{name}\n");
            if fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
                write_atomic(&path, text.as_bytes())?;
            }
            Ok(Operator::Named(name))
        }
        None => {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot remove {}", path.display()));
                }
            }
            Ok(Operator::Unnamed)
        }
    }
}

/// The operator name in the `operator` file, when it holds a usable one.
///
/// The file is a convenience for Claude, never a security control, so a file
/// that someone edited into another shape reads as no name.
pub fn read_operator(config: &Config) -> Option<String> {
    let text = fs::read_to_string(config.operator_file()).ok()?;
    let name = text.strip_suffix('\n').unwrap_or(&text);
    is_slug(name, MAX_NAME_LEN).then(|| name.to_string())
}

// -------------------------------------------------------------- the push ---

/// What `push.json` records: the last push that sent a note.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushState {
    pub last_push_at_unix: u64,
    pub last_push_notes: u64,
}

/// Reads `push.json`. A missing or damaged file reads as no push.
pub fn read_push_state(config: &Config) -> Option<PushState> {
    let text = fs::read_to_string(config.push_state_file()).ok()?;
    serde_json::from_str(&text).ok()
}

/// What one push did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pushed {
    /// Where each note that the server holds now went, in the order sent.
    pub sent: Vec<PathBuf>,
    /// Each note that moved to `rejected/`, with the reason.
    pub rejected: Vec<(String, String)>,
    /// Notes left for the next run, because they changed in the last 60
    /// seconds.
    pub settling: usize,
    /// Why the run stopped with notes still waiting, when it did.
    pub stopped: Option<String>,
    /// True when another run held the push lock, so this one did nothing.
    pub busy: bool,
}

/// One note that passed every check here, ready to send.
struct Ready {
    name: String,
    path: PathBuf,
    modified: SystemTime,
    bytes: Vec<u8>,
    note: Note,
}

/// What the checks here decided about one file.
enum Checked {
    Ready(Ready),
    /// Changed in the last 60 seconds.
    Settling,
    /// Broke a rule, for this reason.
    Refused(String),
    /// Went away after the listing, as a file that Claude renamed does.
    Gone,
}

/// Sends the notes in the outbox, oldest first.
///
/// No request leaves this function when no note is ready: an empty outbox
/// asks the issuer for nothing. A failure of the server, or of the issuer,
/// keeps every note that was not sent, and stops the run with a reason in
/// [`Pushed::stopped`].
pub fn push(config: &Config, log: &dyn Fn(&str)) -> Result<Pushed> {
    let mut pushed = Pushed::default();
    ensure_dir(config)?;
    let Some(_lock) = lock::acquire(&config.outbox_lock_file(), Duration::ZERO)? else {
        pushed.busy = true;
        return Ok(pushed);
    };

    let now = SystemTime::now();
    let mut ready = Vec::new();
    for (name, path) in notes_in(&config.outbox_dir()) {
        match check(&name, &path, now) {
            Checked::Ready(note) => ready.push(note),
            Checked::Settling => pushed.settling += 1,
            Checked::Gone => {}
            Checked::Refused(reason) => {
                reject(config, &path, &name, &reason)?;
                log(&format!("Rejected {name}: {reason}"));
                pushed.rejected.push((name, reason));
            }
        }
    }
    if ready.is_empty() {
        return Ok(pushed);
    }

    let token = match auth::bearer(config, auth::SCOPE_OUTBOX_WRITE) {
        Ok(token) => token,
        Err(error) if auth::is_invalid_scope(&error) => {
            pushed.stopped = Some(format!(
                "the issuer does not grant this client the scope {}, so {} note(s) stay in {}",
                auth::SCOPE_OUTBOX_WRITE,
                ready.len(),
                config.outbox_dir().display()
            ));
            return Ok(pushed);
        }
        Err(error) => {
            pushed.stopped = Some(format!("cannot get a token to send the notes: {error:#}"));
            return Ok(pushed);
        }
    };

    ready.sort_by(|a, b| (a.modified, &a.name).cmp(&(b.modified, &b.name)));
    let total = ready.len();
    for (index, note) in ready.into_iter().enumerate() {
        let answer = match remote::post_note(config, &note.name, &note.bytes, token.as_deref()) {
            Ok(answer) => answer,
            Err(error) => {
                pushed.stopped = Some(format!(
                    "{error:#}; {} note(s) stay in the outbox",
                    total - index
                ));
                break;
            }
        };
        match answer.status {
            200 | 201 => {
                let month = received_month(&answer.body);
                let dest = move_unique(&note.path, &config.sent_dir().join(month), &note.name)?;
                log(&format!(
                    "Sent {} ({}, {}).",
                    note.name, note.note.kind, note.note.domain
                ));
                pushed.sent.push(dest);
            }
            400 | 413 => {
                let reason = format!(
                    "the server refused the note with HTTP {}: {}",
                    answer.status,
                    remote::message_from_body(&answer.body)
                        .unwrap_or_else(|| "no reason given".to_string())
                );
                reject(config, &note.path, &note.name, &reason)?;
                log(&format!("Rejected {}: {reason}", note.name));
                pushed.rejected.push((note.name, reason));
            }
            status => {
                pushed.stopped = Some(format!(
                    "{}; {} note(s) stay in the outbox",
                    refusal(status, &answer.body),
                    total - index
                ));
                break;
            }
        }
    }

    if !pushed.sent.is_empty() {
        write_push_state(config, pushed.sent.len())?;
    }
    Ok(pushed)
}

/// Applies the checks of this side to one outbox entry.
fn check(name: &str, path: &Path, now: SystemTime) -> Checked {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Checked::Gone,
        Err(error) => return Checked::Refused(format!("cannot read the file: {error}")),
    };
    if meta.file_type().is_symlink() {
        return Checked::Refused(
            "the entry is a symbolic link, and push sends regular files only".to_string(),
        );
    }
    if !meta.is_file() {
        return Checked::Refused("the entry is not a regular file".to_string());
    }
    let modified = meta.modified().unwrap_or(now);
    // A time in the future also waits: its age is not known.
    let settled = now
        .duration_since(modified)
        .is_ok_and(|age| age >= SETTLE_TIME);
    if !settled {
        return Checked::Settling;
    }
    if let Err(reason) = validate_name(name) {
        return Checked::Refused(reason);
    }
    let bytes = match read_capped(path, &meta) {
        Ok(Capped::Bytes(bytes)) => bytes,
        Ok(Capped::TooLarge) => {
            return Checked::Refused(format!(
                "the note is larger than the limit of {MAX_NOTE_BYTES} bytes"
            ));
        }
        Ok(Capped::Gone) => return Checked::Gone,
        Err(reason) => return Checked::Refused(reason),
    };
    match check_note(&bytes) {
        Ok(note) => Checked::Ready(Ready {
            name: name.to_string(),
            path: path.to_path_buf(),
            modified,
            bytes,
            note,
        }),
        Err(reason) => Checked::Refused(reason),
    }
}

/// What [`read_capped`] found.
enum Capped {
    Bytes(Vec<u8>),
    TooLarge,
    Gone,
}

/// Reads a note of at most [`MAX_NOTE_BYTES`] bytes.
///
/// The open follows no link that appeared after the check: the file handle
/// must be the same regular file that `before` describes.
fn read_capped(path: &Path, before: &fs::Metadata) -> Result<Capped, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Capped::Gone),
        Err(error) => return Err(format!("cannot open the file: {error}")),
    };
    let after = file
        .metadata()
        .map_err(|error| format!("cannot read the file: {error}"))?;
    if !after.is_file() || !same_file(before, &after) {
        return Err("the file changed while push read it".to_string());
    }
    let mut bytes = Vec::new();
    file.take(MAX_NOTE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read the file: {error}"))?;
    if bytes.len() as u64 > MAX_NOTE_BYTES {
        return Ok(Capped::TooLarge);
    }
    Ok(Capped::Bytes(bytes))
}

#[cfg(unix)]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.len() == b.len() && a.modified().ok() == b.modified().ok()
}

/// The directory name under `sent/`: the first 7 characters of the
/// `received_at` that the server returned, when they read `YYYY-MM`.
///
/// The value reaches a path, so anything else gives `unknown`.
fn received_month(body: &str) -> String {
    #[derive(Deserialize)]
    struct Received {
        received_at: String,
    }
    let month = serde_json::from_str::<Received>(body)
        .ok()
        .and_then(|parsed| parsed.received_at.get(..7).map(str::to_string));
    match month {
        Some(month)
            if month.len() == 7
                && month.as_bytes()[4] == b'-'
                && month
                    .bytes()
                    .enumerate()
                    .all(|(i, b)| i == 4 || b.is_ascii_digit()) =>
        {
            month
        }
        _ => "unknown".to_string(),
    }
}

/// The notice for a status that keeps the note.
fn refusal(status: u16, body: &str) -> String {
    let message = remote::message_from_body(body)
        .map(|message| format!(": {message}"))
        .unwrap_or_default();
    match status {
        401 => format!("the server refused the token with HTTP 401{message}"),
        403 => format!("the server refused the note with HTTP 403{message}"),
        404 => format!("the server answered HTTP 404, so it has no outbox route yet{message}"),
        429 => format!("the server limits the notes, HTTP 429{message}"),
        _ => format!("the server answered HTTP {status}{message}"),
    }
}

/// Moves a note to `rejected/`, and writes the reason beside it.
fn reject(config: &Config, path: &Path, name: &str, reason: &str) -> Result<()> {
    let dest = move_unique(path, &config.rejected_dir(), name)?;
    let file_name = dest
        .file_name()
        .context("a rejected note has no file name")?
        .to_string_lossy()
        .into_owned();
    let reason_file = dest.with_file_name(format!("{file_name}.reason.txt"));
    fs::write(&reason_file, format!("{}\n", remote::printable(reason)))
        .with_context(|| format!("cannot write {}", reason_file.display()))
}

/// Moves `from` into `dir` under `name`, or under `name` with `-2`, `-3`, and
/// so on before `.md` when that name is taken. Returns the new path.
///
/// A symbolic link moves as a link: the file it names stays where it is.
fn move_unique(from: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    fs::create_dir_all(dir)
        .with_context(|| format!("cannot create the directory {}", dir.display()))?;
    let stem = name.strip_suffix(".md").unwrap_or(name);
    let mut dest = dir.join(name);
    let mut number = 2;
    while fs::symlink_metadata(&dest).is_ok() {
        dest = dir.join(format!("{stem}-{number}.md"));
        number += 1;
    }
    fs::rename(from, &dest)
        .with_context(|| format!("cannot move {} to {}", from.display(), dest.display()))?;
    Ok(dest)
}

/// Writes `push.json` through a temporary file and a rename.
fn write_push_state(config: &Config, notes: usize) -> Result<()> {
    let state = PushState {
        last_push_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        last_push_notes: notes as u64,
    };
    let text = serde_json::to_string_pretty(&state).context("cannot serialize the push state")?;
    write_atomic(&config.push_state_file(), text.as_bytes())
}

/// The temporary file that [`write_atomic`] renames over `path`.
pub fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Writes a file through a temporary file and a rename, so that a crash never
/// leaves half a file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = temporary_path(path);
    fs::write(&temp, bytes).with_context(|| format!("cannot write {}", temp.display()))?;
    fs::rename(&temp, path)
        .with_context(|| format!("cannot rename {} to {}", temp.display(), path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Route, Server, temp_dir};

    fn note(frontmatter: &str) -> Result<Note, String> {
        check_note(format!("---\n{frontmatter}\n---\n\n# Body\n").as_bytes())
    }

    // The vectors of the plan. Synapsis keeps the same ones in its outbox.rs.

    #[test]
    fn the_name_vectors_hold() {
        validate_name("2026-09-29-lezioni-turchi-scatec.md").unwrap();
        for bad in [
            "2026-09-29-Lezioni.md",
            "../x.md",
            ".2026-09-29-a.md",
            "2026-09-29-a.txt",
        ] {
            assert!(validate_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_frontmatter_vectors_hold() {
        let n = note("kind: fact   # il tipo prevalente\ndomain: damac").unwrap();
        assert_eq!(n.kind, "fact");

        assert!(note("kind: <fact | decision>\ndomain: damac").is_err());

        let n = note("kind: fact\ndomain: \"solarecompleto\"").unwrap();
        assert_eq!(n.domain, "solarecompleto");

        let error = note("kind: fact\nkind: decision\ndomain: damac").unwrap_err();
        assert!(error.contains("repeats the key kind"), "got {error}");

        let error = check_note(b"---\nkind: fact\ndomain: damac\n\n# Body\n").unwrap_err();
        assert!(error.contains("closing"), "got {error}");

        let crlf = "---\r\nkind: fact\r\ndomain: damac\r\nauthor: gabriele\r\n---\r\n\r\nBody\r\n";
        let n = check_note(crlf.as_bytes()).unwrap();
        assert_eq!(n.kind, "fact");
        assert_eq!(n.domain, "damac");
    }

    #[test]
    fn a_name_keeps_to_its_pattern() {
        validate_name("2026-09-29-a.md").unwrap();
        validate_name(&format!("2026-09-29-{}.md", "a".repeat(MAX_SLUG_LEN))).unwrap();
        for bad in [
            "",
            ".md",
            "2026-09-29-.md",
            "2026-09-29--a.md",
            "2026-9-29-a.md",
            "2026-09-29-a/b.md",
            "2026-09-29-à.md",
            "2026-09-29-a.MD",
            &format!("2026-09-29-{}.md", "a".repeat(MAX_SLUG_LEN + 1)),
        ] {
            assert!(validate_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn kind_and_domain_are_required_and_keys_are_unique() {
        assert!(note("domain: damac").unwrap_err().contains("no kind"));
        assert!(note("kind: fact").unwrap_err().contains("no domain"));
        assert!(note("kind: fact\ndomain: <damac | turchi>").is_err());
        assert!(note("kind: fact\ndomain: damac\nauthor: a\nauthor: b").is_err());
        assert!(note("kind: fact\ndomain: damac\nreview_flags: []\nreview_flags: []").is_err());
        assert!(note("kind: fact\ndomain: damac\nauthor: \"open").is_err());
        let error = note("kind: fact\ndomain: damac\njust words").unwrap_err();
        assert!(error.contains("line 4"), "got {error}");
    }

    #[test]
    fn indented_lines_list_items_and_comments_are_ignored() {
        let n = note("# a comment\nkind: decision\ntags:\n  - one\n- two\nsummary: >\n  kind: fact\n\ndomain: 'turchi'")
            .unwrap();
        assert_eq!(n.kind, "decision");
        assert_eq!(n.domain, "turchi");
    }

    #[test]
    fn the_body_must_be_text_that_starts_with_the_frontmatter() {
        assert!(check_note(b"").is_err());
        assert!(check_note(b"\xEF\xBB\xBF---\nkind: fact\ndomain: d\n---\n").is_err());
        assert!(check_note(b"---\nkind: fact\ndomain: d\n---\n\xff").is_err());
        assert!(check_note(b"---\nkind: fact\ndomain: d\n---\na\0b").is_err());
        assert!(check_note(b"# Title\n---\nkind: fact\ndomain: d\n---\n").is_err());
        check_note(b"---\nkind: fact\ndomain: d\n---").unwrap();
    }

    // ------------------------------------------------------------ push ---

    const NOTE: &str = "---\nkind: fact\ndomain: damac\nauthor: gabriele\n---\n\n# Lezione\n";

    /// Writes a note that changed `age` ago.
    fn write_note(config: &Config, name: &str, body: &[u8], age: Duration) -> PathBuf {
        fs::create_dir_all(config.outbox_dir()).unwrap();
        let path = config.outbox_dir().join(name);
        fs::write(&path, body).unwrap();
        let file = File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
        path
    }

    fn old() -> Duration {
        Duration::from_secs(120)
    }

    /// A token route for each scope, and `routes` after them.
    fn server_with(write_token: Route, routes: Vec<Route>) -> Server {
        let mut all = vec![
            Route::token("sync", r#"{"access_token":"sync-token"}"#),
            write_token,
        ];
        all.extend(routes);
        Server::start(all)
    }

    fn write_token() -> Route {
        Route::token("outbox:write", r#"{"access_token":"write-token"}"#)
    }

    fn stored(received_at: &str) -> String {
        format!(
            r#"{{"id":"5b1c","sha256":"ab","operator":"gabriele","received_at":"{received_at}","duplicate":false}}"#
        )
    }

    fn config_for(server: &Server, tag: &str) -> (PathBuf, Config) {
        let dir = temp_dir(tag);
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());
        (dir, config)
    }

    fn quiet(_: &str) {}

    #[test]
    fn an_empty_outbox_makes_no_request() {
        let server = server_with(write_token(), Vec::new());
        let (dir, config) = config_for(&server, "outbox-empty");

        let pushed = push(&config, &quiet).unwrap();

        assert_eq!(pushed, Pushed::default());
        assert!(server.received().is_empty(), "push asked the server");
        assert!(config.outbox_dir().is_dir(), "push creates the outbox");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stored_note_moves_to_the_month_the_server_names() {
        let server = server_with(
            write_token(),
            vec![
                Route::post(
                    "/outbox/2026-09-30-a.md",
                    stored("2026-10-01T00:30:00+02:00"),
                )
                .status(201),
                Route::post(
                    "/outbox/2026-09-29-b.md",
                    stored("2026-09-29T23:10:00+02:00"),
                ),
            ],
        );
        let (dir, config) = config_for(&server, "outbox-sent");
        // b is older, so it goes first.
        write_note(&config, "2026-09-30-a.md", NOTE.as_bytes(), old());
        write_note(
            &config,
            "2026-09-29-b.md",
            NOTE.as_bytes(),
            Duration::from_secs(600),
        );

        let pushed = push(&config, &quiet).unwrap();

        assert_eq!(
            pushed.sent,
            vec![
                config.sent_dir().join("2026-09").join("2026-09-29-b.md"),
                config.sent_dir().join("2026-10").join("2026-09-30-a.md"),
            ]
        );
        assert!(pushed.sent.iter().all(|path| path.is_file()));
        assert_eq!(counts(&config), Counts::default());
        let posts: Vec<_> = server
            .received()
            .into_iter()
            .filter(|r| r.path.starts_with("/outbox/"))
            .collect();
        assert_eq!(posts[0].path, "/outbox/2026-09-29-b.md");
        assert_eq!(posts[0].header("authorization"), Some("Bearer write-token"));
        assert_eq!(posts[0].body, NOTE.as_bytes());
        let state = read_push_state(&config).unwrap();
        assert_eq!(state.last_push_notes, 2);
        assert!(!temporary_path(&config.push_state_file()).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_taken_in_sent_gets_a_number() {
        let server = server_with(
            write_token(),
            vec![Route::post(
                "/outbox/2026-09-30-a.md",
                stored("2026-09-30T10:00:00+02:00"),
            )],
        );
        let (dir, config) = config_for(&server, "outbox-numbered");
        let month = config.sent_dir().join("2026-09");
        fs::create_dir_all(&month).unwrap();
        fs::write(month.join("2026-09-30-a.md"), "earlier").unwrap();
        fs::write(month.join("2026-09-30-a-2.md"), "earlier too").unwrap();
        write_note(&config, "2026-09-30-a.md", NOTE.as_bytes(), old());

        let pushed = push(&config, &quiet).unwrap();

        assert_eq!(pushed.sent, vec![month.join("2026-09-30-a-3.md")]);
        assert_eq!(
            fs::read_to_string(month.join("2026-09-30-a.md")).unwrap(),
            "earlier"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_refused_note_moves_to_rejected_with_the_reason() {
        let server = server_with(
            write_token(),
            vec![
                Route::post(
                    "/outbox/2026-09-30-a.md",
                    r#"{"error":"the kind \"facts\" is not one of fact"}"#,
                )
                .status(400),
                Route::post(
                    "/outbox/2026-09-30-b.md",
                    r#"{"error":"the note is larger"}"#,
                )
                .status(413),
            ],
        );
        let (dir, config) = config_for(&server, "outbox-refused");
        write_note(&config, "2026-09-30-a.md", NOTE.as_bytes(), old());
        write_note(&config, "2026-09-30-b.md", NOTE.as_bytes(), old());

        let pushed = push(&config, &quiet).unwrap();

        assert!(pushed.sent.is_empty());
        assert_eq!(pushed.rejected.len(), 2);
        let reason =
            fs::read_to_string(config.rejected_dir().join("2026-09-30-a.md.reason.txt")).unwrap();
        assert!(reason.contains("HTTP 400"), "got {reason}");
        assert!(reason.contains("is not one of fact"), "got {reason}");
        assert!(config.rejected_dir().join("2026-09-30-b.md").is_file());
        assert_eq!(
            counts(&config),
            Counts {
                waiting: 0,
                rejected: 2
            }
        );
        assert!(read_push_state(&config).is_none(), "nothing was sent");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_note_that_breaks_a_rule_here_never_leaves() {
        let server = server_with(write_token(), Vec::new());
        let (dir, config) = config_for(&server, "outbox-local-rule");
        write_note(&config, "2026-09-30-Note.md", NOTE.as_bytes(), old());
        write_note(&config, "2026-09-30-bad.md", b"no frontmatter", old());
        let big = format!("{NOTE}{}", "x".repeat(MAX_NOTE_BYTES as usize));
        write_note(&config, "2026-09-30-big.md", big.as_bytes(), old());
        // Not a note at all: ignored.
        write_note(&config, "notes.txt", b"x", old());
        write_note(&config, ".2026-09-30-hidden.md", NOTE.as_bytes(), old());

        let pushed = push(&config, &quiet).unwrap();

        assert_eq!(pushed.rejected.len(), 3, "got {:?}", pushed.rejected);
        assert!(
            server.received().is_empty(),
            "a rejected note asks for no token"
        );
        let reason =
            fs::read_to_string(config.rejected_dir().join("2026-09-30-big.md.reason.txt")).unwrap();
        assert!(reason.contains("larger than the limit"), "got {reason}");
        assert!(config.outbox_dir().join("notes.txt").is_file());
        assert!(config.outbox_dir().join(".2026-09-30-hidden.md").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_refused_and_its_target_stays_unsent() {
        let server = server_with(write_token(), Vec::new());
        let (dir, config) = config_for(&server, "outbox-link");
        fs::create_dir_all(config.outbox_dir()).unwrap();
        let secret = dir.join("secret.md");
        fs::write(&secret, NOTE).unwrap();
        let link = config.outbox_dir().join("2026-09-30-link.md");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let pushed = push(&config, &quiet).unwrap();

        assert_eq!(pushed.rejected.len(), 1);
        assert!(pushed.rejected[0].1.contains("symbolic link"));
        assert!(server.received().is_empty(), "nothing was sent");
        let moved = config.rejected_dir().join("2026-09-30-link.md");
        assert!(
            fs::symlink_metadata(&moved)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&secret).unwrap(),
            NOTE,
            "the target stays"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_note_younger_than_a_minute_waits() {
        let server = server_with(write_token(), Vec::new());
        let (dir, config) = config_for(&server, "outbox-young");
        write_note(
            &config,
            "2026-09-30-a.md",
            NOTE.as_bytes(),
            Duration::from_secs(5),
        );

        let pushed = push(&config, &quiet).unwrap();

        assert_eq!(pushed.settling, 1);
        assert!(server.received().is_empty());
        assert!(config.outbox_dir().join("2026-09-30-a.md").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_status_that_keeps_the_note_stops_the_run() {
        for (status, needle) in [
            (401, "HTTP 401"),
            (403, "not registered"),
            (404, "no outbox route"),
            (429, "limits the notes"),
            (500, "HTTP 500"),
        ] {
            let server = server_with(
                write_token(),
                vec![
                    Route::post("/outbox/2026-09-29-a.md", r#"{"error":"not registered"}"#)
                        .status(status),
                ],
            );
            let (dir, config) = config_for(&server, "outbox-kept");
            write_note(
                &config,
                "2026-09-29-a.md",
                NOTE.as_bytes(),
                Duration::from_secs(600),
            );
            write_note(&config, "2026-09-30-b.md", NOTE.as_bytes(), old());

            let pushed = push(&config, &quiet).unwrap();

            let stopped = pushed.stopped.expect("the run stopped");
            assert!(stopped.contains(needle), "{status}: got {stopped}");
            assert!(
                stopped.contains("2 note(s) stay"),
                "{status}: got {stopped}"
            );
            assert_eq!(counts(&config).waiting, 2, "{status}: the notes stay");
            let posts = server
                .received()
                .iter()
                .filter(|r| r.path.starts_with("/outbox/"))
                .count();
            assert_eq!(posts, 1, "{status}: the run stops at the first");
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn an_unreachable_server_keeps_the_note() {
        let server = server_with(write_token(), Vec::new());
        let dir = temp_dir("outbox-unreachable");
        // The token comes from the live server, the note goes nowhere.
        let config = Config::for_test_with_credentials(
            &dir,
            &crate::testutil::closed_port_base(),
            &server.base(),
        );
        write_note(&config, "2026-09-30-a.md", NOTE.as_bytes(), old());

        let pushed = push(&config, &quiet).unwrap();

        assert!(pushed.stopped.is_some());
        assert_eq!(counts(&config).waiting, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_issuer_without_the_write_scope_keeps_the_notes() {
        let server = server_with(
            Route::token("outbox:write", r#"{"error":"invalid_scope"}"#).status(400),
            Vec::new(),
        );
        let (dir, config) = config_for(&server, "outbox-scope");
        write_note(&config, "2026-09-30-a.md", NOTE.as_bytes(), old());

        let pushed = push(&config, &quiet).unwrap();

        let stopped = pushed.stopped.unwrap();
        assert!(stopped.contains("does not grant"), "got {stopped}");
        assert!(stopped.contains("outbox:write"), "got {stopped}");
        assert_eq!(counts(&config).waiting, 1);
        assert_eq!(
            server.received()[0].form("scope").as_deref(),
            Some("outbox:write")
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_held_lock_skips_the_run() {
        let server = server_with(write_token(), Vec::new());
        let (dir, config) = config_for(&server, "outbox-lock");
        write_note(&config, "2026-09-30-a.md", NOTE.as_bytes(), old());
        let held = lock::acquire(&config.outbox_lock_file(), Duration::ZERO)
            .unwrap()
            .unwrap();

        let pushed = push(&config, &quiet).unwrap();

        assert!(pushed.busy);
        assert!(server.received().is_empty());
        assert_eq!(counts(&config).waiting, 1);
        drop(held);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_note_that_went_away_after_the_listing_is_skipped() {
        let dir = temp_dir("outbox-gone");
        let gone = dir.join("2026-09-30-a.md");
        assert!(matches!(
            check("2026-09-30-a.md", &gone, SystemTime::now()),
            Checked::Gone
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_month_the_server_garbles_reads_unknown() {
        assert_eq!(
            received_month(&stored("2026-09-30T10:00:00+02:00")),
            "2026-09"
        );
        assert_eq!(received_month(&stored("../../etc")), "unknown");
        assert_eq!(received_month(&stored("2026/09")), "unknown");
        assert_eq!(received_month("not json"), "unknown");
    }

    // -------------------------------------------------------- operator ---

    #[test]
    fn whoami_writes_the_operator_file_and_null_deletes_it() {
        let server = Server::start(vec![
            Route::token("sync", r#"{"access_token":"s"}"#),
            Route::get(
                "/whoami",
                r#"{"client_id":"c","operator":"gabriele","display_name":"G"}"#,
            )
            .then(
                200,
                r#"{"client_id":"c","operator":null,"display_name":null}"#,
            ),
        ]);
        let (dir, config) = config_for(&server, "outbox-whoami");

        assert_eq!(
            update_operator(&config).unwrap(),
            Operator::Named("gabriele".into())
        );
        assert_eq!(
            fs::read_to_string(config.operator_file()).unwrap(),
            "gabriele\n"
        );
        assert_eq!(read_operator(&config).as_deref(), Some("gabriele"));

        assert_eq!(update_operator(&config).unwrap(), Operator::Unnamed);
        assert!(!config.operator_file().exists());
        assert_eq!(read_operator(&config), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_whoami_that_fails_leaves_the_operator_file() {
        for route in [
            Route::get("/nothing", "x"),
            Route::get("/whoami", r#"{"operator":"Not A Slug"}"#),
            Route::get("/whoami", "not json"),
        ] {
            let server =
                Server::start(vec![Route::token("sync", r#"{"access_token":"s"}"#), route]);
            let (dir, config) = config_for(&server, "outbox-whoami-kept");
            fs::write(config.operator_file(), "gabriele\n").unwrap();

            assert!(update_operator(&config).is_err());
            assert_eq!(
                fs::read_to_string(config.operator_file()).unwrap(),
                "gabriele\n"
            );
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn an_edited_operator_file_reads_as_no_name() {
        let dir = temp_dir("outbox-operator-edited");
        let config = Config::for_test(&dir, "http://127.0.0.1:9");
        fs::write(config.operator_file(), "gabriele\nX-Other: 1\n").unwrap();
        assert_eq!(read_operator(&config), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_headers_report_the_platform_the_content_and_the_notes() {
        let dir = temp_dir("outbox-headers");
        let config = Config::for_test(&dir, "http://127.0.0.1:9");
        let headers = report_headers(&config);
        assert_eq!(
            headers[0],
            (HEADER_PLATFORM, crate::selfupdate::platform_key())
        );
        assert_eq!(headers[1], (HEADER_CONTENT, "none".to_string()));
        assert_eq!(headers[2], (HEADER_OUTBOX, "0".to_string()));

        crate::state::write(&config.state_file(), &crate::state::State::new("a377aa94")).unwrap();
        fs::create_dir_all(config.content_dir()).unwrap();
        write_note(&config, "2026-09-30-a.md", NOTE.as_bytes(), old());
        let headers = report_headers(&config);
        assert_eq!(headers[1], (HEADER_CONTENT, "a377aa94".to_string()));
        assert_eq!(headers[2], (HEADER_OUTBOX, "1".to_string()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_outbox_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("outbox-mode");
        let config = Config::for_test(&dir, "http://127.0.0.1:9");
        ensure_dir(&config).unwrap();
        let mode = fs::metadata(config.outbox_dir())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        fs::remove_dir_all(&dir).unwrap();
    }
}
