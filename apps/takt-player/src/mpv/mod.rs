// SPDX-License-Identifier: GPL-3.0-or-later
//! libmpv in the player: [`MpvBackend`] behind the `VideoBackend` trait
//! ([`backend`]), the C interface under it (`ffi`), and the measuring run
//! without a window ([`measure`]) that the A/B protocol of
//! `docs/reviews/phase-0.md` is made with.
//!
//! The measuring run drives the backend exactly as the window does: one call
//! of `next_frame` per pass of a simulated display, the error of a frame read
//! from the clock of the backend. Sound is silent (`ao=null`) unless
//! `TAKT_AUDIO_DEVICE` names a device.
//!
//! Only built when `build.rs` found libmpv (`TAKT_LIBMPV_DIR`,
//! `docs/BUILD.md`).

// The properties of mpv are seconds as floating point; they are turned into
// whole microseconds where they are read and only shown after that, never
// fed back into a clock.
#![allow(clippy::cast_precision_loss)]

pub mod backend;
mod ffi;

use std::sync::Arc;
use std::time::{Duration, Instant};

use takt_backend::VideoBackend;
use takt_clock::{Clock, SystemMonotonic};
use takt_core::Rational;

pub use backend::{Config, Early, MpvBackend, Sound, Sources, Track};

/// How long mpv gets to settle before errors count: it starts a file with
/// the picture some 30 ms ahead of the sound and closes the gap over the
/// first frames (measured: about 20 frames at 23.976 fps).
const SETTLE: Duration = Duration::from_secs(1);
/// What a run measured.
#[derive(Debug, Default)]
pub struct Report {
    /// Size of the picture mpv rendered.
    pub size: (u32, u32),
    /// Frame rate of the container as mpv names it, frames per 1000 s.
    pub milli_fps: Option<i64>,
    /// Passes of the simulated display.
    pub passes: u64,
    /// Where the sound went.
    pub sound: String,
    /// The lead mpv handed frames out with, in microseconds.
    pub early_micros: u32,
    /// Passes with a new frame, rendered.
    pub shown: u64,
    /// Passes without a new frame.
    pub repeated: u64,
    /// The clock of the backend (the sound position of mpv) minus the
    /// timestamp of each frame handed out after the first second, in
    /// microseconds: positive when the frame is behind the sound.
    pub errors_micros: Vec<i64>,
    /// What rendering one frame into memory took, `None` before the run.
    pub render: Option<takt_audio::DurationSnapshot>,
    /// Frames mpv dropped, at the output and in the decoder.
    pub dropped: u64,
    /// Wall time of the run.
    pub elapsed: Duration,
}

impl Report {
    /// Mean of the error magnitudes in microseconds.
    pub fn mean_abs_micros(&self) -> Option<u64> {
        let count = u64::try_from(self.errors_micros.len()).ok()?;
        let sum: u64 = self.errors_micros.iter().map(|e| e.unsigned_abs()).sum();
        sum.checked_div(count)
    }

    /// The quantile `num/den` of the error magnitudes in microseconds, exact
    /// (the smallest value that covers that share).
    pub fn quantile_abs_micros(&self, num: usize, den: usize) -> Option<u64> {
        let mut sorted: Vec<u64> = self
            .errors_micros
            .iter()
            .map(|e| e.unsigned_abs())
            .collect();
        if sorted.is_empty() || den == 0 {
            return None;
        }
        sorted.sort_unstable();
        let rank = (sorted.len() * num.min(den)).div_ceil(den).max(1);
        sorted.get(rank - 1).copied()
    }

    /// An upper bound of the quantile `num/den` of the render time, in
    /// microseconds.
    pub fn render_quantile_micros(&self, num: u64, den: u64) -> Option<u64> {
        self.render.as_ref()?.quantile_micros(num, den)
    }

    /// The longest render of a frame, in nanoseconds.
    pub fn render_max_nanos(&self) -> u64 {
        self.render
            .as_ref()
            .map_or(0, takt_audio::DurationSnapshot::max_nanos)
    }

    /// The largest error behind the sound and before it, in microseconds.
    pub fn worst_micros(&self) -> (i64, i64) {
        let late = self.errors_micros.iter().copied().max().unwrap_or(0).max(0);
        let early = self.errors_micros.iter().copied().min().unwrap_or(0).min(0);
        (late, early.saturating_abs())
    }
}

