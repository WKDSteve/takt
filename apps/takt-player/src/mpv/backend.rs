// SPDX-License-Identifier: GPL-3.0-or-later
//! libmpv behind [`VideoBackend`]: mpv opens, decodes, plays the sound and
//! renders each frame into memory; the player gets finished `rgb0` pictures
//! and a clock that follows the sound of mpv.
//!
//! # Who decides what
//!
//! * **When a frame is due: mpv.** The render API says "a frame waits" at the
//!   time of that frame on the clock of mpv, so [`VideoBackend::next_frame`]
//!   ignores the clock it is given. What the player contributes is the
//!   display period ([`VideoBackend::set_display_period`]): mpv hands a frame
//!   out half a period early (`video-timing-offset`), so it goes to the
//!   refresh nearest its time, the rule of `NullBackend`. Measured in
//!   `docs/reviews/phase-0.md`.
//! * **The clock: the sound of mpv.** mpv plays the sound on its own output
//!   device. [`MpvBackend::clock`] is an [`AudioMasterClock`] whose anchor is
//!   the audio position of mpv (`audio-pts`), published once per pass and
//!   carried on by the monotonic clock in between; its "samples" are
//!   microseconds. Without a sound track the position of the video stands in,
//!   and the presentation error then says nothing.
//! * **The picture: `rgb0` as mpv made it.** The frame is rendered at the
//!   display size of the video, subtitles drawn in, straight into a pooled
//!   [`PixelFormat::Rgb0`] frame (its plane starts aligned and has an
//!   aligned stride, which is what mpv asks for). There is no surface in
//!   between and no copy, and it is never converted to NV12.
//! * **The colour: mpv converts, the frames are BT.709.** The software
//!   renderer of mpv does not only apply the matrix of the file: it brings
//!   primaries and transfer to BT.709 with the gamma of BT.1886 (through
//!   zimg, which libmpv is built with). So every frame is BT.709, full
//!   range, whatever the file is, and its [`FrameMeta`] says that. What the
//!   file names is told apart from it ([`MpvBackend::source_color`],
//!   `MediaInfo::source_color`). HDR is not tone-mapped on this path: what
//!   is brighter than SDR white is cut off at white, in 8 bit. The tests
//!   `the_frames_are_bt709_whatever_primaries_the_file_names` and
//!   `hdr_is_cut_off_at_white_not_tone_mapped` measure both with files that
//!   differ in their tags alone; they fail on a libmpv that converts
//!   otherwise (one built without zimg applies the matrix only).
//!
//! # Two threads: one renders, one steers
//!
//! The render API of mpv wants a thread of its own that never waits for the
//! core (`render.h`, "Threading"): while the core waits for a frame to be
//! rendered (after a jump, a load, a change of the subtitles), a question to
//! the core from the thread that should render blocks both, until mpv gives
//! up after 200 ms.
//!
//! So the frames are rendered by a thread of this backend (`render_loop`).
//! mpv wakes it when a frame waits (its update callback), it renders that
//! frame at once into a pooled frame and puts it into a slot;
//! [`VideoBackend::next_frame`] takes the newest one from there. The thread
//! that calls the backend (the window) steers: every command, every
//! property, the events. It may wait for the core, because the core no
//! longer waits for it.
//!
//! The render thread asks the core two things all the same, because the
//! render API names neither: the position, which is the time of the frame
//! (measured: in the run it is that of the very frame, test
//! `a_frame_carries_its_own_time_except_on_the_way_to_a_jump`), and the size
//! of the picture. It asks both **after** it has rendered, never before:
//! with a frame waiting, the question blocked for 200 ms at every jump
//! (measured). So a frame is rendered at the size the thread knows; when
//! the size it reads afterwards is another one (the first frame of a file,
//! a change within one), it draws the same picture once more at that size.
//! A question from there blocks only when the next frame starts to wait in
//! the moment between; then it costs the 200 ms once.
//!
//! Commands that answer later (`mpv_command_async`) are not used: reading
//! subtitle files that way froze the core of mpv 0.41 for good while one
//! thread rendered and asked. `sub-add` waits for mpv (a local file:
//! milliseconds).
//!
//! # What may be opened
//!
//! [`Sources::Files`] takes a file that exists and nothing else: a text that
//! names no file never reaches mpv, so no protocol of mpv (`http://`,
//! `av://`, `edl://`, `memory://`) can be reached through the playlist or a
//! drop. The same rule holds for what mpv opens on its own afterwards: a
//! playlist file (`.m3u`, or any file whose content is one) hands its
//! entries to mpv, which calls `http://` entries safe. The hook `on_load`
//! stops mpv before every source it opens, and one that is no file is
//! replaced by a name nothing opens. A playlist file itself is refused, and
//! mpv is told to stop. What a media file may pull in is cut down as well
//! (no ordered chapters, no references to other files, no scripts, no
//! configuration, no `ytdl`).
//!
//! A subtitle file next to the film is opened by mpv on its own, and the
//! hook does not see it. Its formatting tags are allowed (libass reads
//! them); what it must not do is lead mpv to another file or into the
//! network. `access-references=no` is what holds that: with the default of
//! mpv a `.srt` whose content is an EDL shows the text of the file it names
//! and connects to the address it names (the test
//! `a_subtitle_file_leads_mpv_to_no_other_file_and_not_into_the_network`
//! has both controls).
//!
//! Limiting the protocols of FFmpeg (`stream-lavf-o=protocol_whitelist`)
//! does not do this: with it, mpv still connected (the test
//! `a_core_for_files_fetches_nothing_from_the_network` has the control).

// The properties of mpv are seconds as floating point; they become whole
// microseconds where they are read ([`super::micros`]) and are integers from
// there on.
#![allow(clippy::cast_precision_loss)]

use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use takt_audio::{DurationHistogram, DurationSnapshot};
use takt_backend::{BackendStats, MediaInfo, SeekMode, VideoBackend};
use takt_clock::{AudioClockWriter, AudioMasterClock, Clock, MonotonicSource};
use takt_core::{
    ColorDescription, ColorPrimaries, ColorRange, Error, FrameMeta, FramePool, PixelFormat,
    PooledFrame, Rational, Timestamp, TransferFunction,
};

use super::ffi::{self, Event, Mpv, Renderer, Target, Waiting, Waker};
use super::micros;

/// The "sample rate" of the clock: its anchor counts microseconds.
const CLOCK_RATE: u32 = 1_000_000;
/// How long the clock runs on without a new anchor: six passes at 60 Hz.
/// Longer than that and the player stopped asking, so the position stands.
const MAX_EXTRAPOLATION_NANOS: u64 = 100_000_000;
/// Frames of the pool. The renderer holds the frame it shows (the frame
/// itself, not a copy) until the next one replaces it, and mpv renders into
/// a second one meanwhile: two are needed. The other two are room for a
/// consumer that keeps a frame a pass longer.
const POOL_FRAMES: usize = 4;
/// The furthest the subtitles are moved against the picture: a minute.
const MAX_SUBTITLE_SHIFT_MILLIS: i32 = 60_000;
/// How long a source may take to open.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest lead mpv is asked for: a tenth of a second.
pub const MAX_EARLY_MICROS: u32 = 100_000;
/// A lead is set again only when it moved by more than this: the measured
/// display period wanders by microseconds from pass to pass.
const EARLY_STEP_MICROS: u32 = 200;
/// The drop counters of mpv are read every this many passes.
const COUNTER_PASSES: u64 = 16;
/// The longest label of a track shown to the user, in characters.
const LABEL_CHARS: usize = 60;
/// The most tracks of one kind a list names, and the most entries of the
/// track list of mpv looked at for them: a file decides how many it has.
const LISTED_TRACKS: usize = 32;
const TRACK_LIST_ENTRIES: i64 = 256;

/// The options of every core. No configuration, scripts or network helper of
/// the user, no window (`vo=libmpv` is the render API), no text of mpv in
/// the picture, and a file pulls in no other file. A video of a single frame
/// without sound (mpv takes it for an image) ends after its frame instead of
/// standing for `image-display-duration`, 5 s by default, before the
/// playlist goes on; picture files are no entries of the playlist anyway.
const OPTIONS: [(&CStr, &CStr); 11] = [
    (c"config", c"no"),
    (c"load-scripts", c"no"),
    (c"ytdl", c"no"),
    (c"vo", c"libmpv"),
    (c"hwdec", c"no"),
    (c"audio-display", c"no"),
    (c"keep-open", c"no"),
    (c"osd-level", c"0"),
    (c"ordered-chapters", c"no"),
    (c"access-references", c"no"),
    (c"image-display-duration", c"0"),
];

/// What [`VideoBackend::open`] takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sources {
    /// Only a file that exists: the window (playlist, drop).
    Files,
    /// Anything mpv opens, also its test patterns (`av://lavfi:...`): the
    /// measuring run and the tests, where the caller wrote the source.
    Any,
}

/// Where the sound of mpv goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sound {
    /// The default output device.
    Default,
    /// The device whose description holds this text (case-insensitive). When
    /// none does, the sound is silent, never the default device.
    Named(String),
    /// No device: timed, silent (`ao=null`).
    Silent,
}

impl Sound {
    /// What the environment asks for, as the sound path of the test picture
    /// reads it: `TAKT_AUDIO=sim` is silent, `TAKT_AUDIO_DEVICE` names a
    /// device, and `otherwise` holds for neither.
    #[must_use]
    pub fn from_env(otherwise: Self) -> Self {
        if std::env::var("TAKT_AUDIO").as_deref() == Ok("sim") {
            return Self::Silent;
        }
        match std::env::var("TAKT_AUDIO_DEVICE") {
            Ok(name) => Self::Named(name),
            Err(_) => otherwise,
        }
    }
}

/// How long before its time mpv hands a frame out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Early {
    /// Half the display period the player measured; nothing while it is
    /// unknown.
    HalfDisplayPeriod,
    /// This many microseconds, whatever the display (a measuring run).
    Fixed(u32),
}

/// How a backend is set up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// What may be opened.
    pub sources: Sources,
    /// Where the sound goes.
    pub sound: Sound,
    /// The lead of the frames.
    pub early: Early,
}

/// A load mpv has not finished.
struct Loading {
    asked: Instant,
    /// The number mpv gave the entry: only events with it belong to this
    /// load. A load that replaced another one sees the end of that one
    /// first, and must not take it for its own. `None` when mpv names no
    /// number; then the first start counts.
    entry: Option<i64>,
    started: bool,
}

/// How far a jump mpv was asked for has come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Jump {
    /// None is on its way.
    Done,
    /// Asked for; mpv has not begun it. Its position is still the old one,
    /// and it does not call itself seeking yet.
    Asked,
    /// mpv is at it; its position is the target.
    Begun,
}

/// A track to choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    /// The sound track.
    Audio,
    /// The subtitle track.
    Subtitle,
}

/// A track of the open file, for a list to choose from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackEntry {
    /// The number mpv gave it; what [`MpvBackend::select_track`] takes.
    pub id: i64,
    /// Its number, and its language and title when the file names them.
    /// The text of the file is foreign: cut to a line, never interpreted.
    pub label: String,
    /// It is the one that plays.
    pub selected: bool,
}

/// What the render thread and the thread that steers share.
#[derive(Default)]
struct Shared {
    /// The newest frame rendered and not yet taken. A frame that is not
    /// taken in time is replaced by the next one and goes back to its pool.
    slot: Mutex<Option<PooledFrame>>,
    /// The render thread ends.
    stop: AtomicBool,
    /// Frames rendered since the last file was asked for.
    decoded: AtomicU64,
    /// How many of them were the frame before, drawn once more: mpv asks
    /// for that when it starts a file, and when the subtitles change while
    /// it is paused. Such a picture carries the time of the frame it shows
    /// again, so two frames with one time are no error of the stamp.
    redrawn: AtomicU64,
    /// Frames of the pool that are out, as of the last render.
    in_flight: AtomicUsize,
    /// Every frame of the pool was out when a picture waited: it was not
    /// rendered. Told once by [`VideoBackend::next_frame`].
    exhausted: AtomicBool,
    /// Frames of mpv that reached no frame of the pool: no picture (no
    /// video), or every frame of the pool out.
    discarded: AtomicU64,
    /// What rendering a frame took.
    render: DurationHistogram,
    /// Why a frame could not be rendered, until `next_frame` tells it.
    fault: Mutex<Option<Error>>,
}

