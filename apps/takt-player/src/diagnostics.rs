// SPDX-License-Identifier: GPL-3.0-or-later
//! What the diagnostic overlay shows (F3) and saves (F4): the counters of all
//! threads in one snapshot, read once per pass of the render loop.
//!
//! The snapshot is plain data and its text block is tested here; the overlay
//! in [`crate::ui`] only draws it. Reading the counters costs atomics and
//! copies, no lock on the audio path.

// Formats measurements for display; time is never computed here, so floating
// point is allowed (as in the headless run).
#![allow(clippy::cast_precision_loss)]

use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use takt_audio::{DurationSnapshot, OutputStats};
use takt_backend::SchedulerStats;

/// The audio counters of a device path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioCounters {
    /// Callbacks so far.
    pub callbacks: u64,
    /// Callbacks that padded with silence.
    pub xruns: u64,
    /// Frames of silence after underruns.
    pub silent_frames: u64,
    /// Seconds since the start of the clock source of the last xrun.
    pub last_xrun_nanos: Option<u64>,
    /// Device latency subtracted from the clock, in frames.
    pub latency_frames: Option<u64>,
    /// Callback runtime: upper bounds of p50, p99, p99.99 in microseconds,
    /// and the maximum in nanoseconds.
    pub runtime: [Option<u64>; 3],
    /// The longest callback in nanoseconds.
    pub runtime_max_nanos: u64,
}

impl AudioCounters {
    /// Reads the counters of a running output.
    #[must_use]
    pub fn read(stats: &OutputStats) -> Self {
        let r = stats.runtime();
        Self {
            callbacks: stats.callbacks(),
            xruns: stats.xruns(),
            silent_frames: stats.silent_frames(),
            last_xrun_nanos: stats.last_xrun_nanos(),
            latency_frames: stats.latency_frames(),
            runtime: [
                r.quantile_micros(1, 2),
                r.quantile_micros(99, 100),
                r.quantile_micros(9_999, 10_000),
            ],
            runtime_max_nanos: r.max_nanos(),
        }
    }
}

/// CPU and memory of this process.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcessLoad {
    /// CPU time in percent of one core over the last interval (above 100
    /// with several busy threads).
    pub cpu_percent: f32,
    /// Resident memory in bytes.
    pub memory_bytes: u64,
}

/// Everything the overlay shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// The presentation counters of the scheduler.
    pub scheduler: SchedulerStats,
    /// Frames the source dropped as late.
    pub dropped: u64,
    /// Frames of the pool out at the last request, and its size.
    pub pool: (usize, usize),
    /// Age of the last anchor of the audio master clock, and the limit of
    /// its extrapolation.
    pub anchor_age_nanos: Option<u64>,
    /// The extrapolation limit of the clock.
    pub max_extrapolation_nanos: u64,
    /// The measured display period.
    pub display_period_nanos: Option<i64>,
    /// The audio path in one line (device, format, buffer, or simulated).
    pub audio_line: String,
    /// The counters of the device, `None` when simulated.
    pub audio: Option<AudioCounters>,
    /// The sample rate the latency is counted in.
    pub sample_rate: u32,
    /// CPU and memory, once measured.
    pub process: Option<ProcessLoad>,
    /// What building the overlay costs per pass: p50 upper bound in
    /// microseconds and the maximum in nanoseconds.
    pub overlay_cost: Option<(u64, u64)>,
}

fn ms(nanos: f64) -> String {
    format!("{:.3} ms", nanos / 1e6)
}

fn bound(us: Option<u64>) -> String {
    us.map_or("-".to_owned(), |us| format!("<{us} us"))
}

/// The label of histogram class `i` of [`SchedulerStats::histogram`].
#[must_use]
pub fn class_label(i: usize) -> String {
    let lo = if i == 0 { 0 } else { 1_u64 << i };
    let hi = 1_u64 << (i + 1);
    if hi >= 1_000 {
        format!("{:.1}..{:.1} ms", lo as f64 / 1e3, hi as f64 / 1e3)
    } else {
        format!("{lo}..{hi} us")
    }
}

