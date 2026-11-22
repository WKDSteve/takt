// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A helper that speaks the protocol of `takt-ipc` and misbehaves on demand.
//!
//! The tests of the crate start it to see what the parent's side does with a helper that hangs,
//! floods, lies or dies. The first argument picks the behaviour:
//!
//! | mode | what it does |
//! |---|---|
//! | `echo` | answers every application frame with the same payload and writes it to the ring as a record; says goodbye when told to |
//! | `hang` | shakes hands, then neither beats nor answers |
//! | `deaf` | beats, but never answers |
//! | `flood` | sends frames as fast as it can |
//! | `garbage` | shakes hands, then writes bytes that are no frame |
//! | `stranger` | writes text to stdout instead of a hello |
//! | `stderr` | writes to stderr without end |
//! | `exit` | writes a line to stderr and exits with code 7 |
//! | `alloc` | tries to reserve 1 GiB and reports whether it got it |
//! | `spawn` | tries to start a process and reports whether it could |
//! | `file` | answers with the first bytes of the file it inherited |
//! | `slow` | takes a second and a half to say hello, then behaves like `echo` |
//! | `nest` | starts another probe in mode `deaf` as its own helper and reports that probe's process id |
//!
//! Not part of any product; `takt-ipc` builds it so that its tests and other implementations of the
//! protocol have a peer.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use takt_ipc::child::{RingEnd, connect};
use takt_ipc::message::{AppId, kind};
use takt_ipc::process::{HelperCommand, Limits};
use takt_ipc::ring::pause;

/// The application the probe speaks.
const APP: AppId = AppId {
    name: *b"PROB",
    version: 1,
};
/// A request; the answer is [`REPLY`].
const REQUEST: u16 = kind::APP;
/// The answer to a [`REQUEST`].
const REPLY: u16 = kind::APP + 1;

fn main() -> ExitCode {
    let mode = std::env::args_os()
        .nth(1)
        .and_then(|arg| arg.into_string().ok())
        .unwrap_or_default();
    if mode == "slow" {
        std::thread::sleep(Duration::from_millis(1500));
    }
    if mode == "stranger" {
        println!("hello, I am not a helper");
        std::thread::sleep(Duration::from_secs(30));
        return ExitCode::SUCCESS;
    }
    let (mut ctx, ring) = match connect(APP) {
        Ok(connected) => connected,
        Err(err) => {
            eprintln!("takt-ipc-probe: {err}");
            return ExitCode::from(2);
        }
    };
    match mode.as_str() {
        "hang" => loop {
            std::thread::sleep(Duration::from_secs(1));
        },
        "deaf" => loop {
            ctx.beat();
            std::thread::sleep(Duration::from_millis(20));
        },
        "flood" => loop {
            if ctx.send(REPLY, 0, &[0; 64]).is_err() {
                return ExitCode::from(3);
            }
        },
        "garbage" => {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(&[0xFF; 64]);
            let _ = stdout.flush();
            drop(stdout);
            loop {
                ctx.beat();
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        "stderr" => loop {
            ctx.beat();
            eprintln!("{}", "noise ".repeat(100));
        },
        "exit" => {
            eprintln!("takt-ipc-probe: giving up\x1b[31m");
            ExitCode::from(7)
        }
        "alloc" => {
            let mut big: Vec<u8> = Vec::new();
            let got = big.try_reserve_exact(1 << 30).is_ok();
            let _ = ctx.send(REPLY, 0, &[u8::from(got)]);
            serve(&mut ctx, ring)
        }
        "spawn" => {
            let me = std::env::current_exe();
            let started = me
                .and_then(|me| std::process::Command::new(me).arg("nothing").status())
                .is_ok();
            let _ = ctx.send(REPLY, 0, &[u8::from(started)]);
            serve(&mut ctx, ring)
        }
        "file" => {
            let mut head = [0u8; 16];
            let n = ctx
                .take_file()
                .and_then(|mut file| file.read(&mut head).ok())
                .unwrap_or(0);
            let _ = ctx.send(REPLY, 0, &head[..n]);
            serve(&mut ctx, ring)
        }
        "nest" => {
            let Ok(me) = std::env::current_exe() else {
                return ExitCode::from(4);
            };
            let inner = HelperCommand::new(me, APP)
                .arg("deaf")
                .limits(Limits {
                    handshake: Duration::from_secs(60),
                    ..Limits::default()
                })
                .spawn_reading();
            let Ok((inner, _reader)) = inner else {
                return ExitCode::from(5);
            };
            let _ = ctx.send(REPLY, 0, &inner.pid().to_le_bytes());
            // Holds its own helper and waits to be ended from outside.
            loop {
                ctx.beat();
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        _ => serve(&mut ctx, ring),
    }
}

/// The well-behaved loop: echo requests, leave when told to.
fn serve(ctx: &mut takt_ipc::child::HelperContext, mut ring: RingEnd) -> ExitCode {
    let mut record = Vec::new();
    loop {
        if let RingEnd::Reader(reader) = &mut ring {
            // Records of the parent come back as frames.
            while let Ok(true) = reader.try_read(&mut record) {
                if ctx.send(REPLY, 0, &record).is_err() {
                    return ExitCode::from(3);
                }
            }
        }
        let Some(frame) = ctx.recv(Duration::from_millis(5)) else {
            continue;
        };
        match frame.kind {
            kind::BYE => {
                let _ = ctx.send(kind::BYE, frame.seq, &[]);
                return ExitCode::SUCCESS;
            }
            REQUEST => {
                if let RingEnd::Writer(writer) = &mut ring {
                    let mut attempt = 0;
                    while let Ok(false) = writer.try_write(&[&frame.payload]) {
                        ctx.beat();
                        pause(attempt);
                        attempt += 1;
                    }
                }
                if ctx.send(REPLY, frame.seq, &frame.payload).is_err() {
                    return ExitCode::from(3);
                }
            }
            _ => {
                let _ = ctx.fault(frame.seq, 1, "unknown request");
            }
        }
    }
}
