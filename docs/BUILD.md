# Bauen

Takt baut ohne jede Systembibliothek: Ohne libmpv und FFmpeg läuft `takt-player` über `NullBackend`, und genau so baut die CI. Die nativen Bibliotheken werden nur eingebunden, wenn eine Umgebungsvariable auf sie zeigt. Dieses Dokument beschreibt, wie man sie beschafft.

## Table of content

  - [Grundregeln](#grundregeln)
  - [Umgebungsvariablen](#umgebungsvariablen)
  - [Windows](#windows)
    - [libmpv](#libmpv)
      - [Lizenz der vorgebauten libmpv](#lizenz-der-vorgebauten-libmpv)
    - [FFmpeg](#ffmpeg)
    - [Prüfen](#prüfen)
  - [Linux](#linux)
  - [macOS](#macos)
  - [Warum nicht vcpkg](#warum-nicht-vcpkg)

---

## Grundregeln

- **Dynamisch linken.** libmpv und FFmpeg stehen unter der LGPL. Bei statischem Linken greift deren Relinking-Pflicht; dynamisch gelinkt bleibt die Bibliothek austauschbar.
- **Keine GPL-Builds von FFmpeg.** Ein mit `--enable-gpl` gebautes FFmpeg macht jede Binärdatei, die es lädt, endgültig zu reiner GPL. Der Smoke-Test `build_is_lgpl` prüft das und scheitert bei einem GPL-Build.
- **libmpv ist derzeit ein GPL-Build.** Siehe [Lizenz der vorgebauten libmpv](#lizenz-der-vorgebauten-libmpv): für den GPL-3-Player verträglich, aber eine offene Abweichung vom Konzept.
- **Keine Bibliothek im Repo.** DLLs, Importbibliotheken und Header liegen außerhalb des Repos (NREL-11), etwa unter `E:\deps\`. Im Repo stehen keine absoluten Pfade, nur die Variablennamen.

## Umgebungsvariablen

| Variable | Zeigt auf | Wirkung |
|---|---|---|
| `TAKT_LIBMPV_DIR` | Ordner mit der Importbibliothek (`mpv.lib` bzw. `libmpv.so`) | Suchpfad für den Linker, cfg `takt_libmpv` |
| `FFMPEG_DIR` | FFmpeg-Wurzel mit `include/` und `lib/` | Suchpfad `lib/` für den Linker, cfg `takt_ffmpeg` |

`FFMPEG_DIR` ist bewusst derselbe Name und dasselbe Layout wie bei `ffmpeg-sys-next`, damit eine spätere FFmpeg-Anbindung dieselbe Einstellung liest.

Ist eine Variable gesetzt, zeigt aber nicht auf einen Ordner, bricht der Build ab, statt still auf `NullBackend` zurückzufallen. Das Build-Skript liegt in [`apps/takt-player/build.rs`](../apps/takt-player/build.rs).

Zur Laufzeit muss das Betriebssystem die Bibliotheken finden: unter Windows liegen die DLLs im `PATH` oder neben der `.exe`, unter Linux im Bibliothekspfad des Systems.

---

## Windows

Voraussetzungen: Rust mit MSVC-Toolchain (`rust-toolchain.toml`), [7-Zip](https://www.7-zip.org/) und `llvm-dlltool` (etwa `scoop install llvm`). Statt `llvm-dlltool` geht auch `lib.exe` aus den Visual-Studio-Build-Tools in einer Developer-PowerShell.

### libmpv

Quelle: die vorgebauten Archive `mpv-dev-x86_64-<Datum>-git-<Hash>.7z` aus [shinchiro/mpv-winbuild-cmake](https://github.com/shinchiro/mpv-winbuild-cmake/releases). Sie enthalten `libmpv-2.dll`, die Header unter `include/mpv/` und die GNU-Importbibliothek `libmpv.dll.a`, aber keine `mpv.lib` für den MSVC-Linker. Die erzeugt man aus den Exporten der DLL:

```powershell
7z x mpv-dev-x86_64-*.7z -oE:\deps\mpv-dev
cd E:\deps\mpv-dev

# Nur die mpv_*-Funktionen: die DLL exportiert zusätzlich Symbole statisch
# eingebundener Bibliotheken (libbluray, opus, sixel), die takt nie aufruft.
$exports = llvm-readobj --coff-exports libmpv-2.dll |
    Select-String '^\s*Name: (mpv_\S+)' | ForEach-Object { $_.Matches[0].Groups[1].Value }
Set-Content mpv.def -Value (@('LIBRARY libmpv-2.dll', 'EXPORTS') + $exports)
llvm-dlltool -m i386:x86-64 -d mpv.def -l mpv.lib
# Alternativ in einer Developer-PowerShell: lib /def:mpv.def /machine:x64 /out:mpv.lib
```

#### Lizenz der vorgebauten libmpv

Die shinchiro-Builds sind **GPL-Builds**: mpv baut ohne `-Dgpl=false` als GPL (Standard), und die Archive binden GPL-Teile wie rubberband und dvdnav ein. Nachprüfbar über die Eigenschaft `mpv-configuration` (Stand Build 20261021: kein `-Dgpl=false`).

Was das bedeutet: `takt-player` steht ohnehin unter `GPL-3.0-or-later`, und mpv unter „GPLv2 or later“ ist damit verträglich. Die Zone-1-Crates linken libmpv nie und bleiben permissiv. Ausgeschlossen ist damit nur, eine Binärdatei mit dieser libmpv unter einer anderen Lizenz als GPL weiterzugeben. Eine LGPL-libmpv gibt es nicht vorgebaut; sie bräuchte einen eigenen Build mit `-Dgpl=false` (ohne rubberband, dvdnav und weitere GPL-Teile). Offen als Entscheidung `takt/entscheidung-libmpv-lizenz-build` im Vault.

### FFmpeg

Quelle: die `win64-lgpl-shared`-Archive aus [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases), etwa `ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip`. Sie bringen `bin/*.dll`, `include/` und unter `lib/` bereits MSVC-Importbibliotheken (`avutil.lib` usw.) mit; es ist nichts zu erzeugen. Wichtig ist `lgpl` im Namen, nicht `gpl`.

```powershell
Expand-Archive ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip -DestinationPath E:\deps
```

Die gängigen Windows-Pakete (winget `Gyan.FFmpeg`, scoop `ffmpeg`) liefern nur Programme ohne Entwicklungsdateien und sind GPL-Builds; sie taugen zum Abspielen, nicht zum Linken.

### Prüfen

```powershell
$env:TAKT_LIBMPV_DIR = 'E:\deps\mpv-dev'
$env:FFMPEG_DIR = 'E:\deps\ffmpeg-n9.0-latest-win64-lgpl-shared-9.0'
$env:PATH = "$env:TAKT_LIBMPV_DIR;$env:FFMPEG_DIR\bin;$env:PATH"
cargo test -p takt-player --test native_link
```

Erwartet: `libmpv::client_api_is_version_2` und `ffmpeg::build_is_lgpl` laufen grün. Ohne die beiden Variablen läuft derselbe Befehl mit null Tests, so wie in der CI. Dauerhaft setzt man die Variablen mit `setx` oder in den Systemeinstellungen; danach Terminal bzw. Editor neu starten.

---

## Linux

Die Distributionen liefern libmpv und FFmpeg als Pakete mit Entwicklungsdateien.

```bash
# Arch
sudo pacman -S mpv ffmpeg pkgconf
# Debian/Ubuntu
sudo apt install libmpv-dev libavutil-dev libavformat-dev libavcodec-dev pkg-config
```

```bash
export TAKT_LIBMPV_DIR="$(pkg-config --variable=libdir mpv)"
export FFMPEG_DIR=/usr
cargo test -p takt-player --test native_link
```

Distributions-FFmpeg ist häufig mit `--enable-gpl` gebaut; dann schlägt `build_is_lgpl` fehl. Für Entwicklung und eigene Tests ist das unerheblich, für eine weitergegebene Binärdatei nicht.

Noch nicht auf echter Hardware geprüft (siehe Plattform-Tabelle im README).

## macOS

Ungetestet, es gibt kein Testgerät. Der naheliegende Weg ist Homebrew (`brew install mpv ffmpeg`, dann `TAKT_LIBMPV_DIR="$(brew --prefix)/lib"` und `FFMPEG_DIR="$(brew --prefix ffmpeg)"`); Homebrew-FFmpeg ist ein GPL-Build.

---

## Warum nicht vcpkg

`docs/PHASE-0.md` nannte vcpkg als ersten Weg. Er funktioniert, baut libmpv aber samt aller Abhängigkeiten aus dem Quelltext (lange Laufzeit, eigener Werkzeugstapel) und ist auf einer frischen Maschine aufwendiger als zwei Archive. Die vorgebauten Archive sind deshalb der dokumentierte Weg; vcpkg bleibt die Ausweichlösung, falls die Archive einmal ausfallen.
