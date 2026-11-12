// SPDX-License-Identifier: GPL-3.0-or-later
//! Die Naht zwischen Dekodierung und Darstellung.
//!
//! # Warum diese Abstraktion zuerst entsteht
//!
//! In Phase 0 steht hinter [`VideoBackend`] libmpv, ab Phase 1 die eigene
//! Pipeline. Der Trait ist kein Zugestaendnis an einen Zwischenschritt,
//! sondern das Sicherheitsnetz des Projekts: libmpv bleibt danach als
//! **Referenz-Orakel** erhalten. Zwei Backends, dieselbe Datei, dieselben
//! Zeitstempel — Abweichungen sind Fehler in der eigenen Pipeline und werden
//! sichtbar, statt sich als „sieht irgendwie ruckeliger aus" zu tarnen.
//!
//! Scheitert Phase 1, bleibt der Player benutzbar. Das ist der Unterschied
//! zwischen einem Gate und einem Absturz.
//!
//! # Zone
//!
//! Lizenz-Zone 2 (`GPL-3.0-or-later`), weil hier FFmpeg und libmpv
//! angebunden werden.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod null;
mod scheduler;

use takt_core::{FrameMeta, Rational, Timestamp};

pub use null::{NullBackend, Pattern};
pub use scheduler::{Presentation, Scheduler, SchedulerStats};

/// Was ein Backend ueber die geoeffnete Quelle weiss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaInfo {
    /// Gesamtdauer, falls bekannt. Bei Live-Quellen `None`.
    pub duration: Option<Timestamp>,
    /// Nominelle Bildwiederholrate, falls bekannt.
    pub frame_rate: Option<Rational>,
    /// Geometrie und Farbraum des ersten Videostroms.
    pub video: Option<FrameMeta>,
    /// Abtastrate des ersten Audiostroms.
    pub sample_rate: Option<u32>,
    /// Ob die Quelle Positionierung erlaubt. Netzwerk-Streams oft nicht.
    pub seekable: bool,
}

impl MediaInfo {
    /// Nominelle Frame-Dauer in Nanosekunden, falls die Rate bekannt ist.
    #[must_use]
    pub fn frame_duration_nanos(&self) -> Option<i64> {
        let fps = self.frame_rate?;
        if fps.num() == 0 {
            return None;
        }
        Some(1_000_000_000_i64 * fps.den() / fps.num())
    }
}

/// Wie positioniert werden soll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekMode {
    /// Zum naechsten Keyframe. Schnell, ungenau. Fuer die Wiedergabe.
    Keyframe,
    /// Frame-genau. Erfordert Dekodieren ab dem Keyframe davor.
    /// Fuer den Editor unverzichtbar.
    Exact,
}

/// Zaehler eines Backends. Bewusst nur Zahlen, kein Text.
///
/// Sie werden aus dem Render-Thread gelesen und muessen deshalb billig sein.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BackendStats {
    /// Erfolgreich dekodierte Frames.
    pub decoded: u64,
    /// Wegen Verzug verworfene Frames.
    pub dropped: u64,
    /// Wiederholt dargestellte Frames, weil kein neues fertig war.
    pub repeated: u64,
    /// Frames, die derzeit im Puffer auf Darstellung warten.
    pub queued: usize,
}

/// Eine Quelle, aus der Frames gelesen werden.
pub trait VideoBackend: Send {
    /// Kurzname des Backends fuer Diagnose-Anzeigen.
    fn name(&self) -> &'static str;

    /// Oeffnet eine Quelle und ermittelt ihre Eigenschaften.
    fn open(&mut self, source: &str) -> takt_core::Result<MediaInfo>;

    /// Positioniert die Quelle.
    fn seek(&mut self, to: Timestamp, mode: SeekMode) -> takt_core::Result<()>;

    /// Liefert das naechste Frame, sofern eines faellig ist.
    ///
    /// Blockiert nicht. `Ok(None)` bedeutet „noch nichts faellig", nicht
    /// „Ende der Quelle" — dafuer gibt es [`VideoBackend::is_eof`].
    ///
    /// Das Backend entscheidet anhand von [`takt_clock::Clock::may_drop`],
    /// ob es verspaetete Frames verwirft oder ausliefert.
    fn next_frame(
        &mut self,
        clock: &dyn takt_clock::Clock,
    ) -> takt_core::Result<Option<takt_core::PooledFrame>>;

    /// The period of the display that shows the frames, in nanoseconds, or
    /// `None` when unknown (the default).
    ///
    /// A frame is due once it is closer to now than half a display period:
    /// it then goes to the refresh nearest its timestamp, and the error stays
    /// within half a period, at 60 Hz as at 120 Hz. Without a period the
    /// tolerance is half a frame duration, which grows the error with the
    /// refresh rate (an early frame waits on screen for its time).
    fn set_display_period(&mut self, nanos: Option<i64>) {
        let _ = nanos;
    }

    /// Ob die Quelle erschoepft ist.
    fn is_eof(&self) -> bool;

    /// Aktuelle Zaehler.
    fn stats(&self) -> BackendStats;
}

#[cfg(test)]
mod tests {
    use super::*;
    use takt_core::{ColorPrimaries, ColorRange, PixelFormat, TransferFunction};

    fn info(fps: Option<Rational>) -> MediaInfo {
        MediaInfo {
            duration: Some(Timestamp::from_millis(1_000)),
            frame_rate: fps,
            video: Some(FrameMeta {
                width: 16,
                height: 16,
                format: PixelFormat::Nv12,
                primaries: ColorPrimaries::Bt709,
                transfer: TransferFunction::Bt709,
                range: ColorRange::Limited,
                pts: Timestamp::from_millis(0),
            }),
            sample_rate: Some(48_000),
            seekable: true,
        }
    }

    #[test]
    fn framedauer_bei_ganzzahliger_rate() {
        let i = info(Rational::new(25, 1));
        assert_eq!(i.frame_duration_nanos(), Some(40_000_000));
    }

    #[test]
    fn framedauer_bei_ntsc() {
        // 24000/1001 fps: 41,708333... ms. Ganzzahlig gerechnet, nicht als f64.
        let i = info(Rational::new(24_000, 1_001));
        assert_eq!(i.frame_duration_nanos(), Some(41_708_333));
    }

    #[test]
    fn ohne_rate_keine_framedauer() {
        assert_eq!(info(None).frame_duration_nanos(), None);
        // Rate 0/1 darf nicht durch Null teilen.
        assert_eq!(info(Rational::new(0, 1)).frame_duration_nanos(), None);
    }
}
