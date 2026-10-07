# Security

`brainmaker` runs as an unprivileged user, downloads content and binaries from one HTTP host, and
writes them into the user's home directory. This document states the trust model, the controls that
exist, and how to report a vulnerability.

## Trust model

| Party | Trusted for |
|---|---|
| The holder of the manifest signing key | Which binary `brainmaker` installs over itself |
| The holder of the content signing key | Which archive lands in `content/`, and so what runs at every session start |
| The holder of the removal signing key | Which client removes `brainmaker` from its machine, and whether the outbox there goes too. Nothing else: the key signs no content and no software. |
| The administrator who issues the provisioning file | The endpoints and the client credentials |
| The employee who runs the binary | Nothing beyond their own account; they already hold the binary |
| The server | Which operator a credential belongs to, and so the name on each note. The client never names it. The server also receives and stores the diagnostic report. It is not trusted to ask for one: no answer of the server makes the client send, read, or run anything. The server also carries a removal order to its client. It is not trusted to write one: an order that no removal key signed, or that names another client, is refused. The server can keep an order from a client, as it can keep a content release from one. |
| The admin's copy | Collecting the notes, reading what each client reports about itself, and storing a removal order, which retires its client on the server. It is an install like every other, in `~/.brainmaker`, and its issuer client holds `sync` and `outbox:read`, never `publish`. `link` connects nothing to Claude for a credential that reads the outbox and cannot send notes. The admin commands grant nothing by themselves: the server decides what the token may read. |

The two paths have different anchors.

**The software path** is anchored on an Ed25519 signature. The manifest carries a signature over
its own bytes, and `brainmaker` checks it against a public key compiled into the binary before it
parses anything. The API host, the CDN in front of it, and the storage bucket behind it are
therefore not trusted for the software path: a manifest they alter fails the check. The SHA-256
inside the signed manifest then carries that trust to the binary, because an attacker who cannot
forge the manifest cannot choose the checksum either.

A replayed older manifest installs nothing, because `version::is_newer` requires a strictly greater
numeric core.

**The content path** is anchored the same way, on a separate key. `content/latest` returns an
envelope whose `payload` carries the hash, the size, the SHA-256, and a sequence, and
`signature::CONTENT_KEYS` verifies it before anything is parsed. The API host is therefore not
trusted for either path.

