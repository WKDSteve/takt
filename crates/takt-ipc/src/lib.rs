// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Helper processes of Takt: the one pattern every helper uses.
//!
//! Takt puts what it does not trust or may not link behind a process boundary: the demuxer that
//! parses foreign files, later an encoder, the VST3 bridge, the AI services. All of them talk to
//! the main program the same way (decided 2026-10-30, KONZEPT "Hilfsprozess-IPC"):
//!
//! * **Data** (packets, frames, audio) goes through anonymous shared memory with a wait-free
//!   single-producer single-consumer ring: [`ring`].
//! * **Control** goes through a pipe with a versioned protocol: [`frame`] is the framing,
//!   [`message`] the messages every helper speaks (handshake, progress, error, cancel, goodbye).
//!   What a helper does beyond that is its own protocol on top, parsed with [`wire`].
//! * **The process** is started by [`process::HelperCommand`] and lives under a watchdog and byte
//!   limits; [`child::connect`] is the first call of the helper itself.
//!
//! # Trust
//!
//! The parent trusts nothing a helper sends: every length, counter and offset from the pipe and
//! from the ring is checked against a limit before it is used, and a helper that sends something
//! impossible is ended ([`ProtocolError`], [`IpcError`]). Timestamps and other values *inside* a
//! record belong to the application protocol and are checked there. The helper checks what it gets
//! as well, but the parent is the side that started it.
//!
//! # Zone
//!
//! License zone 1 (`Apache-2.0 OR MIT`), and it depends on no other crate of Takt: a helper of
//! zone 3 carries its own license (GPL for the VST3 bridge, whatever a model asks for), and it must
//! be able to link the code that speaks this protocol whatever that license is. The protocol itself
//! is documented byte by byte ([`frame`], [`ring`]), so a helper in another language needs no line
//! of this crate.
//!
//! This is the one zone 1 crate with `unsafe`: memory shared between processes and the calls that
//! set it up cannot be had without. All of it lives in the module `ffi`, every block with its
//! `// SAFETY:` comment; the rest of the crate is safe code.

pub mod child;
mod error;
mod ffi;
pub mod frame;
pub mod message;
pub mod process;
pub mod ring;
pub mod wire;

pub use error::{IpcError, ProtocolError, tame};
pub use ffi::Region;
