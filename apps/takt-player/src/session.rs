// SPDX-License-Identifier: GPL-3.0-or-later
//! What plays, and the one clock it plays on.
//!
//! A [`Session`] is either the test picture or a file:
//!
//! * **The test picture** ([`NullBackend`]): the sound path of the player
//!   (`takt-audio`, cpal) plays silence on the output device, and that device
//!   drives the audio master clock, wrapped in a [`SeekableClock`].
//! * **A file** (`MpvBackend`, in a build that links libmpv): mpv plays the
//!   sound on its own device, and the audio master clock follows the sound
//!   position of mpv.
//! * **A file through the own pipeline** (`takt_decode::SoftwareBackend`):
//!   `takt-demux` and `takt-decode` in their helper processes deliver the
//!   picture. The pipeline decodes no sound yet, so the sound path of the
//!   player plays silence and drives the clock, as for the test picture.
//!
//! Which of the two plays files is the [`Pipeline`], and it can be changed
//! while a file plays ([`Session::switch_pipeline`]): the file is opened
//! again by the other one at the position it was at. libmpv stays the
//! reference the own pipeline is compared with.
//!
//! Invariant 3 of `docs/THREADS-UND-UHREN.md` (exactly one clock is active)
//! is the shape of this type: the two cases are the variants of one enum, so
//! a session holds one clock and one sound path, never both. Going from one
//! to the other replaces the variant; the old clock and its device stop when
//! it is dropped. Everything else in the player reads the clock through
//! [`Session::clock`] and cannot keep the old one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use takt_audio::OutputStats;
use takt_backend::{BackendStats, NullBackend, Pattern, SeekMode, VideoBackend};
use takt_clock::{AudioMasterClock, Clock, SeekableClock, SystemMonotonic};
use takt_core::{
    ColorPrimaries, ColorRange, FrameMeta, PixelFormat, PooledFrame, Rational, Timestamp,
    TransferFunction,
};

use takt_decode::{BackendOptions, Layout, SoftwareBackend};

use crate::audio::{self, AudioPath};
#[cfg(takt_libmpv)]
use crate::mpv::{Config, Early, MpvBackend, Sound, Sources, Track};

/// What plays files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pipeline {
    /// libmpv (`MpvBackend`): picture, sound and subtitles. In a build
    /// without libmpv no file plays through it.
    Mpv,
    /// The own pipeline (`takt-demux`, `takt-decode`): the picture, decoded
    /// in software by helper processes. No sound and no subtitles yet.
    Own,
}

impl Pipeline {
    /// The pipeline a session starts with: libmpv, or the own one when the
    /// environment says `TAKT_PIPELINE=own`.
    fn from_env() -> Self {
        match std::env::var("TAKT_PIPELINE") {
            Ok(name) if name.eq_ignore_ascii_case("own") => Self::Own,
            _ => Self::Mpv,
        }
    }

    /// The other one.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Mpv => Self::Own,
            Self::Own => Self::Mpv,
        }
    }
}

/// Where the helper programs of the own pipeline lie: `TAKT_HELPER_DIR`,
/// else next to the player, which is where an install puts them.
///
/// A debug build also looks in the directories above its own, a few levels
/// up: cargo puts a test program below the directory the helpers are built
/// into (`target/debug/deps`, or `target/debug/build/<package>/<hash>/out`
/// with a newer cargo). A release build never does: what it starts lies next
/// to it or where the person said.
fn helper_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("TAKT_HELPER_DIR").filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let exe = std::env::current_exe().map_err(|e| format!("own pipeline: {e}"))?;
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let helper = format!(
        "{}{}",
        takt_decode::HELPER_NAME,
        std::env::consts::EXE_SUFFIX
    );
    let levels = if cfg!(debug_assertions) { 5 } else { 1 };
    Ok(dir
        .ancestors()
        .take(levels)
        .find(|dir| dir.join(&helper).is_file())
        .unwrap_or(dir)
        .to_path_buf())
}

/// How the own pipeline plays in this window: the helpers of `helpers` or,
/// without one, of [`helper_dir`]; frames as NV12 and P010 (what the texture
/// paths and a hardware decoder use).
fn own_options(helpers: Option<&Path>) -> Result<BackendOptions, String> {
    let dir = match helpers {
        Some(dir) => dir.to_path_buf(),
        None => helper_dir()?,
    };
    let mut options = BackendOptions::in_dir(&dir);
    options.decode.layout = Layout::SemiPlanar;
    Ok(options)
}

/// Length of the test picture.
const TEST_MILLIS: i64 = 3_600_000;
/// The most subtitle files kept for a film that is still being opened.
#[cfg(takt_libmpv)]
const WANTED_SUBTITLES: usize = 8;

/// Frames of the pool of the test picture: the one the renderer holds, the
/// one being painted, and room.
const TEST_POOL_FRAMES: usize = 6;

/// A track the person can step through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    /// The sound track.
    Audio,
    /// The subtitle track.
    Subtitle,
}

/// A track of what plays, for a list to choose from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackChoice {
    /// What [`Session::select_track`] takes.
    pub id: i64,
    /// Its number, language and title, as far as the file names them.
    pub label: String,
    /// It is the one that plays.
    pub selected: bool,
}

/// The test picture: the moving block of `NullBackend`, 1920x1080 NV12 at
/// the NTSC rate, one hour long.
fn test_source() -> NullBackend {
    let meta = FrameMeta {
        width: 1920,
        height: 1080,
        format: PixelFormat::Nv12,
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Bt709,
        range: ColorRange::Limited,
        pts: Timestamp::from_millis(0),
    };
    let mut backend = NullBackend::new(
        meta,
        Rational::lit(24_000, 1_001),
        Timestamp::from_millis(TEST_MILLIS),
        Pattern::Motion,
        TEST_POOL_FRAMES,
    );
    // NullBackend opens any name.
    let _ = backend.open("null://motion");
    backend
}

struct Test {
    clock: SeekableClock<AudioMasterClock>,
    // Kept for its life; dropping it stops the sound.
    audio: AudioPath,
    audio_line: String,
    source: NullBackend,
}

#[cfg(takt_libmpv)]
struct File {
    source: MpvBackend,
    clock: Arc<AudioMasterClock>,
    duration_millis: i64,
    /// Size of the picture, for the diagnostics.
    size: Option<(u32, u32)>,
    /// What the file says about its colour, for the diagnostics. Read when
    /// the file opens, and again with its first frame when mpv did not
    /// know it by then.
    color: Option<takt_core::ColorDescription>,
    /// [`Session::take_limit_note`] has said what there is to say.
    limit_told: bool,
}

/// A file played by the own pipeline, on the sound path of the player.
struct Own {
    clock: SeekableClock<AudioMasterClock>,
    // Kept for its life; dropping it stops the (silent) sound.
    audio: AudioPath,
    audio_line: String,
    source: SoftwareBackend,
    info: takt_backend::MediaInfo,
    /// [`Session::take_limit_note`] has said what there is to say.
    limit_told: bool,
}

enum Kind {
    Test(Box<Test>),
    #[cfg(takt_libmpv)]
    File(Box<File>),
    Own(Box<Own>),
}

/// A file the own pipeline is opening on a thread of its own: both helper
/// processes start and the container is read before the first picture.
type OwnOpening = Receiver<Result<(SoftwareBackend, takt_backend::MediaInfo), String>>;

/// A file mpv has not opened yet.
#[cfg(takt_libmpv)]
enum Pending {
    /// By a backend of its own, while the test picture plays on its clock:
    /// the clocks are swapped when the file is open, not before.
    Fresh(Box<MpvBackend>),
    /// By the backend of the file that played; its clock stays.
    Same,
}

/// What plays now, with its clock.
pub struct Session {
    time: Arc<SystemMonotonic>,
    kind: Kind,
    #[cfg(takt_libmpv)]
    pending: Option<Pending>,
    /// Subtitle files asked for while their film was still being opened:
    /// shown once it is open, forgotten when it does not open.
    #[cfg(takt_libmpv)]
    wanted_subtitles: Vec<std::path::PathBuf>,
    /// How the last open ended, where it ends at once (a build without
    /// libmpv), until [`Session::poll_open`] takes it.
    #[cfg(not(takt_libmpv))]
    finished: Option<Result<(), String>>,
    /// What plays files.
    pipeline: Pipeline,
    /// The file asked for last: it plays or is being opened.
    current: Option<PathBuf>,
    /// The file that plays.
    playing: Option<PathBuf>,
    /// A file the own pipeline has not opened yet.
    own_pending: Option<OwnOpening>,
    /// Where the file that is being opened goes on: the position of the
    /// file the other pipeline played ([`Session::switch_pipeline`]).
    resume_millis: Option<i64>,
    /// Where the helpers of the own pipeline are looked for instead of
    /// [`helper_dir`]; set by tests.
    helpers: Option<PathBuf>,
    /// No output device, neither of the player nor of mpv (tests).
    silent: bool,
    paused: bool,
    volume: f32,
}

