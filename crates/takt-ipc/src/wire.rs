// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Reading and writing the fields of a message: little endian, lengths checked before anything is
//! taken, no field read past the end.
//!
//! The application protocols of the helpers (the packets of the demuxer, later the frames of an
//! encoder) parse with the same [`Reader`], so every parser of bytes from another process has the
//! same guards.

use crate::error::ProtocolError;

/// A cursor over the bytes of one message.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    /// A cursor at the start of `bytes`.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    /// Bytes not yet read.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.rest.len()
    }

    /// The next `n` bytes.
    ///
    /// # Errors
    /// [`ProtocolError::Truncated`] when fewer than `n` bytes are left.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        let (head, tail) = self
            .rest
            .split_at_checked(n)
            .ok_or(ProtocolError::Truncated)?;
        self.rest = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        let bytes = self.bytes(N)?;
        <[u8; N]>::try_from(bytes).map_err(|_| ProtocolError::Truncated)
    }

    /// One byte.
    ///
    /// # Errors
    /// [`ProtocolError::Truncated`] at the end of the message.
    pub fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.array::<1>()?[0])
    }

    /// A `u16`, little endian.
    ///
    /// # Errors
    /// [`ProtocolError::Truncated`] when fewer than two bytes are left.
    pub fn u16(&mut self) -> Result<u16, ProtocolError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    /// A `u32`, little endian.
    ///
    /// # Errors
    /// [`ProtocolError::Truncated`] when fewer than four bytes are left.
    pub fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// An `i32`, little endian.
    ///
    /// # Errors
    /// [`ProtocolError::Truncated`] when fewer than four bytes are left.
    pub fn i32(&mut self) -> Result<i32, ProtocolError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    /// A `u64`, little endian.
    ///
    /// # Errors
    /// [`ProtocolError::Truncated`] when fewer than eight bytes are left.
    pub fn u64(&mut self) -> Result<u64, ProtocolError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// An `i64`, little endian.
    ///
    /// # Errors
    /// [`ProtocolError::Truncated`] when fewer than eight bytes are left.
    pub fn i64(&mut self) -> Result<i64, ProtocolError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    /// A byte string with a `u32` length in front, at most `max` bytes.
    ///
    /// # Errors
    /// [`ProtocolError::TooLarge`] when the length is above `max` (nothing is read then), or
    /// [`ProtocolError::Truncated`].
    pub fn blob(&mut self, max: usize) -> Result<&'a [u8], ProtocolError> {
        let len = self.u32()?;
        let len = checked_len(u64::from(len), max)?;
        self.bytes(len)
    }

    /// A UTF-8 text with a `u16` length in front, at most `max` bytes.
    ///
    /// # Errors
    /// [`ProtocolError::TooLarge`], [`ProtocolError::Truncated`] or [`ProtocolError::Utf8`].
    pub fn text(&mut self, max: usize) -> Result<&'a str, ProtocolError> {
        let len = self.u16()?;
        let len = checked_len(u64::from(len), max)?;
        std::str::from_utf8(self.bytes(len)?).map_err(|_| ProtocolError::Utf8)
    }

    /// Ends the message.
    ///
    /// # Errors
    /// [`ProtocolError::Trailing`] when bytes are left: a message with more than its fields is
    /// another message.
    pub const fn finish(self) -> Result<(), ProtocolError> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(ProtocolError::Trailing)
        }
    }
}

/// `len` as a `usize`, refused above `max`.
///
/// # Errors
/// [`ProtocolError::TooLarge`].
pub fn checked_len(len: u64, max: usize) -> Result<usize, ProtocolError> {
    let too_large = || ProtocolError::TooLarge {
        len,
        max: u64::try_from(max).unwrap_or(u64::MAX),
    };
    let as_usize = usize::try_from(len).map_err(|_| too_large())?;
    if as_usize > max {
        return Err(too_large());
    }
    Ok(as_usize)
}

/// Appends a `u16`, little endian.
pub fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a `u32`, little endian.
pub fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends an `i32`, little endian.
pub fn put_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a `u64`, little endian.
pub fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends an `i64`, little endian.
pub fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a byte string with its `u32` length in front. A string longer than `u32::MAX` bytes is cut
/// there (the reader refuses far below).
pub fn put_blob(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    put_u32(out, len);
    out.extend_from_slice(&bytes[..len as usize]);
}

/// Appends a text with its `u16` length in front, cut at a character boundary to at most `max` bytes
/// (and at most `u16::MAX`).
pub fn put_text(out: &mut Vec<u8>, text: &str, max: usize) {
    let max = max.min(usize::from(u16::MAX));
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    put_u16(out, u16::try_from(end).unwrap_or(u16::MAX));
    out.extend_from_slice(&text.as_bytes()[..end]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_come_back_as_written() {
        let mut out = Vec::new();
        put_u16(&mut out, 0xBEEF);
        put_u32(&mut out, 7);
        put_i32(&mut out, -7);
        put_u64(&mut out, u64::MAX - 1);
        put_i64(&mut out, i64::MIN);
        put_blob(&mut out, b"abc");
        put_text(&mut out, "zäh", 16);
        let mut r = Reader::new(&out);
        assert_eq!(r.u16(), Ok(0xBEEF));
        assert_eq!(r.u32(), Ok(7));
        assert_eq!(r.i32(), Ok(-7));
        assert_eq!(r.u64(), Ok(u64::MAX - 1));
        assert_eq!(r.i64(), Ok(i64::MIN));
        assert_eq!(r.blob(3), Ok(&b"abc"[..]));
        assert_eq!(r.text(16), Ok("zäh"));
        assert_eq!(r.finish(), Ok(()));
    }

    #[test]
    fn a_length_above_the_limit_is_refused_before_it_is_read() {
        let mut out = Vec::new();
        put_blob(&mut out, &[0; 9]);
        let mut r = Reader::new(&out);
        assert_eq!(r.blob(8), Err(ProtocolError::TooLarge { len: 9, max: 8 }));
        // A length that lies about what follows.
        let lie = [0xFF, 0xFF, 0xFF, 0x7F, 1, 2];
        assert_eq!(
            Reader::new(&lie).blob(usize::MAX),
            Err(ProtocolError::Truncated)
        );
    }

    #[test]
    fn a_short_message_and_a_long_one_are_both_refused() {
        assert_eq!(Reader::new(&[1, 2, 3]).u32(), Err(ProtocolError::Truncated));
        let mut r = Reader::new(&[1, 2, 3]);
        assert_eq!(r.u16(), Ok(0x0201));
        assert_eq!(r.finish(), Err(ProtocolError::Trailing));
    }

    #[test]
    fn a_text_is_cut_at_a_character_and_must_be_utf8() {
        let mut out = Vec::new();
        put_text(&mut out, "aä", 2);
        assert_eq!(Reader::new(&out).text(8), Ok("a"));
        let bad = [2, 0, 0xC3, 0x28];
        assert_eq!(Reader::new(&bad).text(8), Err(ProtocolError::Utf8));
    }
}
