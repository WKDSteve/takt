// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A wait-free SPSC ring for interleaved `f32` frames.
//!
//! # What it promises
//!
//! * **Allocation only in [`ring`].** [`Producer::push`] and [`Consumer::pop`]
//!   never allocate, never lock and never wait; each is a bounded number of
//!   atomic operations plus one copy.
//! * **Fixed capacity.** A push that does not fit writes what fits and drops
//!   the rest. The dropped frames are counted ([`Producer::overflowed`]), the
//!   producer is never blocked.
//! * **Whole frames.** Both sides move whole frames (one sample per channel),
//!   so a channel never slips against the others.
//! * **One producer, one consumer.** [`ring`] hands out each side exactly once
//!   and neither side is `Clone`; the counters below rely on it.
//!
//! # Why no `unsafe`
//!
//! The slots are `AtomicU32` holding the bits of an `f32`. A relaxed atomic
//! store or load compiles to a plain move on x86-64 and ARM64, so this costs
//! nothing in the loop, and even a broken protocol could never be a data race
//! in the sense of the language. The crate stays `unsafe`-free, as the rest of
//! zone 1.
//!
//! # The orderings
//!
//! `head` is the number of frames written (owned by the producer), `tail` the
//! number read (owned by the consumer). The producer only writes the slots in
//! `[head, tail + capacity)`, the consumer only reads `[tail, head)`: no slot is
//! ever touched by both sides at once, which is the difference to the seqlock
//! in `takt-clock` (there the reader reads what the writer may be overwriting,
//! and Release/Acquire on single operations was not enough on ARM64).
//!
//! * The producer stores its slots, then `head` with `Release`. The consumer
//!   loads `head` with `Acquire`: every slot below it is visible.
//! * The consumer reads its slots, then stores `tail` with `Release`. The
//!   producer loads `tail` with `Acquire`: every read below it is done, so an
//!   overwrite can never be seen by a read of the old frame.
//!
//! Each side keeps the last seen value of the other side's counter and only
//! loads it again when that value says the ring is full (producer) or empty
//! (consumer), so the shared cache line moves only when it has to.
//!
//! # Counter range
//!
//! Both counters run in `[0, 2 * capacity)` instead of growing freely: a free
//! `usize` would wrap after 2^32 samples on a 32-bit target (12 hours of stereo
//! at 48 kHz), and the index arithmetic after a wrap is only right for a power
//! of two capacity. With the doubled range, full (`head - tail == capacity`)
//! and empty (`head == tail`) stay distinct for any capacity.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Keeps each counter on its own cache line (128 bytes: Apple Silicon fetches
/// pairs of 64-byte lines), so the two sides do not slow each other down.
#[derive(Debug)]
#[repr(align(128))]
struct Padded<T>(T);

#[derive(Debug)]
struct Shared {
    slots: Box<[AtomicU32]>,
    channels: usize,
    capacity: usize,
    head: Padded<AtomicUsize>,
    tail: Padded<AtomicUsize>,
    overflowed: Padded<AtomicU64>,
}

impl Shared {
    /// Frames between `tail` and `head`, both in `[0, 2 * capacity)`.
    fn len(&self, head: usize, tail: usize) -> usize {
        if head >= tail {
            head - tail
        } else {
            head + 2 * self.capacity - tail
        }
    }

    /// `pos` moved on by `frames` (at most `capacity`), back in range.
    fn advance(&self, pos: usize, frames: usize) -> usize {
        let next = pos + frames;
        if next >= 2 * self.capacity {
            next - 2 * self.capacity
        } else {
            next
        }
    }

    /// The two runs of slots that hold `frames` frames from frame `pos` on:
    /// the part up to the end of the ring, then the part from its start.
    fn runs(&self, pos: usize, frames: usize) -> (&[AtomicU32], &[AtomicU32]) {
        let start = (pos % self.capacity) * self.channels;
        let samples = frames * self.channels;
        let first = samples.min(self.slots.len() - start);
        (
            &self.slots[start..start + first],
            &self.slots[..samples - first],
        )
    }
}

