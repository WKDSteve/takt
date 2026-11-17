// SPDX-License-Identifier: GPL-3.0-or-later
//! The decode helper of Takt: the process that decodes one video stream for the main program.
//! Started by `takt_decode::Decoder`, never by hand; `--about` prints the FFmpeg it runs with.
//!
//! It gets no file and no handle but its pipes and the ring: compressed packets arrive on the
//! control pipe, decoded pictures leave through the ring. Between any two steps it looks for a
//! request of its parent, and every step is a heartbeat: a helper that neither delivers nor waits
//! is taken for hung and ended ([`takt_ipc::process`]).
//!
//! This program is the only code of `takt-decode` that links FFmpeg ([`ffi`], in a build with
//! `FFMPEG_DIR`).

use std::process::ExitCode;

use takt_decode::proto;
use takt_ipc::child::connect;

#[cfg(takt_ffmpeg)]
mod ffi;

/// Exit code when the process was not started by a parent that speaks the protocol.
const EXIT_NO_PARENT: u8 = 2;

/// With `--about` as its only argument the helper prints which FFmpeg it runs with and exits;
/// otherwise it serves the parent that started it.
fn main() -> ExitCode {
    // `args_os`: an argument that is not Unicode must not end the helper in a panic.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--about")
    {
        println!("{}", about());
        return ExitCode::SUCCESS;
    }
    let (ctx, ring) = match connect(proto::APP) {
        Ok(connected) => connected,
        Err(err) => {
            eprintln!(
                "takt-decode-helper {}: {err}\nThis program is started by Takt; it does nothing on its own. \
                 `--about` prints the FFmpeg it uses.",
                env!("CARGO_PKG_VERSION")
            );
            return ExitCode::from(EXIT_NO_PARENT);
        }
    };
    serve(ctx, ring)
}

/// One line per library and per codec: what this build of the helper decodes with.
fn about() -> String {
    #[cfg(takt_ffmpeg)]
    {
        use std::fmt::Write as _;

        let libs = crate::ffi::libraries();
        let [c, u] = libs.versions;
        let mut text = format!(
            "takt-decode-helper {}\nlibavcodec {}.{}.{}\nlibavutil {}.{}.{}\nlicense: {}",
            env!("CARGO_PKG_VERSION"),
            c.0,
            c.1,
            c.2,
            u.0,
            u.1,
            u.2,
            libs.license
        );
        for (codec, decoder) in proto::CODECS {
            let state = if crate::ffi::has_decoder(decoder) {
                ""
            } else {
                " (missing in this libavcodec)"
            };
            let _ = write!(text, "\n{codec}: decoder {decoder}{state}");
        }
        text
    }
    #[cfg(not(takt_ffmpeg))]
    {
        format!(
            "takt-decode-helper {}\nbuilt without FFmpeg (FFMPEG_DIR was not set at build time)",
            env!("CARGO_PKG_VERSION")
        )
    }
}

/// Without FFmpeg the helper still speaks the protocol and says why it decodes nothing.
#[cfg(not(takt_ffmpeg))]
fn serve(mut ctx: takt_ipc::child::HelperContext, _ring: takt_ipc::child::RingEnd) -> ExitCode {
    // The answer to the configuration, whenever that comes; a parent that says goodbye first
    // gets no answer.
    let seq = loop {
        match ctx.recv(std::time::Duration::from_millis(20)) {
            Some(frame) if frame.kind == proto::kind::CONFIGURE => break frame.seq,
            Some(frame) if frame.kind == takt_ipc::message::kind::BYE => {
                let _ = ctx.send(takt_ipc::message::kind::BYE, 0, &[]);
                return ExitCode::SUCCESS;
            }
            _ => {}
        }
    };
    let _ = ctx.fault(
        seq,
        proto::fault::NO_FFMPEG,
        "built without FFmpeg: set FFMPEG_DIR and build again (docs/BUILD.md)",
    );
    ExitCode::SUCCESS
}

#[cfg(takt_ffmpeg)]
use session::serve;

#[cfg(takt_ffmpeg)]
mod session {
    use std::collections::VecDeque;
    use std::process::ExitCode;
    use std::time::Duration;

