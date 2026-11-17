// SPDX-License-Identifier: GPL-3.0-or-later
//! The software decoder of Takt: libavcodec and dav1d behind a process boundary.
//!
//! A decoder parses bitstreams somebody else wrote, as a demuxer parses containers. So it never
//! runs in the main program either (decided 2026-11-17): [`Decoder`] starts a helper process
//! (`takt-decode-helper`) per video stream, and the helper is the only code that touches
//! libavcodec. Packets of a `takt_demux::Demuxer` go in over the control pipe, pictures come back
//! through a ring in shared memory and are copied into frames of a [`takt_core::FramePool`], each
//! checked as carefully as a file would be (`takt-ipc` has the pattern, [`proto`] the messages of
//! the decoder, [`intake`] the rules of the reading side).
//!
//! ```no_run
//! use std::time::Duration;
//! use takt_decode::{Decoder, Next, Sent};
//! use takt_demux::{Demuxer, Event, StreamKind};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut demuxer = Demuxer::open_path("film.mkv", &takt_demux::Options::beside_current_exe()?)?;
//! let video = demuxer
//!     .info()
//!     .streams
//!     .iter()
//!     .find(|stream| stream.kind == StreamKind::Video)
//!     .ok_or("no picture")?
//!     .clone();
//! let mut decoder = Decoder::open(&video, &takt_decode::Options::beside_current_exe()?)?;
//! loop {
//!     // Packets in, as far as the window allows.
//!     if decoder.can_send() {
//!         match demuxer.next(Duration::from_millis(5))? {
//!             Some(Event::Packet(packet)) => {
//!                 if packet.stream == video.index {
//!                     assert_ne!(decoder.send(&packet)?, Sent::WindowFull);
//!                 }
//!                 demuxer.recycle(packet);
//!             }
//!             Some(Event::End { .. }) => decoder.drain()?,
//!             _ => {}
//!         }
//!     }
//!     // Pictures out. The frame goes back to its pool when it is dropped.
//!     match decoder.try_next()? {
//!         Next::Frame(picture) => println!("{:?}", picture.frame.get().meta().pts),
//!         Next::End => break,
//!         Next::Idle | Next::PoolExhausted => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! [`SoftwareBackend`] is that loop on a thread of its own behind `takt_backend::VideoBackend`:
//! the second backend of the player next to libmpv.
//!
//! # The frame pool
//!
//! Pictures land in a pool of fixed size ([`Options::pool_frames`]). When every frame is out, the
//! next picture waits in the ring, [`Decoder::try_next`] says [`Next::PoolExhausted`] and
//! [`DecodeStats::pool_exhausted`] counts the stall: an empty pool is a measurement of a consumer
//! that does not keep up, never a reason to allocate.
//!
//! # What the helper may do
//!
//! It gets nothing but its pipes and the ring: no file, no path, no handle. It decodes the codecs
//! of a fixed list ([`proto::CODECS`]: H.264, HEVC and VP9 with libavcodec's own decoders, AV1 with
//! dav1d) and lives under the watchdog and the limits of `takt-ipc`. It is not yet confined by the
//! operating system beyond that (`docs/reviews/phase-1.md`).
//!
//! # FFmpeg
//!
//! The helper links FFmpeg 9.0 dynamically (LGPL build, without `--enable-gpl`) when `FFMPEG_DIR`
//! points to it at build time (`docs/BUILD.md`). Without it the crate and the helper build all
//! the same, and every stream is answered with [`DecodeError::Refused`] and the code
//! [`proto::fault::NO_FFMPEG`].
//!
//! # Zone
//!
//! License zone 2 (`GPL-3.0-or-later`). This library is free of `unsafe` and of FFmpeg: the
//! bindings to libavcodec are a module of the helper program
//! (`src/bin/takt-decode-helper/ffi.rs`), so a program that links this crate links and loads no
//! FFmpeg.

#![forbid(unsafe_code)]

mod backend;
mod client;
#[doc(hidden)]
pub mod fuzzing;
pub mod intake;
pub mod proto;

pub use backend::{BackendOptions, PipelineStats, SoftwareBackend};
pub use client::{DecodeError, Decoder, HELPER_NAME, Next, Options, Sent};
pub use intake::{DecodeStats, Decoded};
pub use proto::Layout;
