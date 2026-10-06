# API

`brainmaker` exposes a command-line surface, and it consumes the HTTP routes that ten route keys
name. Both are described here. The crate is a binary, not a library, so it exports nothing to other Rust code.

## Command-line surface

```text
brainmaker [COMMAND] [OPTIONS]
```

### Commands

| Command | Effect | Writes |
|---|---|---|
| `sync` | Update the content when the server has a newer version. The default when no command is given. Then ask the server for the operator name, send the notes in the outbox, and send the diagnostic report when one is due. | `content/`, `state.json`, `operator`, `outbox/`, `push.json`, `diagnostics.json` |
| `status` | Print the installed hash, the latest hash, both software versions, the operator, and the notes in the outbox | nothing |
| `push` | Send the notes in the outbox now | `outbox/`, `push.json` |
| `admin pull-outbox <DIR>` | Write each note that waits on the server into `DIR`, and mark it as collected | files in `DIR` |
| `admin status` | Print the fleet: one line per operator, then one per client that no operator holds | nothing |
| `admin syncs <OPERATOR>` | Print the syncs of every client of `OPERATOR`, newest first | nothing |
| `admin syncs --client <CLIENT-ID>` | Print the syncs of one client, newest first | nothing |
| `admin diagnose <OPERATOR>` | Print, for every client of `OPERATOR`, what the server saw of it, the state that it last reported, why no note arrives, and the newest lines of its run log | nothing |
| `admin diagnose --client <CLIENT-ID>` | Print the same for one client | nothing |
| `self-update` | Replace this binary with the newest build for this platform | the binary |
| `link` | Bridge the synced content into `~/.claude`, and create the outbox. On macOS, also install and load the hourly LaunchAgent. For the admin's credential, write the LaunchAgent alone, and remove the rest. | `~/.claude/skills`, `settings.json`, `CLAUDE.md`, `~/Library/LaunchAgents/it.damac.brainmaker.plist`, `outbox/` |
| `unlink` | Remove what `link` wrote, and nothing else | the same four |
| `uninstall` | Remove what `link` wrote, then what brainmaker wrote under the root, then the root when it is empty. It asks first. | the same four, and the root |
| `session-context` | Print the `SessionStart` JSON the linked hook returns | nothing |

The parser accepts one command. A second command is an error, and so is any unrecognised argument.
`admin` takes one subcommand, `pull-outbox`, `status`, `syncs`, or `diagnose`. `pull-outbox`,
`syncs`, and `diagnose` take one value after it. `brainmaker admin --help` prints the admin help.

