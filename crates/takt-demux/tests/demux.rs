// SPDX-License-Identifier: GPL-3.0-or-later
//! The demuxer against real files: the clips of `apps/takt-player/tests/data` (made by
//! `scripts/make-*-clip.py`, nothing from a film), every packet compared with what `ffprobe` of the
//! same FFmpeg says about the file.
//!
//! The tests that read a file need a helper built with FFmpeg (`FFMPEG_DIR`, `docs/BUILD.md`);
//! without it they say SKIP and one test checks that the helper says so too.
// Test data is built from small numbers; a cast that cuts one would only change the noise.
#![allow(clippy::cast_possible_truncation)]

use std::path::PathBuf;

use takt_demux::Options;

fn helper() -> Options {
    Options::new(env!("CARGO_BIN_EXE_takt-demux-helper"))
}

fn clip(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/takt-player/tests/data")
        .join(name)
}

/// The program that uses the demuxer must not load FFmpeg: that is what the helper process is
/// for. This test program links the library of `takt-demux` like any user of it, so its own file
/// must not ask the system for libavformat (the import of a DLL under Windows, a needed library
/// elsewhere). In a build without FFmpeg this holds trivially; with `FFMPEG_DIR` it is the check.
#[test]
fn the_client_links_no_ffmpeg() {
    // Spelled backwards, so that this file does not contain what it looks for.
    let needles: Vec<Vec<u8>> = ["lld.36-tamrofva", "os.tamrofvabil", "bilyd.tamrofvabil"]
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
    let helper = std::fs::read(env!("CARGO_BIN_EXE_takt-demux-helper")).unwrap();
    let links = needles.iter().any(|needle| {
        helper
            .windows(needle.len())
            .any(|window| window == needle.as_slice())
    });
    assert_eq!(links, cfg!(takt_ffmpeg));
}

#[cfg(not(takt_ffmpeg))]
mod without_ffmpeg {
    use super::*;
    use takt_demux::{DemuxError, Demuxer, proto};

    #[test]
    fn a_helper_without_ffmpeg_says_so_instead_of_reading() {
        let err = Demuxer::open_path(clip("format-h264.mp4"), &helper()).unwrap_err();
        let DemuxError::Refused { code, text } = err else {
            panic!("{err:?}");
        };
        assert_eq!(code, proto::fault::NO_FFMPEG);
        assert!(text.contains("FFMPEG_DIR"), "{text}");
    }

    #[test]
    #[ignore = "SKIP: FFmpeg not linked, no file is demuxed; set FFMPEG_DIR (docs/BUILD.md)"]
    fn demux_of_mkv_mp4_and_webm_against_ffprobe() {}
}

#[cfg(takt_ffmpeg)]
mod with_ffmpeg {
    use super::*;
    use std::io::Write;
    use std::process::Command;
    use std::time::Duration;

    use takt_core::{Rational, Timestamp};
    use takt_demux::{DemuxError, Demuxer, Event, MediaInfo, Packet, StreamKind, proto};

    const WAIT: Duration = Duration::from_secs(30);

