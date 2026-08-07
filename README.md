<h1 align="center">brainmaker</h1>
<p align="center">Keeps <code>~/.brainmaker/content</code> in step with the content API.</p>
<p align="center">
  <img alt="license: GPL-3.0-or-later" src="https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg">
</p>

`brainmaker` is a single Rust binary. It reads a content hash from an HTTP API, downloads the
matching zip archive when the local copy is out of date, and replaces `~/.brainmaker/content/`
with the extracted result. It also replaces itself with a newer published build when you run
`self-update`. It is not a content editor and not a sync daemon: it runs once per invocation and
exits.

The binary carries no endpoint and no token. Both arrive in a provisioning file that you issue.

## Quick start

`brainmaker` needs a provisioning file before it can reach a server. Your administrator issues it.

```bash
cargo install --path .
```

The binary lands in `~/.cargo/bin/brainmaker`.

Put the issued `brainmaker.env` next to the binary, then run:

```bash
brainmaker sync
```

The first run prints the import lines, then the content check:

```text
Imported the configuration from ./brainmaker.env
Removed ./brainmaker.env after the import.
Checking the latest content version.
```

The settings are now sealed at `~/.brainmaker/confidential/config.enc`, and the plain file is
gone. Later runs read the sealed copy and need no file.

## Features

| Feature | What it does |
|---|---|
| Hash comparison | Skips the download when `state.json` matches `GET {base}/content/latest` |
| Atomic swap | Extracts to `.staging`, then replaces `content/` with two renames |
| Rollback | Restores the previous `content/` when the second rename fails |
| Zip hardening | Rejects escaping paths and symbolic links; caps one entry at 256 MiB |
| Sealed settings | Stores the endpoint and token AES-256-GCM encrypted, bound to the machine |
| No compiled endpoint | A unit test fails the build if a URL with a host enters `src/config.rs` |
| TLS-only base URL | Refuses a plain-HTTP base URL, except one whose host is this machine |
| Signed updates | Refuses a software manifest without an Ed25519 signature from a compiled-in key |
| Self-update | Verifies origin, SHA-256, and `--version` output before it swaps the binary |
| Static Linux builds | `x86_64` and `arm64` link against musl, so there is no glibc version floor |

## Usage

### Update the content

```bash
brainmaker sync
```

`sync` is the default command, so `brainmaker` alone does the same.

### Read the current state

```bash
brainmaker status
```

`status` prints the root, the store path, the settings source, the API base, the token length, the
key class, the number of trusted signing keys, the installed hash, the latest hash, the platform
key, and the published version. It changes nothing.

### Replace the binary

```bash
brainmaker self-update
```

`self-update` installs only a manifest signed by a key in `PUBLIC_KEYS` in
[src/signature.rs](src/signature.rs). A build with an empty list installs nothing, and
`brainmaker status` reports `signing   0 trusted key(s)`. See [Signing keys](#signing-keys).

### Run it at the start of every Claude session

Add a `SessionStart` hook to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "brainmaker sync --quiet --no-update-check",
            "timeout": 60
          }
        ]
      }
    ]
  }
}
```

`--quiet` keeps a successful run silent. A failed run still prints to stderr and exits 1.
`--no-update-check` drops one HTTP request per session. The hook installs no binary; run
`brainmaker self-update` yourself.

## Configuration

### Provisioning file

The file is `KEY=VALUE` text. The parser skips blank lines and `#` comments, accepts an `export `
prefix, and strips one pair of matching quotes. It keeps any key it does not read, so a file
written for a later version survives the round trip through the sealed store.

```text
# brainmaker.env - issued by IT, do not share
SWETSI_API_BASE=https://api.example.test/v1/brainmaker
SWETSI_TOKEN=the-token-you-issued
```

| Key | Default | Description |
|---|---|---|
| `SWETSI_API_BASE` | none | Required. Base of every API route. Must use `https://`, unless the host is `localhost` or a loopback address. |
| `SWETSI_TOKEN` | none | Optional. Sent as `Authorization: Bearer <value>`. |

`brainmaker` searches for the file in this order, and imports the first hit:

1. `--config <PATH>`. A path that is not a file is an error, not a silent skip.
2. `$BRAINMAKER_CONFIG`. A path that is not a file is an error.
3. `brainmaker.env`, then `.brainmaker.env`, next to the running binary.
4. `brainmaker.env`, then `.brainmaker.env`, in the working directory.

