#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Makes the long-GOP test file of the MpvBackend tests: apps/takt-player/tests/data/long-gop-12s.mkv.

    python scripts/make-long-gop-clip.py [OUT.mkv]

Twelve seconds of a flat grey picture (64x64, 25 fps, H.264, no B-frames, no sound) with a keyframe every four
seconds and nowhere else: at 0 s, 4 s and 8 s. A jump to a keyframe therefore lands up to four seconds away from its
target, which is what the test measures; the 3-s file has a keyframe every second and cannot show that.

The grey of a frame is its number within its second: frame N has the value 16 + (N mod 25) * 8 in red, green and
blue. So a test can read from the picture which frame it holds and compare that with the time the frame carries.

The file is kept in the repository, so the tests need no ffmpeg: this script is how it was made and how it is made
again. Needs ffmpeg in PATH (lavfi, libx264). Exit 1 when ffmpeg fails.
"""

import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DEFAULT = ROOT / "apps" / "takt-player" / "tests" / "data" / "long-gop-12s.mkv"
SECONDS = 12
FPS = 25
GOP_SECONDS = 4


def command(out):
    """The ffmpeg call as an argument list: no shell reads the filter expressions."""
    gop = str(FPS * GOP_SECONDS)
    return [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        # `gray` is full range: the value written is the value mpv hands out as R, G and B.
        f"nullsrc=size=64x64:rate={FPS},format=gray,geq=lum='16+mod(N,{FPS})*8',format=yuv420p",
        "-t",
        str(SECONDS),
        "-c:v",
        "libx264",
        "-preset",
        "veryslow",
        "-crf",
        "4",
        "-pix_fmt",
        "yuv420p",
        "-bf",
        "0",
        "-g",
        gop,
        "-keyint_min",
        gop,
        "-sc_threshold",
        "0",
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        # No encoder version and no date in the file: two runs give the same bytes with the same ffmpeg.
        "-fflags",
        "+bitexact",
        "-flags:v",
        "+bitexact",
        str(out),
    ]


def main():
    out = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT
    if shutil.which("ffmpeg") is None:
        print("make-long-gop-clip: ffmpeg not found in PATH", file=sys.stderr)
        return 1
    out.parent.mkdir(parents=True, exist_ok=True)
    done = subprocess.run(command(out), check=False)
    if done.returncode != 0 or not out.is_file():
        print(f"make-long-gop-clip: ffmpeg failed ({done.returncode})", file=sys.stderr)
        return 1
    print(f"make-long-gop-clip: {out} ({out.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