**The removal path** is the third anchor. A removal order is an envelope like the other two, and
`signature::REMOVAL_KEYS` verifies it before anything is parsed. The API host is not trusted for
it either: it carries the order, and it cannot write one. [The removal order](#the-removal-order)
gives the checks.

The three key lists are disjoint, and a unit test fails the build if a key appears in two.

A signature proves who made a release, and not when, so the payload also carries a sequence.
`brainmaker-sign` writes the time of signing, and the client refuses a release whose sequence is not
higher than the installed one. A replayed older release therefore installs nothing, as a replayed
older manifest installs nothing.

No anchor is TLS. TLS still runs, and it protects the credential in transit, but a host that
serves altered bytes is caught by the signature rather than by the transport.

### What the sealed store protects, and what it does not

The file key is derived with HKDF-SHA256 from two parts: a secret compiled into the binary at build
time, and an identifier of the machine. That combination covers these cases:

- A copy of `config.enc` taken from a backup, a cloud-sync folder, or a stolen disk does not decrypt
  on another machine, even with the binary.
- A reader who holds the file but not the binary learns nothing.
- A `grep` over the home directory finds no URL and no credential.

The identifier is the platform UUID on macOS, read through `/usr/sbin/ioreg`, `/etc/machine-id` on
Linux (or `/var/lib/dbus/machine-id` when the first file is missing), and `MachineGuid` on Windows,
read through `reg.exe` under `%SystemRoot%\System32`. Both tools are called by their full path, so
a program of the same name earlier on `PATH` never answers. When the
system gives none, which is usual in a container, the key uses the home directory path instead, and
the first bullet point does not hold: the copy opens on any machine with the same binary and the
same account name. When the home directory is unknown too, the key uses a constant, and the copy
opens on any machine with the same binary. `brainmaker status` prints the source on its `binding`
line, as `machine identifier`, `home directory path (weak)`, or `none (weak)`, and an import on a
system with a weak binding prints a warning.

**It hides nothing from the employee who runs the binary.** They hold the binary, so they hold the
compiled-in secret, and they run on the bound machine. Anyone who receives the distribution zip can
recover the endpoints and the client credentials. Treat all four as known to every employee who
receives the distribution. Issue one client identifier per person where that is possible, and revoke
that client on the server when someone leaves. Revoking the client stops the next token request; a
token already issued stays valid for the rest of its lifetime, which the server sets to 10 minutes.

## Trust boundaries

| Boundary | Untrusted input | Control |
|---|---|---|
| Content API to disk | The content release | Ed25519 signature over the served bytes, checked before the parse; the hash, the size, the SHA-256, and the sequence are then taken from the signed payload, and the sequence must be higher than the installed one unless `--force` is given |
| Content API to disk | The hash string | Exactly 8 ASCII alphanumeric characters, checked before it enters a URL or a path |
| Content API to disk | The zip archive | Path containment, symbolic-link rejection, permission stripping, size caps |
| Software API to the binary | The manifest | Ed25519 signature over the served bytes, checked before the parse; then version character set and length, platform key lookup, checksum format. The manifest names no URL, so it cannot direct a download. |
| Software API to the binary | The replacement binary | SHA-256 match, then a `--version` run whose output must be exactly `brainmaker <version>`, before the swap |
| Provisioning file to the store | `KEY=VALUE` text | Size cap, key character set, TLS rule on both URLs, all-or-none credential check, character rule on both credential values, route rule on all ten routes. All run before the store is written, so a file that fails leaves the previous store and the file in place. |
| Disk to the network | The notes in `outbox/` | A regular file only, read with `symlink_metadata`, never through a symbolic link, and the opened file must be the file that was checked; the note-name rule; 64 KiB at most; UTF-8 with no NUL and no byte-order mark; the frontmatter rules; 60 seconds with no change; one run at a time under `.outbox.lock`. A file that fails moves to `rejected/`, and nothing of its name or of its text is sent. The diagnostic report counts it, and names the rule that it broke as one word. |
| Disk to the network | The run log and the state of the machine, in the diagnostic report | The log must be a regular file and no symbolic link. Each line is parsed into a typed event, and a line of another shape, or of more than 512 bytes, is skipped. Every word must be in its fixed list, and every number, content hash, version, and time must pass its rule. The report is written from the values that passed, never from the bytes of a file. At most 200 lines and 64 KiB for each report, one report in 30 minutes, one run at a time under `.diagnostics.lock`. |
| Content API to disk | The operator name from `whoami` | Written only when it matches `^[a-z0-9][a-z0-9-]{0,31}$`, as the name and one line feed |
| Content API to disk | The answer to a note | The month for `sent/` must read `YYYY-MM`, or the note goes to `sent/unknown/`. The reason for `rejected/` loses every control character. |
| Server text to the terminal | An error message | Collapsed onto one line, cut at 200 characters, and stripped of every control character and every character that changes the direction of the text |
| Content API to disk | A note that `admin pull-outbox` writes | The operator, the client ID, the note name, `received_at`, the kind, the domain, and each flag are checked again, and the text must match its size and its SHA-256. The file name is built from checked values alone. A hard link from a flushed temporary file never replaces a file. `author` and `review_flags` are always written from the server's values. |
| Content API to the terminal and the dashboard | The fleet view and the sync log | Typed parse that refuses a field it does not know; the client ID rule, the operator rule, and the time shape; control characters removed from every value the lines print |
| Content API to the terminal and the dashboard | The diagnostics of a client, for `admin diagnose` | Typed parse that refuses a field or a word it does not know; the client ID must be the one that was asked for; the time shape, the version rule, and the content hash rule; control characters removed from every value the lines print |
| Content API to the client | The answer to a diagnostic report | The client reads the status and no field of the body. Nothing in the answer changes what the client sends, when it sends, or what it runs. |
| Content API to the whole install | A removal order, in the body of a `410` answer to the content check | Ed25519 signature over the served bytes against `REMOVAL_KEYS`, checked before the parse; then a typed parse that refuses a field it does not know, the one order word `remove`, a time of signing between 2000 and 2099, and the signed client ID against the identifier of this client. An order that fails any check removes nothing, and the installed content stays. |
| Content API to the client | The answer to a removal report | The client reads nothing of it, the status included |
| Disk to the signer | The removal key file that `admin retire` reads | Hexadecimal, then a PKCS#8 Ed25519 parse. A probe that the key signs must verify against `REMOVAL_KEYS`, or the run stops before it stores anything. The key is never printed, logged, or sent. |
| Content API to the terminal | The answer to a stored removal order | The same typed parse as for the removal that `admin diagnose` reads, and the order must be there, for the client that was asked for |
| Content API to the terminal and the dashboard | The removal of a client, for `admin diagnose` | Typed parse that refuses a field or a word it does not know; the client ID must be the one that was asked for; the time shape |
| Token endpoint to a header | The access token | Length cap, printable-ASCII rule, and a `token_type` that must read `Bearer` when the response carries one |
| Token endpoint to the clock | `expires_in` | Cut to one hour before it is added to a clock reading; a sum that the clock cannot hold caches nothing |
| Store to the process | `config.enc` | AES-256-GCM authenticates the header and the ciphertext before any byte is used |

## Authentication and authorization

`brainmaker` holds a client identifier and a client secret, and exchanges them for an access token
at `POST {SWETSI_JWT_ENDPOINT}/oauth2/token`, with HTTP Basic and the `client_credentials` grant. It
names one scope in each request, so a client that the server grants more than one scope still
requests only the scope that the next request needs: `sync` for every read and for the diagnostic
report, and `outbox:write` to send a note, asked for only when a note is ready, and `outbox:read`
for the admin commands. It keeps
one token per scope for the run, so a sync token never carries the right to write. It then sends
`Authorization: Bearer <token>` on every request under the base URL. With no credential configured
it sends no header. It performs no authorization of its own: the server decides what the token may
read and write.

An issuer that does not grant `outbox:write` answers `invalid_scope`. The push then stops with a
notice, the notes stay, and the sync is not affected.

`link` asks for `outbox:read` and, when the issuer grants it, for `outbox:write`, once, to learn the
role. It uses neither token for a request. A credential that reads the outbox and cannot send notes
is the admin's, and `link` connects nothing to Claude for it: see
[Content is code, once it is linked](#content-is-code-once-it-is-linked). The role check is a
convenience for the install and grants nothing: the server still checks every token.

The server expires the token after 10 minutes. That bounds what a token taken from a laptop is
worth: the client secret stays valuable, and it stays sealed. The token lives in memory for one run
and never reaches the disk, so nothing on the disk holds a usable bearer token between runs.
`brainmaker` stops using a token 30 seconds before it expires, so a request that starts near the
boundary does not arrive with an expired token. The client uses a token for one hour at most,
whatever `expires_in` says, and it adds that time to the clock with a checked sum, so a large value
from the server cannot stop the process.

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

No credential is printed. `status` reports the credentials as `absent`, or as their scopes and their
two lengths. The admin commands print the client IDs that the server reports, which is the one
exception to the rule against printing a client identifier: the admin needs them to tell two
machines of one operator apart, and `admin syncs --client` takes one. A client ID alone
authenticates nothing, and the convention `brainmaker-sync-<user>` makes it easy to guess. No
command prints the client identifier or the secret of the machine it runs on. The unit tests `config::tests::the_credentials_summary_never_shows_the_secret`,
`config::tests::the_credentials_debug_output_never_shows_the_secret`, and
`auth::tests::the_cache_debug_output_never_shows_the_token` enforce that.

The diagnostic report and the run log hold no credential either. No field of them holds text, so
nothing can carry one: see [The diagnostic report](#the-diagnostic-report). The test
`diagnostics::tests::a_report_holds_no_credential_no_path_and_nothing_of_a_note` enforces that.

HTTP 401 and HTTP 403 produce a message that names the credential variables and nothing else.

## Endpoint confidentiality

Nothing in this repository, in the built binary, or in a published release names the API host or
any route of a deployment.

| Place | Why it holds no endpoint |
|---|---|
| The binary | Only `BRAINMAKER_CONFIG_KEY` and `CARGO_PKG_VERSION` are read at compile time. Two unit tests, `config::tests::no_endpoint_is_compiled_into_this_module` and `auth::tests::no_endpoint_is_compiled_into_this_module`, fail the build if a URL with a host enters either module. `cli::tests::the_help_text_names_no_endpoint` does the same for the help text. |
| The repository | Every document uses `api.example.test`. The ten route keys let a deployment replace every default route name, so even the route layout need not appear here. |
| The workflow logs | The release workflow takes no URL as input. It builds, checksums, and signs. |
| The release notes and assets | The manifest carries a version and one SHA-256 per platform. `selfupdate::tests::the_manifest_type_carries_no_url` fails the build if a `url` field returns to the manifest type. GitHub writes the notes from merged pull request titles, so a title must name no endpoint. |

The base URL, the OAuth2 endpoint, and any custom routes reach a machine only in the provisioning
file, and that file is sealed on first use and then deleted. A public release therefore discloses
which versions exist, and nothing about where they are served.

The ten routes are checked before use. A route that holds `://` would move a request to another
host, and a `..` segment would climb out of the base path; `config::check_route` refuses both, along
with a space or any other character that cannot go into a URL.

## Secret handling

| Secret | Where it lives | Protection |
|---|---|---|
| `SWETSI_CLIENT_SECRET` | `~/.brainmaker/confidential/config.enc` | AES-256-GCM, mode `0600` in a `0700` directory |
| `SWETSI_CLIENT_ID` | the same file | the same |
| `BRAINMAKER_API_BASE`, `SWETSI_JWT_ENDPOINT` | the same file | the same |
| The access token | process memory only | Never written to disk. The server expires it 10 minutes after it issues it, and the client uses it for one hour at most. |
| `BRAINMAKER_CONFIG_KEY` | a repository secret, read at build time | Never in the repository. `build.rs` declares `cargo:rerun-if-env-changed`, so a cached build cannot ship a stale key. The dependencies build first with no secret set. |
| `BRAINMAKER_SIGNING_KEY` | a repository secret, read at release time | Never in the repository, and never on a machine that serves the API. Only a run for a `v*` tag reads it. The signing step reads it from the environment, so it never reaches the runner's disk. |

The manifest signing key's public half is not a secret. It lives in `PUBLIC_KEYS` in
[`src/signature.rs`](../src/signature.rs) and is committed. `brainmaker status` prints how many keys
a binary trusts on its `signing` line.

To rotate the signing key, put the new public key first in `PUBLIC_KEYS`, keep the old one, and
release. Remove the old key only once every client carries a binary that holds the new one. A
manifest verifies when any listed key accepts it.

`brainmaker-sign keygen` creates the key file and sets its mode in one call, so on Unix the file
is `0600` from the moment it exists. It refuses a path where anything already exists, a symbolic
link included, so it never writes a key through a link to another place.

### Release workflow

The release workflow applies these controls:

| Control | Effect |
|---|---|
| Token scope | The workflow token can read the repository and nothing more. The manifest job alone may write, because it creates the release. |
| Pinned actions | Every action in every workflow is pinned by commit SHA. Dependabot raises the updates. |
| Checkout credentials | `persist-credentials: false` on both checkouts, so the token does not stay on the runner's disk |
| Split build | The dependencies build with no secret set. A second build then compiles the `brainmaker` crate alone with `BRAINMAKER_CONFIG_KEY` set, so no dependency build script sees the key. |
| Signing tool | `brainmaker-sign` builds in a step with no secret set. The signing step then runs the finished binary. |
| Tag gate | Only a run for a `v*` tag checks the signing key, signs the manifest, verifies it against `PUBLIC_KEYS` alone, and creates the release. A run started by hand signs nothing. |
| Toolchain | The build and manifest jobs set up the toolchain with the pinned action that the `test` workflow uses. |

The store is written through a temporary file and a rename, so a crash never leaves a partial file.
The temporary file gets mode `0600` before the rename.

After a successful import, `brainmaker` deletes the plain provisioning file. `--keep-config` skips
the deletion and prints a warning on every run, because the file still holds the client secret. A deletion
that fails also prints a warning telling you to delete the file yourself.

Both names that `brainmaker` searches for carry the word `brainmaker`: `brainmaker.env` and
`.brainmaker.env`. It never reads a bare `.env`, so running it inside an unrelated project cannot
import and then delete that project's file.

Once `config.enc` exists, the working directory is not searched at all. The `SessionStart` hook
runs `sync` inside whatever project the user has open, so without that rule a `brainmaker.env`
committed to any repository would overwrite the sealed settings, and be deleted, the first time a
session started there. `--config`, `$BRAINMAKER_CONFIG`, and a file beside the binary still work,
so an administrator can still reissue endpoints.

A build that leaves `BRAINMAKER_CONFIG_KEY` unset falls back to a published development key.
`brainmaker status` reports which one a binary carries on its `key` line: `release` or
`development`. The release workflow fails before it builds anything when the repository secret is
absent.

Rotating `BRAINMAKER_CONFIG_KEY` makes every existing stored configuration unreadable. Every
employee then has to import a fresh provisioning file.

The binary itself discloses no endpoint. Two unit tests enforce that: one reads `src/config.rs` at
compile time and fails when a scheme is followed by a host character outside the test module, and
one asserts that the help text holds no `http://` or `https://`.

### Removal

`brainmaker uninstall` deletes `confidential/config.enc` and its temporary file, together with the
rest of the install. It reads no settings to do so, and it never imports a provisioning file.
Deleting the store does not revoke the client on the server: the client identifier and secret stay
valid until an administrator revokes them, so revoke them when a machine leaves service.

The removal stays inside what `brainmaker` wrote. It acts only on a root that holds a parsable
`state.json` or a `config.enc` that starts with the sealed header, and otherwise changes nothing,
not even the hook in `~/.claude`. It removes named paths rather than the whole root. A symbolic link
that it removes goes without the target it names.

A removal that the administrator orders runs the same code, with the same limits. See
[The removal order](#the-removal-order). To take a person off the team, do the three steps in this
order:

1. Run `brainmaker admin retire <operator>` on the admin's machine. It signs a removal order for
   each client of that person, and stores it. From then on the server gives those clients no
   content.
2. Wait until the client reports the removal. `brainmaker admin diagnose <operator>` shows it.
3. Revoke the client at the issuer.

A client that the issuer refuses gets no token, asks the server for nothing, and so never receives
the order. Its content then stays on the machine, as it does today.

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
4. The staged binary must run, and its `--version` output must be exactly `brainmaker <version>`
   for the manifest's version. A substring match would accept `0.1.10` for `0.1.1`. This catches a
   build for the wrong architecture and a manifest that points at the wrong file.
5. Only then does the swap run, through two renames. A failure on the second rename restores the
   previous binary.

A second lock, on `.update.lock` under the root, keeps two updates apart, and the run that holds it
removes the staged files that a killed update left beside the program.

A verified binary then replaces `<root>/bin/brainmaker` as well, when that copy exists and is a
different file from the one that ran. The `SessionStart` hook names that copy, so an update that
skipped it would leave every session running the old version, including one with a fixed
vulnerability outstanding. The copy is written under a temporary name beside the target and
renamed over it, so a failure part-way leaves the previous copy intact.

The download address is not a control that can fail: `Config::binary_url` derives it from the base
URL in the provisioning file, so a manifest cannot name another host at all.

On macOS, `link` installs a LaunchAgent that runs `self-update` every hour with no person present.
A build signed by a key in `PUBLIC_KEYS` therefore reaches every linked Mac within an hour of its
promotion. The five controls above apply unchanged, and the agent passes no `--force`, so it never
reinstalls or downgrades. The agent runs as the user, in the user's GUI session, and writes only
under the root and its own property list. `unlink` and `uninstall` remove it.

### Content install

`sync` applies the same shape of control the software path uses, in this order:

1. The content release must carry an Ed25519 signature from a key in `CONTENT_KEYS`. The check runs
   over the served bytes, before any parse. A build whose content list is empty refuses every
   release.
2. The hash comes from the signed payload, never from the unsigned `hash` beside it, so a server
   cannot name one archive while signing another.
3. The sequence in the signed payload must be higher than the installed one, unless `--force` is
   given. The rule applies when the hash differs from the installed hash. A release with no
   sequence is refused once the installed release has one. The check runs again after the install
   lock is taken, on the state as it stands then. A reinstall of the installed hash, such as one
   after `content/` went missing, keeps the higher of the installed and the offered sequence.
4. The signed size must be above 0 and at or below the archive cap.
5. The downloaded file must match the signed size and the signed SHA-256. Both are checked before
   `archive::extract` opens the file.
6. Extraction then applies its own rules, and only then does the directory swap run.

The signature moves the trust anchor off the API host, as it does for the software manifest: the
host, any proxy in front of it, and the blob store behind it can all serve altered bytes and be
caught. It does not protect against whoever holds the content signing key.

A request that fails outright is treated differently from one that fails a check. When the server
cannot be reached and `content/` already holds an installed hash, `sync` keeps that content and
exits 0 with a `notice:` on stderr, because it runs at every session start and a laptop is often
offline. Someone who can block the connection can therefore hold a machine at the content it
already has, but cannot replace it: every path that writes `content/` goes through the signature
check first. A hash the server serves is never trusted over one already installed, so this is a
freeze rather than a downgrade. `brainmaker status` shows it as
`state     cannot check: <reason>`.

A host that serves an older signed release is a different case. The sequence rule refuses that
release, and the run fails with exit code 1 and leaves `content/` as it was. `--force` installs such
a release anyway, for a deliberate rollback on one machine. The rule has one limit: a machine whose
`state.json` holds no sequence has nothing to compare against, so it accepts an older signed
release, and the rule holds from the first install of a release that carries a sequence.

### Three signing keys, held by different parties

`PUBLIC_KEYS` verifies a software manifest. `CONTENT_KEYS` verifies a content release.
`REMOVAL_KEYS` verifies a removal order. A key in one list cannot sign for another, and a unit test
fails the build if a key appears in two.

The separation is what makes it acceptable for a deploy host to hold a signing key at all. Content
changes whenever the shared material does, so publishing must be automatic, so the key must sit on
the machine that publishes. A single list would make that machine able to sign a software manifest
and replace every binary in the fleet. With two, a compromise there yields what the publish scope
already yields, and no more.

The release workflow reads each block on its own for the same reason. A single grep over the file
would collect every list, and verification passes when any key given accepts, so a manifest signed
with the content key would have passed the check that exists to catch exactly that.

The removal key is the admin's. The admin signs an order each time that a person leaves, so the key
sits on the admin's own machine, in `~/.brainmaker/removal-signing.key`. A list of its own keeps
that routine away from the other two keys: the removal key can make one client run `uninstall`, and
it can install nothing. A compromise of it removes `brainmaker` from machines, which a new install
puts right. It cannot change what a machine runs.

`brainmaker admin retire` reads that file, and no command writes it, prints it, or sends it. A
removal on the admin's own machine leaves it too, because `brainmaker` did not write it. So rotate
the removal key when the admin who holds it leaves: put a new key first in `REMOVAL_KEYS`, release,
and drop the old key once the fleet runs that release.

### Content is code, once it is linked

`brainmaker link` registers a `SessionStart` hook and links the content's skills into `~/.claude`.
The content directory therefore holds files that Claude reads as instructions on every session, and
the archive's permission bits are applied on extraction, so a shipped script arrives executable.

Whoever can publish content decides what runs on every machine that synced it. Two controls narrow
that. The signature means only the content key holder can publish, not merely anyone who reaches
the API host. And `link` installs the `SessionStart` hook alone: the content's `PreToolUse`,
`PostToolUse`, `PreCompact` and `Stop` hooks stay project-scoped, because at user scope they would
run on every tool call in every project on the machine.

The hook command and the LaunchAgent script run through a shell, so `link` writes the program path
and the root as double-quoted words, with a backslash before `$`, the backtick, `"`, and `\`. A
path that holds a control character stops `link`. The hook and the `CLAUDE.md` block are written
through a temporary file and a rename, and a file that cannot be read as text is left alone.

The admin's Claude must not read the content as its own instructions: the briefing is written for
operators, and the admin's credential can read every note. So `link` asks the issuer for the role
before it writes anything. For a credential that reads the outbox and cannot send notes, it links no
skill and writes no hook and no block, removes any that an earlier run wrote, and schedules
`self-update` alone. A role check that gets no clear answer stops `link` with nothing changed.

A client that holds both `outbox:write` and `outbox:read` gets the bridge, because its person is also
an operator. Its Claude then reads the content as instructions with a credential that could read
every note, so a bad line in the content reaches further than on a laptop. Give both scopes only to
a person who needs both. `pull-outbox` also marks each note as collected, so a second admin who runs
it takes notes away from the admin's harvest.

### The notes

A note is the first thing that `brainmaker` sends from the disk to the network. The controls of that
path are in the trust boundary table above. Two more properties matter.

The name on a note comes from the server, from the credential that sent it. Nothing on the laptop
names the operator: the `author` line of a note is only a claim, and the server keeps it apart. The
`operator` file that `sync` writes from `whoami` is a convenience for Claude, never a security
control, and a file that someone edited into another shape reads as no name.

A note that the server stores, or already holds, moves to `sent/`. Nothing else leaves the outbox
for the network, and a note that fails a check here never leaves the machine. `uninstall` keeps the
outbox, because a note that was never sent exists nowhere else.

### The diagnostic report

The report is the second path from the disk to the network. It exists so that an admin can see
why a client sends no note, without a visit to the machine. These properties bound it.

**The client decides.** The server has no way to ask for a report, to choose what one holds, or to
make the client run anything. The client sends a report from `sync`, at an interval that is a
constant of this binary. It reads the status of the answer and nothing else, so a server that
answers with any body changes nothing.

**No field holds text.** A report holds the state of the machine and lines of the run log. Every
value is one of these:

| Kind of value | Examples |
|---|---|
| A word from a fixed list | `sync`, `push.rejected`, `unreachable`, `kind`, `present` |
| A bounded number | a count from 0 to 100000, an HTTP status from 100 to 599, a time within the years 2000 to 2099 |
| A content hash | 8 lowercase hexadecimal characters |
| A version | two to four groups of digits joined by dots, and a suffix of 16 characters at most |
| An event id | 16 hexadecimal characters, drawn at random |

So a report cannot hold the client identifier, the client secret, an access token, a URL, a path,
the name or the text of a note, the reason beside a rejected note, or an error message. An error
message can name a URL and can quote a server, and a reason can quote a note, so neither is ever
written to the run log: a failure is logged as a `Cause`, and a refused note as a `Rule`. The
version rule is narrower than the one for a manifest, which takes any 64 letters and digits, a
shape that a token has too.

**The file is not trusted.** The run log is a file under the root, and any program of the user can
write it. Before it sends, the client refuses a symbolic link, reads at most 512 KiB, parses each
line into a typed event, and checks every value again. The report is written from those values. A
line that holds anything else is skipped, so a file that someone put in place of the log sends
nothing of its content. The same checks run on the server, which stores no value that fails one.

**It reads `~/.claude`, and sends three facts about it.** To say whether `link` connected Claude,
the client reads `settings.json` and `CLAUDE.md` there, and the links under `skills/`. It sends
whether its own hook entry and its own block are there, and how many links name the content. It
sends no other part of those files.

**Only the admin reads it.** The server gives a report to a token with the scope `outbox:read`.
No route gives a client a report, its own included.

**It cannot break the real work.** The report runs after the content step and the outbox steps.
It prints nothing when it fails and changes no exit code. A run that received no `sync` token
makes no request for it, and a request has 10 seconds in all.

What the server keeps, and for how long, is the server's rule: see the documents of the server.

### The removal order

The removal order is the one thing that `brainmaker` does on the word of another. It exists so that
an admin can remove the content from the machine of a person who left the team. These properties
bound it.

**It takes the key and the admin's credential.** `admin retire` signs with the removal key, and
stores the order with the admin's `outbox:read` token. A person with the key and no credential has
an order that no server carries. A person with the credential and no key can make the server retire
a client and stop its content, and can remove nothing: the order that they store is one that the
client refuses. Before it stores anything, `admin retire` checks its key against `REMOVAL_KEYS` of
its own build, and asks the admin to confirm the clients by name.

**The holder of a removal key decides, and nobody else.** `sync` obeys an order only after
`signature::verify_removal` accepted the signature over the exact bytes that the server sent, with
the keys that are compiled into this binary. The server carries the order in the body of a `410`
answer to the content check, and it holds no key. A server that an attacker controls can therefore
remove no client. A build with an empty `REMOVAL_KEYS` obeys no order at all, and
`brainmaker status` prints the count.

**An order names one client.** The signed payload holds a client ID, and the client compares it
with its own identifier. So the server cannot give the order of one person to the machine of
another, and one signature cannot remove a fleet. A client with no credential obeys no order.

**An order can do one thing.** It makes the client remove what `uninstall` removes: the bridge in
`~/.claude`, and what `brainmaker` wrote under a recognised root. It names no path and no command.
Its one choice is whether `outbox/` goes too, and that choice is inside the signed payload. A
payload with a field that this build does not know is refused, because a removal has no way back,
and a field that a later version adds must not pass unread.

**It is not silent.** `sync` prints that the administrator ordered the removal, and what stays,
even with `--quiet`. The help text of the binary says that the order exists.

**It has no end, on purpose.** A signature proves who signed, and not when, and a client that
removed itself keeps no record. The same order therefore removes a later install that carries the
same credential. Treat a client ID that received an order as spent, and never issue it again. The
server refuses to restore such a client.

**A refused order changes nothing.** An order that fails a check is a failed content check: the
installed content stays, the run exits 0, and the run log holds a line with the status `410` and a
cause, which `admin diagnose` reads. The cause is `signature` when no removal key signed the order.

**The report holds no text.** After the removal, the client tells the server whether it ran to its
end, whether the root directory stays, and how many notes were never sent. Each value is a word
from a fixed list or a bounded number, as in the diagnostic report. The client reads nothing of the
answer.

What the order does not do:

- It does not reach a machine that never asks for content, or a client that the issuer refuses.
- It does not remove a copy of `content/` that a person made elsewhere, or the program file that a
  person keeps outside the root.
- It does not revoke the client. Revoke it at the issuer, after the removal is reported.

So the order is a cleanup for the usual case, and no guarantee. Treat the content as known to
every person who received it.

### Size caps

| Input | Cap |
|---|---|
| Provisioning file | 64 KiB |
| One note in the outbox | 64 KiB |
| The answer to a note, the answer of `whoami`, and the answer to a diagnostic report | 64 KiB |
| One diagnostic report | 64 KiB, and 200 lines of the run log |
| The run log on the disk | 256 KiB. Past that size it is cut to its newest 128 KiB. |
| One line of the run log | 512 bytes. A longer line is skipped. |
| The diagnostics of one client, and the removal of one client, for `admin diagnose` | 4 MiB each |
| The `410` answer that carries a removal order | 64 KiB |
| One page of notes for `admin pull-outbox` | 8 MiB |
| The fleet view, and one client's sync log | 4 MiB |
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
| `ring` | 0.17 | AES-256-GCM, HKDF-SHA256, SHA-256, and Ed25519 verification |
| `serde`, `serde_json` | 1.0.229, 1.0.151 | Manifest, state, and `settings.json` parsing; `serde_json` with `preserve_order` |
| `ureq` | 3.3.0 | HTTP client |
| `zip` | 8.6.0 | Archive extraction, `deflate-flate2-zlib-rs` only, which reads and does not build a second compressor, default features off |

The requirement column is the one in `Cargo.toml`. The release workflow runs
`cargo build --release --locked`, so a release builds the versions in the committed `Cargo.lock`,
which a Dependabot bump can raise inside a requirement without changing it. Adding a dependency
therefore requires a lock-file commit and a review. `Cargo.toml` declares `rust-version = "1.89"`.

The `dependency-review` workflow blocks a pull request that adds a dependency with a high-severity
advisory. Dependabot raises weekly updates for Cargo and for GitHub Actions.

## Reporting a vulnerability

Report privately through GitHub's private vulnerability reporting, on the Security tab of
[damac-italia/brainmaker](https://github.com/damac-italia/brainmaker/security/advisories/new). Do
not open a public issue.

Include the version from `brainmaker --version`, the platform key and signing-key count from
`brainmaker status`, and the steps to reproduce.

<!-- docsgen: unverified: private vulnerability reporting must be switched on in the repository's
     Settings > Code security before the link above accepts a report. -->
