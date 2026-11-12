// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Die Zeitmodelle der Takt-Betriebsmodi.
//!
//! # Warum es drei Uhren gibt und nicht eine
//!
//! Player, Studio und Editor machen dasselbe mit Bild und Ton. Sie
//! unterscheiden sich ausschliesslich darin, **wer den Takt vorgibt** — und
//! genau deshalb koennen sie sich keinen gemeinsamen Scheduler teilen:
//!
//! | Modus  | Taktquelle                 | Bei Verzug        |
//! |--------|----------------------------|-------------------|
//! | Player | Audio-Wiedergabeposition   | Frame verwerfen   |
//! | Studio | monotone Systemuhr         | Frame verwerfen   |
//! | Editor | Timeline-Position          | warten            |
//! | DAW    | Audio-Callback             | darf nicht auftreten |
//!
//! Der Rest der Pipeline kennt nur [`Clock`] und muss nicht wissen, in
//! welchem Modus er laeuft.
//!
//! # Genau eine Uhr ist aktiv
//!
//! Zu jedem Zeitpunkt gibt **eine** Uhr den Takt vor. Zwei gleichzeitig
//! laufende Uhren bedeuten zwei Wahrheiten ueber „jetzt", und die daraus
//! entstehenden Fehler sind praktisch nicht auffindbar.
//!
//! # Zone
//!
//! Lizenz-Zone 1 (permissiv), frei von `unsafe`. Die Synchronisation zwischen
//! Audio- und Render-Thread laeuft ueber ein Seqlock aus Atomics, siehe
//! [`anchor`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod anchor;
mod audio_master;
mod seekable;
mod timeline;
mod wall;

use takt_core::Timestamp;

pub use anchor::{Anchor, AnchorCell, ManualMonotonic, MonotonicSource, SystemMonotonic};
pub use audio_master::{AudioClockWriter, AudioMasterClock};
pub use seekable::SeekableClock;
pub use timeline::TimelineClock;
pub use wall::WallClock;

/// Welches Zeitmodell eine Uhr umsetzt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClockKind {
    /// Die Audio-Wiedergabeposition gibt den Takt vor. Player und DAW.
    AudioMaster,
    /// Die monotone Systemuhr gibt den Takt vor. Studio.
    Wall,
    /// Eine explizit gesetzte Position gibt den Takt vor. Editor.
    Timeline,
}

/// Die Praesentationsuhr eines Betriebsmodus.
///
/// Implementierungen muessen `Send + Sync` sein, weil Render- und UI-Thread
/// gleichzeitig lesen.
pub trait Clock: Send + Sync {
    /// Welches Zeitmodell.
    fn kind(&self) -> ClockKind;

    /// Aktuelle Position der Praesentationsuhr.
    fn now(&self) -> Timestamp;

    /// Nanosekunden bis zur Praesentation von `pts`.
    ///
    /// Negativ bedeutet, dass der Zeitpunkt bereits verstrichen ist — das
    /// Frame ist zu spaet. Wie darauf reagiert wird, entscheidet
    /// [`Clock::may_drop`].
    fn until_nanos(&self, pts: Timestamp) -> i64;

    /// Ob dieser Modus bei Verzug Frames verwerfen darf.
    ///
    /// `true` bei Realtime-Modi, `false` beim Editor: dort wartet der Renderer,
    /// weil Qualitaet vor Tempo geht.
    fn may_drop(&self) -> bool;

    /// Ob `pts` jetzt praesentiert werden soll.
    ///
    /// `tolerance_nanos` ist die halbe Frame-Dauer: ein Frame gilt als
    /// faellig, sobald es naeher an jetzt liegt als an seinem Nachfolger.
    fn is_due(&self, pts: Timestamp, tolerance_nanos: i64) -> bool {
        self.until_nanos(pts) <= tolerance_nanos
    }
}

/// Nanosekunden zwischen zwei Zeitstempeln, `to - from`.
///
/// Ausgelagert, weil alle drei Uhren dieselbe Rechnung brauchen.
fn delta_nanos(from: Timestamp, to: Timestamp) -> i64 {
    let Some(from_ns) = from.rescale(takt_core::Rational::NANOS) else {
        return 0;
    };
    let Some(to_ns) = to.rescale(takt_core::Rational::NANOS) else {
        return 0;
    };
    to_ns.ticks().saturating_sub(from_ns.ticks())
}
