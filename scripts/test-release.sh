#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Test package-release.py, the archive it makes and its install and uninstall (Linux, macOS), without a build
# and without a real install: the counterpart of test-release.ps1.
#
#   sh scripts/test-release.sh [--archive FILE --program FILE]
#
# Packs the archive from a stand-in (a shell script: nothing is built) and checks
#   - the archive holds exactly takt-player, install.sh, uninstall.sh, LICENSE, README.md and VERSION, with the
#     bytes of the sources, the program and the scripts executable, and VERSION says <version>+<short commit>;
#   - SHA256SUMS names the archive with its real SHA-256, and packing twice gives the same bytes;
#   - what is refused: a commit that is no hex, a missing program, an unknown platform;
#   - the unpacked archive installed into temp places (TAKT_BIN_DIR, TAKT_DATA_DIR, TAKT_APPLICATIONS_DIR; the
#     account's own are never touched): the files, the menu entry on Linux, an update over it, a folder that is no
#     archive refused; then the installed uninstall.sh, twice, and the one of the archive;
# With --archive and --program (the rolling workflow does) the archive of a real build is checked too: it
# holds that very program, and the program answers `--keys`.
#
# Exit 0 when all checks pass.

set -u

here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
python="${PYTHON:-python3}"
command -v "$python" >/dev/null 2>&1 || python=python

real_archive=""
real_program=""
while [ $# -gt 0 ]; do
    case "$1" in
        --archive) real_archive="$2"; shift 2 ;;
        --program) real_program="$2"; shift 2 ;;
        *) printf 'test-release.sh: unknown option: %s\n' "$1" >&2; exit 2 ;;
    esac
done

failed=0
check() {
    # check "<what>" <command...>
    what="$1"
    shift
    if "$@" >/dev/null 2>&1; then
        printf 'ok     %s\n' "$what"
    else
        printf 'FAIL   %s\n' "$what"
        failed=$((failed + 1))
    fi
}
refused() {
    # refused "<what>" <command...>: the command must fail
    what="$1"
    shift
    if "$@" >/dev/null 2>&1; then
        printf 'FAIL   %s (it was not refused)\n' "$what"
        failed=$((failed + 1))
    else
        printf 'ok     %s\n' "$what"
    fi
}
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d ' ' -f 1; else shasum -a 256 "$1" | cut -d ' ' -f 1; fi
}

work="$(mktemp -d 2>/dev/null || mktemp -d -t takt-test-release)"
trap 'rm -rf "$work"' EXIT INT TERM
sha=0123456789abcdef0123456789abcdef01234567
version="$(sed -n 's/^version *= *"\([0-9][0-9.]*\)"/\1/p' "$root/apps/takt-player/Cargo.toml" | head -n 1)"
label="$version+0123456"

# The stand-in program: it answers --keys like the player and nothing else.
mkdir -p "$work/build"
printf '#!/bin/sh\necho stand-in\n' >"$work/build/takt-player"
chmod 755 "$work/build/takt-player"

pack() {
    "$python" "$here/package-release.py" --platform linux-x86_64 --build-dir "$work/build" --out-dir "$1" --sha "$sha"
}

pack "$work/out1" >/dev/null 2>&1
check "package-release.py packs the archive" test -f "$work/out1/takt-linux-x86_64.tar.gz"
mkdir "$work/x1"
tar -xzf "$work/out1/takt-linux-x86_64.tar.gz" -C "$work/x1"
names="$(cd "$work/x1" && ls -A | sort | tr '\n' ' ')"
check "the archive holds exactly the six files" test "$names" = "LICENSE README.md VERSION install.sh takt-player uninstall.sh "
check "VERSION says <version>+<short commit>" test "$(cat "$work/x1/VERSION")" = "$label"
check "the program is the stand-in" cmp "$work/x1/takt-player" "$work/build/takt-player"
check "install.sh is the source" cmp "$work/x1/install.sh" "$here/install.sh"
check "uninstall.sh is the source" cmp "$work/x1/uninstall.sh" "$here/uninstall.sh"
check "LICENSE is the GPL text" cmp "$work/x1/LICENSE" "$root/LICENSES/GPL-3.0-or-later.txt"
check "the program is executable" test -x "$work/x1/takt-player"
check "install.sh is executable" test -x "$work/x1/install.sh"
want="$(sha256_of "$work/out1/takt-linux-x86_64.tar.gz")  takt-linux-x86_64.tar.gz"
check "SHA256SUMS names the archive with its SHA-256" test "$(cat "$work/out1/SHA256SUMS")" = "$want"
pack "$work/out2" >/dev/null 2>&1
check "packing twice gives the same bytes" cmp "$work/out1/takt-linux-x86_64.tar.gz" "$work/out2/takt-linux-x86_64.tar.gz"