/// Seconds of an mpv property as whole microseconds; a value that is no
/// number or out of range is dropped.
#[allow(clippy::cast_possible_truncation)]
fn micros(seconds: f64) -> Option<i64> {
    let micros = seconds * 1e6;
    // Far inside i64: 1e15 us are 31 years.
    (micros.is_finite() && micros.abs() < 1e15).then(|| micros.round() as i64)
}

/// Plays `source` for at most `seconds` and measures it: one pass per period
/// of a display with `hz` refreshes, on a fixed grid.
///
/// `early_micros` is how long before its time mpv hands a frame out
/// (`video-timing-offset`); `None` leaves it to the backend, which takes
/// half the display period. mpv never holds a frame back for the player,
/// which takes it when its pass comes. With 0 a frame is due at its
/// timestamp and waits for the next pass, half a period on average; with
/// half a period it goes to the pass nearest its time, the rule of
/// `NullBackend` (`VideoBackend::set_display_period`).
///
/// # Errors
///
/// A text for the user when mpv does not start, the source does not open, or
/// a frame cannot be rendered.
pub fn measure(
    source: &str,
    hz: Rational,
    seconds: i64,
    early_micros: Option<u32>,
) -> Result<Report, String> {
    let config = Config {
        // The caller wrote the source: test patterns of lavfi are welcome.
        sources: Sources::Any,
        // A measuring run is silent unless a device is named.
        sound: Sound::from_env(Sound::Silent),
        early: early_micros.map_or(Early::HalfDisplayPeriod, Early::Fixed),
    };
    let mut backend = MpvBackend::new(config, Arc::new(SystemMonotonic::new()))
        .map_err(|e| format!("libmpv: {e}"))?;
    let info = backend.open(source).map_err(|e| format!("{source}: {e}"))?;
    let video = info
        .video
        .ok_or_else(|| format!("{source}: no picture to measure"))?;

    let period_nanos = 1_000_000_000_i128 * i128::from(hz.den()) / i128::from(hz.num().max(1));
    let period_nanos = u64::try_from(period_nanos).unwrap_or(u64::MAX);
    backend.set_display_period(i64::try_from(period_nanos).ok());
    let mut report = Report {
        size: (video.width, video.height),
        milli_fps: info.frame_rate.and_then(|rate| {
            let milli = i128::from(rate.num()) * 1_000 / i128::from(rate.den().max(1));
            i64::try_from(milli).ok()
        }),
        sound: backend.sound_line().to_owned(),
        early_micros: backend.early_micros(),
        ..Report::default()
    };

    let clock = backend.clock();
    let length = Duration::from_secs(u64::try_from(seconds).unwrap_or(0));
    let started = Instant::now();
    while started.elapsed() < length {
        let frame = backend
            .next_frame(clock.as_ref())
            .map_err(|e| format!("libmpv: {e}"))?;
        match frame {
            Some(frame) => {
                report.shown += 1;
                // Positive: the frame comes after its time on the clock.
                let error = -clock.until_nanos(frame.get().meta().pts) / 1_000;
                if started.elapsed() >= SETTLE {
                    report.errors_micros.push(error);
                }
            }
            None if backend.is_eof() => break,
            None => report.repeated += 1,
        }
        report.passes += 1;
        std::thread::sleep(crate::headless::until_next_refresh(
            started.elapsed(),
            period_nanos,
        ));
    }
    report.elapsed = started.elapsed();
    report.render = Some(backend.render_times());
    report.dropped = backend.refresh_stats().dropped;
    Ok(report)
}

