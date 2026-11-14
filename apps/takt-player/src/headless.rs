// SPDX-License-Identifier: GPL-3.0-or-later
//! The headless run of Phase 0: the playback loop over [`NullBackend`] for a
//! few seconds, then the measurements of the scheduler and the audio path.
//! It stays as `--headless`: the numbers of `docs/PHASE-0.md` are measured
//! with it, on all three platforms and without a window.
//!
//! `--fps`, `--hz` and `--seconds` set the rate of the source, the refresh
//! rate of the simulated display and the length of the run; the A/B protocol
//! of `docs/reviews/phase-0.md` (`scripts/ab-measure.py`) reads the last
//! line, `takt-ab key=value ...`. `--mpv SOURCE` measures libmpv on a file
//! instead (`crate::mpv`), in a build that links it.

// Dieses Modul formatiert ausschliesslich Messwerte fuer die Ausgabe. Zeit
// wird hier nie gerechnet, nur angezeigt — deshalb ist Gleitkomma erlaubt.
#![allow(clippy::cast_precision_loss)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use takt_backend::{NullBackend, Pattern, Presentation, Scheduler, VideoBackend};
use takt_clock::{Clock, SystemMonotonic};
use takt_core::{
    ColorPrimaries, ColorRange, FrameMeta, PixelFormat, Rational, Timestamp, TransferFunction,
};

use crate::audio::{self, DeviceSummary};

/// The longest `--mpv-early`: a tenth of a second (`crate::mpv` has the same bound).
const MAX_EARLY_MICROS: u32 = 100_000;
/// The longest run `--seconds` takes: a day.
const MAX_SECONDS: i64 = 86_400;

/// What a headless run measures with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// Frame rate of the test source. Default 24000/1001: the NTSC rate
    /// shows rounding errors first.
    pub fps: Rational,
    /// Refresh rate of the simulated display. Default 60.
    pub hz: Rational,
    /// Length of the source in seconds. Default 3.
    pub seconds: i64,
    /// `--mpv SOURCE`: measure libmpv on this file or URL of mpv instead of
    /// the test source; `--fps` is then the rate of the file.
    pub mpv: Option<String>,
    /// `--mpv-early MICROS`: how long before its time mpv hands a frame out
    /// (`video-timing-offset`). Default 0; at most a tenth of a second.
    pub mpv_early_micros: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            fps: Rational::lit(24_000, 1_001),
            hz: Rational::lit(60, 1),
            seconds: 3,
            mpv: None,
            mpv_early_micros: 0,
        }
    }
}

impl Options {
    /// Reads `--fps N[/D]`, `--hz N[/D]`, `--seconds N` and `--mpv SOURCE`
    /// from the arguments of the program; everything else is left to `main`.
    ///
    /// # Errors
    ///
    /// A text for the user when a value is missing, not a number, or out of
    /// range (rates from 1 to 1000 per second, 1 to 86400 seconds).
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| format!("{arg} needs a value"))
                    .map(String::as_str)
            };
            match arg.as_str() {
                "--fps" => options.fps = rate(arg, value()?)?,
                "--hz" => options.hz = rate(arg, value()?)?,
                "--mpv" => options.mpv = Some(value()?.to_owned()),
                "--mpv-early" => {
                    let text = value()?;
                    options.mpv_early_micros = text
                        .parse()
                        .ok()
                        .filter(|&us: &u32| us <= MAX_EARLY_MICROS)
                        .ok_or_else(|| {
                            format!("{arg} {text}: 0 to {MAX_EARLY_MICROS} microseconds")
                        })?;
                }
                "--seconds" => {
                    let text = value()?;
                    options.seconds = text
                        .parse()
                        .ok()
                        .filter(|s| (1..=MAX_SECONDS).contains(s))
                        .ok_or_else(|| format!("{arg} {text}: 1 to {MAX_SECONDS} seconds"))?;
                }
                _ => {}
            }
        }
        Ok(options)
    }

    /// The period of the simulated display in nanoseconds, rounded down.
    fn display_period_nanos(&self) -> u64 {
        // Both parts are positive and the rate is at most 1000 (see `rate`).
        let nanos = 1_000_000_000_i128 * i128::from(self.hz.den()) / i128::from(self.hz.num());
        u64::try_from(nanos).unwrap_or(u64::MAX)
    }
}

