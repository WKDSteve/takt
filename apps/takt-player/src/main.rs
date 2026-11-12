// SPDX-License-Identifier: GPL-3.0-or-later
//! Takt Player.
//!
//! Opens a window (egui through eframe, rendered with wgpu) with play and
//! pause, the position of the audio master clock and the volume; the video
//! surface stays empty until the NV12 upload comes. `--headless` runs the
//! measuring loop of Phase 0 without a window instead.
//!
//! Sound goes to the default output device (`takt-audio`, cpal) and drives the
//! audio master clock; without a device, or with `TAKT_AUDIO=sim`, a thread
//! stands in for the callback and the player says so.

mod app;
mod audio;
mod diagnostics;
mod headless;
mod refresh;
mod render;
mod ui;

use std::process::ExitCode;

use assert_no_alloc::AllocDisabler;

// Jede Allokation im Audio-Callback bricht den Prozess ab, im Debug und im
// Release (`assert_no_alloc` ohne `disable_release`).
#[global_allocator]
static ALLOCATOR: AllocDisabler = AllocDisabler;

fn main() -> ExitCode {
    if std::env::args().skip(1).any(|arg| arg == "--headless") {
        headless::run();
        return ExitCode::SUCCESS;
    }
    // `--seconds N`: a measuring run of the window, closed after N seconds.
    let args: Vec<String> = std::env::args().collect();
    let seconds = args
        .iter()
        .position(|arg| arg == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse().ok());
    // `--overlay`: start with the diagnostic overlay shown (F3 toggles it).
    let overlay = args.iter().any(|arg| arg == "--overlay");
    match ui::run(seconds, overlay) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("takt-player: no window: {e} (--headless runs without one)");
            ExitCode::FAILURE
        }
    }
}
