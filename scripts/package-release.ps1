# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Pack the rolling zip of takt from a prebuilt program: takt-windows-x86_64.zip and SHA256SUMS.

.DESCRIPTION
Nothing is built here. The zip holds, in one folder, exactly what scripts\install.ps1 installs:

  takt-player.exe     the player (with its icon compiled in)
  install.ps1         installs this zip's content for the current user
  uninstall.ps1, LICENSE (GPL-3.0-or-later), README.md (how to install), VERSION

VERSION says <version of takt-player>+<short commit>. The zip is reproducible: entries sorted, a fixed
timestamp. SHA256SUMS ("<hash>  <file>") is written next to it for a local check; the publish action
of appkit's release-kit makes the one of the pre-release.

A versioned release (a tag, release.yml) comes with Gate 0 (`v0.1.0`); until then there is only the
rolling build.

build-install.ps1 uses -StageDir and -NoZip: it stages the same folder from a fresh build and hands it
to install.ps1, so the checkout install and the zip install are one code path.

.PARAMETER BuildDir
The folder with takt-player.exe (default target\release of this checkout).

.PARAMETER OutDir
Where the zip and SHA256SUMS go (default dist\ in the checkout). Not needed with -NoZip.

.PARAMETER StageDir
Stage the folder here (it must not exist or be empty) and keep it.

.PARAMETER NoZip
Only stage (-StageDir is then required).

.PARAMETER Sha
The commit of the build (7 to 40 hex digits); the first 7 go into the version label. Without it the
label is <version>+0000000 (a local build).

.EXAMPLE
.\scripts\package-release.ps1 -Sha 0123456789abcdef0123456789abcdef01234567 -OutDir dist
#>
[CmdletBinding()]
param(
    [string] $BuildDir = '',
    [string] $OutDir = '',
    [string] $StageDir = '',
    [switch] $NoZip,
    [string] $Sha = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$zipName = 'takt-windows-x86_64.zip'

function Exit-Package([string] $Message) {
    throw "package-release.ps1: $Message"
}

$root = Split-Path -Parent $PSScriptRoot
if (-not $BuildDir) { $BuildDir = Join-Path $root 'target\release' }
if ($NoZip -and -not $StageDir) { Exit-Package '-NoZip stages only: give -StageDir' }
if (-not $NoZip -and -not $OutDir) { $OutDir = Join-Path $root 'dist' }

# The version is the one of the program (apps/takt-player/Cargo.toml, the first version at the start of
# a line: that of [package]).
$cargoText = Get-Content -LiteralPath (Join-Path $root 'apps\takt-player\Cargo.toml') -Raw
if ($cargoText -notmatch '(?m)^version\s*=\s*"([^"]+)"') { Exit-Package 'no version in apps/takt-player/Cargo.toml' }
$appVersion = $Matches[1]
if ($appVersion -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { Exit-Package "the version '$appVersion' is not X.Y.Z" }
if (-not $Sha) { $Sha = '0000000' }
if ($Sha -cnotmatch '^[0-9a-f]{7,40}$') { Exit-Package "-Sha must be a commit id in lower case hex (got '$Sha')" }
$label = "$appVersion+$($Sha.Substring(0, 7))"

$exe = Join-Path $BuildDir 'takt-player.exe'
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { Exit-Package "no $exe (build first: cargo build --release --locked -p takt-player)" }
if ((Get-Item -LiteralPath $exe).Length -eq 0) { Exit-Package "$exe is empty" }

if ($StageDir) {
    if ((Test-Path -LiteralPath $StageDir) -and (Get-ChildItem -LiteralPath $StageDir -Force)) { Exit-Package "$StageDir is not empty" }
    $stage = $StageDir
    $temporary = $false
}
else {
    $stage = Join-Path ([IO.Path]::GetTempPath()) ('takt-stage-' + [Guid]::NewGuid().ToString('N'))
    $temporary = $true
}

try {
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    Copy-Item -LiteralPath $exe -Destination (Join-Path $stage 'takt-player.exe') -Force
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install.ps1') -Destination (Join-Path $stage 'install.ps1') -Force
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'uninstall.ps1') -Destination (Join-Path $stage 'uninstall.ps1') -Force
    Copy-Item -LiteralPath (Join-Path $root 'LICENSES\GPL-3.0-or-later.txt') -Destination (Join-Path $stage 'LICENSE') -Force
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'release-readme.md') -Destination (Join-Path $stage 'README.md') -Force
    # LF and no BOM: it is read by install.ps1 and by people.
    [IO.File]::WriteAllText((Join-Path $stage 'VERSION'), "$label`n", (New-Object Text.UTF8Encoding($false)))

    if ($NoZip) {
        Write-Output "staged $label in $stage"
        return
    }

    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $zipPath = Join-Path (Resolve-Path -LiteralPath $OutDir).ProviderPath $zipName
    $sumsPath = Join-Path (Split-Path -Parent $zipPath) 'SHA256SUMS'
    # The old checksum goes first: a zip that fails halfway must not leave it beside it.
    Remove-Item -LiteralPath $sumsPath, $zipPath -Force -ErrorAction SilentlyContinue
    $names = @(Get-ChildItem -LiteralPath $stage -File -Force | ForEach-Object { $_.Name })
    [Array]::Sort($names, [StringComparer]::Ordinal)
    Add-Type -AssemblyName System.IO.Compression
    $fixed = New-Object DateTimeOffset(2000, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
    $stream = [IO.File]::Create($zipPath)
    try {
        $archive = New-Object IO.Compression.ZipArchive($stream, [IO.Compression.ZipArchiveMode]::Create)
        try {
            foreach ($rel in $names) {
                $entry = $archive.CreateEntry($rel, [IO.Compression.CompressionLevel]::Optimal)
                $entry.LastWriteTime = $fixed
                $target = $entry.Open()
                try {
                    $source = [IO.File]::OpenRead((Join-Path $stage $rel))
                    try { $source.CopyTo($target) } finally { $source.Dispose() }
                }
                finally { $target.Dispose() }
            }
        }
        finally { $archive.Dispose() }
    }
    finally { $stream.Dispose() }

    $hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText($sumsPath, "$hash  $zipName`n", (New-Object Text.UTF8Encoding($false)))
    Write-Output "packaged $label"
    Write-Output $zipPath
    Write-Output "$hash  $zipName"
}
finally {
    if ($temporary -and (Test-Path -LiteralPath $stage)) { Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue }
}
