// SPDX-License-Identifier: GPL-3.0-or-later
//! The demuxer as the main program sees it: [`Demuxer`] starts the helper for one file and hands
//! out its packets.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use takt_core::{Rational, Timestamp};
use takt_ipc::message::{Fault, kind as core_kind};
use takt_ipc::process::{Helper, HelperCommand, Limits};
use takt_ipc::ring::{RingReader, pause};
use takt_ipc::{IpcError, ProtocolError};

use crate::proto::{self, MediaInfo, Packet, Record, Seek};

/// File name of the helper program, without the `.exe` of Windows.
pub const HELPER_NAME: &str = "takt-demux-helper";

/// Buffers of handed-back packets kept for the next ones.
const SPARE_BUFFERS: usize = 16;

/// How a file is opened.
#[derive(Debug, Clone)]
pub struct Options {
    /// The helper program.
    pub helper: PathBuf,
    /// The bounds of the helper. [`Limits::max_record`] minus the 44 bytes of a packet's tail is
    /// the largest packet that is passed on; a larger one arrives as [`Event::Dropped`].
    pub limits: Limits,
    /// Bytes of the ring the packets travel through, a power of two. The helper reads this far
    /// ahead of the decoder.
    pub ring_capacity: usize,
    /// How long the helper has to open the file and to find its streams.
    pub open_timeout: Duration,
    /// How long the helper has to answer a seek.
    pub seek_timeout: Duration,
}

impl Options {
    /// The defaults for the helper at `helper`: a ring of 16 MiB, packets up to 8 MiB, 2 GiB of
    /// memory for the helper, 30 s to open a file.
    #[must_use]
    pub fn new(helper: impl Into<PathBuf>) -> Self {
        Self {
            helper: helper.into(),
            limits: Limits {
                max_frame: 4 << 20,
                max_record: 8 << 20,
                memory: Some(2 << 30),
                ..Limits::default()
            },
            ring_capacity: 16 << 20,
            open_timeout: Duration::from_secs(30),
            seek_timeout: Duration::from_secs(10),
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
}

/// Why the demuxer has no packet.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DemuxError {
    /// The file could not be opened by this program. The text names the reason.
    File(String),
    /// The helper refused: no container it knows, a seek that failed, built without FFmpeg. The
    /// code is one of [`proto::fault`]; the text is the helper's (foreign text, tamed).
    Refused {
        /// One of [`proto::fault`].
        code: u32,
        /// The reason, for a person.
        text: String,
    },
    /// The helper is gone: it crashed, hung, lied or went over a limit, and was ended. The demuxer
    /// is finished; open the file again for a new helper.
    Helper(IpcError),
    /// A time that does not fit the protocol (beyond 292 000 years in microseconds).
    Time,
}

impl std::fmt::Display for DemuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(why) => write!(f, "file not opened: {why}"),
            Self::Refused { code, text } => write!(f, "demuxer refused ({code}): {text}"),
            Self::Helper(err) => write!(f, "demux helper: {err}"),
            Self::Time => f.write_str("time out of range"),
        }
    }
}

impl std::error::Error for DemuxError {}

impl From<IpcError> for DemuxError {
    fn from(err: IpcError) -> Self {
        Self::Helper(err)
    }
}

impl From<DemuxError> for takt_core::Error {
    fn from(err: DemuxError) -> Self {
        match err {
            DemuxError::File(_) | DemuxError::Refused { .. } => {
                Self::SourceUnavailable(err.to_string())
            }
            DemuxError::Helper(_) => Self::Backend(err.to_string()),
            DemuxError::Time => Self::TimeOverflow,
        }
    }
}

/// What the demuxer hands out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The next packet, in the order of the file.
    Packet(Packet),
    /// No more packets until the next seek: the end of the file (`error` is `None`) or a read
    /// error in the middle of it (FFmpeg's error number and text). Handed out once.
    End {
        /// `None` at the end of the file.
        error: Option<(i32, String)>,
    },
    /// A packet of `size` bytes of stream `stream` was left out: it is larger than
    /// [`Options::limits`] allow. The stream goes on with the next packet.
    Dropped {
        /// Index of the stream.
        stream: u32,
        /// Size of the packet in bytes, as the helper states it.
        size: u64,
    },
}

