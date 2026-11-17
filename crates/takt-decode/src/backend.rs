// SPDX-License-Identifier: GPL-3.0-or-later
//! The software path behind [`VideoBackend`]: a demuxer, a decoder and the thread between them.
//!
//! [`SoftwareBackend`] is the second backend of the player next to libmpv, and the reference the
//! hardware path will be checked against. It plays the first video stream of a file; sound,
//! subtitles and the choice of tracks are not its business yet (the clock it is asked with is
//! the caller's).
//!
//! # Threads
//!
//! [`VideoBackend::next_frame`] must not block, and copying a picture out of the shared memory
//! takes time that grows with the picture. So one thread per open file (`takt-decode-pump`) does
//! all of it: it moves packets from the demuxer to the decoder, fetches the pictures and hands
//! them on through a channel of a few frames. `next_frame` only looks into that channel and at
//! the clock. The thread is the single owner of both helper processes; ending the backend ends
//! the thread, and the thread ends the helpers.
//!
//! The pool is larger than the channel, so a consumer that returns its frames never sees the pool
//! empty; one that holds them makes the decoder wait, and the wait is counted
//! ([`PipelineStats::decode`]).

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use takt_backend::{BackendStats, MediaInfo, SeekMode, VideoBackend};
use takt_clock::Clock;
use takt_core::{ColorDescription, FrameMeta, PooledFrame, Rational, Timestamp};
use takt_demux::{Demuxer, Event, Packet, StreamInfo, StreamKind};
use takt_ipc::ring::pause;

use crate::client::{Decoder, Next, Options, Sent};
use crate::intake::{DecodeStats, Decoded};
use crate::proto::{self, WireFormat};

/// FFmpeg's disposition bit of a stream that is a cover picture, not the film.
const DISPOSITION_ATTACHED_PIC: u32 = 0x400;

/// Packets the thread moves in one pass before it looks for pictures again.
const PACKETS_PER_PASS: usize = 16;

/// How a file is played by the software path.
#[derive(Debug, Clone)]
pub struct BackendOptions {
    /// How the file is demuxed.
    pub demux: takt_demux::Options,
    /// How its picture is decoded.
    pub decode: Options,
    /// Pictures decoded ahead of the clock and waiting to be shown, at least 1. The pool
    /// ([`Options::pool_frames`]) must hold these and the frames the caller keeps.
    pub queue: usize,
}

impl BackendOptions {
    /// The defaults for the two helper programs in the directory `dir`.
    #[must_use]
    pub fn in_dir(dir: &Path) -> Self {
        let exe = |name: &str| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        Self {
            demux: takt_demux::Options::new(exe(takt_demux::HELPER_NAME)),
            decode: Options::new(exe(crate::HELPER_NAME)),
            queue: 3,
        }
    }

    /// The defaults for the helpers that lie next to the running program, which is where an
    /// install puts them.
    ///
    /// # Errors
    /// When the path of the running program is not known.
    pub fn beside_current_exe() -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        Ok(Self::in_dir(exe.parent().unwrap_or_else(|| Path::new("."))))
    }
}

/// The counters of the software path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineStats {
    /// The counters of the decoder, [`DecodeStats::pool_exhausted`] among them.
    pub decode: DecodeStats,
    /// Packets left out because they are larger than the demuxer or the decoder allows.
    pub oversized: u64,
    /// Pictures the stream gave no timestamp; they follow the one before at the frame rate.
    pub untimed: u64,
    /// libavcodec's name of the decoder.
    pub decoder: String,
    /// Matrix coefficients (ITU-T H.273) of the last picture, 2 for unspecified.
    pub matrix: u8,
    /// How the file ended when it did not end at its end: the read error of the demuxer.
    pub read_error: Option<String>,
}

/// What the thread tells the backend.
enum Message {
    /// A picture of the jump `generation`.
    Frame(u64, Decoded),
    /// No more pictures until the next jump.
    End(u64),
    /// The pipeline is gone.
    Failed(String),
}

/// What the backend tells the thread.
enum Command {
    /// Go to `to` (a time of the file, microseconds) and hand out no picture before `first`
    /// (stream ticks); pictures from here on belong to `generation`.
    Seek {
        to: Timestamp,
        first: Option<Timestamp>,
        generation: u64,
    },
}

