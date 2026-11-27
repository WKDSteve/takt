#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Pack the rolling archive of takt for Linux or macOS from a prebuilt program.

    python3 scripts/package-release.py --platform linux-x86_64 --build-dir target/release --out-dir dist --sha <commit>

Nothing is built here. The archive `takt-<platform>.tar.gz` holds, in one folder, exactly what
scripts/install.sh installs:

  takt-player   the player
  install.sh    installs this archive's content for the current user
  uninstall.sh  removes that install again
  LICENSE (GPL-3.0-or-later), README.md (how to install), VERSION

VERSION says <version of takt-player>+<short commit>. The archive is reproducible: entries sorted,
a fixed timestamp, no owner, no name or time in the gzip header. SHA256SUMS ("<hash>  <file>") is
written next to it for a local check; the publish action of appkit's release-kit makes the one of
the pre-release. The Windows zip is scripts/package-release.ps1.

Exit 0 when packed; 2 when an argument or the build is wrong.
"""

import argparse
import gzip
import hashlib
import io
import pathlib
import re
import sys
import tarfile

PLATFORMS = ("linux-x86_64", "macos-aarch64")
FIXED_TIME = 946684800  # 2000-01-01T00:00:00Z
ROOT = pathlib.Path(__file__).resolve().parent.parent


def refuse(message):
    print(f"package-release.py: {message}", file=sys.stderr)
    sys.exit(2)


def app_version():
    """The version of takt-player: the first version at the start of a line (that of [package])."""
    text = (ROOT / "apps" / "takt-player" / "Cargo.toml").read_text(encoding="utf-8")
    found = re.search(r'(?m)^version\s*=\s*"([^"]+)"', text)
    if not found:
        refuse("no version in apps/takt-player/Cargo.toml")
    version = found.group(1)
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        refuse(f"the version '{version}' is not X.Y.Z")
    return version


def main():
    parser = argparse.ArgumentParser(description="Pack the rolling archive of takt for Linux or macOS.")
    parser.add_argument("--platform", required=True, choices=PLATFORMS)
    parser.add_argument("--build-dir", default=str(ROOT / "target" / "release"))
    parser.add_argument("--out-dir", default=str(ROOT / "dist"))
    parser.add_argument("--sha", default="0000000", help="the commit of the build, lower case hex, 7 to 40 digits")
    args = parser.parse_args()

    if not re.fullmatch(r"[0-9a-f]{7,40}", args.sha):
        refuse(f"--sha must be a commit id in lower case hex (got '{args.sha}')")
    label = f"{app_version()}+{args.sha[:7]}"

    program = pathlib.Path(args.build_dir) / "takt-player"
    if not program.is_file():
        refuse(f"no {program} (build first: cargo build --release --locked -p takt-player)")
    if program.stat().st_size == 0:
        refuse(f"{program} is empty")

    scripts = pathlib.Path(__file__).resolve().parent
    members = {
        "takt-player": (program.read_bytes(), 0o755),
        "install.sh": ((scripts / "install.sh").read_bytes(), 0o755),
        "uninstall.sh": ((scripts / "uninstall.sh").read_bytes(), 0o755),
        "LICENSE": ((ROOT / "LICENSES" / "GPL-3.0-or-later.txt").read_bytes(), 0o644),
        "README.md": ((scripts / "release-readme-unix.md").read_bytes(), 0o644),
        "VERSION": (f"{label}\n".encode("utf-8"), 0o644),
    }

    out_dir = pathlib.Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    name = f"takt-{args.platform}.tar.gz"
    archive = out_dir / name
    sums = out_dir / "SHA256SUMS"
    # The old checksum goes first: an archive that fails halfway must not leave it beside it.
    sums.unlink(missing_ok=True)
    archive.unlink(missing_ok=True)

    raw = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w", format=tarfile.USTAR_FORMAT) as tar:
            for member in sorted(members):
                data, mode = members[member]
                info = tarfile.TarInfo(member)
                info.size = len(data)
                info.mode = mode
                info.mtime = FIXED_TIME
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                tar.addfile(info, io.BytesIO(data))
    archive.write_bytes(raw.getvalue())

    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    sums.write_text(f"{digest}  {name}\n", encoding="utf-8", newline="\n")
    print(f"packaged {label}")
    print(archive)
    print(f"{digest}  {name}")


if __name__ == "__main__":
    main()
