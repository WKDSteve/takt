// SPDX-License-Identifier: GPL-3.0-or-later
//! The protocol between the demux helper and its parent, on top of `takt-ipc`.
//!
//! # The course of a session
//!
//! The parent starts the helper with the file as an inherited handle. After the handshake of
//! `takt-ipc` the helper opens the file and sends [`kind::OPENED`] with the [`MediaInfo`], or a
//! fault. From then on it demuxes ahead on its own: every packet becomes a record in the ring, in
//! the order libavformat delivers them, until the ring is full; then it waits for the parent to
//! read. The end of the file is a record too, so it keeps its place behind the last packet.
//!
//! A seek is the one request: [`kind::SEEK`] with a new **serial**. The helper drops what it was
//! about to write, positions, answers [`kind::SEEK_DONE`] (or a fault) and goes on with records
//! that carry the new serial. Records with an older serial are still in the ring at that moment;
//! the parent throws them away as it meets them. So no packet from before a seek is ever taken for
//! one after it, without the two sides having to stop each other.
//!
//! # Records
//!
//! A record is a body followed by a fixed tail; its last byte is the tag. The body of a packet is
//! the compressed data, so the reader gets it at the start of its buffer and cuts the tail off:
//! no second copy.
//!
//! ```text
//! packet   data | pts i64 | dts i64 | duration i64 | pos i64 | serial u32 | stream u32
//!               | flags u8 | 0 | 0 | tag 1                                    (tail: 44 bytes)
//! end      text | code i32 | serial u32 | 0 | 0 | 0 | tag 2                   (tail: 12 bytes)
//! dropped       | size u64 | serial u32 | stream u32 | 0 | 0 | 0 | tag 3      (tail: 20 bytes)
//! ```
//!
//! # Trust
//!
//! Everything here is parsed from bytes of a process that reads foreign files. Each parser checks
//! lengths, counts and values against the limits below and against the stream table before a value
//! is used: a stream index must exist, a time base must be a positive fraction of two numbers below
//! 2^31, a timestamp must not be the "no value" marker, reserved bytes must be zero. A message that fails is a
//! [`ProtocolError`], and the owner ends the helper.

use takt_core::{Rational, Timestamp};
use takt_ipc::message::AppId;
use takt_ipc::wire::{Reader, put_blob, put_i32, put_i64, put_text, put_u32, put_u64};
use takt_ipc::{ProtocolError, tame};

/// The application of the demux helper in the handshake.
pub const APP: AppId = AppId {
    name: *b"DMUX",
    version: 1,
};

/// The kinds of frames of this protocol.
pub mod kind {
    use takt_ipc::message::kind::APP;

    /// Helper to parent, once, `seq` 0: the file is open, the payload is the [`super::MediaInfo`].
    pub const OPENED: u16 = APP;
    /// Parent to helper: a [`super::Seek`].
    pub const SEEK: u16 = APP + 1;
    /// Helper to parent, no payload: the seek with this `seq` is done.
    pub const SEEK_DONE: u16 = APP + 2;
}

/// Codes of the faults the helper reports.
pub mod fault {
    /// The file could not be opened as a container (unknown format, damaged head, no stream).
    pub const OPEN: u32 = 1;
    /// A seek failed; the helper reads on where it was.
    pub const SEEK: u32 = 2;
    /// A request the helper does not know or cannot parse.
    pub const REQUEST: u32 = 3;
    /// The helper was built without FFmpeg (`FFMPEG_DIR` not set at build time).
    pub const NO_FFMPEG: u32 = 4;
    /// The FFmpeg libraries found at run time are not the major versions the helper was written
    /// for, or a GPL build.
    pub const FFMPEG_VERSION: u32 = 5;
}

/// Most streams a file may have.
pub const MAX_STREAMS: usize = 256;
/// Longest name of a format, codec, pixel format or sample format, in bytes.
pub const MAX_NAME: usize = 64;
/// Longest title of a stream, in bytes.
pub const MAX_TITLE: usize = 256;
/// Longest language tag of a stream, in bytes.
pub const MAX_LANGUAGE: usize = 16;
/// Most bytes of codec configuration (extradata) per stream.
pub const MAX_EXTRADATA: usize = 1 << 20;
/// Largest picture dimension that is taken for one.
pub const MAX_DIMENSION: u32 = 1 << 16;
/// Longest text of a read error, in bytes.
pub const MAX_ERROR_TEXT: usize = 512;

/// Bytes of the tail of a packet record.
pub const PACKET_TAIL: usize = 44;
const END_TAIL: usize = 12;
const DROPPED_TAIL: usize = 20;

const TAG_PACKET: u8 = 1;
const TAG_END: u8 = 2;
const TAG_DROPPED: u8 = 3;

const FLAG_KEY: u8 = 1;
const FLAG_CORRUPT: u8 = 1 << 1;
const FLAG_DISCARD: u8 = 1 << 2;
const FLAG_PTS: u8 = 1 << 3;
const FLAG_DTS: u8 = 1 << 4;
const FLAG_DURATION: u8 = 1 << 5;
const FLAG_POSITION: u8 = 1 << 6;

/// "No value" for an optional `i64` on the wire (what FFmpeg calls `AV_NOPTS_VALUE`).
const NONE: i64 = i64::MIN;