refused "a commit that is no hex is refused" "$python" "$here/package-release.py" --platform linux-x86_64 --build-dir "$work/build" --out-dir "$work/o3" --sha NOTHEX1
refused "an unknown platform is refused" "$python" "$here/package-release.py" --platform plan9 --build-dir "$work/build" --out-dir "$work/o4" --sha "$sha"
refused "a missing program is refused" "$python" "$here/package-release.py" --platform linux-x86_64 --build-dir "$work/nothing" --out-dir "$work/o5" --sha "$sha"

# Install into throw-away places.
home="$work/home"
mkdir -p "$home"
install_here() {
    env -u XDG_BIN_HOME -u XDG_DATA_HOME HOME="$home" TAKT_BIN_DIR="$home/bin" TAKT_DATA_DIR="$home/data/takt" \
        TAKT_APPLICATIONS_DIR="$home/data/applications" sh "$work/x1/install.sh" --allow-root "$@"
}
check "install.sh installs" install_here
check "the program is installed and executable" test -x "$home/bin/takt-player"
check "the installed program is the stand-in" cmp "$home/bin/takt-player" "$work/build/takt-player"
check "VERSION is installed" test "$(cat "$home/data/takt/VERSION")" = "$label"
check "uninstall.sh is installed" test -x "$home/data/takt/uninstall.sh"
if [ "$(uname -s)" = "Linux" ]; then
    check "the menu entry names the installed program" grep -q "^Exec=\"$home/bin/takt-player\"" "$home/data/applications/takt.desktop"
fi
check "an update over the install works" install_here
check "no staging file is left" test -z "$(ls -A "$home/bin" | grep -v '^takt-player$')"
check "--no-desktop-entry leaves the entry out" sh -c "rm -f '$home/data/applications/takt.desktop' && TAKT_BIN_DIR='$home/bin' TAKT_DATA_DIR='$home/data/takt' TAKT_APPLICATIONS_DIR='$home/data/applications' sh '$work/x1/install.sh' --allow-root --no-desktop-entry && test ! -e '$home/data/applications/takt.desktop'"

mkdir "$work/notarchive"
cp "$work/x1/install.sh" "$work/notarchive/install.sh"
refused "a folder that is no archive is refused" env TAKT_BIN_DIR="$work/nb" TAKT_DATA_DIR="$work/nd" TAKT_APPLICATIONS_DIR="$work/na" sh "$work/notarchive/install.sh" --allow-root
check "the refused install wrote nothing" test ! -e "$work/nb"
refused "a relative place is refused" env TAKT_BIN_DIR="relative/bin" TAKT_DATA_DIR="$home/data/takt" TAKT_APPLICATIONS_DIR="$home/data/applications" sh "$work/x1/install.sh" --allow-root

# Remove: the installed copy, twice.
check "the installed uninstall.sh removes it" sh "$home/data/takt/uninstall.sh"
check "the program is gone" test ! -e "$home/bin/takt-player"
check "the data folder is gone" test ! -e "$home/data/takt"
install_here >/dev/null 2>&1
check "uninstall.sh of the archive removes what the install wrote" env TAKT_BIN_DIR="$home/bin" TAKT_DATA_DIR="$home/data/takt" TAKT_APPLICATIONS_DIR="$home/data/applications" sh "$work/x1/uninstall.sh"
check "that removed the program too" test ! -e "$home/bin/takt-player"
check "removing twice is no error" env TAKT_BIN_DIR="$home/bin" TAKT_DATA_DIR="$home/data/takt" TAKT_APPLICATIONS_DIR="$home/data/applications" sh "$work/x1/uninstall.sh"

# The archive of a real build, made by the workflow.
if [ -n "$real_archive" ]; then
    check "the real archive exists" test -f "$real_archive"
    mkdir "$work/real"
    tar -xzf "$real_archive" -C "$work/real"
    check "the real archive holds the program of the build" cmp "$work/real/takt-player" "$real_program"
    check "the real program answers --keys" sh -c "'$work/real/takt-player' --keys | grep -q ."
fi

if [ "$failed" = 0 ]; then
    printf 'test-release.sh: all checks passed\n'
    exit 0
fi
printf 'test-release.sh: %s check(s) failed\n' "$failed"
exit 1
