#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Fail when a workflow uses an action by tag or branch instead of a commit.

A tag can be moved to other code after the review; a commit cannot (rule
NREL-03). Every `uses:` line under .github/workflows must name a 40-digit
SHA and say in a comment which tag or branch it was:

    - uses: actions/checkout@<40 hex digits> # v4, resolved 2026-11-12

Local actions (`./path`) and `docker://` images are not checked.

    python scripts/check-action-pins.py

Exit 0: all pinned. Exit 1: the lines are listed.
"""

import pathlib
import re
import sys

USES = re.compile(r"^\s*-?\s*uses:\s*(?P<ref>[^\s#]+)\s*(?P<comment>#.*)?$")
PINNED = re.compile(r"^[^@\s]+@[0-9a-f]{40}$")


def findings(text):
    """The (line, reason) pairs of one workflow file."""
    found = []
    for number, line in enumerate(text.split("\n"), start=1):
        match = USES.match(line)
        if not match:
            continue
        ref = match.group("ref").strip("'\"")
        if ref.startswith("./") or ref.startswith("docker://"):
            continue
        if not PINNED.match(ref):
            found.append((number, f"{ref} is not pinned to a commit"))
        elif not match.group("comment"):
            found.append((number, f"{ref} has no comment naming its tag"))
    return found


def main():
    root = pathlib.Path(__file__).resolve().parent.parent / ".github" / "workflows"
    bad = 0
    for path in sorted(root.glob("*.y*ml")):
        for line, reason in findings(path.read_text(encoding="utf-8")):
            bad += 1
            print(f"{path.relative_to(root.parent.parent).as_posix()}:{line}: {reason}")
    if bad:
        print(f"{bad} action(s) not pinned", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
