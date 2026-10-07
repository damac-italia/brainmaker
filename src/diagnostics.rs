// SPDX-License-Identifier: GPL-3.0-or-later

//! The diagnostic report: what this machine tells the server about itself, so
//! that the admin can see why a client does not do what it should.
//!
//! # Why this exists
//!
//! A client that syncs and sends no note looks healthy from the server. The
//! causes are all local: no note was written, every note broke a rule, the
//! program is too old, or `link` never connected Claude. Without this module,
//! the admin had to ask the operator to run `status` and `push`, and to send
//! the output.
//!
//! # What a report holds
//!
//! Two things:
//!
//! - The state: the version, the platform, the installed content, which
//!   pieces of the bridge are in `~/.claude`, and how many notes wait, were
//!   rejected, and were sent.
//! - The new lines of the run log. A run of `sync`, `push`, `self-update`,
//!   `link`, or `unlink` writes one line for each thing that it did.
//!
//! Every value is a word from a fixed list, a bounded number, a content hash,
//! or a version of digits and dots. No field holds free text. A report
//! therefore cannot hold a
//! credential, a token, a URL, a path, the name or the text of a note, or an
//! error message, whatever happens on the machine. A failure reaches the log
//! as a [`Code`] and a [`Cause`], and a refused note as a [`Rule`].
//!
//! # A trust boundary
//!
//! The run log is a file, and a file can be replaced. So the report never
//! sends the bytes of the log. It parses each line into an [`Event`], checks
//! every value against the rule that the server applies, and writes the events
//! out again. A line that does not pass is not sent, and a symbolic link in
//! place of the log is not read.
//!
//! # When it goes
//!
//! `sync` sends the report after the outbox steps, with the `sync` token that
//! the run already holds, and at most once in [`REPORT_INTERVAL`]. A `link` or
//! an `unlink` that worked cuts the wait to [`WAIT_AFTER_LINK`], so a `sync`
//! soon after it reports what it changed. The client decides all of it: the
//! server never asks for a report,
//! and nothing in its answer changes what this program does. A report that
//! fails prints nothing and changes no exit code, and a run that received no
//! `sync` token makes no request at all.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::cause::{self, Cause, Failure};
use crate::config::{Config, MAX_REPORT_BYTES};
use crate::link::{self, Presence};
use crate::lock;
use crate::outbox::{self, Rule};
use crate::remote;
use crate::schedule::Agents;
use crate::selfupdate;
use crate::state;
use crate::sync;

/// The version of the shape of a report. The server refuses any other.
pub const SCHEMA: u32 = 1;

/// Shortest time between two reports of one machine. A report that failed
/// counts too, so a server that does not answer is asked no more often.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// The wait before the next report once a `link` or an `unlink` worked.
///
/// It is not zero. On macOS, `link` loads the hourly agent, which runs `sync`
/// at once, a few seconds after the installer's own `sync` sent a report. The
/// server refuses a second report of one client inside a minute, and a
/// refused report waits for the whole interval.
pub const WAIT_AFTER_LINK: Duration = Duration::from_secs(90);

/// Most lines of the run log that one report carries. The older lines go
/// first, and the rest goes with the next report.
pub const MAX_EVENTS_PER_REPORT: usize = 200;

/// Size at which the run log is cut.
const MAX_LOG_BYTES: u64 = 256 * 1024;

/// Size of the newest part of the run log that a cut keeps.
const KEPT_LOG_BYTES: usize = 128 * 1024;

/// Longest line of the run log that is read as an event.
const MAX_LINE_BYTES: usize = 512;

/// Largest count that a report carries. A larger count is sent as this value.
pub const MAX_COUNT: u64 = 100_000;

/// The first second of the year 2000, and the last second of the year 2099,
/// in seconds since the Unix epoch. A time outside them is a clock that is
/// wrong, and it is not sent.
const MIN_TIME: u64 = 946_684_800;
const MAX_TIME: u64 = 4_102_444_799;

// ------------------------------------------------------------- the words ---

/// The command that wrote a line of the run log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    #[serde(rename = "sync")]
    Sync,
    #[serde(rename = "push")]
    Push,
    #[serde(rename = "self-update")]
    SelfUpdate,
    #[serde(rename = "link")]
    Link,
    #[serde(rename = "unlink")]
    Unlink,
}

/// What a run did, as one word from a fixed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Code {
    /// The installed content is the release that the server offers.
    #[serde(rename = "content.up_to_date")]
    ContentUpToDate,
    /// The run installed a release.
    #[serde(rename = "content.updated")]
    ContentUpdated,
    /// The release could not be read, and the installed content stays.
    #[serde(rename = "content.unreachable")]
    ContentUnreachable,
    /// Another run was installing, and the installed content stays.
    #[serde(rename = "content.busy")]
    ContentBusy,
    /// The content step failed.
    #[serde(rename = "content.failed")]
    ContentFailed,
    /// The server did not answer which operator this client belongs to.
    #[serde(rename = "operator.failed")]
    OperatorFailed,
    /// The server holds `count` more notes.
    #[serde(rename = "push.sent")]
    PushSent,
    /// `count` notes broke `rule`, and moved to `rejected/`.
    #[serde(rename = "push.rejected")]
    PushRejected,
    /// `count` notes changed in the last 60 seconds, and wait for the next run.
    #[serde(rename = "push.settling")]
    PushSettling,
    /// The push stopped, and the notes that were not sent stay.
    #[serde(rename = "push.stopped")]
    PushStopped,
    /// Another run was sending the notes.
    #[serde(rename = "push.busy")]
    PushBusy,
    /// The push failed on this machine, before or after a request.
    #[serde(rename = "push.failed")]
    PushFailed,
    /// This program is the newest published build.
    #[serde(rename = "update.current")]
    UpdateCurrent,
    /// A newer build is published, and `--check` installed nothing.
    #[serde(rename = "update.available")]
    UpdateAvailable,
    /// The run installed the build `version`.
    #[serde(rename = "update.installed")]
    UpdateInstalled,
    /// `version` is published, with no build for this platform.
    #[serde(rename = "update.no_build")]
    UpdateNoBuild,
    /// `self-update` failed.
    #[serde(rename = "update.failed")]
    UpdateFailed,
    /// `link` ran to its end.
    #[serde(rename = "link.done")]
    LinkDone,
    /// `link` failed.
    #[serde(rename = "link.failed")]
    LinkFailed,
    /// `unlink` ran to its end.
    #[serde(rename = "unlink.done")]
    UnlinkDone,
    /// `unlink` failed.
    #[serde(rename = "unlink.failed")]
    UnlinkFailed,
}

/// Whether the hourly agent of this machine has its definition file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    Present,
    Absent,
    /// This system has no agent.
    None,
}

/// The word that `value` is written as, such as `content.updated`.
pub fn word<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(word)) => word,
        _ => String::new(),
    }
}

// ------------------------------------------------------------- the rules ---

/// True for 16 lowercase hexadecimal characters: the id of an event.
fn is_event_id(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(is_lower_hex)
}

