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
- **Keine GPL-Builds von FFmpeg.** Ein mit `--enable-gpl` gebautes FFmpeg macht jede Binärdatei, die es lädt, endgültig zu reiner GPL. Der Smoke-Test `build_is_lgpl` prüft das für das separat gelinkte FFmpeg aus `FFMPEG_DIR` und scheitert bei einem GPL-Build. Die BtbN-Builds melden sich als „LGPL version 3 or later“ (gebaut mit `--enable-version3`): verträglich mit `GPL-3.0-or-later`, aber nicht mit GPL-2.0-only, und mit den eigenen Pflichten der LGPLv3.
- **libmpv ist derzeit ein GPL-Build mit eingebautem GPL-FFmpeg.** Siehe [Lizenz der vorgebauten libmpv](#lizenz-der-vorgebauten-libmpv): für den GPL-3-Player verträglich und am 2026-10-31 so entschieden (nur Zone 2, nie Zone 1); `build_is_lgpl` sieht die eingebaute FFmpeg nicht.
- **Keine Bibliothek im Repo.** DLLs, Importbibliotheken und Header liegen außerhalb des Repos (NREL-11), etwa unter `E:\deps\`. Im Repo stehen keine absoluten Pfade, nur die Variablennamen.

## Umgebungsvariablen

| Variable | Zeigt auf | Wirkung |
|---|---|---|
| `TAKT_LIBMPV_DIR` | absoluter Pfad zum Ordner mit der Importbibliothek (MSVC `mpv.lib`; windows-gnu `libmpv.dll.a` oder `mpv.lib`; Linux `libmpv.so`; macOS `libmpv.dylib`) | Suchpfad für den Linker, cfg `takt_libmpv` |
| `FFMPEG_DIR` | absoluter Pfad zur FFmpeg-Wurzel mit `include/` und `lib/` (MSVC `avutil.lib`; windows-gnu `libavutil.dll.a` oder `avutil.lib`; Linux `libavutil.so`; macOS `libavutil.dylib`) | Suchpfad `lib/` für den Linker (vor dem libmpv-Ordner, damit eine System-FFmpeg dort ihn nicht überschattet), cfg `takt_ffmpeg` |

`FFMPEG_DIR` ist bewusst derselbe Name und dasselbe Layout wie bei `ffmpeg-sys-next`, damit eine spätere FFmpeg-Anbindung dieselbe Einstellung liest. Der Name ist allgemein: Ist er auf dem Rechner schon für ein anderes Projekt gesetzt (etwa auf einen GPL-Build), läuft der takt-Smoke-Test damit und scheitert.

Ist eine Variable gesetzt, aber kein absoluter Pfad oder fehlt die Importbibliothek im Ordner, bricht der Build mit einer Meldung ab, statt still auf `NullBackend` zurückzufallen. Absolut muss der Pfad sein, weil das Build-Skript im Paketordner läuft, der Compiler aber im Workspace-Wurzelordner. Das Build-Skript liegt in [`apps/takt-player/build.rs`](../apps/takt-player/build.rs).

Bequem für Terminal und Editor zugleich: die Variablen in `~/.cargo/config.toml` unter `[env]` setzen; dann bauen beide mit denselben Werten und kompilieren nicht abwechselnd neu.

Zur Laufzeit muss das Betriebssystem die Bibliotheken finden: unter Windows liegen die DLLs im `PATH` oder neben der `.exe`, unter Linux im Bibliothekspfad des Systems oder, für Archive außerhalb davon, in `LD_LIBRARY_PATH`.

---

## Windows

Voraussetzungen: Rust mit der Standard-Toolchain `x86_64-pc-windows-msvc` (samt Visual-Studio-Build-Tools), 7-Zip und LLVM mit `llvm-readobj` und `llvm-dlltool`, am einfachsten `scoop install 7zip llvm` (der Installer von 7-zip.org trägt `7z` nicht in den `PATH` ein). Ohne LLVM geht es in einer Developer-PowerShell mit `dumpbin` und `lib` (siehe unten).

### libmpv

Quelle: die vorgebauten Archive `mpv-dev-x86_64-<Datum>-git-<Hash>.7z` aus [shinchiro/mpv-winbuild-cmake](https://github.com/shinchiro/mpv-winbuild-cmake/releases), die Variante **ohne** `-v3` im Namen (`-v3` setzt AVX2 voraus). Sie enthalten `libmpv-2.dll`, die Header unter `include/mpv/` und die GNU-Importbibliothek `libmpv.dll.a`, aber keine `mpv.lib` für den MSVC-Linker. Die erzeugt man aus den Exporten der DLL:

```powershell
7z x mpv-dev-x86_64-20261021-git-e471496c56.7z -oE:\deps\mpv-dev
cd E:\deps\mpv-dev

# Nur die mpv_*-Funktionen: die DLL exportiert zusätzlich Symbole statisch
# eingebundener Bibliotheken (libbluray, opus, sixel), die takt nie aufruft.
$exports = llvm-readobj --coff-exports libmpv-2.dll |
    Select-String '^\s*Name: (mpv_\S+)' | ForEach-Object { $_.Matches[0].Groups[1].Value }
Set-Content mpv.def -Value (@('LIBRARY libmpv-2.dll', 'EXPORTS') + $exports)
llvm-dlltool -m i386:x86-64 -d mpv.def -l mpv.lib
```

Ohne LLVM, in einer Developer-PowerShell im selben Ordner:

```powershell
$exports = dumpbin /exports libmpv-2.dll |
    Select-String '\s(mpv_\S+)$' | ForEach-Object { $_.Matches[0].Groups[1].Value }
Set-Content mpv.def -Value (@('LIBRARY libmpv-2.dll', 'EXPORTS') + $exports)
lib /def:mpv.def /machine:x64 /out:mpv.lib
```

#### Lizenz der vorgebauten libmpv

Die shinchiro-Builds sind **GPL-Builds**, in zweierlei Hinsicht:

- mpv selbst baut ohne `-Dgpl=false` als GPL (Standard) und bindet GPL-Teile wie rubberband und dvdnav ein. Nachprüfbar über die Eigenschaft `mpv-configuration` (Stand Build 20261021: kein `-Dgpl=false`).
- Die DLL enthält eine **statisch eingebundene FFmpeg** (Build 20261021: `N-127893-g9a861e968`) mit libx264, libx265 und postproc, also ein `--enable-gpl`-Build. Sie lädt keine `av*.dll`; diese FFmpeg ist weder austauschbar noch LGPL. `build_is_lgpl` sieht sie nicht, denn der Test prüft nur das separat gelinkte FFmpeg aus `FFMPEG_DIR`.

Was das bedeutet: `takt-player` steht ohnehin unter `GPL-3.0-or-later`, und mpv unter „GPLv2 or later“ ist damit verträglich. Die Zone-1-Crates linken libmpv nie und bleiben permissiv. Ausgeschlossen ist, eine Binärdatei mit dieser libmpv unter einer anderen Lizenz als GPL weiterzugeben. Technisch liegen außerdem zwei FFmpeg-Kopien verschiedener Version im Prozess, sobald takt selbst FFmpeg linkt; Frames lassen sich zwischen libmpv und einer eigenen FFmpeg-Schicht dann nicht direkt austauschen. Eine LGPL-libmpv gibt es nicht vorgebaut; sie bräuchte einen eigenen Build mit `-Dgpl=false` gegen eine LGPL-FFmpeg (ohne rubberband, dvdnav, x264 und weitere GPL-Teile). **Entschieden am 2026-10-31** (`takt/entscheidung-libmpv-lizenz-build` im Vault): der vorgebaute GPL-Build bleibt, GPL-libmpv nur in Zone 2, nie in Zone 1. Der Smoke-Test `license_state_matches_build_md` liest `mpv-configuration` und schlägt fehl, sobald ein LGPL-Build (`-Dgpl=false`) auftaucht, damit dieser Abschnitt dann nachgezogen wird.

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
cargo test -p takt-player --test ffi_native_link
```

Erwartet: `libmpv::client_api_is_version_2`, `libmpv::license_state_matches_build_md` und `ffmpeg::build_is_lgpl` laufen grün. Ohne die beiden Variablen läuft derselbe Befehl mit null Tests, so wie in der CI.

Dauerhaft gehören **drei** Einstellungen dazu: die beiden Variablen und die beiden DLL-Ordner im `PATH`. Fehlt der `PATH`-Teil, bricht jedes `cargo test --workspace` mit `STATUS_DLL_NOT_FOUND` (`0xc0000135`) ab. Die beiden Variablen:

```powershell
[Environment]::SetEnvironmentVariable('TAKT_LIBMPV_DIR', 'E:\deps\mpv-dev', 'User')
[Environment]::SetEnvironmentVariable('FFMPEG_DIR', 'E:\deps\ffmpeg-n9.0-latest-win64-lgpl-shared-9.0', 'User')
```

Die beiden `PATH`-Einträge von Hand in den Systemeinstellungen („Umgebungsvariablen bearbeiten“, Benutzer-`Path`) eintragen, vor anderen Einträgen mit libmpv oder FFmpeg. Windows setzt den `PATH` aus System-`Path` und danach Benutzer-`Path` zusammen; liegt schon im System-`Path` eine andere `libmpv-2.dll` oder `avutil-*.dll`, gehören die Einträge in den System-`Path`. Nicht per Skript: `setx` kürzt auf 1024 Zeichen, und `[Environment]::SetEnvironmentVariable('Path', …)` friert Einträge wie `%USERPROFILE%\…` als feste Pfade ein. Danach Terminal bzw. Editor neu starten.

---

## Linux

Die Distributionen liefern libmpv als Paket mit Entwicklungsdateien.

```bash
# Arch
sudo pacman -S mpv pkgconf
# Debian/Ubuntu
sudo apt install libmpv-dev pkg-config
```

```bash
# Eine Kette: scheitert pkg-config oder liefert nichts, läuft cargo nicht.
# Leer heißt für build.rs „nicht gesetzt“, also stumm null Tests.
TAKT_LIBMPV_DIR="$(pkg-config --variable=libdir mpv)" && [ -n "$TAKT_LIBMPV_DIR" ] \
    && export TAKT_LIBMPV_DIR && cargo test -p takt-player --test ffi_native_link
```

Erwartet: genau zwei Tests, `libmpv::client_api_is_version_2` und `libmpv::license_state_matches_build_md`, grün (Distributions-libmpv ist ebenfalls ein GPL-Build).

`FFMPEG_DIR` bleibt unter Linux zunächst ungesetzt. Arch, Debian/Ubuntu und Fedora (RPM Fusion) bauen FFmpeg mit `--enable-gpl`; mit der Distributions-FFmpeg würde `build_is_lgpl` also immer scheitern, und die Distributions-libmpv lädt ohnehin diese GPL-FFmpeg. Wer das separat gelinkte FFmpeg prüfen will, nimmt das Archiv `linux64-lgpl-shared` von BtbN und exportiert `FFMPEG_DIR` (dessen Wurzel) und `LD_LIBRARY_PATH="$FFMPEG_DIR/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"`. Dann liegen zwei FFmpeg-Versionen im Prozess; haben beide dieselbe SONAME-Hauptversion, lädt sogar die Distributions-libmpv die BtbN-Bibliotheken (ABI-Mischung). Das taugt zum Prüfen, nicht zum Alltag.

Noch nicht auf echter Hardware geprüft (siehe Plattform-Tabelle im README).

## macOS

Ungetestet, es gibt kein Testgerät. Der naheliegende Weg ist Homebrew (`brew install mpv`, dann `export TAKT_LIBMPV_DIR="$(brew --prefix mpv)/lib"`). `FFMPEG_DIR` wie unter Linux ungesetzt lassen: Homebrew-FFmpeg ist ein GPL-Build.

---

## Warum nicht vcpkg

`docs/PHASE-0.md` nannte vcpkg als ersten Weg. Er funktioniert, baut libmpv aber samt aller Abhängigkeiten aus dem Quelltext (lange Laufzeit, eigener Werkzeugstapel) und ist auf einer frischen Maschine aufwendiger als zwei Archive. Die vorgebauten Archive sind deshalb der dokumentierte Weg; vcpkg bleibt die Ausweichlösung, falls die Archive einmal ausfallen.
