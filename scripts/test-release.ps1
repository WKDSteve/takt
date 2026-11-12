# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Test package-release.ps1, the zip it makes and its install and uninstall, without a build and without
a real install.

.DESCRIPTION
Packs the zip from a stand-in (a copy of cmd.exe: nothing is built, nothing of it is started) and
checks:

- the zip holds exactly takt-player.exe, install.ps1, uninstall.ps1, LICENSE, README.md and VERSION,
  with the bytes of the sources, and VERSION says <version>+<short commit>;
- SHA256SUMS names the zip with its real SHA-256, and packing twice gives the same bytes;
- what is refused: a commit that is no hex, a missing program;
- the unpacked zip installed into temp places (a throw-away registry root, temp Start menu, desktop
  and install folder; the account's own are never touched), under PowerShell 7 and Windows PowerShell
  5.1: the files, the Start menu entry and the desktop shortcut with the icon of the program, the
  Settings > Apps entry; an update over it; a payload that is no release and a folder of somebody
  else's refused; a Claude session refused; then the installed uninstall.ps1, twice.

With -Zip the zip of a real build is checked too, against the takt-player.exe of -BuildDir (the
rolling workflow checks the zip it is about to publish this way).

Exit 0 when all checks pass.

.PARAMETER Zip
A zip made by package-release.ps1 from -BuildDir, checked as well.

.PARAMETER BuildDir
The folder with the takt-player.exe the -Zip was made from.

.PARAMETER Label
The version label the -Zip must say (X.Y.Z+<short commit>).

.EXAMPLE
.\scripts\test-release.ps1
.\scripts\test-release.ps1 -Zip dist\takt-windows-x86_64.zip -BuildDir target\release -Label 0.0.1+0123456
#>
[CmdletBinding()]
param(
    [string] $Zip = '',
    [string] $BuildDir = '',
    [string] $Label = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$packager = Join-Path $PSScriptRoot 'package-release.ps1'
$expected = @('LICENSE', 'README.md', 'VERSION', 'install.ps1', 'takt-player.exe', 'uninstall.ps1')
$script:failed = 0

function Write-Check([bool] $Passed, [string] $Name) {
    if ($Passed) { Write-Host "ok    $Name" }
    else {
        Write-Host "FAIL  $Name"
        $script:failed++
    }
}

function Get-Sha([string] $Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }

# A script in a child PowerShell; the child runs without CLAUDECODE (all its places are temp places).
function Invoke-Child([string] $Script, [string[]] $Arguments, [string] $Shell = 'pwsh') {
    $saved = $env:CLAUDECODE
    try {
        $env:CLAUDECODE = $null
        $output = & $Shell -NoProfile -ExecutionPolicy Bypass -File $Script @Arguments 2>&1 | Out-String
        return [pscustomobject]@{ Code = $LASTEXITCODE; Output = $output }
    }
    finally {
        $env:CLAUDECODE = $saved
    }
}

# The entries of a zip, sorted, and the folder it was unpacked into.
function Expand-Zip([string] $Path) {
    $dir = Join-Path $work ('unpacked-' + [Guid]::NewGuid().ToString('N'))
    Expand-Archive -LiteralPath $Path -DestinationPath $dir
    return $dir
}

function Get-Names([string] $Dir) {
    $names = @(Get-ChildItem -LiteralPath $Dir -Force | ForEach-Object { $_.Name })
    [Array]::Sort($names, [StringComparer]::Ordinal)
    return , $names
}

# The checks every zip must pass: content, VERSION, SHA256SUMS next to it.
function Test-Zip([string] $Path, [string] $Exe, [string] $WantLabel, [string] $Title) {
    $dir = Expand-Zip $Path
    Write-Check ((Get-Names $dir) -join ',' -ceq ($expected -join ',')) "${Title}: the zip holds exactly $($expected -join ', ')"
    Write-Check ((Get-Sha (Join-Path $dir 'takt-player.exe')) -eq (Get-Sha $Exe)) "${Title}: takt-player.exe has the bytes of the build"
    foreach ($name in 'install.ps1', 'uninstall.ps1') {
        Write-Check ((Get-Sha (Join-Path $dir $name)) -eq (Get-Sha (Join-Path $PSScriptRoot $name))) "${Title}: $name has the bytes of scripts\$name"
    }
    $label = [IO.File]::ReadAllText((Join-Path $dir 'VERSION'))
    Write-Check ($label -ceq "$WantLabel`n") "${Title}: VERSION says $WantLabel"
    $sums = Join-Path (Split-Path -Parent $Path) 'SHA256SUMS'
    if (Test-Path -LiteralPath $sums) {
        $line = [IO.File]::ReadAllText($sums)
        Write-Check ($line -ceq "$(Get-Sha $Path)  $(Split-Path -Leaf $Path)`n") "${Title}: SHA256SUMS names the zip with its SHA-256"
    }
    return $dir
}

$work = Join-Path ([IO.Path]::GetTempPath()) ('takt-release-test-' + [Guid]::NewGuid().ToString('N'))
$regRoot = 'HKCU:\Software\takt-release-test-' + [Guid]::NewGuid().ToString('N')
New-Item -ItemType Directory -Force -Path $work | Out-Null
try {
    $root = Split-Path -Parent $PSScriptRoot
    $cargo = Get-Content -LiteralPath (Join-Path $root 'apps\takt-player\Cargo.toml') -Raw
    $null = $cargo -match '(?m)^version\s*=\s*"([^"]+)"'
    $version = $Matches[1]

    # The stand-in program.
    $build = Join-Path $work 'build'
    New-Item -ItemType Directory -Force -Path $build | Out-Null
    $standIn = Join-Path $build 'takt-player.exe'
    Copy-Item -LiteralPath (Join-Path $env:SystemRoot 'System32\cmd.exe') -Destination $standIn
    $sha = '0123456789abcdef0123456789abcdef01234567'

    $out1 = Join-Path $work 'dist1'
    $out2 = Join-Path $work 'dist2'
    $null = & $packager -BuildDir $build -OutDir $out1 -Sha $sha
    $null = & $packager -BuildDir $build -OutDir $out2 -Sha $sha
    $zip1 = Join-Path $out1 'takt-windows-x86_64.zip'
    $payload = Test-Zip $zip1 $standIn "$version+0123456" 'stand-in'
    Write-Check ((Get-Sha $zip1) -eq (Get-Sha (Join-Path $out2 'takt-windows-x86_64.zip'))) 'packing twice gives the same bytes'

    $refused = $false
    try { $null = & $packager -BuildDir $build -OutDir (Join-Path $work 'x') -Sha 'nothex' } catch { $refused = $true }
    Write-Check $refused 'a commit that is no hex is refused'
    $refused = $false
    try { $null = & $packager -BuildDir (Join-Path $work 'empty') -OutDir (Join-Path $work 'y') -Sha $sha } catch { $refused = $true }
    Write-Check $refused 'a missing program is refused'

    foreach ($shell in 'pwsh', 'powershell') {
        $install = Join-Path $work "$shell-install\takt\bin"
        $startMenu = Join-Path $work "$shell-startmenu"
        $desktop = Join-Path $work "$shell-desktop"
        # The hosted runners run elevated; the places are temp places, so that is allowed here.
        $places = @('-InstallDir', $install, '-StartMenuDir', $startMenu, '-DesktopDir', $desktop, '-RegistryRoot', $regRoot, '-AllowElevated')
        $r = Invoke-Child (Join-Path $payload 'install.ps1') ($places + '-DesktopShortcut') $shell
        Write-Check ($r.Code -eq 0) "${shell}: install exits 0"
        if ($r.Code -ne 0) { Write-Output $r.Output }
        $installed = Get-Names $install
        Write-Check (($installed -join ',') -ceq 'LICENSE,README.md,VERSION,takt-player.exe,uninstall.ps1') "${shell}: the install folder holds the release without install.ps1"
        $app = Join-Path ([IO.Path]::GetFullPath($install)) 'takt-player.exe'
        $wsh = New-Object -ComObject WScript.Shell
        foreach ($linkDir in $startMenu, $desktop) {
            $lnk = Join-Path $linkDir 'Takt.lnk'
            $ok = (Test-Path -LiteralPath $lnk) -and ($wsh.CreateShortcut($lnk).TargetPath -ieq $app) -and ($wsh.CreateShortcut($lnk).IconLocation -ieq "$app,0")
            Write-Check $ok "${shell}: $(Split-Path -Leaf $linkDir) has Takt.lnk with the program and its icon"
        }
        $key = "$regRoot\Microsoft\Windows\CurrentVersion\Uninstall\takt"
        $entry = Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue
        Write-Check ($null -ne $entry -and $entry.DisplayVersion -ceq "$version+0123456" -and $entry.UninstallString -like '*uninstall.ps1*-PauseOnError') "${shell}: the Settings > Apps entry names the version and the uninstall"

        $r = Invoke-Child (Join-Path $payload 'install.ps1') $places $shell
        Write-Check ($r.Code -eq 0 -and -not (Get-ChildItem -LiteralPath $install -Filter '*.old')) "${shell}: an update over the install leaves no .old"

        $r = Invoke-Child (Join-Path $install 'uninstall.ps1') $places $shell
        Write-Check ($r.Code -eq 0 -and -not (Test-Path -LiteralPath $install) -and -not (Test-Path -LiteralPath (Join-Path $startMenu 'Takt.lnk')) -and
            -not (Test-Path -LiteralPath (Join-Path $desktop 'Takt.lnk')) -and -not (Test-Path -LiteralPath $key)) "${shell}: uninstall removes files, links and the entry"
        $r = Invoke-Child (Join-Path $PSScriptRoot 'uninstall.ps1') $places $shell
        Write-Check ($r.Code -eq 0 -and $r.Output -match 'nothing to remove') "${shell}: a second uninstall finds nothing and says so"
    }

    $startMenu = Join-Path $work 'refuse-startmenu'
    $foreign = Join-Path $work 'foreign'
    New-Item -ItemType Directory -Force -Path $foreign | Out-Null
    Set-Content -LiteralPath (Join-Path $foreign 'notes.txt') -Value 'mine'
    $r = Invoke-Child (Join-Path $payload 'install.ps1') @('-InstallDir', $foreign, '-StartMenuDir', $startMenu, '-RegistryRoot', $regRoot, '-AllowElevated')
    Write-Check ($r.Code -ne 0 -and $r.Output -match 'no takt install' -and (Get-Names $foreign).Count -eq 1) 'a folder of somebody else is refused and left alone'
    $extra = Join-Path $work 'extra'
    Copy-Item -LiteralPath $payload -Destination $extra -Recurse
    Set-Content -LiteralPath (Join-Path $extra 'evil.dll') -Value 'x'
    $r = Invoke-Child (Join-Path $extra 'install.ps1') @('-InstallDir', (Join-Path $work 'extra-install'), '-StartMenuDir', $startMenu, '-RegistryRoot', $regRoot, '-AllowElevated')
    Write-Check ($r.Code -ne 0 -and $r.Output -match 'evil.dll' -and -not (Test-Path -LiteralPath (Join-Path $work 'extra-install'))) 'a payload with a file that is no release is refused'
    $r = Invoke-Child (Join-Path $payload 'install.ps1') @('-InstallDir', (Join-Path $work 'hklm'), '-StartMenuDir', $startMenu, '-RegistryRoot', 'HKLM:\Software\x', '-AllowElevated')
    Write-Check ($r.Code -ne 0 -and $r.Output -match 'below HKCU') 'a registry root outside HKCU is refused'
    $savedClaude = $env:CLAUDECODE
    $env:CLAUDECODE = '1'
    $refused = $false
    try { $null = & (Join-Path $payload 'install.ps1') -InstallDir (Join-Path $work 'claude') -StartMenuDir $startMenu -RegistryRoot $regRoot }
    catch { $refused = $_.Exception.Message -match 'Claude session' }
    finally { $env:CLAUDECODE = $savedClaude }
    Write-Check $refused 'a Claude session is refused'

    if ($Zip) {
        if (-not $BuildDir) { throw 'test-release.ps1: -Zip needs -BuildDir' }
        $want = if ($Label) { $Label } else { ([IO.File]::ReadAllText((Join-Path (Expand-Zip $Zip) 'VERSION'))).Trim() }
        $null = Test-Zip (Resolve-Path -LiteralPath $Zip).ProviderPath (Join-Path $BuildDir 'takt-player.exe') $want 'the build'
    }
}
finally {
    Remove-Item -LiteralPath $regRoot -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}

if ($script:failed) {
    Write-Output "$($script:failed) check(s) failed"
    exit 1
}
Write-Output 'all checks passed'