`sync`, `push`, `self-update`, `link`, and `unlink` also append to the run log,
`diagnostics.jsonl`. [The diagnostic report](#the-diagnostic-report) describes it.

`link` and `unlink` replace `settings.json` and `CLAUDE.md` through a temporary file and a rename.
They exit 1 and leave the file as it is when it exists but cannot be read as text, and when
`CLAUDE.md` holds a marker pair that is not one start marker followed by one end marker. The error
names the file and the count of each marker. `link` also exits 1 when the path of the program or of
the root holds a control character, because the hook command cannot carry it.

Before it writes anything, `link` asks the issuer for an `outbox:read` token and, when it gets one,
for an `outbox:write` token. A credential that gets the first and not the second is the admin's.
For it, `link` prints
`This credential collects the notes and sends none, so link connects nothing to Claude.`, removes
any skill link, hook entry, or block that an earlier run wrote, copies the program, writes the
LaunchAgent with `self-update` alone, and prints the `admin pull-outbox` command. It creates no
outbox and needs no content directory. Every other credential, and a run with no credential, gets
the bridge. When a token request fails for a reason other than `invalid_scope`, `link` exits 1 with
`cannot learn from the issuer which role this credential holds, so link changed nothing`.

### Options

| Option | Argument | Applies to | Effect |
|---|---|---|---|
| `--force` | none | `sync`, `self-update` | With `sync`, download and extract even when the content is up to date, and install a release whose sequence is not higher than the installed one. With `self-update`, reinstall the same version. |
| `--check` | none | `self-update` | Report the newer version and install nothing |
| `--no-update-check` | none | `sync` | Skip the software version check |
| `-y`, `--yes` | none | `uninstall` | Remove without asking first |
| `--config` | `<PATH>` | all but `uninstall`, which rejects it | Import the provisioning file at `PATH` |
| `--keep-config` | none | all but `uninstall`, which rejects it | Do not remove the provisioning file after the import |
| `--dir` | `<PATH>` | all | Use `PATH` as the root instead of `~/.brainmaker` |
| `--url` | `<URL>` | all but `uninstall`, which rejects it | Use `URL` as the API base |
| `--claude-dir` | `<PATH>` | `link`, `unlink`, `uninstall` | Write to `PATH` instead of `~/.claude`. The LaunchAgent is then left alone, unless `--agent-dir` is also given. |
| `--agent-dir` | `<PATH>` | `link`, `unlink`, `uninstall` | Write the LaunchAgent to `PATH` instead of `~/Library/LaunchAgents`, and do not load it |
| `--json` | none | `admin status`, `admin syncs`, `admin diagnose` | Print JSON on stdout. Every other line goes to stderr. |
| `--limit` | `<N>` | `admin syncs`, `admin diagnose` | With `admin syncs`, print at most `N` rows, from 1 to 1000. Default: 50. With `admin diagnose`, print at most `N` lines of the run log of each client. Default: 20. |
| `--client` | `<CLIENT-ID>` | `admin syncs`, `admin diagnose` | Read one client instead of an operator |
| `-q`, `--quiet` | none | all | Print errors only |
| `-h`, `--help` | none | any | Print the help text and exit 0 |
| `-V`, `--version` | none | any | Print `brainmaker <version>` and exit 0 |

The parser enforces the "Applies to" column. An option given with another command exits 1 with
`<flag> has no effect with <command>; run brainmaker --help`.

`--config`, `--dir`, `--url`, `--claude-dir`, `--agent-dir`, `--limit`, and `--client` take a value,
and each one fails:

- when the value is absent, with `<flag> needs a path` or `<flag> needs a URL`;
- when the value starts with a hyphen, because that is the next option. For a path, the message
  tells you to write it as `./<value>`;
- when the option is given twice, with `<flag> is given twice; give it once`.

A switch such as `--quiet` may be given twice.

### The LaunchAgent

`link` writes `it.damac.brainmaker.plist` on macOS only. It runs this through `/bin/sh -c` at
minute 0 of every hour, and once at each login:

```sh
date; "<root>/bin/brainmaker" --dir "<root>" self-update --quiet; "<root>/bin/brainmaker" --dir "<root>" sync --quiet --no-update-check
```

`sync` runs even when `self-update` fails. Both write to `<root>/agent.log`, so the log holds one
date line per run and the errors, if any. The admin's agent runs `date` and `self-update --quiet`
only. `link` loads the agent with `launchctl bootstrap`; a
`launchctl` failure prints a notice and does not fail `link`, because launchd loads the file at the
next login. An unchanged file is neither rewritten nor reloaded. `unlink` and `uninstall` run
`launchctl bootout` and remove the file.

With `--claude-dir` and no `--agent-dir`, `link`, `unlink`, and `uninstall` leave the agent alone.
The agent belongs to the account, so a link into another Claude directory, such as the installer's
dry run, must not start an hourly job against its root. On Windows and Linux there is no agent.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | The content is up to date, or the update succeeded |
| 1 | The command failed. The previous content and the previous binary are unchanged. |

A failed software check during `sync` does not change the exit code. `brainmaker` prints
`notice: cannot check for a software update: ...` to stderr and exits 0, because the content is
already in place. `--quiet` suppresses that notice.

A server that `sync` cannot reach is treated the same way while content is installed: `brainmaker`
prints `notice: cannot check the latest content version: ...` and
`notice: the installed content <hash> stays in place.` to stderr, and exits 0. With nothing
installed, or with `--force`, the failure exits 1.

A `sync` that waits 30 seconds for the install lock of another run, and still finds it held, is
treated the same way while content is installed: `brainmaker` prints
`notice: another brainmaker run is installing content.` and
`notice: the installed content <hash> stays in place.` to stderr, and exits 0. `--quiet` suppresses
both lines. With nothing installed, or with `--force`, it exits 1. A `self-update` that cannot take
the update lock within 30 seconds exits 1.

A `sync` exits 1 when the server offers a release with another hash whose sequence is not higher
than the installed one. The content stays as it was. `--force` installs that release.

The outbox steps of `sync` never change its exit code. A failure of `whoami` or of the push prints
one `notice:` line to stderr, which `--quiet` hides, and the content result decides the code. A
failed content step does not stop the outbox steps: they run first, and the content error is then
returned.

`push` exits 1 when a note stays in the outbox because the issuer or the server did not take it, or
could not be reached. It exits 0 when every ready note was sent or rejected, when no note waits,
and when another run holds the push lock.

The diagnostic report never changes an exit code, and a report that fails prints nothing, with or
without `--quiet`. A report that the server stored prints one line, which `--quiet` hides. A run
that received no `sync` token makes no request for it, so a machine that is offline waits no
longer than before.

`admin pull-outbox` exits 1 when a note could not be written. That note stays on the server,
unacknowledged, for the next run. It also exits 1 when `DIR` is not a directory, and when another
admin command holds `.admin.lock`. Every admin command exits 1 when the issuer does not grant the
scope `outbox:read`, with `this credential cannot read the outbox`.

`admin diagnose` exits 1 when the server registers no client to the operator, when it does not
know the client, and when it has no diagnostics route. A server older than that route answers
`404`, and the error says so.

`uninstall` exits 0 and removes nothing when the answer to its question is not `y` or `yes`. It
exits 1 when stdin is not a terminal and `--yes` is absent. A removal that fails part-way exits 1
with part of the install already gone; run the command again to finish.

### `uninstall` removal

`uninstall` loads no settings, so it runs where the sealed store no longer opens, and it never
imports a provisioning file. With `uninstall`, the parser rejects `--config`, `--keep-config`,
and `--url` with `<flag> has no effect with uninstall, which loads no settings`, and the run exits
1. It works in this order:

| Step | Removes | When |
|---|---|---|
| 1 | The LaunchAgent, the skill links, the `SessionStart` hook, and the `CLAUDE.md` block that `link` wrote, as `unlink` removes them | the root is recognised or does not exist |
| 2 | `content/`, `.staging/`, `.trash/`, `.download.zip`, `.lock`, `.update.lock`, `.outbox.lock`, `.admin.lock`, `.diagnostics.lock`, `agent.log`, `operator.tmp`, `operator`, `push.json.tmp`, `push.json`, `diagnostics.jsonl.tmp`, `diagnostics.jsonl`, `diagnostics.json.tmp`, and `diagnostics.json` | the root is recognised |
| 3 | `bin/brainmaker`, and every `bin/.brainmaker*` file that `link` or `self-update` left | the root is recognised |
| 4 | `state.json.tmp`, `state.json`, `confidential/config.tmp`, and `confidential/config.enc` | the root is recognised |
| 5 | `bin/`, `confidential/`, and then the root | each one is empty |

The root is recognised when it holds a `state.json` of the shape that `sync` writes, or a
`confidential/config.enc` that starts with the sealed header. When the root exists but is not
recognised, the run changes nothing, step 1 included, and says so. The agent, the hook, and the
`CLAUDE.md` block are not checked against their root, so step 1 under a wrong `--dir` would cut off a real install
elsewhere. Step 4 comes last because those two files are the marks: a run that stops part-way
keeps them, and the next run still recognises the root.

`outbox/` stays in every case, with every note in it, sent or not. The run prints how many notes
were never sent: those that wait in `outbox/` and those in `outbox/rejected/`. The root then stays
too, because it holds `outbox/`.

A root that still holds other entries after step 5 stays, and the run names them. A symbolic link
goes without the target that it names. That includes the root: when it is a link, step 5 removes
the link, and the directory it names stays, empty. The program that runs `uninstall` stays, unless it is
`bin/brainmaker`, and the run prints its path. Windows cannot remove a running program, so there
the run prints a notice and the path to delete after the command exits.

### `status` output

`status` prints one `key value` pair per line:

| Key | Value |
|---|---|
| `root` | The root directory |
| `content` | The content directory |
| `store` | Path of `config.enc` |
| `settings` | `imported from <path>`, `read from the sealed store`, or `read from the environment` |
| `api base` | The base URL in use |
| `token url` | The token endpoint in use, or `<none>` |
| `auth` | `absent`, or `client-credentials grant, scope sync, outbox:write for the outbox, and outbox:read for the admin commands, client id N characters, secret N characters`. Never a credential itself. |
| `key` | `release` or `development`, naming which build key this binary carries |
| `binding` | What ties the sealed store to this machine: `machine identifier`, `home directory path (weak)`, or `none (weak)`. A `(weak)` value means a copy of `config.enc` opens on another machine. |
| `signing` | Both key counts, as `N trusted software key(s), N trusted content key(s)`. `0` software keys means it installs no update; `0` content keys means it installs no content. |
| `installed` | The hash in `state.json`, or `<none>` |
| `present` | `yes` when `content/` is a directory |
| `latest` | The hash the server reports, or `<unknown>` when it cannot be reached |
| `state` | `up to date`, `stale`, `not installed`, or `cannot check: <reason>` |
| `software` | This binary's version |
| `platform` | This machine's manifest key, for example `darwin-arm64` |
| `published` | The manifest version, or `<unknown>` |
| `update` | `none`, `available; run <path of this binary> self-update`, or a reason |
| `operator` | The name in `operator`, which the last `sync` wrote, or `<none>` |
| `outbox` | `N waiting, N rejected`, counted from the files |
| `pushed` | The age of the last push that sent a note, in its largest whole unit, such as `2 days ago`, or `<none>` |
| `reported` | The age of the last diagnostic report that the server stored, in the same form, or `<none>` |

The last four rows read local files only.

### `session-context` output

One JSON object on `stdout`, in the shape a Claude `SessionStart` hook returns:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": "# Shared content, synced by brainmaker\n\n..."
  }
}
```

The context carries these files from the content directory, each capped on its own so that one
growing file cannot crowd out the rest. A missing or empty file is skipped, and a cut is announced
in the text with the path to read for the rest.

| File | Cap |
|---|---|
| `CLAUDE.md` | 24 KiB |
| `wiki/hot.md` | 8 KiB |
| `agent-memory/OPEN-THREADS.md` | 8 KiB |
| `agent-memory/PREFERENCES.md` | 8 KiB |

After the files, the context carries one more section, `The outbox`, from local files only: the
operator name in `operator`, or a line that says the server named none; the path of the outbox, where
Claude writes the end-of-session note; and the count of notes that wait and that were rejected.

With no content, or with every file empty, the command prints `{}` and the session is unchanged.
The command always prints, even under `--quiet`, because the hook reads its `stdout`.

### The outbox

The operator's Claude writes one note per session into `<root>/outbox/`. `sync` and `push` send the
notes, oldest first, under the lock `<root>/.outbox.lock`. A run that finds the lock held sends
nothing.

| Check, in order | A file that fails it |
|---|---|
| The name does not start with `.`, ends with `.md`, and the entry is not a directory | Is ignored |
| The entry is a regular file, and not a symbolic link | Moves to `rejected/` |
| The file has not changed for 60 seconds | Waits for the next run |
| The name matches `^[0-9]{4}-[0-9]{2}-[0-9]{2}-[a-z0-9][a-z0-9-]{0,99}\.md$` | Moves to `rejected/` |
| The file is 1 byte to 64 KiB, valid UTF-8, with no NUL and no byte-order mark | Moves to `rejected/` |
| The frontmatter passes the rules below | Moves to `rejected/` |

The frontmatter starts with `---` and a line end, LF or CRLF, and ends at the next line that is
exactly `---`. A key line is `key: value` at column 0; indented lines, list items, blank lines, and
`#` comments carry nothing; any other line fails. A value is trimmed; a quoted value ends at its
closing quote, and an unquoted value ends before ` #`. `kind` is required and is `fact`,
`decision`, `anomaly`, or `question`. `domain` is required and matches `^[a-z0-9][a-z0-9-]{0,31}$`.
No top-level key appears twice, `author` and `review_flags` included. The server applies the same
rules.

