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
| [`takt-core`](../crates/takt-core) | 1 — `Apache-2.0 OR MIT` | `Rational`, `Timestamp`, `PixelFormat` (darunter `Rgb0`: fertiges R'G'B' mit Füllbyte statt Alpha), Farbraum, `VideoFrame`, `FramePool`, `AudioBuffer`, Fehlertypen |
| [`takt-clock`](../crates/takt-clock) | 1 — `Apache-2.0 OR MIT` | `Clock`-Trait, `AudioMasterClock`, `WallClock`, `TimelineClock`, `AnchorCell` (Seqlock) |
| [`takt-audio`](../crates/takt-audio) | 1 — `Apache-2.0 OR MIT` | `ring`: wartefreier SPSC-Ring für verschachtelte `f32`-Frames, Überlauf gezählt, ohne `unsafe`; `OutputCallback` (Ring → Gerät, Stille und Xrun bei leerem Ring, stellt die Audio-Master-Uhr, unter `assert_no_alloc`, misst die eigene Laufzeit in `DurationHistogram` mit log-linearen µs-Klassen für p99.99, zählt Xruns mit Zeitpunkt); `Output` über `cpal` hinter dem Feature `cpal` (nach Phase 0 begonnen) |
| [`takt-backend`](../crates/takt-backend) | 2 — `GPL-3.0-or-later` | `VideoBackend`-Trait, `NullBackend`, `Scheduler` mit Messwerten |
| [`takt-player`](../apps/takt-player) | 2 — `GPL-3.0-or-later` | Fenster (egui über wgpu) mit zwei Texturpfaden (NV12, `rgb0`), `Session` (Testbild oder Datei, genau eine Uhr), `MpvBackend` über libmpv (nur in einem Build mit `TAKT_LIBMPV_DIR`), kopflose Messschleife |

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

1. ~~**`takt-audio`: lock-freier SPSC-Ringpuffer**, dazu ein `cpal`-Ausgabepfad.~~ Erledigt: `takt-player` spielt über das Standardgerät (WASAPI shared, ALSA, CoreAudio) und die Audio-Master-Uhr folgt dem echten Callback; ohne Gerät oder mit `TAKT_AUDIO=sim` meldet er „SIMULIERT“ und nimmt den simulierten Thread. Der Gerätetest läuft nur mit `TAKT_DEVICE_TESTS=1` (`cargo test -p takt-audio --features cpal --test device -- --ignored`), in der CI ist er „ignored“. Die Latenz des Geräts (`playback - callback` aus cpal, auf STEVESPC unter WASAPI rund 32 ms) wird von der Position der Uhr abgezogen; der kopflose Lauf nennt sie. Geräte ohne f32 bekommen i32 oder i16 (mit Dither).
2. **Fenster und Anzeige.** `winit` plus `egui`, Video über `wgpu` als NV12-Textur mit Farbraum-Shader. Das Fenster steht (eframe 0.33 mit wgpu, `apps/takt-player/src/ui`; Leertaste Play/Pause, Escape beendet, `--headless` für die Messschleife); das Testbild läuft als NV12 durch zwei Texturen und `render/nv12.wgsl` (BT.709, limited range). Der Farbpfad ist gegen die CPU-Referenz `render/colour.rs` geprüft: 75-%-Farbbalken und ein Graukeil, gepolstert und ungepolstert, Toleranz 2/255 je Kanal (`TAKT_GPU_TESTS=1 cargo test -p takt-player gpu -- --ignored`, auf einer GPU; in der CI „ignored“). Die Upload-Zeit je Frame steht in der Leiste. Die Render-Schleife läuft im Fenster: je Bildwiederholung (vsync, ohne `sleep`) das fällige Frame an den `Scheduler`, Uhr ist `SeekableClock` um die Audio-Master-Uhr (Pause hält die Medienposition, Pfeile springen 5 s, Pos1 an den Anfang, ein Klick oder Ziehen auf der Zeitleiste springt dorthin; `9`/`0` leiser/lauter und `m` stumm wie bei mpv, die Lautstärke wirkt wartefrei im Audio-Callback (`takt_audio::Gain`); das Overlay nennt Ziel und erstes Frame nach dem letzten Sprung). `takt-player --seconds N` misst das Fenster: auf einem 120-Hz-Monitor (STEVESPC, 2026-11-12) 79,6 % Wiederholungen bei 118,8 Durchläufen/s, erwartet 1 − 23,976/118,8 = 79,8 %; dieselbe Rechnung für 60 Hz ergibt die 60 % des kopflosen Laufs. Ein Frame ist fällig, sobald es näher als eine halbe Anzeigeperiode an der Uhr liegt (`VideoBackend::set_display_period`, höchstens eine halbe Frame-Dauer); das Fenster misst die Periode als Median seiner Durchläufe. Vorher galt die halbe Frame-Dauer, und der mittlere Fehler stieg mit der Rate (15,6 ms bei 120 Hz); jetzt 2,6 ms bei 120 Hz im Fenster und 4,3 ms im kopflosen 60-Hz-Lauf (vorher 10,6 ms). Das Testbild aus `NullBackend` ist dabei die erste Prüfung des Farbpfads.
3. ~~**Diagnose-Overlay**, per Taste umschaltbar: Histogramm, Anker-Alter, Pool-Belegung, Xrun-Zähler.~~ Erledigt: F3 zeigt über dem Video Präsentationszähler und Fehler-Histogramm (Balken), Anker-Alter gegen die Extrapolationsgrenze (`STALE` darüber), Pool-Belegung, gemessene Anzeigeperiode, Audio-Zähler (Xruns, Stille, Gerätelatenz, Callback-Perzentile), CPU und RAM des Prozesses (`sysinfo`, eigener Thread, einmal je Sekunde) und die eigenen Kosten je Durchlauf. F4 legt denselben Text in die Zwischenablage und als `takt-diagnostics-<Unix-Sekunden>.txt` ins Temp-Verzeichnis (für Gate-Berichte); `--overlay` startet mit sichtbarem Overlay. Gemessen (Release, 120 Hz, STEVESPC): Aufbau des Overlays p50 < 0,1 ms, p99 < 0,64 ms; die Spitzen bis 0,9 ms entstehen beim Neusatz der Texte, die das Overlay deshalb nur viermal je Sekunde neu liest.
4. ~~**`MpvBackend`** hinter dem bestehenden Trait.~~ Erster Schnitt erledigt (2026-11-14), über mpvs Software-Renderer: in einem Build mit `TAKT_LIBMPV_DIR` spielt das Fenster Dateien (`apps/takt-player/src/mpv/backend.rs`). mpv öffnet, dekodiert, gibt den Ton auf seinem eigenen Gerät aus und rendert jedes Frame samt Untertiteln in den Speicher; Takt bekommt fertige Bilder als `PixelFormat::Rgb0` aus dem Pool und zeigt sie über einen zweiten Texturpfad neben NV12 (`render/gpu_rgb.rs`, `rgb0.wgsl`), ohne Wandlung nach NV12. Die Uhr ist weiter eine `AudioMasterClock`; ihr Anker ist jetzt mpvs Tonposition (`audio-pts`), einmal je Durchlauf veröffentlicht. Wann ein Frame fällig ist, entscheidet mpv; Takt gibt ihm die halbe gemessene Anzeigeperiode als Vorlauf (`video-timing-offset`), damit ein Frame an die Bildwiederholung geht, die seinem Zeitstempel am nächsten liegt. Eine `Session` (`session.rs`) hält entweder das Testbild mit dem cpal-Pfad oder eine Datei mit mpv, nie beides: das ist Invariante 3 als Typ. Dateien kommen aus der Playlist, per Drag & Drop oder von der Kommandozeile (`takt-player film.mkv`); Pause, Sprünge und Lautstärke gehen an mpv, `j` und `#` schalten Untertitel- und Tonspur. Geöffnet wird nur, was eine vorhandene Datei ist, auch wenn mpv von sich aus weiter öffnet (Einträge einer Playlist-Datei). Zahlen und offene Punkte in [reviews/phase-0.md](reviews/phase-0.md), Abschnitt „MpvBackend“. Offen: mpvs Render-API direkt in die GPU-Fläche (für 4K), ein Rolling-Build mit libmpv, und die Abnahme von Hand (Bild sehen, Ton hören).
5. **Playlist, Tastaturbelegung, Drag & Drop, Untertitel.**

Erst nach Schritt 4 beginnt die vierwöchige Alltagsnutzung, die Gate 0 entscheidet.

---

## libmpv beschaffen

Erledigt am 2026-10-31, beschrieben in [BUILD.md](BUILD.md): vorgebaute Archive für libmpv (shinchiro) und FFmpeg (BtbN, LGPL), auf Windows eine selbst erzeugte `mpv.lib`, eingebunden über die Umgebungsvariablen `TAKT_LIBMPV_DIR` und `FFMPEG_DIR` und geprüft durch den Smoke-Test `apps/takt-player/tests/ffi_native_link.rs`. Ohne die Variablen bleibt alles beim Alten: der Player läuft über `NullBackend`, die CI braucht keine Systembibliothek. Seit dem `MpvBackend` (2026-11-14) linkt auch das Player-Binary libmpv, wenn `TAKT_LIBMPV_DIR` gesetzt ist, und braucht die Bibliothek dann zur Laufzeit.

Zwei Abweichungen von der ursprünglichen Planung: vcpkg ist nicht der erste Weg, sondern die Ausweichlösung (baut alles aus dem Quelltext), und die vorgebaute libmpv ist ein GPL-Build mit statisch eingebauter GPL-FFmpeg. Für den GPL-3-Player ist das verträglich, widerspricht aber der Regel „kein libmpv-Build mit GPL“. Entschieden am 2026-10-31: der GPL-Build bleibt, aber nur in Zone 2 (BUILD.md, Abschnitt „Lizenz der vorgebauten libmpv“).

---

## Gate 0

> **Benutzt du den Player nach vier Wochen selbst statt VLC?**

Zu beantworten, sobald `MpvBackend` und das Fenster stehen. Wenn nein: aufhören und ehrlich sein. Das ist der billigste Test des ganzen Projekts, und ihn zu überspringen wäre die teuerste Entscheidung.

Zusätzlich wird beim Gate das Review-Dokument [reviews/phase-0.md](reviews/phase-0.md) fortgeschrieben — Performance und Angriffsfläche werden am Ende jeder Phase erneut betrachtet, nicht nur am Anfang.