/// What both sides read without asking each other.
#[derive(Default)]
struct Shared {
    decoded: AtomicU64,
    pool_out: AtomicUsize,
    stats: Mutex<PipelineStats>,
}

/// The thread's side: both helpers and the state of the hand-over.
struct Pump {
    demuxer: Demuxer,
    decoder: Decoder,
    stream: u32,
    commands: Receiver<Command>,
    out: SyncSender<Message>,
    shared: Arc<Shared>,
    generation: u64,
    /// A packet the window of the decoder had no room for.
    held: Option<Packet>,
    /// A message the channel had no room for.
    pending: Option<Message>,
    /// The demuxer said that no more packets come.
    ended: bool,
    oversized: u64,
    read_error: Option<String>,
}

impl Pump {
    fn run(mut self) {
        let mut attempt = 0u32;
        loop {
            let progress = match self.pass() {
                Ok(Some(progress)) => progress,
                // The backend is gone.
                Ok(None) => return,
                Err(why) => {
                    // A full channel holds pictures of a pipeline that is gone anyway.
                    let _ = self.out.send(Message::Failed(why));
                    return;
                }
            };
            self.publish();
            if progress {
                attempt = 0;
            } else {
                pause(attempt);
                attempt = attempt.saturating_add(1);
            }
        }
    }

    /// One round: commands, packets in, a picture out. `Ok(None)` when the backend is gone,
    /// otherwise whether anything moved.
    fn pass(&mut self) -> Result<Option<bool>, String> {
        let mut progress = false;
        // The last of several jumps is the one that counts.
        let mut seek = None;
        loop {
            match self.commands.try_recv() {
                Ok(command) => seek = Some(command),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(None),
            }
        }
        if let Some(Command::Seek {
            to,
            first,
            generation,
        }) = seek
        {
            self.seek(to, first, generation)?;
            progress = true;
        }

        for _ in 0..PACKETS_PER_PASS {
            if self.held.is_none() && !self.ended {
                match self.demuxer.try_next().map_err(|e| e.to_string())? {
                    Some(Event::Packet(packet)) if packet.stream == self.stream => {
                        self.held = Some(packet);
                    }
                    Some(Event::Packet(packet)) => {
                        self.demuxer.recycle(packet);
                        progress = true;
                        continue;
                    }
                    Some(Event::End { error }) => {
                        self.ended = true;
                        self.read_error = error.map(|(code, text)| format!("{text} ({code})"));
                        self.decoder.drain().map_err(|e| e.to_string())?;
                        progress = true;
                    }
                    Some(Event::Dropped { stream, .. }) => {
                        if stream == self.stream {
                            self.oversized += 1;
                        }
                        progress = true;
                        continue;
                    }
                    None => {}
                }
            }
            let Some(packet) = self.held.take() else {
                break;
            };
            match self.decoder.send(&packet).map_err(|e| e.to_string())? {
                Sent::WindowFull => {
                    self.held = Some(packet);
                    break;
                }
                Sent::TooLarge => {
                    self.oversized += 1;
                    self.demuxer.recycle(packet);
                }
                Sent::Taken => self.demuxer.recycle(packet),
            }
            progress = true;
        }

        if self.pending.is_none() {
            match self.decoder.try_next().map_err(|e| e.to_string())? {
                Next::Frame(decoded) => {
                    self.pending = Some(Message::Frame(self.generation, decoded));
                }
                Next::End => self.pending = Some(Message::End(self.generation)),
                Next::Idle | Next::PoolExhausted => {}
            }
        }
        if let Some(message) = self.pending.take() {
            match self.out.try_send(message) {
                Ok(()) => progress = true,
                Err(TrySendError::Full(message)) => self.pending = Some(message),
                Err(TrySendError::Disconnected(_)) => return Ok(None),
            }
        }
        Ok(Some(progress))
    }

    fn seek(
        &mut self,
        to: Timestamp,
        first: Option<Timestamp>,
        generation: u64,
    ) -> Result<(), String> {
        // Whatever was on its way belongs to the position before.
        self.pending = None;
        if let Some(packet) = self.held.take() {
            self.demuxer.recycle(packet);
        }
        self.generation = generation;
        match self.demuxer.seek(to) {
            Ok(()) => {}
            // The container cannot go there: the demuxer reads on where it was, and so does the
            // decoder. The pictures simply go on.
            Err(takt_demux::DemuxError::Refused { .. } | takt_demux::DemuxError::Time) => {
                return Ok(());
            }
            Err(err) => return Err(err.to_string()),
        }
        self.decoder.flush().map_err(|e| e.to_string())?;
        self.decoder.skip_before(first);
        self.ended = false;
        self.read_error = None;
        Ok(())
    }

