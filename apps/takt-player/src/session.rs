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
//!
//! Invariant 3 of `docs/THREADS-UND-UHREN.md` (exactly one clock is active)
//! is the shape of this type: the two cases are the variants of one enum, so
//! a session holds one clock and one sound path, never both. Going from one
//! to the other replaces the variant; the old clock and its device stop when
//! it is dropped. Everything else in the player reads the clock through
//! [`Session::clock`] and cannot keep the old one.

use std::path::Path;
use std::sync::Arc;

use takt_audio::OutputStats;
use takt_backend::{BackendStats, NullBackend, Pattern, SeekMode, VideoBackend};
use takt_clock::{AudioMasterClock, Clock, SeekableClock, SystemMonotonic};
use takt_core::{
    ColorPrimaries, ColorRange, FrameMeta, PixelFormat, PooledFrame, Rational, Timestamp,
    TransferFunction,
};

use crate::audio::{self, AudioPath};
#[cfg(takt_libmpv)]
use crate::mpv::{Config, Early, MpvBackend, Sound, Sources, Track};

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
    /// What the file says about its colour, for the diagnostics.
    color: Option<takt_core::ColorDescription>,
}

enum Kind {
    Test(Box<Test>),
    #[cfg(takt_libmpv)]
    File(Box<File>),
}

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
            silent,
            paused: false,
            volume: 1.0,
        }
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
                    self.pending = Some(Pending::Same);
                }
                // The backend that is opening already takes the new file: it
                // is not dropped while mpv may still be busy with the old one.
                (Kind::Test(_), Some(Pending::Fresh(source))) => {
                    source.begin_open(&name).map_err(|e| e.to_string())?;
                }
                (Kind::Test(_), _) => {
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
            Ok(())
        }
        #[cfg(not(takt_libmpv))]
        {
            let Kind::Test(test) = &mut self.kind;
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
                            source: *source,
                        }));
                        Ok(())
                    }
                    Some(Err(e)) => Err(e.to_string()),
                },
                // Nothing of a file is left to wait for.
                (Pending::Same, Kind::Test(_)) => return None,
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
        }
    }

    /// The audio master clock underneath, for the diagnostics.
    #[must_use]
    pub fn master(&self) -> &AudioMasterClock {
        match &self.kind {
            Kind::Test(test) => test.clock.inner(),
            #[cfg(takt_libmpv)]
            Kind::File(file) => file.clock.as_ref(),
        }
    }

    fn source(&self) -> &dyn VideoBackend {
        match &self.kind {
            Kind::Test(test) => &test.source,
            #[cfg(takt_libmpv)]
            Kind::File(file) => &file.source,
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
            Kind::File(file) => file.source.next_frame(file.clock.as_ref()),
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
        let to = Timestamp::from_millis(millis.clamp(0, self.seek_limit()));
        let done = match &mut self.kind {
            Kind::Test(test) => {
                test.clock.seek(to);
                test.source.seek(to, SeekMode::Keyframe)
            }
            // The clock follows mpv to the new position by itself.
            #[cfg(takt_libmpv)]
            Kind::File(file) => file.source.seek(to, SeekMode::Keyframe),
        };
        if let Err(e) = done {
            eprintln!("takt-player: seek: {e}");
        }
    }

    /// Whether the source has arrived at the target of the last jump: a
    /// frame that comes before may be one from before the jump (mpv jumps
    /// on its own thread; the test picture is there at once).
    pub fn seek_settled(&mut self) -> bool {
        match &mut self.kind {
            Kind::Test(_) => true,
            #[cfg(takt_libmpv)]
            Kind::File(file) => !file.source.seeking(),
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
            #[cfg(takt_libmpv)]
            Kind::File(file) => {
                let track = match kind {
                    TrackKind::Audio => Track::Audio,
                    TrackKind::Subtitle => Track::Subtitle,
                };
                Some(
                    file.source
                        .cycle_track(track)
                        .unwrap_or_else(|e| e.to_string()),
                )
            }
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
        }
    }

    /// Frames of the pool that are out, and its size.
    #[must_use]
    pub fn pool(&self) -> (usize, usize) {
        let size = match &self.kind {
            Kind::Test(_) => TEST_POOL_FRAMES,
            #[cfg(takt_libmpv)]
            Kind::File(file) => file.source.pool_frames(),
        };
        (self.stats().queued, size)
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
    #[cfg(takt_libmpv)]
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
        assert!(
            session
                .source_line()
                .contains("file bt.709/bt.1886, shown as bt.709, lead"),
            "{}",
            session.source_line()
        );

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
            Kind::Test(_) => false,
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
