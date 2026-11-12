// SPDX-License-Identifier: GPL-3.0-or-later
//! The window: egui through eframe, rendered with wgpu. A bar with play and
//! pause, the position and the volume, above it the video surface.
//!
//! The source is the moving test picture of [`NullBackend`] (NV12, BT.709,
//! limited range), drawn through the colour path of [`render`]. Every display
//! refresh takes the frame that is due on the audio master clock; scheduler,
//! measurements and seeking come with the render loop (task
//! `render-schleife-an-scheduler-und-uhr-koppeln`).

use std::sync::Arc;

use takt_backend::{NullBackend, Pattern, VideoBackend};
use takt_clock::{AudioMasterClock, Clock, SystemMonotonic};
use takt_core::{
    ColorPrimaries, ColorRange, FrameMeta, PixelFormat, Rational, Timestamp, TransferFunction,
};

use crate::app::{Command, Key, PlayerState, command_for};
use crate::audio::{self, AudioPath};
use crate::render::{self, Shown, gpu::Nv12Image};

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
        Timestamp::from_millis(3_600_000),
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
    clock: AudioMasterClock,
    // Kept for its life; dropping it stops the sound.
    _audio: AudioPath,
    audio_line: String,
    source: NullBackend,
    shown: Option<Shown>,
}

impl PlayerApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        if let Some(state) = cc.wgpu_render_state.as_ref() {
            render::install(state);
        }
        let time = Arc::new(SystemMonotonic::new());
        let (clock, audio) = audio::start(&time);
        let audio_line = audio.describe();
        Self {
            state: PlayerState::default(),
            clock,
            _audio: audio,
            audio_line,
            source: test_source(),
            shown: None,
        }
    }

    /// Takes the frame that is due now, if there is a new one.
    fn take_due_frame(&mut self) {
        match self.source.next_frame(&self.clock) {
            Ok(Some(frame)) => match Nv12Image::from_frame(frame.get()) {
                Ok(image) => {
                    let generation = self.shown.as_ref().map_or(0, |s| s.generation + 1);
                    self.shown = Some(Shown {
                        image: Arc::new(image),
                        generation,
                    });
                }
                Err(e) => eprintln!("takt-player: frame not shown: {e}"),
            },
            Ok(None) => {}
            Err(e) => eprintln!("takt-player: source: {e}"),
        }
    }

    fn run(&mut self, ctx: &egui::Context, command: Command) {
        if self.state.apply(command) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.clock.set_paused(!self.state.playing());
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

/// Opens the window and runs until it closes.
///
/// # Errors
///
/// When no window or no wgpu device could be created.
pub fn run() -> eframe::Result {
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
        Box::new(|cc| Ok(Box::new(PlayerApp::new(cc)))),
    )
}
