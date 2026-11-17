// SPDX-License-Identifier: GPL-3.0-or-later
//! The time a rendered frame carries, when the position of mpv is no longer
//! it.
//!
//! The render API of mpv names no time for a frame. The render thread reads
//! the position of the core right after it has rendered
//! ([`super::backend`], "Two threads"), and in the run that is the time of
//! the very frame. It is not when the thread comes late: the core hands the
//! next frame to its output a lead ahead of time and names that one from
//! then on, whether the frame before has been rendered or not. On a busy
//! machine the picture of one frame so got the time of the next or the one
//! after (measured under load in `ubuntu:24.04` with libmpv 0.37: about one
//! run of the test in ten, 27 ms to 224 ms between render and read).
//!
//! What mpv does name for every frame is when it should be shown
//! (`mpv_render_frame_info.target_time`). In the run, position and target
//! time move together: their difference is one number, and a position that
//! is that of a later frame stands out above it. A [`Stamper`] keeps the
//! lowest difference of the last frames and gives a frame that is above it
//! the time its target time says. A frame that was read before the next one
//! can have been handed out is never reckoned: when the run has shifted
//! (another sound track, mpv catching up with its sound), such a frame is
//! what it is learned anew from.
//!
//! Limits: a reckoned time is the time of the frame within what the target
//! times of mpv are uneven by (measured: under 0.1 ms), not the number the
//! file has. And the first frame after a start, a jump or a pause has
//! nothing to be held against: it carries what mpv says, which on a busy
//! machine may be the time of a later frame.

use std::collections::VecDeque;

/// How many frames the lowest difference is taken from. Long enough that a
/// few late frames in a row leave it standing, short enough to follow the
/// drift between the clock of mpv and the time of the file.
const WINDOW: usize = 32;

/// A position this far above the lowest difference is that of a later
/// frame, in microseconds. Above what the target times are uneven by, below
/// the distance of two frames at 120 frames a second.
const SLACK: i64 = 3_000;

/// A frame read this long after mpv handed it out, in microseconds, was
/// read in time whatever the file: the next frame is handed out one frame
/// later, and at 120 frames a second that is 8.3 ms.
const IN_TIME: i64 = 5_000;

/// A late read is that of one of the next frames, not of one far away:
/// further off than this, in microseconds, is no late read but a jump
/// nobody told ([`Stamper::forget`]), and the position stands.
const REACH: i64 = 250_000;

/// What the render thread knows about a frame it has rendered.
#[derive(Clone, Copy, Debug)]
pub struct Reading {
    /// The frame before, drawn once more (a redraw, a repeat).
    pub again: bool,
    /// When mpv wants it shown, in the unit of mpv: microseconds by
    /// `render.h`, nanoseconds in libmpv 0.37 (measured).
    pub target: i64,
    /// The clock of mpv when the frame was rendered, in microseconds
    /// (`mpv_get_time_us`): what the unit of the target time is told from.
    pub now: i64,
    /// The position of the core, read after the render, in microseconds.
    pub position: i64,
    /// The clock of mpv when the position had been read, in microseconds.
    pub read_at: i64,
    /// How long before its target time mpv hands a frame out, in
    /// microseconds (`video-timing-offset`).
    pub lead: i64,
}

/// Gives every rendered frame its time. One per render thread.
#[derive(Debug, Default)]
pub struct Stamper {
    /// Position minus target time, of the last frames that were in time.
    differences: VecDeque<i64>,
    reckoned: u64,
}

impl Stamper {
    /// The time of the frame in microseconds.
    pub fn stamp(&mut self, reading: Reading) -> i64 {
        if reading.again {
            // A redraw or a repeat has no target time of its own, and the
            // core stands where the frame is (measured at the start of a
            // file and at its end): the position, and nothing is learned.
            return reading.position;
        }
        self.checked(reading)
    }

    /// Start, jump, pause: position and target time no longer move
    /// together, and what was learned goes.
    pub fn forget(&mut self) {
        self.differences.clear();
    }

    /// How many frames got a reckoned time instead of the position of mpv.
    pub fn reckoned(&self) -> u64 {
        self.reckoned
    }

