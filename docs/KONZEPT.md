# Takt — Konzept

*Stand: 2026-10-31. Lebendes Dokument.*

> **Aktueller Stand (2026-10-31):** Phase 0 läuft seit 2026-08-23. Fundament und Zeitmechanik stehen (`takt-core`, `takt-clock`, `takt-backend`, kopfloser `takt-player`; 64 Tests, CI grün auf Windows, Linux und macOS). Es fehlen noch Fenster, Audio-Ausgabe und `MpvBackend`. Siehe [PHASE-0.md](PHASE-0.md) und [reviews/phase-0.md](reviews/phase-0.md).
>
> **Planung** (Plan mit Etappen und Gates, Tasks, offene Entscheidungen, Architektur-Notizen) liegt nicht im Repo, sondern im Vault unter [`tasks-vault/takt/`](https://github.com/WKDSteve/tasks-vault/tree/main/takt).

**Takt** ist eine modulare Medien-Suite in Rust: ein gemeinsamer Kern aus Dekodierung, GPU-Compositing und Audio-Verarbeitung, auf dem vier Betriebsmodi aufsetzen — Player, Live-Studio, Video-Editor und, als Fernziel, eine Digital Audio Workstation.

Der Name ist Programm: *Takt* bedeutet gleichzeitig den musikalischen Takt und den Taktgeber. Die Master-Clock ist die eine Komponente, an der dieses Projekt technisch hängt. Alles Schwere in Takt ist ein Zeitproblem.

---

## Table of content

  - [Motivation und Zielbild](#motivation-und-zielbild)
  - [Nicht-Ziele](#nicht-ziele)
  - [Leitprinzipien](#leitprinzipien)
  - [Die vier Modi und ihre Zeitmodelle](#die-vier-modi-und-ihre-zeitmodelle)
  - [Crate-Topologie](#crate-topologie)
  - [Die drei harten Probleme](#die-drei-harten-probleme)
  - [Technologie-Entscheidungen](#technologie-entscheidungen)
  - [Lizenz-Topologie](#lizenz-topologie)
  - [Sprachen](#sprachen)
  - [Finanzierung, falls es je relevant wird](#finanzierung-falls-es-je-relevant-wird)
  - [Roadmap mit Gates](#roadmap-mit-gates)
  - [Phase 0 im Detail](#phase-0-im-detail)
  - [Test-Hardware-Matrix](#test-hardware-matrix)
  - [Getroffene Entscheidungen](#getroffene-entscheidungen)
  - [Offene Entscheidungen](#offene-entscheidungen)

---

## Motivation und Zielbild

Die Motivation ist ausdrücklich **nicht kommerziell**. Sie ist:

1. Freude an nativer Systementwicklung.
2. Langjähriges Interesse an Audio und Video.
3. Der Wunsch, Protokolle und Spezifikationen nicht nachzulesen, sondern zu implementieren — Bitstream-Formate, Codec-Container, Streaming-Protokolle, Plugin-ABIs, Audio-Schnittstellen des Betriebssystems.
4. Schwere technische Probleme lösen, mit einem Stack ohne zwanzig Jahre Altlasten.

Das ist wichtig festzuhalten, weil es jede spätere Prioritätsfrage entscheidet: **im Zweifel gewinnt das Stück Arbeit, das mehr über das Fachgebiet lehrt, nicht das, was schneller zu einem Feature führt.** Ein Projekt, das aus Interesse entsteht, darf sich Umwege leisten, die ein Produkt sich nicht leisten dürfte. Es darf sich aber keine Selbsttäuschung über den Zeithorizont leisten.

**Zielbild:** eine Anwendung, die als 50-MB-Player startet und per Moduswechsel zur Medien-Workstation aufsteigt, ohne die Ressourcen der nicht genutzten Modi zu belegen.

**Realistischer Zeithorizont** bei etwa 8 bis 12 Wochenstunden neben einer Vollzeitstelle (geplantes Budget seit 2026-10-30: 8 Stunden pro Woche, also eher das obere Ende der Kalenderzeiten):

| Ausbaustufe | Aufwand | Kalenderzeit |
|---|---|---|
| Player auf libmpv, täglich nutzbar | 100–200 h | 3–5 Monate |
| Eigene Pipeline statt libmpv | +300–500 h | +1 Jahr |
| Studio-Modus | 400–800 h | 1–1,5 Jahre |
| Editor-Modus | 600–1000 h | 1,5–2 Jahre |
| DAW-Schicht | 800–1500 h | 2–3 Jahre |

Die Vollausbaustufe ist damit ein **Projekt über vier bis acht Jahre**. VLC, mpv und OBS sind genau so entstanden. Wer mit der Erwartung „in zwölf Monaten fertig" startet, hört im siebten Monat frustriert auf.

---

## Nicht-Ziele

Bei einem Projekt dieser Größe sind die Nicht-Ziele wichtiger als die Ziele.

* **Kein Nachbau von VLC, OBS oder Cubase.** Deren Wert liegt in tausenden Randfällen aus Jahrzehnten. Der Anspruch ist eine saubere, moderne Kernarchitektur, nicht Featureparität.
* **Kein Nachbau von GIMP oder Krita.** Der Bild-Modus (beschlossen 2026-10-30) ist schlank: Ebenen, Farbraum und Export auf `takt-gpu`, keine Malprogramm-Werkzeugkiste.
* **Keine KI-Modelle vor ihrer Zeit.** Die KI-Schnittstelle (Protokoll, Datenmodell, Hilfsprozess-Rahmen) entsteht früh, weil sie sich nachträglich schlecht einziehen lässt. Die Modelle selbst kommen spät: Whisper-Untertitel nach Gate 0, Upscaling (Real-ESRGAN, Anime4K) und weitere Modelle ab Phase 3. Modelle laufen immer in einem eigenen Prozess.
* **Keine Wasm-Plugins für Realtime-Video.** Die Idee ist reizvoll, aber Wasm mit GPU-Zugriff bei Frame-Deadlines ist heute nicht praktikabel. Für Audio-DSP später ein möglicher Zweig.
* **Keine Mobilplattformen.** Android und iOS verdoppeln den Aufwand bei GPU- und Audio-Schnittstellen.
* **Keine eigenen Codecs.** Demuxer schreiben ist lehrreich, Codecs schreiben ist ein eigenes Lebenswerk.
* **Kein VST2.** Rechtlich nicht mehr erwerbbar. Erledigt, keine offene Frage.
* **Keine Cloud-Komponente**, solange keine konkrete Notwendigkeit besteht.

---

## Leitprinzipien

**1. Die Prozessgrenze ist Lizenzgrenze, Sprachgrenze und Fehlergrenze zugleich.**
Was in einem eigenen Prozess läuft und nur über IPC kommuniziert, bildet urheberrechtlich in der Regel kein Gesamtwerk, darf in einer anderen Sprache geschrieben sein und kann abstürzen, ohne die Anwendung mitzureißen. Diese dreifache Übereinstimmung ist selten und wird konsequent ausgenutzt: GPL-Encoder, VST3-Bridge und alles mit fremdem SDK wandern hinter eine Prozessgrenze.

**2. Ein Zeitmodell pro Modus. Keine Universalabstraktion.**
Der klassische Fehler bei Suiten dieser Art ist die eine Pipeline-Abstraktion, die Realtime-Wiedergabe, Timeline-Rendering und Audio-Callback-Determinismus gleichzeitig bedienen soll. Sie wird für alle drei schlecht. Geteilt werden **Bausteine**, nicht der Scheduler.

**3. Universell dekodieren, selektiv enkodieren.**
Dekodieren ist der lizenz- und patentrechtlich harmlose Pfad und muss alles können, was in freier Wildbahn vorkommt. Enkodieren ist der kritische Pfad und beschränkt sich auf Hardware-Encoder und permissiv lizenzierte Software-Encoder.

**4. Jede Schicht muss einzeln fertig werden können.**
Solo-Projekte sterben nicht an Technik, sondern an ausbleibenden Abschlüssen. Jedes Crate ist ein eigenständig nützliches, veröffentlichbares Stück Arbeit — auch wenn die Suite nie fertig wird.

**5. Messen statt glauben.**
Frame-Timing, Audio-Xruns, Speicherverbrauch und Latenz werden ab Phase 0 instrumentiert. In diesem Fachgebiet ist Bauchgefühl systematisch falsch, und ohne Zahlen kann man kein Gate ehrlich beantworten.

**6. libmpv als Referenz-Orakel, nicht als Sackgasse.**
Die eigene Pipeline wird gegen libmpv A/B-getestet — gleiche Datei, gleiche Frame-Zeitstempel, gleiche Ausgabe. Damit gibt es eine Korrektheitsreferenz statt nur eines subjektiven Eindrucks.

---

## Die vier Modi und ihre Zeitmodelle

Alle vier Modi machen im Kern dasselbe mit Bild und Ton. Sie unterscheiden sich ausschließlich im **Zeitverhalten** — und genau das ist der Grund, warum sie sich keinen Scheduler teilen können.

| | Player | Studio | Editor | DAW |
|---|---|---|---|---|
| **Taktquelle** | Audio-Clock, an Display-Refresh angeglichen | Wall-Clock | Timeline-Position | Audio-Callback |
| **Datenfluss** | Push, realtime | Push, realtime | Pull, nicht-linear | Push, hart-realtime |
| **Bei Überlast** | Frame verwerfen | Frame verwerfen (Stream darf nicht stocken) | warten (Qualität vor Tempo) | **darf nicht auftreten** — Xrun ist hörbar |
| **Deadline** | ~16,6 ms bei 60 fps, weich | ~16,6 ms, weich | keine | 2,7 ms bei 128 Samples / 48 kHz, hart |
| **Quellen** | eine Datei | viele Live-Eingänge | viele Dateien, beliebige Position | Spuren und Plugins |

Der Sprung von Video zu Audio ist der eigentliche Bruch: Video-Engines arbeiten **frame-basiert** und dürfen verwerfen. Audio-Engines arbeiten **puffer-basiert** und dürfen es nicht. Ein zwei Millisekunden zu spätes Video-Frame sieht niemand. Ein zwei Millisekunden zu später Audio-Puffer knackt hörbar.

**Konsequenz für die Architektur, ab Tag eins:**

```
  Audio-Thread (höchste Priorität)      Render-Thread (GPU)
  - keine Allokation                    - synchronisiert gegen die Audio-Clock
  - kein Lock                           - darf Frames verwerfen
  - kein File-I/O, kein Logging         - darf blockieren
  - keine Panik-Pfade                   
            |                                     ^
            +---- lock-freier SPSC-Ringpuffer ----+
```

Diese Trennung wird auch dann eingehalten, wenn die DAW-Stufe nie gebaut wird. Sie nachträglich einzuziehen bedeutet, die halbe Anwendung umzuschreiben.

Wichtig zur Einordnung: **Zwei-Thread-Modell ist keine Thread-Anzahl, sondern eine Invariante über Klassen.** Tatsächlich laufen deutlich mehr Threads — Demux, Decode-Pool, GPU-Submission, UI, später Encoder und Offline-Render. Entscheidend ist, dass es genau *einen* hart-echtzeitfähigen gibt und dass ihn keiner der anderen warten lässt. Und dieser eine wird nicht von uns erzeugt: das Audio-Subsystem ruft uns auf seinem Thread auf, mit seiner Puffergröße und seiner Priorität.

Modi sind dabei **Zustände, keine Threads**. Zu jedem Zeitpunkt gibt genau eine Uhr den Takt vor; zwei gleichzeitig laufende Modus-Uhren bedeuten zwei Antworten auf die Frage, welche Zeit jetzt ist.

Vollständige Thread-Aufstellung, Uhrenmodell und Invarianten: [THREADS-UND-UHREN.md](THREADS-UND-UHREN.md).

Der `Clock`-Trait ist die Stelle, an der die Modi auseinandergehen:

```rust
/// Rationale Zeitbasis, analog zu AVRational. Niemals f64 für Zeitstempel.
pub struct Rational { pub num: i64, pub den: i64 }

pub struct Timestamp { pub ticks: i64, pub base: Rational }

pub trait Clock {
    /// Aktuelle Position der Präsentationsuhr.
    fn now(&self) -> Timestamp;
    /// Wie lange bis zur Präsentation dieses Zeitstempels? Negativ = bereits zu spät.
    fn until(&self, pts: Timestamp) -> i64;
    /// Darf dieser Modus bei Verzug Frames verwerfen?
    fn may_drop(&self) -> bool;
}
```

Drei Implementierungen: `AudioMasterClock` (Player, DAW), `WallClock` (Studio), `TimelineClock` (Editor). Der Rest der Pipeline kennt nur den Trait.

---

## Crate-Topologie

Ein Repository, ein Cargo-Workspace, viele Crates. Jedes Crate trägt ab dem ersten Commit sein eigenes `license`-Feld und SPDX-Kopfzeilen. Ein Crate wandert erst dann in ein eigenes Repository, wenn es tatsächlich eine andere Lizenz braucht oder externe Nutzer hat.

```
takt/
  crates/
    takt-core/      Frame- und Sample-Typen, Rational, Timestamp, Fehlertypen
    takt-clock/     Clock-Trait und die drei Implementierungen
    takt-demux/     libavformat-Wrapper, Container-Parsing
    takt-decode/    Decode-Backends: SW, D3D11VA, VA-API, VideoToolbox, Vulkan Video
    takt-gpu/       wgpu-Device, Textur-Import, Farbraum, Tonemapping, Compositing
    takt-audio/     lock-freier SPSC-Ring, Resampler, Geräte-Backends
    takt-graph/     Node-Graph mit zwei Executoren: realtime und offline
    takt-fmt/       Bitstream-Parser: AV1-OBU, Untertitel, Metadaten
    takt-backend/   VideoBackend-Trait, NullBackend, Praesentations-Scheduler
  apps/
    takt-player/
    takt-studio/    ab Phase 2
    takt-edit/      ab Phase 3
  bridges/
    takt-vst3/      eigener Prozess, GPLv3 — ab Phase 4
    takt-x264/      eigener Prozess, GPLv3 — optional
  tools/
    takt-probe/     Analyse: Frame-Timing, Bitstream-Dump, Xrun-Zähler
```

Die Trennung von `takt-decode` und `takt-gpu` ist bewusst: die Naht zwischen beiden ist das schwerste Problem des Projekts und verdient eine explizite, testbare Schnittstelle statt einer stillschweigenden Kopplung.

Der zentrale Trait, hinter dem in Phase 0 libmpv und ab Phase 1 die eigene Pipeline steht:

```rust
pub trait VideoBackend: Send {
    fn open(&mut self, source: &Source) -> Result<MediaInfo>;
    fn seek(&mut self, to: Timestamp, mode: SeekMode) -> Result<()>;
    /// Nächstes Frame, sofern eines fällig ist. Blockiert nicht.
    fn next_frame(&mut self, clock: &dyn Clock) -> Result<Option<GpuFrame>>;
    fn stats(&self) -> BackendStats;   // dropped, decoded, queue depth
}
```

`GpuFrame` ist der Knackpunkt: idealerweise ein Handle auf eine bereits im VRAM liegende Textur, nur im Fallback ein CPU-Puffer.

---

## Die drei harten Probleme

### 1. Zero-Copy vom Hardware-Decoder zur GPU-Textur

**Was es ist.** Der Hardware-Decoder legt das dekodierte Frame im VRAM ab, meist als NV12. Der Renderer soll es dort verwenden, ohne es über den Systemspeicher zu schleifen.

**Warum es schwer ist.** Jede Plattform liefert einen anderen Handle-Typ, und `wgpu` kann externe Texturen nur eingeschränkt importieren. Konkret:

| Plattform | Decoder-Handle | Weg nach wgpu |
|---|---|---|
| Windows | `ID3D11Texture2D` (D3D11VA) | DXGI Shared Handle, D3D11-nach-D3D12-Interop über `wgpu-hal` |
| Linux | VA-API-Surface | `VK_KHR_external_memory_fd`, DMA-BUF-Import |
| macOS | `CVPixelBuffer` / `IOSurface` | Metal-Textur aus IOSurface |

Der CPU-Umweg kostet bei 4K60 zweistellige Prozent CPU-Last und macht das ganze „cutting edge"-Argument zunichte. Dies ist die Stelle, an der Gemini mit „Woche 1" um Größenordnungen danebenlag.

**Vorgehen.** Zuerst auf Linux mit Nvidia prototypen — `VK_KHR_video_decode` und `VK_KHR_external_memory_fd` sind dort sauber zugänglich, der Treiber ist gut, und RADV und ANV stehen als lesbare Vergleichsimplementierungen zur Verfügung. Danach nach Windows portieren. Der CPU-Fallback wird zuerst gebaut, damit es immer einen funktionierenden Pfad gibt.

**Messgröße.** CPU-Last und Speicherbandbreite bei 4K60 AV1, gegen `mpv --hwdec=auto` als Referenz.

### 2. Audio-Video-Synchronisation

**Was es ist.** Video-Frames zum richtigen Zeitpunkt anzeigen, relativ zur Audio-Wiedergabe und zum tatsächlichen Bildschirm-Refresh.

**Warum es schwer ist.** Es klingt nach „PTS vergleichen und warten", ist aber ein Regelkreis: Audio-Uhr und Systemuhr driften, der Bildschirm hat eine feste Refresh-Rate, die selten ein ganzzahliges Vielfaches der Framerate ist, und die Präsentationszeit ist nicht die Zeit des Aufrufs. mpv hat mit `--video-sync=display-resample` über Jahre einen Algorithmus entwickelt, der die Audio-Rate minimal an den Display-Refresh anpasst statt Frames zu doppeln. Die naive Variante erzeugt sichtbares Ruckeln bei Schwenks — der Klassiker „24 fps auf 60 Hz".

**Vorgehen.** Zuerst die einfache Audio-Master-Variante mit Frame-Dropping. Dann Display-Resampling. A/B gegen libmpv mit identischem Material.

**Messgröße.** Histogramm der Präsentationsfehler in Millisekunden, Zahl gedoppelter und verworfener Frames pro Minute.

### 3. Realtime-Audio-Determinismus

**Was es ist.** Der Audio-Callback muss innerhalb seines Budgets fertig sein — bei 128 Samples und 48 kHz sind das 2,7 ms. Immer, ohne Ausnahme.

**Warum es schwer ist.** **Rust hilft hier nicht automatisch.** Rust erlaubt Allokation überall, `Vec::push` kann reallozieren, `println!` nimmt ein Lock, ein `panic!` entrollt den Stack, und `Mutex` kann den Thread schlafen legen. Sicherheit vor Datenrennen ist etwas anderes als Echtzeitfähigkeit.

**Vorgehen.**
* `assert_no_alloc` als Guard um den gesamten Audio-Callback, ab dem ersten Tag.
* Alle Puffer vorab allozieren, Kapazitäten festlegen.
* Kommunikation ausschließlich über lock-freie SPSC-Ringpuffer und Atomics.
* Kein Logging im Audio-Thread; stattdessen Zähler, die der UI-Thread ausliest.
* Xrun-Zähler von Anfang an sichtbar in der Oberfläche.

**Messgröße.** Xruns pro Stunde unter Last, Verteilung der Callback-Laufzeit als Perzentile — der Median ist bedeutungslos, das 99,99. Perzentil entscheidet.

---

## Technologie-Entscheidungen

| Bereich | Entscheidung | Begründung | Neu bewerten, wenn |
|---|---|---|---|
| Sprache (in-process) | Rust | Speicher- und Threadsicherheit, gute FFI, moderner Werkzeugkasten | nie |
| Demux / Decode | FFmpeg, dynamisch gelinkt, ohne `--enable-gpl` | Alternativlos in der Formatabdeckung | – |
| Video-Backend Phase 0 | libmpv hinter `VideoBackend` | Sofort ein benutzbarer Player; bleibt danach als Referenz-Orakel | – |
| GPU | wgpu | Portabel, aktiv entwickelt, WGSL | Externe-Textur-Import erweist sich als Sackgasse → Vulkan direkt über `ash` |
| Ausgabe-Codec | AV1 | Einzige Familie mit durchgehend permissiver Werkzeugkette und lizenzgebührenfreier Patentlage | – |
| Encoder | Hardware zuerst: NVENC, Quick Sync, AMF | Kein GPL-Code im Prozess, keine eigene Patentnutzung, schneller und sparsamer | – |
| Software-Encoder | SVT-AV1, rav1e | BSD mit Patentgrant | – |
| Audio-Geräte | cpal, plus eigener WASAPI-Exclusive-Pfad | cpal deckt die Breite ab, reicht aber für bitperfekte Ausgabe nicht | – |
| Audio Linux | PipeWire direkt | Moderne Schnittstelle, gute Latenz | – |
| Oberfläche | egui für Rahmen-UI, eigenes wgpu-Rendering für Video und Timeline | MIT/Apache, Windows-solide; Timeline muss ohnehin custom sein | Barrierefreiheit wird zum Thema |
| Plugin-Format | CLAP zuerst, LV2 früh, VST3 über Bridge-Prozess | CLAP ist MIT, technisch moderner, ohne Rechtsreibung | – |
| Streaming | SRT und WHIP vor RTMP | Sub-Sekunden-Latenz, moderne Protokolle | – |
| Serialisierung | Projektdateien als Text (TOML oder JSON) | Diffbar, versionierbar, von Hand reparierbar | Dateigröße wird zum Problem |

**Bewusst abgelehnt:** Slint (Lizenz nicht permissiv), GPUI (Windows-Unterstützung zu schwach), Tauri (für Video-Compositing zu langsam), JUCE (GPL oder Kauf), Electron.

---

## Lizenz-Topologie

```
ZONE 1 — Permissiver Kern:  Apache-2.0 OR MIT
  takt-core, takt-clock, takt-audio, takt-graph, takt-fmt
  Ziel: maximale Wiederverwendung. Diese Crates sind das sichtbare
  Ergebnis der Spezifikationsarbeit und sollen von anderen benutzt werden.

ZONE 2 — Anwendung:  GPL-3.0-or-later
  takt-player, takt-studio, takt-edit, takt-demux, takt-decode, takt-gpu
  GPLv3 ist hier die ermöglichende, nicht die einschränkende Wahl:
  sie schaltet VST3, x264/x265 und Qt frei, falls je gebraucht.

ZONE 3 — Isolierte Hilfsprozesse:  eigene Lizenz je Prozess
  takt-vst3 (GPLv3), takt-x264 (GPLv3), KI-Dienste (Lizenz je Modell)
  Kommunikation über anonymes Shared Memory (lock-freier Ring) für Daten
  und eine Pipe mit versioniertem Protokoll für die Steuerung.
  Bilden mit der Hauptanwendung kein Gesamtwerk.
```

Beitragsmodell zunächst **DCO**, kein CLA. In `CONTRIBUTING.md` wird festgehalten, dass eine spätere kommerzielle Lizenzierung nicht ausgeschlossen ist — nicht als juristische Absicherung, sondern damit später niemand einen Wortbruch behaupten kann. Ein CLA kommt erst, wenn der erste regelmäßige externe Beitragende auftaucht.

`cargo-deny` mit Allowlist läuft in der CI ab dem ersten Tag. Nachträgliches Entwirren von Lizenzen in einem gewachsenen Monolithen ist teuer; die Prüfung ist billig.

Details und Herleitung: Repository `WKDBooks`, Ordner `Development/wkdbook-SoftwareDevelopment/Lizenzen/` (`Medien-Stack-Lizenzen.md`, `Software-Lizenzen-Grundlagen.md`).

---

## Sprachen

> **Innerhalb eines Prozesses: Rust. Über eine Prozessgrenze hinweg: was am besten passt.**

Dieselbe Grenze, die Lizenzen und Abstürze trennt, trennt auch Sprachen.

| Sprache | Rolle |
|---|---|
| **Rust** | Alles in-process. Deckt hier ohnehin den anspruchsvollen Teil der Sprache ab: `unsafe`, FFI, Atomics mit expliziten Memory-Orderings, manuelle `Send`/`Sync`-Zusicherungen, `MaybeUninit`, Allokationsvermeidung, Reibung zwischen async und Echtzeit-Threads |
| **WGSL / SPIR-V** | Nicht optional. Die zweite Sprache dieses Projekts |
| **Zig** | Als Build-Werkzeug (`zig cc`) für das Cross-Compilieren von FFmpeg und C-Abhängigkeiten. Kein Anwendungscode |
| **Go** | Nur für Netzwerkdienste jenseits der Prozessgrenze: Relay, Ingest-Server, Fernsteuerung. Für den Kern durch Garbage Collection disqualifiziert |
| **C** | Wird viel gelesen, nicht geschrieben: FFmpeg, dav1d, CLAP-Header |
| **Python** | Nur Werkzeuge, die nicht ausgeliefert werden: Bitstream-Analyse, Timing-Auswertung, Testdaten |

---

## Finanzierung, falls es je relevant wird

Kein Ziel, aber es kostet nichts, sich die Tür offenzuhalten. Das Vorbild ist **Ardour**: GPL-lizenziert, Quellcode frei, **fertige Binaries kosten Geld**. Das ist vollständig GPL-konform, weil die GPL Verkauf nicht verbietet, sondern nur die Verfügbarkeit des Quellcodes verlangt.

Warum das bei nativen Medienprojekten funktioniert: der Selbstbau-Graben entsteht von allein. FFmpeg mit den richtigen Flags, Vulkan-SDK, CUDA-Header, plattformspezifische Codesignatur, Abhängigkeitsketten. Wer das durchsteht, ist Entwickler und wäre nie zahlender Kunde gewesen. Alle anderen zahlen bereitwillig einen kleinen Betrag für „funktioniert nach dem Doppelklick".

Die Bedingung ist, dass der Graben **natürlich** bleibt. Ardour bleibt glaubwürdig, weil der Build ehrlich dokumentiert und tatsächlich durchführbar ist. Sobald man ihn absichtlich erschwert, kippt das Modell in Feindseligkeit gegenüber der eigenen Community.

Weitere Optionen ohne Lizenzwechsel: bezahlter Support, ein proprietäres Zusatzmodul in Alleinbesitz, Auftragsentwicklung an der eigenen Codebasis. Der Lizenzwechsel ist der letzte Hebel, nicht der erste — und er setzt voraus, dass man zu diesem Zeitpunkt alle Rechte hält.

---

## Roadmap mit Gates

Gates sind **ehrliche Abbruch- oder Umsteuerpunkte**, keine Formalität. Sie werden anhand der instrumentierten Messwerte beantwortet, nicht nach Gefühl.

**Jede Phase endet mit einem Review-Dokument** unter `docs/reviews/phase-N.md`, das drei Fragen beantwortet: Was hat die Architektur gelernt? Wo liegen jetzt die Performance-Grenzen? Welche Angriffsfläche ist neu entstanden? Das Review wird während der Phase fortgeschrieben, nicht am Ende geschrieben — sonst entsteht eine Rechtfertigung statt einer Prüfung.

### Phase 0 — Fundament und Zeitmechanik (4–6 Wochen)
Oberflächen-Rahmen, Playlist, Tastaturbelegung, Untertitel, Drag & Drop. Video-Backend ist libmpv hinter `VideoBackend`. Frame-Timing wird instrumentiert.

> **Gate:** Benutzt du ihn nach vier Wochen selbst statt VLC? Wenn nein — aufhören und ehrlich sein. Diese Frage ist der billigste Test des ganzen Projekts.

### Phase 1 — Eigene Pipeline (4–8 Monate)
`takt-demux`, `takt-decode`, `takt-gpu` als zweites Backend hinter demselben Trait. libmpv bleibt als A/B-Referenz zur Laufzeit umschaltbar.

> **Gate:** Frame-Timing und CPU-Last messbar auf mpv-Niveau? Wenn nach acht Monaten nicht — beim libmpv-Backend bleiben und darauf weiterbauen. Kein Gesichtsverlust; der Trait war genau dafür da.

### Phase 2 — Studio-Modus (6–12 Monate)
Screen- und Kamera-Capture, Multi-Layer-Compositing, Hardware-Encoding, SRT und WHIP. Hier zahlt sich `takt-gpu` zum ersten Mal aus.

> **Gate:** Ein einstündiger Stream ohne Verbindungsabbruch und ohne Frame-Drops über einem Prozent?

### Phase 3 — Editor-Modus (12–24 Monate)
`TimelineClock`, nicht-lineares Seeking, Scrubbing-Cache, Multi-Track-Compositing, Offline-Render-Queue.

> **Gate:** Flüssiges Scrubben über drei parallele 4K-Spuren?

### Phase 4 — DAW-Schicht (offen)
CLAP-Host, MIDI-Engine, Arranger. Nur wenn Phase 0 bis 3 stehen und die Lust noch da ist. **Das Thread-Modell dafür wird jedoch in Phase 0 festgelegt**, weil es sich nachträglich nicht einziehen lässt.

---

## Phase 0 im Detail

Der Prototyp soll zwei Fragen beantworten: *Macht das Spaß?* und *Steht die Grundstruktur?*

**Umfang**

1. Cargo-Workspace mit `takt-core`, `takt-clock`, `takt-player`.
2. `takt-core`: `Rational`, `Timestamp`, `Frame`, `AudioBuffer`, Fehlertypen.
3. `takt-clock`: `Clock`-Trait plus `AudioMasterClock`.
4. `VideoBackend`-Trait mit einer Implementierung: `MpvBackend` über `libmpv2`.
5. `takt-player`: egui-Fenster, Video über den Render-Handle von mpv, Steuerleiste mit Play/Pause, Timeline, Lautstärke.
6. Tastaturbelegung, Drag & Drop, Playlist, Untertitel laden.
7. **Instrumentierung von Anfang an:** ein Overlay mit Frame-Zeitstempeln, Präsentationsfehler, verworfenen Frames, CPU-Last, Speicherverbrauch. Umschaltbar per Taste.
8. `LICENSE`-Dateien, SPDX-Kopfzeilen, `cargo-deny` in der CI, `CONTRIBUTING.md` mit DCO.

**Ausdrücklich nicht in Phase 0:** eigene Dekodierung, Hardware-Interop, Studio- oder Editor-Ansätze, Netzwerkprotokolle, Plugin-Hosting.

**Erfolgskriterium:** Der Player läuft vier Wochen als Standardanwendung für Videodateien — und das Timing-Overlay liefert Zahlen, an denen Phase 1 später gemessen werden kann.

---

## Test-Hardware-Matrix

| Gerät | Rolle | Deckt ab |
|---|---|---|
| Desktop: RTX 4070 SUPER, i5-12400F, 48 GB, Windows 11 | Hauptentwicklung | NVENC/NVDEC inkl. AV1-Encode (Ada), Vulkan auf Nvidia, WASAPI |
| Arch / Ubuntu (nativ, gleiche Maschine) — **geplant**, Stand 2026-10-31 kein Dual-Boot eingerichtet | GPU-Interop-Forschung | `VK_KHR_video_decode`, DMA-BUF, VA-API, PipeWire |
| Dell 14 Pro (Core Ultra, Arc-iGPU) | Zweiter Treiberstack | Quick Sync inkl. AV1, Intel-Vulkan, mobile Energieprofile |
| Laptop, ~10 Jahre alt | **Negativtest** | Software-Fallback, fehlendes AV1, schwaches Vulkan, saubere Degradation |

Desktop-Hardware gemessen am 2026-10-30. Bis der Linux-Dual-Boot steht, deckt nur die CI (`ubuntu-latest`, ohne GPU) Linux ab; GPU-Interop-Forschung unter Linux wartet darauf.

Der 12400F hat kein iGPU — Quick Sync ist auf dem Desktop nicht verfügbar, alles läuft über die Nvidia-Karte. Die Dell deckt diese Lücke ab.

**Bekannte Lücken:** AMD (RDNA, AMF, RADV) und Apple (VideoToolbox, Metal). AMD ist die relevantere, weil deren Vulkan- und VA-API-Verhalten in der Praxis am stärksten abweicht.

---

## Getroffene Entscheidungen

Diese Punkte standen zunächst offen und sind inzwischen entschieden.

**Cross-Platform ab Commit eins**, nicht Windows zuerst. Die Kostenschätzung dagegen war richtig, das Argument dafür ist stärker: Linux-Unterstützung kann ein Alleinstellungsmerkmal werden, und es wäre widersprüchlich, sich über fehlende Linux-Anwendungen zu ärgern und dann selbst eine Windows-Insel zu bauen.

Was daraus folgt, ist Disziplin, nicht nur Absicht:

* **CI auf allen drei Zielen ab dem ersten Commit.** Nicht später ergänzen — der Moment, in dem eine Plattform drei Wochen nicht gebaut wurde, ist der Moment, in dem sie kaputt ist.
* **Kein `#[cfg(target_os)]` in Anwendungslogik.** Plattformcode lebt ausschließlich in dafür benannten Modulen hinter einem Trait. Sobald `cfg` durch die Geschäftslogik wandert, sind es drei Programme in einem Repository.
* **Der schwache Punkt ist macOS, nicht Linux.** Kein Vulkan, kein PipeWire, eigenes Codesigning, kein Testgerät. Die Abstraktionen halten macOS möglich, aber es wird als ungetestet geführt statt vorgetäuscht (präzisiert am 2026-10-30, siehe unten: nur nicht verbauen).

**Vulkan Video als bevorzugter, nicht als einziger Decoder-Pfad.** Die Entscheidung für die modernere Schnittstelle steht, aber sie kann die Abstraktion nicht ersetzen: **macOS hat kein `VK_KHR_video_decode`** — MoltenVK bildet Vulkan auf Metal ab, ohne die Video-Erweiterungen. VideoToolbox ist dort nicht eine Option, sondern der einzige Weg.

| Plattform | Primär | Fallback |
|---|---|---|
| Linux | Vulkan Video | VA-API |
| Windows | Vulkan Video | D3D11VA, D3D12 Video |
| macOS | VideoToolbox | — |
| überall | | Software (dav1d, FFmpeg) |

Der Software-Pfad wird zuerst gebaut: er ist der Fallback, er läuft auf dem zehn Jahre alten Testgerät, und er ist die Korrektheitsreferenz für den Hardware-Pfad.

---

### Entscheidungen vom 2026-10-30

Alle bis dahin offenen Fragen wurden an diesem Tag entschieden. Die Herleitung steht jeweils in der erledigten Task `takt/entscheidung-*` im Vault.

* **macOS: nur nicht verbauen.** Die CI baut und testet macOS weiter, es entstehen aber keine macOS-Backends (kein VideoToolbox-, kein CoreAudio-Pfad), solange es kein Testgerät gibt. Das README führt macOS als ungetestet. Die Decoder-Tabelle oben beschreibt den Weg, falls sich das ändert.
* **Hilfsprozess-IPC: Shared Memory plus Pipe.** Daten (Pakete, Frames, Audio) laufen über anonymes Shared Memory mit einem lock-freien Ring, die Steuerung über eine Pipe mit versioniertem Protokoll. Das gilt für alle Hilfsprozesse: Demux-Sandbox, Encoder, VST3-Bridge und KI-Dienste. Ein kleiner Prototyp bestätigt das Muster vor Phase 1.
* **Untertitel: eigen für SRT und WebVTT, libass für ASS.** Eigener SRT/WebVTT-Parser in `takt-fmt` plus GPU-Textdarstellung; ASS/SSA über libass hinter einem Trait, weil dessen Typografie-Umfang ein eigenes Projekt wäre.
* **KI: Schnittstelle früh, Modelle spät.** Protokoll, Datenmodell und Hilfsprozess-Rahmen in Phase 0/1; Whisper-Untertitel nach Gate 0; weitere Modelle ab Phase 3. Modelle tragen eigene Lizenzen (Whisper MIT, viele Bildmodelle nicht-kommerziell); die Prozessgrenze hält sie aus Zone 1 und 2 heraus.
* **Web: Spike nach Gate 0.** Eine Machbarkeitsstudie nach Gate 0, danach der Player-Umfang als eigenes Build-Ziel (Wasm). Studio und DAW ausdrücklich nicht. Für `#[cfg(target_arch)]` gilt dieselbe Regel wie für `target_os`: nur in benannten Modulen hinter Traits.
* **`unsafe`-Policy.** Workspace-weit `unsafe_code = "deny"`. Nur Module mit `ffi` oder `sys` im Namen oder Pfad dürfen `#![allow(unsafe_code)]` setzen, und jeder `unsafe`-Block trägt einen `// SAFETY:`-Kommentar. `takt-clock` kommt heute ohne `unsafe` aus (Seqlock über Atomics) und soll das behalten.
* **Bild-Modus: ja, schlank.** Ein Bild-Modus ohne Uhr auf `takt-gpu` (Ebenen, Farbraum, Export), Start nach Phase 1. Daraus das Nicht-Ziel „Kein Nachbau von GIMP oder Krita“.
* **Zeitbudget und Gate 0.** 8 Stunden pro Woche. Zieldatum für den Start der vierwöchigen Alltagsnutzung (Gate 0): 2027-01-04.
* **Doku-Ort und Sprache.** Dieses Konzept wandert in den Vault (`tasks-vault/takt/`); im Repo bleiben README, CONTRIBUTING, THREADS-UND-UHREN und BUILD. Die Doku bleibt Deutsch, das README bekommt einen englischen Absatz.

---

## Offene Entscheidungen

Dieses Dokument führt keinen eigenen Fragenkatalog. Offene Entscheidungen sind Tasks im Vault (`takt/entscheidung-…`, Status `decision`) und stehen hier nur mit ihrer Task-ID; ist eine entschieden, wandert das Ergebnis nach [Getroffene Entscheidungen](#getroffene-entscheidungen).

* `takt/entscheidung-kern-crates-eigenes-repo`
* `takt/entscheidung-takt-gpu-als-bibliothek`
