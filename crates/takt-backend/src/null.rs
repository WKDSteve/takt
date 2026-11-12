// SPDX-License-Identifier: GPL-3.0-or-later
//! Ein Backend, das Frames selbst erzeugt.
//!
//! # Warum das kein Platzhalter ist
//!
//! `NullBackend` liefert deterministische Frames mit exakt bekannten
//! Zeitstempeln, ohne FFmpeg, ohne libmpv, ohne GPU, auf jeder Plattform.
//! Das macht es zu drei Dingen gleichzeitig:
//!
//! 1. **Testquelle.** Die gesamte Uhr-, Scheduler- und Anzeigemechanik laesst
//!    sich pruefen, ohne dass eine Datei oder eine native Bibliothek im Spiel
//!    ist. Fehler lassen sich damit eindeutig zuordnen: Liegt es an der
//!    Pipeline oder am Decoder?
//! 2. **Erste Plattform-Portierung.** Der Build laeuft auf Windows, Linux und
//!    macOS ohne eine einzige Systemabhaengigkeit. Damit steht die CI auf
//!    allen Zielen ab Tag eins gruen, statt erst „wenn mpv paketiert ist".
//! 3. **Referenzmuster.** Ein bekanntes Testbild macht Farbraum- und
//!    Chroma-Fehler im GPU-Pfad ab Phase 1 sofort sichtbar.

use takt_clock::Clock;
use takt_core::{Error, FrameMeta, FramePool, PooledFrame, Rational, Timestamp};

use crate::{BackendStats, MediaInfo, SeekMode, VideoBackend};

/// Welches Testbild erzeugt wird.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pattern {
    /// Gleichmaessiges Grau. Zeigt Wertebereichsfehler (Limited gegen Full).
    #[default]
    Flat,
    /// Senkrechte Balken. Zeigt Chroma-Fehlausrichtung.
    Bars,
    /// Ein wanderndes Rechteck. Zeigt Frame-Aussetzer und Ruckeln.
    Motion,
}

/// Ein Backend, das Frames rechnerisch erzeugt statt sie zu dekodieren.
#[derive(Debug)]
pub struct NullBackend {
    meta: FrameMeta,
    frame_rate: Rational,
    duration: Timestamp,
    pattern: Pattern,
    pool: FramePool,
    /// Index des naechsten zu erzeugenden Frames.
    next_index: i64,
    total_frames: i64,
    stats: BackendStats,
    /// See [`VideoBackend::set_display_period`].
    display_period_ns: Option<i64>,
}

impl NullBackend {
    /// Neues Backend fuer die angegebene Geometrie und Rate.
    ///
    /// `pool_capacity` begrenzt die gleichzeitig ausgegebenen Frames — genau
    /// wie ein Hardware-Decoder es spaeter tut.
    #[must_use]
    pub fn new(
        meta: FrameMeta,
        frame_rate: Rational,
        duration: Timestamp,
        pattern: Pattern,
        pool_capacity: usize,
    ) -> Self {
        let total = total_frames(frame_rate, duration);
        Self {
            meta,
            frame_rate,
            duration,
            pattern,
            pool: FramePool::new(meta, pool_capacity),
            next_index: 0,
            total_frames: total,
            stats: BackendStats::default(),
            display_period_ns: None,
        }
    }

    /// Zeitstempel des Frames mit dem angegebenen Index.
    ///
    /// Exakt in der Zeitbasis der Bildwiederholrate: Frame 24000 bei
    /// 24000/1001 fps liegt bei genau 1001 Sekunden, ohne Rundung.
    #[must_use]
    pub fn pts_of(&self, index: i64) -> Option<Timestamp> {
        Some(Timestamp::new(index, self.frame_rate.as_time_base()?))
    }

    /// Wie viele Frames die Quelle insgesamt hat.
    #[must_use]
    pub const fn total_frames(&self) -> i64 {
        self.total_frames
    }

    /// Index des naechsten Frames.
    #[must_use]
    pub const fn next_index(&self) -> i64 {
        self.next_index
    }

    /// How early a frame may be handed out: half the display period when it
    /// is known, never more than half a frame duration (then the next frame
    /// is nearer).
    #[must_use]
    pub fn due_tolerance_nanos(&self) -> i64 {
        let frame_ns = 1_000_000_000_i64 * self.frame_rate.den() / self.frame_rate.num().max(1);
        let half_frame = frame_ns / 2;
        self.display_period_ns
            .filter(|&p| p > 0)
            .map_or(half_frame, |p| (p / 2).min(half_frame))
    }

