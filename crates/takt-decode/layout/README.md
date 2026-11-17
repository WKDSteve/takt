# Layout-Probe für die libavcodec-Bindings

`src/bin/takt-decode-helper/ffi.rs` füllt und liest drei FFmpeg-Strukturen direkt, weil es dafür keine Zugriffsfunktionen gibt: `AVCodecParameters` (aus der Stream-Tabelle gefüllt), `AVPacket` (je Paket gefüllt) und `AVFrame` (je Bild gelesen); dazu den Anfang von `AVCodec` (Name, Art, Kennung). Der Decoder-Kontext `AVCodecContext` wird nur über einen Zeiger angefasst (`avcodec_parameters_to_context`, Optionen über `av_opt_set_int`), sein Layout geht die Bindings nichts an. Die Offsets sind als Konstanten zur Compile-Zeit geprüft, gegen die Zahlen, die `layout.c` aus den echten Headern druckt. Stand: FFmpeg 9.0 (libavcodec 63, libavutil 61), gemessen am 2026-11-17 für `x86_64-pc-windows-msvc` (clang) und `x86_64-unknown-linux-gnu` (gcc), beide gleich.

Bei einer neuen Hauptversion von FFmpeg: Probe laufen lassen, Ausgabe mit den `assert!`-Zeilen in `src/bin/takt-decode-helper/ffi.rs` vergleichen, Strukturen und Zahlen nachziehen, danach die Versionsnummern in `build.rs` und `ffi.rs` anheben. Der Build verweigert Header einer anderen Hauptversion, der Helper zur Laufzeit Bibliotheken einer anderen. Die Probe von `takt-demux` (`crates/takt-demux/layout/`) gehört zum selben Schritt.

```bash
# Linux, FFmpeg-Header im System
gcc layout.c -o /tmp/takt-decode-layout && /tmp/takt-decode-layout

# Windows, Header aus FFMPEG_DIR (nur Header, es wird nichts gelinkt)
clang --target=x86_64-pc-windows-msvc -I"$FFMPEG_DIR/include" layout.c -o layout.exe && ./layout.exe
```

Die letzten Zeilen der Ausgabe nennen die Zahlenwerte der Konstanten, die `ffi.rs` ebenfalls von Hand führt (`AVERROR_EOF`, `AV_PKT_FLAG_*`, `AV_FRAME_FLAG_*`, `AV_INPUT_BUFFER_PADDING_SIZE`). `AVERROR(EAGAIN)` ist unter Windows und Linux -11, unter macOS -35.
