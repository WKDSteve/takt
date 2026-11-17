// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A wait-free SPSC ring of records in a [`Region`], one side per process.
//!
//! # Why not the ring of `takt-audio`
//!
//! That ring carries `f32` frames between two threads of one program: both sides trust each other
//! and share an `Arc`. This one lies in memory a second process writes, and that process parses
//! foreign files; it may be compromised. So the pattern is the same (one producer, one consumer,
//! counters with `Release`/`Acquire`, no lock, no wait), and the reading rules are new: each side
//! keeps its own counter to itself, reads the other side's counter as a claim, and checks every
//! claim against the size of the ring before it touches a byte.
//!
//! # Layout
//!
//! ```text
//! head of the region (words of 8 bytes)
//!   word  0  magic "TAKTRING"
//!   word  1  version of this layout
//!   word  2  capacity of the data area in bytes
//!   word 16  head: bytes written since the start (the writer stores it)
//!   word 32  tail: bytes read since the start (the reader stores it)
//!   word 48  heartbeat of the helper (crate::process)
//! data area: capacity bytes, a power of two
//!   record = len: u32 | check: u32 (len ^ CHECK) | payload | padding to 8 bytes
//! ```
//!
//! The counters run freely and never wrap in practice (2^64 bytes); the position in the data area
//! is the counter modulo the capacity. A record may wrap around the end of the data area; its head
//! never does (every record starts at a multiple of 8).
//!
//! # What a side may assume
//!
//! Nothing about the other side. The reader checks: the distance of the counters is at most the
//! capacity and a multiple of 8; the head of a record matches its check word; the record fits the
//! limit and the bytes the writer claims to have written. A failed check is a
//! [`ProtocolError::Ring`]; the owner of the helper ends the helper then. The bytes of a payload
//! are copied out before they are looked at, so a writer that changes them afterwards changes
//! nothing the reader already took.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::error::ProtocolError;
use crate::ffi::Region;
use crate::wire::checked_len;

const MAGIC: u64 = u64::from_le_bytes(*b"TAKTRING");
const LAYOUT_VERSION: u64 = 1;
const WORD_MAGIC: usize = 0;
const WORD_VERSION: usize = 1;
const WORD_CAPACITY: usize = 2;
/// The word of the region's head that holds the writer's counter. Public for tools that look at
/// a ring from outside (a fuzz harness, a debugger); the two ends never need it.
pub const WORD_HEAD: usize = 16;
/// The word of the region's head that holds the reader's counter.
pub const WORD_TAIL: usize = 32;
/// The word of the region's head that holds the heartbeat.
pub const WORD_BEAT: usize = 48;

/// Bytes in front of every payload.
pub const RECORD_HEAD: usize = 8;
const CHECK: u32 = 0x54_4B_52_31;

/// Smallest ring.
pub const MIN_CAPACITY: usize = 64;

/// The capacity of the ring a region carries.
///
/// # Errors
/// [`ProtocolError::Ring`] when the data area is no power of two of at least [`MIN_CAPACITY`] bytes.
pub fn capacity_of(region: &Region) -> Result<usize, ProtocolError> {
    let capacity = region.data_len();
    if capacity < MIN_CAPACITY || !capacity.is_power_of_two() {
        return Err(ProtocolError::Ring("capacity is no power of two"));
    }
    Ok(capacity)
}

/// Writes the head of a fresh ring. The side that created the memory calls this once, before the
/// other side exists.
///
/// # Errors
/// As [`capacity_of`].
pub fn format(region: &Region) -> Result<(), ProtocolError> {
    let capacity = capacity_of(region)?;
    region.word(WORD_MAGIC).store(MAGIC, Ordering::Relaxed);
    region
        .word(WORD_VERSION)
        .store(LAYOUT_VERSION, Ordering::Relaxed);
    region
        .word(WORD_CAPACITY)
        .store(capacity as u64, Ordering::Relaxed);
    region.word(WORD_HEAD).store(0, Ordering::Relaxed);
    region.word(WORD_TAIL).store(0, Ordering::Relaxed);
    region.word(WORD_BEAT).store(0, Ordering::Release);
    Ok(())
}

