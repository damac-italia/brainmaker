# Flow

Nine runtime paths matter: loading the settings, getting an access token, the `sync` command, the
push of the outbox, the diagnostic report, the `self-update` command, `link` with its hourly agent,
the `uninstall` command, and the admin commands. `status` reuses
the first two paths and then reads both remote endpoints without writing anything. Because it
writes nothing, an endpoint it cannot reach becomes a value it prints rather than a reason to exit:
[`main.rs:465`](../src/main.rs) prints `latest    <unknown>` and `state     cannot check: <reason>`.

## Settings load

Every command but `uninstall` starts here. `Config::load` runs before the dispatch in
[`src/main.rs:102`](../src/main.rs).

```mermaid
sequenceDiagram
    participant main
    participant config
    participant provision
    participant secretstore
    main->>config: load(options, log)
    config->>config: make --dir absolute, else ~/.brainmaker
    config->>provision: find(--config, config.enc exists)
    alt a provisioning file was found
        provision-->>config: path
        config->>provision: read(path)
        config->>config: check_importable(settings)
        config->>secretstore: seal(text)
        config->>secretstore: write_owner_only(config.enc)
        config->>config: remove the file unless --keep-config
    else no file, and config.enc exists
        config->>secretstore: open(config.enc)
        secretstore-->>config: KEY=VALUE text
    end
    config->>config: apply the environment, then --url
    config->>url: check_base_url(base), then check_base_url(jwt endpoint)
    config->>provision: check_credential_set(the merged keys)
    config-->>main: Config
```

Steps:

1. [`config.rs:449`](../src/config.rs) resolves the root through
   [`config.rs:296`](../src/config.rs): `--dir` made absolute, else `~/.brainmaker`.
2. [`provision.rs:347`](../src/provision.rs) searches up to four locations in order and returns the
   first hit. A `--config` or `$BRAINMAKER_CONFIG` path that is not a file fails the run. The fourth
   location, the working directory, is searched only while `config.enc` does not yet exist.
3. On a hit, [`config.rs:456`](../src/config.rs) reads the file, and
   [`config.rs:457`](../src/config.rs) checks the credential values and the ten routes through
   [`config.rs:271`](../src/config.rs). A file that fails here changes nothing: the previous store
   and the file itself stay in place.
4. [`config.rs:459`](../src/config.rs) seals the parsed settings, and
   [`config.rs:460`](../src/config.rs) writes `confidential/config.enc` with mode `0600` inside a
   `0700` directory.
5. [`config.rs:466`](../src/config.rs) logs a warning when the system gives no machine identifier,
   because the store is then bound to the home directory path or to nothing.
6. [`config.rs:478`](../src/config.rs) removes the plain file. A failed removal logs a warning and
   the run continues, because the settings are already stored.
7. With no hit and an existing store, [`config.rs:507`](../src/config.rs) decrypts it. A store from
   another machine or another build fails here with a message that tells you to reimport.
8. [`config.rs:521`](../src/config.rs) lets `BRAINMAKER_API_BASE`, `SWETSI_JWT_ENDPOINT`,
   `SWETSI_CLIENT_ID`, and `SWETSI_CLIENT_SECRET` override the stored values, and
   [`config.rs:532`](../src/config.rs) lets `--url` override the base URL again.
9. [`config.rs:536`](../src/config.rs) and [`config.rs:548`](../src/config.rs) fail when no source
   supplied a base URL.
10. [`config.rs:556`](../src/config.rs) fails when the base URL is not `https://`, unless its host
    is this machine.
11. [`config.rs:560`](../src/config.rs) checks the merged credential set. Three keys, or none of
    them, passes. Any other count fails and names the missing keys. A configuration that still
    carries `SWETSI_TOKEN` and none of the three fails here.
12. [`config.rs:572`](../src/config.rs) builds the `Credentials`. It applies the TLS rule to
    `SWETSI_JWT_ENDPOINT`, and it refuses a credential value that cannot go into a header.
13. [`config.rs:579`](../src/config.rs) reads the ten routes, and the environment overrides one
    route at a time. Each absent key takes its default. A route that holds `://`, a `..` segment,
    or a space fails here, because it would leave the base URL.

[`config.rs:516`](../src/config.rs) runs before steps 8 to 13 and fails when a configuration still
carries a key under its old name, such as `SWETSI_API_BASE`.

## Access token

