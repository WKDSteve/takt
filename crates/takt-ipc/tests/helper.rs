// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The parent's side against a real process: `takt-ipc-probe`, a helper that behaves or misbehaves
//! as its first argument says.

use std::io::Write;
use std::process::Command;
use std::time::{Duration, Instant};

use takt_ipc::message::{AppId, kind};
use takt_ipc::process::{Helper, HelperCommand, Limits};
use takt_ipc::ring::pause;
use takt_ipc::{IpcError, ProtocolError};

const APP: AppId = AppId {
    name: *b"PROB",
    version: 1,
};
const REQUEST: u16 = kind::APP;
const REPLY: u16 = kind::APP + 1;

fn probe(mode: &str) -> HelperCommand {
    HelperCommand::new(env!("CARGO_BIN_EXE_takt-ipc-probe"), APP)
        .arg(mode)
        .ring_capacity(64 * 1024)
        .limits(quick())
}

/// Limits short enough for a test and long enough for a loaded machine.
fn quick() -> Limits {
    Limits {
        heartbeat: Duration::from_millis(500),
        handshake: Duration::from_secs(20),
        ..Limits::default()
    }
}

/// Whether a process with this id exists.
fn alive(pid: u32) -> bool {
    if cfg!(windows) {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("tasklist runs");
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .expect("kill runs")
            .status
            .success()
    }
}

fn gone_within(pid: u32, time: Duration) -> bool {
    let deadline = Instant::now() + time;
    while alive(pid) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    true
}

