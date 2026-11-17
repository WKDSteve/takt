// SPDX-License-Identifier: GPL-3.0-or-later
//! The decoder as the main program sees it: [`Decoder`] starts the helper for one video stream,
//! feeds it packets and hands out pictures in frames of the pool.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use takt_demux::{Packet, StreamInfo, StreamKind};
use takt_ipc::message::{Fault, kind as core_kind};
use takt_ipc::process::{Helper, HelperCommand, Limits};
use takt_ipc::ring::{RingReader, pause};
use takt_ipc::{IpcError, ProtocolError};

use crate::intake::{DecodeStats, Decoded, Intake, Taken};
use crate::proto::{self, Config, Configured, Layout, PACKET_TAIL, PacketHead};

/// File name of the helper program, without the `.exe` of Windows.
pub const HELPER_NAME: &str = "takt-decode-helper";

/// Smallest and largest ring a decoder asks for.
const MIN_RING: usize = 16 << 20;
const MAX_RING: usize = 1 << 30;

/// How a stream is decoded.
#[derive(Debug, Clone)]
pub struct Options {
    /// The helper program.
    pub helper: PathBuf,
    /// The bounds of the helper. [`Limits::max_frame`] minus the 32 bytes of a packet's tail is
    /// the largest packet that is sent; [`Limits::max_record`] is set from the ring when the
    /// decoder opens.
    pub limits: Limits,
    /// Bytes of the ring the pictures travel through, a power of two; `None` for three pictures
    /// of the stream's size (at least 16 MiB). A picture must fit the ring to arrive at all.
    pub ring_capacity: Option<usize>,
    /// How long the helper has to open the decoder.
    pub open_timeout: Duration,
    /// Frames of the pool the pictures land in. All of them out means the consumer does not keep
    /// up: the decoder waits and counts it, it never allocates more.
    pub pool_frames: usize,
    /// How the pictures are laid out in the frames.
    pub layout: Layout,
    /// Packets that may be on their way to the helper at once.
    pub window: u32,
    /// Threads of the decoder, 0 for as many as the machine has cores.
    pub threads: u32,
}

impl Options {
    /// The defaults for the helper at `helper`: packets up to 8 MiB, 3 GiB of memory for the
    /// helper, a pool of 8 frames, 8 packets on their way, planar frames.
    #[must_use]
    pub fn new(helper: impl Into<PathBuf>) -> Self {
        Self {
            helper: helper.into(),
            limits: Limits {
                max_frame: (8 << 20) + PACKET_TAIL,
                memory: Some(3 << 30),
                ..Limits::default()
            },
            ring_capacity: None,
            open_timeout: Duration::from_secs(10),
            pool_frames: 8,
            layout: Layout::Planar,
            window: 8,
            threads: 0,
        }
    }

    /// The defaults for the helper that lies next to the running program, which is where an
    /// install puts it.
    ///
    /// # Errors
    /// When the path of the running program is not known.
    pub fn beside_current_exe() -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().unwrap_or_else(|| Path::new("."));
        Ok(Self::new(dir.join(format!(
            "{HELPER_NAME}{}",
            std::env::consts::EXE_SUFFIX
        ))))
    }

    /// The ring for pictures of `width` by `height` pixels: room for three of them in the largest
    /// format the protocol carries (three bytes a pixel), a power of two.
    fn ring_for(&self, width: u32, height: u32) -> usize {
        self.ring_capacity.unwrap_or_else(|| {
            let picture = (width as usize)
                .saturating_mul(height as usize)
                .saturating_mul(3);
            picture
                .saturating_mul(3)
                .clamp(MIN_RING, MAX_RING)
                .next_power_of_two()
        })
    }
}

/// Why the decoder has no picture.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The stream is nothing the software path decodes: no video, or a codec that is not on the
    /// list ([`proto::CODECS`]). The text names it.
    Unsupported(String),
    /// The helper refused: the decoder did not open, built without FFmpeg. The code is one of
    /// [`proto::fault`]; the text is the helper's (foreign text, tamed).
    Refused {
        /// One of [`proto::fault`].
        code: u32,
        /// The reason, for a person.
        text: String,
    },
    /// The helper is gone: it crashed, hung, lied or went over a limit, and was ended. The
    /// decoder is finished; open a new one.
    Helper(IpcError),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(what) => write!(f, "not decoded by the software path: {what}"),
            Self::Refused { code, text } => write!(f, "decoder refused ({code}): {text}"),
            Self::Helper(err) => write!(f, "decode helper: {err}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<IpcError> for DecodeError {
    fn from(err: IpcError) -> Self {
        Self::Helper(err)
    }
}

