// SPDX-License-Identifier: GPL-3.0-or-later
//! Takt Player.
//!
//! Opens a window (egui through eframe, rendered with wgpu) with play and
//! pause, the position of the audio master clock and the volume. It starts
//! with a moving test picture; a file dropped on it plays through libmpv
//! (`MpvBackend`) in a build that links it (`TAKT_LIBMPV_DIR`,
//! `docs/BUILD.md`); files and folders named on the command line are the
//! playlist it starts with. `--headless` runs the measuring loop of Phase 0
//! without a window instead.
//!
//! For the test picture, sound goes to the default output device
//! (`takt-audio`, cpal) and drives the audio master clock; without a device,
//! or with `TAKT_AUDIO=sim`, a thread stands in for the callback and the
//! player says so. For a file, mpv plays the sound and the clock follows it
//! (`session`).

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
mod session;
mod theme;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use assert_no_alloc::AllocDisabler;

// Jede Allokation im Audio-Callback bricht den Prozess ab, im Debug und im
// Release (`assert_no_alloc` ohne `disable_release`).
#[global_allocator]
static ALLOCATOR: AllocDisabler = AllocDisabler;

/// The files named on the command line: every argument that is no option
/// and not the value of `--seconds`. After `--` everything is a file, also
/// a name that starts with a dash.
fn files(args: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" => {
                files.extend(args.by_ref().map(PathBuf::from));
            }
            "--seconds" => {
                args.next();
            }
            option if option.starts_with("--") => {}
            file => files.push(PathBuf::from(file)),
        }
    }
    files
}

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
    match ui::run(seconds, overlay, files(&args[1..])) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("takt-player: no window: {e} (--headless runs without one)");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|&a| a.to_owned()).collect()
    }

    #[test]
    fn files_are_what_is_no_option_and_no_value_of_one() {
        let named = files(&args(&[
            "--overlay",
            "a film.mkv",
            "--seconds",
            "8",
            "folder",
            "--",
            "--odd name.mp4",
            "--seconds",
        ]));
        assert_eq!(
            named,
            ["a film.mkv", "folder", "--odd name.mp4", "--seconds"].map(PathBuf::from)
        );
        assert!(files(&args(&["--seconds", "5", "--overlay"])).is_empty());
        assert!(files(&[]).is_empty());
    }
}