/// Checks the head of a ring the other side created (the helper checks what its parent set up).
///
/// # Errors
/// [`ProtocolError::WrongPeer`] for another magic number, [`ProtocolError::Ring`] for another layout
/// or a capacity that is not the size of the region.
pub fn check_format(region: &Region) -> Result<(), ProtocolError> {
    let capacity = capacity_of(region)?;
    if region.word(WORD_MAGIC).load(Ordering::Acquire) != MAGIC {
        return Err(ProtocolError::WrongPeer);
    }
    if region.word(WORD_VERSION).load(Ordering::Relaxed) != LAYOUT_VERSION {
        return Err(ProtocolError::Ring("layout version"));
    }
    if region.word(WORD_CAPACITY).load(Ordering::Relaxed) != capacity as u64 {
        return Err(ProtocolError::Ring("capacity differs from the region"));
    }
    Ok(())
}

/// Both ends of a ring in one region, for a ring inside one process (tests, a helper in a thread).
///
/// # Errors
/// As [`RingWriter::attach`].
pub fn pair(
    region: &Arc<Region>,
    max_record: usize,
) -> Result<(RingWriter, RingReader), ProtocolError> {
    format(region)?;
    Ok((
        RingWriter::attach(Arc::clone(region), max_record)?,
        RingReader::attach(Arc::clone(region), max_record)?,
    ))
}

fn padded(len: usize) -> usize {
    RECORD_HEAD + len.div_ceil(8) * 8
}

/// Limits `max_record` to what a ring of `capacity` bytes can hold at all.
fn record_limit(capacity: usize, max_record: usize) -> usize {
    max_record
        .min(capacity - RECORD_HEAD)
        .min(u32::MAX as usize - 8)
}

/// The writing end of a ring.
#[derive(Debug)]
pub struct RingWriter {
    region: Arc<Region>,
    capacity: usize,
    max_record: usize,
    /// Bytes written; only this side changes it, the shared word is its copy.
    head: u64,
    /// The reader's counter as last seen and checked.
    seen_tail: u64,
}

impl RingWriter {
    /// The writing end of the ring in `region`. Records above `max_record` bytes are refused (the
    /// limit is lowered to what the ring can hold).
    ///
    /// # Errors
    /// As [`capacity_of`].
    pub fn attach(region: Arc<Region>, max_record: usize) -> Result<Self, ProtocolError> {
        let capacity = capacity_of(&region)?;
        Ok(Self {
            max_record: record_limit(capacity, max_record),
            region,
            capacity,
            head: 0,
            seen_tail: 0,
        })
    }

    /// The largest payload this end writes.
    #[must_use]
    pub const fn max_record(&self) -> usize {
        self.max_record
    }

    /// Writes one record whose payload is `parts` one after the other. `Ok(false)` when the ring
    /// has no room for it right now: nothing was written, try again once the reader has read.
    ///
    /// Does not lock, wait or allocate.
    ///
    /// # Errors
    /// [`ProtocolError::TooLarge`] for a payload above the limit (it would never fit), or
    /// [`ProtocolError::Ring`] when the reader's counter is impossible.
    pub fn try_write(&mut self, parts: &[&[u8]]) -> Result<bool, ProtocolError> {
        let len = parts
            .iter()
            .try_fold(0usize, |sum, part| sum.checked_add(part.len()))
            .unwrap_or(usize::MAX);
        checked_len(len as u64, self.max_record)?;
        let total = padded(len);
        if self.free_after_seen() < total {
            self.seen_tail = self.checked_tail()?;
            if self.free_after_seen() < total {
                return Ok(false);
            }
        }
        #[allow(clippy::cast_possible_truncation)]
        let len32 = len as u32; // at most max_record, which is below u32::MAX
        let mut head = [0u8; RECORD_HEAD];
        head[..4].copy_from_slice(&len32.to_le_bytes());
        head[4..].copy_from_slice(&(len32 ^ CHECK).to_le_bytes());
        let mut at = self.head;
        self.put(at, &head)?;
        at += RECORD_HEAD as u64;
        for part in parts {
            self.put(at, part)?;
            at += part.len() as u64;
        }
        self.head += total as u64;
        self.region
            .word(WORD_HEAD)
            .store(self.head, Ordering::Release);
        Ok(true)
    }

