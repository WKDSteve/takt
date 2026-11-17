// SPDX-License-Identifier: GPL-3.0-or-later
//! The demuxer of Takt: libavformat behind a process boundary.
//!
//! Container parsers read bytes somebody else wrote, and they are the classic way into a media
//! player. So libavformat never runs in the main program: [`Demuxer`] starts a helper process
//! (`takt-demux-helper`) per file, and the helper is the only code that touches the container.
//! What comes back are [`Packet`]s through a ring in shared memory and a [`MediaInfo`] over a
//! pipe, both checked as carefully as a file would be (`takt-ipc` has the pattern, [`proto`] the
//! messages of the demuxer).
//!
//! ```no_run
//! use std::time::Duration;
//! use takt_demux::{Demuxer, Event, Options};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut demuxer = Demuxer::open_path("film.mkv", &Options::beside_current_exe()?)?;
//! for stream in &demuxer.info().streams {
//!     println!("{} {:?} {}", stream.index, stream.kind, stream.codec);
//! }
//! while let Some(event) = demuxer.next(Duration::from_secs(1))? {
//!     match event {
//!         // `pts`, `dts` and `duration` are `takt_core::Timestamp`s in the time base of the
//!         // packet's stream: exact, never a float.
//!         Event::Packet(packet) => {
//!             println!("{} {:?} {} bytes", packet.stream, packet.pts, packet.data.len());
//!             demuxer.recycle(packet);
//!         }
//!         Event::End { .. } => break,
//!         Event::Dropped { .. } => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # What the helper may do
//!
//! It gets the open file as an inherited handle, no path, and reads it through callbacks; the
//! demuxers it may use are a fixed list (Matroska, WebM, the MP4 family) and libavformat is told to
//! open nothing else by itself. It lives under the watchdog and the limits of `takt-ipc`: a
//! heartbeat with every read and every packet, a bound on its memory, an end with its parent. It is
//! not yet confined by the operating system beyond that (no restricted token, no seccomp); that is
//! the open point `docs/reviews/phase-1.md` names.
//!
//! # FFmpeg
//!
//! The helper links FFmpeg 9.0 dynamically (LGPL build, without `--enable-gpl`) when `FFMPEG_DIR`
//! points to it at build time (`docs/BUILD.md`). Without it the crate and the helper build all the
//! same, and every file is answered with [`DemuxError::Refused`] and the code
//! [`proto::fault::NO_FFMPEG`].
//!
//! # Zone
//!
//! License zone 2 (`GPL-3.0-or-later`). `unsafe` only in the module `ffi`, the bindings to
//! libavformat, which only exists in a build with FFmpeg.

mod client;
#[cfg(takt_ffmpeg)]
mod ffi;
#[doc(hidden)]
pub mod fuzzing;
pub mod helper;
pub mod proto;

pub use client::{DemuxError, Demuxer, Event, HELPER_NAME, Options};
pub use proto::{AudioParams, MediaInfo, Packet, StreamInfo, StreamKind, VideoParams};
