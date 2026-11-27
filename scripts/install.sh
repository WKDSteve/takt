#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Install takt for the current user (Linux, macOS), without administrator rights. The counterpart of
# install.ps1; it installs the folder it sits in (the unpacked takt-<platform>.tar.gz).
#
#   ./install.sh [--no-desktop-entry] [--allow-root]
#
# It puts
#   takt-player                          in ${XDG_BIN_HOME:-~/.local/bin}
#   VERSION, uninstall.sh, paths         in ${XDG_DATA_HOME:-~/.local/share}/takt
#   takt.desktop (Linux only)            in ${XDG_DATA_HOME:-~/.local/share}/applications
# TAKT_BIN_DIR, TAKT_DATA_DIR and TAKT_APPLICATIONS_DIR (absolute paths) name other places; the test uses
# them, so it never touches the account's own. An update over an earlier install replaces the program.
#
# Remove it again with the installed uninstall.sh (or the one in this folder).

set -eu

want_desktop_entry=1
allow_root=0
for argument in "$@"; do
    case "$argument" in
        --no-desktop-entry) want_desktop_entry=0 ;;
        --allow-root) allow_root=1 ;;
        -h | --help)
            sed -n '4,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            printf 'install.sh: unknown option: %s (see --help)\n' "$argument" >&2
            exit 2
            ;;
    esac
done

fail() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}

here="$(cd "$(dirname "$0")" && pwd)"

# The app belongs to one user: as root it would be root's.
if [ "$allow_root" = 0 ] && [ "$(id -u)" = 0 ]; then
    fail "do not run this as root: it installs for one user (--allow-root for a build machine)"
fi

# What is installed is a release payload, not any folder.
[ -f "$here/takt-player" ] && [ -s "$here/takt-player" ] || fail "no takt-player next to this script: unpack the archive and run it from there"
[ -f "$here/uninstall.sh" ] || fail "no uninstall.sh next to this script: this folder is no takt archive"
[ -f "$here/VERSION" ] || fail "no VERSION next to this script: this folder is no takt archive"
version="$(head -n 1 "$here/VERSION")"
case "$version" in
    [0-9]*.[0-9]*.[0-9]*+[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
    *) fail "VERSION says '$version', not <version>+<short commit>: this folder is no takt archive" ;;
esac

absolute() {
    case "$2" in
        /*) ;;
        *) fail "$1 must be an absolute path (got '$2')" ;;
    esac
}
if [ -z "${TAKT_BIN_DIR:-}${XDG_BIN_HOME:-}${TAKT_DATA_DIR:-}${XDG_DATA_HOME:-}" ]; then
    [ -n "${HOME:-}" ] || fail "HOME is not set"
fi
data_home="${XDG_DATA_HOME:-${HOME:-}/.local/share}"
bin_dir="${TAKT_BIN_DIR:-${XDG_BIN_HOME:-${HOME:-}/.local/bin}}"
data_dir="${TAKT_DATA_DIR:-$data_home/takt}"
apps_dir="${TAKT_APPLICATIONS_DIR:-$data_home/applications}"
absolute "the program folder" "$bin_dir"
absolute "the data folder" "$data_dir"
absolute "the applications folder" "$apps_dir"

mkdir -p "$bin_dir" "$data_dir"
# Copied beside the target and moved over it: an update over a running player replaces the file, it does not
# rewrite it in place.
staged="$bin_dir/.takt-player.new.$$"
cp "$here/takt-player" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$bin_dir/takt-player"

cp "$here/VERSION" "$data_dir/VERSION"
cp "$here/uninstall.sh" "$data_dir/uninstall.sh"
chmod 755 "$data_dir/uninstall.sh"

# The places, for the installed uninstall.sh (it must find what this run put where it did).
desktop_entry=""
if [ "$want_desktop_entry" = 1 ] && [ "$(uname -s)" = "Linux" ]; then
    mkdir -p "$apps_dir"
    desktop_entry="$apps_dir/takt.desktop"
    {
        printf '[Desktop Entry]\n'
        printf 'Type=Application\n'
        printf 'Name=Takt\n'
        printf 'Comment=Video player\n'
        printf 'Exec="%s/takt-player" %%F\n' "$bin_dir"
        printf 'Terminal=false\n'
        printf 'Categories=AudioVideo;Video;Player;\n'
    } >"$desktop_entry"
fi
{
    printf 'bin_dir=%s\n' "$bin_dir"
    printf 'desktop_entry=%s\n' "$desktop_entry"
} >"$data_dir/paths"

printf 'installed takt %s\n' "$version"
printf '  program: %s/takt-player\n' "$bin_dir"
[ -n "$desktop_entry" ] && printf '  menu entry: %s\n' "$desktop_entry"
case ":${PATH:-}:" in
    *":$bin_dir:"*) ;;
    *) printf '  note: %s is not on your PATH\n' "$bin_dir" ;;
esac
printf 'remove it with %s/uninstall.sh\n' "$data_dir"
