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
//!   display size of the video, subtitles drawn in, and copied once into a
//!   pooled [`PixelFormat::Rgb0`] frame. It is never converted to NV12.
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
//! Limiting the protocols of FFmpeg (`stream-lavf-o=protocol_whitelist`)
//! does not do this: with it, mpv still connected (the test
//! `a_core_for_files_fetches_nothing_from_the_network` has the control).

// The properties of mpv are seconds as floating point; they become whole
// microseconds where they are read ([`super::micros`]) and are integers from
// there on.
#![allow(clippy::cast_precision_loss)]

use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::time::{Duration, Instant};

use takt_audio::{DurationHistogram, DurationSnapshot};
use takt_backend::{BackendStats, MediaInfo, SeekMode, VideoBackend};
use takt_clock::{AudioClockWriter, AudioMasterClock, Clock, MonotonicSource};
use takt_core::{
    ColorPrimaries, ColorRange, Error, FrameMeta, FramePool, PixelFormat, PooledFrame, Rational,
    Timestamp, TransferFunction,
};

use super::ffi::{Event, Mpv, Surface};
use super::micros;

/// The "sample rate" of the clock: its anchor counts microseconds.
const CLOCK_RATE: u32 = 1_000_000;
/// How long the clock runs on without a new anchor: six passes at 60 Hz.
/// Longer than that and the player stopped asking, so the position stands.
const MAX_EXTRAPOLATION_NANOS: u64 = 100_000_000;
/// Frames of the pool: the one being shown, the one being copied, and room.
const POOL_FRAMES: usize = 4;
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

/// The options of every core. No configuration, scripts or network helper of
/// the user, no window (`vo=libmpv` is the render API), no text of mpv in
/// the picture, and a file pulls in no other file.
const OPTIONS: [(&CStr, &CStr); 10] = [
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

/// A track to choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    /// The sound track.
    Audio,
    /// The subtitle track.
    Subtitle,
}

/// libmpv as a [`VideoBackend`].
pub struct MpvBackend {
    mpv: Mpv,
    /// The core was made for files: whatever mpv opens is checked
    /// ([`MpvBackend::on_hook`]).
    guard: bool,
    sources: Sources,
    early: Early,
    /// The lead mpv has now, in microseconds.
    early_micros: u32,
    sound_line: String,
    clock: Arc<AudioMasterClock>,
    writer: AudioClockWriter,
    time: Arc<dyn MonotonicSource>,
    surface: Option<Surface>,
    pool: Option<FramePool>,
    /// The video changed: look at its size again before the next frame.
    refit: bool,
    loaded: bool,
    eof: bool,
    /// The file has a sound track that plays (the clock follows it).
    has_sound: bool,
    stats: BackendStats,
    passes: u64,
    render: DurationHistogram,
}

fn backend_error(error: super::ffi::MpvError) -> Error {
    Error::Backend(error.to_string())
}

/// Whole microseconds as seconds with six decimals, from integers: no float
/// is formatted. Negative values are zero.
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