    /// Fuellt ein Frame mit dem eingestellten Muster.
    fn paint(&self, frame: &mut takt_core::VideoFrame, index: i64) {
        let w = self.meta.width as usize;
        let h = self.meta.height as usize;

        // Luma-Ebene.
        if let (Some(stride), Some(plane)) = (frame.stride(0), frame.plane_mut(0)) {
            match self.pattern {
                Pattern::Flat => plane.fill(128),
                Pattern::Bars => {
                    for y in 0..h {
                        let row = &mut plane[y * stride..y * stride + w.min(stride)];
                        for (x, px) in row.iter_mut().enumerate() {
                            // Acht Balken von dunkel nach hell.
                            let bar = u8::try_from(x * 8 / w.max(1)).unwrap_or(7).min(7);
                            *px = 16 + bar * 30;
                        }
                    }
                }
                Pattern::Motion => {
                    plane.fill(16);
                    // Ein Block, der pro Frame um eine Spalte wandert.
                    let block = (w / 8).max(1);
                    let x0 = (usize::try_from(index.rem_euclid(i64::MAX)).unwrap_or(0)) % w.max(1);
                    for y in (h / 4)..(h * 3 / 4).max(h / 4 + 1) {
                        for dx in 0..block {
                            let x = (x0 + dx) % w.max(1);
                            let idx = y * stride + x;
                            if idx < plane.len() {
                                plane[idx] = 235;
                            }
                        }
                    }
                }
            }
        }

        // Chroma-Ebenen: neutral, also farblos.
        for p in 1..self.meta.format.planes() {
            if let Some(plane) = frame.plane_mut(p) {
                plane.fill(128);
            }
        }
    }
}

/// Anzahl Frames, die in `duration` bei `rate` passen.
fn total_frames(rate: Rational, duration: Timestamp) -> i64 {
    let Some(base) = rate.as_time_base() else {
        return 0;
    };
    duration.rescale(base).map_or(0, Timestamp::ticks).max(0)
}

