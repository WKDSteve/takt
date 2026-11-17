// SPDX-License-Identifier: GPL-3.0-or-later
//! Measures the software path on one file: how fast pictures come out of the helper, what the
//! copy out of the shared memory costs per picture, and how often the pool ran dry.
//!
//! ```text
//! cargo run --release -p takt-demux -p takt-decode --example decode-speed -- <file> [--semi-planar]
//!     [--pool N] [--hold-ms M] [--threads N]
//! ```
//!
//! Without `--hold-ms` the pictures are dropped as they arrive: the number is the throughput of
//! demuxer, decoder and both process boundaries, as fast as the machine goes. With it every
//! picture is held that long before its frame goes back, as a renderer that does not keep up
//! would: the pool runs dry, and the run shows that this is counted and nothing is allocated.
//!
//! Both helper programs are looked for next to this program's directory (`target/<profile>`), so
//! build `takt-demux` in the same run. The numbers of the review are in
//! `docs/reviews/phase-1.md`.
// Rates and megabytes for a person to read; no time of the pipeline is a float.
#![allow(clippy::cast_precision_loss)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use takt_decode::{Decoded, Decoder, Layout, Next, Options, Sent};
use takt_demux::{Demuxer, Event, StreamKind};

struct Args {
    file: PathBuf,
    layout: Layout,
    pool: usize,
    hold: Duration,
    threads: u32,
}

fn args() -> Option<Args> {
    let mut args = std::env::args_os().skip(1);
    let mut parsed = Args {
        file: PathBuf::new(),
        layout: Layout::Planar,
        pool: 8,
        hold: Duration::ZERO,
        threads: 0,
    };
    let number = |args: &mut dyn Iterator<Item = std::ffi::OsString>| {
        args.next()?.to_str()?.parse::<u64>().ok()
    };
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--semi-planar") => parsed.layout = Layout::SemiPlanar,
            Some("--pool") => parsed.pool = usize::try_from(number(&mut args)?).ok()?,
            Some("--hold-ms") => parsed.hold = Duration::from_millis(number(&mut args)?),
            Some("--threads") => parsed.threads = u32::try_from(number(&mut args)?).ok()?,
            _ => parsed.file = PathBuf::from(arg),
        }
    }
    (!parsed.file.as_os_str().is_empty()).then_some(parsed)
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    // `target/<profile>/examples/decode-speed`: the helpers lie one directory above.
    let exe = std::env::current_exe()?;
    let dir = exe
        .parent()
        .and_then(|dir| dir.parent())
        .ok_or("no directory above this program")?;
    let helper = |name: &str| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let mut demuxer = Demuxer::open_path(
        &args.file,
        &takt_demux::Options::new(helper(takt_demux::HELPER_NAME)),
    )?;
    let stream = demuxer
        .info()
        .streams
        .iter()
        .find(|stream| stream.kind == StreamKind::Video)
        .ok_or("the file has no picture")?
        .clone();
    let mut decoder = Decoder::open(
        &stream,
        &Options {
            layout: args.layout,
            pool_frames: args.pool,
            threads: args.threads,
            ..Options::new(helper(takt_decode::HELPER_NAME))
        },
    )?;
    let video = stream.video.clone().unwrap_or_default();
    println!(
        "{}: {} {}x{} {}, decoder {}",
        args.file.display(),
        stream.codec,
        video.width,
        video.height,
        video.pixel_format,
        decoder.decoder_name()
    );

    let started = Instant::now();
    let mut held: VecDeque<(Instant, Decoded)> = VecDeque::new();
    let mut waiting = None;
    let mut ended = false;
    let mut first_picture = None;
    let mut attempt = 0u32;
    loop {
        let mut progress = false;
        // Frames whose time with the "renderer" is over go back to the pool.
        while held
            .front()
            .is_some_and(|(since, _)| since.elapsed() >= args.hold)
        {
            held.pop_front();
        }
        while !ended && decoder.can_send() {
            let packet = match waiting.take() {
                Some(packet) => packet,
                None => match demuxer.try_next()? {
                    Some(Event::Packet(packet)) if packet.stream == stream.index => packet,
                    Some(Event::Packet(packet)) => {
                        demuxer.recycle(packet);
                        continue;
                    }
                    Some(Event::End { error }) => {
                        if let Some((code, text)) = error {
                            println!("read error: {text} ({code})");
                        }
                        ended = true;
                        decoder.drain()?;
                        break;
                    }
                    Some(Event::Dropped { size, .. }) => {
                        println!("a packet of {size} bytes was left out");
                        continue;
                    }
                    None => break,
                },
            };
            match decoder.send(&packet)? {
                Sent::Taken | Sent::TooLarge => demuxer.recycle(packet),
                Sent::WindowFull => {
                    waiting = Some(packet);
                    break;
                }
            }
            progress = true;
        }
        match decoder.try_next()? {
            Next::Frame(decoded) => {
                first_picture.get_or_insert_with(|| started.elapsed());
                if !args.hold.is_zero() {
                    held.push_back((Instant::now(), decoded));
                }
                progress = true;
            }
            Next::End => break,
            Next::Idle | Next::PoolExhausted => {}
        }
        if progress {
            attempt = 0;
        } else {
            takt_ipc::ring::pause(attempt);
            attempt = attempt.saturating_add(1);
        }
    }
    let elapsed = started.elapsed();
    let stats = decoder.stats();
    let seconds = elapsed.as_secs_f64();
    println!(
        "{} pictures in {:.3} s: {:.1} pictures/s; first picture after {:?}",
        stats.frames,
        seconds,
        stats.frames as f64 / seconds,
        first_picture.unwrap_or_default()
    );
    if stats.frames > 0 {
        let both = stats.read_nanos + stats.copy_nanos;
        println!(
            "per picture ({:.1} MiB): {} us out of the ring, {} us into the pool; both {:.2} GiB/s, {:.1} % of the run",
            stats.copied_bytes as f64 / stats.frames as f64 / 1_048_576.0,
            stats.read_nanos / stats.frames / 1_000,
            stats.copy_nanos / stats.frames / 1_000,
            2.0 * stats.copied_bytes as f64 / 1_073_741_824.0 / (both as f64 / 1e9),
            both as f64 / 1e7 / seconds
        );
    }
    println!(
        "pool of {}: empty {} times, {} pool(s) made; decode errors {}, packets {}",
        args.pool, stats.pool_exhausted, stats.pools, stats.errors, stats.packets
    );
    Ok(())
}

fn main() -> ExitCode {
    let Some(args) = args() else {
        eprintln!(
            "usage: decode-speed <file> [--semi-planar] [--pool N] [--hold-ms M] [--threads N]"
        );
        return ExitCode::from(2);
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("decode-speed: {err}");
            ExitCode::FAILURE
        }
    }
}
