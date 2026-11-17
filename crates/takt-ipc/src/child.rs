// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The helper's side: what a helper program calls first.
//!
//! [`connect`] maps the ring the parent passed on, starts the thread that reads the control pipe
//! and shakes hands. From then on the helper owes its parent one thing besides its work: a
//! heartbeat. [`HelperContext::recv`] and [`HelperContext::send`] beat by themselves; a helper that
//! computes for long between them calls [`HelperContext::beat`]. A helper whose heartbeat stands
//! still for [`crate::process::Limits::heartbeat`] is ended by its parent.
//!
//! A helper never outlives its parent: when the parent is gone, however that happened, the
//! helper's stdin closes, and the thread that reads it ends the process on the spot, also when the
//! main thread hangs.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::error::{IpcError, ProtocolError};
use crate::ffi::{Region, SharedMemory};
use crate::frame::{Frame, FrameDecoder, encode};
use crate::message::{AppId, Fault, Hello, Progress, kind};
use crate::ring::{self, Heartbeat, RingReader, RingWriter};

/// The argument that carries the ring: `--takt-ipc=<handle>,<bytes>,<w|r>,<max record>,<max frame>`.
pub(crate) const ARG_RING: &str = "--takt-ipc=";
/// The argument that carries one inherited file: `--takt-ipc-file=<handle>`.
pub(crate) const ARG_FILE: &str = "--takt-ipc-file=";

/// Exit code of a helper whose parent is gone.
pub const EXIT_PARENT_GONE: i32 = 70;
/// Exit code of a helper whose parent sent something that is not the protocol.
pub const EXIT_PROTOCOL: i32 = 71;

/// How long a helper waits for the hello of its parent.
const HANDSHAKE: Duration = Duration::from_secs(10);

/// The helper's end of the ring.
#[derive(Debug)]
pub enum RingEnd {
    /// The helper writes records for its parent.
    Writer(RingWriter),
    /// The helper reads records of its parent.
    Reader(RingReader),
}

/// The helper's connection to its parent.
#[derive(Debug)]
pub struct HelperContext {
    rx: Receiver<Frame>,
    beat: Heartbeat,
    max_frame: usize,
    files: VecDeque<File>,
    scratch: Vec<u8>,
}

struct Setup {
    raw: u64,
    len: usize,
    writes: bool,
    max_record: usize,
    max_frame: usize,
}

fn parse_setup(value: &str) -> Option<Setup> {
    let mut fields = value.split(',');
    let raw = fields.next()?.parse().ok()?;
    let len = fields.next()?.parse().ok()?;
    let writes = match fields.next()? {
        "w" => true,
        "r" => false,
        _ => return None,
    };
    let max_record = fields.next()?.parse().ok()?;
    let max_frame = fields.next()?.parse().ok()?;
    fields.next().is_none().then_some(Setup {
        raw,
        len,
        writes,
        max_record,
        max_frame,
    })
}

/// Connects a helper process to the parent that started it, as the application `app`.
///
/// Call it once, first thing in `main`, before anything is written to stdout: stdout is the
/// control pipe.
///
/// # Errors
/// [`IpcError::Spawn`] when the process was not started by [`crate::process::HelperCommand`] (no
/// ring on the command line) or the ring is unusable, [`IpcError::Protocol`] when the parent speaks
/// another version.
pub fn connect(app: AppId) -> Result<(HelperContext, RingEnd), IpcError> {
    let mut setup = None;
    let mut file_handles = Vec::new();
    for arg in std::env::args() {
        if let Some(value) = arg.strip_prefix(ARG_RING) {
            setup = parse_setup(value);
        } else if let Some(value) = arg.strip_prefix(ARG_FILE) {
            file_handles.push(value.parse::<u64>().ok());
        }
    }
    let setup = setup.ok_or_else(|| {
        IpcError::Spawn("not started as a helper: no usable --takt-ipc argument".to_owned())
    })?;
    let memory = SharedMemory::from_inherited(setup.raw, setup.len)
        .map_err(|e| IpcError::spawn("shared memory", &e))?;
    let region = Region::shared(memory)
        .ok_or_else(|| IpcError::Spawn("shared memory: unusable mapping".to_owned()))?;
    ring::check_format(&region)?;
    let region = Arc::new(region);
    let beat = Heartbeat::new(Arc::clone(&region));
    beat.beat();

    let mut files = VecDeque::with_capacity(file_handles.len());
    for raw in file_handles {
        let raw = raw.ok_or_else(|| IpcError::Spawn("file handle is no number".to_owned()))?;
        #[cfg(windows)]
        let file = crate::ffi::windows::adopt_file(raw);
        #[cfg(unix)]
        let file = crate::ffi::unix::adopt_file(raw);
        files.push_back(file.map_err(|e| IpcError::spawn("inherited file", &e))?);
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let max_frame = setup.max_frame;
    std::thread::Builder::new()
        .name("takt-ipc-control".to_owned())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut decoder = FrameDecoder::new(max_frame);
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let n = match stdin.read(&mut chunk) {
                    Ok(0) => std::process::exit(EXIT_PARENT_GONE),
                    Ok(n) => n,
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => std::process::exit(EXIT_PARENT_GONE),
                };
                decoder.push(&chunk[..n]);
                loop {
                    match decoder.next() {
                        // Nobody listens any more: keep watching for the end of the parent.
                        Ok(Some(frame)) => drop(tx.send(frame)),
                        Ok(None) => break,
                        Err(_) => std::process::exit(EXIT_PROTOCOL),
                    }
                }
            }
        })
        .map_err(|e| IpcError::spawn("control thread", &e))?;

    let mut context = HelperContext {
        rx,
        beat,
        max_frame,
        files,
        scratch: Vec::new(),
    };
    context.send(kind::HELLO, 0, &Hello::ours(app).encode())?;
    let first = context
        .recv(HANDSHAKE)
        .ok_or(IpcError::Timeout("hello of the parent"))?;
    if first.kind != kind::HELLO {
        return Err(IpcError::Protocol(ProtocolError::WrongPeer));
    }
    Hello::parse(&first.payload)?.accept(app)?;

    let end = if setup.writes {
        RingEnd::Writer(RingWriter::attach(region, setup.max_record)?)
    } else {
        RingEnd::Reader(RingReader::attach(region, setup.max_record)?)
    };
    Ok((context, end))
}