impl Snapshot {
    /// The lines of the overlay, label and value.
    #[must_use]
    pub fn lines(&self) -> Vec<(&'static str, String)> {
        let s = &self.scheduler;
        let mut lines = vec![
            (
                "presented",
                format!(
                    "{} shown, {} repeated ({:.1} %), {} dropped",
                    s.shown,
                    s.repeated,
                    s.repeat_ratio(),
                    self.dropped
                ),
            ),
            (
                "error",
                format!(
                    "mean {}, late max {}, early max {}",
                    ms(s.mean_abs_error_nanos() as f64),
                    ms(s.worst_late_nanos as f64),
                    ms(s.worst_early_nanos as f64)
                ),
            ),
            (
                "anchor age",
                self.anchor_age_nanos.map_or("no anchor".to_owned(), |a| {
                    let stale = if a > self.max_extrapolation_nanos {
                        "  STALE"
                    } else {
                        ""
                    };
                    format!(
                        "{} (limit {}){stale}",
                        ms(a as f64),
                        ms(self.max_extrapolation_nanos as f64)
                    )
                }),
            ),
            (
                "pool",
                format!("{} of {} frames out", self.pool.0, self.pool.1),
            ),
            (
                "display",
                self.display_period_nanos
                    .filter(|&p| p > 0)
                    .map_or("not measured yet".to_owned(), |p| {
                        format!("{:.1} Hz ({})", 1e9 / p as f64, ms(p as f64))
                    }),
            ),
            ("audio", self.audio_line.clone()),
        ];
        if let Some(a) = &self.audio {
            lines.push((
                "xruns",
                format!(
                    "{} in {} callbacks, {} frames silence{}",
                    a.xruns,
                    a.callbacks,
                    a.silent_frames,
                    a.last_xrun_nanos.map_or(String::new(), |t| format!(
                        ", last at {:.3} s",
                        t as f64 / 1e9
                    ))
                ),
            ));
            lines.push((
                "latency",
                a.latency_frames
                    .map_or("unknown, not subtracted".to_owned(), |f| {
                        format!(
                            "{f} frames ({}), subtracted",
                            ms(f as f64 * 1e9 / f64::from(self.sample_rate.max(1)))
                        )
                    }),
            ));
            lines.push((
                "callback",
                format!(
                    "p50 {}  p99 {}  p99.99 {}  max {:.1} us",
                    bound(a.runtime[0]),
                    bound(a.runtime[1]),
                    bound(a.runtime[2]),
                    a.runtime_max_nanos as f64 / 1e3
                ),
            ));
        }
        lines.push((
            "process",
            self.process.map_or("measuring".to_owned(), |p| {
                format!(
                    "CPU {:.1} % of a core, RAM {:.1} MiB",
                    p.cpu_percent,
                    p.memory_bytes as f64 / (1024.0 * 1024.0)
                )
            }),
        ));
        lines.push((
            "overlay",
            self.overlay_cost.map_or("-".to_owned(), |(p50, max)| {
                format!("p50 <{p50} us, max {:.1} us per pass", max as f64 / 1e3)
            }),
        ));
        lines
    }

    /// The text block F4 saves: the lines, then the histogram of the errors.
    #[must_use]
    pub fn report(&self, title: &str) -> String {
        let mut out = format!("{title}\n");
        for (label, value) in self.lines() {
            let _ = writeln!(out, "  {label:<11} {value}");
        }
        let _ = writeln!(out, "  histogram of the presentation errors");
        let peak = self.scheduler.histogram.iter().copied().max().unwrap_or(0);
        for (i, &count) in self.scheduler.histogram.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let bar = usize::try_from(u64::from(count) * 40 / u64::from(peak.max(1))).unwrap_or(40);
            let _ = writeln!(
                out,
                "    {:>16} {count:>7} {}",
                class_label(i),
                "#".repeat(bar.max(1))
            );
        }
        out
    }
}

/// CPU and memory of this process, measured once a second on a thread of
/// its own (a refresh takes milliseconds on Windows, too long for a pass of
/// the render loop). The thread ends with the meter.
pub struct ProcessMeter {
    load: Arc<Mutex<Option<ProcessLoad>>>,
}

