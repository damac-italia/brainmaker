# Design: a machine-readable status, the health of the agent, and a bounded log

Status: proposal. This document builds nothing and changes no behavior.

Since version 0.1.6, a LaunchAgent installs signed builds every hour with no person present. Three gaps
make that agent hard to watch.

1. A script cannot ask a machine "are you current?". `status` prints text in fixed columns for a person.
   `self-update --check` exits 0 whether an update exists or not, and it prints nothing under `--quiet`.
2. A broken agent is silent. `link` prints one notice when `launchctl` cannot load the agent. After that,
   nothing reports whether the file exists, whether launchd holds it, when it last ran, or what it last
   printed. `state.json` records the time of the last install in `updated_at_unix`, and `status` does not
   print it.
3. `agent.log` grows without a limit. Every run appends a line, and every failed run appends more. Only
   `uninstall` removes the file.

The program already writes JSON for a machine in one place: `session-context` prints the JSON that the
hook reads. A JSON form of `status` fits the product. The open questions are about the contract: which
fields, which names, which exit codes, and which values stay stable. This document decides them in
writing, so that the build that follows is a plan and not a guess.

The prototype in [`src/schedule.rs`](../../src/schedule.rs) is small. One function, `tail_of`, cuts a log
text to its tail at the start of a line. Four tests check it. The function carries `cfg(test)`, so no
command changes.

## How to read the answers

