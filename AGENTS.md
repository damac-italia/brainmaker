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

- `signing.key` and `content-signing.key`, in the repository root, are private signing keys.
- `brainmaker.env`, `.brainmaker.env`, and any `*.env` file hold a client secret.

`.gitignore` excludes all of these files. It excludes `dist/` too.

## Rules of the code

- In `src/`, only `src/main.rs` prints to standard output. Every other module returns an
  `anyhow::Result`, or reports through the `log` closure that it receives.
- No URL with a host may enter `src/config.rs`, `src/auth.rs`, or the help text in `src/cli.rs`. A
  test fails when one does. A document that names the host of the content API uses
  `api.example.test`.
- Never print, log, or format a client identifier, a client secret, or an access token into text
  that a person or a log can read.
- Check a value from the network before it enters a URL, a path, or a header.
- `PUBLIC_KEYS` and `CONTENT_KEYS` in `src/signature.rs` keep one key per line. No key may be in
  both lists. The release workflow reads each list with `sed` and `grep`.
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
