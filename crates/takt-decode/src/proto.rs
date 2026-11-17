// SPDX-License-Identifier: GPL-3.0-or-later
//! The protocol between the decode helper and its parent, on top of `takt-ipc`.
//!
//! # The course of a session
//!
//! The parent starts the helper and sends [`kind::CONFIGURE`] with the [`Config`] of one video
//! stream. The helper opens the decoder and answers [`kind::CONFIGURED`] or a fault. From then on
//! compressed packets travel to the helper as frames of the control pipe ([`kind::PACKET`]), and
//! decoded pictures come back as records of the ring.
//!
//! Packets are small against pictures, so they take the pipe and the ring carries one direction
//! only. The pipe has no room of its own to speak of, so the parent keeps a **window**: every
//! record of the helper states how many packets it has taken so far ([`FrameHead::consumed`]), and
//! the parent sends no more than a fixed number beyond that. A helper whose ring is full takes no
//! packets, the count stands still, and the parent stops sending: nothing piles up anywhere.
//!
//! A jump is [`kind::FLUSH`] with a new **serial**: the helper drops what it holds and every record
//! from then on carries the new serial. Records with an older serial are still in the ring at that
//! moment; the parent throws them away as it meets them. The end of the stream is
//! [`kind::DRAIN`]: the helper hands out the pictures the decoder still holds and then writes an
//! end record.
//!
//! # Records
//!
//! A record is a body followed by a fixed tail; its last byte is the tag.
//!
//! ```text
//! frame    planes | pts i64 | consumed u64 | serial u32 | width u32 | height u32
//!                 | stride u32 x 3 | format u8 | range u8 | primaries u8 | transfer u8
//!                 | matrix u8 | flags u8 | 0 | tag 1                          (tail: 48 bytes)
//! status          | consumed u64 | serial u32 | 0 | 0 | 0 | tag 2             (tail: 16 bytes)
//! end      text   | code i32 | consumed u64 | serial u32 | 0 | 0 | 0 | tag 3  (tail: 20 bytes)
//! error    text   | code i32 | consumed u64 | serial u32 | 0 | 0 | 0 | tag 4  (tail: 20 bytes)
//! ```
//!
//! The planes of a frame lie one after the other as the decoder holds them: each row is `stride`
//! bytes apart, and the last row of a plane ends with its last pixel (so a plane is
//! `stride * (rows - 1) + row_bytes` long). What lies between the end of a row and the next row is
//! padding of the decoder and never read.
//!
//! # Trust
//!
//! Everything here is parsed from bytes of a process that decodes foreign bitstreams. A frame is
//! only taken when its format is known, its size is within [`MAX_DIMENSION`], every stride is at
//! least a row and at most [`MAX_STRIDE_PADDING`] more, and the body is exactly as long as the
//! planes of that geometry are. A message that fails is a [`ProtocolError`], and the owner ends
//! the helper. The helper checks what it gets the same way.

use takt_core::{ColorPrimaries, ColorRange, PixelFormat, TransferFunction};
use takt_ipc::message::AppId;
use takt_ipc::wire::{Reader, put_blob, put_i32, put_text, put_u32};
use takt_ipc::{ProtocolError, tame};

/// The application of the decode helper in the handshake.
pub const APP: AppId = AppId {
    name: *b"DECV",
    version: 1,
};

/// The kinds of frames of this protocol.
pub mod kind {
    use takt_ipc::message::kind::APP;

    /// Parent to helper, once: the [`super::Config`] of the stream to decode.
    pub const CONFIGURE: u16 = APP;
    /// Helper to parent, the answer to [`CONFIGURE`]: a [`super::Configured`].
    pub const CONFIGURED: u16 = APP + 1;
    /// Parent to helper: one compressed packet, the data followed by a [`super::PacketHead`].
    pub const PACKET: u16 = APP + 2;
    /// Parent to helper: drop everything, the payload is the new serial (`u32`).
    pub const FLUSH: u16 = APP + 3;
    /// Parent to helper, no payload: no more packets until the next flush; hand out what the
    /// decoder holds and write an end record.
    pub const DRAIN: u16 = APP + 4;
}

/// Codes of the faults the helper reports.
pub mod fault {
    /// The decoder could not be opened for this stream (no such codec in the library, a
    /// configuration it refuses).
    pub const OPEN: u32 = 1;
    /// The codec is not one the helper decodes (the list is fixed, see [`super::CODECS`]).
    pub const CODEC: u32 = 2;
    /// A request the helper does not know or cannot parse.
    pub const REQUEST: u32 = 3;
    /// The helper was built without FFmpeg (`FFMPEG_DIR` not set at build time).
    pub const NO_FFMPEG: u32 = 4;
    /// The FFmpeg libraries found at run time are not the major versions the helper was written
    /// for.
    pub const FFMPEG_VERSION: u32 = 5;
}