impl Session {
    /// The test picture on the sound path of the player.
    #[must_use]
    pub fn test(time: Arc<SystemMonotonic>) -> Self {
        Self::start(time, false)
    }

    /// The same without any output device: the sound path is simulated and
    /// a file plays silently. For tests.
    #[cfg(test)]
    #[must_use]
    pub fn silent(time: Arc<SystemMonotonic>) -> Self {
        Self::start(time, true)
    }

    fn start(time: Arc<SystemMonotonic>, silent: bool) -> Self {
        let kind = Self::test_kind(&time, silent);
        Self {
            time,
            kind,
            #[cfg(takt_libmpv)]
            pending: None,
            #[cfg(takt_libmpv)]
            wanted_subtitles: Vec::new(),
            #[cfg(not(takt_libmpv))]
            finished: None,
            pipeline: Pipeline::from_env(),
            current: None,
            playing: None,
            own_pending: None,
            resume_millis: None,
            helpers: None,
            silent,
            paused: false,
            volume: 1.0,
        }
    }

    /// What plays files.
    #[must_use]
    pub const fn pipeline(&self) -> Pipeline {
        self.pipeline
    }

    /// The file that plays or is being opened.
    #[must_use]
    pub fn current(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    /// Files play through `to` from now on. A file that plays is opened
    /// again by that pipeline and goes on where it was, once it is open
    /// ([`Session::poll_open`]); until then it plays on as it did. A file
    /// that is still being opened is opened by that pipeline instead. Says
    /// in a line what plays files now.
    ///
    /// # Errors
    ///
    /// A text for the user when `to` cannot play here (a build without
    /// libmpv, helpers that are not found) or the file is refused; the
    /// pipeline stays the one it was.
    pub fn switch_pipeline(&mut self, to: Pipeline) -> Result<String, String> {
        if to == Pipeline::Mpv && !cfg!(takt_libmpv) {
            return Err("this build has no libmpv: files play through the own pipeline".to_owned());
        }
        let line = match to {
            Pipeline::Mpv => "pipeline: libmpv",
            Pipeline::Own => "pipeline: own (takt-demux, takt-decode): picture only, no sound yet",
        };
        if to == self.pipeline {
            return Ok(line.to_owned());
        }
        let before = self.pipeline;
        self.pipeline = to;
        // The file on its way has no position yet; the one that plays goes
        // on where it is.
        let again = if self.opening() {
            self.current.clone().map(|path| (path, None))
        } else if self.is_file() {
            let position = self.now_millis();
            self.playing.clone().map(|path| (path, Some(position)))
        } else {
            None
        };
        if let Some((path, position)) = again {
            if let Err(e) = self.open_file(&path) {
                self.pipeline = before;
                return Err(e);
            }
            self.resume_millis = position;
        }
        Ok(line.to_owned())
    }

    /// Begins to open `path` with the own pipeline, on a thread.
    fn open_own(&mut self, path: &Path) -> Result<(), String> {
        if !path.is_file() {
            return Err("no such file".to_owned());
        }
        let options = own_options(self.helpers.as_deref())?;
        let name = path.to_string_lossy().into_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("takt-open".to_owned())
            .spawn(move || {
                let mut backend = SoftwareBackend::new(options);
                let opened = backend
                    .open(&name)
                    .map(|info| (backend, info))
                    .map_err(|e| e.to_string());
                // Nobody waits any more: the backend ends here, and its helpers with it.
                let _ = tx.send(opened);
            })
            .map_err(|e| format!("own pipeline: {e}"))?;
        self.own_pending = Some(rx);
        // A file libmpv was opening is given up for this one.
        #[cfg(takt_libmpv)]
        {
            self.pending = None;
            self.wanted_subtitles.clear();
        }
        Ok(())
    }

    /// How the open of the own pipeline ended, if it has.
    fn poll_own(&mut self) -> Option<Result<(), String>> {
        let opened = match self.own_pending.as_ref()?.try_recv() {
            Ok(opened) => opened,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err("own pipeline: the open ended".to_owned()),
        };
        self.own_pending = None;
        Some(opened.map(|(source, info)| {
            // The picture plays on the sound path of the player, which plays
            // silence: the pipeline decodes no sound yet.
            let (clock, audio) = if self.silent {
                audio::simulated(&self.time, "no device asked for")
            } else {
                audio::start(&self.time)
            };
            self.kind = Kind::Own(Box::new(Own {
                clock: SeekableClock::new(clock),
                audio_line: audio.describe(),
                audio,
                source,
                info,
                limit_told: false,
            }));
        }))
    }

    fn test_kind(time: &Arc<SystemMonotonic>, silent: bool) -> Kind {
        let (clock, audio) = if silent {
            audio::simulated(time, "no device asked for")
        } else {
            audio::start(time)
        };
        Kind::Test(Box::new(Test {
            clock: SeekableClock::new(clock),
            audio_line: audio.describe(),
            audio,
            source: test_source(),
        }))
    }

    /// Whether a file plays (and not the test picture).
    #[must_use]
    pub fn is_file(&self) -> bool {
        !matches!(self.kind, Kind::Test(_))
    }

    /// Begins to open the file `path` and returns at once: the window stays
    /// in use while mpv opens it, and [`Session::poll_open`] says how it
    /// ended. A file that is still being opened is given up for this one. In
    /// a build without libmpv the test picture starts again in its place:
    /// the file is not read.
    ///
    /// While the test picture plays it plays on, on its clock, until the
    /// file is open. A file that plays is gone at once: mpv replaces it.
    ///
    /// # Errors
    ///
    /// A text for the user when the file is refused before mpv is asked
    /// (it does not exist); what played or was being opened goes on then.
    pub fn open_file(&mut self, path: &Path) -> Result<(), String> {
        if self.pipeline == Pipeline::Own {
            self.open_own(path)?;
            self.current = Some(path.to_path_buf());
            self.resume_millis = None;
            return Ok(());
        }
        // A file the own pipeline was opening is given up for this one.
        self.own_pending = None;
        self.resume_millis = None;
        #[cfg(takt_libmpv)]
        {
            let name = path.to_string_lossy();
            // Subtitles that waited belong to the file asked for before.
            self.wanted_subtitles.clear();
            match (&mut self.kind, &mut self.pending) {
                (Kind::File(file), _) => {
                    file.source.begin_open(&name).map_err(|e| e.to_string())?;
                    file.duration_millis = 0;
                    file.size = None;
                    file.color = None;
                    file.limit_told = false;
                    self.pending = Some(Pending::Same);
                }
                // The backend that is opening already takes the new file: it
                // is not dropped while mpv may still be busy with the old one.
                (Kind::Test(_) | Kind::Own(_), Some(Pending::Fresh(source))) => {
                    source.begin_open(&name).map_err(|e| e.to_string())?;
                }
                (Kind::Test(_) | Kind::Own(_), _) => {
                    let config = Config {
                        sources: Sources::Files,
                        sound: if self.silent {
                            Sound::Silent
                        } else {
                            Sound::from_env(Sound::Default)
                        },
                        early: Early::HalfDisplayPeriod,
                    };
                    let time = Arc::clone(&self.time) as Arc<dyn takt_clock::MonotonicSource>;
                    let mut source = MpvBackend::new(config, time).map_err(|e| e.to_string())?;
                    source.begin_open(&name).map_err(|e| e.to_string())?;
                    self.pending = Some(Pending::Fresh(Box::new(source)));
                }
            }
            self.current = Some(path.to_path_buf());
            Ok(())
        }
        #[cfg(not(takt_libmpv))]
        {
            // Without libmpv this pipeline plays no file: the test picture
            // starts again in its place.
            if matches!(self.kind, Kind::Own(_)) {
                self.kind = Self::test_kind(&self.time, self.silent);
                self.apply();
            }
            self.current = None;
            let Kind::Test(test) = &mut self.kind else {
                return Err("no test picture".to_owned());
            };
            test.source
                .open(&path.to_string_lossy())
                .map_err(|e| e.to_string())?;
            self.seek_to(0);
            self.finished = Some(Ok(()));
            Ok(())
        }
    }

    /// How the open of [`Session::open_file`] ended; asked once a pass.
    /// `None` while it is on its way (or nothing was asked), then once
    /// `Ok` when the file plays, or the text of why it does not. Never
    /// waits.
    ///
    /// On `Ok` after the test picture the clock has been swapped: the test
    /// picture, its clock and its device are dropped, and the clock of mpv
    /// is the one. On an error the test picture plays on; a file that
    /// played before is gone (mpv let it go when it was asked).
    pub fn poll_open(&mut self) -> Option<Result<(), String>> {
        let ended = self.poll_opened()?;
        match (&ended, self.resume_millis.take()) {
            // The file the other pipeline played goes on where it was, to the
            // frame: a jump to the keyframe before would set a long GOP back
            // by seconds.
            (Ok(()), Some(position)) => self.seek_with(position, SeekMode::Exact),
            (Ok(()), None) => {}
            (Err(_), _) => {}
        }
        if ended.is_ok() {
            self.playing.clone_from(&self.current);
        } else {
            // What was asked for does not play: the file is the one that
            // plays on, if one does.
            if !self.is_file() {
                self.playing = None;
            }
            self.current.clone_from(&self.playing);
        }
        Some(ended)
    }

    fn poll_opened(&mut self) -> Option<Result<(), String>> {
        if self.own_pending.is_some() {
            let ended = self.poll_own()?;
            if ended.is_ok() {
                self.apply();
            }
            return Some(ended);
        }
        #[cfg(takt_libmpv)]
        {
            let ended = match (self.pending.take()?, &mut self.kind) {
                (Pending::Same, Kind::File(file)) => match file.source.poll_open() {
                    None => {
                        self.pending = Some(Pending::Same);
                        return None;
                    }
                    Some(Ok(info)) => {
                        file.duration_millis = duration_millis(&info);
                        file.size = info.video.map(|v| (v.width, v.height));
                        file.color = info.source_color;
                        Ok(())
                    }
                    Some(Err(e)) => Err(e.to_string()),
                },
                (Pending::Fresh(mut source), _) => match source.poll_open() {
                    None => {
                        self.pending = Some(Pending::Fresh(source));
                        return None;
                    }
                    Some(Ok(info)) => {
                        self.kind = Kind::File(Box::new(File {
                            clock: source.clock(),
                            duration_millis: duration_millis(&info),
                            size: info.video.map(|v| (v.width, v.height)),
                            color: info.source_color,
                            limit_told: false,
                            source: *source,
                        }));
                        Ok(())
                    }
                    Some(Err(e)) => Err(e.to_string()),
                },
                // Nothing of a file is left to wait for.
                (Pending::Same, Kind::Test(_) | Kind::Own(_)) => return None,
            };
            let wanted = std::mem::take(&mut self.wanted_subtitles);
            if ended.is_ok() {
                self.apply();
                for file in wanted {
                    if let Err(e) = self.add_subtitles(&file) {
                        eprintln!("takt-player: subtitles {}: {e}", file.display());
                    }
                }
            }
            Some(ended)
        }
        #[cfg(not(takt_libmpv))]
        {
            self.finished.take()
        }
    }

    /// Whether a file is being opened.
    #[must_use]
    pub const fn opening(&self) -> bool {
        if self.own_pending.is_some() {
            return true;
        }
        #[cfg(takt_libmpv)]
        {
            self.pending.is_some()
        }
        #[cfg(not(takt_libmpv))]
        {
            self.finished.is_some()
        }
    }

    /// Back to the test picture (the playlist is empty). Nothing happens
    /// when it plays already.
    pub fn show_test_picture(&mut self) {
        #[cfg(takt_libmpv)]
        {
            // A file that was on its way is given up, with its subtitles.
            self.pending = None;
            self.wanted_subtitles.clear();
        }
        self.own_pending = None;
        self.resume_millis = None;
        self.current = None;
        self.playing = None;
        if self.is_file() {
            self.kind = Self::test_kind(&self.time, self.silent);
            self.apply();
        }
    }

    /// Pause and volume, as the person set them, on what plays now.
    fn apply(&mut self) {
        let (paused, volume) = (self.paused, self.volume);
        self.set_paused(paused);
        self.set_volume(volume);
    }

    /// The clock everything is timed on.
    #[must_use]
    pub fn clock(&self) -> &dyn Clock {
        match &self.kind {
            Kind::Test(test) => &test.clock,
            #[cfg(takt_libmpv)]
            Kind::File(file) => file.clock.as_ref(),
            Kind::Own(own) => &own.clock,
        }
    }

    /// The audio master clock underneath, for the diagnostics.
    #[must_use]
    pub fn master(&self) -> &AudioMasterClock {
        match &self.kind {
            Kind::Test(test) => test.clock.inner(),
            #[cfg(takt_libmpv)]
            Kind::File(file) => file.clock.as_ref(),
            Kind::Own(own) => own.clock.inner(),
        }
    }

    fn source(&self) -> &dyn VideoBackend {
        match &self.kind {
            Kind::Test(test) => &test.source,
            #[cfg(takt_libmpv)]
            Kind::File(file) => &file.source,
            Kind::Own(own) => &own.source,
        }
    }

    /// The frame that is due on the clock, if one is.
    ///
    /// # Errors
    ///
    /// What the backend reports.
    pub fn next_frame(&mut self) -> takt_core::Result<Option<PooledFrame>> {
        match &mut self.kind {
            Kind::Test(test) => test.source.next_frame(&test.clock),
            #[cfg(takt_libmpv)]
            Kind::File(file) => {
                let frame = file.source.next_frame(file.clock.as_ref())?;
                // mpv may name the colour of a file only once it has
                // decoded a frame of it.
                if frame.is_some() && file.color.is_none() {
                    file.color = file.source.source_color();
                }
                Ok(frame)
            }
            Kind::Own(own) => own.source.next_frame(&own.clock),
        }
    }

    /// Whether the source is at its end.
    #[must_use]
    pub fn is_eof(&self) -> bool {
        self.source().is_eof()
    }

    /// The counters of the source.
    #[must_use]
    pub fn stats(&self) -> BackendStats {
        self.source().stats()
    }

    /// Tells the source the measured display period.
    pub fn set_display_period(&mut self, nanos: Option<i64>) {
        match &mut self.kind {
            Kind::Test(test) => test.source.set_display_period(nanos),
            #[cfg(takt_libmpv)]
            Kind::File(file) => file.source.set_display_period(nanos),
            Kind::Own(own) => own.source.set_display_period(nanos),
        }
    }

    /// The latest position a jump may go to, in milliseconds: the length,
    /// or no limit when what plays does not say how long it is.
    #[must_use]
    pub fn seek_limit(&self) -> i64 {
        match self.duration_millis() {
            known if known > 0 => known,
            _ => i64::MAX,
        }
    }

    /// Jumps to `millis`.
    pub fn seek_to(&mut self, millis: i64) {
        self.seek_with(millis, SeekMode::Keyframe);
    }

    /// Jumps to `millis`; `mode` is how libmpv gets there (the test picture
    /// and the own pipeline are exact in either).
    fn seek_with(&mut self, millis: i64, mode: SeekMode) {
        // Only libmpv has two ways to get there.
        #[cfg(not(takt_libmpv))]
        let _ = mode;
        let to = Timestamp::from_millis(millis.clamp(0, self.seek_limit()));
        let done = match &mut self.kind {
            Kind::Test(test) => {
                test.clock.seek(to);
                test.source.seek(to, SeekMode::Keyframe)
            }
            // The clock follows mpv to the new position by itself.
            #[cfg(takt_libmpv)]
            Kind::File(file) => file.source.seek(to, mode),
            // The clock stands at the target, and the pipeline hands out
            // no picture from before it.
            Kind::Own(own) => {
                own.clock.seek(to);
                own.source.seek(to, SeekMode::Exact)
            }
        };
        if let Err(e) = done {
            eprintln!("takt-player: seek: {e}");
        }
    }

    /// Where the last jump landed, in milliseconds, once that is known;
    /// asked once a pass after a jump, with the time of the frame the pass
    /// shows, if it shows one.
    ///
    /// The test picture is there at once: the frame it shows next is where
    /// it landed. mpv jumps on its own thread, and while it is on its way
    /// its position is the target and the frames carry that
    /// (`MpvBackend::next_frame`): where it landed is its position once it
    /// has arrived, whether a frame comes in that pass or not (paused, the
    /// picture of the jump came before).
    pub fn seek_landed(&mut self, shown_millis: Option<i64>) -> Option<i64> {
        match &mut self.kind {
            Kind::Test(_) | Kind::Own(_) => shown_millis,
            #[cfg(takt_libmpv)]
            Kind::File(file) => {
                if file.source.seeking() {
                    None
                } else {
                    file.source.position_millis()
                }
            }
        }
    }

    /// Holds or releases the position (and, for a file, picture and sound).
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        match &mut self.kind {
            Kind::Test(test) => test.clock.set_paused(paused),
            #[cfg(takt_libmpv)]
            Kind::File(file) => {
                if let Err(e) = file.source.set_paused(paused) {
                    eprintln!("takt-player: pause: {e}");
                }
            }
            Kind::Own(own) => own.clock.set_paused(paused),
        }
    }

