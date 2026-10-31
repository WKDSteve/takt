# Phase 0 — Fundament und Zeitmechanik

*Begonnen 2026-08-23. Status: Fundament steht, Fenster fehlt noch.*

Phase 0 soll zwei Fragen beantworten: **Steht die Grundstruktur?** und **Macht das Spaß?** Die zweite ist die wichtigere und lässt sich nur durch Benutzen beantworten, nicht durch Nachdenken.

---

## Table of content

  - [Was steht](#was-steht)
  - [Die Reihenfolge und ihre Begründung](#die-reihenfolge-und-ihre-begruendung)
  - [Der erste Lauf](#der-erste-lauf)
  - [Was als Nächstes kommt](#was-als-naechstes-kommt)
  - [libmpv beschaffen](#libmpv-beschaffen)
  - [Gate 0](#gate-0)

---

## Was steht

64 Tests, alle grün, CI grün auf Windows, Linux und macOS (Apple Silicon).

Der erste Push bestand nur lokal auf x86: die CI-Matrix fand auf ARM64 einen echten Nebenläufigkeitsfehler im Seqlock (fehlende Speicherordnung zwischen den beiden Datenfeldern und dem Zähler) sowie eine API-Lücke, die `cargo doc -D warnings` aufdeckte (`AudioClockWriter` von einer öffentlichen Methode zurückgegeben, aber nie re-exportiert). Beides behoben — siehe [reviews/phase-0.md](reviews/phase-0.md#seqlock-statt-mutex--gemessen-nicht-vermutet) für die technische Analyse. Genau das Szenario, für das die Drei-Plattform-Matrix ab Commit eins gedacht war.

| Crate | Zone / Lizenz | Inhalt |
|---|---|---|
| [`takt-core`](../crates/takt-core) | 1 — `Apache-2.0 OR MIT` | `Rational`, `Timestamp`, `PixelFormat`, Farbraum, `VideoFrame`, `FramePool`, `AudioBuffer`, Fehlertypen |
| [`takt-clock`](../crates/takt-clock) | 1 — `Apache-2.0 OR MIT` | `Clock`-Trait, `AudioMasterClock`, `WallClock`, `TimelineClock`, `AnchorCell` (Seqlock) |
| [`takt-backend`](../crates/takt-backend) | 2 — `GPL-3.0-or-later` | `VideoBackend`-Trait, `NullBackend`, `Scheduler` mit Messwerten |
| [`takt-player`](../apps/takt-player) | 2 — `GPL-3.0-or-later` | Kopflose Wiedergabeschleife, Messwertausgabe |

Dazu: `deny.toml` mit Lizenz-Allowlist, CI für Windows, Linux und macOS, `rust-toolchain.toml`, SPDX-Kopfzeilen in jeder Datei.

Drei Entscheidungen, die hier festgelegt und nicht mehr geändert werden:

**Zeit ist rational, nie Gleitkomma.** `Timestamp` ist ein ganzzahliger Tick-Wert plus eine rationale Basis, Umrechnungen laufen über `i128`. `f64`-Sekunden akkumulieren bei 24000/1001 fps oder 44,1 kHz Rundungsfehler, die sich nach Stunden als Bild-Ton-Drift zeigen und dann nicht mehr rekonstruierbar sind. Der Test `samples_rechnen_verlustfrei` prüft eine Stunde Audio bei 44,1 kHz hin und zurück ohne Abweichung.

**Frames gehören einem Pool, nicht dem Aufrufer.** In Phase 0 wäre ein `Vec<u8>` pro Frame völlig ausreichend. Der Pool steht trotzdem, weil ein Frame ab Phase 1 kein Puffer mehr ist, den man besitzt, sondern ein *Platz in einem begrenzten Ringpuffer des Hardware-Decoders*. Gibt der Renderer ihn nicht zurück, läuft der Decoder leer. Diese Rückgabepflicht nachträglich einzuziehen bedeutet, jede Stelle anzufassen, die ein Frame anfasst. `PooledFrame` gibt beim `Drop` zurück; ein erschöpfter Pool meldet einen Fehler statt still nachzuallozieren, weil Nachallozieren das Problem verstecken würde, das man messen will.

**Das Thread- und Uhrenmodell steht.** Ausführlich in [THREADS-UND-UHREN.md](THREADS-UND-UHREN.md).

---

## Die Reihenfolge und ihre Begruendung

Die Ausgangsplanung sah als ersten Schritt „libmpv-Player mit egui-Fenster" vor. Tatsächlich gebaut wurde zuerst die Zeitmechanik mit einem selbst rechnenden Backend. Das war die bessere Reihenfolge, aus drei Gründen:

**Cross-Platform ab Commit eins ist nur ohne Systemabhängigkeiten möglich.** `NullBackend` erzeugt Frames rechnerisch — ohne FFmpeg, ohne libmpv, ohne GPU. Damit steht die CI auf Windows, Linux und macOS sofort grün, statt erst „wenn mpv paketiert ist". Bei einem Projekt, dessen Cross-Platform-Fähigkeit ein Alleinstellungsmerkmal sein soll, ist das keine Nebensache.

**Fehler lassen sich zuordnen.** Wenn die Wiedergabe ruckelt und im Spiel sind gleichzeitig eine unerprobte Uhr, ein unerprobter Scheduler und libmpv, weiß man nicht, wo man suchen muss. Mit einer Quelle, deren Zeitstempel per Konstruktion exakt sind, ist jede Abweichung eindeutig ein Fehler in der eigenen Mechanik.

**Das Testbild bleibt nützlich.** Ab Phase 1 macht ein bekanntes Muster Farbraum- und Chroma-Fehler im GPU-Pfad sofort sichtbar. `NullBackend` ist damit kein Platzhalter, den man später löscht, sondern dauerhaftes Werkzeug.

---

## Der erste Lauf

```
Takt Player — Phase 0 (kopflos)
  Backend      null
  Geometrie    1920x1080 Nv12
  Rate         24000/1001 fps
  Framedauer   41.708 ms
  Frames       72

Messwerte
  dargestellt     72
  wiederholt      105 (59.3 %)
  verworfen       0
  Fehler Mittel   10.668 ms
  Endposition     0:00:03.012

Histogramm der Fehlerbetraege
    1024..2048   us      1  #
    2048..4096   us      3  ###
    4096..8192   us     27  ###########################
    8192..16384  us     36  ####################################
   16384..32768  us      5  #####
```

Die Zahlen sind erwartungsgemäß und lehrreich:

**59,3 % Wiederholungen sind korrekt, kein Fehler.** Die Quelle läuft mit 23,976 fps, die Anzeigeschleife mit 60 Hz. 60 dividiert durch 23,976 ergibt etwa 2,5 — auf jeden zweiten bis dritten Bildschirmdurchlauf entfällt kein neues Frame. Das ist der Normalfall bei Kinomaterial auf einem 60-Hz-Monitor.

**Der mittlere Fehler von 10,7 ms ist genau das Problem, um das es geht.** Er entsteht nicht aus schlechtem Code, sondern aus dem Verhältnis 23,976 zu 60: die Frame-Dauer von 41,708 ms ist kein Vielfaches der Anzeigeperiode von 16,667 ms. Jedes Frame wird also zum nächstbesten Durchlauf gezeigt, mit einem Fehler zwischen 0 und 16,7 ms.

**Und die Streuung ist der Punkt, nicht der Mittelwert.** Das Histogramm zeigt die Fehler über mehrere Klassen verteilt — 4 bis 8 ms und 8 bis 16 ms sind besetzt. Genau diese Streuung erzeugt sichtbares Ruckeln bei Kameraschwenks. Ein konstanter Fehler von 10 ms wäre unsichtbar; ein zwischen 2 und 16 ms schwankender ist es nicht. Deshalb führt der Scheduler Extrema und Histogramm und nicht nur den Durchschnitt.

Genau hier setzt später `--video-sync=display-resample` an, mpvs Ansatz: statt Frames zu doppeln, wird die Audio-Rate minimal an den tatsächlichen Display-Refresh angepasst. Das ist ein Phase-1-Thema — aber die Messung, die den Nutzen belegen wird, steht jetzt.

---

## Was als Naechstes kommt

In dieser Reihenfolge:

1. **`takt-audio`: lock-freier SPSC-Ringpuffer**, dazu ein `cpal`-Ausgabepfad. Damit wird die Audio-Master-Uhr von einem echten Callback gefüttert statt von einem simulierten Thread, und `assert_no_alloc` kommt in den Callback.
2. **Fenster und Anzeige.** `winit` plus `egui`, Video über `wgpu` als NV12-Textur mit Farbraum-Shader. Das Testbild aus `NullBackend` ist dabei die erste Prüfung des Farbpfads.
3. **Diagnose-Overlay**, per Taste umschaltbar: Histogramm, Anker-Alter, Pool-Belegung, Xrun-Zähler.
4. **`MpvBackend`** hinter dem bestehenden Trait. Ab hier ist es ein benutzbarer Player.
5. **Playlist, Tastaturbelegung, Drag & Drop, Untertitel.**

Erst nach Schritt 4 beginnt die vierwöchige Alltagsnutzung, die Gate 0 entscheidet.

---

## libmpv beschaffen

Erledigt am 2026-10-31, beschrieben in [BUILD.md](BUILD.md): vorgebaute Archive für libmpv (shinchiro) und FFmpeg (BtbN, LGPL), auf Windows eine selbst erzeugte `mpv.lib`, eingebunden über die Umgebungsvariablen `TAKT_LIBMPV_DIR` und `FFMPEG_DIR` und geprüft durch den Smoke-Test `apps/takt-player/tests/ffi_native_link.rs`. Ohne die Variablen bleibt alles beim Alten: der Player läuft über `NullBackend`, die CI braucht keine Systembibliothek. Das Player-Binary selbst linkt libmpv erst mit dem `MpvBackend`; bis dahin beweist der Smoke-Test, dass Beschaffung und Linken funktionieren.

Zwei Abweichungen von der ursprünglichen Planung: vcpkg ist nicht der erste Weg, sondern die Ausweichlösung (baut alles aus dem Quelltext), und die vorgebaute libmpv ist ein GPL-Build mit statisch eingebauter GPL-FFmpeg. Für den GPL-3-Player ist das verträglich, widerspricht aber der Regel „kein libmpv-Build mit GPL“. Entschieden am 2026-10-31: der GPL-Build bleibt, aber nur in Zone 2 (BUILD.md, Abschnitt „Lizenz der vorgebauten libmpv“).

---

## Gate 0

> **Benutzt du den Player nach vier Wochen selbst statt VLC?**

Zu beantworten, sobald `MpvBackend` und das Fenster stehen. Wenn nein: aufhören und ehrlich sein. Das ist der billigste Test des ganzen Projekts, und ihn zu überspringen wäre die teuerste Entscheidung.

Zusätzlich wird beim Gate das Review-Dokument [reviews/phase-0.md](reviews/phase-0.md) fortgeschrieben — Performance und Angriffsfläche werden am Ende jeder Phase erneut betrachtet, nicht nur am Anfang.
