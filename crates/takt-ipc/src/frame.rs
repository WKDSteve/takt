// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The frames of the control pipe.
//!
//! ```text
//! offset  size  field
//!      0     4  len      bytes of the payload, little endian
//!      4     2  kind     what the payload is (crate::message::kind)
//!      6     2  flags    reserved, must be 0
//!      8     4  seq      the request this frame belongs to; an answer repeats it
//!     12   len  payload
//! ```
//!
//! [`FrameDecoder`] is the reading side: it takes the bytes as the pipe delivers them and hands out
//! whole frames. It refuses a length above its limit before it buffers a single byte of that
//! payload, so the memory it holds is bounded by the limit, whatever the other side announces.

use crate::error::ProtocolError;
use crate::wire::checked_len;

/// Bytes in front of every payload.
pub const HEADER_LEN: usize = 12;

/// One message of the control pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// What the payload is.
    pub kind: u16,
    /// The request this frame belongs to.
    pub seq: u32,
    /// The message itself, parsed by its kind.
    pub payload: Vec<u8>,
}

/// Appends one frame to `out`.
///
/// # Errors
/// [`ProtocolError::TooLarge`] when the payload is longer than `max_payload`: a side never sends
/// what the other side must refuse.
pub fn encode(
    kind: u16,
    seq: u32,
    payload: &[u8],
    max_payload: usize,
    out: &mut Vec<u8>,
) -> Result<(), ProtocolError> {
    let len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
    checked_len(len, max_payload.min(u32::MAX as usize))?;
    out.reserve(HEADER_LEN + payload.len());
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Turns the byte stream of a pipe into frames.
#[derive(Debug)]
pub struct FrameDecoder {
    max_payload: usize,
    buf: Vec<u8>,
    /// Bytes of `buf` already handed out.
    start: usize,
    failed: Option<ProtocolError>,
}

impl FrameDecoder {
    /// A decoder that refuses payloads above `max_payload` bytes.
    #[must_use]
    pub const fn new(max_payload: usize) -> Self {
        Self {
            max_payload,
            buf: Vec::new(),
            start: 0,
            failed: None,
        }
    }

    /// Takes the next bytes of the stream. Call [`next`](Self::next) until it returns `None` before
    /// pushing more, then the buffer never holds more than one payload plus what was pushed.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.failed.is_some() {
            return;
        }
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start >= 64 * 1024 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole frame, `None` when the stream has to deliver more first.
    ///
    /// # Errors
    /// A [`ProtocolError`] when the stream is not the protocol. The decoder then stays failed: a
    /// stream that lied once has no frame boundary to trust any more.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Frame>, ProtocolError> {
        if let Some(err) = &self.failed {
            return Err(err.clone());
        }
        match self.parse() {
            Ok(frame) => Ok(frame),
            Err(err) => {
                self.buf = Vec::new();
                self.start = 0;
                self.failed = Some(err.clone());
                Err(err)
            }
        }
    }

    fn parse(&mut self) -> Result<Option<Frame>, ProtocolError> {
        let pending = &self.buf[self.start..];
        let Some((header, body)) = pending.split_at_checked(HEADER_LEN) else {
            return Ok(None);
        };
        let field = |at: usize| [header[at], header[at + 1], header[at + 2], header[at + 3]];
        let len = u32::from_le_bytes(field(0));
        let kind = u16::from_le_bytes([header[4], header[5]]);
        let flags = u16::from_le_bytes([header[6], header[7]]);
        let seq = u32::from_le_bytes(field(8));
        let len = checked_len(u64::from(len), self.max_payload)?;
        if flags != 0 {
            return Err(ProtocolError::Invalid("frame flags"));
        }
        let Some(payload) = body.get(..len) else {
            return Ok(None);
        };
        let frame = Frame {
            kind,
            seq,
            payload: payload.to_vec(),
        };
        self.start += HEADER_LEN + len;
        Ok(Some(frame))
    }

    /// Bytes taken and not yet handed out as a frame.
    #[must_use]
    pub const fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(kind: u16, seq: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        encode(kind, seq, payload, 1 << 20, &mut out).unwrap();
        out
    }

    #[test]
    fn frames_survive_any_cut_of_the_stream() {
        let mut stream = bytes(7, 1, b"hello");
        stream.extend(bytes(8, 2, b""));
        stream.extend(bytes(9, u32::MAX, &[0xAB; 300]));
        for chunk in [1, 2, 5, 11, 12, 13, 64, 1000] {
            let mut dec = FrameDecoder::new(1024);
            let mut got = Vec::new();
            for part in stream.chunks(chunk) {
                dec.push(part);
                while let Some(frame) = dec.next().unwrap() {
                    got.push(frame);
                }
            }
            assert_eq!(got.len(), 3, "chunk {chunk}");
            assert_eq!(
                (got[0].kind, got[0].seq, &got[0].payload[..]),
                (7, 1, &b"hello"[..])
            );
            assert_eq!((got[1].kind, got[1].payload.len()), (8, 0));
            assert_eq!((got[2].seq, got[2].payload.len()), (u32::MAX, 300));
            assert_eq!(dec.buffered(), 0);
        }
    }

    #[test]
    fn a_length_above_the_limit_fails_before_the_payload_arrives() {
        let mut dec = FrameDecoder::new(16);
        // Only the header: 17 bytes announced.
        dec.push(&bytes(1, 0, &[0; 17])[..HEADER_LEN]);
        assert_eq!(
            dec.next(),
            Err(ProtocolError::TooLarge { len: 17, max: 16 })
        );
        // Failed for good, and nothing is buffered any more.
        dec.push(&bytes(1, 0, b"ok"));
        assert_eq!(
            dec.next(),
            Err(ProtocolError::TooLarge { len: 17, max: 16 })
        );
        assert_eq!(dec.buffered(), 0);
    }

    #[test]
    fn reserved_flags_must_be_zero() {
        let mut raw = bytes(1, 0, b"x");
        raw[6] = 1;
        let mut dec = FrameDecoder::new(16);
        dec.push(&raw);
        assert_eq!(dec.next(), Err(ProtocolError::Invalid("frame flags")));
    }

    #[test]
    fn a_sender_does_not_build_what_the_reader_refuses() {
        let mut out = Vec::new();
        assert_eq!(
            encode(1, 0, &[0; 9], 8, &mut out),
            Err(ProtocolError::TooLarge { len: 9, max: 8 })
        );
        assert!(out.is_empty());
    }

    #[test]
    fn the_buffer_stays_small_over_a_long_stream() {
        let mut dec = FrameDecoder::new(256);
        let one = bytes(3, 9, &[1; 200]);
        for _ in 0..10_000 {
            dec.push(&one);
            assert!(dec.next().unwrap().is_some());
            assert!(dec.next().unwrap().is_none());
        }
        assert!(dec.buf.capacity() < 128 * 1024);
    }
}
