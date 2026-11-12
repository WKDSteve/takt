#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Count what a `cargo test` run did not run, so a skip never passes as green.

Reads the output of `cargo test` (a file, or stdin) and counts the passed,
failed and ignored tests over all `test result:` lines, and lists every
ignored test with its reason (`test name ... ignored, SKIP: ...`) (rule
TST-16: a test that cannot run says SKIP and is no evidence). The table goes
to stdout and, in GitHub Actions, to the job summary ($GITHUB_STEP_SUMMARY).

    cargo test --workspace 2>&1 | tee test.log
    python scripts/ci-skip-summary.py test.log [--title NAME]

Exit 0 always when the log could be read (the test step fails on its own);
exit 2 when it holds no `test result:` line at all, which means no test ran.
"""

import argparse
import os
import re
import sys

RESULT = re.compile(
    r"test result: \w+\. (?P<passed>\d+) passed; (?P<failed>\d+) failed; (?P<ignored>\d+) ignored"
)
IGNORED = re.compile(r"^test (?P<name>\S+) \.\.\. ignored(?:, (?P<reason>.*))?$")


def summarize(text):
    """(passed, failed, ignored, [(name, reason)]) of one log, None without results."""
    totals = [0, 0, 0]
    seen = False
    skipped = []
    for line in text.splitlines():
        line = line.strip()
        match = RESULT.search(line)
        if match:
            seen = True
            for i, key in enumerate(("passed", "failed", "ignored")):
                totals[i] += int(match.group(key))
            continue
        match = IGNORED.match(line)
        if match:
            skipped.append((match.group("name"), match.group("reason") or "no reason given"))
    if not seen:
        return None
    return (*totals, skipped)


def markdown(title, summary):
    passed, failed, ignored, skipped = summary
    lines = [
        f"### {title}",
        "",
        f"{passed} passed, {failed} failed, **{ignored} skipped** (ignored, not evidence)",
    ]
    if skipped:
        lines += ["", "| skipped test | reason |", "|---|---|"]
        lines += [f"| `{name}` | {reason.replace('|', '/')} |" for name, reason in skipped]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("log", nargs="?", help="output of cargo test (default: stdin)")
    parser.add_argument("--title", default="Tests")
    args = parser.parse_args()
    if args.log:
        with open(args.log, encoding="utf-8", errors="replace") as handle:
            text = handle.read()
    else:
        text = sys.stdin.read()
    summary = summarize(text)
    if summary is None:
        print("no `test result:` line: no test ran", file=sys.stderr)
        return 2
    block = markdown(args.title, summary)
    print(block)
    target = os.environ.get("GITHUB_STEP_SUMMARY")
    if target:
        with open(target, "a", encoding="utf-8") as handle:
            handle.write(block + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
