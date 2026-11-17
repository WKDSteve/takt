// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The messages every helper speaks, whatever it does: who it is, how far it is, what went wrong,
//! stop, goodbye. What a helper does beyond that is its own protocol, in frames of a kind from
//! [`kind::APP`] on.

use crate::error::{ProtocolError, tame};
use crate::wire::{Reader, put_text, put_u16, put_u32, put_u64};

/// The version of the frame format and of the messages in this module. A change that an older peer
/// would misread raises it; both sides refuse another version in the handshake.
pub const PROTOCOL_VERSION: u16 = 1;

/// First field of a [`Hello`]: `TAKT`, so a program that merely writes to its stdout is told apart
/// in the first frame.
pub const MAGIC: u32 = u32::from_le_bytes(*b"TAKT");

/// Longest text of a [`Fault`], in bytes.
pub const MAX_FAULT_TEXT: usize = 1024;

/// The kinds of frames.
pub mod kind {
    /// [`super::Hello`], the first frame of each side.
    pub const HELLO: u16 = 1;
    /// [`super::Progress`], helper to parent.
    pub const PROGRESS: u16 = 2;
    /// [`super::Fault`], helper to parent.
    pub const FAULT: u16 = 3;
    /// Parent to helper, no payload: give up the request with this `seq`.
    pub const CANCEL: u16 = 4;
    /// Parent to helper, no payload: end now. The helper answers with the same and exits.
    pub const BYE: u16 = 5;
    /// First kind an application protocol may use.
    pub const APP: u16 = 0x0100;
}

/// Which helper this is: four ASCII bytes and a version of its own protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppId {
    /// The name, e.g. `*b"DMUX"`.
    pub name: [u8; 4],
    /// The version of the application protocol. Both sides must agree.
    pub version: u16,
}

/// The handshake: each side sends one as its first frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// [`PROTOCOL_VERSION`] of the sender.
    pub protocol: u16,
    /// The application the sender speaks.
    pub app: AppId,
}

impl Hello {
    /// The hello of this build for `app`.
    #[must_use]
    pub const fn ours(app: AppId) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            app,
        }
    }

    /// The payload of the frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12);
        put_u32(&mut out, MAGIC);
        put_u16(&mut out, self.protocol);
        out.extend_from_slice(&self.app.name);
        put_u16(&mut out, self.app.version);
        out
    }

    /// Reads the payload of a frame of kind [`kind::HELLO`].
    ///
    /// # Errors
    /// [`ProtocolError::WrongPeer`] for another magic number, otherwise what the fields say.
    pub fn parse(payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(payload);
        if r.u32()? != MAGIC {
            return Err(ProtocolError::WrongPeer);
        }
        let protocol = r.u16()?;
        let name = <[u8; 4]>::try_from(r.bytes(4)?).map_err(|_| ProtocolError::Truncated)?;
        let version = r.u16()?;
        r.finish()?;
        Ok(Self {
            protocol,
            app: AppId { name, version },
        })
    }

    /// Checks the other side's hello against what this side expects.
    ///
    /// # Errors
    /// [`ProtocolError::Version`] for another protocol or application version,
    /// [`ProtocolError::WrongPeer`] for another application.
    pub fn accept(&self, expected: AppId) -> Result<(), ProtocolError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ProtocolError::Version {
                theirs: self.protocol,
                ours: PROTOCOL_VERSION,
            });
        }
        if self.app.name != expected.name {
            return Err(ProtocolError::WrongPeer);
        }
        if self.app.version != expected.version {
            return Err(ProtocolError::Version {
                theirs: self.app.version,
                ours: expected.version,
            });
        }
        Ok(())
    }
}

/// How far a long request is. `total` is 0 while the helper does not know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Units done (bytes, frames: the application says which).
    pub done: u64,
    /// Units in all, 0 for unknown.
    pub total: u64,
}