impl MpvBackend {
    /// A core of mpv, set up as `config` says. `time` is the monotonic clock
    /// the anchors of [`MpvBackend::clock`] are stamped with.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] when libmpv does not start or refuses an option.
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
        let mut mpv = Mpv::new(&options).map_err(backend_error)?;
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
        Ok(Self {
            mpv,
            guard,
            sources: config.sources,
            early: config.early,
            early_micros: fixed,
            sound_line,
            clock: Arc::new(clock),
            writer,
            time,
            surface: None,
            pool: None,
            refit: false,
            loaded: false,
            eof: false,
            has_sound: false,
            stats: BackendStats::default(),
            passes: 0,
            render: DurationHistogram::default(),
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

    /// What rendering a frame into memory and copying it took so far.
    #[must_use]
    pub fn render_times(&self) -> DurationSnapshot {
        self.render.snapshot()
    }

    /// Frames of the pool, for the diagnostics.
    #[must_use]
    pub const fn pool_frames(&self) -> usize {
        POOL_FRAMES
    }

    /// Whether mpv is still on its way to the target of the last jump: a
    /// frame that comes meanwhile may be one from before it.
    pub fn seeking(&mut self) -> bool {
        self.mpv.flag(c"seeking").unwrap_or(false)
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

    /// Waits until mpv has opened the file it was just given.
    fn wait_loaded(&mut self) -> takt_core::Result<()> {
        let asked = Instant::now();
        // What comes before the start of the new file belongs to the old one
        // (its end, when a file replaces a playing one).
        let mut started = false;
        loop {
            match self.mpv.poll() {
                Event::StartFile => started = true,
                Event::FileLoaded if started => return Ok(()),
                Event::EndFile { .. } if started => {
                    return Err(Error::SourceUnavailable("mpv cannot open it".to_owned()));
                }
                Event::Shutdown => return Err(Error::Backend("mpv shut down".to_owned())),
                Event::Hook { id } => self.on_hook(id),
                Event::None => {
                    if asked.elapsed() > OPEN_TIMEOUT {
                        return Err(Error::SourceUnavailable(format!(
                            "not open after {OPEN_TIMEOUT:?}"
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Event::FileLoaded | Event::EndFile { .. } | Event::VideoReconfig | Event::Other => {
                }
            }
        }
    }

    /// The size of the picture: the display size once a frame is decoded
    /// (it holds the aspect of anamorphic video), the size of the container
    /// before that. `None` without video.
    fn picture_size(&mut self) -> Option<(u32, u32)> {
        let mut side = |name: &CStr| {
            self.mpv
                .int(name)
                .and_then(|v| u32::try_from(v).ok())
                .filter(|&v| v > 0)
        };
        match (side(c"dwidth"), side(c"dheight")) {
            (Some(w), Some(h)) => Some((w, h)),
            _ => side(c"width").zip(side(c"height")),
        }
    }

    fn meta(size: (u32, u32)) -> FrameMeta {
        FrameMeta {
            width: size.0,
            height: size.1,
            format: PixelFormat::Rgb0,
            // mpv applies the matrix of the file and leaves primaries and
            // transfer as they are: R'G'B' in full range. The primaries of
            // the file are not asked for yet (colour management, Phase 1).
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt709,
            range: ColorRange::Full,
            pts: Timestamp::new(0, Rational::MICROS),
        }
    }

    /// Makes surface and pool fit the picture; both gone without video.
    fn fit(&mut self) -> takt_core::Result<()> {
        self.refit = false;
        let Some(size) = self.picture_size() else {
            self.surface = None;
            self.pool = None;
            return Ok(());
        };
        if self.surface.as_ref().map(Surface::size) == Some(size) {
            return Ok(());
        }
        // A size beyond 8K is refused here, before anything is allocated.
        self.surface = Some(Surface::new(size.0, size.1).ok_or(Error::InvalidGeometry {
            width: size.0,
            height: size.1,
        })?);
        self.pool = Some(FramePool::new(Self::meta(size), POOL_FRAMES));
        Ok(())
    }

    /// Takes what mpv reported since the last pass.
    fn pump(&mut self) {
        loop {
            match self.mpv.poll() {
                Event::None => break,
                Event::EndFile { .. } | Event::Shutdown => {
                    self.eof = true;
                    self.loaded = false;
                }
                Event::VideoReconfig => self.refit = true,
                Event::Hook { id } => self.on_hook(id),
                Event::StartFile | Event::FileLoaded | Event::Other => {}
            }
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

    /// Reads the drop counters of mpv now (they are read every few passes
    /// otherwise): for the last line of a measuring run.
    pub fn refresh_stats(&mut self) -> BackendStats {
        self.read_counters();
        self.stats
    }
}

impl VideoBackend for MpvBackend {
    fn name(&self) -> &'static str {
        "mpv-sw"
    }

    /// Blocks until mpv has opened the source (a local file: milliseconds),
    /// at most ten seconds.
    fn open(&mut self, source: &str) -> takt_core::Result<MediaInfo> {
        let source = self.checked(source)?;
        self.loaded = false;
        self.eof = true;
        self.stats = BackendStats::default();
        self.mpv.load(&source).map_err(backend_error)?;
        if let Err(error) = self.wait_loaded() {
            // What did not open must not go on behind the back of the
            // player: a playlist file "ends" by handing its entries to mpv,
            // which would play them without a picture here.
            let _ = self.mpv.command("stop", &["stop"]);
            return Err(error);
        }
        self.loaded = true;
        self.eof = false;
        self.fit()?;
        self.publish_position();

        let duration = self
            .mpv
            .double(c"duration")
            .and_then(micros)
            .filter(|&d| d > 0)
            .map(|d| Timestamp::new(d, Rational::MICROS));
        Ok(MediaInfo {
            duration,
            frame_rate: self
                .mpv
                .double(c"container-fps")
                .and_then(micros)
                .and_then(rate_of),
            video: self.surface.as_ref().map(|s| Self::meta(s.size())),
            sample_rate: self
                .mpv
                .int(c"audio-params/samplerate")
                .and_then(|rate| u32::try_from(rate).ok()),
            seekable: self.mpv.flag(c"seekable").unwrap_or(false),
        })
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
            .map_err(backend_error)
    }

    /// The clock is not asked: mpv decides when a frame is due (module
    /// documentation). Each call is one pass of the display: it publishes the
    /// position of mpv to [`MpvBackend::clock`].
    fn next_frame(&mut self, _clock: &dyn Clock) -> takt_core::Result<Option<PooledFrame>> {
        self.pump();
        self.publish_position();
        self.passes += 1;
        if self.passes.is_multiple_of(COUNTER_PASSES) {
            self.read_counters();
        }
        if !self.loaded {
            return Ok(None);
        }
        if self.refit {
            self.fit()?;
        }
        if !self.mpv.frame_ready() {
            return Ok(None);
        }
        let (Some(surface), Some(pool)) = (self.surface.as_mut(), self.pool.as_mut()) else {
            return Ok(None);
        };

        let before = Instant::now();
        self.mpv.render(surface).map_err(backend_error)?;
        // The position of the frame just rendered; without one (the very
        // first frame of some files) the clock itself, an error of zero.
        let pts = self
            .mpv
            .double(c"time-pos")
            .and_then(micros)
            .map_or_else(|| self.clock.now(), |t| Timestamp::new(t, Rational::MICROS));
        let mut frame = pool.acquire(pts).ok_or(Error::PoolExhausted {
            capacity: pool.capacity(),
        })?;
        let (width, height) = surface.size();
        let geometry = Error::InvalidGeometry { width, height };
        let target = frame.get_mut();
        let to_stride = target.stride(0).ok_or(geometry.clone())?;
        let from_stride = surface.stride();
        let row = to_stride.min(from_stride);
        let plane = target.plane_mut(0).ok_or(geometry)?;
        // One copy, row by row: the surface is padded to 64 bytes a row for
        // mpv, the frame is tight.
        for (to, from) in plane
            .chunks_mut(to_stride)
            .zip(surface.pixels().chunks(from_stride))
        {
            let bytes = row.min(to.len()).min(from.len());
            to[..bytes].copy_from_slice(&from[..bytes]);
        }
        self.render
            .record(u64::try_from(before.elapsed().as_nanos()).unwrap_or(u64::MAX));
        self.stats.decoded += 1;
        self.stats.queued = pool.in_flight();
        Ok(Some(frame))
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
        self.stats
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
        let mut errors = Vec::new();
        passes(&mut backend, Duration::from_secs(2), |_, frame| {
            let meta = *frame.get().meta();
            assert_eq!(meta.format, PixelFormat::Rgb0);
            assert_eq!((meta.width, meta.height), (320, 240));
            assert_eq!(frame.get().stride(0), Some(320 * 4));
            assert!(
                frame.get().plane(0).unwrap().iter().any(|&b| b != 0),
                "a picture reached the frame"
            );
            stamps.push(meta.pts);
            errors.push(clock.until_nanos(meta.pts));
            true
        });
        // 25 frames a second for two seconds; a loaded machine shows fewer.
        assert!((10..=51).contains(&stamps.len()), "{}", stamps.len());
        assert!(
            stamps.windows(2).all(|pair| pair[0] < pair[1]),
            "{stamps:?}"
        );
        // The clock follows mpv: every frame within a tenth of a second.
        assert!(errors.iter().all(|e| e.abs() < 100_000_000), "{errors:?}");
        assert_eq!(backend.stats().decoded, stamps.len() as u64);
        assert_eq!(backend.render_times().total(), stamps.len() as u64);
        assert_eq!(
            backend.stats().queued,
            1,
            "the frames went back to the pool"
        );
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
            })
        };
        let _ = backend.open(&format!("http://127.0.0.1:{port}/film.mkv"));
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
        assert!(backend.next_frame(clock.as_ref()).unwrap().is_none());
        // And the same backend opens the next file.
        assert!(backend.open(&clip()).is_ok());
        assert!(!backend.is_eof());
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
