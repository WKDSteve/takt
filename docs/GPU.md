# Das Bild auf der GPU (`takt-gpu`)

`crates/takt-gpu` (Zone 2, `GPL-3.0-or-later`) zeichnet Bilder: Es nimmt ein Frame als Textur im VRAM oder, als
Fallback, als Ebenen im Systemspeicher, rechnet die Samples in das R'G'B' eines SDR-Ziels um (Matrix, Wertebereich,
Primärwerte, Tonemapping für HDR) und legt mehrere Lagen übereinander. Der Player zeichnet seine Videofläche damit
(`apps/takt-player/src/render`); Studio, Editor und Bild-Modus bauen auf demselben Crate auf.

Dieses Dokument beschreibt die Naht zu `takt-decode`, den Farbweg, das Compositing und wie das alles geprüft ist.

## Table of content

  - [Die Naht: Geräte-Auskunft hin, GpuFrame zurück](#die-naht-geräte-auskunft-hin-gpuframe-zurück)
  - [Fremde Texturen importieren](#fremde-texturen-importieren)
  - [Der Farbweg](#der-farbweg)
  - [Tonemapping](#tonemapping)
  - [Compositing](#compositing)
  - [Ein verlorenes Gerät](#ein-verlorenes-gerät)
  - [Tests](#tests)
  - [Messen](#messen)
  - [Was fehlt](#was-fehlt)

---

## Die Naht: Geräte-Auskunft hin, GpuFrame zurück

Die Reihenfolge beim Start ist: erst das wgpu-Gerät, dann der Decoder. Ein Hardware-Decoder muss auf dem Adapter
dekodieren, der auch zeichnet, sonst lassen sich seine Texturen nicht importieren.

1. **Geräte-Auskunft** (`DeviceInfo::of(adapter, device)`): Backend, Name des Adapters, unter Windows die LUID des
   Adapters (darauf öffnet der Decoder sein D3D11-Gerät) und welche Formate das Gerät zeichnen kann (`nv12`, `p010`,
   `sixteen_bit`). Das Gerät braucht dafür die Features aus `takt_gpu::wanted_features(adapter.features())`; der
   Player trägt sie in den `device_descriptor` von eframe ein.
2. **`GpuFrame`** kommt zurück, in zwei Varianten:

   | Variante | Inhalt | Weg zur GPU |
   |---|---|---|
   | `GpuFrame::Texture(TextureFrame)` | eine `wgpu::Texture` im Format `NV12` oder `P010`, die Array-Scheibe, die sichtbare Größe, die Farbangabe (`FrameColor`), das Bereitschaftssignal (`Ready`) und ein Rückgabe-Guard | keine Kopie: zwei Views auf die Ebenen der Textur |
   | `GpuFrame::Cpu(PooledFrame)` | das Frame aus dem Pool selbst (`takt_core::FramePool`), keine Kopie davon | `write_texture` je Ebene, mit dem Stride der Ebene als `bytes_per_row` |

   Der CPU-Weg nimmt `Nv12`, `P010`, `Yuv420P`, `Yuv420P10`, `Yuv444P`, `Rgb0` und `Rgba8`. Ein Format ohne Weg
   (`Rgba16F`) wird abgelehnt, nicht gewandelt. 10 bit aus dem Systemspeicher braucht `TEXTURE_FORMAT_16BIT_NORM`.
3. **Zeichnen**: eine Lage (`LayerSlot`) zeigt ein Frame (`show`) und zeichnet es (`draw`). Beide Varianten enden im
   selben Shader (`layer.wgsl`) und im selben Bind-Group-Layout.

Was ein `TextureFrame` zusätzlich trägt:

- **Sichtbare Größe**: die Textur darf größer sein als das Bild (kodierte Größe, Scheibe eines Arrays). Der Shader
  schneidet zu und liest auch mit einem Filter nichts, was neben dem Bild liegt.
- **`Ready`**: `Now`, oder ein Zaun mit Wert (`Ready::Fence`). Die Queue von wgpu wartet dann auf der GPU, bis das
  schreibende Gerät den Zaun signalisiert hat; kein Thread blockiert. Ein Zaun, der seinen Wert nie erreicht, hält die
  Queue für immer an: wer ihn signalisiert, muss vertrauenswürdig sein oder mit diesem Prozess enden.
- **Rückgabe-Guard** (`returned_by`): fällt mit dem Frame und gibt das Bild an seinen Besitzer zurück (Pool des
  Decoders, Ring geteilter Texturen). Die Lage lässt ein Frame nicht in dem Moment los, in dem ein anderes Bild es
  ersetzt: der erste `LayerSlot::before_pass` danach schreibt den Rückweg der Textur in den Zustand, in dem sie
  hereinkam (`D3D12_RESOURCE_STATE_COMMON`), der zweite gibt das Frame frei, sobald die GPU damit fertig ist. Der
  Guard fällt also bis zu zwei Einreichungen nach dem Ersetzen; eine Quelle braucht so viele Bilder mehr im Pool.

Alles, was über diese Naht kommt, kann aus einem Hilfsprozess stammen. Größen, Scheiben und Formate werden gegen die
Textur selbst und die Limits des Geräts geprüft, bevor wgpu sie sieht (`TextureFrame::new`, `open_shared_texture`,
`PlanesView`).

## Fremde Texturen importieren

Das Ergebnis des Zero-Copy-Spikes ([reviews/phase-1.md](reviews/phase-1.md)) ist hier umgesetzt: wgpu hat in seiner
eigenen API keinen Import eines fremden Handles, also geht es über wgpu-hal. Das `unsafe` dafür steht in den zwei
Modulen unter `src/interop` und nirgends sonst im Crate. Bisher Windows auf DX12:

- `interop::dx12::open_shared_texture(device, handle, format)` öffnet eine geteilte Textur (NT-Handle aus
  `IDXGIResource1::CreateSharedHandle`) auf dem D3D12-Gerät von wgpu und hüllt sie als `wgpu::Texture` ein. Der
  Aufrufer nennt nur das erwartete Format; Größe und Array-Scheiben liest die Funktion aus der Ressource.
- `interop::dx12::open_shared_fence` und `Fence::wait_on` sind das Bereitschaftssignal.
- `interop::d3d11::SharedSource` ist die andere Seite: ein D3D11-Gerät auf demselben Adapter, das in geteilte Texturen
  schreibt und einen geteilten Zaun signalisiert. Dort steht später der Hardware-Decoder (D3D11VA, der erste
  Hardware-Weg unter Windows). Heute schreibt es Ebenen aus dem Systemspeicher: so entsteht eine Textur, die für wgpu
  in jeder Hinsicht fremd ist (anderes Gerät, andere API, eigene Fertigmeldung), ohne Decoder und ohne Datei. Die
  Tests, die Messung und `takt-player --imported` benutzen das.

Unter Windows sucht `takt-gpu` sein Gerät auf DX12 (`preferred_backends`), weil der Import je Backend ist. Der Player
bleibt bei der Wahl von eframe und wechselt nur mit `--imported` auf DX12; welches Backend der Player auf Dauer nimmt,
entscheidet sich mit dem Hardware-Decoder (`takt/hardware-decode-vulkan-video-d3d`).

`windows` 0.58 ist die Version, gegen die wgpu-hal 27 gebaut ist: `texture_from_raw` nimmt deren `ID3D12Resource`.
Ein wgpu-Update zieht diese Version mit.

## Der Farbweg

Die Zahlen und ihre Herleitung stehen in `src/colour.rs`, das zugleich die CPU-Referenz des Shaders ist
(`Conversion::apply` rechnet in `f32`, was `layer.wgsl` rechnet).

1. **Y'CbCr nach R'G'B'** mit einer affinen Matrix. Sie enthält die Skalierung der Samples (8 bit, 10 bit in den
   oberen oder den unteren Bits eines Worts), den Wertebereich (limited oder full) und die Koeffizienten
   (`MatrixCoefficients`: BT.709, BT.601, BT.2020 nicht-konstante Luminanz). `takt_core::FrameMeta` trägt keine
   Matrix; ohne Angabe gilt die zu den Primärwerten passende, ein Decoder kann sie in `FrameColor` selbst nennen.
2. **Auf Primärwerte und Bereich des Ziels**, das ist BT.709 SDR:

   | Quelle | Was passiert |
   |---|---|
   | SDR mit BT.709-Primärwerten | nichts: das R'G'B' wird gezeigt, wie es ist |
   | SDR mit anderen Primärwerten (BT.601, BT.2020, P3) | Gamma 2,4 ins Licht (BT.1886), Gamut-Matrix, Gamma 2,4 zurück |
   | PQ (SMPTE ST 2084) | EOTF nach Nits, Tonemapping, Gamut-Matrix, Gamma 2,4 |
   | HLG (ARIB STD-B67) | inverse OETF und OOTF aus BT.2100 (1000 nits, Gamma 1,2) nach Nits, dann wie PQ |

3. Ein Ziel, dessen Format selbst nach sRGB kodiert, bekommt das Ergebnis mit der sRGB-Kurve linearisiert. Der
   kleine Unterschied zwischen dieser Kurve und Gamma 2,4 gehört zum Farbmanagement des Bildschirms, das noch fehlt.

Chroma wird an denselben Koordinaten abgetastet wie Luma (mittig); die Lage der Chroma-Samples (links, wie bei den
meisten Videos) ist nicht berücksichtigt.

## Tonemapping

HDR nach SDR ist die EETF aus Report ITU-R BT.2390 (Abschnitt 5.4.1, der Hermite-Spline im PQ-Raum), angewandt auf
die größte der drei Komponenten; die anderen beiden werden mitskaliert. Damit endet keine Komponente über dem
Zielweiß, und der Farbton bleibt.

- **Spitze der Quelle** (`ToneMap::source_peak_nits`): 1000 nits, solange der Strom nichts anderes sagt. Die
  HDR10-Metadaten (MaxCLL, Mastering Display) trägt `FrameMeta` noch nicht; wer sie kennt, setzt den Wert je Lage
  (`LayerSlot::set_tone_map`).
- **Weiß des Ziels** (`target_peak_nits`): 203 nits, das HDR-Referenzweiß aus Report ITU-R BT.2408.

Was unter dem Knie der Kurve liegt (etwa 88 nits bei 1000 auf 203), bleibt, wie es ist; darüber wird gestaucht.
203 nits in der Quelle kommen deshalb nicht als Weiß heraus, sondern bei 90 % des Signals (230 von 255), und die
Spitze der Quelle landet auf Weiß.

## Compositing

`Compositor` hält Lagen (`LayerSlot`) und zeichnet sie in einem Pass von unten nach oben in ein Ziel. Jede Lage hat
einen Platz (`Placement`: ein Rechteck im Ziel) und eine Deckkraft; ein `Rgba8`-Bild bringt zusätzlich sein eigenes
Alpha mit. Jede Lage rechnet ihr Bild selbst ins R'G'B' des Ziels um, eine HDR-Lage also mit Tonemapping, bevor sie
gemischt wird.

Gemischt wird auf den Werten des Ziels (vormultipliziert, „source over“): bei einem `Unorm`-Ziel auf den
gamma-kodierten Werten, bei einem Zielformat, das selbst nach sRGB kodiert, im linearen Licht. Einen Zwischenraum in
`Rgba16F` für HDR-Ausgabe gibt es noch nicht.

## Ein verlorenes Gerät

wgpu gibt ein Gerät für immer auf, sobald ein Aufruf des Treibers mit „zu wenig Speicher“, „Gerät verloren“ (ein
Reset der GPU) oder einem unbekannten Fehler scheitert. Danach verweigert es jeden Puffer still, und egui-wgpu endet
im nächsten Bild, das etwas zu zeichnen hat, mit einer Panik („Failed to create staging buffer for index data“). Das
war unter starker GPU-Last zu sehen. eframe kann für sein Fenster kein neues Gerät anlegen; also endet der Player
geordnet: er hört den Rückruf von wgpu für ein verlorenes Gerät, zeichnet ab dem nächsten Bild nichts mehr, nennt den
Grund auf stderr, in `%TEMP%\takt-player.log` (und ohne Konsole in einem Meldungsfenster), schließt das Fenster und
endet mit Exit-Code 1 (`apps/takt-player/src/render/loss.rs`). Geht das Gerät genau zwischen der App und dem Zeichnen
desselben Bildes verloren, bleibt es bei der Panik; deren Zeile im Log nennt dann den Verlust mit.

## Tests

| Was | Wo | Läuft |
|---|---|---|
| Farbrechnung gegen die Normen (Matrizen, PQ, HLG, BT.2390, Gamut BT.2020 nach BT.709) | `src/colour.rs` | überall, `cargo test -p takt-gpu` |
| Referenzbilder gegen die CPU-Referenz, und Punkte darin, die aus den Normen folgen | `tests/gpu.rs`, `tests/data/reference-*.ppm` | überall |
| Shader gegen die Referenzbilder (BT.709, BT.601, BT.2020 PQ und HLG nach SDR) | `tests/gpu.rs`, `gpu_*` | auf einem Adapter |
| Gepolsterte Zeilen, zwei und drei Ebenen, `rgb0` deckend | `tests/gpu.rs`, `gpu_*` | auf einem Adapter |
| Vier Lagen gegen eine von Hand gerechnete Mischung | `tests/gpu.rs`, `gpu_four_layers_*` | auf einem Adapter |
| Import einer fremden Textur gegen den Upload ihrer Ebenen: gleiche Prüfsumme, NV12 und P010 | `tests/gpu.rs`, `import::gpu_*` | Windows, auf einem Adapter |
| Rückgabe erst, wenn die GPU fertig ist; Zuschnitt; abgelehnte Handles und Größen | `tests/gpu.rs`, `import::gpu_*` | Windows, auf einem Adapter |
| Ein verlorenes Gerät: egui-wgpu endet mit einem Mesh in der Panik, der Player zeichnet keins | `apps/takt-player/src/render/loss.rs`, `a_lost_device_*` | auf einem Adapter |
| Der Importweg im echten Fenster | `TESTS/sheets/imported-picture-window.sheet.lua` | privater Rechner, `testing sheets run --all` |

Die Tests auf einem Adapter sind `#[ignore]` und laufen so:

```sh
TAKT_GPU_TESTS=1 cargo test -p takt-gpu gpu -- --ignored
TAKT_GPU_TESTS=1 TAKT_GPU_ADAPTER=fallback cargo test -p takt-gpu gpu -- --ignored   # WARP, wie in der CI
```

Die CI rendert sie unter Windows auf dem Software-Adapter von DX12 (WARP), auch den Import; Linux und macOS lassen
sie aus. Toleranzen: 2 von 255 je Kanal für SDR, 3 für HDR (drei Potenzfunktionen in `f32` auf der GPU gegen die
Rundung der CPU-Referenz).

Die **Referenzbilder** sind kleine PPM-Dateien im Textformat (16 mal 8 Pixel: Farbbalken über einem Graukeil). Sie
sind das, was die CPU-Referenz rechnet; im Repository liegen sie, damit eine Änderung der Farbrechnung als Diff von
Bildern sichtbar wird und nicht in einem Test verschwindet, der den Code mit sich selbst vergleicht. Neu schreiben:
`TAKT_GPU_BLESS=1 cargo test -p takt-gpu the_reference_pictures_are`.

`takt-player --imported` (Windows) schickt das Testbild über den Importweg: ein D3D11-Gerät schreibt jedes Bild in
eine geteilte Textur, die Videofläche zeichnet die importierte Textur. Das erste Bild zeichnet der Player auf beiden
Wegen in ein eigenes Ziel und vergleicht die Prüfsummen; das Overlay (F3) sagt in der Zeile „picture path“, wie es
ausging. Das Sheet wartet auf genau diese Aussage.

## Messen

```sh
cargo run --release -p takt-gpu --example composite-bench -- --layers 4 --seconds 30
cargo run --release -p takt-gpu --example composite-bench -- --layers 4 --seconds 30 --window
cargo run --release -p takt-gpu --example composite-bench -- --source upload
```

Vier Lagen (NV12 BT.709 flächig, P010 BT.2020 PQ mit Tonemapping bei halber Deckkraft, NV12 als Bild im Bild, eine
RGBA-Grafik mit Alpha) aus 4K-Quellen in ein 4K-Ziel, jede Video-Lage mit einem neuen Bild je Durchlauf. Ohne
Fenster ist alle 1/60 s ein Bild fällig; es zählt als verspätet, wenn die GPU es nicht fertig hat, bevor das nächste
fällig ist. Mit `--window` wird das Ziel zusätzlich in ein Fenster mit vsync präsentiert; ein Bild ist verspätet,
wenn seit dem vorigen mehr als anderthalb Bildperioden vergangen sind. `--source imported` (Standard unter Windows)
nimmt importierte Texturen, `--source upload` lädt jedes Bild aus dem Systemspeicher hoch. Gemessen wird der
Compositor, kein Decoder: die Bilder entstehen, bevor die Uhr läuft. Zahlen in
[reviews/phase-1.md](reviews/phase-1.md).

`scripts/measure-composite.ps1` startet einen Lauf mit einer Prioritätsklasse, schreibt die CPU-Last der Maschine und
die GPU-Last mit und gibt beides vor der Zahlenzeile des Programms aus; mit `-RequireQuiet` misst es nur, wenn die
Maschine ruhig ist (CPU-Last zehn Sekunden lang höchstens 10 %).

## Was fehlt

- Import unter Linux (Vulkan, DMA-BUF) und macOS (IOSurface, Metal): `takt/spike-zero-copy-linux-vulkan-import`.
  Dort liefert ein Decoder bis dahin Frames im Systemspeicher.
- Der Hardware-Decoder selbst (`takt/hardware-decode-vulkan-video-d3d`) und D3D12VA ohne Kopie.
- HDR-Ausgabe (ein Ziel, das selbst HDR ist), HDR-Metadaten je Strom, Farbmanagement des Bildschirms (ICC),
  Chroma-Lage, Skalierer besser als bilinear.
- Ob `takt-gpu` eine eigenständige Bibliothek wird, ist offen (`takt/entscheidung-takt-gpu-als-bibliothek`, fällig
  am Ende von Phase 2, [reviews/phase-2.md](reviews/phase-2.md)). Vorbereitet ist: das Crate hängt nur an
  `takt-core`, `wgpu` und unter Windows `windows`, an nichts aus `apps/`, und reicht sein `wgpu` als
  `takt_gpu::wgpu` weiter.
