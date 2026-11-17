// SPDX-License-Identifier: Apache-2.0 OR MIT
//! What can go wrong between two processes.

use std::fmt;
use std::io;

/// A byte sequence from the other process that is not the protocol.
///
/// Every variant is a refusal: nothing of the offending message was acted on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolError {
    /// A frame or record announces more bytes than the limit allows.
    TooLarge {
        /// The announced length in bytes.
        len: u64,
        /// The limit in bytes.
        max: u64,
    },
    /// A message ended before a field it must have.
    Truncated,
    /// A message has bytes behind its last field.
    Trailing,
    /// A text is not UTF-8.
    Utf8,
    /// A field holds a value the protocol does not define. The text names the field.
    Invalid(&'static str),
    /// The other side speaks another version of the protocol.
    Version {
        /// The version it announced.
        theirs: u16,
        /// The version this build speaks.
        ours: u16,
    },
    /// The other side is not the program that was expected (magic number or application).
    WrongPeer,
    /// The counters or a record head of the ring contradict each other. The text names the check.
    Ring(&'static str),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { len, max } => {
                write!(f, "{len} bytes announced, the limit is {max}")
            }
            Self::Truncated => f.write_str("message ends before its last field"),
            Self::Trailing => f.write_str("message has bytes behind its last field"),
            Self::Utf8 => f.write_str("text is not UTF-8"),
            Self::Invalid(field) => write!(f, "invalid value in {field}"),
            Self::Version { theirs, ours } => {
                write!(f, "protocol version {theirs}, this build speaks {ours}")
            }
            Self::WrongPeer => f.write_str("the other side is not the expected program"),
            Self::Ring(check) => write!(f, "ring is inconsistent: {check}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// Why a helper process is gone or was never there.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IpcError {
    /// The helper could not be started or set up. The text names the step and the reason.
    Spawn(String),
    /// Reading from or writing to the helper failed. The text names the step and the reason.
    Io(String),
    /// The helper sent something that is not the protocol; it was ended.
    Protocol(ProtocolError),
    /// The helper did not answer or did not show a heartbeat in time; it was ended. The text names
    /// what was waited for.
    Timeout(&'static str),
    /// The helper went over a limit (bytes on stderr, frames nobody fetched); it was ended. The text
    /// names the limit.
    Limit(&'static str),
    /// The helper ended by itself.
    Exited {
        /// Its exit code, `None` when a signal ended it or the code is not known.
        code: Option<i32>,
        /// The end of what it wrote to stderr: foreign text, shortened, control characters replaced.
        stderr: String,
    },
    /// The helper reported an error of its own ([`crate::message::Fault`]).
    Fault {
        /// The code of the helper's application protocol.
        code: u32,
        /// Its text: foreign, bounded, control characters replaced.
        text: String,
    },
}

impl IpcError {
    /// An [`IpcError::Io`] from an I/O error and the step it happened in.
    #[must_use]
    pub fn io(step: &str, err: &io::Error) -> Self {
        Self::Io(format!("{step}: {err}"))
    }

    /// An [`IpcError::Spawn`] from an I/O error and the step it happened in.
    #[must_use]
    pub fn spawn(step: &str, err: &io::Error) -> Self {
        Self::Spawn(format!("{step}: {err}"))
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(why) => write!(f, "helper not started: {why}"),
            Self::Io(why) => write!(f, "helper pipe: {why}"),
            Self::Protocol(err) => write!(f, "helper broke the protocol: {err}"),
            Self::Timeout(what) => write!(f, "helper ended after a timeout ({what})"),
            Self::Limit(what) => write!(f, "helper ended over a limit ({what})"),
            Self::Exited { code, stderr } => {
                match code {
                    Some(code) => write!(f, "helper exited with code {code}")?,
                    None => f.write_str("helper was ended")?,
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::Fault { code, text } => write!(f, "helper reported error {code}: {text}"),
        }
    }
}

impl std::error::Error for IpcError {}

impl From<ProtocolError> for IpcError {
    fn from(err: ProtocolError) -> Self {
        Self::Protocol(err)
    }
}

/// Foreign text made safe to show and to log: at most `max` bytes, control characters replaced.
#[must_use]
pub fn tame(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max));
    for c in text.chars() {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(if c.is_control() { ' ' } else { c });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_text_loses_its_control_characters_and_its_length() {
        assert_eq!(tame("a\x1b[31mb\nc", 64), "a [31mb c");
        assert_eq!(tame("äöü", 3), "ä");
        assert_eq!(tame("", 8), "");
    }

    #[test]
    fn messages_name_the_cause() {
        let err = IpcError::from(ProtocolError::TooLarge { len: 9, max: 8 });
        assert_eq!(
            err.to_string(),
            "helper broke the protocol: 9 bytes announced, the limit is 8"
        );
        let err = IpcError::Exited {
            code: Some(3),
            stderr: "boom".to_owned(),
        };
        assert_eq!(err.to_string(), "helper exited with code 3: boom");
    }
}