/// What a stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// Something this protocol has no name for.
    Other,
    /// Pictures.
    Video,
    /// Sound.
    Audio,
    /// Subtitles.
    Subtitle,
    /// Data that is neither (timecode, chapters as a stream).
    Data,
    /// A file inside the container (a font, a cover).
    Attachment,
}

impl StreamKind {
    const fn code(self) -> u32 {
        match self {
            Self::Other => 0,
            Self::Video => 1,
            Self::Audio => 2,
            Self::Subtitle => 3,
            Self::Data => 4,
            Self::Attachment => 5,
        }
    }

    const fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::Other,
            1 => Self::Video,
            2 => Self::Audio,
            3 => Self::Subtitle,
            4 => Self::Data,
            5 => Self::Attachment,
            _ => return None,
        })
    }
}

/// The picture parameters of a video stream, as the container states them.
///
/// The colour fields are the code points of ITU-T H.273 (the numbers FFmpeg uses too): 2 means
/// "unspecified". Mapping them to `takt_core::ColorDescription` is the decoder's part.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VideoParams {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// FFmpeg's name of the pixel format (`yuv420p`, `yuv420p10le`), empty when unknown.
    pub pixel_format: String,
    /// Shape of a pixel (width by height), `None` when the container does not say.
    pub sample_aspect: Option<Rational>,
    /// Average frame rate, `None` when unknown.
    pub frame_rate: Option<Rational>,
    /// Colour range: 1 limited (MPEG), 2 full (JPEG), 0 unspecified (FFmpeg's `AVColorRange`).
    pub color_range: u32,
    /// Colour primaries (H.273).
    pub color_primaries: u32,
    /// Transfer characteristics (H.273).
    pub color_transfer: u32,
    /// Matrix coefficients (H.273).
    pub color_matrix: u32,
    /// Position of the chroma samples (FFmpeg's `AVChromaLocation`).
    pub chroma_location: u32,
    /// Field order (FFmpeg's `AVFieldOrder`): 0 unknown, 1 progressive.
    pub field_order: u32,
    /// Frames the decoder holds back for reordering.
    pub delay: u32,
}

/// The sound parameters of an audio stream, as the container states them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AudioParams {
    /// Samples per second.
    pub sample_rate: u32,
    /// Number of channels.
    pub channels: u32,
    /// Which speakers (the bits of FFmpeg's native channel order, as in WAVE), 0 when the order is
    /// not the native one or unknown.
    pub channel_mask: u64,
    /// FFmpeg's name of the sample format (`fltp`, `s16`), empty when unknown.
    pub sample_format: String,
    /// Samples per packet, 0 when it varies.
    pub frame_size: u32,
    /// Samples the encoder put in front that are to be thrown away.
    pub initial_padding: u32,
    /// Samples at the end that are to be thrown away.
    pub trailing_padding: u32,
    /// Samples to decode before a seek target for the output to be right.
    pub seek_preroll: u32,
    /// Bits per sample in the coded stream (PCM), 0 when unknown.
    pub bits_per_coded_sample: u32,
    /// Bytes per block (PCM, ADPCM), 0 when unknown.
    pub block_align: u32,
}

/// One stream of a file: what a decoder needs to be set up, and what a track list shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    /// The index packets name the stream by; equal to its position in [`MediaInfo::streams`].
    pub index: u32,
    /// What it carries.
    pub kind: StreamKind,
    /// FFmpeg's name of the codec (`h264`, `hevc`, `av1`, `vp9`, `aac`, `opus`): lower-case ASCII.
    pub codec: String,
    /// The container's own tag for the codec (a FourCC), 0 when it has none.
    pub codec_tag: u32,
    /// The unit of every timestamp of this stream.
    pub time_base: Rational,
    /// Timestamp of the first frame, when known.
    pub start: Option<Timestamp>,
    /// Length of the stream, when known.
    pub duration: Option<Timestamp>,
    /// Number of frames, when the container states it.
    pub frames: Option<u64>,
    /// FFmpeg's disposition bits (`AV_DISPOSITION_DEFAULT` is 1, `FORCED` 0x40, `ATTACHED_PIC`
    /// 0x400).
    pub disposition: u32,
    /// Codec profile (FFmpeg's number), -99 when unknown.
    pub profile: i32,
    /// Codec level (FFmpeg's number), -99 when unknown.
    pub level: i32,
    /// Bits per second, when the container states it.
    pub bit_rate: Option<u64>,
    /// The codec configuration from the container (`avcC`, `hvcC`, `av1C`, the Opus head, ...).
    /// Foreign bytes: a decoder parses them as carefully as a packet.
    pub extradata: Vec<u8>,
    /// Language tag from the metadata, empty when there is none. Foreign text, tamed.
    pub language: String,
    /// Title from the metadata, empty when there is none. Foreign text, tamed.
    pub title: String,
    /// Set for [`StreamKind::Video`].
    pub video: Option<VideoParams>,
    /// Set for [`StreamKind::Audio`].
    pub audio: Option<AudioParams>,
}

/// What a file is, after it was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInfo {
    /// FFmpeg's name of the container format (`matroska,webm`, `mov,mp4,m4a,3gp,3g2,mj2`).
    pub format: String,
    /// Length of the file, when known.
    pub duration: Option<Timestamp>,
    /// Timestamp its presentation starts at, when known.
    pub start: Option<Timestamp>,
    /// The streams, by index.
    pub streams: Vec<StreamInfo>,
}

