# takt (Rust-Workspace)

Aus der Vorlage `templates/rust` der nvim-Config (`node new-project-claude.js rust`), für takt
erweitert. Konzept, Plan und Tasks liegen im Vault unter `tasks-vault/takt` (Plan
`takt/takt-implementierung`), nicht hier.

## Vor „fertig“: genau die Schritte der CI (`.github/workflows/ci.yml`)

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check all
python scripts/check-control-chars.py
python scripts/check-action-pins.py
```

Dazu in der CI: jede Zone-1-Crate allein (`cargo build -p takt-core --locked`, ebenso
`takt-clock`, `takt-audio`; Lizenzen mit `deny-zone1.toml`), die Mindestversion
(`cargo check --workspace --all-features` mit Rust 1.90.0) und auf Windows der Shader-Test
(`TAKT_GPU_TESTS=1 TAKT_GPU_ADAPTER=fallback cargo test -p takt-player gpu -- --ignored`).
Gerätetests nur auf Anfrage: `TAKT_DEVICE_TESTS=1 cargo test -p takt-audio --features cpal --test device -- --ignored`.

- Der globale Hook prüft nach jedem Edit einer `.rs`-Datei `rustfmt --check`.
- Keine `unwrap()`/`expect()` in Library-Code; Fehler über `Result` propagieren.
- Neue Abhängigkeiten nur nach Rückfrage; eine neue Lizenz nie still in `deny.toml` aufnehmen.

## Invarianten (`docs/THREADS-UND-UHREN.md`, `CONTRIBUTING.md`)

- Zeit nie als `f64`: immer `Timestamp` mit rationaler Basis. Gleitkomma nur für Anzeigewerte,
  mit `allow`/`expect` und Begründung.
- Im Audio-Callback keine Allokation, kein Lock, kein I/O, kein Logging; `assert_no_alloc`
  bleibt aktiv, neue Pfade bekommen einen Test unter `AllocDisabler`.
- Der Audio-Thread spricht nur über wartefreie Strukturen (SPSC-Ring, Seqlock-Anker).
- `unsafe` ist verboten (`unsafe_code = "deny"`), außer in benannten FFI-Modulen mit
  `#![allow(unsafe_code)]`; jeder Block mit `// SAFETY:`.
- Lizenz-Zonen: Zone 1 (`takt-core`, `takt-clock`, `takt-audio`, …) `Apache-2.0 OR MIT`, hängt
  nie von Zone 2 ab; Zone 2 (`apps/`, Decoder, GPU) GPL-3.0-or-later; Zone 3 hinter
  Prozessgrenzen. Jede Datei beginnt mit ihrer `SPDX-License-Identifier`.

## Sprache und Commits

- Code, Kommentare und Commit-Messages englisch; die Repo-Doku (`README.md`, `docs/`) ist deutsch.
- Commit-Stil `bereich: was` (etwa `audio: ...`, `player: ...`), DCO: `git commit -s`, signiert.
- Regelwerk für native Apps: `$RULES_VAULT/rulesets/native` (u. a. RS-01, NREL-03, NREL-14).
- Nichts lokal bauen oder starten auf der Workstation (`NVIM_MACHINE_ROLE=workstation`).
