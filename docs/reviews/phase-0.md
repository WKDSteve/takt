# Review Phase 0 — Performance und Sicherheit

*Erstellt 2026-08-23, offen bis zum Abschluss von Phase 0.*

Jede Phase endet mit einem Review, das drei Fragen beantwortet: Was hat die Architektur gelernt? Wo liegen jetzt die Performance-Grenzen? Welche Angriffsfläche ist neu entstanden? Dieses Dokument wird während der Phase fortgeschrieben, nicht erst am Ende geschrieben.

---

## Table of content

  - [Performance](#performance)
  - [Sicherheit und Angriffsflaeche](#sicherheit-und-angriffsflaeche)
  - [Was in dieser Phase bereits entschieden wurde](#was-in-dieser-phase-bereits-entschieden-wurde)
  - [Offene Punkte fuer Phase 1](#offene-punkte-fuer-phase-1)

---

## Performance

### Frame-Eigentümerschaft — entschieden, umgesetzt

Der teuerste vermeidbare Fehler wäre gewesen, Frames als `Vec<u8>` mit Aufrufer-Eigentum zu modellieren und den Pool später nachzurüsten. Ab Phase 1 ist ein Frame ein Platz in einem begrenzten Decoder-Ringpuffer; die Rückgabepflicht durchdringt dann die gesamte Pipeline.

Umgesetzt über `PooledFrame` mit Rückgabe beim `Drop` und `FramePool` mit harter Kapazitätsgrenze. **Ein erschöpfter Pool ist ein Messwert, kein Fehlerfall zum Wegabstrahieren** — er bedeutet, dass der Verbraucher nicht nachkommt. Stilles Nachallozieren würde genau die Information zerstören, die man braucht.

### Shared Memory — noch nicht relevant, aber vorgezeichnet

IPC gibt es in Phase 0 nicht. Ab Phase 2 (Encoder-Hilfsprozess) und Phase 4 (VST3-Bridge) schon, und dann gilt: Audio-Puffer über Shared Memory, Steuerung über einen getrennten Kanal. Der Grund ist Latenz — ein Audio-Puffer durch eine Pipe zu schicken kostet einen Kopiervorgang, den man bei 2,7 ms Budget nicht hat.

Was jetzt bereits stimmen muss: `AudioBuffer` ist **verschachtelt**, nicht planar, weil alle Betriebssystem-Schnittstellen verschachtelt arbeiten und eine Umsortierung an der Prozessgrenze ein vermeidbarer Kopiervorgang wäre.

### Scheduler und Thread-Priorität — Fallstrick identifiziert

Der Audio-Thread braucht erhöhte Priorität. Unter Linux erfordert `SCHED_FIFO` `CAP_SYS_NICE` oder eine Regel in `limits.conf`; auf einer nicht vorbereiteten Installation schlägt die Anforderung fehl.

Das ist der klassische Fall „funktioniert bei mir": auf einem für Audio konfigurierten Arch läuft es, auf Standard-Ubuntu nicht — und man sucht die Ursache von Xruns wochenlang im eigenen Code.

**Festgelegt:** Priorität anfordern, Fehlschlag erkennen, sichtbar melden, mit normaler Priorität weiterlaufen. Niemals stumm scheitern.

### Extrapolationsgrenze — bewusste Entscheidung gegen Bequemlichkeit

Die Audio-Master-Uhr interpoliert zwischen Callbacks mit der Systemuhr. Naheliegend wäre unbegrenzte Extrapolation. Umgesetzt ist eine Grenze bei etwa zwei Callback-Perioden: bleibt der Anker länger aus, hängt der Audio-Thread, und eine weiter extrapolierte Position wäre eine Behauptung ohne Grundlage. Stattdessen bleibt die Uhr stehen und `anchor_age_nanos()` macht das Problem sichtbar.

### Seqlock statt Mutex — gemessen, nicht vermutet

Der Nebenläufigkeitstest `nebenlaeufig_niemals_zerrissen` prüft über 200.000 Schreibvorgänge, dass die Invariante `nanos == samples * 1000` bei keinem Lesevorgang bricht. Ein `Mutex` hätte dieselbe Korrektheit geliefert, aber die Realtime-Invariante verletzt.

**Der Test hat trotzdem gelogen — auf x86.** Der erste Push bestand lokal (Windows/x86-64) alle 63 Tests, brach aber auf `macos-latest` (Apple Silicon, ARM64) mit genau diesem Test. Ursache: `publish()` benutzte einen `Release`-Store auf den Zähler, gefolgt von zwei `Relaxed`-Stores auf die Nutzdaten; `load()` einen `Acquire`-Load auf den Zähler, umschlossen von zwei `Relaxed`-Loads.

Das Problem: **`Release`/`Acquire` auf einer einzelnen Operation ordnet nur in eine Richtung.** Ein Release-Store garantiert, dass alles *davor* sichtbar wird, bevor er selbst sichtbar wird — er verhindert nicht, dass *nachfolgende* Relaxed-Stores vor ihn hochwandern. Symmetrisch beim Acquire-Load. x86 hat ein so starkes Speichermodell (TSO), dass diese Umordnung dort praktisch nie auftritt; ARM64 ordnet aggressiver um, und der Test riss den Lesevorgang tatsächlich auf (`assertion left == right failed: zerrissener Lesevorgang trotz Seqlock`, `left: 1282, right: 0`).

Behoben mit expliziten `fence(Ordering::Release)` bzw. `fence(Ordering::Acquire)` an der jeweils kritischen Stelle — ein Fence erzwingt die volle Ordnungsgarantie, statt sie stillschweigend von einer einzelnen Operation zu erwarten. Siehe [`crates/takt-clock/src/anchor.rs`](../../crates/takt-clock/src/anchor.rs).

**Das ist der Beleg für „Cross-Platform ab Commit eins" aus diesem Dokument, nicht nur eine Behauptung.** Ohne die ARM64-Maschine in der CI-Matrix wäre dieser Fehler unentdeckt geblieben, bis er live auf einem Apple-Silicon-Rechner als sporadisches Ruckeln aufgetaucht wäre — dem am schwersten diagnostizierbaren Fehlerbild überhaupt, weil es nicht reproduzierbar aussieht.

**Zweite Korrektur im selben Zug:** `AudioMasterClock::writer()` gab den Schreibgriff bisher beliebig oft heraus; nur ein Kommentar behauptete „genau ein Schreiber". Das Seqlock erhöht seinen Zähler nicht atomar und verträgt tatsächlich nur einen Schreiber — ein zweiter hätte ihn brechen können, ohne dass ein Test das gezeigt hätte. `writer()` liefert jetzt `Option<AudioClockWriter>` und gibt beim zweiten Aufruf `None` zurück; `AudioClockWriter` ist nicht mehr `Clone`. Eine Invariante, die vorher nur im Kommentar stand, erzwingt jetzt der Typ.

### API-Konsistenz — von rustdoc gefunden, nicht von einem Reviewer

Derselbe Push deckte einen zweiten, unabhängigen Fehler auf: `AudioClockWriter` wurde von der öffentlichen Methode `AudioMasterClock::writer()` zurückgegeben, war aber nie in `takt-clock/src/lib.rs` re-exportiert. Der Typ war damit von außerhalb des Crates technisch nicht benennbar, obwohl er Teil der öffentlichen Schnittstelle ist — eine Lücke, die niemandem auffällt, bis jemand versucht, den Rückgabewert in einer eigenen Funktionssignatur zu benennen.

`cargo doc` mit `RUSTDOCFLAGS="-D warnings"` (in der CI als eigener Prüfschritt „Doku baut ohne fehlende Verweise" hinterlegt) verweigerte den Build: `public documentation for 'writer' links to private item 'AudioClockWriter'`. Der Intra-Doc-Link `[`AudioClockWriter`]` im Doc-Kommentar von `writer()` war korrekt; rustdoc bewertete das Ziel zu Recht als nicht öffentlich erreichbar.

**Festgelegt:** `cargo doc --no-deps` mit `-D warnings` bleibt ein Pflichtschritt der CI, nicht nur eine Empfehlung. Es ist die billigste automatisierte Prüfung dafür, dass die öffentliche API in sich konsistent ist — deutlich billiger als der Moment, in dem ein externer Nutzer versucht, einen Typ zu benennen, den er nur indirekt aus einer Signatur kennt.

### A/B-Messprotokoll — Präsentationsfehler je Pfad

Die Zahlen, an denen Phase 1 gemessen wird (Leitprinzip 6, libmpv als Referenz-Orakel): dasselbe Testbild (1920×1080, bewegtes Muster), je Bildrate fünf Minuten, je Pfad eine Zeile. Wiederholbar mit `scripts/ab-measure.py`:

```sh
cargo build --release -p takt-player
python scripts/ab-measure.py --device "<stummes Ausgabegerät>"   # NullBackend, kopflos
python scripts/ab-measure.py --mpv-sw                            # libmpv über den Software-Renderer, kopflos
python scripts/ab-measure.py --mpv                               # mpv selbst, braucht einen echten Bildschirm
```

Die Pfade messen nicht dasselbe, deshalb steht bei jedem, was seine Zahl bedeutet:

* **NullBackend, kopflos** (`takt-player --headless --fps … --hz … --seconds …`): der Scheduler von Takt gegen die Audio-Master-Uhr, die der cpal-Callback des genannten Geräts stellt. Die Anzeige ist simuliert: ein Durchlauf je Periode auf festem Raster, kein vsync. Der Fehler ist der Abstand zwischen dem Zeitstempel des Frames und der Uhr in dem Durchlauf, der es nimmt. Das p99 ist die Obergrenze seiner Histogrammklasse (die Klassen verdoppeln sich, die Grenze ist höchstens doppelt so groß wie der wahre Wert) und nie größer als der größte Fehler des Laufs.
* **libmpv, Software-Renderer, kopflos** (`--headless --mpv QUELLE`, nur in einem Build mit `TAKT_LIBMPV_DIR`): libmpv dekodiert dasselbe Muster (aus lavfi, mit stummer Tonspur, `ao=null`) und hält seine eigene Uhr; Takt fragt je Durchlauf, ob ein Frame wartet, und rendert es in den Speicher (`rgb0`). Der Fehler ist `audio-pts − time-pos` von mpv in diesem Durchlauf, exakt aus allen Werten nach der ersten Sekunde (mpv beginnt eine Datei mit rund 30 ms Vorlauf des Bilds und schließt die Lücke über etwa 20 Frames).
* **MpvBackend, kopflos** (derselbe Aufruf seit dem Backend, 2026-11-14): die Schleife fragt nicht mehr libmpv direkt, sondern `MpvBackend::next_frame`, wie das Fenster. Der Fehler ist die Uhr des Backends (mpvs Tonposition) minus dem Zeitstempel des Frames, also dieselbe Größe; die Zeile nennt Rendern samt Kopie in das Pool-Frame (seit `22d5242` rendert mpv direkt in das Pool-Frame, die Zeilen heißen dann „Rendern“). Der Vorlauf ist die halbe Anzeigeperiode, solange `--mpv-early` nichts anderes sagt.
* **mpv selbst** (`mpv --video-sync=display-resample` und `--video-sync=audio`, `scripts/ab-mpv-stats.lua`): mpv im Vollbild gegen die echte Bildwiederholung. mpv führt kein Histogramm der Präsentationsfehler; die Zeile nennt stattdessen `vsync-jitter` und die Zähler `vo-delayed-frame-count`, `mistimed-frame-count` und `frame-drop-count`.

„wiederholt/min“ sind Durchläufe ohne neues Frame. „erwartet/min“ ist (Hz − fps) · 60: der Anteil, der aus dem Verhältnis von Bildrate und Bildwiederholung folgt und kein Fehler ist.

| Datum | Hardware | Quelle | Pfad | Anzeige | Mittel | p99 | wiederholt/min | erwartet/min | verworfen/min |
|---|---|---|---|---|---|---|---|---|---|
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 23,976 fps, 300 s | NullBackend, kopflos (Gerät) | 60 Hz simuliert | 4,19 ms | < 10,40 ms | 2161,5 | 2161,4 | 0,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 25,000 fps, 300 s | NullBackend, kopflos (Gerät) | 60 Hz simuliert | 4,10 ms | < 8,19 ms | 2098,8 | 2100,0 | 0,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 50,000 fps, 300 s | NullBackend, kopflos (Gerät) | 60 Hz simuliert | 4,11 ms | < 8,19 ms | 600,0 | 600,0 | 0,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 60,000 fps, 300 s | NullBackend, kopflos (Gerät) | 60 Hz simuliert | 3,42 ms | < 8,19 ms | 0,4 | 0,0 | 0,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 23,976 fps, 60 s | libmpv, Software-Renderer, kopflos (Uhr von mpv, Vorlauf 0,00 ms; Rendern p99 < 1,79 ms) | 60 Hz simuliert | 9,53 ms | 17,89 ms | 2162,0 | 2161,4 | 1,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 25,000 fps, 60 s | libmpv, Software-Renderer, kopflos (Uhr von mpv, Vorlauf 0,00 ms; Rendern p99 < 2,56 ms) | 60 Hz simuliert | 9,70 ms | 16,78 ms | 2101,0 | 2100,0 | 1,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 50,000 fps, 60 s | libmpv, Software-Renderer, kopflos (Uhr von mpv, Vorlauf 0,00 ms; Rendern p99 < 2,56 ms) | 60 Hz simuliert | 10,53 ms | 17,50 ms | 601,0 | 600,0 | 1,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 60,000 fps, 60 s | libmpv, Software-Renderer, kopflos (Uhr von mpv, Vorlauf 0,00 ms; Rendern p99 < 3,58 ms) | 60 Hz simuliert | 3,45 ms | 5,63 ms | 2,0 | 0,0 | 1,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 23,976 fps, 60 s | libmpv, Software-Renderer, kopflos (Uhr von mpv, Vorlauf 8,33 ms; Rendern p99 < 2,05 ms) | 60 Hz simuliert | 4,25 ms | 9,32 ms | 2161,0 | 2161,4 | 1,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 25,000 fps, 60 s | libmpv, Software-Renderer, kopflos (Uhr von mpv, Vorlauf 8,33 ms; Rendern p99 < 1,54 ms) | 60 Hz simuliert | 4,29 ms | 8,51 ms | 2100,0 | 2100,0 | 1,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 50,000 fps, 60 s | libmpv, Software-Renderer, kopflos (Uhr von mpv, Vorlauf 8,33 ms; Rendern p99 < 1,79 ms) | 60 Hz simuliert | 4,38 ms | 9,80 ms | 600,0 | 600,0 | 1,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 60,000 fps, 60 s | libmpv, Software-Renderer, kopflos (Uhr von mpv, Vorlauf 8,33 ms; Rendern p99 < 1,54 ms) | 60 Hz simuliert | 3,67 ms | 4,07 ms | 0,0 | 0,0 | 13,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 23,976 fps, 60 s | MpvBackend, kopflos (Uhr von mpv, Vorlauf 8,33 ms; Rendern und Kopie p99 < 3,58 ms) | 60 Hz simuliert | 4,58 ms | 10,99 ms | 2161,0 | 2161,4 | 1,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 25,000 fps, 60 s | MpvBackend, kopflos (Uhr von mpv, Vorlauf 8,33 ms; Rendern und Kopie p99 < 5,12 ms) | 60 Hz simuliert | 4,97 ms | 16,07 ms | 2090,0 | 2100,0 | 1,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 50,000 fps, 60 s | MpvBackend, kopflos (Uhr von mpv, Vorlauf 8,33 ms; Rendern und Kopie p99 < 4,10 ms) | 60 Hz simuliert | 4,40 ms | 9,22 ms | 597,0 | 600,0 | 1,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 60,000 fps, 60 s | MpvBackend, kopflos (Uhr von mpv, Vorlauf 8,33 ms; Rendern und Kopie p99 < 5,12 ms) | 60 Hz simuliert | 3,48 ms | 4,10 ms | 0,0 | 0,0 | 17,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 25,000 fps, 60 s, ruhige Maschine | MpvBackend, kopflos, ohne Kopie (Uhr von mpv, Vorlauf 8,33 ms; Rendern p99 < 3,58 ms) | 60 Hz simuliert | 4,22 ms | 9,78 ms | 2098,0 | 2100,0 | 1,0 |
| 2026-11-14 | STEVESPC (i5-12400F, RTX 4070 SUPER) | Testbild 60,000 fps, 60 s, ruhige Maschine | MpvBackend, kopflos, ohne Kopie (Uhr von mpv, Vorlauf 8,33 ms; Rendern p99 < 2,56 ms) | 60 Hz simuliert | 7,88 ms | 8,32 ms | 1,0 | 0,0 | 1,0 |

Was die Zahlen sagen:

* **Der Scheduler von Takt trifft die Theorie.** Geht jedes Frame an die Bildwiederholung, die seinem Zeitstempel am nächsten liegt, ist der Fehler gleichverteilt zwischen 0 und einer halben Periode: im Mittel ein Viertel von 16,67 ms, also 4,17 ms. Gemessen 4,1 bis 4,2 ms bei 23,976, 25 und 50 fps; das p99 liegt bei der halben Periode (8,3 ms; die Tabelle nennt eine Obergrenze). Bei 60 fps auf 60 Hz gibt es keine Schwebung mehr, der Fehler ist die (langsam wandernde) Phase zwischen Geräteuhr und Anzeige.
* **Keine versteckten Wiederholungen.** „wiederholt/min“ liegt in jeder Zeile innerhalb von 2 je Minute am Erwartungswert; verworfen wird beim NullBackend nichts.
* **libmpv ohne Vorlauf ist eine halbe Periode schlechter**, 9,5 bis 10,5 ms im Mittel bei 23,976, 25 und 50 fps: mpv gibt ein Frame erst zu seinem Zeitstempel heraus, und es wartet dann auf den nächsten Durchlauf (im Mittel 8,3 ms, dazu rund 1 ms Rendern). **Mit einer halben Anzeigeperiode Vorlauf (`video-timing-offset`) sind es 4,3 bis 4,4 ms**, derselbe Wert wie beim Scheduler von Takt. Das `MpvBackend` stellt also den Vorlauf auf die halbe gemessene Anzeigeperiode.
* **60 fps auf 60 Hz mit Vorlauf: 13 verworfene Frames in einer Minute.** Bildrate und Raster sind fast gleich, die Phase wandert, und gelegentlich sind zwei Frames in einem Durchlauf fertig; mpv verwirft dann eines. Das ist der Fall, für den mpv `display-resample` hat (die Tonrate folgt der Bildwiederholung), und der Grund, warum der Lauf mit mpv selbst auf einem echten Bildschirm noch daneben gehört.
* **Die Messung braucht eine ruhige Maschine.** Ein erster Durchgang, während dessen auf demselben Rechner gebaut wurde (auch mit niedrigster Priorität), zählte bei 25, 50 und 60 fps 56 bis 175 Xruns in fünf Minuten und Ausreißer bis 47 ms; diese Zeilen sind verworfen und ohne Nebenlast wiederholt (0 Xruns). Der Audio-Callback selbst hat Echtzeitpriorität (in einem ruhigen Lauf p99,99 unter 192 µs bei 10 ms Periode); leer lief der Ring vermutlich, weil der Thread, der ihn füllt, normale Priorität hat. Belegt ist nur der Zusammenhang mit der Nebenlast, nicht die Ursache. Das wird ein Punkt für Phase 1 (unten, Punkt 6).

### Spike: mpv über den Software-Renderer

`MpvBackend` hat zwei Wege zum Bild (`takt/mpv-backend-hinter-videobackend`): (a) mpv zeichnet per OpenGL in ein Ziel, das wgpu importiert, oder (b) mpv rendert in den Speicher und Takt lädt das Bild hoch. (b) ist einfach, aber `render.h` nennt den Software-Renderer selbst „extremely simple (but slow)“. Der Spike (`apps/takt-player/src/mpv`, `--headless --mpv`) beantwortet deshalb zwei Fragen, bevor das Backend gebaut wird.

**Frage 1: Trägt (b) 1080p bei 60 fps?** Abbruchkriterium: liegt das p99 der Render-Zeit je Frame über der halben Frame-Dauer bei 60 fps (8,3 ms), ist (b) kein Weg und das Backend braucht (a).

Gemessen (STEVESPC, Release, 1920×1080, acht Läufe à 60 s über vier Bildraten): Rendern je Frame p50 unter 0,9 bis 1,0 ms, p99 unter 1,5 bis 3,6 ms, größter Einzelwert 5,7 bis 9,2 ms. Das Kriterium ist um mehr als den Faktor zwei unterschritten: **(b) trägt 1080p**, Phase 0 nimmt diesen Weg. Die Grenze ist absehbar: das Testbild wird nur von YUV nach `rgb0` gewandelt, nicht skaliert; 4K hat viermal so viele Pixel, ein p99 um 10 ms hielte 60 fps nicht mehr. Zero-Copy über (a) bleibt damit Stoff für Phase 1 und wird für 4K gebraucht, nicht für 1080p.

**Frage 2: Wer entscheidet die Fälligkeit, mpv oder Takt?** mpv. Über die Render-API gibt es kein Frame mit Zeitstempel heraus, sondern meldet „ein Frame wartet“ (`mpv_render_context_update`), und zwar zu dessen Zeit auf mpvs eigener Uhr. `VideoBackend::set_display_period` hat für diesen Pfad nichts zu stellen. Ohne weiteres Zutun wartet ein Frame damit im Mittel eine halbe Anzeigeperiode auf den nächsten Durchlauf. Die Regel des `NullBackend` (ein Frame geht an die Bildwiederholung, die seinem Zeitstempel am nächsten liegt) lässt sich nachbilden: `video-timing-offset` auf die halbe Anzeigeperiode gibt jedes Frame so viel früher heraus (`--mpv-early MIKROSEKUNDEN`).

Gemessen (dieselben Läufe, Tabelle oben; 23,976, 25 und 50 fps): ohne Vorlauf im Mittel 9,5 bis 10,5 ms hinter mpvs Tonposition, mit 8,333 ms Vorlauf (halbe Periode bei 60 Hz) 4,3 bis 4,4 ms, bei unveränderter Zahl gezeigter Frames. Bei 60 fps auf 60 Hz steht die Phase fast fest, der Fehler ist dort mit und ohne Vorlauf rund 3,5 ms.

Was der Spike nicht beantwortete: den Upload des `rgb0`-Bilds in eine wgpu-Textur, Ton über ein echtes Gerät, Seek und Spurwahl. Das beantwortet der nächste Abschnitt. Offen bleibt Material, das teurer ist als das Testbild (HDR, 4K, Untertitel: `render.h` warnt, dass der Software-Renderer dort zu langsam werden kann).

### MpvBackend: der Bildweg ist `rgb0` — entschieden, umgesetzt

Entschieden am 2026-11-14 nach dem Spike (Auswahl des Autors): das Bild des `MpvBackend` bleibt `rgb0`. `takt-core` hat dafür `PixelFormat::Rgb0` (R, G, B und ein Füllbyte, das nie als Alpha gelesen wird), der Renderer einen zweiten Texturpfad neben NV12 (`render/gpu_rgb.rs`, `rgb0.wgsl`: eine `Rgba8Unorm`-Textur, der Shader gibt immer deckend aus). Die Alternative, die Fläche auf der CPU nach NV12 zu wandeln und bei einem Texturpfad zu bleiben, ist verworfen:

* **Keine doppelte Wandlung.** mpv hat das Bild schon von YUV nach RGB gewandelt. Zurück nach NV12 wäre je Frame eine zweite Wandlung auf der CPU, die der Shader anschließend wieder umkehrt.
* **Kein Verlust an Farbauflösung.** NV12 hat für je vier Pixel einen Farbwert. Das fertige Bild enthält eingezeichnete Untertitel und Kanten mit scharfen Farbwechseln, also genau das, was 4:2:0 sichtbar verschmiert.
* **RGB wird ohnehin gebraucht.** Bild-Modus, Capture und Compositing arbeiten in RGB; der Pfad ist keine Sackgasse von Phase 0.

Was gebaut ist (`apps/takt-player/src/mpv/backend.rs`, `session.rs`): mpv öffnet, dekodiert, gibt den Ton auf seinem eigenen Gerät aus und rendert jedes Frame direkt in ein `Rgb0`-Frame aus dem Pool, dessen Ebene auf 64 Byte ausgerichtet ist (bis `22d5242` in eine eigene Fläche, die das Backend in das Frame kopierte; siehe „Ohne Kopien bis zur Textur“ unten). Die Uhr ist eine `AudioMasterClock`, deren Anker mpvs Tonposition ist (`audio-pts`, einmal je Durchlauf). Den Vorlauf der Frames stellt das Backend zur Laufzeit auf die halbe gemessene Anzeigeperiode. Das Fenster spielt über eine `Session`, die entweder das Testbild oder eine Datei hält (Invariante 3 als Typ).

Gemessen (2026-11-14, STEVESPC, Release). Kopflos, dieselbe Schleife wie beim Spike, jetzt über das Backend (die vier Zeilen „MpvBackend, kopflos“ der Tabelle oben): mittlerer Fehler 4,4 bis 5,0 ms bei 23,976, 25 und 50 fps; Rendern samt Kopie in das Pool-Frame p50 unter 2,6 ms, p99 unter 3,6 bis 5,1 ms. Im Fenster, zwei Dateien mit H.264 und Ton, Ton auf einem stummen virtuellen Gerät („Voicemeeter In 4“):

| Datei | Anzeige | gezeigt | wiederholt | verworfen | Fehler Mittel | Rendern und Kopie p50 / p99 | Upload p50 / p99 |
|---|---|---|---|---|---|---|---|
| 1920×1080, H.264, 25 fps, 60 s | 60,0 Hz | 1495 | 2091 (58,3 %) | 1 | 4,537 ms | < 2,6 / < 4,1 ms | < 1,0 / < 1,5 ms |
| 1920×1080, H.264, 60 fps, 60 s | 58,8 Hz | 3578 | 3 (0,1 %) | 8 | 1,057 ms | < 2,6 / < 4,1 ms | < 1,0 / < 1,5 ms |

Lesart:

* **Der Fehler bleibt bei der Theorie des Schedulers** (ein Viertel der Anzeigeperiode, 4,17 ms bei 60 Hz), auch im Fenster mit echter Dekodierung und Ton auf einem Gerät (25 fps). Bei 60 fps auf 60 Hz steht die Phase fast fest, der Fehler ist dort kleiner und sagt wenig. Der Vorlauf lässt sich zur Laufzeit setzen: ein kopfloser Kurzlauf ohne ihn (25 fps, 20 s) maß 10,3 ms.
* **Die Kopie kostete so viel wie das Rendern** (Stand dieser Messung; inzwischen entfernt, siehe unten). Der Spike maß das Rendern allein mit p50 unter 1,0 ms; mit der Kopie der 8 MB in das Pool-Frame sind es 2,6 ms. Dazu kommt im Fenster eine zweite Kopie (Frame in das Bild, das der Paint-Callback trägt) und der Upload, der selbst in den Zwischenspeicher von wgpu kopiert. Für 1080p reicht das; es sind aber drei Durchgänge durch 8 MB je Frame, die mit ausgerichteten Pool-Frames und einem geteilten Frame auf einen fallen können (Punkt 7 unten).
* **Die Maschine war bei diesen Läufen nicht ruhig.** Eine zweite Sitzung arbeitete auf demselben Rechner; die Ausreißer (größter Einzelwert des Renderns bis 215,8 ms) sind damit nicht dem Backend zuzuschreiben, aber auch nicht von ihm zu trennen. Die Zeilen des Spikes oben sind die ruhigere Messung desselben Renderers; eine Wiederholung auf ruhiger Maschine gehört zum Handlauf des A/B-Protokolls.
* **60 fps im Fenster: 58,8 Hz und 8 verworfene Frames, bei Nebenlast.** Die Ursache war in diesem Lauf nicht belegt; der nächste Abschnitt beantwortet sie (es war die Nebenlast, nicht die Kopien). Der Lauf mit der 60-fps-Datei zeigte 3578 Frames in 60 s, mpv verwarf 8, und das Fenster maß als Median seiner Durchläufe 58,8 statt 60 Hz. Je Durchlauf fallen dort Rendern, zwei Kopien und der Upload an. Ob das die Durchläufe streckt oder die Nebenlast, sagt diese Messung nicht; sie gehört wiederholt, sobald die Kopien weg sind (Punkt 7).
* **Nicht gemessen:** das Bild auf dem Schirm (niemand hat hingesehen) und der Ton (das Gerät ist stumm). Beides ist die Abnahme von Hand.

### MpvBackend: ohne Kopien bis zur Textur

Seit `6901507` (takt-core) und `22d5242` (Player) wandert das Bild nicht mehr dreimal durch den Speicher:

* **`VideoFrame` hat ausgerichtete Ebenen.** Jede Ebene beginnt an einer durch 64 teilbaren Adresse (`takt_core::PLANE_ALIGN`), ihre Zeilenlänge (`stride`) ist ein Vielfaches davon. Ohne `unsafe`: eine Ebene ist ein `Vec` mit 64 Byte Vorrat und einem Versatz zum ersten ausgerichteten Byte. Für jeden Leser heißt das: eine Zeile ist `stride` Bytes lang, nicht Breite mal Bytes je Pixel; die genutzten Bytes nennt `row_bytes`. Der NV12-Pfad lud schon vorher zeilenweise mit `stride` hoch.
* **mpv rendert direkt in das Pool-Frame.** Die eigene Fläche (`Surface`) und die Kopie in das Frame entfallen; `ffi::Target` ist nur noch eine geprüfte Sicht auf Zeilen, die dem Frame gehören. Die 8K-Grenze bleibt (`ffi::fits`).
* **Der Renderer hält das Frame selbst.** Der Paint-Callback trägt das Pool-Frame (`render::Picture`), nicht mehr eine Kopie seiner Ebenen; das gilt für `rgb0` und für NV12. Das Frame geht in den Pool zurück, wenn das nächste Bild es ersetzt hat. Eine Quelle braucht damit zwei Frames (eines beim Renderer, eines in Arbeit); die Pools haben vier (`MpvBackend`) und sechs (Testbild).
* **Was bleibt:** `write_texture` kopiert in den Zwischenspeicher von wgpu. Das ist der eine Durchgang, den erst die Render-API in die GPU-Fläche erspart (Punkt 8 unten).

Vorher und nachher, beide am 2026-11-14 auf **ruhiger Maschine** (STEVESPC, Release, 1920×1080, je 60 s; Fenster: H.264 mit Ton auf „Voicemeeter In 4“, kopflos: Testbild aus lavfi, stumm). „Vorher“ ist der Stand `5626f17`, „nachher“ `22d5242`:

| Lauf | | Anzeige | gezeigt | verworfen | Fehler Mittel | Rendern p50 / p99 | Upload p50 / p99 |
|---|---|---|---|---|---|---|---|
| Fenster, 60 fps, Lauf 1 | vorher | 59,8 Hz | 3582 | 3 | 5,763 ms | < 1,8 / < 5,1 ms | < 0,64 / < 1,28 ms |
| Fenster, 60 fps, Lauf 2 | vorher | 60,0 Hz | 3585 | 1 | 7,725 ms | < 1,8 / < 4,1 ms | < 0,64 / < 1,28 ms |
| Fenster, 60 fps, Lauf 3 | vorher | 60,1 Hz | 3585 | 1 | 2,651 ms | < 1,8 / < 4,1 ms | < 0,64 / < 1,28 ms |
| Fenster, 60 fps, Lauf 1 | nachher | 60,0 Hz | 3582 | 4 | 7,512 ms | < 0,9 / < 3,6 ms | < 0,64 / < 1,54 ms |
| Fenster, 60 fps, Lauf 2 | nachher | 60,0 Hz | 3585 | 1 | 6,813 ms | < 0,9 / < 1,8 ms | < 0,64 / < 1,28 ms |
| Fenster, 60 fps, Lauf 3 | nachher | 60,0 Hz | 3585 | 1 | 1,182 ms | < 0,9 / < 3,6 ms | < 0,64 / < 1,79 ms |
| Fenster, 25 fps | vorher | 59,8 Hz | 1495 | 1 | 4,318 ms | < 1,8 / < 3,1 ms | < 0,64 / < 1,02 ms |
| Fenster, 25 fps | nachher | 59,9 Hz | 1495 | 1 | 4,304 ms | < 0,9 / < 3,6 ms | < 0,64 / < 1,54 ms |
| kopflos, 25 fps | vorher | 60 Hz simuliert | 1500 | 1 | 4,736 ms | < 2,0 / < 7,2 ms | – |
| kopflos, 25 fps | nachher | 60 Hz simuliert | 1500 | 1 | 4,220 ms | < 1,0 / < 3,6 ms | – |
| kopflos, 60 fps | vorher | 60 Hz simuliert | 3599 | 1 | 6,242 ms | < 1,8 / < 5,1 ms | – |
| kopflos, 60 fps | nachher | 60 Hz simuliert | 3599 | 1 | 7,883 ms | < 0,9 / < 2,6 ms | – |

Lesart:

* **Das Rendern eines Frames kostet die Hälfte.** p50 unter 0,9 statt unter 1,8 ms (Histogrammklassen, die Grenze ist die Obergrenze der Klasse), kopflos und im Fenster; das ist wieder der Wert, den der Spike für das Rendern allein maß. Die zweite Kopie (Frame in das Bild des Paint-Callbacks) lag außerhalb beider Messpunkte und ist nicht einzeln gemessen; sie war ein `to_vec` über 8 MB je Frame.
* **Der Upload ist unverändert,** wie erwartet: er lädt dieselben Zeilen hoch, nur aus dem Frame statt aus einer Kopie.
* **Die 8 verworfenen Frames und die 58,8 Hz lagen nicht an den Kopien.** Auf ruhiger Maschine misst das Fenster mit den Kopien 59,8 bis 60,1 Hz und mpv verwirft 1 bis 3 Frames in 60 s; ohne die Kopien 60,0 Hz und 1 bis 4. Der Lauf vom Vormittag (58,8 Hz, 8 verworfen) lief neben einer zweiten Sitzung, die auf demselben Rechner arbeitete. Die Vermutung „die Kopien strecken die Durchläufe“ ist damit widerlegt: mit denselben Kopien und ohne Nebenlast hält das Fenster 60 Hz.
* **Ein bis vier verworfene Frames je Minute bleiben bei 60 fps auf 60 Hz,** auch kopflos (1). Bildrate und Raster sind fast gleich, die Phase wandert, und gelegentlich sind zwei Frames in einem Durchlauf fertig; das ist der Fall, für den mpv `display-resample` hat (siehe oben beim Spike). Der mittlere Fehler streut bei dieser Rate von 1,2 bis 7,9 ms, je nachdem, wo die Phase steht, und sagt dort wenig.
* **Nicht gemessen:** Speicherbandbreite und CPU-Last des Prozesses, 4K, und jede Messung unter Nebenlast.

### MpvBackend: die Frames sind BT.709, was die Datei auch nennt

Die Annahme war, das Backend beschrifte seine Frames falsch: fest BT.709, obwohl die Datei BT.601 oder BT.2020 sein kann.
Gemessen ist es umgekehrt. Sechs Testdateien enthalten denselben kodierten Strom und unterscheiden sich nur in den
Farb-Tags im Kopf (`scripts/make-colour-clips.py`); was am Ende im Frame steht, kommt also allein von den Tags:

| Datei nennt | Pixel im Frame (R, G, B) | was mpv getan hat |
|---|---|---|
| BT.709, Matrix BT.709 | 56, 152, 195 | nur die Matrix, das Bild kommt an, wie es ist |
| BT.601 525 Zeilen | 77, 159, 192 | Matrix BT.601, Primärwerte nach BT.709 umgerechnet |
| BT.601 625 Zeilen | 54, 159, 192 | dieselbe Matrix, andere Primärwerte, anderes Rot |
| BT.2020, PQ | 0, 255, 255 | nach BT.709 umgerechnet, Helles bei Weiß abgeschnitten |
| BT.2020, HLG | 0, 255, 255 | ebenso |

- **mpvs Software-Renderer rechnet Primärwerte und Übertragungsfunktion selbst um**, nach BT.709 mit dem Gamma von
  BT.1886 (über zimg). Jedes Frame ist also BT.709, voller Wertebereich, und so steht es in `FrameMeta`. Die Werte der
  Datei dort einzutragen, wäre falsch gewesen.
- **Was die Datei nennt, steht jetzt daneben:** `MediaInfo::source_color` und `MpvBackend::source_color` (neuer Typ
  `takt_core::ColorDescription`). Unbekannte Namen bleiben `Unknown` und werden nicht geraten. Die Diagnosezeile nennt
  beides („file bt.2020/pq, shown as bt.709, hdr cut off at white“).
- **`ColorPrimaries::Bt601` gab es nur einmal**, obwohl 525 und 625 Zeilen verschiedene Primärwerte haben (die Tabelle
  zeigt den Unterschied im Rot). Es sind jetzt `Bt601Ntsc` und `Bt601Pal`; `DisplayP3` steht neben `DciP3`.
- **HDR wird nicht tonegemappt.** Was heller ist als SDR-Weiß, wird hart abgeschnitten, in 8 Bit. `rgb0` taugt für
  HDR nicht; der Weg dafür ist mpvs Render-API in die GPU-Fläche oder die eigene Pipeline (Phase 1). Entschieden am
  2026-11-14: bis dahin hinnehmen und sagen. Die Leiste nennt es einmal, wenn eine HDR-Datei geöffnet wird
  (`Session::limit_note`).
- **Gilt für libmpv mit zimg.** Gegenprobe mit `sws-allow-zimg=no`: dann wendet mpv nur die Matrix an und das Frame
  behält die Primärwerte der Datei. Die Tests `the_frames_are_bt709_whatever_primaries_the_file_names` und
  `hdr_is_cut_off_at_white_not_tone_mapped` schlagen auf einem solchen Build fehl. Grün auf dem Windows-Build
  (mpv 0.41) und auf Ubuntu mit dem libmpv aus apt. Seit dem 2026-11-15 merkt das auch die Laufzeit: das Backend
  lehnt einen Kern ab, der nicht umrechnet (`check_colour_conversion`: die Optionen `zimg-*` gibt es nur in einem Build
  mit zimg, und `sws-allow-zimg` muss an sein), statt seine Frames falsch als BT.709 zu beschriften; die Leiste nennt
  den Grund. Test `a_core_that_would_not_convert_the_colours_is_refused`.

### MpvBackend: Sprünge, Spuren, Formate

Gemessen mit `long-gop-12s.mkv` (`scripts/make-long-gop-clip.py`): Keyframes bei 0 s, 4 s und 8 s, und das Grau jedes
Bildes ist seine Frame-Nummer, sodass ein Test am Bild abliest, welches Frame es ist.

| Ziel des Sprungs | Fenster (`SeekMode::Keyframe`) landet bei | `SeekMode::Exact` landet bei |
|---|---|---|
| 6,5 s | 4,0 s | 6,52 s (das erste Frame ab dem Ziel) |
| 1,0 s | 0,0 s | 1,0 s |
| 7,96 s | 4,0 s | 7,96 s |
| 3,99 s | 0,0 s | – |
| 11,0 s | 8,0 s | – |

- **Der Sprung des Fensters landet auf dem Keyframe vor dem Ziel**, also nie weiter weg als ein Keyframe-Abstand und
  nie hinter dem Ziel.
- **Die Overlay-Zeile „seek“ las ein Frame zu spät** (4,04 s statt 4,0 s). Solange mpv unterwegs ist, nennt es als
  Position das Ziel, und das Bild des Sprungs trägt diesen Stempel. Gemessen wird jetzt mpvs Position in dem Moment,
  in dem seine Ereignisse die Ankunft melden (Sprung begonnen, Wiedergabe wieder aufgenommen); die Eigenschaft
  `seeking` allein nennt einen Sprung fertig, bevor er begonnen hat.
- **Im Lauf trägt jedes Frame seine eigene Zeit:** in der Messung stimmten 50 von 50 Frames mit ihrer Bildnummer
  überein; der Test `a_frame_carries_its_own_time_except_on_the_way_to_a_jump` prüft 20 und fünf nach einem Sprung.
- **Spuren wählt man aus einer Liste in der Leiste** („Audio“, „Subtitles“); `j` und `#` schalten weiter durch.
- **Vier weitere Paare aus Container und Codec** spielen mit dem richtigen Frame zur richtigen Zeit und nehmen einen
  Sprung: AV1 in Matroska, H.264 und HEVC in MP4, VP9 in WebM (je 2 s, 64x64, `scripts/make-format-clips.py`). Das
  belegt den Weg, nicht die Last: 1080p und 4K in diesen Codecs sind nicht gemessen.
- **Die zehn Sekunden, nach denen ein Öffnen aufgegeben wird, haben einen Test** (mit verkürzter Frist): der Grund
  kommt einmal, mpv ist danach untätig, die nächste Datei spielt.

### MpvBackend: ein Thread rendert, einer steuert

`render.h` verlangt einen Render-Thread, der nie auf mpvs Kern wartet. Wartet der Kern gerade darauf, dass ein Bild
gerendert wird (nach einem Sprung, beim Laden, beim Wechsel der Untertitel), und der Thread, der rendern sollte, stellt
stattdessen eine Frage, blockieren beide, bis mpv nach 200 ms aufgibt. Bis zum 2026-11-14 hat Takt im selben Thread
gerendert und gefragt.

| Stand | Sprung: Zeit bis zur Ankunft |
|---|---|
| ein Thread rendert und fragt (bis takt `505fa3e`) | etwa 200 ms |
| ein Thread, das wartende Bild wird zuerst gerendert (takt `1d88bc0`) | 4 bis 9 ms |
| eigener Render-Thread, der vor dem Rendern die Bildgröße abfragt (Zwischenstand, nie auf `main`) | etwa 205 ms |
| eigener Render-Thread, der erst rendert und danach fragt (takt `2f3c1f7`) | etwa 4,5 ms |

- **Öffnen bis zum ersten Bild**, mit dem Ablauf des Fensters (öffnen, je Durchlauf abfragen), vier kleine Dateien,
  beide Stände am selben Abend unter derselben Nebenlast gemessen: vorher 232 bis 1218 ms, mit dem Render-Thread 26
  bis 56 ms. Auf ruhiger Maschine nicht wiederholt.
- **Die Frames rendert jetzt ein eigener Thread des Backends.** mpv weckt ihn über seinen Update-Callback, er rendert
  das Bild sofort in ein Pool-Frame und legt es in ein Fach; `next_frame` nimmt das neueste. Der Thread des Fensters
  steuert: Kommandos, Eigenschaften, Ereignisse. Er darf auf den Kern warten, weil der Kern nicht mehr auf ihn wartet.
- **Auch der Render-Thread darf nicht fragen, solange ein Bild wartet.** Die dritte Zeile der Tabelle ist die
  Gegenprobe: dieselbe Abfrage (die Bildgröße) vor dem Rendern statt danach, und jeder Sprung kostete wieder 200 ms.
  Der Thread rendert deshalb in der Größe, die er kennt, liest danach Zeit und Größe und zeichnet dasselbe Bild noch
  einmal, wenn die Größe eine andere ist (das erste Bild einer Datei, ein Wechsel mitten in einer).
- **Der Zeitstempel bleibt eine Abfrage direkt nach dem Rendern.** Die Render-API nennt die Zeit eines Bildes nicht.
  Beobachtet statt abgefragt käme sie im anderen Thread an, ohne Zuordnung zum Bild; einen Durchlauf später gelesen
  wäre sie bei 60 fps oft schon die des nächsten Bildes. Der Test mit der Frame-Nummer im Bild bleibt grün.
- **Dauertest:** 300 Runden laden, springen, Untertitel umschalten und nachladen (rund 9000 Aufrufe), zweimal
  gelaufen: kein Hänger; einmal kein Aufruf über 150 ms, einmal einer mit 197 ms. Das ist der Rest: das nächste Bild
  beginnt zu warten, während der Render-Thread gerade nach der Zeit des letzten fragt. Während eine andere Sitzung
  baute, zählte derselbe Test 43 solche Aufrufe; die Zahl gilt also nur auf ruhiger Maschine.
- **`mpv_command_async` wird weiter nicht benutzt; der Render-Thread war nicht die Ursache.** Mit getrennten Threads
  noch einmal gemessen, je Lauf drei gleichzeitige `sub-add` und ein Laden, während acht anstehen:

  | Aufbau | Läufe | davon dauerhaft angehalten |
  |---|---|---|
  | `sub-add` asynchron, mehrere zugleich | 20 | 5 |
  | dazu: Laden wartet, bis die laufenden abgebrochen und beantwortet sind | 30 | 6 |
  | immer nur einer zur Zeit, ohne Abbruch und ohne fehlerhafte Datei (1200 `sub-add`, 400 Ladevorgänge) | 10 | 0 |
  | immer nur einer zur Zeit, mit Abbruch des laufenden vor dem Laden und einer Datei, die keine Untertitel ist | 40 | 12 |

  mpvs Kern (0.41, Windows-Build) bleibt stehen, sobald ein asynchrones `sub-add` abgebrochen oder von einem Laden
  eingeholt wird. `sub-add` wartet deshalb weiter auf mpv; an einem schlafenden Laufwerk hält das Lesen einer
  Untertiteldatei das Fenster so lange an.
- **Ohne Takt nachgestellt (2026-11-15).** `tools/mpv-sub-add-repro` ist ein kleines Programm nur gegen die C-API von
  libmpv (für `vo=libmpv` mit dem Software-Renderer in einem eigenen Thread, der nach jedem Bild `time-pos` fragt
  wie Takt; wahlweise mit dem Hook `on_load`, beantwortet wie Takt). Je Lauf ein Prozess, der nach 15 s als hängend
  gilt; 50 Läufe je Zeile. `all` ist die Folge des Takt-Tests (drei `sub-add`, eine davon keine Untertiteldatei,
  dann acht und sofort `loadfile`), `abort`, `bad` und `load` sind die drei Auslöser einzeln:

  | libmpv | `vo` | `all` | `abort` | `bad` | `load` |
  |---|---|---|---|---|---|
  | Windows, mpv 0.41 (`E:/deps/mpv-dev`, 20261111) | `null` | 0 | 0 | 0 | 0 |
  | Windows, mpv 0.41 | `libmpv` | 0 | 0 | 0 | 0 |
  | Windows, mpv 0.41, mit Hook `on_load` | `null` / `libmpv` | 0 / 0 | 0 / 0 | 0 / 0 | 0 / 0 |
  | Ubuntu 24.04, libmpv aus apt (2.2.0, mpv 0.37), Container | `null` | 0 | 0 | 0 | 0 |
  | Ubuntu 24.04, libmpv aus apt | `libmpv` | **4** | 0 | 0 | 0 |
  | Arch Linux, mpv 0.41.0 aus pacman, Container | `null` | 0 | 0 | 0 | 0 |
  | Arch Linux, mpv 0.41.0 | `libmpv` | **3** | 0 | 0 | 0 |

  Lesart: unter Linux hängt der Kern **ohne Takt**, auch mit dem aktuellen mpv 0.41.0, nur mit der Render-API und nur
  in der ganzen Folge, keiner der drei Auslöser allein. Im hängenden Lauf kommen alle Antworten der `sub-add`, dann
  beantwortet der Kern keine synchrone Frage mehr; sein Protokoll endet nach `seek done` (wie in Takt). Auf dem
  Windows-Build, mit dem Takt 20 bis 30 % Hänger zählte, hängt der Nachbau in 400 Läufen nie; dort liegt der
  Unterschied also in Takt (etwa die Fragen des Fensters je Durchlauf, die der Nachbau nicht stellt). Entschieden am
  2026-11-15: erst gegen ein aktuelles mpv prüfen, dann melden; die Meldung ist vorbereitet (tasks-vault,
  `takt/mpv-sub-add-haenger-bei-mpv-melden`), abgeschickt wird sie von Hand.

### A/V-Sync von Hand

Die Audio-Master-Uhr zieht die Latenz ab, die cpal für das Gerät vorhersagt; Wandler, Lautsprecher, Bluetooth und die Anzeige-Latenz des Monitors kennt sie nicht. Ob Bild und Ton zusammenfallen, sagt nur eine Messung von außen. Die Testdatei dafür:

```sh
python scripts/make-sync-clip.py            # takt-sync-clip.mkv im Temp-Verzeichnis, 60 s, 1920x1080, 60 fps
```

Das Bild ist schwarz und in den ersten zwei Frames jeder Sekunde weiß; der Ton ist still und spielt in den ersten 10 ms jeder Sekunde 1 kHz. Blitz und Klick beginnen auf demselben Zeitstempel. Gemessen wird mit einer Handykamera in Zeitlupe (240 fps: ein Bild sind 4,2 ms), die Bildschirm und Lautsprecher zugleich aufnimmt: in der Aufnahme die Bilder zwischen dem ersten weißen Bild und dem Einsatz des Klicks in der Tonspur zählen, über zehn Sekunden mitteln. Genauer geht es mit Mikrofon und Fotodiode an zwei Kanälen einer Soundkarte. Die Messung steht aus (`takt/a-v-sync-von-hand-messen-klick-und-blitz-lippensynchronitaet`). Seit dem `MpvBackend` spielt takt die Datei (`takt-player <Pfad>` in einem Build mit libmpv); im mpv-Pfad zieht mpv die Latenz seines Tongeräts selbst ab (`audio-pts`), nicht die Uhr von Takt.

### Was noch nicht gemessen ist

* CPU-Last und Speicherbandbreite unter echter Last. Seit dem `MpvBackend` dekodiert das Fenster echte Dateien (1080p H.264), gemessen sind aber nur Rendern, Kopie und Upload je Frame, nicht die Last des Prozesses über einen Film.
* ~~Callback-Laufzeit als Perzentile.~~ Gemessen seit `takt-audio`: der kopflose Lauf und das Overlay nennen p50, p99 und p99,99 des Callbacks (STEVESPC, WASAPI shared auf einem virtuellen Gerät, 480 Frames, 60 s: p50 unter 2 µs, p99 unter 5 µs, p99,99 unter 192 µs, Maximum 166 µs bei 10 ms Periode). **Der Median ist bedeutungslos; das 99,99. Perzentil entscheidet.**
* Startzeit der Anwendung. Der Player-Modus soll schlank starten; das ist derzeit eine Absicht ohne Zahl.
* mpv selbst auf einem echten Bildschirm (dritte Zeilengruppe des A/B-Protokolls) und der A/V-Versatz von Hand.

---

## Sicherheit und Angriffsflaeche

### Die Angriffsfläche ist von Anfang an groß

Das muss klar benannt werden: **Mediendateien sind ein klassischer Angriffsvektor.** Ein Demuxer, der auf einer manipulierten Datei läuft, ist exakt das Ziel, für das ein großer Teil aller FFmpeg-CVEs geschrieben wird. Container-Parser sind Zustandsmaschinen über nicht vertrauenswürdige Byteströme, in C geschrieben, mit jahrzehntelang gewachsenen Sonderfällen.

Ein Player ist damit sicherheitstechnisch näher an einem Browser als an einem Textverarbeitungsprogramm. Eine Datei zu öffnen, die man aus dem Internet hat, ist eine Vertrauensentscheidung — und die Software sollte sie nicht schlechter machen, als sie sein muss.

### Maßnahmen, die jetzt greifen

* **`#![forbid(unsafe_code)]` in allen Zone-1-Crates.** `takt-core` und `takt-clock` enthalten kein `unsafe`. Das ist billig, solange es keine FFI gibt — und genau deshalb wird es jetzt festgeschrieben, bevor es unbequem wird.
* **Kein Panic-Pfad in der Datenverarbeitung.** `Rational::new` und `Timestamp::rescale` geben `Option` zurück statt zu panicken. `Rational::lit` panickt und ist ausdrücklich nur für Konstanten im Quelltext dokumentiert, nie für Daten aus Dateien. Ein `panic!` auf eine manipulierte Datei ist eine Denial-of-Service-Lücke.
* **Überlaufsicherheit durchgehend.** Zeitumrechnungen laufen über `i128`, Additionen über `checked_add`, Histogramm-Zähler über `saturating_add`. Ein absichtlich absurder Zeitstempel in einer Datei darf keine arithmetische Panik auslösen.
* **Geometrie wird validiert.** `VideoFrame::alloc` lehnt Nullgrößen ab und rechnet Chroma-Ebenen mit `div_ceil`, statt ungerade Abmessungen falsch zu runden. Falsch berechnete Ebenengrößen sind die Standardquelle für Pufferüberläufe in Videocode.
* **`cargo-deny` in der CI** — auch als Sicherheitsprüfung, nicht nur für Lizenzen: `advisories` fängt bekannte Verwundbarkeiten in Abhängigkeiten.
* **Die Bildgröße aus der Datei ist begrenzt, bevor Speicher angefordert wird.** Der Messlauf über libmpv (`--headless --mpv`) legt seine Zielfläche nach der Größe an, die mpv für die Datei nennt; mehr als 8K (8192 × 4320) lehnt er ab, sonst ließe eine Datei, die nur ein riesiges Bild behauptet, den Player Gigabytes anfordern. Das `MpvBackend` hat dieselbe Grenze (`ffi::fits`, geprüft bevor der Pool für die Größe angelegt wird), auch wenn sich die Größe mitten in der Datei ändert.

### Der Prozessschnitt fällt mit dem Lizenzschnitt zusammen

Die wichtigste Erkenntnis dieses Reviews: **Demuxing ist langfristig ein Kandidat für einen eigenen Sandbox-Prozess** — und das ist genau derselbe Schnitt, den die Lizenz-Topologie schon aus anderen Gründen zieht.

```
Hauptanwendung
     |  IPC: dekodierte Frames über Shared Memory
     v
Demux- und Decode-Prozess (reduzierte Rechte, kein Netzwerk, kein Dateisystem
außer der geöffneten Datei)
     |
     v  linkt FFmpeg
nicht vertrauenswürdige Datei
```

Drei Gewinne aus einer Grenze: Lizenz-Isolation, Crash-Isolation, Sandbox. Chrome und Firefox machen es beim Media-Decoding genauso, und dort aus reiner Sicherheitsmotivation.

Für Phase 0 ist das nur eine Notiz. Aber die `VideoBackend`-Schnittstelle ist bereits so geschnitten, dass ein Prozesswechsel dahinter möglich bleibt: sie tauscht Frames und Zeitstempel aus, keine Zeiger auf Interna.

### Was noch nicht adressiert ist

* **Untertitel.** `.ass`-Dateien sind ein eigener Angriffsvektor mit eigener Skriptsprache. Wenn libass eingebunden wird, gehört es in dieselbe Sandbox wie der Demuxer. Mit dem `MpvBackend` liest libass in mpv die Untertitel; die Regel dafür (entschieden am 2026-11-14): **Formatierungs-Tags sind erlaubt** (kursiv, Position, Farbe; libass wertet sie aus, auch in SRT), **aber eine Untertiteldatei führt mpv weder zu einer anderen Datei noch ins Netz, und sie führt kein Skript aus.** Eine Untertiteldatei neben dem Film öffnet mpv von selbst, am Hook `on_load` vorbei; was sie aufhält, ist `access-references=no`. Der Test `a_subtitle_file_leads_mpv_to_no_other_file_and_not_into_the_network` belegt beides mit Gegenprobe: mit mpvs Standard (`access-references=yes`) zeigt eine `.srt`, deren Inhalt eine EDL von mpv ist, den Text der Datei, die sie nennt, und verbindet sich mit der Adresse, die sie nennt; mit der Einstellung des Backends ist sie keine Spur, und es kommt keine Verbindung an. Eine `.srt` mit dem Inhalt einer Playlist und eine `.ass` mit Adressen als Schrift und Bild verbinden sich in keinem Fall. Skripte lädt mpv nicht (`load-scripts=no`). Nicht geprüft: eingebettete Schriften einer MKV (libass lädt sie aus der Datei selbst) und Fehler im Parser von libass; dagegen hilft nur die Sandbox.
  * *Schriften und libass, nachgelesen am 2026-11-15 (mpv 0.41).* Was mpv dazu einstellen lässt, mit seinem Standard,
    den das Backend heute nicht ändert: `embeddedfonts` (an: Schriften aus den Anhängen einer MKV gehen an libass und
    über FreeType und HarfBuzz in den Prozess), `sub-fonts-dir` (leer: kein zusätzlicher Schriftordner) und
    `sub-font-provider` (`auto`:
    die Schriften des Systems über DirectWrite bzw. fontconfig), `sub-ass-override` (`scale`: die Stile der Datei
    gelten, nur die Größe wird angepasst; `strip` würde alle Tags entfernen), `sub-ass` (an). Grenzen gibt es zwei:
    `sub-glyph-limit` (Zahl der Glyphen im Cache von libass) und `sub-bitmap-max-size` (Größe des Bitmap-Caches), beide
    0, also der Standard von libass. Eine Grenze für die Rechenzeit eines Bildes gibt es nicht.
  * *Rechenzeit, gemessen am 2026-11-15.* Eine ASS-Datei mit 60 gleichzeitigen Zeilen mit `\blur40`, `\bord40` und
    Drehung braucht bis zu 190 ms je Bild (Test `subtitles_that_are_costly_to_draw_do_not_hold_up_the_window`). Der
    Render-Thread hält das Fenster dabei **doch** an: jede Frage des Fensters an den Kern (etwa `audio-pts` je
    Durchlauf) wartet, solange libass zeichnet, ein Durchlauf dauert dann so lange wie ein Bild. Der Test steht deshalb
    als bekannt rot auf `ignore`; Bug `takt/mpv-backend-teure-untertitel-halten-das-fenster-an`. Eine Datei kann den
    Player so verlangsamen, aber nicht über die Dauer ihrer Bilder hinaus anhalten.
  * *Entschieden am 2026-11-15:* eingebettete Schriften bleiben an (`embeddedfonts` wie mpvs Standard) und sind eine
    bekannte Fläche, bis der Prozessschnitt der Phase 1 Decoder und Untertitel aus dem Player-Prozess nimmt.
    Abschalten (`embeddedfonts=no`) hätte eine Fläche weniger gebracht, der Parser von libass und die Untertitel
    selbst blieben; der Preis wären sichtbar falsche Untertitel bei Anime und Karaoke gewesen.
* **Netzwerkquellen.** Ab HTTP, HLS oder RTSP kommt TLS-Zertifikatsprüfung dazu, plus die Frage, welche Protokolle überhaupt erlaubt sind. FFmpeg unterstützt mehr Protokolle als man einem Player geben will. Für Phase 0 ist die Antwort: keine. Das Fenster gibt mpv nur, was eine vorhandene Datei ist (`Sources::Files`): ein Text, der keine Datei benennt, erreicht `loadfile` nicht. Dieselbe Regel gilt für alles, was mpv danach von sich aus öffnet. Eine Playlist-Datei (`.m3u`, oder jede Datei, deren Inhalt eine ist) reicht ihre Einträge an mpv weiter, und mpv hält `http://`-Einträge für unbedenklich. Der Hook `on_load` hält mpv vor jeder Quelle an; ist sie keine Datei, bekommt mpv einen Namen, den nichts öffnet. **Die Protokoll-Allowlist von FFmpeg (`stream-lavf-o=protocol_whitelist=file`) leistet das nicht:** der Test `a_core_for_files_fetches_nothing_from_the_network` lauscht auf einem lokalen Port, und mit der Allowlist kam die Verbindung trotzdem an; mit dem Hook kommt keine. Skripte, Konfiguration des Nutzers und der ytdl-Helfer sind abgeschaltet (`config=no`, `load-scripts=no`, `ytdl=no`), ebenso Kapitel aus anderen Dateien und Referenzen auf andere Dateien (`ordered-chapters=no`, `access-references=no`). Der Messlauf `--headless --mpv QUELLE` reicht seine Quelle weiter ungefiltert an mpv (`Sources::Any`), also auch URLs und `av://`; das ist ein Werkzeug auf der Kommandozeile, kein Weg für Dateien aus dem Fenster. Nicht abgedeckt: was mpv innerhalb einer Mediendatei nachlädt, ohne dass es eine neue Quelle wird (Segmente einer HLS-Liste, die als lokale Datei vorliegt).
* **Pfadbehandlung.** Playlisten mit relativen Pfaden dürfen nicht aus dem erwarteten Verzeichnis herausführen. Eine Playlist-Datei spielt das `MpvBackend` deshalb gar nicht: sie endet für mpv mit der Übergabe ihrer Einträge, das Backend meldet sie als nicht verfügbar und lässt mpv anhalten (`stop`), sonst spielte mpv die Einträge ohne Bild weiter. Playlist-Dateien liest seit `ed29bf0` die Playlist von Takt, nicht mpv (`playlist::read_list`): eine `.m3u` oder `.m3u8`, die auf das Fenster fällt oder auf der Kommandozeile steht, wird als Text gelesen (höchstens ein Megabyte, höchstens 10 000 Einträge), und ihre Einträge kommen in die Playlist wie die Dateien eines Ordners. Ein Eintrag ist nur ein Pfad relativ zur Liste, der in ihrem Ordner bleibt. Gezählt, im Playlist-Fenster genannt und übersprungen werden: Adressen (jedes Protokoll, `http://`, `edl://`, `memory://`, `data:`), absolute Pfade (Laufwerk, Wurzel, Netzwerkfreigabe: die erreichte das Netz, bevor irgendein Backend gefragt wird), Pfade, die den Ordner über `..` verlassen, und Listen in Listen. Um das zu entscheiden, wird nichts geöffnet. Die Ablehnung im Backend und der Hook bleiben als zweite Linie, für eine Liste unter anderem Namen. Nicht geprüft: symbolische Verknüpfungen im Ordner der Liste, die hinausführen (der Pfad wird lexikalisch geprüft, nicht aufgelöst).
* **Fuzzing.** Sobald ein echter Demuxer existiert, gehört `cargo-fuzz` auf die Parser-Schicht. Vorher gibt es nichts zu fuzzen.

---

## Was in dieser Phase bereits entschieden wurde

| Entscheidung | Grund | Revidierbar? |
|---|---|---|
| Zeit rational, nie `f64` | Drift bei 24000/1001 und 44,1 kHz | nein, durchdringt alles |
| Frame-Pool mit Rückgabepflicht ab Commit eins | Hardware-Decoder ab Phase 1 | nein |
| Genau ein hart-echtzeitfähiger Thread | Audio-Deadline ist hart, Video-Deadline weich | nein |
| Seqlock für den Anker, kein Mutex | Audio-Thread darf nicht blockieren | ja, aber sinnlos |
| Ordnung über explizite Fences statt Release/Acquire auf Einzeloperationen | ARM64 zerriss sonst den Lesevorgang | nein, ohne Fence nicht plattformkorrekt |
| Schreibgriff einmalig ausgeben (`Option`, kein `Clone`) | Seqlock verträgt nur einen Schreiber | nein |
| Cross-Platform ab Commit eins, CI auf drei Zielen | Linux-Unterstützung als Alleinstellungsmerkmal | nein |
| `NullBackend` als dauerhaftes Werkzeug | Cross-Platform ohne Systemabhängigkeiten, Fehlerzuordnung | – |
| `forbid(unsafe_code)` in Zone 1 | jetzt billig, später unbequem | wäre ein Rückschritt |
| Bild des `MpvBackend` als `rgb0`, zweiter Texturpfad statt Wandlung nach NV12 | keine doppelte Wandlung, volle Farbauflösung an Untertiteln und Kanten, RGB wird für Bild-Modus, Capture und Compositing gebraucht | ja: mit mpvs Render-API direkt in die GPU-Fläche braucht mpv den Pfad nicht mehr, der Typ und der Texturpfad bleiben |
| Die Uhr im mpv-Pfad folgt mpvs Tonposition; `Session` hält Testbild oder Datei, nie beides | genau eine Uhr (Invariante 3), mpv ist in Phase 0 der Tonpfad | ja, mit der eigenen Pipeline in Phase 1 |
| Das Fenster öffnet über mpv nur vorhandene Dateien; `on_load` prüft, was mpv selbst öffnet | Playlist-Dateien führten sonst ins Netz | nein, höchstens um eine Allowlist erweitert |
| Playlist-Dateien liest Takt selbst; ein Eintrag ist nur ein relativer Pfad im Ordner der Liste (am 2026-11-14 gefragt und bestätigt: absolute Pfade bleiben draußen, auch lokale; ein Eintrag, der über eine Verknüpfung erreicht wird, also ein Symlink oder unter Windows eine Junction im Ordner der Liste, wird übersprungen, ohne dass der Verknüpfung gefolgt wird) | ein Backend, das Listen liest, spielt ihre Einträge am Fenster vorbei; absolute Pfade erreichen Netzwerkfreigaben | ja: absolute lokale Pfade ließen sich erlauben, Freigaben und Adressen nicht |
| Die Frames des `MpvBackend` sind BT.709; was die Datei nennt, steht daneben (`source_color`) | mpvs Software-Renderer rechnet selbst um, gemessen | ja, mit der Render-API in die GPU-Fläche liefert mpv das Bild nicht mehr als fertiges `rgb0` |
| Die Frames des `MpvBackend` rendert ein eigener Thread, der erst rendert und danach fragt; keine Kommandos mit späterer Antwort | `render.h` verlangt es, und gemessen kostet jede Frage bei wartendem Bild 200 ms; `mpv_command_async` hat den Kern angehalten, auch mit getrennten Threads | nein für den Thread; das asynchrone `sub-add` kann mit einer anderen mpv-Version neu geprüft werden |
| Eine Untertiteldatei darf Formatierung, aber keine andere Datei, kein Netz, kein Skript (`access-references=no`) | mit mpvs Standard führt eine `.srt` mit EDL-Inhalt zu beidem | nein |
| Schriften aus den Anhängen einer MKV bleiben an (`embeddedfonts`, 2026-11-15) | abschalten nähme nur eine von mehreren Flächen und machte Untertitel mancher Filme falsch; die Abhilfe ist der Prozessschnitt der Phase 1 | ja, mit der Sandbox |

---

## Offene Punkte fuer Phase 1

1. **Vulkan Video kann nicht die universelle Antwort sein.** macOS hat kein `VK_KHR_video_decode`; MoltenVK bildet Vulkan auf Metal ab, ohne die Video-Erweiterungen. VideoToolbox ist dort nicht eine Option, sondern der einzige Weg. Die Backend-Abstraktion in `takt-decode` bleibt damit zwingend, mit Vulkan Video als bevorzugtem, nicht als einzigem Pfad.
2. **Der Software-Pfad wird zuerst gebaut.** Er ist der Fallback, er läuft auf dem zehn Jahre alten Testgerät, und er ist die Referenz für die Korrektheit des Hardware-Pfads.
3. **Zero-Copy messen, nicht behaupten.** Zielgröße ist CPU-Last bei 4K60 AV1 gegen `mpv --hwdec=auto`. Ohne diese Zahl ist „Zero-Copy" eine Absichtserklärung.
4. **`assert_no_alloc` in den Audio-Callback**, sobald `takt-audio` existiert. Danach nie wieder entfernen.
5. **Erste `unsafe`-Blöcke werden kommen** — FFI ist ohne sie nicht möglich. Jeder braucht einen `// SAFETY:`-Kommentar, der die Invariante benennt, nicht die Absicht wiederholt. Der erste Fall ist da: `apps/takt-player/src/mpv/ffi.rs` (libmpv), das einzige Modul des Players mit `unsafe`, dahinter ein sicherer Typ.
6. **Wer den Audio-Ring füllt, darf nicht verhungern.** Der Callback hat Echtzeitpriorität, der Thread, der ihm die Daten bringt, nicht. Unter Last auf der Maschine lief der Ring im A/B-Protokoll leer (bis 175 Xruns in fünf Minuten); ohne Last kein einziger, bei einem Callback, der im p99,99 unter 192 µs bleibt. Für die eigene Pipeline heißt das: der Decoder-Thread braucht entweder eine erhöhte Priorität oder einen Vorrat im Ring, der eine verlorene Zeitscheibe überbrückt (heute 100 ms), und der Füllstand gehört ins Overlay.
7. ~~**Das Bild des `MpvBackend` wandert dreimal durch den Speicher**~~ Erledigt am 2026-11-14 (`6901507`, `22d5242`, Abschnitt „Ohne Kopien bis zur Textur“): es bleibt der eine Durchgang von `write_texture`. Der Stand davor: bevor es eine Textur ist (Fläche in das Pool-Frame, Frame in das Bild des Paint-Callbacks, Bild in den Zwischenspeicher von wgpu). Mit Pool-Frames, deren Ebenen auf 64 Byte ausgerichtet sind, rendert mpv direkt hinein, und ein geteiltes Frame erspart die zweite Kopie. Das ist eine Änderung an `VideoFrame` in `takt-core`, also vor Phase 1 zu entscheiden.
8. **4K braucht mpvs Render-API in die GPU-Fläche** (Weg (a) des Spikes) oder gleich die eigene Pipeline. Der Software-Renderer ist für 1080p gemessen, für 4K nur hochgerechnet.