    fn checked(&mut self, reading: Reading) -> i64 {
        let Some(target) = target_micros(reading) else {
            return reading.position;
        };
        let difference = reading.position.saturating_sub(target);
        let Some(lowest) = self.differences.iter().copied().min() else {
            self.keep(difference);
            return reading.position;
        };
        let above = difference.saturating_sub(lowest);
        // Read before the next frame can have been handed out: the position
        // is that of this frame, whatever the frames before say. That is how
        // a run that has shifted (another sound track, mpv catching up with
        // its sound) is learned anew instead of being reckoned against for
        // good.
        let handed_out = target.saturating_sub(reading.lead);
        let in_time = reading.read_at.saturating_sub(handed_out) < IN_TIME;
        if above > SLACK && above <= REACH && !in_time {
            // The position of a later frame: this one is where its target
            // time says.
            self.reckoned += 1;
            return target.saturating_add(lowest);
        }
        if !(-SLACK..=SLACK).contains(&above) {
            // Far below, or far above: not the run that was learned.
            self.differences.clear();
        }
        self.keep(difference);
        reading.position
    }

    fn keep(&mut self, difference: i64) {
        if self.differences.len() == WINDOW {
            self.differences.pop_front();
        }
        self.differences.push_back(difference);
    }
}

/// The target time in microseconds, `None` when mpv names none. Its unit is
/// told from the clock of mpv, which is in microseconds: a frame is due
/// within seconds of now, and the two units are a factor of a thousand
/// apart.
fn target_micros(reading: Reading) -> Option<i64> {
    if reading.target <= 0 || reading.now <= 0 {
        return None;
    }
    let unit = if reading.target / reading.now >= 30 {
        1_000
    } else {
        1
    };
    Some(reading.target / unit)
}

#[cfg(test)]
mod tests {
    use super::{Reading, Stamper};

    /// 25 frames a second, target times in nanoseconds as libmpv 0.37 has
    /// them, a little uneven as mpv makes them.
    fn frame(number: i64) -> Reading {
        Reading {
            again: false,
            target: 1_000_000_000 + number * 40_000_000 + (number % 3) * 30_000,
            // Handed out and rendered 20 ms ahead of its time.
            now: 980_000 + number * 40_000,
            position: number * 40_000,
            read_at: 980_500 + number * 40_000,
            lead: 20_000,
        }
    }

    /// Frame `number`, read when the core was at `read_as` already.
    fn late(number: i64, read_as: i64) -> Reading {
        Reading {
            position: read_as * 40_000,
            // Not before the frame it was read as was handed out.
            read_at: 980_500 + read_as * 40_000,
            ..frame(number)
        }
    }

    /// Which frame a time is that of.
    fn number(micros: i64) -> i64 {
        (micros + 20_000) / 40_000
    }

    #[test]
    fn a_frame_in_time_carries_the_position_of_mpv() {
        let mut stamper = Stamper::default();
        for number in 0..100 {
            assert_eq!(stamper.stamp(frame(number)), number * 40_000);
        }
        assert_eq!(stamper.reckoned(), 0);
    }

    #[test]
    fn a_late_frame_gets_the_time_of_its_own() {
        let mut stamper = Stamper::default();
        for number in 0..8 {
            stamper.stamp(frame(number));
        }
        // Frame 8 read as 9, frame 9 read as 11 (what the trace showed).
        let eighth = stamper.stamp(late(8, 9));
        assert_eq!(number(eighth), 8);
        // Within what the target times are uneven by, here 60 us.
        assert!((eighth - 320_000).abs() <= 60, "{eighth}");
        assert_eq!(number(stamper.stamp(late(9, 11))), 9);
        assert_eq!(stamper.stamp(frame(10)), 400_000);
        assert_eq!(stamper.reckoned(), 2);
    }

    #[test]
    fn microseconds_are_told_from_nanoseconds() {
        let mut stamper = Stamper::default();
        let micros = |number: i64| Reading {
            target: frame(number).target / 1_000,
            ..frame(number)
        };
        for number in 0..8 {
            stamper.stamp(micros(number));
        }
        let read_late = Reading {
            position: 9 * 40_000,
            read_at: 980_500 + 9 * 40_000,
            ..micros(8)
        };
        assert_eq!(number(stamper.stamp(read_late)), 8);
        assert_eq!(stamper.reckoned(), 1);
        // Without a target time (mpv names none) the position stands.
        let none = Reading {
            target: 0,
            ..late(9, 12)
        };
        assert_eq!(stamper.stamp(none), 480_000);
    }

    #[test]
    fn frames_mpv_left_out_are_no_late_ones() {
        let mut stamper = Stamper::default();
        for number in 0..12 {
            stamper.stamp(frame(number));
        }
        // 12 and 14 were never rendered.
        assert_eq!(stamper.stamp(frame(13)), 520_000);
        assert_eq!(number(stamper.stamp(late(15, 16))), 15);
        assert_eq!(stamper.reckoned(), 1);
    }

