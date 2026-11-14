#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Makes the container and codec test files of the MpvBackend tests: apps/takt-player/tests/data/format-*.

    python scripts/make-format-clips.py [OUT_DIR]

Two seconds of a flat grey picture (64x64, 25 fps, no sound) in the pairs of container and codec the 3-s file
(H.264 in Matroska) does not cover:

    format-av1.mkv     AV1 in Matroska
    format-h264.mp4    H.264 in MP4
    format-hevc.mp4    HEVC in MP4
    format-vp9.webm    VP9 in WebM

The grey of a frame is its number within its second, as in the long-GOP file (`make-long-gop-clip.py`): frame N has
the value 16 + (N mod 25) * 8 in red, green and blue, so a test reads from the picture which frame it holds. The
quality is set high enough that the value survives the codec. Every file has a keyframe at 0 s and at 1 s and nowhere
else (x265 is told so in so many words: left alone it takes the changing grey for scene cuts).

The files are kept in the repository (a few kB each), so the tests need no ffmpeg: this script is how they were made
and how they are made again. Needs ffmpeg in PATH (lavfi, libsvtav1, libx264, libx265, libvpx-vp9). Exit 1 when
ffmpeg fails.
"""

import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
DEFAULT = ROOT / "apps" / "takt-player" / "tests" / "data"
SECONDS = 2
FPS = 25
# name: the encoder and its options.
CASES = {
    "format-av1.mkv": ["-c:v", "libsvtav1", "-preset", "6", "-crf", "12", "-svtav1-params", "lp=1"],
    "format-h264.mp4": ["-c:v", "libx264", "-preset", "veryslow", "-crf", "6", "-bf", "0"],
    "format-hevc.mp4": [
        "-c:v",
        "libx265",
        "-preset",
        "slow",
        "-crf",
        "8",
        "-x265-params",
        "log-level=error:bframes=0:scenecut=0:keyint=25:min-keyint=25:open-gop=0",
    ],
    "format-vp9.webm": ["-c:v", "libvpx-vp9", "-crf", "8", "-b:v", "0", "-row-mt", "0", "-threads", "1"],
}


def command(out, encoder):
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
        f"nullsrc=size=64x64:rate={FPS},format=gray,geq=lum='16+mod(N,{FPS})*8',format=yuv420p",
        "-t",
        str(SECONDS),
        *encoder,
        "-pix_fmt",
        "yuv420p",
        "-g",
        str(FPS),
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        # No encoder version and no date in the file.
        "-fflags",
        "+bitexact",
        "-flags:v",
        "+bitexact",
        str(out),
    ]


def main():
    out = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT
    if shutil.which("ffmpeg") is None:
        print("make-format-clips: ffmpeg not found in PATH", file=sys.stderr)
        return 1
    out.mkdir(parents=True, exist_ok=True)
    for name, encoder in CASES.items():
        target = out / name
        done = subprocess.run(command(target, encoder), check=False)
        if done.returncode != 0 or not target.is_file():
            print(f"make-format-clips: ffmpeg failed for {name} ({done.returncode})", file=sys.stderr)
            return 1
        print(f"make-format-clips: {target} ({target.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