    /// Sets the volume, 0 (silent) to 1 (unchanged).
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume;
        match &mut self.kind {
            Kind::Test(test) => test.audio.set_volume(volume),
            #[cfg(takt_libmpv)]
            Kind::File(file) => {
                if let Err(e) = file.source.set_volume(volume) {
                    eprintln!("takt-player: volume: {e}");
                }
            }
            Kind::Own(own) => own.audio.set_volume(volume),
        }
    }

    /// Goes to the next track of `kind` and names the one that plays now;
    /// `None` when what plays has no tracks to choose (the test picture).
    pub fn cycle_track(&mut self, kind: TrackKind) -> Option<String> {
        match &mut self.kind {
            Kind::Test(_) => {
                let _ = kind;
                None
            }
            Kind::Own(_) => Some(NO_TRACKS.to_owned()),
            #[cfg(takt_libmpv)]
            Kind::File(file) => Some(
                file.source
                    .cycle_track(track_of(kind))
                    .unwrap_or_else(|e| e.to_string()),
            ),
        }
    }

    /// The tracks of `kind` to choose from; empty for the test picture.
    pub fn tracks(&mut self, kind: TrackKind) -> Vec<TrackChoice> {
        match &mut self.kind {
            Kind::Test(_) | Kind::Own(_) => {
                let _ = kind;
                Vec::new()
            }
            #[cfg(takt_libmpv)]
            Kind::File(file) => file
                .source
                .tracks(track_of(kind))
                .into_iter()
                .map(|track| TrackChoice {
                    id: track.id,
                    label: track.label,
                    selected: track.selected,
                })
                .collect(),
        }
    }

    /// Plays the track `id` of `kind` (one of [`Session::tracks`]), or none
    /// of that kind, and names the one that plays now; `None` when what
    /// plays has no tracks to choose (the test picture).
    pub fn select_track(&mut self, kind: TrackKind, id: Option<i64>) -> Option<String> {
        match &mut self.kind {
            Kind::Test(_) => {
                let _ = (kind, id);
                None
            }
            Kind::Own(_) => Some(NO_TRACKS.to_owned()),
            #[cfg(takt_libmpv)]
            Kind::File(file) => Some(
                file.source
                    .select_track(track_of(kind), id)
                    .unwrap_or_else(|e| e.to_string()),
            ),
        }
    }

    /// Shows the subtitle file `path` with the file that plays and names
    /// the track. While a file is being opened (a film dropped together
    /// with its subtitles) the subtitle file waits and is shown once the
    /// film is open; the line says so.
    ///
    /// # Errors
    ///
    /// A text for the user: the test picture takes no subtitles, or why the
    /// file is not shown.
    pub fn add_subtitles(&mut self, path: &Path) -> Result<String, String> {
        #[cfg(takt_libmpv)]
        if self.opening() {
            if self.wanted_subtitles.len() >= WANTED_SUBTITLES {
                return Err("too many subtitle files at once".to_owned());
            }
            self.wanted_subtitles.push(path.to_path_buf());
            return Ok("subtitles: shown when the file is open".to_owned());
        }
        match &mut self.kind {
            Kind::Test(_) => {
                let _ = path;
                Err("no file plays".to_owned())
            }
            Kind::Own(_) => Err(NO_TRACKS.to_owned()),
            #[cfg(takt_libmpv)]
            Kind::File(file) => file
                .source
                .add_subtitles(&path.to_string_lossy())
                .map_err(|e| e.to_string()),
        }
    }

    /// Shows the subtitles `by_millis` later (negative: earlier) and says
    /// in a line where they stand now; `None` when what plays has no
    /// subtitles to move (the test picture).
    pub fn shift_subtitles(&mut self, by_millis: i32) -> Option<String> {
        match &mut self.kind {
            Kind::Test(_) => {
                let _ = by_millis;
                None
            }
            Kind::Own(_) => Some(NO_TRACKS.to_owned()),
            #[cfg(takt_libmpv)]
            Kind::File(file) => Some(match file.source.shift_subtitles(by_millis) {
                Ok(0) => "subtitles: as the file says".to_owned(),
                Ok(shift) if shift > 0 => format!("subtitles: {shift} ms later"),
                Ok(shift) => format!("subtitles: {} ms earlier", shift.unsigned_abs()),
                Err(e) => e.to_string(),
            }),
        }
    }

    /// Length of what plays, in milliseconds; 0 when a file does not say.
    #[must_use]
    pub fn duration_millis(&self) -> i64 {
        match &self.kind {
            Kind::Test(_) => TEST_MILLIS,
            #[cfg(takt_libmpv)]
            Kind::File(file) => file.duration_millis,
            Kind::Own(own) => own
                .info
                .duration
                .and_then(|d| d.rescale(Rational::MILLIS))
                .map_or(0, Timestamp::ticks),
        }
    }

    /// The position of the clock in milliseconds.
    #[must_use]
    pub fn now_millis(&self) -> i64 {
        self.clock()
            .now()
            .rescale(Rational::MILLIS)
            .map_or(0, Timestamp::ticks)
    }

    /// Where the sound goes, in one line.
    #[must_use]
    pub fn audio_line(&self) -> String {
        match &self.kind {
            Kind::Test(test) => test.audio_line.clone(),
            #[cfg(takt_libmpv)]
            Kind::File(file) => {
                let follows = if file.source.has_sound() {
                    "the clock follows its sound"
                } else {
                    "no sound track, the clock follows the picture"
                };
                format!("{} ({follows})", file.source.sound_line())
            }
            Kind::Own(own) => format!(
                "{} (silence: the own pipeline decodes no sound yet)",
                own.audio_line
            ),
        }
    }

    /// [`Session::limit_note`], once for each file that is opened: asked
    /// every pass, it answers as soon as the colour of the file is known
    /// (when it opens, or with its first frame) and `None` from then on.
    pub fn take_limit_note(&mut self) -> Option<&'static str> {
        let note = self.limit_note();
        match &mut self.kind {
            Kind::Test(_) => {
                let _ = note;
                None
            }
            #[cfg(takt_libmpv)]
            Kind::File(file) => {
                let note = note?;
                let news = !file.limit_told;
                file.limit_told = true;
                news.then_some(note)
            }
            Kind::Own(own) => {
                let note = note?;
                let news = !own.limit_told;
                own.limit_told = true;
                news.then_some(note)
            }
        }
    }

    /// What the person should know about how the file that plays is shown;
    /// `None` when there is nothing to say.
    ///
    /// HDR: mpv brings the picture to BT.709 on this path and does not
    /// tone-map, so what is brighter than SDR white is cut off. Decided on
    /// 2026-11-14 to be shown that way until the pipeline of Phase 1, and
    /// to be said.
    #[must_use]
    pub fn limit_note(&self) -> Option<&'static str> {
        match &self.kind {
            Kind::Test(_) => None,
            #[cfg(takt_libmpv)]
            Kind::File(file) => file
                .color
                .is_some_and(|color| color.is_hdr())
                .then_some("HDR is shown as SDR for now: bright parts are cut off at white"),
            Kind::Own(_) => Some(OWN_LIMIT),
        }
    }

    /// The source in one line, for the diagnostics.
    #[must_use]
    pub fn source_line(&self) -> String {
        match &self.kind {
            Kind::Test(_) => "null (test picture), 1920x1080 NV12".to_owned(),
            #[cfg(takt_libmpv)]
            Kind::File(file) => {
                let size = file
                    .size
                    .map_or("no picture".to_owned(), |(w, h)| format!("{w}x{h} rgb0"));
                let render = file.source.render_times();
                let bound = |num, den| {
                    render
                        .quantile_micros(num, den)
                        .map_or("-".to_owned(), |us| format!("<{us} us"))
                };
                let size = match file.color {
                    Some(color) => format!("{size}, {}", color_note(color)),
                    None => size,
                };
                format!(
                    "{}, {size}, lead {} us, render p50 {} p99 {}",
                    file.source.name(),
                    file.source.early_micros(),
                    bound(1, 2),
                    bound(99, 100)
                )
            }
            Kind::Own(own) => {
                let stats = own.source.pipeline_stats();
                let size = own.info.video.map_or("no picture".to_owned(), |video| {
                    format!("{}x{} {:?}", video.width, video.height, video.format)
                });
                // The two copies of a picture in this process: out of the
                // shared memory, and into its frame.
                let copy = (stats.decode.read_nanos + stats.decode.copy_nanos)
                    .checked_div(stats.decode.frames)
                    .map_or("-".to_owned(), |nanos| format!("{} us", nanos / 1_000));
                format!(
                    "{} ({}), {size}, copy {copy}, pool empty {}x, decode errors {}",
                    own.source.name(),
                    stats.decoder,
                    stats.decode.pool_exhausted,
                    stats.decode.errors
                )
            }
        }
    }

    /// Short name of the backend.
    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        self.source().name()
    }

    /// The counters of the device callback of the player; `None` when
    /// simulated, and for a file (mpv has the device).
    #[must_use]
    pub fn device_stats(&self) -> Option<Arc<OutputStats>> {
        match &self.kind {
            Kind::Test(test) => test.audio.device_stats(),
            #[cfg(takt_libmpv)]
            Kind::File(_) => None,
            Kind::Own(own) => own.audio.device_stats(),
        }
    }

    /// What the feeder of the test picture saw of its ring; `None` when
    /// simulated, and for a file.
    #[must_use]
    pub fn feeder_stats(&self) -> Option<Arc<crate::audio::FeederStats>> {
        match &self.kind {
            Kind::Test(test) => test.audio.feeder_stats(),
            #[cfg(takt_libmpv)]
            Kind::File(_) => None,
            Kind::Own(own) => own.audio.feeder_stats(),
        }
    }

    /// Frames of the pool that are out, and its size.
    #[must_use]
    pub fn pool(&self) -> (usize, usize) {
        let size = match &self.kind {
            Kind::Test(_) => TEST_POOL_FRAMES,
            #[cfg(takt_libmpv)]
            Kind::File(file) => file.source.pool_frames(),
            Kind::Own(own) => own.source.pool().1,
        };
        (self.stats().queued, size)
    }
}