A file that moves to `rejected/` gets `<name>.reason.txt` beside it, with the rule it broke or the
reason the server gave. A name that is taken there, or under `sent/`, gets `-2`, `-3`, and so on
before `.md`.

When no note is ready, the run makes no request. Otherwise it asks for an `outbox:write` token and
sends each note to `POST {base}/{outbox route}/<name>`:

| Answer | The note |
|---|---|
| `201`, `200` | Moves to `sent/<YYYY-MM>/`, where `YYYY-MM` is the first 7 characters of the `received_at` that the server returned. A value of another shape gives `sent/unknown/`. |
| `400`, `413` | Moves to `rejected/`, with the server's reason |
| `401`, `403`, `404`, `429`, `5xx`, another status, or no answer | Stays. The run prints a notice and stops. |
| `invalid_scope` from the token endpoint | Every note stays. The run prints a notice and stops. |

A run that sent at least one note writes `<root>/push.json` through a temporary file and a rename:

```json
{ "last_push_at_unix": 1790500000, "last_push_notes": 2 }
```

### The diagnostic report

A client tells the server what it did, so that the admin can see why it sends no note. The client
decides what it reports. The server never asks for a report, and nothing in its answer changes
what `brainmaker` does.

**The run log.** `sync`, `push`, `self-update`, `link`, and `unlink` append one line for each
thing that they did to `<root>/diagnostics.jsonl`. A line is one JSON object:

```json
{"id":"9f86d081884c7d65","at_unix":1790683100,"command":"sync","code":"push.rejected","rule":"kind","count":3}
```

| Field | Rule |
|---|---|
| `id` | 16 lowercase hexadecimal characters, drawn at random. The server stores a line once. |
| `at_unix` | Seconds since the Unix epoch, within the years 2000 to 2099 |
| `command` | `sync`, `push`, `self-update`, `link`, or `unlink` |
| `code` | One word of the table below |
| `cause` | Optional. `unreachable`, `http`, `credentials`, `scope`, `signature`, `order`, or `other` |
| `status` | Optional. The HTTP status of the answer, from 100 to 599 |
| `rule` | Optional. `symlink`, `not_a_file`, `unreadable`, `name`, `size`, `text`, `frontmatter`, `kind`, `domain`, or `server` |
| `count` | Optional. A number from 0 to 100000 |
| `hash` | Optional. A content hash of 8 lowercase hexadecimal characters |
| `version` | Optional. Two to four groups of digits joined by dots, such as `0.2.0`, and then at most `-` or `+` and 16 letters, digits, dots, and hyphens |

