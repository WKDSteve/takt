// SPDX-License-Identifier: GPL-3.0-or-later
//! libavformat behind a safe type: [`Input`] is one opened container, read through callbacks from
//! a file handle the helper inherited.
//!
//! The one module of the crate with `unsafe` (KONZEPT, unsafe policy), and it only exists in a
//! build with FFmpeg (`FFMPEG_DIR`, cfg `takt_ffmpeg`). It is a module of the helper program, not
//! of the library: a program that links `takt-demux` to use the demuxer links no FFmpeg and loads
//! none, which is the point of the process boundary (the test `the_client_links_no_ffmpeg` holds
//! that).
//!
//! # Why hand-written, and what that costs
//!
//! Looked at on 2026-11-17 for FFmpeg 9.0: `ffmpeg-sys-next` and `rusty_ffmpeg` (which `rsmpeg`
//! builds on) generate their bindings with bindgen, which needs libclang on every machine that
//! builds, the CI included, and pulls a build-time tree that is larger than this crate;
//! `ffmpeg-sys-next` is WTFPL, a license `deny.toml` does not allow. A demuxer calls about twenty
//! functions, so they are declared here by hand, as `apps/takt-player/src/mpv/ffi.rs` does for
//! libmpv.
//!
//! libavformat has no accessor functions for what a demuxer reads: the streams, their codec
//! parameters and the fields of a packet are structure members. So this module declares the
//! leading part of four structures, up to the last field it reads. That part is fixed for a major
//! version of the library (fields are only added at the end within one), so:
//!
//! * the build refuses headers of another major version (`build.rs`),
//! * [`Input::open`] refuses libraries of another major version at run time,
//! * the offsets below are asserted at compile time against the numbers a C compiler printed for
//!   the real headers (`layout/layout.c`, for Windows x64 and Linux x64), and
//! * the tests compare what comes out with `ffprobe` on the same files.
//!
//! A new major version of FFmpeg means: run `layout/layout.c` again, compare, raise the numbers.
#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::marker::{PhantomData, PhantomPinned};
use std::mem::offset_of;
use std::ptr::{self, NonNull};

use takt_core::{Rational, Timestamp};
use takt_ipc::ring::Heartbeat;

use takt_demux::proto::{
    AudioParams, MAX_DIMENSION, MAX_EXTRADATA, MAX_LANGUAGE, MAX_NAME, MAX_STREAMS, MAX_TITLE,
    MediaInfo, PacketHead, StreamInfo, StreamKind, VideoParams,
};

/// The major versions this module is written for (FFmpeg 9.0).
const AVFORMAT_MAJOR: c_uint = 63;
const AVCODEC_MAJOR: c_uint = 63;
const AVUTIL_MAJOR: c_uint = 61;

/// The demuxers the helper may use (`format_whitelist`): Matroska and WebM, and the ISO family
/// (MP4, MOV, M4A, 3GP). Every further container is a parser more that reads foreign bytes; it
/// joins this list on purpose, not by being compiled into the library.
const DEMUXERS: &CStr = c"matroska,webm,mov,mp4,m4a,3gp,3g2,mj2";

/// No protocol of libavformat has this name: with it as `protocol_whitelist` a container cannot
/// make the library open anything else (another file, a URL). The file itself does not come
/// through a protocol but through the callbacks below.
const NO_PROTOCOL: &CStr = c"takt-none";

/// Bytes libavformat reads per call of the read callback.
const IO_BUFFER: c_int = 64 * 1024;

/// Bytes the library may read without delivering a packet (while it opens the file, between two
/// packets, in a seek). A file made to keep a parser reading forever ends here.
const IO_BUDGET: u64 = 512 << 20;

const AV_NOPTS_VALUE: i64 = i64::MIN;
const AVERROR_EOF: c_int = -541_478_725;
/// `AVERROR(EAGAIN)`; `EAGAIN` is 11 for the C libraries of Windows and Linux alike.
const AVERROR_EAGAIN: c_int = -11;
const AVERROR_EIO: c_int = -5;
const AVERROR_EINVAL: c_int = -22;
const AV_LOG_QUIET: c_int = -8;
const AVSEEK_SIZE: c_int = 0x1_0000;
const AVSEEK_FORCE: c_int = 0x2_0000;
const AVSEEK_FLAG_BACKWARD: c_int = 1;
const AV_DICT_IGNORE_SUFFIX: c_int = 2;
const AV_PKT_FLAG_KEY: c_int = 1;
const AV_PKT_FLAG_CORRUPT: c_int = 2;
const AV_PKT_FLAG_DISCARD: c_int = 4;
const AVMEDIA_TYPE_VIDEO: c_int = 0;
const AVMEDIA_TYPE_AUDIO: c_int = 1;
const AVMEDIA_TYPE_DATA: c_int = 2;
const AVMEDIA_TYPE_SUBTITLE: c_int = 3;
const AVMEDIA_TYPE_ATTACHMENT: c_int = 4;
const AV_CHANNEL_ORDER_NATIVE: c_int = 1;

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

