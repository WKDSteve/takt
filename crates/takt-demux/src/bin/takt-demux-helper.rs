// SPDX-License-Identifier: GPL-3.0-or-later
//! The demux helper of Takt: the process that parses one container file for the main program
//! (`takt_demux::helper`). Started by `takt_demux::Demuxer`, never by hand; `--about` prints the
//! FFmpeg it runs with.

fn main() -> std::process::ExitCode {
    takt_demux::helper::run()
}