impl From<DecodeError> for takt_core::Error {
    fn from(err: DecodeError) -> Self {
        match err {
            DecodeError::Unsupported(_) | DecodeError::Refused { .. } => {
                Self::SourceUnavailable(err.to_string())
            }
            DecodeError::Helper(_) => Self::Backend(err.to_string()),
        }
    }
}

/// What became of a packet handed to [`Decoder::send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// It is on its way to the helper.
    Taken,
    /// As many packets as [`Options::window`] allows are on their way: fetch pictures and offer
    /// it again.
    WindowFull,
    /// It is larger than a frame of the pipe may be and was left out.
    TooLarge,
}

/// What [`Decoder::try_next`] found.
#[derive(Debug)]
pub enum Next {
    /// The next picture, in the order the decoder puts them out (the order of presentation).
    Frame(Decoded),
    /// The decoder has handed out everything after [`Decoder::drain`]. Handed out once.
    End,
    /// The helper has not delivered the next picture yet.
    Idle,
    /// A picture is ready, and every frame of the pool is out: it waits in the ring until one
    /// comes back. Nothing is allocated in its place.
    PoolExhausted,
}

/// One video stream being decoded: the helper process and the ring its pictures arrive through.
///
/// Dropping the decoder ends the helper.
#[derive(Debug)]
pub struct Decoder {
    helper: Helper,
    ring: RingReader,
    intake: Intake,
    decoder: String,
    window: u64,
    max_packet: usize,
    scratch: Vec<u8>,
    out: Vec<u8>,
    /// Set when the helper was caught lying: nothing it wrote is handed out any more.
    failed: Option<DecodeError>,
}

impl Decoder {
    /// Starts a helper for the video stream `stream` of a demuxer and waits until it has opened
    /// its decoder.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a stream the software path does not decode,
    /// [`DecodeError::Refused`] when the helper cannot open the decoder, [`DecodeError::Helper`]
    /// when the helper could not be started or did not survive.
    pub fn open(stream: &StreamInfo, options: &Options) -> Result<Self, DecodeError> {
        let Some(video) = stream
            .video
            .as_ref()
            .filter(|_| stream.kind == StreamKind::Video)
        else {
            return Err(DecodeError::Unsupported(format!(
                "stream {} is no video stream",
                stream.index
            )));
        };
        if proto::decoder_for(&stream.codec).is_none() {
            return Err(DecodeError::Unsupported(format!("codec {}", stream.codec)));
        }
        let config = Config {
            codec: stream.codec.clone(),
            extradata: stream.extradata.clone(),
            width: video.width.min(proto::MAX_DIMENSION),
            height: video.height.min(proto::MAX_DIMENSION),
            profile: stream.profile,
            level: stream.level,
            threads: options.threads.min(proto::MAX_THREADS),
        };
        let ring_capacity = options.ring_for(config.width, config.height);
        let mut limits = options.limits.clone();
        // A picture may fill the ring; `attach` lowers the limit to what the ring holds.
        limits.max_record = ring_capacity;
        let (mut helper, ring) = HelperCommand::new(&options.helper, proto::APP)
            .limits(limits)
            .ring_capacity(ring_capacity)
            .spawn_reading()
            .map_err(missing_libraries)?;
        let payload = config.encode();
        helper.send(proto::kind::CONFIGURE, 1, &payload)?;
        let reply = helper
            .recv(options.open_timeout)
            .map_err(missing_libraries)?;
        let configured = match reply.kind {
            proto::kind::CONFIGURED => {
                Configured::parse(&reply.payload).map_err(|e| condemn(&helper, e))?
            }
            core_kind::FAULT => {
                let fault = Fault::parse(&reply.payload).map_err(|e| condemn(&helper, e))?;
                return Err(DecodeError::Refused {
                    code: fault.code,
                    text: fault.text,
                });
            }
            _ => {
                return Err(condemn(
                    &helper,
                    ProtocolError::Invalid("first frame of the decoder"),
                ));
            }
        };
        let max_packet = helper.limits().max_frame.saturating_sub(PACKET_TAIL);
        let mut intake = Intake::new(stream.time_base, options.layout, options.pool_frames);
        // A code point is a small number; anything else says nothing.
        let code = |value: u32, unknown: u8| u8::try_from(value).unwrap_or(unknown);
        intake.container_color(
            code(video.color_range, 0),
            code(video.color_primaries, 2),
            code(video.color_transfer, 2),
            code(video.color_matrix, 2),
        );
        Ok(Self {
            helper,
            ring,
            intake,
            decoder: configured.decoder,
            window: u64::from(options.window.max(1)),
            max_packet,
            scratch: Vec::new(),
            out: Vec::new(),
            failed: None,
        })
    }

