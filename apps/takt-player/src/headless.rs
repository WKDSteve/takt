// SPDX-License-Identifier: GPL-3.0-or-later
//! The headless run of Phase 0: the playback loop over [`NullBackend`] for a
//! few seconds, then the measurements of the scheduler and the audio path.
//! It stays as `--headless`: the numbers of `docs/PHASE-0.md` are measured
//! with it, on all three platforms and without a window.

// Dieses Modul formatiert ausschliesslich Messwerte fuer die Ausgabe. Zeit
// wird hier nie gerechnet, nur angezeigt — deshalb ist Gleitkomma erlaubt.
#![allow(clippy::cast_precision_loss)]

use std::sync::Arc;
use std::time::Duration;

use takt_backend::{NullBackend, Pattern, Presentation, Scheduler, VideoBackend};
use takt_clock::{Clock, SystemMonotonic};
use takt_core::{
    ColorPrimaries, ColorRange, FrameMeta, PixelFormat, Rational, Timestamp, TransferFunction,
};

use crate::audio::{self, DeviceSummary};

/// Wie lange die Demo-Wiedergabe laeuft.
const DEMO_SECONDS: i64 = 3;
/// Bildwiederholrate der Testquelle: die NTSC-Rate, weil sie am ehesten
/// Rundungsfehler aufdeckt.
const FPS: (i64, i64) = (24_000, 1_001);

/// The measuring run without a window (`takt-player --headless`).
pub fn run() {
    let fps = Rational::lit(FPS.0, FPS.1);
    let duration = Timestamp::from_millis(DEMO_SECONDS * 1_000);

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
    println!("  Frames       {}", backend.total_frames());
    println!("  Audio        {}", audio.describe());
    println!();

    let mut scheduler = Scheduler::new();
    let frame_ns = info.frame_duration_nanos().unwrap_or(16_666_667);

    // Anzeigedurchlauf bei 60 Hz, so wie ein echter Renderer aufgerufen wuerde.
    let vsync = Duration::from_nanos(16_666_667);
    backend.set_display_period(Some(16_666_667));
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
        std::thread::sleep(vsync);
    }

    let device = audio.stop();

    let s = scheduler.stats();
    let b = backend.stats();
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
        "  Fehler max spaet {:.3} ms",
        s.worst_late_nanos as f64 / 1e6
    );
    println!(
        "  Fehler max frueh {:.3} ms",
        s.worst_early_nanos as f64 / 1e6
    );
    println!("  Endposition     {}", clock.now());
    println!("  Framedauer      {:.3} ms", frame_ns as f64 / 1e6);
    if let Some(DeviceSummary { stats: a, priority }) = device {
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
}