fn put_option(out: &mut Vec<u8>, value: Option<i64>) {
    put_i64(out, value.unwrap_or(NONE));
}

fn put_fraction(out: &mut Vec<u8>, value: Option<Rational>) {
    put_i64(out, value.map_or(0, Rational::num));
    put_i64(out, value.map_or(0, Rational::den));
}

fn option(r: &mut Reader<'_>) -> Result<Option<i64>, ProtocolError> {
    let value = r.i64()?;
    Ok((value != NONE).then_some(value))
}

/// A fraction that may be missing: zero in either place means "not stated". Otherwise both parts
/// must be positive.
fn fraction(r: &mut Reader<'_>, field: &'static str) -> Result<Option<Rational>, ProtocolError> {
    let (num, den) = (r.i64()?, r.i64()?);
    if num == 0 || den == 0 {
        return Ok(None);
    }
    positive_fraction(num, den, field).map(Some)
}

/// A fraction with both parts in `1..=i32::MAX`.
///
/// The bound is what the source can produce (FFmpeg's `AVRational` is two `int`s), and it is what
/// keeps the arithmetic of `Timestamp` exact: comparing and rescaling multiply ticks by a numerator
/// and a denominator in 128 bits, which holds for parts below 2^31 and would overflow for parts a
/// lying helper could otherwise pick (found by the fuzz target on 2026-11-17).
fn positive_fraction(num: i64, den: i64, field: &'static str) -> Result<Rational, ProtocolError> {
    let small = |part: i64| (1..=i64::from(i32::MAX)).contains(&part);
    if !small(num) || !small(den) {
        return Err(ProtocolError::Invalid(field));
    }
    Rational::new(num, den).ok_or(ProtocolError::Invalid(field))
}

/// A name FFmpeg made (format, codec, pixel format): printable ASCII without spaces.
fn name(r: &mut Reader<'_>, field: &'static str) -> Result<String, ProtocolError> {
    let text = r.text(MAX_NAME)?;
    if text.bytes().all(|b| b.is_ascii_graphic()) {
        Ok(text.to_owned())
    } else {
        Err(ProtocolError::Invalid(field))
    }
}

fn count(value: Option<i64>) -> Option<u64> {
    value.and_then(|v| u64::try_from(v).ok()).filter(|v| *v > 0)
}

impl MediaInfo {
    /// The payload of [`kind::OPENED`].
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_text(&mut out, &self.format, MAX_NAME);
        put_option(
            &mut out,
            self.duration
                .and_then(|t| t.rescale(Rational::MICROS))
                .map(Timestamp::ticks),
        );
        put_option(
            &mut out,
            self.start
                .and_then(|t| t.rescale(Rational::MICROS))
                .map(Timestamp::ticks),
        );
        put_u32(
            &mut out,
            u32::try_from(self.streams.len()).unwrap_or(u32::MAX),
        );
        for stream in &self.streams {
            stream.encode(&mut out);
        }
        out
    }

    /// Reads the payload of [`kind::OPENED`].
    ///
    /// # Errors
    /// A [`ProtocolError`] for anything that is not a stream table within the limits.
    pub fn parse(payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(payload);
        let format = name(&mut r, "format name")?;
        let duration = option(&mut r)?;
        let start = option(&mut r)?;
        if duration.is_some_and(|d| d < 0) {
            return Err(ProtocolError::Invalid("duration of the file"));
        }
        let streams = r.u32()? as usize;
        if streams > MAX_STREAMS {
            return Err(ProtocolError::TooLarge {
                len: streams as u64,
                max: MAX_STREAMS as u64,
            });
        }
        let streams = (0..streams)
            .map(|position| StreamInfo::parse(&mut r, position))
            .collect::<Result<Vec<_>, _>>()?;
        r.finish()?;
        Ok(Self {
            format,
            duration: duration.map(|t| Timestamp::new(t, Rational::MICROS)),
            start: start.map(|t| Timestamp::new(t, Rational::MICROS)),
            streams,
        })
    }
}

