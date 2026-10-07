# Rules of this repository

This file states the rules of the repository for a contributor and for a coding agent. Read it
before you change anything.

## What this is

`brainmaker` is one Rust binary. It keeps `~/.brainmaker/content` in step with a content API. It
replaces itself with a signed newer build. It links the content into `~/.claude`.

A second binary, `brainmaker-sign`, is in `tools/sign.rs`. It builds only with `--features sign`.
It never ships to a client.

[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) describes the modules and the decisions behind them.
[`docs/SECURITY.md`](docs/SECURITY.md) describes the trust model.

## Check your work

Run `scripts/check.sh`. It runs the three commands of the `test` workflow, with the same arguments.
A plain `cargo test` skips `tools/sign.rs`. The script does not skip it.

## Files that you must never open, print, or commit

- `signing.key`, `content-signing.key`, and `removal-signing.key`, in the repository root, are
  private signing keys. So is `removal-signing.key` under the root of an install, which
  `admin retire` signs with.
- `brainmaker.env`, `.brainmaker.env`, and any `*.env` file hold a client secret.

`.gitignore` excludes all of these files. It excludes `dist/` too.

## Rules of the code

- In `src/`, only `src/main.rs` prints to standard output. Every other module returns an
  `anyhow::Result`, or reports through the `log` closure that it receives.
- No URL with a host may enter `src/config.rs`, `src/auth.rs`, or the help text in `src/cli.rs`. A
  test fails when one does. A document that names the host of the content API uses
  `api.example.test`.
- Never print, log, or format a client identifier, a client secret, or an access token into text
  that a person or a log can read. The admin commands are the one exception: they print the client
  IDs that the server reports, because the admin needs them to tell two machines apart, and a client
  ID alone authenticates nothing. No command prints the credential of the machine it runs on.
- The diagnostic report and the run log hold words from fixed lists, bounded numbers, content
  hashes, and versions, and nothing else. Never add a field that holds free text: no error
  message, no path, no URL, no name and no text of a note. A failure enters the run log as a
  `Cause` from `src/cause.rs`, and a refused note as a `Rule` from `src/outbox.rs`. A new word
  needs the same word in the server, which stores no word that it does not know.
- The server never tells the client what to send or what to run. Nothing that a command does may
  depend on the body of the answer to a diagnostic report, or to a removal report.
- The removal order is the one thing that a client does on the word of another. It is not the word
  of the server: `sync` obeys an order only when a key in `REMOVAL_KEYS` signed it and its payload
  names this client, and `src/removal.rs` is the one place that decides. An order makes the client
  run `uninstall`, and it can do nothing else. Never add a second order, a field that widens what
  an order removes, or a way to obey without the signature check. A change to this rule needs the
  maintainer's word. See "The removal order" in [`docs/SECURITY.md`](docs/SECURITY.md).
- `removal::Signer` is the one place that reads a private key from outside the sealed store, for
  `admin retire`. Never print, log, send, or write that key, and never sign anything with it but a
  removal order and the probe that checks the key.
- Check a value from the network before it enters a URL, a path, or a header.
- `PUBLIC_KEYS`, `CONTENT_KEYS`, and `REMOVAL_KEYS` in `src/signature.rs` keep one key per line. No
  key may be in two lists. The release workflow reads the first two lists with `sed` and `grep`.
  `REMOVAL_KEYS` may be empty: such a build obeys no removal order.
- The release profile sets `panic = "abort"`. Code that handles a value from the network must not
  be able to panic.
- The code has no `#[allow(...)]` attribute. Do not add one.
- A new dependency needs a review. See "Dependency policy" in
  [`docs/SECURITY.md`](docs/SECURITY.md). Commit `Cargo.lock` with the new dependency.

## Rules of the tests

- Put the tests in a `#[cfg(test)] mod tests` block in the file that they cover.
- Write a test name as a sentence in snake_case.
- A test uses no network host other than the loopback address, no real home directory, and no fixed
  path for a file that it reads or writes. A test that needs a directory makes its own directory
  under `std::env::temp_dir()`.

## Platforms

The release builds macOS, Linux, and Windows. Code under `#[cfg(target_os = ...)]` or
`#[cfg(windows)]` does not compile on the other systems, so a local run does not check it.

The `test` workflow runs on Linux, macOS, and Windows. A failed test on any of the three systems
blocks a merge. A path in a test must suit each system: build it with `Path::join`, and do not
write `/` as a separator.

## Tests that need a server

`src/testutil.rs` is built for tests alone. It holds a small HTTP server on the loopback address, a
signer whose key a test makes the client trust, and a zip builder. A test that needs a server uses
these helpers.

The server records the headers and the body of each request. A route can send response headers,
give its replies in order, and, for the token route, answer only the scope that the form names.
Build a token route with `Route::token`, so that a test states which scope it expects.

The server answers 404 for a route that it does not have, as a server older than that route does.
A test of a new request must also pass against a server with no route for it.

The diagnostic report reads `~/.claude` to say whether `link` connected Claude. In a test it reads
no directory outside the root, unless the test names one with `diagnostics::look_in_this_test`. A
removal order removes what `link` wrote from the same directory, so a test that obeys an order
names that directory first, with the same function.

A test signs a removal order with a key that `Signer::trust_for_removal` makes the client trust.
`Signer::trust` covers the software manifest and the content release, and no removal order.

## Documents

`README.md` and `docs/*.md` are maintained with a tool named docsgen. `docs/.docsgen.json` records a
hash for each document and for each source file that it describes. `docs/FLOW.md` cites source
lines, so a change that moves code makes a citation wrong.

When you change behaviour, change the text of the document that describes it. Leave
`docs/.docsgen.json` alone. Say in the pull request which documents need a docsgen run.

The documents use Simplified Technical English: short sentences, active voice, one meaning per word.

## Commits and releases

A commit message has a conventional-commit subject, such as `fix(link): ...`. Body lines follow the
subject. Each body line starts with ` - ` (a space, a hyphen, a space). The commits of this
repository put the body lines directly under the subject, with no blank line between them. This is
a real message from `git log`:

```text
chore(release): raise the version to 0.1.6
 - Set the crate version to 0.1.6 in Cargo.toml and Cargo.lock
```

A release is a version change in `Cargo.toml` and `Cargo.lock`, then a tag `v<version>`. See
"Release steps" in [`README.md`](README.md). Never push a tag without an instruction to do so.
