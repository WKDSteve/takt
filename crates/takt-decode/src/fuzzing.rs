// SPDX-License-Identifier: GPL-3.0-or-later
//! The reading side of the decoder's protocol as one function over arbitrary bytes: what the fuzz
//! target (`fuzz/fuzz_targets/decode_reader.rs`) runs.
//!
//! It lives here and not in the fuzz crate so that it is compiled, linted and run by the ordinary
//! tests (with the inputs of [`seeds`]) on every platform; the fuzz target is one call. Everything
//! the main program does with a record of the decode helper goes through it, with the same
//! [`Intake`] the real decoder uses: the head of a frame, the copy of its planes into a frame of
//! the pool, the counts of the window. The frames of the pipe and the ring itself are the business
//! of `takt_demux::fuzzing`, which feeds the same `takt-ipc`.
//!
//! A panic in here is a finding. Besides "does not panic" it checks what the parsers promise:
//! nothing larger than the limits comes out, a picture lands in a frame of its own size, and the
//! pool never holds more frames than it was made for.

use takt_core::{PixelFormat, Rational};

use crate::intake::{Intake, Taken};
use crate::proto::{
    self, Config, Configured, FrameHead, Layout, MAX_DIMENSION, MAX_ERROR_TEXT, MAX_EXTRADATA,
    MAX_THREADS, PacketHead, WireFormat, end_tail, error_tail, parse_flush, parse_packet,
    status_record,
};

/// Frames of the pools the harness lets an intake make.
const POOL: usize = 2;
/// Packets the intake of the harness has sent: what a count may reach.
const SENT: u64 = 8;

/// Feeds `data` to the reading side. The first byte picks the entry: 0 one record into planar
/// frames, 1 one record into semi-planar frames, 2 a row of records (each with a `u16` length in
/// front) into one decoder, 3 the answer to a configuration, 4 what the helper reads (a
/// configuration, a packet, a flush).
///
/// # Panics
/// When a parser breaks one of its promises: that is what a fuzzer is looking for.
pub fn read_untrusted(data: &[u8]) {
    let Some((&entry, rest)) = data.split_first() else {
        return;
    };
    match entry % 5 {
        0 => drop(record(&mut intake(Layout::Planar), rest)),
        1 => drop(record(&mut intake(Layout::SemiPlanar), rest)),
        2 => records(rest),
        3 => answer(rest),
        _ => requests(rest),
    }
}

fn intake(layout: Layout) -> Intake {
    let mut intake = Intake::new(Rational::lit(1, 12_800), layout, POOL);
    for _ in 0..SENT {
        intake.sent();
    }
    intake
}

/// One record into `intake`; what came out of it, for the caller to hold or drop.
fn record(intake: &mut Intake, bytes: &[u8]) -> Option<Taken> {
    let before = intake.stats();
    if !intake.ready() {
        // Every frame is out: the record stays in the ring.
        assert_eq!(intake.pool().0, POOL);
        return None;
    }
    let taken = intake.accept(bytes).ok()?;
    let after = intake.stats();
    assert!(intake.in_flight() <= SENT);
    assert!(intake.pool().0 <= POOL);
    if let Taken::Frame(decoded) = &taken {
        let frame = decoded.frame.get();
        let meta = frame.meta();
        assert!((1..=MAX_DIMENSION).contains(&meta.width));
        assert!((1..=MAX_DIMENSION).contains(&meta.height));
        assert!(matches!(
            meta.format,
            PixelFormat::Yuv420P
                | PixelFormat::Yuv420P10
                | PixelFormat::Yuv444P
                | PixelFormat::Nv12
                | PixelFormat::P010
        ));
        // The picture came out of this record: a frame cannot be larger than what was read,
        // but for the padding of its rows and the widening of nothing (the formats keep their
        // sample size).
        let used: usize = (0..meta.format.planes())
            .map(|plane| frame.row_bytes(plane).unwrap_or(0))
            .sum();
        assert!(used <= bytes.len());
        assert_eq!(after.frames, before.frames + 1);
        // A timestamp can be compared and rescaled without a panic.
        let _ = meta.pts.rescale(Rational::NANOS);
        let _ = meta.pts.cmp(&takt_core::Timestamp::from_millis(0));
        let _ = meta.pts.format_hms();
    } else {
        assert_eq!(after.frames, before.frames);
    }
    if let Some((code, text)) = intake.last_error() {
        assert_ne!(*code, 0);
        assert!(text.len() <= MAX_ERROR_TEXT);
        assert!(!text.chars().any(char::is_control));
    }
    Some(taken)
}

