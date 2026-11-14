// SPDX-License-Identifier: GPL-3.0-or-later
//! mpv as the source of pictures, measured without a window: the spike for
//! `MpvBackend` (`docs/reviews/phase-0.md`, "Spike: mpv über den
//! Software-Renderer").
//!
//! libmpv decodes, keeps its own clock and decides when a frame is due; the
//! player asks once per pass of a simulated display whether a frame waits
//! and renders it into memory (`rgb0`). The run answers what that way costs
//! per frame and how far the frame is from the audio position of mpv when
//! the player gets it. Sound goes to `ao=null`: timed, silent.
//!
//! Only built when `build.rs` found libmpv (`TAKT_LIBMPV_DIR`,
//! `docs/BUILD.md`).

// The properties of mpv are seconds as floating point; they are turned into
// whole microseconds where they are read and only shown after that, never
// fed back into a clock.
#![allow(clippy::cast_precision_loss)]

mod ffi;

use std::ffi::{CStr, CString};
use std::time::{Duration, Instant};

use ffi::{Event, Mpv, Surface};
use takt_audio::DurationHistogram;
use takt_core::Rational;

/// How long mpv gets to settle before errors count: it starts a file with
/// the picture some 30 ms ahead of the sound and closes the gap over the
/// first frames (measured: about 20 frames at 23.976 fps).
const SETTLE: Duration = Duration::from_secs(1);
/// How long the file may take to open.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// The options of the core. No configuration, scripts or network helper of
/// the user; no window (`vo=libmpv` is the render API) and no sound device.
const OPTIONS: [(&CStr, &CStr); 8] = [
    (c"config", c"no"),
    (c"load-scripts", c"no"),
    (c"ytdl", c"no"),
    (c"vo", c"libmpv"),
    (c"ao", c"null"),
    (c"hwdec", c"no"),
    (c"audio-display", c"no"),
    (c"keep-open", c"no"),
];

/// The longest `--mpv-early`: a tenth of a second.
pub const MAX_EARLY_MICROS: u32 = 100_000;