/// True for 8 lowercase hexadecimal characters: the name of a content
/// release, as the server gives it.
fn is_content_hash(value: &str) -> bool {
    value.len() == 8 && value.bytes().all(is_lower_hex)
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

/// The platform keys of the published builds. A report names no other key: a
/// build for another system reports no platform, and the header of its sync
/// still names the key.
const PLATFORMS: [&str; 5] = [
    "darwin-arm64",
    "darwin-x86_64",
    "linux-arm64",
    "linux-x86_64",
    "windows-x86_64",
];

fn is_platform(value: &str) -> bool {
    PLATFORMS.contains(&value)
}

/// True for a version that a report may carry: two to four groups of one to
/// five digits, joined by dots, such as `0.1.9`. After them it may hold `-`
/// or `+` and one to 16 letters, digits, dots, and hyphens, such as
/// `0.2.0-rc1`.
///
/// [`crate::version::validate`] takes any 64 letters and digits, which a
/// token would pass. This rule takes only what reads as a version.
fn is_version(value: &str) -> bool {
    let (core, suffix) = match value.find(['-', '+']) {
        Some(at) => (&value[..at], Some(&value[at + 1..])),
        None => (value, None),
    };
    let groups: Vec<&str> = core.split('.').collect();
    (2..=4).contains(&groups.len())
        && groups.iter().all(|group| {
            (1..=5).contains(&group.len()) && group.bytes().all(|b| b.is_ascii_digit())
        })
        && suffix.is_none_or(|suffix| {
            (1..=16).contains(&suffix.len())
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        })
}

/// True for a time that a report may carry.
fn is_time(at_unix: u64) -> bool {
    (MIN_TIME..=MAX_TIME).contains(&at_unix)
}

/// A count as a report carries it: never above [`MAX_COUNT`].
fn bounded(count: usize) -> u64 {
    (count as u64).min(MAX_COUNT)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ------------------------------------------------------------ the events ---

/// One line of the run log: one thing that one run did.
///
/// The same shape is on the disk and in the report. A field that an event
/// does not use is left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// 16 hexadecimal characters, drawn at random. The server stores an event
    /// once, however often a report repeats it.
    pub id: String,
    /// When the line was written, by the clock of this machine.
    pub at_unix: u64,
    pub command: Command,
    pub code: Code,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    /// The HTTP status of the answer, when the cause was an answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// A content hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// A version of this program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl Event {
    fn new(command: Command, code: Code) -> Self {
        Self {
            id: new_id(),
            at_unix: now_unix(),
            command,
            code,
            cause: None,
            status: None,
            rule: None,
            count: None,
            hash: None,
            version: None,
        }
    }

    /// True when every value passes its rule. The server applies the same
    /// rules, and stores no event that breaks one.
    pub fn is_valid(&self) -> bool {
        is_event_id(&self.id)
            && is_time(self.at_unix)
            && self
                .status
                .is_none_or(|status| (100..=599).contains(&status))
            && self.count.is_none_or(|count| count <= MAX_COUNT)
            && self.hash.as_deref().is_none_or(is_content_hash)
            && self.version.as_deref().is_none_or(is_version)
    }

    /// Sets the hash, when it is one that a report may carry.
    fn hash(&mut self, hash: &str) -> &mut Self {
        self.hash = Some(hash.to_string()).filter(|hash| is_content_hash(hash));
        self
    }

    /// Sets the version, when it is one that a report may carry.
    fn version(&mut self, version: &str) -> &mut Self {
        self.version = Some(version.to_string()).filter(|version| is_version(version));
        self
    }

    fn count(&mut self, count: usize) -> &mut Self {
        self.count = Some(bounded(count));
        self
    }

    fn rule(&mut self, rule: Rule) -> &mut Self {
        self.rule = Some(rule);
        self
    }

    fn failure(&mut self, failure: Failure) -> &mut Self {
        self.cause = Some(failure.cause);
        self.status = failure.status.filter(|status| (100..=599).contains(status));
        self
    }
}

/// An id for one event. With no random source, the time and the process make
/// the id.
fn new_id() -> String {
    use std::fmt::Write as _;

    let mut bytes = [0u8; 8];
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        bytes = (nanos ^ (u64::from(std::process::id()) << 40)).to_be_bytes();
    }
    let mut id = String::with_capacity(16);
    for byte in bytes {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// The lines that one run of one command adds to the run log.
///
/// `main.rs` gives each step's result to this value, and saves it once the
/// steps ran. A step that worked and changed nothing leaves no line, except
/// the content step, so that every `sync` leaves one.
#[derive(Debug)]
pub struct Run {
    command: Command,
    events: Vec<Event>,
}

impl Run {
    pub fn new(command: Command) -> Self {
        Self {
            command,
            events: Vec::new(),
        }
    }

    fn add(&mut self, code: Code) -> &mut Event {
        self.events.push(Event::new(self.command, code));
        let last = self.events.len() - 1;
        &mut self.events[last]
    }

    /// True when the run has no line yet.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The content step of `sync`.
    pub fn content(&mut self, result: &Result<sync::Outcome>) {
        match result {
            Ok(sync::Outcome::UpToDate { hash }) => self.add(Code::ContentUpToDate).hash(hash),
            Ok(sync::Outcome::Updated { hash, .. }) => self.add(Code::ContentUpdated).hash(hash),
            Ok(sync::Outcome::Unreachable { hash, failure, .. }) => self
                .add(Code::ContentUnreachable)
                .hash(hash)
                .failure(*failure),
            Ok(sync::Outcome::Busy { hash }) => self.add(Code::ContentBusy).hash(hash),
            // The run log goes with the removal, so the order leaves no line.
            // The removal report says what happened.
            Ok(sync::Outcome::Removal(_)) => return,
            Err(error) => self.add(Code::ContentFailed).failure(cause::of(error)),
        };
    }

    /// The `whoami` step. Only a failure leaves a line.
    pub fn operator<T>(&mut self, result: &Result<T>) {
        if let Err(error) = result {
            self.add(Code::OperatorFailed).failure(cause::of(error));
        }
    }

    /// The push step: one line for the notes sent, one for each rule that a
    /// note broke, one for the notes that wait, and one for a stop.
    ///
    /// The reason text of a rejected note can quote the note, so the line
    /// holds the rule as a word and the count, and nothing else.
    pub fn pushed(&mut self, result: &Result<outbox::Pushed>) {
        let pushed = match result {
            Ok(pushed) => pushed,
            Err(error) => {
                self.add(Code::PushFailed).failure(cause::of(error));
                return;
            }
        };
        if pushed.busy {
            self.add(Code::PushBusy);
            return;
        }
        if !pushed.sent.is_empty() {
            self.add(Code::PushSent).count(pushed.sent.len());
        }
        let mut by_rule: BTreeMap<Rule, usize> = BTreeMap::new();
        for rejected in &pushed.rejected {
            *by_rule.entry(rejected.rule).or_default() += 1;
        }
        for (rule, count) in by_rule {
            self.add(Code::PushRejected).rule(rule).count(count);
        }
        if pushed.settling > 0 {
            self.add(Code::PushSettling).count(pushed.settling);
        }
        if let Some(failure) = pushed.failure {
            self.add(Code::PushStopped).failure(failure);
        }
    }

    /// A step that worked, and that has nothing more to say.
    pub fn did(&mut self, code: Code) {
        self.add(code);
    }

    /// A step of `self-update`, with the version that it names.
    pub fn did_for(&mut self, code: Code, version: &str) {
        self.add(code).version(version);
    }

    /// A step that failed, with the cause of `error` as a word.
    pub fn failed(&mut self, code: Code, error: &anyhow::Error) {
        self.add(code).failure(cause::of(error));
    }

    /// Appends the lines of this run to the run log.
    ///
    /// A failure to write changes nothing else: the log is a record for the
    /// admin, and no step of any command reads it. A root that does not exist
    /// gets no log, because nothing is installed there.
    pub fn save(self, config: &Config) {
        if self.events.is_empty() || !config.root().is_dir() {
            return;
        }
        let _ = append(&config.run_log_file(), &self.events);
    }
}

// ----------------------------------------------------------- the run log ---

/// Appends `events` to the log at `path`, and cuts the log when it passed its
/// size.
fn append(path: &Path, events: &[Event]) -> std::io::Result<()> {
    // A link in place of the log would send these lines into another file.
    if fs::symlink_metadata(path).is_ok_and(|meta| !meta.is_file()) {
        return Err(std::io::Error::other("the run log is not a regular file"));
    }
    let mut lines = String::new();
    for event in events.iter().filter(|event| event.is_valid()) {
        lines.push_str(&serde_json::to_string(event)?);
        lines.push('\n');
    }
    // One write for all the lines of a run, so that the lines of two runs at
    // the same time do not mix.
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(lines.as_bytes())?;
    drop(file);
    cut(path)
}

/// Cuts the log to its newest [`KEPT_LOG_BYTES`], at the start of a line,
/// once it is larger than [`MAX_LOG_BYTES`].
///
/// The newest part goes to a temporary file, which is renamed over the log. A
/// line that another run appends between the read and the rename is lost.
fn cut(path: &Path) -> std::io::Result<()> {
    let size = fs::metadata(path)?.len();
    if size <= MAX_LOG_BYTES {
        return Ok(());
    }
    // Only the part that stays is read, whatever the size of the file.
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(size - KEPT_LOG_BYTES as u64))?;
    let mut tail = Vec::new();
    file.take(KEPT_LOG_BYTES as u64).read_to_end(&mut tail)?;
    // The first line of the tail may be half a line, so the part that stays
    // starts after the next line break.
    let start = tail
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(tail.len(), |offset| offset + 1);
    let temp = outbox::temporary_path(path);
    fs::write(&temp, &tail[start..])?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

/// Reads the run log, and returns the lines that are events, oldest first.
///
/// The entry must be a regular file, and never a symbolic link: a link could
/// name any file that the user can read. Every line is parsed and checked, so
/// a line that is not an event of this program is not returned, whatever it
/// holds.
fn read_log(path: &Path) -> Vec<Event> {
    if !fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()) {
        return Vec::new();
    }
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    // The file that opened must be a regular file too.
    if !file.metadata().is_ok_and(|meta| meta.is_file()) {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    // A cut keeps the log under this size, so a larger file is not this
    // program's log, and its end is not read.
    if file
        .take(2 * MAX_LOG_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Vec::new();
    }
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty() && line.len() <= MAX_LINE_BYTES)
        .filter_map(|line| serde_json::from_slice::<Event>(line).ok())
        .filter(Event::is_valid)
        .collect()
}

/// The events after the one with the id `sent_through`, oldest first, and at
/// most [`MAX_EVENTS_PER_REPORT`] of them.
///
/// When no event has that id, because a cut removed it, every event counts as
/// new. The server stores an event once, so a repeat does no harm.
fn unsent(events: Vec<Event>, sent_through: Option<&str>) -> Vec<Event> {
    let start = sent_through
        .and_then(|id| events.iter().rposition(|event| event.id == id))
        .map_or(0, |index| index + 1);
    events
        .into_iter()
        .skip(start)
        .take(MAX_EVENTS_PER_REPORT)
        .collect()
}

// ------------------------------------------------------------- the state ---

/// The state of this machine, read from its files alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct State {
    /// When the state was read, by the clock of this machine.
    pub at_unix: Option<u64>,
    pub software: Software,
    pub content: Content,
    pub link: Link,
    pub outbox: Outbox,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Software {
    pub version: Option<String>,
    /// The platform key, when it is the key of a published build.
    pub platform: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Content {
    /// The hash in `state.json`.
    pub installed_hash: Option<String>,
    pub installed_at_unix: Option<u64>,
    /// True when `content/` is a directory.
    pub present: bool,
}

/// What `link` wrote outside the root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Link {
    /// The `SessionStart` hook in the Claude settings.
    pub hook: Presence,
    /// The block in `CLAUDE.md`.
    pub block: Presence,
    /// The skill links into the content directory.
    pub skills: u64,
    pub agent: Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outbox {
    pub waiting: u64,
    pub rejected: u64,
    pub sent: u64,
    /// Entries in the outbox that push does not read as a note: a file with
    /// a name that does not end in `.md`, or a directory.
    pub ignored: u64,
    /// The end of the last push that sent a note, from `push.json`.
    pub last_push_at_unix: Option<u64>,
    pub last_push_notes: Option<u64>,
    /// True when the `operator` file holds a name.
    pub operator: bool,
}

/// Where the state looks for what `link` wrote outside the root. A removal
/// order removes what `link` wrote from the same places.
#[derive(Debug, Clone, Default)]
pub struct Places {
    /// The Claude configuration directory, when the home directory is known.
    pub claude: Option<PathBuf>,
    /// The location of the hourly agent, on a system that has one.
    pub agents: Option<Agents>,
}

#[cfg(not(test))]
pub fn places() -> Places {
    Places {
        claude: link::claude_dir().ok(),
        agents: Agents::resolve(None, false).ok().flatten(),
    }
}

#[cfg(test)]
thread_local! {
    /// The places of a test. With none set, the state reads no directory
    /// outside the root, so no test reads the real home directory.
    static TEST_PLACES: std::cell::RefCell<Places> = std::cell::RefCell::new(Places::default());
}

#[cfg(test)]
pub fn places() -> Places {
    TEST_PLACES.with(|places| places.borrow().clone())
}

/// Makes the state of this thread read `claude` as the Claude configuration
/// directory, and `agents` as the location of the hourly agent. A removal
/// order that this thread obeys acts on the same two places.
#[cfg(test)]
pub fn look_in_this_test(claude: &Path, agents: Option<Agents>) {
    TEST_PLACES.with(|places| {
        *places.borrow_mut() = Places {
            claude: Some(claude.to_path_buf()),
            agents,
        };
    });
}

/// Reads the state. It makes no request and changes no file.
pub fn state(config: &Config) -> State {
    let places = places();
    let installed = state::read(&config.state_file());
    let bridge = places
        .claude
        .as_deref()
        .map(|claude| link::bridge(&config.content_dir(), claude));
    let agent = match &places.agents {
        Some(agents) if agents.plist().is_file() => Agent::Present,
        Some(_) => Agent::Absent,
        None => Agent::None,
    };
    let counts = outbox::counts(config);
    let pushed = outbox::read_push_state(config);
    State {
        at_unix: Some(now_unix()).filter(|at| is_time(*at)),
        software: Software {
            version: Some(selfupdate::CURRENT_VERSION.to_string())
                .filter(|version| is_version(version)),
            platform: Some(selfupdate::platform_key()).filter(|key| is_platform(key)),
        },
        content: Content {
            installed_hash: installed
                .as_ref()
                .map(|held| held.hash.clone())
                .filter(|hash| is_content_hash(hash)),
            installed_at_unix: installed
                .map(|held| held.updated_at_unix)
                .filter(|at| is_time(*at)),
            present: config.content_dir().is_dir(),
        },
        link: Link {
            hook: bridge.map_or(Presence::Unknown, |bridge| bridge.hook),
            block: bridge.map_or(Presence::Unknown, |bridge| bridge.block),
            skills: bridge.map_or(0, |bridge| bounded(bridge.skills)),
            agent,
        },
        outbox: Outbox {
            waiting: bounded(counts.waiting),
            rejected: bounded(counts.rejected),
            sent: bounded(outbox::sent_count(config, MAX_COUNT as usize)),
            ignored: bounded(outbox::ignored_count(config, MAX_COUNT as usize)),
            last_push_at_unix: pushed
                .map(|push| push.last_push_at_unix)
                .filter(|at| is_time(*at)),
            last_push_notes: pushed.map(|push| push.last_push_notes.min(MAX_COUNT)),
            operator: outbox::read_operator(config).is_some(),
        },
    }
}

// ------------------------------------------------------------ the report ---

/// The body of a report.
#[derive(Serialize)]
struct Report<'a> {
    schema: u32,
    state: &'a State,
    events: &'a [Event],
}

/// What `diagnostics.json` records: the last try, the last report that the
/// server stored, and the last line of the run log that it holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub last_attempt_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_report_at_unix: Option<u64>,
    /// The id of the newest event that the server holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_through: Option<String>,
}