/// The codecs the helper decodes, by FFmpeg's name of the codec, and the decoder of libavcodec
/// that does it. AV1 is decoded by dav1d (libavcodec's wrapper around it), never by libavcodec's
/// own AV1 decoder, which needs hardware. Every further codec is a parser more that reads foreign
/// bytes; it joins this list on purpose.
pub const CODECS: [(&str, &str); 3] = [("h264", "h264"), ("hevc", "hevc"), ("av1", "libdav1d")];

/// The decoder of libavcodec for the codec `codec`, when the helper decodes it.
#[must_use]
pub fn decoder_for(codec: &str) -> Option<&'static str> {
    CODECS
        .iter()
        .find(|(name, _)| *name == codec)
        .map(|(_, decoder)| *decoder)
}

/// Longest name of a codec or decoder, in bytes.
pub const MAX_NAME: usize = 64;
/// Most bytes of codec configuration (extradata).
pub const MAX_EXTRADATA: usize = 1 << 20;
/// Largest picture dimension that is taken for one.
pub const MAX_DIMENSION: u32 = 1 << 14;
/// Most bytes between the end of a row and the start of the next that a frame may claim.
pub const MAX_STRIDE_PADDING: u32 = 4096;
/// Longest text of an error, in bytes.
pub const MAX_ERROR_TEXT: usize = 512;
/// Most threads a decoder may be asked for.
pub const MAX_THREADS: u32 = 64;

/// Bytes of the tail of a frame record.
pub const FRAME_TAIL: usize = 48;
/// Bytes of the tail of a packet on the pipe.
pub const PACKET_TAIL: usize = 32;
const STATUS_TAIL: usize = 16;
const NOTE_TAIL: usize = 20;

const TAG_FRAME: u8 = 1;
const TAG_STATUS: u8 = 2;
const TAG_END: u8 = 3;
const TAG_ERROR: u8 = 4;

const PACKET_KEY: u8 = 1;
const PACKET_CORRUPT: u8 = 1 << 1;
const PACKET_DISCARD: u8 = 1 << 2;
const PACKET_PTS: u8 = 1 << 3;
const PACKET_DTS: u8 = 1 << 4;
const PACKET_DURATION: u8 = 1 << 5;

const FRAME_PTS: u8 = 1;
const FRAME_KEY: u8 = 1 << 1;
const FRAME_CORRUPT: u8 = 1 << 2;

/// "No value" for an optional `i64` (what FFmpeg calls `AV_NOPTS_VALUE`): never a time on the
/// wire.
const NONE: i64 = i64::MIN;

/// The stream a helper is to decode: what `takt_demux::StreamInfo` says about it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// FFmpeg's name of the codec (`h264`, `hevc`, `av1`).
    pub codec: String,
    /// The codec configuration from the container (`avcC`, `hvcC`, `av1C`). Foreign bytes.
    pub extradata: Vec<u8>,
    /// Width in pixels as the container states it, 0 when unknown.
    pub width: u32,
    /// Height in pixels as the container states it, 0 when unknown.
    pub height: u32,
    /// Codec profile (FFmpeg's number), -99 when unknown.
    pub profile: i32,
    /// Codec level (FFmpeg's number), -99 when unknown.
    pub level: i32,
    /// Threads the decoder may use, 0 for as many as the machine has cores.
    pub threads: u32,
}

impl Config {
    /// The payload of [`kind::CONFIGURE`].
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + self.extradata.len());
        put_text(&mut out, &self.codec, MAX_NAME);
        put_blob(
            &mut out,
            &self.extradata[..self.extradata.len().min(MAX_EXTRADATA)],
        );
        put_u32(&mut out, self.width);
        put_u32(&mut out, self.height);
        put_i32(&mut out, self.profile);
        put_i32(&mut out, self.level);
        put_u32(&mut out, self.threads);
        out
    }

    /// Reads the payload of [`kind::CONFIGURE`].
    ///
    /// # Errors
    /// A [`ProtocolError`] for anything outside the limits.
    pub fn parse(payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(payload);
        let codec = name(&mut r, "codec name")?;
        let extradata = r.blob(MAX_EXTRADATA)?.to_vec();
        let width = r.u32()?;
        let height = r.u32()?;
        let profile = r.i32()?;
        let level = r.i32()?;
        let threads = r.u32()?;
        r.finish()?;
        if width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(ProtocolError::Invalid("picture size"));
        }
        if threads > MAX_THREADS {
            return Err(ProtocolError::Invalid("thread count"));
        }
        Ok(Self {
            codec,
            extradata,
            width,
            height,
            profile,
            level,
            threads,
        })
    }
}

