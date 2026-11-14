// SPDX-License-Identifier: GPL-3.0-or-later
//! Der Praesentations-Scheduler und seine Messwerte.
//!
//! # Was hier gemessen wird
//!
//! Der Scheduler ist die Stelle, an der sich entscheidet, ob die Wiedergabe
//! gut aussieht. Und er ist die Stelle, an der man es **messen** kann, statt
//! es zu beurteilen. Er fuehrt deshalb von Anfang an drei Groessen:
//!
//! * **Praesentationsfehler** — die Abweichung zwischen dem Zeitpunkt, an dem
//!   ein Frame gezeigt werden sollte, und dem, an dem es gezeigt wurde. Das
//!   Histogramm dieser Werte ist die eigentliche Qualitaetskennzahl einer
//!   Video-Pipeline.
//! * **Wiederholungen** — kein neues Frame war fertig, das alte bleibt stehen.
//! * **Verwerfungen** — ein Frame kam zu spaet und wurde uebersprungen.
//!
//! Der Mittelwert des Praesentationsfehlers ist dabei nahezu bedeutungslos.
//! Ruckeln entsteht aus der *Streuung*: gleichmaessig 8 ms zu spaet sieht
//! niemand, abwechselnd 0 und 16 ms sieht jeder. Deshalb werden Extrema und
//! Perzentile gefuehrt, nicht nur der Durchschnitt.

use takt_clock::Clock;
use takt_core::{PooledFrame, Timestamp};

/// Was mit einem Frame geschehen ist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presentation {
    /// Frame wurde dargestellt, mit dem angegebenen Fehler in Nanosekunden.
    ///
    /// Negativ bedeutet zu frueh, positiv zu spaet.
    Shown {
        /// Praesentationsfehler in Nanosekunden.
        error_nanos: i64,
    },
    /// Kein neues Frame verfuegbar, das vorige bleibt stehen.
    Repeated,
    /// Quelle erschoepft.
    Eof,
}

/// Gesammelte Messwerte einer Wiedergabe.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    /// Dargestellte Frames.
    pub shown: u64,
    /// Wiederholte Frames.
    pub repeated: u64,
    /// Summe der Fehlerbetraege, fuer den Mittelwert.
    pub abs_error_sum_nanos: i64,
    /// Groesster Verzug in Nanosekunden.
    pub worst_late_nanos: i64,
    /// Groesste Verfruehung in Nanosekunden.
    pub worst_early_nanos: i64,
    /// Histogramm der Fehlerbetraege in logarithmischen Klassen.
    ///
    /// Klasse `i` sammelt Fehler zwischen `2^i` und `2^(i+1)`
    /// Mikrosekunden — Klasse 0 also unter 2 us, Klasse 14 ab 16,4 ms.
    pub histogram: [u32; 16],
}

impl SchedulerStats {
    /// Mittlerer Fehlerbetrag in Nanosekunden.
    #[must_use]
    pub fn mean_abs_error_nanos(&self) -> i64 {
        if self.shown == 0 {
            return 0;
        }
        self.abs_error_sum_nanos / i64::try_from(self.shown).unwrap_or(1)
    }

    /// Anteil wiederholter Frames in Prozent. **Nur zur Anzeige.**
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn repeat_ratio(&self) -> f64 {
        let total = self.shown + self.repeated;
        if total == 0 {
            return 0.0;
        }
        self.repeated as f64 * 100.0 / total as f64
    }