    /// Bytes of records the reader has not fetched yet, as far as this side knows.
    ///
    /// # Errors
    /// [`ProtocolError::Ring`] when the reader's counter is impossible.
    pub fn pending(&mut self) -> Result<usize, ProtocolError> {
        self.seen_tail = self.checked_tail()?;
        Ok(self.capacity - self.free_after_seen())
    }

    fn free_after_seen(&self) -> usize {
        // `seen_tail` was checked: the distance is at most the capacity.
        let used = self.head.wrapping_sub(self.seen_tail);
        self.capacity
            .saturating_sub(usize::try_from(used).unwrap_or(usize::MAX))
    }

    /// The reader's counter, refused when it claims to have read what was never written.
    fn checked_tail(&self) -> Result<u64, ProtocolError> {
        let tail = self.region.word(WORD_TAIL).load(Ordering::Acquire);
        if self.head.wrapping_sub(tail) > self.capacity as u64 {
            return Err(ProtocolError::Ring("tail beyond head"));
        }
        Ok(tail)
    }

    fn put(&self, at: u64, bytes: &[u8]) -> Result<(), ProtocolError> {
        let (offset, first) = split(at, bytes.len(), self.capacity);
        let (a, b) = bytes.split_at(first);
        if self.region.write(offset, a) && self.region.write(0, b) {
            Ok(())
        } else {
            Err(ProtocolError::Ring("write outside the data area"))
        }
    }
}

/// The reading end of a ring.
#[derive(Debug)]
pub struct RingReader {
    region: Arc<Region>,
    capacity: usize,
    max_record: usize,
    /// Bytes read; only this side changes it, the shared word is its copy.
    tail: u64,
}

impl RingReader {
    /// The reading end of the ring in `region`. Records above `max_record` bytes are refused.
    ///
    /// # Errors
    /// As [`capacity_of`].
    pub fn attach(region: Arc<Region>, max_record: usize) -> Result<Self, ProtocolError> {
        let capacity = capacity_of(&region)?;
        Ok(Self {
            max_record: record_limit(capacity, max_record),
            region,
            capacity,
            tail: 0,
        })
    }

    /// Copies the payload of the next record into `out` (cleared first; its allocation is reused).
    /// `Ok(false)` when the ring is empty.
    ///
    /// Does not lock or wait; allocates only when `out` has to grow, never beyond the record limit.
    ///
    /// # Errors
    /// [`ProtocolError::Ring`] or [`ProtocolError::TooLarge`] when the writer's counter or the head
    /// of the record is impossible. The ring is unusable then: the reader has lost the boundaries.
    pub fn try_read(&mut self, out: &mut Vec<u8>) -> Result<bool, ProtocolError> {
        out.clear();
        let available = self.available()?;
        if available == 0 {
            return Ok(false);
        }
        let mut head = [0u8; RECORD_HEAD];
        self.get(self.tail, &mut head)?;
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        let check = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        if len ^ CHECK != check {
            return Err(ProtocolError::Ring("record head fails its check"));
        }
        let len = checked_len(u64::from(len), self.max_record)?;
        let total = padded(len);
        if total > available {
            return Err(ProtocolError::Ring("record longer than what was written"));
        }
        out.resize(len, 0);
        self.get(self.tail + RECORD_HEAD as u64, out)?;
        self.tail += total as u64;
        self.region
            .word(WORD_TAIL)
            .store(self.tail, Ordering::Release);
        Ok(true)
    }

    /// Bytes of records waiting in the ring.
    ///
    /// # Errors
    /// [`ProtocolError::Ring`] when the writer's counter is impossible.
    pub fn pending(&self) -> Result<usize, ProtocolError> {
        self.available()
    }

