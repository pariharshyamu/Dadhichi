#!/usr/bin/env bash
#
# Rewrite the stable url/sha256/version block of the Homebrew formula for a
# given release tag. Run locally after tagging, or from the release workflow.
#
# Usage:
#   packaging/homebrew/update-formula.sh <version> [sha256]
#
#   <version>  Release version, with or without a leading "v" (e.g. 0.1.0 or v0.1.0).
#   [sha256]   SHA-256 of the source tarball. When omitted, it is downloaded
#              from GitHub and hashed (requires network + curl + shasum).
#
# The formula's managed region is delimited by:
#   # BEGIN stable — managed by packaging/homebrew/update-formula.sh
#   ...
#   # END stable
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
formula="$here/dadhichi.rb"
repo="pariharshyamu/Dadhichi"

raw_version="${1:?usage: update-formula.sh <version> [sha256]}"
version="${raw_version#v}"          # strip a leading v
tag="v${version}"
sha="${2:-}"

url="https://github.com/${repo}/archive/refs/tags/${tag}.tar.gz"

# Compute the checksum from the release tarball when one was not supplied.
if [ -z "$sha" ]; then
    tmp="$(mktemp)"
    trap 'rm -f "$tmp"' EXIT
    echo "fetching $url to compute sha256..." >&2
    curl -fsSL "$url" -o "$tmp"
    if command -v sha256sum >/dev/null 2>&1; then
        sha="$(sha256sum "$tmp" | awk '{print $1}')"
    else
        sha="$(shasum -a 256 "$tmp" | awk '{print $1}')"
    fi
fi

# Validate the checksum shape before writing it in.
case "$sha" in
    [0-9a-fA-F]) : ;;
esac
if ! printf '%s' "$sha" | grep -qE '^[0-9a-f]{64}$'; then
    echo "error: sha256 must be 64 lowercase hex chars, got: $sha" >&2
    exit 1
fi

# Replace the managed block atomically.
tmp_formula="$(mktemp)"
awk -v url="$url" -v sha="$sha" -v ver="$version" '
    /# BEGIN stable/ {
        print
        print "  url \"" url "\""
        print "  sha256 \"" sha "\""
        print "  version \"" ver "\""
        skip = 1
        next
    }
    /# END stable/ { skip = 0 }
    !skip { print }
' "$formula" > "$tmp_formula"

mv "$tmp_formula" "$formula"
echo "updated $formula -> $tag ($sha)"