/// A name FFmpeg made (codec, decoder): printable ASCII without spaces.
fn name(r: &mut Reader<'_>, field: &'static str) -> Result<String, ProtocolError> {
    let text = r.text(MAX_NAME)?;
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_graphic()) {
        Ok(text.to_owned())
    } else {
        Err(ProtocolError::Invalid(field))
    }
}

/// The answer to a [`Config`]: the decoder that was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configured {
    /// libavcodec's name of the decoder (`h264`, `hevc`, `libdav1d`).
    pub decoder: String,
}

impl Configured {
    /// The payload of [`kind::CONFIGURED`].
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.decoder.len());
        put_text(&mut out, &self.decoder, MAX_NAME);
        out
    }

    /// Reads the payload of [`kind::CONFIGURED`].
    ///
    /// # Errors
    /// A [`ProtocolError`].
    pub fn parse(payload: &[u8]) -> Result<Self, ProtocolError> {
        let mut r = Reader::new(payload);
        let decoder = name(&mut r, "decoder name")?;
        r.finish()?;
        Ok(Self { decoder })
    }
}

/// The payload of [`kind::FLUSH`].
#[must_use]
pub fn flush(serial: u32) -> [u8; 4] {
    serial.to_le_bytes()
}

/// Reads the payload of [`kind::FLUSH`]: the new serial.
///
/// # Errors
/// A [`ProtocolError`].
pub fn parse_flush(payload: &[u8]) -> Result<u32, ProtocolError> {
    let mut r = Reader::new(payload);
    let serial = r.u32()?;
    r.finish()?;
    Ok(serial)
}

/// The times and flags of a packet on its way to the helper: ticks in the stream's time base,
/// which the helper hands through to the frames without looking at them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PacketHead {
    /// The serial of the flush this packet comes after.
    pub serial: u32,
    /// Presentation time in ticks.
    pub pts: Option<i64>,
    /// Decoding time in ticks.
    pub dts: Option<i64>,
    /// Duration in ticks.
    pub duration: Option<i64>,
    /// A decoder can start here.
    pub key: bool,
    /// Marked as damaged by the container.
    pub corrupt: bool,
    /// To be decoded but not shown.
    pub discard: bool,
}