/// Reads `diagnostics.json`. A missing or damaged file reads as no report.
pub fn read_record(config: &Config) -> Option<Record> {
    let text = fs::read_to_string(config.report_record_file()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes `diagnostics.json` through a temporary file and a rename.
fn write_record(config: &Config, record: &Record) -> std::io::Result<()> {
    let path = config.report_record_file();
    let temp = outbox::temporary_path(&path);
    fs::write(&temp, serde_json::to_string_pretty(record)?)?;
    fs::rename(&temp, &path)
}

/// Makes the next report due [`WAIT_AFTER_LINK`] from now, when the interval
/// would make it wait longer.
///
/// `link` and `unlink` call this when they worked. The installer runs `sync`
/// and then `link`, so the first report says that the hook is absent, and the
/// admin would read that for the whole interval.
pub fn make_due(config: &Config) {
    let Some(mut record) = read_record(config) else {
        return;
    };
    let wait = REPORT_INTERVAL.saturating_sub(WAIT_AFTER_LINK).as_secs();
    let sooner = now_unix().saturating_sub(wait);
    if record.last_attempt_at_unix > sooner {
        record.last_attempt_at_unix = sooner;
        let _ = write_record(config, &record);
    }
}

/// True when the interval has passed since the last try. A last try in the
/// future is a clock that went back, and it counts as passed.
fn due(record: &Record, now: u64) -> bool {
    now < record.last_attempt_at_unix
        || now - record.last_attempt_at_unix >= REPORT_INTERVAL.as_secs()
}

/// The report as text, and how many of `events` it holds. The count goes down
/// until the text fits in [`MAX_REPORT_BYTES`].
fn render(state: &State, events: &[Event]) -> Option<(String, usize)> {
    let mut count = events.len();
    loop {
        let report = Report {
            schema: SCHEMA,
            state,
            events: &events[..count],
        };
        let body = serde_json::to_string(&report).ok()?;
        if body.len() as u64 <= MAX_REPORT_BYTES {
            return Some((body, count));
        }
        if count == 0 {
            return None;
        }
        count /= 2;
    }
}

/// What one call of [`send`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// This run received no `sync` token, so the issuer did not answer it.
    NoToken,
    /// The last report, or the last try, is younger than the interval.
    NotDue,
    /// Another run sends a report now.
    Busy,
    /// The server stored the report, with this many lines of the run log.
    Stored(usize),
    /// The server did not store the report, or did not answer.
    Failed,
}

/// Sends the report when one is due.
///
/// It never fails and never prints: a report is no part of what `sync` is
/// for. A report that the server did not store leaves the lines of the run
/// log for the next one, and the next try waits for the interval too.
pub fn send(config: &Config) -> Sent {
    if !auth::received(config, auth::SCOPE_SYNC) {
        return Sent::NoToken;
    }
    let now = now_unix();
    if !due(&read_record(config).unwrap_or_default(), now) {
        return Sent::NotDue;
    }
    let Ok(Some(_lock)) = lock::acquire(&config.report_lock_file(), Duration::ZERO) else {
        return Sent::Busy;
    };
    // The run that held the lock a moment ago may have sent a report.
    let mut record = read_record(config).unwrap_or_default();
    if !due(&record, now) {
        return Sent::NotDue;
    }

    let events = unsent(
        read_log(&config.run_log_file()),
        record.sent_through.as_deref(),
    );
    let Some((body, count)) = render(&state(config), &events) else {
        return Sent::Failed;
    };
    record.last_attempt_at_unix = now;
    let stored = auth::bearer(config, auth::SCOPE_SYNC)
        .and_then(|token| remote::post_diagnostics(config, &body, token.as_deref()))
        .is_ok_and(|answer| (200..300).contains(&answer.status));
    if stored {
        record.last_report_at_unix = Some(now);
        if let Some(last) = count.checked_sub(1).and_then(|index| events.get(index)) {
            record.sent_through = Some(last.id.clone());
        }
    }
    let _ = write_record(config, &record);
    if stored {
        Sent::Stored(count)
    } else {
        Sent::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Route, Server, temp_dir};

    const NOTE: &str = "---\nkind: fact\ndomain: damac\n---\n\n# Lezione\n";

    fn quiet(_: &str) {}

    /// A server that gives a `sync` token, and `routes` after it.
    fn server_with(routes: Vec<Route>) -> Server {
        let mut all = vec![Route::token(
            "sync",
            r#"{"access_token":"sync-token-value"}"#,
        )];
        all.extend(routes);
        Server::start(all)
    }

    fn stored() -> Route {
        Route::post("/diagnostics", r#"{"stored":1,"dropped":0}"#)
    }

    /// A configuration whose run already received its `sync` token.
    fn config_for(server: &Server, tag: &str) -> (PathBuf, Config) {
        let dir = temp_dir(tag);
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());
        auth::bearer(&config, auth::SCOPE_SYNC).unwrap();
        (dir, config)
    }

    /// The bodies of the reports that the server received, as JSON.
    fn reports(server: &Server) -> Vec<serde_json::Value> {
        server
            .received()
            .iter()
            .filter(|request| request.path == "/diagnostics")
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    /// Makes the next report due, as if the interval had passed.
    fn pass_the_interval(config: &Config) {
        let mut record = read_record(config).unwrap();
        record.last_attempt_at_unix -= REPORT_INTERVAL.as_secs();
        write_record(config, &record).unwrap();
    }

    fn up_to_date(hash: &str) -> Result<sync::Outcome> {
        Ok(sync::Outcome::UpToDate {
            hash: hash.to_string(),
        })
    }

    /// Saves one run of `sync` whose content step found `hash` up to date.
    fn save_a_sync(config: &Config, hash: &str) {
        let mut run = Run::new(Command::Sync);
        run.content(&up_to_date(hash));
        run.save(config);
    }

    /// Writes a note that changed two minutes ago.
    fn settled_note(config: &Config, name: &str, body: &str) {
        fs::create_dir_all(config.outbox_dir()).unwrap();
        let path = config.outbox_dir().join(name);
        fs::write(&path, body).unwrap();
        let file = File::options().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
    }

    // ---------------------------------------------------------- events ---

    #[test]
    fn the_content_step_leaves_one_line_with_its_outcome() {
        let failure = Failure {
            cause: Cause::Http,
            status: Some(503),
        };
        for (result, code, hash, cause, status) in [
            (
                up_to_date("a377aa94"),
                "content.up_to_date",
                Some("a377aa94"),
                None,
                None,
            ),
            (
                Ok(sync::Outcome::Unreachable {
                    hash: "a377aa94".into(),
                    error: "cannot read https://api.example.test/x: HTTP 503".into(),
                    failure,
                }),
                "content.unreachable",
                Some("a377aa94"),
                Some("http"),
                Some(503),
            ),
            (
                Ok(sync::Outcome::Busy {
                    hash: "a377aa94".into(),
                }),
                "content.busy",
                Some("a377aa94"),
                None,
                None,
            ),
            (
                Err(cause::failed(Cause::Signature, None, "no key".into())),
                "content.failed",
                None,
                Some("signature"),
                None,
            ),
            (
                Err(anyhow::anyhow!("cannot write /Users/gabriele/.brainmaker")),
                "content.failed",
                None,
                Some("other"),
                None,
            ),
        ] {
            let mut run = Run::new(Command::Sync);
            run.content(&result);
            assert_eq!(run.events.len(), 1);
            let line = serde_json::to_value(&run.events[0]).unwrap();
            assert_eq!(line["command"], "sync");
            assert_eq!(line["code"], code);
            assert_eq!(line["hash"].as_str(), hash, "{code}");
            assert_eq!(line["cause"].as_str(), cause, "{code}");
            assert_eq!(line["status"].as_u64(), status, "{code}");
            assert!(run.events[0].is_valid());
        }
    }

    #[test]
    fn an_event_takes_no_value_that_breaks_its_rule() {
        let mut run = Run::new(Command::SelfUpdate);
        run.did_for(Code::UpdateInstalled, "0.1.9 and a secret");
        run.add(Code::ContentUpdated).hash("../../etc");
        run.add(Code::ContentUpdated).hash("A377AA94");
        run.add(Code::PushSent).count(usize::MAX);
        run.add(Code::PushStopped).failure(Failure {
            cause: Cause::Http,
            status: Some(99),
        });

        assert_eq!(run.events[0].version, None);
        assert_eq!(run.events[1].hash, None);
        assert_eq!(run.events[2].hash, None);
        assert_eq!(run.events[3].count, Some(MAX_COUNT));
        assert_eq!(run.events[4].status, None);
        assert!(run.events.iter().all(Event::is_valid));
        // A value that reached an event some other way fails the check.
        let mut bad = Event::new(Command::Sync, Code::ContentUpdated);
        bad.hash = Some("not-a-hash".into());
        assert!(!bad.is_valid());
        let mut bad = Event::new(Command::Sync, Code::ContentUpdated);
        bad.id = "an id of a wrong shape".into();
        assert!(!bad.is_valid());
        let mut bad = Event::new(Command::Sync, Code::ContentUpdated);
        bad.at_unix = 12;
        assert!(!bad.is_valid());
    }

    #[test]
    fn the_push_step_leaves_one_line_for_each_rule_and_never_a_reason() {
        let rejected = |name: &str, rule| outbox::Rejected {
            name: name.to_string(),
            reason: "the kind \"the text of the note\" is not one of fact".to_string(),
            rule,
        };
        let pushed = outbox::Pushed {
            sent: vec![PathBuf::from("a"), PathBuf::from("b")],
            rejected: vec![
                rejected("2026-09-30-a.md", Rule::Kind),
                rejected("2026-09-30-b.md", Rule::Frontmatter),
                rejected("2026-09-30-c.md", Rule::Kind),
            ],
            settling: 1,
            stopped: Some("the server answered HTTP 429: slow down".to_string()),
            failure: Some(Failure {
                cause: Cause::Http,
                status: Some(429),
            }),
            busy: false,
        };
        let mut run = Run::new(Command::Push);
        run.pushed(&Ok(pushed));

        let lines: Vec<serde_json::Value> = run
            .events
            .iter()
            .map(|event| serde_json::to_value(event).unwrap())
            .collect();
        let short: Vec<String> = lines
            .iter()
            .map(|line| {
                format!(
                    "{} {} {} {}",
                    line["code"].as_str().unwrap(),
                    line["rule"].as_str().unwrap_or("-"),
                    line["count"],
                    line["status"]
                )
            })
            .collect();
        assert_eq!(
            short,
            [
                "push.sent - 2 null",
                "push.rejected frontmatter 1 null",
                "push.rejected kind 2 null",
                "push.settling - 1 null",
                "push.stopped - null 429",
            ]
        );
        let text = serde_json::to_string(&lines).unwrap();
        assert!(!text.contains("the text of the note"), "{text}");
        assert!(!text.contains("2026-09-30-a"), "{text}");
        assert!(!text.contains("slow down"), "{text}");

        let mut run = Run::new(Command::Push);
        run.pushed(&Ok(outbox::Pushed::default()));
        assert!(run.is_empty(), "a push with nothing to do leaves no line");
        run.pushed(&Ok(outbox::Pushed {
            busy: true,
            ..outbox::Pushed::default()
        }));
        run.pushed(&Err(anyhow::anyhow!("cannot move the note")));
        run.operator(&Ok(()));
        run.operator::<()>(&Err(remote::unanswered("the request timed out")));
        let codes: Vec<String> = run.events.iter().map(|event| word(&event.code)).collect();
        assert_eq!(codes, ["push.busy", "push.failed", "operator.failed"]);
        assert_eq!(run.events[2].cause, Some(Cause::Unreachable));
    }

    // --------------------------------------------------------- the log ---

    #[test]
    fn a_run_appends_its_lines_and_a_root_that_is_absent_gets_none() {
        let dir = temp_dir("diagnostics-append");
        let config = Config::for_test(&dir, "http://127.0.0.1:9");
        save_a_sync(&config, "a377aa94");
        let mut run = Run::new(Command::SelfUpdate);
        run.did_for(Code::UpdateCurrent, "0.1.9");
        run.did(Code::LinkDone);
        run.save(&config);
        Run::new(Command::Push).save(&config);

        let text = fs::read_to_string(config.run_log_file()).unwrap();
        assert_eq!(text.lines().count(), 3);
        let events = read_log(&config.run_log_file());
        let codes: Vec<String> = events.iter().map(|event| word(&event.code)).collect();
        assert_eq!(codes, ["content.up_to_date", "update.current", "link.done"]);
        assert_eq!(events[1].version.as_deref(), Some("0.1.9"));
        assert_eq!(events[1].command, Command::SelfUpdate);

        let absent = Config::for_test(&dir.join("absent"), "http://127.0.0.1:9");
        save_a_sync(&absent, "a377aa94");
        assert!(!dir.join("absent").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_line_that_is_not_an_event_of_this_program_is_not_read() {
        let dir = temp_dir("diagnostics-foreign");
        let config = Config::for_test(&dir, "http://127.0.0.1:9");
        save_a_sync(&config, "a377aa94");
        let good = fs::read_to_string(config.run_log_file()).unwrap();
        let id = &read_log(&config.run_log_file())[0].id;
        let long = format!(
            r#"{{"id":"{id}","at_unix":1790683100,"command":"sync","code":"push.sent","version":"{}"}}"#,
            "9".repeat(600)
        );
        let foreign = [
            "SWETSI_CLIENT_SECRET=the-client-secret",
            "---\nkind: fact\ndomain: damac\n---\nThe text of a note.",
            r#"{"id":"0123456789abcdef","at_unix":1790683100,"command":"sync","code":"content.updated","note":"a free text"}"#,
            r#"{"id":"0123456789abcdef","at_unix":1790683100,"command":"sync","code":"a code that is a sentence"}"#,
            r#"{"id":"0123456789abcdef","at_unix":1790683100,"command":"rm -rf","code":"content.updated"}"#,
            r#"{"id":"0123456789abcdef","at_unix":1790683100,"command":"sync","code":"content.updated","hash":"the-secret"}"#,
            r#"{"id":"0123456789abcdef","at_unix":1790683100,"command":"sync","code":"push.stopped","cause":"the server said no"}"#,
            r#"{"id":"0123456789abcdef","at_unix":1790683100,"command":"sync","code":"push.rejected","rule":"the kind \"x\""}"#,
            r#"{"id":"0123456789abcdef","at_unix":1790683100,"command":"sync","code":"push.sent","count":100001}"#,
            r#"{"id":"0123456789abcdef","at_unix":1790683100,"command":"sync","code":"push.stopped","status":"403 Forbidden"}"#,
            r#"{"id":"token eyJhbGciOi","at_unix":1790683100,"command":"sync","code":"content.updated"}"#,
            r#"{"id":"0123456789abcdef","at_unix":"yesterday","command":"sync","code":"content.updated"}"#,
            long.as_str(),
            "not json at all",
        ];
        fs::write(
            config.run_log_file(),
            format!("{}\n{good}", foreign.join("\n")),
        )
        .unwrap();

        let events = read_log(&config.run_log_file());

        assert_eq!(events.len(), 1, "got {events:?}");
        assert_eq!(&events[0].id, id);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_place_of_the_run_log_is_neither_read_nor_written() {
        let dir = temp_dir("diagnostics-link");
        let config = Config::for_test(&dir, "http://127.0.0.1:9");
        save_a_sync(&config, "a377aa94");
        let elsewhere = dir.join("elsewhere.jsonl");
        fs::rename(config.run_log_file(), &elsewhere).unwrap();
        let before = fs::read_to_string(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, config.run_log_file()).unwrap();

        assert!(read_log(&config.run_log_file()).is_empty());
        save_a_sync(&config, "a377aa94");

        assert_eq!(fs::read_to_string(&elsewhere).unwrap(), before);
        assert_eq!(read_log(&elsewhere).len(), 1, "the file itself is a log");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_run_log_is_cut_to_its_newest_lines() {
        let dir = temp_dir("diagnostics-cut");
        let config = Config::for_test(&dir, "http://127.0.0.1:9");
        let path = config.run_log_file();
        save_a_sync(&config, "a377aa94");
        let line = fs::read_to_string(&path).unwrap();
        // One line under the size: nothing is cut.
        let lines = (MAX_LOG_BYTES as usize / line.len()) - 1;
        fs::write(&path, line.repeat(lines)).unwrap();
        save_a_sync(&config, "a377aa94");
        assert_eq!(read_log(&path).len(), lines + 1);

        // The line that passes the size cuts the log.
        save_a_sync(&config, "b488bb05");

        let size = fs::metadata(&path).unwrap().len() as usize;
        assert!(size <= KEPT_LOG_BYTES, "the log holds {size} bytes");
        assert!(size > KEPT_LOG_BYTES - 2 * line.len());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\"id\""), "the cut is at a line start");
        let events = read_log(&path);
        assert_eq!(events.len(), text.lines().count(), "every line is whole");
        assert_eq!(events.last().unwrap().hash.as_deref(), Some("b488bb05"));
        assert!(!outbox::temporary_path(&path).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_the_lines_after_the_last_one_sent_are_new() {
        let event = |id: &str| {
            let mut event = Event::new(Command::Sync, Code::ContentUpToDate);
            event.id = id.to_string();
            event
        };
        let ids = |events: Vec<Event>| -> Vec<String> {
            events.into_iter().map(|event| event.id).collect()
        };
        let log = || vec![event("a"), event("b"), event("c")];
        assert_eq!(ids(unsent(log(), None)), ["a", "b", "c"]);
        assert_eq!(ids(unsent(log(), Some("a"))), ["b", "c"]);
        assert_eq!(ids(unsent(log(), Some("c"))), Vec::<String>::new());
        // A cut removed the line: every line is new again.
        assert_eq!(ids(unsent(log(), Some("gone"))), ["a", "b", "c"]);
        // The oldest lines go first, and the rest waits.
        let many: Vec<Event> = (0..MAX_EVENTS_PER_REPORT + 5)
            .map(|n| event(&n.to_string()))
            .collect();
        let first = unsent(many, None);
        assert_eq!(first.len(), MAX_EVENTS_PER_REPORT);
        assert_eq!(first[0].id, "0");
    }

    // ------------------------------------------------------- the state ---

    #[test]
    fn the_state_is_read_from_the_files_of_this_machine() {
        let dir = temp_dir("diagnostics-state");
        let config = Config::for_test(&dir.join("root"), "http://127.0.0.1:9");
        fs::create_dir_all(config.root()).unwrap();

        // Nothing is installed, and no place outside the root is known.
        let empty = state(&config);
        assert_eq!(
            empty.software.version.as_deref(),
            Some(selfupdate::CURRENT_VERSION)
        );
        assert_eq!(
            empty.content,
            Content {
                installed_hash: None,
                installed_at_unix: None,
                present: false
            }
        );
        assert_eq!(empty.link.hook, Presence::Unknown);
        assert_eq!(empty.link.agent, Agent::None);
        assert_eq!(
            empty.outbox,
            Outbox {
                waiting: 0,
                rejected: 0,
                sent: 0,
                ignored: 0,
                last_push_at_unix: None,
                last_push_notes: None,
                operator: false
            }
        );

        state::write(&config.state_file(), &state::State::new("a377aa94")).unwrap();
        fs::create_dir_all(config.content_dir()).unwrap();
        settled_note(&config, "2026-09-30-a.md", NOTE);
        for (folder, name) in [
            (config.rejected_dir(), "2026-09-29-b.md"),
            (config.rejected_dir(), "2026-09-29-c.md"),
            (config.sent_dir().join("2026-09"), "2026-09-28-d.md"),
            (config.sent_dir().join("2026-09"), "2026-09-28-e.md"),
            (config.sent_dir().join("2026-08"), "2026-08-28-f.md"),
        ] {
            fs::create_dir_all(&folder).unwrap();
            fs::write(folder.join(name), NOTE).unwrap();
        }
        fs::write(
            config.rejected_dir().join("2026-09-29-b.md.reason.txt"),
            "why\n",
        )
        .unwrap();
        fs::write(
            config.push_state_file(),
            r#"{"last_push_at_unix":1790500000,"last_push_notes":2}"#,
        )
        .unwrap();
        fs::write(config.operator_file(), "gabriele\n").unwrap();
        // A note with the wrong ending, and a note in a folder: push reads
        // neither, and the state counts both.
        fs::write(config.outbox_dir().join("2026-09-30-g.txt"), NOTE).unwrap();
        let folder = config.outbox_dir().join("2026-09");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("2026-09-30-h.md"), NOTE).unwrap();
        let claude = dir.join("claude");
        fs::create_dir_all(&claude).unwrap();
        fs::write(
            claude.join("settings.json"),
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"x sync # brainmaker-link"}]}]}}"#,
        )
        .unwrap();
        let agents = Agents::unloaded(&dir.join("agents"));
        look_in_this_test(&claude, Some(agents.clone()));

        let read = state(&config);

        assert_eq!(read.content.installed_hash.as_deref(), Some("a377aa94"));
        assert!(read.content.installed_at_unix.is_some());
        assert!(read.content.present);
        assert_eq!(
            read.link,
            Link {
                hook: Presence::Present,
                block: Presence::Absent,
                skills: 0,
                agent: Agent::Absent
            }
        );
        assert_eq!(
            read.outbox,
            Outbox {
                waiting: 1,
                rejected: 2,
                sent: 3,
                ignored: 2,
                last_push_at_unix: Some(1_790_500_000),
                last_push_notes: Some(2),
                operator: true
            }
        );
        fs::create_dir_all(dir.join("agents")).unwrap();
        fs::write(agents.plist(), "<plist/>").unwrap();
        assert_eq!(state(&config).link.agent, Agent::Present);

        // The state as a report carries it: a value that is absent is null.
        let json = serde_json::to_value(&empty).unwrap();
        assert_eq!(json["content"]["installed_hash"], serde_json::Value::Null);
        assert_eq!(json["link"]["hook"], "unknown");
        assert_eq!(json["link"]["agent"], "none");
        assert_eq!(json["outbox"]["operator"], false);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hash_that_is_not_a_release_name_is_not_in_the_state() {
        let dir = temp_dir("diagnostics-hash");
        let config = Config::for_test(&dir, "http://127.0.0.1:9");
        // The client takes 8 letters and digits, and the server names a
        // release with 8 hexadecimal characters.
        state::write(&config.state_file(), &state::State::new("ZZZZZZZZ")).unwrap();
        assert_eq!(state(&config).content.installed_hash, None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_rules_of_a_value_are_the_rules_of_the_server() {
        assert!(is_platform("darwin-arm64"));
        assert!(is_platform("linux-x86_64"));
        for bad in ["darwin", "Darwin-arm64", "freebsd-x86_64", "s3cr3t-9f2c"] {
            assert!(!is_platform(bad), "{bad}");
        }
        for good in ["0.1.9", "1.0", "10.20.30.40", "0.2.0-rc1", "1.0.0+build.5"] {
            assert!(is_version(good), "{good}");
        }
        for bad in [
            "",
            "1",
            "v0.1.9",
            "0.1.9 ",
            "0..9",
            "1.2.3.4.5",
            "123456.1",
            "0.1.9-",
            "0.1.9-rc1-with-a-long-tail",
            // What version::validate takes, and a report does not.
            "s3cr3t-9f2c",
            "abcdefghijklmnopqrstuvwxyz0123456789",
        ] {
            assert!(!is_version(bad), "{bad}");
        }
        // The version of this build is one that a report carries.
        assert!(is_version(selfupdate::CURRENT_VERSION));
        assert!(is_time(1_790_683_100));
        assert!(!is_time(0) && !is_time(MAX_TIME + 1));
        assert!(is_content_hash("a377aa94"));
        assert!(!is_content_hash("a377aa9") && !is_content_hash("a377aa9g"));
        assert_eq!(new_id().len(), 16);
        assert!(is_event_id(&new_id()));
        assert_ne!(new_id(), new_id());
    }

    // ---------------------------------------------------------- sending ---

    #[test]
    fn a_report_carries_the_state_and_the_new_lines_with_the_sync_token() {
        let server = server_with(vec![stored()]);
        let (dir, config) = config_for(&server, "diagnostics-send");
        save_a_sync(&config, "a377aa94");

        assert_eq!(send(&config), Sent::Stored(1));

        let received = server.received();
        let request = received.last().unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/diagnostics");
        assert_eq!(
            request.header("authorization"),
            Some("Bearer sync-token-value")
        );
        assert_eq!(request.header("content-type"), Some("application/json"));
        let body = &reports(&server)[0];
        assert_eq!(body["schema"], 1);
        assert_eq!(
            body["state"]["software"]["version"],
            selfupdate::CURRENT_VERSION
        );
        assert_eq!(body["state"]["outbox"]["waiting"], 0);
        assert_eq!(body["events"].as_array().unwrap().len(), 1);
        assert_eq!(body["events"][0]["code"], "content.up_to_date");
        assert_eq!(body["events"][0]["hash"], "a377aa94");
        // One token request, and one report: the token of the run serves.
        assert_eq!(received.len(), 2);

        let record = read_record(&config).unwrap();
        assert!(record.last_report_at_unix.is_some());
        assert_eq!(
            record.sent_through.as_deref(),
            body["events"][0]["id"].as_str()
        );
        assert!(!outbox::temporary_path(&config.report_record_file()).exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_report_goes_at_most_once_in_the_interval_and_sends_a_line_once() {
        let server = server_with(vec![stored()]);
        let (dir, config) = config_for(&server, "diagnostics-interval");
        save_a_sync(&config, "a377aa94");
        assert_eq!(send(&config), Sent::Stored(1));

        save_a_sync(&config, "b488bb05");
        assert_eq!(send(&config), Sent::NotDue);
        assert_eq!(reports(&server).len(), 1);

        pass_the_interval(&config);
        assert_eq!(send(&config), Sent::Stored(1));
        let second = &reports(&server)[1];
        assert_eq!(second["events"].as_array().unwrap().len(), 1);
        assert_eq!(second["events"][0]["hash"], "b488bb05");

        // With no new line, the state goes alone.
        pass_the_interval(&config);
        assert_eq!(send(&config), Sent::Stored(0));
        assert_eq!(reports(&server)[2]["events"].as_array().unwrap().len(), 0);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_link_that_worked_makes_the_next_report_due() {
        let server = server_with(vec![stored()]);
        let (dir, config) = config_for(&server, "diagnostics-due");
        // With no record there is nothing to change, and no file appears.
        make_due(&config);
        assert!(read_record(&config).is_none());

        save_a_sync(&config, "a377aa94");
        assert_eq!(send(&config), Sent::Stored(1));
        assert_eq!(send(&config), Sent::NotDue);
        let before = read_record(&config).unwrap();

        make_due(&config);

        let after = read_record(&config).unwrap();
        assert_eq!(after.sent_through, before.sent_through, "the cursor stays");
        assert_eq!(after.last_report_at_unix, before.last_report_at_unix);
        // The report is due 90 seconds from now, and not at once: the agent
        // that link loads runs a sync a few seconds later.
        // A second can pass between the report and this call.
        let moved = before.last_attempt_at_unix - after.last_attempt_at_unix;
        let whole = REPORT_INTERVAL.as_secs() - WAIT_AFTER_LINK.as_secs();
        assert!((whole - 5..=whole).contains(&moved), "moved by {moved}");
        assert_eq!(send(&config), Sent::NotDue);
        let mut later = after.clone();
        later.last_attempt_at_unix -= WAIT_AFTER_LINK.as_secs();
        write_record(&config, &later).unwrap();
        assert_eq!(send(&config), Sent::Stored(0));

        // A report that is due already is not made to wait.
        let mut old = read_record(&config).unwrap();
        old.last_attempt_at_unix = 1_000;
        write_record(&config, &old).unwrap();
        make_due(&config);
        assert_eq!(read_record(&config).unwrap().last_attempt_at_unix, 1_000);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_server_without_the_route_keeps_the_lines_for_a_later_report() {
        // The test server answers 404 for a route that it does not have, as
        // a server older than this route does.
        let server = server_with(Vec::new());
        let (dir, config) = config_for(&server, "diagnostics-old-server");
        save_a_sync(&config, "a377aa94");

        assert_eq!(send(&config), Sent::Failed);

        let record = read_record(&config).unwrap();
        assert_eq!(record.last_report_at_unix, None);
        assert_eq!(record.sent_through, None);
        assert!(record.last_attempt_at_unix > 0);
        assert_eq!(read_log(&config.run_log_file()).len(), 1);
        // The next try waits for the interval too.
        assert_eq!(send(&config), Sent::NotDue);
        assert_eq!(reports(&server).len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_answer_but_a_success_leaves_the_lines_unsent() {
        for status in [400, 401, 403, 413, 429, 500] {
            let server = server_with(vec![
                Route::post("/diagnostics", r#"{"error":"no"}"#).status(status),
            ]);
            let (dir, config) = config_for(&server, "diagnostics-refused");
            save_a_sync(&config, "a377aa94");

            assert_eq!(send(&config), Sent::Failed, "{status}");
            assert_eq!(read_record(&config).unwrap().sent_through, None);
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn a_server_that_cannot_be_reached_fails_the_report_and_nothing_else() {
        let server = server_with(Vec::new());
        let dir = temp_dir("diagnostics-unreachable");
        // The token comes from the live server, and the report goes nowhere.
        let config = Config::for_test_with_credentials(
            &dir,
            &crate::testutil::closed_port_base(),
            &server.base(),
        );
        auth::bearer(&config, auth::SCOPE_SYNC).unwrap();
        save_a_sync(&config, "a377aa94");

        assert_eq!(send(&config), Sent::Failed);
        assert_eq!(read_log(&config.run_log_file()).len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_run_that_received_no_sync_token_makes_no_request() {
        let server = server_with(vec![stored()]);
        let dir = temp_dir("diagnostics-no-token");
        let config = Config::for_test_with_credentials(&dir, &server.base(), &server.base());
        save_a_sync(&config, "a377aa94");

        assert_eq!(send(&config), Sent::NoToken);
        // The same holds for a run with no credential at all.
        let bare = Config::for_test(&dir, &server.base());
        assert_eq!(send(&bare), Sent::NoToken);

        assert!(server.received().is_empty());
        assert!(read_record(&config).is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_held_lock_skips_the_report() {
        let server = server_with(vec![stored()]);
        let (dir, config) = config_for(&server, "diagnostics-lock");
        save_a_sync(&config, "a377aa94");
        let held = lock::acquire(&config.report_lock_file(), Duration::ZERO)
            .unwrap()
            .unwrap();

        assert_eq!(send(&config), Sent::Busy);

        assert!(reports(&server).is_empty());
        drop(held);
        assert_eq!(send(&config), Sent::Stored(1));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn more_lines_than_one_report_carries_go_with_the_next() {
        let server = server_with(vec![stored()]);
        let (dir, config) = config_for(&server, "diagnostics-backlog");
        let mut run = Run::new(Command::Sync);
        for _ in 0..MAX_EVENTS_PER_REPORT + 3 {
            run.content(&up_to_date("a377aa94"));
        }
        run.save(&config);

        assert_eq!(send(&config), Sent::Stored(MAX_EVENTS_PER_REPORT));
        pass_the_interval(&config);
        assert_eq!(send(&config), Sent::Stored(3));

        let sent = reports(&server);
        let first = sent[0]["events"].as_array().unwrap();
        let second = sent[1]["events"].as_array().unwrap();
        let log = read_log(&config.run_log_file());
        assert_eq!(first[0]["id"], log[0].id.as_str(), "the oldest go first");
        assert_eq!(
            second[0]["id"],
            log[MAX_EVENTS_PER_REPORT].id.as_str(),
            "no line goes twice"
        );
        let size = server
            .received()
            .iter()
            .map(|request| request.body.len())
            .max()
            .unwrap();
        assert!(size as u64 <= MAX_REPORT_BYTES, "a report of {size} bytes");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_report_holds_no_credential_no_path_and_nothing_of_a_note() {
        let server = Server::start(vec![
            Route::token("sync", r#"{"access_token":"sync-token-value"}"#),
            Route::token("outbox:write", r#"{"access_token":"write-token-value"}"#),
            Route::post(
                "/outbox/2026-09-30-riservata.md",
                r#"{"error":"the note names Mario Rossi"}"#,
            )
            .status(400),
            stored(),
        ]);
        let (dir, config) = config_for(&server, "diagnostics-private");
        // One note that the server refuses, and one that breaks a rule here.
        let private = "---\nkind: fact\ndomain: damac\n---\nIBAN IT60X0542811101000000123456\n";
        settled_note(&config, "2026-09-30-riservata.md", private);
        settled_note(
            &config,
            "2026-09-30-segreto.md",
            "---\nkind: parola-segreta\ndomain: damac\n---\nTesto riservato.\n",
        );
        fs::write(config.operator_file(), "gabriele\n").unwrap();
        // A file that push does not read as a note: only its count may leave.
        fs::write(
            config.outbox_dir().join("preventivo-cliente-nascosto.txt"),
            "Testo riservato.\n",
        )
        .unwrap();
        let mut run = Run::new(Command::Sync);
        run.content(&Err(anyhow::anyhow!(
            "cannot read {}/content/latest with the-client-secret",
            server.base()
        )));
        run.pushed(&outbox::push(&config, &quiet));
        run.save(&config);

        assert_eq!(send(&config), Sent::Stored(3));

        let request = server
            .received()
            .into_iter()
            .find(|request| request.path == "/diagnostics")
            .unwrap();
        let body = String::from_utf8(request.body).unwrap();
        let root = dir.display().to_string();
        let base = server.base();
        for private in [
            "the-client-id",
            "the-client-secret",
            "sync-token-value",
            "write-token-value",
            "riservata",
            "segreto",
            "parola-segreta",
            "IT60X0542811101000000123456",
            "Mario Rossi",
            "gabriele",
            "Testo",
            "preventivo",
            "nascosto",
            root.as_str(),
            base.as_str(),
            "127.0.0.1",
            "://",
        ] {
            assert!(!body.contains(private), "the report holds {private:?}");
        }
        // The run log on the disk holds none of them either.
        let log = fs::read_to_string(config.run_log_file()).unwrap();
        for private in ["the-client-secret", "parola-segreta", "Mario Rossi", "://"] {
            assert!(!log.contains(private), "the run log holds {private:?}");
        }
        // What it does hold: the rule of each refusal, as a word.
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let rules: Vec<&str> = json["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|event| event["rule"].as_str())
            .collect();
        assert_eq!(rules, ["kind", "server"]);
        assert_eq!(json["state"]["outbox"]["rejected"], 2);
        assert_eq!(json["state"]["outbox"]["ignored"], 1);
        assert_eq!(json["state"]["outbox"]["operator"], true);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_report_never_passes_its_size() {
        let dir = temp_dir("diagnostics-size");
        let config = Config::for_test(&dir, "http://127.0.0.1:9");
        // The longest event that the rules allow.
        let mut event = Event::new(Command::SelfUpdate, Code::ContentUnreachable);
        event.cause = Some(Cause::Unreachable);
        event.status = Some(599);
        event.rule = Some(Rule::Frontmatter);
        event.count = Some(MAX_COUNT);
        event.hash = Some("a377aa94".into());
        event.version = Some("99999.99999.99999.99999-abcdefghijklmnop".into());
        assert!(event.is_valid());
        assert!(serde_json::to_string(&event).unwrap().len() <= MAX_LINE_BYTES);
        let events = vec![event; MAX_EVENTS_PER_REPORT];

        let (body, count) = render(&state(&config), &events).unwrap();

        assert!(body.len() as u64 <= MAX_REPORT_BYTES);
        assert_eq!(count, MAX_EVENTS_PER_REPORT, "a full report fits whole");
        fs::remove_dir_all(&dir).unwrap();
    }
}
