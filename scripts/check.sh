#!/usr/bin/env bash
#
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Runs the three checks that the test workflow runs, with the same arguments,
# so that a run that passes here passes there.
#
# Usage:
#   scripts/check.sh
#
# The commands are copied from .github/workflows/test.yml. When that file
# changes, change this one.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "== cargo fmt" >&2
cargo fmt --all --check

echo "== cargo clippy" >&2
cargo clippy --workspace --all-targets --locked --features sign -- -D warnings

echo "== cargo test" >&2
cargo test --workspace --all-targets --locked --features sign

echo "all checks passed" >&2