/// Creates a ring for `capacity_frames` frames of `channels` interleaved
/// samples each.
///
/// The only allocation of the ring happens here.
///
/// # Panics
///
/// If `channels` or `capacity_frames` is zero, or the ring would not fit the
/// address space (`capacity_frames * channels` above `usize::MAX / 4`).
#[must_use]
pub fn ring(channels: usize, capacity_frames: usize) -> (Producer, Consumer) {
    assert!(channels > 0, "a ring needs at least one channel");
    assert!(
        capacity_frames > 0,
        "a ring needs room for at least one frame"
    );
    let samples = capacity_frames
        .checked_mul(channels)
        .filter(|&n| n <= usize::MAX / 4)
        .expect("ring larger than the address space");
    let shared = Arc::new(Shared {
        slots: (0..samples).map(|_| AtomicU32::new(0)).collect(),
        channels,
        capacity: capacity_frames,
        head: Padded(AtomicUsize::new(0)),
        tail: Padded(AtomicUsize::new(0)),
        overflowed: Padded(AtomicU64::new(0)),
    });
    (
        Producer {
            shared: Arc::clone(&shared),
            head: 0,
            cached_tail: 0,
        },
        Consumer {
            shared,
            tail: 0,
            cached_head: 0,
        },
    )
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The writing side of a [`ring`].
#[derive(Debug)]
pub struct Producer {
    shared: Arc<Shared>,
    head: usize,
    cached_tail: usize,
}

impl Producer {
    /// Writes the whole frames of `interleaved` that fit and returns how many
    /// frames that were. The frames that did not fit are dropped and counted
    /// in [`overflowed`](Self::overflowed).
    ///
    /// Samples after the last whole frame are ignored (a caller error that a
    /// debug build reports).
    ///
    /// **Callable from the audio thread.** Does not allocate, lock or wait.
    pub fn push(&mut self, interleaved: &[f32]) -> usize {
        let shared = &*self.shared;
        debug_assert_eq!(
            interleaved.len() % shared.channels,
            0,
            "push takes whole frames"
        );
        let frames = interleaved.len() / shared.channels;
        let mut free = shared.capacity - shared.len(self.head, self.cached_tail);
        if free < frames {
            self.cached_tail = shared.tail.0.load(Ordering::Acquire);
            free = shared.capacity - shared.len(self.head, self.cached_tail);
        }
        let n = frames.min(free);
        let (first, second) = shared.runs(self.head, n);
        for (slot, sample) in first.iter().chain(second).zip(interleaved) {
            slot.store(sample.to_bits(), Ordering::Relaxed);
        }
        self.head = shared.advance(self.head, n);
        shared.head.0.store(self.head, Ordering::Release);
        if n < frames {
            shared
                .overflowed
                .0
                .fetch_add(to_u64(frames - n), Ordering::Relaxed);
        }
        n
    }

    /// Frames that fit right now. The consumer may free more at any moment.
    #[must_use]
    pub fn free_frames(&self) -> usize {
        let tail = self.shared.tail.0.load(Ordering::Acquire);
        self.shared.capacity - self.shared.len(self.head, tail)
    }

    /// Frames dropped so far because the ring was full.
    #[must_use]
    pub fn overflowed(&self) -> u64 {
        self.shared.overflowed.0.load(Ordering::Relaxed)
    }

    /// Samples per frame.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.shared.channels
    }

    /// Frames the ring holds when full.
    #[must_use]
    pub fn capacity_frames(&self) -> usize {
        self.shared.capacity
    }
}

/// The reading side of a [`ring`].
#[derive(Debug)]
pub struct Consumer {
    shared: Arc<Shared>,
    tail: usize,
    cached_head: usize,
}

impl Consumer {
    /// Reads as many whole frames as are there and fit `out`, and returns how
    /// many frames that were. The rest of `out` is left as it was.
    ///
    /// **Callable from the audio thread.** Does not allocate, lock or wait.
    pub fn pop(&mut self, out: &mut [f32]) -> usize {
        let shared = &*self.shared;
        let wanted = out.len() / shared.channels;
        let mut there = shared.len(self.cached_head, self.tail);
        if there < wanted {
            self.cached_head = shared.head.0.load(Ordering::Acquire);
            there = shared.len(self.cached_head, self.tail);
        }
        let n = wanted.min(there);
        let (first, second) = shared.runs(self.tail, n);
        for (sample, slot) in out.iter_mut().zip(first.iter().chain(second)) {
            *sample = f32::from_bits(slot.load(Ordering::Relaxed));
        }
        self.tail = shared.advance(self.tail, n);
        shared.tail.0.store(self.tail, Ordering::Release);
        n
    }

