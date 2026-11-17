// SPDX-License-Identifier: GPL-3.0-or-later
//! The software decoder against real files: the clips of `apps/takt-player/tests/data` (made by
//! `scripts/make-decode-clips.py`, nothing from a film), every decoded picture compared byte for
//! byte with what the `ffmpeg` program of the same FFmpeg decodes from the file.
//!
//! The tests that decode need both helpers built with FFmpeg (`FFMPEG_DIR`, `docs/BUILD.md`):
//! `cargo test -p takt-demux -p takt-decode` builds the demux helper next to the decode helper.
//! Without FFmpeg they say SKIP and one test checks that the helper says so too.
// Test data is built from small numbers; a cast that cuts one would only change the noise.
#![allow(clippy::cast_possible_truncation)]

use takt_core::Rational;
use takt_decode::{DecodeError, Decoder, Options};
use takt_demux::{StreamInfo, StreamKind, VideoParams};

fn options() -> Options {
    let mut options = Options::new(env!("CARGO_BIN_EXE_takt-decode-helper"));
    // A machine that runs every test at once may take longer than a second to end a process;
    // the tests ask for the exit code of a helper that left by itself.
    options.limits.exit_grace = std::time::Duration::from_secs(20);
    options
}

/// A video stream as a demuxer would describe it, for the tests that need no file.
fn stream(codec: &str) -> StreamInfo {
    StreamInfo {
        index: 0,
        kind: StreamKind::Video,
        codec: codec.to_owned(),
        codec_tag: 0,
        time_base: Rational::lit(1, 1000),
        start: None,
        duration: None,
        frames: None,
        disposition: 0,
        profile: -99,
        level: -99,
        bit_rate: None,
        extradata: Vec::new(),
        language: String::new(),
        title: String::new(),
        video: Some(VideoParams {
            width: 64,
            height: 64,
            pixel_format: "yuv420p".to_owned(),
            ..VideoParams::default()
        }),
        audio: None,
    }
}

/// The program that uses the decoder must not load FFmpeg: that is what the helper process is
/// for. This test program links the library of `takt-decode` like any user of it, so its own file
/// must not ask the system for libavcodec (the import of a DLL under Windows, a needed library
/// elsewhere). In a build without FFmpeg this holds trivially; with `FFMPEG_DIR` it is the check.
#[test]
fn the_client_links_no_ffmpeg() {
    // Spelled backwards, so that this file does not contain what it looks for.
    let needles: Vec<Vec<u8>> = ["lld.36-cedocva", "os.cedocvabil", "bilyd.cedocvabil"]
        .iter()
        .map(|name| name.bytes().rev().collect())
        .collect();
    let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    for needle in &needles {
        assert!(
            !me.windows(needle.len())
                .any(|window| window == needle.as_slice()),
            "this program links {}",
            String::from_utf8_lossy(needle)
        );
    }
    // The helper is the program that does (in a build with FFmpeg).
    let helper = std::fs::read(env!("CARGO_BIN_EXE_takt-decode-helper")).unwrap();
    let links = needles.iter().any(|needle| {
        helper
            .windows(needle.len())
            .any(|window| window == needle.as_slice())
    });
    assert_eq!(links, cfg!(takt_ffmpeg));
}

#[test]
fn a_stream_the_software_path_does_not_decode_starts_no_helper() {
    // Refused before any process is started: the path of the helper leads nowhere.
    let nowhere = Options::new("no-such-helper");
    let err = Decoder::open(&stream("vp9"), &nowhere).unwrap_err();
    assert_eq!(err, DecodeError::Unsupported("codec vp9".to_owned()));
    let mut sound = stream("h264");
    sound.kind = StreamKind::Audio;
    assert!(matches!(
        Decoder::open(&sound, &nowhere),
        Err(DecodeError::Unsupported(_))
    ));
    // A codec on the list gets as far as the helper that is not there.
    assert!(matches!(
        Decoder::open(&stream("h264"), &nowhere),
        Err(DecodeError::Helper(_))
    ));
}

#[test]
fn a_helper_started_by_hand_says_what_it_is() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_takt-decode-helper"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("started by Takt"), "{text}");
}

#[cfg(not(takt_ffmpeg))]
mod without_ffmpeg {
    use super::*;
    use takt_decode::proto;