impl Shared {
    /// Takes the frame of the slot, if one is there.
    fn take(&self) -> Option<PooledFrame> {
        self.slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// The size of the picture: the display size once a frame is decoded (it
/// holds the aspect of anamorphic video), the size of the container before
/// that. `None` without video. `int` reads a property of mpv.
fn picture_size(mut int: impl FnMut(&CStr) -> Option<i64>) -> Option<(u32, u32)> {
    let mut side = |name: &CStr| {
        int(name)
            .and_then(|v| u32::try_from(v).ok())
            .filter(|&v| v > 0)
    };
    match (side(c"dwidth"), side(c"dheight")) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => side(c"width").zip(side(c"height")),
    }
}

/// What a frame of the backend is: `rgb0` of this size.
fn frame_meta(size: (u32, u32)) -> FrameMeta {
    FrameMeta {
        width: size.0,
        height: size.1,
        format: PixelFormat::Rgb0,
        // Not what the file names: mpv converted the picture to BT.709
        // primaries with the gamma of BT.1886, R'G'B' in full range (module
        // documentation, measured by the colour tests).
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Bt709,
        range: ColorRange::Full,
        pts: Timestamp::new(0, Rational::MICROS),
    }
}

/// The render thread: renders every frame mpv has ready, as soon as mpv
/// says so, until `shared.stop`.
fn render_loop(mut renderer: Renderer, shared: &Shared, clock: &AudioMasterClock) {
    // The pool is this thread's own: frames that are out stay valid when it
    // is replaced, and go back to the pool they came from.
    let mut pool: Sized = None;
    while !shared.stop.load(Ordering::Acquire) {
        // The limit only bounds how long a lost wake would cost.
        renderer.wait(Duration::from_millis(250));
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        // One wake may stand for several frames: until mpv has none.
        while renderer.frame_ready() {
            if let Err(error) = render_one(&mut renderer, &mut pool, shared, clock) {
                *shared.fault.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
            }
        }
    }
}

/// The pool of the render thread, and the size of its frames.
type Sized = Option<((u32, u32), FramePool)>;

/// Renders the frame mpv has ready into a pooled frame and puts it into the
/// slot.
///
/// First the render, then the questions (module documentation): the frame
/// is drawn at the size the thread knows, or counted as shown when it knows
/// none, so that the core of mpv stops waiting. Then the size is read. Is it
/// another one, the pool is made anew and the same picture drawn once more
/// at that size; mpv draws its current frame as often as it is asked.
fn render_one(
    renderer: &mut Renderer,
    pool: &mut Sized,
    shared: &Shared,
    clock: &AudioMasterClock,
) -> takt_core::Result<()> {
    let before = Instant::now();
    // Asked before the render: afterwards the frame no longer waits.
    let waiting = renderer.waiting();
    let mut frame = match pool.as_mut() {
        Some((size, frames)) => draw(renderer, *size, frames, shared, clock)?,
        None => {
            renderer.discard_frame().map_err(backend_error)?;
            None
        }
    };
    // Only now is mpv asked anything.
    let size = picture_size(|name| renderer.int(name));
    if pool.as_ref().map(|(of, _)| *of) != size {
        // The frame drawn at the old size goes back to its pool.
        frame = None;
        *pool = None;
        if let Some(size) = size {
            // A size beyond 8K is refused before anything is allocated.
            if !ffi::fits(size.0, size.1) {
                return Err(Error::InvalidGeometry {
                    width: size.0,
                    height: size.1,
                });
            }
            let mut frames = FramePool::new(frame_meta(size), POOL_FRAMES);
            frame = draw(renderer, size, &mut frames, shared, clock)?;
            *pool = Some((size, frames));
        }
    }
    let Some(mut frame) = frame else {
        // No picture size (no video), or no frame to draw into.
        shared.discarded.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    };
    // The position of the frame just rendered; without one (the very first
    // frame of some files) the clock itself, an error of zero.
    let position = renderer
        .double(c"time-pos")
        .and_then(micros)
        .map_or_else(|| clock.now(), |t| Timestamp::new(t, Rational::MICROS));
    frame.get_mut().set_pts(position);
    shared
        .render
        .record(u64::try_from(before.elapsed().as_nanos()).unwrap_or(u64::MAX));
    shared.decoded.fetch_add(1, Ordering::Relaxed);
    if waiting == Some(Waiting::Again) {
        shared.redrawn.fetch_add(1, Ordering::Relaxed);
    }
    // Put down before the count: the frame it replaces goes back first.
    *shared.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(frame);
    if let Some((_, frames)) = pool.as_ref() {
        shared
            .in_flight
            .store(frames.in_flight(), Ordering::Relaxed);
    }
    Ok(())
}

/// Draws the current frame of mpv into a frame of `frames`, which are of
/// `size`. With every frame out, the one in the slot is the place: it is the
/// frame this one replaces anyway. Is there none either, mpv is told to
/// count the frame as shown, and `None` comes back.
fn draw(
    renderer: &mut Renderer,
    size: (u32, u32),
    frames: &mut FramePool,
    shared: &Shared,
    clock: &AudioMasterClock,
) -> takt_core::Result<Option<PooledFrame>> {
    let kept = || {
        shared.take().filter(|kept| {
            let meta = kept.get().meta();
            (meta.width, meta.height) == size
        })
    };
    let Some(mut frame) = frames.acquire(clock.now()).or_else(kept) else {
        shared.exhausted.store(true, Ordering::Release);
        renderer.discard_frame().map_err(backend_error)?;
        return Ok(None);
    };
    let (width, height) = size;
    let geometry = Error::InvalidGeometry { width, height };
    let picture = frame.get_mut();
    let stride = picture.stride(0).ok_or(geometry.clone())?;
    // mpv writes into the plane of the frame itself: it starts aligned and
    // its stride is aligned (`takt_core::VideoFrame`).
    let target = picture
        .plane_mut(0)
        .and_then(|rows| Target::new(rows, width, height, stride))
        .ok_or(geometry)?;
    renderer.render(target).map_err(backend_error)?;
    Ok(Some(frame))
}

/// libmpv as a [`VideoBackend`].
pub struct MpvBackend {
    mpv: Mpv,
    /// What the render thread hands over and counts.
    shared: Arc<Shared>,
    /// Wakes the render thread, to end it.
    waker: Waker,
    /// The render thread, until this backend is dropped.
    thread: Option<JoinHandle<()>>,
    /// The core was made for files: whatever mpv opens is checked
    /// ([`MpvBackend::on_hook`]).
    guard: bool,
    sources: Sources,
    early: Early,
    /// The lead mpv has now, in microseconds.
    early_micros: u32,
    /// How much later than the file says the subtitles are shown, in
    /// milliseconds (negative: earlier).
    subtitle_shift: i32,
    sound_line: String,
    clock: Arc<AudioMasterClock>,
    writer: AudioClockWriter,
    time: Arc<dyn MonotonicSource>,
    /// Width and height of the picture as the open file was described,
    /// `None` without video. The frames carry their own size.
    size: Option<(u32, u32)>,
    /// The video changed: look at its size again.
    refit: bool,
    /// The jump that is on its way.
    jump: Jump,
    /// The file mpv was asked for and has not opened yet.
    loading: Option<Loading>,
    /// How long a load may take ([`OPEN_TIMEOUT`]; a test shortens it).
    open_timeout: Duration,
    /// How the last load ended, until [`MpvBackend::poll_open`] takes it.
    opened: Option<takt_core::Result<MediaInfo>>,
    loaded: bool,
    eof: bool,
    /// The file has a sound track that plays (the clock follows it).
    has_sound: bool,
    stats: BackendStats,
    passes: u64,
}

fn backend_error(error: super::ffi::MpvError) -> Error {
    Error::Backend(error.to_string())
}

/// Whole microseconds as seconds with six decimals, from integers: no float
/// is formatted. Negative values are zero.
/// Milliseconds as the seconds mpv reads, with a sign: `-0.100`.
fn signed_seconds_text(millis: i32) -> String {
    let size = millis.unsigned_abs();
    let sign = if millis < 0 { "-" } else { "" };
    format!("{sign}{}.{:03}", size / 1_000, size % 1_000)
}

fn seconds_text(micros: i64) -> String {
    let micros = micros.max(0);
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

/// A frame rate from millionths of a frame per second. The NTSC rates come
/// out of mpv as 23.976024 and the like; they are the exact fraction again
/// (24000/1001), so no rounding error enters the time base.
fn rate_of(micro_fps: i64) -> Option<Rational> {
    if micro_fps <= 0 {
        return None;
    }
    // Whole rates, and whole rates times 1000/1001, within half a thousandth.
    let whole = (micro_fps + 500_000) / 1_000_000;
    if (whole * 1_000_000 - micro_fps).abs() < 500 {
        return Rational::new(whole, 1);
    }
    let ntsc = (micro_fps * 1_001 + 500_000_000) / 1_000_000_000;
    if (ntsc * 1_000_000_000 / 1_001 - micro_fps).abs() < 500 {
        return Rational::new(ntsc * 1_000, 1_001);
    }
    Rational::new(micro_fps, 1_000_000)
}

/// The primaries mpv names (`video-params/primaries`). A name Takt has no
/// type for is unknown, never the nearest one.
fn primaries_of(name: &str) -> ColorPrimaries {
    match name {
        "bt.709" => ColorPrimaries::Bt709,
        "bt.601-525" => ColorPrimaries::Bt601Ntsc,
        "bt.601-625" => ColorPrimaries::Bt601Pal,
        "bt.2020" => ColorPrimaries::Bt2020,
        "dci-p3" => ColorPrimaries::DciP3,
        "display-p3" => ColorPrimaries::DisplayP3,
        _ => ColorPrimaries::Unknown,
    }
}

/// The transfer function mpv names (`video-params/gamma`). BT.1886 is the
/// display side of the BT.709 curve; the pure gammas (2.2, 2.8, ...) and the
/// camera curves have no type here and are unknown.
fn transfer_of(name: &str) -> TransferFunction {
    match name {
        "bt.1886" => TransferFunction::Bt709,
        "srgb" => TransferFunction::Srgb,
        "pq" => TransferFunction::Pq,
        "hlg" => TransferFunction::Hlg,
        "linear" => TransferFunction::Linear,
        _ => TransferFunction::Unknown,
    }
}

/// The range mpv names (`video-params/colorlevels`). Video is limited
/// unless it says full.
fn range_of(name: &str) -> ColorRange {
    if name == "full" {
        ColorRange::Full
    } else {
        ColorRange::Limited
    }
}

/// Reads a JSON string whose opening quote is consumed, up to its closing
/// quote. An escape that is not valid becomes U+FFFD.
fn json_string(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
                    out.push(code.unwrap_or(char::REPLACEMENT_CHARACTER));
                }
                // `\"`, `\\`, `\/` and anything else: the character itself.
                Some(other) => out.push(other),
                None => break,
            },
            c => out.push(c),
        }
    }
    out
}

