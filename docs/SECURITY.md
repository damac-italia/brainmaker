# Security

`brainmaker` runs as an unprivileged user, downloads content and binaries from one HTTP host, and
writes them into the user's home directory. This document states the trust model, the controls that
exist, and how to report a vulnerability.

## Trust model

| Party | Trusted for |
|---|---|
| The holder of the manifest signing key | Which binary `brainmaker` installs over itself |
| The API host, over TLS | The content hash and the content archive |
| The administrator who issues the provisioning file | The endpoint and the token |
| The employee who runs the binary | Nothing beyond their own account; they already hold the binary |

The two paths have different anchors.

**The software path** is anchored on an Ed25519 signature. The manifest carries a signature over
its own bytes, and `brainmaker` checks it against a public key compiled into the binary before it
parses anything. The API host, the CDN in front of it, and the storage bucket behind it are
therefore not trusted for the software path: a manifest they alter fails the check. The SHA-256
inside the signed manifest then carries that trust to the binary, because an attacker who cannot
forge the manifest cannot choose the checksum either.

A replayed older manifest installs nothing, because `version::is_newer` requires a strictly greater
numeric core.

**The content path** is anchored on TLS to the base URL host. The content archive is replaced
wholesale on every update, and it is data rather than code.

### What the sealed store protects, and what it does not

The file key is derived with HKDF-SHA256 from two parts: a secret compiled into the binary at build
time, and an identifier of the machine. That combination covers these cases:

- A copy of `config.enc` taken from a backup, a cloud-sync folder, or a stolen disk does not decrypt
  on another machine, even with the binary.
- A reader who holds the file but not the binary learns nothing.
- A `grep` over the home directory finds no URL and no token.

**It hides nothing from the employee who runs the binary.** They hold the binary, so they hold the
compiled-in secret, and they run on the bound machine. Anyone who receives the distribution zip can
recover the endpoint and the token. Treat both as known to every employee you ship to, issue one
token per person where you can, and revoke the token on the server when someone leaves.

## Trust boundaries

| Boundary | Untrusted input | Control |
|---|---|---|
| Content API to disk | The hash string | Exactly 8 ASCII alphanumeric characters, checked before it enters a URL or a path |
| Content API to disk | The zip archive | Path containment, symbolic-link rejection, permission stripping, size caps |
| Software API to the binary | The manifest | Ed25519 signature over the served bytes, checked before the parse; then version character set and length, platform key lookup, origin check, checksum format |
| Software API to the binary | The replacement binary | SHA-256 match, then a `--version` run before the swap |
| Provisioning file to the store | `KEY=VALUE` text | Size cap, key character set, TLS rule on the base URL |
| Store to the process | `config.enc` | AES-256-GCM authenticates the header and the ciphertext before any byte is used |

## Authentication and authorization

`brainmaker` sends `Authorization: Bearer <token>` on every request when a token is configured, and
sends no header when none is. It performs no authorization of its own: the server decides what a
token may read.

Because the token travels on every request, `url::check_base_url` requires the base URL to use
`https://`. It accepts `http://` only when the host is `localhost` or a loopback address, where the
request never reaches the network. The rule applies to every source of the base URL: the
provisioning file, `SWETSI_API_BASE`, and `--url`. Userinfo does not change the decision, so
`http://localhost@evil.example/` is refused.

The token is never printed. `status` reports it as `absent` or as `present, N characters`. The unit
test `config::tests::the_token_summary_never_shows_the_token` enforces that.

HTTP 401 and HTTP 403 produce a message that names the token variable and nothing else.

## Secret handling

| Secret | Where it lives | Protection |
|---|---|---|
| `SWETSI_TOKEN` | `~/.brainmaker/confidential/config.enc` | AES-256-GCM, mode `0600` in a `0700` directory |
| `SWETSI_API_BASE` | the same file | the same |
| `BRAINMAKER_CONFIG_KEY` | a repository secret, read at build time | Never in the repository. `build.rs` declares `cargo:rerun-if-env-changed`, so a cached build cannot ship a stale key. |
| `BRAINMAKER_SIGNING_KEY` | a repository secret, read at release time | Never in the repository, and never on a machine that serves the API. The signing step reads it from the environment, so it never reaches the runner's disk. |

The manifest signing key's public half is not a secret. It lives in `PUBLIC_KEYS` in
[`src/signature.rs`](../src/signature.rs) and is committed. `brainmaker status` prints how many keys
a binary trusts on its `signing` line.

To rotate the signing key, put the new public key first in `PUBLIC_KEYS`, keep the old one, and
release. Remove the old key only once every client carries a binary that holds the new one. A
manifest verifies when any listed key accepts it.

