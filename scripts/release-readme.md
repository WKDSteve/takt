# Takt (rolling build)

The newest green commit of `main`, for looking at the state of the work: not a release, not signed,
replaced after every green CI run. The player is in Phase 0: a window with the moving test picture of
the NullBackend, the diagnostic overlay (F3) and the sound path; it opens no media files yet.

For private machines only: not for a managed work machine.

## Install

1. Check the zip against `SHA256SUMS`: `(Get-FileHash takt-windows-x86_64.zip -Algorithm SHA256).Hash.ToLower()`.
2. `Unblock-File takt-windows-x86_64.zip`, then unpack it into a new, empty folder.
3. In that folder: `powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1`
   (add `-DesktopShortcut` for a link on the desktop).

It installs for the current user into `%LOCALAPPDATA%\takt\bin`, makes the Start menu entry
"Takt" and an entry in *Settings > Apps*, whose version is `<version>+<short commit>`. No
administrator rights. Windows (SmartScreen) may ask before the first start: the program is unsigned.

## Remove

*Settings > Apps > Takt > Uninstall*, or `%LOCALAPPDATA%\takt\bin\uninstall.ps1`.

## Keys

Space play/pause, arrows jump 5 s, Home to the start, a click on the timeline jumps there, 9/0
quieter/louder, m mute, > and < next and previous entry, F8 the playlist, ? all keys, F3
diagnostics, F4 save them, Escape quits. Drop files or folders on the window to play them (Shift
appends); until the media backend comes, every entry plays as the test picture.

License: GPL-3.0-or-later (`LICENSE`). Source: <https://github.com/WKDSteve/takt>.