    /// libavcodec's name of the decoder the helper opened (`h264`, `hevc`, `libdav1d`).
    #[must_use]
    pub fn decoder_name(&self) -> &str {
        &self.decoder
    }

    /// The counters.
    #[must_use]
    pub const fn stats(&self) -> DecodeStats {
        self.intake.stats()
    }

    /// Frames of the pool that are out, and its size.
    #[must_use]
    pub fn pool(&self) -> (usize, usize) {
        self.intake.pool()
    }

    /// The last error the decoder reported for a packet: the library's number and text.
    #[must_use]
    pub fn last_error(&self) -> Option<&(i32, String)> {
        self.intake.last_error()
    }

    /// Whether [`send`](Self::send) would take a packet now.
    #[must_use]
    pub const fn can_send(&self) -> bool {
        self.intake.in_flight() < self.window
    }

    /// Sends one packet of the stream to the helper, unless the window is full.
    ///
    /// # Errors
    /// [`DecodeError::Helper`] when the helper is gone.
    pub fn send(&mut self, packet: &Packet) -> Result<Sent, DecodeError> {
        if let Some(err) = &self.failed {
            return Err(err.clone());
        }
        if !self.can_send() {
            return Ok(Sent::WindowFull);
        }
        if packet.data.len() > self.max_packet {
            return Ok(Sent::TooLarge);
        }
        let head = PacketHead {
            serial: self.intake.serial(),
            pts: packet.pts.map(takt_core::Timestamp::ticks),
            dts: packet.dts.map(takt_core::Timestamp::ticks),
            duration: packet.duration.map(takt_core::Timestamp::ticks),
            key: packet.key,
            corrupt: packet.corrupt,
            discard: packet.discard,
        };
        self.out.clear();
        self.out.extend_from_slice(&packet.data);
        self.out.extend_from_slice(&head.tail());
        self.helper.send(proto::kind::PACKET, 0, &self.out)?;
        self.intake.sent();
        Ok(Sent::Taken)
    }

    /// Says that no more packets come until the next flush: the helper hands out the pictures
    /// the decoder still holds, then [`Next::End`].
    ///
    /// # Errors
    /// [`DecodeError::Helper`] when the helper is gone.
    pub fn drain(&mut self) -> Result<(), DecodeError> {
        if let Some(err) = &self.failed {
            return Err(err.clone());
        }
        self.helper.send(proto::kind::DRAIN, 0, &[])?;
        Ok(())
    }

    /// Drops everything the decoder holds (a jump): pictures of packets sent before this call
    /// never come out any more. Send the packets from the new position next, starting with a
    /// keyframe.
    ///
    /// # Errors
    /// [`DecodeError::Helper`] when the helper is gone.
    pub fn flush(&mut self) -> Result<(), DecodeError> {
        if let Some(err) = &self.failed {
            return Err(err.clone());
        }
        let serial = self.intake.flush();
        self.helper
            .send(proto::kind::FLUSH, 0, &proto::flush(serial))?;
        Ok(())
    }

    /// Pictures with a timestamp before `before` (in the time base of the stream) are counted
    /// ([`DecodeStats::skipped`]) and not copied into a frame, until the next flush: the pictures
    /// between the keyframe a jump landed on and its target. Call it after
    /// [`flush`](Self::flush).
    pub const fn skip_before(&mut self, before: Option<takt_core::Timestamp>) {
        self.intake.skip_before(match before {
            Some(time) => Some(time.ticks()),
            None => None,
        });
    }