impl StreamInfo {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u32(out, self.index);
        put_u32(out, self.kind.code());
        put_text(out, &self.codec, MAX_NAME);
        put_u32(out, self.codec_tag);
        put_i64(out, self.time_base.num());
        put_i64(out, self.time_base.den());
        put_option(out, self.start.map(Timestamp::ticks));
        put_option(out, self.duration.map(Timestamp::ticks));
        put_option(out, self.frames.and_then(|n| i64::try_from(n).ok()));
        put_u32(out, self.disposition);
        put_i32(out, self.profile);
        put_i32(out, self.level);
        put_option(out, self.bit_rate.and_then(|n| i64::try_from(n).ok()));
        put_blob(
            out,
            &self.extradata[..self.extradata.len().min(MAX_EXTRADATA)],
        );
        put_text(out, &self.language, MAX_LANGUAGE);
        put_text(out, &self.title, MAX_TITLE);
        let video = self.video.clone().unwrap_or_default();
        put_u32(out, video.width);
        put_u32(out, video.height);
        put_text(out, &video.pixel_format, MAX_NAME);
        put_fraction(out, video.sample_aspect);
        put_fraction(out, video.frame_rate);
        for value in [
            video.color_range,
            video.color_primaries,
            video.color_transfer,
            video.color_matrix,
            video.chroma_location,
            video.field_order,
            video.delay,
        ] {
            put_u32(out, value);
        }
        let audio = self.audio.clone().unwrap_or_default();
        put_u32(out, audio.sample_rate);
        put_u32(out, audio.channels);
        put_u64(out, audio.channel_mask);
        put_text(out, &audio.sample_format, MAX_NAME);
        for value in [
            audio.frame_size,
            audio.initial_padding,
            audio.trailing_padding,
            audio.seek_preroll,
            audio.bits_per_coded_sample,
            audio.block_align,
        ] {
            put_u32(out, value);
        }
    }

    fn parse(r: &mut Reader<'_>, position: usize) -> Result<Self, ProtocolError> {
        let index = r.u32()?;
        if index as usize != position {
            return Err(ProtocolError::Invalid("stream index"));
        }
        let kind = StreamKind::from_code(r.u32()?).ok_or(ProtocolError::Invalid("stream kind"))?;
        let codec = name(r, "codec name")?;
        let codec_tag = r.u32()?;
        let (num, den) = (r.i64()?, r.i64()?);
        let time_base = positive_fraction(num, den, "time base")?;
        let start = option(r)?;
        let duration = option(r)?;
        if duration.is_some_and(|d| d < 0) {
            return Err(ProtocolError::Invalid("duration of a stream"));
        }
        let frames = count(option(r)?);
        let disposition = r.u32()?;
        let profile = r.i32()?;
        let level = r.i32()?;
        let bit_rate = count(option(r)?);
        let extradata = r.blob(MAX_EXTRADATA)?.to_vec();
        let language = tame(r.text(MAX_LANGUAGE)?, MAX_LANGUAGE);
        let title = tame(r.text(MAX_TITLE)?, MAX_TITLE);

        let width = r.u32()?;
        let height = r.u32()?;
        if width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(ProtocolError::Invalid("picture size"));
        }
        let video = VideoParams {
            width,
            height,
            pixel_format: name(r, "pixel format")?,
            sample_aspect: fraction(r, "sample aspect ratio")?,
            frame_rate: fraction(r, "frame rate")?,
            color_range: r.u32()?,
            color_primaries: r.u32()?,
            color_transfer: r.u32()?,
            color_matrix: r.u32()?,
            chroma_location: r.u32()?,
            field_order: r.u32()?,
            delay: r.u32()?,
        };
        let audio = AudioParams {
            sample_rate: r.u32()?,
            channels: r.u32()?,
            channel_mask: r.u64()?,
            sample_format: name(r, "sample format")?,
            frame_size: r.u32()?,
            initial_padding: r.u32()?,
            trailing_padding: r.u32()?,
            seek_preroll: r.u32()?,
            bits_per_coded_sample: r.u32()?,
            block_align: r.u32()?,
        };
        if kind == StreamKind::Video && (width == 0 || height == 0) {
            return Err(ProtocolError::Invalid("picture size"));
        }
        if kind == StreamKind::Audio && (audio.sample_rate == 0 || audio.channels == 0) {
            return Err(ProtocolError::Invalid("sample rate or channel count"));
        }
        Ok(Self {
            index,
            kind,
            codec,
            codec_tag,
            time_base,
            start: start.map(|t| Timestamp::new(t, time_base)),
            duration: duration.map(|t| Timestamp::new(t, time_base)),
            frames,
            disposition,
            profile,
            level,
            bit_rate,
            extradata,
            language,
            title,
            video: (kind == StreamKind::Video).then_some(video),
            audio: (kind == StreamKind::Audio).then_some(audio),
        })
    }
}

/// The request to position: read on from the last keyframe at or before `target_micros`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seek {
    /// The serial every record after this seek carries.
    pub serial: u32,
    /// The target in microseconds of the file's own timeline.
    pub target_micros: i64,
}

impl Seek {
    /// The payload of [`kind::SEEK`].
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12);
        put_u32(&mut out, self.serial);
        put_i64(&mut out, self.target_micros);
        out
    }

    /// Reads the payload of [`kind::SEEK`].
    ///
    /// # Errors
    /// A [`ProtocolError`].
    pub fn parse(payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(payload);
        let serial = r.u32()?;
        let target_micros = r.i64()?;
        r.finish()?;
        if target_micros == NONE {
            return Err(ProtocolError::Invalid("seek target"));
        }
        Ok(Self {
            serial,
            target_micros,
        })
    }
}

/// One compressed packet of one stream, as a decoder takes it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Packet {
    /// Index of its stream in [`MediaInfo::streams`]; the stream's `time_base` is the unit of the
    /// three times below.
    pub stream: u32,
    /// When its content is presented. `None` when the container does not say.
    pub pts: Option<Timestamp>,
    /// When it has to be decoded: the order of the packets of a stream. `None` when unknown.
    pub dts: Option<Timestamp>,
    /// How long its content lasts. `None` when unknown.
    pub duration: Option<Timestamp>,
    /// A decoder can start here.
    pub key: bool,
    /// The container marks the content as damaged.
    pub corrupt: bool,
    /// To be decoded but not shown (it lies before the start of an edit).
    pub discard: bool,
    /// Byte position in the file, when known.
    pub position: Option<u64>,
    /// The compressed data. Foreign bytes.
    pub data: Vec<u8>,
}

