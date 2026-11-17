# zero-copy-spike

Prototyp zum Spike `takt/spike-zero-copy-decoder-gpu-import`: Kann das wgpu, das der Workspace benutzt
(27.0.1, wgpu-hal 27.0.4), eine vom Hardware-Decoder beschriebene Textur ohne Umweg über den Systemspeicher
anzeigen? Ergebnis und Messwerte stehen im Vault (`takt/ROADMAP/reports/2026-11-17_spike-zero-copy.md`) und
in `docs/reviews/phase-1.md`. Der Code hier ist Wegwerf-Code eines Spikes, kein Teil von Takt: eigener
Workspace, nur Windows, nicht in der CI.

## Was er tut

FFmpeg (dynamisch gelinkt) demuxt und dekodiert in Hardware, ein WGSL-Shader rechnet NV12 bzw. P010 nach RGB
in ein Ziel in Bildgröße. Drei Wege für das Bild:

| Schalter | Weg | Kopien je Bild |
|---|---|---|
| `--decoder d3d12va --mode zero-copy` | FFmpeg dekodiert auf dem D3D12-Gerät von wgpu, die Decoder-Textur wird über `wgpu::hal::dx12::Device::texture_from_raw` und `Device::create_texture_from_hal` eingehüllt | keine |
| `--decoder d3d11va --mode zero-copy` | FFmpeg dekodiert auf einem D3D11-Gerät desselben Adapters, `CopySubresourceRegion` in eine geteilte Textur, die D3D12 per `OpenSharedHandle` öffnet | eine, auf der GPU |
| `--mode cpu` | `av_hwframe_transfer_data` (hwdownload) und `Queue::write_texture` | zwei im Systemspeicher, zwei auf der GPU |

`--sync strict` (Vorgabe) wartet nach jedem Bild, bis die GPU fertig ist, bevor der Decoder weiterläuft;
`--sync loose` lässt den Decoder weiterlaufen. `--verify` liest das Ziel zurück und druckt Prüfsummen, an denen
sich die Wege vergleichen lassen. `--window` präsentiert zusätzlich in ein Fenster (für den Vergleich mit mpv),
`--fps 60` taktet auf Echtzeit, `--validate` schaltet den D3D12-Debug-Layer ein, `--probe` listet, was die
Adapter der Maschine an Formaten anbieten.

## Bauen und Messen

```sh
export TAKT_FFMPEG_DIR="E:/deps/ffmpeg-n9.0-latest-win64-lgpl-shared-9.0"   # include/, lib/, bin/
cargo build --release
PATH="$TAKT_FFMPEG_DIR/bin:$PATH" target/release/zero-copy-spike clip.mkv --verify --frames 300
pwsh measure.ps1 -Clips av1-4k60.mkv,hevc10-4k60.mkv -Out result.csv -Repeat 3
```

Testclips (30 s, 4K60, etwa 40 Mbit/s; das Rauschen gibt dem Decoder Arbeit):

```sh
ffmpeg -f lavfi -i "testsrc2=size=3840x2160:rate=60,noise=alls=8:allf=t" -t 30 \
  -c:v av1_nvenc -preset p5 -b:v 40M -g 120 -pix_fmt yuv420p av1-4k60.mkv
ffmpeg -f lavfi -i "testsrc2=size=3840x2160:rate=60,noise=alls=8:allf=t" -t 30 \
  -c:v libx265 -preset ultrafast -b:v 40M -x265-params keyint=120 -pix_fmt yuv420p10le hevc10-4k60.mkv
```
