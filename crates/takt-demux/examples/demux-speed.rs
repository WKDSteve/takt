// SPDX-License-Identifier: GPL-3.0-or-later
//! Reads a file through the demuxer and says what it is and how fast it came through: the streams,
//! then packets, bytes and megabytes per second from the helper process to this one.
//!
//! ```text
//! cargo run --release -p takt-demux --example demux-speed -- film.mkv
//! ```
//!
//! Needs a helper built with FFmpeg (`FFMPEG_DIR`, `docs/BUILD.md`). The helper is looked for next
//! to this program and one directory above (where cargo puts it); `TAKT_DEMUX_HELPER` names another.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use takt_demux::{Demuxer, Event, HELPER_NAME, Options};

fn helper() -> PathBuf {
    if let Some(path) = std::env::var_os("TAKT_DEMUX_HELPER") {
        return PathBuf::from(path);
    }
    let name = format!("{HELPER_NAME}{}", std::env::consts::EXE_SUFFIX);
    let exe = std::env::current_exe().unwrap_or_default();
    let beside = exe.parent().map(|dir| dir.join(&name));
    let above = exe
        .parent()
        .and_then(|dir| dir.parent())
        .map(|dir| dir.join(&name));
    [beside, above]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

fn main() -> ExitCode {
    let Some(file) = std::env::args_os().nth(1) else {
        eprintln!("usage: demux-speed <file>");
        return ExitCode::from(2);
    };
    let started = Instant::now();
    let mut demuxer = match Demuxer::open_path(&file, &Options::new(helper())) {
        Ok(demuxer) => demuxer,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let opened = started.elapsed();
    let info = demuxer.info().clone();
    println!(
        "{}, {} streams, {}, opened in {opened:.1?}",
        info.format,
        info.streams.len(),
        info.duration
            .map_or_else(|| "length unknown".to_owned(), |d| d.to_string()),
    );
    for stream in &info.streams {
        println!(
            "  {} {:?} {} time base {}",
            stream.index, stream.kind, stream.codec, stream.time_base
        );
    }

    let mut per_stream = vec![(0u64, 0u64); info.streams.len()];
    let mut dropped = 0u64;
    let reading = Instant::now();
    let end = loop {
        match demuxer.next(Duration::from_secs(30)) {
            Ok(Some(Event::Packet(packet))) => {
                if let Some(count) = per_stream.get_mut(packet.stream as usize) {
                    count.0 += 1;
                    count.1 += packet.data.len() as u64;
                }
                demuxer.recycle(packet);
            }
            Ok(Some(Event::Dropped { .. })) => dropped += 1,
            Ok(Some(Event::End { error })) => break error,
            Ok(None) => {
                eprintln!("no packet within 30 s");
                return ExitCode::FAILURE;
            }
            Err(err) => {
                eprintln!("{err}");
                return ExitCode::FAILURE;
            }
        }
    };
    let took = reading.elapsed();
    let packets: u64 = per_stream.iter().map(|count| count.0).sum();
    let bytes: u64 = per_stream.iter().map(|count| count.1).sum();
    for (index, (count, size)) in per_stream.iter().enumerate() {
        println!("  stream {index}: {count} packets, {size} bytes");
    }
    // Whole megabytes and milliseconds: a rate for a person to read, not a time to compute with.
    let millis = took.as_millis().max(1);
    println!(
        "{packets} packets, {bytes} bytes in {took:.1?}: {} MiB/s, {} packets/s, {dropped} dropped",
        u128::from(bytes) * 1000 / millis / (1 << 20),
        u128::from(packets) * 1000 / millis,
    );
    if let Some((code, text)) = end {
        eprintln!("read error {code}: {text}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
