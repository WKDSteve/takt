#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Makes the colour test files of the MpvBackend tests: apps/takt-player/tests/data/colour-*.mkv.

    python scripts/make-colour-clips.py [OUT_DIR]

One flat colour (0x40a0c0, 64x64, five frames, H.264) is encoded once; every file is that same stream with other
colour tags in its header (`h264_metadata`, no re-encode). So the bytes of the picture are identical in all files,
and what differs on the screen comes from the tags alone: that is what the tests measure.

    colour-bt709.mkv   primaries BT.709,         transfer BT.709, matrix BT.709
    colour-ntsc.mkv    primaries BT.601 (525),   transfer BT.601, matrix BT.601
    colour-pal.mkv     primaries BT.601 (625),   transfer BT.601, matrix BT.601
    colour-hdr10.mkv   primaries BT.2020,        transfer PQ,     matrix BT.2020 (non-constant)
    colour-hlg.mkv     primaries BT.2020,        transfer HLG,    matrix BT.2020 (non-constant)
    colour-odd.mkv     primaries BT.470 M,       transfer gamma 2.8, matrix BT.709 (nothing Takt has a name for)

The files are kept in the repository (2 kB each), so the tests need no ffmpeg: this script is how they were made and
how they are made again. Needs ffmpeg in PATH (lavfi, libx264). Exit 1 when ffmpeg fails.
"""

import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
DEFAULT = ROOT / "apps" / "takt-player" / "tests" / "data"
# name: primaries, transfer, matrix, as the numbers of ITU-T H.273.
CASES = {
    "bt709": (1, 1, 1),
    "ntsc": (6, 6, 6),
    "pal": (5, 6, 5),
    "hdr10": (9, 16, 9),
    "hlg": (9, 18, 9),
    "odd": (4, 5, 1),
}


def ffmpeg(*arguments):
    """Runs ffmpeg with an argument list: no shell reads the filter expressions."""
    done = subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", *arguments],
        check=False,
    )
    return done.returncode == 0


def main():
    out = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT
    out.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as scratch:
        base = pathlib.Path(scratch) / "base.mkv"
        encoded = ffmpeg(
            "-f",
            "lavfi",
            "-i",
            "color=c=0x40a0c0:size=64x64:rate=25",
            "-frames:v",
            "5",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-crf",
            "10",
            "-bitexact",
            str(base),
        )
        if not encoded:
            return 1
        for name, (primaries, transfer, matrix) in CASES.items():
            tags = (
                f"h264_metadata=colour_primaries={primaries}"
                f":transfer_characteristics={transfer}:matrix_coefficients={matrix}"
            )
            target = out / f"colour-{name}.mkv"
            if not ffmpeg("-i", str(base), "-c", "copy", "-bsf:v", tags, "-bitexact", str(target)):
                return 1
            print(f"{target} ({target.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
