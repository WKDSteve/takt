#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Makes the test clip for the A/V sync measurement by hand: a flash and a click once a second.

    python scripts/make-sync-clip.py [OUT.mkv] [--seconds 60] [--fps 60] [--size 1920x1080]

The picture is black and turns white for the first two frames of every second; the sound is silent and plays a
1 kHz tone for the first 10 ms of every second, on both channels. Flash and click start on the same timestamp, so
the distance a camera (240 fps slow motion) or a microphone with a photodiode sees between them is the A/V offset of
the player and its devices. How to measure: docs/reviews/phase-0.md, section "A/V-Sync von Hand".

Needs ffmpeg in PATH (lavfi, libx264, aac). The clip is not kept in the repository; the default is
`takt-sync-clip.mkv` in the temp directory. Exit 1 when ffmpeg fails.
"""

import argparse
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile


def command(out, seconds, fps, size):
    """The ffmpeg call as an argument list: no shell reads the filter expressions."""
    # Commas inside an expression are escaped for the filter graph; a list of arguments needs no shell quoting on top.
    flash = f"color=c=black:s={size}:r={fps},drawbox=c=white:t=fill:enable='lt(mod(n\\,{fps})\\,2)'"
    click = "aevalsrc='if(lt(mod(t\\,1)\\,0.01)\\,0.8*sin(2*PI*1000*t)\\,0)':s=48000:c=stereo"
    return [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        flash,
        "-f",
        "lavfi",
        "-i",
        click,
        "-t",
        str(seconds),
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-pix_fmt",
        "yuv420p",
        # No B-frames and a keyframe every second: a seek lands on a flash.
        "-bf",
        "0",
        "-g",
        str(fps),
        "-c:a",
        "aac",
        "-b:a",
        "192k",
        str(out),
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("out", nargs="?", default=str(pathlib.Path(tempfile.gettempdir()) / "takt-sync-clip.mkv"))
    parser.add_argument("--seconds", type=int, default=60)
    parser.add_argument("--fps", type=int, default=60)
    parser.add_argument("--size", default="1920x1080")
    args = parser.parse_args()
    if not re.fullmatch(r"\d+x\d+", args.size) or args.seconds < 1 or args.fps < 2:
        parser.error("--size WxH, --seconds from 1, --fps from 2")
    if shutil.which("ffmpeg") is None:
        parser.error("ffmpeg not found in PATH")
    done = subprocess.run(command(args.out, args.seconds, args.fps, args.size), check=False)
    if done.returncode != 0:
        print(f"make-sync-clip: ffmpeg failed with exit {done.returncode}", file=sys.stderr)
        return 1
    print(args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
