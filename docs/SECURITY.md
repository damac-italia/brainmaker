# Security

`brainmaker` runs as an unprivileged user, downloads content and binaries from one HTTP host, and
writes them into the user's home directory. This document states the trust model, the controls that
exist, and how to report a vulnerability.

## Trust model

| Party | Trusted for |
|---|---|
| The holder of the manifest signing key | Which binary `brainmaker` installs over itself |
| The API host, over TLS | The content hash and the content archive |
| The administrator who issues the provisioning file | The endpoints and the client credentials |
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
- A `grep` over the home directory finds no URL and no credential.

**It hides nothing from the employee who runs the binary.** They hold the binary, so they hold the
compiled-in secret, and they run on the bound machine. Anyone who receives the distribution zip can
recover the endpoints and the client credentials. Treat all four as known to every employee who
receives the distribution. Issue one client identifier per person where that is possible, and revoke
that client on the server when someone leaves. Revoking the client stops the next token request; a
token already issued stays valid for the rest of its 10 minutes.

## Trust boundaries

| Boundary | Untrusted input | Control |
|---|---|---|
| Content API to disk | The hash string | Exactly 8 ASCII alphanumeric characters, checked before it enters a URL or a path |
| Content API to disk | The zip archive | Path containment, symbolic-link rejection, permission stripping, size caps |
| Software API to the binary | The manifest | Ed25519 signature over the served bytes, checked before the parse; then version character set and length, platform key lookup, checksum format. The manifest names no URL, so it cannot direct a download. |
| Software API to the binary | The replacement binary | SHA-256 match, then a `--version` run before the swap |
| Provisioning file to the store | `KEY=VALUE` text | Size cap, key character set, TLS rule on both URLs, all-or-none credential check, character rule on both credential values, route rule on all five routes |
| Token endpoint to a header | The access token | Length cap, printable-ASCII rule, and a `token_type` that must read `Bearer` |
| Store to the process | `config.enc` | AES-256-GCM authenticates the header and the ciphertext before any byte is used |

## Authentication and authorization

`brainmaker` holds a client identifier and a client secret, and exchanges them for an access token
at `POST {SWETSI_JWT_ENDPOINT}/oauth2/token`, with HTTP Basic and the `client_credentials` grant. It
asks for the scope `sync` explicitly, so a client that the server grants more than one scope still
requests the one scope a sync needs. It then sends `Authorization: Bearer <token>` on every request
under the base URL. With no credential configured it sends no header. It performs no authorization
of its own: the server decides what the token may read.

The server expires the token after 10 minutes. That bounds what a token taken from a laptop is
worth: the client secret stays valuable, and it stays sealed. The token lives in memory for one run
and never reaches the disk, so nothing on the disk holds a usable bearer token between runs.
`brainmaker` stops using a token 30 seconds before it expires, so a request that starts near the
boundary does not arrive with an expired token.

Because a credential travels on every request, `url::check_base_url` requires `https://`. It accepts
`http://` only when the host is `localhost` or a loopback address, where the request never reaches
the network. The rule applies to every source of both URLs: the provisioning file,
`BRAINMAKER_API_BASE`, `SWETSI_JWT_ENDPOINT`, and `--url`. Userinfo does not change the decision, so
`http://localhost@evil.example/` is refused.

The access token arrives from the network and goes into a header, so `auth::check_token` refuses a
token that is empty, longer than 8192 bytes, or holds a character outside printable ASCII. A
carriage return inside a token would otherwise let the server write further request headers. The
client identifier and the client secret face the same character rule in `Credentials::new`, which
also refuses an identifier that holds a colon, because HTTP Basic separates the two values with one.

No credential is printed. `status` reports the credentials as `absent`, or as their scope and their
two lengths. The unit tests `config::tests::the_credentials_summary_never_shows_the_secret`,
`config::tests::the_credentials_debug_output_never_shows_the_secret`, and
`auth::tests::the_cache_debug_output_never_shows_the_token` enforce that.

HTTP 401 and HTTP 403 produce a message that names the credential variables and nothing else.

## Endpoint confidentiality