/// The head of `AVFormatContext` (libavformat 63), up to `duration`.
#[repr(C)]
struct AVFormatContext {
    av_class: *const c_void,
    iformat: *const AVInputFormat,
    oformat: *const c_void,
    priv_data: *mut c_void,
    pb: *mut AVIOContext,
    ctx_flags: c_int,
    nb_streams: c_uint,
    streams: *mut *mut AVStream,
    nb_stream_groups: c_uint,
    stream_groups: *mut *mut c_void,
    nb_chapters: c_uint,
    chapters: *mut *mut c_void,
    url: *mut c_char,
    start_time: i64,
    duration: i64,
}

/// The head of `AVInputFormat`: its name.
#[repr(C)]
struct AVInputFormat {
    name: *const c_char,
}

/// The head of `AVIOContext`: the buffer, which the library may have replaced by a larger one.
#[repr(C)]
struct AVIOContext {
    av_class: *const c_void,
    buffer: *mut u8,
}

/// The head of `AVStream` (libavformat 63), up to `avg_frame_rate`.
#[repr(C)]
struct AVStream {
    av_class: *const c_void,
    index: c_int,
    id: c_int,
    codecpar: *mut AVCodecParameters,
    priv_data: *mut c_void,
    time_base: AVRational,
    start_time: i64,
    duration: i64,
    nb_frames: i64,
    disposition: c_int,
    discard: c_int,
    sample_aspect_ratio: AVRational,
    metadata: *mut Opaque,
    avg_frame_rate: AVRational,
}

/// `AVChannelLayout` (libavutil 61).
#[repr(C)]
struct AVChannelLayout {
    order: c_int,
    nb_channels: c_int,
    /// The union `u`: the mask of the native order, or a pointer to a custom map.
    mask: u64,
    opaque: *mut c_void,
}

/// The head of `AVCodecParameters` (libavcodec 63), up to `seek_preroll`.
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
    video_delay: c_int,
    ch_layout: AVChannelLayout,
    sample_rate: c_int,
    block_align: c_int,
    frame_size: c_int,
    initial_padding: c_int,
    trailing_padding: c_int,
    seek_preroll: c_int,
}

/// The head of `AVPacket` (libavcodec 63), up to `pos`.
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
    pos: i64,
}

/// `AVDictionaryEntry`.
#[repr(C)]
struct AVDictionaryEntry {
    key: *mut c_char,
    value: *mut c_char,
}

