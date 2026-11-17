// SPDX-License-Identifier: GPL-3.0-or-later
//! The reading side of the protocol as one function over arbitrary bytes: what the fuzz target
//! (`fuzz/fuzz_targets/protocol_reader.rs`) runs.
//!
//! It lives here and not in the fuzz crate so that it is compiled, linted and run by the ordinary
//! tests (with the inputs of [`seeds`]) on every platform; the fuzz target is one call. Everything
//! the main program does with bytes of the helper goes through it: the frames of the pipe and the
//! messages in them, the counters and record heads of the ring, the records themselves.
//!
//! A panic in here is a finding. Besides "does not panic" it checks what the parsers promise:
//! nothing larger than the limits comes out, and what was accepted survives being sent again.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use takt_core::Rational;
use takt_ipc::Region;
use takt_ipc::frame::{FrameDecoder, HEADER_LEN, encode};
use takt_ipc::message::{Fault, Hello, MAX_FAULT_TEXT, Progress, kind as core_kind};
use takt_ipc::ring::{self, RingReader, RingWriter, WORD_HEAD, WORD_TAIL};

use crate::proto::{
    self, AudioParams, MAX_ERROR_TEXT, MAX_EXTRADATA, MAX_STREAMS, MediaInfo, PACKET_TAIL,
    PacketHead, Record, Seek, StreamInfo, StreamKind, VideoParams, dropped_record, end_tail,
    parse_record,
};

/// Largest frame the harness lets the decoder take.
const MAX_FRAME: usize = 4096;
/// Bytes of the ring the harness builds.
const RING: usize = 256;
/// Largest record of that ring.
const MAX_RECORD: usize = 128;

/// Feeds `data` to the reading side. The first byte picks the entry: 0 the pipe (the next byte is
/// the size of the chunks the stream arrives in), 1 a stream table, 2 a ring (eight bytes of the
/// writer's counter, then the data area), 3 one record, 4 a ring whose reader lies to the writer.
///
/// # Panics
/// When a parser breaks one of its promises: that is what a fuzzer is looking for.
pub fn read_untrusted(data: &[u8]) {
    let Some((&entry, rest)) = data.split_first() else {
        return;
    };
    match entry % 5 {
        0 => pipe(rest),
        1 => table(rest),
        2 => ring_reader(rest),
        3 => record(rest),
        _ => ring_writer(rest),
    }
}

/// The streams the records of the harness refer to.
fn streams() -> Vec<StreamInfo> {
    sample_table().streams
}

fn pipe(data: &[u8]) {
    let Some((&chunk, stream)) = data.split_first() else {
        return;
    };
    let chunk = usize::from(chunk % 64) + 1;
    let mut decoder = FrameDecoder::new(MAX_FRAME);
    for part in stream.chunks(chunk) {
        decoder.push(part);
        loop {
            match decoder.next() {
                Ok(Some(frame)) => {
                    assert!(frame.payload.len() <= MAX_FRAME);
                    message(frame.kind, &frame.payload);
                }
                Ok(None) => break,
                Err(_) => {
                    assert_eq!(decoder.buffered(), 0);
                    return;
                }
            }
        }
        // One frame at most waits for its rest.
        assert!(decoder.buffered() < HEADER_LEN + MAX_FRAME + chunk);
    }
}

fn message(kind: u16, payload: &[u8]) {
    match kind {
        core_kind::HELLO => {
            if let Ok(hello) = Hello::parse(payload) {
                let _ = hello.accept(proto::APP);
                assert_eq!(Hello::parse(&hello.encode()), Ok(hello));
            }
        }
        core_kind::PROGRESS => {
            if let Ok(progress) = Progress::parse(payload) {
                assert!(progress.total == 0 || progress.done <= progress.total);
            }
        }
        core_kind::FAULT => {
            if let Ok(fault) = Fault::parse(payload) {
                assert!(fault.text.len() <= MAX_FAULT_TEXT);
                assert!(!fault.text.chars().any(char::is_control));
                assert_eq!(Fault::parse(&fault.encode()), Ok(fault));
            }
        }
        proto::kind::OPENED => table(payload),
        proto::kind::SEEK => {
            if let Ok(seek) = Seek::parse(payload) {
                assert_eq!(Seek::parse(&seek.encode()), Ok(seek));
            }
        }
        _ => {}
    }
}