/// Name and description of every entry of the property `audio-device-list`,
/// which mpv hands out as JSON: an array of objects with text values. Only
/// that shape is read; anything else in the text is skipped.
fn devices(json: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = json.chars().peekable();
    let (mut name, mut description, mut key) = (None, None, None::<String>);
    while let Some(c) = chars.next() {
        match c {
            '{' => (name, description, key) = (None, None, None),
            '}' => {
                if let (Some(name), Some(description)) = (name.take(), description.take()) {
                    out.push((name, description));
                }
            }
            '"' => {
                let text = json_string(&mut chars);
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
                if chars.peek() == Some(&':') {
                    chars.next();
                    key = Some(text);
                } else {
                    match key.take().as_deref() {
                        Some("name") => name = Some(text),
                        Some("description") => description = Some(text),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The entry whose description holds `wanted`, case-insensitive; an empty
/// wish fits none (as `Output::open_on` of takt-audio).
fn pick_device<'a>(list: &'a [(String, String)], wanted: &str) -> Option<&'a (String, String)> {
    let wanted = wanted.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    list.iter()
        .find(|(_, description)| description.to_lowercase().contains(&wanted))
}

/// What mpv is given to open in place of a source that is no file: a name
/// with a protocol nothing handles, so the open fails at once.
const REFUSED: &str = "takt-refused://";

/// Whether `name` is a file that exists, named by its whole path: the one
/// kind of source the window lets mpv open.
fn is_plain_file(name: &str) -> bool {
    let path = std::path::Path::new(name);
    path.is_absolute() && path.is_file()
}

/// Foreign text for a label: no control characters, at most
/// [`LABEL_CHARS`] characters.
fn label_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(LABEL_CHARS)
        .collect()
}

/// Whether the core converts every picture to BT.709 primaries and gamma, as
/// [`frame_meta`] says it does. mpv's software renderer does that through
/// zimg; a libmpv built without it, or a core with `sws-allow-zimg=no`, only
/// applies the matrix, and the frames would keep the primaries of the file
/// under a BT.709 label. Such a core is refused rather than mislabelled.
/// `string` reads a property of the core.
fn check_colour_conversion(mut string: impl FnMut(&CStr) -> Option<String>) -> Result<(), String> {
    // The options of zimg exist only in a build with it.
    if string(c"option-info/zimg-scaler/name").is_none() {
        return Err(
            "this libmpv is built without zimg: it would not convert the colours of a file to BT.709 \
             (docs/BUILD.md names builds that do)"
                .to_owned(),
        );
    }
    if string(c"sws-allow-zimg").as_deref() != Some("yes") {
        return Err(
            "sws-allow-zimg is off: mpv would not convert the colours of a file to BT.709"
                .to_owned(),
        );
    }
    Ok(())
}

impl MpvBackend {
    /// A core of mpv, set up as `config` says. `time` is the monotonic clock
    /// the anchors of [`MpvBackend::clock`] are stamped with.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] when libmpv does not start or refuses an option, or
    /// when it would not convert the colours of a file to BT.709
    /// (built without zimg).
    pub fn new(config: Config, time: Arc<dyn MonotonicSource>) -> takt_core::Result<Self> {
        let fixed = match config.early {
            Early::Fixed(micros) => micros.min(MAX_EARLY_MICROS),
            Early::HalfDisplayPeriod => 0,
        };
        let early = CString::new(seconds_text(i64::from(fixed)))
            .map_err(|e| Error::Backend(e.to_string()))?;
        let mut options = OPTIONS.to_vec();
        options.push((c"video-timing-offset", &early));
        if config.sound == Sound::Silent {
            options.push((c"ao", c"null"));
        }
        let (mut mpv, renderer) = Mpv::new(&options).map_err(backend_error)?;
        check_colour_conversion(|name| mpv.string(name)).map_err(Error::Backend)?;
        let guard = config.sources == Sources::Files;
        if guard {
            mpv.hook_on_load().map_err(backend_error)?;
        }

        let sound_line = match &config.sound {
            Sound::Default => "mpv: default device".to_owned(),
            Sound::Silent => "mpv: silent (ao=null)".to_owned(),
            Sound::Named(wanted) => {
                let list = devices(&mpv.string(c"audio-device-list").unwrap_or_default());
                match pick_device(&list, wanted) {
                    Some((name, description)) => {
                        mpv.set(c"audio-device", name).map_err(backend_error)?;
                        format!("mpv: device {:?}", label_text(description))
                    }
                    None => {
                        // Never the default device in place of a named one.
                        mpv.set(c"ao", "null").map_err(backend_error)?;
                        format!(
                            "mpv: silent (ao=null), no device fits {:?}",
                            label_text(wanted)
                        )
                    }
                }
            }
        };

        let clock = AudioMasterClock::new(CLOCK_RATE, Arc::clone(&time), MAX_EXTRAPOLATION_NANOS);
        let writer = clock
            .writer()
            .ok_or_else(|| Error::Backend("the clock has no writer".to_owned()))?;
        let clock = Arc::new(clock);
        let shared = Arc::new(Shared::default());
        let waker = renderer.waker();
        let thread = {
            let (shared, clock) = (Arc::clone(&shared), Arc::clone(&clock));
            std::thread::Builder::new()
                .name("takt-mpv-render".to_owned())
                .spawn(move || render_loop(renderer, &shared, &clock))
                .map_err(|e| Error::Backend(format!("the render thread did not start: {e}")))?
        };
        Ok(Self {
            mpv,
            shared,
            waker,
            thread: Some(thread),
            guard,
            sources: config.sources,
            early: config.early,
            early_micros: fixed,
            subtitle_shift: 0,
            sound_line,
            clock,
            writer,
            time,
            size: None,
            refit: false,
            jump: Jump::Done,
            loading: None,
            open_timeout: OPEN_TIMEOUT,
            opened: None,
            loaded: false,
            eof: false,
            has_sound: false,
            stats: BackendStats::default(),
            passes: 0,
        })
    }

    /// The clock that follows the sound of mpv. It is the only clock while
    /// this backend plays: the player swaps to it, it does not run next to
    /// another one.
    #[must_use]
    pub fn clock(&self) -> Arc<AudioMasterClock> {
        Arc::clone(&self.clock)
    }

    /// Where the sound goes, in one line.
    #[must_use]
    pub fn sound_line(&self) -> &str {
        &self.sound_line
    }

    /// The lead mpv hands frames out with, in microseconds.
    #[must_use]
    pub const fn early_micros(&self) -> u32 {
        self.early_micros
    }

    /// Whether the open file has a sound track the clock follows.
    #[must_use]
    pub const fn has_sound(&self) -> bool {
        self.has_sound
    }

    /// What rendering a frame into its pooled frame took so far.
    #[must_use]
    pub fn render_times(&self) -> DurationSnapshot {
        self.shared.render.snapshot()
    }

    /// Frames of mpv that reached no frame of the pool.
    #[cfg(test)]
    fn discarded(&self) -> u64 {
        self.shared.discarded.load(Ordering::Relaxed)
    }

    /// Frames of the pool, for the diagnostics.
    #[must_use]
    pub const fn pool_frames(&self) -> usize {
        POOL_FRAMES
    }

    /// What the file says about the colour of its picture, as mpv reads it:
    /// for a file that says nothing, mpv puts in its own guess (BT.709
    /// primaries). This is not the colour of the frames: those are
    /// BT.709, converted by mpv (module documentation). `None` without a
    /// picture.
    pub fn source_color(&mut self) -> Option<ColorDescription> {
        let primaries = self.mpv.string(c"video-params/primaries")?;
        let transfer = self.mpv.string(c"video-params/gamma");
        let range = self.mpv.string(c"video-params/colorlevels");
        Some(ColorDescription {
            primaries: primaries_of(&primaries),
            transfer: transfer.map_or(TransferFunction::Unknown, |name| transfer_of(&name)),
            range: range.map_or(ColorRange::Limited, |name| range_of(&name)),
        })
    }

    /// Whether mpv is still on its way to the target of the last jump. A
    /// frame that comes meanwhile may be one from before it, and whichever
    /// it is, it carries the target as its time, not its own
    /// ([`VideoBackend::next_frame`]).
    ///
    /// The events of mpv decide (a jump begins, playback goes on), as far
    /// as [`VideoBackend::next_frame`] has taken them: right after the
    /// command mpv does not call itself seeking yet and still names the old
    /// position, so its property alone would call a jump over before it
    /// began. The property is asked as well, for a second jump that was
    /// asked for while the first one ended.
    pub fn seeking(&mut self) -> bool {
        self.jump != Jump::Done || self.mpv.flag(c"seeking").unwrap_or(false)
    }

    /// Where mpv is in the file, in milliseconds: the time of the frame it
    /// shows, or the target of a jump while it is on its way there
    /// ([`MpvBackend::seeking`]). `None` when nothing plays.
    pub fn position_millis(&mut self) -> Option<i64> {
        self.mpv
            .double(c"time-pos")
            .and_then(micros)
            .map(|micros| micros / 1_000)
    }

    /// Pauses or resumes mpv (picture and sound) and its clock.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] when mpv refuses.
    pub fn set_paused(&mut self, paused: bool) -> takt_core::Result<()> {
        self.mpv
            .set(c"pause", if paused { "yes" } else { "no" })
            .map_err(backend_error)?;
        self.clock.set_paused(paused);
        Ok(())
    }

    /// Sets the volume of mpv, 0 (silent) to 1 (unchanged).
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] when mpv refuses.
    pub fn set_volume(&mut self, volume: f32) -> takt_core::Result<()> {
        let volume = if volume.is_nan() {
            0.0
        } else {
            volume.clamp(0.0, 1.0)
        };
        // Whole percent, clamped before the cast.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "0..=100 after the clamp"
        )]
        let percent = (volume * 100.0).round() as u32;
        self.mpv
            .set(c"volume", &percent.to_string())
            .map_err(backend_error)
    }

    /// Goes to the next track of `kind` (after the last one: none, then the
    /// first again) and says which one plays now, for a label.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] when mpv refuses.
    pub fn cycle_track(&mut self, kind: Track) -> takt_core::Result<String> {
        let property = match kind {
            Track::Audio => "aid",
            Track::Subtitle => "sid",
        };
        self.mpv
            .command("cycle", &["cycle", property])
            .map_err(backend_error)?;
        Ok(self.track_label(kind))
    }

    /// The tracks of `kind` the open file has, in the order of the file, at
    /// most [`LISTED_TRACKS`]; empty when nothing plays. A subtitle file
    /// that was added counts as a track.
    pub fn tracks(&mut self, kind: Track) -> Vec<TrackEntry> {
        let wanted = match kind {
            Track::Audio => "audio",
            Track::Subtitle => "sub",
        };
        let count = self
            .mpv
            .int(c"track-list/count")
            .unwrap_or(0)
            .clamp(0, TRACK_LIST_ENTRIES);
        let mut out = Vec::new();
        for index in 0..count {
            if out.len() >= LISTED_TRACKS {
                break;
            }
            let mut text = |field: &str| {
                CString::new(format!("track-list/{index}/{field}"))
                    .ok()
                    .and_then(|name| self.mpv.string(&name))
            };
            if text("type").as_deref() != Some(wanted) {
                continue;
            }
            let Some(id) = text("id").and_then(|id| id.parse::<i64>().ok()) else {
                continue;
            };
            let named: Vec<String> = [text("lang"), text("title")]
                .into_iter()
                .flatten()
                .map(|text| label_text(&text))
                .filter(|text| !text.is_empty())
                .collect();
            let label = if named.is_empty() {
                id.to_string()
            } else {
                format!("{id} ({})", named.join(", "))
            };
            out.push(TrackEntry {
                id,
                label,
                selected: text("selected").as_deref() == Some("yes"),
            });
        }
        out
    }

    /// Plays the track of `kind` with the number `id`, or none of that
    /// kind, and says which one plays now, for a label.
    ///
    /// # Errors
    ///
    /// When the file has no such track, or mpv refuses.
    pub fn select_track(&mut self, kind: Track, id: Option<i64>) -> takt_core::Result<String> {
        let property = match kind {
            Track::Audio => c"aid",
            Track::Subtitle => c"sid",
        };
        let value = match id {
            None => "no".to_owned(),
            Some(id) => {
                // Only a number the list names: mpv would keep any other
                // one as a wish for the next file.
                if !self.tracks(kind).iter().any(|track| track.id == id) {
                    return Err(Error::SourceUnavailable("no such track".to_owned()));
                }
                id.to_string()
            }
        };
        self.mpv.set(property, &value).map_err(backend_error)?;
        Ok(self.track_label(kind))
    }

    /// Shows the subtitles `by_millis` later than now (negative: earlier)
    /// and returns the shift against the file that holds from here on, at
    /// most a minute either way. It stays for the files that follow.
    ///
    /// # Errors
    ///
    /// When mpv refuses the value; the shift is unchanged then.
    pub fn shift_subtitles(&mut self, by_millis: i32) -> takt_core::Result<i32> {
        let wanted = self
            .subtitle_shift
            .saturating_add(by_millis)
            .clamp(-MAX_SUBTITLE_SHIFT_MILLIS, MAX_SUBTITLE_SHIFT_MILLIS);
        self.mpv
            .set(c"sub-delay", &signed_seconds_text(wanted))
            .map_err(backend_error)?;
        self.subtitle_shift = wanted;
        Ok(wanted)
    }

    /// Adds the subtitle file `path` to the file that plays, shows it, and
    /// names the track. The file is judged like a source ([`Sources`]): for
    /// the window it has to be a file that exists. What it may hold is the
    /// same as for a subtitle file next to the film (module documentation):
    /// formatting, but no way to another file or into the network.
    ///
    /// mpv reads the file before this returns: a local file takes
    /// milliseconds.
    ///
    /// # Errors
    ///
    /// When nothing plays, the file may not be opened, or mpv cannot read
    /// it as subtitles.
    pub fn add_subtitles(&mut self, path: &str) -> takt_core::Result<String> {
        if !self.loaded {
            return Err(Error::SourceUnavailable("no file plays".to_owned()));
        }
        let path = self.checked(path)?;
        self.mpv
            .command("sub-add", &["sub-add", &path, "select"])
            .map_err(|_| Error::SourceUnavailable("mpv cannot read it as subtitles".to_owned()))?;
        Ok(self.track_label(Track::Subtitle))
    }

    /// Whether the subtitles that show come from a file of their own.
    #[cfg(test)]
    pub fn shows_subtitles_of_a_file(&mut self) -> bool {
        self.mpv.flag(c"current-tracks/sub/external") == Some(true)
    }

    /// The track of `kind` that plays now, for a label: its number, and its
    /// language and title when the file names them. The text of the file is
    /// foreign: cut to a line, shown, never interpreted.
    pub fn track_label(&mut self, kind: Track) -> String {
        let (what, id, lang, title) = match kind {
            Track::Audio => (
                "audio",
                c"aid",
                c"current-tracks/audio/lang",
                c"current-tracks/audio/title",
            ),
            Track::Subtitle => (
                "subtitles",
                c"sid",
                c"current-tracks/sub/lang",
                c"current-tracks/sub/title",
            ),
        };
        let id = self.mpv.string(id).unwrap_or_default();
        if id.is_empty() || id == "no" {
            return format!("{what}: off");
        }
        let named: Vec<String> = [lang, title]
            .into_iter()
            .filter_map(|property| self.mpv.string(property))
            .map(|text| label_text(&text))
            .filter(|text| !text.is_empty())
            .collect();
        if named.is_empty() {
            format!("{what}: {}", label_text(&id))
        } else {
            format!("{what}: {} ({})", label_text(&id), named.join(", "))
        }
    }

    /// mpv is about to open a source and waits: in a core for files, what
    /// is no file is replaced by a name nothing opens. Then mpv goes on.
    fn on_hook(&mut self, id: u64) {
        if self.guard {
            let name = self.mpv.string(c"stream-open-filename").unwrap_or_default();
            if !is_plain_file(&name) {
                // If the name cannot be replaced, mpv must not go on with
                // it: the hook stays open and the file never loads.
                if self.mpv.set(c"stream-open-filename", REFUSED).is_err() {
                    return;
                }
            }
        }
        let _ = self.mpv.hook_continue(id);
    }

    /// The source as mpv gets it, or why it does not get it.
    fn checked(&self, source: &str) -> takt_core::Result<String> {
        match self.sources {
            Sources::Any => Ok(source.to_owned()),
            Sources::Files => {
                let path = std::path::absolute(source)
                    .map_err(|e| Error::SourceUnavailable(e.to_string()))?;
                if !path.is_file() {
                    return Err(Error::SourceUnavailable("not a file".to_owned()));
                }
                path.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::SourceUnavailable("its name is not Unicode".to_owned()))
            }
        }
    }

    /// Asks mpv for `source` and returns at once; [`MpvBackend::poll_open`]
    /// says how it ended. What played before is gone from here on. A load
    /// that is still on its way is replaced: mpv gives it up.
    ///
    /// # Errors
    ///
    /// When the source may not be opened ([`Sources`]) or mpv refuses the
    /// command; nothing is loading then.
    pub fn begin_open(&mut self, source: &str) -> takt_core::Result<()> {
        let source = self.checked(source)?;
        // A picture of the file before is not one of this file.
        drop(self.shared.take());
        self.shared.decoded.store(0, Ordering::Relaxed);
        self.shared.redrawn.store(0, Ordering::Relaxed);
        self.loaded = false;
        self.eof = true;
        self.jump = Jump::Done;
        self.stats = BackendStats::default();
        self.loading = None;
        self.opened = None;
        self.mpv.load(&source).map_err(backend_error)?;
        self.loading = Some(Loading {
            asked: Instant::now(),
            // `loadfile` replaces the playlist: its one entry is this load.
            entry: self.mpv.int(c"playlist/0/id").filter(|&id| id > 0),
            started: false,
        });
        Ok(())
    }

    /// How the load of [`MpvBackend::begin_open`] ended: `None` while mpv
    /// is still opening (or nothing was asked), then once the description of
    /// the file or why it does not play. Never waits.
    pub fn poll_open(&mut self) -> Option<takt_core::Result<MediaInfo>> {
        self.pump();
        let limit = self.open_timeout;
        if self
            .loading
            .as_ref()
            .is_some_and(|load| load.asked.elapsed() > limit)
        {
            self.fail_open(Error::SourceUnavailable(format!(
                "not open after {limit:?}"
            )));
        }
        self.opened.take()
    }

    /// The load failed: mpv is told to stop, and the reason is kept.
    fn fail_open(&mut self, error: Error) {
        self.loading = None;
        // What did not open must not go on behind the back of the player: a
        // playlist file "ends" by handing its entries to mpv, which would
        // play them without a picture here.
        let _ = self.mpv.command("stop", &["stop"]);
        self.opened = Some(Err(error));
    }

    /// mpv has opened the file: the description of what plays now.
    fn finish_open(&mut self) {
        self.loading = None;
        self.loaded = true;
        self.eof = false;
        if let Err(error) = self.fit() {
            self.loaded = false;
            self.eof = true;
            self.fail_open(error);
            return;
        }
        self.publish_position();
        let duration = self
            .mpv
            .double(c"duration")
            .and_then(micros)
            .filter(|&d| d > 0)
            .map(|d| Timestamp::new(d, Rational::MICROS));
        self.opened = Some(Ok(MediaInfo {
            duration,
            frame_rate: self
                .mpv
                .double(c"container-fps")
                .and_then(micros)
                .and_then(rate_of),
            video: self.size.map(frame_meta),
            source_color: self.source_color(),
            sample_rate: self
                .mpv
                .int(c"audio-params/samplerate")
                .and_then(|rate| u32::try_from(rate).ok()),
            seekable: self.mpv.flag(c"seekable").unwrap_or(false),
        }));
    }

    /// Reads the size of the picture for the description of the file. The
    /// frames do not depend on it: the render thread sizes each one itself.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidGeometry`] for a picture beyond 8K, which is not
    /// rendered.
    fn fit(&mut self) -> takt_core::Result<()> {
        self.refit = false;
        self.size = None;
        let Some(size) = picture_size(|name| self.mpv.int(name)) else {
            return Ok(());
        };
        if !ffi::fits(size.0, size.1) {
            return Err(Error::InvalidGeometry {
                width: size.0,
                height: size.1,
            });
        }
        self.size = Some(size);
        Ok(())
    }

    /// Takes what mpv reported since the last pass.
    fn pump(&mut self) {
        loop {
            match self.mpv.poll() {
                Event::None => break,
                event => self.on_event(event),
            }
        }
    }

    /// Takes one event. While a load is on its way, the events of its entry
    /// decide how it ends; the events of the file it replaces are not its
    /// own.
    fn on_event(&mut self, event: Event) {
        if let Some(load) = self.loading.as_mut() {
            let mine = |entry: i64| load.entry.is_none_or(|id| id == entry);
            match event {
                Event::StartFile { entry } if mine(entry) => load.started = true,
                Event::FileLoaded if load.started => self.finish_open(),
                Event::EndFile { entry, .. } if load.started && mine(entry) => {
                    self.fail_open(Error::SourceUnavailable("mpv cannot open it".to_owned()));
                }
                Event::Shutdown => {
                    self.fail_open(Error::Backend("mpv shut down".to_owned()));
                }
                Event::Hook { id } => self.on_hook(id),
                Event::VideoReconfig => self.refit = true,
                // A file that starts to play says so with the event of a
                // jump that ended; none was asked for yet.
                Event::None
                | Event::StartFile { .. }
                | Event::FileLoaded
                | Event::EndFile { .. }
                | Event::Seek
                | Event::PlaybackRestart
                | Event::Other => {}
            }
            return;
        }
        match event {
            Event::EndFile { .. } | Event::Shutdown => {
                self.eof = true;
                self.loaded = false;
                self.jump = Jump::Done;
            }
            Event::VideoReconfig => self.refit = true,
            Event::Hook { id } => self.on_hook(id),
            Event::Seek => {
                if self.jump == Jump::Asked {
                    self.jump = Jump::Begun;
                }
            }
            // The end of a jump that began after the last one was asked
            // for; the end of an earlier one says nothing about it.
            Event::PlaybackRestart => {
                if self.jump == Jump::Begun {
                    self.jump = Jump::Done;
                }
            }
            Event::None | Event::StartFile { .. } | Event::FileLoaded | Event::Other => {}
        }
    }

    /// Publishes the position of mpv as the anchor of the clock: the sound
    /// position, or the video position for a file without sound.
    fn publish_position(&mut self) {
        let sound = self.mpv.double(c"audio-pts").and_then(micros);
        self.has_sound = sound.is_some();
        let position = sound.or_else(|| self.mpv.double(c"time-pos").and_then(micros));
        if let Some(position) = position {
            let position = u64::try_from(position).unwrap_or(0);
            self.writer.report(position, self.time.now_nanos());
        }
    }

    fn read_counters(&mut self) {
        let output = self.mpv.int(c"frame-drop-count").unwrap_or(0);
        let decoder = self.mpv.int(c"decoder-frame-drop-count").unwrap_or(0);
        self.stats.dropped = u64::try_from(output.saturating_add(decoder)).unwrap_or(0);
    }

    /// The counters, with what the render thread counted.
    fn counted(&self) -> BackendStats {
        BackendStats {
            decoded: self.shared.decoded.load(Ordering::Relaxed),
            queued: self.shared.in_flight.load(Ordering::Relaxed),
            ..self.stats
        }
    }

    /// Reads the drop counters of mpv now (they are read every few passes
    /// otherwise): for the last line of a measuring run.
    pub fn refresh_stats(&mut self) -> BackendStats {
        self.read_counters();
        self.counted()
    }
}

