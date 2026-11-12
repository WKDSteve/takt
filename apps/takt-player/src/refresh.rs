// SPDX-License-Identifier: GPL-3.0-or-later
//! The period of the display, measured from the passes of the render loop.
//!
//! eframe calls the render loop once per refresh, but also for input events,
//! and a window that is dragged or hidden pauses it. The median of the last
//! intervals ignores both; intervals outside 2..=100 ms are not counted.

/// Intervals the median is taken over.
const WINDOW: usize = 31;
/// Shorter intervals are extra passes (input), not refreshes: above 500 Hz.
const MIN_NANOS: u64 = 2_000_000;
/// Longer intervals are stalls, not refreshes: below 10 Hz.
const MAX_NANOS: u64 = 100_000_000;

/// The measured display period.
#[derive(Debug, Default)]
pub struct RefreshMeter {
    last: Option<u64>,
    intervals: [u64; WINDOW],
    len: usize,
    next: usize,
}

impl RefreshMeter {
    /// One pass of the render loop at monotonic time `now_nanos`.
    pub fn pass(&mut self, now_nanos: u64) {
        if let Some(last) = self.last.replace(now_nanos) {
            let dt = now_nanos.saturating_sub(last);
            if (MIN_NANOS..=MAX_NANOS).contains(&dt) {
                self.intervals[self.next] = dt;
                self.next = (self.next + 1) % WINDOW;
                self.len = (self.len + 1).min(WINDOW);
            }
        }
    }

    /// The median of the counted intervals, once there are eight.
    #[must_use]
    pub fn period_nanos(&self) -> Option<i64> {
        if self.len < 8 {
            return None;
        }
        let mut sorted = self.intervals;
        let counted = &mut sorted[..self.len];
        counted.sort_unstable();
        i64::try_from(counted[self.len / 2]).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_median_ignores_input_passes_and_stalls() {
        let mut m = RefreshMeter::default();
        let mut t = 0;
        for i in 0..40 {
            t += 8_333_333;
            m.pass(t);
            if i % 5 == 0 {
                // An input event between two refreshes, and a stall.
                m.pass(t + 1_000_000);
                t += 1_000_000;
            }
            if i == 20 {
                t += 500_000_000;
                m.pass(t);
            }
        }
        let p = m.period_nanos().unwrap();
        assert!((7_300_000..=8_400_000).contains(&p), "{p}");
    }

    #[test]
    fn no_period_before_eight_intervals() {
        let mut m = RefreshMeter::default();
        for i in 0..8 {
            m.pass(i * 16_666_667);
        }
        assert_eq!(m.period_nanos(), None);
        m.pass(8 * 16_666_667);
        assert_eq!(m.period_nanos(), Some(16_666_667));
    }
}