    /// An upper bound of the quantile `num/den` of the error magnitudes, in
    /// microseconds: the top of the histogram class that holds it, and never
    /// more than the largest error seen. `None` without a shown frame or
    /// with `den == 0`.
    ///
    /// The classes double, so the bound is at most twice the true value;
    /// that is the price of sixteen counters instead of a list of samples.
    #[must_use]
    pub fn quantile_upper_micros(&self, num: u64, den: u64) -> Option<u64> {
        if self.shown == 0 || den == 0 {
            return None;
        }
        let total: u64 = self.histogram.iter().map(|&c| u64::from(c)).sum();
        // The smallest rank that covers num/den of the samples, at least 1.
        let rank = u128::from(total) * u128::from(num.min(den));
        let rank = u64::try_from(rank.div_ceil(u128::from(den)))
            .unwrap_or(u64::MAX)
            .max(1);
        let worst = self
            .worst_late_nanos
            .unsigned_abs()
            .max(self.worst_early_nanos.unsigned_abs())
            .div_ceil(1_000);
        let mut seen = 0_u64;
        for (class, &count) in self.histogram.iter().enumerate() {
            seen += u64::from(count);
            if seen >= rank {
                // The last class is open at the top: only the worst bounds it.
                let top = if class + 1 == self.histogram.len() {
                    u64::MAX
                } else {
                    1_u64 << (class + 1)
                };
                return Some(top.min(worst));
            }
        }
        Some(worst)
    }

    fn record(&mut self, error_nanos: i64) {
        self.shown += 1;
        self.abs_error_sum_nanos = self
            .abs_error_sum_nanos
            .saturating_add(error_nanos.saturating_abs());
        if error_nanos > self.worst_late_nanos {
            self.worst_late_nanos = error_nanos;
        }
        if error_nanos < self.worst_early_nanos {
            self.worst_early_nanos = error_nanos;
        }
        let micros = error_nanos.saturating_abs() / 1_000;
        // Logarithmische Klasse: ilog2 der Mikrosekunden, auf 15 begrenzt.
        let bucket = if micros < 2 {
            0
        } else {
            usize::try_from(micros.ilog2()).unwrap_or(15).min(15)
        };
        self.histogram[bucket] = self.histogram[bucket].saturating_add(1);
    }
}

/// Entscheidet je Anzeigedurchlauf, welches Frame dargestellt wird.
///
/// Der Scheduler haelt selbst kein Frame; er bewertet nur, was das Backend
/// liefert, und fuehrt die Messwerte. Damit bleibt er von der Frage
/// unabhaengig, ob Frames im Hauptspeicher oder im VRAM liegen.
#[derive(Debug, Default)]
pub struct Scheduler {
    stats: SchedulerStats,
    last_shown: Option<Timestamp>,
}

impl Scheduler {
    /// Neuer Scheduler ohne Messwerte.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bewertet ein vom Backend geliefertes Frame.
    ///
    /// `frame` ist `None`, wenn nichts faellig war. Der Rueckgabewert sagt,
    /// was der Renderer tun soll.
    pub fn accept(
        &mut self,
        clock: &dyn Clock,
        frame: Option<&PooledFrame>,
        eof: bool,
    ) -> Presentation {
        match frame {
            Some(f) => {
                let pts = f.get().meta().pts;
                // Positiver Fehler = das Frame kommt zu spaet.
                let error = -clock.until_nanos(pts);
                self.stats.record(error);
                self.last_shown = Some(pts);
                Presentation::Shown { error_nanos: error }
            }
            None if eof => Presentation::Eof,
            None => {
                self.stats.repeated += 1;
                Presentation::Repeated
            }
        }
    }

    /// Aktuelle Messwerte.
    #[must_use]
    pub const fn stats(&self) -> &SchedulerStats {
        &self.stats
    }

    /// Zeitstempel des zuletzt dargestellten Frames.
    #[must_use]
    pub const fn last_shown(&self) -> Option<Timestamp> {
        self.last_shown
    }