Nothing in this repository, in the built binary, or in a published release names the API host or
any route of a deployment.

| Place | Why it holds no endpoint |
|---|---|
| The binary | Only `BRAINMAKER_CONFIG_KEY` and `CARGO_PKG_VERSION` are read at compile time. Two unit tests, `config::tests::no_endpoint_is_compiled_into_this_module` and `auth::tests::no_endpoint_is_compiled_into_this_module`, fail the build if a URL with a host enters either module. `cli::tests::the_help_text_names_no_endpoint` does the same for the help text. |
| The repository | Every document uses `api.example.test`. The five route keys let a deployment replace every default route name, so even the route layout need not appear here. |
| The workflow logs | The release workflow takes no URL as input. It builds, checksums, and signs. |
| The release notes and assets | The manifest carries a version and one SHA-256 per platform. `selfupdate::tests::the_manifest_type_carries_no_url` fails the build if a `url` field returns to the manifest type. |

The base URL, the OAuth2 endpoint, and any custom routes reach a machine only in the provisioning
file, and that file is sealed on first use and then deleted. A public release therefore discloses
which versions exist, and nothing about where they are served.

The five routes are checked before use. A route that holds `://` would move a request to another
host, and a `..` segment would climb out of the base path; `config::check_route` refuses both, along
with a space or any other character that cannot go into a URL.

## Secret handling

| Secret | Where it lives | Protection |
|---|---|---|
| `SWETSI_CLIENT_SECRET` | `~/.brainmaker/confidential/config.enc` | AES-256-GCM, mode `0600` in a `0700` directory |
| `SWETSI_CLIENT_ID` | the same file | the same |
| `BRAINMAKER_API_BASE`, `SWETSI_JWT_ENDPOINT` | the same file | the same |
| The access token | process memory only | Never written to disk. It expires 10 minutes after the server issues it. |
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
the deletion and prints a warning on every run, because the file still holds the client secret. A deletion
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

`self-update` replaces the running executable, so it applies five controls in this order:

1. The manifest must carry an Ed25519 signature from a key in `PUBLIC_KEYS`. The check runs over
   the served bytes, before any parse, so an untrusted manifest never reaches the version logic or
   the platform lookup. A build whose key list is empty refuses every manifest.
2. The install directory must be writable, checked with a probe file before any download.
3. The download must match the `sha256` in the manifest, which must itself be 64 hexadecimal
   characters.
4. The staged binary must run and must report the manifest's version. This catches a build for the
   wrong architecture and a manifest that points at the wrong file.
5. Only then does the swap run, through two renames. A failure on the second rename restores the
   previous binary.

The download address is not a control that can fail: `Config::binary_url` derives it from the base
URL in the provisioning file, so a manifest cannot name another host at all.

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

| Crate | Requirement | Role |
|---|---|---|
| `anyhow` | 1.0.104 | Error context |
| `dirs` | 6.0.0 | Home directory lookup |
| `ring` | 0.17 | AES-256-GCM, HKDF-SHA256, and Ed25519 verification |
| `serde`, `serde_json` | 1.0.229, 1.0.151 | Manifest and state parsing |
| `sha2` | 0.11.0 | Download checksum |
| `ureq` | 3.3.0 | HTTP client |
| `zip` | 8.6.0 | Archive extraction, `deflate` only, default features off |

The requirement column is the one in `Cargo.toml`. The release workflow runs
`cargo build --release --locked`, so a release builds the versions in the committed `Cargo.lock`,
which a Dependabot bump can raise inside a requirement without changing it. Adding a dependency
therefore requires a lock-file commit and a review.

## Reporting a vulnerability

Report privately through GitHub's private vulnerability reporting, on the Security tab of
[damac-italia/brainmaker](https://github.com/damac-italia/brainmaker/security/advisories/new). Do
not open a public issue.

Include the version from `brainmaker --version`, the platform key and signing-key count from
`brainmaker status`, and the steps to reproduce.

<!-- docsgen: unverified — private vulnerability reporting must be switched on in the repository's
     Settings > Code security before the link above accepts a report. -->
