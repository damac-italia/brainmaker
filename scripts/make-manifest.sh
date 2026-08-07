#!/usr/bin/env bash
#
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Writes the brainmaker software manifest for a directory of built binaries.
#
# Usage:
#   scripts/make-manifest.sh <dist-dir> <version> [base-url]
#
# The script reads every file in <dist-dir> named
#   brainmaker-<version>-<platform-key>[.exe]
# computes its SHA-256, and writes <dist-dir>/manifest.json.
#
# <base-url> is where the binaries will be served from. It defaults to
# https://api.example.test/v1/brainmaker/software and must match the API base
# that brainmaker uses, because brainmaker refuses a build URL on another host.
#
# Example:
#   scripts/make-manifest.sh dist 0.2.0
set -euo pipefail

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    echo "usage: $0 <dist-dir> <version> [base-url]" >&2
    exit 2
fi

DIST=$1
VERSION=$2
BASE_URL=${3:-https://api.example.test/v1/brainmaker/software}
BASE_URL=${BASE_URL%/}

if [ ! -d "$DIST" ]; then
    echo "error: $DIST is not a directory" >&2
    exit 1
fi

# brainmaker accepts up to 64 characters of digits, dots, hyphens, plus signs,
# and ASCII letters. Reject anything else here rather than at update time.
if ! printf '%s' "$VERSION" | grep -Eq '^[A-Za-z0-9.+-]{1,64}$'; then
    echo "error: $VERSION is not a valid version string" >&2
    exit 1
fi

# macOS ships shasum; Linux ships sha256sum.
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

entries=""
count=0

for path in "$DIST"/brainmaker-"$VERSION"-*; do
    [ -f "$path" ] || continue

    name=$(basename "$path")
    # Strip the prefix and the .exe suffix to recover the platform key.
    key=${name#brainmaker-$VERSION-}
    key=${key%.exe}

    if [ "$key" = "$name" ]; then
        echo "error: cannot read a platform key from $name" >&2
        exit 1
    fi

    sum=$(sha256 "$path")
    echo "  $key  $sum  $name" >&2

    [ -n "$entries" ] && entries="$entries,"
    entries="$entries
    \"$key\": {
      \"url\": \"$BASE_URL/$name\",
      \"sha256\": \"$sum\"
    }"
    count=$((count + 1))
done

if [ "$count" -eq 0 ]; then
    echo "error: $DIST holds no file named brainmaker-$VERSION-<platform-key>" >&2
    exit 1
fi

cat > "$DIST/manifest.json" <<EOF
{
  "version": "$VERSION",
  "platforms": {$entries
  }
}
EOF

echo "wrote $DIST/manifest.json with $count platform(s)" >&2
echo >&2
echo "This manifest is NOT signed, and brainmaker refuses an unsigned manifest." >&2
echo "Sign it before you publish:" >&2
echo >&2
echo "  cargo run --features sign --bin brainmaker-sign -- \\" >&2
echo "      sign signing.key $DIST/manifest.json $DIST/manifest.signed.json" >&2
echo >&2
echo "Then serve $DIST/manifest.signed.json as {base}/software/brainmaker." >&2
