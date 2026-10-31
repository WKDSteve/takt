# Takt

Eine modulare Medien-Suite in Rust: ein gemeinsamer Kern aus Dekodierung, GPU-Compositing und Audio-Verarbeitung, auf dem vier Betriebsmodi aufsetzen — Player, Live-Studio, Video-Editor und, als Fernziel, eine Digital Audio Workstation.

*Takt* bedeutet gleichzeitig den musikalischen Takt und den Taktgeber. Die Master-Clock ist die Komponente, an der dieses Projekt technisch hängt: Alles Schwere in Takt ist ein Zeitproblem.

> **Status (2026-10-31): Phase 0.** Das Fundament steht: `takt-core`, `takt-clock`, `takt-backend` und ein kopfloser `takt-player`, 64 Tests, CI grün auf Windows, Linux und macOS. Es fehlen noch Fenster, Audio-Ausgabe und das libmpv-Backend (`MpvBackend`) — noch kein benutzbarer Player. Details: [docs/PHASE-0.md](docs/PHASE-0.md).

**English.** Takt is a modular media suite written in Rust: one shared core for decoding, GPU compositing and audio processing, with four modes on top (player, live studio, video editor and, as a long-term goal, a DAW). It is a personal, non-commercial project in an early stage (phase 0: foundation and timing, no usable player yet). The documentation is in German on purpose; code, comments and commit messages are in English.

## Dokumentation und Planung

| Was | Wo |
|---|---|
| Konzept (Zielbild, Nicht-Ziele, Architektur, Entscheidungen) | Vault [`tasks-vault/takt/KONZEPT.md`](https://github.com/WKDSteve/tasks-vault/blob/main/takt/KONZEPT.md) (privat) |
| Threads, Uhren und Invarianten | [docs/THREADS-UND-UHREN.md](docs/THREADS-UND-UHREN.md) |
| Phase 0 und ihr Review | [docs/PHASE-0.md](docs/PHASE-0.md), [docs/reviews/](docs/reviews/) |
| Planung: Plan mit Etappen und Gates, Tasks, offene Entscheidungen | Vault [`tasks-vault/takt/`](https://github.com/WKDSteve/tasks-vault/tree/main/takt) (privat) |

## Plattformen

| Plattform | Stand |
|---|---|
| Windows | Hauptentwicklung, getestet |
| Linux | CI baut und testet; nativer Test auf echter Hardware geplant |
| macOS | **ungetestet** — CI baut und testet, aber es gibt kein Testgerät und keine macOS-spezifischen Backends |

---

## Die Idee

Player, Live-Studio und Video-Editor machen im Kern dasselbe mit Bild und Ton — sie unterscheiden sich nur im Zeitverhalten. Takt teilt deshalb die **Bausteine**, nicht den Scheduler: eine Dekodier- und Compositing-Schicht, darüber vier Modi mit je eigenem Zeitmodell.

| Modus | Taktquelle | Bei Überlast |
|---|---|---|
| Player | Audio-Clock, an Display-Refresh angeglichen | Frame verwerfen |
| Studio | Wall-Clock | Frame verwerfen |
| Editor | Timeline-Position | warten, Qualität vor Tempo |
| DAW | Audio-Callback | darf nicht auftreten |

Die App startet schlank im Player-Modus und fährt die schweren Subsysteme erst hoch, wenn ein anderer Modus aktiviert wird.

## Nicht-Ziele

Kein Nachbau von VLC, OBS, Cubase, GIMP oder Krita. KI-Modelle erst spät (Whisper-Untertitel nach Gate 0, weitere ab Phase 3), immer in einem eigenen Prozess. Keine Mobilplattformen. Keine eigenen Codecs. Kein VST2. Die vollständige Liste steht im Konzept.

## Zeithorizont

Ein Projekt über vier bis acht Jahre, neben einer Vollzeitstelle. Die Aufwandsschätzung pro Ausbaustufe steht im Konzept und wird bewusst nicht schöngerechnet.

## Lizenz

Takt verwendet eine Lizenz-Topologie statt einer einzelnen Lizenz:

| Zone | Bereich | Lizenz |
|---|---|---|
| 1 | Wiederverwendbare Kern-Crates | `Apache-2.0 OR MIT` |
| 2 | Anwendungen und Medien-Pipeline | `GPL-3.0-or-later` |
| 3 | Isolierte Hilfsprozesse (VST3-Bridge, GPL-Encoder) | eigene Lizenz je Prozess |

GPLv3 ist in Zone 2 die *ermöglichende*, nicht die einschränkende Wahl: sie schaltet VST3, x264/x265 und Qt frei, ohne Verträge oder Registrierung.

Jede Datei trägt eine `SPDX-License-Identifier`-Kopfzeile, jedes Crate ein eigenes `license`-Feld. Die vollständigen Lizenztexte liegen unter [LICENSES/](LICENSES/).

## Mitwirken

Beiträge unter [Developer Certificate of Origin](LICENSES/DCO.txt) — siehe [CONTRIBUTING.md](CONTRIBUTING.md).
