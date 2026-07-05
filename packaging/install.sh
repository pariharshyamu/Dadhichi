#!/bin/sh
#
# Dadhichi installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/pariharshyamu/Dadhichi/main/packaging/install.sh | sh
#
# Detects the OS/arch, downloads the matching release tarball from GitHub,
# verifies its SHA-256 checksum, and installs the `dadhichi` binary.
#
# Options (as arguments when run from a file, or via env vars when piped):
#   --version <tag>   Install a specific release tag       (DADHICHI_VERSION)
#   --bin-dir <dir>   Install directory (default: see below) (DADHICHI_BIN_DIR)
#   --dry-run         Print what would happen, download nothing
#   --help            Show this help and exit
#
# POSIX sh only — no bashisms — so it runs under dash, busybox, and zsh.
set -eu

REPO="pariharshyamu/Dadhichi"
BIN_NAME="dadhichi"

VERSION="${DADHICHI_VERSION:-latest}"
BIN_DIR="${DADHICHI_BIN_DIR:-}"
DRY_RUN=0

log() { printf '%s\n' "dadhichi-install ▸ $*" >&2; }
err() { printf '%s\n' "dadhichi-install ✗ $*" >&2; exit 1; }

usage() {
    sed -n '3,17p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//' || cat <<'EOF'
Dadhichi installer. Options: --version <tag> --bin-dir <dir> --dry-run --help
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
        --version=*) VERSION="${1#*=}"; shift ;;
        --bin-dir) BIN_DIR="${2:?--bin-dir needs a value}"; shift 2 ;;
        --bin-dir=*) BIN_DIR="${1#*=}"; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) err "unknown option: $1 (try --help)" ;;
    esac
done

# ── Detect platform ──────────────────────────────────────────────────────────
os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
    Linux)  os_tag="unknown-linux-gnu" ;;
    Darwin) os_tag="apple-darwin" ;;
    *) err "unsupported OS: $os (Linux and macOS only; use install.ps1 on Windows)" ;;
esac

case "$arch" in
    x86_64|amd64) arch_tag="x86_64" ;;
    aarch64|arm64) arch_tag="aarch64" ;;
    *) err "unsupported architecture: $arch" ;;
esac

target="${arch_tag}-${os_tag}"

# ── Default install dir ──────────────────────────────────────────────────────
if [ -z "$BIN_DIR" ]; then
    if [ -w "/usr/local/bin" ] 2>/dev/null; then
        BIN_DIR="/usr/local/bin"
    else
        BIN_DIR="$HOME/.local/bin"
    fi
fi

# ── Resolve download URLs ────────────────────────────────────────────────────
asset="${BIN_NAME}-${target}.tar.gz"
if [ "$VERSION" = "latest" ]; then
    base="https://github.com/${REPO}/releases/latest/download"
else
    base="https://github.com/${REPO}/releases/download/${VERSION}"
fi
url="${base}/${asset}"
sum_url="${url}.sha256"

log "platform : ${target}"
log "version  : ${VERSION}"
log "asset    : ${asset}"
log "install  : ${BIN_DIR}/${BIN_NAME}"

if [ "$DRY_RUN" -eq 1 ]; then
    log "dry-run: would download ${url}"
    log "dry-run: would verify   ${sum_url}"
    log "dry-run: would install to ${BIN_DIR}/${BIN_NAME}"
    exit 0
fi

# ── Pick a downloader ────────────────────────────────────────────────────────
if command -v curl >/dev/null 2>&1; then
    dl() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    dl() { wget -qO "$2" "$1"; }
else
    err "need curl or wget to download"
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/dadhichi-install.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT INT TERM

log "downloading ${url}"
dl "$url" "$tmp/$asset" || err "download failed: $url"

# ── Verify checksum (best effort: skip only if none is published) ────────────
if dl "$sum_url" "$tmp/$asset.sha256" 2>/dev/null; then
    expected="$(awk '{print $1}' "$tmp/$asset.sha256")"
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
    else
        actual=""
        log "warning: no sha256sum/shasum available; skipping verification"
    fi
    if [ -n "$actual" ] && [ "$actual" != "$expected" ]; then
        err "checksum mismatch: expected $expected, got $actual"
    fi
    [ -n "$actual" ] && log "checksum ok"
else
    log "warning: no published checksum for $asset; skipping verification"
fi

# ── Unpack and install ───────────────────────────────────────────────────────
tar -xzf "$tmp/$asset" -C "$tmp" || err "failed to unpack $asset"
src="$(find "$tmp" -name "$BIN_NAME" -type f | head -n 1)"
[ -n "$src" ] || err "binary '$BIN_NAME' not found in archive"

mkdir -p "$BIN_DIR"
install -m 755 "$src" "$BIN_DIR/$BIN_NAME" 2>/dev/null \
    || { cp "$src" "$BIN_DIR/$BIN_NAME" && chmod 755 "$BIN_DIR/$BIN_NAME"; }

# The interactive terminal shell ships alongside the CLI, if present.
tui_src="$(find "$tmp" -name "$BIN_NAME-tui" -type f | head -n 1)"
if [ -n "$tui_src" ]; then
    install -m 755 "$tui_src" "$BIN_DIR/$BIN_NAME-tui" 2>/dev/null \
        || { cp "$tui_src" "$BIN_DIR/$BIN_NAME-tui" && chmod 755 "$BIN_DIR/$BIN_NAME-tui"; }
    log "installed $BIN_NAME-tui (interactive shell) to $BIN_DIR"
fi

log "installed $("$BIN_DIR/$BIN_NAME" --version 2>/dev/null || echo "$BIN_NAME") to $BIN_DIR"

case ":$PATH:" in
    *":$BIN_DIR:"*) : ;;
    *) log "note: $BIN_DIR is not on your PATH; add it to use '$BIN_NAME' directly" ;;
esac
