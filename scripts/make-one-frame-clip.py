#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Makes the one-frame test file of the MpvBackend tests: apps/takt-player/tests/data/one-frame.mkv.

    python scripts/make-one-frame-clip.py [OUT.mkv]

A single frame (64x64, H.264, 25 fps, so 40 ms long) of a flat grey picture, no sound. mpv takes such a file for an
image (a "sparse" video stream without audio) and shows it for `image-display-duration`, 5 s by default; the test
checks that the MpvBackend lets it end after its frame instead.

The file is kept in the repository, so the tests need no ffmpeg: this script is how it was made and how it is made
again. Needs ffmpeg in PATH (lavfi, libx264). Exit 1 when ffmpeg fails.
"""

import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DEFAULT = ROOT / "apps" / "takt-player" / "tests" / "data" / "one-frame.mkv"
FPS = 25


def command(out):
    """The ffmpeg call as an argument list: no shell reads the filter expressions."""
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
        f"nullsrc=size=64x64:rate={FPS},format=gray,geq=lum='128',format=yuv420p",
        "-frames:v",
        "1",
        "-c:v",
        "libx264",
        "-preset",
        "veryslow",
        "-crf",
        "4",
        "-pix_fmt",
        "yuv420p",
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
        print("make-one-frame-clip: ffmpeg not found in PATH", file=sys.stderr)
        return 1
    out.parent.mkdir(parents=True, exist_ok=True)
    result = subprocess.run(command(out), check=False)
    if result.returncode != 0:
        print(f"make-one-frame-clip: ffmpeg failed ({result.returncode})", file=sys.stderr)
        return 1
    print(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