`brainmaker` never reads a bare `.env`, so running it inside another project cannot import and then
delete that project's file.

To change endpoints later, issue a new file and have the employee drop it next to the binary. The
next run imports it, overwrites the sealed store, and removes the file.

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `SWETSI_API_BASE` | the sealed value | Base of every API route. Overrides the sealed value. The TLS rule above applies to it. |
| `SWETSI_TOKEN` | the sealed value | Bearer token. Overrides the sealed value. Unset means no `Authorization` header. |
| `BRAINMAKER_CONFIG` | unset | Path of the provisioning file to import. |
| `BRAINMAKER_CONFIG_KEY` | a development key | Build-time only. Seals the stored settings. |

Precedence for the base URL, strongest first: `--url`, then the environment, then the sealed store.

### Options

| Option | Effect |
|---|---|
| `--force` | With `sync`, download and extract even when the content is up to date. With `self-update`, reinstall the same version. |
| `--check` | With `self-update`, report the newer version and install nothing |
| `--no-update-check` | With `sync`, skip the software version check |
| `--config <PATH>` | Import the provisioning file at `PATH` |
| `--keep-config` | Do not remove the provisioning file after the import |
| `--dir <PATH>` | Use `PATH` as the root instead of `~/.brainmaker` |
| `--url <URL>` | Use `URL` as the API base |
| `-q`, `--quiet` | Print errors only |
| `-h`, `--help` | Print the help text |
| `-V`, `--version` | Print the version |

`--dir` and `--url` let you test against a staging server without touching the real content. `--url`
obeys the same TLS rule as the provisioning file, so a local test server needs an address such as
`http://localhost:8080`.
`--keep-config` leaves the provisioning file in place and prints a warning on every run, because
that file still holds the token.

## Documentation

- [Architecture](docs/ARCHITECTURE.md) — modules, boundaries, data model, decisions
- [Flow](docs/FLOW.md) — the sync, provisioning, and self-update paths
- [API](docs/API.md) — the CLI surface and the three HTTP routes the server must serve
- [Security](docs/SECURITY.md) — trust model, secret handling, input validation

## Build and release

[`.github/workflows/release.yml`](.github/workflows/release.yml) builds five platforms. Each target
builds on its own native runner, so nothing is cross-compiled.

| Platform key | Runner | Rust target | Linking |
|---|---|---|---|
| `darwin-arm64` | `macos-14` | `aarch64-apple-darwin` | dynamic, system libraries |
| `darwin-x86_64` | `macos-13` | `x86_64-apple-darwin` | dynamic, system libraries |
| `linux-x86_64` | `ubuntu-22.04` | `x86_64-unknown-linux-musl` | static |
| `linux-arm64` | `ubuntu-22.04-arm` | `aarch64-unknown-linux-musl` | static |
| `windows-x86_64` | `windows-2022` | `x86_64-pc-windows-msvc` | dynamic, system libraries |

The workflow signs each macOS build with an ad-hoc signature (`codesign -s -`). Without it, a
binary that arrives over the network does not start on Apple Silicon. Every build job then runs its
own binary with `--version` and compares the output against `Cargo.toml`.

### Signing keys

`brainmaker` installs only a software manifest signed with an Ed25519 key it trusts. Set this up
once, before your first release.

```bash
cargo run --features sign --bin brainmaker-sign -- keygen signing.key
```

The tool writes the private key with mode `0600` and prints the public key. Then:

1. Paste the printed line into `PUBLIC_KEYS` in [src/signature.rs](src/signature.rs), and commit
   it. The public key is public, so it belongs in the repository.
2. Store the contents of `signing.key` in the repository secret `BRAINMAKER_SIGNING_KEY`.
3. Keep `signing.key` off every machine that serves the API, and out of git.

To rotate the key, generate a new pair, put the new public key **first** in `PUBLIC_KEYS`, keep the
old one, and release. Remove the old key only once every client runs a binary that holds the new
one. A manifest verifies when any listed key accepts it.

### Repository settings the workflow needs

| Kind | Name | Purpose |
|---|---|---|
| Secret | `BRAINMAKER_CONFIG_KEY` | Seals each employee's stored settings. The workflow fails without it. |
| Secret | `BRAINMAKER_SIGNING_KEY` | Signs the software manifest. The manifest job fails without it. |
| Variable | `SOFTWARE_BASE_URL` | Where you serve the binaries. The manifest URLs come from it. The manifest job fails while it holds the `example.test` placeholder. |