// The offsets `layout/layout.c` printed for FFmpeg 9.0 (libavformat 63.1, libavcodec 63.1,
// libavutil 61.1), identical for x86_64-pc-windows-msvc and x86_64-unknown-linux-gnu. A structure
// above that drifts from the header fails the build here.
#[cfg(all(target_pointer_width = "64", any(windows, unix)))]
const _: () = {
    assert!(offset_of!(AVFormatContext, iformat) == 8);
    assert!(offset_of!(AVFormatContext, pb) == 32);
    assert!(offset_of!(AVFormatContext, nb_streams) == 44);
    assert!(offset_of!(AVFormatContext, streams) == 48);
    assert!(offset_of!(AVFormatContext, start_time) == 96);
    assert!(offset_of!(AVFormatContext, duration) == 104);
    assert!(offset_of!(AVIOContext, buffer) == 8);
    assert!(offset_of!(AVStream, index) == 8);
    assert!(offset_of!(AVStream, codecpar) == 16);
    assert!(offset_of!(AVStream, time_base) == 32);
    assert!(offset_of!(AVStream, start_time) == 40);
    assert!(offset_of!(AVStream, duration) == 48);
    assert!(offset_of!(AVStream, nb_frames) == 56);
    assert!(offset_of!(AVStream, disposition) == 64);
    assert!(offset_of!(AVStream, sample_aspect_ratio) == 72);
    assert!(offset_of!(AVStream, metadata) == 80);
    assert!(offset_of!(AVStream, avg_frame_rate) == 88);
    assert!(offset_of!(AVCodecParameters, codec_id) == 4);
    assert!(offset_of!(AVCodecParameters, codec_tag) == 8);
    assert!(offset_of!(AVCodecParameters, extradata) == 16);
    assert!(offset_of!(AVCodecParameters, extradata_size) == 24);
    assert!(offset_of!(AVCodecParameters, format) == 44);
    assert!(offset_of!(AVCodecParameters, bit_rate) == 48);
    assert!(offset_of!(AVCodecParameters, bits_per_coded_sample) == 56);
    assert!(offset_of!(AVCodecParameters, profile) == 64);
    assert!(offset_of!(AVCodecParameters, level) == 68);
    assert!(offset_of!(AVCodecParameters, width) == 72);
    assert!(offset_of!(AVCodecParameters, height) == 76);
    assert!(offset_of!(AVCodecParameters, sample_aspect_ratio) == 80);
    assert!(offset_of!(AVCodecParameters, framerate) == 88);
    assert!(offset_of!(AVCodecParameters, field_order) == 96);
    assert!(offset_of!(AVCodecParameters, color_range) == 100);
    assert!(offset_of!(AVCodecParameters, color_primaries) == 104);
    assert!(offset_of!(AVCodecParameters, color_trc) == 108);
    assert!(offset_of!(AVCodecParameters, color_space) == 112);
    assert!(offset_of!(AVCodecParameters, chroma_location) == 116);
    assert!(offset_of!(AVCodecParameters, video_delay) == 120);
    assert!(offset_of!(AVCodecParameters, ch_layout) == 128);
    assert!(offset_of!(AVCodecParameters, sample_rate) == 152);
    assert!(offset_of!(AVCodecParameters, block_align) == 156);
    assert!(offset_of!(AVCodecParameters, frame_size) == 160);
    assert!(offset_of!(AVCodecParameters, initial_padding) == 164);
    assert!(offset_of!(AVCodecParameters, trailing_padding) == 168);
    assert!(offset_of!(AVCodecParameters, seek_preroll) == 172);
    assert!(offset_of!(AVChannelLayout, nb_channels) == 4);
    assert!(offset_of!(AVChannelLayout, mask) == 8);
    assert!(offset_of!(AVPacket, pts) == 8);
    assert!(offset_of!(AVPacket, dts) == 16);
    assert!(offset_of!(AVPacket, data) == 24);
    assert!(offset_of!(AVPacket, size) == 32);
    assert!(offset_of!(AVPacket, stream_index) == 36);
    assert!(offset_of!(AVPacket, flags) == 40);
    assert!(offset_of!(AVPacket, duration) == 64);
    assert!(offset_of!(AVPacket, pos) == 72);
};

type ReadFn = unsafe extern "C" fn(opaque: *mut c_void, buf: *mut u8, size: c_int) -> c_int;
type WriteFn = unsafe extern "C" fn(opaque: *mut c_void, buf: *const u8, size: c_int) -> c_int;
type SeekFn = unsafe extern "C" fn(opaque: *mut c_void, offset: i64, whence: c_int) -> i64;

#[link(name = "avformat")]
unsafe extern "C" {
    /// `(major << 16) | (minor << 8) | micro` of the library that was loaded.
    safe fn avformat_version() -> c_uint;
    /// A static, NUL-terminated text.
    safe fn avformat_license() -> *const c_char;
    /// A new context, or null without memory.
    safe fn avformat_alloc_context() -> *mut AVFormatContext;
    fn avformat_open_input(
        ps: *mut *mut AVFormatContext,
        url: *const c_char,
        fmt: *const c_void,
        options: *mut *mut Opaque,
    ) -> c_int;
    fn avformat_find_stream_info(ic: *mut AVFormatContext, options: *mut *mut Opaque) -> c_int;
    fn av_read_frame(s: *mut AVFormatContext, pkt: *mut AVPacket) -> c_int;
    fn av_seek_frame(
        s: *mut AVFormatContext,
        stream_index: c_int,
        timestamp: i64,
        flags: c_int,
    ) -> c_int;
    fn avformat_close_input(s: *mut *mut AVFormatContext);
    fn avio_alloc_context(
        buffer: *mut u8,
        buffer_size: c_int,
        write_flag: c_int,
        opaque: *mut c_void,
        read_packet: Option<ReadFn>,
        write_packet: Option<WriteFn>,
        seek: Option<SeekFn>,
    ) -> *mut AVIOContext;
    fn avio_context_free(s: *mut *mut AVIOContext);
}