/// What the person should know about a file the own pipeline plays.
const OWN_LIMIT: &str = "own pipeline: picture only, no sound and no subtitles yet";

/// Held by every test that plays through the own pipeline: one at a time. Each starts two helper
/// processes with a decoder thread per core, and the tests of the `MpvBackend` next to them
/// measure real time.
#[cfg(all(test, takt_ffmpeg))]
pub static OWN_PIPELINE_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What the own pipeline says to a wish for a track or for subtitles.
const NO_TRACKS: &str = "own pipeline: picture only, no tracks to choose yet";

#[cfg(takt_libmpv)]
const fn track_of(kind: TrackKind) -> Track {
    match kind {
        TrackKind::Audio => Track::Audio,
        TrackKind::Subtitle => Track::Subtitle,
    }
}

/// What the file says about its colour, and what that means on this
/// path, for the diagnostics: mpv converts every picture to BT.709, and it
/// does not tone-map, so HDR is cut off at white.
#[cfg(takt_libmpv)]
fn color_note(color: takt_core::ColorDescription) -> String {
    use takt_core::{ColorPrimaries, TransferFunction};
    let primaries = match color.primaries {
        ColorPrimaries::Bt709 => "bt.709",
        ColorPrimaries::Bt601Ntsc => "bt.601-525",
        ColorPrimaries::Bt601Pal => "bt.601-625",
        ColorPrimaries::Bt2020 => "bt.2020",
        ColorPrimaries::DciP3 => "dci-p3",
        ColorPrimaries::DisplayP3 => "display-p3",
        _ => "unknown primaries",
    };
    let transfer = match color.transfer {
        TransferFunction::Bt709 => "bt.1886",
        TransferFunction::Srgb => "srgb",
        TransferFunction::Pq => "pq",
        TransferFunction::Hlg => "hlg",
        TransferFunction::Linear => "linear",
        _ => "unknown transfer",
    };
    let shown = if color.is_hdr() {
        "shown as bt.709, hdr cut off at white"
    } else {
        "shown as bt.709"
    };
    format!("file {primaries}/{transfer}, {shown}")
}

