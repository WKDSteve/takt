// SPDX-License-Identifier: GPL-3.0-or-later
//! The helper process: what `takt-demux-helper` runs.
//!
//! It opens the one file it inherited, sends the stream table and then writes packet after packet
//! into the ring, as far ahead as the ring has room. Between any two steps it looks for a request
//! of its parent, and every step is a heartbeat: a helper that neither delivers nor waits is
//! taken for hung and ended ([`takt_ipc::process`]).

use std::process::ExitCode;

use takt_ipc::child::connect;

use crate::proto;

/// Exit code when the process was not started by a parent that speaks the protocol.
const EXIT_NO_PARENT: u8 = 2;

/// The body of `takt-demux-helper`.
///
/// With `--about` as its only argument it prints which FFmpeg it runs with and exits; otherwise
/// it serves the parent that started it.
pub fn run() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--about") {
        println!("{}", about());
        return ExitCode::SUCCESS;
    }
    let (ctx, ring) = match connect(proto::APP) {
        Ok(connected) => connected,
        Err(err) => {
            eprintln!(
                "takt-demux-helper {}: {err}\nThis program is started by Takt; it does nothing on its own. \
                 `--about` prints the FFmpeg it uses.",
                env!("CARGO_PKG_VERSION")
            );
            return ExitCode::from(EXIT_NO_PARENT);
        }
    };
    serve(ctx, ring)
}

/// One line per library: what this build of the helper demuxes with.
#[must_use]
pub fn about() -> String {
    #[cfg(takt_ffmpeg)]
    {
        let libs = crate::ffi::libraries();
        let [f, c, u] = libs.versions;
        format!(
            "takt-demux-helper {}\nlibavformat {}.{}.{}\nlibavcodec {}.{}.{}\nlibavutil {}.{}.{}\nlicense: {}",
            env!("CARGO_PKG_VERSION"),
            f.0,
            f.1,
            f.2,
            c.0,
            c.1,
            c.2,
            u.0,
            u.1,
            u.2,
            libs.license
        )
    }
    #[cfg(not(takt_ffmpeg))]
    {
        format!(
            "takt-demux-helper {}\nbuilt without FFmpeg (FFMPEG_DIR was not set at build time)",
            env!("CARGO_PKG_VERSION")
        )
    }
}

/// Without FFmpeg the helper still speaks the protocol and says why it reads nothing.
#[cfg(not(takt_ffmpeg))]
fn serve(mut ctx: takt_ipc::child::HelperContext, _ring: takt_ipc::child::RingEnd) -> ExitCode {
    let _ = ctx.fault(
        0,
        proto::fault::NO_FFMPEG,
        "built without FFmpeg: set FFMPEG_DIR and build again (docs/BUILD.md)",
    );
    ExitCode::SUCCESS
}

#[cfg(takt_ffmpeg)]
use session::serve;

#[cfg(takt_ffmpeg)]
mod session {
    use std::process::ExitCode;
    use std::time::Duration;

    use takt_ipc::child::{HelperContext, RingEnd};
    use takt_ipc::frame::Frame;
    use takt_ipc::message::kind as core_kind;
    use takt_ipc::ring::{RingWriter, backoff};

    use crate::ffi::{AvError, Input};
    use crate::proto::{self, PACKET_TAIL, PacketHead, Seek, dropped_record, end_tail};

    /// How long the helper sleeps on a request while it has nothing to read (after the end of the
    /// file): short against the heartbeat limit of the parent.
    const IDLE: Duration = Duration::from_millis(20);

    /// What waits to be written into the ring.
    enum Pending {
        /// Nothing: read the next packet.
        Nothing,
        /// The packet the input holds.
        Packet(PacketHead),
        /// A record that is not a packet (the end, a dropped packet): body and tail.
        Other(Vec<u8>),
    }

    /// Whether the file has more to give.
    #[derive(PartialEq, Eq)]
    enum Reading {
        /// Packets are read.
        On,
        /// The end (or an error) was reported; only a seek goes on from here.
        Ended,
    }

    /// Why the loop stops.
    enum Stop {
        /// The parent said goodbye.
        Bye,
        /// The parent is gone or its side of the ring is impossible.
        Lost,
    }

    pub(super) fn serve(mut ctx: HelperContext, ring: RingEnd) -> ExitCode {
        let RingEnd::Writer(mut ring) = ring else {
            let _ = ctx.fault(0, proto::fault::REQUEST, "the demuxer writes the ring");
            return ExitCode::from(super::EXIT_NO_PARENT);
        };
        let Some(file) = ctx.take_file() else {
            let _ = ctx.fault(0, proto::fault::OPEN, "no file was passed on");
            return ExitCode::SUCCESS;
        };
        let mut input = match Input::open(file, ctx.heartbeat()) {
            Ok(input) => input,
            Err((code, text)) => {
                let fault = if code == 0 && text.starts_with("libav") {
                    proto::fault::FFMPEG_VERSION
                } else {
                    proto::fault::OPEN
                };
                let _ = ctx.fault(0, fault, &describe(code, &text));
                return ExitCode::SUCCESS;
            }
        };
        let opened = input.info().and_then(|info| {
            let payload = info.encode();
            if payload.len() > ctx.max_frame() {
                Err("the stream table is larger than a frame may be".to_owned())
            } else {
                Ok(payload)
            }
        });
        match opened {
            Ok(payload) => {
                if ctx.send(proto::kind::OPENED, 0, &payload).is_err() {
                    return ExitCode::SUCCESS;
                }
            }
            Err(text) => {
                let _ = ctx.fault(0, proto::fault::OPEN, &text);
                return ExitCode::SUCCESS;
            }
        }
        match pump(&mut ctx, &mut ring, &mut input) {
            Stop::Bye => {
                let _ = ctx.send(core_kind::BYE, 0, &[]);
            }
            Stop::Lost => {}
        }
        ExitCode::SUCCESS
    }

