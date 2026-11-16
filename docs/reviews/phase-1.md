# Review Phase 1 — Eigene Pipeline

*Angelegt 2026-11-16, vor Beginn der Phase. Phase 1 beginnt nach Gate 0 (die vier Wochen Alltagsnutzung
starten am 2027-01-04); bis dahin hält dieses Dokument fest, woran die Phase gemessen wird und was sie aus
Phase 0 mitnimmt. Es wird während der Phase fortgeschrieben, nicht erst am Ende geschrieben.*

Wie jedes Review beantwortet es drei Fragen: Was hat die Architektur gelernt? Wo liegen jetzt die
Performance-Grenzen? Welche Angriffsfläche ist neu entstanden? Dazu kommt die Gate-Frage der Phase.

---

## Table of content

  - [Gate 1](#gate-1)
  - [Ausgangslage aus Phase 0](#ausgangslage-aus-phase-0)
  - [Performance](#performance)
  - [Sicherheit und Angriffsflaeche](#sicherheit-und-angriffsflaeche)
  - [Was in dieser Phase entschieden wurde](#was-in-dieser-phase-entschieden-wurde)

---

## Gate 1

> **Frame-Timing und CPU-Last messbar auf mpv-Niveau?** Wenn nach acht Monaten nicht: beim libmpv-Backend
> bleiben und darauf weiterbauen. Kein Gesichtsverlust; der Trait war genau dafür da. (KONZEPT, Roadmap mit Gates)

„mpv-Niveau“ braucht Zahlen, bevor die eigene Pipeline die erste Zeile Bild liefert, sonst wird das Kriterium
nachträglich an das Ergebnis angepasst. **Entschieden am 2026-11-16** (Auswahl des Autors, der Vorschlag dieses
Dokuments unverändert):

| Größe | Messweg | Bestanden, wenn |
|---|---|---|
| Präsentationsfehler, Mittel und p99 | `scripts/ab-measure.py`, dieselbe Quelle je Pfad, fünf Minuten, ruhige Maschine | eigene Pipeline höchstens 1 ms über dem `MpvBackend` (Mittel und p99) |
| Wiederholte und verworfene Frames je Minute | wie oben | nicht mehr als das `MpvBackend`; „wiederholt“ innerhalb von 2/min am Erwartungswert |
| CPU-Last des Prozesses über einen Film | Overlay `process` (bzw. ein kopfloser Lauf) gegen `mpv --hwdec=auto` auf demselben Bildschirm | höchstens 1,2-fach von mpv, je Datei: 1080p H.264, 4K60 HEVC 10 bit, 4K60 AV1 |
| Dasselbe auf dem alten Testgerät | Software-Pfad, 1080p H.264 | läuft ohne verworfene Frames (der Fallback muss tragen) |
| Xruns | Overlay `xruns`, fünf Minuten unter Nebenlast | 0, wie in Phase 0 seit `99152fc` |

Die Toleranz von 1,2 bei der CPU-Last ist bewusst kein „gleich gut“: mpv ist seit Jahren auf genau diese Zahl
optimiert, und der Gewinn der eigenen Pipeline liegt woanders (Zero-Copy in `takt-gpu`, eine Uhr für Studio und
Editor). Ein Faktor über 1,2 hieße aber, dass Zero-Copy nicht greift, und genau das soll das Gate fangen.

Stichtag: acht Monate nach dem Start der Phase. Die Antwort kommt mit Datum in das KONZEPT unter „Getroffene
Entscheidungen“ (`takt/review-phase-1`).

---

## Ausgangslage aus Phase 0

Belegt in `docs/reviews/phase-0.md`; das sind die Zahlen, gegen die die eigene Pipeline antritt.

* **Scheduler von Takt** (NullBackend, kopflos, 60 Hz simuliert, 2026-11-14): Fehler im Mittel 4,1 bis 4,2 ms,
  p99 unter 8,4 ms bei 23,976, 25 und 50 fps, die Theorie für „jedes Frame an die nächste Bildwiederholung“.
* **MpvBackend** (kopflos, Vorlauf eine halbe Periode): 4,2 bis 5,0 ms im Mittel; bei 60 fps auf 60 Hz verwirft
  es 13 bis 17 Frames je Minute (Phasenwanderung; der Fall für `display-resample`, Spike
  `takt/spike-display-resample-av-sync`).
* **Audio-Callback**: p99,99 unter 192 µs bei 10 ms Periode. Füll-Thread und kopflose Präsentationsschleife
  fordern seit `99152fc` und `810fc33` Echtzeit an; unter 82 bis 100 % Nebenlast danach 0 Xruns und 0 verpasste
  Refreshes in fünf Minuten.
* **Nicht gemessen**: CPU-Last über einen ganzen Film, Startzeit, mpv selbst auf einem echten Bildschirm (dritte
  Zeilengruppe des A/B-Protokolls), A/V-Versatz von Hand. Diese Lücken sind die Messlatte des Gates; wer sie zuerst
  füllt, füllt sie für beide Pfade.

Mitgenommen aus den offenen Punkten von Phase 0:

1. Vulkan Video ist nicht universell (macOS: VideoToolbox); die Backend-Abstraktion in `takt-decode` bleibt.
2. Der Software-Pfad kommt zuerst: Fallback, altes Testgerät, Korrektheitsreferenz für den Hardware-Pfad.
3. Zero-Copy wird gemessen, nicht behauptet (CPU-Last 4K60 AV1 gegen `mpv --hwdec=auto`, siehe Gate).
4. Der Decoder-Thread der eigenen Pipeline übernimmt die Echtzeitpriorität des Füll-Threads; der Füllstand des
   Rings steht schon im Overlay.
5. Jeder neue `unsafe`-Block (FFI zu libavformat, dav1d, Vulkan Video) mit `// SAFETY:` und in einem benannten
   FFI-Modul, wie `apps/takt-player/src/mpv/ffi.rs`.
6. 4K braucht die GPU-Fläche: mpvs Render-API oder gleich die eigene Pipeline.

---

## Performance

*Wird während der Phase gefüllt.* Erwartete Messtabellen, Spalten wie im A/B-Protokoll von Phase 0
(Datum, Hardware, Quelle, Pfad, Anzeige, Mittel, p99, wiederholt/min, erwartet/min, verworfen/min) und dazu:

| Datum | Hardware | Datei | Pfad | CPU-Last (Mittel, p99) | GPU-Last | RAM | Kopien je Frame |
|---|---|---|---|---|---|---|---|

---

## Sicherheit und Angriffsflaeche

Neu in dieser Phase, absehbar aus der Planung:

* **Demuxer im eigenen Prozess** (`takt/takt-demux-libavformat-wrapper`). Container-Parser über fremde Byteströme
  sind das klassische Ziel (Phase 0, „Die Angriffsfläche ist von Anfang an groß“). Der Prozessschnitt ist zugleich
  der Lizenzschnitt; zu prüfen: Rechte des Prozesses (kein Netz, nur die geöffnete Datei), was ein Absturz dort im
  Player auslöst.
* **Das IPC-Protokoll selbst** (Shared Memory plus Pipe, KONZEPT „Hilfsprozess-IPC“). Der Hauptprozess liest jetzt
  Daten aus einem Prozess, der fremde Dateien parst: Größen, Offsets und Zeitstempel aus dem Ring sind nicht
  vertrauenswürdig und werden geprüft wie Daten aus einer Datei.
* **Decoder** (dav1d, FFmpeg-Decoder, Hardware-Decoder über den Treiber). Ein Hardware-Decoder parst Bitströme im
  Treiber, außerhalb jeder Sandbox von Takt.
* **Untertitel** (libass): gehört nach Phase 0 in dieselbe Sandbox wie der Demuxer.
* **Fuzzing**: mit dem ersten echten Demuxer kommt `cargo-fuzz` auf die Parser-Schicht und auf die Leseseite des
  IPC-Protokolls.

---

## Was in dieser Phase entschieden wurde

| Entscheidung | Grund | Revidierbar? |
|---|---|---|