/// The times and flags of a packet as the helper has them: ticks in the stream's time base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PacketHead {
    /// The serial of the seek this packet comes after.
    pub serial: u32,
    /// Index of the stream.
    pub stream: u32,
    /// Presentation time in ticks.
    pub pts: Option<i64>,
    /// Decoding time in ticks.
    pub dts: Option<i64>,
    /// Duration in ticks.
    pub duration: Option<i64>,
    /// Byte position in the file.
    pub position: Option<i64>,
    /// A decoder can start here.
    pub key: bool,
    /// Marked as damaged.
    pub corrupt: bool,
    /// To be decoded but not shown.
    pub discard: bool,
}

impl PacketHead {
    /// The tail of a packet record; the record is the data followed by these bytes.
    #[must_use]
    pub fn tail(&self) -> [u8; PACKET_TAIL] {
        let mut flags = 0u8;
        let mut set = |on: bool, flag: u8| {
            if on {
                flags |= flag;
            }
        };
        set(self.key, FLAG_KEY);
        set(self.corrupt, FLAG_CORRUPT);
        set(self.discard, FLAG_DISCARD);
        set(self.pts.is_some(), FLAG_PTS);
        set(self.dts.is_some(), FLAG_DTS);
        set(self.duration.is_some(), FLAG_DURATION);
        set(self.position.is_some(), FLAG_POSITION);
        let mut tail = [0u8; PACKET_TAIL];
        tail[0..8].copy_from_slice(&self.pts.unwrap_or(0).to_le_bytes());
        tail[8..16].copy_from_slice(&self.dts.unwrap_or(0).to_le_bytes());
        tail[16..24].copy_from_slice(&self.duration.unwrap_or(0).to_le_bytes());
        tail[24..32].copy_from_slice(&self.position.unwrap_or(0).to_le_bytes());
        tail[32..36].copy_from_slice(&self.serial.to_le_bytes());
        tail[36..40].copy_from_slice(&self.stream.to_le_bytes());
        tail[40] = flags;
        tail[43] = TAG_PACKET;
        tail
    }
}

/// The tail of an end record; the record is the text of the error (empty at the end of the file)
/// followed by these bytes. `code` is 0 at the end of the file and FFmpeg's error number otherwise.
#[must_use]
pub fn end_tail(serial: u32, code: i32) -> [u8; END_TAIL] {
    let mut tail = [0u8; END_TAIL];
    tail[0..4].copy_from_slice(&code.to_le_bytes());
    tail[4..8].copy_from_slice(&serial.to_le_bytes());
    tail[11] = TAG_END;
    tail
}

/// The record that stands in for a packet of `size` bytes that did not fit the ring's limit.
#[must_use]
pub fn dropped_record(serial: u32, stream: u32, size: u64) -> [u8; DROPPED_TAIL] {
    let mut tail = [0u8; DROPPED_TAIL];
    tail[0..8].copy_from_slice(&size.to_le_bytes());
    tail[8..12].copy_from_slice(&serial.to_le_bytes());
    tail[12..16].copy_from_slice(&stream.to_le_bytes());
    tail[19] = TAG_DROPPED;
    tail
}

/// What a record of the ring says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// A packet.
    Packet(Packet),
    /// No more packets until the next seek: the end of the file, or a read error.
    End {
        /// `None` at the end of the file, otherwise FFmpeg's error number and text (foreign text,
        /// tamed).
        error: Option<(i32, String)>,
    },
    /// A packet was left out because it is larger than the ring allows.
    Dropped {
        /// Index of its stream.
        stream: u32,
        /// Its size in bytes, as the helper states it.
        size: u64,
    },
}

fn tail_of<const N: usize>(record: &[u8]) -> Result<(&[u8], [u8; N]), ProtocolError> {
    let (body, tail) = record
        .len()
        .checked_sub(N)
        .and_then(|at| record.split_at_checked(at))
        .ok_or(ProtocolError::Truncated)?;
    let tail = <[u8; N]>::try_from(tail).map_err(|_| ProtocolError::Truncated)?;
    Ok((body, tail))
}

fn stream_of(index: u32, streams: &[StreamInfo]) -> Result<&StreamInfo, ProtocolError> {
    streams
        .get(index as usize)
        .ok_or(ProtocolError::Invalid("stream index of a record"))
}

