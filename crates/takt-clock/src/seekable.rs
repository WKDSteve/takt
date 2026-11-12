// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A clock that can jump: the media position as the position of another
//! clock plus an offset.
//!
//! The audio master clock counts the frames the device played since the
//! stream started; it cannot be set, because the device keeps playing. A seek
//! changes what that count *means*: from now on, media position = device
//! position + offset. [`SeekableClock::seek`] sets the offset so that the media
//! position is the target at this moment; the inner clock goes on as before,
//! so its interpolation and its anchor stay untouched.
//!
//! Pausing is done here too, not by the inner clock: the device plays on
//! (silence) during a pause and keeps reporting, so the audio master clock
//! moves on even with `set_paused`. [`SeekableClock::set_paused`] keeps the
//! media position of the moment and, on resume, seeks to it.

use takt_core::Timestamp;

use crate::{Clock, ClockKind};

/// `inner` plus an offset in nanoseconds.
#[derive(Debug)]
pub struct SeekableClock<C> {
    inner: C,
    offset_nanos: i64,
    paused_at: Option<Timestamp>,
}

fn nanos_of(t: Timestamp) -> i64 {
    t.rescale(takt_core::Rational::NANOS)
        .map_or(i64::MAX, Timestamp::ticks)
}

impl<C: Clock> SeekableClock<C> {
    /// `inner` with no offset: the media position is its position.
    #[must_use]
    pub const fn new(inner: C) -> Self {
        Self {
            inner,
            offset_nanos: 0,
            paused_at: None,
        }
    }

    /// The clock underneath.
    #[must_use]
    pub const fn inner(&self) -> &C {
        &self.inner
    }

    /// Makes `to` the media position of this moment; while paused, the
    /// position it holds and resumes from.
    pub fn seek(&mut self, to: Timestamp) {
        if self.paused_at.is_some() {
            // In the base of the inner clock, as `now` gives it otherwise.
            let base = self.inner.now().base();
            self.paused_at = Some(to.rescale(base).unwrap_or(to));
        } else {
            self.offset_nanos = nanos_of(to).saturating_sub(nanos_of(self.inner.now()));
        }
    }

    /// Holds the media position (`true`) or lets it run on from where it was
    /// held (`false`).
    pub fn set_paused(&mut self, paused: bool) {
        match (paused, self.paused_at) {
            (true, None) => self.paused_at = Some(self.now()),
            (false, Some(at)) => {
                self.paused_at = None;
                self.seek(at);
            }
            _ => {}
        }
    }

    /// Whether the media position is held.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }

    /// The offset now in force, in nanoseconds.
    #[must_use]
    pub const fn offset_nanos(&self) -> i64 {
        self.offset_nanos
    }
}

impl<C: Clock> Clock for SeekableClock<C> {
    fn kind(&self) -> ClockKind {
        self.inner.kind()
    }

    fn now(&self) -> Timestamp {
        if let Some(at) = self.paused_at {
            return at;
        }
        let inner = self.inner.now();
        Timestamp::from_nanos(self.offset_nanos)
            .rescale(inner.base())
            .and_then(|offset| inner.offset(offset.ticks()))
            .unwrap_or(inner)
    }

    fn until_nanos(&self, pts: Timestamp) -> i64 {
        if let Some(at) = self.paused_at {
            return nanos_of(pts).saturating_sub(nanos_of(at));
        }
        // until(pts) = pts - media = pts - (inner + offset).
        self.inner
            .until_nanos(pts)
            .saturating_sub(self.offset_nanos)
    }

    fn may_drop(&self) -> bool {
        self.inner.may_drop()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{AudioMasterClock, ManualMonotonic, MonotonicSource};

    fn setup() -> (SeekableClock<AudioMasterClock>, crate::AudioClockWriter) {
        let source = Arc::new(ManualMonotonic::new());
        let clock = AudioMasterClock::new(48_000, source as Arc<dyn MonotonicSource>, 20_000_000);
        let writer = clock.writer().unwrap();
        (SeekableClock::new(clock), writer)
    }

    #[test]
    fn without_a_seek_it_is_the_inner_clock() {
        let (clock, writer) = setup();
        writer.report(48_000, 0);
        assert_eq!(clock.now().ticks(), 48_000);
        assert_eq!(clock.until_nanos(Timestamp::from_millis(1_020)), 20_000_000);
    }

    #[test]
    fn a_seek_moves_the_media_position_and_the_device_goes_on() {
        let (mut clock, writer) = setup();
        writer.report(48_000, 0); // the device has played 1 s
        clock.seek(Timestamp::from_millis(60_000)); // jump to 1 min
        assert_eq!(clock.now().ticks(), 60 * 48_000);
        assert_eq!(clock.offset_nanos(), 59_000_000_000);
        // A frame at 1 min 20 ms is 20 ms away, one at 1 s is long gone.
        assert_eq!(
            clock.until_nanos(Timestamp::from_millis(60_020)),
            20_000_000
        );
        assert!(clock.until_nanos(Timestamp::from_millis(1_000)) < -58_000_000_000);
        // The device plays on: half a second later the media is at 1:00.5.
        writer.report(48_000 + 24_000, 0);
        assert_eq!(clock.now().ticks(), 60 * 48_000 + 24_000);
        // Back to the start.
        clock.seek(Timestamp::from_millis(0));
        assert_eq!(clock.now().ticks(), 0);
        assert!(clock.may_drop());
    }

    #[test]
    fn a_pause_holds_the_media_position_while_the_device_plays_on() {
        let (mut clock, writer) = setup();
        writer.report(48_000, 0);
        clock.seek(Timestamp::from_millis(10_000));
        clock.set_paused(true);
        assert!(clock.is_paused());
        writer.report(96_000, 0); // the device plays on (silence) for 1 s
        assert_eq!(clock.now().ticks(), 10 * 48_000, "held at 10 s");
        assert_eq!(
            clock.until_nanos(Timestamp::from_millis(10_040)),
            40_000_000
        );
        clock.set_paused(false);
        assert_eq!(
            clock.now().ticks(),
            10 * 48_000,
            "resumes where it was held"
        );
        writer.report(96_000 + 4_800, 0);
        assert_eq!(clock.now().ticks(), 10 * 48_000 + 4_800, "and runs on");
    }

    #[test]
    fn a_seek_while_paused_moves_the_held_position() {
        let (mut clock, writer) = setup();
        writer.report(48_000, 0);
        clock.set_paused(true);
        clock.seek(Timestamp::from_millis(30_000));
        assert_eq!(clock.now().ticks(), 30 * 48_000);
        writer.report(96_000, 0);
        clock.set_paused(false);
        assert_eq!(clock.now().ticks(), 30 * 48_000);
    }
}