    #[test]
    fn a_helper_without_ffmpeg_says_so_instead_of_decoding() {
        let err = Decoder::open(&stream("h264"), &options()).unwrap_err();
        let DecodeError::Refused { code, text } = err else {
            panic!("{err:?}");
        };
        assert_eq!(code, proto::fault::NO_FFMPEG);
        assert!(text.contains("FFMPEG_DIR"), "{text}");
    }

    #[test]
    #[ignore = "SKIP: FFmpeg not linked, no picture is decoded; set FFMPEG_DIR (docs/BUILD.md)"]
    fn h264_hevc_and_av1_decode_to_the_bytes_ffmpeg_decodes() {}
}

#[cfg(takt_ffmpeg)]
mod with_ffmpeg {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use takt_backend::{SeekMode, VideoBackend};
    use takt_clock::{Clock, ManualMonotonic, MonotonicSource, WallClock};
    use takt_core::{PixelFormat, PooledFrame, Timestamp, VideoFrame};
    use takt_decode::{BackendOptions, Decoded, Layout, Next, Sent, SoftwareBackend, proto};
    use takt_demux::{Demuxer, Event};

    const WAIT: Duration = Duration::from_secs(30);

    /// The five clips: name, decoder, width, height, bytes per sample.
    const CLIPS: [(&str, &str, u32, u32, usize); 5] = [
        ("decode-h264.mp4", "h264", 160, 90, 1),
        ("decode-hevc.mp4", "hevc", 160, 90, 1),
        ("decode-av1.mkv", "libdav1d", 160, 90, 1),
        ("decode-hevc-10bit.mp4", "hevc", 130, 74, 2),
        ("decode-av1-10bit.mkv", "libdav1d", 130, 74, 2),
    ];
    /// Pictures in every clip: two seconds at 25 a second.
    const FRAMES: usize = 50;

    fn clip(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/takt-player/tests/data")
            .join(name)
    }

    /// The demux helper, which cargo puts next to the decode helper when `takt-demux` is built
    /// in the same run.
    fn demux_options() -> takt_demux::Options {
        let helper = Path::new(env!("CARGO_BIN_EXE_takt-decode-helper")).with_file_name(format!(
            "{}{}",
            takt_demux::HELPER_NAME,
            std::env::consts::EXE_SUFFIX
        ));
        assert!(
            helper.is_file(),
            "{} is missing: build it in the same run (cargo test -p takt-demux -p takt-decode)",
            helper.display()
        );
        let mut options = takt_demux::Options::new(helper);
        options.limits.exit_grace = Duration::from_secs(20);
        options
    }