impl Progress {
    /// The payload of the frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        put_u64(&mut out, self.done);
        put_u64(&mut out, self.total);
        out
    }

    /// Reads the payload of a frame of kind [`kind::PROGRESS`].
    ///
    /// # Errors
    /// A [`ProtocolError`]; also when more is done than there is in all.
    pub fn parse(payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(payload);
        let done = r.u64()?;
        let total = r.u64()?;
        r.finish()?;
        if total != 0 && done > total {
            return Err(ProtocolError::Invalid("progress beyond its total"));
        }
        Ok(Self { done, total })
    }
}

/// An error the helper reports itself, as the answer to the request with the frame's `seq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    /// A code of the application protocol.
    pub code: u32,
    /// A text for a person. Foreign text: [`Fault::parse`] bounds it and replaces control characters.
    pub text: String,
}

impl Fault {
    /// The payload of the frame; the text is cut to [`MAX_FAULT_TEXT`].
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6 + self.text.len().min(MAX_FAULT_TEXT));
        put_u32(&mut out, self.code);
        put_text(&mut out, &self.text, MAX_FAULT_TEXT);
        out
    }

    /// Reads the payload of a frame of kind [`kind::FAULT`].
    ///
    /// # Errors
    /// A [`ProtocolError`].
    pub fn parse(payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(payload);
        let code = r.u32()?;
        let text = tame(r.text(MAX_FAULT_TEXT)?, MAX_FAULT_TEXT);
        r.finish()?;
        Ok(Self { code, text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: AppId = AppId {
        name: *b"TEST",
        version: 3,
    };

    #[test]
    fn a_hello_comes_back_and_is_accepted() {
        let hello = Hello::ours(APP);
        let back = Hello::parse(&hello.encode()).unwrap();
        assert_eq!(back, hello);
        assert_eq!(back.accept(APP), Ok(()));
    }

    #[test]
    fn another_version_or_program_is_refused() {
        let mut other = Hello::ours(APP);
        other.protocol = PROTOCOL_VERSION + 1;
        assert_eq!(
            other.accept(APP),
            Err(ProtocolError::Version {
                theirs: PROTOCOL_VERSION + 1,
                ours: PROTOCOL_VERSION
            })
        );
        let newer = AppId {
            name: *b"TEST",
            version: 4,
        };
        assert_eq!(
            Hello::ours(newer).accept(APP),
            Err(ProtocolError::Version { theirs: 4, ours: 3 })
        );
        let stranger = AppId {
            name: *b"ELSE",
            version: 3,
        };
        assert_eq!(
            Hello::ours(stranger).accept(APP),
            Err(ProtocolError::WrongPeer)
        );
        assert_eq!(Hello::parse(b"hello world!"), Err(ProtocolError::WrongPeer));
        let mut long = Hello::ours(APP).encode();
        long.push(0);
        assert_eq!(Hello::parse(&long), Err(ProtocolError::Trailing));
    }

    #[test]
    fn progress_cannot_pass_its_total() {
        let p = Progress { done: 5, total: 9 };
        assert_eq!(Progress::parse(&p.encode()), Ok(p));
        let unknown = Progress { done: 5, total: 0 };
        assert_eq!(Progress::parse(&unknown.encode()), Ok(unknown));
        let lie = Progress { done: 10, total: 9 };
        assert_eq!(
            Progress::parse(&lie.encode()),
            Err(ProtocolError::Invalid("progress beyond its total"))
        );
    }

    #[test]
    fn the_text_of_a_fault_is_tamed() {
        let fault = Fault {
            code: 2,
            text: "bad\x1b[2Jfile\r\n".to_owned(),
        };
        let back = Fault::parse(&fault.encode()).unwrap();
        assert_eq!(back.code, 2);
        assert_eq!(back.text, "bad [2Jfile  ");
        let long = Fault {
            code: 1,
            text: "x".repeat(5000),
        };
        assert_eq!(
            Fault::parse(&long.encode()).unwrap().text.len(),
            MAX_FAULT_TEXT
        );
    }
}
