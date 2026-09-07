# Flow

Four runtime paths matter: loading the settings, getting an access token, the `sync` command, and
the `self-update` command. `status` reuses the first two paths and then reads both remote endpoints
without writing anything.

## Settings load

Every command starts here. `Config::load` runs before the dispatch in
[`src/main.rs:86`](../src/main.rs).

```mermaid
sequenceDiagram
    participant main
    participant config
    participant provision
    participant secretstore
    main->>config: load(options, log)
    config->>provision: find(--config)
    alt a provisioning file was found
        provision-->>config: path
        config->>provision: read(path)
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

1. [`config.rs:264`](../src/config.rs) resolves the root: `--dir`, else `~/.brainmaker`.
2. [`provision.rs:314`](../src/provision.rs) searches four locations in order and returns the first
   hit. A `--config` or `$BRAINMAKER_CONFIG` path that is not a file fails the run.
3. On a hit, [`config.rs:277`](../src/config.rs) seals the parsed settings and writes
   `confidential/config.enc` with mode `0600` inside a `0700` directory.
4. [`config.rs:286`](../src/config.rs) removes the plain file. A failed removal logs a warning and
   the run continues, because the settings are already stored.
5. With no hit and an existing store, [`config.rs:315`](../src/config.rs) decrypts it. A store from
   another machine or another build fails here with a message that tells you to reimport.
6. [`config.rs:329`](../src/config.rs) lets `BRAINMAKER_API_BASE`, `SWETSI_JWT_ENDPOINT`,
   `SWETSI_CLIENT_ID`, `SWETSI_CLIENT_SECRET`, and the five route keys override the stored
   values, then `--url` overrides the base URL again.
7. [`config.rs:349`](../src/config.rs) fails when no source supplied a base URL.
8. [`config.rs:364`](../src/config.rs) fails when the base URL is not `https://`, unless its host is
   this machine.
9. [`config.rs:368`](../src/config.rs) checks the merged credential set. Three keys, or none of
   them, passes. Any other count fails and names the missing keys. A configuration that still
   carries `SWETSI_TOKEN` and none of the three fails here.
10. [`config.rs:380`](../src/config.rs) builds the `Credentials`. It applies the TLS rule to
    `SWETSI_JWT_ENDPOINT`, and it refuses a credential value that cannot go into a header.
11. [`config.rs:388`](../src/config.rs) reads the five routes. Each absent key takes its default.
    A route that holds `://`, a `..` segment, or a space fails here, because it would leave the
    base URL.

[`config.rs:324`](../src/config.rs) runs before all of this and fails when a configuration still
carries a key under its old name, such as `SWETSI_API_BASE`.

## Access token

`remote.rs` calls [`auth::bearer`](../src/auth.rs) before each request. The first call fetches, and
every later call in the same run reads the cache.

```mermaid
sequenceDiagram
    participant remote
    participant auth
    participant cache as TokenCache
    participant JWT as token endpoint
    remote->>auth: bearer(config)
    alt no credential is configured
        auth-->>remote: None, so no Authorization header
    else the cache holds a usable token
        auth->>cache: get()
        cache-->>auth: token
        auth-->>remote: Bearer token
    else
        auth->>JWT: POST {jwt_endpoint}/{token route}
        JWT-->>auth: {"access_token": "…", "expires_in": 600}
        auth->>auth: check_token(token)
        auth->>cache: put(token, expires_in - 30 s)
        auth-->>remote: Bearer token
    end
```

Steps:

1. [`auth.rs:127`](../src/auth.rs) returns `Ok(None)` when no credential is configured. The request
   then carries no `Authorization` header.
2. [`auth.rs:132`](../src/auth.rs) returns the cached token while it stays usable.
3. [`auth.rs:147`](../src/auth.rs) posts `grant_type=client_credentials&scope=sync` with HTTP Basic,
   and reads the response body up to 64 KiB.
4. [`auth.rs:196`](../src/auth.rs) refuses a token that is empty, longer than 8192 bytes, or holds a
   character outside printable ASCII.
5. [`auth.rs:138`](../src/auth.rs) caches the token for `expires_in` less 30 seconds. The cache
   never reaches the disk.

A failed token request fails the command that asked for it. During `sync`, that happens before any
file changes, so `content/` stays as it was.

## Sync

```mermaid
sequenceDiagram
    participant main
    participant sync
    participant remote
    participant API
    participant archive
    participant state
    main->>sync: sync(config, force, log)
    sync->>remote: latest_hash(config)
    remote->>API: GET {base}/{latest hash route}
    API-->>remote: {"hash": "a1b2c3d4"}
    remote-->>sync: hash
    alt hash matches state.json and content/ exists and not --force
        sync-->>main: UpToDate
    else
        sync->>remote: download_archive(hash, .download.zip)
        remote->>API: GET {base}/{content archive route}
        sync->>archive: extract(.download.zip, .staging)
        sync->>sync: swap(content, .staging, .trash)
        sync->>state: write(state.json, hash)
        sync-->>main: Updated
    end
```

Steps:

1. [`sync.rs:32`](../src/sync.rs) creates the root directory.
2. [`sync.rs:39`](../src/sync.rs) reads the latest hash. `remote::latest_hash` runs
   `config::validate_hash` on the response, so an out-of-range value fails before it reaches a URL
   or a path.
3. [`sync.rs:44`](../src/sync.rs) returns `UpToDate` when the local hash matches, `content/` is a
   directory, and `--force` was not given. No archive is downloaded.