/// Records one after the other into one decoder, as a ring delivers them. The pictures are held
/// for a while, so the pool runs dry and frames come back in between.
fn records(mut data: &[u8]) {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    data = rest;
    let layout = if mode & 1 == 0 {
        Layout::Planar
    } else {
        Layout::SemiPlanar
    };
    let mut intake = intake(layout);
    let mut held = Vec::new();
    // A row of at most 64 records: the input is bounded, and so is the loop.
    for _ in 0..64 {
        let Some((len, rest)) = data.split_at_checked(2) else {
            return;
        };
        let len = usize::from(u16::from_le_bytes([len[0], len[1]]));
        let Some((bytes, rest)) = rest.split_at_checked(len) else {
            return;
        };
        data = rest;
        // A consumer that gives its frames back early, or one that holds them until none is left.
        if (mode & 2 != 0 && held.len() >= POOL) || held.len() >= 8 {
            held.clear();
        }
        if !intake.ready() {
            // The pool is empty and stays so until a frame comes back: the stall is counted once
            // however often it is looked at, and the record stays where it is.
            let stalls = intake.stats().pool_exhausted;
            assert!(!intake.ready());
            assert_eq!(intake.stats().pool_exhausted, stalls);
            held.clear();
            assert!(intake.ready(), "the frames came back");
        }
        if let Some(Taken::Frame(decoded)) = record(&mut intake, bytes) {
            held.push(decoded);
        }
        if mode & 4 != 0 {
            intake.flush();
        }
    }
}

fn answer(payload: &[u8]) {
    if let Ok(configured) = Configured::parse(payload) {
        assert!(!configured.decoder.is_empty());
        assert!(configured.decoder.bytes().all(|b| b.is_ascii_graphic()));
        assert_eq!(Configured::parse(&configured.encode()), Ok(configured));
    }
}

/// What the helper reads: it checks its parent the same way.
fn requests(payload: &[u8]) {
    if let Ok(config) = Config::parse(payload) {
        assert!(config.extradata.len() <= MAX_EXTRADATA);
        assert!(config.width <= MAX_DIMENSION && config.height <= MAX_DIMENSION);
        assert!(config.threads <= MAX_THREADS);
        assert_eq!(Config::parse(&config.encode()), Ok(config));
    }
    if let Ok((data, head)) = parse_packet(payload) {
        assert_eq!(data.len() + proto::PACKET_TAIL, payload.len());
        assert!(head.duration.is_none_or(|d| d >= 0));
        for time in [head.pts, head.dts].into_iter().flatten() {
            assert_ne!(time, i64::MIN);
        }
        let mut again = data.to_vec();
        again.extend_from_slice(&head.tail());
        assert_eq!(parse_packet(&again), Ok((data, head)));
    }
    if let Ok(serial) = parse_flush(payload) {
        assert_eq!(parse_flush(&proto::flush(serial)), Ok(serial));
    }
}

/// A frame record of a small picture whose samples count up.
#[must_use]
pub fn sample_frame(format: WireFormat, serial: u32, consumed: u64) -> Vec<u8> {
    let (width, height) = (6, 4);
    let mut head = FrameHead {
        serial,
        consumed,
        pts: Some(512 * i64::try_from(consumed).unwrap_or(0)),
        width,
        height,
        format,
        stride: [0; 3],
        range: 1,
        primaries: 1,
        transfer: 1,
        matrix: 1,
        key: consumed == 1,
        corrupt: false,
    };
    // A few bytes of padding behind every row, as a decoder leaves them.
    head.stride = [0, 1, 2].map(|plane| {
        let (row, _) = format.plane(plane, width, height);
        u32::try_from(row + 2).unwrap_or(u32::MAX)
    });
    let mut record: Vec<u8> = (0..head.body_len())
        .map(|at| u8::try_from(at % 251).unwrap_or(0))
        .collect();
    record.extend_from_slice(&head.tail());
    record
}

