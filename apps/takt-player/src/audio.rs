// SPDX-License-Identifier: GPL-3.0-or-later
//! The sound path of the player: the default output device, or a thread that
//! stands in for its callback.
//!
//! The test source has no sound, so a feeder keeps the ring full of silence
//! and the device drives the audio master clock at its own pace. Without a
//! device, or with `TAKT_AUDIO=sim`, a thread reports a position every 10 ms
//! instead, and the player says so. `TAKT_AUDIO_DEVICE` names another output
//! device by a part of its name (a measuring run on a silent virtual device);
//! a name that fits none is simulated too, never the default device.
//!
//! The feeder is a thread of normal priority: [`FeederStats`] says how low
//! the ring ran before it filled it again, and the longest time between two
//! fills, for the overlay and the measuring run (an xrun under load: did the
//! feeder starve, or the device?).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use takt_audio::{Gain, Output, OutputCallback, OutputConfig, OutputStats, Priority, ring};
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
        // The wish of `TAKT_AUDIO_DEVICE`, `None` for the default device.
        wanted: Option<String>,
        // Keeps the stream alive; not Send on every platform, so the path
        // stays on the thread that started it.
        output: Output,
        stats: Arc<OutputStats>,
        gain: Arc<Gain>,
        feeder: Arc<FeederStats>,
    },
    Simulated {
        reason: String,
    },
}

/// What the feeder saw of the ring since the last [`FeederStats::take`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingReading {
    /// The lowest stock of the ring found before a fill, in frames.
    pub lowest_frames: usize,
    /// The longest time between two fills, in nanoseconds.
    pub longest_gap_nanos: u64,
}

/// The ring as the feeder sees it: written by the feeder, taken by the
/// overlay and the measuring run. Nothing here is touched by the callback.
#[derive(Debug)]
pub struct FeederStats {
    // usize::MAX: no fill since the last take.
    lowest: AtomicUsize,
    longest_gap: AtomicU64,
}

impl Default for FeederStats {
    fn default() -> Self {
        Self {
            lowest: AtomicUsize::new(usize::MAX),
            longest_gap: AtomicU64::new(0),
        }
    }
}

impl FeederStats {
    /// One fill: the stock found before it, and the time since the fill
    /// before.
    fn record(&self, stock_frames: usize, gap_nanos: u64) {
        self.lowest.fetch_min(stock_frames, Ordering::Relaxed);
        self.longest_gap.fetch_max(gap_nanos, Ordering::Relaxed);
    }

    /// What came since the last take, and starts anew; `None` without a
    /// fill in between.
    pub fn take(&self) -> Option<RingReading> {
        let lowest = self.lowest.swap(usize::MAX, Ordering::Relaxed);
        let gap = self.longest_gap.swap(0, Ordering::Relaxed);
        (lowest != usize::MAX).then_some(RingReading {
            lowest_frames: lowest,
            longest_gap_nanos: gap,
        })
    }
}

/// What a device path measured, read when it stops.
pub struct DeviceSummary {
    /// The counters of the callback.
    pub stats: Arc<OutputStats>,
    /// What became of the real-time priority.
    pub priority: Priority,
    /// The ring over the whole run.
    pub ring: Option<RingReading>,
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
        let gain = callback.gain();
        // A stock before the first callback, or the stream starts with an xrun.
        tx.push(&vec![0.0_f32; RING_FRAMES * 2]);
        let config = OutputConfig {
            // Cargo.toml turns on the feature realtime of cpal.
            realtime: true,
            ..OutputConfig::default()
        };
        let wanted = std::env::var("TAKT_AUDIO_DEVICE").ok();
        match Output::open_on(config, callback, wanted.as_deref()) {
            Ok(output) => {
                let stats_of_feeder = Arc::new(FeederStats::default());
                let feeder = {
                    let stop = Arc::clone(&stop);
                    let seen = Arc::clone(&stats_of_feeder);
                    std::thread::spawn(move || {
                        let silence = vec![0.0_f32; 480 * 2];
                        let mut last = Instant::now();
                        while !stop.load(Ordering::Relaxed) {
                            let free = tx.free_frames();
                            let now = Instant::now();
                            let gap = u64::try_from((now - last).as_nanos()).unwrap_or(u64::MAX);
                            seen.record(tx.capacity_frames().saturating_sub(free), gap);
                            last = now;
                            tx.push(&silence[..free.min(480) * 2]);
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    })
                };
                let path = AudioPath {
                    kind: Some(Kind::Device {
                        wanted,
                        output,
                        stats,
                        gain,
                        feeder: stats_of_feeder,
                    }),
                    stop,
                    thread: Some(feeder),
                };
                return (clock, path);
            }
            Err(e) => e.to_string(),
        }
    };
    simulated_with(source, reason, stop)
}

/// A clock with a simulated callback thread and no device at all, whatever
/// the environment says: for tests, which must never open an output device.
pub fn simulated(source: &Arc<SystemMonotonic>, reason: &str) -> (AudioMasterClock, AudioPath) {
    simulated_with(source, reason.to_owned(), Arc::new(AtomicBool::new(false)))
}

fn simulated_with(
    source: &Arc<SystemMonotonic>,
    reason: String,
    stop: Arc<AtomicBool>,
) -> (AudioMasterClock, AudioPath) {
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
            Some(Kind::Device { wanted, output, .. }) => {
                let device = wanted
                    .as_ref()
                    .map_or("default device".to_owned(), |w| format!("device {w:?}"));
                format!(
                    "{device}, {:?}, buffer {:?}",
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

    /// Sets the volume of the device (0 mutes); the simulated path has no
    /// sound and keeps nothing.
    pub fn set_volume(&self, volume: f32) {
        if let Some(Kind::Device { gain, .. }) = &self.kind {
            gain.set(volume);
        }
    }

    /// The counters of the device callback, `None` when simulated.
    pub fn device_stats(&self) -> Option<Arc<OutputStats>> {
        match &self.kind {
            Some(Kind::Device { stats, .. }) => Some(Arc::clone(stats)),
            _ => None,
        }
    }

    /// What the feeder saw of the ring, `None` when simulated.
    pub fn feeder_stats(&self) -> Option<Arc<FeederStats>> {
        match &self.kind {
            Some(Kind::Device { feeder, .. }) => Some(Arc::clone(feeder)),
            _ => None,
        }
    }

    /// Stops the path; for a device, what it measured.
    pub fn stop(mut self) -> Option<DeviceSummary> {
        self.halt()
    }

    fn halt(&mut self) -> Option<DeviceSummary> {
        let summary = match self.kind.take() {
            Some(Kind::Device {
                output,
                stats,
                feeder,
                ..
            }) => {
                // Read after the callbacks ran: a refusal comes before the first.
                let priority = output.priority();
                drop(output);
                Some(DeviceSummary {
                    stats,
                    priority,
                    ring: feeder.take(),
                })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_feeder_keeps_the_lowest_stock_and_the_longest_gap_until_taken() {
        let stats = FeederStats::default();
        assert_eq!(stats.take(), None, "no fill yet");
        stats.record(4_000, 2_000_000);
        stats.record(1_200, 45_000_000);
        stats.record(4_700, 2_100_000);
        assert_eq!(
            stats.take(),
            Some(RingReading {
                lowest_frames: 1_200,
                longest_gap_nanos: 45_000_000,
            })
        );
        assert_eq!(stats.take(), None, "taken: it starts anew");
        stats.record(4_800, 1_000_000);
        assert_eq!(stats.take().map(|r| r.lowest_frames), Some(4_800));
    }
}
