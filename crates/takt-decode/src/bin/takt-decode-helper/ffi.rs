// SPDX-License-Identifier: GPL-3.0-or-later
//! libavcodec behind a safe type: [`Video`] is one opened video decoder.
//!
//! The one module of the crate with `unsafe` (KONZEPT, unsafe policy), and it only exists in a
//! build with FFmpeg (`FFMPEG_DIR`, cfg `takt_ffmpeg`). It is a module of the helper program, not
//! of the library: a program that links `takt-decode` to use the decoder links no FFmpeg and
//! loads none, which is the point of the process boundary (the test `the_client_links_no_ffmpeg`
//! holds that).
//!
//! # Why hand-written, and what that costs
//!
//! Decided on 2026-11-17 with `takt-demux` (`docs/reviews/phase-1.md`): no `ffmpeg-sys-next`
//! (WTFPL, bindgen and libclang on every machine that builds). A decoder calls about twenty
//! functions; they are declared here by hand.
//!
//! The decoder context is only handled through a pointer: its settings go in through
//! `avcodec_parameters_to_context` and the option interface, so its layout is none of this
//! module's business. Three structures have no such way and are declared by their leading part,
//! up to the last field this module reads or writes: `AVCodecParameters` (filled from the stream
//! table), `AVPacket` (filled per packet) and `AVFrame` (read per picture). That part is fixed
//! for a major version of the library, so:
//!
//! * the build refuses headers of another major version (`build.rs`),
//! * [`libraries_match`] refuses libraries of another major version at run time,
//! * the offsets below are asserted at compile time against the numbers a C compiler printed for
//!   the real headers (`layout/layout.c`, for Windows x64 and Linux x64), and
//! * the tests compare every decoded picture with what the `ffmpeg` program of the same FFmpeg
//!   decodes from the same file, byte for byte.
//!
//! A new major version of FFmpeg means: run `layout/layout.c` again, compare, raise the numbers.
//!
//! # dav1d
//!
//! AV1 is decoded by dav1d through libavcodec's decoder `libdav1d`, which the LGPL builds this
//! helper is made for carry. That keeps one binding and one library to load; dav1d's own API
//! would be a second hand-written binding for the same pictures.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::marker::{PhantomData, PhantomPinned};
use std::mem::offset_of;
use std::ptr::{self, NonNull};

use takt_decode::proto::{
    self, Config, FrameHead, MAX_DIMENSION, MAX_STRIDE_PADDING, PacketHead, WireFormat,
};

/// The major versions this module is written for (FFmpeg 9.0).
const AVCODEC_MAJOR: c_uint = 63;
const AVUTIL_MAJOR: c_uint = 61;

const AV_NOPTS_VALUE: i64 = i64::MIN;
const AVERROR_EOF: c_int = -541_478_725;
/// `AVERROR(EAGAIN)`: `EAGAIN` is 11 for the C libraries of Windows and Linux, 35 for macOS.
#[cfg(not(target_os = "macos"))]
const AVERROR_EAGAIN: c_int = -11;
#[cfg(target_os = "macos")]
const AVERROR_EAGAIN: c_int = -35;
const AVERROR_EINVAL: c_int = -22;
const AV_LOG_QUIET: c_int = -8;
const AVMEDIA_TYPE_VIDEO: c_int = 0;
const AV_PKT_FLAG_KEY: c_int = 1;
const AV_PKT_FLAG_CORRUPT: c_int = 2;
const AV_PKT_FLAG_DISCARD: c_int = 4;
const AV_FRAME_FLAG_CORRUPT: c_int = 1;
const AV_FRAME_FLAG_KEY: c_int = 2;
/// Bytes of zeros libavcodec wants behind every buffer of compressed data.
const AV_INPUT_BUFFER_PADDING_SIZE: usize = 64;

