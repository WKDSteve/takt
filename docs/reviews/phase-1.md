# Review Phase 1 — Eigene Pipeline

*Angelegt 2026-11-16, vor Beginn der Phase. Phase 1 beginnt nach Gate 0 (die vier Wochen Alltagsnutzung
starten am 2027-01-04); bis dahin hält dieses Dokument fest, woran die Phase gemessen wird und was sie aus
Phase 0 mitnimmt. Es wird während der Phase fortgeschrieben, nicht erst am Ende geschrieben. Die ersten
Bausteine der Pipeline sind am 2026-11-17 vorgezogen worden (Entscheidung des Autors): `takt-ipc` und
`takt-demux` stehen, ihr Stand ist unten unter Sicherheit und Entscheidungen eingetragen; am selben Tag kam
`takt-decode` dazu, der Software-Pfad als zweites Backend des Players neben libmpv.*

Wie jedes Review beantwortet es drei Fragen: Was hat die Architektur gelernt? Wo liegen jetzt die
Performance-Grenzen? Welche Angriffsfläche ist neu entstanden? Dazu kommt die Gate-Frage der Phase.

---

## Table of content

  - [Gate 1](#gate-1)
  - [Ausgangslage aus Phase 0](#ausgangslage-aus-phase-0)
  - [Performance](#performance)
    - [Software-Decoder durch die Prozessgrenze](#software-decoder-durch-die-prozessgrenze)
    - [Bildgleichheit gegen libmpv](#bildgleichheit-gegen-libmpv)
    - [Zero-Copy-Spike (2026-11-17)](#zero-copy-spike-2026-11-17)
    - [Compositing in takt-gpu (2026-11-17)](#compositing-in-takt-gpu-2026-11-17)
  - [Sicherheit und Angriffsflaeche](#sicherheit-und-angriffsflaeche)
    - [Stand Hilfsprozesse und Demuxer](#stand-hilfsprozesse-und-demuxer)
    - [Stand Software-Decoder](#stand-software-decoder)
    - [Fuzzing der Leseseite](#fuzzing-der-leseseite)
    - [Stand takt-gpu](#stand-takt-gpu)
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

### Software-Decoder durch die Prozessgrenze

*Stand 2026-11-17, `takt-decode` (`takt/takt-decode-software-pfad`).* Der Software-Pfad dekodiert H.264 und HEVC
mit den Decodern von libavcodec und AV1 mit dav1d, im Hilfsprozess `takt-decode-helper`; die Bilder kommen über den
Ring und landen in einem `takt_core::FramePool`. **Die Bilder sind Byte für Byte die, die das Programm `ffmpeg`
derselben FFmpeg aus derselben Datei dekodiert** (`h264_hevc_and_av1_decode_to_the_bytes_ffmpeg_decodes`: fünf Clips,
H.264 und HEVC mit B-Frames, AV1, HEVC und AV1 in 10 bit mit ungerader Chroma-Größe, je 50 Bilder; dasselbe als NV12
und P010 in `the_same_pictures_arrive_as_nv12_and_p010`).

Durchsatz, frei laufend (`cargo run --release -p takt-decode --example decode-speed -- <Datei>`; die Zeit läuft vom
Start beider Hilfsprozesse bis zum letzten Bild, der Leser verwirft die Bilder sofort; drei Läufe; STEVESPC,
i5-12400F mit 6 Kernen und 12 Threads, Windows 11, FFmpeg 9.0 von BtbN (LGPL), Clips mit ffmpeg aus `testsrc2`
erzeugt):

| Datum | Datei | Decoder | Bilder/s | je Bild aus dem Ring | je Bild in den Pool | Anteil beider Kopien an der Laufzeit des lesenden Threads |
|---|---|---|---|---|---|---|
| 2026-11-17 | 1080p60 H.264 8 bit, 20 s (x264 veryfast, 3,0 MiB je Bild) | `h264` | 678 bis 733 | 0,72 bis 0,78 ms | 0,45 bis 0,53 ms | 86 bis 89 % |
| 2026-11-17 | 4K60 HEVC 10 bit, 10 s (x265 ultrafast, 23,7 MiB je Bild) | `hevc` | 104 bis 110 | 4,6 bis 4,9 ms | 3,7 bis 4,0 ms | 93 % |
| 2026-11-17 | 4K60 AV1 8 bit, 10 s (SVT-AV1, 11,9 MiB je Bild) | `libdav1d` | 116 bis 130 | 2,4 ms | 1,6 bis 1,7 ms | 48 bis 53 % |

Mit verschachteltem Chroma (NV12, P010; das Verschachteln geschieht in der zweiten Kopie) dieselben Größenordnungen:
679, 109 und 116 Bilder/s.

Was die Zahlen sagen:

* **Der Software-Pfad schafft 4K60 auf dieser Maschine**, mit dem 1,7- bis 2-Fachen der Echtzeit, 1080p60 mit dem
  Elffachen. Das ist der Durchsatz der ganzen Strecke (Demuxer, zwei Prozessgrenzen, Decoder), nicht die Last beim
  Abspielen.
* **Die Kopie ist nicht umsonst.** Die Entscheidung für den Hilfsprozess nannte als Preis „eine Kopie je Frame“.
  Gemessen sind es drei: eine im Hilfsprozess (aus dem Puffer des Decoders in den Ring) und zwei im Hauptprozess
  (aus dem Ring in einen Puffer, weil auf den geteilten Speicher nie eine Referenz zeigt, und von dort Zeile für
  Zeile in das Frame des Pools, dessen Zeilen anders ausgerichtet sind). Im Hauptprozess kosten sie bei 4K60 HEVC
  10 bit zusammen 8,5 ms je Bild, also bei 60 Bildern/s rund einen halben Kern nur fürs Kopieren, und frei laufend
  ist der kopierende Thread der Engpass, nicht der Decoder. Bei 1080p sind es 1,2 ms je Bild.
* Die zweite Kopie im Hauptprozess lässt sich sparen, ohne die Leseregeln aufzugeben (den Kopf des Records
  herauskopieren und prüfen, dann die Ebenen direkt in das Frame lesen): eine Erweiterung von `takt-ipc`, nicht
  gebaut (`takt/takt-decode-eine-kopie-je-bild-sparen`). Ob der Hilfsprozess für 4K bleibt, entscheidet erst die
  Messung des Gates (CPU-Last über einen Film gegen `mpv`); der Hardware-Pfad geht diesen Weg ohnehin nicht.

**Pool-Erschöpfung ist ein Messwert.** Der Pool wächst nie: Sind alle Frames ausgegeben, bleibt das nächste Bild im
Ring, `Decoder::try_next` meldet `PoolExhausted`, und `DecodeStats::pool_exhausted` zählt den Stau (einmal je Stau,
nicht je Nachfrage). Der Hilfsprozess findet den Ring voll und nimmt keine Pakete mehr an, das Paket-Fenster des
Hauptprozesses läuft voll, und der Demuxer liest nicht weiter: der Stau reicht von selbst bis zur Quelle zurück.
Gemessen mit einem Verbraucher, der jedes Bild 20 ms hält (1080p60 H.264, Pool von 4 Frames, `decode-speed --pool 4
--hold-ms 20`): 1200 Bilder in 7,07 s (170 Bilder/s, was vier Frames in 20 ms hergeben), **783-mal Pool leer, ein
Pool über den ganzen Lauf, kein Bild verloren, kein Fehler**. Als Test:
`an_empty_pool_stalls_the_decoder_and_nothing_is_lost_or_allocated` (Pool von zwei Frames: alle 50 Bilder kommen in
der richtigen Reihenfolge durch dieselben zwei Speicherbereiche) und `a_backend_whose_frames_are_held_stalls_and_counts_it`. Im Player steht der
Zähler in der Quellzeile des Overlays (`pool empty Nx`).

**Im Fenster** (Rauchtest, kein Messlauf: Debug-Build, 6 s je Datei, `TAKT_PIPELINE=own takt-player --seconds 6
<Datei>`, 60 Hz, derselbe Rechner): 1080p60 H.264 läuft über die eigene Pipeline als NV12 durch die Lage von
`takt-gpu` (345 Bilder gezeigt, 4 wiederholt, 5 verworfen, mittlerer Präsentationsfehler 0,66 ms, Pool nie leer).
4K60 HEVC 10 bit kommt als P010 an und wird gezeigt, hält im Debug-Build aber die 60 Bilder/s nicht (332 gezeigt,
19 verworfen, 15 wiederholt; die Kopien kosten dort 13,6 ms je Bild, im Release-Build 8,5 ms). Ein Messlauf mit
Release-Build, fünf Minuten und dem Vergleich gegen `mpv` steht aus (`takt/eigene-pipeline-timing-und-cpu-last-messen`),
ebenso die Abnahme von Hand (`takt/eigene-pipeline-im-fenster-von-hand-abnehmen`).

Nicht gemessen: die CPU-Last des ganzen Prozessbaums beim Abspielen gegen `mpv` (die Zeile des Gates), das alte
Testgerät, Linux, das Frame-Timing des Fensters mit der eigenen Pipeline (`scripts/ab-measure.py` kennt sie noch
nicht), die Latenz eines Bildes bei leerem Ring (abgefragt wird mit Pausen bis 1 ms).

### Bildgleichheit gegen libmpv

libmpv ist das Referenz-Orakel (`takt-backend`). `apps/takt-player/src/compare.rs` vergleicht, was beide Backends
zum selben Zeitstempel zeigen: libmpv wird angehalten und springt exakt auf die Zeit eines Bildes; seine Position
muss genau diese Zeit sein, und das Bild, das es dann zeigt, wird mit dem Bild verglichen, das die eigene Pipeline
mit genau dieser Zeit stempelt. (Im laufenden Abspielen stempelt das `MpvBackend` ein Bild mit der Position von mpv
beim Rendern; auf einer belasteten Maschine ist das gelegentlich die Zeit eines Nachbarn, deshalb der Weg über den
Sprung.) Die beiden geben nicht dieselben Bytes heraus: die eigene Pipeline das Y'CbCr des Decoders, mpv ein fertiges
R'G'B'-Bild (`rgb0`, von seinem Software-Renderer umgerechnet). Der Test rechnet das dekodierte Bild als BT.709 im
begrenzten Wertebereich mit interpoliertem Chroma um und misst den Abstand als PSNR über R', G' und B' aller Pixel.

Gemessen am 2026-11-17 (Windows 11, FFmpeg 9.0 LGPL von BtbN, mpv 0.41.0-dev von shinchiro, Build 20261111), je
Datei 18 der 50 Bilder (jedes dritte und das letzte), fünf Läufe mit denselben Werten bis auf ±0,1 dB:

| Datei (`scripts/make-decode-clips.py`) | Decoder | niedrigster PSNR | mittlerer PSNR |
|---|---|---|---|
| `decode-h264.mp4`, 160x90, 8 bit, B-Frames | `h264` | 55,98 dB | 56,23 dB |
| `decode-hevc.mp4`, 160x90, 8 bit, B-Frames | `hevc` | 55,94 dB | 56,07 dB |
| `decode-av1.mkv`, 160x90, 8 bit | `libdav1d` | 56,01 dB | 56,18 dB |
| `decode-hevc-10bit.mp4`, 130x74, 10 bit | `hevc` | 51,79 dB | 52,00 dB |
| `decode-av1-10bit.mkv`, 130x74, 10 bit | `libdav1d` | 51,94 dB | 52,12 dB |

Zum Vergleich, mit demselben Maß (`a_picture_one_frame_off_or_of_the_wrong_matrix_fails_the_threshold`): das Bild
neben dem richtigen hat 21,7 dB (die Clips bewegen sich), das richtige Bild mit der Matrix von BT.601 gelesen
28,9 dB.

**Die Schwelle** (`MIN_PSNR_DB`, `MIN_PSNR_DB_10BIT`): **45 dB in 8 bit, 42 dB in 10 bit.** Sie liegt 10 dB unter
dem Gemessenen (Luft für einen anderen Build von mpv oder FFmpeg, für andere Rundung und anderes Chroma-Upsampling)
und 13 dB über dem nächsten Fehlerbild (falsche Matrix); ein Bild daneben oder ein vertauschter Kanal liegt 20 dB
darunter. Der Rest zu „gleich“ ist kein Decoder-Unterschied (die dekodierten Bytes sind die von FFmpeg, siehe oben),
sondern die Umrechnung: mpv interpoliert Chroma anders als die Referenz hier, und bei 10 bit bringt es die Werte mit
einem Dither-Muster auf 8 bit, das die Referenz nicht nachahmt. In der CI läuft der Vergleich im Job „libmpv gelinkt
(Windows, Archiv gepinnt)“, der dafür auch das FFmpeg-Archiv lädt.

Nicht verglichen: HDR (das `MpvBackend` schneidet bei Weiß ab, `takt-gpu` bildet ab; ein Vergleich wäre einer der
Tone-Mapper), 4:4:4, echte Filme, Linux (die libmpv der Distribution lädt eine GPL-FFmpeg; zwei FFmpeg in einem Lauf
sind dort ABI-Glück, `docs/BUILD.md`).

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

### Compositing in takt-gpu (2026-11-17)

`takt/takt-gpu-compositing-farbraum-tonemapping`: der Render-Code des Players ist das Crate `takt-gpu`
([GPU.md](../GPU.md)), mit einem Compositor für mehrere Lagen. Gemessen mit
`cargo run --release -p takt-gpu --example composite-bench`: 4K-Quellen in ein 4K-Ziel (`Rgba8Unorm`), jede
Video-Lage mit einem neuen Bild je Durchlauf aus einem Ring von drei, 30 s je Lauf (1800 Bilder). Die vier Lagen:
NV12 BT.709 flächig, P010 BT.2020 PQ mit Tonemapping flächig bei halber Deckkraft, NV12 als Bild im Bild, eine
RGBA-Grafik mit Alpha flächig; bei 8 und 16 Lagen wiederholt. „Importiert“ heißt: jedes Bild ist eine Textur, die
ein D3D11-Gerät geschrieben hat und die über wgpu-hal importiert ist (der Weg eines Hardware-Decoders, ohne
Decoder). Gemessen wird der Compositor, kein Decoder.

Kopflos ist alle 1/60 s ein Bild fällig; verspätet ist es, wenn die GPU es nicht fertig hat, bevor das nächste
fällig ist. Mit Fenster (1280x720, vsync, auf dem 60-Hz-Bildschirm) ist ein Bild verspätet, wenn seit dem vorigen
mehr als anderthalb Bildperioden vergangen sind. „GPU-Zeit“ ist die Zeit von der Einreichung bis die GPU fertig
ist. Hardware wie beim Spike: i5-12400F, RTX 4070 SUPER (Treiber 610.88), Windows 11, DX12.

**Die Maschine war nicht ruhig**: während aller Läufe lasteten Builds und Tests anderer Sitzungen die CPU zu 60
bis 100 % aus (Mittel je Lauf in der Spalte), und auf der GPU lag ein `llama-server` mit 9,6 GB VRAM. Der
Messprozess lief deshalb mit der Prioritätsklasse „High“, so wie der Player seinen Render-Thread auf
Echtzeit-Priorität hebt; die letzte Zeile zeigt, was ohne das unter dieser Last passiert.

| Lagen | Quelle | Anzeige | Läufe | Verspätet je Lauf (von 1800) | GPU-Zeit Median | GPU-Zeit p99 | GPU-Zeit max | GPU-Last | CPU der Maschine |
|---|---|---|---|---|---|---|---|---|---|
| 4 | importiert | kopflos | 5 | 0, 0, 1, 0, 0 | 4,0 bis 5,1 ms | 7,8 bis 10,9 ms | 12,9 bis 15,9 ms | 26 bis 30 % | 80 bis 99 % |
| 8 | importiert | kopflos | 1 | 0 | 6,4 ms | 9,9 ms | 11,0 ms | 30 % | 75 % |
| 16 | importiert | kopflos | 1 | 0 | 6,3 ms | 7,4 ms | 9,0 ms | 32 % | 100 % |
| 4 | importiert | Fenster, vsync | 10 | 0, 0, 6, 0, 7, 9, 1, 0, 0, 4 | 5,7 bis 6,1 ms | 8,4 bis 11,8 ms | 9,5 bis 54,5 ms | 27 bis 34 % | 60 bis 96 % |
| 4 | Upload aus dem Systemspeicher | kopflos | 1 | 598 | 3,1 ms | 12,3 ms | 15,1 ms | 22 % | 88 % |
| 4 | importiert, Priorität „Normal“ | kopflos | 3 | 159, 734, 612 | 1,0 bis 5,9 ms | 19,7 bis 50,6 ms | 56,7 bis 156,8 ms | 7 bis 21 % | 99 % |

Zweite Serie am selben Abend mit langen Läufen, nachdem die andere takt-Sitzung ihre Builds angehalten hatte.
Ruhig war die Maschine auch dann nicht: selbst gehostete CI-Runner anderer Repositories, ein `cargo test` und
ein Mobile-Build liefen weiter.

| Lagen | Quelle | Anzeige | Dauer | Priorität | Verspätet | GPU-Zeit Median | GPU-Zeit p99 | GPU-Zeit max | Längster Abstand | CPU der Maschine |
|---|---|---|---|---|---|---|---|---|---|---|
| 4 | importiert | kopflos | 300 s | Normal | 319 von 18 000 (1,8 %) | 4,2 ms | 13,2 ms | 38,2 ms | 53,2 ms | 74 % |
| 4 | importiert | kopflos | 180 s | High | 8 von 10 800 (0,07 %) | 4,1 ms | 10,7 ms | 20,6 ms | 31,6 ms | 88 % |
| 4 | importiert | Fenster, vsync | 300 s | High | 16 von 17 996 (0,09 %) | 6,3 ms | 10,0 ms | 41,3 ms | 53,6 ms | 77 % |

Was die Zahlen sagen:

- **Das Compositing selbst hält 4K60 mit Abstand.** Vier Lagen kosten im Median 4 bis 6 ms GPU-Zeit je Bild, im
  p99 unter 12 ms, bei 16,7 ms Budget; acht und sechzehn Lagen liegen im selben Bereich (die GPU taktet unter
  mehr Last höher, deshalb fällt das p99 mit der Lagenzahl nicht ab). Kopflos war in sieben Läufen mit 4, 8 und
  16 Lagen ein Bild von 12 600 verspätet.
- **Im Fenster gab es in fünf von zehn Läufen einzelne Aussetzer** (1 bis 9 Bilder von 1800, höchstens 0,5 %), in
  zwei Läufen als ein Stillstand von 1,7 und 1,9 s. Die GPU-Zeit je Bild war in diesen Läufen dieselbe wie in den
  anderen; es sind einzelne Ereignisse der Präsentation, nicht die Kosten der Lagen. Was sie auslöst, ist nicht
  belegt (Vermutung: Fenster anderer Sitzungen, die während der Läufe aufgingen und den Fokus nahmen, und die
  Last der Maschine).
- **Der Upload-Weg trägt drei 4K-Video-Lagen bei 60 Hz nicht**: 10,6 ms CPU je Bild für das Kopieren von rund
  50 MB in den Staging-Speicher von wgpu, ein Drittel der Bilder verspätet. Das ist der Fallback für eine Lage
  aus dem Software-Decoder, nicht der Weg für Mehrlagen-4K.
- **Ohne angehobene Priorität** verliert der Prozess unter dieser Last 9 bis 41 % der Bilder, bei einer GPU, die
  kaum zu tun hat: der Zeitplan des Betriebssystems, nicht das Compositing.

Für die Akzeptanz der Task („≥ 4 Lagen bei 4K60 ohne Frame-Drops auf der Entwicklungsmaschine“) heißt das: das
Compositing hat ein Mehrfaches an Reserve, aber „ohne Frame-Drops“ ist auf dieser ausgelasteten Maschine nicht
sauber belegt (kopflos ein verspätetes Bild in 9000 bei vier Lagen, mit Fenster Aussetzer in der Hälfte der
Läufe; in der zweiten Serie über fünf Minuten mit Fenster 16 von 17 996, ohne Stillstand, der längste Abstand
zweier Bilder 54 ms). Eine Wiederholung auf wirklich ruhiger Maschine steht aus: die Last kommt auch von
CI-Runnern und Sitzungen anderer Repositories, die sich aus dieser Arbeit heraus nicht anhalten lassen. Rohdaten: im Vault unter `takt/ROADMAP/reports/2026-11-17_takt-gpu-compositing-messung.md`.

Nicht gemessen: mit echtem Decoder (das Bild entsteht hier vor der Uhr), Vollbild und 4K-Bildschirm (der
Bildschirm hat 1680x1050), Linux, macOS, andere Grafikkarten, mehr als 30 s je Lauf.

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

### Stand Software-Decoder

*Stand 2026-11-17, `takt-decode` (`takt/takt-decode-software-pfad`).*

Bitströme werden nie im Hauptprozess dekodiert. `takt_decode::Decoder` startet je Videostream
`takt-decode-helper`; nur dieses Programm linkt libavcodec (die Bibliothek `takt-decode` enthält kein FFmpeg und
kein `unsafe`, `#![forbid(unsafe_code)]`, der Test `the_client_links_no_ffmpeg` hält das für libavcodec fest). Der
Player linkt und lädt weder FFmpeg noch dav1d.

| Was | Stand | Beleg |
|---|---|---|
| Was der Hilfsprozess bekommt | nichts außer seinen Pipes und dem Ring: keine Datei, keinen Pfad, kein Handle. Die Pakete kommen vom Hauptprozess über die Steuer-Pipe | `takt-decode-helper/main.rs` (kein `take_file`), `client.rs` (kein `HelperCommand::file`) |
| Welche Decoder laufen | eine feste Liste: `h264`, `hevc`, und für AV1 `libdav1d`; ein anderer Codec startet gar keinen Hilfsprozess, und der Hilfsprozess prüft die Liste selbst noch einmal | `a_stream_the_software_path_does_not_decode_starts_no_helper`, `proto::CODECS`, `av1_goes_to_dav1d_and_other_codecs_nowhere` |
| Wie viel auf dem Weg zum Decoder liegen kann | höchstens 8 Pakete (Fenster), jedes höchstens 8 MiB: der Hilfsprozess meldet in jedem Record, wie viele Pakete er genommen hat; eine Zahl, die zurückläuft oder über das Gesendete hinausgeht, ist eine Lüge und beendet ihn | `a_helper_that_counts_packets_nobody_sent_has_lied` |
| Speicher, eigene Prozesse, Watchdog, Ende mit dem Elternprozess | wie beim Demuxer (`takt-ipc`), Speicher 3 GiB | Tests von `takt-ipc` |
| Netz, Dateien des Benutzers, Registry | **nicht eingeschränkt**, wie beim Demuxer; der Schritt zur Sandbox ist hier kleiner, weil der Prozess gar nichts öffnet | offen, siehe unten |

**Die Leseseite** (`takt_decode::intake`, dieselbe Regel wie beim Demuxer: Bytes aus einem Hilfsprozess sind fremde
Daten):

* Ein Frame-Record wird nur angenommen, wenn sein Format bekannt ist, Breite und Höhe in 1 bis 16384 liegen, jede
  Zeilenlänge mindestens eine Zeile und höchstens 4096 Bytes mehr ist und die Ebenen **genau** so lang sind, wie
  diese Geometrie es verlangt; reservierte Bytes sind null, ein Zeitstempel ist nie die „kein Wert“-Zahl. Erst dann
  wird kopiert, und kopiert wird in ein Frame, dessen Geometrie mit dem Record verglichen ist
  (`a_frame_whose_numbers_do_not_fit_its_bytes_is_refused`, `a_frame_of_another_geometry_is_not_written_to`).
* Samples sind Daten, keine Indizes: 10-bit-Werte gehen für P010 durch eine Maske, planar bleiben sie, wie sie
  kamen. Wer sie später als Index benutzt, muss sie begrenzen.
* Die Farbe eines Bildes sagt der Bitstrom; wo er nichts sagt, die Stream-Tabelle des Containers.
* Fehlertexte von libavcodec werden gekürzt und von Steuerzeichen befreit.
* Ein Bild, das nicht in den Ring passt, ein Pixelformat, das das Protokoll nicht trägt (4:2:2, 12 bit), und ein
  Paket, das der Decoder ablehnt, sind Fehler-Records: gezählt, der Strom läuft weiter
  (`a_picture_that_does_not_fit_the_ring_is_an_error_not_a_hang`, `damaged_packets_are_counted_and_the_decoder_goes_on`:
  jedes dritte Paket Rauschen, eines leer, eines halbiert; der Hilfsprozess überlebt alle drei Codecs und geht
  ordentlich).

**Was ein Absturz oder ein Hänger dort im Hauptprozess auslöst:** `DecodeError::Helper` mit dem Grund, wie beim
Demuxer; das `SoftwareBackend` meldet ihn einmal aus `next_frame` und ist danach am Ende. Die Datei wird mit zwei
neuen Hilfsprozessen neu geöffnet.

Nicht gebaut: ein Hilfsprozess, der mit Absicht lügt (wie `tests/liar.rs` beim Demuxer); die Regeln der Leseseite
sind über `Intake` ohne Prozess getestet und im Fuzz-Ziel.

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

Ziel `decode_reader` (seit 2026-11-17; der Harness ist `takt_decode::fuzzing::read_untrusted` und läuft mit festen
Eingaben, allen Schnitten und Bitkippern davon auch in `cargo test`). Es füttert, was der Hauptprozess vom
Decoder-Hilfsprozess liest, durch genau den Code, den der echte Decoder benutzt (`Intake`): den Kopf eines
Frame-Records, die Kopie der Ebenen in ein Frame des Pools (planar, NV12, P010), die Zähler des Paket-Fensters,
Folgen von Records mit vollem und leerem Pool und mit Formatwechsel, dazu was der Hilfsprozess von seinem
Elternprozess liest. Geprüft wird auch, was zugesagt ist: der Pool hält nie mehr Frames als seine Größe, ein Stau
wird einmal gezählt, ein Bild landet in einem Frame seiner Größe.

| Datum | Stand | Lauf | Ergebnis |
|---|---|---|---|
| 2026-11-17 | `3cd6845` (Parser und `Intake` seither unverändert) | 1 h (3601 s), WSL2 (Arch) auf STEVESPC, libFuzzer über cargo-fuzz mit Nightly, Eingaben bis 4096 Bytes, Startkorpus aus den echten Encodern (13 Eingaben; 2026-11-17, 18:52:31 bis 19:52:54 UTC) | **kein Fund**: 70 009 518 Läufe, im Mittel 19 441 je Sekunde, 4329 neue Eingaben (Korpus am Ende 563), kein Absturz, kein Artefakt, Spitze 514 MiB Speicher |

Der Fund zeigt auf `takt-core`: `Timestamp::cmp` und `Timestamp::rescale` multiplizieren ungeprüft in `i128`
und halten nur für Zeitbasen, deren Teile klein sind. Für alles, was aus dem Demuxer kommt, stellt die
Protokollgrenze das jetzt sicher; die Funktionen selbst sind unverändert (`takt/takt-core-timestamp-ueberlauf`).
Nach `989efe6` haben sich Parser nicht mehr geändert, nur der Umgang des Demuxers mit einem Fehler.

### Stand takt-gpu

Die Naht zwischen Decoder und GPU ([GPU.md](../GPU.md)) nimmt Daten an, die aus einem Hilfsprozess stammen können:

- **Ebenen im Systemspeicher**: jede Ebene wird gegen Format, Breite, Höhe und Stride geprüft, bevor ein Byte an
  `write_texture` geht (`PlanesView`); ein Format ohne Texturpfad wird abgelehnt.
- **Texturen**: `TextureFrame::new` prüft Format, Dimension, sichtbare Größe und Scheibe gegen die Textur selbst.
  Beim Import nennt der Aufrufer nur das erwartete Format; Größe und Scheiben liest `open_shared_texture` aus
  der Ressource (die Antwort des Treibers, nicht des Absenders) und prüft sie gegen die Limits des Geräts.
- **Handles**: ein Handle, das keine Textur bzw. kein Zaun dieses Adapters ist, lehnt Direct3D ab; der Test
  `gpu_a_handle_that_is_not_the_texture_it_is_said_to_be_is_refused` hält das fest.
- **`unsafe`** steht nur in `src/interop/dx12.rs` und `src/interop/d3d11.rs` (benannte FFI-Module, jeder Block
  mit `// SAFETY:`).

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
   verwerfende Samples) überträgt das Protokoll noch nicht (`takt/takt-demux-seitendaten-der-pakete`). Der
   Software-Decoder kam für die Testclips und für H.264, HEVC und AV1 aus MP4 und Matroska ohne sie aus: die
   Konfiguration steht in der Extradata des Streams bzw. im Bitstrom. Was fehlt, sind Dateien, die mitten im Strom
   die Konfiguration nur über Seitendaten wechseln, und HDR10+/Dolby-Vision-Metadaten für `takt-gpu`.
7. **Die Bindings sind an eine FFmpeg-Hauptversion gebunden** (9.0); Build und Hilfsprozess verweigern eine
   andere. Ein Versionssprung ist Handarbeit nach `crates/takt-demux/layout/README.md`.
8. **Nur eigene Testclips**: der Vergleich mit `ffprobe` läuft über Clips aus `scripts/`, nicht über echte
   Dateien mit vielen Spuren, Anhängen und alten Muxern (`takt/takt-demux-echte-dateien-gegen-ffprobe`). Dasselbe
   gilt für den Decoder: bitgleich zu `ffmpeg` und PSNR gegen libmpv sind an fünf erzeugten Clips belegt, nicht an
   Filmen (Interlacing, wechselnde Auflösung, kaputte Streams aus der Praxis).
9. **Die eigene Pipeline spielt nur das Bild.** Kein Ton (die Uhr läuft auf der stummen Tonausgabe des Players),
   keine Untertitel, keine Spurwahl; das Fenster sagt es. Ton ist `takt-audio` plus ein Audio-Decoder im
   Hilfsprozess, nicht Teil des Software-Pfads für Bilder (`takt/eigene-pipeline-ton`).
10. **Drei Kopien je Bild** (eine im Hilfsprozess, zwei im Hauptprozess), bei 4K60 10 bit rund ein halber Kern im
    Hauptprozess; eine davon ist einzusparen (siehe Performance, `takt/takt-decode-eine-kopie-je-bild-sparen`).
11. **Der Pumpen-Thread läuft mit normaler Priorität.** Die Vorgabe aus Phase 0 (der Decoder-Thread übernimmt die
    Echtzeitpriorität des Füll-Threads) ist nicht umgesetzt: `takt-decode` bindet `audio_thread_priority` nicht
    ein, und unter Nebenlast ist der Pfad nicht gemessen (`takt/eigene-pipeline-timing-und-cpu-last-messen`).
12. **Formate**: 4:2:0 in 8 und 10 bit und 4:4:4 in 8 bit. 4:2:2, 12 bit und Alpha meldet der Hilfsprozess als
    Fehler; VP9 steht nicht auf der Liste der Decoder (eine Zeile und eine Entscheidung, wie bei den Containern).
13. **Eine Installation bringt die Hilfsprogramme nicht mit** (`scripts/build-install.ps1`, das Rolling-Zip): die
    eigene Pipeline spielt aus dem Checkout. Das Zip ist ohnehin ohne FFmpeg gebaut
    (`takt/installation-bringt-hilfsprogramme-mit`).
14. **Der Vergleich mit libmpv läuft nur unter Windows** (Job mit gepinntem libmpv und gepinnter FFmpeg), siehe
    Bildgleichheit.
15. **Ein Zaun, der nie signalisiert wird, hält die Queue an.** `takt-gpu` lässt die Queue von wgpu auf der GPU
    auf den Zaun eines importierten Bildes warten (`Fence::wait_on`). Stirbt der Prozess, der ihn signalisieren
    soll, mitten im Bild, wartet die Queue für immer, und mit ihr das Fenster. Heute signalisiert nur Code im
    eigenen Prozess; mit einem Hardware-Decoder im Hilfsprozess braucht das eine Grenze
    (`takt/takt-gpu-zaun-eines-hilfsprozesses-begrenzen`).
16. **Der Import ist nur unter Windows auf DX12 umgesetzt und nur auf einer Nvidia-Karte und WARP geprüft.**
    Intel und AMD, der Texture-Array-Modus eines Decoders (die Scheibe im View ist vorbereitet, aber ungetestet)
    und Linux/macOS stehen aus (`takt/spike-zero-copy-linux-vulkan-import`).
17. **Tonemapping ohne Metadaten**: die Spitze der Quelle ist fest 1000 nits, weil `FrameMeta` weder MaxCLL noch
    die Mastering-Angaben trägt (`takt/takt-gpu-hdr-metadaten-und-chroma-lage`, zusammen mit der Chroma-Lage).

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
| 2026-11-17: Pakete gehen über die Steuer-Pipe zum Decoder, nur die Bilder über den Ring; ein Fenster von 8 Paketen begrenzt, was unterwegs ist | Der Ring von `takt-ipc` hat eine Richtung; Pakete sind klein gegen Bilder (4K60: unter 10 MiB/s gegen 1,4 GiB/s). Der Hilfsprozess meldet in jedem Record, wie viele Pakete er genommen hat; ist sein Ring voll, nimmt er keine, und der Stau reicht ohne eigenes Signal bis zum Demuxer | ja: ein zweiter Ring in `takt-ipc`, wenn die Pipe misst, dass sie stört |
| 2026-11-17: dav1d über den Decoder `libdav1d` von libavcodec, nicht über die API von dav1d | Die LGPL-Builds, gegen die der Hilfsprozess ohnehin linkt, bringen dav1d mit; ein zweites handgeschriebenes Binding und eine zweite Bibliothek zum Laden lieferten dieselben Bilder. libavcodecs eigener AV1-Decoder braucht Hardware und steht nicht auf der Liste | ja |
| 2026-11-17: der Pool hat eine feste Größe je Format; wechselt der Strom Größe, Format oder Farbe, entsteht ein neuer Pool | `FramePool` hat eine Geometrie. „Kein Nachallozieren“ gilt für die Erschöpfung (der Verbraucher kommt nicht nach), nicht für einen Strom, der andere Bilder liefert | ja |
| 2026-11-17: das `SoftwareBackend` positioniert in beiden Modi frame-genau | Die Uhr des Aufrufers steht am Ziel, nicht am Keyframe; Bilder vor dem Ziel werden gezählt und nicht kopiert. Preis: bei langen GOPs dekodiert ein Sprung bis zu einer GOP | ja, wenn ein Modus „zum Keyframe“ mit eigener Uhr gebraucht wird |
| 2026-11-17: nach dem Umschalten der Pipeline geht die Datei frame-genau weiter | Ein Sprung zum Keyframe davor würde eine lange GOP um Sekunden zurückwerfen (gemessen am Testclip mit Keyframes alle 4 s) | ja |
| 2026-11-17: die Bildgleichheit wird an angehaltenem libmpv mit exakten Sprüngen gemessen, nicht im Lauf | Im Lauf stempelt das `MpvBackend` mit der Position beim Rendern; unter Last ist das die Zeit eines Nachbarn, und ein Test, der davon abhängt, ist ein Lasttest | ja |
| `takt-gpu` bleibt auf wgpu; fremde Texturen kommen über wgpu-hal-Interop herein, nicht über `ash` direkt (2026-11-17, Empfehlung des Zero-Copy-Spikes) | Auf Windows gemessen: Import ohne CPU-Kopie gelingt mit dem gelockten wgpu 27.0.1, NV12 und P010, ohne Fork. Für Linux nach Quelltext von wgpu-hal 27.0.4 erwartbar (`vulkan::Device::texture_from_raw`), dort mit eigenem `ash`-Code für den DMA-BUF-Import auf dem Gerät von wgpu; nicht gemessen | Ja: zeigt der Linux-Prototyp eine Sackgasse (Layout importierter Bilder, Vulkan-Video-Queues), gilt „Neu bewerten“ aus der Technologie-Tabelle des KONZEPTs |
| Windows: D3D11VA mit geteilter Textur (`OpenSharedHandle`, eine GPU-Kopie) als erster Hardware-Weg, D3D12VA direkt (keine Kopie) als spätere Optimierung (2026-11-17, Empfehlung des Spikes) | Gleiche CPU-Last wie der direkte Weg, aber Decoder und Renderer teilen keinen Zustand an Referenzbildern, und FFmpegs D3D11VA spielt Material, an dem sein D3D12VA hier scheitert | Ja, hinter der Schnittstelle von `takt-decode` |
| `GpuFrame` trägt eine `wgpu::Texture` (NV12/P010, zwei Ebenen-Views) plus sichtbare Größe, Bereitschaftssignal und Rückgabe-Guard; der CPU-Puffer bleibt die zweite Variante (2026-11-17, Vorschlag des Spikes für `takt-gpu`) | Beide Wege enden im selben Shader und Bind-Group-Layout; das wgpu-Gerät entsteht vor dem Decoder und braucht `TEXTURE_FORMAT_NV12` und `TEXTURE_FORMAT_P010` | Ja, bis `takt-gpu` die Schnittstelle festschreibt |
| 2026-11-17: ein Shader (`layer.wgsl`) für alle Formate statt je einem für NV12 und `rgb0`; die Umrechnung reist als affine Matrix im Uniform | Zwei und drei Ebenen, 8 und 10 bit, limited und full, drei Matrizen und vier Transferpfade wären sonst ein Dutzend Shader; die CPU-Referenz (`colour.rs`) rechnet dieselbe Matrix, also gibt es eine Quelle der Zahlen | ja |
| 2026-11-17: HDR nach SDR mit der EETF aus BT.2390 auf der größten Komponente, Zielweiß 203 nits (BT.2408), Spitze der Quelle 1000 nits ohne Metadaten | Eine genormte Kurve mit nachrechenbaren Punkten statt einer eigenen; die größte Komponente hält den Farbton und lässt nichts über das Zielweiß laufen | ja, `ToneMap` je Lage |
| 2026-11-17: gemischt wird auf den Werten des Ziels (gamma-kodiert bei `Unorm`), nicht in einem linearen Zwischenraum | Der Player hat eine Lage und braucht keinen; ein `Rgba16F`-Zwischenraum kommt mit der HDR-Ausgabe und dem Studio | ja |
| 2026-11-17: der Player bleibt beim Backend, das eframe wählt; DX12 nur mit `--imported` | Der Import braucht DX12, der Player hat aber noch keinen Hardware-Decoder; ein Wechsel des Backends ändert das gemessene Frame-Timing aus Phase 0 und gehört zur Entscheidung mit dem Decoder | ja, mit `takt/hardware-decode-vulkan-video-d3d` |
| 2026-11-17: Referenzbilder als PPM im Textformat (16 mal 8 Pixel), aus der CPU-Referenz erzeugt, im Repository | Eine Änderung der Farbrechnung zeigt sich als Diff von Bildern; kein Bildformat-Crate nötig; Punkte daraus sind zusätzlich gegen die Normen geprüft | ja |