    /// The next picture if one is there.
    ///
    /// Does not wait; made to be polled by the thread that feeds the decoder. The picture is
    /// copied out of the shared memory into a frame of the pool before anything looks at it.
    ///
    /// # Errors
    /// [`DecodeError::Helper`] when the helper is gone. Records it wrote before are handed out
    /// first.
    pub fn try_next(&mut self) -> Result<Next, DecodeError> {
        loop {
            // A helper that lied once is not listened to again: what else is in the ring comes
            // from the same process.
            if let Some(err) = &self.failed {
                return Err(err.clone());
            }
            match self.ring.pending() {
                Ok(0) => {
                    self.helper.check()?;
                    return Ok(Next::Idle);
                }
                Ok(_) => {}
                Err(err) => return Err(self.fail(err)),
            }
            // The frame first, the record after: a picture without a frame stays in the ring.
            if !self.intake.ready() {
                return Ok(Next::PoolExhausted);
            }
            let started = Instant::now();
            match self.ring.try_read(&mut self.scratch) {
                Ok(true) => self
                    .intake
                    .read_took(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)),
                Ok(false) => return Ok(Next::Idle),
                Err(err) => return Err(self.fail(err)),
            }
            match self.intake.accept(&self.scratch) {
                Ok(Taken::Frame(decoded)) => return Ok(Next::Frame(decoded)),
                Ok(Taken::End) => return Ok(Next::End),
                Ok(Taken::Nothing) => {}
                Err(err) => return Err(self.fail(err)),
            }
        }
    }

    /// The next picture or the end, waiting at most `timeout` for it; [`Next::Idle`] or
    /// [`Next::PoolExhausted`] when neither came in that time.
    ///
    /// # Errors
    /// As [`try_next`](Self::try_next).
    pub fn next(&mut self, timeout: Duration) -> Result<Next, DecodeError> {
        let deadline = Instant::now() + timeout;
        let mut attempt = 0;
        loop {
            let next = self.try_next()?;
            if matches!(next, Next::Frame(_) | Next::End) || Instant::now() >= deadline {
                return Ok(next);
            }
            pause(attempt);
            attempt = attempt.saturating_add(1);
        }
    }

    /// Ends the helper for a lie and remembers it: every later call returns this error.
    fn fail(&mut self, err: ProtocolError) -> DecodeError {
        let err = condemn(&self.helper, err);
        self.failed = Some(err.clone());
        err
    }

    /// The process id of the helper.
    #[must_use]
    pub const fn helper_pid(&self) -> u32 {
        self.helper.pid()
    }

    /// Ends the helper in good order and returns its exit code, if it left by itself.
    #[must_use]
    pub fn close(self) -> Option<i32> {
        self.helper.shutdown()
    }
}

/// A helper that died while Windows was loading it for want of a DLL is not a helper that
/// crashed: it is a build with FFmpeg whose libraries are not where the system looks for them.
fn missing_libraries(err: IpcError) -> DecodeError {
    /// `STATUS_DLL_NOT_FOUND` as the exit code of a process.
    const DLL_NOT_FOUND: i32 = 0xC000_0135_u32.cast_signed();
    match err {
        IpcError::Exited {
            code: Some(DLL_NOT_FOUND),
            ..
        } => DecodeError::Refused {
            code: proto::fault::NO_FFMPEG,
            text:
                "the helper did not start: the FFmpeg libraries were not found (PATH, or next to \
                   the helper; docs/BUILD.md)"
                    .to_owned(),
        },
        other => DecodeError::Helper(other),
    }
}

/// Ends a helper that sent something impossible and makes the error of it.
fn condemn(helper: &Helper, err: ProtocolError) -> DecodeError {
    let reason = IpcError::Protocol(err);
    helper.condemn(reason.clone());
    DecodeError::Helper(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_holds_three_pictures_and_is_a_power_of_two() {
        let options = Options::new("helper");
        // Small pictures: the smallest ring.
        assert_eq!(options.ring_for(320, 240), 16 << 20);
        // 1080p: three pictures of 6 MiB each, rounded up.
        assert_eq!(options.ring_for(1920, 1080), 32 << 20);
        // 4K: 3 * 24 MiB, rounded up.
        assert_eq!(options.ring_for(3840, 2160), 128 << 20);
        // Unknown size, and a size no ring is made for.
        assert_eq!(options.ring_for(0, 0), 16 << 20);
        assert_eq!(options.ring_for(u32::MAX, u32::MAX), 1 << 30);
        let fixed = Options {
            ring_capacity: Some(1 << 22),
            ..options
        };
        assert_eq!(fixed.ring_for(3840, 2160), 1 << 22);
    }
}
