// SPDX-License-Identifier: GPL-3.0-or-later

//! The removal order: the one thing that this program does on the word of
//! another.
//!
//! # Why this exists
//!
//! A person leaves the team, and the content stays on their machine. The
//! administrator can revoke the client at the issuer, which stops every later
//! request. It removes nothing: `sync` then keeps the installed content, and
//! Claude keeps reading it. Only `uninstall` removes it, and only the person
//! at that machine can run `uninstall`.
//!
//! A removal order closes that gap. The administrator signs a short document
//! that names one client, and stores it on the server. At its next `sync`,
//! that client receives the document, checks it, and removes brainmaker from
//! its machine as `uninstall` does.
//!
//! # What an order is
//!
//! A JSON document, signed with a key in
//! [`crate::signature::REMOVAL_KEYS`]:
//!
//! ```text
//! {"order":"remove","client_id":"the-client-id","outbox":"keep","issued_at":1791300000}
//! ```
//!
//! `outbox` says what happens to the notes on the machine: `keep` leaves the
//! outbox as `uninstall` does, and `remove` takes it away with every note in
//! it. `issued_at` is the time of signing. It is a record for the admin, and
//! nothing here acts on it.
//!
//! # Who can give one
//!
//! The holder of a removal key, and nobody else. The server carries the order
//! in the body of a `410 Gone` answer to the content check, and it cannot
//! write one: the signature check runs over the served bytes, with keys that
//! are compiled in. An order that names another client is refused, so the
//! server cannot give the order of one person to another person's machine. A
//! build with no removal key obeys no order.
//!
//! An order has no end date. A client ID that received one is spent: issue a
//! new client to a person who comes back.
//!
//! # Who signs one
//!
//! The admin, with `admin retire`, on the machine that holds the removal key.
//! [`Signer`] reads the key from its file, checks that this build trusts it,
//! and signs one order for each client of the person who left. The key file
//! is the one key that this program reads from outside the sealed store, and
//! no command writes it.
//!
//! # What obeying does
//!
//! [`obey`] waits for a running install to end, runs
//! [`crate::uninstall::uninstall`], and sends the removal report: one small
//! document that says whether the removal ran to its end. The hourly agent of
//! a Mac goes last of all, after the report. That agent is the usual caller of
//! the `sync` that obeys, and launchd stops a run when it unloads the agent
//! that started it. The report follows
//! the rule of the diagnostic report: every value is a word from a fixed list
//! or a bounded number. A report that fails changes nothing, because the
//! removal is already done.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::cause::{self, Cause};
use crate::config::Config;
use crate::diagnostics;
use crate::lock;
use crate::remote;
use crate::schedule;
use crate::signature;
use crate::sync;
use crate::uninstall::{self, Notes};

/// The status of the answer that carries a removal order: `410 Gone`.
///
/// A client older than the order reads the status as a failed content check,
/// keeps the content that it holds, and exits 0.
pub const GONE: u16 = 410;

/// The version of the shape of a removal report. The server refuses any
/// other.
const SCHEMA: u32 = 1;

/// The one order that this program knows.
const REMOVE: &str = "remove";

/// The first second of the year 2000, and the last second of the year 2099,
/// in seconds since the Unix epoch. The signing tool writes no time outside
/// them.
const MIN_TIME: u64 = 946_684_800;
const MAX_TIME: u64 = 4_102_444_799;

/// A signed document as the server carries it: the payload as a string, and
/// the signature over the bytes of that string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    payload: String,
    signature: String,
}

/// The body of a `410 Gone` answer, as far as this module reads it.
#[derive(Debug, Deserialize)]
struct Gone {
    removal: Option<Envelope>,
}

/// What an order says about the notes on the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outbox {
    /// The outbox stays, with every note in it, as `uninstall` leaves it.
    Keep,
    /// The outbox goes, with every note in it, sent or not.
    Remove,
}

/// The signed payload. A field that this build does not know fails the parse:
/// it could change what the order means, and a removal has no way back.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    order: String,
    client_id: String,
    outbox: Outbox,
    issued_at: u64,
}

/// A removal order that a trusted key signed, and that names this client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Order {
    pub outbox: Outbox,
    /// When the order was signed, in seconds since the Unix epoch.
    pub issued_at: u64,
}

