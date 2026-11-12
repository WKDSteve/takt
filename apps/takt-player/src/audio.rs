// SPDX-License-Identifier: GPL-3.0-or-later
//! The sound path of the player: the default output device, or a thread that
//! stands in for its callback.
//!
//! The test source has no sound, so a feeder keeps the ring full of silence
//! and the device drives the audio master clock at its own pace. Without a
//! device, or with `TAKT_AUDIO=sim`, a thread reports a position every 10 ms
//! instead, and the player says so.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use takt_audio::{Output, OutputCallback, OutputConfig, OutputStats, Priority, ring};
use takt_clock::{AudioMasterClock, MonotonicSource, SystemMonotonic};

/// Frames per second of the clock and the device.
pub const SAMPLE_RATE: u32 = 48_000;
/// Extrapolation limit of the clock: twice the 10 ms callback period.
const MAX_EXTRAPOLATION_NANOS: u64 = 20_000_000;
/// Stock of the ring: 100 ms of stereo.
const RING_FRAMES: usize = 4_800;
/// Frames per period of the simulated callback (10 ms).
const PERIOD_FRAMES: u64 = 480;

enum Kind {
    Device {
        // Keeps the stream alive; not Send on every platform, so the path
        // stays on the thread that started it.
        output: Output,
        stats: Arc<OutputStats>,
    },
    Simulated {
        reason: String,
    },
}

/// What a device path measured, read when it stops.
pub struct DeviceSummary {
    /// The counters of the callback.
    pub stats: Arc<OutputStats>,
    /// What became of the real-time priority.
    pub priority: Priority,
}

/// A running sound path. Dropping it stops it too.
pub struct AudioPath {
    kind: Option<Kind>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn new_clock(source: &Arc<SystemMonotonic>) -> AudioMasterClock {
    AudioMasterClock::new(
        SAMPLE_RATE,
        Arc::clone(source) as Arc<dyn MonotonicSource>,
        MAX_EXTRAPOLATION_NANOS,
    )
}

/// Opens the output device; without one (or with `TAKT_AUDIO=sim`) a new
/// clock with a simulated callback thread. A clock hands out its writer once,
/// and after a failed open it is gone with the dropped callback, so the
/// simulation gets a clock of its own.
pub fn start(source: &Arc<SystemMonotonic>) -> (AudioMasterClock, AudioPath) {
    let stop = Arc::new(AtomicBool::new(false));
    let reason = if std::env::var("TAKT_AUDIO").as_deref() == Ok("sim") {
        "TAKT_AUDIO=sim".to_owned()
    } else {
        let clock = new_clock(source);
        let writer = clock.writer().expect("the writer of a new clock is free");
        let (mut tx, rx) = ring(2, RING_FRAMES);
        let callback =
            OutputCallback::new(rx, writer, Arc::clone(source) as Arc<dyn MonotonicSource>);
        let stats = callback.stats();
        // A stock before the first callback, or the stream starts with an xrun.
        tx.push(&vec![0.0_f32; RING_FRAMES * 2]);
        let config = OutputConfig {
            // Cargo.toml turns on the feature realtime of cpal.
            realtime: true,
            ..OutputConfig::default()
        };
        match Output::open(config, callback) {
            Ok(output) => {
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
                let path = AudioPath {
                    kind: Some(Kind::Device { output, stats }),
                    stop,
                    thread: Some(feeder),
                };
                return (clock, path);
            }
            Err(e) => e.to_string(),
        }
    };

    let clock = new_clock(source);
    let writer = clock.writer().expect("the writer of a new clock is free");
    let thread = {
        let stop = Arc::clone(&stop);
        let source = Arc::clone(source);
        // Reports every 10 ms, as a WASAPI or PipeWire callback would, with
        // the times of the source the clock reads.
        std::thread::spawn(move || {
            let mut period = 0_u64;
            while !stop.load(Ordering::Relaxed) {
                writer.report(period * PERIOD_FRAMES, source.now_nanos());
                period += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let path = AudioPath {
        kind: Some(Kind::Simulated { reason }),
        stop,
        thread: Some(thread),
    };
    (clock, path)
}

impl AudioPath {
    /// One line for the start of the player.
    pub fn describe(&self) -> String {
        match &self.kind {
            Some(Kind::Device { output, .. }) => {
                format!(
                    "default device, {:?}, buffer {:?}",
                    output.format(),
                    output.buffer()
                )
            }
            Some(Kind::Simulated { reason }) => {
                format!("SIMULATED ({reason}): no sound, a thread sets the clock")
            }
            None => "stopped".to_owned(),
        }
    }

    /// Stops the path; for a device, what it measured.
    pub fn stop(mut self) -> Option<DeviceSummary> {
        self.halt()
    }

    fn halt(&mut self) -> Option<DeviceSummary> {
        let summary = match self.kind.take() {
            Some(Kind::Device { output, stats }) => {
                // Read after the callbacks ran: a refusal comes before the first.
                let priority = output.priority();
                drop(output);
                Some(DeviceSummary { stats, priority })
            }
            _ => None,
        };
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        summary
    }
}

impl Drop for AudioPath {
    fn drop(&mut self) {
        let _ = self.halt();
    }
}