impl HelperContext {
    /// Says "alive" to the watchdog of the parent. Wait-free.
    pub fn beat(&self) {
        self.beat.beat();
    }

    /// The heartbeat, for a thread or a callback that beats on behalf of long work.
    #[must_use]
    pub fn heartbeat(&self) -> Heartbeat {
        self.beat.clone()
    }

    /// The next frame of the parent, waiting at most `timeout`; `None` when none came. Beats.
    ///
    /// Keep `timeout` well below the heartbeat limit of the parent, or beat from elsewhere.
    pub fn recv(&mut self, timeout: Duration) -> Option<Frame> {
        self.beat.beat();
        // An error is "none came": the control thread only ends with the process.
        let frame = self.rx.recv_timeout(timeout).ok();
        self.beat.beat();
        frame
    }

    /// Sends one frame to the parent. Beats.
    ///
    /// # Errors
    /// [`IpcError::Io`] when the parent is gone, [`ProtocolError::TooLarge`] for a payload above
    /// the frame limit.
    pub fn send(&mut self, kind: u16, seq: u32, payload: &[u8]) -> Result<(), IpcError> {
        self.scratch.clear();
        encode(kind, seq, payload, self.max_frame, &mut self.scratch)?;
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&self.scratch)
            .and_then(|()| stdout.flush())
            .map_err(|e| IpcError::io("write to the parent", &e))?;
        self.beat.beat();
        Ok(())
    }

    /// Reports an error of the request `seq`.
    ///
    /// # Errors
    /// As [`send`](Self::send).
    pub fn fault(&mut self, seq: u32, code: u32, text: &str) -> Result<(), IpcError> {
        let fault = Fault {
            code,
            text: text.to_owned(),
        };
        self.send(kind::FAULT, seq, &fault.encode())
    }

    /// Reports how far the request `seq` is.
    ///
    /// # Errors
    /// As [`send`](Self::send).
    pub fn progress(&mut self, seq: u32, done: u64, total: u64) -> Result<(), IpcError> {
        self.send(kind::PROGRESS, seq, &Progress { done, total }.encode())
    }

    /// The next file the parent passed on ([`crate::process::HelperCommand::file`]), in its order.
    pub fn take_file(&mut self) -> Option<File> {
        self.files.pop_front()
    }

    /// The largest payload of a frame.
    #[must_use]
    pub const fn max_frame(&self) -> usize {
        self.max_frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_argument_is_read_or_refused_as_a_whole() {
        let setup = parse_setup("412,1049088,w,4194304,1048576").unwrap();
        assert_eq!(
            (
                setup.raw,
                setup.len,
                setup.writes,
                setup.max_record,
                setup.max_frame
            ),
            (412, 1_049_088, true, 4_194_304, 1_048_576)
        );
        assert!(!parse_setup("1,2,r,3,4").unwrap().writes);
        for bad in [
            "",
            "1,2,w,3",
            "1,2,x,3,4",
            "1,2,w,3,4,5",
            "-1,2,w,3,4",
            "a,2,w,3,4",
        ] {
            assert!(parse_setup(bad).is_none(), "{bad}");
        }
    }
}
