# Packaging & Installers

Dadhichi ships as a single self-contained binary per platform, plus native
packages and one-line installers. This document describes the layout, how a
release is cut, and how to build each artifact locally.

## What ships

| Platform | Artifact | How it is built |
| --- | --- | --- |
| Linux x86_64 / aarch64 | `dadhichi-<target>.tar.gz` | `cargo build --release` |
| macOS x86_64 / aarch64 | `dadhichi-<target>.tar.gz`, `Dadhichi.app` | `cargo build` + `packaging/macos/bundle.sh` |
| Windows x86_64 | `dadhichi-x86_64-pc-windows-msvc.zip`, `.msi` | `cargo build` + WiX (`packaging/windows/dadhichi.wxs`) |
| Debian/Ubuntu | `.deb` | `cargo deb` (metadata in `crates/dadhichi/Cargo.toml`) |
| Fedora/RHEL | `.rpm` | `cargo generate-rpm` (same metadata) |
| macOS/Linux (Homebrew) | `dadhichi.rb` formula | `cargo install` from source (`packaging/homebrew/`) |

Every archive is published alongside a `.sha256` checksum. The installers verify
it before installing.

## Homebrew

Dadhichi ships a formula you can install as a tap:

```bash
brew tap pariharshyamu/dadhichi https://github.com/pariharshyamu/Dadhichi
brew install dadhichi                 # latest tagged release (builds from source)
brew install --HEAD dadhichi          # build the tip of the main branch
```

The formula builds only the `dadhichi` binary from the workspace, installs the
man page, and self-tests with `dadhichi --version`/`--help`. Its stable
`url`/`sha256`/`version` block is delimited by `# BEGIN stable` / `# END stable`
markers and rewritten on each release by `packaging/homebrew/update-formula.sh`
(the release workflow runs it and attaches the rendered `dadhichi.rb` to the
release). Until the first tag is cut, use `--HEAD`, which needs no checksum.

To refresh the formula manually after tagging:

```bash
# Downloads the source tarball and computes its sha256:
packaging/homebrew/update-formula.sh v0.1.0
# ...or pass a known checksum to stay offline:
packaging/homebrew/update-formula.sh v0.1.0 <sha256>
```

## One-line install

**Linux / macOS**

```bash
curl -fsSL https://raw.githubusercontent.com/pariharshyamu/Dadhichi/main/packaging/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/pariharshyamu/Dadhichi/main/packaging/install.ps1 | iex
```

Both scripts detect the OS/architecture, download the matching release asset,
verify its SHA-256 checksum, and install the binary (adding it to `PATH` on
Windows). They accept a specific `--version`/`-Version` and a custom install
directory, and support `--dry-run` for a no-download preview:

```bash
sh packaging/install.sh --version v0.1.0 --bin-dir "$HOME/.local/bin"
sh packaging/install.sh --dry-run          # print the plan, download nothing
```

## Layout

```
packaging/
├── install.sh                 # POSIX installer (Linux + macOS)
├── install.ps1                # PowerShell installer (Windows)
├── verify.sh                  # offline validation of every asset (runs in CI)
├── linux/
│   ├── dadhichi.desktop       # freedesktop application entry
│   ├── dadhichi.metainfo.xml  # AppStream metadata (software centres)
│   └── dadhichi.1             # man page
├── macos/
│   ├── Info.plist             # .app bundle manifest
│   └── bundle.sh              # assembles Dadhichi.app from a built binary
├── homebrew/
│   ├── dadhichi.rb            # Homebrew formula (tap)
│   └── update-formula.sh      # rewrites the formula's stable block on release
└── windows/
    └── dadhichi.wxs           # WiX v4 MSI definition
```

## Cutting a release

Pushing a semver tag triggers `.github/workflows/release.yml`:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The workflow:

1. Builds a release binary for each target in a matrix (Linux x86_64/aarch64,
   macOS x86_64/aarch64, Windows x86_64), archives it with the README, LICENSE,
   and man page, and emits a `.sha256`.
2. Builds the `.deb` and `.rpm` from the Linux binary.
3. Runs `packaging/verify.sh`, then publishes every artifact to a GitHub Release
   with auto-generated notes.

You can also run it on demand from the Actions tab (`workflow_dispatch`) with a
tag input.

## Building packages locally

```bash
# Debian package
cargo install cargo-deb
cargo build --release --bin dadhichi
cargo deb -p dadhichi --no-build

# RPM package
cargo install cargo-generate-rpm
cargo generate-rpm -p crates/dadhichi

# macOS .app bundle
cargo build --release --bin dadhichi
packaging/macos/bundle.sh target/release/dadhichi dist

# Windows MSI (on Windows, WiX toolset installed)
cargo build --release --bin dadhichi
wix build packaging/windows/dadhichi.wxs -d BinDir=target\release -d Version=0.1.0 -o dist\dadhichi.msi
```

## Version single-sourcing

The binary reports `env!("CARGO_PKG_VERSION")` via `dadhichi --version`, and
`bundle.sh` derives the bundle version from that output. `packaging/verify.sh`
asserts that the workspace version in `Cargo.toml` is referenced by the
`Info.plist`, the AppStream metainfo, and the man page — so a version bump that
misses an asset fails CI rather than shipping inconsistent packages.

## Validation

`packaging/verify.sh` runs offline in CI on every push. It checks that all
assets exist, the XML manifests are well-formed, the desktop entry has the
required keys, the version is consistent across assets, the shell scripts parse,
and `install.sh --dry-run`/`--help` behave. Run it locally with:

```bash
bash packaging/verify.sh
```