/// The removal order in the body of a `410 Gone` answer, when the body holds
/// one. A body of any other shape holds none.
pub fn envelope_in(body: &str) -> Option<Envelope> {
    serde_json::from_str::<Gone>(body).ok()?.removal
}

/// Checks a removal order, and returns it when this client must obey it.
///
/// The signature check comes first, and it runs over the served bytes, so
/// nothing parses a document that no trusted key signed. Every failure
/// carries the status 410 for the run log, and a signature that no key
/// accepts carries the cause `signature`. `admin diagnose` reads the two to
/// tell the admin that the client received an order and did not obey it.
pub fn read(config: &Config, envelope: &Envelope) -> Result<Order> {
    signature::verify_removal(envelope.payload.as_bytes(), &envelope.signature)
        .map_err(|error| refused(Cause::Signature, &format!("{error:#}")))?;

    let payload: Payload = serde_json::from_str(&envelope.payload)
        .map_err(|error| refused(Cause::Other, &format!("its payload is not usable: {error}")))?;
    if payload.order != REMOVE {
        return Err(refused(
            Cause::Other,
            "it carries an order that this brainmaker does not know",
        ));
    }
    if !(MIN_TIME..=MAX_TIME).contains(&payload.issued_at) {
        return Err(refused(Cause::Other, "its time of signing is not a time"));
    }
    // The server chooses which document it sends. Only the signed client ID
    // says whose order it is.
    let ours = config
        .credentials()
        .is_some_and(|credentials| credentials.is_client(&payload.client_id));
    if !ours {
        return Err(refused(Cause::Other, "it names another client"));
    }

    Ok(Order {
        outbox: payload.outbox,
        issued_at: payload.issued_at,
    })
}

/// An error for an order that this client does not obey. The installed
/// content then stays, as it does when the content check fails for any other
/// reason.
fn refused(cause: Cause, reason: &str) -> anyhow::Error {
    cause::failed(
        cause,
        Some(GONE),
        format!("the server sent a removal order, and this client does not obey it: {reason}"),
    )
}

/// The removal key of the admin, read from its file and checked against the
/// removal keys of this build. `admin retire` signs with it.
pub struct Signer {
    pair: Ed25519KeyPair,
}

impl Signer {
    /// Reads the key at `path`: a PKCS#8 Ed25519 key as hexadecimal, which is
    /// what `brainmaker-sign keygen` writes.
    ///
    /// It fails when this build does not trust the key for removal orders.
    /// No client of this build would obey an order from such a key, and the
    /// admin must learn that before the server retires a client for nothing.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read the removal key {}. Make one with brainmaker-sign keygen, put its \
                 public key in REMOVAL_KEYS, and keep the file there, or name another file with \
                 --key",
                path.display()
            )
        })?;
        let bytes = decode_hex(text.trim())
            .with_context(|| format!("the removal key {} is not usable", path.display()))?;
        let pair = Ed25519KeyPair::from_pkcs8(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "the removal key {} is not a PKCS#8 Ed25519 key: {error}",
                path.display()
            )
        })?;

        // A probe, signed and checked the way an order is.
        let probe = b"brainmaker removal key probe";
        let signed = hex(pair.sign(probe).as_ref());
        if signature::verify_removal(probe, &signed).is_err() {
            bail!(
                "the key in {} is not a removal key of this build: its public key, {}, is not \
                 in REMOVAL_KEYS in src/signature.rs. No client of this build obeys an order \
                 that it signs.",
                path.display(),
                hex(pair.public_key().as_ref())
            );
        }
        Ok(Self { pair })
    }

    /// Signs a removal order for the client `client_id`, with the time now as
    /// the time of signing. The caller checks the client ID first.
    pub fn order(&self, client_id: &str, outbox: Outbox) -> Result<Envelope> {
        let issued_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| anyhow::anyhow!("the clock reads a time before 1970"))?
            .as_secs();
        Ok(self.order_at(client_id, outbox, issued_at))
    }

    /// The same, with the time of signing given.
    fn order_at(&self, client_id: &str, outbox: Outbox, issued_at: u64) -> Envelope {
        // The signature covers these exact bytes, and a client checks them
        // before it parses them, so the text is made once and never again.
        let payload = serde_json::json!({
            "order": REMOVE,
            "client_id": client_id,
            "outbox": outbox,
            "issued_at": issued_at,
        })
        .to_string();
        Envelope {
            signature: hex(self.pair.sign(payload.as_bytes()).as_ref()),
            payload,
        }
    }
}