| Code | Written when | Carries |
|---|---|---|
| `content.up_to_date` | The installed content is the release that the server offers | `hash` |
| `content.updated` | The run installed a release | `hash` |
| `content.unreachable` | The release could not be read, and the installed content stays | `hash`, `cause`, `status` |
| `content.busy` | Another run was installing | `hash` |
| `content.failed` | The content step failed | `cause`, `status` |
| `operator.failed` | The server did not answer `whoami` | `cause`, `status` |
| `push.sent` | The server holds `count` more notes | `count` |
| `push.rejected` | `count` notes broke `rule` and moved to `rejected/` | `rule`, `count` |
| `push.settling` | `count` notes changed in the last 60 seconds and wait | `count` |
| `push.stopped` | The push stopped, and the notes that were not sent stay | `cause`, `status` |
| `push.busy` | Another run was sending the notes | nothing |
| `push.failed` | The push failed on this machine | `cause` |
| `update.current` | This program is the newest published build | `version` |
| `update.available` | A newer build is published, and `--check` installed nothing | `version` |
| `update.installed` | The run installed that build | `version` |
| `update.no_build` | That version is published with no build for this platform | `version` |
| `update.failed` | `self-update` failed | `cause`, `status` |
| `link.done`, `unlink.done` | The command ran to its end | nothing |
| `link.failed`, `unlink.failed` | The command failed | `cause` |