fn table(payload: &[u8]) {
    let Ok(info) = MediaInfo::parse(payload) else {
        return;
    };
    assert!(info.streams.len() <= MAX_STREAMS);
    for (position, stream) in info.streams.iter().enumerate() {
        assert_eq!(stream.index as usize, position);
        assert!(stream.time_base.num() > 0 && stream.time_base.den() > 0);
        assert!(stream.extradata.len() <= MAX_EXTRADATA);
        assert!(!stream.title.chars().any(char::is_control));
        assert!(!stream.language.chars().any(char::is_control));
        assert_eq!(stream.video.is_some(), stream.kind == StreamKind::Video);
        assert_eq!(stream.audio.is_some(), stream.kind == StreamKind::Audio);
        // Times of a stream can be compared and rescaled without a panic.
        if let (Some(start), Some(duration)) = (stream.start, stream.duration) {
            let _ = start.cmp(&duration);
            let _ = start.rescale(Rational::NANOS);
            let _ = duration.format_hms();
        }
    }
    // What the reader accepted, a sender can send, and it reads the same again.
    assert_eq!(MediaInfo::parse(&info.encode()).as_ref(), Ok(&info));
}

fn checked_record(bytes: &mut Vec<u8>, streams: &[StreamInfo]) {
    let len = bytes.len();
    let Ok((_serial, record)) = parse_record(bytes, streams) else {
        return;
    };
    match record {
        Record::Packet(packet) => {
            assert_eq!(packet.data.len() + PACKET_TAIL, len);
            let stream = &streams[packet.stream as usize];
            for time in [packet.pts, packet.dts, packet.duration]
                .into_iter()
                .flatten()
            {
                assert_eq!(time.base(), stream.time_base);
                assert_ne!(time.ticks(), i64::MIN);
                let _ = time.rescale(Rational::MICROS);
                let _ = time.format_hms();
            }
            if let (Some(pts), Some(dts)) = (packet.pts, packet.dts) {
                let _ = pts.diff(dts);
            }
            assert!(packet.duration.is_none_or(|d| d.ticks() >= 0));
        }
        Record::End { error } => {
            if let Some((code, text)) = error {
                assert_ne!(code, 0);
                assert!(text.len() <= MAX_ERROR_TEXT);
                assert!(!text.chars().any(char::is_control));
            }
        }
        Record::Dropped { stream, .. } => assert!((stream as usize) < streams.len()),
    }
}

fn record(data: &[u8]) {
    checked_record(&mut data.to_vec(), &streams());
}

/// A ring whose writer wrote `data`: the counter and the bytes are whatever the input says.
fn ring_reader(data: &[u8]) {
    let Some((head, area)) = data.split_at_checked(8) else {
        return;
    };
    let region = Arc::new(Region::heap(RING));
    if ring::format(&region).is_err() {
        return;
    }
    let Ok(mut reader) = RingReader::attach(Arc::clone(&region), MAX_RECORD) else {
        return;
    };
    let area = &area[..area.len().min(RING)];
    assert!(region.write(0, area));
    let head = u64::from_le_bytes([
        head[0], head[1], head[2], head[3], head[4], head[5], head[6], head[7],
    ]);
    region.word(WORD_HEAD).store(head, Ordering::Release);
    let streams = streams();
    let mut out = Vec::new();
    // A ring of 256 bytes holds at most 32 records.
    for _ in 0..64 {
        match reader.try_read(&mut out) {
            Ok(true) => {
                assert!(out.len() <= MAX_RECORD);
                checked_record(&mut out, &streams);
            }
            Ok(false) | Err(_) => return,
        }
    }
    panic!("more records than the ring can hold");
}