    #[test]
    fn a_run_that_has_shifted_is_learned_anew_from_a_frame_in_time() {
        let mut stamper = Stamper::default();
        for number in 0..8 {
            stamper.stamp(frame(number));
        }
        // From frame 8 on mpv shows everything 10 ms earlier against the
        // file (it follows another sound track): the position is 10 ms
        // further on for the same target time, read in time.
        let shifted = |number: i64| Reading {
            position: number * 40_000 + 10_000,
            ..frame(number)
        };
        for number in 8..12 {
            assert_eq!(stamper.stamp(shifted(number)), number * 40_000 + 10_000);
        }
        assert_eq!(stamper.reckoned(), 0);
        // A late read is told against the run as it is now.
        let read_late = Reading {
            position: 13 * 40_000 + 10_000,
            read_at: 980_500 + 13 * 40_000,
            ..frame(12)
        };
        let twelfth = stamper.stamp(read_late);
        assert!((twelfth - 490_000).abs() <= 60, "{twelfth}");
    }

    #[test]
    fn many_late_frames_in_a_row_stay_right() {
        let mut stamper = Stamper::default();
        for number in 0..4 {
            stamper.stamp(frame(number));
        }
        for at in 4..60 {
            assert_eq!(number(stamper.stamp(late(at, at + 1))), at);
        }
    }

    #[test]
    fn a_redraw_carries_the_position_and_teaches_nothing() {
        let mut stamper = Stamper::default();
        for number in 0..4 {
            stamper.stamp(frame(number));
        }
        // Drawn again much later, by a target time that says nothing.
        let again = Reading {
            again: true,
            target: frame(40).target,
            ..frame(3)
        };
        assert_eq!(stamper.stamp(again), 120_000);
        // The run goes on as it was learned.
        assert_eq!(stamper.stamp(frame(4)), 160_000);
        assert_eq!(number(stamper.stamp(late(5, 6))), 5);
        assert_eq!(stamper.reckoned(), 1);
    }

    #[test]
    fn the_first_frames_carry_what_mpv_says() {
        // Nothing to hold it against: frame 0 read as 2 stays 2.
        let mut stamper = Stamper::default();
        assert_eq!(stamper.stamp(late(0, 2)), 80_000);
        // The frames in time after it are not taken for a jump back for
        // good: from the third on, a late one is seen.
        assert_eq!(stamper.stamp(frame(1)), 40_000);
        assert_eq!(stamper.stamp(frame(2)), 80_000);
        assert_eq!(number(stamper.stamp(late(3, 4))), 3);
    }

    #[test]
    fn after_a_jump_the_position_stands_and_the_run_is_learned_anew() {
        let mut stamper = Stamper::default();
        for number in 0..5 {
            stamper.stamp(frame(number));
        }
        // Told: ten seconds ahead, shown right after frame 4.
        stamper.forget();
        let at = |shown: i64, number: i64| Reading {
            target: frame(shown).target,
            ..frame(number)
        };
        assert_eq!(stamper.stamp(at(5, 250)), 10_000_000);
        assert_eq!(stamper.stamp(at(6, 251)), 10_040_000);
        let read_late = Reading {
            position: 253 * 40_000,
            ..at(7, 252)
        };
        assert_eq!(number(stamper.stamp(read_late)), 252);

        // Not told (mpv went to the next file on its own): back to zero,
        // and half a minute ahead. The position stands both times.
        assert_eq!(stamper.stamp(at(8, 0)), 0);
        assert_eq!(stamper.stamp(at(9, 750)), 30_000_000);
        assert_eq!(stamper.reckoned(), 1);
    }

    #[test]
    fn on_the_way_to_a_jump_the_target_of_the_jump_stands() {
        let mut stamper = Stamper::default();
        for number in 0..5 {
            stamper.stamp(frame(number));
        }
        stamper.forget();
        // mpv names the target of the jump while it is on its way.
        for shown in 5..9 {
            let on_the_way = Reading {
                position: 6_500_000,
                ..frame(shown)
            };
            assert_eq!(stamper.stamp(on_the_way), 6_500_000);
        }
        // Arrived at the keyframe before it.
        let arrived = Reading {
            position: 4_000_000,
            ..frame(9)
        };
        assert_eq!(stamper.stamp(arrived), 4_000_000);
        assert_eq!(stamper.reckoned(), 0);
    }
}