    /// The capacity of the ring in bytes.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// What the writer claims to have written and this side has not read, checked.
    fn available(&self) -> Result<usize, ProtocolError> {
        let head = self.region.word(WORD_HEAD).load(Ordering::Acquire);
        let available = head.wrapping_sub(self.tail);
        if available > self.capacity as u64 {
            return Err(ProtocolError::Ring("head beyond the capacity"));
        }
        if !available.is_multiple_of(8) {
            return Err(ProtocolError::Ring("head inside a record"));
        }
        // At most the capacity, which is a usize.
        Ok(usize::try_from(available).unwrap_or(usize::MAX))
    }

    fn get(&self, at: u64, out: &mut [u8]) -> Result<(), ProtocolError> {
        let (offset, first) = split(at, out.len(), self.capacity);
        let (a, b) = out.split_at_mut(first);
        if self.region.read(offset, a) && self.region.read(0, b) {
            Ok(())
        } else {
            Err(ProtocolError::Ring("read outside the data area"))
        }
    }
}

/// Where `len` bytes at counter `at` lie: the offset in the data area and how many of the bytes
/// fit before its end (the rest starts at offset 0).
fn split(at: u64, len: usize, capacity: usize) -> (usize, usize) {
    // The capacity is a power of two and a usize, so the remainder is one too.
    let offset = usize::try_from(at % capacity as u64).unwrap_or(0);
    (offset, len.min(capacity - offset))
}

/// The sign of life of a helper: a counter in the shared memory it raises while it works and while
/// it waits. The watchdog of the parent ends a helper whose counter stands still.
#[derive(Debug, Clone)]
pub struct Heartbeat {
    region: Arc<Region>,
}

impl Heartbeat {
    /// The heartbeat word of `region`.
    #[must_use]
    pub const fn new(region: Arc<Region>) -> Self {
        Self { region }
    }

    /// Says "alive". Wait-free.
    pub fn beat(&self) {
        self.region.word(WORD_BEAT).fetch_add(1, Ordering::Relaxed);
    }

    /// The counter. Only whether it changes means anything: the helper may write any value.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.region.word(WORD_BEAT).load(Ordering::Relaxed)
    }
}

/// How long to stay away before polling a ring again, for the attempt number `attempt` since the
/// last success: a few yields first, then sleeps that grow to one millisecond. Processes have no
/// common condition variable; a ring that is empty or full is polled.
#[must_use]
pub fn backoff(attempt: u32) -> Option<Duration> {
    match attempt {
        0..=15 => None,
        16..=31 => Some(Duration::from_micros(100)),
        _ => Some(Duration::from_millis(1)),
    }
}