impl PacketHead {
    /// The tail of a packet; the payload of [`kind::PACKET`] is the data followed by these bytes.
    #[must_use]
    pub fn tail(&self) -> [u8; PACKET_TAIL] {
        let mut flags = 0u8;
        let mut set = |on: bool, flag: u8| {
            if on {
                flags |= flag;
            }
        };
        set(self.key, PACKET_KEY);
        set(self.corrupt, PACKET_CORRUPT);
        set(self.discard, PACKET_DISCARD);
        set(self.pts.is_some(), PACKET_PTS);
        set(self.dts.is_some(), PACKET_DTS);
        set(self.duration.is_some(), PACKET_DURATION);
        let mut tail = [0u8; PACKET_TAIL];
        tail[0..8].copy_from_slice(&self.pts.unwrap_or(0).to_le_bytes());
        tail[8..16].copy_from_slice(&self.dts.unwrap_or(0).to_le_bytes());
        tail[16..24].copy_from_slice(&self.duration.unwrap_or(0).to_le_bytes());
        tail[24..28].copy_from_slice(&self.serial.to_le_bytes());
        tail[28] = flags;
        tail
    }
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

/// A time that is given or is not: the "no value" number must not arrive as a time, and a field
/// that is not given must be zero.
fn optional(value: i64, given: bool, field: &'static str) -> Result<Option<i64>, ProtocolError> {
    if !given {
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
}

/// Reads the payload of [`kind::PACKET`]: the compressed data and its head.
///
/// # Errors
/// A [`ProtocolError`].
pub fn parse_packet(payload: &[u8]) -> Result<(&[u8], PacketHead), ProtocolError> {
    let (data, tail) = tail_of::<PACKET_TAIL>(payload)?;
    let mut r = Reader::new(&tail);
    let (pts, dts, duration) = (r.i64()?, r.i64()?, r.i64()?);
    let serial = r.u32()?;
    let flags = r.u8()?;
    if r.u8()? != 0 || r.u8()? != 0 || r.u8()? != 0 {
        return Err(ProtocolError::Invalid("reserved bytes of a packet"));
    }
    let known =
        PACKET_KEY | PACKET_CORRUPT | PACKET_DISCARD | PACKET_PTS | PACKET_DTS | PACKET_DURATION;
    if flags & !known != 0 {
        return Err(ProtocolError::Invalid("flags of a packet"));
    }
    let duration = optional(
        duration,
        flags & PACKET_DURATION != 0,
        "duration of a packet",
    )?;
    if duration.is_some_and(|d| d < 0) {
        return Err(ProtocolError::Invalid("duration of a packet"));
    }
    Ok((
        data,
        PacketHead {
            serial,
            pts: optional(pts, flags & PACKET_PTS != 0, "pts")?,
            dts: optional(dts, flags & PACKET_DTS != 0, "dts")?,
            duration,
            key: flags & PACKET_KEY != 0,
            corrupt: flags & PACKET_CORRUPT != 0,
            discard: flags & PACKET_DISCARD != 0,
        },
    ))
}

/// How the planes of a frame lie in a record: what the decoder produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireFormat {
    /// Three planes, 8 bit, 4:2:0 (FFmpeg's `yuv420p` and `yuvj420p`).
    Yuv420P8,
    /// Three planes, 10 bit in 16-bit words of little endian, 4:2:0 (`yuv420p10le`).
    Yuv420P10,
    /// Three planes, 8 bit, 4:4:4 (`yuv444p` and `yuvj444p`).
    Yuv444P8,
}

impl WireFormat {
    /// The number on the wire.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Yuv420P8 => 1,
            Self::Yuv420P10 => 2,
            Self::Yuv444P8 => 3,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Yuv420P8,
            2 => Self::Yuv420P10,
            3 => Self::Yuv444P8,
            _ => return None,
        })
    }

    /// The format for FFmpeg's name of a pixel format, and whether that name alone says "full
    /// range" (the `yuvj` formats). `None` for a format the protocol does not carry.
    #[must_use]
    pub fn from_ffmpeg_name(name: &str) -> Option<(Self, bool)> {
        Some(match name {
            "yuv420p" => (Self::Yuv420P8, false),
            "yuvj420p" => (Self::Yuv420P8, true),
            "yuv420p10le" => (Self::Yuv420P10, false),
            "yuv444p" => (Self::Yuv444P8, false),
            "yuvj444p" => (Self::Yuv444P8, true),
            _ => return None,
        })
    }

    /// Bytes per sample.
    #[must_use]
    pub const fn sample_bytes(self) -> usize {
        match self {
            Self::Yuv420P8 | Self::Yuv444P8 => 1,
            Self::Yuv420P10 => 2,
        }
    }

    /// Whether chroma has half the width and half the height of luma.
    #[must_use]
    pub const fn is_420(self) -> bool {
        matches!(self, Self::Yuv420P8 | Self::Yuv420P10)
    }

    /// Bytes of a row and number of rows of plane `plane` (0 luma, 1 and 2 chroma) of a picture
    /// of `width` by `height` pixels.
    #[must_use]
    pub const fn plane(self, plane: usize, width: u32, height: u32) -> (usize, usize) {
        let (w, h) = if plane > 0 && self.is_420() {
            (width.div_ceil(2), height.div_ceil(2))
        } else {
            (width, height)
        };
        (w as usize * self.sample_bytes(), h as usize)
    }

    /// The frame format a picture of this format is kept in: as the decoder made it
    /// ([`Layout::Planar`]) or with the chroma planes interleaved ([`Layout::SemiPlanar`], which
    /// 4:4:4 has no form of and keeps its three planes).
    #[must_use]
    pub const fn pixel_format(self, layout: Layout) -> PixelFormat {
        match (self, layout) {
            (Self::Yuv420P8, Layout::Planar) => PixelFormat::Yuv420P,
            (Self::Yuv420P8, Layout::SemiPlanar) => PixelFormat::Nv12,
            (Self::Yuv420P10, Layout::Planar) => PixelFormat::Yuv420P10,
            (Self::Yuv420P10, Layout::SemiPlanar) => PixelFormat::P010,
            (Self::Yuv444P8, _) => PixelFormat::Yuv444P,
        }
    }
}

/// How the pictures are laid out in the frames of the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Layout {
    /// Three planes, as the decoder made them (`Yuv420P`, `Yuv420P10`, `Yuv444P`).
    #[default]
    Planar,
    /// Luma and one plane of interleaved chroma (`Nv12`, `P010`), the layout of every hardware
    /// decoder; the chroma planes are interleaved while the picture is copied into its frame.
    SemiPlanar,
}

/// What a frame record says about its picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHead {
    /// The serial of the flush this frame comes after.
    pub serial: u32,
    /// Packets the helper has taken since it started.
    pub consumed: u64,
    /// Presentation time in ticks of the stream's time base, as the packet carried it.
    pub pts: Option<i64>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// How the planes lie in the record.
    pub format: WireFormat,
    /// Bytes from one row to the next, per plane.
    pub stride: [u32; 3],
    /// Colour range: 2 is full, everything else limited (FFmpeg's `AVColorRange`).
    pub range: u8,
    /// Colour primaries (ITU-T H.273), 2 for unspecified.
    pub primaries: u8,
    /// Transfer characteristics (H.273), 2 for unspecified.
    pub transfer: u8,
    /// Matrix coefficients (H.273), 2 for unspecified.
    pub matrix: u8,
    /// The picture is a keyframe.
    pub key: bool,
    /// The decoder marks the picture as damaged.
    pub corrupt: bool,
}