impl VideoBackend for NullBackend {
    fn name(&self) -> &'static str {
        "null"
    }

    fn open(&mut self, _source: &str) -> takt_core::Result<MediaInfo> {
        self.next_index = 0;
        self.stats = BackendStats::default();
        Ok(MediaInfo {
            duration: Some(self.duration),
            frame_rate: Some(self.frame_rate),
            video: Some(self.meta),
            sample_rate: Some(48_000),
            seekable: true,
        })
    }

    fn seek(&mut self, to: Timestamp, _mode: SeekMode) -> takt_core::Result<()> {
        // Eine gerechnete Quelle ist immer frame-genau positionierbar; der
        // Unterschied zwischen Keyframe und Exact existiert hier nicht.
        let base = self
            .frame_rate
            .as_time_base()
            .ok_or(Error::InvalidTimeBase)?;
        let index = to.rescale(base).ok_or(Error::TimeOverflow)?.ticks();
        self.next_index = index.clamp(0, self.total_frames);
        Ok(())
    }

    fn next_frame(&mut self, clock: &dyn Clock) -> takt_core::Result<Option<PooledFrame>> {
        if self.is_eof() {
            return Ok(None);
        }

        let pts = self.pts_of(self.next_index).ok_or(Error::InvalidTimeBase)?;

        // Due at the display refresh nearest the timestamp (half a display
        // period), see `due_tolerance_nanos`.
        if !clock.is_due(pts, self.due_tolerance_nanos()) {
            return Ok(None);
        }

        // Verspaetete Frames ueberspringen, falls der Modus das erlaubt.
        if clock.may_drop() {
            let mut skipped = 0_u64;
            while self.next_index + 1 < self.total_frames {
                let next = self
                    .pts_of(self.next_index + 1)
                    .ok_or(Error::InvalidTimeBase)?;
                if clock.until_nanos(next) > 0 {
                    break;
                }
                self.next_index += 1;
                skipped += 1;
            }
            self.stats.dropped += skipped;
        }

        let pts = self.pts_of(self.next_index).ok_or(Error::InvalidTimeBase)?;
        let mut pooled = self.pool.acquire(pts).ok_or(Error::PoolExhausted {
            capacity: self.pool.capacity(),
        })?;

        let index = self.next_index;
        self.paint(pooled.get_mut(), index);
        self.next_index += 1;
        self.stats.decoded += 1;
        self.stats.queued = self.pool.in_flight();
        Ok(Some(pooled))
    }

    fn is_eof(&self) -> bool {
        self.next_index >= self.total_frames
    }

    fn stats(&self) -> BackendStats {
        self.stats
    }

    fn set_display_period(&mut self, nanos: Option<i64>) {
        self.display_period_ns = nanos;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use takt_clock::{AudioMasterClock, ManualMonotonic, MonotonicSource, TimelineClock};
    use takt_core::{ColorPrimaries, ColorRange, PixelFormat, TransferFunction};

    use super::*;

    fn meta() -> FrameMeta {
        FrameMeta {
            width: 64,
            height: 32,
            format: PixelFormat::Nv12,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt709,
            range: ColorRange::Limited,
            pts: Timestamp::from_millis(0),
        }
    }

    fn backend(pattern: Pattern) -> NullBackend {
        NullBackend::new(
            meta(),
            Rational::lit(25, 1),
            Timestamp::from_millis(1_000),
            pattern,
            4,
        )
    }

    #[test]
    fn kennt_seine_framezahl() {
        let b = backend(Pattern::Flat);
        assert_eq!(b.total_frames(), 25); // 1 s bei 25 fps
        assert!(!b.is_eof());
    }

    #[test]
    fn zeitstempel_sind_exakt_bei_ntsc() {
        let b = NullBackend::new(
            meta(),
            Rational::lit(24_000, 1_001),
            Timestamp::from_millis(60_000),
            Pattern::Flat,
            4,
        );
        // Frame 24000 liegt bei exakt 1001 Sekunden.
        let pts = b.pts_of(24_000).unwrap();
        assert_eq!(pts.rescale(Rational::MILLIS).unwrap().ticks(), 1_001_000);
    }

    #[test]
    fn liefert_erst_wenn_faellig() {
        let src = Arc::new(ManualMonotonic::new());
        let clock = AudioMasterClock::new(
            48_000,
            Arc::clone(&src) as Arc<dyn MonotonicSource>,
            20_000_000,
        );
        let w = clock.writer().unwrap();
        src.set(0);
        w.report(0, 0);

        let mut b = backend(Pattern::Flat);
        b.open("test").unwrap();

        // Frame 0 liegt bei 0 ms und ist sofort faellig.
        assert!(b.next_frame(&clock).unwrap().is_some());

        // Frame 1 liegt bei 40 ms; die Uhr steht noch bei 0.
        assert!(b.next_frame(&clock).unwrap().is_none());

        // Uhr auf 40 ms: jetzt ist es faellig.
        w.report(48_000 * 40 / 1_000, 0);
        assert!(b.next_frame(&clock).unwrap().is_some());
    }

    #[test]
    fn verwirft_bei_verzug_wenn_erlaubt() {
        let src = Arc::new(ManualMonotonic::new());
        let clock = AudioMasterClock::new(
            48_000,
            Arc::clone(&src) as Arc<dyn MonotonicSource>,
            20_000_000,
        );
        let w = clock.writer().unwrap();
        let mut b = backend(Pattern::Flat);
        b.open("test").unwrap();

        // Die Uhr springt auf 400 ms: Frames 0..9 sind ueberholt.
        src.set(0);
        w.report(48_000 * 400 / 1_000, 0);

        let f = b.next_frame(&clock).unwrap().unwrap();
        let pts_ms = f
            .get()
            .meta()
            .pts
            .rescale(Rational::MILLIS)
            .unwrap()
            .ticks();
        assert_eq!(
            pts_ms, 400,
            "es muss das aktuelle Frame kommen, nicht Frame 0"
        );
        assert_eq!(b.stats().dropped, 10);
        assert_eq!(b.stats().decoded, 1);
    }

    #[test]
    fn editor_verwirft_nichts() {
        let clock = TimelineClock::new(Timestamp::from_millis(1_000));
        clock.seek(Timestamp::from_millis(400));

        let mut b = backend(Pattern::Flat);
        b.open("test").unwrap();

        // Trotz weit vorgerueckter Position kommt Frame 0 zuerst.
        let f = b.next_frame(&clock).unwrap().unwrap();
        assert_eq!(f.get().meta().pts.ticks(), 0);
        assert_eq!(b.stats().dropped, 0, "der Editor darf nicht verwerfen");
    }

    #[test]
    fn seek_setzt_den_frameindex() {
        let mut b = backend(Pattern::Flat);
        b.open("test").unwrap();

        b.seek(Timestamp::from_millis(400), SeekMode::Exact)
            .unwrap();
        assert_eq!(b.next_index(), 10);

        // Ueber das Ende hinaus wird begrenzt.
        b.seek(Timestamp::from_millis(99_999), SeekMode::Exact)
            .unwrap();
        assert_eq!(b.next_index(), 25);
        assert!(b.is_eof());
    }

    #[test]
    fn erschoepfter_pool_meldet_sich_statt_zu_allozieren() {
        let src = Arc::new(ManualMonotonic::new());
        let clock = AudioMasterClock::new(
            48_000,
            Arc::clone(&src) as Arc<dyn MonotonicSource>,
            20_000_000,
        );
        let w = clock.writer().unwrap();
        src.set(0);

        let mut b = NullBackend::new(
            meta(),
            Rational::lit(25, 1),
            Timestamp::from_millis(10_000),
            Pattern::Flat,
            2,
        );
        b.open("test").unwrap();

        // Frames festhalten, statt sie fallen zu lassen: der Renderer haelt
        // sie noch, waehrend der Decoder das naechste anfordert. Genau so
        // verhaelt sich ein Hardware-Decoder mit begrenztem Surface-Pool.
        w.report(0, 0);
        let _a = b.next_frame(&clock).unwrap().unwrap();

        // Uhr eine Frame-Dauer weiter, damit das naechste Frame faellig ist.
        w.report(48_000 * 40 / 1_000, 0);
        let _c = b.next_frame(&clock).unwrap().unwrap();

        w.report(48_000 * 80 / 1_000, 0);
        let err = b.next_frame(&clock).unwrap_err();
        assert_eq!(err, Error::PoolExhausted { capacity: 2 });
    }

    #[test]
    fn muster_werden_tatsaechlich_gezeichnet() {
        let src = Arc::new(ManualMonotonic::new());
        let clock = AudioMasterClock::new(
            48_000,
            Arc::clone(&src) as Arc<dyn MonotonicSource>,
            20_000_000,
        );
        clock.writer().unwrap().report(0, 0);

        for pattern in [Pattern::Flat, Pattern::Bars, Pattern::Motion] {
            let mut b = backend(pattern);
            b.open("test").unwrap();
            let f = b.next_frame(&clock).unwrap().unwrap();
            let luma = f.get().plane(0).unwrap();
            assert!(!luma.is_empty());
            // Chroma ist immer neutral.
            assert!(f.get().plane(1).unwrap().iter().all(|&c| c == 128));
            match pattern {
                Pattern::Flat => assert!(luma.iter().all(|&v| v == 128)),
                // Balken und Bewegung erzeugen mehrere Helligkeitswerte.
                Pattern::Bars | Pattern::Motion => {
                    let first = luma[0];
                    assert!(luma.iter().any(|&v| v != first));
                }
            }
        }
    }

    #[test]
    fn the_tolerance_is_half_the_display_period_at_most_half_a_frame() {
        let mut b = backend(Pattern::Flat);
        assert_eq!(
            b.due_tolerance_nanos(),
            20_000_000,
            "unknown: half of 40 ms"
        );
        b.set_display_period(Some(8_333_333));
        assert_eq!(b.due_tolerance_nanos(), 4_166_666, "120 Hz");
        b.set_display_period(Some(16_666_667));
        assert_eq!(b.due_tolerance_nanos(), 8_333_333, "60 Hz");
        b.set_display_period(Some(100_000_000));
        assert_eq!(b.due_tolerance_nanos(), 20_000_000, "a slow display");
        b.set_display_period(Some(0));
        assert_eq!(b.due_tolerance_nanos(), 20_000_000);
    }

    #[test]
    fn at_120_hz_a_frame_is_not_handed_out_20_ms_early() {
        let src = Arc::new(ManualMonotonic::new());
        let clock = AudioMasterClock::new(
            48_000,
            Arc::clone(&src) as Arc<dyn MonotonicSource>,
            20_000_000,
        );
        let w = clock.writer().unwrap();
        let mut b = backend(Pattern::Flat);
        b.open("test").unwrap();
        b.set_display_period(Some(8_333_333));
        w.report(0, 0);
        assert!(b.next_frame(&clock).unwrap().is_some(), "frame 0 at 0 ms");
        // Frame 1 at 40 ms: at 25 ms it was due with half a frame (20 ms),
        // at 120 Hz it waits for the refresh nearest 40 ms.
        w.report(48 * 25, 0);
        assert!(b.next_frame(&clock).unwrap().is_none());
        w.report(48 * 35, 0);
        assert!(b.next_frame(&clock).unwrap().is_none());
        w.report(48 * 36, 0);
        assert!(b.next_frame(&clock).unwrap().is_some());
    }
}