/// Waits as [`backoff`] says: yields the thread or sleeps.
pub fn pause(attempt: u32) {
    match backoff(attempt) {
        None => std::thread::yield_now(),
        Some(time) => std::thread::sleep(time),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(capacity: usize, max_record: usize) -> (Arc<Region>, RingWriter, RingReader) {
        let region = Arc::new(Region::heap(capacity));
        let (w, r) = pair(&region, max_record).unwrap();
        (region, w, r)
    }

    #[test]
    fn records_come_out_in_order_and_whole() {
        let (_region, mut w, mut r) = ring(256, 64);
        let mut out = Vec::new();
        assert_eq!(r.try_read(&mut out), Ok(false));
        assert_eq!(w.try_write(&[b"one"]), Ok(true));
        assert_eq!(w.try_write(&[b"tw", b"", b"o!"]), Ok(true));
        assert_eq!(w.try_write(&[]), Ok(true));
        assert_eq!(r.pending(), Ok(16 + 16 + 8));
        assert_eq!(r.try_read(&mut out), Ok(true));
        assert_eq!(out, b"one");
        assert_eq!(r.try_read(&mut out), Ok(true));
        assert_eq!(out, b"two!");
        assert_eq!(r.try_read(&mut out), Ok(true));
        assert_eq!(out, b"");
        assert_eq!(r.try_read(&mut out), Ok(false));
        assert_eq!(w.pending(), Ok(0));
    }

    #[test]
    fn a_full_ring_refuses_and_takes_again_after_a_read() {
        let (_region, mut w, mut r) = ring(64, 64);
        // 8 + 24 bytes each: two fit.
        assert_eq!(w.try_write(&[&[1; 24]]), Ok(true));
        assert_eq!(w.try_write(&[&[2; 24]]), Ok(true));
        assert_eq!(w.try_write(&[&[3; 24]]), Ok(false));
        assert_eq!(w.try_write(&[&[3; 1]]), Ok(false));
        let mut out = Vec::new();
        assert_eq!(r.try_read(&mut out), Ok(true));
        assert_eq!(out, [1; 24]);
        assert_eq!(w.try_write(&[&[3; 24]]), Ok(true));
        assert_eq!(r.try_read(&mut out), Ok(true));
        assert_eq!(out, [2; 24]);
        assert_eq!(r.try_read(&mut out), Ok(true));
        assert_eq!(out, [3; 24]);
    }

    #[test]
    fn records_wrap_around_the_end_many_times() {
        let (_region, mut w, mut r) = ring(128, 100);
        let mut out = Vec::new();
        let mut next = 0u8;
        let mut expect = 0u8;
        for round in 0..5000usize {
            let len = round % 97;
            let payload: Vec<u8> = (0..len)
                .map(|i| next.wrapping_add(u8::try_from(i % 251).unwrap()))
                .collect();
            if w.try_write(&[&payload]).unwrap() {
                next = next.wrapping_add(1);
            }
            if round % 3 != 0 {
                while r.try_read(&mut out).unwrap() {
                    assert!(
                        out.iter()
                            .enumerate()
                            .all(|(i, b)| *b == expect.wrapping_add(u8::try_from(i % 251).unwrap()))
                    );
                    expect = expect.wrapping_add(1);
                }
            }
        }
        while r.try_read(&mut out).unwrap() {
            expect = expect.wrapping_add(1);
        }
        assert_eq!(next, expect);
    }

    #[test]
    fn a_record_above_the_limit_is_never_written() {
        let (_region, mut w, _r) = ring(256, 16);
        assert_eq!(w.max_record(), 16);
        assert_eq!(
            w.try_write(&[&[0; 17]]),
            Err(ProtocolError::TooLarge { len: 17, max: 16 })
        );
        // The limit never exceeds what the ring holds.
        let (_region, w, _r) = ring(64, usize::MAX);
        assert_eq!(w.max_record(), 56);
    }

    #[test]
    fn a_region_of_the_wrong_size_is_no_ring() {
        for len in [0, 8, 56, 96] {
            let region = Arc::new(Region::heap(len));
            assert!(pair(&region, 16).is_err(), "{len}");
        }
    }

    #[test]
    fn a_helper_checks_the_ring_it_was_given() {
        let region = Region::heap(64);
        assert_eq!(check_format(&region), Err(ProtocolError::WrongPeer));
        format(&region).unwrap();
        assert_eq!(check_format(&region), Ok(()));
        region.word(WORD_CAPACITY).store(128, Ordering::Relaxed);
        assert!(check_format(&region).is_err());
        region.word(WORD_CAPACITY).store(64, Ordering::Relaxed);
        region.word(WORD_VERSION).store(2, Ordering::Relaxed);
        assert!(check_format(&region).is_err());
    }

    // What a compromised writer can do to the reader.

    #[test]
    fn a_head_beyond_the_capacity_is_refused() {
        let (region, _w, mut r) = ring(64, 64);
        let mut out = Vec::new();
        for lie in [72, 65, u64::MAX, 1 << 40] {
            region.word(WORD_HEAD).store(lie, Ordering::Release);
            assert!(matches!(r.try_read(&mut out), Err(ProtocolError::Ring(_))));
        }
    }

    #[test]
    fn a_head_inside_a_record_is_refused() {
        let (region, _w, mut r) = ring(64, 64);
        region.word(WORD_HEAD).store(12, Ordering::Release);
        assert_eq!(
            r.try_read(&mut Vec::new()),
            Err(ProtocolError::Ring("head inside a record"))
        );
    }

    #[test]
    fn a_record_that_claims_more_than_was_written_is_refused() {
        let (region, mut w, mut r) = ring(64, 64);
        assert_eq!(w.try_write(&[b"abcd"]), Ok(true));
        // The writer rewrites the head of its record: 40 bytes, with a matching check word.
        let len = 40u32;
        let mut head = [0u8; 8];
        head[..4].copy_from_slice(&len.to_le_bytes());
        head[4..].copy_from_slice(&(len ^ CHECK).to_le_bytes());
        assert!(region.write(0, &head));
        assert_eq!(
            r.try_read(&mut Vec::new()),
            Err(ProtocolError::Ring("record longer than what was written"))
        );
    }

    #[test]
    fn a_record_head_without_its_check_word_is_refused() {
        let (region, mut w, mut r) = ring(64, 64);
        assert_eq!(w.try_write(&[b"abcd"]), Ok(true));
        assert!(region.write(0, &2u32.to_le_bytes()));
        assert_eq!(
            r.try_read(&mut Vec::new()),
            Err(ProtocolError::Ring("record head fails its check"))
        );
    }

    #[test]
    fn a_record_above_the_readers_limit_is_refused_before_it_is_copied() {
        let region = Arc::new(Region::heap(1024));
        format(&region).unwrap();
        let mut w = RingWriter::attach(Arc::clone(&region), 512).unwrap();
        let mut r = RingReader::attach(Arc::clone(&region), 16).unwrap();
        assert_eq!(w.try_write(&[&[7; 100]]), Ok(true));
        let mut out = Vec::new();
        assert_eq!(
            r.try_read(&mut out),
            Err(ProtocolError::TooLarge { len: 100, max: 16 })
        );
        assert!(out.capacity() < 100);
    }

    // What a compromised reader can do to the writer.

    #[test]
    fn a_tail_beyond_the_head_is_refused() {
        let (region, mut w, _r) = ring(64, 64);
        assert_eq!(w.try_write(&[&[1; 48]]), Ok(true));
        region.word(WORD_TAIL).store(64, Ordering::Release);
        assert_eq!(
            w.try_write(&[&[1; 48]]),
            Err(ProtocolError::Ring("tail beyond head"))
        );
        assert!(w.pending().is_err());
    }

    #[test]
    fn two_threads_move_a_million_bytes() {
        let (_region, mut w, mut r) = ring(4096, 512);
        let writer = std::thread::spawn(move || {
            let mut attempt = 0;
            for i in 0..20_000u32 {
                let len = (i % 61) as usize;
                let payload = vec![(i % 251) as u8; len];
                while !w.try_write(&[&i.to_le_bytes(), &payload]).unwrap() {
                    pause(attempt);
                    attempt += 1;
                }
                attempt = 0;
            }
        });
        let mut out = Vec::new();
        let mut attempt = 0;
        for i in 0..20_000u32 {
            while !r.try_read(&mut out).unwrap() {
                pause(attempt);
                attempt += 1;
            }
            attempt = 0;
            assert_eq!(out[..4], i.to_le_bytes());
            assert_eq!(out.len(), 4 + (i % 61) as usize);
            assert!(out[4..].iter().all(|b| *b == (i % 251) as u8));
        }
        writer.join().unwrap();
    }

    #[test]
    fn the_heartbeat_counts() {
        let region = Arc::new(Region::heap(64));
        let beat = Heartbeat::new(Arc::clone(&region));
        let before = beat.count();
        beat.beat();
        beat.clone().beat();
        assert_eq!(beat.count(), before + 2);
    }

    #[test]
    fn the_backoff_grows_and_stops_at_a_millisecond() {
        assert_eq!(backoff(0), None);
        assert_eq!(backoff(20), Some(Duration::from_micros(100)));
        assert_eq!(backoff(u32::MAX), Some(Duration::from_millis(1)));
    }
}