impl FrameHead {
    /// The tail of a frame record; the record is the planes followed by these bytes.
    #[must_use]
    pub fn tail(&self) -> [u8; FRAME_TAIL] {
        let mut flags = 0u8;
        if self.pts.is_some() {
            flags |= FRAME_PTS;
        }
        if self.key {
            flags |= FRAME_KEY;
        }
        if self.corrupt {
            flags |= FRAME_CORRUPT;
        }
        let mut tail = [0u8; FRAME_TAIL];
        tail[0..8].copy_from_slice(&self.pts.unwrap_or(0).to_le_bytes());
        tail[8..16].copy_from_slice(&self.consumed.to_le_bytes());
        tail[16..20].copy_from_slice(&self.serial.to_le_bytes());
        tail[20..24].copy_from_slice(&self.width.to_le_bytes());
        tail[24..28].copy_from_slice(&self.height.to_le_bytes());
        tail[28..32].copy_from_slice(&self.stride[0].to_le_bytes());
        tail[32..36].copy_from_slice(&self.stride[1].to_le_bytes());
        tail[36..40].copy_from_slice(&self.stride[2].to_le_bytes());
        tail[40] = self.format.code();
        tail[41] = self.range;
        tail[42] = self.primaries;
        tail[43] = self.transfer;
        tail[44] = self.matrix;
        tail[45] = flags;
        tail[47] = TAG_FRAME;
        tail
    }

    /// Bytes of plane `plane` in the record: every row but the last is a stride long, the last
    /// ends with its last pixel.
    #[must_use]
    pub const fn plane_len(&self, plane: usize) -> usize {
        let (row, rows) = self.format.plane(plane, self.width, self.height);
        // Checked by the parser: rows is at least 1, the stride at least a row and bounded.
        self.stride[plane] as usize * rows.saturating_sub(1) + row
    }

    /// Bytes of all planes in the record.
    #[must_use]
    pub const fn body_len(&self) -> usize {
        self.plane_len(0) + self.plane_len(1) + self.plane_len(2)
    }

    /// The colour of the picture as the frame types of Takt name it.
    #[must_use]
    pub const fn color(&self) -> takt_core::ColorDescription {
        takt_core::ColorDescription {
            primaries: primaries_of(self.primaries),
            transfer: transfer_of(self.transfer),
            range: if self.range == 2 {
                ColorRange::Full
            } else {
                ColorRange::Limited
            },
        }
    }
}

/// The primaries of Takt for a code point of H.273.
#[must_use]
pub const fn primaries_of(code: u8) -> ColorPrimaries {
    match code {
        1 => ColorPrimaries::Bt709,
        5 => ColorPrimaries::Bt601Pal,
        6 | 7 => ColorPrimaries::Bt601Ntsc,
        9 => ColorPrimaries::Bt2020,
        11 => ColorPrimaries::DciP3,
        12 => ColorPrimaries::DisplayP3,
        _ => ColorPrimaries::Unknown,
    }
}

/// The transfer function of Takt for a code point of H.273.
#[must_use]
pub const fn transfer_of(code: u8) -> TransferFunction {
    match code {
        1 | 6 | 14 | 15 => TransferFunction::Bt709,
        8 => TransferFunction::Linear,
        13 => TransferFunction::Srgb,
        16 => TransferFunction::Pq,
        18 => TransferFunction::Hlg,
        _ => TransferFunction::Unknown,
    }
}

/// The record that says how many packets the helper has taken, for a packet that made no
/// picture.
#[must_use]
pub fn status_record(serial: u32, consumed: u64) -> [u8; STATUS_TAIL] {
    let mut tail = [0u8; STATUS_TAIL];
    tail[0..8].copy_from_slice(&consumed.to_le_bytes());
    tail[8..12].copy_from_slice(&serial.to_le_bytes());
    tail[15] = TAG_STATUS;
    tail
}

fn note_tail(serial: u32, consumed: u64, code: i32, tag: u8) -> [u8; NOTE_TAIL] {
    let mut tail = [0u8; NOTE_TAIL];
    tail[0..4].copy_from_slice(&code.to_le_bytes());
    tail[4..12].copy_from_slice(&consumed.to_le_bytes());
    tail[12..16].copy_from_slice(&serial.to_le_bytes());
    tail[19] = tag;
    tail
}

/// The tail of an end record: the decoder has handed out everything after a drain. The record is
/// these bytes alone.
#[must_use]
pub fn end_tail(serial: u32, consumed: u64) -> [u8; 20] {
    note_tail(serial, consumed, 0, TAG_END)
}