#[link(name = "avcodec")]
unsafe extern "C" {
    safe fn avcodec_version() -> c_uint;
    /// The name of a codec id; never null ("unknown_codec" for a number it does not know).
    safe fn avcodec_get_name(id: c_int) -> *const c_char;
    /// A new, blank packet, or null without memory.
    safe fn av_packet_alloc() -> *mut AVPacket;
    fn av_packet_free(pkt: *mut *mut AVPacket);
    fn av_packet_unref(pkt: *mut AVPacket);
}

#[link(name = "avutil")]
unsafe extern "C" {
    safe fn avutil_version() -> c_uint;
    safe fn av_log_set_level(level: c_int);
    /// Memory for the library, or null.
    safe fn av_malloc(size: usize) -> *mut c_void;
    fn av_free(ptr: *mut c_void);
    fn av_dict_set(
        pm: *mut *mut Opaque,
        key: *const c_char,
        value: *const c_char,
        flags: c_int,
    ) -> c_int;
    fn av_dict_get(
        m: *const Opaque,
        key: *const c_char,
        prev: *const AVDictionaryEntry,
        flags: c_int,
    ) -> *mut AVDictionaryEntry;
    fn av_dict_free(m: *mut *mut Opaque);
    fn av_strerror(errnum: c_int, errbuf: *mut c_char, errbuf_size: usize) -> c_int;
    /// The name of a pixel format, or null for a number it does not know.
    safe fn av_get_pix_fmt_name(pix_fmt: c_int) -> *const c_char;
    /// The name of a sample format, or null for a number it does not know.
    safe fn av_get_sample_fmt_name(sample_fmt: c_int) -> *const c_char;
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

/// A static name of the library as a string; empty for null and for anything that is not plain
/// ASCII without spaces.
fn name_of(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: the pointers passed in come from functions that return static, NUL-terminated
    // strings of the library (or null, handled above).
    let text = unsafe { CStr::from_ptr(ptr) }.to_string_lossy();
    if text.len() <= MAX_NAME && text.bytes().all(|b| b.is_ascii_graphic()) {
        text.into_owned()
    } else {
        String::new()
    }
}

/// The libraries the helper runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Libraries {
    /// `(major, minor, micro)` of libavformat, libavcodec and libavutil.
    pub versions: [(u32, u32, u32); 3],
    /// What libavformat says about its license ("LGPL version 3 or later").
    pub license: String,
}

/// The versions and the license of the loaded libraries.
pub(crate) fn libraries() -> Libraries {
    let split = |v: c_uint| (v >> 16, (v >> 8) & 0xFF, v & 0xFF);
    // SAFETY: avformat_license returns a static NUL-terminated string that lives for the whole
    // process and is never written to.
    let license = unsafe { CStr::from_ptr(avformat_license()) }
        .to_string_lossy()
        .into_owned();
    Libraries {
        versions: [
            split(avformat_version()),
            split(avcodec_version()),
            split(avutil_version()),
        ],
        license,
    }
}

/// Whether the loaded libraries are the major versions the structures above describe.
pub(crate) fn libraries_match() -> Result<(), String> {
    let got = libraries().versions;
    let want = [AVFORMAT_MAJOR, AVCODEC_MAJOR, AVUTIL_MAJOR];
    if got.iter().map(|v| v.0).eq(want) {
        Ok(())
    } else {
        Err(format!(
            "libavformat {}, libavcodec {}, libavutil {} loaded; built for {}, {}, {}",
            got[0].0, got[1].0, got[2].0, want[0], want[1], want[2]
        ))
    }
}

/// What the I/O callbacks work on.
struct Source {
    file: File,
    beat: Heartbeat,
    /// Bytes read since the last delivered packet.
    spent: u64,
}

