# Mitwirken an Takt

## Beitragsmodell: DCO

Takt verwendet das [Developer Certificate of Origin](LICENSES/DCO.txt) (DCO), kein Contributor License Agreement. Jeder Commit braucht eine Signatur:

```bash
git commit -s -m "deine Nachricht"
```

Das `-s` fügt eine `Signed-off-by:`-Zeile hinzu. Damit bestätigst du, dass du den Beitrag unter der Lizenz der jeweiligen Datei beisteuern darfst. Es überträgt **keine** Rechte an den Projektinhaber — dein Beitrag bleibt dein Urheberrecht.

## Lizenz-Topologie

Takt hat nicht eine Lizenz, sondern drei Zonen. Achte darauf, in welcher du arbeitest:

| Zone | Pfade | Lizenz |
|---|---|---|
| 1 | `crates/takt-core`, `takt-clock`, `takt-audio`, `takt-graph`, `takt-fmt` | `Apache-2.0 OR MIT` |
| 2 | `crates/takt-demux`, `takt-decode`, `takt-gpu`, alles unter `apps/` | `GPL-3.0-or-later` |
| 3 | alles unter `bridges/` | eigene Lizenz je Prozess, siehe dortige `LICENSE` |

Zwei Regeln, die daraus folgen:

* **Zone 1 darf niemals von Zone 2 abhängen.** Permissiver Code darf keinen Copyleft-Code einziehen. Die Abhängigkeitsrichtung ist immer Zone 2 → Zone 1.
* **Kein GPL-Code in-process in Zone 1.** Wenn eine Bibliothek nur unter GPL verfügbar ist, gehört sie hinter eine Prozessgrenze in Zone 3.

Jede Quelldatei beginnt mit der passenden Kennung:

```rust
// SPDX-License-Identifier: Apache-2.0 OR MIT
```

`cargo-deny` prüft das in der CI. Wenn der Lauf fehlschlägt, ist meist eine neue Abhängigkeit mit unerwarteter Lizenz hinzugekommen — bitte nicht die Allowlist erweitern, ohne den Fall im Pull Request zu erklären.

## Hinweis zur zukünftigen Lizenzierung

Takt entsteht aus Interesse an der Technik, nicht als Produkt. Sollte das Projekt jedoch eine Größe erreichen, in der die Pflege regelmäßig Zeit kostet, behält sich der Projektinhaber vor, Finanzierungsmodelle zu erwägen — nach dem Vorbild von Ardour etwa kostenpflichtige vorkompilierte Binaries bei weiterhin frei verfügbarem Quellcode.

Das ist keine juristische Klausel und keine Rechteeinräumung. Es steht hier, damit die Absicht von Anfang an transparent ist und niemand sich später überrumpelt fühlt. **Der Quellcode bleibt in jedem Fall Open Source.**

Falls das Projekt einmal reguläre externe Beitragende hat, wird ein CLA eingeführt werden müssen, um solche Optionen überhaupt offenzuhalten. Bis dahin gilt DCO.

## Code-Konventionen

* `cargo fmt` und `cargo clippy -- -D warnings` müssen sauber durchlaufen.
* Zeitstempel niemals als `f64`. Immer `Timestamp` mit rationaler Basis.
* Im Audio-Callback: keine Allokation, kein Lock, kein I/O, kein Logging. `assert_no_alloc` bleibt aktiv.
* `unsafe` ist im Workspace verboten (`unsafe_code = "deny"` in `Cargo.toml`). Erlaubt nur in benannten FFI-Modulen (`ffi`/`sys`, heute nur der Link-Test `apps/takt-player/tests/ffi_native_link.rs`), die das mit `#![allow(unsafe_code)]` sagen; jeder Block dort braucht einen `// SAFETY:`-Kommentar, der die Invariante benennt. Zone 1 bleibt ganz ohne `unsafe`.
* Formatierung nach `rustfmt.toml` (LF auch unter Windows). Textdateien enthalten keine Steuerzeichen außer Tab und Zeilenende (`python scripts/check-control-chars.py`), jede Action in `.github/workflows` ist per Commit-SHA gepinnt (`python scripts/check-action-pins.py`); beides prüft die CI.
* Commit-Nachrichten im Stil `bereich: was` — etwa `decode: NV12-Import über DMA-BUF`.
