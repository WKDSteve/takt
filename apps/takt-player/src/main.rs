// SPDX-License-Identifier: GPL-3.0-or-later
//! Takt Player — Phase 0.
//!
//! Derzeit ohne Fenster: der Player fuehrt eine kopflose Wiedergabeschleife
//! ueber [`NullBackend`] aus und gibt die Messwerte des Schedulers aus. Das
//! ist absichtlich der erste Schritt — die Zeit- und Scheduling-Mechanik ist
//! damit auf allen drei Plattformen pruefbar, bevor eine Fensterbibliothek
//! oder libmpv im Spiel ist.
//!
//! Der Ton laeuft ueber das Standard-Ausgabegeraet (`takt-audio`, cpal): der
//! Callback spielt Stille aus dem Ring und treibt damit die Audio-Master-Uhr.
//! Ohne Geraet, oder mit `TAKT_AUDIO=sim`, simuliert ein Thread den Callback,
//! und der Player sagt das beim Start.
//!
//! Naechster Schritt: egui-Fenster, dann `MpvBackend`.

// Dieses Modul formatiert ausschliesslich Messwerte fuer die Ausgabe. Zeit
// wird hier nie gerechnet, nur angezeigt — deshalb ist Gleitkomma erlaubt.
#![allow(clippy::cast_precision_loss)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use assert_no_alloc::AllocDisabler;
use takt_audio::{Output, OutputCallback, OutputConfig, OutputStats, ring};

use takt_backend::{NullBackend, Pattern, Presentation, Scheduler, VideoBackend};
use takt_clock::{AudioMasterClock, Clock, MonotonicSource, SystemMonotonic};
use takt_core::{
    ColorPrimaries, ColorRange, FrameMeta, PixelFormat, Rational, Timestamp, TransferFunction,
};

/// Wie lange die Demo-Wiedergabe laeuft.
const DEMO_SECONDS: i64 = 3;
/// Bildwiederholrate der Testquelle: die NTSC-Rate, weil sie am ehesten
/// Rundungsfehler aufdeckt.
const FPS: (i64, i64) = (24_000, 1_001);
/// 20 ms Extrapolationsgrenze, passend zu einer 10-ms-Callback-Periode.
const MAX_EXTRAPOLATION_NANOS: u64 = 20_000_000;
/// Vorrat des Rings: 100 ms Stereo bei 48 kHz.
const RING_FRAMES: usize = 4_800;

// Jede Allokation im Audio-Callback bricht den Prozess ab, im Debug und im
// Release (`assert_no_alloc` ohne `disable_release`).
#[global_allocator]
static ALLOCATOR: AllocDisabler = AllocDisabler;

/// Wer die Uhr stellt: das Geraet oder der simulierte Thread.
enum Audio {
    Device {
        // Haelt den Stream am Leben; nicht `Send`, bleibt im Hauptthread.
        output: Output,
        stats: Arc<OutputStats>,
        feeder: JoinHandle<()>,
        stop: Arc<AtomicBool>,
    },
    Simulated {
        thread: JoinHandle<()>,
        reason: String,
    },
}

fn new_clock(source: &Arc<SystemMonotonic>) -> AudioMasterClock {
    AudioMasterClock::new(
        48_000,
        Arc::clone(source) as Arc<dyn MonotonicSource>,
        MAX_EXTRAPOLATION_NANOS,
    )
}

/// Oeffnet das Ausgabegeraet; ohne Geraet (oder mit `TAKT_AUDIO=sim`) eine
/// neue Uhr mit simuliertem Audio-Thread. Der Schreibgriff einer Uhr wird nur
/// einmal herausgegeben und steckt nach einem gescheiterten Oeffnen im
/// verworfenen Callback, deshalb bekommt die Simulation ihre eigene Uhr.
fn start_audio(source: &Arc<SystemMonotonic>) -> (AudioMasterClock, Audio) {
    let reason = if std::env::var("TAKT_AUDIO").as_deref() == Ok("sim") {
        "TAKT_AUDIO=sim".to_owned()
    } else {
        let clock = new_clock(source);
        let writer = clock.writer().expect("Schreibgriff ist noch frei");
        let (mut tx, rx) = ring(2, RING_FRAMES);
        let callback =
            OutputCallback::new(rx, writer, Arc::clone(source) as Arc<dyn MonotonicSource>);
        let stats = callback.stats();
        // Vorrat vor dem ersten Callback, sonst beginnt der Stream mit einem Xrun.
        tx.push(&vec![0.0_f32; RING_FRAMES * 2]);
        let config = OutputConfig {
            // Cargo.toml turns on the feature realtime of cpal.
            realtime: true,
            ..OutputConfig::default()
        };
        match Output::open(config, callback) {
            Ok(output) => {
                // Die Testquelle hat keinen Ton: der Zufuehrer haelt den Ring
                // mit Stille voll, damit das Geraet die Uhr gleichmaessig stellt.
                let stop = Arc::new(AtomicBool::new(false));
                let feeder = {
                    let stop = Arc::clone(&stop);
                    std::thread::spawn(move || {
                        let silence = vec![0.0_f32; 480 * 2];
                        while !stop.load(Ordering::Relaxed) {
                            let frames = tx.free_frames().min(480);
                            tx.push(&silence[..frames * 2]);
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    })
                };
                return (
                    clock,
                    Audio::Device {
                        output,
                        stats,
                        feeder,
                        stop,
                    },
                );
            }
            Err(e) => e.to_string(),
        }
    };

    let clock = new_clock(source);
    let writer = clock.writer().expect("Schreibgriff ist noch frei");
    // Der simulierte Audio-Thread: alle 10 ms meldet er seine Position, genau
    // wie ein echter WASAPI- oder PipeWire-Callback.
    let thread = std::thread::spawn(move || {
        let period = Duration::from_millis(10);
        let samples_per_period = 480_u64; // 10 ms bei 48 kHz
        let total_periods = u64::try_from(DEMO_SECONDS).unwrap_or(3) * 100;
        let start = SystemMonotonic::new();
        for period_index in 0..=total_periods {
            writer.report(period_index * samples_per_period, start.now_nanos());
            std::thread::sleep(period);
        }
    });
    (clock, Audio::Simulated { thread, reason })
}

fn main() {
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
    let (clock, audio) = start_audio(&source);

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
    match &audio {
        Audio::Device { output, .. } => {
            println!(
                "  Audio        Standardgeraet, Puffer {:?}",
                output.buffer()
            );
        }
        Audio::Simulated { reason, .. } => {
            println!("  Audio        SIMULIERT ({reason}): kein Ton, ein Thread stellt die Uhr");
        }
    }
    println!();

    let mut scheduler = Scheduler::new();
    let frame_ns = info.frame_duration_nanos().unwrap_or(16_666_667);

    // Anzeigedurchlauf bei 60 Hz, so wie ein echter Renderer aufgerufen wuerde.
    let vsync = Duration::from_nanos(16_666_667);
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

    let device_stats = match audio {
        Audio::Device {
            output,
            stats,
            feeder,
            stop,
        } => {
            // Read after the callbacks ran: a refusal comes before the first.
            let priority = output.priority();
            drop(output);
            stop.store(true, Ordering::Relaxed);
            let _ = feeder.join();
            Some((stats, priority))
        }
        Audio::Simulated { thread, .. } => {
            let _ = thread.join();
            None
        }
    };

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
    if let Some((a, priority)) = device_stats {
        println!("  Prioritaet      {priority}");
        println!(
            "  Audio           {} Callbacks, {} Xruns, {} Frames Stille",
            a.callbacks(),
            a.xruns(),
            a.silent_frames()
        );
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