/// One open file: the helper process that parses it and the ring its packets arrive through.
///
/// Dropping the demuxer ends the helper.
#[derive(Debug)]
pub struct Demuxer {
    helper: Helper,
    ring: RingReader,
    info: MediaInfo,
    /// The serial of the last seek; records with another one are from before it.
    serial: u32,
    seq: u32,
    seek_timeout: Duration,
    scratch: Vec<u8>,
    spare: Vec<Vec<u8>>,
    /// Set when the helper was caught lying: nothing it wrote is handed out any more.
    failed: Option<DemuxError>,
}

/// Smallest record limit a demuxer can work with: the tail of a packet and a little data.
const MIN_RECORD: usize = 64;

impl Demuxer {
    /// Opens the file at `path` in this process and hands it to a new helper.
    ///
    /// # Errors
    /// [`DemuxError::File`] when the file cannot be opened, otherwise as [`Demuxer::open`].
    pub fn open_path(path: impl AsRef<Path>, options: &Options) -> Result<Self, DemuxError> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|e| DemuxError::File(e.to_string()))?;
        Self::open(file, options)
    }

    /// Starts a helper for `file` and waits until it has found the streams.
    ///
    /// The helper gets the open file and nothing else: no path, no name. It reads the file from
    /// wherever `file` stands.
    ///
    /// # Errors
    /// [`DemuxError::Refused`] when the helper cannot read the file as a container,
    /// [`DemuxError::Helper`] when the helper could not be started or did not survive the file.
    pub fn open(file: File, options: &Options) -> Result<Self, DemuxError> {
        if options.limits.max_record < MIN_RECORD {
            return Err(DemuxError::Helper(IpcError::Spawn(format!(
                "a record limit of {} bytes is below the {MIN_RECORD} a demuxer needs",
                options.limits.max_record
            ))));
        }
        let (mut helper, ring) = HelperCommand::new(&options.helper, proto::APP)
            .limits(options.limits.clone())
            .ring_capacity(options.ring_capacity)
            .file(file)
            .spawn_reading()
            .map_err(missing_libraries)?;
        let first = helper
            .recv(options.open_timeout)
            .map_err(missing_libraries)?;
        let info = match first.kind {
            proto::kind::OPENED => {
                MediaInfo::parse(&first.payload).map_err(|e| condemn(&helper, e))?
            }
            core_kind::FAULT => {
                let fault = Fault::parse(&first.payload).map_err(|e| condemn(&helper, e))?;
                return Err(DemuxError::Refused {
                    code: fault.code,
                    text: fault.text,
                });
            }
            _ => {
                return Err(condemn(
                    &helper,
                    ProtocolError::Invalid("first frame of the demuxer"),
                ));
            }
        };
        Ok(Self {
            helper,
            ring,
            info,
            serial: 0,
            seq: 0,
            seek_timeout: options.seek_timeout,
            scratch: Vec::new(),
            spare: Vec::new(),
            failed: None,
        })
    }

    /// What the file is and which streams it has.
    #[must_use]
    pub const fn info(&self) -> &MediaInfo {
        &self.info
    }

    /// The next event if one is there. `Ok(None)` when the helper has not delivered the next packet
    /// yet, and after an [`Event::End`] until the next seek.
    ///
    /// Does not wait and does not lock; made to be polled by the thread that feeds a decoder.
    ///
    /// # Errors
    /// [`DemuxError::Helper`] when the helper is gone. Records it wrote before are handed out
    /// first.
    pub fn try_next(&mut self) -> Result<Option<Event>, DemuxError> {
        loop {
            // A helper that lied once is not listened to again: what else is in the ring comes
            // from the same process.
            if let Some(err) = &self.failed {
                return Err(err.clone());
            }
            match self.ring.try_read(&mut self.scratch) {
                Ok(true) => {}
                Ok(false) => {
                    self.helper.check()?;
                    return Ok(None);
                }
                Err(err) => return Err(self.fail(err)),
            }
            let parsed = proto::parse_record(&mut self.scratch, &self.info.streams);
            let (serial, record) = match parsed {
                Ok(parsed) => parsed,
                Err(err) => return Err(self.fail(err)),
            };
            let current = serial == self.serial;
            let event = match record {
                Record::Packet(packet) => {
                    // The packet took the buffer of the record.
                    self.scratch = self.spare.pop().unwrap_or_default();
                    if !current {
                        self.recycle(packet);
                        continue;
                    }
                    Event::Packet(packet)
                }
                Record::End { error } => Event::End { error },
                Record::Dropped { stream, size } => Event::Dropped { stream, size },
            };
            if current {
                return Ok(Some(event));
            }
        }
    }

    /// The next event, waiting at most `timeout` for it. `Ok(None)` when none came in that time;
    /// that alone says nothing about the helper, whose life the watchdog guards.
    ///
    /// # Errors
    /// As [`try_next`](Self::try_next).
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Event>, DemuxError> {
        let deadline = Instant::now() + timeout;
        let mut attempt = 0;
        loop {
            if let Some(event) = self.try_next()? {
                return Ok(Some(event));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            pause(attempt);
            attempt = attempt.saturating_add(1);
        }
    }

    /// Positions the file: the next packets start at the last keyframe at or before `to`, a time
    /// on the same axis as the timestamps of the packets. Packets from before the seek that are
    /// still on their way are thrown away.
    ///
    /// Frame-exact positioning is the decoder's part: decode from that keyframe and drop what
    /// lies before `to`.
    ///
    /// # Errors
    /// [`DemuxError::Refused`] when the container cannot seek there; the demuxer then reads on
    /// where it was. [`DemuxError::Helper`] when the helper is gone.
    pub fn seek(&mut self, to: Timestamp) -> Result<(), DemuxError> {
        if let Some(err) = &self.failed {
            return Err(err.clone());
        }
        let target_micros = to
            .rescale(Rational::MICROS)
            .ok_or(DemuxError::Time)?
            .ticks();
        let serial = self.serial.wrapping_add(1);
        let seq = self.seq.wrapping_add(1);
        let seek = Seek {
            serial,
            target_micros,
        };
        if seek.target_micros == i64::MIN {
            return Err(DemuxError::Time);
        }
        self.helper.send(proto::kind::SEEK, seq, &seek.encode())?;
        // From here on the helper writes the new serial, whether the seek works or not.
        self.serial = serial;
        self.seq = seq;
        let reply = self.helper.recv(self.seek_timeout)?;
        if reply.seq != seq {
            return Err(self.fail(ProtocolError::Invalid("answer to another request")));
        }
        match reply.kind {
            proto::kind::SEEK_DONE if reply.payload.is_empty() => Ok(()),
            core_kind::FAULT => match Fault::parse(&reply.payload) {
                Ok(fault) => Err(DemuxError::Refused {
                    code: fault.code,
                    text: fault.text,
                }),
                Err(err) => Err(self.fail(err)),
            },
            _ => Err(self.fail(ProtocolError::Invalid("answer to a seek"))),
        }
    }

    /// Ends the helper for a lie and remembers it: every later call returns this error.
    fn fail(&mut self, err: ProtocolError) -> DemuxError {
        let err = condemn(&self.helper, err);
        self.failed = Some(err.clone());
        err
    }

    /// Hands the buffer of a packet back, so the next packet needs no allocation.
    pub fn recycle(&mut self, packet: Packet) {
        if self.spare.len() < SPARE_BUFFERS {
            let mut data = packet.data;
            data.clear();
            self.spare.push(data);
        }
    }

    /// Bytes of packets that wait in the ring: how far the helper is ahead.
    ///
    /// # Errors
    /// [`DemuxError::Helper`] when the helper's counter is impossible; the helper is ended.
    pub fn buffered(&mut self) -> Result<usize, DemuxError> {
        if let Some(err) = &self.failed {
            return Err(err.clone());
        }
        match self.ring.pending() {
            Ok(bytes) => Ok(bytes),
            Err(err) => Err(self.fail(err)),
        }
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
fn missing_libraries(err: IpcError) -> DemuxError {
    /// `STATUS_DLL_NOT_FOUND` as the exit code of a process.
    const DLL_NOT_FOUND: i32 = 0xC000_0135_u32.cast_signed();
    match err {
        IpcError::Exited {
            code: Some(DLL_NOT_FOUND),
            ..
        } => DemuxError::Refused {
            code: proto::fault::NO_FFMPEG,
            text:
                "the helper did not start: the FFmpeg libraries were not found (PATH, or next to \
                   the helper; docs/BUILD.md)"
                    .to_owned(),
        },
        other => DemuxError::Helper(other),
    }
}

/// Ends a helper that sent something impossible and makes the error of it.
fn condemn(helper: &Helper, err: ProtocolError) -> DemuxError {
    let reason = IpcError::Protocol(err);
    helper.condemn(reason.clone());
    DemuxError::Helper(reason)
}