/// Well-formed inputs for [`read_untrusted`], one per entry and message: where a fuzzer starts.
/// Each is a name and the bytes.
#[must_use]
pub fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let with = |entry: u8, bytes: &[u8]| {
        let mut out = vec![entry];
        out.extend_from_slice(bytes);
        out
    };
    let mut error = b"Invalid data found when processing input".to_vec();
    error.extend_from_slice(&error_tail(0, 2, -1_094_995_529));

    // A row as a decoder writes it: pictures, a count, an error, a flush, pictures again, the end.
    let mut row = vec![2u8, 2];
    let mut push = |record: &[u8]| {
        row.extend_from_slice(&u16::try_from(record.len()).unwrap_or(0).to_le_bytes());
        row.extend_from_slice(record);
    };
    push(&sample_frame(WireFormat::Yuv420P8, 0, 1));
    push(&status_record(0, 2));
    push(&error);
    push(&sample_frame(WireFormat::Yuv420P8, 0, 3));
    push(&sample_frame(WireFormat::Yuv420P10, 0, 4));
    push(&sample_frame(WireFormat::Yuv444P8, 0, 5));
    push(&end_tail(0, 5));

    let config = Config {
        codec: "h264".to_owned(),
        extradata: vec![1, 100, 0, 10, 255, 225, 0, 0],
        width: 1920,
        height: 1080,
        profile: 100,
        level: 40,
        threads: 0,
    };
    let head = PacketHead {
        serial: 1,
        pts: Some(1024),
        dts: Some(512),
        duration: Some(512),
        key: true,
        corrupt: false,
        discard: false,
    };
    let mut packet = b"compressed data".to_vec();
    packet.extend_from_slice(&head.tail());
    let configured = Configured {
        decoder: "libdav1d".to_owned(),
    };
    vec![
        (
            "frame-planar",
            with(0, &sample_frame(WireFormat::Yuv420P8, 0, 1)),
        ),
        (
            "frame-planar-10",
            with(0, &sample_frame(WireFormat::Yuv420P10, 0, 1)),
        ),
        (
            "frame-nv12",
            with(1, &sample_frame(WireFormat::Yuv420P8, 0, 1)),
        ),
        (
            "frame-p010",
            with(1, &sample_frame(WireFormat::Yuv420P10, 0, 1)),
        ),
        (
            "frame-444",
            with(1, &sample_frame(WireFormat::Yuv444P8, 0, 1)),
        ),
        ("status", with(0, &status_record(0, 3))),
        ("end", with(0, &end_tail(0, 3))),
        ("error", with(0, &error)),
        ("row", row),
        ("configured", with(3, &configured.encode())),
        ("config", with(4, &config.encode())),
        ("packet", with(4, &packet)),
        ("flush", with(4, &proto::flush(7))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{Record, parse_record};

    #[test]
    fn the_seeds_are_read_and_accepted() {
        let seeds = seeds();
        assert_eq!(seeds.len(), 13);
        for (name, bytes) in &seeds {
            assert!(bytes.len() > 1, "{name}");
            read_untrusted(bytes);
        }
        // The seeds really are well-formed: the parsers take them, they are not merely survived.
        for (name, bytes) in &seeds[..5] {
            let mut intake = intake(Layout::Planar);
            assert!(
                matches!(record(&mut intake, &bytes[1..]), Some(Taken::Frame(_))),
                "{name}"
            );
        }
        for (name, bytes) in &seeds[5..8] {
            assert!(parse_record(&bytes[1..]).is_ok(), "{name}");
        }
        assert!(matches!(
            parse_record(&seeds[7].1[1..]),
            Ok((_, _, Record::Error(..)))
        ));
        assert!(Configured::parse(&seeds[9].1[1..]).is_ok());
        assert!(Config::parse(&seeds[10].1[1..]).is_ok());
        assert!(parse_packet(&seeds[11].1[1..]).is_ok());
        assert_eq!(parse_flush(&seeds[12].1[1..]), Ok(7));
    }

    #[test]
    fn the_row_of_records_fills_and_empties_the_pool() {
        // The row seed: seven records, four of them pictures in three different formats, each of
        // which makes a pool of its own.
        let row = &seeds()[8].1;
        let mut intake = intake(Layout::SemiPlanar);
        let mut data = &row[2..];
        let mut frames = 0;
        while let Some((len, rest)) = data.split_at_checked(2) {
            let len = usize::from(u16::from_le_bytes([len[0], len[1]]));
            let (bytes, rest) = rest.split_at(len);
            data = rest;
            if let Some(Taken::Frame(_)) = record(&mut intake, bytes) {
                frames += 1;
            }
        }
        assert_eq!(frames, 4);
        assert_eq!(intake.stats().errors, 1);
        assert_eq!(intake.stats().pools, 3);
        assert_eq!(intake.in_flight(), SENT - 5);
    }

    #[test]
    fn every_cut_and_every_flipped_byte_of_a_seed_is_survived() {
        for (_name, bytes) in seeds() {
            for cut in 0..bytes.len() {
                read_untrusted(&bytes[..cut]);
            }
            for at in 0..bytes.len() {
                for flip in [0x01, 0x80, 0xFF] {
                    let mut damaged = bytes.clone();
                    damaged[at] ^= flip;
                    read_untrusted(&damaged);
                }
            }
        }
    }

    #[test]
    fn a_seed_can_be_written_out_for_the_fuzzer() {
        // `TAKT_FUZZ_SEEDS_DECODE=<dir>` writes the corpus the fuzz run starts from
        // (fuzz/README.md).
        let Some(dir) = std::env::var_os("TAKT_FUZZ_SEEDS_DECODE") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in seeds() {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
    }
}