/// How long to sleep until the next refresh of a display that started at
/// zero and refreshes every `period_nanos`, `elapsed` after its start.
///
/// The refreshes lie on a fixed grid. A pass that took longer than a period
/// misses the refreshes in between, as on a real display: they are not made
/// up for in a burst of passes afterwards, which would count as repeats that
/// no screen ever showed.
pub fn until_next_refresh(elapsed: Duration, period_nanos: u64) -> Duration {
    let period = u128::from(period_nanos.max(1));
    let elapsed = elapsed.as_nanos();
    let next = (elapsed / period + 1) * period;
    Duration::from_nanos(u64::try_from(next - elapsed).unwrap_or(u64::MAX))
}

/// A rate as `N` or `N/D`, from 1 to 1000 per second.
fn rate(option: &str, text: &str) -> Result<Rational, String> {
    let (num, den) = text.split_once('/').unwrap_or((text, "1"));
    let parsed = num
        .parse::<i64>()
        .ok()
        .zip(den.parse::<i64>().ok())
        .filter(|&(num, den)| den > 0 && num >= den && num / den <= 1_000)
        .and_then(|(num, den)| Rational::new(num, den));
    parsed.ok_or_else(|| format!("{option} {text}: a rate from 1 to 1000, as N or N/D"))
}

/// The measuring run without a window (`takt-player --headless`).
///
/// # Errors
///
/// A text for the user when `--mpv` cannot be measured: a build without
/// libmpv, or a source mpv does not play.
pub fn run(options: &Options) -> Result<(), String> {
    match &options.mpv {
        Some(source) => run_mpv(source, options),
        None => {
            run_null(options);
            Ok(())
        }
    }
}

#[cfg(takt_libmpv)]
fn run_mpv(source: &str, options: &Options) -> Result<(), String> {
    let early = options.mpv_early_micros;
    let report = crate::mpv::measure(source, options.hz, options.seconds, early)?;
    crate::mpv::print(source, options.hz, options.seconds, early, &report);
    Ok(())
}

#[cfg(not(takt_libmpv))]
fn run_mpv(_source: &str, _options: &Options) -> Result<(), String> {
    Err("--mpv: this build has no libmpv (TAKT_LIBMPV_DIR, docs/BUILD.md)".to_owned())
}