/// A structure of the library that is only handled through pointers. The marker keeps it from
/// being `Send`, `Sync` or `Unpin` by accident (Nomicon pattern).
#[repr(C)]
struct Opaque {
    _private: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// `AVRational`.
#[repr(C)]
#[derive(Clone, Copy)]
struct AVRational {
    num: c_int,
    den: c_int,
}

/// The head of `AVCodec`: its name and what it codes.
#[repr(C)]
struct AVCodec {
    name: *const c_char,
    long_name: *const c_char,
    kind: c_int,
    id: c_int,
}

/// The head of `AVCodecParameters` (libavcodec 63), up to `chroma_location`.
#[repr(C)]
struct AVCodecParameters {
    codec_type: c_int,
    codec_id: c_int,
    codec_tag: u32,
    extradata: *mut u8,
    extradata_size: c_int,
    coded_side_data: *mut c_void,
    nb_coded_side_data: c_int,
    format: c_int,
    bit_rate: i64,
    bits_per_coded_sample: c_int,
    bits_per_raw_sample: c_int,
    profile: c_int,
    level: c_int,
    width: c_int,
    height: c_int,
    sample_aspect_ratio: AVRational,
    framerate: AVRational,
    field_order: c_int,
    color_range: c_int,
    color_primaries: c_int,
    color_trc: c_int,
    color_space: c_int,
    chroma_location: c_int,
}

/// The head of `AVPacket` (libavcodec 63), up to `duration`.
#[repr(C)]
struct AVPacket {
    buf: *mut c_void,
    pts: i64,
    dts: i64,
    data: *mut u8,
    size: c_int,
    stream_index: c_int,
    flags: c_int,
    side_data: *mut c_void,
    side_data_elems: c_int,
    duration: i64,
}

/// The head of `AVFrame` (libavutil 61), up to `best_effort_timestamp`.
#[repr(C)]
struct AVFrame {
    data: [*mut u8; 8],
    linesize: [c_int; 8],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
    pict_type: c_int,
    sample_aspect_ratio: AVRational,
    pts: i64,
    pkt_dts: i64,
    time_base: AVRational,
    quality: c_int,
    opaque: *mut c_void,
    repeat_pict: c_int,
    sample_rate: c_int,
    buf: [*mut c_void; 8],
    extended_buf: *mut *mut c_void,
    nb_extended_buf: c_int,
    side_data: *mut *mut c_void,
    nb_side_data: c_int,
    flags: c_int,
    color_range: c_int,
    color_primaries: c_int,
    color_trc: c_int,
    colorspace: c_int,
    chroma_location: c_int,
    best_effort_timestamp: i64,
}

// The offsets `layout/layout.c` printed for FFmpeg 9.0 (libavcodec 63, libavutil 61), identical
// for x86_64-pc-windows-msvc and x86_64-unknown-linux-gnu. A structure above that drifts from the
// header fails the build here.
#[cfg(all(target_pointer_width = "64", any(windows, unix)))]
const _: () = {
    assert!(offset_of!(AVCodec, name) == 0);
    assert!(offset_of!(AVCodec, kind) == 16);
    assert!(offset_of!(AVCodec, id) == 20);
    assert!(offset_of!(AVCodecParameters, codec_id) == 4);
    assert!(offset_of!(AVCodecParameters, codec_tag) == 8);
    assert!(offset_of!(AVCodecParameters, extradata) == 16);
    assert!(offset_of!(AVCodecParameters, extradata_size) == 24);
    assert!(offset_of!(AVCodecParameters, format) == 44);
    assert!(offset_of!(AVCodecParameters, profile) == 64);
    assert!(offset_of!(AVCodecParameters, level) == 68);
    assert!(offset_of!(AVCodecParameters, width) == 72);
    assert!(offset_of!(AVCodecParameters, height) == 76);
    assert!(offset_of!(AVCodecParameters, color_range) == 100);
    assert!(offset_of!(AVCodecParameters, color_primaries) == 104);
    assert!(offset_of!(AVCodecParameters, color_trc) == 108);
    assert!(offset_of!(AVCodecParameters, color_space) == 112);
    assert!(offset_of!(AVCodecParameters, chroma_location) == 116);
    assert!(offset_of!(AVPacket, pts) == 8);
    assert!(offset_of!(AVPacket, dts) == 16);
    assert!(offset_of!(AVPacket, data) == 24);
    assert!(offset_of!(AVPacket, size) == 32);
    assert!(offset_of!(AVPacket, flags) == 40);
    assert!(offset_of!(AVPacket, duration) == 64);
    assert!(offset_of!(AVFrame, data) == 0);
    assert!(offset_of!(AVFrame, linesize) == 64);
    assert!(offset_of!(AVFrame, width) == 104);
    assert!(offset_of!(AVFrame, height) == 108);
    assert!(offset_of!(AVFrame, format) == 116);
    assert!(offset_of!(AVFrame, pts) == 136);
    assert!(offset_of!(AVFrame, flags) == 276);
    assert!(offset_of!(AVFrame, color_range) == 280);
    assert!(offset_of!(AVFrame, color_primaries) == 284);
    assert!(offset_of!(AVFrame, color_trc) == 288);
    assert!(offset_of!(AVFrame, colorspace) == 292);
    assert!(offset_of!(AVFrame, chroma_location) == 296);
    assert!(offset_of!(AVFrame, best_effort_timestamp) == 304);
};

#[link(name = "avcodec")]
unsafe extern "C" {
    /// `(major << 16) | (minor << 8) | micro` of the library that was loaded.
    safe fn avcodec_version() -> c_uint;
    /// A static, NUL-terminated text.
    safe fn avcodec_license() -> *const c_char;
    /// The static description of a decoder, or null when the library has none of that name.
    fn avcodec_find_decoder_by_name(name: *const c_char) -> *const AVCodec;
    fn avcodec_alloc_context3(codec: *const AVCodec) -> *mut Opaque;
    fn avcodec_free_context(avctx: *mut *mut Opaque);
    /// New parameters with every field at its "unknown", or null without memory.
    safe fn avcodec_parameters_alloc() -> *mut AVCodecParameters;
    fn avcodec_parameters_free(par: *mut *mut AVCodecParameters);
    fn avcodec_parameters_to_context(codec: *mut Opaque, par: *const AVCodecParameters) -> c_int;
    fn avcodec_open2(avctx: *mut Opaque, codec: *const AVCodec, options: *mut *mut Opaque)
    -> c_int;
    fn avcodec_send_packet(avctx: *mut Opaque, avpkt: *const AVPacket) -> c_int;
    fn avcodec_receive_frame(avctx: *mut Opaque, frame: *mut AVFrame) -> c_int;
    fn avcodec_flush_buffers(avctx: *mut Opaque);
    /// A new, blank packet, or null without memory.
    safe fn av_packet_alloc() -> *mut AVPacket;
    fn av_packet_free(pkt: *mut *mut AVPacket);
    fn av_packet_unref(pkt: *mut AVPacket);
    fn av_new_packet(pkt: *mut AVPacket, size: c_int) -> c_int;
}

#[link(name = "avutil")]
unsafe extern "C" {
    safe fn avutil_version() -> c_uint;
    safe fn av_log_set_level(level: c_int);
    /// A new, blank frame, or null without memory.
    safe fn av_frame_alloc() -> *mut AVFrame;
    fn av_frame_free(frame: *mut *mut AVFrame);
    fn av_frame_unref(frame: *mut AVFrame);
    /// Zeroed memory for the library, or null.
    safe fn av_mallocz(size: usize) -> *mut c_void;
    fn av_strerror(errnum: c_int, errbuf: *mut c_char, errbuf_size: usize) -> c_int;
    /// The name of a pixel format, or null for a number it does not know.
    safe fn av_get_pix_fmt_name(pix_fmt: c_int) -> *const c_char;
    fn av_opt_set_int(
        obj: *mut c_void,
        name: *const c_char,
        val: i64,
        search_flags: c_int,
    ) -> c_int;
}

/// An error of the library: its number and its text.
pub(crate) type AvError = (i32, String);

fn av_error(code: c_int) -> AvError {
    let mut buf = [0 as c_char; 128];
    // SAFETY: the buffer is 128 bytes long and that length is passed; the call writes a
    // NUL-terminated text into it, also for a number it does not know.
    let text = unsafe {
        av_strerror(code, buf.as_mut_ptr(), buf.len());
        CStr::from_ptr(buf.as_ptr())
    };
    (code, text.to_string_lossy().into_owned())
}

fn no_memory() -> AvError {
    (0, "out of memory".to_owned())
}

/// The libraries the helper runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Libraries {
    /// `(major, minor, micro)` of libavcodec and libavutil.
    pub versions: [(u32, u32, u32); 2],
    /// What libavcodec says about its license ("LGPL version 3 or later").
    pub license: String,
}

