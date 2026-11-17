# Fuzzing

Ein Ziel: `protocol_reader`, die Leseseite des Hilfsprozess-Protokolls (`takt-ipc` und `takt-demux`). Der Hauptprozess liest dort Bytes eines Prozesses, der fremde Dateien parst; das Ziel füttert genau diese Leseseite mit beliebigen Bytes: die Frames der Steuer-Pipe und ihre Nachrichten, die Zähler und Records des Rings, die Stream-Tabelle und die Pakete. Der Harness selbst ist `takt_demux::fuzzing::read_untrusted` und läuft mit festen Eingaben auch in den normalen Tests (`cargo test -p takt-demux fuzzing`).

Braucht eine Nightly-Toolchain und `cargo-fuzz`; libFuzzer trägt unter Linux und macOS, unter Windows über WSL:

```bash
rustup toolchain install nightly
cargo install cargo-fuzz
# Startkorpus: gültige Eingaben je Nachricht, aus den echten Encodern.
TAKT_FUZZ_SEEDS="$PWD/fuzz/corpus/protocol_reader" cargo test -p takt-demux fuzzing
# Eine Stunde, Eingaben bis 4096 Bytes.
cargo +nightly fuzz run protocol_reader -- -max_total_time=3600 -max_len=4096
```

Ein Fund liegt danach unter `fuzz/artifacts/protocol_reader/`; `cargo +nightly fuzz run protocol_reader <Datei>` spielt ihn nach. Ergebnisse der Läufe stehen in `docs/reviews/phase-1.md`.

Dieses Verzeichnis ist ein eigener Cargo-Workspace und kein Teil eines Builds, Tests oder Release; `libfuzzer-sys` ist deshalb keine Abhängigkeit des Workspace.
