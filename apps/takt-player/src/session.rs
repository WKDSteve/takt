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
/// Frames of the pool of the test picture.
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
}

enum Kind {
    Test(Box<Test>),
    #[cfg(takt_libmpv)]
    File(Box<File>),
}

/// What plays now, with its clock.
pub struct Session {
    time: Arc<SystemMonotonic>,
    kind: Kind,
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

    /// Plays the file `path` from its start. In a build without libmpv the
    /// test picture starts again in its place: the file is not read.
    ///
    /// # Errors
    ///
    /// A text for the user when the file cannot be played; what played
    /// before goes on then, unless it was a file that mpv has already let go.
    pub fn open_file(&mut self, path: &Path) -> Result<(), String> {
        #[cfg(takt_libmpv)]
        {
            let name = path.to_string_lossy();
            let fresh = match &mut self.kind {
                Kind::File(file) => {
                    let info = file.source.open(&name).map_err(|e| e.to_string())?;
                    file.duration_millis = duration_millis(&info);
                    file.size = info.video.map(|v| (v.width, v.height));
                    None
                }
                Kind::Test(_) => {
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
                    let info = source.open(&name).map_err(|e| e.to_string())?;
                    Some(File {
                        clock: source.clock(),
                        duration_millis: duration_millis(&info),
                        size: info.video.map(|v| (v.width, v.height)),
                        source,
                    })
                }
            };
            if let Some(file) = fresh {
                // The swap of the clock: the test picture, its clock and its
                // device are dropped here, and the clock of mpv is the one.
                self.kind = Kind::File(Box::new(file));
            }
            self.apply();
            Ok(())
        }
        #[cfg(not(takt_libmpv))]
        {
            let Kind::Test(test) = &mut self.kind;
            test.source
                .open(&path.to_string_lossy())
                .map_err(|e| e.to_string())?;
            self.seek_to(0);
            Ok(())
        }
    }

    /// Back to the test picture (the playlist is empty). Nothing happens
    /// when it plays already.
    pub fn show_test_picture(&mut self) {
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

        session.open_file(Path::new(clip)).unwrap();
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
        assert!(session.cycle_track(TrackKind::Subtitle).is_some());

        // The next file plays on the same backend and the same clock.
        session.open_file(Path::new(clip)).unwrap();
        assert_eq!(std::ptr::from_ref(session.master()), second);

        session.show_test_picture();
        assert!(!session.is_file());
        assert_eq!(session.backend_name(), "null");
        assert_ne!(std::ptr::from_ref(session.master()), second);
    }
}
