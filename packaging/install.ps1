<#
.SYNOPSIS
    Dadhichi installer for Windows.

.DESCRIPTION
    Downloads the matching release archive from GitHub, verifies its SHA-256
    checksum, and installs dadhichi.exe, adding the install directory to the
    user PATH.

.EXAMPLE
    irm https://raw.githubusercontent.com/pariharshyamu/Dadhichi/main/packaging/install.ps1 | iex

.PARAMETER Version
    Release tag to install (default: latest).

.PARAMETER BinDir
    Install directory (default: %LOCALAPPDATA%\Dadhichi\bin).

.PARAMETER DryRun
    Print what would happen without downloading anything.
#>
[CmdletBinding()]
param(
    [string]$Version = $(if ($env:DADHICHI_VERSION) { $env:DADHICHI_VERSION } else { 'latest' }),
    [string]$BinDir  = $(if ($env:DADHICHI_BIN_DIR) { $env:DADHICHI_BIN_DIR } else { "$env:LOCALAPPDATA\Dadhichi\bin" }),
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$Repo    = 'pariharshyamu/Dadhichi'
$BinName = 'dadhichi'

function Log($msg) { Write-Host "dadhichi-install > $msg" }

# Windows release currently ships x86_64 only.
$arch = if ([Environment]::Is64BitOperatingSystem) { 'x86_64' } else {
    throw 'Dadhichi requires a 64-bit version of Windows.'
}
$target = "$arch-pc-windows-msvc"
$asset  = "$BinName-$target.zip"

$base = if ($Version -eq 'latest') {
    "https://github.com/$Repo/releases/latest/download"
} else {
    "https://github.com/$Repo/releases/download/$Version"
}
$url    = "$base/$asset"
$sumUrl = "$url.sha256"

Log "platform : $target"
Log "version  : $Version"
Log "asset    : $asset"
Log "install  : $BinDir\$BinName.exe"

if ($DryRun) {
    Log "dry-run: would download $url"
    Log "dry-run: would verify   $sumUrl"
    Log "dry-run: would install to $BinDir\$BinName.exe"
    return
}

$tmp = Join-Path $env:TEMP ("dadhichi-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
    $archivePath = Join-Path $tmp $asset
    Log "downloading $url"
    Invoke-WebRequest -Uri $url -OutFile $archivePath -UseBasicParsing

    # Verify checksum when one is published.
    try {
        $sumPath = "$archivePath.sha256"
        Invoke-WebRequest -Uri $sumUrl -OutFile $sumPath -UseBasicParsing
        $expected = (Get-Content $sumPath -Raw).Trim().Split()[0]
        $actual   = (Get-FileHash $archivePath -Algorithm SHA256).Hash.ToLower()
        if ($actual -ne $expected.ToLower()) {
            throw "checksum mismatch: expected $expected, got $actual"
        }
        Log 'checksum ok'
    } catch [System.Net.WebException] {
        Log "warning: no published checksum for $asset; skipping verification"
    }

    Expand-Archive -Path $archivePath -DestinationPath $tmp -Force
    $exe = Get-ChildItem -Path $tmp -Filter "$BinName.exe" -Recurse | Select-Object -First 1
    if (-not $exe) { throw "binary '$BinName.exe' not found in archive" }

    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    Copy-Item -Path $exe.FullName -Destination (Join-Path $BinDir "$BinName.exe") -Force

    # The interactive terminal shell ships alongside the CLI, if present.
    $tui = Get-ChildItem -Path $tmp -Filter "$BinName-tui.exe" -Recurse | Select-Object -First 1
    if ($tui) {
        Copy-Item -Path $tui.FullName -Destination (Join-Path $BinDir "$BinName-tui.exe") -Force
        Log "installed $BinName-tui.exe (interactive shell)"
    }

    # Add to the user PATH if it is not already there.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -notlike "*$BinDir*") {
        [Environment]::SetEnvironmentVariable('Path', "$userPath;$BinDir", 'User')
        Log "added $BinDir to your user PATH (restart your shell to pick it up)"
    }

    $installed = & (Join-Path $BinDir "$BinName.exe") --version
    Log "installed $installed to $BinDir"
} finally {
    Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