Each point has an **Answer.** An answer that needs the maintainer says **Decision for the maintainer**
and carries an identifier such as D-1. An answer that no source confirms says **Not verified** and carries
an identifier such as V-1. It also says how to verify it. The section
[Open questions for the maintainer](#open-questions-for-the-maintainer) lists every one.

The sources:

- The code of this repository at commit `26debea`. The document names a function and a file. It gives a
  line number only where the line number is the point.
- The manual pages `launchctl(1)` and `launchd.plist(5)` on macOS 27.0 (`man launchctl`). The
  `launchctl(1)` page is dated 1 October 2014. The copy on the Apple archive site answered HTTP status
  404, so this document gives no URL for it.
- The documentation of `std::fs::File`: <https://doc.rust-lang.org/std/fs/struct.File.html>. The text
  matches the source of the standard library on the machine that wrote this document (Rust 1.97.1).
- Five checks on macOS 27.0, made while this document was written. They touch only scratch files outside
  the repository, and they never change the state of launchd. The text names each one where its result
  appears.
- [scheduled-agent.md](scheduled-agent.md), for the agents on Linux and on Windows.

The limits of this work:

- No Linux system and no Windows system was available. The answers for those systems come from
  scheduled-agent.md, and they inherit its **Not verified** marks.
- No `launchctl` verb that changes state ran: no `bootstrap`, `bootout`, `kickstart`, `enable`, or
  `disable`. So no answer here describes how launchd itself opens the log file (V-9).
- No real `agent.log`, no real settings file, and no real `status` output was read. Every example uses
  `api.example.test` and made-up values.
- Plans 003 (the run lock), 009 (the content sequence), and 013 (the key binding) are not in the tree. This
  document treats their planned behavior as fixed, and it names each place where it does so.

## What stays fixed

Six rules bind every decision below.

| # | Rule | What it means here |
|---|---|---|
| 1 | No credential and no token in any output | `status` prints the lengths of the credentials, never the values. The JSON holds neither |
| 2 | An endpoint is confidential toward the public, and not toward the user of the machine | A JSON document travels more easily than text. Section "The two URLs" decides the case |
| 3 | Only `src/main.rs` prints | Other modules return values, and `main` prints them |
| 4 | No new dependency | `serde` and `serde_json` are already in `Cargo.toml` |
| 5 | `status` changes nothing on disk | A probe opens a file for reading and never creates one. See the note below |
| 6 | The documents use Simplified Technical English | This document too |

Note on rule 5. `status` writes nothing itself. `Config::load` runs first, and on a first run it can
import a provisioning file: it seals the settings and removes the file. Every command that loads settings
does the same, and this design keeps that. It matters for the JSON, because `Config::load` reports through
the `log` closure, and that closure prints to standard output. Point 1 of section "status --json" says
what to do.

## status --json

### 1. The flag

**Answer.** The flag is `status --json`. It prints one JSON document on standard output in place of the
text rows.

It is a flag of `status`, and not a new command, for three reasons.

1. `status` already reads every fact. A second command needs a second copy of that code, and the two
   copies drift.
2. `check_options` in `src/cli.rs` holds one row for each flag. A new flag is one more row: `--json`
   applies to `status`. Every other command refuses it, with the message that the table already gives for
   a flag that has no effect.
3. `session-context` is a command because it has another job. It builds text for the model. It does not
   describe the machine.

**`--quiet`.** The document still prints. The reader asked for it. `session-context` also prints past
`--quiet`, and so do the text rows of `status` today, because they do not go through `log`.

**Standard output holds the document and nothing else.** One thing breaks this. `Config::load` reports
through the `log` closure of `run`, and that closure prints to standard output. On a first run it prints
`Imported the configuration from ...`. With `--json`, `run` must build a `log` that prints to standard
error. `--quiet` still silences it.

**One line.** The program prints the document on one line and ends it with a line break, as
`session-context` does. A log collector then stores one machine on one line. A person pipes the text to
`jq`. The examples below add line breaks for reading.

### 2. The fields

**Answer.** The table holds every field. Six rules shape it.

1. A name is snake_case. A group of related fields is an object.
2. A time is a number of seconds since the Unix epoch. Its name ends in `_unix`.
3. A fact with two values is a boolean. A fact that can be unknown is a word: a string from a fixed list
   (point 3).
4. A value that does not exist is `null`. The text rows write `<none>` and `<unknown>`, and the JSON never
   does.
5. A field is never left out, except the fields that the last column names.
6. The order of the fields is not part of the contract.

Every row of the text output has a field below. At the base commit the rows are `root`, `content`,
`store`, `settings`, `api base`, `token url`, `auth`, `key`, `signing`, `installed`, `present`, `latest`,
`state`, `software`, `platform`, `published`, and `update`. The rows that other plans add, and the agent
rows, have fields too.

| Field | Type | Text row | Absent, or `null` |
|---|---|---|---|
| `schema` | integer | none | never. The value is 1 |
| `generated_at_unix` | integer | none | never |
| `root` | string | `root` | never |
| `content_dir` | string | `content` | never |
| `store_path` | string | `store` | never |
| `settings.source` | word | `settings` | never |
| `settings.imported_from` | string | `settings`, the path | absent unless the source is `imported` |
| `settings.import_file_removed` | boolean | `settings`, the words in brackets | absent unless the source is `imported` |
| `api_base` | string | `api base` | absent without `--endpoints` |
| `token_url` | string | `token url` | absent without `--endpoints`. `null` when no credential is set |
| `auth.kind` | word | `auth` | never |
| `key_class` | word | `key` | never |
| `binding.kind` | word | `binding` (plan 013) | absent on a build before plan 013 |
| `binding.weak` | boolean | `binding` (plan 013), the word `weak` | absent on a build before plan 013 |
| `signing.software_keys` | integer | `signing` | never |
| `signing.content_keys` | integer | `signing` | never |
| `content.installed_hash` | string | `installed` | never. `null` when nothing is installed, or when `state.json` is missing or damaged |
| `content.installed_at_unix` | integer | `updated` (new row) | never. `null` when nothing is installed |
| `content.sequence` | integer | `sequence` (plan 009) | absent before plan 009. `null` when `state.json` holds none |
| `content.present` | boolean | `present` | never |
| `content.latest_hash` | string | `latest` | never. `null` when the server gives no usable release |
| `content.latest_sequence` | integer | none | absent before plan 009. `null` when the release has none, or the server gives none |
| `content.state` | word | `state` | never |
| `content.state_reason` | string | `state`, the text after `cannot check:` | absent without `--endpoints`. `null` unless the state is `cannot_check` |
| `software.version` | string | `software` | never |
| `software.platform` | string | `platform` | never |
| `software.published_version` | string | `published` | never. `null` when the manifest is not usable |
| `software.update` | word | `update` | never |
| `software.update_reason` | string | `update`, the text after `cannot check:` | absent without `--endpoints`. `null` unless the update is `cannot_check` |
| `locks.install` | word | `lock` (plan 003) | absent before plan 003 |
| `locks.update` | word | `lock` (plan 003) | absent before plan 003 |
| `agent.kind` | word | `agent` | never |
| `agent.definition` | word | `agent` | never. `null` when the kind is `none` |
| `agent.loaded` | word | `agent` | never. `null` when the kind is `none` |
| `agent.last_run` | object | `agent run` | never. `null` when the kind is `none`, or when the log is missing or empty |
| `agent.last_run.at_unix` | integer | `agent run` | never inside the object |
| `agent.last_run.error_logged` | boolean | `agent run` | never inside the object |

**Details that the JSON leaves out on purpose.** Three details of the text rows have no field.

- The two lengths and the scope in the `auth` row (D-1).
- The command in `update    available; run <command>`. A script knows its own program.
- The platform list in `update    not built for this platform; the manifest offers ...`. The word
  `not_built_for_platform` carries the fact.

One rule decides these cases: when in doubt, leave the field out. A later release can add a field. No
release can remove one (point 6).

**Decision for the maintainer (D-1).** The JSON carries no credential length and no scope. `auth.kind` is
the word `absent` or the word `client_credentials`. The lengths help a person who checks a paste, and the
text row keeps them for that person. A script has no use for them. A length is not a credential, so rule 1
does not forbid it. But a length in the contract cannot leave it. `auth` is an object so that the
maintainer can add the two lengths to it later, without a break.

**The lock fields.** Plan 003 adds two lock files under the root. `sync` holds `.lock` while it installs,
and `self-update` holds `.update.lock` while it replaces the program. `status` reads each one this way:
it opens the file for reading only, and it asks for a shared lock without waiting (`File::try_lock_shared`).
A file that does not exist reads `free`. A refused lock reads `held`. Any other error reads `unknown`. The
probe creates no file, so rule 5 holds. It holds a shared lock for an instant. Plan 003 asks for the
install lock in two places. The install waits and asks again every 200 ms, so it loses one poll at most.
The restore of stranded content does not wait, so in that rare case it skips its work, and the next run
does it. Neither effect does harm.

A `held` reading does not show a fault by itself, because an install holds the lock during a download and
an extraction. The same reading on two calls an hour apart shows a stuck run.

Checked on macOS 27.0 with a scratch program that lives outside the repository. The program opened a file
that did not exist. The open failed with `NotFound`, and no file appeared. While a second process held an
exclusive lock, the probe read `held`. After that process ended, the probe read `free`. The documentation
of `File` names `flock` on Unix and `LockFileEx` on Windows. On Windows, a file must be open with
`.read(true)`, `.read(true).append(true)`, or `.write(true)` for a lock. A read-only open is in that list.

Not verified (V-1). That the probe reads `held` on Linux and on Windows. To verify: run the test
`peek_reports_a_held_lock` of the build in the CI jobs of both systems.

**The sequence.** Plan 009 makes the content sequence an unsigned 64-bit integer. A JSON number above
2^53 (9,007,199,254,740,991) loses digits in a reader that uses floating point, such as JavaScript. The
signer writes the time of signing, which stays far below that limit in seconds and in milliseconds.

Not verified (V-2). The unit that plan 009 picks. If it can pass 2^53, for example nanoseconds, the two
sequence fields become strings. To verify: read plan 009 before the build.

### 3. Values that are words

**Answer.** The rows `state` and `update` each hold a fixed meaning and, sometimes, a free text. The JSON
splits each row into a word in one field, and the free text in a second field. Only `--endpoints` adds the
second field (point 5). The table lists every word.

| Field | Word | Meaning |
|---|---|---|
| `settings.source` | `imported` | This run imported a provisioning file |
| | `sealed_store` | The sealed store held the settings |
| | `environment` | The environment supplied the settings, and no store was needed |
| `auth.kind` | `absent` | No credential is set, and the run sends no `Authorization` header |
| | `client_credentials` | The client-credentials grant is set |
| `key_class` | `release` | The binary carries the release key |
| | `development` | The binary carries the published development key |
| `binding.kind` | `machine_identifier` | The sealed store is bound to the machine identifier (plan 013) |
| | `home_directory_path` | The store is bound to the home directory path. `binding.weak` is true |
| | `none` | The store is bound to nothing. `binding.weak` is true |
| `content.state` | `up_to_date` | The installed hash equals the latest hash, and `content/` is a directory |
| | `stale` | The installed hash differs from the latest hash, or the two match and `content/` is missing. `content.present` tells which |
| | `not_installed` | No hash is installed |
| | `cannot_check` | The server gave no release that a key accepts. The cause can be the network, an HTTP status, or a failed signature |
| | `not_newer` | Plan 009 only. The release has no higher sequence than the installed one, so `sync` refuses it |
| `software.update` | `none` | No newer build is published |
| | `available` | A newer build exists for this platform |
| | `not_built_for_platform` | A newer version exists, and the manifest has no build for this platform |
| | `cannot_check` | The manifest is unreadable, or it fails its signature check |
| `locks.install`, `locks.update` | `free` | No run holds the lock, or the lock file does not exist |
| | `held` | A run holds the lock now |
| | `unknown` | The probe failed |
| `agent.kind` | `launchd` | The system has a LaunchAgent (macOS) |
| | `systemd` | The system has a systemd user timer (Linux, plan 017) |
| | `task` | The system has a scheduled task (Windows, plan 017) |
| | `none` | The system has no agent |
| `agent.definition` | `missing` | The definition file does not exist |
| | `current` | The text of the file equals what `link` writes now |
| | `stale` | The text of the file differs |
| | `unknown` | `status` cannot read the file, or cannot compute the text |
| `agent.loaded` | `yes` | The system holds the agent (Agent health, point 1) |
| | `no` | The system does not hold the agent |
| | `unknown` | The check did not run |

**Decision for the maintainer (D-2).** The word `not_newer`. Plan 009 refuses a release whose sequence is
not higher. `status` must say so. With the word `stale`, the reader expects an update, and none comes. This
design reserves the word `not_newer`, and plan 009 or the build must confirm it.

### 4. Time

**Answer.** The JSON gives the number alone: `content.installed_at_unix`, `generated_at_unix`, and
`agent.last_run.at_unix`. It gives no ISO 8601 text.

1. A number is exact and needs no time zone. ISO text needs a rule for the zone. UTC text needs the
   conversion from a day count to a calendar date, and the program has no date library. That conversion is
   about ten lines of integer arithmetic with its own tests. It is possible, and it is new code.
2. Two forms of one fact can disagree. One form cannot.
3. A script that needs a calendar date converts the number with `date`.
4. A later release can add an ISO field without a break. The reverse is not possible.

`generated_at_unix` is the time that `status` reads the clock, once, before it asks the server. Every age
in the text rows uses the same reading. A consumer can then compute each age with the clock of the machine.
A difference between that clock and the clock of the consumer does not matter.

The text rows show an age, and not a date: the largest whole unit of seconds, minutes, hours, or days, such
as `2 days ago`. A subtraction makes that text.

### 5. The two URLs

**Recommendation.** The JSON omits `api_base` and `token_url`. A second flag, `--endpoints`, adds them.
The same flag adds the two reason texts, `content.state_reason` and `software.update_reason`.

The reason texts belong to the flag because they name the URL. `remote::fetch_text` adds
`cannot read <url>` to every failure of a request, and `<url>` is the base URL and the route. A JSON that
leaves out `api_base` and then prints such a reason still discloses the base URL. The agent log has the
same property (Agent health, point 4).

The default leaves the URLs out for one reason. Rule 2 does not say that the endpoint is a secret from the
user of the machine: [SECURITY.md](../SECURITY.md) says that every employee who receives the distribution
can recover it. Rule 2 says that the endpoint does not spread. A JSON document spreads more easily than
text: a script sends it to a ticket, a dashboard, or a chat with one command. SECURITY.md already asks a
person to paste parts of `status` into a report. A default without the URLs keeps the common case safe. The
flag serves the person who wants them.

The rule for the documents: **`--endpoints` adds only values that `status` prints today. It adds nothing
new. Without it, the JSON holds fewer of those values than the text does.** The text output does not change.

`--endpoints` has an effect only with `--json`. The parser refuses it alone, as it refuses every flag that
has no effect.

**Decision for the maintainer (D-3).** Whether the default JSON omits the URLs and the reason texts. This
decision is hard to take back after a release. A field in the default document cannot leave it. A field
behind the flag can move into the default at any time.

### 6. Stability

**Answer.** Four promises.

1. A field is never renamed and never removed within one `schema` number.
2. A release can add a field. A consumer must ignore a field that it does not know.
3. A word keeps its meaning. A release can add a word. A consumer must read a word that it does not know
   as `unknown`.
4. A change that breaks a promise needs a new `schema` number and a new flag value, such as `--json=2`.
   `--json` alone stays schema 1.

The document carries `"schema": 1`. The program is at version 0.1.6, and a promise tied to "a major
version" binds nothing before 1.0. A number in the document holds the promise by itself. A consumer reads
`schema`, and it stops when the value is not 1.

**Decision for the maintainer (D-4).** The `schema` field, and the names of the two flags, `--json` and
`--endpoints`.

### 7. Errors

**Answer.** `status` fails only when the settings do not load. Then `status --json` prints nothing on
standard output, prints `error: <reason>` on standard error as it does today, and exits 1.

The design uses no JSON error object, for three reasons.

1. A consumer already handles the exit status and standard error of every command. An error object adds a
   second shape that each consumer must test for.
2. The error text can name a path or a URL (rule 2), and the default document must not.
3. The cases are few. The settings are missing, they do not open, or a value fails a check. A consumer that
   reads exit 1 and an empty output knows that the machine is not provisioned, or that its store does not
   open. That is the whole message.

Every other failure is a value in the document, and not an exit. Examples: an unreachable server, an
unreadable log, and a `launchctl` that does not run. `status --json` exits 0 whenever it prints the
document, even when every check fails. A monitor reads the fields, and not the exit status.

### 8. Example

A macOS machine that is current, with plans 003, 009, and 013 in the tree. The program prints the
document on one line. The example adds line breaks and indentation.

```json
{
  "schema": 1,
  "generated_at_unix": 1790683200,
  "root": "/Users/you/.brainmaker",
  "content_dir": "/Users/you/.brainmaker/content",
  "store_path": "/Users/you/.brainmaker/confidential/config.enc",
  "settings": {
    "source": "sealed_store"
  },
  "auth": {
    "kind": "client_credentials"
  },
  "key_class": "release",
  "binding": {
    "kind": "machine_identifier",
    "weak": false
  },
  "signing": {
    "software_keys": 1,
    "content_keys": 1
  },
  "content": {
    "installed_hash": "a1b2c3d4",
    "installed_at_unix": 1790500000,
    "sequence": 1790498500,
    "present": true,
    "latest_hash": "a1b2c3d4",
    "latest_sequence": 1790498500,
    "state": "up_to_date"
  },
  "software": {
    "version": "0.1.6",
    "platform": "darwin-arm64",
    "published_version": "0.1.6",
    "update": "none"
  },
  "locks": {
    "install": "free",
    "update": "free"
  },
  "agent": {
    "kind": "launchd",
    "definition": "current",
    "loaded": "yes",
    "last_run": {
      "at_unix": 1790681400,
      "error_logged": false
    }
  }
}
```

`--endpoints` adds four fields. The document below shows them for a machine that cannot reach the server.
Each key goes into the object of the same name in the document above. The two reason texts name the base
URL, which is why they sit behind the flag.

```json
{
  "api_base": "https://api.example.test/v1/brainmaker",
  "token_url": "https://api.example.test/swetsi/v1/oauth2/token",
  "content": {
    "state": "cannot_check",
    "state_reason": "cannot read https://api.example.test/v1/brainmaker/content/latest: the request timed out"
  },
  "software": {
    "update": "cannot_check",
    "update_reason": "cannot read https://api.example.test/v1/brainmaker/software/brainmaker: the request timed out"
  }
}
```

On a machine that cannot reach the server, the default document has `"state": "cannot_check"`,
`"update": "cannot_check"`, `"latest_hash": null`, and `"published_version": null`. It has no reason
text and no URL.

## Agent health

The rows apply to macOS now. [scheduled-agent.md](scheduled-agent.md) designs the agent for Linux and for
Windows. So no field name uses a launchd term. One field, `agent.kind`, says which mechanism a machine has.

| Field | macOS (built) | Linux (plan 017) | Windows (plan 017) |
|---|---|---|---|
| `agent.kind` | `launchd` | `systemd` | `task` |
| The definition | the property list | the service file and the timer file | the task XML file |
| `agent.loaded` is `yes` when | `launchctl print gui/<uid>/it.damac.brainmaker` exits 0 | `systemctl --user is-enabled brainmaker.timer` and `is-active brainmaker.timer` both exit 0 | `schtasks.exe /Query /TN <name> /XML` exits 0, and the XML holds `<Enabled>true</Enabled>` |
| The log | `<root>/agent.log` | the same | the same |

The row `agent.loaded` follows the definition in section "Code", point 5, of scheduled-agent.md. On a system
with no agent, `agent.kind` is `none`, and `agent.definition`, `agent.loaded`, and `agent.last_run` are
`null`.

### 1. Facts to show

Four facts. `status` reads each one without a change to the machine.

**The definition exists.** `status` reads the file that `Agents::plist()` names, with
`fs::read_to_string`. A missing file gives `missing`. Any other read error gives `unknown`.

**The definition is current.** `status` compares the file with the text that `link` writes now.
`render` needs two inputs: the command prefix, and the log path. `status` has both, because both come from
the root. The prefix is `command_prefix(&installed_program(root), Some(root))`. The log path is
`log_path(root)`. `status` has `config.root()`. It does not need the path of the running program: `link`
names the copy under the root, and never the running program.

The build must not copy that logic. It adds one function in `schedule.rs` that returns the files and their
text for an `Agents`, a prefix, and a root. `install` writes what it returns, and `status` compares with
it, so the two cannot drift. `link` gets a small public function, `program_prefix(root)`, and `link` and
`status` both call it. `schedule` takes the prefix as an argument, as `install` does, so `schedule` keeps
no dependency on `link`.

`stale` means that the text differs. Three causes give it.

1. A newer program writes another script.
2. `link --dir` wrote the agent for another root. The agent belongs to the account, and one label serves
   one root.
3. A person edited the file.

The remedy is the same for all three: run `link`.

**The system holds the agent.** On macOS the verb is `launchctl print gui/<uid>/it.damac.brainmaker`. It
names the same domain that `install` and `remove` use with `bootstrap` and `bootout`. The uid is the owner
of the home directory. `launchctl` runs by its full path, `/bin/launchctl`, with its arguments as a list.

The manual says that the output of `print` is not an API. The output has no fixed format, and it can
change between releases. A script must not rely on it. So `status` uses the exit status only. The manual
says that the exit status is 0 when the subcommand succeeded. Any other value is an error code that
`launchctl error` decodes. `status` reads 0 as `yes`, and any other status as `no`. It reads a `launchctl`
that does not run as `unknown`.

Checked on macOS 27.0. Two system agents that are loaded, `com.apple.Dock.agent` and `com.apple.Finder`,
gave exit status 0. A service that does not exist gave 113. `launchctl error` decodes 113 as "Could not
find specified service". A `gui` domain that does not exist gave 112. The manual documents neither number,
so `status` does not branch on them. Both mean that launchd does not run the agent for this account now.

The manual says that the output of the legacy verb `launchctl list <label>` is meant to match older
releases. But that verb picks its domain by whether the caller is root, and it does not name `gui/<uid>`.
`link` loads the agent into `gui/<uid>`, so `status` asks that domain.

Not verified (V-3). That every macOS version that brainmaker supports exits non-zero for a service that is
not loaded. The check ran on 27.0 only. To verify: run the verb with an absent label on the oldest macOS
version that the fleet runs.

Not verified (V-4). That the owner of the home directory is the uid of the account that runs `status`. A
run under `sudo` can differ. To verify: run `sudo brainmaker status`, and compare with `id -u`.

Not verified (V-5). The checks for Linux and for Windows. They come from scheduled-agent.md and inherit its
marks. Two matter most: L-5 (the error of `systemctl --user` without a manager) and W-7 (the exit status of
`schtasks /Query` for a task that does not exist). To verify: follow the rows of that document.

**The last run and the last error.** The macOS script starts each run with `date`, so the log holds one
`date` line for each run. The run that wrote the last `date` line is the last run. To read the time from
that line, a program must parse the text. That is fragile for four reasons.

1. The format depends on the locale. The table shows the result of `date` for three of the eight locales
   that were checked, at one moment on macOS 27.0.

   | Locale | Output of `date` |
   |---|---|
   | `C` | `Tue Sep 29 00:12:49 CEST 2026` |
   | `de_DE.UTF-8` | `Di. 29 Sep. 2026 00:12:49 CEST` |
   | `zh_CN.UTF-8` | `2026年 9月29日 星期二 00时12分49秒 CEST` |

2. The line names the zone with an abbreviation and gives no offset. An abbreviation such as `CST` can
   name more than one zone.
3. The program has no date library.
4. The scripts for Linux and for Windows write other text: `date` under the environment of the user
   manager, and `%date% %time%` under `cmd.exe`.

A LaunchAgent normally starts with no `LANG` or `LC_*` variable, so its `date` line probably has the first
form. The design does not need that to hold.

Not verified (V-6). That the `date` line of the LaunchAgent has the `C` form. To verify: read the
`agent.log` of a machine with a non-English system language.

Not verified (V-7). That `%date%` on Windows follows the regional settings. To verify: change the regional
format, and run `echo %date%`.

**Answer: read the file, and not the text.** Every run writes at least its `date` line. So the
modification time of `agent.log` is the time of the last write of the last run. That time is between the
start and the end of that run, and a run lasts seconds while the schedule is hourly. The time needs no
parse and no change of the script. It works on every machine that has the agent, including a machine that
never runs `link` again. It is `agent.last_run.at_unix`.

The last line of the log decides `agent.last_run.error_logged`. Both commands run with `--quiet`, so a run
that works prints nothing after its `date` line. `main` prints every failure as a line that starts with
`error: `. So when the last non-empty line of the log starts with `error: `, the last run failed. `status`
reads the last 16 KiB of the file, so a big log costs no more than a small one.

Two gaps remain.

- `status` does not see a crash that prints no `error: ` line. A release build turns a panic into an
  abort, and the panic message does not start with `error: `.
- A run in progress shows only its `date` line. It reads as a run with no error.

**Recommended change of the script.** A line of a fixed shape closes both gaps. Every line after the last
such line belongs to the last run, and a panic message is one of those lines. Two ways give that line.

1. **`brainmaker` writes the line.** This way needs product decision 7 of scheduled-agent.md: one new
   command that runs the update and then the sync. That command writes one line at the start of each run,
   with the time in seconds. It writes one line at the end, with the result of each step. A run with a
   start line and no end line did not end. The format is the same on all three systems, so the locale
   problem and the `%date%` problem go away. The same command can cut the log itself. That is the best
   home for the code of section "Log size".
2. **The script writes a line of one shape.** Use this way if the maintainer does not accept that command.
   Change the first command of the macOS script and of the Linux script from `date` to
   `date -u +%Y-%m-%dT%H:%M:%SZ`. Each run then starts with a line such as `2026-09-28T22:12:49Z`, and a
   program recognizes the line by its shape. On macOS 27.0 the command printed the same text under
   `de_DE.UTF-8`, `fa_IR`, and `zh_CN.UTF-8`. Windows keeps the file time.

Not verified (V-8). That `date -u +%Y-%m-%dT%H:%M:%SZ` prints the same text in every locale on GNU `date`.
To verify: run it under three locales on Debian and on Fedora.

**Rollout.** A change of the script reaches a machine only at the next `link`, and nothing runs `link` by
itself. `self-update` does not fix it: it replaces the program, and `link` owns the agent files. So after
any change of the script, every machine reads `agent.definition` as `stale` until someone runs `link`. The
reading is right. It is the only signal that a machine needs `link`. Therefore change the script once, for
all reasons together: D-5, D-6, and product decision 7 of scheduled-agent.md.

**Decision for the maintainer (D-5).** Which change of the script, if any: way 1 or way 2. The build that
follows works without either. The recommendation is way 1, in the same build as the command of product
decision 7. Way 2 is the fallback.

**What the log does not show.** The agent runs both commands with `--quiet`, and `report` prints its
notices only without `--quiet`. So two outcomes never reach the log.

- `sync` returns `Unreachable` for any failure of `remote::latest_release` while content is installed and
  `--force` is absent. That includes a failed signature check on the content release.
- Plan 003 adds `Busy`, which `report` also prints only without `--quiet`.

`self-update` differs: `main` prints any failure of it as an `error:` line. So a machine that cannot reach
the server logs one `error:` line each hour from `self-update`, and none from `sync`. A machine whose server
answers, but whose content release fails the signature check, logs no error line. Only `status` shows it:
`content.state` is `cannot_check`.

**Decision for the maintainer (D-6).** Whether the agent's `sync` stops hiding these outcomes. The change is
to drop `--quiet` from the `sync` in the script. The log then holds two progress lines for each run, and
the notice lines. That adds about 75 bytes for each run, and the cap of section "Log size" still bounds the
log. It is a change of the script, so it reaches a machine at the next `link`. The recommendation is to
make it, in the same change as D-5.

### 2. Where the rows go

The text output keeps every row that it has, in the same place. New rows go in these places. A label has at
most 9 characters, so every value still starts at column 11.

| Row | Value | Example |
|---|---|---|
| `binding` | plan 013 puts it after `key` | `machine identifier` |
| `updated` | the age of the install, after `installed` | `2 days ago`, or `<none>` |
| `sequence` | plan 009, after `updated` | `1790498500`, or `<none>` |
| `lock` | plan 003, after `update` | `install free, update free` |
| `agent` | the kind, the definition, and the load state | `launchd, definition current, loaded` |
| `agent run` | the age of the last run, and the error word | `30 minutes ago, no error logged` |

The complete output for the machine of the example:

```text
root      /Users/you/.brainmaker
content   /Users/you/.brainmaker/content
store     /Users/you/.brainmaker/confidential/config.enc
settings  read from the sealed store
api base  https://api.example.test/v1/brainmaker
token url https://api.example.test/swetsi/v1/oauth2/token
auth      client-credentials grant, scope sync, client id 24 characters, secret 40 characters
key       release
binding   machine identifier
signing   1 trusted software key(s), 1 trusted content key(s)
installed a1b2c3d4
updated   2 days ago
sequence  1790498500
present   yes
latest    a1b2c3d4
state     up to date
software  0.1.6
platform  darwin-arm64
published 0.1.6
update    none
lock      install free, update free
agent     launchd, definition current, loaded
agent run 30 minutes ago, no error logged
```

The load state reads `loaded`, `not loaded`, or `load state unknown`. A system with no agent prints
`agent     none` and no `agent run` row. A missing log prints `agent run <none>`.

### 3. `--claude-dir` and `--agent-dir`

**Answer.** `status` reads the default agent location only: the one that `Agents::resolve(None, false)`
returns. Three reasons.

1. An agent in the folder that `--agent-dir` names is never loaded. It belongs to a test, and it has
   nothing to report about the system.
2. Plan 014 makes the parser refuse both flags with `status`, and this design keeps that rule. It needs no
   new parser row for them.
3. The build gives the function that reads the health an `Agents` value as an argument. A test passes
   `Agents::unloaded(dir)`, and it never reads the real folder. For an `Agents` that never loads,
   `loaded` reads `unknown`, and `launchctl` does not run.

One consequence follows. The agent belongs to the account, and not to a root. `status --dir <other>`
compares the agent with the text for `<other>`, so it reads `stale`. That is right: the agent does not
serve that root.

### 4. Privacy

The last error line can hold a URL, because an error message names the URL that failed. Two more
properties of the line matter.

- The line can hold text that the server chose. `message_from_body` in `src/remote.rs` collapses the
  whitespace of an error body and stops at 200 characters. It removes no other character. A control
  character in the body reaches the line, and reaches a terminal that prints the line. A scratch program
  that applies the same collapse to a body with an escape character kept that character.
- The line does not belong in a document that a script copies to a ticket.

**Answer.** `status` never prints the line, in the text and in the JSON, with or without `--endpoints`. It
prints the time and the word `error logged`. The JSON has the two fields `at_unix` and `error_logged`. A
person who needs the text reads `<root>/agent.log`. The root is in the first row of the output.

This keeps the rule of section "The two URLs" simple. The flag adds only values that `status` prints today,
and `status` does not print this line today. So the line never joins the default document, and it needs no
flag.

**Decision for the maintainer (D-7).** That `status` never prints the last error line. The decision is hard
to take back, for the same reason as D-3: a field that is in the contract cannot leave it. A later release
can add a field behind `--endpoints`, if the maintainer wants the line there.

## Exit codes

### 1. The need

A script asks: is an update available? Today it must read text. `self-update --check` prints
`Version <latest> is published for <platform>. This binary is <current>.` when an update exists. It prints
`brainmaker <version> is the newest published build.` when none exists. Both lines go through `log`, so
`--quiet` removes them. Then the command prints nothing, and it exits 0 in both cases. A script that wants
a silent check has no answer.

### 2. The risk of a change

The LaunchAgent, the `SessionStart` hook, and the workflows of this repository never pass
`self-update --check`. A search of `src/link.rs`, `src/schedule.rs`, `.github/workflows`, and `scripts`
found no `self-update --check`. So a change of the exit status of `--check` does not touch them.

A person's script can pass the flag. With (a), a script that runs
`brainmaker self-update --check && next-step` skips `next-step` when an update exists. A script with
`set -e` ends at that line. Both worked before, so the change breaks them.

### 3. The choice

Two ways.

- **(a)** A new exit code, for example 10, for "an update is available, and `--check` was given".
- **(b)** No change to `--check`. The answer comes from `status --json`: `software.update` is `available`.

**Recommendation: (b).** It breaks nobody, and it adds nothing to the interface. The cost: `status` makes
three requests where `self-update --check` makes two. `status` also asks for the content release. A script
needs a JSON reader such as `jq`, and it needs steps 1 and 2 of section "Build steps".

A variant of (a) breaks nobody: a flag that gives the new code only when asked, for example
`self-update --check --exit-code`. `git diff --exit-code` is a known example of the same idea. This design
does not recommend the variant as a first step. It adds a flag to the interface, and `status --json`
already answers the question.

**Decision for the maintainer (D-8).** (a) or (b). The recommendation is (b).

### 4. If the maintainer chooses (a)

The build changes these places. The list comes from a search of `src/cli.rs`, `docs/API.md`, and
`README.md` for `EXIT CODES`, `exit code`, `exits 0`, and `exits 1`, at the base commit. It adds a second
search for `--check`, and one table that the first search misses.

| Place | Change |
|---|---|
| `src/cli.rs`, line 84, the block `EXIT CODES` | Add the row for the new code |
| `src/cli.rs`, line 37, the option `--check` | Say that the exit code differs |
| `docs/API.md`, lines 65 to 70, the table "Exit codes" | Add the row. The first search misses this table, because its heading is `Exit codes` |
| `docs/API.md`, line 31, the option row `--check` | Say that the exit code differs |
| `README.md`, line 318, the option row `--check` | The same |
| `docs/FLOW.md`, line 253 | Add the exit code to the sentence about `--check` |
| `docs/API.md`, lines 72 to 83, and `README.md`, lines 58 and 157 to 159 | No change. They describe `sync`, the hook, and `uninstall` |
| `docs/API.md`, lines 396 and 431 | No change. They describe `scripts/make-manifest.sh` and `brainmaker-sign` |

`run()` returns `Result<()>` today, and `main()` maps `Ok` to `ExitCode::SUCCESS` and `Err` to
`ExitCode::FAILURE`. The build changes `run()` to return `Result<ExitCode>`. Every command returns
`Ok(ExitCode::SUCCESS)` as today. `self_update` returns the new code in the `Newer` branch when
`args.check_only` is true. `main()` returns the `ExitCode` that `run()` gives. A small pure function,
`check_exit(&Check, check_only)`, holds the decision, so a test can call it without a network.

## Log size

`agent.log` gets a line for each run, and a line for each failure. Only `uninstall` removes it (it is in the
list of `remove_root`). Nothing else cuts or rotates it.

### 1. The options

**(a) The script cuts the file before it writes.**

- The path needs the shell quoting that `link` already writes, then the XML escape of the property list. On
  Linux it also needs the two extra layers of the systemd line. On Windows the script needs a tail, and
  `cmd.exe` has no built-in `tail`.
- The change reaches a machine only at the next `link`.
- launchd names the log in `StandardOutPath` and in `StandardErrorPath`, so launchd opens the file before
  the script starts. A script that replaces the file by a rename leaves those descriptors on the old file.
  The rest of the run then writes to a file that has no name. A script that truncates the file in place
  keeps the name. Whether that is safe depends on V-9.

**(b) `brainmaker` cuts the file at the start of `sync`, when it is past a cap.**

- The code is in the binary, so it reaches every machine at the next `self-update`, and no `link` is
  needed. This is the only option that reaches machines that are installed now.
- The open-file question is the same as in (a). The cut must keep the file in place: read the tail, set the
  length to 0, and write the tail at the start.
- `status` must not do it. Rule 5 forbids a write.

**(c) Rotation to `agent.log.1`.**

- The rename moves the file that launchd holds open. The rest of the run writes to `agent.log.1`, and
  `agent.log` does not exist until the next run. `status` then finds no log right after a rotation.
- The worst case doubles the space.
- `uninstall` removes named paths, and `remove_root` names `agent.log` only. It must also name
  `agent.log.1`, or it leaves that file and the root.

Checked on macOS 27.0 with a shell that holds a `>>` redirect open while another process cuts the file. This
is the redirect that the Linux unit uses. It is not launchd's own descriptor.

- A cut in place: the lines that the shell wrote after the cut land after the kept line, with no gap. `cat
  -v` showed no `^@` byte.
- A cut by rename: the lines that the shell wrote after the cut were lost. They went to the old file, which
  had no name.

Not verified (V-9). That launchd opens the log in append mode. `launchd.plist(5)` says that the writes of
the job go to the file, and that the error file opens for reading and writing. It does not say whether the
open appends. If it does not append, a cut in the middle of a run leaves a gap of zero bytes. The next cut
reads that gap as an empty log. The cost is one lost log. To verify: load a scratch agent under a test
label whose job prints a line every second. Cut its log in place from a shell. Read the file with `cat -v`.
A `^@` byte means a gap.

Not verified (V-10). That the cut in place works on Linux and on Windows while the redirect of the script
holds the file. The Linux redirect is the shell redirect of the check above. On Windows the sharing mode of
the `cmd.exe` redirect can refuse a second writer. A failed cut is not an error of `sync`, so the log then
grows until a cut works. To verify: cut the file from a second process while the redirect writes, on both
systems.

### 2. The numbers

**Answer.** The cap is 1 MiB (1,048,576 bytes). After a cut the log holds the last 256 KiB (262,144 bytes),
cut at a line start.

The estimate assumes 24 runs a day, as the hourly trigger gives. A logon adds a few. A `date` line is 30
bytes. A failed `self-update` adds one `error:` line of 108 to 163 bytes for an endpoint such as
`api.example.test`, and a longer endpoint adds more. `sync` adds nothing under `--quiet`.

| Machine | Bytes each day | Days to reach the cap | Days to refill after a cut |
|---|---|---|---|
| Every run works | 720 | about 1,450 | about 1,090 |
| The server is out of reach | 3,300 to 4,600 | 230 to 320 | 170 to 240 |

A cut therefore happens at most about twice a year. The gap between the cap and the target stops a cut at
every run. 256 KiB holds more than 2,000 lines, which is more history than a person reads.

### 3. The recommendation

**Answer.** Option (b): cut in place, at the start of `sync`. Three reasons.

1. It is the only option that reaches an installed machine without `link`.
2. It needs no shell quoting on any system.
3. A failed cut is harmless. The log is a record for a person and for `status`, and no decision uses it.

The build adds `cut_log`. It works in six steps.

1. Read the metadata of the log. A missing file, or a length at or under the cap, ends the function.
2. Open the file for reading and writing. Do not create it.
3. Read the last 256 KiB.
4. Decode the bytes, and call `tail_of` on the text.
5. Set the length to 0, and write the tail from the start.
6. On any error, stop and leave the file as it is. `sync` goes on.

`main.rs` calls it before `sync::sync`. It runs for every `sync`, including the run of the hook, where no
agent holds the file. The check costs one `stat`. A line that another writer appends between the two parts
of step 5 can be lost. The design accepts that loss.

**Decision for the maintainer (D-9).** Option (b) with the cap of 1 MiB and the target of 256 KiB.

### 4. The prototype

`tail_of(text, keep)` returns the tail of `text` that fits in `keep` bytes and starts at the beginning of a
line. It carries `cfg(test)` until the build. The rules:

1. When `text.len() <= keep`, it returns `text`.
2. Otherwise it starts at `text.len() - keep`, moves forward to the next character boundary, and then moves
   forward to the byte after the next `\n`.
3. When no `\n` follows, it returns an empty text.

The cut never falls inside a character, and never inside a line. A log that starts with half a line reads
as a fault.

When the start lands exactly on the first byte of a line, rule 2 still skips that line. The result stays
under `keep`, and the log loses one more line than it must. The prototype keeps the rule as written,
because a result of whole lines is the point, and the rule is simple.

Four tests check it:

| Test | What it pins |
|---|---|
| `keeps_a_log_that_fits` | A text at or under `keep` comes back whole |
| `cuts_a_log_at_the_start_of_a_line` | For every `keep`, the result is a tail of the text, starts after a line break, and fits in `keep` |
| `cuts_a_multi_byte_log_without_a_panic` | A text of `é` and a line break, repeated 1000 times, does not panic for any `keep`. The odd value 1001 lands inside a character, and the cut still falls at a line start |
| `returns_nothing_when_the_tail_holds_no_line_break` | One line of 5000 characters with `keep` 100 gives an empty text |

## Build steps

The steps run in order. The step that breaks nobody comes first. Each step names the file that it changes
and the test that proves it. A field that comes with a plan that is not in the tree yet stays out until
that plan lands.

1. **The parser.** In `src/cli.rs`, add `json` and `endpoints` to `Args`. Parse `--json` and `--endpoints`.
   Add both to `check_options` (`--json` and `--endpoints` apply to `status`). Refuse `--endpoints` without
   `--json`. Add both to the help text. Proof: `parses_the_json_flags`, `refuses_json_with_another_command`,
   `refuses_endpoints_without_json`. The test `the_help_text_names_no_endpoint` still passes.
2. **One snapshot, two printers.** In `src/main.rs`, make `status` collect one `Snapshot` and print it as
   text or as JSON. A new module can hold the type, but only `main` prints (rule 3). The text rows stay as
   they are. With `--json`, `run` builds a `log` that prints to standard error. This step holds only the
   facts of today. Proof:
   - `the_text_rows_stay_as_they_are`: a golden test on a fixture.
   - `the_json_matches_the_contract`: the example of this document, parsed and compared with a fixture.
   - `the_json_holds_no_placeholder_text`: no `<none>` and no `<unknown>`.
   - `the_json_names_no_endpoint_unless_asked`: a fixture with URLs in its reason texts.
   - `status_json_prints_one_document`: an integration test. It runs the built binary
     (`CARGO_BIN_EXE_brainmaker`) with `--dir <temp>` and a `--url` for a loopback address, port 1, where
     no server listens. It checks that standard output is one line of JSON that holds no part of that URL.
3. **The agent rows for macOS.** In `src/schedule.rs`, add the function that returns the files and their
   text, and `health`. Add `program_prefix` to `src/link.rs`. Read the log by its file time and its last
   line. Ask `launchctl print` for its exit status. Proof: `reads_a_current_definition`,
   `reads_a_stale_definition`, `reads_a_missing_definition`, `status_reads_what_link_writes`,
   `the_last_line_decides_the_error_flag`, `the_log_time_is_the_file_time`, and
   `a_missing_log_gives_no_last_run`. A macOS-only test, `launchctl_print_fails_for_an_absent_label`, runs
   the verb with a label that does not exist.
4. **The log cut.** In `src/schedule.rs`, remove `cfg(test)` from `tail_of`, and add `cut_log`. In
   `src/main.rs`, call it before `sync::sync`. Proof: the four prototype tests, and
   `cuts_a_log_past_the_cap_in_place`, `leaves_a_log_under_the_cap_alone`, and `creates_no_log`.
5. **The fields of plans 003, 009, and 013.** Add each one when its plan is in the tree. The fields go into
   the snapshot, and the rows go into the text.
   - Plan 003: `locks.install` and `locks.update`. In `src/lock.rs`, add `peek`, which opens for reading
     and asks for a shared lock. Proof: `peek_reports_a_held_lock`, `peek_reports_a_free_lock`, and
     `peek_creates_no_file`.
   - Plan 009: `content.sequence` and `content.latest_sequence`, and the word `not_newer` if D-2 stands.
   - Plan 013: `binding.kind` and `binding.weak`.
   - Proof for the last two: extend `the_json_matches_the_contract` and `the_text_rows_stay_as_they_are`.
6. **The script.** After the maintainer answers D-5 and D-6, change `render` in `src/schedule.rs` once, for
   every reason together. Say in the notes of the release that each machine reads `stale` until `link`
   runs. Proof: update the tests that pin the script text.
7. **An exit code.** Only when the maintainer chooses (a) in D-8. Change `src/main.rs`, `src/cli.rs`, and
   the documents that section "Exit codes" lists. Proof: `check_exit_gives_the_new_code_for_a_newer_build`
   and `check_exit_gives_success_for_the_others`.

**The documents.** Each step changes the documents that it touches, in the same commit as the code:

- README.md and `docs/FLOW.md`.
- `docs/API.md`: the option table, the `status` table, and the contract of the JSON.
- `docs/ARCHITECTURE.md`.
- `docs/SECURITY.md`: the rule of the JSON, and the log cut.
- `docs/.docsgen.json`: run the docsgen skill, as the earlier builds did, so that the manifest follows the
  sources.

No test proves a document. The docsgen run is the check.

## Open questions for the maintainer

### Decisions

D-3 and D-7 are the privacy decisions. They are the hardest to take back after a release, so read them
first.

1. **D-1. Does the JSON carry the lengths of the credentials?** The recommendation is no.
2. **D-2. Is `not_newer` the word for a release that plan 009 refuses?** The recommendation is yes.
3. **D-3. Does the default JSON omit `api_base`, `token_url`, and the two reason texts?** The
   recommendation is yes. `--endpoints` adds them.
4. **D-4. Does the JSON carry `"schema": 1`, and are `--json` and `--endpoints` the flag names?** The
   recommendation is yes to both.
5. **D-5. Which change of the script, if any?** The recommendation is a command that writes the log lines
   (product decision 7 of scheduled-agent.md), and the `date -u` line as the fallback. Every machine reads
   `stale` until `link` runs.
6. **D-6. Does the agent's `sync` stop hiding `Unreachable` and `Busy`?** The recommendation is yes: drop
   `--quiet` from that command, in the same change as D-5.
7. **D-7. Does `status` never print the last error line of the log?** The recommendation is yes. It prints
   the time and `error logged` only.
8. **D-8. Does `self-update --check` get a new exit code?** The recommendation is no. The answer comes from
   `status --json`.
9. **D-9. Is the log cut in place at the start of `sync`, with a cap of 1 MiB and a target of 256 KiB?**
   The recommendation is yes.

### Not verified

Each row is an answer that this document does not confirm.

| ID | What is not verified | How to verify |
|---|---|---|
| V-1 | That the lock probe reads `held` on Linux and on Windows | Run `peek_reports_a_held_lock` in the CI job of each system |
| V-2 | The unit and the range of the plan 009 sequence, and so whether a JSON number can hold it | Read plan 009. A value above 2^53 needs a string |
| V-3 | That `launchctl print gui/<uid>/<label>` exits non-zero for a service that is not loaded, on every supported macOS version | Run it with an absent label on the oldest macOS version that the fleet runs |
| V-4 | That the owner of the home directory is the uid of the account that runs `status` | Run `sudo brainmaker status`, and compare with `id -u` |
| V-5 | The `loaded` checks for Linux and for Windows | Follow L-5 and W-7 of scheduled-agent.md, and the rows for the other checks |
| V-6 | That the `date` line of the LaunchAgent has the `C` form, because the job has no `LANG` | Read the `agent.log` of a machine with a non-English system language |
| V-7 | That `%date%` on Windows follows the regional settings | Change the regional format, and run `echo %date%` |
| V-8 | That `date -u +%Y-%m-%dT%H:%M:%SZ` prints the same text in every locale on GNU `date` | Run it under three locales on Debian and on Fedora |
| V-9 | That launchd opens the log in append mode | Load a scratch agent that prints a line every second. Cut its log in place. Read it with `cat -v` and look for `^@` |
| V-10 | That a cut in place works on Linux and on Windows while the redirect of the script holds the file | Cut the file from a second process while the redirect writes, on both systems |