/// A ring whose reader lies about how far it has read: the writer must stay inside the data area.
fn ring_writer(data: &[u8]) {
    let region = Arc::new(Region::heap(RING));
    if ring::format(&region).is_err() {
        return;
    }
    let Ok(mut writer) = RingWriter::attach(Arc::clone(&region), MAX_RECORD) else {
        return;
    };
    for step in data.chunks(9) {
        let Some((&len, tail)) = step.split_first() else {
            return;
        };
        if let Ok(tail) = <[u8; 8]>::try_from(tail) {
            region
                .word(WORD_TAIL)
                .store(u64::from_le_bytes(tail), Ordering::Release);
        }
        let payload = [len; MAX_RECORD];
        if writer
            .try_write(&[&payload[..usize::from(len) % (MAX_RECORD + 1)]])
            .is_err()
        {
            return;
        }
    }
}

/// A stream table with one video and one audio stream.
#[must_use]
pub fn sample_table() -> MediaInfo {
    let video_base = Rational::lit(1, 12_800);
    let audio_base = Rational::lit(1, 48_000);
    let stream = |index: u32, kind: StreamKind, codec: &str, time_base: Rational| StreamInfo {
        index,
        kind,
        codec: codec.to_owned(),
        codec_tag: 0,
        time_base,
        start: Some(takt_core::Timestamp::new(0, time_base)),
        duration: Some(takt_core::Timestamp::new(25_600, time_base)),
        frames: Some(50),
        disposition: 1,
        profile: 100,
        level: 10,
        bit_rate: Some(64_000),
        extradata: vec![1, 100, 0, 10],
        language: "deu".to_owned(),
        title: "Titel".to_owned(),
        video: None,
        audio: None,
    };
    let mut video = stream(0, StreamKind::Video, "h264", video_base);
    video.video = Some(VideoParams {
        width: 64,
        height: 64,
        pixel_format: "yuv420p".to_owned(),
        sample_aspect: Rational::new(1, 1),
        frame_rate: Rational::new(25, 1),
        color_range: 1,
        color_primaries: 1,
        color_transfer: 1,
        color_matrix: 1,
        chroma_location: 1,
        field_order: 1,
        delay: 0,
    });
    let mut audio = stream(1, StreamKind::Audio, "opus", audio_base);
    audio.audio = Some(AudioParams {
        sample_rate: 48_000,
        channels: 2,
        channel_mask: 3,
        sample_format: "fltp".to_owned(),
        frame_size: 960,
        initial_padding: 312,
        trailing_padding: 0,
        seek_preroll: 3840,
        bits_per_coded_sample: 0,
        block_align: 0,
    });
    MediaInfo {
        format: "matroska,webm".to_owned(),
        duration: Some(takt_core::Timestamp::new(2_000_000, Rational::MICROS)),
        start: Some(takt_core::Timestamp::new(0, Rational::MICROS)),
        streams: vec![video, audio],
    }
}

