#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Remove the takt that install.sh installed for the current user (Linux, macOS). The counterpart of
# uninstall.ps1. Run from the installed copy (<data folder>/uninstall.sh) it removes the places the install
# wrote down (the file `paths` beside it); run from the unpacked archive it removes the default places.
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
    data_dir="${TAKT_DATA_DIR:-$data_home/takt}"
    bin_dir="${TAKT_BIN_DIR:-${XDG_BIN_HOME:-${HOME:-}/.local/bin}}"
    desktop_entry="${TAKT_APPLICATIONS_DIR:-$data_home/applications}/takt.desktop"
fi
case "$bin_dir$data_dir" in
    /*/*) ;;
    *) fail "no absolute places to remove from (HOME is not set?)" ;;
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
