#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Builds the player for the test sheets (.testing.lua), each kind into a target folder of its own.

    python scripts/build-sheet-app.py mpv     # app `takt-file`: with libmpv, into target/sheets-mpv
    python scripts/build-sheet-app.py plain   # app `takt-test-picture`: without libmpv, into target/sheets-plain
    python scripts/build-sheet-app.py own     # app `takt-own-pipeline`: with libmpv and FFmpeg, into target/sheets-own

`mpv`: `cargo build -p takt-player --locked` with TAKT_LIBMPV_DIR (docs/BUILD.md), and libmpv-2.dll copied next to the
program: the sheet starts the program itself (the driver finds the window by its process), so the DLL cannot come from
a PATH set by a wrapper. `plain`: the same without TAKT_LIBMPV_DIR and FFMPEG_DIR, the build the CI makes, whatever the
environment of the shell says. `own`: the player with libmpv as for `mpv`, and with FFMPEG_DIR the two helper programs
of the own pipeline (takt-demux-helper, takt-decode-helper) next to it, with the FFmpeg libraries they load copied
beside them: the player starts the helpers itself, and they find their DLLs next to themselves. The other builds of the
checkout are not touched. Windows only, as desktop sheets are.

Exit 1 when TAKT_LIBMPV_DIR or libmpv-2.dll in it is missing (mpv, own), when FFMPEG_DIR or its bin folder is missing
(own), or when cargo fails; 2 for another argument.
"""

import os
import pathlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent


def cargo(target, env, packages=("takt-player",)):
    done = subprocess.run(
        ["cargo", "build", *(arg for name in packages for arg in ("-p", name)), "--locked", "--target-dir", str(target)],
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


def own():
    target = ROOT / "target" / "sheets-own"
    folder = os.environ.get("TAKT_LIBMPV_DIR", "")
    dll = pathlib.Path(folder) / "libmpv-2.dll"
    ffmpeg = pathlib.Path(os.environ.get("FFMPEG_DIR", "")) / "bin"
    if not folder or not dll.is_file():
        print(
            "build-sheet-app: set TAKT_LIBMPV_DIR to the libmpv development archive with libmpv-2.dll (docs/BUILD.md)",
            file=sys.stderr,
        )
        return 1
    libraries = sorted(ffmpeg.glob("*.dll")) if os.environ.get("FFMPEG_DIR") else []
    if not libraries:
        print(
            "build-sheet-app: set FFMPEG_DIR to an FFmpeg 9.0 with bin/*.dll (LGPL shared build, docs/BUILD.md)",
            file=sys.stderr,
        )
        return 1
    if not cargo(target, dict(os.environ), ("takt-player", "takt-demux", "takt-decode")):
        return 1
    shutil.copy2(dll, target / "debug" / "libmpv-2.dll")
    for library in libraries:
        shutil.copy2(library, target / "debug" / library.name)
    print(target / "debug" / "takt-player.exe")
    return 0


def main():
    kinds = {"mpv": mpv, "plain": plain, "own": own}
    if len(sys.argv) != 2 or sys.argv[1] not in kinds:
        print("usage: build-sheet-app.py mpv|plain|own", file=sys.stderr)
        return 2
    return kinds[sys.argv[1]]()


if __name__ == "__main__":
    sys.exit(main())
