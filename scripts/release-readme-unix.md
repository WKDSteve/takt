# Takt (rolling build, Linux and macOS)

The newest green commit of `main`, for looking at the state of the work: not a release, not signed,
replaced after every green CI run. The player is in Phase 0: a window with the moving test picture of
the NullBackend, the diagnostic overlay (F3) and the sound path.

**This build plays no media files.** It is built without libmpv, and libmpv is not part of this archive.
The player does play files, with sound, when it is built from a checkout with libmpv
(<https://github.com/WKDSteve/takt>, `docs/BUILD.md`).

**macOS is untested:** the build is made on a hosted runner and has never been started on a real Mac.
Linux needs the system libraries of a desktop (ALSA `libasound2`, D-Bus `libdbus-1-3`, a Vulkan or OpenGL
driver, X11 or Wayland).

For private machines only: not for a managed work machine.

## Install

1. Check the archive against `SHA256SUMS`: `sha256sum takt-<platform>.tar.gz` (macOS: `shasum -a 256`).
2. Unpack it into a new, empty folder: `mkdir takt && tar -xzf takt-<platform>.tar.gz -C takt`.
3. In that folder: `./install.sh` (add `--no-desktop-entry` to leave out the menu entry on Linux).

It installs for the current user, without administrator rights: the program in `~/.local/bin/takt-player`,
its version and uninstall script in `~/.local/share/takt`, and on Linux the menu entry `Takt` in
`~/.local/share/applications`. The version is `<version>+<short commit>`. A file you downloaded with a
browser on macOS carries a quarantine mark: remove it once with
`xattr -dr com.apple.quarantine takt-<platform>.tar.gz` (the program is unsigned).

## Remove

`~/.local/share/takt/uninstall.sh`, or `./uninstall.sh` from the unpacked archive. It removes only what
`install.sh` put there.