/// What a run measured.
#[derive(Debug, Default)]
pub struct Report {
    /// Size of the picture mpv rendered.
    pub size: (u32, u32),
    /// Frame rate of the container as mpv names it, frames per 1000 s.
    pub milli_fps: Option<i64>,
    /// Passes of the simulated display.
    pub passes: u64,
    /// Passes with a new frame, rendered.
    pub shown: u64,
    /// Passes without a new frame.
    pub repeated: u64,
    /// `audio-pts - time-pos` at each frame rendered after the first second,
    /// in microseconds: positive when the frame is behind the sound.
    pub errors_micros: Vec<i64>,
    /// What rendering one frame into memory took, `None` before the run.
    pub render: Option<takt_audio::DurationSnapshot>,
    /// `frame-drop-count`: frames mpv dropped at the output.
    pub dropped: Option<i64>,
    /// `decoder-frame-drop-count`.
    pub decoder_dropped: Option<i64>,
    /// `vo-delayed-frame-count`.
    pub delayed: Option<i64>,
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

/// A side of the picture from a property of mpv, 1920x1080 without one.
fn side(value: Option<i64>, fallback: u32) -> u32 {
    value
        .and_then(|v| u32::try_from(v).ok())
        .filter(|&v| v > 0)
        .unwrap_or(fallback)
}

/// Plays `source` for at most `seconds` and measures it: one pass per period
/// of a display with `hz` refreshes, on a fixed grid.
///
/// mpv hands a frame out `early_micros` before its time
/// (`video-timing-offset`) and never holds it back for the player, which
/// renders when its pass comes. With 0 a frame is due at its timestamp and
/// waits for the next pass, half a period on average; with half a period it
/// goes to the pass nearest its time, the rule of `NullBackend`
/// (`VideoBackend::set_display_period`).
///
/// # Errors
///
/// A text for the user when mpv does not start, the source does not open, or
/// a frame cannot be rendered.
pub fn measure(
    source: &str,
    hz: Rational,
    seconds: i64,
    early_micros: u32,
) -> Result<Report, String> {
    let early = early_micros.min(MAX_EARLY_MICROS);
    // Seconds with six decimals, from whole numbers: no float is formatted.
    let early = CString::new(format!("{}.{:06}", early / 1_000_000, early % 1_000_000))
        .map_err(|e| e.to_string())?;
    let mut options = OPTIONS.to_vec();
    options.push((c"video-timing-offset", &early));
    let mut mpv = Mpv::new(&options).map_err(|e| format!("libmpv: {e}"))?;
    mpv.load(source).map_err(|e| format!("libmpv: {e}"))?;

    let opened = Instant::now();
    loop {
        match mpv.poll() {
            Event::FileLoaded => break,
            Event::EndFile { .. } | Event::Shutdown => {
                return Err(format!("mpv cannot open {source}"));
            }
            Event::None => {
                if opened.elapsed() > OPEN_TIMEOUT {
                    return Err(format!("mpv did not open {source} within {OPEN_TIMEOUT:?}"));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Event::Other => {}
        }
    }

    let size = (
        side(mpv.int(c"width"), 1920),
        side(mpv.int(c"height"), 1080),
    );
    let mut surface = Surface::new(size.0, size.1)
        .ok_or_else(|| format!("a picture of {}x{} is beyond 8K", size.0, size.1))?;
    let mut report = Report {
        size,
        // Frames per second as frames per 1000 s: the same rounding as for
        // seconds to microseconds, then a thousandth of it.
        milli_fps: mpv
            .double(c"container-fps")
            .and_then(micros)
            .map(|micro_fps| micro_fps / 1_000),
        ..Report::default()
    };

    let period_nanos = 1_000_000_000_i128 * i128::from(hz.den()) / i128::from(hz.num().max(1));
    let period_nanos = u64::try_from(period_nanos).unwrap_or(u64::MAX);
    let length = Duration::from_secs(u64::try_from(seconds).unwrap_or(0));
    let render = DurationHistogram::default();
    let started = Instant::now();
    'passes: while started.elapsed() < length {
        loop {
            match mpv.poll() {
                Event::None => break,
                Event::EndFile { .. } | Event::Shutdown => break 'passes,
                Event::FileLoaded | Event::Other => {}
            }
        }
        if mpv.frame_ready() {
            let before = Instant::now();
            mpv.render(&mut surface)
                .map_err(|e| format!("libmpv: {e}"))?;
            render.record(u64::try_from(before.elapsed().as_nanos()).unwrap_or(u64::MAX));
            report.shown += 1;
            let error = mpv
                .double(c"audio-pts")
                .zip(mpv.double(c"time-pos"))
                .and_then(|(audio, video)| micros(audio - video));
            if let Some(error) = error
                && started.elapsed() >= SETTLE
            {
                report.errors_micros.push(error);
            }
        } else {
            report.repeated += 1;
        }
        report.passes += 1;
        std::thread::sleep(crate::headless::until_next_refresh(
            started.elapsed(),
            period_nanos,
        ));
    }
    report.elapsed = started.elapsed();
    report.render = Some(render.snapshot());
    report.dropped = mpv.int(c"frame-drop-count");
    report.decoder_dropped = mpv.int(c"decoder-frame-drop-count");
    report.delayed = mpv.int(c"vo-delayed-frame-count");
    // The picture is not shown anywhere; one byte says it was written at all.
    if report.shown > 0 && surface.pixels().iter().all(|&b| b == 0) {
        return Err("mpv rendered only black: no picture reached the surface".to_owned());
    }
    Ok(report)
}

/// Prints the report of [`measure`] and its `takt-ab` line.
pub fn print(source: &str, hz: Rational, seconds: i64, early_micros: u32, report: &Report) {
    let number = |n: Option<u64>| n.map_or("-".to_owned(), |n| n.to_string());
    let count = |n: Option<i64>| n.map_or("-".to_owned(), |n| n.to_string());
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
    println!("  Audio        ao=null (mpv, stumm)");
    println!("  Vorlauf      {early_micros} us (video-timing-offset)");
    println!();
    println!("Messwerte");
    println!("  dargestellt     {}", report.shown);
    println!("  wiederholt      {}", report.repeated);
    println!(
        "  verworfen       {} (Decoder {}), verspaetet {}",
        count(report.dropped),
        count(report.decoder_dropped),
        count(report.delayed)
    );
    println!(
        "  Fehler Mittel   {} us (audio-pts - time-pos)",
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
         repeated={} dropped={} mean_us={} p99_us={} late_us={late} early_us={early} audio=mpv-null \
         xruns=- delayed={} render_p50_us={} render_p99_us={} render_max_us={} offset_us={early_micros}",
        report.elapsed.as_millis(),
        report.passes,
        report.shown,
        report.repeated,
        count(report.dropped),
        number(report.mean_abs_micros()),
        number(report.quantile_abs_micros(99, 100)),
        count(report.delayed),
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
        let report = measure(source, Rational::lit(60, 1), 2, 8_333).unwrap();
        assert_eq!(report.size, (320, 240));
        assert_eq!(report.milli_fps, Some(25_000));
        // 25 frames a second for two seconds; the start may cost a few.
        // A loaded machine shows fewer; none at all would be the fault.
        assert!((10..=51).contains(&report.shown), "{report:?}");
        assert_eq!(report.passes, report.shown + report.repeated);
        assert_eq!(report.render.map(|r| r.total()), Some(report.shown));
    }

    #[test]
    fn a_source_that_does_not_exist_is_an_error() {
        let error = measure("takt-no-such-file.mkv", Rational::lit(60, 1), 1, 0).unwrap_err();
        assert!(error.contains("cannot open"), "{error}");
    }
}
