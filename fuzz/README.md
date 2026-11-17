# Fuzzing

Zwei Ziele. `protocol_reader` ist die Leseseite des Hilfsprozess-Protokolls (`takt-ipc` und `takt-demux`). Der Hauptprozess liest dort Bytes eines Prozesses, der fremde Dateien parst; das Ziel füttert genau diese Leseseite mit beliebigen Bytes: die Frames der Steuer-Pipe und ihre Nachrichten, die Zähler und Records des Rings, die Stream-Tabelle und die Pakete. Der Harness selbst ist `takt_demux::fuzzing::read_untrusted` und läuft mit festen Eingaben auch in den normalen Tests (`cargo test -p takt-demux fuzzing`).

`decode_reader` ist die Leseseite des Decoders (`takt-decode`): der Kopf eines Frame-Records (Format, Größe, Zeilenlängen gegen die Länge der Ebenen), die Kopie der Ebenen in ein Frame des Pools (planar, NV12, P010), die Zähler des Paket-Fensters, Folgen von Records in einen Decoder mit vollem und leerem Pool, und was der Hilfsprozess von seinem Elternprozess liest (Konfiguration, Paket, Flush). Der Harness ist `takt_decode::fuzzing::read_untrusted` und läuft ebenfalls in den normalen Tests (`cargo test -p takt-decode fuzzing`). Er liegt in `takt-decode` und nicht in `takt_demux::fuzzing`, weil `takt-decode` von `takt-demux` abhängt und nicht umgekehrt.

Braucht eine Nightly-Toolchain und `cargo-fuzz`; libFuzzer trägt unter Linux und macOS, unter Windows über WSL:

```bash
rustup toolchain install nightly
cargo install cargo-fuzz
# Startkorpus: gültige Eingaben je Nachricht, aus den echten Encodern.
TAKT_FUZZ_SEEDS="$PWD/fuzz/corpus/protocol_reader" cargo test -p takt-demux fuzzing
# Eine Stunde, Eingaben bis 4096 Bytes.
cargo +nightly fuzz run protocol_reader -- -max_total_time=3600 -max_len=4096

# Dasselbe für den Decoder.
TAKT_FUZZ_SEEDS_DECODE="$PWD/fuzz/corpus/decode_reader" cargo test -p takt-decode fuzzing
cargo +nightly fuzz run decode_reader -- -max_total_time=3600 -max_len=4096
```

Ein Fund liegt danach unter `fuzz/artifacts/<Ziel>/`; `cargo +nightly fuzz run <Ziel> <Datei>` spielt ihn nach. Ergebnisse der Läufe stehen in `docs/reviews/phase-1.md`.

Dieses Verzeichnis ist ein eigener Cargo-Workspace und kein Teil eines Builds, Tests oder Release; `libfuzzer-sys` ist deshalb keine Abhängigkeit des Workspace.
