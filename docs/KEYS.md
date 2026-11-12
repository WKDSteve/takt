# Tasten von takt-player

Erzeugt aus `BINDINGS` in `apps/takt-player/src/app.rs`; der Test `docs_keys_md_is_the_table` hält die Datei daran fest (neu schreiben: `TAKT_WRITE_KEYS=1 cargo test -p takt-player docs_keys_md`). Das Hilfe-Overlay (`?`) zeigt dieselbe Tabelle.

| Taste | Wirkung |
|---|---|
| `Space` | play / pause |
| `Left` | jump back 5 s |
| `Right` | jump forward 5 s |
| `Home` | jump to the start |
| `9` | quieter (5 %) |
| `0` | louder (5 %) |
| `m` | mute / unmute |
| `>` | next entry of the playlist |
| `<` | entry before in the playlist |
| `F8` | show / hide the playlist |
| `?` | show / hide this list of keys |
| `F3` | show / hide the diagnostics |
| `F4` | save the diagnostics (clipboard and a file) |
| `Escape` | quit |

| Maus | Wirkung |
|---|---|
| Click / drag on the timeline | jump there |
| Drop files or folders | play them (a folder: its media files by name) |
| Drop with Shift held | append them to the playlist |