/// Waits until the helper is gone and returns why.
fn end_of(helper: &Helper, time: Duration) -> IpcError {
    let deadline = Instant::now() + time;
    loop {
        if let Err(err) = helper.check() {
            return err;
        }
        assert!(Instant::now() < deadline, "the helper still runs");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn frames_and_records_go_back_and_forth_and_the_helper_leaves_when_told() {
    let (mut helper, mut reader) = probe("echo").spawn_reading().unwrap();
    let mut record = Vec::new();
    for seq in 0..200u32 {
        let payload = vec![(seq % 251) as u8; (seq as usize * 7) % 900];
        helper.send(REQUEST, seq, &payload).unwrap();
        let reply = helper.recv(Duration::from_secs(20)).unwrap();
        assert_eq!((reply.kind, reply.seq), (REPLY, seq));
        assert_eq!(reply.payload, payload);
        // The record was written before the reply was sent.
        assert!(reader.try_read(&mut record).unwrap());
        assert_eq!(record, payload);
    }
    assert!(!reader.try_read(&mut record).unwrap());
    let pid = helper.pid();
    assert_eq!(helper.shutdown(), Some(0));
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn the_parent_writes_the_ring_when_the_helper_is_the_reader() {
    let (mut helper, mut writer) = probe("echo").spawn_writing().unwrap();
    for i in 0..50u8 {
        let mut attempt = 0;
        while !writer.try_write(&[&[i; 100]]).unwrap() {
            pause(attempt);
            attempt += 1;
        }
    }
    for i in 0..50u8 {
        let reply = helper.recv(Duration::from_secs(20)).unwrap();
        assert_eq!(reply.payload, [i; 100]);
    }
    assert_eq!(helper.shutdown(), Some(0));
}

#[test]
fn an_unknown_request_is_answered_with_a_fault() {
    let (mut helper, _reader) = probe("echo").spawn_reading().unwrap();
    helper.send(kind::APP + 77, 5, b"?").unwrap();
    let reply = helper.recv(Duration::from_secs(20)).unwrap();
    assert_eq!((reply.kind, reply.seq), (kind::FAULT, 5));
    let fault = takt_ipc::message::Fault::parse(&reply.payload).unwrap();
    assert_eq!((fault.code, fault.text.as_str()), (1, "unknown request"));
}

#[test]
fn a_helper_that_hangs_is_ended_by_the_watchdog() {
    let (helper, _reader) = probe("hang").spawn_reading().unwrap();
    let pid = helper.pid();
    let started = Instant::now();
    assert_eq!(
        end_of(&helper, Duration::from_secs(30)),
        IpcError::Timeout("heartbeat")
    );
    // Not before the limit, and the process is really gone.
    assert!(started.elapsed() >= Duration::from_millis(500));
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn a_helper_that_is_slow_to_start_is_not_taken_for_hung() {
    // A second and a half until its hello, against a heartbeat limit of half a second: the time a
    // program needs to start counts against the handshake, not against the watchdog.
    let (mut helper, _reader) = probe("slow").spawn_reading().unwrap();
    helper.send(REQUEST, 1, b"there?").unwrap();
    let reply = helper.recv(Duration::from_secs(20)).unwrap();
    assert_eq!(reply.payload, b"there?");
    assert_eq!(helper.check(), Ok(()));
    assert_eq!(helper.shutdown(), Some(0));
}

#[test]
fn a_helper_that_never_answers_is_ended_at_the_deadline_of_the_call() {
    let (mut helper, _reader) = probe("deaf").spawn_reading().unwrap();
    let pid = helper.pid();
    helper.send(REQUEST, 1, b"anyone?").unwrap();
    assert_eq!(
        helper.recv(Duration::from_millis(300)),
        Err(IpcError::Timeout("answer"))
    );
    // And it stays the reason.
    assert_eq!(helper.check(), Err(IpcError::Timeout("answer")));
    assert_eq!(helper.try_recv(), Err(IpcError::Timeout("answer")));
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn a_helper_that_floods_the_pipe_is_ended() {
    let limits = Limits {
        max_queued_frames: 16,
        ..quick()
    };
    let (helper, _reader) = probe("flood").limits(limits).spawn_reading().unwrap();
    let pid = helper.pid();
    assert_eq!(
        end_of(&helper, Duration::from_secs(30)),
        IpcError::Limit("frames nobody fetched")
    );
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn bytes_that_are_no_frame_end_the_helper() {
    let (helper, _reader) = probe("garbage").spawn_reading().unwrap();
    let pid = helper.pid();
    assert!(matches!(
        end_of(&helper, Duration::from_secs(30)),
        IpcError::Protocol(ProtocolError::TooLarge { .. })
    ));
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn a_program_that_is_no_helper_fails_the_handshake() {
    let err = probe("stranger").spawn_reading().unwrap_err();
    assert!(
        matches!(err, IpcError::Protocol(ProtocolError::TooLarge { .. })),
        "{err:?}"
    );
}

#[test]
fn a_helper_of_another_version_fails_the_handshake() {
    let newer = AppId {
        name: *b"PROB",
        version: 2,
    };
    let err = HelperCommand::new(env!("CARGO_BIN_EXE_takt-ipc-probe"), newer)
        .arg("echo")
        .limits(quick())
        .spawn_reading()
        .unwrap_err();
    // Whichever side notices first: the parent reads the helper's hello, or the helper refuses the
    // parent's and exits.
    assert!(
        matches!(
            err,
            IpcError::Protocol(ProtocolError::Version { theirs: 1, ours: 2 })
                | IpcError::Exited { code: Some(2), .. }
        ),
        "{err:?}"
    );
}

#[test]
fn too_much_on_stderr_ends_the_helper() {
    let limits = Limits {
        max_stderr: 64 * 1024,
        ..quick()
    };
    let (helper, _reader) = probe("stderr").limits(limits).spawn_reading().unwrap();
    assert_eq!(
        end_of(&helper, Duration::from_secs(30)),
        IpcError::Limit("bytes on stderr")
    );
}

#[test]
fn a_helper_that_dies_leaves_its_code_and_its_last_words() {
    let (mut helper, _reader) = probe("exit").spawn_reading().unwrap();
    let err = helper.recv(Duration::from_secs(20)).unwrap_err();
    let IpcError::Exited { code, stderr } = err else {
        panic!("{err:?}");
    };
    assert_eq!(code, Some(7));
    // Foreign text: the escape sequence it ended with is gone.
    assert_eq!(stderr, "takt-ipc-probe: giving up [31m");
}

#[test]
fn a_program_that_does_not_exist_is_an_error_and_no_panic() {
    let err = HelperCommand::new("no-such-program-of-takt", APP)
        .spawn_reading()
        .unwrap_err();
    assert!(matches!(err, IpcError::Spawn(_)), "{err:?}");
}

#[test]
fn a_ring_that_is_no_power_of_two_is_refused_before_anything_starts() {
    let err = probe("echo")
        .ring_capacity(1000)
        .spawn_reading()
        .unwrap_err();
    assert!(matches!(err, IpcError::Protocol(ProtocolError::Ring(_))));
}

#[test]
fn a_file_reaches_the_helper_as_a_handle() {
    let path = std::env::temp_dir().join(format!("takt-ipc-test-{}.bin", std::process::id()));
    std::fs::File::create(&path)
        .unwrap()
        .write_all(b"0123456789abcdefTAIL")
        .unwrap();
    let file = std::fs::File::open(&path).unwrap();
    let (mut helper, _reader) = probe("file").file(file).spawn_reading().unwrap();
    let reply = helper.recv(Duration::from_secs(20)).unwrap();
    assert_eq!(reply.payload, b"0123456789abcdef");
    assert_eq!(helper.shutdown(), Some(0));
    std::fs::remove_file(&path).unwrap();
}

// The memory limit is a job object under Windows and RLIMIT_AS under Linux; macOS enforces neither.
#[cfg(any(windows, target_os = "linux"))]
#[test]
fn the_memory_limit_refuses_a_large_allocation() {
    let limits = Limits {
        memory: Some(512 << 20),
        ..quick()
    };
    let (mut helper, _reader) = probe("alloc").limits(limits).spawn_reading().unwrap();
    let reply = helper.recv(Duration::from_secs(20)).unwrap();
    assert_eq!(
        reply.payload,
        [0],
        "1 GiB was handed out under a 512 MiB limit"
    );
    // Without the limit the same helper gets it: the refusal above was the limit.
    let (mut helper, _reader) = probe("alloc").spawn_reading().unwrap();
    let reply = helper.recv(Duration::from_secs(20)).unwrap();
    assert_eq!(reply.payload, [1]);
}

#[cfg(not(any(windows, target_os = "linux")))]
#[test]
#[ignore = "SKIP: no memory limit for helpers on this system (macOS does not enforce RLIMIT_AS)"]
fn the_memory_limit_refuses_a_large_allocation() {}

// Only the job object of Windows takes the right to start processes away.
#[cfg(windows)]
#[test]
fn a_helper_cannot_start_a_process_of_its_own() {
    let (mut helper, _reader) = probe("spawn").spawn_reading().unwrap();
    let reply = helper.recv(Duration::from_secs(20)).unwrap();
    assert_eq!(reply.payload, [0]);
    let limits = Limits {
        child_processes: true,
        ..quick()
    };
    let (mut helper, _reader) = probe("spawn").limits(limits).spawn_reading().unwrap();
    let reply = helper.recv(Duration::from_secs(20)).unwrap();
    assert_eq!(reply.payload, [1]);
}

#[cfg(not(windows))]
#[test]
#[ignore = "SKIP: helpers are kept from starting processes under Windows only (job object)"]
fn a_helper_cannot_start_a_process_of_its_own() {}

#[test]
fn a_helper_does_not_outlive_its_parent() {
    // The probe in the middle is the parent here: it starts a helper of its own, and then it is
    // ended without any chance to clean up.
    let limits = Limits {
        child_processes: true,
        ..quick()
    };
    let (mut middle, _reader) = probe("nest").limits(limits).spawn_reading().unwrap();
    let reply = middle.recv(Duration::from_secs(30)).unwrap();
    let inner = u32::from_le_bytes(reply.payload.try_into().unwrap());
    assert!(alive(inner));
    let middle_pid = middle.pid();
    drop(middle);
    assert!(gone_within(middle_pid, Duration::from_secs(10)));
    assert!(
        gone_within(inner, Duration::from_secs(10)),
        "the helper of an ended parent still runs"
    );
}

#[test]
fn dropping_the_helper_ends_the_process() {
    let (helper, _reader) = probe("deaf").spawn_reading().unwrap();
    let pid = helper.pid();
    assert!(alive(pid));
    drop(helper);
    assert!(gone_within(pid, Duration::from_secs(10)));
}