The build job also fails when `PUBLIC_KEYS` in `src/signature.rs` is empty, because such a binary
could never install an update.

### Release steps

1. Raise `version` in `Cargo.toml` and commit `Cargo.lock`. The workflow runs
   `cargo build --release --locked`, which needs a committed lock file.
2. Push a matching tag: `git tag v0.2.0 && git push origin v0.2.0`. The manifest job fails if the
   tag and `Cargo.toml` disagree.
3. Download the release assets.
4. Upload the five binaries to `{base}/software/`.
5. Upload `manifest.signed.json` to `{base}/software/brainmaker` **last**. A manifest that names
   binaries you have not uploaded makes every `self-update` fail.

Serve `manifest.signed.json` byte for byte. The signature covers the exact bytes, so a proxy that
reformats the JSON breaks every client. `manifest.json` is the unsigned copy, kept for reading; do
not publish it.

`workflow_dispatch` runs the same build without creating a release.

### Build locally

macOS builds both of its own targets with no extra tooling:

```bash
rustup target add x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
```

The Linux and Windows targets need a C cross-compiler, because `ring` compiles C. `rustup target
add` alone is not enough. Use the workflow, or install `cargo-zigbuild` or `cross`.

Name each binary `brainmaker-<version>-<platform-key>[.exe]`, put them in one directory, and write
the manifest:

```bash
scripts/make-manifest.sh dist 0.2.0
```

The script computes each SHA-256 and writes `dist/manifest.json`. A third argument sets the base
URL; it defaults to `https://api.example.test/v1/brainmaker/software`.

That manifest is unsigned, and `brainmaker` refuses an unsigned manifest. Sign it, then check the
result against the keys this revision compiles in:

```bash
cargo run --features sign --bin brainmaker-sign -- sign signing.key dist/manifest.json dist/manifest.signed.json
```

```bash
cargo run --features sign --bin brainmaker-sign -- verify dist/manifest.signed.json $(grep -Eo '"[0-9a-fA-F]{64}"' src/signature.rs | tr -d '"')
```

### Building linux-arm64 without an arm64 runner

GitHub's `ubuntu-22.04-arm` runners are free for public repositories and need a paid plan for
private ones. If that job cannot start, cross-compile on the x86_64 runner: set the `linux-arm64`
matrix runner to `ubuntu-22.04`, install `gcc-aarch64-linux-gnu`, target
`aarch64-unknown-linux-gnu`, and set both `CC_aarch64_unknown_linux_gnu` and
`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER` to `aarch64-linux-gnu-gcc`. Then delete that job's
`--version` check, because an arm64 binary does not run on an x86_64 runner. That build links
against glibc, so it will not run on an older distribution.

## Project structure

<details>
<summary>Directory layout</summary>

```text
src/                     the crate, one module per concern (13 files)
tools/                   sign.rs, the signing tool; builds only under the sign feature
scripts/                 make-manifest.sh, which writes the software manifest
.github/workflows/       release.yml, the five-platform build
build.rs                 declares the BRAINMAKER_CONFIG_KEY rebuild dependency
docs/                    architecture, flow, API, and security documents
```
</details>

Module roles are in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Contributing

```bash
cargo test
cargo clippy --all-targets --features sign
cargo fmt --check
```

The test suite is 87 unit tests in `#[cfg(test)]` modules beside the code they cover. Pass
`--features sign` to clippy so that the signing tool is linted too; a plain `cargo build` skips it.

A local `cargo build` uses the development key, so a locally built binary cannot open a store
written by a release build, and the reverse also holds. That is the key binding working. Reimport
the provisioning file after you switch binaries.

## License

Copyright (C) 2026 atom7xyz

`brainmaker` is free software: you can redistribute it and modify it under the terms of the GNU
General Public License as published by the Free Software Foundation, either version 3 of the
License, or (at your option) any later version. [LICENSE](LICENSE) holds the full text.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without
even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU
General Public License for more details.

Every one of the 77 crates in `Cargo.lock` carries a permissive license that GPL-3 accepts: MIT,
Apache-2.0, ISC, BSD-3-Clause, 0BSD, Zlib, Unlicense, Unicode-3.0, CDLA-Permissive-2.0, or MPL-2.0
without an Exhibit B notice. List them with:

```bash
cargo tree --format '{p} {l}'
```
