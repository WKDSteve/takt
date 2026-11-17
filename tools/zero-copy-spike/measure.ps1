# SPDX-License-Identifier: GPL-3.0-or-later
# Measures the spike against mpv: the CPU time of each process (kernel plus user) over one pass of a clip,
# taken from outside in the same way for both, plus the GPU load nvidia-smi reports while it runs.
#
#   pwsh measure.ps1 -Clips av1-4k60.mkv -Out av1.csv [-Repeat 2]
#
# One clip per call when started with `pwsh -File` from another shell (a comma list arrives as one string).
#
# Needs: the release build of the spike, the FFmpeg DLLs of TAKT_FFMPEG_DIR, mpv and nvidia-smi in PATH.
param(
    [Parameter(Mandatory)] [string[]] $Clips,
    [Parameter(Mandatory)] [string] $Out,
    [int] $Repeat = 2
)
$ErrorActionPreference = 'Stop'
$spike = Join-Path $PSScriptRoot 'target\release\zero-copy-spike.exe'
$env:PATH = "$env:TAKT_FFMPEG_DIR\bin;$env:PATH"
# mpv.exe itself: `mpv` resolves to mpv.com, a console wrapper whose child would go unmeasured.
$mpvExe = (Get-Command mpv.exe).Source
$mpv = @('--no-config', '--no-audio', '--geometry=1280x720', '--really-quiet')

function Measure-Run([string] $exe, [string[]] $arguments) {
    $smi = Start-Process nvidia-smi -PassThru -WindowStyle Hidden -RedirectStandardOutput "$env:TEMP\spike-smi.csv" `
        -ArgumentList '--query-gpu=utilization.gpu,utilization.decoder', '--format=csv,noheader,nounits', '-lms', '500'
    $p = Start-Process $exe -ArgumentList $arguments -PassThru -WindowStyle Minimized `
        -RedirectStandardOutput "$env:TEMP\spike-out.txt" -RedirectStandardError "$env:TEMP\spike-err.txt"
    $null = $p.Handle # keeps the times readable after the exit
    if (-not $p.WaitForExit(120000)) { $p.Kill() } # a hung run must not hang the series
    Stop-Process -Id $smi.Id -Force
    $wall = ($p.ExitTime - $p.StartTime).TotalSeconds
    $rows = @(Get-Content "$env:TEMP\spike-smi.csv" | Where-Object { $_ -match '^\d' } | ForEach-Object { , ($_ -split ',\s*') })
    [pscustomobject]@{
        exit    = $p.ExitCode
        wall_s  = [math]::Round($wall, 2)
        cpu_s   = [math]::Round($p.TotalProcessorTime.TotalSeconds, 2)
        cpu_pct = [math]::Round($p.TotalProcessorTime.TotalSeconds / $wall * 100, 1)
        gpu_pct = if ($rows) { [math]::Round(($rows | ForEach-Object { [int]$_[0] } | Measure-Object -Average).Average, 0) }
        dec_pct = if ($rows) { [math]::Round(($rows | ForEach-Object { [int]$_[1] } | Measure-Object -Average).Average, 0) }
        note    = (Get-Content "$env:TEMP\spike-out.txt" -Tail 1)
    }
}

$runs = @(
    @{ name = 'spike zero-copy strict, headless'; exe = $spike; args = @('--mode', 'zero-copy', '--sync', 'strict', '--fps', '60') }
    @{ name = 'spike d3d11va shared, headless'; exe = $spike; args = @('--decoder', 'd3d11va', '--mode', 'zero-copy', '--fps', '60') }
    @{ name = 'spike zero-copy loose, headless'; exe = $spike; args = @('--mode', 'zero-copy', '--sync', 'loose', '--fps', '60') }
    @{ name = 'spike cpu copy, headless'; exe = $spike; args = @('--mode', 'cpu', '--fps', '60') }
    @{ name = 'spike zero-copy strict, window'; exe = $spike; args = @('--mode', 'zero-copy', '--sync', 'strict', '--fps', '60', '--window') }
    @{ name = 'spike d3d11va shared, window'; exe = $spike; args = @('--decoder', 'd3d11va', '--mode', 'zero-copy', '--fps', '60', '--window') }
    @{ name = 'spike zero-copy loose, window'; exe = $spike; args = @('--mode', 'zero-copy', '--sync', 'loose', '--fps', '60', '--window') }
    @{ name = 'spike cpu copy, window'; exe = $spike; args = @('--mode', 'cpu', '--fps', '60', '--window') }
    @{ name = 'mpv --hwdec=auto, window'; exe = $mpvExe; args = $mpv + '--hwdec=auto' }
    @{ name = 'mpv --hwdec=auto-copy, window'; exe = $mpvExe; args = $mpv + '--hwdec=auto-copy' }
)
$results = foreach ($clip in $Clips) {
    if (-not (Test-Path $clip)) { throw "no such clip: $clip" }
    foreach ($run in $runs) {
        foreach ($i in 1..$Repeat) {
            $r = Measure-Run $run.exe (@($clip) + $run.args)
            $r | Select-Object @{ n = 'clip'; e = { Split-Path $clip -Leaf } }, @{ n = 'run'; e = { $run.name } }, @{ n = 'pass'; e = { $i } }, *
        }
    }
}
$results | Export-Csv -NoTypeInformation -Encoding utf8 $Out
$results | Format-Table clip, run, pass, exit, wall_s, cpu_s, cpu_pct, gpu_pct, dec_pct -AutoSize