    fn describe(code: i32, text: &str) -> String {
        if code == 0 {
            text.to_owned()
        } else {
            format!("{text} ({code})")
        }
    }

    /// The record for the end of reading: `None` is the end of the file.
    fn end_record(serial: u32, error: Option<&AvError>) -> Vec<u8> {
        let mut record = Vec::new();
        let code = match error {
            None => 0,
            Some((code, text)) => {
                let mut end = text.len().min(proto::MAX_ERROR_TEXT);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                record.extend_from_slice(&text.as_bytes()[..end]);
                // 0 means "end of file" on the wire; an error without a number gets one.
                if *code == 0 { -1 } else { *code }
            }
        };
        record.extend_from_slice(&end_tail(serial, code));
        record
    }

    fn pump(ctx: &mut HelperContext, ring: &mut RingWriter, input: &mut Input) -> Stop {
        let mut serial = 0u32;
        let mut pending = Pending::Nothing;
        let mut reading = Reading::On;
        let mut attempt = 0u32;
        let largest = ring.max_record().saturating_sub(PACKET_TAIL);
        loop {
            // Requests first: a seek makes whatever waits to be written worthless.
            let wait = match (&pending, &reading) {
                (Pending::Nothing, Reading::Ended) => IDLE,
                (Pending::Nothing, Reading::On) => Duration::ZERO,
                _ => backoff(attempt).unwrap_or(Duration::ZERO),
            };
            while let Some(frame) = ctx.recv(wait) {
                match request(ctx, input, &frame) {
                    Ok(Some(new_serial)) => {
                        serial = new_serial;
                        input.release();
                        pending = Pending::Nothing;
                        reading = Reading::On;
                        attempt = 0;
                    }
                    Ok(None) => {}
                    Err(stop) => return stop,
                }
                if wait.is_zero() {
                    continue;
                }
                break;
            }

            if matches!(pending, Pending::Nothing) {
                if reading == Reading::Ended {
                    continue;
                }
                pending = match input.read() {
                    Ok(Some(head)) => {
                        let size = input.data().len();
                        if size > largest {
                            input.release();
                            Pending::Other(
                                dropped_record(serial, head.stream, size as u64).to_vec(),
                            )
                        } else {
                            Pending::Packet(PacketHead { serial, ..head })
                        }
                    }
                    Ok(None) => {
                        reading = Reading::Ended;
                        Pending::Other(end_record(serial, None))
                    }
                    Err(err) => {
                        reading = Reading::Ended;
                        Pending::Other(end_record(serial, Some(&err)))
                    }
                };
            }

            let written = match &pending {
                Pending::Nothing => continue,
                Pending::Packet(head) => ring.try_write(&[input.data(), &head.tail()]),
                Pending::Other(record) => ring.try_write(&[record]),
            };
            match written {
                Ok(true) => {
                    input.release();
                    pending = Pending::Nothing;
                    attempt = 0;
                    ctx.beat();
                }
                // The ring is full: the parent reads at its own pace. Waiting is alive.
                Ok(false) => attempt = attempt.saturating_add(1),
                Err(_) => return Stop::Lost,
            }
        }
    }

    /// Handles one frame of the parent. `Ok(Some(serial))` after a seek: every record from now on
    /// carries that serial.
    fn request(
        ctx: &mut HelperContext,
        input: &mut Input,
        frame: &Frame,
    ) -> Result<Option<u32>, Stop> {
        match frame.kind {
            core_kind::BYE => Err(Stop::Bye),
            // Nothing here runs long enough to be given up.
            core_kind::CANCEL => Ok(None),
            proto::kind::SEEK => {
                let Ok(seek) = Seek::parse(&frame.payload) else {
                    ctx.fault(
                        frame.seq,
                        proto::fault::REQUEST,
                        "seek request not readable",
                    )
                    .map_err(|_| Stop::Lost)?;
                    return Ok(None);
                };
                let answer = match input.seek(seek.target_micros) {
                    Ok(()) => ctx.send(proto::kind::SEEK_DONE, frame.seq, &[]),
                    Err((code, text)) => {
                        ctx.fault(frame.seq, proto::fault::SEEK, &describe(code, &text))
                    }
                };
                answer.map_err(|_| Stop::Lost)?;
                // Also after a failed seek: the parent counts on the new serial from here on.
                Ok(Some(seek.serial))
            }
            _ => {
                ctx.fault(frame.seq, proto::fault::REQUEST, "unknown request")
                    .map_err(|_| Stop::Lost)?;
                Ok(None)
            }
        }
    }
}
