#!/usr/bin/env bash
#
# Assemble a macOS .app bundle from a built `dadhichi` binary.
#
# Usage:
#   packaging/macos/bundle.sh <path-to-dadhichi-binary> [output-dir]
#
# Produces "<output-dir>/Dadhichi.app". The bundle's Info.plist version is
# rewritten to match the binary's `--version` so the two never drift.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

bin="${1:-}"
out_dir="${2:-dist}"

if [ -z "$bin" ] || [ ! -x "$bin" ]; then
    echo "error: pass the path to an executable 'dadhichi' binary" >&2
    echo "usage: $0 <path-to-dadhichi-binary> [output-dir]" >&2
    exit 2
fi

# Derive the version from the binary itself (e.g. "dadhichi 0.1.0" -> "0.1.0").
version="$("$bin" --version | awk '{print $2}')"
version="${version:-0.0.0}"

app="$out_dir/Dadhichi.app"
macos_dir="$app/Contents/MacOS"
res_dir="$app/Contents/Resources"

rm -rf "$app"
mkdir -p "$macos_dir" "$res_dir"

install -m 755 "$bin" "$macos_dir/dadhichi"

# Copy the plist, substituting the version placeholders with the real version.
sed "s/>0\.1\.0</>$version</g" "$here/Info.plist" > "$app/Contents/Info.plist"

# Ship an icon if one exists next to this script.
if [ -f "$here/dadhichi.icns" ]; then
    install -m 644 "$here/dadhichi.icns" "$res_dir/dadhichi.icns"
fi

echo "created $app (version $version)"