4. [`sync.rs:70`](../src/sync.rs) clears `.staging`, `.trash`, and `.download.zip` left by a run
   that failed between steps.
5. [`sync.rs:75`](../src/sync.rs) streams the archive to `.download.zip`, stopping at 512 MiB.
6. [`sync.rs:78`](../src/sync.rs) extracts to `.staging`. Rejected entries are counted, not fatal;
   the log line reads `Skipped N unsafe archive entries.`
7. [`sync.rs:90`](../src/sync.rs) swaps the directories.
8. [`sync.rs:95`](../src/sync.rs) removes the three temporary paths, on success and on failure
   alike.
9. [`sync.rs:101`](../src/sync.rs) writes `state.json`, only after the swap succeeded.
10. [`main.rs:94`](../src/main.rs) runs the software check unless `--no-update-check` was given.

### The swap and its rollback

[`sync.rs:112`](../src/sync.rs):

1. Rename `content/` to `.trash/`, when `content/` exists.
2. Rename `.staging/` to `content/`.
3. If step 2 fails and step 1 ran, rename `.trash/` back to `content/` and return the error.

### Failure paths in sync

| Failure | Result |
|---|---|
| The response is not `{"hash": "..."}` | The run fails; nothing on disk changed |
| The hash is not 8 ASCII alphanumeric characters | The run fails before any URL is built |
| The download body exceeds 512 MiB or is empty | The partial file is deleted and the run fails |
| The archive is not a zip | The run fails; `content/` is untouched |
| One entry expands past 256 MiB, or the archive past 1 GiB | The run fails; `content/` is untouched |
| The second rename fails | The previous `content/` is restored, then the run fails |
| `state.json` cannot be written | The content is installed, and the run fails with that stated |
| The software check fails, including on a bad signature | A `notice:` line goes to stderr and the exit code stays 0 |

## Self-update

```mermaid
sequenceDiagram
    participant main
    participant selfupdate
    participant remote
    participant API
    participant signature
    participant staged as staged binary
    main->>selfupdate: check(config)
    selfupdate->>remote: fetch_text(the software manifest route)
    remote->>API: GET {base}/{software manifest route}
    API-->>selfupdate: {"payload": "...", "signature": "..."}
    selfupdate->>signature: verify(payload, signature)
    selfupdate->>selfupdate: parse payload, validate version, pick platform key
    selfupdate->>selfupdate: binary_url(version, platform) from the base URL
    selfupdate->>selfupdate: check_writable(install directory)
    selfupdate->>remote: download(the derived URL, .brainmaker-update-<pid>)
    selfupdate->>selfupdate: sha256_of(staged) == manifest sha256
    selfupdate->>staged: run --version
    staged-->>selfupdate: brainmaker <latest>
    selfupdate->>selfupdate: swap(exe, staged, .brainmaker-old)
```

Steps:

1. [`selfupdate.rs:118`](../src/selfupdate.rs) reads the envelope, stopping at 1 MiB.
2. [`selfupdate.rs:127`](../src/selfupdate.rs) checks the Ed25519 signature over the `payload`
   bytes, against the keys in `signature::PUBLIC_KEYS`. A manifest that no key accepts stops here,
   so nothing below ever sees it. A build with no key stops here too.
3. [`selfupdate.rs:130`](../src/selfupdate.rs) parses the verified `payload` into the manifest.
4. [`selfupdate.rs:137`](../src/selfupdate.rs) rejects a version string that is empty, longer than
   64 characters, or holds a character outside `[A-Za-z0-9.+-]`.
5. [`selfupdate.rs:146`](../src/selfupdate.rs) returns `UpToDate` when the manifest version is not
   newer. `--force` then reinstalls the same version, if the manifest has a build for this
   platform. A replayed older manifest therefore installs nothing, even with a valid signature.
6. [`selfupdate.rs:154`](../src/selfupdate.rs) looks up `<os>-<arch>`, for example `darwin-arm64`.
   A missing key yields `NewerElsewhere`, whose error names the keys that are present.
7. [`selfupdate.rs:189`](../src/selfupdate.rs) derives the download URL from the base URL and
   `BRAINMAKER_SOFTWARE_BINARY_PATH`, filling in `{version}`, `{platform}`, and `{ext}`. The
   manifest names no URL, so it cannot move the download to another host.
8. [`selfupdate.rs:188`](../src/selfupdate.rs) rejects a checksum that is not 64 hexadecimal
   characters.
9. [`selfupdate.rs:204`](../src/selfupdate.rs) writes a probe file in the install directory, so a
   permission problem fails in about a second rather than after a large download.
10. [`selfupdate.rs:208`](../src/selfupdate.rs) downloads to `.brainmaker-update-<pid>`, stopping at
    128 MiB.
11. [`selfupdate.rs:211`](../src/selfupdate.rs) streams the SHA-256 and compares it to the manifest.
12. [`selfupdate.rs:220`](../src/selfupdate.rs) runs the staged file with `--version` and requires
    the output to contain the manifest version. This catches a build for the wrong architecture and
    a manifest that points at the wrong file.
13. [`selfupdate.rs:239`](../src/selfupdate.rs) renames the running binary to `.brainmaker-old`,
    then renames the staged file into place. A failure on the second rename restores the old
    binary. A running process keeps its open image, so the swap is safe while `brainmaker` runs.
14. [`selfupdate.rs:226`](../src/selfupdate.rs) removes the staged file on failure, and removes the
    backup either way.

`--check` stops after step 6 and installs nothing.