    /// A file of this test run in the temp directory, removed when dropped.
    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str, content: &[u8]) -> Self {
            let path =
                std::env::temp_dir().join(format!("takt-demux-{}-{name}", std::process::id()));
            std::fs::File::create(&path)
                .unwrap()
                .write_all(content)
                .unwrap();
            Self(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// `ffprobe` of the FFmpeg the helper was built with, else the one in PATH.
    fn ffprobe(args: &[&str], file: &std::path::Path) -> String {
        let program = std::env::var_os("FFMPEG_DIR")
            .map(|root| {
                PathBuf::from(root)
                    .join("bin")
                    .join(format!("ffprobe{}", std::env::consts::EXE_SUFFIX))
            })
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("ffprobe"));
        let out = Command::new(&program)
            .args(["-v", "error"])
            .args(args)
            .arg(file)
            .output()
            .unwrap_or_else(|e| panic!("{} does not run: {e}", program.display()));
        assert!(
            out.status.success(),
            "ffprobe: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// One packet as ffprobe prints it: stream, pts, dts, duration, size, pos, key.
    type Probed = (
        u32,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        usize,
        Option<u64>,
        bool,
    );

    fn probed_packets(file: &std::path::Path) -> Vec<Probed> {
        let text = ffprobe(
            &[
                "-show_entries",
                "packet=stream_index,pts,dts,duration,size,pos,flags",
                "-of",
                "csv=p=0",
            ],
            file,
        );
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let f: Vec<&str> = line.trim().split(',').collect();
                let number = |s: &str| s.parse::<i64>().ok();
                (
                    f[0].parse().unwrap(),
                    number(f[1]),
                    number(f[2]),
                    number(f[3]).filter(|d| *d > 0),
                    f[4].parse().unwrap(),
                    number(f[5]).and_then(|p| u64::try_from(p).ok()),
                    f[6].starts_with('K'),
                )
            })
            .collect()
    }

    /// `index|codec_name|codec_type|time_base` per stream, as ffprobe prints them.
    fn probed_streams(file: &std::path::Path) -> Vec<(String, String, Rational)> {
        let text = ffprobe(
            &[
                "-show_entries",
                "stream=codec_name,codec_type,time_base",
                "-of",
                "csv=p=0",
            ],
            file,
        );
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let f: Vec<&str> = line.trim().split(',').collect();
                let (num, den) = f[2].split_once('/').unwrap();
                (
                    f[0].to_owned(),
                    f[1].to_owned(),
                    Rational::new(num.parse().unwrap(), den.parse().unwrap()).unwrap(),
                )
            })
            .collect()
    }

    /// Reads until the end and returns the packets and how it ended.
    fn drain(demuxer: &mut Demuxer) -> (Vec<Packet>, Option<(i32, String)>) {
        let mut packets = Vec::new();
        loop {
            match demuxer.next(WAIT).unwrap() {
                Some(Event::Packet(packet)) => packets.push(packet),
                Some(Event::End { error }) => return (packets, error),
                Some(Event::Dropped { stream, size }) => {
                    panic!("packet of {size} bytes of stream {stream} dropped")
                }
                None => panic!("no event within {WAIT:?}"),
            }
        }
    }

    /// The whole file through the demuxer, compared packet by packet with ffprobe.
    fn matches_ffprobe(name: &str, format: &str, codecs: &[(&str, StreamKind)]) -> MediaInfo {
        let path = clip(name);
        let mut demuxer = Demuxer::open_path(&path, &helper()).unwrap();
        let info = demuxer.info().clone();
        assert_eq!(info.format, format, "{name}");
        let probed = probed_streams(&path);
        assert_eq!(info.streams.len(), codecs.len(), "{name}");
        assert_eq!(probed.len(), codecs.len(), "{name}");
        for (stream, ((codec, kind), probe)) in info.streams.iter().zip(codecs.iter().zip(&probed))
        {
            assert_eq!(
                (stream.codec.as_str(), stream.kind),
                (*codec, *kind),
                "{name}"
            );
            assert_eq!(stream.codec, probe.0, "{name}");
            // The time base is the exact fraction of the container, not a rounded number.
            assert_eq!(stream.time_base, probe.2, "{name}");
        }

        let (packets, error) = drain(&mut demuxer);
        assert_eq!(error, None, "{name}");
        let expected = probed_packets(&path);
        assert_eq!(packets.len(), expected.len(), "{name}");
        for (n, (packet, want)) in packets.iter().zip(&expected).enumerate() {
            let base = info.streams[packet.stream as usize].time_base;
            let stamp = |ticks: Option<i64>| ticks.map(|t| Timestamp::new(t, base));
            let at = format!("{name}, packet {n}");
            assert_eq!(packet.stream, want.0, "{at}");
            assert_eq!(packet.pts, stamp(want.1), "{at}");
            assert_eq!(packet.dts, stamp(want.2), "{at}");
            assert_eq!(packet.duration, stamp(want.3), "{at}");
            assert_eq!(packet.data.len(), want.4, "{at}");
            assert_eq!(packet.position, want.5, "{at}");
            assert_eq!(packet.key, want.6, "{at}");
            // Exactly the ticks and the base, not merely an equal instant.
            if let (Some(pts), Some(ticks)) = (packet.pts, want.1) {
                assert_eq!((pts.ticks(), pts.base()), (ticks, base), "{at}");
            }
        }
        // After the end nothing comes, and the helper leaves in good order.
        assert_eq!(demuxer.try_next(), Ok(None));
        assert_eq!(demuxer.close(), Some(0));
        info
    }

    #[test]
    fn mp4_with_h264_matches_ffprobe() {
        let info = matches_ffprobe(
            "format-h264.mp4",
            "mov,mp4,m4a,3gp,3g2,mj2",
            &[("h264", StreamKind::Video)],
        );
        let stream = &info.streams[0];
        let video = stream.video.as_ref().unwrap();
        assert_eq!((video.width, video.height), (64, 64));
        assert_eq!(video.pixel_format, "yuv420p");
        assert_eq!(video.frame_rate, Rational::new(25, 1));
        assert_eq!(stream.time_base, Rational::lit(1, 12_800));
        assert_eq!(stream.frames, Some(50));
        assert_eq!(stream.duration, Some(Timestamp::from_millis(2000)));
        assert_eq!(info.duration, Some(Timestamp::from_millis(2000)));
        // The decoder configuration of MP4 (avcC) starts with its version byte.
        assert_eq!(stream.extradata.first(), Some(&1));
    }

    #[test]
    fn the_colour_of_an_hdr_file_arrives_as_its_code_points() {
        let demuxer = Demuxer::open_path(clip("colour-hdr10.mkv"), &helper()).unwrap();
        let video = demuxer.info().streams[0].video.clone().unwrap();
        // What ffprobe calls bt2020, smpte2084, bt2020nc and tv: H.273 says 9, 16, 9; limited is 1.
        assert_eq!(
            (
                video.color_primaries,
                video.color_transfer,
                video.color_matrix,
                video.color_range
            ),
            (9, 16, 9, 1)
        );
    }

    #[test]
    fn mp4_with_hevc_matches_ffprobe() {
        matches_ffprobe(
            "format-hevc.mp4",
            "mov,mp4,m4a,3gp,3g2,mj2",
            &[("hevc", StreamKind::Video)],
        );
    }

    #[test]
    fn webm_with_vp9_matches_ffprobe() {
        let info = matches_ffprobe(
            "format-vp9.webm",
            "matroska,webm",
            &[("vp9", StreamKind::Video)],
        );
        assert_eq!(info.streams[0].time_base, Rational::MILLIS);
    }

    #[test]
    fn mkv_with_av1_matches_ffprobe() {
        matches_ffprobe(
            "format-av1.mkv",
            "matroska,webm",
            &[("av1", StreamKind::Video)],
        );
    }

    #[test]
    fn mkv_with_picture_sound_and_subtitles_matches_ffprobe() {
        let info = matches_ffprobe(
            "pattern-3s.mkv",
            "matroska,webm",
            &[
                ("h264", StreamKind::Video),
                ("vorbis", StreamKind::Audio),
                ("subrip", StreamKind::Subtitle),
                ("subrip", StreamKind::Subtitle),
            ],
        );
        let audio = info.streams[1].audio.as_ref().unwrap();
        assert_eq!((audio.sample_rate, audio.channels), (48_000, 1));
        assert_eq!(audio.sample_format, "fltp");
        // Vorbis carries its three headers as codec configuration.
        assert!(info.streams[1].extradata.len() > 100);
        assert!(info.streams[0].video.is_some() && info.streams[0].audio.is_none());
        assert!(info.streams[2].video.is_none() && info.streams[2].audio.is_none());
    }

    #[test]
    fn a_seek_lands_on_the_keyframe_before_the_target_and_the_file_reads_on() {
        let path = clip("long-gop-12s.mkv");
        let expected = probed_packets(&path);
        let keyframes: Vec<i64> = expected
            .iter()
            .filter(|p| p.6)
            .filter_map(|p| p.1)
            .collect();
        assert!(keyframes.len() >= 3, "the clip has keyframes to seek to");
        let mut demuxer = Demuxer::open_path(&path, &helper()).unwrap();
        let base = demuxer.info().streams[0].time_base;

        for target_ms in [7000i64, 0, 11_900, 5000] {
            demuxer.seek(Timestamp::from_millis(target_ms)).unwrap();
            let want = *keyframes
                .iter()
                .filter(|pts| Timestamp::new(**pts, base) <= Timestamp::from_millis(target_ms))
                .max()
                .unwrap();
            let Some(Event::Packet(first)) = demuxer.next(WAIT).unwrap() else {
                panic!("no packet after the seek to {target_ms} ms");
            };
            assert!(first.key, "seek to {target_ms} ms");
            assert_eq!(
                first.pts,
                Some(Timestamp::new(want, base)),
                "{target_ms} ms"
            );
            // From there to the end, exactly what the file has from that packet on.
            let (rest, error) = drain(&mut demuxer);
            assert_eq!(error, None);
            let from = expected.iter().position(|p| p.1 == Some(want)).unwrap();
            assert_eq!(rest.len() + 1, expected.len() - from, "{target_ms} ms");
        }
        // A seek after the end reads again; that was the last round above (it ended each time).
    }

    #[test]
    fn packets_from_before_a_seek_never_arrive_after_it() {
        // A ring too small for the file: the helper is stopped in the middle of writing when the
        // seek comes, and the ring is full of packets from before.
        let options = Options {
            ring_capacity: 4096,
            ..helper()
        };
        let path = clip("long-gop-12s.mkv");
        let mut demuxer = Demuxer::open_path(&path, &options).unwrap();
        let base = demuxer.info().streams[0].time_base;
        // Let the helper fill the ring.
        let Some(Event::Packet(first)) = demuxer.next(WAIT).unwrap() else {
            panic!("no first packet");
        };
        assert_eq!(first.pts, Some(Timestamp::new(0, base)));
        std::thread::sleep(Duration::from_millis(200));
        assert!(demuxer.buffered().unwrap() > 0);

        demuxer.seek(Timestamp::from_millis(9000)).unwrap();
        let (packets, error) = drain(&mut demuxer);
        assert_eq!(error, None);
        let first = packets.first().unwrap();
        assert!(first.key);
        assert!(first.pts.unwrap() <= Timestamp::from_millis(9000));
        assert!(first.pts.unwrap() > Timestamp::from_millis(1000));
        // In order from there: nothing of the old position slipped in.
        assert!(
            packets
                .windows(2)
                .all(|pair| pair[0].dts.unwrap() < pair[1].dts.unwrap())
        );
    }

    #[test]
    fn a_buffer_handed_back_is_used_again() {
        let mut demuxer = Demuxer::open_path(clip("format-h264.mp4"), &helper()).unwrap();
        let Some(Event::Packet(mut first)) = demuxer.next(WAIT).unwrap() else {
            panic!("no packet");
        };
        first.data.reserve(4096);
        let (address, capacity) = (first.data.as_ptr(), first.data.capacity());
        demuxer.recycle(first);
        // The buffer of the record in flight was taken before; the handed-back one is next.
        let Some(Event::Packet(_second)) = demuxer.next(WAIT).unwrap() else {
            panic!("no packet");
        };
        let Some(Event::Packet(third)) = demuxer.next(WAIT).unwrap() else {
            panic!("no packet");
        };
        assert_eq!(
            (third.data.as_ptr(), third.data.capacity()),
            (address, capacity)
        );
    }

    fn refused(content: &[u8], name: &str) -> (u32, String) {
        let file = Temp::new(name, content);
        match Demuxer::open_path(&file.0, &helper()) {
            Err(DemuxError::Refused { code, text }) => (code, text),
            other => panic!("{name}: {other:?}"),
        }
    }

    #[test]
    fn what_is_no_container_is_refused_and_the_helper_survives_it() {
        assert_eq!(refused(b"", "empty").0, proto::fault::OPEN);
        assert_eq!(
            refused(b"just a text file, nothing to play\n", "text").0,
            proto::fault::OPEN
        );
        let noise: Vec<u8> = (0..100_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        assert_eq!(refused(&noise, "noise").0, proto::fault::OPEN);
    }

    #[test]
    fn a_container_that_is_not_on_the_list_is_refused() {
        // A correct WAVE file: libavformat has a demuxer for it, the helper may not use it.
        let mut wav = Vec::new();
        let samples = [0u8; 4800];
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&96_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        wav.extend_from_slice(&samples);
        let (code, text) = refused(&wav, "sound.wav");
        assert_eq!(code, proto::fault::OPEN, "{text}");
    }

    /// Opens `content` and reads it to its end. Every outcome is fine but two: this process must
    /// not panic, and nothing may take longer than the limits allow.
    fn survive(content: &[u8], name: &str) -> &'static str {
        let file = Temp::new(name, content);
        let mut demuxer = match Demuxer::open_path(&file.0, &helper()) {
            Ok(demuxer) => demuxer,
            Err(DemuxError::Refused { .. }) => return "refused",
            Err(DemuxError::Helper(_)) => return "helper lost at open",
            Err(other) => panic!("{name}: {other:?}"),
        };
        loop {
            match demuxer.next(WAIT) {
                Ok(Some(Event::End { error: None })) => return "read to the end",
                Ok(Some(Event::End { error: Some(_) })) => return "read error",
                Ok(Some(_)) => {}
                Ok(None) => panic!("{name}: no event within {WAIT:?}"),
                Err(DemuxError::Helper(_)) => return "helper lost",
                Err(other) => panic!("{name}: {other:?}"),
            }
        }
    }

    #[test]
    fn cut_off_files_end_in_an_answer() {
        for name in ["pattern-3s.mkv", "format-h264.mp4", "format-vp9.webm"] {
            let whole = std::fs::read(clip(name)).unwrap();
            let mut outcomes = Vec::new();
            for percent in [1, 5, 20, 50, 80, 99] {
                let cut = &whole[..whole.len() * percent / 100];
                outcomes.push(survive(cut, &format!("cut-{percent}-{name}")));
            }
            // A Matroska file cut in its second half still plays up to the cut.
            if name == "pattern-3s.mkv" {
                assert_eq!(outcomes[3..], ["read to the end"; 3], "{outcomes:?}");
            }
            assert!(
                outcomes.iter().all(|o| !o.starts_with("helper lost")),
                "{name}: {outcomes:?}"
            );
        }
    }

    #[test]
    fn damaged_files_end_in_an_answer() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut lost = 0;
        for name in ["pattern-3s.mkv", "format-h264.mp4", "format-vp9.webm"] {
            let whole = std::fs::read(clip(name)).unwrap();
            for round in 0..12 {
                let mut damaged = whole.clone();
                // A few bytes anywhere, and a run of them in the head, where the structure is.
                for _ in 0..8 {
                    let at = (random() % damaged.len() as u64) as usize;
                    damaged[at] = random() as u8;
                }
                let head = damaged.len().min(600);
                let at = (random() % head as u64) as usize;
                for byte in damaged.iter_mut().skip(at).take(4) {
                    *byte = random() as u8;
                }
                let outcome = survive(&damaged, &format!("damaged-{round}-{name}"));
                if outcome.starts_with("helper lost") {
                    lost += 1;
                }
            }
        }
        // A helper that dies on a damaged file is what the process boundary is for, and the parent
        // survived it; but on these files libavformat is expected to cope.
        assert_eq!(lost, 0, "the helper was lost {lost} times");
    }

    #[test]
    fn a_file_that_is_gone_is_an_error_of_this_program() {
        let err = Demuxer::open_path(clip("no-such-file.mkv"), &helper()).unwrap_err();
        assert!(matches!(err, DemuxError::File(_)), "{err:?}");
    }

    #[test]
    fn a_packet_above_the_limit_is_dropped_and_the_stream_goes_on() {
        // The first packet of this file has 668 bytes, the others at most 25.
        let mut options = helper();
        options.limits.max_record = 256;
        let path = clip("long-gop-12s.mkv");
        let mut demuxer = Demuxer::open_path(&path, &options).unwrap();
        let mut dropped = Vec::new();
        let mut packets = 0;
        loop {
            match demuxer.next(WAIT).unwrap().unwrap() {
                Event::Packet(packet) => {
                    assert!(packet.data.len() <= 256 - proto::PACKET_TAIL);
                    packets += 1;
                }
                Event::Dropped { stream, size } => dropped.push((stream, size)),
                Event::End { error } => {
                    assert_eq!(error, None);
                    break;
                }
            }
        }
        let expected = probed_packets(&path);
        let large: Vec<(u32, u64)> = expected
            .iter()
            .filter(|p| p.4 > 256 - proto::PACKET_TAIL)
            .map(|p| (p.0, p.4 as u64))
            .collect();
        assert!(!large.is_empty());
        assert_eq!(dropped, large);
        assert_eq!(packets + dropped.len(), expected.len());
    }

    #[test]
    fn the_ffmpeg_of_the_helper_is_an_lgpl_build() {
        let out = Command::new(env!("CARGO_BIN_EXE_takt-demux-helper"))
            .arg("--about")
            .output()
            .unwrap();
        let about = String::from_utf8_lossy(&out.stdout);
        println!("{about}");
        assert!(about.contains("libavformat 63."), "{about}");
        // A GPL build of FFmpeg is not what Takt ships with (KONZEPT, license zones; BUILD.md).
        let license = about
            .lines()
            .find_map(|line| line.strip_prefix("license: "))
            .unwrap_or_default();
        assert!(
            license.starts_with("LGPL"),
            "FFmpeg build is licensed {license:?}, takt needs an LGPL build"
        );
    }
}
