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

/// The measured display period, and how far the render loop fell behind it.
#[derive(Debug, Default)]
pub struct RefreshMeter {
    last: Option<u64>,
    intervals: [u64; WINDOW],
    len: usize,
    next: usize,
    /// Longest time between two passes, stalls included.
    longest_gap: u64,
    /// Refreshes no pass came for, counted once the period is known.
    missed: u64,
}

impl RefreshMeter {
    /// One pass of the render loop at monotonic time `now_nanos`.
    pub fn pass(&mut self, now_nanos: u64) {
        if let Some(last) = self.last.replace(now_nanos) {
            let dt = now_nanos.saturating_sub(last);
            self.longest_gap = self.longest_gap.max(dt);
            // Measured before this interval joins the median: a stall must
            // not count against a period it moved itself.
            if let Some(period) = self.period_nanos().and_then(|p| u64::try_from(p).ok()) {
                // Rounded to whole refreshes: a pass that comes a little late
                // for its refresh has missed none.
                let refreshes = dt.saturating_add(period / 2) / period.max(1);
                self.missed = self.missed.saturating_add(refreshes.saturating_sub(1));
            }
            if (MIN_NANOS..=MAX_NANOS).contains(&dt) {
                self.intervals[self.next] = dt;
                self.next = (self.next + 1) % WINDOW;
                self.len = (self.len + 1).min(WINDOW);
            }
        }
    }

    /// The longest time between two passes, in nanoseconds, and the
    /// refreshes no pass came for: what the headless run reports as
    /// `pass_gap_us` and `missed`. A window that is dragged or hidden counts
    /// here too; a measuring run leaves it alone.
    #[must_use]
    pub fn gaps(&self) -> (u64, u64) {
        (self.longest_gap, self.missed)
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
    fn a_stall_counts_the_refreshes_it_skipped_and_a_late_pass_none() {
        let mut m = RefreshMeter::default();
        let period = 16_666_667;
        let mut t = 0;
        for _ in 0..10 {
            t += period;
            m.pass(t);
        }
        assert_eq!(m.gaps(), (period, 0));
        // A pass 6 ms late for its refresh, and the next on time again.
        t += period + 6_000_000;
        m.pass(t);
        t += period - 6_000_000;
        m.pass(t);
        assert_eq!(m.gaps(), (period + 6_000_000, 0));
        // 300 ms without a pass: 18 refreshes, 17 of them missed.
        t += 300_000_000;
        m.pass(t);
        assert_eq!(m.gaps(), (300_000_000, 17));
        assert_eq!(m.period_nanos(), Some(16_666_667));
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
