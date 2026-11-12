// SPDX-License-Identifier: Apache-2.0 OR MIT
//! What the audio callback does, without a device.
//!
//! [`OutputCallback::fill`] is the whole body of the device callback: take
//! frames from the [`Consumer`], pad with silence when the ring runs dry (and
//! count that as an xrun), report the position to the audio master clock. The
//! device layer ([`crate::output`], feature `cpal`) only hands it the buffer of
//! the device; everything that can go wrong in real time is here and is
//! tested without a sound card.
//!
//! # The position
//!
//! The clock is told every frame handed to the device, silence after an
//! underrun included: the device plays on through a dropout, and the clock
//! must stay monotonic. Reporting only the frames from the ring would hold the
//! anchor still while the clock extrapolates up to its limit between two
//! callbacks, so the position would step back at every callback of a dropout.
//! How much was silence is in [`OutputStats`].
//!
//! The time of a report is the monotonic clock at the start of the callback.
//! Without a known device latency the position is the write head: all frames
//! handed over so far, this buffer included.
//!
//! # The device latency
//!
//! The device layer tells [`OutputCallback::set_latency`] before each fill how
//! many frames lie between the callback and the moment the first frame of
//! this buffer reaches the output (`cpal`: `playback - callback` of the
//! `OutputCallbackInfo`). The clock is then told the frame at the output
//! instead: the frames handed over before this buffer, minus that latency.
//! Video follows that position, so the picture waits for the sound.
//!
//! Until the first frame reached the output no position is reported (the
//! clock stays at zero), and a latency that grows holds the position instead
//! of stepping it back: the clock stays monotonic.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use takt_clock::{AudioClockWriter, MonotonicSource};

use crate::histogram::{DurationHistogram, DurationSnapshot};
use crate::ring::Consumer;
use crate::sample::{DeviceSample, Dither};

/// Samples of the stack buffer [`OutputCallback::fill_as`] converts through:
/// 4 KiB, a device buffer larger than that is converted in parts.
const SCRATCH_SAMPLES: usize = 1_024;

/// Counters of an output, readable from any thread while it runs.
#[derive(Debug, Default)]
pub struct OutputStats {
    callbacks: AtomicU64,
    frames: AtomicU64,
    xruns: AtomicU64,
    silent_frames: AtomicU64,
    /// Monotonic time of the last xrun plus one; 0 = never.
    last_xrun: AtomicU64,
    /// Device latency of the last callback in frames plus one; 0 = unknown.
    latency: AtomicU64,
    runtime: DurationHistogram,
}

impl OutputStats {
    /// Callbacks so far.
    #[must_use]
    pub fn callbacks(&self) -> u64 {
        self.callbacks.load(Ordering::Relaxed)
    }

    /// Frames taken from the ring and handed to the device so far (without
    /// the silence of [`silent_frames`](Self::silent_frames)).
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Callbacks that found too little in the ring and padded with silence.
    #[must_use]
    pub fn xruns(&self) -> u64 {
        self.xruns.load(Ordering::Relaxed)
    }

    /// Monotonic time (of the source the callback was given) of the start of
    /// the last callback with an xrun, or `None` before the first.
    #[must_use]
    pub fn last_xrun_nanos(&self) -> Option<u64> {
        self.last_xrun.load(Ordering::Relaxed).checked_sub(1)
    }

    /// The runtime of the callbacks so far, from the start of the callback to
    /// the end of its work (`KONZEPT.md`: the 99.99th percentile decides).
    #[must_use]
    pub fn runtime(&self) -> DurationSnapshot {
        self.runtime.snapshot()
    }

    /// The device latency of the last callback in frames, as the device layer
    /// reported it, or `None` if it reports none.
    #[must_use]
    pub fn latency_frames(&self) -> Option<u64> {
        self.latency.load(Ordering::Relaxed).checked_sub(1)
    }

    /// Frames of silence handed out because the ring was empty.
    #[must_use]
    pub fn silent_frames(&self) -> u64 {
        self.silent_frames.load(Ordering::Relaxed)
    }
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The body of the audio callback.
#[derive(Debug)]
pub struct OutputCallback {
    ring: Consumer,
    clock: AudioClockWriter,
    time: Arc<dyn MonotonicSource>,
    stats: Arc<OutputStats>,
    played: u64,
    dither: Dither,
    latency: Option<u64>,
    /// The last position reported with a latency, so it never steps back.
    heard: u64,
}

impl OutputCallback {
    /// A callback that plays from `ring` and reports to `clock`, with the
    /// times of `time` (the source the clock itself reads).
    #[must_use]
    pub fn new(ring: Consumer, clock: AudioClockWriter, time: Arc<dyn MonotonicSource>) -> Self {
        Self {
            ring,
            clock,
            time,
            stats: Arc::new(OutputStats::default()),
            played: 0,
            dither: Dither::default(),
            latency: None,
            heard: 0,
        }
    }

