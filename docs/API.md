# API

`brainmaker` exposes a command-line surface, and it consumes three HTTP routes. Both are described
here. The crate is a binary, not a library, so it exports nothing to other Rust code.

## Command-line surface

```text
brainmaker [COMMAND] [OPTIONS]
```

### Commands

| Command | Effect | Writes |
|---|---|---|
| `sync` | Update the content when the server has a newer version. The default when no command is given. | `content/`, `state.json` |
| `status` | Print the installed hash, the latest hash, and both software versions | nothing |
| `self-update` | Replace this binary with the newest build for this platform | the binary |

The parser accepts one command. A second command is an error, and so is any unrecognised argument.

### Options

| Option | Argument | Applies to | Effect |
|---|---|---|---|
| `--force` | none | `sync`, `self-update` | With `sync`, download and extract even when the content is up to date. With `self-update`, reinstall the same version. |
| `--check` | none | `self-update` | Report the newer version and install nothing |
| `--no-update-check` | none | `sync` | Skip the software version check |
| `--config` | `<PATH>` | all | Import the provisioning file at `PATH` |
| `--keep-config` | none | all | Do not remove the provisioning file after the import |
| `--dir` | `<PATH>` | all | Use `PATH` as the root instead of `~/.brainmaker` |
| `--url` | `<URL>` | all | Use `URL` as the API base |
| `-q`, `--quiet` | none | all | Print errors only |
| `-h`, `--help` | none | — | Print the help text and exit 0 |
| `-V`, `--version` | none | — | Print `brainmaker <version>` and exit 0 |

`--config`, `--dir`, and `--url` fail when their value is absent.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | The content is up to date, or the update succeeded |
| 1 | The command failed. The previous content and the previous binary are unchanged. |

A failed software check during `sync` does not change the exit code. `brainmaker` prints
`notice: cannot check for a software update: ...` to stderr and exits 0, because the content is
already in place. `--quiet` suppresses that notice.

### `status` output

`status` prints one `key value` pair per line:

| Key | Value |
|---|---|
| `root` | The root directory |
| `content` | The content directory |
| `store` | Path of `config.enc` |
| `settings` | `imported from <path>`, `read from the sealed store`, or `read from the environment` |
| `api base` | The base URL in use |
| `token` | `absent`, or `present, N characters`. Never the token itself. |
| `key` | `release` or `development`, naming which build key this binary carries |
| `signing` | How many manifest signing keys this binary trusts. `0` means it installs no update. |
| `installed` | The hash in `state.json`, or `<none>` |
| `present` | `yes` when `content/` is a directory |
| `latest` | The hash the server reports |
| `state` | `up to date`, `stale`, or `not installed` |
| `software` | This binary's version |
| `platform` | This machine's manifest key, for example `darwin-arm64` |
| `published` | The manifest version, or `<unknown>` |
| `update` | `none`, `available; run brainmaker self-update`, or a reason |

## HTTP routes the server must serve

All three sit under one base URL, which comes from `SWETSI_API_BASE`. Serve them over TLS:
`brainmaker` refuses a plain-HTTP base URL unless its host is this machine. `brainmaker` sends
`Authorization: Bearer <token>` on every request when a token is configured, and sends the
`User-Agent` `brainmaker/<version>`.

Timeouts: 10 s to connect; 20 s total for a text request; 300 s total for a download.

### `GET {base}/content/latest`

Returns the hash of the current content.

```json
{ "hash": "a1b2c3d4" }
```

| Field | Rule |
|---|---|
| `hash` | Exactly 8 ASCII alphanumeric characters. Any other value fails the run. |

The response body is read up to 1 MiB. Serve this route with `Cache-Control: no-store`; a cached
response makes the client skip an update that is already published.

### `GET {base}/content/<hash>.zip`

Returns the zip archive for that hash. `<hash>` is the validated value from the route above.

| Condition | Client behavior |
|---|---|
| Body larger than 512 MiB | The partial file is deleted and the run fails |
| Empty body | The file is deleted and the run fails |
| Not a zip archive | The run fails; `content/` is untouched |

### `GET {base}/software/brainmaker`

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
      "url": "https://api.example.test/v1/brainmaker/software/brainmaker-0.2.0-darwin-arm64",
      "sha256": "e2959e4c4f210dbdfe848f49776a32279362a52760bbd0b26e3effb6cd0df350"
    },
    "darwin-x86_64": { "url": "...", "sha256": "..." },
    "linux-x86_64":  { "url": "...", "sha256": "..." }
  }
}
```

| Field | Rule |
|---|---|
| `version` | 1 to 64 characters of digits, dots, hyphens, plus signs, and ASCII letters |
| `platforms` | One key per platform, spelled `<os>-<arch>`. The client reads only its own key. |
| `url` | Must carry the same scheme, host, and port as the base URL, compared case-insensitively, with any userinfo discarded and the scheme's default port filled in |
| `sha256` | 64 hexadecimal characters, in either case |

Because the signature covers the bytes rather than a re-serialization, any proxy that reformats
this JSON body breaks every client. Serve it as a static file.

The client reads the body up to 1 MiB, and the binary it names up to 128 MiB. Serve this route with
`Cache-Control: no-store`.

Platform keys use `darwin` for macOS and `arm64` for `aarch64`. Run `brainmaker status` to read the
key a given machine asks for. A machine whose key is absent from the manifest gets an error naming
the keys that are present.

### Error responses

| Status | Message the client prints |
|---|---|
| 401, 403 | `the server rejected the request with HTTP <code>; set SWETSI_TOKEN to a valid bearer token` |
| 404 | `the server returned HTTP 404 Not Found` |
| other | `the server returned HTTP <code>` |
| timeout | `the request timed out` |
| DNS failure | `cannot resolve the host name` |

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
| `SWETSI_API_BASE` | Required. Must be an `https://` URL with a host, and carry no surrounding whitespace. `http://` is accepted only for `localhost` or a loopback address. |
| `SWETSI_TOKEN` | Optional. An empty value counts as absent. |

## `scripts/make-manifest.sh`

Writes the unsigned software manifest for a directory of built binaries.

```bash
scripts/make-manifest.sh <dist-dir> <version> [base-url]
```

| Argument | Required | Default |
|---|---|---|
| `<dist-dir>` | yes | — |
| `<version>` | yes | — |
| `[base-url]` | no | `https://api.example.test/v1/brainmaker/software` |

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
| `keygen <KEY-FILE>` | Write a new PKCS#8 Ed25519 key with mode `0600`, and print its public key. Fails when the file exists. |
| `sign <KEY-FILE\|-> <MANIFEST-FILE> <ENVELOPE-FILE>` | Check the manifest, sign it, and write the envelope |
| `verify <ENVELOPE-FILE> <PUBLIC-KEY>...` | Check an envelope against one or more public keys, the way `brainmaker` checks it |

Pass `-` in place of the key file to read the key from `$BRAINMAKER_SIGNING_KEY` as hexadecimal.
The release workflow uses that form, so the private key never reaches the runner's disk.

`sign` refuses a manifest that `brainmaker` could not use: one that is not JSON, one whose
`version` is empty or longer than 64 characters, one with no platform, one whose `url` does not use
`https://`, or one whose `sha256` is not 64 hexadecimal characters.

Both commands exit 0 on success and 1 on failure.
