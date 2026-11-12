The rolling build of the newest green commit of `main`, for looking at the state of the work. Not a release, unsigned, replaced after every green CI run. Phase 0: the player shows its test picture, it opens no media files yet.
For private machines only: not for a managed work machine.

The file is `takt-windows-x86_64.zip` (a zip with install scripts, not a setup.exe); get it from this page or with `gh release download latest -R WKDSteve/takt`. Check it against `SHA256SUMS` (`(Get-FileHash <zip> -Algorithm SHA256).Hash.ToLower()`), run `Unblock-File <zip>`, unpack it into a new, empty folder and run `install.ps1` from there:
`powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1` (add `-DesktopShortcut` for a link on the desktop).
The version shown in *Settings > Apps* is `<version>+<short commit>`. Details: the `README.md` inside the zip.