    /// The counters, to be read while the callback runs elsewhere.
    #[must_use]
    pub fn stats(&self) -> Arc<OutputStats> {
        Arc::clone(&self.stats)
    }

    /// The device latency for the next fill, in frames from the start of the
    /// callback to the output of the first frame of its buffer.
    ///
    /// **Callable from the audio thread**: a plain store.
    pub fn set_latency(&mut self, frames: u64) {
        self.latency = Some(frames);
    }

    /// Samples per frame, as the ring carries them.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.ring.channels()
    }

    /// Fills the interleaved device buffer `out`.
    ///
    /// **The audio callback.** Runs under `assert_no_alloc`: an allocation
    /// in here aborts the process when the program installs
    /// `assert_no_alloc::AllocDisabler` as its global allocator (takt-player
    /// does, in debug and in release). Does not lock or wait.
    pub fn fill(&mut self, out: &mut [f32]) {
        assert_no_alloc::assert_no_alloc(|| self.fill_inner(out));
    }

    /// Fills the interleaved device buffer `out` of a device that takes no
    /// `f32`: the frames of the ring are converted (`i16` with dither, see
    /// [`crate::sample`]) through a buffer on the stack.
    ///
    /// **The audio callback**, under `assert_no_alloc` like [`fill`](Self::fill).
    pub fn fill_as<S: DeviceSample>(&mut self, out: &mut [S]) {
        assert_no_alloc::assert_no_alloc(|| self.fill_as_inner(out));
    }

    fn fill_inner(&mut self, out: &mut [f32]) {
        let now = self.time.now_nanos();
        let channels = self.ring.channels();
        let frames = self.ring.pop(out);
        let wanted = out.len() / channels;
        out[frames * channels..].fill(0.0);
        self.finish(now, frames, wanted);
    }

    fn fill_as_inner<S: DeviceSample>(&mut self, out: &mut [S]) {
        let now = self.time.now_nanos();
        let channels = self.ring.channels();
        let wanted = out.len() / channels;
        // Whole frames per part; a frame wider than the stack buffer gets silence.
        let part = SCRATCH_SAMPLES / channels * channels;
        let mut frames = 0;
        if part > 0 {
            let mut scratch = [0.0_f32; SCRATCH_SAMPLES];
            for chunk in out[..wanted * channels].chunks_mut(part) {
                let got = self.ring.pop(&mut scratch[..chunk.len()]) * channels;
                for (o, &x) in chunk.iter_mut().zip(&scratch[..got]) {
                    *o = S::from_f32(x, &mut self.dither);
                }
                frames += got / channels;
                if got < chunk.len() {
                    break;
                }
            }
        }
        out[frames * channels..].fill(S::SILENCE);
        self.finish(now, frames, wanted);
    }

