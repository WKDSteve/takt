#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Makes the test files of the software decoder (takt-decode): apps/takt-player/tests/data/decode-*.

    python scripts/make-decode-clips.py [OUT_DIR]

Two seconds of a moving test picture with edges, colour and noise-free gradients (ffmpeg's `testsrc2`, 25 fps, no
sound), so that a decoder that gets a picture wrong, or hands the pictures out in the wrong order, is seen in the
bytes:

    decode-h264.mp4        H.264 in MP4, 160x90, 8 bit, with B-frames (pictures leave the decoder reordered)
    decode-hevc.mp4        HEVC in MP4, 160x90, 8 bit, with B-frames
    decode-av1.mkv         AV1 in Matroska, 160x90, 8 bit
    decode-hevc-10bit.mp4  HEVC in MP4, 130x74, 10 bit (a chroma plane of odd width and height)
    decode-av1-10bit.mkv   AV1 in Matroska, 130x74, 10 bit

Every file has a keyframe every second and names the BT.709 matrix and the limited range (the primaries and the
transfer function stay unnamed, as in the other test files). The tests compare each decoded picture
with what the `ffmpeg` program decodes from the same file, byte for byte, and with what libmpv shows of it (PSNR).

The files are kept in the repository (about 40 kB each), so the tests need no encoder: this script is how they were
made and how they are made again. Needs ffmpeg in PATH (lavfi, libx264, libx265 with 10 bit, libsvtav1). Exit 1 when
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
X265 = "log-level=error:bframes=2:keyint=25:min-keyint=25:scenecut=0:open-gop=0"
# name: size, pixel format, the encoder and its options.
CASES = {
    "decode-h264.mp4": ("160x90", "yuv420p", ["-c:v", "libx264", "-preset", "slow", "-crf", "20", "-bf", "2"]),
    "decode-hevc.mp4": ("160x90", "yuv420p", ["-c:v", "libx265", "-preset", "medium", "-crf", "22", "-x265-params", X265]),
    "decode-av1.mkv": ("160x90", "yuv420p", ["-c:v", "libsvtav1", "-preset", "6", "-crf", "28", "-svtav1-params", "lp=1"]),
    "decode-hevc-10bit.mp4": (
        "130x74",
        "yuv420p10le",
        ["-c:v", "libx265", "-preset", "medium", "-crf", "22", "-x265-params", X265],
    ),
    "decode-av1-10bit.mkv": (
        "130x74",
        "yuv420p10le",
        ["-c:v", "libsvtav1", "-preset", "6", "-crf", "28", "-svtav1-params", "lp=1"],
    ),
}


def command(out, size, pixel_format, encoder):
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
        f"testsrc2=size={size}:rate={FPS}",
        "-t",
        str(SECONDS),
        *encoder,
        "-pix_fmt",
        pixel_format,
        "-g",
        str(FPS),
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-color_range",
        "tv",
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
        print("make-decode-clips: ffmpeg not found in PATH", file=sys.stderr)
        return 1
    out.mkdir(parents=True, exist_ok=True)
    for name, (size, pixel_format, encoder) in CASES.items():
        target = out / name
        done = subprocess.run(command(target, size, pixel_format, encoder), check=False)
        if done.returncode != 0 or not target.is_file():
            print(f"make-decode-clips: ffmpeg failed for {name} ({done.returncode})", file=sys.stderr)
            return 1
        print(f"make-decode-clips: {target} ({target.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