A step that worked and changed nothing leaves no line, except the content step, so every `sync`
leaves one line at least. The rule `server` names a note that the server refused with `400` or
`413`. Every other rule names a check of [The outbox](#the-outbox).

The log stays under 256 KiB. A run that takes it past that size cuts it to its newest 128 KiB, at
the start of a line.

**The state.** The report also carries the state of the machine, read from local files alone:

| Field | Value |
|---|---|
| `at_unix` | When the state was read |
| `software.version`, `software.platform` | The version of this program, and its platform key when that is the key of a published build |
| `content.installed_hash`, `content.installed_at_unix`, `content.present` | The hash and the time in `state.json`, and whether `content/` is a directory |
| `link.hook`, `link.block` | `present`, `absent`, or `unknown`: the `SessionStart` hook entry in `~/.claude/settings.json`, and the block in `~/.claude/CLAUDE.md`. `unknown` is a file that could not be read. |
| `link.skills` | How many links under `~/.claude/skills` name the content directory |
| `link.agent` | `present` or `absent` for the file of the hourly agent, or `none` on a system that has no agent |
| `outbox.waiting`, `outbox.rejected`, `outbox.sent` | The notes in `outbox/`, in `outbox/rejected/`, and under `outbox/sent/`, counted from the files |
| `outbox.ignored` | The entries directly in `outbox/` that `push` does not read as a note: a file with a name that does not end in `.md`, or a directory other than `sent/` and `rejected/`. A note in a directory is not read, and the directory counts as one entry. A name that starts with a dot does not count. `push` never sends such an entry and never rejects it, so nothing else shows it. |
| `outbox.last_push_at_unix`, `outbox.last_push_notes` | The values in `push.json` |
| `outbox.operator` | `true` when the `operator` file holds a name |

A value that does not exist is `null`. No field holds free text: every value is a word from a
fixed list, a bounded number, a content hash, or a version. The report therefore holds no client
identifier, no client secret, no token, no URL, no path, no note name, no note text, no reason of a
rejection, and no error message.

**When it goes.** `sync` sends the report after the outbox steps, to
[`POST {base}/{diagnostics route}`](#post-basediagnostics-route), with the `sync` token that the
run already holds. A run sends no report in these cases:

| Case | What happens |
|---|---|
| The run received no `sync` token | No request. The machine is offline, or has no credential. |
| The last report, or the last try, is younger than 30 minutes | No request. A `link` or an `unlink` that worked cuts this wait to 90 seconds from its end. |
| Another run holds `<root>/.diagnostics.lock` | No request |

A report carries at most 200 lines of the run log, the oldest first, and at most 64 KiB. The lines
that did not fit go with the next report. The client reads the log back before it sends: the file
must be a regular file and no symbolic link, a line of more than 512 bytes is skipped, and every
line is parsed and checked against the rules above. Only the lines that pass are written into the
report, so no byte of the file reaches the network unchecked.

On a `2xx` answer, the client writes `<root>/diagnostics.json` through a temporary file and a
rename:

```json
{ "last_attempt_at_unix": 1790683200, "last_report_at_unix": 1790683200, "sent_through": "9f86d081884c7d65" }
```

`sent_through` is the `id` of the newest line that the server holds. On any other answer, and on
no answer, the client changes `last_attempt_at_unix` and no other value. The lines stay for the
next report, and the next try waits 30 minutes too.

### `admin diagnose` output

The command reads the fleet view, and then the diagnostics of each selected client. Without
`--json` it prints one block for each client. A value that the server does not know prints as `-`.

```text
gabriele  brainmaker-sync-gabriele
  server    last sync 2026-10-05T10:00:03+02:00  version 0.2.0  notes stored 0
  report    received 2026-10-05T10:00:04+02:00  read on the client at 2026-10-05T10:00:02+02:00
  software  0.2.0  darwin-arm64
  content   a377aa94  installed 2026-10-01T09:00:00+02:00  present
  link      hook absent  block absent  skills 0  agent present
  outbox    waiting 0  rejected 0  sent 0  ignored 0  last push -  operator named
  finding   The SessionStart hook of brainmaker is not in the Claude settings of that machine. No session there reads the briefing or learns of the outbox, so Claude writes no note. Run link on that machine. The admin's own install has no hook on purpose.
  log       2026-10-05T10:00:02+02:00  sync  content.up_to_date  a377aa94
  log       2026-10-05T09:00:01+02:00  self-update  update.current  0.2.0
```

| Line | Source |
|---|---|
| The first line | The operator, or `unregistered`, the client ID, and `retired` for a retired client: the fleet view |
| `server` | What the server saw: the last sync, the version in its `User-Agent`, and the notes of this client that the server holds |
| `report` | When the server received the last report, and the clock of the client when it read its state. `none` when the server holds no report. |
| `software`, `content`, `link`, `outbox` | The state in that report |
| `finding` | One line for each cause that the command found, or `none` |
| `log` | The run log, newest first: the time on the client, the command, the code, and each value that the line carries |

The command names these causes:

| Finding | When |
|---|---|
| The client is too old to send notes | The server holds no report, and the last sync named a version older than 0.1.8, the first one with `push` |
| The server holds no report | The server holds no report, and the last sync named another version, or none |
| The server never saw this client sync | The fleet view holds no last sync |
| The report is older than the syncs | The last sync is more than a day after the last report. The state can then be old. |
| The `SessionStart` hook is absent | `link.hook` is `absent`. `link` never ran, or `unlink` ran. The admin's own install has no hook on purpose. |
| Notes broke a rule | `outbox.rejected` is above 0. The finding names each rule that the run log holds, with its count. |
| Notes wait | `outbox.waiting` is above 0. The finding names what the newest push line of the run log says. |
| Entries in the outbox are not notes | `outbox.ignored` is above 0. Something wrote a file with another ending than `.md`, or wrote into a folder. `push` never sends such an entry and never rejects it. |
| No note was ever written | The hook is not absent, no note waits, none was rejected, none was sent, no entry is ignored, and the server holds none |
| The registry names no operator, or the client is retired | The fleet view says so. The server refuses the notes of such a client. |
| That machine runs an older version | The reported version is older than the version of this program |
| The server dropped lines | `events_dropped` is above 0 |

With `--json`, the command prints an array with one object for each client. It holds `client_id`,
`operator`, `retired`, `last_sync_at`, `brainmaker_version`, `notes_stored`, `report`, `findings`,
and `events`, and a key without a value is left out.

Every time passes through as the server sent it. The command refuses a body of another shape: a
field or a word that it does not know, a time, a version, or a content hash of another shape, and
the diagnostics of another client than the one it asked for.

### `admin pull-outbox <DIR>`

1. `DIR` must be a directory. Otherwise the command writes nothing and exits 1.
2. The command takes `<root>/.admin.lock` without waiting, and removes every
   `.brainmaker-pull-*` file in `DIR`: the temporary file of a run that was stopped.
3. It reads one page of 100 notes from `GET {base}/{admin outbox route}?limit=100`.
4. For each note it checks again the operator, the client ID, the note name, `received_at`, the kind,
   the domain, each flag, the size, and the SHA-256 of the text. A note that fails one stays on the
   server.
5. It names the file `<YYYY-MM-DD>-<operator>-<name>`, where the date is the first 10 characters of
   `received_at`.
6. It sets `author: <operator>` and `review_flags: [<flags>]`, or `review_flags: []`, in the
   frontmatter. A key that is present gives way, with the indented and list lines under it, to the
   new line in its place. A key that is absent goes right after the opening `---`. The note's own
   line end is kept.
7. It writes a temporary file in `DIR`, flushes it, and hard-links it to the final name, so an
   existing file is never replaced. A name that holds the same bytes counts as written. A name that
   holds other bytes gets `-2`, `-3`, and so on before `.md`.
8. It posts the ids of the notes on disk to `POST {base}/{admin outbox route}/ack`, then reads the
   next page.
9. It stops at a page with no note, at a page that brings no note to disk, or after 1000 notes.

### `admin status` output

Without `--json`, one line per operator, then one line per client that no operator holds. A value
the server does not know prints as `-`.

```text
gabriele  last sync 2026-09-29T20:00:03+02:00  version 0.1.8  platform darwin-arm64  waiting 0  sent in 7 days 3
unregistered  brainmaker-sync-old  last sync 2026-09-20T09:00:00+02:00  ip 198.51.100.4
```

With `--json`, the command prints this shape. A key without a value is left out.

```json
{
  "generated_at": "2026-09-29T21:00:00+02:00",
  "bundle": { "version": "a377aa94", "published_at": "2026-09-29T18:00:00+02:00", "files": 641 },
  "operators": {
    "gabriele": {
      "client_id": "brainmaker-sync-gabriele",
      "platform": "darwin-arm64",
      "brainmaker_version": "0.1.8",
      "content_version": "a377aa94",
      "last_sync_at": "2026-09-29T20:00:03+02:00",
      "last_push_at": "2026-09-29T18:12:40+02:00",
      "notes_pushed_total": 12,
      "notes_pushed_7d": 3,
      "outbox_pending": 0,
      "clients": [
        {
          "client_id": "brainmaker-sync-gabriele",
          "retired": false,
          "platform": "darwin-arm64",
          "brainmaker_version": "0.1.8",
          "content_version": "a377aa94",
          "last_sync_at": "2026-09-29T20:00:03+02:00",
          "last_ip": "203.0.113.7"
        }
      ]
    }
  },
  "unregistered": [
    { "client_id": "brainmaker-sync-old", "last_sync_at": "2026-09-20T09:00:00+02:00", "last_ip": "198.51.100.4" }
  ]
}
```

| Field | Source |
|---|---|
| `last_sync_at` | The server's `last_seen_at` |
| `client_id`, `platform`, `brainmaker_version`, `content_version`, `last_sync_at`, `outbox_pending` | The operator's most recently seen client that is not retired |
| `last_push_at`, `notes_pushed_total`, `notes_pushed_7d` | All of the operator's clients |
| `platform` | Brainmaker's platform key, such as `darwin-arm64` |

Every time passes through as the server sent it, in its report time zone with the offset. The
command checks the shape of each time, and compares two times by the instant that each one names.

### `admin syncs` output

One line per sync, newest first: the time, the client ID, the status that `content/latest`
answered, the IP address, the platform, the version, and the installed content.

```text
2026-09-29T20:00:03+02:00  brainmaker-sync-gabriele  200  203.0.113.7  darwin-arm64  0.1.8  a377aa94
```

With `--json`, the command prints the rows as a JSON array. Each row holds `client_id`, `id`, `at`,
`status`, `ip`, `user_agent`, `platform`, `brainmaker_version`, `content_version`, and
`outbox_pending`, and a key without a value is left out.

### The operator file

When a `sync` run received a `sync` token, it asks `GET {base}/{whoami route}`. A name in the answer
must match `^[a-z0-9][a-z0-9-]{0,31}$`, and `sync` writes it to `<root>/operator` as the name and one
line feed. An answer of `"operator": null` deletes the file. Any failure, such as a `404` from a
server older than this route, leaves the file as it is. The file is a convenience for Claude, never
a security control.

## HTTP routes the server must serve

Twelve routes sit under `BRAINMAKER_API_BASE`. One further route, the token endpoint, sits under
`SWETSI_JWT_ENDPOINT`. The two hosts may differ. Serve them all over TLS: `brainmaker` refuses a
plain-HTTP URL unless its host is this machine. `brainmaker` sends `Authorization: Bearer <token>`
on every request under the base URL when a credential is configured, and sends the `User-Agent`
`brainmaker/<version>`.

Every route name below is the default. Each one has a key in the provisioning file, so a deployment
can serve these requests at any path it likes:

| Route | Key | Default |
|---|---|---|
| Token | `SWETSI_TOKEN_PATH` | `oauth2/token` |
| Latest hash | `BRAINMAKER_CONTENT_LATEST_PATH` | `content/latest` |
| Content archive | `BRAINMAKER_CONTENT_ARCHIVE_PATH` | `content/{hash}.zip` |
| Software manifest | `BRAINMAKER_SOFTWARE_MANIFEST_PATH` | `software/brainmaker` |
| Replacement binary | `BRAINMAKER_SOFTWARE_BINARY_PATH` | `software/brainmaker-{version}-{platform}{ext}` |
| Note upload | `BRAINMAKER_OUTBOX_PATH` | `outbox`, then `/<note name>` |
| Operator name | `BRAINMAKER_WHOAMI_PATH` | `whoami` |
| Diagnostic report | `BRAINMAKER_DIAGNOSTICS_PATH` | `diagnostics` |
| Notes for the admin, and their acknowledgement | `BRAINMAKER_ADMIN_OUTBOX_PATH` | `admin/outbox`, and `/ack` under it |
| Fleet view, one client's sync log, and one client's diagnostics | `BRAINMAKER_ADMIN_CLIENTS_PATH` | `admin/clients`, and `/<client_id>/syncs` and `/<client_id>/diagnostics` under it |

A laptop package never carries the two admin keys. Only the admin commands read them.

`brainmaker` substitutes `{hash}`, `{version}`, `{platform}`, and `{ext}`. `{ext}` is `.exe` on
Windows and empty everywhere else. A route is a path under its base URL: a value holding `://`, a
`..` segment, or a space fails at load, and a leading `/` is stripped.

Timeouts: 10 s to connect; 20 s total for a text request; 300 s total for a download; 10 s total
for the diagnostic report.

### `POST {jwt_endpoint}/{token route}`

Returns an access token for the client-credentials grant.

```text
Authorization: Basic base64(client_id:client_secret)
Content-Type: application/x-www-form-urlencoded

grant_type=client_credentials&scope=sync
```

`brainmaker` names one scope in every request rather than relying on a server default: `sync` for
every read and for the diagnostic report, `outbox:write` for the notes, asked for only when a note
is ready, and `outbox:read` for the admin commands. `link` asks for the two outbox scopes once, to learn the role. It keeps one
token per scope for the run. An answer of `400` or `401` whose body is `{"error": "invalid_scope"}`
means the issuer does not grant that scope to the client: the push stops with a notice, and the
sync is not affected.

```json
{ "access_token": "…", "token_type": "Bearer", "expires_in": 600, "scope": "sync" }
```

| Field | Rule |
|---|---|
| `access_token` | Required. 1 to 8192 bytes of printable ASCII. A control character fails the run, because the value goes into a header. |
| `token_type` | Optional. `Bearer` in any case. Any other value fails the run. |
| `expires_in` | Optional, in seconds. It defaults to 600. The client uses at most 3600 of it, and stops using the token 30 seconds before that time ends. |

The response body is read up to 64 KiB. One run gets one token per scope and reuses it, so a server
that issues a 10-minute token serves one token request per scope per run.

| Status | Message the client prints |
|---|---|
| 400, 401 | `the token endpoint rejected the client credentials with HTTP <code>; check SWETSI_CLIENT_ID and SWETSI_CLIENT_SECRET, and check that the client may ask for the scope sync` |
| 404 | `the token endpoint returned HTTP 404 Not Found; check SWETSI_JWT_ENDPOINT and SWETSI_TOKEN_PATH` |
| other | `the token endpoint returned HTTP <code>` |

Each message ends with the message the endpoint itself returned, after a colon. An OAuth2 endpoint
answers `{"error": "invalid_client"}`, and it may add an `error_description`; the client prints the
pair as `invalid_client: <description>`. A body that is not that JSON object is printed as it
stands. Either way the text is collapsed onto one line and stops at 200 characters.

### `GET {base}/{latest hash route}`

Returns the signed description of the current content.

```json
{
  "hash": "a1b2c3d4",
  "payload": "{\"hash\":\"a1b2c3d4\",\"sha256\":\"<64 hex>\",\"size_bytes\":1152430,\"sequence\":1760000000}",
  "signature": "<128 hex>"
}
```

| Field | Rule |
|---|---|
| `hash` | Exactly 8 ASCII alphanumeric characters. The client ignores it and reads the hash inside `payload`. |
| `payload` | Required. The signed document, byte for byte. |
| `signature` | Required. Ed25519 over the `payload` bytes, as 128 hexadecimal characters. |

The client verifies `signature` over `payload` against `CONTENT_KEYS` before it parses anything,
then reads these fields from the payload:

| Field | Rule |
|---|---|
| `hash` | Exactly 8 ASCII alphanumeric characters. |
| `sha256` | 64 hexadecimal characters, the digest of the archive. |
| `size_bytes` | Above 0 and at or below 512 MiB. |
| `sequence` | Optional. A whole number, higher for a later release. `brainmaker-sign` writes the time of signing. The client refuses a release whose sequence is not higher than the installed one. |

A response with no `payload` or no `signature` fails the run. The bare `hash` is kept beside them
so a client older than the signing change keeps working, which is what lets a deployment publish
signatures before its fleet updates. The payload carries no URL: the client derives the download
address from its own base URL, so a signed document cannot move the download to another host.

The response body is read up to 1 MiB. Serve this route with `Cache-Control: no-store`; a cached
response makes the client skip an update that is already published.

The request carries three headers in which the client reports itself. The server may record them.
Nothing that the client does depends on them.

| Header | Value |
|---|---|
| `Brainmaker-Platform` | The platform key, such as `darwin-arm64` |
| `Brainmaker-Content` | The installed hash, or `none` when `state.json` names none or `content/` is missing |
| `Brainmaker-Outbox` | The count of notes that wait in the outbox |

### `GET {base}/{content archive route}`

Returns the zip archive for one hash. `{hash}` is the validated value from the route above.

| Condition | Client behavior |
|---|---|
| Body larger than 512 MiB | The partial file is deleted and the run fails |
| Empty body | The file is deleted and the run fails |
| Not a zip archive | The run fails; `content/` is untouched |

### `GET {base}/{software manifest route}`

Returns the signed software manifest. Serve the file that `brainmaker-sign` produces, unchanged,
byte for byte. The client checks the signature before it parses anything.

```json
{
  "payload": "{\n  \"version\": \"0.2.0\",\n  \"platforms\": { ... }\n}\n",
  "signature": "15741a4f769ab359…"
}
```

| Field | Rule |
|---|---|
| `payload` | The manifest, as a JSON string. The signature covers exactly these bytes. |
| `signature` | 128 hexadecimal characters: the Ed25519 signature over `payload` |

The manifest inside `payload` is:

```json
{
  "version": "0.2.0",
  "platforms": {
    "darwin-arm64": {
      "sha256": "e2959e4c4f210dbdfe848f49776a32279362a52760bbd0b26e3effb6cd0df350"
    },
    "darwin-x86_64": { "sha256": "..." },
    "linux-x86_64":  { "sha256": "..." }
  }
}
```

| Field | Rule |
|---|---|
| `version` | 1 to 64 characters of digits, dots, hyphens, plus signs, and ASCII letters |
| `platforms` | One key per platform, spelled `<os>-<arch>`. The client reads only its own key. |
| `sha256` | 64 hexadecimal characters, in either case |

The manifest carries no URL. The client derives the download address from its own base URL and
`BRAINMAKER_SOFTWARE_BINARY_PATH`, so a published manifest names no host, and a manifest cannot move
a download to another host.

Because the signature covers the bytes rather than a re-serialization, any proxy that reformats this
JSON body breaks every client. Serve it as a static file. The client reads the body up to 1 MiB.
Serve this route with `Cache-Control: no-store`.

Platform keys use `darwin` for macOS and `arm64` for `aarch64`. Run `brainmaker status` to read the
key a given machine asks for. A machine whose key is absent from the manifest gets an error naming
the keys that are present.

### `GET {base}/{software binary route}`

Returns one replacement binary. The client asks for the route with `{version}`, `{platform}`, and
`{ext}` filled in, so the served file names must match the route you configure. The default route
asks for `software/brainmaker-0.2.0-darwin-arm64`, which is the name the release workflow produces.

The client reads the body up to 128 MiB, checks its SHA-256 against the signed manifest, and runs it
with `--version` before it swaps. The output, with surrounding white space removed, must be exactly
`brainmaker <version>`, where `<version>` is the manifest version.

### `GET {base}/{whoami route}`

Returns the operator of the client that the token names, with the `sync` token.

```json
{ "client_id": "brainmaker-sync-gabriele", "operator": "gabriele", "display_name": "Gabriele" }
```

The client reads `operator` alone: a name, or `null` when the server names no operator. It reads the
body up to 64 KiB. Serve this route with `Cache-Control: no-store`.

### `POST {base}/{outbox route}/<name>`

Receives one note, with an `outbox:write` token. `<name>` passed the name rule above. The body is
the file as it is on disk, with `Content-Type: text/markdown; charset=utf-8`.

| Status | The client |
|---|---|
| `201` | Moves the note to `sent/`. The body must carry `received_at`, ISO 8601, in the time zone of the server's report, such as `2026-09-29T18:12:40+02:00`. |
| `200` | The same. The server already held these bytes. |
| `400`, `413` | Moves the note to `rejected/`, with the reason from `{"error": "..."}` |
| any other | Keeps the note, and stops the run |

The client reads the answer body up to 64 KiB. It removes every control character from the reason
before the reason reaches a file or the terminal.

### `POST {base}/{diagnostics route}`

Receives the diagnostic report of the client that the token names, with the `sync` token. The body
is JSON, with `Content-Type: application/json`, of at most 64 KiB:

```json
{
  "schema": 1,
  "state": {
    "at_unix": 1790683200,
    "software": { "version": "0.2.0", "platform": "darwin-arm64" },
    "content": { "installed_hash": "a377aa94", "installed_at_unix": 1790500000, "present": true },
    "link": { "hook": "present", "block": "present", "skills": 12, "agent": "present" },
    "outbox": {
      "waiting": 0, "rejected": 4, "sent": 0, "ignored": 0,
      "last_push_at_unix": null, "last_push_notes": null, "operator": true
    }
  },
  "events": [
    { "id": "9f86d081884c7d65", "at_unix": 1790683100, "command": "sync", "code": "push.rejected", "rule": "kind", "count": 3 }
  ]
}
```

[The diagnostic report](#the-diagnostic-report) gives the rule of every field. `schema` is 1.
Nothing in the body names the client: the server takes it from the token.

| Status | The client |
|---|---|
| `2xx` | Records the report as stored, and the last line that it sent |
| any other, or no answer | Keeps the lines for the next report, and waits 30 minutes before the next try |

The client reads the answer up to 64 KiB, and reads its status alone. The server must apply the
rules of the fields too, and must store no value that breaks one.

### `GET {base}/{admin outbox route}?limit=<N>`

Returns the notes that nobody acknowledged, oldest first, with an `outbox:read` token.

```json
{ "notes": [ { "id": "5b1c…", "operator": "gabriele", "client_id": "brainmaker-sync-gabriele",
  "name": "2026-09-29-lezioni-damac-firma.md", "kind": "fact", "domain": "damac", "flags": ["iban"],
  "sha256": "…", "size_bytes": 812, "received_at": "2026-09-29T18:12:40+02:00", "body": "---\n…" } ] }
```

The client asks for 100 notes a page, and reads a page up to 8 MiB. A field that it does not know
fails the command.

### `POST {base}/{admin outbox route}/ack`

Marks notes as collected, with an `outbox:read` token. The body is `{"ids": [...]}`, and the answer
is `{"acked": n}`.

### `GET {base}/{admin clients route}`

Returns the fleet view, with an `outbox:read` token. The client reads it up to 4 MiB, and refuses
any other shape: a field that it does not know, a client ID or an operator outside its rule, or a
time of another shape.

### `GET {base}/{admin clients route}/<client_id>/syncs?limit=<N>`

Returns one client's sync log, newest first, with an `outbox:read` token. The client checks the
client ID before it becomes a path segment, and reads the log up to 4 MiB.

### `GET {base}/{admin clients route}/<client_id>/diagnostics?limit=<N>`

Returns the last report of one client and the newest `N` lines of its run log, newest first, with
an `outbox:read` token. The client checks the client ID before it becomes a path segment, and reads
the body up to 4 MiB.

```json
{
  "client_id": "brainmaker-sync-gabriele",
  "report": {
    "received_at": "2026-10-05T10:00:04+02:00", "at": "2026-10-05T10:00:02+02:00",
    "version": "0.2.0", "platform": "darwin-arm64",
    "content": { "installed_hash": "a377aa94", "installed_at": "2026-10-01T09:00:00+02:00", "present": true },
    "link": { "hook": "present", "block": "present", "skills": 12, "agent": "present" },
    "outbox": { "waiting": 0, "rejected": 4, "sent": 0, "ignored": 0, "last_push_at": null, "last_push_notes": null, "operator": true },
    "events_dropped": 0
  },
  "events": [
    { "id": 4812, "at": "2026-10-05T10:00:02+02:00", "command": "sync", "code": "push.rejected",
      "cause": null, "status": null, "rule": "kind", "count": 3, "hash": null, "version": null }
  ]
}
```

`report` is `null` for a client that sent none. Every time is in the time zone of the server's
report, with its offset. A field that the client does not know fails the command, and so does a
`command`, a `code`, a `cause`, a `rule`, or a link word that it does not know.

### Error responses

| Status | Message the client prints |
|---|---|
| 401, 403 | `the server rejected the request with HTTP <code>; check that SWETSI_CLIENT_ID is configured, and that the client may read this route with the scope sync` |
| 404 | `the server returned HTTP 404 Not Found` |
| other | `the server returned HTTP <code>` |
| timeout | `the request timed out` |
| DNS failure | `cannot resolve the host name` |
| body past the limit | `the body of <url> is larger than the limit of <n> bytes` |

Every status message ends with the message the server itself returned, after a colon. A server that
answers `{"error": "..."}` contributes that string, so an empty deployment reads
`the server returned HTTP 404 Not Found: no content release is published` rather than the status
alone. A body that is not that JSON object is printed as it stands, which keeps a proxy's own page
readable. Either way the text is collapsed onto one line, loses every control character, and stops
at 200 characters. The two transport rows and the size row carry no such message. A download that fails for any reason after
its file was created removes that file.

## Provisioning file format

`KEY=VALUE` text, read once and then sealed. The file is read up to 64 KiB.

| Rule | Behavior |
|---|---|
| Blank line, or a line starting with `#` | Skipped |
| `export ` prefix | Accepted and stripped |
| One pair of matching `"` or `'` around a value | Stripped |
| A line with no `=` | Fails, naming the line number |
| An empty key, or a key outside `[A-Za-z0-9_.]` | Fails, naming the line number |
| A file with no `KEY=VALUE` line | Fails |
| An unknown key | Kept and stored, so a file for a later version round-trips |

| Key | Rule |
|---|---|
| `BRAINMAKER_API_BASE` | Required. Must be an `https://` URL with a host, and carry no surrounding whitespace. `http://` is accepted only for `localhost` or a loopback address. |
| `SWETSI_JWT_ENDPOINT` | Base of the OAuth2 routes. The same URL rule applies. A trailing slash is accepted and stripped. |
| `SWETSI_CLIENT_ID` | Client identifier. Printable ASCII, and no colon. |
| `SWETSI_CLIENT_SECRET` | Client secret. Printable ASCII. |
| The ten `*_PATH` keys | Optional. Each is a route under its base URL. A value holding `://`, a `..` segment, or a space fails. A leading `/` is stripped. |
| `SWETSI_TOKEN` | Refused. The key names the static token that earlier versions read. A file that carries it, and none of the three credential keys, fails. |
| `SWETSI_API_BASE` | Refused. The key is now `BRAINMAKER_API_BASE`. A file that carries the old name and not the new one fails. |

Key names carry two prefixes, because the two hosts can differ. `SWETSI_` names the service that
issues the token: `SWETSI_JWT_ENDPOINT`, `SWETSI_CLIENT_ID`, `SWETSI_CLIENT_SECRET`, and
`SWETSI_TOKEN_PATH`. `BRAINMAKER_` names the service that serves the content and the software.

The three credential keys are supplied together or not at all. An empty value counts as absent, so
`SWETSI_CLIENT_SECRET=` is the same as an absent line. None of the three means `brainmaker` sends
no `Authorization` header.

## `scripts/make-manifest.sh`

Writes the unsigned software manifest for a directory of built binaries.

```bash
scripts/make-manifest.sh <dist-dir> <version>
```

| Argument | Required |
|---|---|
| `<dist-dir>` | yes |
| `<version>` | yes |

The script takes no URL, and the manifest it writes holds none.

The script reads every file named `brainmaker-<version>-<platform-key>[.exe]` in `<dist-dir>`,
computes each SHA-256, and writes `<dist-dir>/manifest.json`. It exits 2 on a wrong argument count,
and exits 1 when the directory is absent, when the version fails the `[A-Za-z0-9.+-]{1,64}` check,
or when no matching file is present.

Its output is unsigned, and `brainmaker` refuses an unsigned manifest. Sign it with the tool below.

## `brainmaker-sign`

The signing tool. It is not part of the shipped binary, and a plain `cargo build` skips it.

```bash
cargo build --features sign --bin brainmaker-sign
```

| Command | Effect |
|---|---|
| `keygen <KEY-FILE>` | Write a new PKCS#8 Ed25519 key, and print its public key. On Unix the file has mode `0600` from the moment it is created. Fails when anything exists at the path, a symbolic link included. |
| `sign <KEY-FILE\|-> <MANIFEST-FILE> <ENVELOPE-FILE>` | Check the manifest, sign it, and write the envelope |
| `sign-content <KEY-FILE\|-> <ARCHIVE> <HASH> <ENVELOPE-FILE> [SEQUENCE]` | Digest the archive, sign `hash`, `sha256`, `size_bytes` and `sequence`, and write the envelope. The sequence is the time of signing unless `SEQUENCE` names one. |
| `verify <ENVELOPE-FILE> <PUBLIC-KEY>...` | Check an envelope against one or more public keys, the way `brainmaker` checks it |

Pass `-` in place of the key file to read the key from `$BRAINMAKER_SIGNING_KEY` as hexadecimal.
The release workflow uses that form, so the private key never reaches the runner's disk.

`sign` refuses a manifest that `brainmaker` could not use: one that is not JSON, one whose
`version` is not 1 to 64 characters of ASCII letters, digits, dots, hyphens, and plus signs, one
with no platform, or one whose `sha256` is not 64 hexadecimal characters.

`sign-content` refuses a hash that is not 8 alphanumeric ASCII characters, an empty archive, an
archive of more than 512 MiB, and a `SEQUENCE` that is not a whole number. It signs no URL, because
the client derives the download address itself.

`verify` accepts either shape and names which it read. It tells them apart by field: a payload with
`platforms` is a software manifest, and one with `sha256` is a content release. It then applies the
checks of `sign` or `sign-content` to the payload, and it also refuses a content release that
carries a `url` or a `sequence` that is not a whole number. Neither list of keys is implied: pass
the keys from `PUBLIC_KEYS` to check a manifest, and those from `CONTENT_KEYS` to check a content
release, because `verify` passes when any key given accepts.

Every command exits 0 on success and 1 on failure.