    fn publish(&self) {
        let stats = self.decoder.stats();
        self.shared.decoded.store(stats.frames, Ordering::Relaxed);
        self.shared
            .pool_out
            .store(self.decoder.pool().0, Ordering::Relaxed);
        let mut shared = self
            .shared
            .stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        shared.decode = stats;
        shared.oversized = self.oversized;
        if shared.read_error != self.read_error {
            shared.read_error.clone_from(&self.read_error);
        }
    }
}

/// An open file: the thread and what it has handed over.
struct Playing {
    info: MediaInfo,
    messages: Receiver<Message>,
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    /// Pictures fetched from the channel, in the order of presentation.
    ready: VecDeque<Decoded>,
    /// The jump the pictures must belong to.
    generation: u64,
    /// The thread said that no more pictures come.
    ended: bool,
    failed: Option<String>,
    /// The next picture is the first after the file opened or after a jump: it is shown at once.
    first: bool,
    /// The unit of the timestamps the decoder hands out.
    time_base: Rational,
    /// Ticks of the stream's time base the file starts at: the pictures are shown from zero.
    start_ticks: i64,
    /// Ticks one picture lasts, for pictures without a timestamp.
    frame_ticks: i64,
    /// The timestamp of the picture handed over last, in ticks of the stream.
    last_ticks: Option<i64>,
    untimed: u64,
    matrix: u8,
}

impl Playing {
    /// Fetches pictures from the channel until `want` of them are ready or the channel is empty.
    fn fill(&mut self, want: usize) {
        while self.ready.len() < want && !self.ended && self.failed.is_none() {
            match self.messages.try_recv() {
                Ok(Message::Frame(generation, decoded)) if generation == self.generation => {
                    self.ready.push_back(decoded);
                }
                Ok(Message::End(generation)) if generation == self.generation => self.ended = true,
                // From before the last jump.
                Ok(Message::Frame(..) | Message::End(_)) => {}
                Ok(Message::Failed(why)) => self.failed = Some(why),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.failed = Some("the decoder thread ended".to_owned());
                }
            }
        }
    }

    /// Gives the next ready picture its place on the caller's timeline, once.
    fn stamp(&mut self, index: usize) -> Option<Timestamp> {
        // The picture before: the one in front of it in the queue, or the one handed over last.
        let before = match index.checked_sub(1) {
            Some(front) => Some(self.ready.get(front)?.frame.get().meta().pts.ticks()),
            None => self.last_ticks,
        };
        let decoded = self.ready.get_mut(index)?;
        if !decoded.timed {
            // Follows the picture before; the first one stands at the start.
            let ticks = before.map_or(self.start_ticks, |last| {
                last.saturating_add(self.frame_ticks)
            });
            decoded
                .frame
                .get_mut()
                .set_pts(Timestamp::new(ticks, self.time_base));
            decoded.timed = true;
            self.untimed += 1;
        }
        Some(decoded.frame.get().meta().pts)
    }

    /// The time of a picture as the caller sees it: from zero at the start of the file.
    fn shown(&self, pts: Timestamp) -> Timestamp {
        pts.offset(self.start_ticks.saturating_neg()).unwrap_or(pts)
    }

    fn take(&mut self) -> Option<PooledFrame> {
        let mut decoded = self.ready.pop_front()?;
        let pts = decoded.frame.get().meta().pts;
        self.last_ticks = Some(pts.ticks());
        self.matrix = decoded.matrix;
        let shown = self.shown(pts);
        decoded.frame.get_mut().set_pts(shown);
        Some(decoded.frame)
    }
}