/// Reads one record of the ring. Returns its serial and what it says.
///
/// For a packet the data is taken out of `record` without a copy (the buffer is left empty);
/// otherwise `record` is left as it is.
///
/// # Errors
/// A [`ProtocolError`] for anything that is not a record of this protocol about a stream of
/// `streams`.
pub fn parse_record(
    record: &mut Vec<u8>,
    streams: &[StreamInfo],
) -> Result<(u32, Record), ProtocolError> {
    let tag = *record.last().ok_or(ProtocolError::Truncated)?;
    match tag {
        TAG_PACKET => {
            let (data, tail) = tail_of::<PACKET_TAIL>(record)?;
            let data_len = data.len();
            let mut r = Reader::new(&tail);
            let (pts, dts, duration, position) = (r.i64()?, r.i64()?, r.i64()?, r.i64()?);
            let serial = r.u32()?;
            let stream_index = r.u32()?;
            let flags = r.u8()?;
            if r.u8()? != 0 || r.u8()? != 0 {
                return Err(ProtocolError::Invalid("reserved bytes of a packet"));
            }
            if flags
                & !(FLAG_KEY
                    | FLAG_CORRUPT
                    | FLAG_DISCARD
                    | FLAG_PTS
                    | FLAG_DTS
                    | FLAG_DURATION
                    | FLAG_POSITION)
                != 0
            {
                return Err(ProtocolError::Invalid("flags of a packet"));
            }
            let base = stream_of(stream_index, streams)?.time_base;
            // A time is given or it is not: the "no value" number of FFmpeg must not arrive as a
            // time, and a field that is not given must be zero.
            let time = |value: i64, flag: u8, field: &'static str| {
                if flags & flag == 0 {
                    return if value == 0 {
                        Ok(None)
                    } else {
                        Err(ProtocolError::Invalid(field))
                    };
                }
                if value == NONE {
                    return Err(ProtocolError::Invalid(field));
                }
                Ok(Some(value))
            };
            let pts = time(pts, FLAG_PTS, "pts")?;
            let dts = time(dts, FLAG_DTS, "dts")?;
            let duration = time(duration, FLAG_DURATION, "duration of a packet")?;
            let position = time(position, FLAG_POSITION, "position of a packet")?;
            if duration.is_some_and(|d| d < 0) {
                return Err(ProtocolError::Invalid("duration of a packet"));
            }
            let position = match position {
                None => None,
                Some(p) => Some(
                    u64::try_from(p).map_err(|_| ProtocolError::Invalid("position of a packet"))?,
                ),
            };
            let mut data = std::mem::take(record);
            data.truncate(data_len);
            let packet = Packet {
                stream: stream_index,
                pts: pts.map(|t| Timestamp::new(t, base)),
                dts: dts.map(|t| Timestamp::new(t, base)),
                duration: duration.map(|t| Timestamp::new(t, base)),
                key: flags & FLAG_KEY != 0,
                corrupt: flags & FLAG_CORRUPT != 0,
                discard: flags & FLAG_DISCARD != 0,
                position,
                data,
            };
            Ok((serial, Record::Packet(packet)))
        }
        TAG_END => {
            let (text, tail) = tail_of::<END_TAIL>(record)?;
            let mut r = Reader::new(&tail);
            let code = r.i32()?;
            let serial = r.u32()?;
            if r.u8()? != 0 || r.u8()? != 0 || r.u8()? != 0 {
                return Err(ProtocolError::Invalid("reserved bytes of an end"));
            }
            if text.len() > MAX_ERROR_TEXT {
                return Err(ProtocolError::TooLarge {
                    len: text.len() as u64,
                    max: MAX_ERROR_TEXT as u64,
                });
            }
            let error = if code == 0 {
                if !text.is_empty() {
                    return Err(ProtocolError::Invalid("text at the end of the file"));
                }
                None
            } else {
                let text = std::str::from_utf8(text).map_err(|_| ProtocolError::Utf8)?;
                Some((code, tame(text, MAX_ERROR_TEXT)))
            };
            Ok((serial, Record::End { error }))
        }
        TAG_DROPPED => {
            let (body, tail) = tail_of::<DROPPED_TAIL>(record)?;
            if !body.is_empty() {
                return Err(ProtocolError::Trailing);
            }
            let mut r = Reader::new(&tail);
            let size = r.u64()?;
            let serial = r.u32()?;
            let stream = r.u32()?;
            if r.u8()? != 0 || r.u8()? != 0 || r.u8()? != 0 {
                return Err(ProtocolError::Invalid("reserved bytes of a dropped packet"));
            }
            stream_of(stream, streams)?;
            Ok((serial, Record::Dropped { stream, size }))
        }
        _ => Err(ProtocolError::Invalid("tag of a record")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn video_stream(index: u32) -> StreamInfo {
        let time_base = Rational::lit(1, 12_800);
        StreamInfo {
            index,
            kind: StreamKind::Video,
            codec: "h264".to_owned(),
            codec_tag: u32::from_le_bytes(*b"avc1"),
            time_base,
            start: Some(Timestamp::new(0, time_base)),
            duration: Some(Timestamp::new(25_600, time_base)),
            frames: Some(50),
            disposition: 1,
            profile: 100,
            level: 10,
            bit_rate: Some(123_456),
            extradata: vec![1, 100, 0, 10, 0xFF],
            language: "und".to_owned(),
            title: "Bild".to_owned(),
            video: Some(VideoParams {
                width: 64,
                height: 64,
                pixel_format: "yuv420p".to_owned(),
                sample_aspect: Some(Rational::lit(1, 1)),
                frame_rate: Some(Rational::lit(25, 1)),
                color_range: 1,
                color_primaries: 1,
                color_transfer: 1,
                color_matrix: 1,
                chroma_location: 1,
                field_order: 1,
                delay: 0,
            }),
            audio: None,
        }
    }

    fn audio_stream(index: u32) -> StreamInfo {
        let time_base = Rational::lit(1, 48_000);
        StreamInfo {
            index,
            kind: StreamKind::Audio,
            codec: "opus".to_owned(),
            codec_tag: 0,
            time_base,
            start: None,
            duration: None,
            frames: None,
            disposition: 0,
            profile: -99,
            level: -99,
            bit_rate: None,
            extradata: b"OpusHead".to_vec(),
            language: "deu".to_owned(),
            title: String::new(),
            video: None,
            audio: Some(AudioParams {
                sample_rate: 48_000,
                channels: 2,
                channel_mask: 3,
                sample_format: "fltp".to_owned(),
                frame_size: 960,
                initial_padding: 312,
                trailing_padding: 0,
                seek_preroll: 3840,
                bits_per_coded_sample: 0,
                block_align: 0,
            }),
        }
    }

    fn info() -> MediaInfo {
        MediaInfo {
            format: "matroska,webm".to_owned(),
            duration: Some(Timestamp::new(2_000_000, Rational::MICROS)),
            start: Some(Timestamp::new(0, Rational::MICROS)),
            streams: vec![video_stream(0), audio_stream(1)],
        }
    }

    #[test]
    fn the_stream_table_comes_back_as_sent() {
        let sent = info();
        assert_eq!(MediaInfo::parse(&sent.encode()), Ok(sent));
        let empty = MediaInfo {
            format: "mov,mp4,m4a,3gp,3g2,mj2".to_owned(),
            duration: None,
            start: None,
            streams: Vec::new(),
        };
        assert_eq!(MediaInfo::parse(&empty.encode()), Ok(empty));
    }

    #[test]
    fn every_cut_of_the_stream_table_is_refused() {
        let bytes = info().encode();
        for cut in 0..bytes.len() {
            assert!(MediaInfo::parse(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut long = bytes;
        long.push(0);
        assert_eq!(MediaInfo::parse(&long), Err(ProtocolError::Trailing));
    }

    /// The table with one stream changed before it is sent.
    fn with(change: impl FnOnce(&mut StreamInfo)) -> Result<MediaInfo, ProtocolError> {
        let mut info = info();
        change(&mut info.streams[0]);
        MediaInfo::parse(&info.encode())
    }

    #[test]
    fn a_stream_table_with_impossible_values_is_refused() {
        assert_eq!(
            with(|s| s.index = 1),
            Err(ProtocolError::Invalid("stream index"))
        );
        assert_eq!(
            with(|s| s.codec = "h 264".to_owned()),
            Err(ProtocolError::Invalid("codec name"))
        );
        assert_eq!(
            with(|s| s.video.as_mut().unwrap().width = 0),
            Err(ProtocolError::Invalid("picture size"))
        );
        assert_eq!(
            with(|s| s.video.as_mut().unwrap().height = MAX_DIMENSION + 1),
            Err(ProtocolError::Invalid("picture size"))
        );
        assert_eq!(
            with(|s| s.duration = Some(Timestamp::new(-1, s.time_base))),
            Err(ProtocolError::Invalid("duration of a stream"))
        );
        // The sender cuts extradata at the limit, so the reader accepts it.
        assert!(with(|s| s.extradata = vec![0; MAX_EXTRADATA + 1]).is_ok());
        let mut info = info();
        info.streams[1].audio.as_mut().unwrap().sample_rate = 0;
        assert_eq!(
            MediaInfo::parse(&info.encode()),
            Err(ProtocolError::Invalid("sample rate or channel count"))
        );
    }

    /// The bytes of a table with a raw time base and a raw stream count, as a lying helper would
    /// write them.
    fn raw_table(streams: u32, num: i64, den: i64) -> Vec<u8> {
        let mut out = Vec::new();
        put_text(&mut out, "matroska,webm", MAX_NAME);
        put_i64(&mut out, NONE);
        put_i64(&mut out, NONE);
        put_u32(&mut out, streams);
        put_u32(&mut out, 0);
        put_u32(&mut out, 1);
        put_text(&mut out, "h264", MAX_NAME);
        put_u32(&mut out, 0);
        put_i64(&mut out, num);
        put_i64(&mut out, den);
        out
    }

    #[test]
    fn a_time_base_must_be_a_positive_fraction() {
        for (num, den) in [
            (0, 1),
            (1, 0),
            (-1, 25),
            (1, -25),
            (i64::MIN, 1),
            (1, i64::MIN),
            // More than the `int` a container's time base is made of: with such parts the
            // arithmetic of a timestamp would leave its 128 bits.
            (1 << 31, 1),
            (1, 1 << 31),
            (i64::MAX, i64::MAX - 1),
        ] {
            assert_eq!(
                MediaInfo::parse(&raw_table(1, num, den)),
                Err(ProtocolError::Invalid("time base")),
                "{num}/{den}"
            );
        }
        // A good one fails later, at the end of the message.
        assert_eq!(
            MediaInfo::parse(&raw_table(1, 1, 25)),
            Err(ProtocolError::Truncated)
        );
    }

    #[test]
    fn more_streams_than_the_limit_are_refused_before_one_is_read() {
        assert_eq!(
            MediaInfo::parse(&raw_table(u32::MAX, 1, 25)),
            Err(ProtocolError::TooLarge {
                len: u64::from(u32::MAX),
                max: MAX_STREAMS as u64
            })
        );
    }

    #[test]
    fn foreign_titles_are_tamed() {
        let got = with(|s| s.title = "a\x1b]0;evil\x07b".to_owned()).unwrap();
        assert_eq!(got.streams[0].title, "a ]0;evil b");
    }

    fn record(data: &[u8], head: &PacketHead) -> Vec<u8> {
        let mut out = data.to_vec();
        out.extend_from_slice(&head.tail());
        out
    }

    #[test]
    fn a_packet_comes_back_with_times_in_the_base_of_its_stream() {
        let streams = [video_stream(0), audio_stream(1)];
        let head = PacketHead {
            serial: 7,
            stream: 0,
            pts: Some(1024),
            dts: Some(512),
            duration: Some(512),
            position: Some(4096),
            key: true,
            corrupt: false,
            discard: false,
        };
        let mut bytes = record(b"compressed", &head);
        let (serial, got) = parse_record(&mut bytes, &streams).unwrap();
        assert!(bytes.is_empty());
        let Record::Packet(packet) = got else {
            panic!("{got:?}");
        };
        let base = Rational::lit(1, 12_800);
        assert_eq!(serial, 7);
        assert_eq!(packet.data, b"compressed");
        assert_eq!(packet.pts, Some(Timestamp::new(1024, base)));
        assert_eq!(packet.pts.unwrap().base(), base);
        assert_eq!(packet.dts, Some(Timestamp::new(512, base)));
        assert_eq!(packet.duration, Some(Timestamp::from_millis(40)));
        assert_eq!(packet.position, Some(4096));
        assert!(packet.key && !packet.corrupt && !packet.discard);

        let bare = PacketHead {
            stream: 1,
            ..PacketHead::default()
        };
        let (_, got) = parse_record(&mut record(b"", &bare), &streams).unwrap();
        let Record::Packet(packet) = got else {
            panic!("{got:?}");
        };
        assert_eq!(
            (packet.pts, packet.dts, packet.duration, packet.position),
            (None, None, None, None)
        );
        assert!(packet.data.is_empty());
    }

    #[test]
    fn a_packet_with_impossible_values_is_refused() {
        let streams = [video_stream(0)];
        let good = PacketHead {
            pts: Some(1),
            dts: Some(1),
            duration: Some(1),
            position: Some(1),
            ..PacketHead::default()
        };
        let refuse = |change: &dyn Fn(&mut Vec<u8>), why: &str| {
            let mut bytes = record(b"data", &good);
            let at = bytes.len() - PACKET_TAIL;
            change(&mut bytes);
            assert!(
                parse_record(&mut bytes, &streams).is_err(),
                "{why} (tail at {at})"
            );
        };
        let tail = |bytes: &mut Vec<u8>, offset: usize, value: &[u8]| {
            let at = bytes.len() - PACKET_TAIL + offset;
            bytes[at..at + value.len()].copy_from_slice(value);
        };
        refuse(&|b| tail(b, 36, &1u32.to_le_bytes()), "unknown stream");
        refuse(
            &|b| tail(b, 0, &NONE.to_le_bytes()),
            "pts is the no-value number",
        );
        refuse(
            &|b| tail(b, 8, &NONE.to_le_bytes()),
            "dts is the no-value number",
        );
        refuse(
            &|b| tail(b, 16, &(-1i64).to_le_bytes()),
            "negative duration",
        );
        refuse(
            &|b| tail(b, 24, &(-1i64).to_le_bytes()),
            "negative position",
        );
        refuse(&|b| tail(b, 40, &[0x80 | FLAG_PTS]), "unknown flag");
        refuse(&|b| tail(b, 41, &[1]), "reserved byte");
        refuse(&|b| tail(b, 40, &[0]), "times without their flags");
        refuse(&|b| b.truncate(PACKET_TAIL - 1), "shorter than its tail");
        refuse(&|b| tail(b, 43, &[9]), "unknown tag");
        refuse(&|b| b.clear(), "empty");
    }

    #[test]
    fn the_end_of_the_file_and_a_read_error_are_told_apart() {
        let streams = [video_stream(0)];
        let mut eof = end_tail(3, 0).to_vec();
        assert_eq!(
            parse_record(&mut eof, &streams),
            Ok((3, Record::End { error: None }))
        );
        let mut failed = b"Invalid data\x07 found".to_vec();
        failed.extend_from_slice(&end_tail(4, -1_094_995_529));
        assert_eq!(
            parse_record(&mut failed, &streams),
            Ok((
                4,
                Record::End {
                    error: Some((-1_094_995_529, "Invalid data  found".to_owned()))
                }
            ))
        );
        let mut lie = b"text".to_vec();
        lie.extend_from_slice(&end_tail(4, 0));
        assert!(parse_record(&mut lie, &streams).is_err());
        let mut long = vec![b'x'; MAX_ERROR_TEXT + 1];
        long.extend_from_slice(&end_tail(4, -5));
        assert!(matches!(
            parse_record(&mut long, &streams),
            Err(ProtocolError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_dropped_packet_names_an_existing_stream() {
        let streams = [video_stream(0)];
        let mut ok = dropped_record(1, 0, 99_000_000).to_vec();
        assert_eq!(
            parse_record(&mut ok, &streams),
            Ok((
                1,
                Record::Dropped {
                    stream: 0,
                    size: 99_000_000
                }
            ))
        );
        let mut unknown = dropped_record(1, 5, 1).to_vec();
        assert!(parse_record(&mut unknown, &streams).is_err());
        let mut body = b"x".to_vec();
        body.extend_from_slice(&dropped_record(1, 0, 1));
        assert_eq!(
            parse_record(&mut body, &streams),
            Err(ProtocolError::Trailing)
        );
    }

    #[test]
    fn a_seek_comes_back_and_needs_a_target() {
        let seek = Seek {
            serial: 9,
            target_micros: -1_500_000,
        };
        assert_eq!(Seek::parse(&seek.encode()), Ok(seek));
        let none = Seek {
            serial: 9,
            target_micros: NONE,
        };
        assert!(Seek::parse(&none.encode()).is_err());
        assert!(Seek::parse(&seek.encode()[..11]).is_err());
    }
}
