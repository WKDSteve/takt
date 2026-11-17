// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A block of memory two processes see: a head of 64-bit words and a data area behind it.
//!
//! The other process may write any byte at any time, also while this one reads. So no reference to
//! the data area ever exists: bytes are copied in and out through raw pointers, and what comes out
//! is treated as arbitrary bytes by everything above. The words of the head are atomics, which is
//! what an atomic is for.
#![allow(unsafe_code)]

use std::ptr::NonNull;
use std::sync::atomic::AtomicU64;

use super::SharedMemory;

/// Bytes of the head: 64 words. The ring keeps its counters 128 bytes apart in it, so the two sides
/// do not share a cache line.
pub const HEAD_LEN: usize = 512;

/// Words in the head.
pub const HEAD_WORDS: usize = HEAD_LEN / 8;

/// Memory shared with a helper process, or a block on the heap with the same shape (for tests and
/// for a ring inside one process).
pub struct Region {
    /// Start of the block: aligned to 8, valid for `HEAD_LEN + data_len` bytes while `_owner` lives.
    ptr: NonNull<u8>,
    data_len: usize,
    _owner: Owner,
}

enum Owner {
    Heap(#[allow(dead_code)] Box<[AtomicU64]>),
    Shared(SharedMemory),
}

// SAFETY: the block is owned by `_owner` and stays where it is until the region is dropped. All
// access goes through atomics (the head) or through raw copies that never form a reference (the
// data), so several threads may use a region at once exactly as several processes do.
unsafe impl Send for Region {}
// SAFETY: as above.
unsafe impl Sync for Region {}

impl std::fmt::Debug for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Region")
            .field("data_len", &self.data_len)
            .finish_non_exhaustive()
    }
}

impl Region {
    /// A zeroed region on the heap with `data_len` bytes of data (rounded up to a multiple of 8).
    #[must_use]
    pub fn heap(data_len: usize) -> Self {
        let data_words = data_len.div_ceil(8);
        let words: Box<[AtomicU64]> = (0..HEAD_WORDS + data_words)
            .map(|_| AtomicU64::new(0))
            .collect();
        // An atomic has interior mutability, so writing through a pointer derived from the shared
        // slice is allowed.
        let ptr = NonNull::from(&*words).cast::<u8>();
        Self {
            ptr,
            data_len: data_words * 8,
            _owner: Owner::Heap(words),
        }
    }

    /// The region of a mapping. `None` when the mapping is shorter than the head.
    pub(crate) fn shared(memory: SharedMemory) -> Option<Self> {
        let data_len = memory.len().checked_sub(HEAD_LEN)?;
        let ptr = memory.ptr();
        // A mapping starts at a page boundary; checked all the same, the words below rely on it.
        if !ptr.as_ptr().addr().is_multiple_of(8) {
            return None;
        }
        Some(Self {
            ptr,
            data_len,
            _owner: Owner::Shared(memory),
        })
    }

    /// The mapping behind the region, `None` for a region on the heap.
    pub(crate) const fn memory(&self) -> Option<&SharedMemory> {
        match &self._owner {
            Owner::Heap(_) => None,
            Owner::Shared(memory) => Some(memory),
        }
    }

    /// Bytes of the data area.
    #[must_use]
    pub const fn data_len(&self) -> usize {
        self.data_len
    }

    /// Word `slot` of the head, counted modulo 64 (the head has 64 words).
    #[must_use]
    pub fn word(&self, slot: usize) -> &AtomicU64 {
        let slot = slot % HEAD_WORDS;
        // SAFETY: the block is at least HEAD_LEN bytes long and aligned to 8 (checked when the
        // region was made), so word `slot < HEAD_WORDS` lies inside it and is aligned. It lives as
        // long as `self`. Another process may write it at any time: an atomic allows exactly that.
        unsafe { &*self.ptr.as_ptr().add(slot * 8).cast::<AtomicU64>() }
    }

    /// Copies `out.len()` bytes from the data area at `offset`. `false`, and nothing copied, when
    /// that range is not inside the data area.
    #[must_use]
    pub fn read(&self, offset: usize, out: &mut [u8]) -> bool {
        if !self.contains(offset, out.len()) {
            return false;
        }
        // SAFETY: the range `offset..offset + out.len()` lies inside the data area (checked above),
        // which is valid for reads while `self` lives; `out` is a distinct, exclusive buffer. The
        // other process may write the source while it is copied: no reference to the source exists,
        // and the copy may hold any mix of old and new bytes, which every caller accounts for (the
        // bytes of the other process are never trusted).
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.ptr.as_ptr().add(HEAD_LEN + offset),
                out.as_mut_ptr(),
                out.len(),
            );
        }
        true
    }

    /// Copies `bytes` into the data area at `offset`. `false`, and nothing copied, when that range
    /// is not inside the data area.
    #[must_use]
    pub fn write(&self, offset: usize, bytes: &[u8]) -> bool {
        if !self.contains(offset, bytes.len()) {
            return false;
        }
        // SAFETY: the range lies inside the data area (checked above), which is valid for writes
        // while `self` lives; `bytes` is a distinct buffer. No reference to the destination exists
        // in this process; the ring on top makes sure the other side does not read these bytes
        // before the counter that publishes them is stored.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.ptr.as_ptr().add(HEAD_LEN + offset),
                bytes.len(),
            );
        }
        true
    }

    fn contains(&self, offset: usize, len: usize) -> bool {
        offset
            .checked_add(len)
            .is_some_and(|end| end <= self.data_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn bytes_and_words_come_back() {
        let region = Region::heap(20);
        assert_eq!(region.data_len(), 24);
        assert!(region.write(3, b"takt"));
        let mut out = [0u8; 4];
        assert!(region.read(3, &mut out));
        assert_eq!(&out, b"takt");
        region.word(5).store(77, Ordering::Relaxed);
        assert_eq!(region.word(5 + HEAD_WORDS).load(Ordering::Relaxed), 77);
        // The head and the data do not overlap.
        assert!(region.read(0, &mut out));
        assert_eq!(out, [0, 0, 0, b't']);
    }

    #[test]
    fn nothing_is_copied_outside_the_data_area() {
        let region = Region::heap(16);
        let mut out = [9u8; 8];
        assert!(!region.read(9, &mut out));
        assert!(!region.read(usize::MAX, &mut out));
        assert!(!region.write(16, b"x"));
        assert!(region.write(16, b""));
        assert_eq!(out, [9; 8]);
    }
}
