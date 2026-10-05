// SPDX-License-Identifier: GPL-3.0-or-later

//! Why a step failed, as one word from a fixed list.
//!
//! # Why this exists
//!
//! An error is text for the person at the terminal. That text can name a URL
//! or a path, and it can hold words that a server chose, so it never leaves
//! the machine. The diagnostic report still needs the cause: an admin who
//! reads that the content step failed cannot tell a laptop that was offline
//! from a release that no key accepts.
//!
//! So the few places that know the cause say it twice: as the text, and as a
//! [`Cause`]. [`failed`] makes an error that prints the text and carries the
//! word, and [`of`] reads the word back from any error. An error that no place
//! marked reads as [`Cause::Other`]. See [`crate::diagnostics`] for the report.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::auth;

/// The cause of a failure. A report carries the word, and never the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// No answer came: the connection, the name lookup, or the wait failed.
    Unreachable,
    /// The server answered with a status that is not a success.
    Http,
    /// The issuer refused the client credentials.
    Credentials,
    /// The issuer does not grant the scope that the step needs.
    Scope,
    /// No key that this binary trusts signed the document.
    Signature,
    /// The content release is not newer than the installed one.
    Order,
    /// Any other failure.
    Other,
}

/// A cause, and the HTTP status when the failure was an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub cause: Cause,
    pub status: Option<u16>,
}

impl Failure {
    /// A failure with a cause and no status.
    pub fn new(cause: Cause) -> Self {
        Self {
            cause,
            status: None,
        }
    }
}

/// An error that prints its text and carries a [`Failure`].
#[derive(Debug)]
struct Marked {
    failure: Failure,
    text: String,
}

impl fmt::Display for Marked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl std::error::Error for Marked {}

/// An error that prints `text`, and that [`of`] reads as `cause`.
///
/// The text is all that a person sees, so a place that changes from `bail!` to
/// this function prints what it printed before.
pub fn failed(cause: Cause, status: Option<u16>, text: String) -> anyhow::Error {
    anyhow::Error::new(Marked {
        failure: Failure { cause, status },
        text,
    })
}

/// The cause of `error`, whatever context lies around it.
///
/// A transport error of the HTTP client that no place turned into text reads
/// as [`Cause::Unreachable`], and an `invalid_scope` answer as [`Cause::Scope`].
pub fn of(error: &anyhow::Error) -> Failure {
    if let Some(marked) = error.downcast_ref::<Marked>() {
        return marked.failure;
    }
    if auth::is_invalid_scope(error) {
        return Failure::new(Cause::Scope);
    }
    if error.downcast_ref::<ureq::Error>().is_some() {
        return Failure::new(Cause::Unreachable);
    }
    Failure::new(Cause::Other)
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    #[test]
    fn a_marked_error_prints_its_text_and_nothing_else() {
        let error = failed(
            Cause::Http,
            Some(503),
            "the server returned HTTP 503".into(),
        );
        assert_eq!(format!("{error:#}"), "the server returned HTTP 503");
        assert_eq!(
            format!("{:#}", error.context("cannot read the route")),
            "cannot read the route: the server returned HTTP 503"
        );
    }

    #[test]
    fn the_cause_is_read_through_every_context() {
        let error: anyhow::Error = Err::<(), _>(failed(Cause::Signature, None, "no key".into()))
            .context("the content release")
            .context("cannot trust the content release")
            .unwrap_err();
        assert_eq!(of(&error), Failure::new(Cause::Signature));

        let error = failed(Cause::Credentials, Some(401), "refused".into());
        assert_eq!(
            of(&error),
            Failure {
                cause: Cause::Credentials,
                status: Some(401)
            }
        );
    }

    #[test]
    fn an_error_that_no_place_marked_reads_as_other() {
        assert_eq!(
            of(&anyhow::anyhow!("cannot write the file")),
            Failure::new(Cause::Other)
        );
    }

    #[test]
    fn a_refused_scope_and_a_transport_error_have_their_own_words() {
        let scope = anyhow::Error::new(auth::InvalidScope {
            scope: auth::SCOPE_OUTBOX_WRITE.to_string(),
        })
        .context("cannot get an access token");
        assert_eq!(of(&scope), Failure::new(Cause::Scope));

        let transport = anyhow::anyhow!(ureq::Error::ConnectionFailed).context("cannot read");
        assert_eq!(of(&transport), Failure::new(Cause::Unreachable));
    }

    #[test]
    fn a_cause_is_written_as_one_lowercase_word() {
        assert_eq!(
            serde_json::to_string(&Cause::Unreachable).unwrap(),
            "\"unreachable\""
        );
        assert_eq!(
            serde_json::from_str::<Cause>("\"credentials\"").unwrap(),
            Cause::Credentials
        );
        assert!(serde_json::from_str::<Cause>("\"a free text\"").is_err());
    }
}