    /// The clock and the counters, once per callback: `frames` came from the
    /// ring, `wanted` were handed to the device.
    fn finish(&mut self, now: u64, frames: usize, wanted: usize) {
        let before = self.played;
        self.played += to_u64(wanted);
        let stats = &*self.stats;
        match self.latency {
            None => self.clock.report(self.played, now),
            Some(latency) => {
                stats
                    .latency
                    .store(latency.saturating_add(1), Ordering::Relaxed);
                if let Some(heard) = before.checked_sub(latency) {
                    self.heard = self.heard.max(heard);
                    self.clock.report(self.heard, now);
                }
            }
        }

        stats.callbacks.fetch_add(1, Ordering::Relaxed);
        stats.frames.fetch_add(to_u64(frames), Ordering::Relaxed);
        if frames < wanted {
            stats.xruns.fetch_add(1, Ordering::Relaxed);
            stats
                .last_xrun
                .store(now.saturating_add(1), Ordering::Relaxed);
            stats
                .silent_frames
                .fetch_add(to_u64(wanted - frames), Ordering::Relaxed);
        }
        stats
            .runtime
            .record(self.time.now_nanos().saturating_sub(now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ring;
    use takt_clock::{AudioMasterClock, ManualMonotonic};

    fn setup(
        capacity: usize,
    ) -> (
        ring::Producer,
        OutputCallback,
        AudioMasterClock,
        Arc<ManualMonotonic>,
    ) {
        let time = Arc::new(ManualMonotonic::new());
        let clock = AudioMasterClock::new(
            48_000,
            Arc::clone(&time) as Arc<dyn MonotonicSource>,
            20_000_000,
        );
        let (tx, rx) = ring::ring(2, capacity);
        let cb = OutputCallback::new(
            rx,
            clock.writer().unwrap(),
            Arc::clone(&time) as Arc<dyn MonotonicSource>,
        );
        (tx, cb, clock, time)
    }

    #[test]
    fn a_full_ring_plays_and_moves_the_clock() {
        let (mut tx, mut cb, clock, time) = setup(960);
        tx.push(&[0.5; 960 * 2]);
        let mut out = [9.0_f32; 480 * 2];
        time.set(10_000_000);
        cb.fill(&mut out);
        assert!(out.iter().all(|&s| s == 0.5));
        assert_eq!(clock.anchored_position().unwrap().ticks(), 480);
        let stats = cb.stats();
        assert_eq!(
            (stats.callbacks(), stats.frames(), stats.xruns()),
            (1, 480, 0)
        );
        time.set(20_000_000);
        cb.fill(&mut out);
        assert_eq!(clock.anchored_position().unwrap().ticks(), 960);
        assert_eq!(stats.last_xrun_nanos(), None, "a full ring has no xrun");
        assert_eq!(stats.runtime().total(), 2, "every callback is timed");
    }

    #[test]
    fn an_empty_ring_gives_silence_counts_an_xrun_and_the_clock_runs_on() {
        let (mut tx, mut cb, clock, time) = setup(960);
        tx.push(&[0.5; 100 * 2]);
        let mut out = [9.0_f32; 480 * 2];
        cb.fill(&mut out);
        assert!(out[..200].iter().all(|&s| s == 0.5));
        assert!(out[200..].iter().all(|&s| s == 0.0), "the rest is silence");
        let stats = cb.stats();
        assert_eq!((stats.xruns(), stats.silent_frames()), (1, 380));

        time.set(10_000_000);
        cb.fill(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!((stats.xruns(), stats.silent_frames()), (2, 860));
        assert_eq!(stats.last_xrun_nanos(), Some(10_000_000));
        // The device played two buffers, 100 frames of content and 860 of
        // silence: the clock follows the device and never steps back.
        assert_eq!(clock.anchored_position().unwrap().ticks(), 960);
        assert_eq!(stats.frames(), 100);
    }

    #[test]
    fn an_i16_device_gets_converted_frames_in_parts_then_silence() {
        // 1500 frames in the ring, a buffer of 2048: three parts of 512, the
        // third short; the rest silence.
        let (mut tx, mut cb, clock, _) = setup(4_096);
        tx.push(&[0.5; 1_500 * 2]);
        let mut out = [7_i16; 2_048 * 2];
        cb.fill_as(&mut out);
        let half = i16::MAX / 2;
        assert!(
            out[..3_000]
                .iter()
                .all(|&s| (half - 1..=half + 1).contains(&s)),
            "converted with at most one LSB of dither"
        );
        assert!(out[3_000..].iter().all(|&s| s == 0), "the rest is silence");
        let stats = cb.stats();
        assert_eq!(
            (stats.frames(), stats.xruns(), stats.silent_frames()),
            (1_500, 1, 548)
        );
        assert_eq!(clock.anchored_position().unwrap().ticks(), 2_048);
    }

    #[test]
    fn an_i32_device_gets_the_full_buffer() {
        let (mut tx, mut cb, _, _) = setup(960);
        tx.push(&[-1.0; 480 * 2]);
        let mut out = [7_i32; 480 * 2];
        cb.fill_as(&mut out);
        assert!(out.iter().all(|&s| s == -i32::MAX));
        assert_eq!(cb.stats().xruns(), 0);
    }

    #[test]
    fn with_a_latency_the_clock_reports_the_frame_at_the_output() {
        let (mut tx, mut cb, clock, time) = setup(9_600);
        tx.push(&[0.5; 4_800 * 2]);
        let mut out = [0.0_f32; 480 * 2];
        // 960 frames (20 ms) between the callback and the output: the first
        // two buffers are not heard yet, the clock has no position.
        for step in 1..=2 {
            cb.set_latency(960);
            time.set(step * 10_000_000);
            cb.fill(&mut out);
            assert_eq!(clock.anchored_position(), None, "callback {step}");
        }
        cb.set_latency(960);
        time.set(30_000_000);
        cb.fill(&mut out);
        assert_eq!(clock.anchored_position().unwrap().ticks(), 0);
        cb.set_latency(960);
        cb.fill(&mut out);
        assert_eq!(clock.anchored_position().unwrap().ticks(), 480);
        assert_eq!(cb.stats().latency_frames(), Some(960));
        // A latency that grows holds the position instead of stepping back.
        cb.set_latency(2_000);
        cb.fill(&mut out);
        assert_eq!(clock.anchored_position().unwrap().ticks(), 480);
        cb.set_latency(2_000);
        cb.fill_as(&mut [0_i16; 480 * 2]);
        assert_eq!(clock.anchored_position().unwrap().ticks(), 480);
        cb.set_latency(960);
        cb.fill(&mut out);
        // 6 buffers handed over before this one, 2880 frames, minus 960.
        assert_eq!(clock.anchored_position().unwrap().ticks(), 1_920);
    }

    #[test]
    fn without_a_latency_the_latency_is_unknown() {
        let (_, mut cb, _, _) = setup(960);
        cb.fill(&mut [0.0_f32; 480 * 2]);
        assert_eq!(cb.stats().latency_frames(), None);
    }
}
