#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Remove the takt that install.sh installed for the current user (Linux, macOS). The counterpart of
# uninstall.ps1. Run from the installed copy (<data folder>/uninstall.sh) it removes the places the install
# wrote down (the file `paths` beside it); run from the unpacked archive it reads the `paths` of an install in the
# data folder (TAKT_DATA_DIR or the default), and only without one does it remove the default places.
# It removes only what install.sh put there: the program, the menu entry, VERSION, uninstall.sh and paths, and
# the data folder when that is empty then. Nothing is an error when it is already gone.

set -eu

fail() {
    printf 'uninstall.sh: %s\n' "$*" >&2
    exit 1
}

here="$(cd "$(dirname "$0")" && pwd)"
data_home="${XDG_DATA_HOME:-${HOME:-}/.local/share}"

if [ -f "$here/paths" ] && [ -f "$here/VERSION" ]; then
    # The installed copy.
    data_dir="$here"
    bin_dir="$(sed -n 's/^bin_dir=//p' "$here/paths" | head -n 1)"
    desktop_entry="$(sed -n 's/^desktop_entry=//p' "$here/paths" | head -n 1)"
else
    if [ -z "${HOME:-}" ]; then
        { [ -n "${TAKT_BIN_DIR:-}" ] && [ -n "${TAKT_DATA_DIR:-}" ] && [ -n "${TAKT_APPLICATIONS_DIR:-}" ]; } ||
            fail "HOME is not set (name TAKT_BIN_DIR, TAKT_DATA_DIR and TAKT_APPLICATIONS_DIR instead)"
    fi
    data_dir="${TAKT_DATA_DIR:-$data_home/takt}"
    if [ -f "$data_dir/paths" ]; then
        # An install is there: what it wrote down wins over the defaults and the variables of this shell, or a
        # program installed under another XDG_BIN_HOME or TAKT_BIN_DIR would stay while its record is removed.
        bin_dir="$(sed -n 's/^bin_dir=//p' "$data_dir/paths" | head -n 1)"
        desktop_entry="$(sed -n 's/^desktop_entry=//p' "$data_dir/paths" | head -n 1)"
    else
        bin_dir="${TAKT_BIN_DIR:-${XDG_BIN_HOME:-${HOME:-}/.local/bin}}"
        desktop_entry="${TAKT_APPLICATIONS_DIR:-$data_home/applications}/takt.desktop"
    fi
fi
# Each place on its own: the `paths` file of an install is read back here, and a damaged one (an empty bin_dir would
# turn "$bin_dir/takt-player" into /takt-player, `//` or a `..` segment would name another folder) must remove
# nothing. The same rule as install.sh.
place() {
    case "$2" in /?*) ;; *) fail "$1 is not an absolute path ('$2')" ;; esac
    case "$2" in //* | */. | */./* | */.. | */../*) fail "$1 holds '//', '.' or '..' ('$2')" ;; esac
}
place "the program folder" "$bin_dir"
place "the data folder" "$data_dir"
case "$desktop_entry" in
    "") ;;
    /?*/takt.desktop) place "the menu entry" "$desktop_entry" ;;
    *) fail "the menu entry is not <absolute folder>/takt.desktop ('$desktop_entry')" ;;
esac

removed=0
remove_file() {
    if [ -e "$1" ] || [ -L "$1" ]; then
        rm -f "$1"
        removed=1
        printf 'removed %s\n' "$1"
    fi
}

remove_file "$bin_dir/takt-player"
[ -n "$desktop_entry" ] && remove_file "$desktop_entry"
remove_file "$data_dir/VERSION"
remove_file "$data_dir/paths"
# This file may be the running script itself: a shell has read what it needs, and unlinking is safe.
remove_file "$data_dir/uninstall.sh"
rmdir "$data_dir" 2>/dev/null || true

[ "$removed" = 1 ] || printf 'nothing of takt was installed here\n'
exit 0
