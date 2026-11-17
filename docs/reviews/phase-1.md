# Review Phase 1 — Eigene Pipeline

*Angelegt 2026-11-16, vor Beginn der Phase. Phase 1 beginnt nach Gate 0 (die vier Wochen Alltagsnutzung
starten am 2027-01-04); bis dahin hält dieses Dokument fest, woran die Phase gemessen wird und was sie aus
Phase 0 mitnimmt. Es wird während der Phase fortgeschrieben, nicht erst am Ende geschrieben. Die ersten
Bausteine der Pipeline sind am 2026-11-17 vorgezogen worden (Entscheidung des Autors): `takt-ipc` und
`takt-demux` stehen, ihr Stand ist unten unter Sicherheit und Entscheidungen eingetragen.*

Wie jedes Review beantwortet es drei Fragen: Was hat die Architektur gelernt? Wo liegen jetzt die
Performance-Grenzen? Welche Angriffsfläche ist neu entstanden? Dazu kommt die Gate-Frage der Phase.

---

## Table of content

  - [Gate 1](#gate-1)
  - [Ausgangslage aus Phase 0](#ausgangslage-aus-phase-0)
  - [Performance](#performance)
  - [Sicherheit und Angriffsflaeche](#sicherheit-und-angriffsflaeche)
    - [Stand Hilfsprozesse und Demuxer](#stand-hilfsprozesse-und-demuxer)
    - [Fuzzing der Leseseite](#fuzzing-der-leseseite)
    - [Offen](#offen)
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

**Demuxer durch die Prozessgrenze** (`cargo run --release -p takt-demux --example demux-speed -- <Datei>`; die
Zeit läuft vom ersten Paket bis zum Dateiende, die Datei liegt im Cache des Systems, der Leser verwirft die
Pakete):

| Datum | Hardware | Datei | Pakete | Bytes | Zeit | Durchsatz |
|---|---|---|---|---|---|---|
| 2026-11-17 | STEVESPC (i5-12400F), Windows 11, FFmpeg 9.0 (BtbN, LGPL) | 60 s Testbild 1080p60, MPEG-2 mit 60 Mbit/s und PCM in Matroska (414 MiB, mit ffmpeg erzeugt) | 6413 | 433 578 231 | 197 bis 258 ms (drei Läufe) | 1,6 bis 2,1 GiB/s, 25 000 bis 32 000 Pakete/s |

Die Datei braucht zum Abspielen 7 MiB/s: Ring und die zwei Kopien je Paket (in den Ring, aus dem Ring) sind
nicht der Engpass der Pipeline. Nicht gemessen: die Latenz eines einzelnen Pakets bei leerem Ring (abgefragt
wird mit Pausen bis 1 ms) und die CPU-Last des wartenden Hilfsprozesses.

### Zero-Copy-Spike (2026-11-17)

Vor der ersten Zeile der eigenen Pipeline vermessen (`takt/spike-zero-copy-decoder-gpu-import`, Prototyp
`tools/zero-copy-spike/`, Bericht im Vault unter `takt/ROADMAP/reports/2026-11-17_spike-zero-copy.md`): wgpu 27.0.1
nimmt die D3D12-Textur des Hardware-Decoders über wgpu-hal (`dx12::Device::texture_from_raw`,
`Device::create_texture_from_hal`) als `NV12`- bzw. `P010`-Textur an. Das Bild ist byte-gleich zum CPU-Kopierweg
desselben Prototyps (Prüfsummen des zurückgelesenen Ziels).

Die Zahlen sind die eines Prototyps, nicht die der Pipeline: 30 s je Lauf statt eines Films, zwei Durchgänge, kein
p99, kein RAM; ohne Ton, Uhr, Untertitel und Farbmanagement. CPU-Last in Prozent eines Kerns (Prozesszeit über die
Laufzeit, für mpv genauso gemessen), Fenster 1280x720 mit vsync auf demselben Bildschirm. Hardware: i5-12400F,
RTX 4070 SUPER (Treiber 610.88), Windows 11, FFmpeg 9.0.2, mpv 0.41.0-dev.

| Datum | Datei | Pfad | CPU-Last (Mittel) | GPU-Last | Kopien je Frame |
|---|---|---|---|---|---|
| 2026-11-17 | 4K60 AV1 8 bit | Prototyp, D3D12VA direkt | 8,1 % | 13 % | 0 |
| 2026-11-17 | 4K60 AV1 8 bit | Prototyp, D3D11VA mit geteilter Textur | 8,6 % | 24 % | 1 (GPU) |
| 2026-11-17 | 4K60 AV1 8 bit | Prototyp, CPU-Kopierweg (hwdownload) | 29,2 %, hält 60 fps nicht | 42 % | 2 CPU, 2 GPU |
| 2026-11-17 | 4K60 AV1 8 bit | `mpv --hwdec=auto` (d3d11va) | 13,9 % | nicht erfasst | 0 |
| 2026-11-17 | 4K60 HEVC 10 bit | Prototyp, D3D12VA direkt | 9,0 % | 22 % | 0 |
| 2026-11-17 | 4K60 HEVC 10 bit | Prototyp, D3D11VA mit geteilter Textur | 8,8 % | 27 % | 1 (GPU) |
| 2026-11-17 | 4K60 HEVC 10 bit | Prototyp, CPU-Kopierweg (hwdownload) | 48,2 %, hält 60 fps nicht | 46 % | 2 CPU, 2 GPU |
| 2026-11-17 | 4K60 HEVC 10 bit | `mpv --hwdec=auto` (d3d11va) | 16,2 % | 40 % | 0 |
| 2026-11-17 | 4K60 HEVC 10 bit | `mpv --hwdec=auto-copy` | 68,1 % | 40 % | CPU-Kopie |

Was das für das Gate heißt: Der Importweg verbraucht das Budget von „höchstens 1,2-fach von mpv“ nicht (der
Prototyp liegt beim 0,5- bis 0,7-fachen, mit viel weniger Arbeit als ein Player), und der CPU-Umweg kostet das
Drei- bis Fünffache und schafft 4K60 nicht mehr in Echtzeit. Die Messlatte des Gates bleibt die ganze Pipeline
über einen Film.

Nicht gemessen: Linux, Vulkan (auch Vulkan Video), macOS, Intel- und AMD-Grafik, Frame-Timing, 1080p H.264.
Nebenbefund: D3D12VA von FFmpeg 9.0.2 scheitert auf dieser Maschine an `hevc_nvenc`-Material in 3840x2160
(`hardware accelerator failed to decode picture`), D3D11VA spielt dieselbe Datei.

---

## Sicherheit und Angriffsflaeche

Neu in dieser Phase, absehbar aus der Planung:

* **Demuxer im eigenen Prozess** und **das IPC-Protokoll selbst**: gebaut, siehe
  [Stand Hilfsprozesse und Demuxer](#stand-hilfsprozesse-und-demuxer).
* **Decoder** (dav1d, FFmpeg-Decoder, Hardware-Decoder über den Treiber). Ein Hardware-Decoder parst Bitströme im
  Treiber, außerhalb jeder Sandbox von Takt.
* **Untertitel** (libass): gehört nach Phase 0 in dieselbe Sandbox wie der Demuxer.
* **Fuzzing**: die Leseseite des IPC-Protokolls hat ein `cargo-fuzz`-Ziel, siehe
  [Fuzzing der Leseseite](#fuzzing-der-leseseite). Die Parser-Schicht darunter ist libavformat selbst und
  liegt hinter der Prozessgrenze; eigene Bitstream-Parser (`takt-fmt`) bekommen ihr Ziel, wenn sie entstehen.

### Stand Hilfsprozesse und Demuxer

*Stand 2026-11-17, `takt-ipc` und `takt-demux` (`takt/takt-demux-libavformat-wrapper`).*

Container werden nie im Hauptprozess geparst. `takt_demux::Demuxer` startet je Datei `takt-demux-helper`; nur
dieses Programm linkt libavformat (die Bibliothek `takt-demux` enthält kein FFmpeg und kein `unsafe`, der Test
`the_client_links_no_ffmpeg` hält das fest). Der Prozessschnitt ist zugleich der Lizenzschnitt: `takt-ipc` ist
Zone 1 und hängt von keinem anderen Crate ab, damit ein Zone-3-Prozess das Protokoll unter jeder Lizenz
sprechen kann.

**Rechte des Hilfsprozesses** (die Frage von oben, geprüft):

| Was | Stand | Beleg |
|---|---|---|
| Welche Datei er liest | nur die geöffnete: er erbt ein Handle und bekommt keinen Pfad; gelesen wird über Callbacks, nicht über ein Protokoll von libavformat | `a_file_reaches_the_helper_as_a_handle`, `ffi.rs` (`read_cb`) |
| Was libavformat von sich aus öffnen darf | nichts: die Protokoll-Whitelist nennt kein Protokoll, und nimmt die Bibliothek eine der beiden Whitelists nicht an, wird die Datei nicht gelesen | `ffi.rs` (`NO_PROTOCOL`, Prüfung nach `avformat_open_input`) |
| Welche Parser laufen | eine feste Liste: Matroska, WebM, die MP4-Familie; jeder weitere Container ist eine Entscheidung | `a_container_that_is_not_on_the_list_is_refused` (eine korrekte WAVE-Datei) |
| Eigene Prozesse starten | unter Windows nicht (Job-Objekt, höchstens ein Prozess); unter Linux und macOS nicht verhindert | `a_helper_cannot_start_a_process_of_its_own` |
| Speicher | 2 GiB: zugesicherter Speicher unter Windows (Job-Objekt), Adressraum unter Linux (`RLIMIT_AS`); macOS setzt beides nicht durch | `the_memory_limit_refuses_a_large_allocation` |
| Netz, übrige Dateien des Benutzers, Registry | **nicht eingeschränkt**: der Prozess läuft mit den Rechten des Benutzers. Ein kompromittierter Hilfsprozess kann, was der Benutzer kann, nur nicht im Adressraum des Players | offen, siehe unten |

**Was ein Absturz oder ein Hänger dort im Hauptprozess auslöst:** einen Fehlerwert, keinen Absturz.
`DemuxError::Helper` nennt den Grund (beendet mit Code, Frist verpasst, Herzschlag steht, Limit überschritten,
Protokoll gebrochen); die Datei wird mit einem neuen Hilfsprozess neu geöffnet. Jeder Lesevorgang und jedes
Paket ist ein Herzschlag: ein Parser in einer Endlosschleife ohne Lesen ist nach 5 s beendet, einer, der ohne
Ende liest, nach 512 MiB ohne Paket. Stirbt der Hauptprozess, endet der Hilfsprozess mit ihm (sein stdin
schließt, ein eigener Thread wartet nur darauf; unter Windows zusätzlich das Job-Objekt). Belege:
`a_helper_that_hangs_is_ended_by_the_watchdog`, `a_helper_that_never_answers_is_ended_at_the_deadline_of_the_call`,
`a_helper_does_not_outlive_its_parent`, `too_much_on_stderr_ends_the_helper`, `a_helper_that_floods_the_pipe_is_ended`,
`cut_off_files_end_in_an_answer`, `damaged_files_end_in_an_answer` (36 beschädigte Dateien, der Hilfsprozess
ging dabei kein Mal verloren: libavformat 9.0 kommt mit ihnen zurecht).

**Die Leseseite** behandelt Pipe und Ring wie eine fremde Datei:

* Pipe: die Länge eines Frames wird gegen das Limit geprüft, bevor ein Byte der Nutzlast gepuffert ist;
  reservierte Bits müssen null sein; ein Strom, der einmal gelogen hat, hat keine Frame-Grenze mehr.
* Ring: jede Seite führt ihren Zähler für sich und liest den der anderen als Behauptung (höchstens die
  Kapazität voraus, Vielfaches von 8); der Kopf eines Records trägt ein Prüfwort, passt in das Limit und in
  das, was der Schreiber geschrieben haben will. Die Nutzlast wird herauskopiert, bevor sie angesehen wird;
  auf den geteilten Speicher gibt es nie eine Referenz.
* Demux-Nachrichten: ein Stream-Index muss existieren, eine Zeitbasis ein positiver Bruch aus zwei Zahlen
  unter 2^31 sein, ein Zeitstempel nie die „kein Wert“-Zahl, Dauern und Positionen nie negativ, reservierte
  Bytes null; Namen sind druckbares ASCII, Titel und Fehlertexte werden gekürzt und von Steuerzeichen befreit.
* Wer lügt, wird beendet, und der Demuxer bleibt beim Fehler: was derselbe Prozess sonst noch in den Ring
  geschrieben hat, wird nicht mehr ausgeliefert (`tests/liar.rs`, ein Hilfsprozess, der mit Absicht lügt).

Im Review der eigenen Commits gefunden und behoben (2026-11-17): die Bindings lagen zuerst in der Bibliothek,
womit jedes Programm, das den Demuxer benutzt, FFmpeg mitgeladen hätte (`ae5bafc`); der Watchdog zählte ab
Prozessstart statt ab dem Handshake; unter Windows konnte ein zweiter, gleichzeitig gestarteter Hilfsprozess
die Handles des ersten erben (`4ce8e9f`).

### Fuzzing der Leseseite

Ziel `protocol_reader` (`fuzz/`, cargo-fuzz mit libFuzzer; der Harness ist `takt_demux::fuzzing::read_untrusted`
und läuft mit festen Eingaben, allen Schnitten und Bitkippern davon auch in `cargo test`). Es füttert alles, was
der Hauptprozess von einem Hilfsprozess liest: Frames der Pipe und ihre Nachrichten, Zähler und Records des
Rings (auch mit einem lügenden Leser gegen den Schreiber), Stream-Tabelle und Pakete. Geprüft wird nicht nur
„kein Absturz“, sondern auch, was die Parser zusagen: nichts über den Limits, und was angenommen wurde, übersteht
erneutes Senden.

| Datum | Stand | Lauf | Ergebnis |
|---|---|---|---|
| 2026-11-17 | Arbeitsstand vor `989efe6` | 1 min, WSL2 (Arch), Startkorpus aus den echten Encodern | **ein Fund**: eine Stream-Tabelle mit einer Zeitbasis nahe 2^63 wurde angenommen; der Vergleich zweier solcher Zeitstempel lief in `Timestamp::cmp` über 128 Bit hinaus (Panik im Debug-Build, falsche Ordnung im Release-Build). Behoben in `989efe6`: die Leseseite begrenzt beide Teile jedes Bruchs auf das, was FFmpegs `AVRational` halten kann; damit rechnet `Timestamp` exakt. Regressionstest `a_time_base_too_large_for_exact_arithmetic_is_refused` |
| 2026-11-17 | `989efe6` | 1 h (3601 s), WSL2 (Arch) auf STEVESPC, libFuzzer über cargo-fuzz mit Nightly, Eingaben bis 4096 Bytes, Startkorpus aus den echten Encodern (2026-11-17, 11:18:38 bis 12:18:44 UTC) | **kein Fund**: 67 318 990 Läufe, im Mittel 18 694 je Sekunde, 5987 neue Eingaben im Korpus, kein Absturz, kein Artefakt, Spitze 587 MiB Speicher |

Der Fund zeigt auf `takt-core`: `Timestamp::cmp` und `Timestamp::rescale` multiplizieren ungeprüft in `i128`
und halten nur für Zeitbasen, deren Teile klein sind. Für alles, was aus dem Demuxer kommt, stellt die
Protokollgrenze das jetzt sicher; die Funktionen selbst sind unverändert (`takt/takt-core-timestamp-ueberlauf`).
Nach `989efe6` haben sich Parser nicht mehr geändert, nur der Umgang des Demuxers mit einem Fehler.

### Offen

1. **Keine Sandbox des Betriebssystems.** Der Hilfsprozess ist ein eigener Prozess mit Watchdog und Limits,
   aber mit den Rechten des Benutzers: kein eingeschränktes Token oder AppContainer unter Windows, kein seccomp
   oder Landlock unter Linux, keine Sandbox unter macOS. Die Vorarbeit ist da (er braucht keinen Pfad, kein
   Netz und keine weiteren Dateien), der Schritt selbst fehlt (`takt/takt-ipc-os-sandbox`).
2. **macOS**: kein Speicherlimit, kein Verbot eigener Prozesse, kein Lauf mit FFmpeg in der CI; `takt-ipc` und
   der Demuxer ohne FFmpeg sind dort getestet. Der FFmpeg-Lauf bleibt offen, bis macOS ein Zielgerät wird
   (`takt/ci-takt-demux-mit-ffmpeg-auf-macos`); Limits gehören zu `takt/takt-ipc-os-sandbox`.
3. **Unter Unix erbt ein Hilfsprozess**, was im Hauptprozess ohne close-on-exec offen ist. std öffnet alles
   mit dem Flag; eine C-Bibliothek im Hauptprozess (libmpv) muss das nicht tun (`takt/takt-ipc-os-sandbox`).
4. **Zwischen Start und Job-Zuweisung** läuft ein Hilfsprozess unter Windows einen Augenblick ohne Job; er hat
   bis dahin nichts Fremdes gelesen (die Datei liest er erst nach dem Handshake).
5. **Polling statt Ereignis**: ein leerer oder voller Ring wird mit Pausen bis 1 ms abgefragt. Für einen
   Demuxer, der dem Decoder vorausliest, genügt das (Durchsatz siehe Performance); die Latenz eines Pakets
   bei leerem Ring ist nicht gemessen (`takt/takt-ipc-latenz-bei-leerem-ring-messen`; mit dem Decoder im
   Hilfsprozess wird die Zahl gebraucht).
6. **Seitendaten der Pakete** (neue Codec-Konfiguration mitten im Strom, dynamische HDR-Metadaten, zu
   verwerfende Samples) überträgt das Protokoll noch nicht; `takt-decode` wird zeigen, was es braucht
   (`takt/takt-demux-seitendaten-der-pakete`).
7. **Die Bindings sind an eine FFmpeg-Hauptversion gebunden** (9.0); Build und Hilfsprozess verweigern eine
   andere. Ein Versionssprung ist Handarbeit nach `crates/takt-demux/layout/README.md`.
8. **Nur eigene Testclips**: der Vergleich mit `ffprobe` läuft über Clips aus `scripts/`, nicht über echte
   Dateien mit vielen Spuren, Anhängen und alten Muxern (`takt/takt-demux-echte-dateien-gegen-ffprobe`).

---

## Was in dieser Phase entschieden wurde

| Entscheidung | Grund | Revidierbar? |
|---|---|---|
| 2026-11-17: die Pipeline-Bausteine werden vor Gate 0 begonnen (Autor) | Demuxer und IPC-Muster hängen nicht an der Antwort von Gate 0; fällt sie negativ aus, bleibt das Muster für Encoder, VST3-Bridge und KI-Dienste | ja, die Crates sind noch nicht im Player verdrahtet |
| `takt-ipc` liegt in Zone 1 und hängt von keinem anderen Takt-Crate ab | Zone-3-Prozesse tragen eigene Lizenzen (GPL-Bridge, Modell-Lizenzen) und müssen das Protokoll trotzdem linken können; es ist außerdem Byte für Byte dokumentiert | ja |
| `takt-ipc` ist das eine Zone-1-Crate mit `unsafe`, alles im Modul `ffi` | Geteilter Speicher zwischen Prozessen und die Systemaufrufe dafür gehen nicht ohne | nein, solange das Muster gilt |
| Der Ring aus `takt-audio` wird nicht wiederverwendet, nur sein Muster | Dort teilen zwei Threads, die einander trauen, ein `Arc`; hier schreibt ein Prozess, der fremde Dateien parst. Gleiche Ordnungen, neue Leseregeln, Records variabler Länge | ja |
| Steuerkanal ist stdin/stdout des Hilfsprozesses | Geht mit std auf allen drei Plattformen ohne eigene Systemaufrufe, und das Schließen von stdin ist zugleich das Signal „Elternprozess ist fort“ | ja |
| FFmpeg-Bindings von Hand (rund zwanzig Funktionen, der Anfang von vier Strukturen) statt `ffmpeg-sys-next` oder `rsmpeg` | `ffmpeg-sys-next` steht unter WTFPL (nicht in `deny.toml`); beide Wege erzeugen ihre Bindings mit bindgen (libclang auf jeder Baumaschine, ein Build-Baum größer als das Crate). Preis: Bindung an eine Hauptversion, abgesichert durch Build-Prüfung, Laufzeit-Prüfung, geprüfte Offsets und den Vergleich mit `ffprobe` | ja: wird die Liste der Funktionen lang oder kommt `takt-decode` mit libavcodec dazu, neu abwägen (dann mit Frage zur Lizenz) |
| 2026-11-17: der Software-Decoder (libavcodec, dav1d) wird ebenfalls ein Hilfsprozess (Autor) | Ein Decoder parst fremde Bitströme wie der Demuxer Container; im Hauptprozess wäre die Demux-Grenze nur halber Schutz, und der Player lüde FFmpeg. Preis: eine Kopie je Frame in den `FramePool` | ja, wenn die Messung der Kopie bei 4K60 dagegen spricht |
| 2026-11-17: die Bindings bleiben auch für libavcodec von Hand (Autor) | Keine neue Lizenz (WTFPL), kein libclang auf der Baumaschine | ja |
| Die Bindings gehören zum Hilfsprogramm, nicht zur Bibliothek | Wer den Demuxer benutzt, darf FFmpeg weder linken noch laden; sonst ist die Prozessgrenze nur Dekoration | nein |
| Die Datei kommt als geerbtes Handle, nie als Pfad | Der Hilfsprozess braucht damit kein Recht auf das Dateisystem: Voraussetzung für eine spätere Sandbox, und ein Container kann libavformat nichts anderes öffnen lassen | nein |
| Feste Liste der Demuxer (Matroska, WebM, MP4-Familie) | Jeder Container ist ein Parser mehr über fremde Bytes; was der Player nicht braucht, läuft nicht | ja, je Container eine Zeile |
| Zeitstempel reisen als Ticks, die Zeitbasis einmal je Stream; Brüche sind auf 31 Bit je Teil begrenzt | Exakt wie im Container (nie `f64`), und in diesem Bereich rechnet `Timestamp` ohne Überlauf (Fuzz-Fund) | ja |
| Ein Sprung trägt eine Seriennummer; Pakete von davor verwirft die Leseseite | Keine Seite muss die andere anhalten, um den Ring zu leeren | ja |
| Limits des Demuxers: Ring 16 MiB, Paket bis 8 MiB, Speicher 2 GiB, Herzschlag 5 s, Öffnen 30 s | Reicht für 4K-Intra-Frames; ein größeres Paket wird gemeldet und ausgelassen, statt den Strom zu beenden | ja, Felder von `takt_demux::Options` |
| `takt-gpu` bleibt auf wgpu; fremde Texturen kommen über wgpu-hal-Interop herein, nicht über `ash` direkt (2026-11-17, Empfehlung des Zero-Copy-Spikes) | Auf Windows gemessen: Import ohne CPU-Kopie gelingt mit dem gelockten wgpu 27.0.1, NV12 und P010, ohne Fork. Für Linux nach Quelltext von wgpu-hal 27.0.4 erwartbar (`vulkan::Device::texture_from_raw`), dort mit eigenem `ash`-Code für den DMA-BUF-Import auf dem Gerät von wgpu; nicht gemessen | Ja: zeigt der Linux-Prototyp eine Sackgasse (Layout importierter Bilder, Vulkan-Video-Queues), gilt „Neu bewerten“ aus der Technologie-Tabelle des KONZEPTs |
| Windows: D3D11VA mit geteilter Textur (`OpenSharedHandle`, eine GPU-Kopie) als erster Hardware-Weg, D3D12VA direkt (keine Kopie) als spätere Optimierung (2026-11-17, Empfehlung des Spikes) | Gleiche CPU-Last wie der direkte Weg, aber Decoder und Renderer teilen keinen Zustand an Referenzbildern, und FFmpegs D3D11VA spielt Material, an dem sein D3D12VA hier scheitert | Ja, hinter der Schnittstelle von `takt-decode` |
| `GpuFrame` trägt eine `wgpu::Texture` (NV12/P010, zwei Ebenen-Views) plus sichtbare Größe, Bereitschaftssignal und Rückgabe-Guard; der CPU-Puffer bleibt die zweite Variante (2026-11-17, Vorschlag des Spikes für `takt-gpu`) | Beide Wege enden im selben Shader und Bind-Group-Layout; das wgpu-Gerät entsteht vor dem Decoder und braucht `TEXTURE_FORMAT_NV12` und `TEXTURE_FORMAT_P010` | Ja, bis `takt-gpu` die Schnittstelle festschreibt |