impl Drop for Playing {
    fn drop(&mut self) {
        // Without its two channels the thread ends at its next pass, also when it is about to
        // report why it failed. The pictures still on their way go back to their pool.
        let (commands, _) = std::sync::mpsc::channel();
        self.commands = commands;
        let (_, messages) = std::sync::mpsc::sync_channel(1);
        self.messages = messages;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The software path of Takt as a [`VideoBackend`]: `takt-demux` and [`Decoder`] in their helper
/// processes, pictures in frames of the pool.
pub struct SoftwareBackend {
    options: BackendOptions,
    playing: Option<Playing>,
    display_period_ns: Option<i64>,
    dropped: u64,
}

impl std::fmt::Debug for SoftwareBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoftwareBackend")
            .field("open", &self.playing.is_some())
            .finish_non_exhaustive()
    }
}

impl SoftwareBackend {
    /// A backend that plays with `options`. Nothing is started until a file is opened.
    #[must_use]
    pub const fn new(options: BackendOptions) -> Self {
        Self {
            options,
            playing: None,
            display_period_ns: None,
            dropped: 0,
        }
    }

    /// The counters of the pipeline; the defaults while no file is open.
    #[must_use]
    pub fn pipeline_stats(&self) -> PipelineStats {
        let Some(playing) = &self.playing else {
            return PipelineStats::default();
        };
        let mut stats = playing
            .shared
            .stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        stats.untimed = playing.untimed;
        stats.matrix = playing.matrix;
        stats
    }

    /// Frames of the pool that are out, and its size.
    #[must_use]
    pub fn pool(&self) -> (usize, usize) {
        let out = self
            .playing
            .as_ref()
            .map_or(0, |playing| playing.shared.pool_out.load(Ordering::Relaxed));
        (out, self.options.decode.pool_frames)
    }

    /// How early a picture may be handed out: half the display period when it is known, never
    /// more than half a frame duration (as `NullBackend`).
    fn due_tolerance_nanos(&self) -> i64 {
        let half_frame = self
            .playing
            .as_ref()
            .and_then(|playing| playing.info.frame_duration_nanos())
            .map_or(i64::MAX, |nanos| nanos / 2);
        self.display_period_ns
            .filter(|&period| period > 0)
            .map_or(half_frame, |period| (period / 2).min(half_frame))
    }
}

/// The video stream the software path plays: the first one that is a film and not a cover, of a
/// codec on the list.
fn pick_stream(streams: &[StreamInfo]) -> Result<&StreamInfo, takt_core::Error> {
    let video = || {
        streams
            .iter()
            .filter(|stream| stream.kind == StreamKind::Video)
            .filter(|stream| stream.disposition & DISPOSITION_ATTACHED_PIC == 0)
    };
    if let Some(stream) = video().find(|stream| proto::decoder_for(&stream.codec).is_some()) {
        return Ok(stream);
    }
    Err(takt_core::Error::SourceUnavailable(
        video().next().map_or_else(
            || "the file has no picture".to_owned(),
            |stream| {
                format!(
                    "the software path does not decode {} (it decodes h264, hevc and av1)",
                    stream.codec
                )
            },
        ),
    ))
}

/// What the stream table says the frames will be; the frames themselves say what they are.
fn expected_meta(
    stream: &StreamInfo,
    layout: proto::Layout,
) -> Option<(FrameMeta, ColorDescription)> {
    let video = stream.video.as_ref()?;
    let (format, full) =
        WireFormat::from_ffmpeg_name(&video.pixel_format).unwrap_or((WireFormat::Yuv420P8, false));
    let code = |value: u32| u8::try_from(value).unwrap_or(2);
    let color = ColorDescription {
        primaries: proto::primaries_of(code(video.color_primaries)),
        transfer: proto::transfer_of(code(video.color_transfer)),
        range: if full || video.color_range == 2 {
            takt_core::ColorRange::Full
        } else {
            takt_core::ColorRange::Limited
        },
    };
    let meta = FrameMeta {
        width: video.width,
        height: video.height,
        format: format.pixel_format(layout),
        primaries: color.primaries,
        transfer: color.transfer,
        range: color.range,
        pts: Timestamp::zero(stream.time_base),
    };
    Some((meta, color))
}