/// Prints the report of [`measure`] and its `takt-ab` line.
pub fn print(source: &str, hz: Rational, seconds: i64, report: &Report) {
    let early_micros = report.early_micros;
    let number = |n: Option<u64>| n.map_or("-".to_owned(), |n| n.to_string());
    let fps = report.milli_fps.map_or("-".to_owned(), |milli| {
        format!("{}.{:03}", milli / 1_000, milli % 1_000)
    });
    let (late, early) = report.worst_micros();
    let quantile = |num, den| number(report.render_quantile_micros(num, den));
    println!("Takt Player — mpv ueber den Software-Renderer (kopflos)");
    println!("  Quelle       {source}");
    println!("  Geometrie    {}x{} rgb0", report.size.0, report.size.1);
    println!("  Rate         {fps} fps");
    println!("  Anzeige      {hz} Hz simuliert");
    println!("  Audio        {}", report.sound);
    println!("  Vorlauf      {early_micros} us (video-timing-offset)");
    println!();
    println!("Messwerte");
    println!("  dargestellt     {}", report.shown);
    println!("  wiederholt      {}", report.repeated);
    println!("  verworfen       {}", report.dropped);
    println!(
        "  Fehler Mittel   {} us (Uhr von mpv - Zeitstempel des Frames)",
        number(report.mean_abs_micros())
    );
    println!(
        "  Fehler p99      {} us",
        number(report.quantile_abs_micros(99, 100))
    );
    println!("  Fehler max spaet {late} us, frueh {early} us");
    println!(
        "  Render-Zeit     p50 <{} us  p99 <{} us  max {:.1} us",
        quantile(1, 2),
        quantile(99, 100),
        report.render_max_nanos() as f64 / 1e3
    );
    println!();
    println!(
        "takt-ab backend=mpv-sw fps={fps} hz={hz} seconds={seconds} elapsed_ms={} passes={} shown={} \
         repeated={} dropped={} mean_us={} p99_us={} late_us={late} early_us={early} audio={} \
         xruns=- render_p50_us={} render_p99_us={} render_max_us={} offset_us={early_micros}",
        report.elapsed.as_millis(),
        report.passes,
        report.shown,
        report.repeated,
        report.dropped,
        number(report.mean_abs_micros()),
        number(report.quantile_abs_micros(99, 100)),
        if report.sound.contains("silent") {
            "mpv-null"
        } else {
            "mpv-device"
        },
        quantile(1, 2),
        quantile(99, 100),
        report.render_max_nanos() / 1_000,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_of_mpv_become_whole_microseconds() {
        assert_eq!(micros(0.041_708), Some(41_708));
        assert_eq!(micros(-0.000_000_6), Some(-1));
        assert_eq!(micros(f64::NAN), None);
        assert_eq!(micros(f64::INFINITY), None);
        assert_eq!(micros(1e12), None);
    }

    #[test]
    fn the_quantile_of_the_errors_is_exact() {
        let report = Report {
            errors_micros: vec![-300, 100, 200, -5_000],
            ..Report::default()
        };
        assert_eq!(report.mean_abs_micros(), Some(1_400));
        assert_eq!(report.quantile_abs_micros(1, 2), Some(200));
        assert_eq!(report.quantile_abs_micros(99, 100), Some(5_000));
        assert_eq!(report.worst_micros(), (200, 5_000));
        assert_eq!(Report::default().quantile_abs_micros(99, 100), None);
        assert_eq!(Report::default().mean_abs_micros(), None);
    }

    /// The pattern of lavfi through the whole path: mpv opens it, decodes,
    /// and the software renderer writes pictures into the surface.
    #[test]
    fn a_test_pattern_is_rendered_into_memory() {
        let source = "av://lavfi:testsrc2=size=320x240:rate=25";
        let report = measure(source, Rational::lit(60, 1), 2, None).unwrap();
        assert_eq!(report.size, (320, 240));
        assert_eq!(report.early_micros, 8_333, "half a period of 60 Hz");
        assert!(report.sound.starts_with("mpv: silent"), "{}", report.sound);
        assert_eq!(report.milli_fps, Some(25_000));
        // 25 frames a second for two seconds; the start may cost a few.
        // A loaded machine shows fewer; none at all would be the fault.
        assert!((10..=51).contains(&report.shown), "{report:?}");
        assert_eq!(report.passes, report.shown + report.repeated);
        assert_eq!(
            report
                .render
                .as_ref()
                .map(takt_audio::DurationSnapshot::total),
            Some(report.shown)
        );
        // The frames go to the pass nearest their time: within a period.
        assert!(
            report.mean_abs_micros().is_some_and(|mean| mean < 16_667),
            "{report:?}"
        );
    }

    #[test]
    fn a_source_that_does_not_exist_is_an_error() {
        let error = measure("takt-no-such-file.mkv", Rational::lit(60, 1), 1, Some(0)).unwrap_err();
        assert!(error.contains("cannot open"), "{error}");
    }
}
