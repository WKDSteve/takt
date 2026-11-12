// SPDX-License-Identifier: GPL-3.0-or-later
//! The window: egui through eframe, rendered with wgpu. A bar with play and
//! pause, the position and the volume, above it the video surface.
//!
//! The render loop is not coupled to the clock yet (task
//! `render-schleife-an-scheduler-und-uhr-koppeln`): the window repaints on
//! every display refresh and shows the position of the audio master clock.

use std::sync::Arc;

use takt_clock::{AudioMasterClock, Clock, SystemMonotonic};

use crate::app::{Command, Key, PlayerState, command_for};
use crate::audio::{self, AudioPath};
use crate::render;

/// The player in its window.
struct PlayerApp {
    state: PlayerState,
    clock: AudioMasterClock,
    // Kept for its life; dropping it stops the sound.
    _audio: AudioPath,
    audio_line: String,
}

impl PlayerApp {
    fn new() -> Self {
        let source = Arc::new(SystemMonotonic::new());
        let (clock, audio) = audio::start(&source);
        let audio_line = audio.describe();
        Self {
            state: PlayerState::default(),
            clock,
            _audio: audio,
            audio_line,
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        for command in commands(ctx) {
            self.run(ctx, command);
        }

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
            });
        });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ctx, render::video_surface);

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
        Box::new(|_cc| Ok(Box::new(PlayerApp::new()))),
    )
}
