<#
.SYNOPSIS
Remove takt-player for the current user on Windows, without administrator rights.

.DESCRIPTION
Removes what scripts/build-install.ps1 installed: takt-player.exe in the install folder
(%LOCALAPPDATA%\takt\bin, or $env:TAKT_INSTALL_DIR). A second run finds nothing to do and says so;
it is not an error. A program that still runs cannot be removed: that is an error, not a "removed".

.PARAMETER AllowElevated
Remove although the session is elevated (for a build machine).

.EXAMPLE
.\scripts\uninstall.ps1
#>
[CmdletBinding()]
param(
    [switch] $AllowElevated
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Exit-Uninstall([string] $Message) {
    throw "uninstall.ps1: $Message"
}

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $AllowElevated -and $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Exit-Uninstall 'do not run this elevated: it removes the installation of one user (-AllowElevated for a build machine)'
}

$dir = if ($env:TAKT_INSTALL_DIR) { $env:TAKT_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'takt\bin' }
$present = @(@('takt-player.exe', 'takt-player.exe.new', 'takt-player.exe.old') |
        ForEach-Object { Join-Path $dir $_ } | Where-Object { Test-Path -LiteralPath $_ })
if ($present.Count -eq 0) {
    Write-Output "nothing to remove: no takt-player.exe in $dir"
    return
}

# Only the files the installer puts there; a folder that holds anything else stays.
foreach ($file in $present) {
    try { Remove-Item -LiteralPath $file -Force }
    catch { Exit-Uninstall "cannot remove ${file} (is takt-player running? close it and run again): $($_.Exception.Message)" }
    if (Test-Path -LiteralPath $file) { Exit-Uninstall "$file is still there" }
    Write-Output "removed $file"
}
if (-not (Get-ChildItem -LiteralPath $dir -Force)) {
    Remove-Item -LiteralPath $dir -Force -ErrorAction SilentlyContinue
    # The parent only when it is the folder the installer makes (never a folder you chose).
    $parent = Split-Path -Parent $dir
    if ((Split-Path -Leaf $dir) -eq 'bin' -and (Split-Path -Leaf $parent) -eq 'takt' -and -not (Get-ChildItem -LiteralPath $parent -Force)) {
        Remove-Item -LiteralPath $parent -Force -ErrorAction SilentlyContinue
    }
}
