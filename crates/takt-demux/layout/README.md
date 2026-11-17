# Layout-Probe für die FFmpeg-Bindings

`src/ffi.rs` liest einige Felder von vier FFmpeg-Strukturen direkt (libavformat hat dafür keine Zugriffsfunktionen) und deklariert deshalb deren Anfang von Hand. Die Offsets dort sind als Konstanten zur Compile-Zeit geprüft, gegen die Zahlen, die `layout.c` aus den echten Headern druckt. Stand: FFmpeg 9.0 (libavformat 63, libavcodec 63, libavutil 61), gemessen am 2026-11-17 für `x86_64-pc-windows-msvc` und `x86_64-unknown-linux-gnu`, beide gleich.

Bei einer neuen Hauptversion von FFmpeg: Probe laufen lassen, Ausgabe mit den `assert!`-Zeilen in `src/ffi.rs` vergleichen, Strukturen und Zahlen nachziehen, danach die Versionsnummern in `build.rs` und `src/ffi.rs` anheben. Der Build verweigert Header einer anderen Hauptversion, der Helper zur Laufzeit Bibliotheken einer anderen.

```bash
# Linux, FFmpeg-Header im System
gcc layout.c -o /tmp/takt-layout && /tmp/takt-layout

# Windows, Header aus FFMPEG_DIR (nur Header, es wird nichts gelinkt)
clang --target=x86_64-pc-windows-msvc -I"$FFMPEG_DIR/include" layout.c -o layout.exe && ./layout.exe
```

Die letzte Zeile der Ausgabe nennt die Zahlenwerte der Konstanten, die `src/ffi.rs` ebenfalls von Hand führt (`AVERROR_EOF`, `AVMEDIA_TYPE_*`, …).
