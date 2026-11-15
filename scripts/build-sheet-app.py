#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Builds the player for the test sheets (.testing.lua), each kind into a target folder of its own.

    python scripts/build-sheet-app.py mpv     # app `takt-file`: with libmpv, into target/sheets-mpv
    python scripts/build-sheet-app.py plain   # app `takt-test-picture`: without libmpv, into target/sheets-plain

`mpv`: `cargo build -p takt-player --locked` with TAKT_LIBMPV_DIR (docs/BUILD.md), and libmpv-2.dll copied next to the
program: the sheet starts the program itself (the driver finds the window by its process), so the DLL cannot come from
a PATH set by a wrapper. `plain`: the same without TAKT_LIBMPV_DIR and FFMPEG_DIR, the build the CI makes, whatever the
environment of the shell says. The other builds of the checkout are not touched. Windows only, as desktop sheets are.

Exit 1 when TAKT_LIBMPV_DIR or libmpv-2.dll in it is missing (mpv), or when cargo fails; 2 for another argument.
"""

import os
import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def cargo(target, env):
    done = subprocess.run(
        ["cargo", "build", "-p", "takt-player", "--locked", "--target-dir", str(target)],
        cwd=ROOT,
        env=env,
        check=False,
    )
    if done.returncode != 0:
        print(f"build-sheet-app: cargo failed ({done.returncode})", file=sys.stderr)
    return done.returncode == 0


def plain():
    env = {k: v for k, v in os.environ.items() if k not in ("TAKT_LIBMPV_DIR", "FFMPEG_DIR")}
    target = ROOT / "target" / "sheets-plain"
    if not cargo(target, env):
        return 1
    print(target / "debug" / "takt-player.exe")
    return 0


def mpv():
    target = ROOT / "target" / "sheets-mpv"
    folder = os.environ.get("TAKT_LIBMPV_DIR", "")
    dll = pathlib.Path(folder) / "libmpv-2.dll"
    if not folder or not dll.is_file():
        print(
            "build-sheet-app: set TAKT_LIBMPV_DIR to the libmpv development archive with libmpv-2.dll (docs/BUILD.md)",
            file=sys.stderr,
        )
        return 1
    if not cargo(target, dict(os.environ)):
        return 1
    shutil.copy2(dll, target / "debug" / "libmpv-2.dll")
    print(target / "debug" / "takt-player.exe")
    return 0


def main():
    kinds = {"mpv": mpv, "plain": plain}
    if len(sys.argv) != 2 or sys.argv[1] not in kinds:
        print("usage: build-sheet-app.py mpv|plain", file=sys.stderr)
        return 2
    return kinds[sys.argv[1]]()


if __name__ == "__main__":
    sys.exit(main())
