# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
Runs composite-bench of takt-gpu once and prints one line: the load of the machine and of the GPU during the run,
and the line of numbers the program prints (docs/GPU.md, "Messen").

.DESCRIPTION
The program is the release build of the example:

    cargo build --release -p takt-gpu --example composite-bench

It makes its own pictures before the clock starts; no file is needed. This script starts it, sets the priority
class of the process, samples `nvidia-smi` (when there is one) and the CPU load of the machine every 400 ms, and
prints their means in front of the last line of the program.

A measurement means something only on a quiet machine: -RequireQuiet refuses to start while the CPU load is above
10 % (ten samples, a second apart).

.EXAMPLE
pwsh -NoProfile -File scripts/measure-composite.ps1 -Label headless-1 -Priority High -Layers 4 -Seconds 300 -RequireQuiet

.EXAMPLE
pwsh -NoProfile -File scripts/measure-composite.ps1 -Label window-1 -Priority High -Layers 4 -Seconds 300 -Window -RequireQuiet
#>
param(
    [string] $Label = 'run',
    [ValidateSet('Normal', 'AboveNormal', 'High')]
    [string] $Priority = 'High',
    [int] $Layers = 4,
    [int] $Seconds = 300,
    [ValidateSet('imported', 'upload')]
    [string] $Source = 'imported',
    [switch] $Window,
    [switch] $RequireQuiet
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$bench = Join-Path $root 'target\release\examples\composite-bench.exe'
if (-not (Test-Path -LiteralPath $bench)) {
    [Console]::Error.WriteLine("measure-composite: no $bench (cargo build --release -p takt-gpu --example composite-bench)")
    exit 2
}

function Get-MachineLoad {
    [int] (Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average
}

if ($RequireQuiet) {
    $samples = 1..10 | ForEach-Object { Get-MachineLoad; Start-Sleep -Seconds 1 }
    $worst = ($samples | Measure-Object -Maximum).Maximum
    if ($worst -gt 10) {
        [Console]::Error.WriteLine("measure-composite: the machine is not quiet, CPU load up to $worst % in ten seconds ($($samples -join ' ')); nothing was measured")
        exit 3
    }
}

$benchArgs = @('--layers', "$Layers", '--seconds', "$Seconds", '--source', $Source)
if ($Window) { $benchArgs += '--window' }

$out = [System.IO.Path]::GetTempFileName()
$err = [System.IO.Path]::GetTempFileName()
$process = Start-Process -FilePath $bench -ArgumentList $benchArgs -NoNewWindow -PassThru `
    -RedirectStandardOutput $out -RedirectStandardError $err
try { $process.PriorityClass = $Priority } catch { Write-Warning "priority not set: $_" }

$smi = Get-Command nvidia-smi -ErrorAction SilentlyContinue
$gpu = @()
$cpu = @()
while (-not $process.HasExited) {
    if ($smi) {
        $value = (& $smi.Source --query-gpu=utilization.gpu --format=csv,noheader,nounits) 2>$null
        if ($value -match '^\s*\d+') { $gpu += [int] $value }
    }
    $cpu += Get-MachineLoad
    Start-Sleep -Milliseconds 400
}
$process.WaitForExit()

$line = Get-Content -LiteralPath $out | Select-Object -Last 1
$failed = -not $line
if ($failed) { $line = 'FAILED: ' + (Get-Content -LiteralPath $err | Select-Object -Last 1) }
$gpuMean = if ($gpu.Count) { [math]::Round(($gpu | Measure-Object -Average).Average) } else { '?' }
$cpuMean = if ($cpu.Count) { [math]::Round(($cpu | Measure-Object -Average).Average) } else { '?' }
$cpuMax = if ($cpu.Count) { ($cpu | Measure-Object -Maximum).Maximum } else { '?' }
Remove-Item -LiteralPath $out, $err -Force -ErrorAction SilentlyContinue
"$Label | priority=$Priority | machine_cpu=$cpuMean% (max $cpuMax%) | gpu_load=$gpuMean% | $line"
if ($failed) { exit 1 }