/// The read callback of the `AVIOContext`.
///
/// # Safety
/// `opaque` must be the `Source` the context was made with, not used by anyone else during the
/// call, and `buf` must be valid for `size` bytes. libavformat guarantees both for the context it
/// was given.
unsafe extern "C" fn read_cb(opaque: *mut c_void, buf: *mut u8, size: c_int) -> c_int {
    let Ok(len) = usize::try_from(size) else {
        return AVERROR_EINVAL;
    };
    if opaque.is_null() || buf.is_null() || len == 0 {
        return AVERROR_EINVAL;
    }
    // SAFETY: see the contract above: the library passes the opaque pointer of its context, which
    // is the boxed `Source` of the `Input` that owns the context, and calls its callbacks from the
    // one thread that is inside a call of that `Input`.
    let source = unsafe { &mut *opaque.cast::<Source>() };
    // Every read is a sign of life; a parser that spins without reading shows none.
    source.beat.beat();
    if source.spent >= IO_BUDGET {
        return AVERROR_EIO;
    }
    // SAFETY: the library hands out a buffer of `size` bytes it owns and does not touch during
    // the call.
    let out = unsafe { std::slice::from_raw_parts_mut(buf, len) };
    loop {
        match source.file.read(out) {
            Ok(0) => return AVERROR_EOF,
            Ok(n) => {
                source.spent = source.spent.saturating_add(n as u64);
                // `n` is at most `size`, a c_int.
                return c_int::try_from(n).unwrap_or(AVERROR_EIO);
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return AVERROR_EIO,
        }
    }
}

/// The seek callback of the `AVIOContext`.
///
/// # Safety
/// As [`read_cb`]: `opaque` is the `Source` of the context.
unsafe extern "C" fn seek_cb(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    if opaque.is_null() {
        return i64::from(AVERROR_EINVAL);
    }
    // SAFETY: as in `read_cb`.
    let source = unsafe { &mut *opaque.cast::<Source>() };
    source.beat.beat();
    let whence = whence & !AVSEEK_FORCE;
    if whence & AVSEEK_SIZE != 0 {
        return source
            .file
            .metadata()
            .ok()
            .and_then(|m| i64::try_from(m.len()).ok())
            .unwrap_or_else(|| i64::from(AVERROR_EIO));
    }
    let target = match whence {
        0 => match u64::try_from(offset) {
            Ok(offset) => SeekFrom::Start(offset),
            Err(_) => return i64::from(AVERROR_EINVAL),
        },
        1 => SeekFrom::Current(offset),
        2 => SeekFrom::End(offset),
        _ => return i64::from(AVERROR_EINVAL),
    };
    source
        .file
        .seek(target)
        .ok()
        .and_then(|pos| i64::try_from(pos).ok())
        .unwrap_or_else(|| i64::from(AVERROR_EINVAL))
}

/// One opened container.
///
/// Not `Send`: the library's context stays with the thread that opened it.
pub(crate) struct Input {
    ctx: NonNull<AVFormatContext>,
    io: NonNull<AVIOContext>,
    source: NonNull<Source>,
    packet: NonNull<AVPacket>,
    /// The streams the table of [`Input::info`] named; packets of later ones are skipped.
    streams: u32,
}

impl Input {
    /// Opens `file` as a container and reads what its streams are.
    ///
    /// # Errors
    /// The library's error number and text; 0 with a text for a failure outside the library.
    pub(crate) fn open(file: File, beat: Heartbeat) -> Result<Self, AvError> {
        libraries_match().map_err(|why| (0, why))?;
        av_log_set_level(AV_LOG_QUIET);
        let no_memory = || (0, "out of memory".to_owned());

        let source = NonNull::from(Box::leak(Box::new(Source {
            file,
            beat,
            spent: 0,
        })));
        // From here on `Parts` frees what was made so far, on every return.
        let mut parts = Parts {
            ctx: ptr::null_mut(),
            io: ptr::null_mut(),
            source: Some(source),
            packet: ptr::null_mut(),
        };
        let buffer = av_malloc(IO_BUFFER as usize).cast::<u8>();
        if buffer.is_null() {
            return Err(no_memory());
        }
        // SAFETY: `buffer` is IO_BUFFER bytes from av_malloc, as the call demands, and is owned by
        // the context from here on; `source` is a live box that outlives the context (both are
        // freed together, the context first); the callbacks match the signatures of the header.
        parts.io = unsafe {
            avio_alloc_context(
                buffer,
                IO_BUFFER,
                0,
                source.as_ptr().cast(),
                Some(read_cb),
                None,
                Some(seek_cb),
            )
        };
        if parts.io.is_null() {
            // SAFETY: the context was not made, so the buffer is still ours to free.
            unsafe { av_free(buffer.cast()) };
            return Err(no_memory());
        }
        parts.ctx = avformat_alloc_context();
        if parts.ctx.is_null() {
            return Err(no_memory());
        }
        parts.packet = av_packet_alloc();
        if parts.packet.is_null() {
            return Err(no_memory());
        }

        let mut options: *mut Opaque = ptr::null_mut();
        // SAFETY: `parts.ctx` is a live context nobody else has; storing the I/O context before
        // the open is how the header says custom I/O is set up. The dictionary calls get a pointer
        // to a local that starts as null (an empty dictionary) and NUL-terminated literals. The
        // open takes the context pointer by reference and sets it to null when it fails (it frees
        // the context then); the dictionary is freed on every path.
        let opened = unsafe {
            (*parts.ctx).pb = parts.io;
            let mut rc = av_dict_set(
                &raw mut options,
                c"format_whitelist".as_ptr(),
                DEMUXERS.as_ptr(),
                0,
            );
            if rc >= 0 {
                rc = av_dict_set(
                    &raw mut options,
                    c"protocol_whitelist".as_ptr(),
                    NO_PROTOCOL.as_ptr(),
                    0,
                );
            }
            if rc >= 0 {
                rc = avformat_open_input(
                    &raw mut parts.ctx,
                    ptr::null(),
                    ptr::null(),
                    &raw mut options,
                );
            }
            // An option the library did not take is left in the dictionary: then a whitelist is
            // not in force, and the file is not read.
            let ignored =
                !av_dict_get(options, c"".as_ptr(), ptr::null(), AV_DICT_IGNORE_SUFFIX).is_null();
            av_dict_free(&raw mut options);
            if rc >= 0 && ignored {
                Err((0, "libavformat did not accept the whitelists".to_owned()))
            } else if rc < 0 {
                Err(av_error(rc))
            } else {
                Ok(())
            }
        };
        opened?;
        // SAFETY: the context is open; no options are passed.
        let rc = unsafe { avformat_find_stream_info(parts.ctx, ptr::null_mut()) };
        if rc < 0 {
            return Err(av_error(rc));
        }
        // SAFETY: the context is open and stays so while `parts` is alive.
        let streams = unsafe { (*parts.ctx).nb_streams };
        let (Some(ctx), Some(io), Some(packet)) = (
            NonNull::new(parts.ctx),
            NonNull::new(parts.io),
            NonNull::new(parts.packet),
        ) else {
            return Err(no_memory());
        };
        // Ownership moves to the `Input`; `parts` must not free anything any more.
        parts.ctx = ptr::null_mut();
        parts.io = ptr::null_mut();
        parts.packet = ptr::null_mut();
        parts.source = None;
        let mut input = Self {
            ctx,
            io,
            source,
            packet,
            streams,
        };
        input.reset_budget();
        Ok(input)
    }

    fn reset_budget(&mut self) {
        // SAFETY: the source is a live box this `Input` owns; no call of the library is running
        // (this is `&mut self`), so no callback uses it.
        unsafe { (*self.source.as_ptr()).spent = 0 };
    }

    /// The format and the streams of the file.
    ///
    /// # Errors
    /// A text when the file has more streams than the protocol carries or a stream without a
    /// usable time base.
    pub(crate) fn info(&self) -> Result<MediaInfo, String> {
        // SAFETY: the context is open for as long as `self` lives, and nothing writes it during
        // this call (the library only does inside its own calls, which take `&mut self`).
        let ctx = unsafe { self.ctx.as_ref() };
        let count = ctx.nb_streams as usize;
        if count > MAX_STREAMS {
            return Err(format!("{count} streams, the limit is {MAX_STREAMS}"));
        }
        let mut streams = Vec::with_capacity(count);
        for position in 0..count {
            // SAFETY: `streams` points to `nb_streams` pointers to streams the context owns, each
            // with its codec parameters; `position` is below that count.
            let (stream, par) = unsafe {
                let stream = *ctx.streams.add(position);
                if stream.is_null() || (*stream).codecpar.is_null() {
                    return Err(format!("stream {position} is missing"));
                }
                (&*stream, &*(*stream).codecpar)
            };
            streams.push(stream_info(position, stream, par)?);
        }
        let format = if ctx.iformat.is_null() {
            String::new()
        } else {
            // SAFETY: `iformat` points to the static description of the demuxer that was picked.
            name_of(unsafe { (*ctx.iformat).name })
        };
        let micros =
            |value: i64| (value != AV_NOPTS_VALUE).then(|| Timestamp::new(value, Rational::MICROS));
        Ok(MediaInfo {
            format,
            duration: micros(ctx.duration).filter(|t| t.ticks() >= 0),
            start: micros(ctx.start_time),
            streams,
        })
    }

    /// Reads the next packet. `Ok(None)` at the end of the file. The packet stays in this input
    /// until [`release`](Self::release); [`data`](Self::data) is its content.
    ///
    /// # Errors
    /// The library's error number and text.
    pub(crate) fn read(&mut self) -> Result<Option<PacketHead>, AvError> {
        loop {
            self.release();
            // SAFETY: the context is open and the packet is blank (released above); both belong to
            // this `Input`, and `&mut self` keeps every other use away during the call.
            let rc = unsafe { av_read_frame(self.ctx.as_ptr(), self.packet.as_ptr()) };
            if rc == AVERROR_EOF {
                return Ok(None);
            }
            if rc == AVERROR_EAGAIN {
                continue;
            }
            if rc < 0 {
                return Err(av_error(rc));
            }
            self.reset_budget();
            // SAFETY: the call above filled the packet; it is valid until the next release.
            let packet = unsafe { self.packet.as_ref() };
            let Some(stream) = u32::try_from(packet.stream_index)
                .ok()
                .filter(|index| *index < self.streams)
            else {
                // A stream that appeared after the table was sent: the parent does not know it.
                continue;
            };
            let time = |value: i64| (value != AV_NOPTS_VALUE).then_some(value);
            return Ok(Some(PacketHead {
                serial: 0,
                stream,
                pts: time(packet.pts),
                dts: time(packet.dts),
                duration: Some(packet.duration).filter(|d| *d > 0),
                position: Some(packet.pos).filter(|p| *p >= 0),
                key: packet.flags & AV_PKT_FLAG_KEY != 0,
                corrupt: packet.flags & AV_PKT_FLAG_CORRUPT != 0,
                discard: packet.flags & AV_PKT_FLAG_DISCARD != 0,
            }));
        }
    }

    /// The content of the packet the last [`read`](Self::read) returned; empty after a release.
    pub(crate) fn data(&self) -> &[u8] {
        // SAFETY: the packet belongs to this `Input`; after a read its `data` points to `size`
        // bytes that stay valid until the packet is released, which needs `&mut self`.
        unsafe {
            let packet = self.packet.as_ref();
            match usize::try_from(packet.size) {
                Ok(len) if len > 0 && !packet.data.is_null() => {
                    std::slice::from_raw_parts(packet.data, len)
                }
                _ => &[],
            }
        }
    }

    /// Gives the packet of the last read back to the library.
    pub(crate) fn release(&mut self) {
        // SAFETY: the packet belongs to this `Input`; unref on a blank packet does nothing.
        unsafe { av_packet_unref(self.packet.as_ptr()) };
    }

    /// Positions the file at the last keyframe at or before `micros` (microseconds on the file's
    /// timeline).
    ///
    /// # Errors
    /// The library's error number and text; the position is then undefined by the library, in
    /// practice unchanged.
    pub(crate) fn seek(&mut self, micros: i64) -> Result<(), AvError> {
        self.release();
        self.reset_budget();
        // SAFETY: the context is open; stream -1 takes the time in AV_TIME_BASE units.
        let rc = unsafe { av_seek_frame(self.ctx.as_ptr(), -1, micros, AVSEEK_FLAG_BACKWARD) };
        self.reset_budget();
        if rc < 0 {
            return Err(av_error(rc));
        }
        Ok(())
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        drop(Parts {
            ctx: self.ctx.as_ptr(),
            io: self.io.as_ptr(),
            source: Some(self.source),
            packet: self.packet.as_ptr(),
        });
    }
}

/// The pieces of an input, freed in the order the library needs.
struct Parts {
    ctx: *mut AVFormatContext,
    io: *mut AVIOContext,
    source: Option<NonNull<Source>>,
    packet: *mut AVPacket,
}

impl Drop for Parts {
    fn drop(&mut self) {
        // SAFETY: each pointer is null or the sole owner of what the matching allocation call
        // returned. The packet and the format context are freed by their own calls (both accept a
        // pointer to null). With custom I/O the library leaves the I/O context to its owner: its
        // buffer (which the library may have replaced) is freed first, then the context, and the
        // source last, when no callback can run any more.
        unsafe {
            av_packet_free(&raw mut self.packet);
            avformat_close_input(&raw mut self.ctx);
            if !self.io.is_null() {
                av_free((*self.io).buffer.cast());
                (*self.io).buffer = ptr::null_mut();
                avio_context_free(&raw mut self.io);
            }
            if let Some(source) = self.source.take() {
                drop(Box::from_raw(source.as_ptr()));
            }
        }
    }
}

fn unsigned(value: c_int) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

fn fraction(value: AVRational) -> Option<Rational> {
    (value.num > 0 && value.den > 0)
        .then(|| Rational::new(i64::from(value.num), i64::from(value.den)))
        .flatten()
}

/// A value of the stream's metadata, cut to `max` bytes.
fn metadata(stream: &AVStream, key: &CStr, max: usize) -> String {
    // SAFETY: `metadata` is null or a dictionary the stream owns; the key is NUL-terminated; a
    // found entry and its value live as long as the dictionary, and the value is copied here.
    let value = unsafe {
        let entry = av_dict_get(stream.metadata, key.as_ptr(), ptr::null(), 0);
        if entry.is_null() || (*entry).value.is_null() {
            return String::new();
        }
        CStr::from_ptr((*entry).value).to_string_lossy()
    };
    takt_ipc::tame(&value, max)
}

fn stream_info(
    position: usize,
    stream: &AVStream,
    par: &AVCodecParameters,
) -> Result<StreamInfo, String> {
    let index = u32::try_from(position).map_err(|_| "stream index".to_owned())?;
    let time_base = fraction(stream.time_base)
        .ok_or_else(|| format!("stream {position} has no usable time base"))?;
    let mut kind = match par.codec_type {
        AVMEDIA_TYPE_VIDEO => StreamKind::Video,
        AVMEDIA_TYPE_AUDIO => StreamKind::Audio,
        AVMEDIA_TYPE_DATA => StreamKind::Data,
        AVMEDIA_TYPE_SUBTITLE => StreamKind::Subtitle,
        AVMEDIA_TYPE_ATTACHMENT => StreamKind::Attachment,
        _ => StreamKind::Other,
    };
    let (width, height) = (unsigned(par.width), unsigned(par.height));
    let (sample_rate, channels) = (
        unsigned(par.sample_rate),
        unsigned(par.ch_layout.nb_channels),
    );
    // A picture without a size or a sound without a rate is nothing a decoder can be set up for:
    // the stream stays in the table (the indices of the others must not move) under no name.
    let sized = (1..=MAX_DIMENSION).contains(&width) && (1..=MAX_DIMENSION).contains(&height);
    if (kind == StreamKind::Video && !sized)
        || (kind == StreamKind::Audio && (sample_rate == 0 || channels == 0))
    {
        kind = StreamKind::Other;
    }
    let codec = Some(name_of(avcodec_get_name(par.codec_id)))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    let extradata = match usize::try_from(par.extradata_size) {
        Ok(len) if len > 0 && len <= MAX_EXTRADATA && !par.extradata.is_null() => {
            // SAFETY: `extradata` points to `extradata_size` bytes the codec parameters own.
            unsafe { std::slice::from_raw_parts(par.extradata, len) }.to_vec()
        }
        _ => Vec::new(),
    };
    let time = |value: i64| (value != AV_NOPTS_VALUE).then(|| Timestamp::new(value, time_base));
    let video = (kind == StreamKind::Video).then(|| VideoParams {
        width,
        height,
        pixel_format: name_of(av_get_pix_fmt_name(par.format)),
        sample_aspect: fraction(stream.sample_aspect_ratio)
            .or_else(|| fraction(par.sample_aspect_ratio)),
        frame_rate: fraction(stream.avg_frame_rate).or_else(|| fraction(par.framerate)),
        color_range: unsigned(par.color_range),
        color_primaries: unsigned(par.color_primaries),
        color_transfer: unsigned(par.color_trc),
        color_matrix: unsigned(par.color_space),
        chroma_location: unsigned(par.chroma_location),
        field_order: unsigned(par.field_order),
        delay: unsigned(par.video_delay),
    });
    let audio = (kind == StreamKind::Audio).then(|| AudioParams {
        sample_rate,
        channels,
        channel_mask: if par.ch_layout.order == AV_CHANNEL_ORDER_NATIVE {
            par.ch_layout.mask
        } else {
            0
        },
        sample_format: name_of(av_get_sample_fmt_name(par.format)),
        frame_size: unsigned(par.frame_size),
        initial_padding: unsigned(par.initial_padding),
        trailing_padding: unsigned(par.trailing_padding),
        seek_preroll: unsigned(par.seek_preroll),
        bits_per_coded_sample: unsigned(par.bits_per_coded_sample),
        block_align: unsigned(par.block_align),
    });
    Ok(StreamInfo {
        index,
        kind,
        codec,
        codec_tag: par.codec_tag,
        time_base,
        start: time(stream.start_time),
        duration: time(stream.duration).filter(|t| t.ticks() >= 0),
        frames: u64::try_from(stream.nb_frames).ok().filter(|n| *n > 0),
        disposition: stream.disposition.cast_unsigned(),
        profile: par.profile,
        level: par.level,
        bit_rate: u64::try_from(par.bit_rate).ok().filter(|n| *n > 0),
        extradata,
        language: metadata(stream, c"language", MAX_LANGUAGE),
        title: metadata(stream, c"title", MAX_TITLE),
        video,
        audio,
    })
}
