// SPDX-License-Identifier: GPL-3.0-or-later
//! The window: egui through eframe, rendered with wgpu. A bar with play and
//! pause, the position and the volume, above it the video surface.
//!
//! The source is the moving test picture of [`NullBackend`] (NV12, BT.709,
//! limited range), drawn through the colour path of [`render`].
//!
//! # The render loop
//!
//! eframe calls [`eframe::App::update`] once per display refresh (vsync, no
//! sleep anywhere). Each call asks the source for the frame that is due on the
//! clock and gives the answer to the [`Scheduler`], exactly as the headless
//! loop does, so the measurements of both can be compared. While paused,
//! nothing is asked and nothing counted. The clock is the audio master clock
//! wrapped in a [`SeekableClock`]: a jump moves the media position, resets
//! source and scheduler (their numbers are about the old position), and the
//! device plays on.
//!
//! The source learns the display period ([`RefreshMeter`], the median of the
//! passes), so a frame is due at the refresh nearest its timestamp.

use std::sync::Arc;

use std::time::{Duration, Instant};

use takt_backend::{NullBackend, Pattern, Presentation, Scheduler, SeekMode, VideoBackend};
use takt_clock::{AudioMasterClock, Clock, SeekableClock, SystemMonotonic};
use takt_core::{
    ColorPrimaries, ColorRange, FrameMeta, PixelFormat, Rational, Timestamp, TransferFunction,
};

use crate::app::{Command, Key, PlayerState, command_for, seek_target};
use crate::audio::{self, AudioPath};
use crate::refresh::RefreshMeter;
use crate::render::{self, Shown, gpu::Nv12Image};

/// Length of the test source.
const SOURCE_MILLIS: i64 = 3_600_000;

/// The test source: the moving block of `NullBackend`, 1920x1080 NV12 at the
/// NTSC rate, one hour long.
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
        Timestamp::from_millis(SOURCE_MILLIS),
        Pattern::Motion,
        6,
    );
    // NullBackend opens any name.
    let _ = backend.open("null://motion");
    backend
}

/// The player in its window.
struct PlayerApp {
    state: PlayerState,
    clock: SeekableClock<AudioMasterClock>,
    scheduler: Scheduler,
    /// `--seconds N`: close after N seconds (a measuring run).
    close_at: Option<Instant>,
    // Kept for its life; dropping it stops the sound.
    _audio: AudioPath,
    audio_line: String,
    source: NullBackend,
    shown: Option<Shown>,
    refresh: RefreshMeter,
    started: Instant,
}

impl PlayerApp {
    fn new(cc: &eframe::CreationContext<'_>, seconds: Option<u64>) -> Self {
        if let Some(state) = cc.wgpu_render_state.as_ref() {
            render::install(state);
        }
        let time = Arc::new(SystemMonotonic::new());
        let (clock, audio) = audio::start(&time);
        let audio_line = audio.describe();
        Self {
            state: PlayerState::default(),
            clock: SeekableClock::new(clock),
            scheduler: Scheduler::new(),
            close_at: seconds.map(|s| Instant::now() + Duration::from_secs(s)),
            _audio: audio,
            audio_line,
            source: test_source(),
            shown: None,
            refresh: RefreshMeter::default(),
            started: Instant::now(),
        }
    }

    /// One pass of the render loop: the frame that is due now, judged by the
    /// scheduler; a new picture when it is shown.
    fn take_due_frame(&mut self) {
        let elapsed = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.refresh.pass(elapsed);
        self.source.set_display_period(self.refresh.period_nanos());
        if !self.state.playing() {
            return;
        }
        let frame = match self.source.next_frame(&self.clock) {
            Ok(frame) => frame,
            Err(e) => {
                eprintln!("takt-player: source: {e}");
                return;
            }
        };
        let eof = self.source.is_eof();
        if !matches!(
            self.scheduler.accept(&self.clock, frame.as_ref(), eof),
            Presentation::Shown { .. }
        ) {
            return;
        }
        if let Some(frame) = frame {
            match Nv12Image::from_frame(frame.get()) {
                Ok(image) => {
                    let generation = self.shown.as_ref().map_or(0, |s| s.generation + 1);
                    self.shown = Some(Shown {
                        image: Arc::new(image),
                        generation,
                    });
                }
                Err(e) => eprintln!("takt-player: frame not shown: {e}"),
            }
        }
    }