impl Drop for MpvBackend {
    /// Ends the render thread before the core goes: the thread frees the
    /// render context, which has to happen first (`render.h`).
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.waker.wake();
        if let Some(thread) = self.thread.take() {
            // A render thread that panicked has nothing left to hand over.
            let _ = thread.join();
        }
    }
}

impl VideoBackend for MpvBackend {
    fn name(&self) -> &'static str {
        "mpv-sw"
    }

    /// Blocks until mpv has opened the source (a local file: milliseconds),
    /// at most ten seconds: [`MpvBackend::begin_open`] and then
    /// [`MpvBackend::poll_open`] until it answers. The window does not call
    /// this; it polls once a pass.
    fn open(&mut self, source: &str) -> takt_core::Result<MediaInfo> {
        self.begin_open(source)?;
        loop {
            if let Some(ended) = self.poll_open() {
                return ended;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn seek(&mut self, to: Timestamp, mode: SeekMode) -> takt_core::Result<()> {
        let micros = to
            .rescale(Rational::MICROS)
            .ok_or(Error::TimeOverflow)?
            .ticks();
        let how = match mode {
            SeekMode::Keyframe => "absolute+keyframes",
            SeekMode::Exact => "absolute+exact",
        };
        self.mpv
            .command("seek", &["seek", &seconds_text(micros), how])
            .map_err(backend_error)?;
        self.jump = Jump::Asked;
        Ok(())
    }

    /// The clock is not asked: mpv decides when a frame is due (module
    /// documentation). Each call is one pass of the display: it publishes the
    /// position of mpv to [`MpvBackend::clock`].
    ///
    /// The frame is the newest one the render thread has put down (module
    /// documentation); one that was not taken in time is gone.
    ///
    /// The time of a frame is the position of mpv when it was rendered. In
    /// the run that is the time of that very frame (measured with a file
    /// whose picture is its frame number, test
    /// `a_frame_carries_its_own_time_except_on_the_way_to_a_jump`). While
    /// mpv is on its way to a jump its position is the target, so the
    /// picture of the jump carries the target and not its own time: it is
    /// due at once, which is what a jump wants, and where the jump landed
    /// is read from [`MpvBackend::position_millis`] once mpv has arrived.
    fn next_frame(&mut self, _clock: &dyn Clock) -> takt_core::Result<Option<PooledFrame>> {
        self.pump();
        self.publish_position();
        self.passes += 1;
        if self.passes.is_multiple_of(COUNTER_PASSES) {
            self.read_counters();
        }
        if self.refit {
            self.fit()?;
        }
        let fault = self
            .shared
            .fault
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(fault) = fault {
            return Err(fault);
        }
        if self.shared.exhausted.swap(false, Ordering::AcqRel) {
            return Err(Error::PoolExhausted {
                capacity: POOL_FRAMES,
            });
        }
        if self.loading.is_some() {
            // What is in the slot may be the first picture of the file that
            // opens: it stays until the file is open, then it is handed out.
            return Ok(None);
        }
        // Also after the file has ended: its last picture may still lie in
        // the slot, and a file of a few frames has no other. (A picture of
        // the file before does not: asking for a file empties the slot.)
        Ok(self.shared.take())
    }

    /// Half the period becomes the lead of the frames
    /// ([`Early::HalfDisplayPeriod`]); a fixed lead stays.
    fn set_display_period(&mut self, nanos: Option<i64>) {
        if self.early != Early::HalfDisplayPeriod {
            return;
        }
        let Some(period) = nanos.filter(|&p| p > 0) else {
            return;
        };
        let wanted = u32::try_from(period / 2_000)
            .unwrap_or(MAX_EARLY_MICROS)
            .min(MAX_EARLY_MICROS);
        if wanted.abs_diff(self.early_micros) <= EARLY_STEP_MICROS {
            return;
        }
        // A refusal leaves the lead as it was; the error of the frames shows it.
        if self
            .mpv
            .set(c"video-timing-offset", &seconds_text(i64::from(wanted)))
            .is_ok()
        {
            self.early_micros = wanted;
        }
    }

    fn is_eof(&self) -> bool {
        self.eof
    }

    fn stats(&self) -> BackendStats {
        self.counted()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use takt_clock::SystemMonotonic;

    const PATTERN: &str = "av://lavfi:testsrc2=size=320x240:rate=25";

    fn backend(sources: Sources) -> MpvBackend {
        let config = Config {
            sources,
            sound: Sound::Silent,
            early: Early::HalfDisplayPeriod,
        };
        MpvBackend::new(config, Arc::new(SystemMonotonic::new())).unwrap()
    }

    /// The test file of the repository: 3 s of a 160x90 pattern at 25 fps
    /// with a keyframe every second, a sine tone and two subtitle tracks
    /// (`scripts/make-test-clip.py`).
    fn clip() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/pattern-3s.mkv").to_owned()
    }

    /// Runs passes of 4 ms for at most `limit` and hands every frame to
    /// `take`, until it says stop or the source ends.
    fn passes(
        backend: &mut MpvBackend,
        limit: Duration,
        mut take: impl FnMut(&MpvBackend, &PooledFrame) -> bool,
    ) {
        let clock = backend.clock();
        let started = Instant::now();
        while started.elapsed() < limit && !backend.is_eof() {
            if let Some(frame) = backend.next_frame(clock.as_ref()).unwrap()
                && !take(backend, &frame)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    #[test]
    fn milliseconds_as_signed_seconds_without_a_float() {
        assert_eq!(signed_seconds_text(0), "0.000");
        assert_eq!(signed_seconds_text(100), "0.100");
        assert_eq!(signed_seconds_text(-100), "-0.100");
        assert_eq!(signed_seconds_text(-60_000), "-60.000");
        assert_eq!(signed_seconds_text(1_005), "1.005");
        assert_eq!(signed_seconds_text(i32::MIN), "-2147483.648");
    }

    #[test]
    fn microseconds_as_seconds_without_a_float() {
        assert_eq!(seconds_text(0), "0.000000");
        assert_eq!(seconds_text(8_333), "0.008333");
        assert_eq!(seconds_text(3_600_000_001), "3600.000001");
        assert_eq!(seconds_text(-5), "0.000000");
    }

    #[test]
    fn rates_of_mpv_are_exact_fractions_again() {
        assert_eq!(rate_of(25_000_000), Rational::new(25, 1));
        assert_eq!(rate_of(23_976_024), Rational::new(24_000, 1_001));
        assert_eq!(rate_of(29_970_030), Rational::new(30_000, 1_001));
        assert_eq!(rate_of(59_940_060), Rational::new(60_000, 1_001));
        assert_eq!(rate_of(12_500_000), Rational::new(25, 2));
        assert_eq!(rate_of(0), None);
        assert_eq!(rate_of(-1), None);
    }

    #[test]
    fn the_device_list_of_mpv_is_read_and_a_device_found_by_a_part_of_its_name() {
        let json = r#"[{"name":"auto","description":"Autoselect device"},
            {"name":"wasapi/{1a-2b}","description":"Voicemeeter In 4 (VB-Audio \"Voicemeeter\" VAIO)"},
            {"description":"Lautsprecher (Gerät)","name":"wasapi/{3c}","extra":7},
            {"name":"broken"}]"#;
        let list = devices(json);
        assert_eq!(list.len(), 3, "an entry without a description is none");
        assert_eq!(list[0], ("auto".to_owned(), "Autoselect device".to_owned()));
        assert_eq!(
            list[1].1,
            "Voicemeeter In 4 (VB-Audio \"Voicemeeter\" VAIO)"
        );
        assert_eq!(
            list[2].0, "wasapi/{3c}",
            "the order of the keys does not matter"
        );
        assert_eq!(
            pick_device(&list, "voicemeeter in 4").map(|d| d.0.as_str()),
            Some("wasapi/{1a-2b}")
        );
        assert_eq!(pick_device(&list, "Voicemeeter In 5"), None);
        assert_eq!(pick_device(&list, "  "), None, "an empty wish fits none");
        assert!(devices("").is_empty());
        assert!(devices("not json at all {").is_empty());
        // Escapes: a code point, a line feed, a backslash.
        let escaped = format!(
            r#"[{{"name":"a{0}u00e4{0}n","description":"b{0}{0}c"}}]"#,
            '\\'
        );
        assert_eq!(
            devices(&escaped)[0],
            ("a\u{e4}\n".to_owned(), "b\\c".to_owned())
        );
    }

    #[test]
    fn the_subtitles_move_against_the_picture_by_steps_up_to_a_minute() {
        let mut backend = backend(Sources::Files);
        assert_eq!(backend.shift_subtitles(100).unwrap(), 100);
        assert_eq!(backend.shift_subtitles(100).unwrap(), 200);
        assert_eq!(
            backend.mpv.string(c"sub-delay").as_deref(),
            Some("0.200000")
        );
        assert_eq!(backend.shift_subtitles(-300).unwrap(), -100);
        assert_eq!(
            backend.mpv.string(c"sub-delay").as_deref(),
            Some("-0.100000")
        );
        assert_eq!(backend.shift_subtitles(i32::MAX).unwrap(), 60_000);
        assert_eq!(backend.shift_subtitles(i32::MIN).unwrap(), -60_000);
        assert_eq!(backend.shift_subtitles(i32::MIN).unwrap(), -60_000);
        // It holds for the file that is opened next.
        backend.open(&clip()).unwrap();
        assert_eq!(
            backend.mpv.string(c"sub-delay").as_deref(),
            Some("-60.000000")
        );
    }

    #[test]
    fn the_colour_names_of_mpv_become_types_and_an_unknown_one_is_not_guessed() {
        assert_eq!(primaries_of("bt.709"), ColorPrimaries::Bt709);
        assert_eq!(primaries_of("bt.601-525"), ColorPrimaries::Bt601Ntsc);
        assert_eq!(primaries_of("bt.601-625"), ColorPrimaries::Bt601Pal);
        assert_eq!(primaries_of("bt.2020"), ColorPrimaries::Bt2020);
        assert_eq!(primaries_of("dci-p3"), ColorPrimaries::DciP3);
        assert_eq!(primaries_of("display-p3"), ColorPrimaries::DisplayP3);
        for unknown in ["bt.470m", "adobe", "apple", "", "BT.709", "bt.709 "] {
            assert_eq!(
                primaries_of(unknown),
                ColorPrimaries::Unknown,
                "{unknown:?}"
            );
        }
        assert_eq!(transfer_of("bt.1886"), TransferFunction::Bt709);
        assert_eq!(transfer_of("srgb"), TransferFunction::Srgb);
        assert_eq!(transfer_of("pq"), TransferFunction::Pq);
        assert_eq!(transfer_of("hlg"), TransferFunction::Hlg);
        assert_eq!(transfer_of("linear"), TransferFunction::Linear);
        for unknown in ["gamma2.2", "gamma2.8", "v-log", "auto", ""] {
            assert_eq!(
                transfer_of(unknown),
                TransferFunction::Unknown,
                "{unknown:?}"
            );
        }
        assert_eq!(range_of("full"), ColorRange::Full);
        assert_eq!(range_of("limited"), ColorRange::Limited);
        assert_eq!(range_of(""), ColorRange::Limited);
    }

    /// One of the colour files (`scripts/make-colour-clips.py`): the same
    /// encoded picture in all of them, only the tags differ.
    fn colour_clip(name: &str) -> String {
        format!(
            "{}/tests/data/colour-{name}.mkv",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    /// Opens a colour file and returns what it names, the description of
    /// its first frame, and red, green and blue in the middle of it.
    fn colour_of(name: &str) -> (ColorDescription, FrameMeta, [u8; 3]) {
        let mut backend = backend(Sources::Files);
        // The files are a fifth of a second long: held at their first
        // picture, they are still there when they are asked about.
        backend.set_paused(true).unwrap();
        let info = backend.open(&colour_clip(name)).unwrap();
        let mut seen = None;
        passes(&mut backend, Duration::from_secs(5), |_, frame| {
            let picture = frame.get();
            let rows = picture.plane(0).unwrap();
            let at = 32 * picture.stride(0).unwrap() + 32 * 4;
            seen = Some((*picture.meta(), [rows[at], rows[at + 1], rows[at + 2]]));
            false
        });
        let (meta, pixel) = seen.expect("a frame of the colour file");
        // mpv names the colour when the file opens or, on a busy machine,
        // only once it has decoded a frame: by now it does.
        let named = backend.source_color().expect("the colour of the file");
        assert!(info.source_color.is_none_or(|at_open| at_open == named));
        (named, meta, pixel)
    }

    fn near(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(&a, b)| a.abs_diff(b) <= 3)
    }

    #[test]
    fn the_frames_are_bt709_whatever_primaries_the_file_names() {
        let frames = ColorDescription {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt709,
            range: ColorRange::Full,
        };
        let limited = |primaries| ColorDescription {
            primaries,
            transfer: TransferFunction::Bt709,
            range: ColorRange::Limited,
        };

        // A BT.709 file: named as such, and its picture arrives as it is
        // (the encoded colour through the BT.709 matrix, nothing else).
        let (named, meta, plain) = colour_of("bt709");
        assert_eq!(named, limited(ColorPrimaries::Bt709));
        assert_eq!(meta.color(), frames);
        assert!(near(plain, [56, 152, 195]), "{plain:?}");

        // The two BT.601: the same bytes and the same matrix, named apart,
        // and both frames say BT.709. That they say so rightly shows in the
        // red: mpv converted the primaries, each kind its own way. Were the
        // picture left as the file has it, both would be the same.
        let (named, meta, ntsc) = colour_of("ntsc");
        assert_eq!(named, limited(ColorPrimaries::Bt601Ntsc));
        assert_eq!(meta.color(), frames);
        let (named, meta, pal) = colour_of("pal");
        assert_eq!(named, limited(ColorPrimaries::Bt601Pal));
        assert_eq!(meta.color(), frames);
        assert!(
            ntsc[0] > pal[0].saturating_add(10),
            "this libmpv did not convert the primaries (built without zimg?): \
             the frames would not be BT.709. ntsc {ntsc:?}, pal {pal:?}"
        );

        // Tags Takt has no type for: unknown, not the nearest name.
        let (named, meta, _) = colour_of("odd");
        assert_eq!(named.primaries, ColorPrimaries::Unknown);
        assert_eq!(named.transfer, TransferFunction::Unknown);
        assert_eq!(meta.color(), frames);
    }

    /// The labels of the frames hold only where mpv converts the colours: a
    /// core that does not (here `sws-allow-zimg=no`, as a build without zimg)
    /// is refused, and so is a build without the options of zimg.
    #[test]
    fn a_core_that_would_not_convert_the_colours_is_refused() {
        let (mut mpv, _renderer) = Mpv::new(&OPTIONS).unwrap();
        assert_eq!(check_colour_conversion(|name| mpv.string(name)), Ok(()));

        let mut without = OPTIONS.to_vec();
        without.push((c"sws-allow-zimg", c"no"));
        let (mut mpv, _renderer) = Mpv::new(&without).unwrap();
        let refused = check_colour_conversion(|name| mpv.string(name)).unwrap_err();
        assert!(refused.contains("sws-allow-zimg is off"), "{refused}");

        // A build without zimg has no option of it.
        let refused = check_colour_conversion(|name| {
            (name != c"option-info/zimg-scaler/name").then(|| "yes".to_owned())
        })
        .unwrap_err();
        assert!(refused.contains("built without zimg"), "{refused}");
    }

    #[test]
    fn hdr_is_cut_off_at_white_not_tone_mapped() {
        let (_, _, plain) = colour_of("bt709");
        for (name, transfer) in [
            ("hdr10", TransferFunction::Pq),
            ("hlg", TransferFunction::Hlg),
        ] {
            let (named, meta, pixel) = colour_of(name);
            assert_eq!(named.primaries, ColorPrimaries::Bt2020, "{name}");
            assert_eq!(named.transfer, transfer, "{name}");
            assert!(named.is_hdr());
            // The frame is SDR, and says so.
            assert!(!meta.is_hdr());
            assert_eq!(meta.primaries, ColorPrimaries::Bt709);
            // Green and blue of the file are mid greys of the HDR curve,
            // brighter than SDR white: a tone mapper would bring them below
            // white, this path cuts them off at it.
            assert!(plain[1] < 200 && plain[2] < 200, "{plain:?}");
            assert!(pixel[1] >= 250 && pixel[2] >= 250, "{name}: {pixel:?}");
        }
    }

    #[test]
    fn a_label_is_one_short_line() {
        assert_eq!(label_text("Deutsch\n\u{1b}[31m"), "Deutsch[31m");
        assert_eq!(label_text(&"x".repeat(200)).chars().count(), LABEL_CHARS);
    }

    #[test]
    fn a_device_that_does_not_exist_is_silent_never_the_default() {
        let config = Config {
            sources: Sources::Any,
            sound: Sound::Named("takt-no-such-device".to_owned()),
            early: Early::Fixed(0),
        };
        let mut backend = MpvBackend::new(config, Arc::new(SystemMonotonic::new())).unwrap();
        assert!(
            backend.sound_line().starts_with("mpv: silent"),
            "{}",
            backend.sound_line()
        );
        assert_eq!(backend.mpv.string(c"ao").as_deref(), Some("null"));
    }

    #[test]
    fn a_pattern_comes_as_rgb0_frames_on_the_clock_of_mpv() {
        let mut backend = backend(Sources::Any);
        let info = backend.open(PATTERN).unwrap();
        let video = info.video.unwrap();
        assert_eq!((video.width, video.height), (320, 240));
        assert_eq!(video.format, PixelFormat::Rgb0);
        assert_eq!(info.frame_rate, Rational::new(25, 1));
        assert_eq!(backend.name(), "mpv-sw");
        backend.set_display_period(Some(16_666_667));
        assert_eq!(backend.early_micros(), 8_333);

        let clock = backend.clock();
        let mut stamps = Vec::new();
        let mut pictures = Vec::new();
        let mut errors = Vec::new();
        passes(&mut backend, Duration::from_secs(2), |_, frame| {
            let meta = *frame.get().meta();
            assert_eq!(meta.format, PixelFormat::Rgb0);
            assert_eq!((meta.width, meta.height), (320, 240));
            assert_eq!(frame.get().stride(0), Some(320 * 4));
            assert_eq!(
                frame.get().plane(0).unwrap().as_ptr().align_offset(64),
                0,
                "mpv rendered into an aligned plane"
            );
            assert!(
                frame.get().plane(0).unwrap().iter().any(|&b| b != 0),
                "a picture reached the frame"
            );
            stamps.push(meta.pts);
            pictures.push(frame.get().plane(0).unwrap().to_vec());
            errors.push(clock.until_nanos(meta.pts));
            true
        });
        // The time never goes back. Two frames with one time are one frame
        // of the file drawn twice, which mpv does when it starts a file
        // (measured: up to three pictures of the first frame): then they
        // are the same picture, byte for byte, and there are no more of
        // them than the render thread was told to draw again. Two pictures
        // that differ under one time would be a wrong stamp.
        assert!(
            stamps.windows(2).all(|pair| pair[0] <= pair[1]),
            "{stamps:?}"
        );
        for (i, pair) in stamps.windows(2).enumerate() {
            assert!(
                pair[0] != pair[1] || pictures[i] == pictures[i + 1],
                "frames {i} and {} differ under one time: {stamps:?}",
                i + 1
            );
        }
        let again = stamps.windows(2).filter(|pair| pair[0] == pair[1]).count();
        let redrawn = backend.shared.redrawn.load(Ordering::Relaxed);
        assert!(again as u64 <= redrawn, "{again} > {redrawn}: {stamps:?}");
        // 25 frames a second for two seconds; a loaded machine shows fewer.
        let frames = stamps.len() - again;
        assert!((10..=51).contains(&frames), "{frames}: {stamps:?}");
        // The clock follows mpv: every frame within a tenth of a second.
        assert!(errors.iter().all(|e| e.abs() < 100_000_000), "{errors:?}");
        // Every frame that came was rendered; the render thread may have
        // rendered one more that no pass took.
        let rendered = backend.stats().decoded;
        assert!(rendered >= stamps.len() as u64, "{rendered}");
        assert_eq!(backend.render_times().total(), rendered);
        // The frames went back to the pool: at most the one in the slot
        // and the one being rendered are out.
        assert!(backend.stats().queued <= 2, "{}", backend.stats().queued);
    }

    #[test]
    fn only_a_file_that_exists_reaches_mpv_from_the_window() {
        let mut backend = backend(Sources::Files);
        for source in [
            PATTERN,
            "http://127.0.0.1:9/film.mkv",
            "memory://abc",
            "takt-no-such-file.mkv",
            env!("CARGO_MANIFEST_DIR"),
            "",
        ] {
            let error = backend.open(source).unwrap_err();
            assert!(
                matches!(error, Error::SourceUnavailable(_)),
                "{source}: {error}"
            );
        }
        // Nothing was loaded: mpv never heard of them.
        assert_eq!(backend.mpv.string(c"path"), None);
        assert!(backend.is_eof() || !backend.loaded);
    }

    /// Opens `http://127.0.0.1:<port>/film.mkv` on `backend` and says
    /// whether a connection reached the port.
    fn reaches_the_network(backend: &mut MpvBackend) -> bool {
        connects_during(|port| {
            let _ = backend.open(&format!("http://127.0.0.1:{port}/film.mkv"));
        })
    }

    /// Listens on a local port while `act` runs (it gets the port) and for
    /// 300 ms after it, and says whether a connection reached it. One that
    /// arrives at the very end still counts: the port is asked once more
    /// after the last wait.
    fn connects_during(act: impl FnOnce(u16)) -> bool {
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (reached, done) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let server = {
            let (reached, done) = (Arc::clone(&reached), Arc::clone(&done));
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    // A connection is closed at once: mpv fails fast.
                    if listener.accept().is_ok() {
                        reached.store(true, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                if listener.accept().is_ok() {
                    reached.store(true, Ordering::Relaxed);
                }
            })
        };
        act(port);
        std::thread::sleep(Duration::from_millis(300));
        done.store(true, Ordering::Relaxed);
        server.join().unwrap();
        reached.load(Ordering::Relaxed)
    }

    #[test]
    fn a_core_for_files_fetches_nothing_from_the_network() {
        // The control: a core for any source does connect, so the listener
        // would see it.
        assert!(reaches_the_network(&mut backend(Sources::Any)));
        // A core for files does not, even when an address gets past the
        // check of the source (as an entry of a playlist file does).
        let mut files = backend(Sources::Files);
        files.sources = Sources::Any;
        assert!(!reaches_the_network(&mut files));
        // And it still plays a file.
        files.sources = Sources::Files;
        assert!(files.open(&clip()).is_ok());
    }

    /// A folder of its own with the clip as `film.mkv`, removed on drop.
    struct Folder(std::path::PathBuf);

    impl Folder {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("takt-test-{}-{tag}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::copy(clip(), dir.join("film.mkv")).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, content: &str) -> String {
            let path = self.0.join(name);
            std::fs::write(&path, content).unwrap();
            path.to_string_lossy().into_owned()
        }

        fn film(&self) -> String {
            self.0.join("film.mkv").to_string_lossy().into_owned()
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Plays the film of `folder` for a second with every subtitle track
    /// that came from a file of its own switched on in turn, and returns the
    /// number of such tracks and every text mpv showed. `references` allows
    /// what the backend forbids (`access-references`), for the control.
    fn external_subtitles(folder: &Folder, references: bool) -> (usize, Vec<String>) {
        let mut backend = backend(Sources::Files);
        if references {
            // What the backend switches off, on again: the control.
            backend.mpv.set(c"access-references", "yes").unwrap();
        }
        backend.open(&folder.film()).unwrap();
        let name = |text: String| CString::new(text).unwrap();
        let tracks = backend.mpv.int(c"track-list/count").unwrap_or(0);
        let external: Vec<i64> = (0..tracks)
            .filter(|i| {
                backend
                    .mpv
                    .string(&name(format!("track-list/{i}/type")))
                    .as_deref()
                    == Some("sub")
                    && backend.mpv.flag(&name(format!("track-list/{i}/external"))) == Some(true)
            })
            .collect();
        let external: Vec<i64> = external
            .into_iter()
            .filter_map(|i| backend.mpv.int(&name(format!("track-list/{i}/id"))))
            .collect();
        let mut texts = Vec::new();
        for id in &external {
            backend.mpv.set(c"sid", &id.to_string()).unwrap();
            let _ = backend.seek(Timestamp::new(500_000, Rational::MICROS), SeekMode::Exact);
            let clock = backend.clock();
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(1_000) {
                let _ = backend.next_frame(clock.as_ref());
                if let Some(text) = backend.mpv.string(c"sub-text")
                    && !text.is_empty()
                    && !texts.contains(&text)
                {
                    texts.push(text);
                }
                std::thread::sleep(Duration::from_millis(4));
            }
        }
        (external.len(), texts)
    }

    /// One cue over the whole clip.
    fn srt(text: &str) -> String {
        format!("1\n00:00:00,000 --> 00:00:03,000\n{text}\n")
    }

    /// An ASS file made to be expensive to draw: `lines` events at once, each
    /// a long text with a wide border and a strong blur, scaled up, so that
    /// libass works on every frame of the three seconds.
    fn costly_ass(lines: usize) -> String {
        let mut ass = String::from(
            "[Script Info]\nScriptType: v4.00+\nPlayResX: 1920\nPlayResY: 1080\n\n\
             [V4+ Styles]\n\
             Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, \
             Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, \
             Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
             Style: Default,Arial,90,&H00FFFFFF,&H000000FF,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,\
             30,20,5,0,0,0,1\n\n\
             [Events]\n\
             Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
        );
        let text = "TAKT-COSTLY ".repeat(20);
        for line in 0..lines {
            // Every event moves and turns a little in every frame: nothing of
            // the frame before can be reused.
            ass.push_str(&format!(
                "Dialogue: {line},0:00:00.00,0:00:03.00,Default,,0,0,0,,\
                 {{\\blur40\\bord40\\move(0,{y},1920,{y2})\\t(\\frz360\\fscx400\\fscy400)}}{text}\n",
                y = line * 7 % 1080,
                y2 = (line * 13 + 500) % 1080,
            ));
        }
        ass
    }

    /// A subtitle file that takes libass long to draw must hold up the
    /// render thread only, not the window: the passes of the window stay
    /// short while the frames take long to come.
    ///
    /// Known red (measured 2026-11-15, mpv 0.41): a pass waits as long as a
    /// frame takes to draw (about 150 ms with this file), because every
    /// question to the core (`audio-pts` in `publish_position`) waits while
    /// the render thread draws the subtitles. Bug
    /// `takt/mpv-backend-teure-untertitel-halten-das-fenster-an`; the test
    /// runs with `--ignored`.
    #[test]
    #[ignore = "known red: a pass waits for the core while libass draws (takt/mpv-backend-teure-untertitel-halten-das-fenster-an)"]
    fn subtitles_that_are_costly_to_draw_do_not_hold_up_the_window() {
        let folder = Folder::new("costly-ass");
        let subtitles = folder.write("film.ass", &costly_ass(60));
        let mut backend = backend(Sources::Files);
        backend.open(&folder.film()).unwrap();
        backend.add_subtitles(&subtitles).unwrap();
        let clock = backend.clock();
        let started = Instant::now();
        let mut longest = Duration::ZERO;
        while started.elapsed() < Duration::from_secs(2) && !backend.is_eof() {
            let pass = Instant::now();
            let _ = backend.next_frame(clock.as_ref()).unwrap();
            longest = longest.max(pass.elapsed());
            std::thread::sleep(Duration::from_millis(4));
        }
        // The file is costly indeed: else the test would show nothing.
        let drawing = Duration::from_nanos(backend.render_times().max_nanos());
        assert!(
            drawing >= Duration::from_millis(50),
            "drawing took only {drawing:?}"
        );
        assert!(
            longest < Duration::from_millis(50),
            "a pass took {longest:?} while drawing took up to {drawing:?}"
        );
    }

    /// An EDL of mpv with one entry (`%length%name` takes any character).
    fn edl(entry: &str) -> String {
        format!("# mpv EDL v0\n%{}%{entry}\n", entry.len())
    }

    /// What a subtitle file that is not one could hold, with `address` as
    /// the place it points at.
    fn hostile_subtitles(kind: &str, address: &str) -> String {
        match kind {
            "edl" => edl(address),
            "m3u" => format!("#EXTM3U\n{address}\n"),
            // What a script format could try: a font and a picture by
            // address.
            _ => format!(
                "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\n\
                 Format: Name, Fontname, Fontsize\nStyle: Default,{address},20\n\n\
                 [Events]\nFormat: Layer, Start, End, Style, Text\n\
                 Dialogue: 0,0:00:00.00,0:00:03.00,Default,{{\\1img({address})}}TAKT-ASS\n"
            ),
        }
    }

    /// Whether mpv connects to a local port when the film has a subtitle
    /// file of `kind` next to it that points there.
    fn subtitles_reach_the_network(kind: &str, references: bool) -> bool {
        connects_during(|port| {
            let address = format!("http://127.0.0.1:{port}/other.srt");
            let folder = Folder::new(&format!("sub-net-{kind}-{references}"));
            let name = if kind == "ass" {
                "film.ass"
            } else {
                "film.srt"
            };
            folder.write(name, &hostile_subtitles(kind, &address));
            let _ = external_subtitles(&folder, references);
        })
    }

    #[test]
    fn a_subtitle_file_leads_mpv_to_no_other_file_and_not_into_the_network() {
        let shows = |texts: &[String], word: &str| texts.iter().any(|t| t.contains(word));

        // A subtitle file next to the film is found, and its text is what
        // mpv shows. Formatting tags are allowed; they are not text.
        let plain = Folder::new("sub-plain");
        plain.write("film.srt", &srt("<i>TAKT-PLAIN</i>"));
        let (tracks, texts) = external_subtitles(&plain, false);
        assert_eq!(tracks, 1, "the file next to the film is a track");
        assert!(shows(&texts, "TAKT-PLAIN"), "{texts:?}");
        assert!(!shows(&texts, "<i>"), "{texts:?}");

        // A subtitle file that is a list of other files (an EDL of mpv)
        // does not make mpv read them: it is no track, and the text of the
        // other file is never shown.
        let folder = Folder::new("sub-file-edl");
        let other = folder.write("other.txt", &srt("TAKT-CANARY"));
        folder.write("film.srt", &edl(&other));
        let (tracks, texts) = external_subtitles(&folder, false);
        assert_eq!(tracks, 0, "{texts:?}");
        assert!(!shows(&texts, "TAKT-CANARY"), "{texts:?}");
        // The control: with the references mpv allows by default, the same
        // file does show the text of the other one. `access-references=no`
        // is what keeps it out, and this test would notice its loss.
        let (_, texts) = external_subtitles(&folder, true);
        assert!(shows(&texts, "TAKT-CANARY"), "the control: {texts:?}");

        // And it is the track mpv shows without being asked, although this
        // film has two subtitle tracks of its own.
        let mut chosen = backend(Sources::Files);
        chosen.open(&plain.film()).unwrap();
        assert_eq!(
            chosen.mpv.flag(c"current-tracks/sub/external"),
            Some(true),
            "sid {:?}",
            chosen.mpv.string(c"sid")
        );
        drop(chosen);

        // The same rule for a subtitle file dropped on the window.
        let dropped = Folder::new("sub-dropped");
        let good = dropped.write("later.srt", &srt("{\\an8}TAKT-DROPPED"));
        let other = dropped.write("other.txt", &srt("TAKT-CANARY"));
        let hostile = dropped.write("hostile.srt", &edl(&other));
        let mut backend = self::backend(Sources::Files);
        // Nothing plays: nothing to add it to.
        assert!(backend.add_subtitles(&good).is_err());
        backend.open(&dropped.film()).unwrap();
        let before = backend.mpv.int(c"track-list/count");
        assert!(
            backend.add_subtitles(&hostile).is_err(),
            "a list is no subtitles"
        );
        assert!(backend.add_subtitles("takt-no-such-file.srt").is_err());
        assert!(
            backend.add_subtitles("http://127.0.0.1:9/a.srt").is_err(),
            "an address is no file"
        );
        assert_eq!(
            backend.mpv.int(c"track-list/count"),
            before,
            "none was added"
        );
        let label = backend.add_subtitles(&good).unwrap();
        assert!(label.starts_with("subtitles: "), "{label}");
        assert_eq!(
            backend.mpv.int(c"track-list/count"),
            before.map(|n| n + 1),
            "the dropped file is a track, and the one that shows"
        );
        assert_eq!(backend.mpv.flag(c"current-tracks/sub/external"), Some(true));
        let clock = backend.clock();
        let started = Instant::now();
        let mut texts = Vec::new();
        while started.elapsed() < Duration::from_millis(800) {
            let _ = backend.next_frame(clock.as_ref());
            texts.extend(backend.mpv.string(c"sub-text").filter(|t| !t.is_empty()));
            std::thread::sleep(Duration::from_millis(4));
        }
        assert!(shows(&texts, "TAKT-DROPPED"), "{texts:?}");
        assert!(
            !shows(&texts, "an8") && !shows(&texts, "TAKT-CANARY"),
            "{texts:?}"
        );

        // And it does not make mpv connect anywhere, whatever it holds.
        for kind in ["edl", "m3u", "ass"] {
            assert!(
                !subtitles_reach_the_network(kind, false),
                "{kind}: mpv connected"
            );
        }
        // The control again: with references allowed the EDL does connect,
        // so the listener would have seen it above.
        assert!(subtitles_reach_the_network("edl", true), "the control");
    }

    #[test]
    fn a_plain_file_is_named_by_its_whole_path_and_exists() {
        assert!(is_plain_file(&clip()));
        assert!(!is_plain_file("pattern-3s.mkv"), "not a whole path");
        assert!(!is_plain_file(env!("CARGO_MANIFEST_DIR")), "a folder");
        assert!(!is_plain_file("http://127.0.0.1/film.mkv"));
        assert!(!is_plain_file(REFUSED));
        assert!(!is_plain_file(""));
    }

    #[test]
    fn a_playlist_file_is_refused_and_mpv_does_not_play_on_behind_it() {
        let list = std::env::temp_dir().join(format!("takt-test-{}.m3u", std::process::id()));
        std::fs::write(&list, format!("#EXTM3U\n{}\n", clip())).unwrap();
        let mut backend = backend(Sources::Files);
        let opened = backend.open(&list.to_string_lossy());
        let _ = std::fs::remove_file(&list);
        assert!(
            matches!(opened, Err(Error::SourceUnavailable(_))),
            "{opened:?}"
        );
        // mpv would go on to the entry of the list; it was told to stop.
        std::thread::sleep(Duration::from_millis(300));
        let clock = backend.clock();
        assert!(backend.next_frame(clock.as_ref()).unwrap().is_none());
        assert_eq!(backend.mpv.string(c"path"), None, "nothing plays");
        assert_eq!(backend.mpv.int(c"playlist-count"), Some(0));
    }

    #[test]
    fn a_source_mpv_cannot_open_is_unavailable() {
        let mut backend = backend(Sources::Any);
        let error = backend.open("takt-no-such-file.mkv").unwrap_err();
        assert_eq!(
            error,
            Error::SourceUnavailable("mpv cannot open it".to_owned())
        );
        // The backend is usable afterwards.
        assert!(backend.open(PATTERN).is_ok());
    }

    #[test]
    fn opening_never_waits_and_a_second_ask_replaces_the_first() {
        // A port that takes a connection and never answers: mpv waits for
        // it as it would for a file on a drive that sleeps.
        let slow = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = slow.local_addr().unwrap().port();
        let mut backend = backend(Sources::Any);
        let clock = backend.clock();

        let asked = Instant::now();
        backend
            .begin_open(&format!("http://127.0.0.1:{port}/slow.mkv"))
            .unwrap();
        assert!(
            asked.elapsed() < Duration::from_millis(500),
            "asking returned at once: {:?}",
            asked.elapsed()
        );
        // Passes as the window makes them: no answer yet, no frame, and no
        // pass is held up.
        let mut longest = Duration::ZERO;
        while asked.elapsed() < Duration::from_millis(400) {
            let pass = Instant::now();
            assert!(backend.poll_open().is_none(), "still opening");
            assert!(backend.next_frame(clock.as_ref()).unwrap().is_none());
            longest = longest.max(pass.elapsed());
            std::thread::sleep(Duration::from_millis(4));
        }
        assert!(longest < Duration::from_millis(100), "{longest:?}");

        // The next entry is asked for while the first still opens: its
        // answer is the one that comes, and it comes long before the first
        // would have given up.
        let asked = Instant::now();
        backend.begin_open(&clip()).unwrap();
        let info = loop {
            if let Some(ended) = backend.poll_open() {
                break ended.unwrap();
            }
            assert!(asked.elapsed() < Duration::from_secs(5), "never opened");
            std::thread::sleep(Duration::from_millis(4));
        };
        assert_eq!(info.video.map(|v| (v.width, v.height)), Some((160, 90)));
        assert!(backend.poll_open().is_none(), "one answer");
        let mut frames = 0;
        passes(&mut backend, Duration::from_secs(2), |_, _| {
            frames += 1;
            frames < 3
        });
        assert_eq!(frames, 3, "and it plays");
        drop(slow);
    }

    /// A source that never answers is given up after the time limit: the
    /// reason comes once, mpv is told to stop, and the backend opens the
    /// next file. The limit is ten seconds; the test shortens it.
    #[test]
    fn a_source_that_never_answers_is_given_up_after_the_time_limit() {
        assert_eq!(OPEN_TIMEOUT, Duration::from_secs(10));
        let slow = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = slow.local_addr().unwrap().port();
        let mut backend = backend(Sources::Any);
        assert_eq!(backend.open_timeout, OPEN_TIMEOUT);
        backend.open_timeout = Duration::from_millis(300);

        let asked = Instant::now();
        backend
            .begin_open(&format!("http://127.0.0.1:{port}/slow.mkv"))
            .unwrap();
        let ended = loop {
            if let Some(ended) = backend.poll_open() {
                break ended;
            }
            assert!(asked.elapsed() < Duration::from_secs(5), "never given up");
            std::thread::sleep(Duration::from_millis(4));
        };
        assert!(
            asked.elapsed() >= Duration::from_millis(300),
            "not before the limit: {:?}",
            asked.elapsed()
        );
        let error = ended.unwrap_err();
        assert!(matches!(error, Error::SourceUnavailable(_)), "{error}");
        assert!(
            error.to_string().contains("not open after 300ms"),
            "{error}"
        );
        assert!(backend.poll_open().is_none(), "the reason comes once");
        assert!(backend.is_eof() && !backend.loaded);

        // mpv gave the source up as well: it is idle, with nothing to play.
        let started = Instant::now();
        while backend.mpv.flag(c"idle-active") != Some(true) {
            assert!(started.elapsed() < Duration::from_secs(5), "mpv goes on");
            let _ = backend.poll_open();
            std::thread::sleep(Duration::from_millis(4));
        }
        assert_eq!(backend.mpv.int(c"playlist-count"), Some(0));

        // The blocking open has the same limit.
        let asked = Instant::now();
        assert!(
            backend
                .open(&format!("http://127.0.0.1:{port}/slow.mkv"))
                .is_err()
        );
        assert!(asked.elapsed() < Duration::from_secs(5));

        // And the next file plays, with the limit of the player again: the
        // 300 ms above are for the source that never answers, and a loaded
        // machine opens the test file slower than that (measured: 15 of 80
        // parallel runs gave "not open after 300ms" here).
        backend.open_timeout = OPEN_TIMEOUT;
        backend.open(&clip()).unwrap();
        let mut frames = 0;
        passes(&mut backend, Duration::from_secs(2), |_, _| {
            frames += 1;
            frames < 3
        });
        assert_eq!(frames, 3);
        drop(slow);
    }

    /// A file opened while paused shows one picture, its first, and that
    /// picture may wait at mpv together with the news that the file is
    /// loaded. It must not be taken for a picture of the file before and
    /// thrown away: nothing would follow it.
    #[test]
    fn the_first_picture_of_a_file_opened_while_paused_is_shown() {
        for round in 0..5 {
            let mut backend = backend(Sources::Files);
            backend.set_paused(true).unwrap();
            backend.begin_open(&long_gop()).unwrap();
            // Long enough for mpv to load the file and decode its picture
            // before it is asked: both wait for the same pass.
            std::thread::sleep(Duration::from_millis(300));
            let clock = backend.clock();
            let (mut opened, mut picture) = (false, None);
            let started = Instant::now();
            // A pass as the window makes it: the load, then the frame.
            while picture.is_none() && started.elapsed() < Duration::from_secs(2) {
                if let Some(ended) = backend.poll_open() {
                    ended.unwrap();
                    opened = true;
                }
                picture = backend.next_frame(clock.as_ref()).unwrap();
                std::thread::sleep(Duration::from_millis(4));
            }
            assert!(opened, "round {round}");
            assert_eq!(
                backend.discarded(),
                0,
                "round {round}: a picture of the new file was counted as shown"
            );
            let picture = picture.unwrap_or_else(|| panic!("round {round}: no picture"));
            // Frame 0 of the file: its grey is 16, its time 0.
            let rows = picture.get().plane(0).unwrap();
            let at = 32 * picture.get().stride(0).unwrap() + 32 * 4;
            assert!(rows[at].abs_diff(16) <= 3, "round {round}: {}", rows[at]);
            assert_eq!(picture.get().meta().pts.ticks(), 0, "round {round}");
        }
    }

    /// Rounds of what made mpv and the player wait for each other while one
    /// thread rendered and asked: a file is loaded, a jump made, the
    /// subtitles switched and a subtitle file added, with passes in between
    /// as the window makes them. No round hangs. A call may still take the
    /// 200 ms mpv waits before it gives up on a frame nobody renders, when
    /// the next frame starts to wait while the render thread asks for the
    /// time of the last one: measured once in 300 rounds (some 9000 calls).
    ///
    /// `TAKT_STRESS_ROUNDS` sets the number of rounds (30 without it) and
    /// makes the test count those calls and allow that rate, not more: a
    /// run of its own on a quiet machine. Next to the other tests, or
    /// while something else builds, a call takes that long for want of a
    /// processor (measured: 43 in 300 rounds during a build), so there the
    /// test only checks that nothing hangs.
    #[test]
    fn rounds_of_loading_jumping_and_switching_subtitles_hold_nothing_up() {
        let asked: Option<u32> = std::env::var("TAKT_STRESS_ROUNDS")
            .ok()
            .and_then(|text| text.parse().ok());
        let rounds = asked.unwrap_or(30);
        let folder = Folder::new("stress");
        let subtitles = folder.write("extra.srt", &srt("TAKT-STRESS"));
        let mut backend = backend(Sources::Files);
        let clock = backend.clock();
        // The longest call of each kind, and how many took 150 ms or more.
        let mut longest = [
            ("open", Duration::ZERO),
            ("pass", Duration::ZERO),
            ("ask", Duration::ZERO),
        ];
        let mut stalls = 0_u32;
        let mut timed = |kind: usize, took: Duration| {
            if took > longest[kind].1 {
                longest[kind].1 = took;
            }
            if took >= Duration::from_millis(150) {
                stalls += 1;
            }
        };
        let mut frames = 0_u64;
        for round in 0..rounds {
            let asked = Instant::now();
            backend.begin_open(&folder.film()).unwrap();
            loop {
                let before = Instant::now();
                let ended = backend.poll_open();
                timed(1, before.elapsed());
                if let Some(ended) = ended {
                    ended.unwrap_or_else(|e| panic!("round {round}: {e}"));
                    break;
                }
                assert!(asked.elapsed() < Duration::from_secs(8), "round {round}");
                std::thread::sleep(Duration::from_millis(2));
            }
            timed(0, asked.elapsed());
            for step in 0..24_i64 {
                let before = Instant::now();
                match step {
                    4 => backend
                        .seek(Timestamp::from_millis(1_500), SeekMode::Keyframe)
                        .unwrap(),
                    8 => drop(backend.cycle_track(Track::Subtitle).unwrap()),
                    12 => drop(backend.add_subtitles(&subtitles).unwrap()),
                    16 => backend
                        .seek(Timestamp::from_millis(700), SeekMode::Exact)
                        .unwrap(),
                    20 => drop(backend.tracks(Track::Subtitle)),
                    _ => {}
                }
                if step % 4 == 0 && step > 0 {
                    timed(2, before.elapsed());
                }
                let before = Instant::now();
                if backend.next_frame(clock.as_ref()).unwrap().is_some() {
                    frames += 1;
                }
                timed(1, before.elapsed());
                std::thread::sleep(Duration::from_millis(4));
            }
        }
        // For docs/reviews/phase-0.md (`--nocapture`).
        println!(
            "{rounds} rounds, {frames} frames, {stalls} calls of 150 ms or more, longest {longest:?}"
        );
        assert!(frames >= u64::from(rounds), "pictures came: {frames}");
        // No call hung: the longest one is far from the limits of mpv.
        assert!(
            longest
                .iter()
                .all(|(_, took)| *took < Duration::from_secs(5)),
            "{longest:?}"
        );
        // One in a hundred rounds would be some three times what was
        // measured.
        assert!(
            asked.is_none() || stalls <= 1 + rounds / 100,
            "{stalls} stalls, longest {longest:?}"
        );
    }

    /// A file may end before its last picture was taken (a file of a few
    /// frames, a pass that came late): the picture is handed out all the
    /// same, with the pass that learns of the end.
    #[test]
    fn the_last_picture_of_a_file_that_has_ended_is_still_handed_out() {
        let mut backend = backend(Sources::Files);
        // Five frames, a fifth of a second.
        backend.open(&colour_clip("bt709")).unwrap();
        // No pass while it plays: it has ended when the first one comes
        // (mpv is idle then).
        let started = Instant::now();
        while backend.mpv.flag(c"idle-active") != Some(true) {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "it never ended"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let clock = backend.clock();
        let picture = backend.next_frame(clock.as_ref()).unwrap();
        assert!(backend.is_eof(), "the file has ended");
        let picture = picture.expect("its last picture");
        let rows = picture.get().plane(0).unwrap();
        let at = 32 * picture.get().stride(0).unwrap() + 32 * 4;
        assert!(near([rows[at], rows[at + 1], rows[at + 2]], [56, 152, 195]));
        // One picture, once.
        assert!(backend.next_frame(clock.as_ref()).unwrap().is_none());
    }

    /// Giving a backend up while mpv opens a source that never answers
    /// does not wait for that source: the window does it in its own thread
    /// when the playlist is emptied.
    #[test]
    fn a_backend_that_opens_a_source_that_never_answers_is_dropped_at_once() {
        let slow = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = slow.local_addr().unwrap().port();
        let mut opening = backend(Sources::Any);
        opening
            .begin_open(&format!("http://127.0.0.1:{port}/slow.mkv"))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(opening.poll_open().is_none(), "still opening");
        let before = Instant::now();
        drop(opening);
        let took = before.elapsed();
        // For the task and docs/reviews/phase-0.md (`--nocapture`).
        println!("dropping a backend that opens: {took:?}");
        assert!(took < Duration::from_millis(500), "{took:?}");
        drop(slow);
    }

    /// A file of a single frame and no sound
    /// (`scripts/make-one-frame-clip.py`): mpv takes it for an image and
    /// showed it for 5 s (`image-display-duration`), so a playlist stood
    /// still there. It shows its frame and ends.
    #[test]
    fn a_file_of_a_single_frame_shows_it_and_ends() {
        let file = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/one-frame.mkv");
        // On a machine so busy that the 40 ms of the file are over before
        // the render thread has drawn it, there is no picture: the frame is
        // gone with the file (the thread learns the size of a picture only
        // after it, and a file that ended has none). That may happen to a
        // run, not to three in a row: a backend that never shows the
        // picture stays red.
        let mut shown = 0;
        for _ in 0..3 {
            let mut backend = backend(Sources::Files);
            let asked = Instant::now();
            // Its length may be gone already when the description is read:
            // the file can end while it opens.
            backend.open(file).unwrap();
            let clock = backend.clock();
            let mut pictures = Vec::new();
            passes(&mut backend, Duration::from_secs(4), |_, frame| {
                pictures.push(frame.get().plane(0).unwrap().to_vec());
                true
            });
            assert!(backend.is_eof(), "not ended after {:?}", asked.elapsed());
            assert!(
                asked.elapsed() < Duration::from_secs(2),
                "ended only after {:?}",
                asked.elapsed()
            );
            // Its picture was there to be shown, also when the end came
            // first.
            if pictures.is_empty()
                && let Some(frame) = backend.next_frame(clock.as_ref()).unwrap()
            {
                pictures.push(frame.get().plane(0).unwrap().to_vec());
            }
            // One picture. mpv may have it drawn again when it starts the
            // file: then it comes once more, the same bytes, and no more
            // often than the render thread was told.
            let redrawn = backend.shared.redrawn.load(Ordering::Relaxed);
            assert!(
                pictures.len() as u64 <= 1 + redrawn,
                "{} pictures, {redrawn} drawn again",
                pictures.len()
            );
            assert!(pictures.windows(2).all(|pair| pair[0] == pair[1]));
            // A picture that was rendered is never lost.
            let rendered = backend.stats().decoded;
            assert!(!pictures.is_empty() || rendered == 0, "{rendered} lost");
            if !pictures.is_empty() {
                shown += 1;
                break;
            }
        }
        assert_eq!(shown, 1, "its one picture, in one of three runs");
    }

    #[test]
    fn a_file_plays_pauses_jumps_and_ends() {
        let mut backend = backend(Sources::Files);
        let info = backend.open(&clip()).unwrap();
        assert_eq!(
            info.duration.map(|d| d.ticks() / 100_000),
            Some(30),
            "3.0 s"
        );
        assert_eq!(info.frame_rate, Rational::new(25, 1));
        assert!(info.seekable);
        assert_eq!(info.video.map(|v| (v.width, v.height)), Some((160, 90)));
        let clock = backend.clock();

        // Plays: frames come and the clock moves.
        let mut count = 0;
        passes(&mut backend, Duration::from_secs(2), |_, _| {
            count += 1;
            count < 5
        });
        assert_eq!(count, 5);
        assert!(backend.has_sound(), "the clock follows the sound track");

        // Paused: the clock stands.
        backend.set_paused(true).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let _ = backend.next_frame(clock.as_ref()).unwrap();
        let held = clock.now();
        std::thread::sleep(Duration::from_millis(120));
        let _ = backend.next_frame(clock.as_ref()).unwrap();
        let drift = clock.until_nanos(held).abs();
        assert!(drift < 30_000_000, "paused, but the clock moved {drift} ns");

        // A jump to 2 s: the first frame after it is the keyframe at 2 s.
        backend
            .seek(Timestamp::from_millis(2_000), SeekMode::Keyframe)
            .unwrap();
        backend.set_paused(false).unwrap();
        let mut landed = None;
        let started = Instant::now();
        while landed.is_none() && started.elapsed() < Duration::from_secs(2) {
            // A frame from before the jump may still be on its way: the
            // first one once mpv has arrived is the one that counts.
            let frame = backend.next_frame(clock.as_ref()).unwrap();
            if let Some(frame) = frame
                && !backend.seeking()
            {
                landed = Some(frame.get().meta().pts.ticks() / 1_000);
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        assert!(
            landed.is_some_and(|ms| (1_900..=2_100).contains(&ms)),
            "{landed:?}"
        );

        // The rest plays to the end.
        passes(&mut backend, Duration::from_secs(4), |_, _| true);
        assert!(backend.is_eof());
        // At most the last picture is still to come, then nothing.
        let _ = backend.next_frame(clock.as_ref()).unwrap();
        assert!(backend.next_frame(clock.as_ref()).unwrap().is_none());
        // And the same backend opens the next file.
        assert!(backend.open(&clip()).is_ok());
        assert!(!backend.is_eof());
    }

    /// The long-GOP file (`scripts/make-long-gop-clip.py`): 12 s, keyframes
    /// at 0 s, 4 s and 8 s and nowhere else, no sound.
    fn long_gop() -> String {
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/long-gop-12s.mkv").to_owned()
    }

    /// Jumps and returns where the jump landed, in milliseconds, measured
    /// as the window measures it for its overlay line "seek": the position
    /// of mpv once it has arrived.
    fn lands_at(backend: &mut MpvBackend, target: i64, mode: SeekMode) -> i64 {
        // Held, so that the position read is where the jump landed and not
        // a frame or two further on a busy machine.
        backend.set_paused(true).unwrap();
        let clock = backend.clock();
        backend.seek(Timestamp::from_millis(target), mode).unwrap();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            let _ = backend.next_frame(clock.as_ref()).unwrap();
            if !backend.seeking()
                && let Some(landed) = backend.position_millis()
            {
                // For docs/reviews/phase-0.md (`--nocapture`).
                println!("jump to {target} ms arrived after {:?}", started.elapsed());
                return landed;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        panic!("mpv did not arrive at {target} ms");
    }

    /// The picture of the long-GOP file is the number of its frame within
    /// its second: in the run, the time a frame carries is the time of the
    /// frame that is in it, not of a neighbour. On the way to a jump the
    /// frames carry the target (or are from before it); once mpv has
    /// arrived they carry their own time again.
    #[test]
    fn a_frame_carries_its_own_time_except_on_the_way_to_a_jump() {
        /// Which frame of its second the picture holds, and which one its
        /// time says it is.
        fn held_and_stamped(frame: &PooledFrame) -> (i64, i64) {
            let picture = frame.get();
            let rows = picture.plane(0).unwrap();
            let at = 32 * picture.stride(0).unwrap() + 32 * 4;
            let held = (i64::from(rows[at]) - 16 + 4) / 8;
            (held, picture.meta().pts.ticks() / 40_000 % 25)
        }

        let mut backend = backend(Sources::Files);
        backend.open(&long_gop()).unwrap();
        let mut seen = 0;
        passes(&mut backend, Duration::from_secs(3), |_, frame| {
            let (held, stamped) = held_and_stamped(frame);
            assert_eq!(held, stamped, "frame {seen} of the run");
            seen += 1;
            seen < 20
        });
        assert_eq!(seen, 20);

        // To 6.5 s: the keyframe before it is at 4 s.
        backend
            .seek(Timestamp::from_millis(6_500), SeekMode::Keyframe)
            .unwrap();
        let clock = backend.clock();
        let (mut on_the_way, mut after) = (Vec::new(), 0);
        let started = Instant::now();
        while after < 5 && started.elapsed() < Duration::from_secs(5) {
            let frame = backend.next_frame(clock.as_ref()).unwrap();
            let seeking = backend.seeking();
            if let Some(frame) = frame {
                let millis = frame.get().meta().pts.ticks() / 1_000;
                if seeking {
                    on_the_way.push(millis);
                } else {
                    let (held, stamped) = held_and_stamped(&frame);
                    assert_eq!(held, stamped, "after the jump, at {millis} ms");
                    assert!((4_000..4_600).contains(&millis), "{millis}");
                    after += 1;
                }
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        assert_eq!(after, 5, "it plays on from the keyframe");
        // On the way: the target, a frame from before the jump, or (the
        // event of the arrival came in the same pass) the keyframe itself.
        assert!(
            on_the_way
                .iter()
                .all(|&ms| ms == 6_500 || ms == 4_000 || ms < 1_200),
            "{on_the_way:?}"
        );
    }

    /// The containers and codecs next to H.264 in Matroska
    /// (`scripts/make-format-clips.py`): each opens, names its length and
    /// rate, and plays; the picture of every frame is the frame its time
    /// names (the grey is the frame number), and a jump lands on a frame.
    #[test]
    fn av1_hevc_vp9_and_mp4_play_with_the_right_frame_at_the_right_time() {
        for name in [
            "format-av1.mkv",
            "format-h264.mp4",
            "format-hevc.mp4",
            "format-vp9.webm",
        ] {
            let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
            let mut backend = backend(Sources::Files);
            let info = backend
                .open(&path)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                info.video.map(|v| (v.width, v.height)),
                Some((64, 64)),
                "{name}"
            );
            assert_eq!(info.frame_rate, Rational::new(25, 1), "{name}");
            assert_eq!(
                info.duration.map(|d| d.ticks() / 100_000),
                Some(20),
                "{name}: 2.0 s"
            );
            assert!(info.seekable, "{name}");

            let mut seen = 0;
            passes(&mut backend, Duration::from_secs(3), |_, frame| {
                let picture = frame.get();
                let rows = picture.plane(0).unwrap();
                let at = 32 * picture.stride(0).unwrap() + 32 * 4;
                let held = (i64::from(rows[at]) - 16 + 4) / 8;
                let stamped = picture.meta().pts.ticks() / 40_000 % 25;
                assert_eq!(held, stamped, "{name}: frame {seen}");
                seen += 1;
                seen < 20
            });
            assert_eq!(seen, 20, "{name}");
            // A keyframe every second: the jump lands on the one at 1 s.
            assert_eq!(
                lands_at(&mut backend, 1_500, SeekMode::Keyframe),
                1_000,
                "{name}"
            );
            assert_eq!(
                lands_at(&mut backend, 1_480, SeekMode::Exact),
                1_480,
                "{name}"
            );
        }
    }

    /// The jump of the window (`SeekMode::Keyframe`) in a file whose
    /// keyframes are four seconds apart: it lands on the keyframe before the
    /// target, so never further away than one keyframe distance, and never
    /// after the target. The exact jump lands on the frame.
    #[test]
    fn a_jump_lands_on_the_keyframe_before_its_target_within_one_gop() {
        const GOP_MILLIS: i64 = 4_000;
        let mut backend = backend(Sources::Files);
        let info = backend.open(&long_gop()).unwrap();
        assert_eq!(
            info.duration.map(|d| d.ticks() / 100_000),
            Some(120),
            "12.0 s"
        );
        passes(&mut backend, Duration::from_secs(2), |_, _| false);

        let mut measured = Vec::new();
        for target in [6_500, 1_000, 7_960, 4_000, 11_000, 3_990, 3_500, 7_900] {
            let landed = lands_at(&mut backend, target, SeekMode::Keyframe);
            measured.push((target, landed));
            let keyframe = target / GOP_MILLIS * GOP_MILLIS;
            assert!(
                (keyframe..keyframe + 80).contains(&landed),
                "to {target}: landed at {landed}, keyframe {keyframe}"
            );
            assert!(target - landed < GOP_MILLIS && landed <= target + 80);
        }
        // For docs/reviews/phase-0.md (`--nocapture`).
        println!("keyframe jumps (target, landed) in ms: {measured:?}");

        let mut exact = Vec::new();
        for target in [6_500, 1_000, 7_960] {
            let landed = lands_at(&mut backend, target, SeekMode::Exact);
            exact.push((target, landed));
            // The frame at the target or the one after it (25 fps).
            assert!(
                (target..target + 80).contains(&landed),
                "exact to {target}: landed at {landed}"
            );
        }
        println!("exact jumps (target, landed) in ms: {exact:?}");
    }

    #[test]
    fn the_tracks_of_a_file_are_listed_and_one_is_chosen_by_its_number() {
        let mut backend = backend(Sources::Files);
        assert!(backend.tracks(Track::Subtitle).is_empty(), "nothing plays");
        backend.open(&clip()).unwrap();
        passes(&mut backend, Duration::from_secs(2), |_, _| false);

        let sound = backend.tracks(Track::Audio);
        assert_eq!(sound.len(), 1);
        assert_eq!((sound[0].id, sound[0].selected), (1, true));
        let subtitles = backend.tracks(Track::Subtitle);
        let labels: Vec<&str> = subtitles.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["1 (deu)", "2 (eng)"]);
        assert!(subtitles.iter().all(|track| !track.selected));

        // The second one by its number, without stepping through the first.
        assert_eq!(
            backend.select_track(Track::Subtitle, Some(2)).unwrap(),
            "subtitles: 2 (eng)"
        );
        let chosen: Vec<bool> = backend
            .tracks(Track::Subtitle)
            .iter()
            .map(|track| track.selected)
            .collect();
        assert_eq!(chosen, [false, true]);
        // A number the file does not have changes nothing.
        assert!(backend.select_track(Track::Subtitle, Some(7)).is_err());
        assert!(backend.select_track(Track::Subtitle, Some(-1)).is_err());
        assert_eq!(backend.mpv.string(c"sid").as_deref(), Some("2"));
        // None of them.
        assert_eq!(
            backend.select_track(Track::Subtitle, None).unwrap(),
            "subtitles: off"
        );
        assert!(backend.tracks(Track::Subtitle).iter().all(|t| !t.selected));
        // The sound track off and on again.
        assert_eq!(
            backend.select_track(Track::Audio, None).unwrap(),
            "audio: off"
        );
        assert!(
            backend
                .select_track(Track::Audio, Some(1))
                .unwrap()
                .starts_with("audio: 1")
        );

        // A subtitle file that was added is a track of the list.
        let folder = Folder::new("track-list");
        let added = folder.write("extra.srt", &srt("added"));
        backend.add_subtitles(&added).unwrap();
        let subtitles = backend.tracks(Track::Subtitle);
        assert_eq!(subtitles.len(), 3);
        assert_eq!((subtitles[2].id, subtitles[2].selected), (3, true));
    }

    #[test]
    fn the_tracks_of_a_file_are_cycled_and_named() {
        let mut backend = backend(Sources::Files);
        backend.open(&clip()).unwrap();
        passes(&mut backend, Duration::from_secs(2), |_, _| false);
        // The file has two subtitle tracks, "deu" and "eng".
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(backend.cycle_track(Track::Subtitle).unwrap());
        }
        assert!(
            seen.iter().any(|label| label == "subtitles: off"),
            "{seen:?}"
        );
        assert!(seen.iter().any(|label| label.contains("deu")), "{seen:?}");
        assert!(seen.iter().any(|label| label.contains("eng")), "{seen:?}");
        assert!(backend.track_label(Track::Audio).starts_with("audio: 1"));
        backend.set_volume(0.5).unwrap();
        assert_eq!(backend.mpv.double(c"volume"), Some(50.0));
        backend.set_volume(f32::NAN).unwrap();
        assert_eq!(backend.mpv.double(c"volume"), Some(0.0));
    }

    #[test]
    fn the_clock_of_one_backend_is_one_clock() {
        // Invariant 3: the backend hands out the same clock every time, so
        // a player that swaps to it has exactly one.
        let backend = backend(Sources::Any);
        assert!(Arc::ptr_eq(&backend.clock(), &backend.clock()));
        assert_eq!(backend.clock().kind(), takt_clock::ClockKind::AudioMaster);
        assert!(backend.clock().may_drop());
    }
}
