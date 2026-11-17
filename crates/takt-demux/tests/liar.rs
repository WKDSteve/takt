// SPDX-License-Identifier: GPL-3.0-or-later
//! The parent's side of the demuxer against a helper that lies.
//!
//! The helper of these tests is this very program: started with the ring argument of `takt-ipc` it
//! plays a compromised demux helper (it speaks the handshake, then sends what its first argument
//! says); started plainly it is the test runner. That needs a `main` of its own (`harness = false`
//! in `Cargo.toml`), because the standard harness writes to stdout, which is the control pipe of a
//! helper. No FFmpeg is involved: what is tested is what the main program does with bytes of a
//! helper it cannot trust.

use std::fs::File;
use std::io::Read;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use takt_core::{Rational, Timestamp};
use takt_demux::proto::{self, MediaInfo, PacketHead, StreamInfo, StreamKind, VideoParams};
use takt_demux::{DemuxError, Demuxer, Event, Options};
use takt_ipc::child::{HelperContext, RingEnd, connect};
use takt_ipc::ring::RingWriter;
use takt_ipc::{IpcError, ProtocolError};

fn main() -> ExitCode {
    if std::env::args().any(|arg| arg.starts_with("--takt-ipc=")) {
        return lie();
    }
    let cases: [(&str, fn()); 9] = [
        ("an_honest_stream_arrives", an_honest_stream_arrives as fn()),
        (
            "a_stream_table_with_a_zero_time_base_ends_the_helper",
            a_stream_table_with_a_zero_time_base_ends_the_helper,
        ),
        (
            "a_stream_table_that_is_cut_off_ends_the_helper",
            a_stream_table_that_is_cut_off_ends_the_helper,
        ),
        (
            "another_frame_than_the_stream_table_ends_the_helper",
            another_frame_than_the_stream_table_ends_the_helper,
        ),
        (
            "a_packet_of_a_stream_that_does_not_exist_ends_the_helper",
            a_packet_of_a_stream_that_does_not_exist_ends_the_helper,
        ),
        (
            "a_timestamp_that_is_the_no_value_number_ends_the_helper",
            a_timestamp_that_is_the_no_value_number_ends_the_helper,
        ),
        (
            "a_record_that_is_no_record_ends_the_helper",
            a_record_that_is_no_record_ends_the_helper,
        ),
        (
            "an_answer_to_another_seek_ends_the_helper",
            an_answer_to_another_seek_ends_the_helper,
        ),
        (
            "a_helper_that_never_opens_is_ended_at_the_deadline",
            a_helper_that_never_opens_is_ended_at_the_deadline,
        ),
    ];
    let filter = std::env::args().nth(1).filter(|arg| !arg.starts_with('-'));
    println!("\nrunning {} tests", cases.len());
    let mut passed = 0;
    for (name, case) in cases {
        if filter.as_ref().is_some_and(|f| !name.contains(f.as_str())) {
            continue;
        }
        // A failed assertion panics and ends the run with the message; that is the report.
        case();
        println!("test {name} ... ok");
        passed += 1;
    }
    println!(
        "\ntest result: ok. {passed} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
    );
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------------------------
// The lying helper.

fn table() -> MediaInfo {
    let time_base = Rational::lit(1, 1000);
    MediaInfo {
        format: "matroska,webm".to_owned(),
        duration: None,
        start: None,
        streams: vec![StreamInfo {
            index: 0,
            kind: StreamKind::Video,
            codec: "h264".to_owned(),
            codec_tag: 0,
            time_base,
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
                ..VideoParams::default()
            }),
            audio: None,
        }],
    }
}