/// The versions and the license of the loaded libraries.
pub(crate) fn libraries() -> Libraries {
    let split = |v: c_uint| (v >> 16, (v >> 8) & 0xFF, v & 0xFF);
    // SAFETY: avcodec_license returns a static NUL-terminated string that lives for the whole
    // process and is never written to.
    let license = unsafe { CStr::from_ptr(avcodec_license()) }
        .to_string_lossy()
        .into_owned();
    Libraries {
        versions: [split(avcodec_version()), split(avutil_version())],
        license,
    }
}

/// Whether the loaded libraries are the major versions the structures above describe.
pub(crate) fn libraries_match() -> Result<(), String> {
    let got = libraries().versions;
    let want = [AVCODEC_MAJOR, AVUTIL_MAJOR];
    if got.iter().map(|v| v.0).eq(want) {
        Ok(())
    } else {
        Err(format!(
            "libavcodec {}, libavutil {} loaded; built for {}, {}",
            got[0].0, got[1].0, want[0], want[1]
        ))
    }
}

/// Whether the loaded libavcodec has the decoder `name`.
pub(crate) fn has_decoder(name: &str) -> bool {
    find_decoder(name).is_some()
}

fn find_decoder(name: &str) -> Option<NonNull<AVCodec>> {
    let name = CString::new(name).ok()?;
    // SAFETY: the name is NUL-terminated and lives across the call; the result is null or a
    // pointer to a static description of the library.
    let codec = unsafe { avcodec_find_decoder_by_name(name.as_ptr()) };
    NonNull::new(codec.cast_mut())
}

