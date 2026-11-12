# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Remove takt for the current user on Windows, without administrator rights.

.DESCRIPTION
Removes what scripts\install.ps1 (and so scripts\build-install.ps1) installed: the files of the
release in the install folder (%LOCALAPPDATA%\takt\bin, or $env:TAKT_INSTALL_DIR), the Start menu
entry and the desktop shortcut "Takt" when they point at that folder, and the entry in Settings >
Apps when it names that folder. A second run finds nothing to do and says so; it is not an error. A
program that still runs cannot be removed: that is an error, not a "removed".

Settings > Apps runs the copy in the install folder with the places of that install.

.PARAMETER InstallDir
The install folder (default $env:TAKT_INSTALL_DIR, else %LOCALAPPDATA%\takt\bin).

.PARAMETER StartMenuDir
The folder of the Start menu entry (default $env:TAKT_START_MENU_DIR, else the Start menu programs
folder of the current user).

.PARAMETER DesktopDir
The folder of the desktop shortcut (default $env:TAKT_DESKTOP_DIR, else the desktop).

.PARAMETER RegistryRoot
The key that stands for HKCU\Software (default $env:TAKT_REGISTRY_ROOT, else HKCU:\Software).

.PARAMETER AllowElevated
Remove although the session is elevated (for a build machine).

.PARAMETER PauseOnError
Wait for a key after an error (Settings > Apps opens a window that would close at once).

.EXAMPLE
.\scripts\uninstall.ps1
#>
[CmdletBinding()]
param(
    [string] $InstallDir = '',
    [string] $StartMenuDir = '',
    [string] $DesktopDir = '',
    [string] $RegistryRoot = '',
    [switch] $AllowElevated,
    [switch] $PauseOnError
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$files = @('takt-player.exe', 'uninstall.ps1', 'LICENSE', 'README.md', 'VERSION')

function Get-Setting([string] $Given, [string] $EnvName, [string] $Default) {
    if ($Given) { return $Given }
    $fromEnv = [Environment]::GetEnvironmentVariable($EnvName)
    if ($fromEnv) { return $fromEnv }
    return $Default
}

function Remove-Takt {
    if ($env:CLAUDECODE) {
        throw 'remove in a normal terminal, not in a Claude session (the app redirects AppData and the registry)'
    }
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $AllowElevated -and $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'do not run this elevated: it removes the installation of one user (-AllowElevated for a build machine)'
    }

    $dir = [IO.Path]::GetFullPath((Get-Setting $InstallDir 'TAKT_INSTALL_DIR' (Join-Path $env:LOCALAPPDATA 'takt\bin'))).TrimEnd('\')
    $startMenu = Get-Setting $StartMenuDir 'TAKT_START_MENU_DIR' ([Environment]::GetFolderPath('Programs'))
    $desktop = Get-Setting $DesktopDir 'TAKT_DESKTOP_DIR' ([Environment]::GetFolderPath('Desktop'))
    $root = (Get-Setting $RegistryRoot 'TAKT_REGISTRY_ROOT' 'HKCU:\Software').TrimEnd('\')
    if ($root -notmatch '^(HKCU:|Registry::HKEY_CURRENT_USER)\\.') { throw "not a key below HKCU: '$root'" }
    $app = Join-Path $dir 'takt-player.exe'
    $removed = 0

    # The links only when they start this install: one of another install stays.
    $shell = New-Object -ComObject WScript.Shell
    foreach ($linkDir in $startMenu, $desktop) {
        $path = Join-Path $linkDir 'Takt.lnk'
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { continue }
        $target = $shell.CreateShortcut($path).TargetPath
        if ($target -and $target -ieq $app) {
            Remove-Item -LiteralPath $path -Force
            Write-Output "removed $path"
            $removed++
        }
        else {
            Write-Output "left $path alone: it starts $target"
        }
    }

    $key = "$root\Microsoft\Windows\CurrentVersion\Uninstall\takt"
    $entry = Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue
    if ($entry) {
        $location = ''
        if ($entry.PSObject.Properties['InstallLocation']) { $location = ([string]$entry.InstallLocation).TrimEnd('\') }
        if ($location -ieq $dir) {
            Remove-Item -LiteralPath $key -Recurse -Force
            Write-Output "removed the Settings > Apps entry ($key)"
            $removed++
        }
        else {
            Write-Output "left the Settings > Apps entry alone: it names $location"
        }
    }

    # Only the files the installer puts there; a folder that holds anything else stays.
    $present = @(foreach ($name in $files) {
            foreach ($suffix in '', '.new', '.old') {
                $file = Join-Path $dir "$name$suffix"
                if (Test-Path -LiteralPath $file -PathType Leaf) { $file }
            }
        })
    foreach ($file in $present) {
        try { Remove-Item -LiteralPath $file -Force }
        catch { throw "cannot remove ${file} (is takt-player running? close it and run again): $($_.Exception.Message)" }
        if (Test-Path -LiteralPath $file) { throw "$file is still there" }
        Write-Output "removed $file"
        $removed++
    }
    if ((Test-Path -LiteralPath $dir -PathType Container) -and -not (Get-ChildItem -LiteralPath $dir -Force)) {
        Remove-Item -LiteralPath $dir -Force -ErrorAction SilentlyContinue
        # The parent only when it is the folder the installer makes (never a folder you chose).
        $parent = Split-Path -Parent $dir
        if ((Split-Path -Leaf $dir) -eq 'bin' -and (Split-Path -Leaf $parent) -eq 'takt' -and -not (Get-ChildItem -LiteralPath $parent -Force)) {
            Remove-Item -LiteralPath $parent -Force -ErrorAction SilentlyContinue
        }
    }
    if ($removed -eq 0) { Write-Output "nothing to remove: no takt install in $dir" }
}

try {
    Remove-Takt
}
catch {
    Write-Error "uninstall.ps1: $($_.Exception.Message)" -ErrorAction Continue
    if ($PauseOnError) { Read-Host 'press Enter to close' | Out-Null }
    exit 1
}