/// The scheduler of takt on [`NullBackend`].
fn run_null(options: &Options) {
    let fps = options.fps;
    let duration = Timestamp::from_millis(options.seconds * 1_000);

    let meta = FrameMeta {
        width: 1920,
        height: 1080,
        format: PixelFormat::Nv12,
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Bt709,
        range: ColorRange::Limited,
        pts: Timestamp::from_millis(0),
    };

    let source = Arc::new(SystemMonotonic::new());
    let (clock, audio) = audio::start(&source);

    let mut backend = NullBackend::new(meta, fps, duration, Pattern::Motion, 6);
    let info = match backend.open("null://motion") {
        Ok(info) => info,
        Err(e) => {
            eprintln!("Quelle konnte nicht geoeffnet werden: {e}");
            std::process::exit(1);
        }
    };

    let period_ns = options.display_period_nanos();
    println!("Takt Player — Phase 0 (kopflos)");
    println!("  Backend      {}", backend.name());
    println!(
        "  Geometrie    {}x{} {:?}",
        meta.width, meta.height, meta.format
    );
    println!("  Rate         {fps} fps");
    println!(
        "  Framedauer   {:.3} ms",
        info.frame_duration_nanos().unwrap_or(0) as f64 / 1e6
    );
    println!(
        "  Anzeige      {} Hz ({:.3} ms je Durchlauf)",
        options.hz,
        period_ns as f64 / 1e6
    );
    println!("  Frames       {}", backend.total_frames());
    println!("  Audio        {}", audio.describe());
    println!();

    let mut scheduler = Scheduler::new();
    let frame_ns = info.frame_duration_nanos().unwrap_or(16_666_667);

    // One pass per refresh of the simulated display, as a renderer would be
    // called. The passes follow a fixed grid from the start: sleeping one
    // period after each pass would add the work of the pass to every period
    // and let the display run slow.
    backend.set_display_period(i64::try_from(period_ns).ok());
    let started = Instant::now();
    let mut pass = 0_u64;
    loop {
        let frame = match backend.next_frame(&clock) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Dekodierfehler: {e}");
                break;
            }
        };
        match scheduler.accept(&clock, frame.as_ref(), backend.is_eof()) {
            Presentation::Eof => break,
            Presentation::Shown { .. } | Presentation::Repeated => {}
        }
        drop(frame); // Frame zurueck in den Pool
        pass += 1;
        std::thread::sleep(until_next_refresh(started.elapsed(), period_ns));
    }
    let elapsed = started.elapsed();

    let simulated = audio.device_stats().is_none();
    let device = audio.stop();

    let s = scheduler.stats();
    let b = backend.stats();
    let p99 = s.quantile_upper_micros(99, 100);
    println!("Messwerte");
    println!("  dargestellt     {}", s.shown);
    println!(
        "  wiederholt      {} ({:.1} %)",
        s.repeated,
        s.repeat_ratio()
    );
    println!("  verworfen       {}", b.dropped);
    println!(
        "  Fehler Mittel   {:.3} ms",
        s.mean_abs_error_nanos() as f64 / 1e6
    );
    println!(
        "  Fehler p99      {}",
        p99.map_or("-".to_owned(), |us| format!("<{:.3} ms", us as f64 / 1e3))
    );
    println!(
        "  Fehler max spaet {:.3} ms",
        s.worst_late_nanos as f64 / 1e6
    );
    println!(
        "  Fehler max frueh {:.3} ms",
        s.worst_early_nanos as f64 / 1e6
    );
    println!("  Endposition     {}", clock.now());
    println!("  Framedauer      {:.3} ms", frame_ns as f64 / 1e6);
    let mut xruns = None;
    if let Some(DeviceSummary { stats: a, priority }) = device {
        xruns = Some(a.xruns());
        println!("  Prioritaet      {priority}");
        println!(
            "  Audio           {} Callbacks, {} Xruns, {} Frames Stille",
            a.callbacks(),
            a.xruns(),
            a.silent_frames()
        );
        match a.latency_frames() {
            Some(frames) => println!(
                "  Geraetelatenz   {frames} Frames ({:.1} ms), von der Uhr abgezogen",
                frames as f64 * 1e3 / f64::from(crate::audio::SAMPLE_RATE)
            ),
            None => println!("  Geraetelatenz   unbekannt, nicht abgezogen"),
        }
        if let Some(at) = a.last_xrun_nanos() {
            println!("  letzter Xrun    bei {:.3} s", at as f64 / 1e9);
        }
        // Obergrenzen der Klassen: nie zu niedrig, hoechstens 25 % zu hoch.
        let r = a.runtime();
        let q = |num, den| {
            r.quantile_micros(num, den)
                .map_or("-".to_owned(), |us| format!("<{us} us"))
        };
        println!(
            "  Callback-Zeit   p50 {}  p99 {}  p99.99 {}  max {:.1} us",
            q(1, 2),
            q(99, 100),
            q(9_999, 10_000),
            r.max_nanos() as f64 / 1e3
        );
    }
    println!();
    println!("Histogramm der Fehlerbetraege");
    for (i, count) in s.histogram.iter().enumerate() {
        if *count == 0 {
            continue;
        }
        let lo = if i == 0 { 0 } else { 1_u32 << i };
        let hi = 1_u32 << (i + 1);
        println!(
            "  {lo:>6}..{hi:<6} us  {:>5}  {}",
            count,
            "#".repeat((*count as usize).min(40))
        );
    }
    println!();
    // The line scripts/ab-measure.py reads; a missing number is `-`.
    let number = |n: Option<u64>| n.map_or("-".to_owned(), |n| n.to_string());
    println!(
        "takt-ab backend={} fps={fps} hz={} seconds={} elapsed_ms={} passes={pass} shown={} \
         repeated={} dropped={} mean_us={} p99_us={} late_us={} early_us={} audio={} xruns={}",
        backend.name(),
        options.hz,
        options.seconds,
        elapsed.as_millis(),
        s.shown,
        s.repeated,
        b.dropped,
        s.mean_abs_error_nanos() / 1_000,
        number(p99),
        s.worst_late_nanos / 1_000,
        s.worst_early_nanos.saturating_abs() / 1_000,
        if simulated { "simulated" } else { "device" },
        number(xruns),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|&a| a.to_owned()).collect()
    }

    #[test]
    fn without_options_the_run_of_phase_0() {
        let options = Options::parse(&args(&["takt-player", "--headless"])).unwrap();
        assert_eq!(options, Options::default());
        assert_eq!(options.fps, Rational::lit(24_000, 1_001));
        assert_eq!(options.display_period_nanos(), 16_666_666);
    }

    #[test]
    fn rates_as_a_number_or_a_fraction() {
        let options = Options::parse(&args(&[
            "--headless",
            "--fps",
            "50",
            "--hz",
            "120000/1001",
            "--seconds",
            "300",
        ]))
        .unwrap();
        assert_eq!(options.fps, Rational::lit(50, 1));
        assert_eq!(options.hz, Rational::lit(120_000, 1_001));
        assert_eq!(options.seconds, 300);
        assert_eq!(options.display_period_nanos(), 8_341_666);
        assert_eq!(options.mpv, None);
        let options = Options::parse(&args(&["--headless", "--mpv", "a b.mkv"])).unwrap();
        assert_eq!(options.mpv.as_deref(), Some("a b.mkv"));
    }

    #[test]
    fn a_late_pass_waits_for_the_next_refresh_and_makes_up_for_none() {
        let period = 16_666_666;
        let after = |nanos| until_next_refresh(Duration::from_nanos(nanos), period).as_nanos();
        assert_eq!(after(0), 16_666_666);
        assert_eq!(after(1_000_000), 15_666_666);
        // Exactly on a refresh: the next one, not this one again.
        assert_eq!(after(16_666_666), 16_666_666);
        // A pass of 50 ms missed two refreshes; the wait is to the fourth, never zero.
        assert_eq!(after(50_000_000), 4 * 16_666_666 - 50_000_000);
        assert_eq!(until_next_refresh(Duration::from_secs(1), 0).as_nanos(), 1);
    }

    #[test]
    fn a_value_out_of_range_is_named() {
        for bad in [
            &["--fps"][..],
            &["--fps", "0"],
            &["--fps", "-24"],
            &["--fps", "24/0"],
            &["--fps", "1/2"],
            &["--fps", "1001"],
            &["--hz", "sixty"],
            &["--hz", "60/"],
            &["--mpv"],
            &["--mpv-early", "100001"],
            &["--mpv-early", "-1"],
            &["--seconds", "0"],
            &["--seconds", "86401"],
            &["--seconds", "1.5"],
        ] {
            let err = Options::parse(&args(bad)).unwrap_err();
            assert!(err.starts_with(bad[0]), "{bad:?}: {err}");
        }
    }
}
