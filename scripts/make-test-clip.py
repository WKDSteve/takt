#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Makes the test file of the MpvBackend tests: apps/takt-player/tests/data/pattern-3s.mkv.

    python scripts/make-test-clip.py [OUT.mkv]

Three seconds of the lavfi pattern `testsrc2` (160x90, 25 fps, H.264 with a keyframe every second and no B-frames,
so a jump to a whole second lands on it), a 440 Hz sine tone (Vorbis, mono, 48 kHz) and two subtitle tracks, "deu"
and "eng", with one line per second. Everything in it is generated; nothing is taken from a film.

The file is kept in the repository (some 40 kB), so the tests need no ffmpeg: this script is how it was made and how
it is made again. Needs ffmpeg in PATH (lavfi, libx264, libvorbis). Exit 1 when ffmpeg fails.
"""

import pathlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
DEFAULT = ROOT / "apps" / "takt-player" / "tests" / "data" / "pattern-3s.mkv"
SECONDS = 3
FPS = 25


def subtitles(word):
    """An SRT text with one line per second: `word` and the number of the second."""
    blocks = []
    for second in range(SECONDS):
        blocks.append(f"{second + 1}\n00:00:0{second},100 --> 00:00:0{second},900\n{word} {second + 1}\n")
    return "\n".join(blocks)


def command(out, deu, eng):
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
        f"testsrc2=size=160x90:rate={FPS}",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:sample_rate=48000",
        "-i",
        str(deu),
        "-i",
        str(eng),
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-map",
        "2:s",
        "-map",
        "3:s",
        "-t",
        str(SECONDS),
        "-c:v",
        "libx264",
        "-preset",
        "veryslow",
        "-crf",
        "30",
        "-pix_fmt",
        "yuv420p",
        "-bf",
        "0",
        "-g",
        str(FPS),
        "-keyint_min",
        str(FPS),
        "-sc_threshold",
        "0",
        "-c:a",
        "libvorbis",
        "-ac",
        "1",
        "-q:a",
        "0",
        "-c:s",
        "srt",
        "-metadata:s:s:0",
        "language=deu",
        "-metadata:s:s:1",
        "language=eng",
        # Neither subtitle track is on by default: the tests turn them on.
        "-disposition:s",
        "0",
        # No encoder version and no date in the file: two runs give the same bytes with the same ffmpeg.
        "-fflags",
        "+bitexact",
        "-flags:v",
        "+bitexact",
        "-flags:a",
        "+bitexact",
        str(out),
    ]


def main():
    out = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT
    if shutil.which("ffmpeg") is None:
        print("make-test-clip: ffmpeg not found in PATH", file=sys.stderr)
        return 1
    out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="takt-clip-") as temp:
        deu = pathlib.Path(temp) / "deu.srt"
        eng = pathlib.Path(temp) / "eng.srt"
        deu.write_text(subtitles("Zeile"), encoding="utf-8", newline="\n")
        eng.write_text(subtitles("line"), encoding="utf-8", newline="\n")
        done = subprocess.run(command(out, deu, eng), check=False)
    if done.returncode != 0 or not out.is_file():
        print(f"make-test-clip: ffmpeg failed ({done.returncode})", file=sys.stderr)
        return 1
    print(f"make-test-clip: {out} ({out.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