#[cfg(takt_libmpv)]
fn duration_millis(info: &takt_backend::MediaInfo) -> i64 {
    info.duration
        .and_then(|d| d.rescale(Rational::MILLIS))
        .map_or(0, Timestamp::ticks)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session that opens no output device.
    fn session() -> Session {
        Session::silent(Arc::new(SystemMonotonic::new()))
    }

    /// Passes of 4 ms, as the window makes them, until the open has ended.
    fn opened(session: &mut Session) -> Result<(), String> {
        let asked = std::time::Instant::now();
        loop {
            if let Some(ended) = session.poll_open() {
                return ended;
            }
            assert!(
                asked.elapsed() < std::time::Duration::from_secs(15),
                "the open never ended"
            );
            let _ = session.next_frame();
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    }

    #[cfg(not(takt_libmpv))]
    #[test]
    fn without_libmpv_an_entry_is_the_test_picture_again_and_ends_at_once() {
        let mut session = session();
        assert!(session.poll_open().is_none(), "nothing was asked");
        session.seek_to(90_000);
        session.open_file(Path::new("film.mkv")).unwrap();
        assert!(session.opening());
        assert_eq!(session.poll_open(), Some(Ok(())));
        assert!(!session.opening() && session.poll_open().is_none());
        assert!(!session.is_file());
        assert!(session.now_millis() < 1_000, "from the start");
    }

    /// The own pipeline, whatever the environment of the test run says.
    fn own(session: &mut Session) {
        if session.pipeline() != Pipeline::Own {
            session.switch_pipeline(Pipeline::Own).unwrap();
        }
    }

    /// Passes of 4 ms until a frame comes, at most five seconds.
    #[cfg(takt_ffmpeg)]
    fn frame(session: &mut Session) -> PooledFrame {
        let asked = std::time::Instant::now();
        loop {
            if let Some(frame) = session.next_frame().unwrap() {
                return frame;
            }
            assert!(
                asked.elapsed() < std::time::Duration::from_secs(5),
                "no frame"
            );
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
    }

    #[test]
    fn the_pipeline_is_chosen_while_nothing_plays_and_says_what_it_is() {
        let mut session = session();
        let line = session.switch_pipeline(Pipeline::Own).unwrap();
        assert!(line.contains("own") && line.contains("no sound"), "{line}");
        assert_eq!(session.pipeline(), Pipeline::Own);
        assert!(!session.opening(), "nothing plays, nothing is opened again");
        assert_eq!(session.current(), None);
        assert_eq!(Pipeline::Own.other(), Pipeline::Mpv);
        assert_eq!(Pipeline::Mpv.other(), Pipeline::Own);
        let back = session.switch_pipeline(Pipeline::Mpv);
        if cfg!(takt_libmpv) {
            assert_eq!(back, Ok("pipeline: libmpv".to_owned()));
            assert_eq!(session.pipeline(), Pipeline::Mpv);
        } else {
            assert!(back.unwrap_err().contains("no libmpv"));
            assert_eq!(session.pipeline(), Pipeline::Own, "it stays the one it was");
        }
        // What is no file is refused before a helper starts.
        own(&mut session);
        assert_eq!(
            session.open_file(Path::new("takt-no-such-file.mkv")),
            Err("no such file".to_owned())
        );
        assert!(!session.opening() && !session.is_file());
    }

    /// Without its helper programs (an install that does not carry them) the open ends with
    /// the reason, and the test picture plays on.
    #[test]
    fn the_own_pipeline_without_its_helpers_says_why_and_the_test_picture_plays_on() {
        let clip = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/decode-h264.mp4");
        let mut session = session();
        own(&mut session);
        // A directory that holds no helper, whatever was built next to this test.
        session.helpers = Some(std::env::temp_dir().join("takt-no-helpers-here"));
        let clock = std::ptr::from_ref(session.master());
        session.open_file(Path::new(clip)).unwrap();
        assert!(session.opening());
        let why = opened(&mut session).unwrap_err();
        assert!(why.contains("helper not started"), "{why}");
        assert!(!session.opening() && !session.is_file());
        assert_eq!(session.backend_name(), "null");
        assert_eq!(std::ptr::from_ref(session.master()), clock);
        assert_eq!(session.current(), None);
        assert_eq!(session.pipeline(), Pipeline::Own);
    }

    /// The second backend behind `VideoBackend`: a file plays through `takt-demux` and
    /// `takt-decode`, on the clock of the player.
    #[cfg(takt_ffmpeg)]
    #[test]
    fn a_file_plays_through_the_own_pipeline_on_the_clock_of_the_player() {
        let _one_at_a_time = OWN_PIPELINE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let clip = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/decode-hevc.mp4");
        let mut session = session();
        own(&mut session);
        session.set_paused(true);
        session.open_file(Path::new(clip)).unwrap();
        assert!(session.opening() && !session.is_file());
        assert_eq!(session.backend_name(), "null", "the test picture plays on");
        assert_eq!(opened(&mut session), Ok(()));
        assert!(session.is_file() && !session.opening());
        assert_eq!(session.backend_name(), "takt-sw");
        assert_eq!(session.current(), Some(Path::new(clip)));
        assert_eq!(session.duration_millis(), 2_000);
        let held = session.now_millis();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(session.now_millis(), held, "the pause went along");
        assert!(session.audio_line().contains("no sound yet"));
        assert_eq!(session.take_limit_note(), Some(OWN_LIMIT));
        assert_eq!(session.take_limit_note(), None, "said once");
        assert!(session.tracks(TrackKind::Audio).is_empty());
        assert_eq!(
            session.cycle_track(TrackKind::Subtitle),
            Some(NO_TRACKS.to_owned())
        );
        assert_eq!(
            session.add_subtitles(Path::new("film.srt")),
            Err(NO_TRACKS.to_owned())
        );

        // The first picture comes although the clock stands, as NV12 for the texture path.
        let first = frame(&mut session);
        assert_eq!(first.get().meta().format, PixelFormat::Nv12);
        assert_eq!(first.get().meta().pts, Timestamp::from_millis(0));
        assert_eq!(
            (first.get().meta().width, first.get().meta().height),
            (160, 90)
        );
        assert!(crate::render::Picture::from_frame(first).is_ok());

        // A jump shows the picture of the target, exactly, and the clock stands there.
        session.seek_to(1_210);
        assert_eq!(session.now_millis(), 1_210);
        let jumped = frame(&mut session);
        assert_eq!(jumped.get().meta().pts, Timestamp::from_millis(1_240));
        assert_eq!(session.seek_landed(Some(1_240)), Some(1_240));
        drop(jumped);

        // Released, the file plays to its end on the clock.
        session.set_paused(false);
        let started = std::time::Instant::now();
        let mut frames = 0;
        while !session.is_eof() {
            assert!(started.elapsed() < std::time::Duration::from_secs(10));
            if session.next_frame().unwrap().is_some() {
                frames += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        assert!(frames >= 10, "{frames} frames from 1.24 s to the end");
        let line = session.source_line();
        assert!(line.starts_with("takt-sw (hevc), 160x90 Nv12"), "{line}");
        assert!(line.contains("pool empty 0x"), "{line}");
        assert_eq!(session.pool().1, 8);

        // A file the pipeline cannot play is refused with the reason, and the file that
        // plays is still the one F6 would open again.
        let vp9 = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/format-vp9.webm");
        session.open_file(Path::new(vp9)).unwrap();
        assert_eq!(session.current(), Some(Path::new(vp9)));
        let why = opened(&mut session).unwrap_err();
        assert!(why.contains("vp9"), "{why}");
        assert!(session.is_file());
        assert_eq!(session.backend_name(), "takt-sw");
        assert_eq!(session.current(), Some(Path::new(clip)));

        // Back to the test picture: the helpers end with the file.
        session.show_test_picture();
        assert!(!session.is_file());
        assert_eq!(session.backend_name(), "null");
        assert_eq!(session.current(), None);
    }

    /// 10-bit material comes as P010, which the layer of `takt-gpu` takes.
    #[cfg(takt_ffmpeg)]
    #[test]
    fn ten_bits_come_as_p010_frames_the_window_takes() {
        let _one_at_a_time = OWN_PIPELINE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let clip = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/decode-av1-10bit.mkv"
        );
        let mut session = session();
        own(&mut session);
        session.open_file(Path::new(clip)).unwrap();
        assert_eq!(opened(&mut session), Ok(()));
        assert_eq!(session.limit_note(), Some(OWN_LIMIT));
        let first = frame(&mut session);
        assert_eq!(first.get().meta().format, PixelFormat::P010);
        assert_eq!(
            (first.get().meta().width, first.get().meta().height),
            (130, 74)
        );
        assert!(session.source_line().contains("(libdav1d)"));
        assert!(crate::render::Picture::from_frame(first).is_ok());
    }

    /// A file that is still being opened when the pipeline changes is opened by the other one.
    #[cfg(all(takt_libmpv, takt_ffmpeg))]
    #[test]
    fn a_switch_while_a_file_is_being_opened_opens_it_with_the_other_pipeline() {
        let _one_at_a_time = OWN_PIPELINE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let clip = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/decode-h264.mp4");
        let mut session = session();
        if session.pipeline() != Pipeline::Mpv {
            session.switch_pipeline(Pipeline::Mpv).unwrap();
        }
        session.open_file(Path::new(clip)).unwrap();
        assert!(session.opening());
        session.switch_pipeline(Pipeline::Own).unwrap();
        assert!(session.opening());
        assert_eq!(opened(&mut session), Ok(()));
        assert_eq!(session.backend_name(), "takt-sw");
        assert_eq!(session.current(), Some(Path::new(clip)));
        assert!(session.now_millis() < 1_000, "from its start");
        // And the other way round.
        session.show_test_picture();
        session.open_file(Path::new(clip)).unwrap();
        session.switch_pipeline(Pipeline::Mpv).unwrap();
        assert_eq!(opened(&mut session), Ok(()));
        assert_eq!(session.backend_name(), "mpv-sw");
    }

    /// The acceptance point of `takt-decode`: the two backends are swapped while a file plays,
    /// and it goes on where it was.
    #[cfg(all(takt_libmpv, takt_ffmpeg))]
    #[test]
    fn the_pipelines_are_swapped_while_a_file_plays_and_it_goes_on_where_it_was() {
        let _one_at_a_time = OWN_PIPELINE_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let clip = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/long-gop-12s.mkv");
        let mut session = session();
        if session.pipeline() != Pipeline::Mpv {
            session.switch_pipeline(Pipeline::Mpv).unwrap();
        }
        session.set_paused(true);
        session.open_file(Path::new(clip)).unwrap();
        assert_eq!(opened(&mut session), Ok(()));
        assert_eq!(session.backend_name(), "mpv-sw");
        session.seek_to(7_000);
        let arrived = std::time::Instant::now();
        while session.seek_landed(None).is_none() {
            assert!(arrived.elapsed() < std::time::Duration::from_secs(10));
            let _ = session.next_frame();
            std::thread::sleep(std::time::Duration::from_millis(4));
        }

        // A jump of libmpv goes to a keyframe: the file has one at 4 s and at 8 s.
        let before = session.now_millis();
        assert!((3_900..=8_100).contains(&before), "libmpv at {before} ms");

        // To the own pipeline: libmpv plays on until the other one has the file open.
        let line = session.switch_pipeline(Pipeline::Own).unwrap();
        assert!(line.starts_with("pipeline: own"), "{line}");
        assert!(session.opening());
        assert_eq!(session.backend_name(), "mpv-sw");
        assert_eq!(opened(&mut session), Ok(()));
        assert_eq!(session.backend_name(), "takt-sw");
        assert_eq!(session.pipeline(), Pipeline::Own);
        let position = session.now_millis();
        assert_eq!(position, before, "where libmpv was");
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(session.now_millis(), position, "the pause went along");
        let shown = frame(&mut session);
        let pts = shown.get().meta().pts.rescale(Rational::MILLIS).unwrap();
        assert!(
            (before..before + 40).contains(&pts.ticks()),
            "the picture of {before} ms has {pts:?}"
        );
        assert_eq!(shown.get().meta().format, PixelFormat::Nv12);
        drop(shown);

        // And back to libmpv, at the same place: between two keyframes, where a jump to a
        // keyframe would land seconds away.
        session.seek_to(3_000);
        assert_eq!(
            session.switch_pipeline(Pipeline::Mpv),
            Ok("pipeline: libmpv".to_owned())
        );
        assert_eq!(
            session.backend_name(),
            "takt-sw",
            "until libmpv has it open"
        );
        assert_eq!(opened(&mut session), Ok(()));
        assert_eq!(session.backend_name(), "mpv-sw");
        let arrived = std::time::Instant::now();
        let landed = loop {
            assert!(arrived.elapsed() < std::time::Duration::from_secs(10));
            let _ = session.next_frame();
            if let Some(landed) = session.seek_landed(None) {
                break landed;
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
        };
        assert!(
            (3_000..3_040).contains(&landed),
            "libmpv went on at {landed} ms"
        );
        assert_eq!(session.current(), Some(Path::new(clip)));
    }

    #[test]
    fn the_test_picture_plays_on_the_clock_of_the_player() {
        let mut session = session();
        assert!(!session.is_file());
        assert_eq!(session.backend_name(), "null");
        assert_eq!(session.duration_millis(), TEST_MILLIS);
        assert_eq!(session.seek_limit(), TEST_MILLIS);
        assert_eq!(session.pool().1, TEST_POOL_FRAMES);
        assert!(session.audio_line().starts_with("SIMULATED"));
        assert_eq!(session.cycle_track(TrackKind::Subtitle), None);
        assert!(session.tracks(TrackKind::Audio).is_empty());
        assert_eq!(session.limit_note(), None);
        assert_eq!(session.select_track(TrackKind::Subtitle, Some(1)), None);
        // A jump of the test picture landed where its next frame is.
        assert_eq!(session.seek_landed(None), None);
        assert_eq!(session.seek_landed(Some(90_000)), Some(90_000));
        assert_eq!(session.shift_subtitles(100), None);
        assert_eq!(
            session.add_subtitles(Path::new("film.srt")),
            Err("no file plays".to_owned())
        );
        session.set_paused(true);
        session.seek_to(90_000);
        assert_eq!(session.now_millis(), 90_000, "held at the target");
        session.seek_to(TEST_MILLIS + 5);
        assert_eq!(session.now_millis(), TEST_MILLIS, "never beyond the end");
        session.show_test_picture();
        assert!(!session.is_file());
    }

    /// Invariant 3: going from the test picture to a file and back swaps
    /// the clock; there is one before, one after, and it is another one.
    #[cfg(takt_libmpv)]
    #[test]
    fn a_file_swaps_the_clock_and_the_test_picture_swaps_it_back() {
        let clip = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/pattern-3s.mkv");
        let mut session = session();
        let first = std::ptr::from_ref(session.master());
        session.set_volume(0.25);
        session.set_paused(true);

        // What is no file leaves the test picture and its clock in place.
        assert!(
            session
                .open_file(Path::new("takt-no-such-file.mkv"))
                .is_err()
        );
        assert!(!session.is_file());
        assert_eq!(std::ptr::from_ref(session.master()), first);

        // Opening returns at once; until mpv has the file open, the test
        // picture plays on, on its clock.
        assert!(session.poll_open().is_none(), "nothing was asked");
        session.open_file(Path::new(clip)).unwrap();
        assert!(session.opening());
        assert!(!session.is_file());
        assert_eq!(std::ptr::from_ref(session.master()), first);
        assert_eq!(opened(&mut session), Ok(()));
        assert!(!session.opening());
        assert!(session.is_file());
        assert_eq!(session.backend_name(), "mpv-sw");
        let second = std::ptr::from_ref(session.master());
        assert_ne!(first, second, "the clock of mpv took over");
        assert!(session.audio_line().starts_with("mpv: silent"));
        assert_eq!(session.duration_millis(), 3_003);
        assert!(session.device_stats().is_none(), "mpv has the device");
        assert!(session.master().is_paused(), "the pause went along");
        assert!(session.source_line().contains("160x90 rgb0"));

        // Frames come on the new clock.
        session.set_paused(false);
        let started = std::time::Instant::now();
        let mut frames = 0;
        while frames < 3 && started.elapsed() < std::time::Duration::from_secs(2) {
            if let Some(frame) = session.next_frame().unwrap() {
                assert_eq!(frame.get().meta().format, PixelFormat::Rgb0);
                frames += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        assert_eq!(frames, 3);
        // The colour of the file: mpv may name it only once it has decoded
        // a frame, so it is asked for after the first ones, not at once.
        assert!(
            session
                .source_line()
                .contains("file bt.709/bt.1886, shown as bt.709, lead"),
            "{}",
            session.source_line()
        );
        assert!(session.cycle_track(TrackKind::Subtitle).is_some());
        let line = |s: &str| Some(s.to_owned());
        assert_eq!(
            session.shift_subtitles(100),
            line("subtitles: 100 ms later")
        );
        assert_eq!(
            session.shift_subtitles(-200),
            line("subtitles: 100 ms earlier")
        );
        assert_eq!(
            session.shift_subtitles(100),
            line("subtitles: as the file says")
        );
        assert!(
            session
                .add_subtitles(Path::new("takt-no-such.srt"))
                .is_err()
        );

        // The next file plays on the same backend and the same clock.
        session.open_file(Path::new(clip)).unwrap();
        assert!(session.opening());
        assert_eq!(opened(&mut session), Ok(()));
        assert_eq!(std::ptr::from_ref(session.master()), second);
        assert_eq!(session.duration_millis(), 3_003);

        session.show_test_picture();
        assert!(!session.is_file());
        assert_eq!(session.backend_name(), "null");
        assert_ne!(std::ptr::from_ref(session.master()), second);
    }

    /// A file of bytes mpv cannot play, with the name of a film.
    #[cfg(takt_libmpv)]
    fn junk(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("takt-junk-{}-{tag}.mkv", std::process::id()));
        std::fs::write(&path, b"this is not a film\n").unwrap();
        path
    }

    #[cfg(takt_libmpv)]
    #[test]
    fn a_file_mpv_cannot_open_is_reported_once_and_the_test_picture_plays_on() {
        let junk = junk("alone");
        let mut session = session();
        let first = std::ptr::from_ref(session.master());
        // It exists, so the open begins; mpv finds out that it is no film.
        session.open_file(&junk).unwrap();
        assert!(session.opening());
        let ended = opened(&mut session);
        let _ = std::fs::remove_file(&junk);
        assert_eq!(
            ended,
            Err("Quelle nicht verfuegbar: mpv cannot open it".to_owned())
        );
        assert!(!session.opening());
        assert!(session.poll_open().is_none(), "reported once");
        assert!(!session.is_file());
        assert_eq!(std::ptr::from_ref(session.master()), first);
        assert!(session.next_frame().is_ok());
    }

    /// An HDR file says once that it is shown as SDR; an SDR file and the
    /// test picture say nothing.
    #[cfg(takt_libmpv)]
    #[test]
    fn an_hdr_file_says_that_it_is_cut_off_at_white() {
        let data = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data");
        let mut session = session();
        // The files are a fifth of a second long: held at their first
        // picture, they are still there when they are asked about.
        session.set_paused(true);
        // Opens a file and waits until its colour is known: mpv names it
        // when the file opens or, on a busy machine, with its first frame.
        let open = |session: &mut Session, name: &str| {
            session.open_file(&Path::new(data).join(name)).unwrap();
            assert_eq!(opened(session), Ok(()), "{name}");
            let asked = std::time::Instant::now();
            while !session.source_line().contains("file ") {
                assert!(
                    asked.elapsed() < std::time::Duration::from_secs(10),
                    "{name}: {}",
                    session.source_line()
                );
                let _ = session.next_frame().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(4));
            }
        };
        for (name, says) in [
            ("colour-hdr10.mkv", true),
            ("colour-bt709.mkv", false),
            ("colour-hlg.mkv", true),
            ("colour-ntsc.mkv", false),
        ] {
            open(&mut session, name);
            assert_eq!(
                session.limit_note().is_some(),
                says,
                "{name}: {:?}",
                session.limit_note()
            );
        }
        assert!(session.source_line().contains("shown as bt.709"));
        // Said once for a file: HLG was the last HDR file opened, NTSC came
        // after it and has nothing to say.
        assert_eq!(session.take_limit_note(), None);
        open(&mut session, "colour-hdr10.mkv");
        assert!(session.take_limit_note().is_some());
        assert_eq!(session.take_limit_note(), None, "once");
        assert!(session.limit_note().is_some(), "still true");

        // mpv did not know the colour when the file opened: it is read
        // again with the first frame, and the note comes then.
        session
            .open_file(&Path::new(data).join("colour-hlg.mkv"))
            .unwrap();
        assert_eq!(opened(&mut session), Ok(()));
        match &mut session.kind {
            Kind::File(file) => {
                file.color = None;
                file.limit_told = false;
            }
            Kind::Test(_) | Kind::Own(_) => panic!("libmpv plays a file"),
        }
        assert_eq!(session.take_limit_note(), None, "not known yet");
        // Held at its first picture, the file shows another one for a jump.
        session.seek_to(0);
        let started = std::time::Instant::now();
        let mut note = None;
        while note.is_none() && started.elapsed() < std::time::Duration::from_secs(5) {
            let _ = session.next_frame().unwrap();
            note = session.take_limit_note();
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        assert!(note.is_some(), "said with the first frame");
        assert_eq!(session.take_limit_note(), None);

        session.show_test_picture();
        assert_eq!(session.limit_note(), None);
        assert_eq!(session.take_limit_note(), None);
    }

    /// A jump in a file landed on the keyframe before its target (the test
    /// file has one every second), and the tracks are there to choose from.
    #[cfg(takt_libmpv)]
    #[test]
    fn a_jump_in_a_file_is_measured_and_its_tracks_are_listed() {
        let clip = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/pattern-3s.mkv");
        let mut session = session();
        session.open_file(Path::new(clip)).unwrap();
        assert_eq!(opened(&mut session), Ok(()));

        let labels: Vec<String> = session
            .tracks(TrackKind::Subtitle)
            .into_iter()
            .map(|track| track.label)
            .collect();
        assert_eq!(labels, ["1 (deu)", "2 (eng)"]);
        assert_eq!(
            session
                .select_track(TrackKind::Subtitle, Some(2))
                .as_deref(),
            Some("subtitles: 2 (eng)")
        );
        assert_eq!(
            session
                .select_track(TrackKind::Subtitle, Some(9))
                .as_deref(),
            Some("Quelle nicht verfuegbar: no such track")
        );

        // Held, so that the position read is where the jump landed and not
        // a frame or two further on a busy machine.
        session.set_paused(true);
        session.seek_to(2_500);
        let started = std::time::Instant::now();
        let mut landed = None;
        while landed.is_none() && started.elapsed() < std::time::Duration::from_secs(5) {
            let shown = session.next_frame().unwrap().map(|_| 0);
            // What the frame says is not asked: mpv names where it is.
            landed = session.seek_landed(shown);
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        // The keyframe at 2 s, not the target at 2.5 s. Or a little after
        // it: mpv names the position of the sound once the sound runs
        // (`time-pos` is then `audio-pts`), and after the change of the
        // subtitle track above the sound of the jump sometimes starts one or
        // two of its packets (about 21 ms each) after the keyframe, at 2039
        // or 2060 ms. Measured without load: 8 of 200 jumps with the track
        // change, none of 200 without it. The events of the jump are not
        // mistaken for those of another one: when the end of the jump was
        // taken, mpv itself already named 2060 ms and no longer seeked.
        assert!(
            landed.is_some_and(|ms| (2_000..=2_120).contains(&ms)),
            "{landed:?}"
        );
    }

    /// A film dropped together with its subtitle file: the subtitles are
    /// asked for while the film still opens, and show once it is open.
    #[cfg(takt_libmpv)]
    #[test]
    fn subtitles_that_come_with_their_film_wait_until_it_is_open() {
        let clip = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/pattern-3s.mkv");
        let subtitles =
            std::env::temp_dir().join(format!("takt-with-film-{}.srt", std::process::id()));
        std::fs::write(
            &subtitles,
            "1\n00:00:00,000 --> 00:00:03,000\nwith the film\n",
        )
        .unwrap();
        let shows = |session: &mut Session| match &mut session.kind {
            Kind::File(file) => file.source.shows_subtitles_of_a_file(),
            Kind::Test(_) | Kind::Own(_) => false,
        };

        let mut session = session();
        session.open_file(Path::new(clip)).unwrap();
        assert_eq!(
            session.add_subtitles(&subtitles),
            Ok("subtitles: shown when the file is open".to_owned())
        );
        assert_eq!(opened(&mut session), Ok(()));
        assert!(shows(&mut session), "shown once the film is open");

        // Asked for with a film that does not open: forgotten, and not
        // shown with the film that comes after.
        let junk = junk("with-subtitles");
        session.open_file(&junk).unwrap();
        assert!(session.add_subtitles(&subtitles).is_ok());
        assert!(opened(&mut session).is_err());
        session.open_file(Path::new(clip)).unwrap();
        assert_eq!(opened(&mut session), Ok(()));
        assert!(!shows(&mut session), "the film alone");

        // And another film asked for meanwhile takes over without them.
        session.open_file(Path::new(clip)).unwrap();
        assert!(session.add_subtitles(&subtitles).is_ok());
        session.open_file(Path::new(clip)).unwrap();
        assert_eq!(opened(&mut session), Ok(()));
        assert!(!shows(&mut session));

        // No more than a few wait.
        session.open_file(Path::new(clip)).unwrap();
        for _ in 0..WANTED_SUBTITLES {
            assert!(session.add_subtitles(&subtitles).is_ok());
        }
        assert!(session.add_subtitles(&subtitles).is_err());
        assert_eq!(opened(&mut session), Ok(()));
        assert!(shows(&mut session));
        let _ = std::fs::remove_file(&subtitles);
        let _ = std::fs::remove_file(&junk);
    }

    /// The next entry while one is still being opened: the answer is the
    /// one of the file asked for last, not the end of the one given up.
    #[cfg(takt_libmpv)]
    #[test]
    fn a_file_asked_for_while_another_opens_replaces_it() {
        let clip = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/pattern-3s.mkv");
        let junk = junk("replaced");

        // From the test picture: the backend that is opening takes the next.
        let mut session = session();
        session.open_file(&junk).unwrap();
        session.open_file(Path::new(clip)).unwrap();
        assert_eq!(opened(&mut session), Ok(()));
        assert!(session.is_file());
        assert_eq!(session.duration_millis(), 3_003);
        assert!(session.poll_open().is_none(), "one answer for two asks");

        // And while a file plays, both ways round.
        session.open_file(&junk).unwrap();
        session.open_file(Path::new(clip)).unwrap();
        assert_eq!(opened(&mut session), Ok(()));
        assert_eq!(session.duration_millis(), 3_003);
        session.open_file(Path::new(clip)).unwrap();
        session.open_file(&junk).unwrap();
        assert!(opened(&mut session).is_err());
        assert!(session.poll_open().is_none());

        // What is no file is refused at once; the open on its way goes on.
        session.open_file(Path::new(clip)).unwrap();
        assert!(
            session
                .open_file(Path::new("takt-no-such-file.mkv"))
                .is_err()
        );
        assert!(session.opening());
        assert_eq!(opened(&mut session), Ok(()));

        // Back to the test picture gives an open up.
        session.open_file(Path::new(clip)).unwrap();
        session.show_test_picture();
        assert!(!session.opening() && !session.is_file());
        let _ = std::fs::remove_file(&junk);
    }
}