/// What [`Video::receive`] found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Received {
    /// A picture: [`Video::picture`] is its view until [`Video::release`].
    Frame,
    /// The decoder wants more packets first.
    Again,
    /// The decoder has handed out everything after [`Video::send_end`].
    End,
}

/// One decoded picture as the decoder holds it.
pub(crate) struct Picture<'a> {
    /// What the record says about it; `serial` and `consumed` are left at 0 for the session.
    pub head: FrameHead,
    /// The three planes: every row but the last a stride long, the last ending with its last
    /// pixel.
    pub planes: [&'a [u8]; 3],
}

/// One opened video decoder.
///
/// Not `Send`: the library's context stays with the thread that opened it.
pub(crate) struct Video {
    ctx: NonNull<Opaque>,
    packet: NonNull<AVPacket>,
    frame: NonNull<AVFrame>,
    decoder: &'static str,
}

impl Video {
    /// Opens the decoder for `config`.
    ///
    /// # Errors
    /// The library's error number and text; 0 with a text for a failure outside the library.
    pub(crate) fn open(config: &Config) -> Result<Self, AvError> {
        libraries_match().map_err(|why| (0, why))?;
        av_log_set_level(AV_LOG_QUIET);
        let decoder = proto::decoder_for(&config.codec)
            .ok_or_else(|| (0, format!("codec {} is not decoded", config.codec)))?;
        let codec = find_decoder(decoder)
            .ok_or_else(|| (0, format!("this libavcodec has no decoder {decoder}")))?;
        // SAFETY: `codec` points to a static description of the library.
        if unsafe { codec.as_ref() }.kind != AVMEDIA_TYPE_VIDEO {
            return Err((0, format!("{decoder} is no video decoder")));
        }
        let int = |value: u32| c_int::try_from(value).map_err(|_| (0, "size".to_owned()));
        let (width, height) = (int(config.width)?, int(config.height)?);
        let extradata_size = c_int::try_from(config.extradata.len())
            .map_err(|_| (0, "codec configuration too large".to_owned()))?;

        // From here on `parts` frees what was made so far, on every return.
        let mut parts = Parts {
            ctx: ptr::null_mut(),
            packet: ptr::null_mut(),
            frame: ptr::null_mut(),
        };
        let mut par = avcodec_parameters_alloc();
        if par.is_null() {
            return Err(no_memory());
        }
        // SAFETY: `par` is a live structure of the library nobody else has. The codec
        // configuration is copied into memory of the library's allocator with the zeroed padding
        // the decoders read into (`av_mallocz` of size plus padding), and the parameters own it
        // from the moment the pointer is stored. The parameters are copied into the new context
        // and freed, on every path (nothing in the block returns early). `None` is "no memory".
        let made = unsafe {
            (*par).codec_type = AVMEDIA_TYPE_VIDEO;
            (*par).codec_id = codec.as_ref().id;
            (*par).width = width;
            (*par).height = height;
            (*par).profile = config.profile;
            (*par).level = config.level;
            let mut copied = true;
            if !config.extradata.is_empty() {
                let copy =
                    av_mallocz(config.extradata.len() + AV_INPUT_BUFFER_PADDING_SIZE).cast::<u8>();
                if copy.is_null() {
                    copied = false;
                } else {
                    ptr::copy_nonoverlapping(
                        config.extradata.as_ptr(),
                        copy,
                        config.extradata.len(),
                    );
                    (*par).extradata = copy;
                    (*par).extradata_size = extradata_size;
                }
            }
            if copied {
                parts.ctx = avcodec_alloc_context3(codec.as_ptr());
            }
            let rc = if parts.ctx.is_null() {
                None
            } else {
                Some(avcodec_parameters_to_context(parts.ctx, par))
            };
            avcodec_parameters_free(&raw mut par);
            rc
        };
        match made {
            None => return Err(no_memory()),
            Some(rc) if rc < 0 => return Err(av_error(rc)),
            Some(_) => {}
        }
        // SAFETY: the context is live and not open yet; the option name is NUL-terminated. The
        // context of a decoder has the option `threads` (0 lets the library count the cores); a
        // library that refuses the value decodes with its default, which is no error.
        // The decoder is opened with the codec the context was made for and no options.
        let opened = unsafe {
            av_opt_set_int(
                parts.ctx.cast(),
                c"threads".as_ptr(),
                i64::from(config.threads),
                0,
            );
            avcodec_open2(parts.ctx, codec.as_ptr(), ptr::null_mut())
        };
        if opened < 0 {
            return Err(av_error(opened));
        }
        parts.packet = av_packet_alloc();
        parts.frame = av_frame_alloc();
        let (Some(ctx), Some(packet), Some(frame)) = (
            NonNull::new(parts.ctx),
            NonNull::new(parts.packet),
            NonNull::new(parts.frame),
        ) else {
            return Err(no_memory());
        };
        // Ownership moves to the `Video`; `parts` must not free anything any more.
        parts.ctx = ptr::null_mut();
        parts.packet = ptr::null_mut();
        parts.frame = ptr::null_mut();
        Ok(Self {
            ctx,
            packet,
            frame,
            decoder,
        })
    }