The store is written through a temporary file and a rename, so a crash never leaves a partial file.
The temporary file gets mode `0600` before the rename.

After a successful import, `brainmaker` deletes the plain provisioning file. `--keep-config` skips
the deletion and prints a warning on every run, because the file still holds the token. A deletion
that fails also prints a warning telling you to delete the file yourself.

Both names that `brainmaker` searches for carry the word `brainmaker`: `brainmaker.env` and
`.brainmaker.env`. It never reads a bare `.env`, so running it inside an unrelated project cannot
import and then delete that project's file.

A build that leaves `BRAINMAKER_CONFIG_KEY` unset falls back to a published development key.
`brainmaker status` reports which one a binary carries on its `key` line: `release` or
`development`. The release workflow fails before it builds anything when the repository secret is
absent.

Rotating `BRAINMAKER_CONFIG_KEY` makes every existing stored configuration unreadable. Every
employee then has to import a fresh provisioning file.

The binary itself discloses no endpoint. Two unit tests enforce that: one reads `src/config.rs` at
compile time and fails when a scheme is followed by a host character outside the test module, and
one asserts that the help text holds no `http://` or `https://`.

## Input validation

### Content hash

Exactly 8 ASCII alphanumeric characters, checked in `config::validate_hash` before the value enters
either a URL or a file name. That excludes `/`, `.`, `..`, `%`, and `?`.

### Archive extraction

The archive arrives over the network, so `archive::extract` applies five controls:

1. It rejects any entry whose path escapes the destination, which covers `..` segments and absolute
   paths.
2. It rejects symbolic links, which can point outside the destination after the extraction.
3. It discards the setuid bit, the setgid bit, the sticky bit, the group-write bit, and the
   other-write bit from every extracted file, and always keeps the owner able to read and write.
4. It stops one entry at 256 MiB.
5. It stops one archive at 1 GiB after expansion, which bounds a zip bomb.

Rejected entries are counted and reported as `Skipped N unsafe archive entries.` The extraction
continues with the entries that pass.

### Self-update

`self-update` replaces the running executable, so it applies six controls in this order:

1. The manifest must carry an Ed25519 signature from a key in `PUBLIC_KEYS`. The check runs over
   the served bytes, before any parse, so an untrusted manifest never reaches the version logic or
   the platform lookup. A build whose key list is empty refuses every manifest.
2. The `url` in the manifest must carry the same scheme, host, and port as the base URL. Comparison
   is case-insensitive, discards any userinfo, and fills in the scheme's default port, so
   `https://api.example.test@evil.example/` is refused and `https://api.example.test:443/` is
   accepted against `https://api.example.test/`. This also blocks a downgrade from `https` to
   `http`.
3. The install directory must be writable, checked with a probe file before any download.
4. The download must match the `sha256` in the manifest, which must itself be 64 hexadecimal
   characters.
5. The staged binary must run and must report the manifest's version. This catches a build for the
   wrong architecture and a manifest that points at the wrong file.
6. Only then does the swap run, through two renames. A failure on the second rename restores the
   previous binary.

### Size caps

| Input | Cap |
|---|---|
| Provisioning file | 64 KiB |
| Content manifest and software manifest | 1 MiB |
| Content archive | 512 MiB |
| One archive entry | 256 MiB |
| One archive after expansion | 1 GiB |
| Replacement binary | 128 MiB |

Every cap reads one byte past the limit, so an oversized body fails rather than being silently
truncated. The caps are constants in [`src/config.rs`](../src/config.rs).

## Dependency policy

| Crate | Version | Role |
|---|---|---|
| `anyhow` | 1.0.104 | Error context |
| `dirs` | 6.0.0 | Home directory lookup |
| `ring` | 0.17 | AES-256-GCM and HKDF-SHA256 |
| `serde`, `serde_json` | 1.0.229, 1.0.151 | Manifest and state parsing |
| `sha2` | 0.11.0 | Download checksum |
| `ureq` | 3.3.0 | HTTP client |
| `zip` | 8.6.0 | Archive extraction, `deflate` only, default features off |

The release workflow runs `cargo build --release --locked`, so a release builds only from the
committed `Cargo.lock`. Adding a dependency therefore requires a lock-file commit and a review.

## Reporting a vulnerability

Report privately through GitHub's private vulnerability reporting, on the Security tab of
[damac-italia/brainmaker](https://github.com/damac-italia/brainmaker/security/advisories/new). Do
not open a public issue.

Include the version from `brainmaker --version`, the platform key and signing-key count from
`brainmaker status`, and the steps to reproduce.

<!-- docsgen: unverified — private vulnerability reporting must be switched on in the repository's
     Settings > Code security before the link above accepts a report. -->