impl ProcessMeter {
    /// Starts measuring this process.
    #[must_use]
    pub fn start() -> Self {
        let load = Arc::new(Mutex::new(None));
        let weak = Arc::downgrade(&load);
        let spawned = std::thread::Builder::new()
            .name("takt-process-meter".to_owned())
            .spawn(move || measure(&weak));
        if let Err(e) = spawned {
            eprintln!("takt-player: no CPU/RAM measurement: {e}");
        }
        Self { load }
    }

    /// The last measurement, `None` before the second one.
    #[must_use]
    pub fn load(&self) -> Option<ProcessLoad> {
        *self.load.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The loop of the meter thread, until the meter is dropped.
fn measure(into: &Weak<Mutex<Option<ProcessLoad>>>) {
    let Ok(pid) = sysinfo::get_current_pid() else {
        return;
    };
    let mut system = sysinfo::System::new();
    let mut first = true;
    loop {
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            false,
            sysinfo::ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory(),
        );
        let Some(slot) = into.upgrade() else {
            return;
        };
        // The first CPU value has nothing to compare with.
        if !first && let Some(p) = system.process(pid) {
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(ProcessLoad {
                cpu_percent: p.cpu_usage(),
                memory_bytes: p.memory(),
            });
        }
        drop(slot);
        first = false;
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The p50 upper bound and maximum of `cost`, once something was recorded.
#[must_use]
pub fn overlay_cost(cost: &DurationSnapshot) -> Option<(u64, u64)> {
    Some((cost.quantile_micros(1, 2)?, cost.max_nanos()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        let mut scheduler = SchedulerStats {
            shown: 4,
            repeated: 4,
            abs_error_sum_nanos: 8_000_000,
            worst_late_nanos: 3_000_000,
            worst_early_nanos: -1_000_000,
            ..SchedulerStats::default()
        };
        scheduler.histogram[10] = 3;
        scheduler.histogram[11] = 1;
        Snapshot {
            scheduler,
            dropped: 2,
            pool: (1, 6),
            anchor_age_nanos: Some(30_000_000),
            max_extrapolation_nanos: 20_000_000,
            display_period_nanos: Some(8_333_333),
            audio_line: "default device, F32, buffer Fixed(480)".to_owned(),
            audio: Some(AudioCounters {
                callbacks: 100,
                xruns: 1,
                silent_frames: 380,
                last_xrun_nanos: Some(1_500_000_000),
                latency_frames: Some(1_531),
                runtime: [Some(4), Some(5), None],
                runtime_max_nanos: 6_000,
            }),
            sample_rate: 48_000,
            process: Some(ProcessLoad {
                cpu_percent: 12.5,
                memory_bytes: 64 * 1024 * 1024,
            }),
            overlay_cost: Some((128, 200_000)),
        }
    }

    #[test]
    fn the_report_names_every_counter() {
        let text = snapshot().report("takt diagnostics");
        for expected in [
            "4 shown, 4 repeated (50.0 %), 2 dropped",
            "mean 2.000 ms",
            "30.000 ms (limit 20.000 ms)  STALE",
            "1 of 6 frames out",
            "120.0 Hz",
            "F32",
            "1 in 100 callbacks, 380 frames silence, last at 1.500 s",
            "1531 frames (31.896 ms), subtracted",
            "p50 <4 us  p99 <5 us  p99.99 -",
            "CPU 12.5 % of a core, RAM 64.0 MiB",
            "p50 <128 us, max 200.0 us",
            "1.0..2.0 ms       3 ########################################",
            "2.0..4.1 ms       1 #############",
        ] {
            assert!(text.contains(expected), "{expected:?} missing in\n{text}");
        }
    }

    #[test]
    fn a_simulated_path_has_no_audio_lines() {
        let mut s = snapshot();
        s.audio = None;
        s.anchor_age_nanos = None;
        s.process = None;
        let text = s.report("t");
        assert!(!text.contains("xruns"));
        assert!(text.contains("no anchor"));
        assert!(text.contains("measuring"));
    }

    #[test]
    fn the_classes_are_labelled_in_their_unit() {
        assert_eq!(class_label(0), "0..2 us");
        assert_eq!(class_label(8), "256..512 us");
        assert_eq!(class_label(14), "16.4..32.8 ms");
    }
}
