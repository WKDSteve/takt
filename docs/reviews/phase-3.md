# Review Phase 3 — Editor-Modus

*Angelegt 2026-11-16, vor Beginn der Phase. Phase 3 beginnt nach Gate 2; bis dahin hält dieses Dokument fest,
woran die Phase gemessen wird. Es wird während der Phase fortgeschrieben, nicht erst am Ende geschrieben.*

Wie jedes Review beantwortet es drei Fragen: Was hat die Architektur gelernt? Wo liegen jetzt die
Performance-Grenzen? Welche Angriffsfläche ist neu entstanden? Dazu kommen die Gate-Frage und die Entscheidung über
die DAW-Schicht (Phase 4).

---

## Table of content

  - [Gate 3](#gate-3)
  - [DAW-Schicht ja oder nein](#daw-schicht-ja-oder-nein)
  - [Performance](#performance)
  - [Sicherheit und Angriffsflaeche](#sicherheit-und-angriffsflaeche)
  - [Was in dieser Phase entschieden wurde](#was-in-dieser-phase-entschieden-wurde)

---

## Gate 3

> **Flüssiges Scrubben über drei parallele 4K-Spuren?** (KONZEPT, Roadmap mit Gates)

**Entschieden am 2026-11-16** (Auswahl des Autors, der Vorschlag dieses Dokuments unverändert), was „flüssig“ heißt:

| Größe | Bestanden, wenn |
|---|---|
| Bild beim Ziehen der Zeitleiste | p99 der Zeit vom Zeiger zum zusammengesetzten Bild höchstens zwei Anzeigeperioden (33 ms bei 60 Hz) |
| Sprung an eine beliebige Stelle | erstes korrektes Bild nach höchstens 100 ms, auch ohne Treffer im Scrubbing-Cache |
| Wiedergabe der drei Spuren | keine verworfenen Frames bei 4K60, Präsentationsfehler wie im Player (Gate 1) |
| Quellen | drei 4K60-Dateien mit langen GOPs (HEVC 10 bit, AV1), nicht nur Intra-Codecs |

Der letzte Punkt ist der eigentliche Test: mit Intra-Codecs (ProRes, DNxHR) ist Scrubben leicht, mit langen GOPs
muss der Cache die Arbeit tun. Die Antwort kommt mit Datum in das KONZEPT unter „Getroffene Entscheidungen“
(`takt/review-phase-3`).

---

## DAW-Schicht ja oder nein

KONZEPT, Phase 4: „Nur wenn Phase 0 bis 3 stehen und die Lust noch da ist.“ Das Thread-Modell dafür ist seit
Phase 0 festgelegt (genau ein hart-echtzeitfähiger Thread). Was das Review für die Entscheidung festhalten soll:

* Stehen die Gates 0 bis 3, oder wurde eines umgesteuert?
* Hat der Audiopfad in Studio und Editor ohne Xruns gehalten (die Messlatte einer DAW ist härter: Latenz unter
  10 ms, Plugins im Callback)?
* Ist das Zeitbudget (8 Stunden pro Woche) mit der Pflege von Player, Studio und Editor schon aufgebraucht?

---

## Performance

*Wird während der Phase gefüllt.* Erwartete Messtabelle:

| Datum | Hardware | Spuren (Codec, Auflösung, Rate) | Vorgang | p50 | p99 | Cache-Treffer | VRAM | RAM |
|---|---|---|---|---|---|---|---|---|

---

## Sicherheit und Angriffsflaeche

Neu in dieser Phase, absehbar aus der Planung:

* **Projektdateien**: ein eigenes Format, das Takt liest; es ist so wenig vertrauenswürdig wie eine Mediendatei,
  sobald Projekte weitergegeben werden.
* **Pfade in Projekten**: ein Projekt verweist auf viele Dateien. Dieselbe Regel wie für Playlisten aus Phase 0
  (relative Pfade bleiben im Ordner, keine Adressen) liegt nahe; zu entscheiden, wenn das Format entsteht.
* **Offline-Render**: Takt schreibt jetzt große Dateien; nie durch eine vorhandene Datei oder Verknüpfung hindurch
  (wie die Diagnose-Datei seit `6cd2380`).

---

## Was in dieser Phase entschieden wurde

| Entscheidung | Grund | Revidierbar? |
|---|---|---|