fn write(ring: &mut RingWriter, ctx: &HelperContext, parts: &[&[u8]]) {
    while !ring.try_write(parts).unwrap_or(true) {
        ctx.beat();
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn packet(ring: &mut RingWriter, ctx: &HelperContext, data: &[u8], head: &PacketHead) {
    write(ring, ctx, &[data, &head.tail()]);
}

/// Stays alive (and beating) until the parent ends it.
fn linger(mut ctx: HelperContext) -> ExitCode {
    loop {
        let _ = ctx.recv(Duration::from_millis(20));
    }
}

fn lie() -> ExitCode {
    let Ok((mut ctx, RingEnd::Writer(mut ring))) = connect(proto::APP) else {
        return ExitCode::from(2);
    };
    let mut mode = String::new();
    if let Some(mut file) = ctx.take_file() {
        let _ = file.read_to_string(&mut mode);
    }
    let opened = table().encode();
    let good = PacketHead {
        pts: Some(40),
        dts: Some(40),
        duration: Some(40),
        key: true,
        ..PacketHead::default()
    };
    match mode.as_str() {
        "honest" => {
            let _ = ctx.send(proto::kind::OPENED, 0, &opened);
            packet(&mut ring, &ctx, b"first", &good);
            packet(
                &mut ring,
                &ctx,
                b"second",
                &PacketHead {
                    pts: Some(80),
                    ..good
                },
            );
            write(&mut ring, &ctx, &[&proto::end_tail(0, 0)]);
        }
        "zero-time-base" => {
            // The time base follows the format name, two optional times, the stream count, the
            // index, the kind, the codec name and the codec tag.
            let at = (2 + "matroska,webm".len()) + 8 + 8 + 4 + 4 + 4 + (2 + "h264".len()) + 4;
            let mut bytes = opened;
            bytes[at..at + 8].copy_from_slice(&0i64.to_le_bytes());
            let _ = ctx.send(proto::kind::OPENED, 0, &bytes);
        }
        "cut-table" => {
            let _ = ctx.send(proto::kind::OPENED, 0, &opened[..opened.len() - 3]);
        }
        "wrong-first-frame" => {
            let _ = ctx.send(proto::kind::SEEK_DONE, 0, &[]);
        }
        "unknown-stream" => {
            let _ = ctx.send(proto::kind::OPENED, 0, &opened);
            packet(&mut ring, &ctx, b"fine", &good);
            packet(&mut ring, &ctx, b"lie", &PacketHead { stream: 7, ..good });
        }
        "no-value-time" => {
            let _ = ctx.send(proto::kind::OPENED, 0, &opened);
            let head = PacketHead {
                pts: Some(i64::MIN),
                ..good
            };
            packet(&mut ring, &ctx, b"lie", &head);
        }
        "no-record" => {
            let _ = ctx.send(proto::kind::OPENED, 0, &opened);
            write(&mut ring, &ctx, &[b"neither a packet nor an end"]);
        }
        "wrong-seek-answer" => {
            let _ = ctx.send(proto::kind::OPENED, 0, &opened);
            loop {
                if let Some(frame) = ctx.recv(Duration::from_millis(20)) {
                    let _ = ctx.send(proto::kind::SEEK_DONE, frame.seq.wrapping_add(1), &[]);
                    break;
                }
            }
        }
        // "silent": the handshake, then nothing.
        _ => {}
    }
    linger(ctx)
}

// ---------------------------------------------------------------------------------------------
// The cases.

fn open(mode: &str, open_timeout: Duration) -> Result<Demuxer, DemuxError> {
    let me = std::env::current_exe().expect("own path");
    // The file the helper inherits carries the name of the lie: the liar reads no container.
    let path = std::env::temp_dir().join(format!("takt-liar-{}-{mode}", std::process::id()));
    std::fs::write(&path, mode).expect("temp file");
    let file = File::open(&path).expect("temp file opens");
    let options = Options {
        open_timeout,
        ..Options::new(me)
    };
    let opened = Demuxer::open(file, &options);
    let _ = std::fs::remove_file(&path);
    opened
}

const PATIENT: Duration = Duration::from_secs(30);

fn gone(demuxer_pid: u32) {
    // The process was ended and reaped before the error was returned; give the system a moment to
    // drop it from its table, then look.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let alive = if cfg!(windows) {
            let out = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {demuxer_pid}"), "/NH", "/FO", "CSV"])
                .output()
                .expect("tasklist runs");
            String::from_utf8_lossy(&out.stdout).contains(&format!("\"{demuxer_pid}\""))
        } else {
            std::process::Command::new("kill")
                .args(["-0", &demuxer_pid.to_string()])
                .output()
                .expect("kill runs")
                .status
                .success()
        };
        if !alive {
            return;
        }
        assert!(Instant::now() < deadline, "the lying helper still runs");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn protocol(err: &DemuxError) -> &ProtocolError {
    let DemuxError::Helper(IpcError::Protocol(err)) = err else {
        panic!("not a protocol error: {err:?}");
    };
    err
}

fn an_honest_stream_arrives() {
    let mut demuxer = open("honest", PATIENT).unwrap();
    assert_eq!(demuxer.info(), &table());
    let Some(Event::Packet(first)) = demuxer.next(PATIENT).unwrap() else {
        panic!("no packet");
    };
    assert_eq!(first.data, b"first");
    assert_eq!(first.pts, Some(Timestamp::from_millis(40)));
    let Some(Event::Packet(second)) = demuxer.next(PATIENT).unwrap() else {
        panic!("no packet");
    };
    assert_eq!(second.pts, Some(Timestamp::from_millis(80)));
    assert_eq!(
        demuxer.next(PATIENT).unwrap(),
        Some(Event::End { error: None })
    );
}

fn a_stream_table_with_a_zero_time_base_ends_the_helper() {
    let err = open("zero-time-base", PATIENT).unwrap_err();
    assert_eq!(protocol(&err), &ProtocolError::Invalid("time base"));
}

fn a_stream_table_that_is_cut_off_ends_the_helper() {
    let err = open("cut-table", PATIENT).unwrap_err();
    assert_eq!(protocol(&err), &ProtocolError::Truncated);
}

fn another_frame_than_the_stream_table_ends_the_helper() {
    let err = open("wrong-first-frame", PATIENT).unwrap_err();
    assert_eq!(
        protocol(&err),
        &ProtocolError::Invalid("first frame of the demuxer")
    );
}

/// Reads until the demuxer fails, and checks that the helper is gone and stays the reason.
fn fails_with(mode: &str, good_packets: usize, want: &ProtocolError) {
    let mut demuxer = open(mode, PATIENT).unwrap();
    let pid = demuxer.helper_pid();
    for _ in 0..good_packets {
        assert!(matches!(
            demuxer.next(PATIENT).unwrap(),
            Some(Event::Packet(_))
        ));
    }
    let err = match demuxer.next(PATIENT) {
        Ok(None) => panic!("{mode}: nothing came"),
        Ok(Some(event)) => panic!("{mode}: the lie was handed out: {event:?}"),
        Err(err) => err,
    };
    assert_eq!(protocol(&err), want, "{mode}");
    gone(pid);
    // Finished for good: the same error from now on, never a packet.
    assert_eq!(demuxer.try_next().unwrap_err(), err);
    assert_eq!(demuxer.seek(Timestamp::from_millis(0)).unwrap_err(), err);
}

fn a_packet_of_a_stream_that_does_not_exist_ends_the_helper() {
    fails_with(
        "unknown-stream",
        1,
        &ProtocolError::Invalid("stream index of a record"),
    );
}

fn a_timestamp_that_is_the_no_value_number_ends_the_helper() {
    fails_with("no-value-time", 0, &ProtocolError::Invalid("pts"));
}

fn a_record_that_is_no_record_ends_the_helper() {
    fails_with("no-record", 0, &ProtocolError::Invalid("tag of a record"));
}

fn an_answer_to_another_seek_ends_the_helper() {
    let mut demuxer = open("wrong-seek-answer", PATIENT).unwrap();
    let pid = demuxer.helper_pid();
    let err = demuxer.seek(Timestamp::from_millis(500)).unwrap_err();
    assert_eq!(
        protocol(&err),
        &ProtocolError::Invalid("answer to another request")
    );
    gone(pid);
}

fn a_helper_that_never_opens_is_ended_at_the_deadline() {
    let started = Instant::now();
    let err = open("silent", Duration::from_millis(400)).unwrap_err();
    assert_eq!(err, DemuxError::Helper(IpcError::Timeout("answer")));
    assert!(started.elapsed() >= Duration::from_millis(400));
}
