# Quellen-API (Entwurf)

Stand: Entwurf. Nichts davon ist im Player gebaut; die Umsetzung kommt nach Gate 0
(`takt/quellen-api-externe-quellen`). Das Dokument steht jetzt, damit Lieferanten wie streamtap gegen eine feste
Schnittstelle bauen können. Heute kennt takt nur `VideoBackend::open(&mut self, source: &str)` und
`MediaInfo { duration, seekable }`, also keine wachsende Quelle.

Hintergrund der Entscheidung: `takt/ROADMAP/reports/2026-10-31_streamtap-integration.md` im Vault.

## Grundsätze

1. **Daten in Standardformaten, Steuerung im eigenen Protokoll.** Medien sind fragmentiertes MP4/CMAF oder Matroska.
   Das Protokoll trägt nur, was kein Container kennt: Sitzung, Lücken, Qualitätswechsel, DRM-Kennzeichen, Fortschritt.
2. **Versioniert, mit Fähigkeiten-Handshake.** takt fragt, was die Quelle kann, und nimmt nichts an.
3. **Nur lokal, nur mit Kopplung.** Named Pipe oder Unix-Socket, Token aus dem Pairing, nie in einer URL (dort landet
   es in Logs und Prozesslisten).
4. **Fremde Eingabe bleibt fremd.** Alles, was über die Schnittstelle kommt, wird geprüft und in einem
   Sandbox-Prozess demuxt, egal wer liefert.

## Ein Rahmen, kein zweiter

Das Provider-Protokoll nutzt denselben Rahmen wie die KI-Hilfsprozesse (`takt/ki-schnittstelle-zone-3-protokoll`,
`docs/AI-PROTOCOL.md`, sobald es sie gibt) und das beschlossene IPC: Pipe mit versioniertem Protokoll für die Steuerung,
Nachrichten mit Länge vorweg, Shared Memory für große Daten. Beide unterscheiden sich nur in den Nachrichten. Wer den
Rahmen zuerst baut, baut ihn für beide.

## Quelltypen

| Typ | Was es ist | Beispiel |
|---|---|---|
| `File` | eine lokale Datei, fertig oder wachsend | `D:\mitschnitt.mkv` |
| `Uri` | eine lokale URL eines Standardstroms, ohne Geheimnis darin | `http://127.0.0.1:8123/session/42.mp4` |
| `Provider` | ein Hilfsprozess, der über das Protokoll unten liefert | streamtap, yt-dlp, eine Capture-Karte |

Eine entfernte URL ist in diesem Entwurf keine Quelle; `Uri` meint Loopback.

## Typ-Skizze

Nur hier, nicht im Code. Zeiten sind `Timestamp`, nie `f64`.

```rust
pub enum Source {
    File { path: PathBuf, growing: bool },
    Uri { url: LoopbackUrl },
    Provider { program: ProviderId, session: SessionId },
}

pub struct MediaInfo {
    /// Bei einer wachsenden Quelle die bisherige Länge, sie wird nachgeführt.
    pub duration: Option<Timestamp>,
    pub seekable: bool,
    /// Die Quelle wird noch länger.
    pub growing: bool,
    /// Wohin gesprungen werden kann; bei Timeshift kleiner als `0..duration`.
    pub seekable_range: Option<Range<Timestamp>>,
}

pub enum Marker {
    /// Hier fehlen Daten; der Player zeigt die Lücke und springt nicht stumm darüber.
    Gap { at: Timestamp, length: Timestamp },
    Chapter { at: Timestamp, title: String },
    QualityChange { at: Timestamp, width: u32, height: u32, bitrate: Option<u64> },
}
```

## Provider-Protokoll

Jede Nachricht ist JSON in einem Rahmen mit Länge vorweg. Unbekannte Felder werden ignoriert, eine unbekannte Nachricht
beantwortet die Gegenseite mit `error` und läuft weiter.

**Handshake.** takt beginnt; die Versionen sind ganze Zahlen, und es gilt die höchste, die beide kennen.

```json
{"type":"hello","versions":[1],"token":"<aus dem Pairing>"}
{"type":"hello","version":1,"name":"streamtap","capabilities":["live","timeshift","gaps","tracks"]}
```

Fehlt eine Fähigkeit, nutzt takt sie nicht:

| Fähigkeit | Bedeutung |
|---|---|
| `live` | die Quelle wächst, während sie gespielt wird |
| `timeshift` | Springen innerhalb des Puffers geht (`seekable_range`) |
| `gaps` | die Quelle meldet Lücken als `marker` |
| `tracks` | die Quelle meldet Spuren einzeln |

**Sitzungen und Daten.**

```json
{"type":"list_sessions"}
{"type":"sessions","sessions":[{"id":"42","title":"Tab 3","codecs":["avc1","mp4a"],"drm":false}]}
{"type":"open","session":"42"}
{"type":"opened","session":"42","uri":"http://127.0.0.1:8123/session/42.mp4","growing":true}
```

**Ereignisse** kommen ohne Anfrage:

```json
{"type":"progress","session":"42","duration_us":183000000,"seekable_from_us":120000000}
{"type":"marker","session":"42","marker":{"kind":"gap","at_us":90000000,"length_us":1500000}}
{"type":"end","session":"42","reason":"stopped"}
```

**Fehler.** `{"type":"error","code":"drm","message":"..."}`. Die Codes sind Teil des Protokolls, der Text ist nur für
Menschen.

## Wachsende Quellen im Player

- Die `AudioMasterClock` bleibt die eine Uhr (`docs/THREADS-UND-UHREN.md`, Invariante 3). Eine wachsende Quelle ändert
  nichts an der Uhr, nur an der Dauer.
- Die Dauer wird aus `progress` nachgeführt. Die Zeitleiste zeigt sie als wachsend.
- Am wachsenden Ende wartet der Player: Er pausiert die Uhr und meldet „wartet auf Daten“, statt EOF zu melden. Erst ein
  `end` der Quelle ist das Ende.
- Eine Lücke wird als `Marker::Gap` gezeigt. Die Uhr läuft nicht über eine Lücke hinweg, ohne dass die Zeitleiste sie
  zeigt.
- Ein Sprung außerhalb von `seekable_range` wird auf die Grenze gelegt und im Overlay gemeldet.

## DRM

Eine Quelle mit DRM-Kennzeichen (`"drm": true`) wird abgelehnt, mit einem Satz, warum. takt versucht nie, sie zu
öffnen und umgeht keinen Schutz (KONZEPT, Nicht-Ziele).

## Offen bis zur Umsetzung

- Beispielclient und Konformitätstests im Testkit, damit ein dritter Lieferant ohne den Code von takt gebaut werden
  kann.
- Ob Provider Daten auch über Shared Memory statt über eine Loopback-URL geben.
