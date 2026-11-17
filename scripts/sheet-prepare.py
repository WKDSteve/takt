#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Puts the files of a test sheet into the folder of its run (.testing.lua, `prepare`).

    python scripts/sheet-prepare.py film <folder>       # film.mkv: the test clip, a minute long
    python scripts/sheet-prepare.py broken <folder>     # the same, and broken.mkv: a text with the ending of a film
    python scripts/sheet-prepare.py sidecar <folder>    # the same, and film.srt beside the film

What a hand run did by dropping files: the folder is the `${temp}` of the run, removed afterwards, and the app of the
sheet is started with the files in it. The clip is the one of the tests (scripts/make-test-clip.py: the pattern, a
tone, the subtitle tracks "deu" and "eng"), but a minute long: the three seconds of the file in the repository are
over before a window that starts on a busy machine is paused, and a file at its end has no tracks.

Needs ffmpeg in PATH, as make-test-clip.py does. Exit 1 when the clip cannot be made, 2 for another call.
"""

import pathlib
import subprocess
import sys

SCRIPTS = pathlib.Path(__file__).resolve().parent
SECONDS = 60
# One line over the whole clip, so that a picture taken at any time shows it.
SIDECAR = "1\n00:00:00,000 --> 00:01:00,000\nbeside the film\n"


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("film", "broken", "sidecar"):
        print(__doc__, file=sys.stderr)
        return 2
    kind = sys.argv[1]
    folder = pathlib.Path(sys.argv[2])
    folder.mkdir(parents=True, exist_ok=True)
    made = subprocess.run(
        [sys.executable, str(SCRIPTS / "make-test-clip.py"), str(folder / "film.mkv"), str(SECONDS)],
        check=False,
    )
    if made.returncode != 0:
        return 1
    if kind == "broken":
        (folder / "broken.mkv").write_text("this is no film\n", encoding="utf-8", newline="\n")
    if kind == "sidecar":
        (folder / "film.srt").write_text(SIDECAR, encoding="utf-8", newline="\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