    use takt_ipc::ProtocolError;
    use takt_ipc::child::{HelperContext, RingEnd};
    use takt_ipc::frame::Frame;
    use takt_ipc::message::kind as core_kind;
    use takt_ipc::ring::{RingWriter, backoff};

    use crate::ffi::{AvError, Received, Video};
    use takt_decode::proto::{
        self, Config, Configured, MAX_ERROR_TEXT, end_tail, error_tail, parse_flush, parse_packet,
        status_record,
    };

    /// How long the helper sleeps on a request while it has nothing to decode: short against the
    /// heartbeat limit of the parent.
    const IDLE: Duration = Duration::from_millis(20);

    /// Most errors in a row the decoder may report while it is drained, before the helper takes
    /// it for finished.
    const MAX_DRAIN_ERRORS: u32 = 64;

    /// Why the loop stops.
    enum Stop {
        /// The parent said goodbye.
        Bye,
        /// The parent is gone or its side of the ring is impossible.
        Lost,
    }

    /// What became of a record.
    #[derive(PartialEq, Eq)]
    enum Put {
        /// It is in the ring.
        Written,
        /// A flush arrived while the ring was full: the record is worthless and was dropped.
        Abandoned,
        /// It is larger than the ring allows.
        TooLarge,
    }

    pub(super) fn serve(mut ctx: HelperContext, ring: RingEnd) -> ExitCode {
        let RingEnd::Writer(ring) = ring else {
            let _ = ctx.fault(0, proto::fault::REQUEST, "the decoder writes the ring");
            return ExitCode::from(super::EXIT_NO_PARENT);
        };
        let request = loop {
            match ctx.recv(IDLE) {
                Some(frame) if frame.kind == proto::kind::CONFIGURE => break frame,
                Some(frame) if frame.kind == core_kind::BYE => {
                    let _ = ctx.send(core_kind::BYE, 0, &[]);
                    return ExitCode::SUCCESS;
                }
                Some(frame)
                    if ctx
                        .fault(frame.seq, proto::fault::REQUEST, "no decoder is open yet")
                        .is_err() =>
                {
                    return ExitCode::SUCCESS;
                }
                Some(_) | None => {}
            }
        };
        if let Err(why) = crate::ffi::libraries_match() {
            let _ = ctx.fault(request.seq, proto::fault::FFMPEG_VERSION, &why);
            return ExitCode::SUCCESS;
        }
        let Ok(config) = Config::parse(&request.payload) else {
            let _ = ctx.fault(
                request.seq,
                proto::fault::REQUEST,
                "configuration not readable",
            );
            return ExitCode::SUCCESS;
        };
        if proto::decoder_for(&config.codec).is_none() {
            let text = format!("codec {} is not decoded by this helper", config.codec);
            let _ = ctx.fault(request.seq, proto::fault::CODEC, &text);
            return ExitCode::SUCCESS;
        }
        let video = match Video::open(&config) {
            Ok(video) => video,
            Err((code, text)) => {
                let _ = ctx.fault(request.seq, proto::fault::OPEN, &describe(code, &text));
                return ExitCode::SUCCESS;
            }
        };
        let configured = Configured {
            decoder: video.decoder().to_owned(),
        };
        if ctx
            .send(proto::kind::CONFIGURED, request.seq, &configured.encode())
            .is_err()
        {
            return ExitCode::SUCCESS;
        }
        let mut session = Session {
            ctx,
            ring,
            video,
            queue: VecDeque::new(),
            serial: 0,
            consumed: 0,
            drained: false,
        };
        match session.run() {
            Stop::Bye => {
                let _ = session.ctx.send(core_kind::BYE, 0, &[]);
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

    /// The decoder of one stream between its two channels.
    struct Session {
        ctx: HelperContext,
        ring: RingWriter,
        video: Video,
        /// Requests that arrived while the ring was full, in their order.
        queue: VecDeque<Frame>,
        /// The serial of the last flush; every record carries it.
        serial: u32,
        /// Packets taken off the pipe since the start: what the parent's window counts by.
        consumed: u64,
        /// The decoder was told that no more packets come and needs a flush before the next.
        drained: bool,
    }

    impl Session {
        fn run(&mut self) -> Stop {
            loop {
                let frame = match self.queue.pop_front() {
                    Some(frame) => frame,
                    None => {
                        if let Some(frame) = self.ctx.recv(IDLE)
                            && let Err(stop) = self.absorb(frame)
                        {
                            return stop;
                        }
                        continue;
                    }
                };
                if let Err(stop) = self.request(&frame) {
                    return stop;
                }
            }
        }

        /// Takes a frame of the parent into the queue. `Ok(true)` for a flush: everything that
        /// waited in front of it is worthless, and so is the record being written.
        fn absorb(&mut self, frame: Frame) -> Result<bool, Stop> {
            absorb(&mut self.queue, &mut self.consumed, frame)
        }

        /// Writes one record, waiting for room in the ring and listening to the parent meanwhile.
        fn put(&mut self, parts: &[&[u8]]) -> Result<Put, Stop> {
            put_parts(
                &mut self.ring,
                &mut self.ctx,
                &mut self.queue,
                &mut self.consumed,
                parts,
            )
        }

        /// Reports an error of the decoder: the stream goes on.
        fn put_error(&mut self, error: &AvError) -> Result<Put, Stop> {
            let (code, text) = error;
            let room = self
                .ring
                .max_record()
                .saturating_sub(error_tail(0, 0, -1).len());
            let mut end = text.len().min(MAX_ERROR_TEXT).min(room);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let tail = error_tail(self.serial, self.consumed, *code);
            self.put(&[&text.as_bytes()[..end], &tail])
        }

        /// Fetches the pictures the decoder has ready and writes them into the ring. With
        /// `to_the_end` it goes on until the decoder says it has no more (after the end of the
        /// packets). `Ok(None)` when a flush made the rest worthless, otherwise whether a record
        /// was written.
        fn deliver(&mut self, to_the_end: bool) -> Result<Option<bool>, Stop> {
            let mut wrote = false;
            let mut errors = 0u32;
            loop {
                let put = match self.video.receive() {
                    Ok(Received::Frame) => self.put_picture()?,
                    Ok(Received::Again | Received::End) => return Ok(Some(wrote)),
                    Err(error) => {
                        errors += 1;
                        let put = self.put_error(&error)?;
                        if !to_the_end || errors >= MAX_DRAIN_ERRORS {
                            return Ok((put != Put::Abandoned).then_some(true));
                        }
                        put
                    }
                };
                match put {
                    Put::Abandoned => return Ok(None),
                    Put::Written | Put::TooLarge => wrote = true,
                }
            }
        }

        /// Writes the picture the decoder holds, or an error record when it is nothing the
        /// protocol or the ring carries.
        fn put_picture(&mut self) -> Result<Put, Stop> {
            let refused = match self.video.picture() {
                Ok(mut picture) => {
                    picture.head.serial = self.serial;
                    picture.head.consumed = self.consumed;
                    let tail = picture.head.tail();
                    let [luma, cb, cr] = picture.planes;
                    let size = picture.head.body_len();
                    // The ring and the heartbeat are fields of their own: the planes borrow the
                    // decoder while they are written.
                    match put_parts(
                        &mut self.ring,
                        &mut self.ctx,
                        &mut self.queue,
                        &mut self.consumed,
                        &[luma, cb, cr, &tail],
                    )? {
                        Put::TooLarge => format!("a picture of {size} bytes does not fit the ring"),
                        put => {
                            self.video.release();
                            return Ok(put);
                        }
                    }
                }
                Err(why) => why,
            };
            self.video.release();
            match self.put_error(&(-1, refused))? {
                Put::Abandoned => Ok(Put::Abandoned),
                Put::Written | Put::TooLarge => Ok(Put::TooLarge),
            }
        }

        /// Handles one frame of the parent.
        fn request(&mut self, frame: &Frame) -> Result<(), Stop> {
            match frame.kind {
                core_kind::BYE => Err(Stop::Bye),
                // Nothing here runs long enough to be given up.
                core_kind::CANCEL => Ok(()),
                proto::kind::FLUSH => {
                    let Ok(serial) = parse_flush(&frame.payload) else {
                        return self.refuse(frame, "flush request not readable");
                    };
                    self.serial = serial;
                    self.video.flush();
                    self.drained = false;
                    // The new serial and the count reach the parent at once.
                    self.put(&[&status_record(self.serial, self.consumed)])?;
                    Ok(())
                }
                proto::kind::DRAIN => {
                    if !self.drained {
                        self.video.send_end();
                        self.drained = true;
                        if self.deliver(true)?.is_none() {
                            return Ok(());
                        }
                    }
                    self.put(&[&end_tail(self.serial, self.consumed)])?;
                    Ok(())
                }
                proto::kind::PACKET => {
                    self.consumed += 1;
                    self.packet(&frame.payload)
                }
                _ => self.refuse(frame, "unknown request"),
            }
        }

        fn refuse(&mut self, frame: &Frame, why: &str) -> Result<(), Stop> {
            self.ctx
                .fault(frame.seq, proto::fault::REQUEST, why)
                .map_err(|_| Stop::Lost)
        }

        /// Decodes one packet and writes what comes out of the decoder.
        fn packet(&mut self, payload: &[u8]) -> Result<(), Stop> {
            let wrote = match parse_packet(payload) {
                // A packet from before the flush this side has already made.
                Ok((_, head)) if head.serial != self.serial => false,
                Ok((data, head)) => {
                    if self.drained {
                        // After the end of the packets the decoder takes none before a flush.
                        self.video.flush();
                        self.drained = false;
                    }
                    let mut wrote = false;
                    loop {
                        match self.video.send(data, &head) {
                            Ok(true) => break,
                            // The decoder wants its pictures fetched first.
                            Ok(false) => match self.deliver(false)? {
                                None => return Ok(()),
                                Some(true) => wrote = true,
                                // Neither a packet taken nor a picture given: it would spin.
                                Some(false) => {
                                    let stuck = (-1, "the decoder takes no packet".to_owned());
                                    if self.put_error(&stuck)? == Put::Abandoned {
                                        return Ok(());
                                    }
                                    wrote = true;
                                    break;
                                }
                            },
                            Err(error) => {
                                if self.put_error(&error)? == Put::Abandoned {
                                    return Ok(());
                                }
                                wrote = true;
                                break;
                            }
                        }
                    }
                    match self.deliver(false)? {
                        None => return Ok(()),
                        Some(more) => wrote || more,
                    }
                }
                Err(_) => {
                    let unreadable = (-1, "packet not readable".to_owned());
                    if self.put_error(&unreadable)? == Put::Abandoned {
                        return Ok(());
                    }
                    true
                }
            };
            if !wrote {
                // No record carried the count: the window of the parent must hear of the packet.
                self.put(&[&status_record(self.serial, self.consumed)])?;
            }
            Ok(())
        }
    }

    /// [`Session::put`] for a record whose parts borrow the decoder: the same loop over the
    /// fields it needs.
    fn put_parts(
        ring: &mut RingWriter,
        ctx: &mut HelperContext,
        queue: &mut VecDeque<Frame>,
        consumed: &mut u64,
        parts: &[&[u8]],
    ) -> Result<Put, Stop> {
        let mut attempt = 0u32;
        loop {
            match ring.try_write(parts) {
                Ok(true) => {
                    ctx.beat();
                    return Ok(Put::Written);
                }
                Ok(false) => {}
                Err(ProtocolError::TooLarge { .. }) => return Ok(Put::TooLarge),
                Err(_) => return Err(Stop::Lost),
            }
            let wait = backoff(attempt).unwrap_or(Duration::ZERO);
            if wait.is_zero() {
                std::thread::yield_now();
            }
            if let Some(frame) = ctx.recv(wait)
                && absorb(queue, consumed, frame)?
            {
                return Ok(Put::Abandoned);
            }
            attempt = attempt.saturating_add(1);
        }
    }

    /// Takes a frame of the parent into the queue. `Ok(true)` for a flush: everything that
    /// waited in front of it is worthless, and so is the record being written.
    fn absorb(queue: &mut VecDeque<Frame>, consumed: &mut u64, frame: Frame) -> Result<bool, Stop> {
        match frame.kind {
            core_kind::BYE => Err(Stop::Bye),
            proto::kind::FLUSH => {
                // The packets in front of the flush are taken, if not decoded: the window of the
                // parent counts them.
                let packets = queue
                    .iter()
                    .filter(|waiting| waiting.kind == proto::kind::PACKET)
                    .count();
                *consumed += packets as u64;
                queue.clear();
                queue.push_back(frame);
                Ok(true)
            }
            _ => {
                queue.push_back(frame);
                Ok(false)
            }
        }
    }
}
