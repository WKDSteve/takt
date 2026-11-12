# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Install takt for the current user from an unpacked release zip (or a staged folder).

.DESCRIPTION
Run it from the folder you unpacked takt-windows-x86_64.zip into. It installs exactly what that
folder holds, all in one folder (%LOCALAPPDATA%\takt\bin):

  takt-player.exe, uninstall.ps1, LICENSE, README.md, VERSION

plus a Start menu entry "Takt" with the icon of the program, on request a desktop shortcut, and an
entry in Settings > Apps (which runs the installed uninstall.ps1). No administrator rights. The files
are all copied first and then swapped in; a copy or a swap that fails puts the old ones back, so the
install is never a mix of versions. A takt that runs goes on with the old program until it is closed.

Nothing is signed: Windows (SmartScreen) may ask before the first start. Run it in a normal terminal:
a Claude session (the Claude app redirects AppData and HKCU) is refused.

This is the one installer: scripts\build-install.ps1 stages a fresh build in the same shape and calls
this script.

.PARAMETER PayloadDir
The folder to install from (default: the folder of this script, i.e. the unpacked zip).

.PARAMETER DesktopShortcut
Also put a shortcut on the desktop (off by default; the Start menu entry is always made).

.PARAMETER AllowElevated
Install although the session is elevated (for a build machine).

.PARAMETER InstallDir
Where the program goes (default $env:TAKT_INSTALL_DIR, else %LOCALAPPDATA%\takt\bin).

.PARAMETER StartMenuDir
Where the Start menu entry goes (default $env:TAKT_START_MENU_DIR, else the Start menu programs folder
of the current user).

.PARAMETER DesktopDir
Where the desktop shortcut goes (default $env:TAKT_DESKTOP_DIR, else the desktop of the current user).

.PARAMETER RegistryRoot
The key that stands for HKCU\Software; the Settings > Apps entry is below it (default
$env:TAKT_REGISTRY_ROOT, else HKCU:\Software). For tests.