/// Writes bytes as lowercase hexadecimal.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Reads hexadecimal of any even length, in either case.
fn decode_hex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        bail!("it holds an odd number of characters, so it is not hexadecimal");
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let mut byte = 0u8;
        for half in pair {
            let value = match half {
                b'0'..=b'9' => half - b'0',
                b'a'..=b'f' => half - b'a' + 10,
                b'A'..=b'F' => half - b'A' + 10,
                _ => bail!("it holds a character that is not hexadecimal"),
            };
            byte = byte << 4 | value;
        }
        out.push(byte);
    }
    Ok(out)
}

/// What [`obey`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Done {
    /// Another run was installing content, and this run removed nothing. The
    /// hook and the agent are still there, so the next `sync` obeys the order.
    Waiting,
    /// brainmaker is removed from this machine.
    Removed {
        /// False when the root directory stays, with what the run left in it.
        root_removed: bool,
        /// Notes that were never sent. They stayed with the outbox, or went
        /// with it, as the order says.
        unsent: usize,
    },
}

/// Removes brainmaker from this machine, as `order` says, and tells the
/// server what happened.
///
/// A removal that stops part-way is an error, and the server learns of it. On
/// a Mac the hourly agent then stays, so the next hour tries again. Elsewhere
/// nothing runs `sync` again once the hook is gone.
pub fn obey(config: &Config, order: &Order, log: &dyn Fn(&str)) -> Result<Done> {
    let places = diagnostics::places();
    let Some(claude) = places.claude else {
        bail!(
            "cannot find the home directory, so the removal order cannot be obeyed: the Claude \
             configuration directory is unknown"
        );
    };

    // A run that is installing content would put the content back after the
    // removal. Wait for it to end. The lock is released before the removal,
    // because the lock file is one of the files that go.
    if config.root().is_dir() {
        match lock::acquire(&config.lock_file(), sync::LOCK_WAIT)? {
            Some(held) => drop(held),
            None => return Ok(Done::Waiting),
        }
    }

    let notes = match order.outbox {
        Outbox::Keep => Notes::Keep,
        Outbox::Remove => Notes::Remove,
    };
    // Everything but the hourly agent, which goes last: see below.
    let removed = uninstall::uninstall(config.layout(), &claude, None, notes, log);

    let report = match &removed {
        Ok(removed) if !removed.unrecognised => Report {
            schema: SCHEMA,
            outcome: Outcome::Removed,
            root: Some(if removed.root_removed {
                Root::Removed
            } else {
                Root::Kept
            }),
            notes_unsent: Some((removed.unsent as u64).min(diagnostics::MAX_COUNT)),
        },
        _ => Report {
            schema: SCHEMA,
            outcome: Outcome::Failed,
            root: None,
            notes_unsent: None,
        },
    };
    send(config, &report);

    let removed = removed?;
    if removed.unrecognised {
        bail!(
            "the removal order cannot be obeyed: {} carries no mark of brainmaker, so nothing \
             was removed",
            config.root().display()
        );
    }

    // Last of all, and after the report. On a Mac the hourly agent is the
    // usual caller of this run, and launchd stops the run when its agent is
    // unloaded. A removal that failed above returned before this line, so
    // the agent is still there and the next hour tries again.
    if let Some(agents) = &places.agents {
        match schedule::remove_from_within(agents) {
            Ok(true) => log(&format!(
                "Removed the hourly LaunchAgent {}.",
                agents.plist().display()
            )),
            Ok(false) => {}
            Err(error) => log(&format!(
                "notice: cannot remove the hourly LaunchAgent {}: {error:#}",
                agents.plist().display()
            )),
        }
    }
    Ok(Done::Removed {
        root_removed: removed.root_removed,
        unsent: removed.unsent,
    })
}

/// Whether the removal ran to its end. The removal report carries the word,
/// and `admin diagnose` reads it back from the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Removed,
    /// The removal stopped on an error, and files of brainmaker can remain.
    Failed,
}

/// Whether the root directory is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Root {
    Removed,
    /// The root stays, because it still holds something: the outbox, a file
    /// that brainmaker did not write, or on Windows the program that ran.
    Kept,
}