impl VideoBackend for SoftwareBackend {
    fn name(&self) -> &'static str {
        "takt-sw"
    }

    fn open(&mut self, source: &str) -> takt_core::Result<MediaInfo> {
        // The file that played ends first: its thread and its two helpers.
        self.playing = None;
        self.dropped = 0;
        let demuxer = Demuxer::open_path(source, &self.options.demux)?;
        let stream = pick_stream(&demuxer.info().streams)?.clone();
        let decoder = Decoder::open(&stream, &self.options.decode)?;
        let file = demuxer.info();
        let (meta, color) = expected_meta(&stream, self.options.decode.layout).ok_or(
            takt_core::Error::Unsupported("a video stream without a size"),
        )?;
        let frame_rate = stream.video.as_ref().and_then(|video| video.frame_rate);
        // Where the file starts, in ticks of the stream: the pictures are shown from zero.
        let start_ticks = file
            .start
            .and_then(|start| start.rescale(stream.time_base))
            .or(stream.start)
            .map_or(0, Timestamp::ticks);
        let duration = file.duration.or_else(|| {
            stream
                .duration
                .and_then(|duration| duration.rescale(Rational::MICROS))
        });
        let info = MediaInfo {
            duration,
            frame_rate,
            video: Some(meta),
            source_color: Some(color),
            sample_rate: None,
            seekable: true,
        };
        // One tick at least: pictures without a timestamp must not stand on one spot.
        let frame_ticks = info
            .frame_duration_nanos()
            .and_then(|nanos| Timestamp::from_nanos(nanos).rescale(stream.time_base))
            .map_or(1, |duration| duration.ticks().max(1));

        let shared = Arc::new(Shared::default());
        shared
            .stats
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .decoder = decoder.decoder_name().to_owned();
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let (message_tx, message_rx) = std::sync::mpsc::sync_channel(self.options.queue.max(1));
        let pump = Pump {
            demuxer,
            decoder,
            stream: stream.index,
            commands: command_rx,
            out: message_tx,
            shared: Arc::clone(&shared),
            generation: 0,
            held: None,
            pending: None,
            ended: false,
            oversized: 0,
            read_error: None,
        };
        let thread = std::thread::Builder::new()
            .name("takt-decode-pump".to_owned())
            .spawn(move || pump.run())
            .map_err(|e| takt_core::Error::Backend(format!("decoder thread: {e}")))?;
        self.playing = Some(Playing {
            info: info.clone(),
            messages: message_rx,
            commands: command_tx,
            thread: Some(thread),
            shared,
            ready: VecDeque::new(),
            generation: 0,
            ended: false,
            failed: None,
            first: true,
            time_base: stream.time_base,
            start_ticks,
            frame_ticks,
            last_ticks: None,
            untimed: 0,
            matrix: 2,
        });
        Ok(info)
    }

    /// Positions the file. Both modes are exact here: decoding starts at the keyframe before
    /// `to`, and pictures before `to` are not handed out, because the caller's clock stands at
    /// `to` and not at the keyframe.
    fn seek(&mut self, to: Timestamp, _mode: SeekMode) -> takt_core::Result<()> {
        let Some(playing) = &mut self.playing else {
            return Err(takt_core::Error::Backend(
                "seek: no file is open".to_owned(),
            ));
        };
        if let Some(why) = &playing.failed {
            return Err(takt_core::Error::Backend(why.clone()));
        }
        // The caller counts from zero, the file from its start.
        let first = to
            .rescale(playing.time_base)
            .and_then(|to| to.offset(playing.start_ticks))
            .ok_or(takt_core::Error::TimeOverflow)?;
        let target = first
            .rescale(Rational::MICROS)
            .ok_or(takt_core::Error::TimeOverflow)?;
        playing.generation += 1;
        playing.ready.clear();
        playing.ended = false;
        playing.first = true;
        playing.last_ticks = Some(first.ticks().saturating_sub(playing.frame_ticks));
        playing
            .commands
            .send(Command::Seek {
                to: target,
                first: Some(first),
                generation: playing.generation,
            })
            .map_err(|_| takt_core::Error::Backend("seek: the decoder thread ended".to_owned()))
    }

    fn next_frame(&mut self, clock: &dyn Clock) -> takt_core::Result<Option<PooledFrame>> {
        let tolerance = self.due_tolerance_nanos();
        let Some(playing) = &mut self.playing else {
            return Ok(None);
        };
        playing.fill(1);
        let Some(pts) = playing.stamp(0) else {
            // Nothing is ready: say once why nothing will come.
            return match playing.failed.take() {
                Some(why) => {
                    playing.ended = true;
                    Err(takt_core::Error::Backend(why))
                }
                None => Ok(None),
            };
        };
        // The picture of a jump (and the first of a file) is shown when it is there, also while
        // the clock stands: it is what the person jumped to.
        if !playing.first && !clock.is_due(playing.shown(pts), tolerance) {
            return Ok(None);
        }
        playing.first = false;
        if clock.may_drop() {
            // Late: skip to the last picture whose time has come.
            loop {
                playing.fill(2);
                let Some(next) = playing.stamp(1) else {
                    break;
                };
                if clock.until_nanos(playing.shown(next)) > 0 {
                    break;
                }
                // Its frame goes back to the pool here.
                drop(playing.take());
                self.dropped += 1;
            }
        }
        Ok(playing.take())
    }

    fn set_display_period(&mut self, nanos: Option<i64>) {
        self.display_period_ns = nanos;
    }

    fn is_eof(&self) -> bool {
        self.playing
            .as_ref()
            .is_none_or(|playing| playing.ended && playing.ready.is_empty())
    }

    fn stats(&self) -> BackendStats {
        let Some(playing) = &self.playing else {
            return BackendStats::default();
        };
        BackendStats {
            decoded: playing.shared.decoded.load(Ordering::Relaxed),
            dropped: self.dropped,
            repeated: 0,
            queued: playing.shared.pool_out.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use takt_demux::VideoParams;

    fn stream(index: u32, kind: StreamKind, codec: &str, disposition: u32) -> StreamInfo {
        StreamInfo {
            index,
            kind,
            codec: codec.to_owned(),
            codec_tag: 0,
            time_base: Rational::lit(1, 1000),
            start: None,
            duration: None,
            frames: None,
            disposition,
            profile: -99,
            level: -99,
            bit_rate: None,
            extradata: Vec::new(),
            language: String::new(),
            title: String::new(),
            video: (kind == StreamKind::Video).then(|| VideoParams {
                width: 1920,
                height: 1080,
                pixel_format: "yuv420p10le".to_owned(),
                color_primaries: 9,
                color_transfer: 16,
                color_range: 1,
                ..VideoParams::default()
            }),
            audio: None,
        }
    }

    #[test]
    fn the_film_is_picked_and_not_its_cover_or_a_codec_off_the_list() {
        let streams = [
            stream(0, StreamKind::Audio, "opus", 0),
            stream(1, StreamKind::Video, "mjpeg", DISPOSITION_ATTACHED_PIC),
            stream(2, StreamKind::Video, "vp9", 0),
            stream(3, StreamKind::Video, "hevc", 0),
        ];
        assert_eq!(pick_stream(&streams).unwrap().index, 3);
        let err = pick_stream(&streams[..3]).unwrap_err().to_string();
        assert!(err.contains("vp9"), "{err}");
        let err = pick_stream(&streams[..2]).unwrap_err().to_string();
        assert!(err.contains("no picture"), "{err}");
    }

    #[test]
    fn the_stream_table_says_what_the_frames_will_be() {
        let hdr = stream(0, StreamKind::Video, "hevc", 0);
        let (meta, color) = expected_meta(&hdr, proto::Layout::SemiPlanar).unwrap();
        assert_eq!(meta.format, takt_core::PixelFormat::P010);
        assert_eq!((meta.width, meta.height), (1920, 1080));
        assert!(color.is_hdr());
        assert_eq!(color.primaries, takt_core::ColorPrimaries::Bt2020);
        let (meta, _) = expected_meta(&hdr, proto::Layout::Planar).unwrap();
        assert_eq!(meta.format, takt_core::PixelFormat::Yuv420P10);
        assert!(
            expected_meta(
                &stream(0, StreamKind::Audio, "opus", 0),
                proto::Layout::Planar
            )
            .is_none()
        );
    }

    #[test]
    fn a_backend_without_a_file_is_at_its_end_and_refuses_a_jump() {
        let mut backend = SoftwareBackend::new(BackendOptions::in_dir(Path::new("nowhere")));
        assert_eq!(backend.name(), "takt-sw");
        assert!(backend.is_eof());
        assert_eq!(backend.stats(), BackendStats::default());
        assert_eq!(backend.pool(), (0, 8));
        assert!(
            backend
                .seek(Timestamp::from_millis(0), SeekMode::Exact)
                .is_err()
        );
        // A helper that does not exist is an error, not a panic.
        assert!(backend.open("nothing.mkv").is_err());
    }
}
