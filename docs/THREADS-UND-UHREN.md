# Threads und Uhren

*Die Architekturentscheidung, die sich später nicht mehr ändern lässt.*

Dieses Dokument hält zwei Dinge fest, die in Phase 0 festgelegt werden und danach nur mit einem Umbau der halben Anwendung revidierbar sind: die Thread-Aufstellung und das Uhrenmodell.

---

## Table of content

  - [Das Zwei-Thread-Modell ist keine Anzahl](#das-zwei-thread-modell-ist-keine-anzahl)
  - [Die tatsächliche Thread-Aufstellung](#die-tatsaechliche-thread-aufstellung)
  - [Warum nicht ein Thread pro Modus](#warum-nicht-ein-thread-pro-modus)
  - [Warum mehr Threads schlechter sein können](#warum-mehr-threads-schlechter-sein-koennen)
  - [Die drei Uhren](#die-drei-uhren)
  - [Der Ankerpunkt und das Seqlock](#der-ankerpunkt-und-das-seqlock)
  - [Thread-Priorität und ihre Fallstricke](#thread-prioritaet-und-ihre-fallstricke)
  - [Invarianten](#invarianten)

---

## Das Zwei-Thread-Modell ist keine Anzahl

Die Formulierung „Zwei-Thread-Modell" ist irreführend, wenn man sie als Thread-Zahl liest. Sie beschreibt eine **Invariante über Klassen**:

> Es gibt genau **einen** hart-echtzeitfähigen Thread. Jeder andere Thread im Prozess darf ihn unter keinen Umständen warten lassen.

Alles Weitere folgt daraus. Wie viele Threads es insgesamt gibt, ist eine Optimierungsfrage; dass es genau einen unantastbaren gibt, ist eine Architekturfrage.

Der Grund für die Sonderstellung liegt im Unterschied der Fehlermodi:

| | Video | Audio |
|---|---|---|
| Arbeitseinheit | Frame | Puffer |
| Deadline bei 60 fps / 48 kHz mit 128 Samples | 16,6 ms | 2,7 ms |
| Deadline verpasst | Frame wird verworfen oder gedoppelt | **hörbares Knacken** |
| Bemerkt der Nutzer es? | selten | immer |
| Behebbar durch Nachliefern? | ja, das nächste Frame kommt | nein, der Moment ist vorbei |

Ein Video-Frame, das zwei Millisekunden zu spät kommt, sieht niemand. Ein Audio-Puffer, der zwei Millisekunden zu spät kommt, ist ein Xrun und knackt.

**Rust hilft hier nicht automatisch.** Speichersicherheit und Echtzeitfähigkeit sind verschiedene Eigenschaften. `Vec::push` kann reallozieren, `println!` nimmt ein Lock, `Mutex::lock` kann den Thread schlafen legen, `panic!` entrollt den Stack, und jede dieser Operationen ist in sicherem Rust erlaubt. Die Disziplin muss von außen kommen — im Fall von Takt über `assert_no_alloc` und über den Typ [`AudioClockWriter`](../crates/takt-clock/src/audio_master.rs), der dem Audio-Thread nur genau die eine erlaubte Operation anbietet.

---

## Die tatsaechliche Thread-Aufstellung

| Thread | Wer erzeugt ihn | Klasse | Deadline |
|---|---|---|---|
| Audio-Callback | **das Betriebssystem** — WASAPI, CoreAudio, PipeWire | hart-realtime | Puffergröße / Abtastrate |
| Demux | wir | soft | keine, nur Durchsatz |
| Decode-Pool | FFmpeg intern, ab Phase 1 teils wir | soft | Durchsatz |
| GPU-Submission | wir | soft, framegebunden | Bildwiederholrate |
| UI | die Fensterbibliothek, meist der Haupt-Thread | soft | Bildwiederholrate |
| Offline-Render-Pool | wir, ab Phase 3 | Batch | keine |
| Encoder | wir, ab Phase 2 | Batch bis soft | bei Live-Streams die Ausgaberate |

Zwei Punkte daran werden regelmäßig missverstanden.

**Der Audio-Thread ist nicht unser Thread.** Wir erzeugen ihn nicht, wir konfigurieren ihn nicht, und wir können seine Deadline nicht verhandeln. Das Audio-Subsystem ruft uns auf *seinem* Thread auf, mit *seiner* Puffergröße und *seiner* Priorität. Wir erben die Bedingungen und müssen sie einhalten. Das ist der Grund, warum die Kommunikation zur Uhr über einen wartefreien Kanal laufen muss und nicht über einen Mutex: wir können nicht entscheiden, dass wir „diesmal kurz warten".

**Die Modi sind Zustände, keine Threads.** Dazu der nächste Abschnitt.

---

## Warum nicht ein Thread pro Modus

Der Gedanke, jedem Modus — Player, Studio, Editor, DAW — einen eigenen Thread zu geben, klingt nach saubererer Trennung. Er ist aber eine Kategorienverwechslung:

* Threads sind **gleichzeitige Arbeitslasten**.
* Modi sind **einander ausschließende Zustände** der Anwendung.

Zu jedem Zeitpunkt gibt genau eine Uhr den Takt vor. Zwei gleichzeitig laufende Modus-Uhren bedeuten zwei Antworten auf die Frage „welche Zeit ist jetzt" — und die Fehler, die daraus entstehen, sind praktisch nicht auffindbar, weil sie sich als sporadisches Ruckeln oder als Drift zeigen, nicht als Absturz.

Was tatsächlich parallel läuft, ist eine **Rollen**-Aufteilung: eine Quelle liest, ein Decoder dekodiert, ein Renderer stellt dar, ein Encoder schreibt. Diese Rollen existieren in allen Modi; nur ihre Verschaltung und ihre Taktquelle ändern sich.

Ein Modus wird deshalb in Takt nicht durch einen Thread abgebildet, sondern durch:

1. die aktive [`Clock`](../crates/takt-clock/src/lib.rs)-Implementierung,
2. die Verschaltung des Node-Graphen,
3. welche Subsysteme überhaupt hochgefahren sind.

Genau daraus folgt der Ressourcenvorteil des Player-Modus: Encoder- und Capture-Subsysteme existieren dort nicht, statt untätig Threads zu belegen.

---

## Warum mehr Threads schlechter sein koennen

Für Realtime-Arbeit ist ein zusätzlicher Thread nicht neutral, sondern hat Kosten:

* **Scheduling-Jitter.** Jeder Thread ist ein weiterer Kandidat für Verdrängung durch den Betriebssystem-Scheduler. Bei einer 2,7-ms-Deadline ist eine Verdrängung von 1 ms bereits fatal.
* **Cache-Verdrängung.** Threads, die auf verschiedenen Kernen dieselben Daten anfassen, erzeugen Cache-Line-Bouncing. Das ist bei Audio-Puffern besonders teuer, weil sie klein und heiß sind.
* **Synchronisationspunkte.** Jede Thread-Grenze braucht einen Kanal, und jeder Kanal ist eine Stelle, an der Latenz und Fehler entstehen.
* **Nicht-Determinismus in Tests.** Ein Fehler, der nur bei einer bestimmten Verschränkung auftritt, ist um Größenordnungen teurer zu finden.

Die Regel lautet also nicht „so viele Threads wie Kerne", sondern: **so wenige Threads wie möglich, und genau einer davon ist heilig.**

---

## Die drei Uhren

Alle Modi arbeiten gegen denselben Trait, [`Clock`](../crates/takt-clock/src/lib.rs). Der Rest der Pipeline weiß nicht, in welchem Modus er läuft.

```rust
pub trait Clock: Send + Sync {
    fn kind(&self) -> ClockKind;
    fn now(&self) -> Timestamp;
    fn until_nanos(&self, pts: Timestamp) -> i64;   // negativ = zu spät
    fn may_drop(&self) -> bool;
    fn is_due(&self, pts: Timestamp, tolerance_nanos: i64) -> bool { ... }
}
```

| Implementierung | Modus | Taktquelle | `may_drop` |
|---|---|---|---|
| `AudioMasterClock` | Player, DAW | Audio-Wiedergabeposition, zwischen Callbacks interpoliert | `true` |
| `WallClock` | Studio | monotone Systemuhr | `true` |
| `TimelineClock` | Editor | explizit gesetzte Position | **`false`** |

`may_drop` ist die eigentliche Trennlinie. Sie unterscheidet nicht Taktquellen, sondern **Datenflussrichtungen**:

* Player und Studio **schieben** Frames. Die Uhr läuft weiter, ob der Renderer fertig ist oder nicht. Wer zu spät kommt, fällt weg. Latenz schlägt Qualität.
* Der Editor **zieht** Frames. Die Position steht, bis jemand sie ändert. Der Renderer darf sich Zeit nehmen. Qualität schlägt Latenz — ein übersprungenes Frame im Export ist ein Fehler, kein Kompromiss.

Ein einziger Boolean-Wert trägt hier den ganzen Unterschied zwischen einem Player und einem Schnittprogramm. Das ist der Grund, warum eine gemeinsame Pipeline-Abstraktion über alle Modi zwangsläufig für alle schlecht wird: sie müsste diesen Unterschied verwischen.

---

## Der Ankerpunkt und das Seqlock

Die Audio-Master-Uhr steht vor einem konkreten Problem: der Audio-Callback läuft grob — bei 480 Samples und 48 kHz alle 10 ms. Der Renderer braucht die Position feiner, nämlich zu jedem Bildschirmdurchlauf, also zu einem beliebigen Zeitpunkt zwischen zwei Callbacks.

Die Lösung ist ein **Anker**: der Audio-Thread hinterlegt „bei Systemzeit T waren N Samples abgespielt". Der Renderer liest den Anker und rechnet mit der monotonen Systemuhr auf jetzt weiter.

```
Audio-Thread                          Render-Thread
     |                                      |
     | publish(samples=480, nanos=10ms)     |
     |------------> [AnchorCell] <----------- load()
     | (wartefrei)                          | (interpoliert auf jetzt)
```

Zwei Werte müssen dabei **gemeinsam konsistent** gelesen werden. Zwei unabhängige `AtomicU64` erlauben einen zerrissenen Lesevorgang: neue Sample-Zahl mit altem Zeitstempel, was einen Sprung in der interpolierten Position ergibt — genau die Art Ruckeln, die man nie erklären kann.

Ein `Mutex` löst das, verletzt aber die Invariante des Audio-Threads. „Meist frei" ist bei einer harten Deadline keine Zusage.

Die Antwort ist ein **Seqlock**:

* Der Schreiber erhöht einen Zähler auf ungerade, schreibt beide Werte, erhöht auf gerade. Er blockiert nie und wartet nie.
* Der Leser merkt sich den Zähler, liest, prüft den Zähler erneut. Bei Abweichung oder ungeradem Wert wiederholt er.

Der ungerade Zustand hält nur zwei Speicherzugriffe lang, ein Wiederholversuch ist entsprechend selten und immer kurz. Der Aufbau kommt ohne `unsafe` aus, weil beide Nutzdaten selbst Atomics sind — das Seqlock schützt nicht den *Zugriff*, sondern nur die *Zusammengehörigkeit*. Implementierung und Nebenläufigkeitstest: [`crates/takt-clock/src/anchor.rs`](../crates/takt-clock/src/anchor.rs).

**Eine Falle darin, die ARM64 in der CI tatsächlich auslöste:** `Release` auf dem Zähler-Store und `Acquire` auf dem Zähler-Load reichen nicht aus. Eine einzelne `Release`-Operation garantiert nur, dass alles *davor* sichtbar wird, bevor sie selbst sichtbar wird — sie verhindert nicht, dass *nachfolgende* `Relaxed`-Stores vor sie hochwandern. Symmetrisch beim `Acquire`-Load. x86 ordnet durch sein starkes Speichermodell (TSO) ohnehin kaum um, weshalb der Fehler dort unsichtbar blieb; auf Apple Silicon (ARM64) riss der Lesevorgang tatsächlich. Behoben mit einem expliziten `fence(Release)` zwischen dem ungeraden Zählerstand und den beiden Datenschreibvorgängen, und einem `fence(Acquire)` zwischen den beiden Datenlesevorgängen und dem zweiten Zählercheck — ein Fence erzwingt die volle Ordnung, statt sie stillschweigend von einer Einzeloperation zu erhoffen.

**Die Extrapolation ist begrenzt.** Bleibt der Anker länger aus als etwa zwei Callback-Perioden, wird nicht weiter interpoliert. Ein alter Anker bedeutet, dass der Audio-Thread ins Stocken geraten ist; eine weiter extrapolierte Position wäre eine Behauptung ohne Grundlage. Stattdessen bleibt die Uhr stehen, und `anchor_age_nanos()` macht das Problem in der Diagnoseanzeige sichtbar.

---

## Thread-Prioritaet und ihre Fallstricke

Der Audio-Thread braucht erhöhte Priorität. Das ist plattformabhängig und schlägt in der Praxis regelmäßig fehl:

| Plattform | Mechanismus | Typischer Fehlschlag |
|---|---|---|
| Windows | `AvSetMmThreadCharacteristics` mit `"Pro Audio"` | funktioniert meist; Energiesparprofile untergraben es |
| Linux | `SCHED_FIFO` über `pthread_setschedparam` | **braucht `CAP_SYS_NICE` oder eine Regel in `limits.conf`** — auf einer nicht vorbereiteten Installation schlägt es fehl |
| macOS | Thread Time Constraint Policy | eigene API, andere Semantik |

Der Linux-Fall ist der klassische „funktioniert bei mir": auf einem für Audio konfigurierten Arch läuft es, auf einem Standard-Ubuntu nicht. Wer das nicht bemerkt, sucht die Ursache von Xruns wochenlang im eigenen Code.

**Regel für Takt:** Priorität anfordern, Fehlschlag *erkennen*, dem Nutzer sichtbar melden, und mit normaler Priorität weiterlaufen. Niemals stumm scheitern, und niemals abbrechen.

---

## Invarianten

Die Punkte, die ohne Diskussion gelten und in Reviews geprüft werden:

1. **Genau ein hart-echtzeitfähiger Thread.** Er wird vom Betriebssystem erzeugt, nicht von uns.
2. **Im Audio-Callback:** keine Allokation, kein Lock, kein Datei- oder Netzwerkzugriff, kein Logging, kein `panic!`. `assert_no_alloc` bleibt aktiv, auch im Release-Build der Entwicklungsversionen.
3. **Genau eine Uhr ist aktiv.** Der Moduswechsel tauscht die Uhr, er startet keine zweite.
4. **Zeitstempel sind niemals `f64`.** Immer `Timestamp` mit rationaler Basis. Gleitkomma ausschließlich zur Anzeige, und das Ergebnis fließt nie in eine Rechnung zurück.
5. **Kommunikation zum Audio-Thread ausschließlich wartefrei:** Atomics, Seqlock, lock-freie SPSC-Ringpuffer.
6. **Zähler statt Protokoll.** Der Audio-Thread schreibt Zahlen, die ein anderer Thread ausliest und ausgibt.
7. **`takt-audio` und `takt-clock` kennen keine Modi.** Sie kennen Puffer und Anker. Welcher Modus sie füllt, ist ihnen gleichgültig.

Wachsende Quellen (eine Datei oder ein Strom, der beim Abspielen noch länger wird) ändern an den Uhren nichts: Siehe [SOURCES.md](SOURCES.md), „Wachsende Quellen im Player“.
