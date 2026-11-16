# Review Phase 2 — Studio-Modus

*Angelegt 2026-11-16, vor Beginn der Phase. Phase 2 beginnt nach Gate 1; bis dahin hält dieses Dokument fest,
woran die Phase gemessen wird. Es wird während der Phase fortgeschrieben, nicht erst am Ende geschrieben.*

Wie jedes Review beantwortet es drei Fragen: Was hat die Architektur gelernt? Wo liegen jetzt die
Performance-Grenzen? Welche Angriffsfläche ist neu entstanden? Dazu kommen die Gate-Frage und die Entscheidung, ob
`takt-gpu` eine eigenständige Bibliothek wird.

---

## Table of content

  - [Gate 2](#gate-2)
  - [takt-gpu als Bibliothek](#takt-gpu-als-bibliothek)
  - [Performance](#performance)
  - [Sicherheit und Angriffsflaeche](#sicherheit-und-angriffsflaeche)
  - [Was in dieser Phase entschieden wurde](#was-in-dieser-phase-entschieden-wurde)

---

## Gate 2

> **Ein einstündiger Stream ohne Verbindungsabbruch und ohne Frame-Drops über einem Prozent?** (KONZEPT, Roadmap
> mit Gates)

**Vorschlag, noch nicht entschieden**, was gezählt wird:

| Größe | Bestanden, wenn |
|---|---|
| Verbindungsabbrüche (SRT und WHIP getrennt) | 0 in 60 Minuten; ein automatischer Wiederaufbau zählt als Abbruch |
| Frame-Drops | (erwartete Frames bei Nennrate − gesendete Frames) / erwartete Frames ≤ 1 %, über alle Stufen: Capture, Compositing, Encoder |
| Ende-zu-Ende-Latenz | wird gemessen und genannt, ist aber kein Gate-Kriterium |
| Xruns im Audiopfad des Studios | 0 (die Regel aus Phase 0 gilt weiter) |

Gemessen in zwei Läufen: ein Lauf gegen einen lokalen Empfänger (SRT- und WHIP-Gegenstelle auf demselben Rechner
oder im LAN), der sich automatisieren und wiederholen lässt, und ein Lauf gegen eine echte Plattform über das
Internet. Das Gate zählt den Lauf über das Internet; der lokale Lauf trennt Fehler von Takt von Fehlern der
Leitung. Die Zähler gehören, wie in Phase 0, ins Overlay und in die Diagnose-Datei (F4).

Die Antwort kommt mit Datum in das KONZEPT unter „Getroffene Entscheidungen“ (`takt/review-phase-2`).

---

## takt-gpu als Bibliothek

Offene Entscheidung `takt/entscheidung-takt-gpu-als-bibliothek`, fällig am Ende dieser Phase: Hier zahlt sich
`takt-gpu` zum ersten Mal aus (Compositing mehrerer Ebenen, Capture-Import, Encoder-Übergabe). Was das Review dafür
festhalten soll, damit die Entscheidung an Belegen hängt:

* Wie oft hat sich die öffentliche Schnittstelle von `takt-gpu` in dieser Phase brechend geändert?
* Hängt `takt-gpu` von etwas ab, das nur der Player oder das Studio kennt (Typen aus `apps/`)?
* In welcher Lizenz-Zone steht es, und was zieht es an Zone-2-Abhängigkeiten nach?

---

## Performance

*Wird während der Phase gefüllt.* Erwartete Messtabelle:

| Datum | Hardware | Szene (Ebenen, Auflösung, Rate) | Ziel | Dauer | Abbrüche | Drops | Encoder-Last | CPU | Latenz |
|---|---|---|---|---|---|---|---|---|---|

Aus Phase 0 mitgenommen: Jeder Thread, der einen Echtzeit-Thread füttert, verhungert unter Nebenlast, wenn er normale
Priorität hat (Füll-Thread des Audio-Rings: 2017 Xruns in fünf Minuten, mit Echtzeit 0). Für das Studio heißt das:
Capture und die Übergabe an den Encoder werden unter Nebenlast gemessen, nicht nur auf ruhiger Maschine.

---

## Sicherheit und Angriffsflaeche

Neu in dieser Phase, absehbar aus der Planung:

* **Netz**: SRT (Handshake, Verschlüsselung, Passphrase) und WHIP (HTTP, DTLS, ICE). Erstmals spricht Takt selbst
  mit dem Netz; Phase 0 hat dafür die Regel „keine Netzwerkquellen“.
* **Zugangsdaten**: Stream-Schlüssel und Passphrasen werden gespeichert; wo und wie, ist zu entscheiden (nicht im
  Klartext neben der Konfiguration).
* **Capture**: Bildschirmaufnahme sieht alles auf dem Bildschirm, auch Fenster, die nicht in den Stream sollen.
* **Encoder als Hilfsprozess** (Zone 3, `takt/hardware-encoder-hilfsprozess-nvenc-av1`): dasselbe IPC-Protokoll wie
  der Demuxer, in die andere Richtung.

---

## Was in dieser Phase entschieden wurde

| Entscheidung | Grund | Revidierbar? |
|---|---|---|
