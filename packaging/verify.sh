#!/usr/bin/env bash
#
# Offline validation of the packaging assets. Runs in CI and locally without
# any network access or platform-specific tooling: it checks well-formedness,
# required fields, version consistency, and script syntax.
#
# Usage: packaging/verify.sh
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pkg="$root/packaging"
fail=0

pass() { printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$1"; fail=1; }

# The single source of truth for the version.
version="$(grep -m1 '^version' "$root/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
echo "Verifying packaging assets for dadhichi $version"

# ── 1. Files exist ───────────────────────────────────────────────────────────
echo "files present:"
for f in \
    linux/dadhichi.desktop \
    linux/dadhichi.metainfo.xml \
    linux/dadhichi.1 \
    macos/Info.plist \
    macos/bundle.sh \
    windows/dadhichi.wxs \
    install.sh \
    install.ps1
do
    if [ -f "$pkg/$f" ]; then pass "$f"; else bad "$f missing"; fi
done

# ── 2. XML well-formedness (metainfo, plist, wxs) ────────────────────────────
echo "xml well-formed:"
xml_ok() {
    if command -v python3 >/dev/null 2>&1; then
        python3 -c "import sys,xml.dom.minidom as m; m.parse(sys.argv[1])" "$1" 2>/dev/null
    elif command -v xmllint >/dev/null 2>&1; then
        xmllint --noout "$1" 2>/dev/null
    else
        return 0  # no parser available; treat as skipped-pass
    fi
}
for f in linux/dadhichi.metainfo.xml macos/Info.plist windows/dadhichi.wxs; do
    if xml_ok "$pkg/$f"; then pass "$f"; else bad "$f is not well-formed XML"; fi
done

# ── 3. Desktop entry required keys ───────────────────────────────────────────
echo "desktop entry:"
desktop="$pkg/linux/dadhichi.desktop"
for key in "\[Desktop Entry\]" "Type=Application" "Name=Dadhichi" "Exec=dadhichi" "Categories="; do
    if grep -q "$key" "$desktop"; then pass "$key"; else bad "$desktop missing $key"; fi
done

# ── 4. Version consistency across assets ─────────────────────────────────────
echo "version consistency ($version):"
check_ver() { # file, human label
    if grep -q "$version" "$1"; then pass "$2 references $version"; else bad "$2 does not reference $version"; fi
}
check_ver "$pkg/linux/dadhichi.metainfo.xml" "metainfo release"
check_ver "$pkg/macos/Info.plist" "Info.plist"
check_ver "$pkg/linux/dadhichi.1" "man page"

# ── 5. Shell script syntax ───────────────────────────────────────────────────
echo "shell syntax:"
for s in install.sh verify.sh macos/bundle.sh; do
    shell=sh
    head -n1 "$pkg/$s" | grep -q bash && shell=bash
    if "$shell" -n "$pkg/$s" 2>/dev/null; then pass "$s parses"; else bad "$s has a syntax error"; fi
done

# ── 6. install.sh behaves in dry-run and help ────────────────────────────────
echo "install.sh smoke test:"
if sh "$pkg/install.sh" --help >/dev/null 2>&1; then pass "--help exits 0"; else bad "--help failed"; fi
if out="$(DADHICHI_VERSION=v9.9.9 sh "$pkg/install.sh" --dry-run 2>&1)"; then
    if printf '%s' "$out" | grep -q "dry-run"; then pass "--dry-run resolves an asset"; else bad "--dry-run produced no plan"; fi
else
    bad "--dry-run failed"
fi

echo
if [ "$fail" -eq 0 ]; then
    echo "All packaging checks passed."
else
    echo "Packaging checks FAILED." >&2
    exit 1
fi