.EXAMPLE
.\install.ps1
.\install.ps1 -DesktopShortcut
# Windows PowerShell 5.1 under its default execution policy (Restricted), from the unpacked folder:
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
#>
[CmdletBinding()]
param(
    [switch] $AllowElevated,
    [switch] $DesktopShortcut,
    [string] $DesktopDir = '',
    [string] $PayloadDir = '',
    [string] $InstallDir = '',
    [string] $StartMenuDir = '',
    [string] $RegistryRoot = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$program = 'takt-player'
# What a payload holds, no more and no less: anything else is a folder that is not a release.
$payloadFiles = @('takt-player.exe', 'install.ps1', 'uninstall.ps1', 'LICENSE', 'README.md', 'VERSION')

function Exit-Install([string] $Message) {
    throw "install.ps1: $Message"
}

function Get-Setting([string] $Given, [string] $EnvName, [string] $Default) {
    if ($Given) { return $Given }
    $fromEnv = [Environment]::GetEnvironmentVariable($EnvName)
    if ($fromEnv) { return $fromEnv }
    return $Default
}

# An absolute folder that is not a drive root, without a trailing backslash (a "C:\x\" would end the
# quoted argument of the Settings > Apps command line with \", which escapes the quote).
function Get-Place([string] $Dir) {
    if (-not $Dir -or $Dir.Contains('"') -or -not [IO.Path]::IsPathRooted($Dir)) { Exit-Install "not an absolute path: '$Dir'" }
    $full = [IO.Path]::GetFullPath($Dir)
    if ($full.TrimEnd('\') -eq ([IO.Path]::GetPathRoot($full)).TrimEnd('\')) { Exit-Install "a drive root is no place for this: '$Dir'" }
    return $full.TrimEnd('\')
}

# The registry root stands for HKCU\Software: a key of the current user, never another hive.
function Get-RegistryRoot([string] $Key) {
    $Key = $Key.TrimEnd('\')
    if ($Key.Contains('"') -or $Key -notmatch '^(HKCU:|Registry::HKEY_CURRENT_USER)\\.') { Exit-Install "not a key below HKCU: '$Key'" }
    return $Key
}

# Whether $Dir holds a takt install: the program (also a .new/.old of a cut short install), or the
# VERSION and uninstall.ps1 of a release.
function Test-Installation([string] $Dir) {
    foreach ($suffix in '', '.new', '.old') {
        if (Test-Path -LiteralPath (Join-Path $Dir "$program.exe$suffix") -PathType Leaf) { return $true }
    }
    return (Test-Path -LiteralPath (Join-Path $Dir 'uninstall.ps1') -PathType Leaf) -and (Test-Path -LiteralPath (Join-Path $Dir 'VERSION') -PathType Leaf)
}

if ($env:CLAUDECODE) {
    Exit-Install 'install in a normal terminal, not in a Claude session (the app redirects AppData and the registry)'
}
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $AllowElevated -and $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Exit-Install 'do not run this elevated: it installs for one user (-AllowElevated for a build machine)'
}

$InstallDir = Get-Place (Get-Setting $InstallDir 'TAKT_INSTALL_DIR' (Join-Path $env:LOCALAPPDATA 'takt\bin'))
$StartMenuDir = Get-Place (Get-Setting $StartMenuDir 'TAKT_START_MENU_DIR' ([Environment]::GetFolderPath('Programs')))
$DesktopDir = Get-Place (Get-Setting $DesktopDir 'TAKT_DESKTOP_DIR' ([Environment]::GetFolderPath('Desktop')))
$RegistryRoot = Get-RegistryRoot (Get-Setting $RegistryRoot 'TAKT_REGISTRY_ROOT' 'HKCU:\Software')

# The payload: the shape of the release zip, checked before anything is written.
if (-not $PayloadDir) { $PayloadDir = $PSScriptRoot }
if (-not (Test-Path -LiteralPath $PayloadDir -PathType Container)) { Exit-Install "no such folder: $PayloadDir" }
$PayloadDir = (Resolve-Path -LiteralPath $PayloadDir).ProviderPath.TrimEnd('\')
foreach ($item in Get-ChildItem -LiteralPath $PayloadDir -Force) {
    if ($payloadFiles -cnotcontains $item.Name -or $item.PSIsContainer) {
        Exit-Install "$PayloadDir holds '$($item.Name)', which a release does not: nothing was installed"
    }
}
foreach ($name in $payloadFiles) {
    if (-not (Test-Path -LiteralPath (Join-Path $PayloadDir $name) -PathType Leaf)) {
        Exit-Install "no $name in $PayloadDir (run this from the unpacked release zip)"
    }
}
$version = (Get-Content -LiteralPath (Join-Path $PayloadDir 'VERSION') -TotalCount 1).Trim()
if ($version -cnotmatch '^[0-9]+\.[0-9]+\.[0-9]+(\+[0-9a-f]{7})?$') { Exit-Install "VERSION says '$version', which is not a version of a release" }

# The install folder is new, empty or an earlier install: files of somebody else's are neither
# replaced nor swept away.
if ((Test-Path -LiteralPath $InstallDir -PathType Container) -and -not (Test-Installation $InstallDir) -and
    (Get-ChildItem -LiteralPath $InstallDir -Force | Select-Object -First 1)) {
    Exit-Install "$InstallDir holds files but no takt install: nothing was installed; use an empty folder (-InstallDir) or none"
}

$files = @($payloadFiles | Where-Object { $_ -cne 'install.ps1' })

Write-Output "==> copying takt $version to $InstallDir"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
# All files go to .new first and are renamed only when every copy succeeded.
try {
    foreach ($name in $files) {
        Copy-Item -LiteralPath (Join-Path $PayloadDir $name) -Destination (Join-Path $InstallDir "$name.new") -Force
    }
}
catch {
    foreach ($name in $files) {
        Remove-Item -LiteralPath (Join-Path $InstallDir "$name.new") -Force -ErrorAction SilentlyContinue
    }
    Exit-Install "copying the files failed, the installed ones are unchanged: $($_.Exception.Message)"
}
# Each old file moves aside to .old and its .new takes its place (Windows lets a running program be
# renamed). A rename that fails puts every file swapped so far back.
$swapped = New-Object System.Collections.Generic.List[object]
try {
    foreach ($name in $files) {
        $dest = Join-Path $InstallDir $name
        $hadOld = Test-Path -LiteralPath $dest
        if ($hadOld) { Move-Item -LiteralPath $dest -Destination "$dest.old" -Force }
        $step = [pscustomobject]@{ Dest = $dest; HadOld = $hadOld; Placed = $false }
        $swapped.Add($step)
        Move-Item -LiteralPath "$dest.new" -Destination $dest -Force
        $step.Placed = $true
    }
}
catch {
    $why = $_.Exception.Message
    for ($i = $swapped.Count - 1; $i -ge 0; $i--) {
        $s = $swapped[$i]
        if ($s.Placed) { Remove-Item -LiteralPath $s.Dest -Force -ErrorAction SilentlyContinue }
        if ($s.HadOld) { Move-Item -LiteralPath "$($s.Dest).old" -Destination $s.Dest -Force -ErrorAction SilentlyContinue }
    }
    foreach ($name in $files) {
        Remove-Item -LiteralPath (Join-Path $InstallDir "$name.new") -Force -ErrorAction SilentlyContinue
    }
    Exit-Install "replacing the files failed, the installed ones were put back: $why"
}
# An .old still held by a running takt stays; the next install or uninstall.ps1 takes it away.
foreach ($name in $files) {
    Remove-Item -LiteralPath (Join-Path $InstallDir "$name.old") -Force -ErrorAction SilentlyContinue
}
$app = Join-Path $InstallDir "$program.exe"

function New-Shortcut([string] $Dir) {
    New-Item -ItemType Directory -Force -Path $Dir | Out-Null
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut((Join-Path $Dir 'Takt.lnk'))
    $link.TargetPath = $app
    $link.WorkingDirectory = $InstallDir
    # The icon compiled into the program (assets/takt.res).
    $link.IconLocation = "$app,0"
    $link.Description = 'Takt: media player'
    $link.Save()
}

Write-Output "==> Start menu entry in $StartMenuDir"
New-Shortcut $StartMenuDir
if ($DesktopShortcut) {
    Write-Output "==> desktop shortcut in $DesktopDir"
    New-Shortcut $DesktopDir
}

# The entry of Settings > Apps runs the copy of uninstall.ps1 with the places of this install.
$uninstallKey = "$RegistryRoot\Microsoft\Windows\CurrentVersion\Uninstall\takt"
Write-Output "==> Settings > Apps entry ($uninstallKey)"
$ps = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
# -PauseOnError: Settings opens a console window that would close before an error could be read.
$command = "`"$ps`" -NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $InstallDir 'uninstall.ps1')`"" +
" -InstallDir `"$InstallDir`" -StartMenuDir `"$StartMenuDir`" -DesktopDir `"$DesktopDir`" -RegistryRoot `"$RegistryRoot`" -PauseOnError"
$size = [int](((Get-ChildItem -LiteralPath $InstallDir -File | Measure-Object -Property Length -Sum).Sum) / 1KB)
if (Test-Path -LiteralPath $uninstallKey) { Remove-Item -LiteralPath $uninstallKey -Recurse -Force }
New-Item -Path $uninstallKey -Force | Out-Null
$values = [ordered]@{
    DisplayName     = 'Takt'
    DisplayVersion  = [string]$version
    Publisher       = 'WKDSteve'
    InstallLocation = $InstallDir
    DisplayIcon     = "$app,0"
    UninstallString = $command
}
foreach ($k in $values.Keys) {
    New-ItemProperty -LiteralPath $uninstallKey -Name $k -Value $values[$k] -PropertyType String -Force | Out-Null
}
foreach ($k in 'NoModify', 'NoRepair') {
    New-ItemProperty -LiteralPath $uninstallKey -Name $k -Value 1 -PropertyType DWord -Force | Out-Null
}
New-ItemProperty -LiteralPath $uninstallKey -Name 'EstimatedSize' -Value $size -PropertyType DWord -Force | Out-Null

Write-Output ''
Write-Output "installed takt $version to $InstallDir"