    /// libavcodec's name of the decoder.
    pub(crate) const fn decoder(&self) -> &'static str {
        self.decoder
    }

    /// Hands one packet to the decoder. `Ok(false)` when it takes none before its pictures were
    /// fetched ([`receive`](Self::receive)): offer the packet again then.
    ///
    /// # Errors
    /// The library's error number and text: the packet was not decoded, the decoder goes on.
    pub(crate) fn send(&mut self, data: &[u8], head: &PacketHead) -> Result<bool, AvError> {
        if data.is_empty() {
            // An empty packet means "no more packets" to the library; a container can hold one.
            return Ok(true);
        }
        let size = c_int::try_from(data.len()).map_err(|_| av_error(AVERROR_EINVAL))?;
        // SAFETY: the packet belongs to this `Video` and is blank (it is unreferenced after every
        // use). `av_new_packet` gives it a buffer of `size` bytes plus the zeroed padding; the
        // data is copied into exactly those bytes. The context is open; the library copies or
        // references what it needs before the call returns, and the packet is blank again after.
        let rc = unsafe {
            let packet = self.packet.as_ptr();
            let rc = av_new_packet(packet, size);
            if rc < 0 {
                return Err(av_error(rc));
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (*packet).data, data.len());
            (*packet).pts = head.pts.unwrap_or(AV_NOPTS_VALUE);
            (*packet).dts = head.dts.unwrap_or(AV_NOPTS_VALUE);
            (*packet).duration = head.duration.unwrap_or(0);
            let mut flags = 0;
            if head.key {
                flags |= AV_PKT_FLAG_KEY;
            }
            if head.corrupt {
                flags |= AV_PKT_FLAG_CORRUPT;
            }
            if head.discard {
                flags |= AV_PKT_FLAG_DISCARD;
            }
            (*packet).flags = flags;
            let rc = avcodec_send_packet(self.ctx.as_ptr(), packet);
            av_packet_unref(packet);
            rc
        };
        match rc {
            AVERROR_EAGAIN => Ok(false),
            rc if rc < 0 => Err(av_error(rc)),
            _ => Ok(true),
        }
    }

    /// Tells the decoder that no more packets come: it hands out what it holds, then
    /// [`Received::End`].
    pub(crate) fn send_end(&mut self) {
        // SAFETY: the context is open; a null packet is how the header says the end is signalled.
        // Saying it twice is an error of no consequence.
        unsafe { avcodec_send_packet(self.ctx.as_ptr(), ptr::null()) };
    }

    /// Fetches the next picture of the decoder. The picture before it is released.
    ///
    /// # Errors
    /// The library's error number and text: a picture was lost, the decoder goes on.
    pub(crate) fn receive(&mut self) -> Result<Received, AvError> {
        // SAFETY: context and frame belong to this `Video`; the call unreferences the frame
        // before it fills it.
        let rc = unsafe { avcodec_receive_frame(self.ctx.as_ptr(), self.frame.as_ptr()) };
        match rc {
            AVERROR_EAGAIN => Ok(Received::Again),
            AVERROR_EOF => Ok(Received::End),
            rc if rc < 0 => Err(av_error(rc)),
            _ => Ok(Received::Frame),
        }
    }

    /// The picture the last [`receive`](Self::receive) returned.
    ///
    /// # Errors
    /// A text when the picture is nothing the protocol carries (its format, its size, the way
    /// its rows lie).
    pub(crate) fn picture(&self) -> Result<Picture<'_>, String> {
        // SAFETY: the frame belongs to this `Video` and nothing writes it while `&self` is held
        // (the library only does inside calls that take `&mut self`).
        let frame = unsafe { self.frame.as_ref() };
        let name = av_get_pix_fmt_name(frame.format);
        let name = if name.is_null() {
            String::new()
        } else {
            // SAFETY: a non-null result is a static, NUL-terminated name of the library.
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        };
        let Some((format, full_range)) = WireFormat::from_ffmpeg_name(&name) else {
            return Err(format!("pixel format {name} is not carried"));
        };
        let dimension = |value: c_int| {
            u32::try_from(value)
                .ok()
                .filter(|v| (1..=MAX_DIMENSION).contains(v))
        };
        let (Some(width), Some(height)) = (dimension(frame.width), dimension(frame.height)) else {
            return Err(format!("picture size {}x{}", frame.width, frame.height));
        };
        let mut stride = [0u32; 3];
        let mut planes: [&[u8]; 3] = [&[]; 3];
        for plane in 0..3 {
            let (row, rows) = format.plane(plane, width, height);
            let line = usize::try_from(frame.linesize[plane]).unwrap_or(0);
            if frame.data[plane].is_null()
                || line < row
                || line - row > MAX_STRIDE_PADDING as usize
                || rows == 0
            {
                return Err(format!(
                    "plane {plane} with a row of {row} bytes has a line of {} bytes",
                    frame.linesize[plane]
                ));
            }
            // At most `MAX_DIMENSION * 2 + MAX_STRIDE_PADDING`.
            stride[plane] = u32::try_from(line).unwrap_or(u32::MAX);
            // SAFETY: a software decoder hands out planes of `linesize` bytes per row for every
            // row of the plane, top down (a positive line size, checked above), valid until the
            // frame is unreferenced, which needs `&mut self`. The slice ends with the last pixel
            // of the last row, inside that memory.
            planes[plane] =
                unsafe { std::slice::from_raw_parts(frame.data[plane], line * (rows - 1) + row) };
        }
        let code = |value: c_int| u8::try_from(value).unwrap_or(2);
        let pts = [frame.best_effort_timestamp, frame.pts]
            .into_iter()
            .find(|t| *t != AV_NOPTS_VALUE);
        Ok(Picture {
            head: FrameHead {
                serial: 0,
                consumed: 0,
                pts,
                width,
                height,
                format,
                stride,
                range: if full_range {
                    2
                } else {
                    code(frame.color_range)
                },
                primaries: code(frame.color_primaries),
                transfer: code(frame.color_trc),
                matrix: code(frame.colorspace),
                key: frame.flags & AV_FRAME_FLAG_KEY != 0,
                corrupt: frame.flags & AV_FRAME_FLAG_CORRUPT != 0,
            },
            planes,
        })
    }

    /// Gives the picture of the last receive back to the decoder.
    pub(crate) fn release(&mut self) {
        // SAFETY: the frame belongs to this `Video`; unref on a blank frame does nothing.
        unsafe { av_frame_unref(self.frame.as_ptr()) };
    }

    /// Drops everything the decoder holds: after a jump, and before packets follow an end.
    pub(crate) fn flush(&mut self) {
        self.release();
        // SAFETY: the context is open.
        unsafe { avcodec_flush_buffers(self.ctx.as_ptr()) };
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        drop(Parts {
            ctx: self.ctx.as_ptr(),
            packet: self.packet.as_ptr(),
            frame: self.frame.as_ptr(),
        });
    }
}

/// The pieces of a decoder, freed by the calls that made them.
struct Parts {
    ctx: *mut Opaque,
    packet: *mut AVPacket,
    frame: *mut AVFrame,
}

impl Drop for Parts {
    fn drop(&mut self) {
        // SAFETY: each pointer is null or the sole owner of what the matching allocation call
        // returned, and each of the three calls accepts a pointer to null.
        unsafe {
            av_frame_free(&raw mut self.frame);
            av_packet_free(&raw mut self.packet);
            avcodec_free_context(&raw mut self.ctx);
        }
    }
}