    /// The pictures of `file` as the `ffmpeg` program of the helper's FFmpeg decodes them: each
    /// one its three planes, row after row without padding.
    fn reference(file: &Path, width: u32, height: u32, sample: usize) -> Vec<Vec<u8>> {
        let program = std::env::var_os("FFMPEG_DIR")
            .map(|root| {
                PathBuf::from(root)
                    .join("bin")
                    .join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX))
            })
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("ffmpeg"));
        let out = Command::new(&program)
            .args(["-v", "error", "-i"])
            .arg(file)
            .args([
                "-map",
                "0:v:0",
                "-fps_mode",
                "passthrough",
                "-f",
                "rawvideo",
                "-",
            ])
            .output()
            .unwrap_or_else(|e| panic!("{} does not run: {e}", program.display()));
        assert!(
            out.status.success(),
            "ffmpeg: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let (w, h) = (width as usize, height as usize);
        let picture = (w * h + 2 * w.div_ceil(2) * h.div_ceil(2)) * sample;
        assert_eq!(out.stdout.len() % picture, 0, "whole pictures");
        out.stdout.chunks(picture).map(<[u8]>::to_vec).collect()
    }

    /// The used bytes of a frame, plane after plane, row after row without the padding.
    fn packed(frame: &VideoFrame) -> Vec<u8> {
        let mut out = Vec::new();
        for plane in 0..frame.meta().format.planes() {
            let (stride, row) = (
                frame.stride(plane).unwrap(),
                frame.row_bytes(plane).unwrap(),
            );
            for line in frame.plane(plane).unwrap().chunks(stride) {
                out.extend_from_slice(&line[..row]);
            }
        }
        out
    }

    /// A file being decoded: the demuxer, the decoder and the loop between them, by hand.
    struct Run {
        demuxer: Demuxer,
        decoder: Decoder,
        video: u32,
        held: Option<takt_demux::Packet>,
        ended: bool,
    }

    impl Run {
        fn open(name: &str, options: &Options) -> Self {
            let demuxer = Demuxer::open_path(clip(name), &demux_options()).unwrap();
            let stream = demuxer
                .info()
                .streams
                .iter()
                .find(|stream| stream.kind == StreamKind::Video)
                .unwrap()
                .clone();
            let decoder = Decoder::open(&stream, options).unwrap();
            Self {
                demuxer,
                decoder,
                video: stream.index,
                held: None,
                ended: false,
            }
        }

        /// Moves packets to the decoder as far as its window allows.
        fn feed(&mut self) {
            while !self.ended && self.decoder.can_send() {
                if self.held.is_none() {
                    match self.demuxer.try_next().unwrap() {
                        Some(Event::Packet(packet)) if packet.stream == self.video => {
                            self.held = Some(packet);
                        }
                        Some(Event::Packet(packet)) => self.demuxer.recycle(packet),
                        Some(Event::End { error }) => {
                            assert_eq!(error, None);
                            self.ended = true;
                            self.decoder.drain().unwrap();
                        }
                        Some(Event::Dropped { .. }) => panic!("a packet too large"),
                        None => return,
                    }
                }
                if let Some(packet) = self.held.take() {
                    match self.decoder.send(&packet).unwrap() {
                        Sent::Taken => self.demuxer.recycle(packet),
                        Sent::WindowFull => {
                            self.held = Some(packet);
                            return;
                        }
                        Sent::TooLarge => panic!("a packet too large"),
                    }
                }
            }
        }

        /// The next picture, `None` at the end. Panics when the pool is empty: the caller holds
        /// no frames.
        fn next(&mut self) -> Option<Decoded> {
            let deadline = Instant::now() + WAIT;
            loop {
                self.feed();
                match self.decoder.try_next().unwrap() {
                    Next::Frame(decoded) => return Some(decoded),
                    Next::End => return None,
                    Next::Idle => {}
                    Next::PoolExhausted => panic!("the pool is empty"),
                }
                assert!(Instant::now() < deadline, "no picture in {WAIT:?}");
                std::thread::sleep(Duration::from_micros(200));
            }
        }
    }

    #[test]
    fn h264_hevc_and_av1_decode_to_the_bytes_ffmpeg_decodes() {
        for (name, decoder, width, height, sample) in CLIPS {
            let wanted = reference(&clip(name), width, height, sample);
            assert_eq!(wanted.len(), FRAMES, "{name}");
            let mut run = Run::open(name, &options());
            assert_eq!(run.decoder.decoder_name(), decoder, "{name}");
            let mut last = None;
            let mut count = 0;
            while let Some(decoded) = run.next() {
                let frame = decoded.frame.get();
                let meta = frame.meta();
                assert_eq!((meta.width, meta.height), (width, height), "{name}");
                let format = if sample == 2 {
                    PixelFormat::Yuv420P10
                } else {
                    PixelFormat::Yuv420P
                };
                assert_eq!(meta.format, format, "{name}");
                // The clips name their matrix and their range, not their primaries.
                assert_eq!(meta.primaries, takt_core::ColorPrimaries::Unknown, "{name}");
                assert_eq!(meta.range, takt_core::ColorRange::Limited, "{name}");
                assert_eq!(decoded.matrix, 1, "{name}: BT.709");
                assert!(decoded.timed, "{name}");
                // Presentation order: 25 pictures a second from zero, whatever order the
                // packets came in (the H.264 and HEVC clips have B-frames).
                assert_eq!(
                    meta.pts,
                    Timestamp::from_millis(40 * count as i64),
                    "{name} picture {count}"
                );
                assert!(last.is_none_or(|last| last < meta.pts), "{name}");
                last = Some(meta.pts);
                assert_eq!(decoded.key, count % 25 == 0, "{name} picture {count}");
                assert!(
                    packed(frame) == wanted[count],
                    "{name}: picture {count} differs from what ffmpeg decodes"
                );
                count += 1;
            }
            assert_eq!(count, FRAMES, "{name}");
            let stats = run.decoder.stats();
            assert_eq!(stats.frames, FRAMES as u64, "{name}");
            assert_eq!(stats.errors, 0, "{name}");
            assert_eq!(stats.pools, 1, "{name}: one pool for the whole file");
            assert_eq!(stats.pool_exhausted, 0, "{name}");
            assert!(stats.copied_bytes >= (FRAMES * wanted[0].len()) as u64);
            assert_eq!(run.decoder.pool(), (0, 8), "{name}: every frame came back");
            // Both helpers leave in good order.
            assert_eq!(run.decoder.close(), Some(0), "{name}");
            assert_eq!(run.demuxer.close(), Some(0), "{name}");
        }
    }

    #[test]
    fn the_same_pictures_arrive_as_nv12_and_p010() {
        for (name, _, width, height, sample) in [CLIPS[0], CLIPS[2], CLIPS[3], CLIPS[4]] {
            let wanted = reference(&clip(name), width, height, sample);
            let mut run = Run::open(
                name,
                &Options {
                    layout: Layout::SemiPlanar,
                    ..options()
                },
            );
            let (w, h) = (width as usize, height as usize);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let mut count = 0;
            while let Some(decoded) = run.next() {
                let frame = decoded.frame.get();
                let got = packed(frame);
                let planar = &wanted[count];
                let (luma, chroma) = planar.split_at(w * h * sample);
                let (cb, cr) = chroma.split_at(cw * ch * sample);
                let mut semi = Vec::with_capacity(planar.len());
                if sample == 1 {
                    assert_eq!(frame.meta().format, PixelFormat::Nv12, "{name}");
                    semi.extend_from_slice(luma);
                    for (cb, cr) in cb.iter().zip(cr) {
                        semi.extend_from_slice(&[*cb, *cr]);
                    }
                } else {
                    assert_eq!(frame.meta().format, PixelFormat::P010, "{name}");
                    // P010 holds the ten bits in the high bits of its words.
                    let high =
                        |word: &[u8]| (u16::from_le_bytes([word[0], word[1]]) << 6).to_le_bytes();
                    for word in luma.chunks(2) {
                        semi.extend_from_slice(&high(word));
                    }
                    for (cb, cr) in cb.chunks(2).zip(cr.chunks(2)) {
                        semi.extend_from_slice(&high(cb));
                        semi.extend_from_slice(&high(cr));
                    }
                }
                assert!(got == semi, "{name}: picture {count}");
                count += 1;
            }
            assert_eq!(count, FRAMES, "{name}");
        }
    }

    #[test]
    fn an_empty_pool_stalls_the_decoder_and_nothing_is_lost_or_allocated() {
        let (name, _, width, height, sample) = CLIPS[0];
        let wanted = reference(&clip(name), width, height, sample);
        // A ring for three pictures and a decoder without threads of its own: so few pictures
        // and packets are on their way that the stall can be followed back to the packets.
        let mut run = Run::open(
            name,
            &Options {
                pool_frames: 2,
                ring_capacity: Some(1 << 16),
                threads: 1,
                ..options()
            },
        );
        let mut reached_the_packets = false;
        // A consumer that holds what it gets: after two pictures the pool is empty.
        let mut held: Vec<Decoded> = Vec::new();
        let mut addresses = std::collections::BTreeSet::new();
        let mut count = 0;
        let mut stalls = 0u64;
        let deadline = Instant::now() + WAIT;
        loop {
            assert!(Instant::now() < deadline);
            run.feed();
            match run.decoder.try_next().unwrap() {
                Next::Frame(decoded) => {
                    assert!(held.len() < 2, "a third frame out of a pool of two");
                    addresses.insert(decoded.frame.get().plane(0).unwrap().as_ptr().addr());
                    assert!(
                        packed(decoded.frame.get()) == wanted[count],
                        "picture {count}"
                    );
                    count += 1;
                    held.push(decoded);
                }
                Next::End => break,
                Next::Idle => std::thread::sleep(Duration::from_micros(200)),
                Next::PoolExhausted => {
                    assert_eq!(held.len(), 2, "exhausted means both frames are out");
                    assert_eq!(run.decoder.pool(), (2, 2));
                    stalls += 1;
                    // The stall is one stall, however often it is looked at, and the decoder
                    // waits: nothing arrives, nothing is allocated.
                    let counted = run.decoder.stats().pool_exhausted;
                    for _ in 0..20 {
                        run.feed();
                        assert!(matches!(
                            run.decoder.try_next().unwrap(),
                            Next::PoolExhausted
                        ));
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    // The wait reaches the packets: the helper finds its ring full and takes
                    // none, so the window fills and the demuxer is not read any further (until
                    // the whole file is on its way, near the end).
                    let waiting = Instant::now();
                    while run.decoder.can_send() && !run.ended {
                        assert!(waiting.elapsed() < WAIT, "the window never filled");
                        run.feed();
                        assert!(matches!(
                            run.decoder.try_next().unwrap(),
                            Next::PoolExhausted
                        ));
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    reached_the_packets |= !run.ended;
                    assert_eq!(run.decoder.stats().pool_exhausted, counted);
                    assert_eq!(run.decoder.stats().frames, count as u64);
                    held.clear();
                }
            }
        }
        // Every picture arrived, in order, through the same two frames.
        assert_eq!(count, FRAMES);
        assert_eq!(addresses.len(), 2, "two frames were allocated in all");
        let stats = run.decoder.stats();
        assert_eq!(stats.pools, 1);
        assert_eq!(stats.pool_exhausted, stalls);
        assert!(stalls >= 20, "the pool ran dry {stalls} times");
        assert!(
            reached_the_packets,
            "the window was full while the file had packets left"
        );
    }

    #[test]
    fn a_flush_drops_what_was_on_its_way_and_the_new_position_plays() {
        for (name, _, width, height, sample) in [CLIPS[0], CLIPS[1], CLIPS[2]] {
            let wanted = reference(&clip(name), width, height, sample);
            let mut run = Run::open(name, &options());
            // A few pictures from the start, with more on their way in the ring.
            for wanted in wanted.iter().take(5) {
                let decoded = run.next().unwrap();
                assert!(packed(decoded.frame.get()) == *wanted, "{name}");
            }
            // A jump to 1.2 s: the demuxer lands on the keyframe at 1 s, the decoder forgets
            // what it held, and the five pictures before the target are not copied.
            let target = Timestamp::from_millis(1200);
            run.demuxer.seek(target).unwrap();
            run.held = None;
            run.ended = false;
            run.decoder.flush().unwrap();
            let base = run.demuxer.info().streams[run.video as usize].time_base;
            run.decoder.skip_before(target.rescale(base));
            let mut count = 30;
            while let Some(decoded) = run.next() {
                assert_eq!(
                    decoded.frame.get().meta().pts,
                    Timestamp::from_millis(40 * count as i64),
                    "{name}"
                );
                assert!(
                    packed(decoded.frame.get()) == wanted[count],
                    "{name}: picture {count} after the jump"
                );
                count += 1;
            }
            assert_eq!(count, FRAMES, "{name}");
            let stats = run.decoder.stats();
            assert_eq!(stats.skipped, 5, "{name}: pictures 25 to 29");
            assert_eq!(stats.errors, 0, "{name}");
            // After the end a jump plays again: the decoder takes packets after a drain.
            run.demuxer.seek(Timestamp::from_millis(1000)).unwrap();
            run.ended = false;
            run.decoder.flush().unwrap();
            let mut count = 25;
            while let Some(decoded) = run.next() {
                assert!(packed(decoded.frame.get()) == wanted[count], "{name}");
                count += 1;
            }
            assert_eq!(count, FRAMES, "{name}");
        }
    }

    #[test]
    fn damaged_packets_are_counted_and_the_decoder_goes_on() {
        for (name, ..) in [CLIPS[0], CLIPS[1], CLIPS[2]] {
            let mut demuxer = Demuxer::open_path(clip(name), &demux_options()).unwrap();
            let stream = demuxer.info().streams[0].clone();
            let mut decoder = Decoder::open(&stream, &options()).unwrap();
            let mut packets = Vec::new();
            loop {
                match demuxer.next(WAIT).unwrap() {
                    Some(Event::Packet(packet)) => packets.push(packet),
                    Some(Event::End { .. }) => break,
                    other => panic!("{other:?}"),
                }
            }
            assert_eq!(packets.len(), FRAMES, "{name}");
            // Every third packet is noise, one is empty, one is cut in half.
            for (index, packet) in packets.iter_mut().enumerate() {
                if index % 3 == 1 {
                    for (at, byte) in packet.data.iter_mut().enumerate() {
                        *byte = (at as u8).wrapping_mul(151).wrapping_add(index as u8);
                    }
                } else if index == 12 {
                    packet.data.clear();
                } else if index == 30 {
                    packet.data.truncate(packet.data.len() / 2);
                }
            }
            let mut pictures = 0;
            let mut ended = false;
            let mut sent = 0;
            let deadline = Instant::now() + WAIT;
            while !ended {
                assert!(Instant::now() < deadline, "{name}");
                while sent < packets.len() && decoder.can_send() {
                    assert_eq!(decoder.send(&packets[sent]).unwrap(), Sent::Taken);
                    sent += 1;
                    if sent == packets.len() {
                        decoder.drain().unwrap();
                    }
                }
                match decoder.try_next().unwrap() {
                    Next::Frame(_) => pictures += 1,
                    Next::End => ended = true,
                    Next::Idle | Next::PoolExhausted => {
                        std::thread::sleep(Duration::from_micros(200));
                    }
                }
            }
            // The helper lived through it and says how it went: some pictures, some errors.
            let stats = decoder.stats();
            println!("{name}: {pictures} pictures, {} errors", stats.errors);
            assert!(pictures <= FRAMES, "{name}");
            assert!(pictures > 0, "{name}: the sound packets still decode");
            assert_eq!(stats.packets, FRAMES as u64, "{name}");
            if stats.errors > 0 {
                let (code, text) = decoder.last_error().unwrap();
                assert_ne!(*code, 0);
                assert!(!text.is_empty(), "{name}");
            }
            assert_eq!(decoder.close(), Some(0), "{name}");
        }
    }

    #[test]
    fn a_picture_that_does_not_fit_the_ring_is_an_error_not_a_hang() {
        // A ring of 16 KiB cannot hold a picture of 160x90 (21 600 bytes).
        let (name, ..) = CLIPS[0];
        let mut run = Run::open(
            name,
            &Options {
                ring_capacity: Some(1 << 14),
                ..options()
            },
        );
        assert!(run.next().is_none(), "no picture fits");
        let stats = run.decoder.stats();
        assert_eq!(stats.frames, 0);
        assert_eq!(stats.errors, FRAMES as u64);
        let (_, text) = run.decoder.last_error().unwrap();
        assert!(text.contains("does not fit the ring"), "{text}");
    }

    /// A clock that stands where it is put and lets nothing be dropped.
    struct Still(std::sync::atomic::AtomicI64);

    impl Clock for Still {
        fn kind(&self) -> takt_clock::ClockKind {
            takt_clock::ClockKind::Timeline
        }
        fn now(&self) -> Timestamp {
            Timestamp::from_millis(self.0.load(std::sync::atomic::Ordering::Relaxed))
        }
        fn until_nanos(&self, pts: Timestamp) -> i64 {
            pts.rescale(Rational::NANOS).map_or(0, Timestamp::ticks)
                - self
                    .now()
                    .rescale(Rational::NANOS)
                    .map_or(0, Timestamp::ticks)
        }
        fn may_drop(&self) -> bool {
            false
        }
    }

    fn backend() -> SoftwareBackend {
        let helpers = Path::new(env!("CARGO_BIN_EXE_takt-decode-helper"))
            .parent()
            .unwrap();
        // The demux helper must be there.
        let _ = demux_options();
        SoftwareBackend::new(BackendOptions::in_dir(helpers))
    }

    /// Asks for the frame that is due until it comes or the file ends.
    fn due(backend: &mut SoftwareBackend, clock: &dyn Clock) -> Option<PooledFrame> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(frame) = backend.next_frame(clock).unwrap() {
                return Some(frame);
            }
            if backend.is_eof() {
                return None;
            }
            assert!(Instant::now() < deadline, "no frame in {WAIT:?}");
            std::thread::sleep(Duration::from_micros(200));
        }
    }

    #[test]
    fn the_backend_plays_every_picture_of_a_file_on_a_clock_that_does_not_drop() {
        for (name, decoder, width, height, sample) in CLIPS {
            let wanted = reference(&clip(name), width, height, sample);
            let mut backend = backend();
            let info = backend.open(clip(name).to_str().unwrap()).unwrap();
            assert_eq!(info.frame_rate, Rational::new(25, 1), "{name}");
            assert_eq!(info.frame_duration_nanos(), Some(40_000_000), "{name}");
            let meta = info.video.unwrap();
            assert_eq!((meta.width, meta.height), (width, height), "{name}");
            assert_eq!(
                info.duration,
                Some(Timestamp::new(2_000_000, Rational::MICROS))
            );
            assert!(info.seekable && info.sample_rate.is_none());
            assert!(!backend.is_eof());

            let clock = Still(std::sync::atomic::AtomicI64::new(0));
            for (count, wanted) in wanted.iter().enumerate() {
                // The clock stands at the picture's time: it is due, the one after is not.
                clock
                    .0
                    .store(40 * count as i64, std::sync::atomic::Ordering::Relaxed);
                let frame = due(&mut backend, &clock).unwrap();
                assert_eq!(
                    frame.get().meta().pts,
                    Timestamp::from_millis(40 * count as i64),
                    "{name}"
                );
                assert!(packed(frame.get()) == *wanted, "{name}: picture {count}");
                // Not due yet: nothing comes while the clock stands.
                assert!(backend.next_frame(&clock).unwrap().is_none(), "{name}");
            }
            clock.0.store(10_000, std::sync::atomic::Ordering::Relaxed);
            assert!(due(&mut backend, &clock).is_none(), "{name}: the end");
            assert!(backend.is_eof());
            let stats = backend.stats();
            assert_eq!(stats.decoded, FRAMES as u64, "{name}");
            assert_eq!(stats.dropped, 0, "{name}");
            let pipeline = backend.pipeline_stats();
            assert_eq!(pipeline.decoder, decoder, "{name}");
            assert_eq!(pipeline.matrix, 1, "{name}");
            assert_eq!(pipeline.decode.pool_exhausted, 0, "{name}");
            assert_eq!((pipeline.oversized, pipeline.untimed), (0, 0), "{name}");
            assert_eq!(pipeline.read_error, None, "{name}");
        }
    }

    #[test]
    fn the_backend_jumps_exactly_and_shows_the_picture_of_the_jump_at_once() {
        let (name, _, width, height, sample) = CLIPS[1];
        let wanted = reference(&clip(name), width, height, sample);
        let mut backend = backend();
        backend.open(clip(name).to_str().unwrap()).unwrap();
        let clock = Still(std::sync::atomic::AtomicI64::new(0));
        assert!(due(&mut backend, &clock).is_some());
        // 1.21 s lies between two pictures: the next one (1.24 s) is the picture of the jump, and
        // it comes although the clock stands before it.
        for (millis, picture) in [(1210, 31), (80, 2), (1960, 49), (1000, 25)] {
            backend
                .seek(Timestamp::from_millis(millis), SeekMode::Exact)
                .unwrap();
            clock.0.store(millis, std::sync::atomic::Ordering::Relaxed);
            assert!(!backend.is_eof());
            let frame = due(&mut backend, &clock).unwrap();
            assert_eq!(
                frame.get().meta().pts,
                Timestamp::from_millis(40 * picture as i64),
                "jump to {millis} ms"
            );
            assert!(
                packed(frame.get()) == wanted[picture],
                "jump to {millis} ms"
            );
        }
        // Past the end there is no picture, and the file is at its end.
        backend
            .seek(Timestamp::from_millis(5000), SeekMode::Keyframe)
            .unwrap();
        assert!(due(&mut backend, &clock).is_none());
        assert!(backend.is_eof());
        // And back again.
        backend
            .seek(Timestamp::from_millis(0), SeekMode::Keyframe)
            .unwrap();
        clock.0.store(0, std::sync::atomic::Ordering::Relaxed);
        let frame = due(&mut backend, &clock).unwrap();
        assert!(packed(frame.get()) == wanted[0]);
    }

    #[test]
    fn a_clock_that_ran_ahead_makes_the_backend_drop_what_is_late() {
        let (name, ..) = CLIPS[2];
        let mut backend = backend();
        backend.open(clip(name).to_str().unwrap()).unwrap();
        let time = Arc::new(ManualMonotonic::new());
        let clock = WallClock::new(Arc::clone(&time) as Arc<dyn MonotonicSource>);
        let first = due(&mut backend, &clock).unwrap();
        assert_eq!(first.get().meta().pts, Timestamp::from_millis(0));
        drop(first);
        // Half a second passes without a look at the backend, while the thread decodes ahead
        // as far as its queue goes. The pictures whose time has passed are skipped as they
        // arrive, until the one that is due now is shown.
        time.advance(500_000_000);
        std::thread::sleep(Duration::from_millis(100));
        let mut shown = Vec::new();
        loop {
            let frame = due(&mut backend, &clock).unwrap();
            let pts = frame.get().meta().pts;
            shown.push(pts);
            if pts >= Timestamp::from_millis(440) {
                break;
            }
        }
        // Nothing from the future: at 500 ms the picture of 520 ms is the last that is due.
        assert!(shown.iter().all(|pts| *pts <= Timestamp::from_millis(520)));
        let stats = backend.stats();
        assert!(stats.dropped >= 2, "{} dropped", stats.dropped);
        // Shown or dropped, every picture up to here is accounted for.
        let last = shown[shown.len() - 1]
            .rescale(Rational::MILLIS)
            .unwrap()
            .ticks()
            / 40;
        assert_eq!(shown.len() as u64 + stats.dropped, last as u64);
    }

    #[test]
    fn a_backend_whose_frames_are_held_stalls_and_counts_it() {
        let (name, ..) = CLIPS[0];
        let helpers = Path::new(env!("CARGO_BIN_EXE_takt-decode-helper"))
            .parent()
            .unwrap();
        let mut options = BackendOptions::in_dir(helpers);
        // A pool smaller than what the thread hands on and the caller keeps.
        options.decode.pool_frames = 3;
        options.queue = 1;
        let mut backend = SoftwareBackend::new(options);
        backend.open(clip(name).to_str().unwrap()).unwrap();
        let clock = Still(std::sync::atomic::AtomicI64::new(10_000));
        // Everything is due; the caller keeps what it gets.
        let mut held = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && held.len() < 3 {
            if let Some(frame) = backend.next_frame(&clock).unwrap() {
                held.push(frame);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(held.len(), 3, "the three frames of the pool");
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            backend.next_frame(&clock).unwrap().is_none(),
            "no fourth frame"
        );
        assert_eq!(backend.pool(), (3, 3));
        let stalled = backend.pipeline_stats().decode;
        assert_eq!(stalled.pool_exhausted, 1);
        assert_eq!(stalled.frames, 3);
        assert_eq!(stalled.pools, 1);
        // Giving the frames back lets it play to the end.
        held.clear();
        let mut count = 3;
        while due(&mut backend, &clock).is_some() {
            count += 1;
        }
        assert_eq!(count, FRAMES);
    }

    #[test]
    fn a_file_the_software_path_cannot_play_is_an_error_with_a_reason() {
        let mut backend = backend();
        let err = backend
            .open(clip("format-vp9.webm").to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("vp9"), "{err}");
        assert!(backend.is_eof());
        let err = backend.open("no such file.mkv").unwrap_err();
        assert!(
            matches!(err, takt_core::Error::SourceUnavailable(_)),
            "{err}"
        );
        // The grey clips of the MpvBackend tests play too.
        for name in [
            "format-h264.mp4",
            "format-hevc.mp4",
            "format-av1.mkv",
            "pattern-3s.mkv",
        ] {
            let info = backend.open(clip(name).to_str().unwrap()).unwrap();
            assert!(info.video.is_some(), "{name}");
            let clock = Still(std::sync::atomic::AtomicI64::new(60_000));
            let mut count = 0;
            while due(&mut backend, &clock).is_some() {
                count += 1;
            }
            assert_eq!(
                count,
                if name == "pattern-3s.mkv" { 75 } else { 50 },
                "{name}"
            );
        }
    }

    #[test]
    fn the_ffmpeg_of_the_helper_is_an_lgpl_build_with_dav1d() {
        let out = Command::new(env!("CARGO_BIN_EXE_takt-decode-helper"))
            .arg("--about")
            .output()
            .unwrap();
        let about = String::from_utf8_lossy(&out.stdout);
        println!("{about}");
        assert!(about.contains("libavcodec 63."), "{about}");
        // A GPL build of FFmpeg is not what Takt ships with (KONZEPT, license zones; BUILD.md).
        let license = about
            .lines()
            .find_map(|line| line.strip_prefix("license: "))
            .unwrap_or_default();
        assert!(
            license.starts_with("LGPL"),
            "FFmpeg build is licensed {license:?}, takt needs an LGPL build"
        );
        // Every codec of the list has its decoder in this libavcodec, dav1d among them.
        for (codec, decoder) in proto::CODECS {
            assert!(
                about.contains(&format!("{codec}: decoder {decoder}\n"))
                    || about
                        .trim_end()
                        .ends_with(&format!("{codec}: decoder {decoder}")),
                "{about}"
            );
        }
        assert!(!about.contains("missing"), "{about}");
    }
}