`remote.rs` and `outbox.rs` call [`auth::bearer`](../src/auth.rs) before each request, with the
scope that the request needs: `sync` for every read, and `outbox:write` for a note. `diagnostics.rs`
calls it with `sync` for the diagnostic report, and reads the token that the run already holds.
The first call
for a scope fetches, and every later call for it in the same run reads the cache. `link` calls
`auth::grants`, which calls `bearer` and turns `invalid_scope` into `false`, to learn the role; see
[Link and the hourly agent](#link-and-the-hourly-agent).

```mermaid
sequenceDiagram
    participant remote
    participant auth
    participant cache as TokenCache
    participant JWT as token endpoint
    remote->>auth: bearer(config, scope)
    alt no credential is configured
        auth-->>remote: None, so no Authorization header
    else the cache holds a usable token for the scope
        auth->>cache: get(scope)
        cache-->>auth: token
        auth-->>remote: Bearer token
    else
        auth->>JWT: POST {jwt_endpoint}/{token route}, scope=<scope>
        JWT-->>auth: {"access_token": "…", "expires_in": 600}
        auth->>auth: check the token_type, then check_token(token)
        auth->>cache: put(scope, token, min(expires_in, 3600 s) - 30 s)
        auth-->>remote: Bearer token
    end
```

Steps:

1. [`auth.rs:199`](../src/auth.rs) returns `Ok(None)` when no credential is configured. The request
   then carries no `Authorization` header.
2. [`auth.rs:203`](../src/auth.rs) returns the cached token for the scope while it stays usable.
3. [`auth.rs:259`](../src/auth.rs) posts `grant_type=client_credentials&scope=<scope>` with HTTP
   Basic, and [`auth.rs:268`](../src/auth.rs) reads the response body up to 64 KiB. An answer of 400
   or 401 whose `error` is `invalid_scope` becomes `InvalidScope` at
   [`auth.rs:322`](../src/auth.rs), which a caller can tell apart from every other failure.
4. [`auth.rs:279`](../src/auth.rs) refuses a `token_type` other than `Bearer`, in any case. A
   response with no `token_type` passes.
5. [`auth.rs:298`](../src/auth.rs) refuses a token that is empty, longer than 8192 bytes, or holds
   a character outside printable ASCII.
6. [`auth.rs:239`](../src/auth.rs) computes how long the client uses the token: `expires_in`, or
   600 seconds when the response has none, cut to at most one hour, less 30 seconds.
7. [`auth.rs:209`](../src/auth.rs) caches the token for that scope and that time. A lifetime that
   the clock cannot hold caches nothing, so the next request asks for a new token. The cache never
   reaches the disk.
8. [`auth.rs:217`](../src/auth.rs) answers whether this run received a token for a scope. The
   outbox steps of `sync` ask it for `sync`, because that token proves that the issuer answered.

A failed token request fails the command that asked for it. During `sync`, that happens before any
file changes, so `content/` stays as it was.

## Sync

```mermaid
sequenceDiagram
    participant main
    participant sync
    participant lock
    participant remote
    participant API
    participant archive
    participant state
    main->>sync: sync(config, force, log)
    sync->>sync: restore_stranded()
    sync->>remote: latest_release(config, report headers)
    remote->>API: GET {base}/{latest hash route}
    API-->>remote: {"payload": "...", "signature": "..."}
    remote-->>sync: ContentRelease
    alt the request failed, content/ exists and not --force
        sync-->>main: Unreachable
    else hash matches state.json and content/ exists and not --force
        sync-->>main: UpToDate
    else
        sync->>sync: check_order(installed sequence, offered sequence)
        sync->>lock: acquire(.lock, 30 s)
        alt the lock is still held, content/ exists and not --force
            sync-->>main: Busy
        else
            sync->>state: read again, then check_order again
            sync->>remote: download_archive(hash, .download.zip)
            remote->>API: GET {base}/{content archive route}
            sync->>archive: extract(.download.zip, .staging)
            sync->>sync: swap(content, .staging, .trash)
            sync->>state: write(state.json, hash, sequence)
            sync-->>main: Updated
        end
    end
```

Steps:

1. [`sync.rs:77`](../src/sync.rs) creates the root directory.
2. [`sync.rs:80`](../src/sync.rs) restores content that a stopped run left in `.trash/`, through
   [`sync.rs:284`](../src/sync.rs). It acts only when `content/` is missing, `.trash/` is a
   directory, and the install lock is free at once.
3. [`sync.rs:89`](../src/sync.rs) reads the signed content release, with the three headers that
   [`outbox.rs:452`](../src/outbox.rs) builds: the platform, the installed hash or `none`, and the
   count of notes that wait. `remote::latest_release` checks
   the Ed25519 signature against `signature::CONTENT_KEYS`, and runs `config::validate_hash` on the
   hash from the signed payload, so an out-of-range value fails before it reaches a URL or a path.
4. [`sync.rs:92`](../src/sync.rs) turns a failed request into `Unreachable` when `state.json` names
   a hash, `content/` is a directory, and `--force` was not given. With nothing installed, or with
   `--force`, the error is returned instead.
5. [`sync.rs:104`](../src/sync.rs) returns `UpToDate` when the local hash matches, `content/` is a
   directory, and `--force` was not given. No archive is downloaded.
6. [`sync.rs:114`](../src/sync.rs) applies the order rule of [`sync.rs:161`](../src/sync.rs) when
   the offered hash differs from the installed hash and `--force` was not given. The run fails when
   the installed release has a sequence and the offered one is not higher, or has none.
7. [`sync.rs:119`](../src/sync.rs) takes the exclusive lock on `.lock`, and waits up to 30 seconds
   for another run to release it. When the lock is still held, [`sync.rs:121`](../src/sync.rs)
   returns `Busy` if content is installed and `--force` was not given, and an error otherwise.
8. [`sync.rs:133`](../src/sync.rs) reads `state.json` again, because the run that held the lock may
   have installed a release. [`sync.rs:134`](../src/sync.rs) returns `UpToDate` when it installed
   this one, and [`sync.rs:137`](../src/sync.rs) applies the order rule again to the state as it
   stands now.
9. [`sync.rs:142`](../src/sync.rs) picks the sequence to record. A reinstall of the installed hash
   keeps the higher of the installed and the offered sequence.
10. [`sync.rs:202`](../src/sync.rs) clears `.staging`, `.trash`, and `.download.zip` left by a run
    that failed between steps.
11. [`sync.rs:207`](../src/sync.rs) streams the archive to `.download.zip`, stopping at 512 MiB.
12. [`sync.rs:213`](../src/sync.rs) compares the byte count, and
    [`sync.rs:220`](../src/sync.rs) the SHA-256, against the signed release, before the extractor
    opens the file.
13. [`sync.rs:223`](../src/sync.rs) extracts to `.staging`. Rejected entries are counted, not fatal;
    the log line reads `Skipped N unsafe archive entries.`
14. [`sync.rs:235`](../src/sync.rs) swaps the directories.
15. [`sync.rs:240`](../src/sync.rs) removes the three temporary paths, on success and on failure
    alike.
16. [`sync.rs:246`](../src/sync.rs) writes `state.json` with the hash and the sequence, only after
    the swap succeeded. The lock is released when `sync` returns.
17. [`main.rs:256`](../src/main.rs) keeps the result of the content step, and
    [`main.rs:261`](../src/main.rs) runs the outbox
    steps, which [Push](#push) describes.
    [`main.rs:262`](../src/main.rs) then writes the lines of this
    run to the run log, and sends the diagnostic report when one is due, which
    [Diagnostic report](#diagnostic-report) describes. Only then does
    [`main.rs:263`](../src/main.rs) return a content error.
18. [`main.rs:264`](../src/main.rs) runs the software check unless `--no-update-check` was given.

### The swap and its rollback

[`sync.rs:257`](../src/sync.rs):

1. Rename `content/` to `.trash/`, when `content/` exists.
2. Rename `.staging/` to `content/`.
3. If step 2 fails and step 1 ran, rename `.trash/` back to `content/` and return the error.

A run that is killed between steps 1 and 2 leaves no `content/`. Step 2 of the next `sync` renames
`.trash/` back.

### Failure paths in sync

| Failure | Result |
|---|---|
| The server cannot be reached, and content is installed | Two `notice:` lines go to stderr and the exit code stays 0 |
| The server cannot be reached, and no content is installed | The run fails; nothing on disk changed |
| The content release fails the `CONTENT_KEYS` signature check | The run fails; nothing on disk changed |
| The response is not a signed envelope | The run fails; nothing on disk changed |
| The hash is not 8 ASCII alphanumeric characters | The run fails before any URL is built |
| The offered sequence is not higher than the installed one | The run fails; nothing on disk changed. `--force` installs it. |
| Another run holds the lock for 30 seconds, and content is installed | Two `notice:` lines go to stderr and the exit code stays 0 |
| Another run holds the lock for 30 seconds, and no content is installed | The run fails; nothing on disk changed |
| The download body exceeds 512 MiB or is empty | The partial file is deleted and the run fails |
| The archive size or SHA-256 differs from the signed release | The run fails; `content/` is untouched |
| The archive is not a zip | The run fails; `content/` is untouched |
| One entry expands past 256 MiB, or the archive past 1 GiB | The run fails; `content/` is untouched |
| The second rename fails | The previous `content/` is restored, then the run fails |
| `state.json` cannot be written | The content is installed, and the run fails with that stated |
| The software check fails, including on a bad signature | A `notice:` line goes to stderr and the exit code stays 0 |

## Push

`sync` runs these steps after the content step, and `push` runs the push alone.

```mermaid
sequenceDiagram
    participant main
    participant outbox
    participant auth
    participant API
    participant disk as outbox/
    main->>outbox: ensure_dir(config), mode 0700
    main->>auth: received(config, sync)?
    alt this run received a sync token
        main->>outbox: update_operator(config)
        outbox->>API: GET {base}/{whoami route}
        outbox->>outbox: write operator, or delete it on null
        main->>outbox: push(config, log)
        outbox->>outbox: take .outbox.lock without waiting
        outbox->>disk: list the .md entries, check each one
        alt a note is ready
            outbox->>auth: bearer(config, outbox:write)
            loop each ready note, oldest first
                outbox->>API: POST {base}/{outbox route}/<name>
                outbox->>disk: move to sent/<YYYY-MM>/, or to rejected/, or stop
            end
            outbox->>disk: write push.json when a note was sent
        end
    end
```

Steps:

1. [`main.rs:285`](../src/main.rs) creates `outbox/` with mode `0700` through
   [`outbox.rs:331`](../src/outbox.rs), because a Mac that linked before the outbox existed never
   runs `link` again.
2. [`main.rs:288`](../src/main.rs) stops the steps when this run received no `sync` token. The
   issuer then did not answer, and a further request would only wait.
3. [`main.rs:291`](../src/main.rs) asks `whoami`.
   [`outbox.rs:480`](../src/outbox.rs) writes a name
   that matches the operator rule to `operator`, and deletes the file on `null`. A failure prints a
   notice and leaves the file.
4. [`main.rs:298`](../src/main.rs) runs the push.
   [`outbox.rs:593`](../src/outbox.rs) takes
   `.outbox.lock` without waiting, and a run that finds it held sends nothing.
5. [`outbox.rs:600`](../src/outbox.rs) checks each `.md` entry through
   [`outbox.rs:696`](../src/outbox.rs). A symbolic link, and anything that is not a regular file,
   is refused at [`outbox.rs:704`](../src/outbox.rs). A file that changed in the last 60 seconds
   waits, at [`outbox.rs:718`](../src/outbox.rs). Then the name rule, the 64 KiB cap, and the
   frontmatter rules apply, and [`outbox.rs:770`](../src/outbox.rs) refuses a file that is not the
   file that was checked. A refused file moves to `rejected/` through
   [`outbox.rs:836`](../src/outbox.rs), beside its reason. Each refusal also names its rule as one
   word of [`outbox.rs:82`](../src/outbox.rs), which is all that the run log keeps
   of it.
6. [`outbox.rs:612`](../src/outbox.rs) returns when no note is ready, before any request.
7. [`outbox.rs:616`](../src/outbox.rs) asks for an `outbox:write` token. `InvalidScope` stops the
   run with a notice, and every note stays.
8. [`outbox.rs:635`](../src/outbox.rs) orders the notes oldest first, and
   [`outbox.rs:638`](../src/outbox.rs) sends each one through
   [`remote.rs:303`](../src/remote.rs).
9. A `201` or a `200` moves the note to `sent/<YYYY-MM>/` at [`outbox.rs:650`](../src/outbox.rs).
   The month comes from `received_at`, and [`outbox.rs:798`](../src/outbox.rs) gives `unknown` for
   any other shape. A `400` or a `413` moves it to `rejected/` at
   [`outbox.rs:659`](../src/outbox.rs). Every other answer, and a failure to reach the server,
   keeps it and stops the run at [`outbox.rs:674`](../src/outbox.rs).
10. [`outbox.rs:852`](../src/outbox.rs) adds `-2`, `-3`, and so on to a name that is taken.
11. [`outbox.rs:690`](../src/outbox.rs) writes `push.json` when the run sent a note. The lock is
    released when `push` returns.

Under `sync`, each failure of these steps prints one `notice:` line, which `--quiet` hides, and the
exit code stays as the content step decided. `push` alone exits 1 at
[`main.rs:384`](../src/main.rs) when a note had to stay.

## Diagnostic report

`sync` runs these steps after the outbox steps. `push`, `self-update`, `link`, and `unlink` run
step 1 alone: they write their lines, and send nothing.

```mermaid
sequenceDiagram
    participant main
    participant diagnostics
    participant disk as root directory
    participant auth
    participant API
    main->>diagnostics: Run::content, Run::operator, Run::pushed
    main->>diagnostics: Run::save(config)
    diagnostics->>disk: append the lines to diagnostics.jsonl, cut it past 256 KiB
    main->>diagnostics: send(config)
    alt no sync token, or the interval has not passed, or the lock is held
        diagnostics-->>main: NoToken, NotDue, or Busy
    else
        diagnostics->>disk: read diagnostics.jsonl, parse and check each line
        diagnostics->>disk: read the state: state.json, outbox/, push.json, ~/.claude
        diagnostics->>auth: bearer(config, sync), from the cache
        diagnostics->>API: POST {base}/{diagnostics route}
        diagnostics->>disk: write diagnostics.json
        diagnostics-->>main: Stored or Failed
    end
```

Steps:

1. Each step of a command gives its result to a `Run`.
   [`diagnostics.rs:428`](../src/diagnostics.rs)
   turns the content step into one line, and
   [`diagnostics.rs:453`](../src/diagnostics.rs)
   turns the push into one line for the notes sent, one for each rule that a note broke, one for
   the notes that wait, and one for a stop. A failure enters a line as the word that
   [`cause.rs:91`](../src/cause.rs) reads from the
   error, never as its text.
2. [`main.rs:318`](../src/main.rs) saves the run.
   [`diagnostics.rs:515`](../src/diagnostics.rs) refuses a
   log that is not a regular file, and appends the lines in one write.
   [`diagnostics.rs:538`](../src/diagnostics.rs) cuts a log of more than 256 KiB
   to its newest 128 KiB, at the start of a line.
3. [`diagnostics.rs:865`](../src/diagnostics.rs) stops
   when this run received no `sync` token: the issuer did not answer, so no request is made.
4. [`diagnostics.rs:869`](../src/diagnostics.rs)
   stops when the last try is younger than 30 minutes. A `link` or an `unlink` that worked cuts
   that wait to 90 seconds through [`diagnostics.rs:804`](../src/diagnostics.rs),
   so a `sync` soon after it reports what it changed.
5. [`diagnostics.rs:872`](../src/diagnostics.rs)
   takes `.diagnostics.lock` without waiting. A run that finds it held sends nothing.
6. [`diagnostics.rs:567`](../src/diagnostics.rs) reads the
   run log. [`diagnostics.rs:568`](../src/diagnostics.rs)
   refuses a symbolic link and anything that is not a regular file.
   [`diagnostics.rs:591`](../src/diagnostics.rs)
   keeps a line only when it parses as an event, and
   [`diagnostics.rs:337`](../src/diagnostics.rs) checks every value
   of it.
7. [`diagnostics.rs:601`](../src/diagnostics.rs)
   takes the lines after the last one that the server holds, the oldest first, and 200 at most.
8. [`diagnostics.rs:710`](../src/diagnostics.rs) reads the
   state from local files. [`link.rs:937`](../src/link.rs)
   reads which pieces of the bridge are in `~/.claude`, and changes nothing there.
   [`outbox.rs:404`](../src/outbox.rs) counts the entries in the outbox that
   push does not read as a note, such as a file with another ending than `.md`, or a folder.
9. [`diagnostics.rs:825`](../src/diagnostics.rs) writes
   the report from those values, and takes fewer lines until it fits in 64 KiB.
10. [`diagnostics.rs:890`](../src/diagnostics.rs)
    sends it through [`remote.rs:354`](../src/remote.rs), with the `sync`
    token of the run and 10 seconds in all.
11. On a `2xx` answer, [`diagnostics.rs:895`](../src/diagnostics.rs)
    records the newest line that the server holds. On every other answer, and on no answer, the
    lines stay for the next report.
    [`diagnostics.rs:898`](../src/diagnostics.rs) writes
    `diagnostics.json` in both cases, so the next try waits 30 minutes too.

No step here prints a failure or returns one. `sync` prints one line when the server stored the
report, which `--quiet` hides, and its exit code stays as the content step decided.

## Self-update

```mermaid
sequenceDiagram
    participant main
    participant selfupdate
    participant remote
    participant API
    participant signature
    participant staged as staged binary
    participant link
    main->>selfupdate: check(config)
    selfupdate->>remote: fetch_text(the software manifest route)
    remote->>API: GET {base}/{software manifest route}
    API-->>selfupdate: {"payload": "...", "signature": "..."}
    selfupdate->>signature: verify(payload, signature)
    selfupdate->>selfupdate: parse payload, validate version, pick platform key
    main->>selfupdate: apply(config, version, build)
    selfupdate->>selfupdate: check the sha256, take .update.lock
    selfupdate->>selfupdate: binary_url(version, platform) from the base URL
    selfupdate->>selfupdate: remove leftovers, check_writable(install directory)
    selfupdate->>remote: download(the derived URL, .brainmaker-update-<pid>)
    selfupdate->>selfupdate: sha256_of(staged) == manifest sha256
    selfupdate->>staged: run --version
    staged-->>selfupdate: brainmaker <latest>
    selfupdate->>selfupdate: swap(exe, staged, .brainmaker-old)
    selfupdate->>link: copy_program(exe, <root>/bin/brainmaker)
```

Steps:

1. [`selfupdate.rs:123`](../src/selfupdate.rs) reads the envelope, stopping at 1 MiB.
2. [`selfupdate.rs:132`](../src/selfupdate.rs) checks the Ed25519 signature over the `payload`
   bytes, against the keys in `signature::PUBLIC_KEYS`. A manifest that no key accepts stops here,
   so nothing below ever sees it. A build with no key stops here too.
3. [`selfupdate.rs:135`](../src/selfupdate.rs) parses the verified `payload` into the manifest.
4. [`selfupdate.rs:142`](../src/selfupdate.rs) rejects a version string that is empty, longer than
   64 characters, or holds a character outside `[A-Za-z0-9.+-]`.
5. [`selfupdate.rs:151`](../src/selfupdate.rs) returns `UpToDate` when the manifest version is not
   newer. `--force` then reinstalls the same version, if the manifest has a build for this
   platform. A replayed older manifest therefore installs nothing, even with a valid signature.
6. [`selfupdate.rs:159`](../src/selfupdate.rs) looks up `<os>-<arch>`, for example `darwin-arm64`.
   A missing key yields `NewerElsewhere`, whose error names the keys that are present.
7. [`selfupdate.rs:194`](../src/selfupdate.rs) rejects a checksum that is not 64 hexadecimal
   characters.
8. [`selfupdate.rs:197`](../src/selfupdate.rs) takes the exclusive lock on `.update.lock` under the
   root, and waits up to 30 seconds. A lock that is still held fails the run.
9. [`selfupdate.rs:205`](../src/selfupdate.rs) derives the download URL from the base URL and
   `BRAINMAKER_SOFTWARE_BINARY_PATH`, filling in `{version}`, `{platform}`, and `{ext}`. The
   manifest names no URL, so it cannot move the download to another host.
10. [`selfupdate.rs:222`](../src/selfupdate.rs) removes every `.brainmaker-update-*` and
    `.brainmaker-probe-*` file in the install directory. A stopped update left them, and the lock
    means no other update owns them.
11. [`selfupdate.rs:223`](../src/selfupdate.rs) writes a probe file in the install directory, so a
    permission problem fails in about a second rather than after a large download.
12. [`selfupdate.rs:227`](../src/selfupdate.rs) downloads to `.brainmaker-update-<pid>`, stopping at
    128 MiB.
13. [`selfupdate.rs:231`](../src/selfupdate.rs) compares the streamed SHA-256 with the manifest.
14. [`selfupdate.rs:235`](../src/selfupdate.rs) runs the staged file with `--version`, and
    [`selfupdate.rs:392`](../src/selfupdate.rs) requires the trimmed output to be exactly
    `brainmaker <version>`. This catches a build for the wrong architecture and a manifest that
    points at the wrong file.
15. [`selfupdate.rs:238`](../src/selfupdate.rs) renames the running binary to `.brainmaker-old`,
    then renames the staged file into place. A failure on the second rename restores the old
    binary. A running process keeps its open image, so the swap is safe while `brainmaker` runs.
16. [`selfupdate.rs:241`](../src/selfupdate.rs) removes the staged file on failure, and removes the
    backup either way.
17. [`selfupdate.rs:248`](../src/selfupdate.rs) replaces `<root>/bin/brainmaker` as well, when that
    file exists and is not the file that ran. `link::same_file` decides that by canonical
    path. The `SessionStart` hook runs that copy, so an update that skipped it would leave every
    session on the old version.

`--check` stops after step 6 and installs nothing.

## Link and the hourly agent

```mermaid
sequenceDiagram
    participant main
    participant link
    participant auth
    participant schedule
    participant launchd
    main->>schedule: Agents::resolve(--agent-dir, --claude-dir given)
    main->>link: link(config, claude, agents, log)
    link->>auth: grants(outbox:read), then grants(outbox:write) when read is granted
    alt reads the outbox and cannot send notes: the admin
        link->>link: copy the program, quote the command prefix
        link->>link: remove any skill link, hook entry, and block
        link->>schedule: install(agents, prefix, root, UpdateOnly, log)
    else every other credential, or none
        link->>link: link skills, copy the program, quote the command prefix
        link->>link: write the hook and the block, each through a rename
        link->>schedule: install(agents, prefix, root, UpdateAndSync, log)
    end
    alt the property list is unchanged
        schedule-->>link: false
    else
        schedule->>schedule: write the property list
        schedule->>launchd: bootout, then bootstrap, when it loads
        schedule-->>link: true
    end
    launchd->>launchd: each hour and at login, self-update, then sync on an operator's Mac
```

Steps:

1. [`main.rs:123`](../src/main.rs) resolves the agent location through
   [`schedule.rs:63`](../src/schedule.rs). `--agent-dir` names a directory that is never loaded.
   Without it, [`schedule.rs:67`](../src/schedule.rs) returns no agent on a system other than
   macOS, or when `--claude-dir` was given.
2. [`link.rs:158`](../src/link.rs) asks for the role before it writes anything.
   [`link.rs:193`](../src/link.rs) asks [`auth.rs:226`](../src/auth.rs) for `outbox:read` and, only
   when the issuer grants it, for `outbox:write`. `invalid_scope` counts as not granted, and a run
   with no credential grants neither. Any other failure stops the run with nothing written.
3. For a credential that reads the outbox and cannot send notes,
   [`link.rs:159`](../src/link.rs) copies the program, and [`link.rs:209`](../src/link.rs) runs the
   admin's install. [`link.rs:219`](../src/link.rs) removes the skill links into the content,
   [`link.rs:220`](../src/link.rs) the hook entries, and [`link.rs:221`](../src/link.rs) the block.
   [`link.rs:227`](../src/link.rs) installs the agent that runs `self-update` alone, and
   [`link.rs:236`](../src/link.rs) prints the `admin pull-outbox` command. The run needs no content
   directory and makes no outbox. Steps 4 to 8 do not run.
4. Every other credential goes on here. [`link.rs:165`](../src/link.rs) fails when the content
   directory does not exist yet, and [`link.rs:173`](../src/link.rs) creates `outbox/` with mode
   `0700`.
5. [`link.rs:174`](../src/link.rs) links the skills, and
   [`link.rs:175`](../src/link.rs) copies the program under the root.
6. [`link.rs:176`](../src/link.rs) builds the command prefix. [`link.rs:593`](../src/link.rs)
   writes each path as one double-quoted shell word, puts a backslash before `$`, the backtick,
   `"`, and `\`, and refuses a path that holds a control character.
7. [`link.rs:177`](../src/link.rs) writes the hook into `settings.json`, and
   [`link.rs:178`](../src/link.rs) writes the block into `CLAUDE.md`. A file that exists but cannot
   be read as text fails the run and stays as it is. A `CLAUDE.md` whose markers are not one start
   followed by one end fails the run too. [`link.rs:680`](../src/link.rs) writes each file through
   a temporary file and a rename, keeps the mode of an existing file, and keeps a symbolic link as
   a link.
8. [`link.rs:181`](../src/link.rs) installs the agent with the same command prefix the hook runs.
9. [`schedule.rs:121`](../src/schedule.rs) returns with nothing changed when the property list
   already holds the same text.
10. [`schedule.rs:127`](../src/schedule.rs) writes the property list.
    [`schedule.rs:132`](../src/schedule.rs) unloads an earlier copy, and
    [`schedule.rs:133`](../src/schedule.rs) loads the new one. A load failure prints a `notice:`
    line, and `link` still exits 0.
11. launchd then runs `/bin/sh -c` with the script that
    [`schedule.rs:166`](../src/schedule.rs) renders: `self-update --quiet`, then
    `sync --quiet --no-update-check`, joined by `;`, with the output in `<root>/agent.log`. The
    admin's agent runs `self-update --quiet` alone ([`schedule.rs:171`](../src/schedule.rs)).

`unlink` runs the same resolve step, then `link::remove_bridge`, which unloads and removes the
agent before it removes the skill links, the hook entries, and the block. It removes each hook
entry that ends with `# brainmaker-link`, so a command of yours in the same group stays.

## Uninstall

```mermaid
sequenceDiagram
    participant main
    participant uninstall
    participant link
    participant disk as root directory
    main->>main: ask the question, unless --yes
    main->>uninstall: uninstall(layout, claude, agents, log)
    alt the root exists and holds neither mark
        uninstall-->>main: Report, unrecognised
    else
        uninstall->>link: remove_bridge(content, claude, agents)
        uninstall->>disk: remove content, temporaries, lock files, the program copy
        uninstall->>disk: remove state.json and config.enc
        uninstall->>disk: remove bin, confidential, the root, when empty
        uninstall-->>main: Report
    end
```

Steps:

1. [`main.rs:104`](../src/main.rs) sends `uninstall` down its own path before `Config::load`, so no
   setting is read and no provisioning file is imported.
2. [`main.rs:563`](../src/main.rs) fails when stdin is not a terminal and `--yes` was not given.
   Otherwise the question prints past `--quiet`, and any answer but `y` or `yes` exits 0 with
   `Nothing was removed.`
3. [`uninstall.rs:127`](../src/uninstall.rs) stops the run with nothing changed when the root
   exists and holds neither a parsable `state.json` nor a sealed `confidential/config.enc`.
4. [`uninstall.rs:139`](../src/uninstall.rs) resolves the running program before any file goes,
   so it can later say whether that program was the copy under the root.
5. [`uninstall.rs:144`](../src/uninstall.rs) removes the bridge through `link::remove_bridge`, the
   code that `unlink` runs. The LaunchAgent goes first, before the program copy it runs.
6. [`uninstall.rs:200`](../src/uninstall.rs) lists `content/`, `.staging/`, `.trash/`,
   `.download.zip`, `.lock`, `.update.lock`, `.outbox.lock`, `.admin.lock`, `.diagnostics.lock`,
   `agent.log`, `operator`, `push.json`, `diagnostics.jsonl`, and `diagnostics.json` with their
   temporary files, the program copy and every
   `bin/.brainmaker*` file, then the two temporary files and the two marks, in that order.
   `outbox/` is not in the list, and [`uninstall.rs:248`](../src/uninstall.rs) counts the notes in
   it that were never sent, and says how many.
7. [`uninstall.rs:228`](../src/uninstall.rs) removes each path that exists. A symbolic link goes
   without its target. On Windows, a program copy that is running now yields a `notice:` line
   instead of an error.
8. [`uninstall.rs:259`](../src/uninstall.rs) removes `bin/`, `confidential/`, and the root when
   each one is empty, and names what the root still holds otherwise.
9. [`uninstall.rs:155`](../src/uninstall.rs) prints the path of the program that ran, unless it
   was the copy under the root, and [`uninstall.rs:161`](../src/uninstall.rs) asks you to restart
   any open Claude session when the hook was removed.

A removal that fails part-way returns the error, and the run exits 1. The marks go last, so the
next run still recognises the root and removes the rest. `uninstall` takes no lock, so do not run
it while a `sync` or a `self-update` runs.

## Admin commands

The admin runs these on an install like everyone else's, which `link` left unconnected to Claude;
see [Link and the hourly agent](#link-and-the-hourly-agent). Each one loads the settings as every
other command does, then asks for a token with the scope `outbox:read`.

```mermaid
sequenceDiagram
    participant main
    participant admin
    participant auth
    participant API
    participant dir as DIR
    main->>admin: pull_outbox(config, DIR, log)
    admin->>admin: DIR is a directory, take .admin.lock
    loop until a page is empty or brings nothing to disk, or 1000 notes
        admin->>auth: bearer(config, outbox:read)
        admin->>API: GET {base}/{admin outbox route}?limit=100
        loop each note
            admin->>admin: check again, compare the SHA-256, stamp author and review_flags
            admin->>dir: temporary file, then hard link to <date>-<operator>-<name>
        end
        admin->>API: POST {base}/{admin outbox route}/ack, the notes on disk
    end
```

Steps of `admin pull-outbox`:

1. [`main.rs:174`](../src/main.rs) runs the command with the directory that the parser took.
2. [`admin.rs:254`](../src/admin.rs) refuses a directory that does not exist, before any request,
   so a wrong working directory writes nothing.
3. [`admin.rs:261`](../src/admin.rs) takes `.admin.lock` without waiting, and then removes the temporary
   files that a stopped run left in the directory.
4. [`admin.rs:275`](../src/admin.rs) reads one page of 100 notes. `InvalidScope` becomes
   `this credential cannot read the outbox` at [`admin.rs:192`](../src/admin.rs).
5. [`admin.rs:352`](../src/admin.rs) checks the operator, the client ID, the name, `received_at`,
   the kind, the domain, the flags, and the size again, and
   [`admin.rs:375`](../src/admin.rs) compares the SHA-256 of the text with the one the server
   stored.
6. [`admin.rs:382`](../src/admin.rs) takes the date from `received_at`, and
   [`admin.rs:384`](../src/admin.rs) sets `author` and `review_flags` through
   [`admin.rs:395`](../src/admin.rs).
7. [`admin.rs:489`](../src/admin.rs) writes a flushed temporary file and hard-links it at
   [`admin.rs:517`](../src/admin.rs). A name that holds the same bytes counts as written, and a
   name that holds other bytes gets a number.
8. [`admin.rs:307`](../src/admin.rs) acknowledges the notes of the page that are on disk.
   [`admin.rs:304`](../src/admin.rs) and [`admin.rs:308`](../src/admin.rs) stop the loop.
9. A note that could not be written stays unacknowledged, and the command exits 1.

`admin status` reads the fleet view through [`admin.rs:594`](../src/admin.rs), which refuses any
other shape at [`admin.rs:603`](../src/admin.rs), and builds the admin's shape at
[`admin.rs:691`](../src/admin.rs). `admin syncs` resolves an operator to its clients through the
same view, reads each client's log, and merges the rows newest first at
[`admin.rs:878`](../src/admin.rs), by the instant that each time names
([`admin.rs:102`](../src/admin.rs)).

Steps of `admin diagnose`:

1. [`main.rs:225`](../src/main.rs) runs the command with the operator, or with
   the client of `--client`, and with 20 lines of the run log unless `--limit` names another
   number.
2. [`admin.rs:1053`](../src/admin.rs)
   reads the fleet view, and keeps the clients of the operator, or the one client. The view gives
   what the server saw of each one: the last sync, the version in its `User-Agent`, and the notes
   that the server holds.
3. [`admin.rs:1078`](../src/admin.rs)
   reads the diagnostics of each client. A `404` gets the hint of
   [`admin.rs:1101`](../src/admin.rs): a
   server older than this route answers `404` too.
4. The body parses into types that refuse a field or a word they do not know.
   [`admin.rs:1113`](../src/admin.rs)
   then checks that the answer is for the client that was asked for, and checks each time, each
   version, and each content hash.
5. [`admin.rs:1172`](../src/admin.rs)
   names each cause that the two sources show: a client too old for push, a missing hook, notes
   that broke a rule, notes that wait, entries in the outbox that are not notes, and an outbox
   that never held a note.
6. [`admin.rs:1375`](../src/admin.rs)
   builds the lines that `main.rs` prints. With `--json`, `main.rs` prints the same values as
   JSON.

The command asks no client for anything. Each client sent its report with its own `sync`.
