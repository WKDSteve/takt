// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A histogram of durations that the audio thread fills with atomics.
//!
//! # Why log-linear classes
//!
//! The decisive number of the audio path is a far tail, the 99.99th
//! percentile of the callback runtime (`KONZEPT.md`, the third hard problem):
//! one callback in ten thousand that overruns is an audible click every few
//! minutes. Pure powers of two (as in the scheduler) are too coarse there, a
//! tail of 600 us and one of 1000 us would land in one class. So each power of
//! two is split into four classes: below 8 us one class per microsecond, above
//! that a class is at most a quarter of its lower bound wide. A percentile is
//! reported as the upper bound of its class, so it is never too low and at
//! most 25 % too high.
//!
//! # Who does what
//!
//! The audio thread only calls [`DurationHistogram::record`]: one division,
//! one `ilog2`, one relaxed `fetch_add`. The reader copies the counters and
//! computes percentiles from the copy; a copy taken while the audio thread
//! records may be off by the callbacks of that moment, which is fine for a
//! display and for the numbers printed at the end.

use std::sync::atomic::{AtomicU64, Ordering};

/// Classes below 8 us, one per microsecond.
const LINEAR: usize = 8;
/// Classes per power of two above that.
const SUB: usize = 4;
/// The highest power of two with classes of its own: 2^20 us, about a second.
/// Longer durations land in the last class.
const MAX_POW: usize = 20;
/// Number of classes.
pub const CLASSES: usize = LINEAR + (MAX_POW - 2) * SUB;

/// The class of `micros`.
fn class_of(micros: u64) -> usize {
    if micros < LINEAR as u64 {
        // Lossless: below 8.
        return usize::try_from(micros).unwrap_or(0);
    }
    let pow = micros.ilog2() as usize; // 3 or more
    if pow > MAX_POW {
        return CLASSES - 1;
    }
    let sub = usize::try_from((micros >> (pow - 2)) & 3).unwrap_or(0);
    LINEAR + (pow - 3) * SUB + sub
}

/// The smallest duration in us that no longer falls into `class`.
fn upper_bound(class: usize) -> u64 {
    if class < LINEAR {
        return class as u64 + 1;
    }
    let pow = (class - LINEAR) / SUB + 3;
    let sub = ((class - LINEAR) % SUB) as u64;
    (1_u64 << pow) + (sub + 1) * (1_u64 << (pow - 2))
}

/// Durations in log-linear microsecond classes, filled without locks.
#[derive(Debug)]
pub struct DurationHistogram {
    counts: [AtomicU64; CLASSES],
    max_nanos: AtomicU64,
}

impl Default for DurationHistogram {
    fn default() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            max_nanos: AtomicU64::new(0),
        }
    }
}

impl DurationHistogram {
    /// Counts one duration.
    ///
    /// **Callable from the audio thread.** Does not allocate, lock or wait.
    pub fn record(&self, nanos: u64) {
        self.counts[class_of(nanos / 1_000)].fetch_add(1, Ordering::Relaxed);
        self.max_nanos.fetch_max(nanos, Ordering::Relaxed);
    }

    /// A copy of the counters, to compute from.
    #[must_use]
    pub fn snapshot(&self) -> DurationSnapshot {
        DurationSnapshot {
            counts: std::array::from_fn(|i| self.counts[i].load(Ordering::Relaxed)),
            max_nanos: self.max_nanos.load(Ordering::Relaxed),
        }
    }
}

/// The counters of a [`DurationHistogram`] at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurationSnapshot {
    counts: [u64; CLASSES],
    max_nanos: u64,
}

impl DurationSnapshot {
    /// Durations counted.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// The longest duration counted, in nanoseconds (exact, not a class).
    #[must_use]
    pub fn max_nanos(&self) -> u64 {
        self.max_nanos
    }

    /// The duration in microseconds below which `num / den` of all durations
    /// lie, as the upper bound of its class (never too low, at most 25 % too
    /// high). `None` without any duration or for `num > den` or `den == 0`.
    ///
    /// The 99.99th percentile is `quantile_micros(9_999, 10_000)`.
    #[must_use]
    pub fn quantile_micros(&self, num: u64, den: u64) -> Option<u64> {
        let total = self.total();
        if total == 0 || den == 0 || num > den {
            return None;
        }
        // The rank of the duration, rounded up: the smallest count of
        // durations that covers num/den of all, at least one.
        let rank = (u128::from(total) * u128::from(num))
            .div_ceil(u128::from(den))
            .max(1);
        let mut seen = 0_u128;
        for (class, &count) in self.counts.iter().enumerate() {
            seen += u128::from(count);
            if seen >= rank {
                return Some(upper_bound(class));
            }
        }
        Some(upper_bound(CLASSES - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_are_contiguous_and_at_most_a_quarter_wide() {
        let mut previous = 0;
        for micros in 0..(1_u64 << 21) {
            let class = class_of(micros);
            assert!(
                class == previous || class == previous + 1,
                "jump at {micros}"
            );
            assert!(micros < upper_bound(class), "{micros} not below its bound");
            previous = class;
        }
        assert_eq!(previous, CLASSES - 1);
        for class in LINEAR..CLASSES - 1 {
            let low = upper_bound(class - 1);
            let width = upper_bound(class) - low;
            assert!(width * 4 <= low, "class {class}: {low}+{width}");
        }
    }

    #[test]
    fn a_far_tail_is_seen_in_the_percentile() {
        let h = DurationHistogram::default();
        for _ in 0..9_998 {
            h.record(150_000); // 150 us
        }
        h.record(600_000); // 600 us, twice in ten thousand
        h.record(2_500_000); // 2.5 ms
        let s = h.snapshot();
        assert_eq!(s.total(), 10_000);
        assert_eq!(s.quantile_micros(1, 2), Some(160)); // 150 is in [144, 160)
        assert_eq!(s.quantile_micros(99, 100), Some(160));
        assert_eq!(s.quantile_micros(9_999, 10_000), Some(640)); // 600 in [576, 640)
        assert_eq!(s.quantile_micros(1, 1), Some(2_560)); // 2500 in [2304, 2560)
        assert_eq!(s.max_nanos(), 2_500_000);
    }

    #[test]
    fn no_durations_no_percentile() {
        let s = DurationHistogram::default().snapshot();
        assert_eq!(s.quantile_micros(1, 2), None);
        assert_eq!(s.max_nanos(), 0);
        let h = DurationHistogram::default();
        h.record(1_000);
        assert_eq!(h.snapshot().quantile_micros(2, 1), None);
        assert_eq!(h.snapshot().quantile_micros(1, 0), None);
        assert_eq!(
            h.snapshot().quantile_micros(0, 1),
            Some(2),
            "the smallest is the first"
        );
    }

    #[test]
    fn a_duration_beyond_a_second_lands_in_the_last_class() {
        let h = DurationHistogram::default();
        h.record(5_000_000_000);
        assert_eq!(
            h.snapshot().quantile_micros(1, 1),
            Some(upper_bound(CLASSES - 1))
        );
        assert_eq!(h.snapshot().max_nanos(), 5_000_000_000);
    }
}