    /// Jumps to `millis`: clock, source and scheduler.
    fn seek_to(&mut self, millis: i64) {
        let to = Timestamp::from_millis(millis);
        self.clock.seek(to);
        if let Err(e) = self.source.seek(to, SeekMode::Keyframe) {
            eprintln!("takt-player: seek: {e}");
        }
        self.scheduler.reset();
    }

    fn now_millis(&self) -> i64 {
        self.clock
            .now()
            .rescale(Rational::MILLIS)
            .map_or(0, Timestamp::ticks)
    }

    fn run(&mut self, ctx: &egui::Context, command: Command) {
        if self.state.apply(command) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        match command {
            Command::SeekBy(by) => self.seek_to(seek_target(self.now_millis(), by, SOURCE_MILLIS)),
            Command::SeekStart => self.seek_to(0),
            Command::TogglePlay | Command::Quit => {}
        }
        self.clock.set_paused(!self.state.playing());
    }

    /// The measurements, as the headless run prints them.
    fn summary(&self) -> String {
        let s = self.scheduler.stats();
        #[expect(clippy::cast_precision_loss, reason = "a display value, not time")]
        let mean = s.mean_abs_error_nanos() as f64 / 1e6;
        #[expect(clippy::cast_precision_loss, reason = "a display value, not time")]
        let refresh = self
            .refresh
            .period_nanos()
            .map_or("-".to_owned(), |p| format!("{:.1} Hz", 1e9 / p as f64));
        format!(
            "shown {}, repeated {} ({:.1} %), dropped {}, mean error {mean:.3} ms, display {refresh}",
            s.shown,
            s.repeated,
            s.repeat_ratio(),
            self.source.stats().dropped
        )
    }
}

/// The keys of this frame, as commands, in the order they came.
fn commands(ctx: &egui::Context) -> Vec<Command> {
    ctx.input(|input| {
        input
            .events
            .iter()
            .filter_map(|event| match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    ..
                } => match key {
                    egui::Key::Space => Some(command_for(Key::Space)),
                    egui::Key::Escape => Some(command_for(Key::Escape)),
                    egui::Key::ArrowLeft => Some(command_for(Key::Left)),
                    egui::Key::ArrowRight => Some(command_for(Key::Right)),
                    egui::Key::Home => Some(command_for(Key::Home)),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    })
}

impl eframe::App for PlayerApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        for command in commands(ctx) {
            self.run(ctx, command);
        }
        if self.close_at.is_some_and(|at| Instant::now() >= at) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.take_due_frame();
        let uploads = frame.wgpu_render_state().and_then(render::uploads);

        egui::TopBottomPanel::bottom("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let label = if self.state.playing() {
                    "Pause"
                } else {
                    "Play"
                };
                if ui.button(label).on_hover_text("Space").clicked() {
                    self.run(ctx, Command::TogglePlay);
                }
                ui.monospace(self.clock.now().format_hms());
                ui.separator();
                let mut volume = self.state.volume();
                // The source has no sound yet, so the volume is only kept.
                if ui
                    .add(egui::Slider::new(&mut volume, 0.0..=1.0).text("Volume"))
                    .changed()
                {
                    self.state.set_volume(volume);
                }
                ui.separator();
                ui.weak(self.summary());
                ui.separator();
                ui.weak(&self.audio_line);
                if let Some(u) = uploads.filter(|u| u.total() > 0) {
                    ui.separator();
                    let p50 = u.quantile_micros(1, 2).unwrap_or(0);
                    ui.weak(format!(
                        "upload p50 <{p50} us, max {} us ({} frames)",
                        u.max_nanos() / 1_000,
                        u.total()
                    ));
                }
            });
        });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, |ui| render::video_surface(ui, self.shown.as_ref()));

        // The position moves on: repaint with the next display refresh.
        ctx.request_repaint();
    }
}

impl Drop for PlayerApp {
    /// The measurements of the run, when the window closes.
    fn drop(&mut self) {
        println!("takt-player: {}", self.summary());
    }
}

/// Opens the window and runs until it closes, or for `seconds`.
///
/// # Errors
///
/// When no window or no wgpu device could be created.
pub fn run(seconds: Option<u64>) -> eframe::Result {
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title("Takt")
            .with_inner_size([960.0, 600.0])
            .with_min_inner_size([320.0, 200.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Takt",
        options,
        Box::new(move |cc| Ok(Box::new(PlayerApp::new(cc, seconds)))),
    )
}