    /// Setzt die Messwerte zurueck. Nach einem Seek sind sie nicht mehr
    /// aussagekraeftig.
    pub fn reset(&mut self) {
        self.stats = SchedulerStats::default();
        self.last_shown = None;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use takt_clock::{AudioMasterClock, ManualMonotonic, MonotonicSource};
    use takt_core::{
        ColorPrimaries, ColorRange, FrameMeta, FramePool, PixelFormat, Rational, TransferFunction,
    };

    use super::*;

    fn meta(pts: Timestamp) -> FrameMeta {
        FrameMeta {
            width: 16,
            height: 16,
            format: PixelFormat::Nv12,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt709,
            range: ColorRange::Limited,
            pts,
        }
    }

    fn clock_at_millis(ms: i64) -> (AudioMasterClock, Arc<ManualMonotonic>) {
        let src = Arc::new(ManualMonotonic::new());
        let clock = AudioMasterClock::new(
            48_000,
            Arc::clone(&src) as Arc<dyn MonotonicSource>,
            20_000_000,
        );
        src.set(0);
        clock
            .writer()
            .unwrap()
            .report(u64::try_from(48_000 * ms / 1_000).unwrap(), 0);
        (clock, src)
    }

    #[test]
    fn fehler_ist_null_bei_exaktem_timing() {
        let (clock, _s) = clock_at_millis(100);
        let mut pool = FramePool::new(meta(Timestamp::from_millis(0)), 2);
        let f = pool.acquire(Timestamp::from_millis(100)).unwrap();

        let mut s = Scheduler::new();
        assert_eq!(
            s.accept(&clock, Some(&f), false),
            Presentation::Shown { error_nanos: 0 }
        );
        assert_eq!(s.stats().shown, 1);
        assert_eq!(s.stats().mean_abs_error_nanos(), 0);
    }

    #[test]
    fn verzug_ist_positiv_verfruehung_negativ() {
        let (clock, _s) = clock_at_millis(100);
        let mut pool = FramePool::new(meta(Timestamp::from_millis(0)), 4);
        let mut s = Scheduler::new();

        // Frame haette bei 95 ms laufen sollen: 5 ms zu spaet.
        let late = pool.acquire(Timestamp::from_millis(95)).unwrap();
        assert_eq!(
            s.accept(&clock, Some(&late), false),
            Presentation::Shown {
                error_nanos: 5_000_000
            }
        );
        drop(late);

        // Frame ist fuer 105 ms gedacht: 5 ms zu frueh.
        let early = pool.acquire(Timestamp::from_millis(105)).unwrap();
        assert_eq!(
            s.accept(&clock, Some(&early), false),
            Presentation::Shown {
                error_nanos: -5_000_000
            }
        );

        assert_eq!(s.stats().worst_late_nanos, 5_000_000);
        assert_eq!(s.stats().worst_early_nanos, -5_000_000);
        // Der Mittelwert der Betraege ist 5 ms, obwohl sich die
        // vorzeichenbehafteten Fehler aufheben wuerden.
        assert_eq!(s.stats().mean_abs_error_nanos(), 5_000_000);
    }

    #[test]
    fn kein_frame_bedeutet_wiederholung() {
        let (clock, _s) = clock_at_millis(100);
        let mut s = Scheduler::new();
        assert_eq!(s.accept(&clock, None, false), Presentation::Repeated);
        assert_eq!(s.accept(&clock, None, false), Presentation::Repeated);
        assert_eq!(s.stats().repeated, 2);
        assert_eq!(s.stats().shown, 0);
        assert!((s.stats().repeat_ratio() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn eof_wird_unterschieden() {
        let (clock, _s) = clock_at_millis(100);
        let mut s = Scheduler::new();
        assert_eq!(s.accept(&clock, None, true), Presentation::Eof);
        assert_eq!(s.stats().repeated, 0, "EOF ist keine Wiederholung");
    }

    #[test]
    fn histogramm_sortiert_logarithmisch() {
        let (clock, _s) = clock_at_millis(1_000);
        let mut pool = FramePool::new(meta(Timestamp::from_millis(0)), 8);
        let mut s = Scheduler::new();

        // Exakt: Klasse 0.
        let a = pool.acquire(Timestamp::from_millis(1_000)).unwrap();
        s.accept(&clock, Some(&a), false);
        drop(a);

        // 1 ms Fehler = 1000 us -> ilog2(1000) = 9.
        let b = pool.acquire(Timestamp::from_millis(999)).unwrap();
        s.accept(&clock, Some(&b), false);
        drop(b);

        // 16 ms Fehler = 16000 us -> ilog2(16000) = 13.
        let c = pool.acquire(Timestamp::from_millis(984)).unwrap();
        s.accept(&clock, Some(&c), false);

        assert_eq!(s.stats().histogram[0], 1);
        assert_eq!(s.stats().histogram[9], 1);
        assert_eq!(s.stats().histogram[13], 1);
        assert_eq!(s.stats().shown, 3);
    }

    #[test]
    fn the_quantile_is_the_top_of_its_class_and_never_above_the_worst() {
        let mut stats = SchedulerStats::default();
        assert_eq!(stats.quantile_upper_micros(99, 100), None);

        // 98 errors of 100 us (class 6: 64..128), one of 5 ms, one of 40 ms.
        for _ in 0..98 {
            stats.record(100_000);
        }
        stats.record(-5_000_000);
        stats.record(40_000_000);
        assert_eq!(stats.quantile_upper_micros(1, 2), Some(128));
        assert_eq!(stats.quantile_upper_micros(98, 100), Some(128));
        // Rank 99 of 100 is the 5 ms error: class 12 ends at 8192 us.
        assert_eq!(stats.quantile_upper_micros(99, 100), Some(8_192));
        // The last class has no top; the worst error bounds it.
        assert_eq!(stats.quantile_upper_micros(1, 1), Some(40_000));
        assert_eq!(stats.quantile_upper_micros(7, 1), Some(40_000));
        assert_eq!(stats.quantile_upper_micros(1, 0), None);

        // One error only: the bound is the error itself, not the class top.
        let mut one = SchedulerStats::default();
        one.record(3_000_000);
        assert_eq!(one.quantile_upper_micros(99, 100), Some(3_000));
    }

    #[test]
    fn reset_loescht_die_messwerte() {
        let (clock, _s) = clock_at_millis(100);
        let mut pool = FramePool::new(meta(Timestamp::from_millis(0)), 2);
        let f = pool.acquire(Timestamp::from_millis(90)).unwrap();

        let mut s = Scheduler::new();
        s.accept(&clock, Some(&f), false);
        assert!(s.last_shown().is_some());

        s.reset();
        assert_eq!(s.stats(), &SchedulerStats::default());
        assert!(s.last_shown().is_none());
    }

    #[test]
    fn gleichmaessiger_verzug_streut_nicht() {
        // Der eigentliche Punkt der Messung: konstanter Verzug ist harmlos,
        // Streuung ist das Problem. Beides muss unterscheidbar sein.
        let src = Arc::new(ManualMonotonic::new());
        let clock = AudioMasterClock::new(
            48_000,
            Arc::clone(&src) as Arc<dyn MonotonicSource>,
            20_000_000,
        );
        let w = clock.writer().unwrap();
        let mut pool = FramePool::new(meta(Timestamp::from_millis(0)), 2);
        let mut s = Scheduler::new();

        // Zehn Frames, jedes exakt 4 ms zu spaet.
        for i in 0..10_i64 {
            let now_ms = i * 40 + 4;
            src.set(0);
            w.report(u64::try_from(48_000 * now_ms / 1_000).unwrap(), 0);
            let f = pool.acquire(Timestamp::from_millis(i * 40)).unwrap();
            s.accept(&clock, Some(&f), false);
        }

        assert_eq!(s.stats().shown, 10);
        assert_eq!(s.stats().worst_late_nanos, 4_000_000);
        assert_eq!(s.stats().worst_early_nanos, 0);
        // Alle Fehler liegen in derselben Klasse: keine Streuung.
        let occupied = s.stats().histogram.iter().filter(|&&c| c > 0).count();
        assert_eq!(occupied, 1);

        let _ = Rational::MILLIS; // Zeitbasis-Import belegt
    }
}
