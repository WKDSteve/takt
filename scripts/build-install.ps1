# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Build takt-player from this checkout and install it for the current user, from the terminal.

.DESCRIPTION
The same entry point the other desktop apps have (scripts/build-install.ps1, scripts/uninstall.ps1),
for a machine where no setup may be started: `cargo build --release --locked -p takt-player`, then
the build is staged in the shape of the rolling zip (scripts\package-release.ps1) and installed by
its scripts\install.ps1: %LOCALAPPDATA%\takt\bin (or $env:TAKT_INSTALL_DIR), the Start menu entry
"Takt", an entry in Settings > Apps. No administrator rights; a file built on this machine carries no
Mark-of-the-Web.

The build links libmpv and FFmpeg only when TAKT_LIBMPV_DIR / FFMPEG_DIR point at them
(docs/BUILD.md); without them it is the NullBackend build the CI makes. At run time the DLLs must be
on the PATH or next to the exe: this script does not copy them.

Run it in a normal terminal: a Claude session (the Claude app redirects AppData and HKCU) may
build (-BuildOnly), installing there is refused.

.PARAMETER BuildOnly
Build target\release\takt-player.exe and stop (no install).

.PARAMETER AllowElevated
Install although the session is elevated (for a build machine).

.PARAMETER DesktopShortcut
Also put a shortcut on the desktop.

.EXAMPLE
.\scripts\build-install.ps1
.\scripts\build-install.ps1 -BuildOnly
#>
[CmdletBinding()]
param(
    [switch] $BuildOnly,
    [switch] $AllowElevated,
    [switch] $DesktopShortcut
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$name = 'takt-player.exe'

function Exit-Build([string] $Message) {
    throw "build-install.ps1: $Message"
}

if (-not $BuildOnly) {
    if ($env:CLAUDECODE) {
        Exit-Build 'install in a normal terminal, not in a Claude session (the app redirects AppData and the registry); -BuildOnly builds'
    }
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $AllowElevated -and $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        Exit-Build 'do not run this elevated: it installs for one user (-AllowElevated for a build machine)'
    }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Exit-Build 'cargo is not on PATH' }

$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root "target\release\$name"

# cargo cannot replace a program that runs from target\release ("access denied"): name it instead.
$running = @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
        $path = $null
        try { $path = $_.Path } catch { }
        $path -and $path -ieq $exe
    })
if ($running.Count -gt 0) {
    Exit-Build "takt-player (PID $(($running | ForEach-Object Id) -join ', ')) runs from $exe and blocks the build: close it and run again"
}

Push-Location $root
try {
    Write-Output '==> cargo build --release --locked -p takt-player'
    cargo build --release --locked -p takt-player
    if ($LASTEXITCODE -ne 0) { Exit-Build "cargo build failed (exit code $LASTEXITCODE)" }
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { Exit-Build "the build made no $name" }
    # cargo leaves an exe that is already current untouched; the time says "built now" to :MyNative
    (Get-Item -LiteralPath $exe).LastWriteTime = Get-Date
    Write-Output "built $exe"
    if ($BuildOnly) { return }

    # The same folder the rolling zip holds, staged from this build and installed by its install.ps1:
    # one code path for both (Start menu entry, Settings > Apps, swap with rollback).
    $sha = (git -C $root rev-parse HEAD 2>$null)
    if ($LASTEXITCODE -ne 0 -or -not $sha) { $sha = '0000000' }
    $stage = Join-Path ([IO.Path]::GetTempPath()) ('takt-stage-' + [Guid]::NewGuid().ToString('N'))
    try {
        & (Join-Path $PSScriptRoot 'package-release.ps1') -BuildDir (Split-Path -Parent $exe) -StageDir $stage -NoZip -Sha $sha
        $installArgs = @{ PayloadDir = $stage }
        if ($AllowElevated) { $installArgs.AllowElevated = $true }
        if ($DesktopShortcut) { $installArgs.DesktopShortcut = $true }
        & (Join-Path $stage 'install.ps1') @installArgs
    }
    finally {
        Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
    }
}
finally {
    Pop-Location
}
