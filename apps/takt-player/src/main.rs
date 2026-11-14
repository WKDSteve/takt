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
#[cfg(takt_libmpv)]
mod mpv;
mod palette;
mod playlist;
mod refresh;
mod render;
mod theme;
mod ui;

use std::process::ExitCode;

use assert_no_alloc::AllocDisabler;

// Jede Allokation im Audio-Callback bricht den Prozess ab, im Debug und im
// Release (`assert_no_alloc` ohne `disable_release`).
#[global_allocator]
static ALLOCATOR: AllocDisabler = AllocDisabler;

fn main() -> ExitCode {
    // `--keys`: the list of keys as Markdown (the text of docs/KEYS.md).
    if std::env::args().skip(1).any(|arg| arg == "--keys") {
        print!("{}", app::keys_markdown());
        return ExitCode::SUCCESS;
    }
    let args: Vec<String> = std::env::args().collect();
    // `--headless [--fps N[/D]] [--hz N[/D]] [--seconds N]`: the measuring loop without a window.
    if args.iter().skip(1).any(|arg| arg == "--headless") {
        // 2: the options are wrong; 1: the run itself failed.
        return match headless::Options::parse(&args[1..]) {
            Ok(options) => match headless::run(&options) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("takt-player: {e}");
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("takt-player: {e}");
                ExitCode::from(2)
            }
        };
    }
    // `--seconds N`: a measuring run of the window, closed after N seconds.
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