/// The tail of an error record; the record is the text of the error followed by these bytes.
/// `code` is the library's error number, never 0. The decoder goes on with the next packet.
#[must_use]
pub fn error_tail(serial: u32, consumed: u64, code: i32) -> [u8; 20] {
    note_tail(
        serial,
        consumed,
        if code == 0 { -1 } else { code },
        TAG_ERROR,
    )
}

/// What a record of the ring says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record<'a> {
    /// A decoded picture: its head and its planes, one after the other.
    Frame(FrameHead, &'a [u8]),
    /// Nothing but the count of packets taken.
    Status,
    /// The decoder has handed out everything after a drain.
    End,
    /// A packet was not decoded: the library's error number and text (foreign text, tamed). The
    /// stream goes on.
    Error(i32, String),
}

/// Reads one record of the ring. Returns its serial, the number of packets the helper has taken,
/// and what it says.
///
/// # Errors
/// A [`ProtocolError`] for anything that is not a record of this protocol.
pub fn parse_record(record: &[u8]) -> Result<(u32, u64, Record<'_>), ProtocolError> {
    let tag = *record.last().ok_or(ProtocolError::Truncated)?;
    match tag {
        TAG_FRAME => {
            let (body, tail) = tail_of::<FRAME_TAIL>(record)?;
            let mut r = Reader::new(&tail);
            let pts = r.i64()?;
            let consumed = r.u64()?;
            let serial = r.u32()?;
            let width = r.u32()?;
            let height = r.u32()?;
            let stride = [r.u32()?, r.u32()?, r.u32()?];
            let format =
                WireFormat::from_code(r.u8()?).ok_or(ProtocolError::Invalid("frame format"))?;
            let (range, primaries, transfer, matrix) = (r.u8()?, r.u8()?, r.u8()?, r.u8()?);
            let flags = r.u8()?;
            if r.u8()? != 0 {
                return Err(ProtocolError::Invalid("reserved byte of a frame"));
            }
            if flags & !(FRAME_PTS | FRAME_KEY | FRAME_CORRUPT) != 0 {
                return Err(ProtocolError::Invalid("flags of a frame"));
            }
            if !(1..=MAX_DIMENSION).contains(&width) || !(1..=MAX_DIMENSION).contains(&height) {
                return Err(ProtocolError::Invalid("picture size"));
            }
            let head = FrameHead {
                serial,
                consumed,
                pts: optional(pts, flags & FRAME_PTS != 0, "pts of a frame")?,
                width,
                height,
                format,
                stride,
                range,
                primaries,
                transfer,
                matrix,
                key: flags & FRAME_KEY != 0,
                corrupt: flags & FRAME_CORRUPT != 0,
            };
            for (plane, &stride) in head.stride.iter().enumerate() {
                let (row, _rows) = format.plane(plane, width, height);
                // A row is at most 2 * MAX_DIMENSION bytes: the sums below cannot overflow.
                let row = u32::try_from(row).map_err(|_| ProtocolError::Invalid("row length"))?;
                if stride < row || stride - row > MAX_STRIDE_PADDING {
                    return Err(ProtocolError::Invalid("stride of a frame"));
                }
            }
            if body.len() != head.body_len() {
                return Err(ProtocolError::Invalid("length of the planes"));
            }
            Ok((serial, consumed, Record::Frame(head, body)))
        }
        TAG_STATUS => {
            let (body, tail) = tail_of::<STATUS_TAIL>(record)?;
            if !body.is_empty() {
                return Err(ProtocolError::Trailing);
            }
            let mut r = Reader::new(&tail);
            let consumed = r.u64()?;
            let serial = r.u32()?;
            if r.u8()? != 0 || r.u8()? != 0 || r.u8()? != 0 {
                return Err(ProtocolError::Invalid("reserved bytes of a status"));
            }
            Ok((serial, consumed, Record::Status))
        }
        TAG_END | TAG_ERROR => {
            let (text, tail) = tail_of::<NOTE_TAIL>(record)?;
            let mut r = Reader::new(&tail);
            let code = r.i32()?;
            let consumed = r.u64()?;
            let serial = r.u32()?;
            if r.u8()? != 0 || r.u8()? != 0 || r.u8()? != 0 {
                return Err(ProtocolError::Invalid("reserved bytes of a record"));
            }
            if tag == TAG_END {
                if code != 0 || !text.is_empty() {
                    return Err(ProtocolError::Invalid("end record"));
                }
                return Ok((serial, consumed, Record::End));
            }
            if code == 0 {
                return Err(ProtocolError::Invalid("error without a number"));
            }
            if text.len() > MAX_ERROR_TEXT {
                return Err(ProtocolError::TooLarge {
                    len: text.len() as u64,
                    max: MAX_ERROR_TEXT as u64,
                });
            }
            let text = std::str::from_utf8(text).map_err(|_| ProtocolError::Utf8)?;
            Ok((
                serial,
                consumed,
                Record::Error(code, tame(text, MAX_ERROR_TEXT)),
            ))
        }
        _ => Err(ProtocolError::Invalid("record tag")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head() -> FrameHead {
        FrameHead {
            serial: 3,
            consumed: 17,
            pts: Some(-512),
            width: 5,
            height: 3,
            format: WireFormat::Yuv420P8,
            stride: [8, 4, 3],
            range: 1,
            primaries: 1,
            transfer: 1,
            matrix: 1,
            key: true,
            corrupt: false,
        }
    }

    fn frame(head: &FrameHead) -> Vec<u8> {
        let mut record = vec![7u8; head.body_len()];
        record.extend_from_slice(&head.tail());
        record
    }

    #[test]
    fn a_frame_comes_back_as_it_was_written() {
        let head = head();
        // Luma: two full strides and a row; chroma 3x2: a stride and a row.
        assert_eq!(head.plane_len(0), 8 * 2 + 5);
        assert_eq!(head.plane_len(1), 4 + 3);
        assert_eq!(head.plane_len(2), 3 + 3);
        let record = frame(&head);
        let (serial, consumed, parsed) = parse_record(&record).unwrap();
        assert_eq!((serial, consumed), (3, 17));
        assert_eq!(parsed, Record::Frame(head, &record[..head.body_len()]));
    }

    #[test]
    fn a_frame_whose_numbers_do_not_fit_its_bytes_is_refused() {
        let good = head();
        let refused = |change: &dyn Fn(&mut FrameHead), why: &'static str| {
            let mut head = good;
            change(&mut head);
            // The body stays the one of the good frame: the head lies about it.
            let mut record = vec![7u8; good.body_len()];
            record.extend_from_slice(&head.tail());
            assert_eq!(
                parse_record(&record),
                Err(ProtocolError::Invalid(why)),
                "{why}"
            );
        };
        refused(&|h| h.width = 0, "picture size");
        refused(&|h| h.height = MAX_DIMENSION + 1, "picture size");
        refused(&|h| h.stride[0] = 4, "stride of a frame");
        refused(
            &|h| h.stride[1] = 3 + MAX_STRIDE_PADDING + 1,
            "stride of a frame",
        );
        refused(&|h| h.height = 4, "length of the planes");
        refused(&|h| h.format = WireFormat::Yuv420P10, "stride of a frame");
        refused(&|h| h.format = WireFormat::Yuv444P8, "stride of a frame");

        let mut record = frame(&good);
        let at = record.len() - FRAME_TAIL;
        record[at + 40] = 9;
        assert_eq!(
            parse_record(&record),
            Err(ProtocolError::Invalid("frame format"))
        );
        let mut record = frame(&good);
        record[at + 46] = 1;
        assert_eq!(
            parse_record(&record),
            Err(ProtocolError::Invalid("reserved byte of a frame"))
        );
        let mut record = frame(&good);
        record[at + 45] = 0x80;
        assert_eq!(
            parse_record(&record),
            Err(ProtocolError::Invalid("flags of a frame"))
        );
        // A time that is not given must be zero, and "no value" is never a time.
        let mut record = frame(&FrameHead { pts: None, ..good });
        record[at] = 1;
        assert_eq!(
            parse_record(&record),
            Err(ProtocolError::Invalid("pts of a frame"))
        );
        let mut record = frame(&good);
        record[at..at + 8].copy_from_slice(&i64::MIN.to_le_bytes());
        assert_eq!(
            parse_record(&record),
            Err(ProtocolError::Invalid("pts of a frame"))
        );
        assert_eq!(parse_record(&[]), Err(ProtocolError::Truncated));
        assert_eq!(parse_record(&[TAG_FRAME]), Err(ProtocolError::Truncated));
        assert_eq!(
            parse_record(&[0]),
            Err(ProtocolError::Invalid("record tag"))
        );
    }

    #[test]
    fn status_end_and_error_come_back() {
        assert_eq!(
            parse_record(&status_record(2, 9)),
            Ok((2, 9, Record::Status))
        );
        assert_eq!(parse_record(&end_tail(4, 11)), Ok((4, 11, Record::End)));
        let mut error = b"Invalid data\x1b[2J found".to_vec();
        error.extend_from_slice(&error_tail(4, 12, -1_094_995_529));
        assert_eq!(
            parse_record(&error),
            Ok((
                4,
                12,
                Record::Error(-1_094_995_529, "Invalid data [2J found".to_owned())
            ))
        );
        // An error without a number gets one; an end carries neither text nor number.
        assert_eq!(
            parse_record(&error_tail(1, 1, 0)),
            Ok((1, 1, Record::Error(-1, String::new())))
        );
        let mut end = b"x".to_vec();
        end.extend_from_slice(&end_tail(1, 1));
        assert_eq!(
            parse_record(&end),
            Err(ProtocolError::Invalid("end record"))
        );
        let mut long = vec![b'x'; MAX_ERROR_TEXT + 1];
        long.extend_from_slice(&error_tail(1, 1, -5));
        assert!(matches!(
            parse_record(&long),
            Err(ProtocolError::TooLarge { .. })
        ));
        let mut status = status_record(1, 1).to_vec();
        status.insert(0, 0);
        assert_eq!(parse_record(&status), Err(ProtocolError::Trailing));
    }

    #[test]
    fn a_config_and_a_packet_come_back() {
        let config = Config {
            codec: "hevc".to_owned(),
            extradata: vec![1, 2, 3],
            width: 3840,
            height: 2160,
            profile: 2,
            level: 153,
            threads: 0,
        };
        assert_eq!(Config::parse(&config.encode()), Ok(config.clone()));
        let big = Config {
            width: MAX_DIMENSION + 1,
            ..config.clone()
        };
        assert_eq!(
            Config::parse(&big.encode()),
            Err(ProtocolError::Invalid("picture size"))
        );
        let many = Config {
            threads: MAX_THREADS + 1,
            ..config.clone()
        };
        assert_eq!(
            Config::parse(&many.encode()),
            Err(ProtocolError::Invalid("thread count"))
        );
        let odd = Config {
            codec: "h 264".to_owned(),
            ..config
        };
        assert_eq!(
            Config::parse(&odd.encode()),
            Err(ProtocolError::Invalid("codec name"))
        );

        let head = PacketHead {
            serial: 5,
            pts: Some(1024),
            dts: None,
            duration: Some(512),
            key: true,
            corrupt: false,
            discard: true,
        };
        let mut payload = b"compressed".to_vec();
        payload.extend_from_slice(&head.tail());
        assert_eq!(parse_packet(&payload), Ok((&b"compressed"[..], head)));
        // A packet without data is one (the tail alone); less than a tail is none.
        assert_eq!(parse_packet(&head.tail()), Ok((&[][..], head)));
        assert_eq!(parse_packet(&[0; 31]), Err(ProtocolError::Truncated));
        let mut lie = head.tail();
        lie[8] = 1;
        assert_eq!(parse_packet(&lie), Err(ProtocolError::Invalid("dts")));

        assert_eq!(parse_flush(&flush(7)), Ok(7));
        assert_eq!(parse_flush(&[1, 2, 3]), Err(ProtocolError::Truncated));
        let configured = Configured {
            decoder: "libdav1d".to_owned(),
        };
        assert_eq!(Configured::parse(&configured.encode()), Ok(configured));
    }

    #[test]
    fn av1_goes_to_dav1d_and_other_codecs_nowhere() {
        assert_eq!(decoder_for("av1"), Some("libdav1d"));
        assert_eq!(decoder_for("h264"), Some("h264"));
        assert_eq!(decoder_for("hevc"), Some("hevc"));
        assert_eq!(decoder_for("vp9"), None);
        assert_eq!(decoder_for(""), None);
    }

    #[test]
    fn formats_map_to_the_frames_of_the_pool() {
        use WireFormat::{Yuv420P8, Yuv420P10, Yuv444P8};
        assert_eq!(Yuv420P8.pixel_format(Layout::Planar), PixelFormat::Yuv420P);
        assert_eq!(Yuv420P8.pixel_format(Layout::SemiPlanar), PixelFormat::Nv12);
        assert_eq!(
            Yuv420P10.pixel_format(Layout::SemiPlanar),
            PixelFormat::P010
        );
        assert_eq!(
            Yuv444P8.pixel_format(Layout::SemiPlanar),
            PixelFormat::Yuv444P
        );
        assert_eq!(Yuv420P10.plane(1, 1921, 1081), (961 * 2, 541));
        assert_eq!(Yuv444P8.plane(2, 1921, 1081), (1921, 1081));
        assert_eq!(
            WireFormat::from_ffmpeg_name("yuvj420p"),
            Some((Yuv420P8, true))
        );
        assert_eq!(WireFormat::from_ffmpeg_name("yuv422p"), None);
        for format in [Yuv420P8, Yuv420P10, Yuv444P8] {
            assert_eq!(WireFormat::from_code(format.code()), Some(format));
        }
        assert_eq!(primaries_of(9), ColorPrimaries::Bt2020);
        assert_eq!(primaries_of(2), ColorPrimaries::Unknown);
        assert_eq!(transfer_of(16), TransferFunction::Pq);
        assert_eq!(transfer_of(2), TransferFunction::Unknown);
    }
}
