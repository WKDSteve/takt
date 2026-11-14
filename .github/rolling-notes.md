The rolling build of the newest green commit of `main`, for looking at the state of the work. Not a release, unsigned, replaced after every green CI run. **This build plays no media files: it is built without libmpv and shows the test picture only.** The player does play files, with sound, when it is built from a checkout with libmpv (`TAKT_LIBMPV_DIR`, see [docs/BUILD.md](https://github.com/WKDSteve/takt/blob/main/docs/BUILD.md)); libmpv is not part of this zip (decided 2026-11-14).
For private machines only: not for a managed work machine.

The file is `takt-windows-x86_64.zip` (a zip with install scripts, not a setup.exe); get it from this page or with `gh release download latest -R WKDSteve/takt`. Check it against `SHA256SUMS` (`(Get-FileHash <zip> -Algorithm SHA256).Hash.ToLower()`), run `Unblock-File <zip>`, unpack it into a new, empty folder and run `install.ps1` from there:
`powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1` (add `-DesktopShortcut` for a link on the desktop).
The version shown in *Settings > Apps* is `<version>+<short commit>`. Details: the `README.md` inside the zip.