/// The removal report. Every value is a word from a fixed list or a bounded
/// number, as in the diagnostic report.
#[derive(Debug, Serialize)]
struct Report {
    schema: u32,
    outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<Root>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes_unsent: Option<u64>,
}

/// Sends the removal report, with the `sync` token that the run already
/// holds. The sealed settings are gone by now, and the token is in memory.
///
/// A run that received no `sync` token makes no request. Whatever the answer
/// is, nothing reads it: the removal is done.
fn send(config: &Config, report: &Report) {
    if !auth::received(config, auth::SCOPE_SYNC) {
        return;
    }
    let (Ok(body), Ok(token)) = (
        serde_json::to_string(report),
        auth::bearer(config, auth::SCOPE_SYNC),
    ) else {
        return;
    };
    let _ = remote::post_removal_report(config, &body, token.as_deref());
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::testutil::{Route, Server, Signer, temp_dir};

    const CLIENT_ID: &str = "the-client-id";

    fn quiet(_: &str) {}

    /// The payload of an order for `client_id`.
    fn payload(client_id: &str, outbox: &str) -> String {
        serde_json::json!({
            "order": "remove",
            "client_id": client_id,
            "outbox": outbox,
            "issued_at": 1_791_300_000u64,
        })
        .to_string()
    }

    fn envelope(signer: &Signer, payload: &str) -> Envelope {
        serde_json::from_str(&signer.envelope(payload)).unwrap()
    }

    /// A configuration whose client is [`CLIENT_ID`].
    fn config(tag: &str) -> Config {
        let dir = temp_dir(tag);
        Config::for_test_with_credentials(&dir, "http://127.0.0.1:9", "http://127.0.0.1:9")
    }

    /// The text and the failure of a refused order.
    fn refusal(config: &Config, envelope: &Envelope) -> (String, cause::Failure) {
        let error = read(config, envelope).unwrap_err();
        (format!("{error:#}"), cause::of(&error))
    }

    #[test]
    fn finds_the_order_in_a_gone_answer() {
        let body =
            r#"{"error":"this client is retired","removal":{"payload":"{}","signature":"ab"}}"#;
        let found = envelope_in(body).unwrap();
        assert_eq!(found.payload, "{}");
        assert_eq!(found.signature, "ab");

        // Any other body holds no order.
        for body in [
            r#"{"error":"the link expired"}"#,
            r#"{"error":"x","removal":null}"#,
            r#"{"error":"x","removal":"remove"}"#,
            "gone",
            "",
        ] {
            assert_eq!(envelope_in(body), None, "{body}");
        }
    }

    #[test]
    fn reads_an_order_that_a_removal_key_signed_for_this_client() {
        let signer = Signer::new();
        signer.trust_for_removal();
        let config = config("removal-read");

        let order = read(&config, &envelope(&signer, &payload(CLIENT_ID, "keep"))).unwrap();
        assert_eq!(order.outbox, Outbox::Keep);
        assert_eq!(order.issued_at, 1_791_300_000);

        let order = read(&config, &envelope(&signer, &payload(CLIENT_ID, "remove"))).unwrap();
        assert_eq!(order.outbox, Outbox::Remove);
    }

    #[test]
    fn refuses_an_order_that_no_removal_key_signed() {
        let signer = Signer::new();
        // The thread trusts another key for removal orders.
        Signer::new().trust_for_removal();
        let config = config("removal-unsigned");

        let (text, failure) = refusal(&config, &envelope(&signer, &payload(CLIENT_ID, "keep")));
        assert!(text.contains("does not obey it"), "got {text}");
        assert!(text.contains("no signature from a key"), "got {text}");
        assert_eq!(failure.cause, Cause::Signature);
        assert_eq!(failure.status, Some(410));
    }

    #[test]
    fn a_content_key_and_a_software_key_sign_no_removal_order() {
        // The key that this thread trusts for content and for software is not
        // a removal key. With no removal key, no order is obeyed.
        let signer = Signer::new();
        signer.trust();
        signature::trust_for_removal_in_this_test(&[]);
        let config = config("removal-content-key");

        let (text, failure) = refusal(&config, &envelope(&signer, &payload(CLIENT_ID, "keep")));
        assert!(text.contains("trusts no signing key"), "got {text}");
        assert_eq!(failure.cause, Cause::Signature);
    }

    #[test]
    fn refuses_an_altered_order() {
        let signer = Signer::new();
        signer.trust_for_removal();
        let config = config("removal-altered");

        // Signed for another client, then pointed at this one.
        let mut altered = envelope(&signer, &payload("another-client", "keep"));
        altered.payload = payload(CLIENT_ID, "keep");
        let (_, failure) = refusal(&config, &altered);
        assert_eq!(failure.cause, Cause::Signature);
    }

    #[test]
    fn refuses_an_order_for_another_client() {
        let signer = Signer::new();
        signer.trust_for_removal();
        let config = config("removal-other-client");

        let (text, failure) = refusal(
            &config,
            &envelope(&signer, &payload("another-client", "keep")),
        );
        assert!(text.contains("names another client"), "got {text}");
        // The text names no client ID, this one's or the other one's.
        assert!(!text.contains("another-client"), "got {text}");
        assert!(!text.contains(CLIENT_ID), "got {text}");
        assert_eq!(failure.cause, Cause::Other);
        assert_eq!(failure.status, Some(410));
    }

    #[test]
    fn a_client_with_no_credential_obeys_no_order() {
        let signer = Signer::new();
        signer.trust_for_removal();
        let dir = temp_dir("removal-no-credential");
        let config = Config::for_test(&dir, "http://127.0.0.1:9");

        let (text, _) = refusal(&config, &envelope(&signer, &payload(CLIENT_ID, "keep")));
        assert!(text.contains("names another client"), "got {text}");
    }

    #[test]
    fn refuses_a_payload_of_another_shape() {
        let signer = Signer::new();
        signer.trust_for_removal();
        let config = config("removal-shape");

        let unknown_order = serde_json::json!({
            "order": "wipe",
            "client_id": CLIENT_ID,
            "outbox": "keep",
            "issued_at": 1_791_300_000u64,
        });
        let unknown_field = serde_json::json!({
            "order": "remove",
            "client_id": CLIENT_ID,
            "outbox": "keep",
            "issued_at": 1_791_300_000u64,
            "also": "the home directory",
        });
        let unknown_outbox = payload(CLIENT_ID, "shred");
        let no_time = serde_json::json!({
            "order": "remove",
            "client_id": CLIENT_ID,
            "outbox": "keep",
            "issued_at": 12u64,
        });
        // A content release and a software manifest are no removal order,
        // whoever signed them.
        let release = serde_json::json!({
            "hash": "a1b2c3d4",
            "sha256": "a".repeat(64),
            "size_bytes": 10,
        });
        let manifest = serde_json::json!({ "version": "0.2.0", "platforms": {} });

        for text in [
            unknown_order.to_string(),
            unknown_field.to_string(),
            unknown_outbox,
            no_time.to_string(),
            release.to_string(),
            manifest.to_string(),
            "not json".to_string(),
        ] {
            let (_, failure) = refusal(&config, &envelope(&signer, &text));
            assert_eq!(failure.cause, Cause::Other, "{text}");
            assert_eq!(failure.status, Some(410), "{text}");
        }
    }

    /// A root as `sync` leaves it, and the bridge directory of the test.
    fn installed(config: &Config) -> std::path::PathBuf {
        fs::create_dir_all(config.content_dir()).unwrap();
        fs::write(config.content_dir().join("CLAUDE.md"), b"# briefing").unwrap();
        crate::state::write(&config.state_file(), &crate::state::State::new("a1b2c3d4")).unwrap();
        let claude = config.root().join("claude-home");
        fs::create_dir_all(&claude).unwrap();
        fs::write(
            claude.join("CLAUDE.md"),
            b"# Mine\n\n<!-- brainmaker-link: start -->\nShared.\n<!-- brainmaker-link: end -->\n",
        )
        .unwrap();
        claude
    }

    #[test]
    fn obeying_removes_brainmaker_and_reports_it() {
        let server = Server::start(vec![
            Route::token("sync", r#"{"access_token":"s"}"#),
            Route::post("/diagnostics/removal", "{}"),
        ]);
        let dir = temp_dir("removal-obey");
        let root = dir.join("root");
        let config = Config::for_test_with_credentials(&root, &server.base(), &server.base());
        let claude = installed(&config);
        // The bridge lives outside the root in a real install.
        let claude = {
            let outside = dir.join("claude");
            fs::rename(&claude, &outside).unwrap();
            outside
        };
        diagnostics::look_in_this_test(&claude, None);
        fs::create_dir_all(config.outbox_dir()).unwrap();
        fs::write(config.outbox_dir().join("2026-10-06-a.md"), b"waiting\n").unwrap();
        // The run holds a sync token, as it does after the content check.
        auth::bearer(&config, auth::SCOPE_SYNC).unwrap();

        let order = Order {
            outbox: Outbox::Keep,
            issued_at: 1_791_300_000,
        };
        let done = obey(&config, &order, &quiet).unwrap();

        assert_eq!(
            done,
            Done::Removed {
                root_removed: false,
                unsent: 1
            }
        );
        assert!(!config.content_dir().exists());
        assert!(!config.state_file().exists());
        assert!(config.outbox_dir().join("2026-10-06-a.md").is_file());
        assert_eq!(
            fs::read_to_string(claude.join("CLAUDE.md")).unwrap(),
            "# Mine\n"
        );

        let report = server
            .received()
            .into_iter()
            .find(|request| request.path == "/diagnostics/removal")
            .expect("the removal report was sent");
        assert_eq!(report.method, "POST");
        assert_eq!(report.header("authorization"), Some("Bearer s"));
        let body: serde_json::Value = serde_json::from_slice(&report.body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "schema": 1,
                "outcome": "removed",
                "root": "kept",
                "notes_unsent": 1,
            })
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_order_can_take_the_outbox() {
        let server = Server::start(vec![
            Route::token("sync", r#"{"access_token":"s"}"#),
            Route::post("/diagnostics/removal", "{}"),
        ]);
        let dir = temp_dir("removal-outbox");
        let root = dir.join("root");
        let config = Config::for_test_with_credentials(&root, &server.base(), &server.base());
        installed(&config);
        diagnostics::look_in_this_test(&dir.join("claude"), None);
        fs::create_dir_all(config.sent_dir().join("2026-09")).unwrap();
        fs::write(
            config.sent_dir().join("2026-09").join("2026-09-30-a.md"),
            b"sent\n",
        )
        .unwrap();
        fs::write(config.outbox_dir().join("2026-10-06-b.md"), b"waiting\n").unwrap();
        auth::bearer(&config, auth::SCOPE_SYNC).unwrap();

        let order = Order {
            outbox: Outbox::Remove,
            issued_at: 1_791_300_000,
        };
        let done = obey(&config, &order, &quiet).unwrap();

        // The bridge directory of this test is under the root, so the root
        // stays for it. The outbox and everything of brainmaker is gone.
        assert!(matches!(done, Done::Removed { unsent: 1, .. }), "{done:?}");
        assert!(!config.outbox_dir().exists());
        assert!(!config.content_dir().exists());

        let report = server
            .received()
            .into_iter()
            .find(|request| request.path == "/diagnostics/removal")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&report.body).unwrap();
        assert_eq!(body["outcome"], "removed");
        assert_eq!(body["notes_unsent"], 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn obeying_waits_while_another_run_installs() {
        let dir = temp_dir("removal-busy");
        let root = dir.join("root");
        let config =
            Config::for_test_with_credentials(&root, "http://127.0.0.1:9", "http://127.0.0.1:9");
        installed(&config);
        diagnostics::look_in_this_test(&dir.join("claude"), None);
        let held = lock::acquire(&config.lock_file(), std::time::Duration::ZERO)
            .unwrap()
            .unwrap();

        let order = Order {
            outbox: Outbox::Remove,
            issued_at: 1_791_300_000,
        };
        assert_eq!(obey(&config, &order, &quiet).unwrap(), Done::Waiting);
        assert!(config.content_dir().join("CLAUDE.md").is_file());
        assert!(config.state_file().is_file());

        // Once the install ended, the same order is obeyed.
        drop(held);
        assert!(matches!(
            obey(&config, &order, &quiet).unwrap(),
            Done::Removed { .. }
        ));
        assert!(!config.content_dir().exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_hourly_agent_goes_last_and_stays_when_the_removal_fails() {
        use crate::schedule::{Agents, Runs};

        let server = Server::start(vec![
            Route::token("sync", r#"{"access_token":"s"}"#),
            Route::post("/diagnostics/removal", "{}"),
        ]);
        let dir = temp_dir("removal-agent");
        let agents = Agents::unloaded(&dir.join("LaunchAgents"));
        let order = Order {
            outbox: Outbox::Keep,
            issued_at: 1_791_300_000,
        };

        // A root with no mark of brainmaker: the removal fails, and the agent
        // stays, so that the next hour can try again.
        let foreign = dir.join("site");
        let config = Config::for_test_with_credentials(&foreign, &server.base(), &server.base());
        fs::create_dir_all(foreign.join("content")).unwrap();
        schedule::install(
            &agents,
            "\"/x/bin/brainmaker\"",
            &foreign,
            Runs::UpdateAndSync,
            &quiet,
        )
        .unwrap();
        diagnostics::look_in_this_test(&dir.join("claude"), Some(agents.clone()));
        assert!(obey(&config, &order, &quiet).is_err());
        assert!(agents.plist().is_file(), "the agent stays for the next try");

        // An installed root: everything goes, the report is sent, and the
        // agent goes after it.
        let root = dir.join("root");
        let config = Config::for_test_with_credentials(&root, &server.base(), &server.base());
        installed(&config);
        auth::bearer(&config, auth::SCOPE_SYNC).unwrap();
        let lines = std::cell::RefCell::new(Vec::new());
        let log = |line: &str| lines.borrow_mut().push(line.to_string());

        assert!(matches!(
            obey(&config, &order, &log).unwrap(),
            Done::Removed { .. }
        ));

        assert!(!agents.plist().exists());
        let lines = lines.borrow();
        let last = lines.last().expect("the removal printed its lines");
        assert!(
            last.starts_with("Removed the hourly LaunchAgent "),
            "the agent is the last thing that goes: {lines:?}"
        );
        assert!(
            server
                .received()
                .iter()
                .any(|request| request.path == "/diagnostics/removal"),
            "the report was sent before the agent went"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_report_that_fails_does_not_fail_the_removal() {
        // The server has no removal route, as a server older than it.
        let server = Server::start(vec![Route::token("sync", r#"{"access_token":"s"}"#)]);
        let dir = temp_dir("removal-old-server");
        let root = dir.join("root");
        let config = Config::for_test_with_credentials(&root, &server.base(), &server.base());
        installed(&config);
        diagnostics::look_in_this_test(&dir.join("claude"), None);
        auth::bearer(&config, auth::SCOPE_SYNC).unwrap();

        let order = Order {
            outbox: Outbox::Keep,
            issued_at: 1_791_300_000,
        };
        assert!(matches!(
            obey(&config, &order, &quiet).unwrap(),
            Done::Removed { .. }
        ));
        assert!(!config.content_dir().exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_run_with_no_sync_token_sends_no_report() {
        let server = Server::start(vec![
            Route::token("sync", r#"{"access_token":"s"}"#),
            Route::post("/diagnostics/removal", "{}"),
        ]);
        let dir = temp_dir("removal-no-token");
        let root = dir.join("root");
        let config = Config::for_test_with_credentials(&root, &server.base(), &server.base());
        installed(&config);
        diagnostics::look_in_this_test(&dir.join("claude"), None);

        let order = Order {
            outbox: Outbox::Keep,
            issued_at: 1_791_300_000,
        };
        obey(&config, &order, &quiet).unwrap();
        assert!(server.received().is_empty(), "{:?}", server.received());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_root_with_no_mark_fails_the_order_and_reports_it() {
        let server = Server::start(vec![
            Route::token("sync", r#"{"access_token":"s"}"#),
            Route::post("/diagnostics/removal", "{}"),
        ]);
        let dir = temp_dir("removal-foreign");
        let root = dir.join("site");
        let config = Config::for_test_with_credentials(&root, &server.base(), &server.base());
        fs::create_dir_all(root.join("content")).unwrap();
        fs::write(root.join("content").join("post.md"), b"mine\n").unwrap();
        diagnostics::look_in_this_test(&dir.join("claude"), None);
        auth::bearer(&config, auth::SCOPE_SYNC).unwrap();

        let order = Order {
            outbox: Outbox::Remove,
            issued_at: 1_791_300_000,
        };
        let error = format!("{:#}", obey(&config, &order, &quiet).unwrap_err());
        assert!(error.contains("no mark of brainmaker"), "got {error}");
        assert!(root.join("content").join("post.md").is_file());

        let report = server
            .received()
            .into_iter()
            .find(|request| request.path == "/diagnostics/removal")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&report.body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "schema": 1, "outcome": "failed" })
        );
        fs::remove_dir_all(&dir).ok();
    }

    /// Writes a new removal key to a file as `brainmaker-sign keygen` does,
    /// and returns the path and the public key.
    fn key_file(dir: &std::path::Path) -> (std::path::PathBuf, String) {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let path = dir.join("removal-signing.key");
        fs::write(&path, format!("{}\n", hex(pkcs8.as_ref()))).unwrap();
        (path, hex(pair.public_key().as_ref()))
    }

    #[test]
    fn the_client_obeys_the_order_that_the_admin_signs() {
        let dir = temp_dir("removal-signer");
        let (path, public) = key_file(&dir);
        signature::trust_for_removal_in_this_test(&[public]);
        let config = config("removal-signer-client");

        let signer = super::Signer::load(&path).unwrap();
        let envelope = signer.order_at(CLIENT_ID, Outbox::Remove, 1_791_300_000);

        // The four fields that a client reads, and no other one.
        assert_eq!(
            envelope.payload,
            r#"{"order":"remove","client_id":"the-client-id","outbox":"remove","issued_at":1791300000}"#
        );
        let order = read(&config, &envelope).unwrap();
        assert_eq!(order.outbox, Outbox::Remove);
        assert_eq!(order.issued_at, 1_791_300_000);

        // The envelope goes to the server as the two strings.
        let body: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&envelope).unwrap()).unwrap();
        assert_eq!(body["payload"], envelope.payload);
        assert_eq!(body["signature"].as_str().unwrap().len(), 128);

        // An order signed now carries a time that a client takes.
        let now = signer.order(CLIENT_ID, Outbox::Keep).unwrap();
        assert_eq!(read(&config, &now).unwrap().outbox, Outbox::Keep);
        // And it is an order for that client alone.
        let other = signer.order("another-client", Outbox::Keep).unwrap();
        assert!(read(&config, &other).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_key_that_this_build_does_not_trust_signs_nothing() {
        let dir = temp_dir("removal-signer-untrusted");
        let (path, public) = key_file(&dir);
        // The thread trusts another key for removal orders.
        Signer::new().trust_for_removal();

        let error = match super::Signer::load(&path) {
            Ok(_) => panic!("loaded a key that no client trusts"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            error.contains("not a removal key of this build"),
            "got {error}"
        );
        assert!(error.contains("REMOVAL_KEYS"), "got {error}");
        // The public key is in the message, so that the admin can add it.
        assert!(error.contains(&public), "got {error}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_file_that_holds_no_key_is_refused() {
        let dir = temp_dir("removal-signer-bad");
        let load = |path: &std::path::Path| match super::Signer::load(path) {
            Ok(_) => panic!("loaded {}", path.display()),
            Err(error) => format!("{error:#}"),
        };

        let missing = load(&dir.join("removal-signing.key"));
        assert!(
            missing.contains("cannot read the removal key"),
            "got {missing}"
        );
        assert!(missing.contains("brainmaker-sign keygen"), "got {missing}");

        let path = dir.join("text.key");
        fs::write(&path, "not a key\n").unwrap();
        assert!(load(&path).contains("not usable"), "got {}", load(&path));

        fs::write(&path, "abc\n").unwrap();
        assert!(load(&path).contains("odd number"), "got {}", load(&path));

        fs::write(&path, format!("{}\n", "ab".repeat(40))).unwrap();
        assert!(
            load(&path).contains("not a PKCS#8 Ed25519 key"),
            "got {}",
            load(&path)
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_report_holds_fixed_words_and_numbers_only() {
        let report = Report {
            schema: SCHEMA,
            outcome: Outcome::Removed,
            root: Some(Root::Removed),
            notes_unsent: Some(3),
        };
        assert_eq!(
            serde_json::to_string(&report).unwrap(),
            r#"{"schema":1,"outcome":"removed","root":"removed","notes_unsent":3}"#
        );
    }
}
