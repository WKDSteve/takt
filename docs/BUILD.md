# Bauen

Takt baut ohne Medien-Bibliothek: Ohne libmpv und FFmpeg läuft `takt-player` über `NullBackend`, der Demux-Hilfsprozess `takt-demux-helper` meldet zu jeder Datei „ohne FFmpeg gebaut“, und genau so baut die CI. Systemvoraussetzung ist unter Linux nur die Audio-Ausgabe (`cpal` über ALSA, die Echtzeit-Priorität über rtkit und D-Bus): `sudo apt-get install libasound2-dev libdbus-1-dev` (Fedora: `alsa-lib-devel dbus-devel`); Windows und macOS brauchen nichts. Die nativen Bibliotheken werden nur eingebunden, wenn eine Umgebungsvariable auf sie zeigt. Dieses Dokument beschreibt, wie man sie beschafft.

## Table of content

  - [Grundregeln](#grundregeln)
  - [Umgebungsvariablen](#umgebungsvariablen)
  - [Windows](#windows)
    - [libmpv](#libmpv)
      - [Lizenz der vorgebauten libmpv](#lizenz-der-vorgebauten-libmpv)
    - [FFmpeg](#ffmpeg)
    - [takt-demux mit FFmpeg](#takt-demux-mit-ffmpeg)
    - [Prüfen](#prüfen)
  - [Linux](#linux)
    - [Das Fenster](#das-fenster)
    - [Priorität des Audio-Threads](#priorität-des-audio-threads)
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
| `FFMPEG_DIR` | absoluter Pfad zur FFmpeg-Wurzel mit `include/` und `lib/` (MSVC `avutil.lib`; windows-gnu `libavutil.dll.a` oder `avutil.lib`; Linux `libavutil.so`; macOS `libavutil.dylib`) | Suchpfad `lib/` für den Linker (vor dem libmpv-Ordner, damit eine System-FFmpeg dort ihn nicht überschattet), cfg `takt_ffmpeg`; in `takt-player` nur für den Link-Test, in `takt-demux` linkt damit der Hilfsprozess `takt-demux-helper` libavformat, libavcodec und libavutil (dort muss es **FFmpeg 9.0** sein, siehe [takt-demux mit FFmpeg](#takt-demux-mit-ffmpeg)) |

`FFMPEG_DIR` ist bewusst derselbe Name und dasselbe Layout wie bei `ffmpeg-sys-next`; `takt-demux` liest dieselbe Einstellung (mit eigenen, handgeschriebenen Bindings, nicht mit `ffmpeg-sys-next`). Der Name ist allgemein: Ist er auf dem Rechner schon für ein anderes Projekt gesetzt (etwa auf einen GPL-Build), läuft der takt-Smoke-Test damit und scheitert.

Ist eine Variable gesetzt, aber kein absoluter Pfad oder fehlt die Importbibliothek im Ordner, bricht der Build mit einer Meldung ab, statt still auf `NullBackend` zurückzufallen. Absolut muss der Pfad sein, weil das Build-Skript im Paketordner läuft, der Compiler aber im Workspace-Wurzelordner. Die Build-Skripte liegen in [`apps/takt-player/build.rs`](../apps/takt-player/build.rs) und [`crates/takt-demux/build.rs`](../crates/takt-demux/build.rs).

Bequem für Terminal und Editor zugleich: die Variablen in `~/.cargo/config.toml` unter `[env]` setzen; dann bauen beide mit denselben Werten und kompilieren nicht abwechselnd neu.

Mit `TAKT_LIBMPV_DIR` gebaut, spielt der Player Dateien über libmpv (`MpvBackend`): `cargo run --release -p takt-player -- film.mkv`, oder eine Datei auf das Fenster ziehen. Ohne die Variable baut derselbe Befehl den Player mit dem Testbild allein; so baut ihn die CI auf allen drei Plattformen, und so ist der Rolling-Build `latest` gebaut. Die Tests des Backends spielen die erzeugten Dateien unter `apps/takt-player/tests/data` (`pattern-3s.mkv` aus `scripts/make-test-clip.py`, dazu `make-long-gop-clip.py`, `make-colour-clips.py`, `make-format-clips.py` und `make-one-frame-clip.py`; nichts davon stammt aus einem Film), stumm; sie laufen nur in einem Build mit libmpv (in der CI: die Jobs „libmpv gelinkt (Ubuntu, apt)“ und „libmpv gelinkt (Windows, Archiv gepinnt)“). Was nur das echte Fenster zeigt, prüfen Test-Sheets von testing.nvim (`TESTS/sheets`, `.testing.lua`): sie bedienen das Fenster über die Namen, die auch ein Screenreader hört (`sheet-driver` aus appkit), und legen Bilder für ein Urteil ab; `testing sheets run --all` auf dem privaten Rechner, die Datei-Sheets mit gesetztem `TAKT_LIBMPV_DIR` und ffmpeg im `PATH` (sie spielen einen Clip von einer Minute, den `scripts/sheet-prepare.py` je Lauf erzeugt; `scripts/build-sheet-app.py` baut für die Sheets eigene Builds nach `target/sheets-mpv` und `target/sheets-plain`). Sie laufen nicht in der CI.

Die Tests von `takt-gpu`, die auf einem Grafikadapter rendern (Referenzbilder, Compositing, unter Windows der Import einer fremden Textur), sind `#[ignore]` und brauchen keine Bibliothek: `TAKT_GPU_TESTS=1 cargo test -p takt-gpu gpu -- --ignored`, mit `TAKT_GPU_ADAPTER=fallback` auf dem Software-Adapter (WARP unter Windows; so rendert sie die CI). Mehr in [GPU.md](GPU.md).

Zur Laufzeit muss das Betriebssystem die Bibliotheken finden: unter Windows liegen die DLLs im `PATH` oder neben der `.exe` (`scripts\build-install.ps1` legt `libmpv-2.dll` aus `TAKT_LIBMPV_DIR` neben die installierte `takt-player.exe`; für `cargo run` und `cargo test` bleibt der `PATH` nötig), unter Linux im Bibliothekspfad des Systems oder, für Archive außerhalb davon, in `LD_LIBRARY_PATH`.

---

## Windows

Voraussetzungen: Rust mit der Standard-Toolchain `x86_64-pc-windows-msvc` (samt Visual-Studio-Build-Tools), 7-Zip und LLVM mit `llvm-readobj` und `llvm-dlltool`, am einfachsten `scoop install 7zip llvm` (der Installer von 7-zip.org trägt `7z` nicht in den `PATH` ein). Ohne LLVM geht es in einer Developer-PowerShell mit `dumpbin` und `lib` (siehe unten).

Der Release-Build von `takt-player.exe` ist ein Fenster-Programm: aus dem Startmenü gestartet, öffnet er kein Konsolenfenster neben dem Player. Aus einem Terminal gestartet, schreibt er weiter dorthin (`--keys`, die Zeilen von `--headless`, Fehler), weil er sich an die Konsole des Terminals hängt; eine Umleitung in eine Datei oder Pipe geht wie gewohnt. Zwei Folgen für die Arbeit im Terminal: PowerShell und `cmd` warten nicht auf ein Fenster-Programm, der Prompt kommt also sofort zurück und die Ausgabe eines `--headless`-Laufs erscheint danach (wer warten will: `Start-Process -Wait -NoNewWindow` oder die Ausgabe in eine Pipe leiten, wie `scripts/ab-measure.py` es tut); und wer das Terminal schließt, beendet den Player, den er daraus gestartet hat. Ein Debug-Build (`cargo run`) bleibt ein Konsolen-Programm. Der Test `window_program` und `scripts/test-release.ps1` lesen das am Kopf der `.exe` nach. Endet ein Start ohne Fenster (kein Grafikgerät) oder mit einer Panik, steht die Meldung außerdem in `%TEMP%\takt-player.log` (eine Zeile je Fehler, mit Zeit und Version), und liest niemand stderr (keine Konsole, keine Umleitung in eine Datei oder Pipe), zeigt Windows sie in einem Meldungsfenster.

### libmpv

Quelle: die vorgebauten Archive `mpv-dev-x86_64-<Datum>-git-<Hash>.7z` aus [shinchiro/mpv-winbuild-cmake](https://github.com/shinchiro/mpv-winbuild-cmake/releases), die Variante **ohne** `-v3` im Namen (`-v3` setzt AVX2 voraus). Sie enthalten `libmpv-2.dll`, die Header unter `include/mpv/` und die GNU-Importbibliothek `libmpv.dll.a`, aber keine `mpv.lib` für den MSVC-Linker. Die erzeugt man aus den Exporten der DLL:

```powershell
7z x mpv-dev-x86_64-20261111-git-bcde6b93cc.7z -oE:\deps\mpv-dev
cd E:\deps\mpv-dev

# Nur die mpv_*-Funktionen: die DLL exportiert zusätzlich Symbole statisch
# eingebundener Bibliotheken (libbluray, opus, sixel), die takt nie aufruft.
$exports = llvm-readobj --coff-exports libmpv-2.dll |
    Select-String '^\s*Name: (mpv_\S+)' | ForEach-Object { $_.Matches[0].Groups[1].Value }
Set-Content mpv.def -Value (@('LIBRARY libmpv-2.dll', 'EXPORTS') + $exports)
llvm-dlltool -m i386:x86-64 -d mpv.def -l mpv.lib
```

Ohne LLVM, in einer Developer-PowerShell im selben Ordner (so macht es die CI im Job „libmpv gelinkt (Windows, Archiv gepinnt)“, die dasselbe Archiv lädt und nur mit dem dort gepinnten SHA-256 annimmt; shinchiro hält die letzten 30 Veröffentlichungen vor, ist diese fort, meldet der Job SKIP und es wird hier und in `ci.yml` ein neueres Archiv eingetragen):

```powershell
$exports = dumpbin /exports libmpv-2.dll |
    Select-String '\s(mpv_\S+)$' | ForEach-Object { $_.Matches[0].Groups[1].Value }
Set-Content mpv.def -Value (@('LIBRARY libmpv-2.dll', 'EXPORTS') + $exports)
lib /def:mpv.def /machine:x64 /out:mpv.lib
```

#### Lizenz der vorgebauten libmpv

Die shinchiro-Builds sind **GPL-Builds**, in zweierlei Hinsicht:

- mpv selbst baut ohne `-Dgpl=false` als GPL (Standard) und bindet GPL-Teile wie rubberband und dvdnav ein. Nachprüfbar über die Eigenschaft `mpv-configuration` (Stand Build 20261111: kein `-Dgpl=false`).
- Die DLL enthält eine **statisch eingebundene FFmpeg** (Build 20261111: `N-128766-gcd4e96eb3`) mit libx264, libx265 und postproc, also ein `--enable-gpl`-Build. Sie lädt keine `av*.dll`; diese FFmpeg ist weder austauschbar noch LGPL. `build_is_lgpl` sieht sie nicht, denn der Test prüft nur das separat gelinkte FFmpeg aus `FFMPEG_DIR`.

Was das bedeutet: `takt-player` steht ohnehin unter `GPL-3.0-or-later`, und mpv unter „GPLv2 or later“ ist damit verträglich. Die Zone-1-Crates linken libmpv nie und bleiben permissiv. Ausgeschlossen ist, eine Binärdatei mit dieser libmpv unter einer anderen Lizenz als GPL weiterzugeben. Technisch liegen außerdem zwei FFmpeg-Kopien verschiedener Version im Prozess, sobald takt selbst FFmpeg linkt; Frames lassen sich zwischen libmpv und einer eigenen FFmpeg-Schicht dann nicht direkt austauschen. Eine LGPL-libmpv gibt es nicht vorgebaut; sie bräuchte einen eigenen Build mit `-Dgpl=false` gegen eine LGPL-FFmpeg (ohne rubberband, dvdnav, x264 und weitere GPL-Teile). **Entschieden am 2026-10-31** (`takt/entscheidung-libmpv-lizenz-build` im Vault): der vorgebaute GPL-Build bleibt, GPL-libmpv nur in Zone 2, nie in Zone 1. Der Smoke-Test `license_state_matches_build_md` liest `mpv-configuration` und schlägt fehl, sobald ein LGPL-Build (`-Dgpl=false`) auftaucht, damit dieser Abschnitt dann nachgezogen wird. Er sieht nur die meson-Option auf der Kommandozeile (Groß-/Kleinschreibung egal): ein LGPL-Build, der `gpl=false` über eine Native- oder Cross-Datei setzt, oder ein Tausch nur der eingebauten FFmpeg fällt ihm nicht auf. Beim Aktualisieren der DLL deshalb diesen Abschnitt prüfen (`mpv-version`, `ffmpeg-version`, `mpv-configuration`). Ein robusteres Signal wäre die Log-Zeile „List of enabled features: … gpl …“, die libmpv beim Initialisieren auf Log-Level `v` ausgibt; sie braucht im Test allerdings eine kleine Ereignisschleife (`mpv_request_log_messages`, `mpv_wait_event`).

### FFmpeg

Quelle: die `win64-lgpl-shared`-Archive aus [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases), etwa `ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip`. Sie bringen `bin/*.dll`, `include/` und unter `lib/` bereits MSVC-Importbibliotheken (`avutil.lib` usw.) mit; es ist nichts zu erzeugen. Wichtig ist `lgpl` im Namen, nicht `gpl`.

```powershell
Expand-Archive ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip -DestinationPath E:\deps
```

Die gängigen Windows-Pakete (winget `Gyan.FFmpeg`, scoop `ffmpeg`) liefern nur Programme ohne Entwicklungsdateien und sind GPL-Builds; sie taugen zum Abspielen, nicht zum Linken.

### takt-demux mit FFmpeg

`takt-demux` (der Demuxer der eigenen Pipeline, Phase 1) parst Container nie im Hauptprozess: `takt_demux::Demuxer` startet je Datei den Hilfsprozess `takt-demux-helper`, und nur der linkt FFmpeg ([PROCESSES.md](PROCESSES.md)). Ohne `FFMPEG_DIR` bauen Crate und Hilfsprozess trotzdem; der Hilfsprozess beantwortet dann jede Datei mit „built without FFmpeg“, und die Tests, die Dateien lesen, melden `SKIP`.

Mit `FFMPEG_DIR` muss es **FFmpeg 9.0** sein (libavformat 63, libavcodec 63, libavutil 61): Die Bindings in `crates/takt-demux/src/bin/takt-demux-helper/ffi.rs` sind von Hand geschrieben und lesen einige Felder von FFmpeg-Strukturen direkt, weil libavformat dafür keine Zugriffsfunktionen hat. Das Build-Skript liest die Hauptversionen aus den Headern und bricht bei einer anderen mit einer Meldung ab; der Hilfsprozess prüft die geladenen Bibliotheken beim Start noch einmal. Wie die Offsets bei einer neuen Hauptversion nachgemessen werden, steht in [`crates/takt-demux/layout/README.md`](../crates/takt-demux/layout/README.md).

```powershell
$env:FFMPEG_DIR = 'E:\deps\ffmpeg-n9.0-latest-win64-lgpl-shared-9.0'
$env:PATH = "$env:FFMPEG_DIR\bin;$env:PATH"
cargo test -p takt-demux
cargo run -p takt-demux --bin takt-demux-helper -- --about
```

Erwartet: die Tests `with_ffmpeg::*` laufen grün (MKV, MP4 und WebM Paket für Paket gegen `ffprobe` aus `FFMPEG_DIR\bin`, Sprünge, abgeschnittene und beschädigte Dateien, `the_ffmpeg_of_the_helper_is_an_lgpl_build`), und `--about` nennt `libavformat 63.x` und „LGPL version 3 or later“. Die DLLs müssen zur Laufzeit gefunden werden, wie bei libmpv: im `PATH` oder neben `takt-demux-helper.exe`.

Unter Linux dasselbe mit dem Archiv `linux64-lgpl-shared` von BtbN:

```bash
export FFMPEG_DIR="$HOME/deps/ffmpeg-n9.0-latest-linux64-lgpl-shared-9.0"
export LD_LIBRARY_PATH="$FFMPEG_DIR/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" PATH="$FFMPEG_DIR/bin:$PATH"
cargo test -p takt-demux
```

Mit der FFmpeg einer Distribution (`FFMPEG_DIR=/usr` unter Arch, sofern sie 9.0 ist) laufen die Demux-Tests ebenfalls, nur `the_ffmpeg_of_the_helper_is_an_lgpl_build` scheitert dann mit Absicht: Distributionen bauen mit `--enable-gpl`. In der CI läuft das im Job „takt-demux mit FFmpeg 9.0“ unter Ubuntu und Windows, mit dem Monats-Build `autobuild-2026-10-31-12-59` von BtbN (`ffmpeg-n9.0.2-39-g7adfbe40dc-…-lgpl-shared-9.0`), der nur mit seinem dort gepinnten SHA-256 angenommen wird. BtbN hält die Monats-Builds rund zwei Jahre vor; ist das Archiv fort, meldet der Job `SKIP`, und es wird dort und hier ein neueres eingetragen. macOS hat keinen Job mit FFmpeg (kein vorgebautes LGPL-Archiv, kein Testgerät).

Den Protokoll-Parser der Leseseite prüft außerdem ein Fuzz-Ziel: [`fuzz/README.md`](../fuzz/README.md).

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

Die Distributionen liefern libmpv als Paket mit Entwicklungsdateien. Das Rezept unten läuft wörtlich in der CI im Job „libmpv gelinkt (Ubuntu, apt)“ und nennt dort Version und `mpv-configuration` der libmpv; die FFmpeg von Ubuntu ist ein GPL-Build und bleibt dort draußen. Ohne gesetzte Variable melden die Link-Tests `SKIP`, und jeder Testlauf der CI nennt die Zahl der übersprungenen Tests in seiner Zusammenfassung (`scripts/ci-skip-summary.py`).

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

Geprüft auf Ubuntu (Runner `ubuntu-latest` der CI, libmpv aus `apt`); noch nicht auf eigener Hardware und nicht unter Arch (siehe Plattform-Tabelle im README).

### Das Fenster

Zur Laufzeit lädt das Fenster (winit) `libxkbcommon`, unter X11 dazu `libxkbcommon-x11`. Jeder Desktop bringt beide mit; auf einem System ohne sie (ein Container, ein WSL ohne Desktop-Pakete) endet `takt-player` beim Start mit einer Panik aus winit (`XKBNotFound` bzw. „Library libxkbcommon-x11.so could not be loaded“), und `--headless` läuft weiter. Nachinstallieren: `sudo apt install libxkbcommon0 libxkbcommon-x11-0` (Arch: `libxkbcommon libxkbcommon-x11`).

Ob Tasten im echten Fenster ankommen, prüft `scripts/check-window-keys.py <takt-player>`: Leertaste hält an und spielt weiter, Escape beendet mit Exit 0. Unter Windows liest es den Zustand über `sheet-driver` (appkit), unter Linux braucht es X11 (`Xvfb :99` genügt, dazu `libxcb-xtest0` und für die Darstellung ohne Grafikkarte `mesa-vulkan-drivers`). Gelaufen am 2026-11-17 unter Windows 11 und in einem Container mit Ubuntu 24.04 unter Xvfb.

### Priorität des Audio-Threads

`takt-player` lässt `cpal` den Audio-Thread auf Echtzeit-Priorität (`SCHED_FIFO`) heben. Unter Linux fragt es dafür zuerst rtkit über D-Bus (Feature `realtime-dbus` von `cpal`, seit dem 2026-11-15): auf einem Desktop mit rtkit (die meisten mit PipeWire oder PulseAudio) geht das ohne Einrichtung. Zum Bauen braucht es die Header von D-Bus (`sudo apt install libdbus-1-dev`, Arch: `dbus`). Ohne rtkit braucht ein normaler Benutzer eine rtprio-Grenze; ohne beides läuft der Ton mit normaler Priorität weiter, und der Player meldet am Ende `Prioritaet refused: …`. Einrichtung der Grenze, einmal, danach ab- und wieder anmelden:

```bash
# Gruppe audio (Debian/Ubuntu: gibt es schon; Arch: mit realtime-privileges die Gruppe realtime)
sudo usermod -aG audio "$USER"
printf '@audio - rtprio 95\n@audio - memlock unlimited\n' | sudo tee /etc/security/limits.d/95-audio.conf
```

Prüfen: `ulimit -r` zeigt danach `95`. Ohne Anmeldung über PAM (etwa in einem Container) greift `limits.d` nicht.

Unter Windows fordert `cpal` über `audio_thread_priority` die MMCSS-Klasse „Audio“ an (nicht „Pro Audio“), ohne Einrichtung, und nimmt bei einer Ablehnung `THREAD_PRIORITY_TIME_CRITICAL`; unter macOS ist der CoreAudio-Thread schon ein Echtzeit-Thread.

## macOS

Ungetestet, es gibt kein Testgerät. Der naheliegende Weg ist Homebrew (`brew install mpv`, dann `export TAKT_LIBMPV_DIR="$(brew --prefix mpv)/lib"`). `FFMPEG_DIR` wie unter Linux ungesetzt lassen: Homebrew-FFmpeg ist ein GPL-Build.

---

## Warum nicht vcpkg

`docs/PHASE-0.md` nannte vcpkg als ersten Weg. Er funktioniert, baut libmpv aber samt aller Abhängigkeiten aus dem Quelltext (lange Laufzeit, eigener Werkzeugstapel) und ist auf einer frischen Maschine aufwendiger als zwei Archive. Die vorgebauten Archive sind deshalb der dokumentierte Weg; vcpkg bleibt die Ausweichlösung, falls die Archive einmal ausfallen.