/// Well-formed inputs for [`read_untrusted`], one per entry and message: where a fuzzer starts.
/// Each is a name and the bytes.
#[must_use]
pub fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let table = sample_table().encode();
    let head = PacketHead {
        serial: 1,
        stream: 0,
        pts: Some(1024),
        dts: Some(512),
        duration: Some(512),
        position: Some(4096),
        key: true,
        corrupt: false,
        discard: false,
    };
    let mut packet = b"compressed data".to_vec();
    packet.extend_from_slice(&head.tail());
    let mut failed = b"Invalid data found when processing input".to_vec();
    failed.extend_from_slice(&end_tail(1, -1_094_995_529));
    let end = end_tail(1, 0).to_vec();
    let dropped = dropped_record(1, 1, 9_000_000).to_vec();

    let frame = |kind: u16, seq: u32, payload: &[u8]| {
        let mut out = Vec::new();
        // The payloads here are far below the limit.
        let _ = encode(kind, seq, payload, MAX_FRAME, &mut out);
        out
    };
    let mut pipe = vec![0u8, 40];
    pipe.extend(frame(
        core_kind::HELLO,
        0,
        &Hello::ours(proto::APP).encode(),
    ));
    pipe.extend(frame(proto::kind::OPENED, 0, &table));
    pipe.extend(frame(
        core_kind::PROGRESS,
        1,
        &Progress { done: 3, total: 9 }.encode(),
    ));
    pipe.extend(frame(proto::kind::SEEK_DONE, 2, &[]));
    pipe.extend(frame(
        core_kind::FAULT,
        3,
        &Fault {
            code: proto::fault::SEEK,
            text: "Operation not permitted (-1)".to_owned(),
        }
        .encode(),
    ));
    pipe.extend(frame(
        proto::kind::SEEK,
        4,
        &Seek {
            serial: 2,
            target_micros: 7_000_000,
        }
        .encode(),
    ));

    // A ring as a writer leaves it: the records above, then the counter in front.
    let region = Arc::new(Region::heap(RING));
    let mut written = vec![2u8];
    if let Ok((mut writer, _reader)) = ring::pair(&region, MAX_RECORD) {
        for record in [&packet, &dropped, &failed, &end] {
            let _ = writer.try_write(&[record]);
        }
        let mut area = [0u8; RING];
        let _ = region.read(0, &mut area);
        written.extend_from_slice(&region.word(WORD_HEAD).load(Ordering::Acquire).to_le_bytes());
        written.extend_from_slice(&area);
    }

    let with = |entry: u8, bytes: &[u8]| {
        let mut out = vec![entry];
        out.extend_from_slice(bytes);
        out
    };
    vec![
        ("pipe", pipe),
        ("table", with(1, &table)),
        ("ring", written),
        ("record-packet", with(3, &packet)),
        ("record-end", with(3, &end)),
        ("record-error", with(3, &failed)),
        ("record-dropped", with(3, &dropped)),
        (
            "ring-writer",
            with(
                4,
                &[40, 0, 0, 0, 0, 0, 0, 0, 0, 90, 48, 0, 0, 0, 0, 0, 0, 0],
            ),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seeds_are_read_and_accepted() {
        let seeds = seeds();
        assert_eq!(seeds.len(), 8);
        for (name, bytes) in &seeds {
            assert!(bytes.len() > 1, "{name}");
            read_untrusted(bytes);
        }
        // The seeds really are well-formed: the parsers take them, they are not merely survived.
        let table = &seeds[1].1[1..];
        assert_eq!(MediaInfo::parse(table), Ok(sample_table()));
        let streams = streams();
        for (name, bytes) in &seeds[3..7] {
            assert!(
                parse_record(&mut bytes[1..].to_vec(), &streams).is_ok(),
                "{name}"
            );
        }
        // The ring seed holds its four records.
        let ring = &seeds[2].1;
        assert!(u64::from_le_bytes(ring[1..9].try_into().unwrap()) >= 4 * 8);
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
    fn a_time_base_too_large_for_exact_arithmetic_is_refused() {
        // Found by the fuzz target on 2026-11-17: a stream table whose time base had parts near
        // 2^63 was accepted, and comparing two of its timestamps overflowed 128 bits. The input is
        // the seed of the pipe with the denominator of the second stream's time base replaced.
        let mut table = sample_table().encode();
        let needle = 48_000i64.to_le_bytes();
        let at = table
            .windows(8)
            .position(|window| window == needle)
            .expect("the time base of the audio stream");
        table[at..at + 8].copy_from_slice(&0x7C7F_7F7F_7F7F_006Ai64.to_le_bytes());
        assert_eq!(
            MediaInfo::parse(&table),
            Err(takt_ipc::ProtocolError::Invalid("time base"))
        );
        let mut input = vec![1u8];
        input.extend_from_slice(&table);
        read_untrusted(&input);
    }

    #[test]
    fn a_seed_can_be_written_out_for_the_fuzzer() {
        // `TAKT_FUZZ_SEEDS=<dir>` writes the corpus the fuzz run starts from (fuzz/README.md).
        let Some(dir) = std::env::var_os("TAKT_FUZZ_SEEDS") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in seeds() {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
    }
}
