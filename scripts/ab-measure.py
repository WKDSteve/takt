#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""The A/B protocol of docs/reviews/phase-0.md: presentation errors per path, as a Markdown table.

Runs the same test source (a moving pattern, 1920x1080) at each frame rate, one run after the other, and prints one
table row per run:

    cargo build --release -p takt-player
    python scripts/ab-measure.py                      # NullBackend, headless: 23.976, 25, 50, 60 fps on 60 Hz, 5 min each
    python scripts/ab-measure.py --hz 60 120          # the same on two simulated displays
    python scripts/ab-measure.py --mpv                # mpv itself, display-resample and audio: needs a real screen

Paths:

  null   `takt-player --headless`: the scheduler of takt on NullBackend against the audio master clock. The display is
         simulated (one pass per period, no vsync), so the numbers say what the scheduler decides, not what a screen
         shows. `--device NAME` plays the silence of the run on that output device (TAKT_AUDIO_DEVICE); without a
         device the clock is simulated and the row says so.
  mpv    `mpv --video-sync=<mode>` on the same pattern from lavfi, with scripts/ab-mpv-stats.lua. mpv opens a window
         and measures against the real refresh of the screen it is on; `--ao=null` keeps it silent. mpv has no
         histogram of presentation errors: its row holds the vsync jitter and its own counters instead.