    /// Frames that can be read right now. The producer may add more at any
    /// moment.
    #[must_use]
    pub fn available_frames(&self) -> usize {
        let head = self.shared.head.0.load(Ordering::Acquire);
        self.shared.len(head, self.tail)
    }

    /// Frames the producer dropped so far because the ring was full.
    #[must_use]
    pub fn overflowed(&self) -> u64 {
        self.shared.overflowed.0.load(Ordering::Relaxed)
    }

    /// Samples per frame.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.shared.channels
    }

    /// Frames the ring holds when full.
    #[must_use]
    pub fn capacity_frames(&self) -> usize {
        self.shared.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` as an `f32`, exact because it stays below 2^24.
    #[expect(clippy::cast_precision_loss, reason = "checked to be exact")]
    fn exact(n: usize) -> f32 {
        assert!(n < 1 << 24, "{n} is not exact in f32");
        n as f32
    }

    #[test]
    fn frames_come_out_in_the_order_they_went_in() {
        let (mut tx, mut rx) = ring(2, 4);
        assert_eq!(tx.push(&[1.0, -1.0, 2.0, -2.0]), 2);
        assert_eq!(rx.available_frames(), 2);
        let mut out = [0.0; 6];
        assert_eq!(rx.pop(&mut out), 2);
        assert_eq!(out, [1.0, -1.0, 2.0, -2.0, 0.0, 0.0]);
        assert_eq!(rx.pop(&mut out), 0, "an empty ring gives nothing");
    }

    #[test]
    fn a_full_ring_drops_and_counts_and_never_blocks() {
        let (mut tx, mut rx) = ring(1, 3);
        assert_eq!(tx.push(&[1.0, 2.0]), 2);
        assert_eq!(tx.push(&[3.0, 4.0, 5.0]), 1);
        assert_eq!(tx.overflowed(), 2);
        assert_eq!(tx.push(&[6.0]), 0);
        assert_eq!(rx.overflowed(), 3);
        assert_eq!(tx.free_frames(), 0);
        let mut out = [0.0; 3];
        assert_eq!(rx.pop(&mut out), 3);
        assert_eq!(
            out,
            [1.0, 2.0, 3.0],
            "the oldest frames stay, the newest are dropped"
        );
        assert_eq!(tx.free_frames(), 3);
    }

    #[test]
    fn the_ring_wraps_for_a_capacity_that_is_not_a_power_of_two() {
        // Capacity 3 frames of 2 channels: the counters run through [0, 6) many
        // times, and each push crosses the end of the slots at another place.
        let (mut tx, mut rx) = ring(2, 3);
        let mut next = 0.0_f32;
        let mut expected = 0.0_f32;
        for round in 0..100 {
            let frames = round % 3 + 1;
            let chunk: Vec<f32> = (0..frames * 2).map(|i| next + exact(i)).collect();
            assert_eq!(tx.push(&chunk), frames);
            next += exact(chunk.len());
            let mut out = vec![0.0; frames * 2];
            assert_eq!(rx.pop(&mut out), frames);
            for sample in out {
                assert_eq!(sample, expected);
                expected += 1.0;
            }
        }
        assert_eq!(tx.overflowed(), 0);
    }

    #[test]
    fn pop_takes_whole_frames_only() {
        let (mut tx, mut rx) = ring(2, 4);
        tx.push(&[1.0, 2.0, 3.0, 4.0]);
        let mut out = [0.0; 3];
        assert_eq!(rx.pop(&mut out), 1, "three samples hold one stereo frame");
        assert_eq!(out, [1.0, 2.0, 0.0]);
        assert_eq!(rx.available_frames(), 1);
    }

    #[test]
    fn a_sample_keeps_its_bits() {
        let (mut tx, mut rx) = ring(1, 4);
        let odd = [f32::NAN, -0.0, f32::INFINITY, f32::MIN_POSITIVE / 2.0];
        tx.push(&odd);
        let mut out = [0.0; 4];
        rx.pop(&mut out);
        for (a, b) in odd.iter().zip(out) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    #[should_panic(expected = "at least one channel")]
    fn zero_channels_is_refused() {
        let _ = ring(0, 4);
    }

    #[test]
    #[should_panic(expected = "at least one frame")]
    fn zero_capacity_is_refused() {
        let _ = ring(2, 0);
    }
}