The rows go to stdout, the raw `takt-ab` lines to stderr. Exit 1 when a run fails or prints no result line.
"""

import argparse
import datetime
import os
import pathlib
import platform
import re
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
RATES = ["24000/1001", "25", "50", "60"]
MPV_MODES = ["display-resample", "audio"]
HEADER = (
    "| Datum | Hardware | Quelle | Pfad | Anzeige | Mittel | p99 | wiederholt/min | erwartet/min | verworfen/min |\n"
    "|---|---|---|---|---|---|---|---|---|---|"
)


def fraction(text):
    """A rate `N` or `N/D` as a float; only for the expected count and the labels, never fed back."""
    num, _, den = text.partition("/")
    return int(num) / int(den or "1")


def result_line(output):
    """The key=value pairs of the last `takt-ab` line of a run, None without one."""
    # mpv puts the name of the script in front of the line.
    lines = [line[line.index("takt-ab ") :] for line in output.splitlines() if "takt-ab " in line]
    if not lines:
        return None
    return dict(pair.split("=", 1) for pair in lines[-1].split()[1:] if "=" in pair)


def per_minute(count, seconds):
    return f"{float(count) * 60 / float(seconds):.1f}".replace(".", ",")


def millis(micros):
    return f"{float(micros) / 1000:.2f} ms".replace(".", ",")


def null_row(result, date, hardware):
    """One table row of a headless run of takt-player."""
    seconds = float(result["elapsed_ms"]) / 1000
    fps, hz = fraction(result["fps"]), fraction(result["hz"])
    # A pass without a new frame is no fault when the display is faster than the source: (hz - fps) per second.
    expected = max(hz - fps, 0) * 60
    clock = "Gerät" if result["audio"] == "device" else "Uhr simuliert"
    p99 = "–" if result["p99_us"] == "-" else "< " + millis(result["p99_us"])
    return (
        f"| {date} | {hardware} | Testbild {fps:.3f} fps, {result['seconds']} s | NullBackend, kopflos ({clock}) "
        f"| {hz:g} Hz simuliert | {millis(result['mean_us'])} | {p99} "
        f"| {per_minute(result['repeated'], seconds)} | {expected:.1f} | {per_minute(result['dropped'], seconds)} |"
    ).replace(".", ",")


def mpv_row(result, date, hardware):
    """One table row of an mpv run: jitter and counters in place of the error columns."""

    def number(key):
        return None if result.get(key, "-") == "-" else float(result[key])

    seconds = number("seconds") or 0
    if seconds <= 0:
        return None
    fps, hz, jitter = number("fps"), number("hz"), number("jitter")

    def count(key):
        # mpv counts mistimed frames only with a display-* mode.
        return "–" if number(key) is None else per_minute(number(key), seconds)

    source = "Testbild" if fps is None else f"Testbild {fps:.3f} fps"
    return (
        f"| {date} | {hardware} | {source}, {seconds:.0f} s | mpv `--video-sync={result.get('mode', '?')}` "
        f"| {'?' if hz is None else f'{hz:.3f}'} Hz | vsync-jitter {'–' if jitter is None else f'{jitter:.4f}'} | – "
        f"| verspätet {count('delayed')}, falsch getaktet {count('mistimed')} | – | {count('dropped')} |"
    ).replace(".", ",")


def run(command, env=None):
    """Runs one measurement; (result, None) or (None, why)."""
    print("+ " + " ".join(str(part) for part in command), file=sys.stderr)
    done = subprocess.run(command, capture_output=True, text=True, encoding="utf-8", errors="replace", env=env)
    result = result_line(done.stdout)
    if done.returncode != 0 or result is None:
        return None, f"exit {done.returncode}, no takt-ab line\n{done.stdout[-400:]}{done.stderr[-400:]}"
    print("takt-ab " + " ".join(f"{key}={value}" for key, value in result.items()), file=sys.stderr)
    return result, None


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--seconds", type=int, default=300, help="length of each run (default 300)")
    parser.add_argument("--fps", nargs="+", default=RATES, help="frame rates as N or N/D")
    parser.add_argument("--hz", nargs="+", default=["60"], help="refresh rates of the simulated display (null path)")
    parser.add_argument("--device", help="output device of the null path, a part of its name")
    parser.add_argument("--hardware", default=platform.node(), help="text of the hardware column")
    parser.add_argument("--player", default=str(ROOT / "target" / "release" / "takt-player"), help="takt-player binary")
    parser.add_argument("--mpv", action="store_true", help="measure mpv itself instead (opens a window)")
    parser.add_argument("--mpv-path", default="mpv", help="mpv binary (default: from PATH)")
    parser.add_argument("--no-header", action="store_true", help="rows only")
    args = parser.parse_args()
    if not all(re.fullmatch(r"\d+(/\d+)?", rate) for rate in args.fps + args.hz):
        parser.error("--fps and --hz take rates as N or N/D")

    # The rows hold umlauts; a Windows console or pipe would take its own code page.
    sys.stdout.reconfigure(encoding="utf-8")
    date = datetime.date.today().isoformat()
    failed = False
    if not args.no_header:
        print(HEADER, flush=True)
    if args.mpv:
        if shutil.which(args.mpv_path) is None:
            parser.error(f"{args.mpv_path} not found")
        script = ROOT / "scripts" / "ab-mpv-stats.lua"
        for fps in args.fps:
            for mode in MPV_MODES:
                result, why = run(
                    [
                        args.mpv_path,
                        "--no-config",
                        # Only errors and the line of the script; no status line.
                        "--msg-level=all=error,ab_mpv_stats=info",
                        "--term-status-msg=",
                        f"--script={script}",
                        f"--video-sync={mode}",
                        "--ao=null",
                        "--hwdec=no",
                        "--fullscreen",
                        f"--length={args.seconds}",
                        # The pattern with a silent sound track: display-resample steers the audio speed.
                        f"av://lavfi:testsrc2=size=1920x1080:rate={fps}[out0];anullsrc=r=48000:cl=stereo[out1]",
                    ]
                )
                row = result and mpv_row(result, date, args.hardware)
                if row is None:
                    print(f"ab-measure: mpv {fps} {mode}: {why or 'no playback time'}", file=sys.stderr)
                    failed = True
                    continue
                print(row, flush=True)
    else:
        env = dict(os.environ)
        if args.device:
            env["TAKT_AUDIO_DEVICE"] = args.device
        for hz in args.hz:
            for fps in args.fps:
                result, why = run(
                    [args.player, "--headless", "--fps", fps, "--hz", hz, "--seconds", str(args.seconds)], env
                )
                if result is None:
                    print(f"ab-measure: null {fps} at {hz} Hz: {why}", file=sys.stderr)
                    failed = True
                    continue
                print(null_row(result, date, args.hardware), flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
