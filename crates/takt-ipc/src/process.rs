// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The parent's side: starting a helper, talking to it, and ending it on every path.
//!
//! What holds for every helper started here (rules RS-04 and ENG-03 of the native rule set, applied
//! to a helper that lives as long as its work):
//!
//! * **A watchdog.** The helper raises a counter in the shared memory while it works and while it
//!   waits ([`crate::ring::Heartbeat`]). A thread of the parent ends a helper whose counter stands
//!   still for [`Limits::heartbeat`]: a parser in an endless loop does not outlive that time.
//!   Waiting for an answer has its own deadline ([`Helper::recv`]), and missing it ends the helper
//!   too. Until its hello the helper is under [`Limits::handshake`] instead: how long a program
//!   takes to start is the system's doing (a virus scanner, a cold disk), not a sign of a hang.
//! * **Byte limits.** A frame above [`Limits::max_frame`], a record above [`Limits::max_record`],
//!   more than [`Limits::max_stderr`] bytes on stderr or more than [`Limits::max_queued_frames`]
//!   frames nobody fetched end the helper. Under Windows and Linux [`Limits::memory`] bounds its
//!   memory.
//! * **An end.** Dropping the [`Helper`] ends and reaps the process. Should the parent die without
//!   that, the helper ends itself as soon as its stdin closes (a thread of [`crate::child`] does
//!   nothing but watch for it), and under Windows a job object ends it even when it hangs.
//!
//! The pipes are the child's stdin (commands) and stdout (answers); both are drained by threads, so
//! neither side blocks the other. The handle of the shared memory and of any file given with
//! [`HelperCommand::file`] is inherited; nothing has a name another process could open.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::error::{IpcError, ProtocolError, tame};
use crate::ffi::{Region, SharedMemory};
use crate::frame::{Frame, FrameDecoder, encode};
use crate::message::{AppId, Hello, kind};
use crate::ring::{self, Heartbeat, RingReader, RingWriter};

/// Bytes of stderr kept for the message of an [`IpcError::Exited`].
const STDERR_TAIL: usize = 2048;

/// The bounds a helper lives in.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Largest payload of a frame on the control pipe, either direction.
    pub max_frame: usize,
    /// Largest payload of a record in the ring.
    pub max_record: usize,
    /// Bytes the helper may write to stderr in its whole life.
    pub max_stderr: usize,
    /// Frames of the helper that may wait for [`Helper::recv`].
    pub max_queued_frames: usize,
    /// Bytes of memory the helper may have: committed memory under Windows (job object), address
    /// space under Linux (`RLIMIT_AS`). Not enforced elsewhere.
    pub memory: Option<u64>,
    /// Whether the helper may start processes of its own. Enforced under Windows (job object).
    pub child_processes: bool,
    /// How long the heartbeat may stand still.
    pub heartbeat: Duration,
    /// How long the helper has for its first frame.
    pub handshake: Duration,
    /// How long the helper has to exit after it was told to or closed its pipes.
    pub exit_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame: 1 << 20,
            max_record: 4 << 20,
            max_stderr: 1 << 20,
            max_queued_frames: 1024,
            memory: None,
            child_processes: false,
            heartbeat: Duration::from_secs(5),
            handshake: Duration::from_secs(5),
            exit_grace: Duration::from_secs(1),
        }
    }
}

/// Which side writes the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The helper writes, the parent reads (a demuxer).
    HelperWrites,
    /// The parent writes, the helper reads (an encoder).
    HelperReads,
}

/// A helper to be started.
#[derive(Debug)]
pub struct HelperCommand {
    program: PathBuf,
    args: Vec<OsString>,
    app: AppId,
    limits: Limits,
    ring_capacity: usize,
    files: Vec<File>,
}

impl HelperCommand {
    /// The program at `program`, which must speak the application protocol `app`.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>, app: AppId) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            app,
            limits: Limits::default(),
            ring_capacity: 1 << 20,
            files: Vec::new(),
        }
    }

    /// One more argument for the program.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// The bounds of the helper.
    #[must_use]
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// The size of the ring's data area in bytes, a power of two.
    #[must_use]
    pub const fn ring_capacity(mut self, bytes: usize) -> Self {
        self.ring_capacity = bytes;
        self
    }

    /// An open file the helper inherits; it fetches the files in this order with
    /// [`crate::child::HelperContext::take_file`]. This is how a helper gets its input without
    /// opening anything by name.
    #[must_use]
    pub fn file(mut self, file: File) -> Self {
        self.files.push(file);
        self
    }

    /// Starts the helper as the writer of the ring and waits for its hello.
    ///
    /// # Errors
    /// An [`IpcError`]; whatever was started is ended again.
    pub fn spawn_reading(self) -> Result<(Helper, RingReader), IpcError> {
        let max_record = self.limits.max_record;
        let (helper, region) = self.spawn(Role::HelperWrites)?;
        let reader = RingReader::attach(region, max_record)?;
        Ok((helper, reader))
    }

    /// Starts the helper as the reader of the ring and waits for its hello.
    ///
    /// # Errors
    /// An [`IpcError`]; whatever was started is ended again.
    pub fn spawn_writing(self) -> Result<(Helper, RingWriter), IpcError> {
        let max_record = self.limits.max_record;
        let (helper, region) = self.spawn(Role::HelperReads)?;
        let writer = RingWriter::attach(region, max_record)?;
        Ok((helper, writer))
    }

    fn spawn(self, role: Role) -> Result<(Helper, Arc<Region>), IpcError> {
        let Self {
            program,
            args,
            app,
            limits,
            ring_capacity,
            files,
        } = self;
        let len = crate::ffi::region::HEAD_LEN
            .checked_add(ring_capacity)
            .ok_or_else(|| IpcError::Spawn("ring too large".to_owned()))?;
        let memory = SharedMemory::create(len).map_err(|e| IpcError::spawn("shared memory", &e))?;
        let ring_raw = memory.raw();
        let region = Region::shared(memory)
            .ok_or_else(|| IpcError::Spawn("shared memory: unusable mapping".to_owned()))?;
        ring::format(&region)?;
        let region = Arc::new(region);

        let mut command = Command::new(&program);
        command
            .args(&args)
            .arg(format!(
                "{}{ring_raw},{len},{},{},{}",
                crate::child::ARG_RING,
                match role {
                    Role::HelperWrites => 'w',
                    Role::HelperReads => 'r',
                },
                limits.max_record,
                limits.max_frame,
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (spawned, job) = {
            // One helper is started at a time: between `prepare` and `after_spawn` the handles of
            // this helper are inheritable (Windows), and a second helper started in that window
            // would get them too.
            let _one_at_a_time = lock(&SPAWNING);
            let inherited = Inherited::prepare(&mut command, &region, files, &limits)?;
            let spawned = command.spawn();
            let job = inherited.after_spawn(&region, spawned.as_ref().ok(), &limits);
            (spawned, job)
        };
        let mut child =
            spawned.map_err(|e| IpcError::Spawn(format!("{}: {e}", program.display())))?;
        let job = match job {
            Ok(job) => job,
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(IpcError::spawn("limits of the process", &err));
            }
        };

        let pid = child.id();
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let guard = Arc::new(Guard {
            child: Mutex::new(child),
            reason: Mutex::new(None),
            stop: Mutex::new(false),
            wake: Condvar::new(),
            stderr: Mutex::new(StderrState::default()),
            stderr_wake: Condvar::new(),
            exit_grace: limits.exit_grace,
        });
        let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
            guard.kill();
            return Err(IpcError::Spawn("the pipes of the helper".to_owned()));
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(limits.max_queued_frames.max(1));
        let threads = (|| -> std::io::Result<()> {
            let g = Arc::clone(&guard);
            let max_frame = limits.max_frame;
            std::thread::Builder::new()
                .name("takt-ipc-stdout".to_owned())
                .spawn(move || pump_frames(stdout, max_frame, &tx, &g))?;
            let g = Arc::clone(&guard);
            let max_stderr = limits.max_stderr;
            std::thread::Builder::new()
                .name("takt-ipc-stderr".to_owned())
                .spawn(move || pump_stderr(stderr, max_stderr, &g))?;
            Ok(())
        })();
        if let Err(err) = threads {
            guard.kill();
            return Err(IpcError::spawn("threads of the helper", &err));
        }
        let mut helper = Helper {
            guard,
            stdin: Some(stdin),
            rx,
            watchdog: None,
            limits,
            pid,
            scratch: Vec::new(),
            _job: job,
        };
        // From here on dropping `helper` ends the process.
        helper.send(kind::HELLO, 0, &Hello::ours(app).encode())?;
        let handshake = helper.limits.handshake;
        let first = helper.recv_for(handshake, "handshake")?;
        let hello = if first.kind == kind::HELLO {
            Hello::parse(&first.payload).and_then(|hello| hello.accept(app))
        } else {
            Err(ProtocolError::WrongPeer)
        };
        if let Err(err) = hello {
            helper.guard.end(IpcError::Protocol(err.clone()));
            return Err(IpcError::Protocol(err));
        }
        // The helper is up: from here on it owes a heartbeat.
        let guard = Arc::clone(&helper.guard);
        let beat = Heartbeat::new(Arc::clone(&region));
        let timeout = helper.limits.heartbeat;
        let watchdog = std::thread::Builder::new()
            .name("takt-ipc-watchdog".to_owned())
            .spawn(move || watchdog(&guard, &beat, timeout))
            .map_err(|e| IpcError::spawn("watchdog of the helper", &e))?;
        helper.watchdog = Some(watchdog);
        Ok((helper, region))
    }
}

/// What the child inherits besides its pipes, and the undoing of it in the parent.
struct Inherited {
    /// Copies of the files, inheritable (Windows) or just kept open until the child exists (Unix).
    #[allow(dead_code)]
    files: Vec<File>,
    /// The error mode of this process before the start, to be put back.
    #[cfg(windows)]
    error_mode: u32,
}

#[cfg(windows)]
type Job = Option<std::os::windows::io::OwnedHandle>;
/// Nothing to hold where no job object exists.
#[cfg(not(windows))]
struct Job;

impl Inherited {
    #[cfg(windows)]
    fn prepare(
        command: &mut Command,
        region: &Region,
        files: Vec<File>,
        _limits: &Limits,
    ) -> Result<Self, IpcError> {
        use crate::ffi::windows::{CREATE_NO_WINDOW, set_inheritable};
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;

        command.creation_flags(CREATE_NO_WINDOW);
        // Copies become inheritable, never the caller's own handles: those stay out of every other
        // process this program starts.
        let mut copies = Vec::with_capacity(files.len());
        for file in files {
            let copy = file
                .try_clone()
                .map_err(|e| IpcError::spawn("copy of the file handle", &e))?;
            set_inheritable(&copy, true).map_err(|e| IpcError::spawn("file handle", &e))?;
            command.arg(format!(
                "{}{}",
                crate::child::ARG_FILE,
                copy.as_raw_handle().addr()
            ));
            copies.push(copy);
        }
        if let Some(memory) = region.memory() {
            set_inheritable(memory.handle(), true)
                .map_err(|e| IpcError::spawn("handle of the shared memory", &e))?;
        }
        // A helper that cannot load a library it links must fail, not sit behind a message window
        // of the system that nobody sees. A child inherits the error mode of its parent, so the
        // mode is set for the moment of the start (under the lock of `spawn`) and put back after.
        let error_mode = crate::ffi::windows::quiet_error_mode();
        Ok(Self {
            files: copies,
            error_mode,
        })
    }

    /// Takes the inheritance back and puts the child under its job.
    #[cfg(windows)]
    fn after_spawn(
        self,
        region: &Region,
        child: Option<&Child>,
        limits: &Limits,
    ) -> std::io::Result<Job> {
        use crate::ffi::windows::{assign, job, set_inheritable};

        crate::ffi::windows::restore_error_mode(self.error_mode);
        if let Some(memory) = region.memory() {
            // A failure leaves the handle inheritable: the next child of this program would get a
            // handle to a ring it does not know the number of.
            let _ = set_inheritable(memory.handle(), false);
        }
        drop(self.files);
        let Some(child) = child else {
            return Ok(None);
        };
        let job = job(limits.memory, limits.child_processes)?;
        assign(&job, child)?;
        Ok(Some(job))
    }

    #[cfg(unix)]
    fn prepare(
        command: &mut Command,
        region: &Region,
        files: Vec<File>,
        limits: &Limits,
    ) -> Result<Self, IpcError> {
        use std::os::fd::AsRawFd;

        let mut keep = Vec::with_capacity(files.len() + 1);
        if let Some(memory) = region.memory() {
            keep.push(memory.handle().as_raw_fd());
        }
        for file in &files {
            command.arg(format!("{}{}", crate::child::ARG_FILE, file.as_raw_fd()));
            keep.push(file.as_raw_fd());
        }
        crate::ffi::unix::prepare(command, keep, limits.memory);
        Ok(Self { files })
    }

    #[cfg(unix)]
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    fn after_spawn(
        self,
        _region: &Region,
        _child: Option<&Child>,
        _limits: &Limits,
    ) -> std::io::Result<Job> {
        Ok(Job)
    }
}

/// Held while a helper is being started (see [`HelperCommand`], `spawn`).
static SPAWNING: Mutex<()> = Mutex::new(());

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct StderrState {
    tail: Vec<u8>,
    done: bool,
}

/// What the threads of a helper share: the process and why it was ended.
struct Guard {
    child: Mutex<Child>,
    /// The first reason the helper is gone; set once.
    reason: Mutex<Option<IpcError>>,
    stop: Mutex<bool>,
    wake: Condvar,
    stderr: Mutex<StderrState>,
    stderr_wake: Condvar,
    exit_grace: Duration,
}

impl Guard {
    /// Ends the helper for `reason`, unless it is gone for an earlier one.
    fn end(&self, reason: IpcError) {
        lock(&self.reason).get_or_insert(reason);
        self.kill();
    }

    /// Ends and reaps the process. Harmless when it is gone already.
    fn kill(&self) {
        let mut child = lock(&self.child);
        let _ = child.kill();
        let _ = child.wait();
    }

    fn reason(&self) -> Option<IpcError> {
        lock(&self.reason).clone()
    }

    /// Waits for the process to exit by itself, at most `grace`; ends it after that. The exit code,
    /// if it has one.
    fn reap(&self, grace: Duration) -> Option<i32> {
        let deadline = Instant::now() + grace;
        loop {
            {
                let mut child = lock(&self.child);
                match child.try_wait() {
                    Ok(Some(status)) => return status.code(),
                    Ok(None) if Instant::now() < deadline => {}
                    _ => {
                        let _ = child.kill();
                        return child.wait().ok().and_then(|status| status.code());
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// The helper closed its stdout: it is exiting by itself, unless it was ended before.
    fn exited(&self) {
        if self.reason().is_some() {
            return;
        }
        let code = self.reap(self.exit_grace);
        // Its last words may still be on their way through the other pipe.
        let state = lock(&self.stderr);
        let (state, _) = self
            .stderr_wake
            .wait_timeout_while(state, Duration::from_millis(200), |state| !state.done)
            .unwrap_or_else(PoisonError::into_inner);
        let stderr = tame(String::from_utf8_lossy(&state.tail).trim(), STDERR_TAIL);
        drop(state);
        lock(&self.reason).get_or_insert(IpcError::Exited { code, stderr });
    }
}

fn pump_frames(mut stdout: impl Read, max_frame: usize, tx: &SyncSender<Frame>, guard: &Guard) {
    let mut decoder = FrameDecoder::new(max_frame);
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        decoder.push(&chunk[..n]);
        loop {
            match decoder.next() {
                Ok(Some(frame)) => match tx.try_send(frame) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        guard.end(IpcError::Limit("frames nobody fetched"));
                        return;
                    }
                    // The owner is gone and ends the helper itself.
                    Err(TrySendError::Disconnected(_)) => return,
                },
                Ok(None) => break,
                Err(err) => {
                    guard.end(IpcError::Protocol(err));
                    return;
                }
            }
        }
    }
    guard.exited();
}

fn pump_stderr(mut stderr: impl Read, max_bytes: usize, guard: &Guard) {
    let mut chunk = vec![0u8; 8 * 1024];
    let mut total = 0usize;
    loop {
        let n = match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        total = total.saturating_add(n);
        {
            let mut state = lock(&guard.stderr);
            state.tail.extend_from_slice(&chunk[..n]);
            let excess = state.tail.len().saturating_sub(STDERR_TAIL);
            state.tail.drain(..excess);
        }
        if total > max_bytes {
            guard.end(IpcError::Limit("bytes on stderr"));
            break;
        }
    }
    lock(&guard.stderr).done = true;
    guard.stderr_wake.notify_all();
}

fn watchdog(guard: &Guard, beat: &Heartbeat, timeout: Duration) {
    let interval = (timeout / 4).clamp(Duration::from_millis(1), Duration::from_millis(250));
    let mut last = beat.count();
    let mut since = Instant::now();
    let mut stop = lock(&guard.stop);
    loop {
        if *stop {
            return;
        }
        let (next, _) = guard
            .wake
            .wait_timeout(stop, interval)
            .unwrap_or_else(PoisonError::into_inner);
        stop = next;
        if *stop {
            return;
        }
        let now = beat.count();
        if now != last {
            last = now;
            since = Instant::now();
        } else if since.elapsed() >= timeout {
            drop(stop);
            guard.end(IpcError::Timeout("heartbeat"));
            return;
        }
    }
}

/// A running helper process. Dropping it ends the process.
pub struct Helper {
    guard: Arc<Guard>,
    stdin: Option<ChildStdin>,
    rx: Receiver<Frame>,
    watchdog: Option<JoinHandle<()>>,
    limits: Limits,
    pid: u32,
    scratch: Vec<u8>,
    /// Under Windows the job that ends the helper with this process; closed after the helper is
    /// gone.
    _job: Job,
}

impl std::fmt::Debug for Helper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Helper")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl Helper {
    /// The process id of the helper.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// The bounds the helper was started with.
    #[must_use]
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Sends one frame.
    ///
    /// # Errors
    /// Why the helper is gone, or [`ProtocolError::TooLarge`] for a payload above the frame limit.
    pub fn send(&mut self, kind: u16, seq: u32, payload: &[u8]) -> Result<(), IpcError> {
        self.scratch.clear();
        encode(kind, seq, payload, self.limits.max_frame, &mut self.scratch)?;
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(self.failure());
        };
        match stdin.write_all(&self.scratch).and_then(|()| stdin.flush()) {
            Ok(()) => Ok(()),
            Err(err) => Err(self.guard.reason().unwrap_or_else(|| {
                // A closed pipe: the reader thread is about to find out why.
                self.guard.exited();
                self.guard
                    .reason()
                    .unwrap_or_else(|| IpcError::io("write to the helper", &err))
            })),
        }
    }

    /// The next frame of the helper, waiting at most `timeout`. A helper that does not answer in
    /// time is ended.
    ///
    /// # Errors
    /// Why the helper is gone.
    pub fn recv(&mut self, timeout: Duration) -> Result<Frame, IpcError> {
        self.recv_for(timeout, "answer")
    }

    fn recv_for(&mut self, timeout: Duration, what: &'static str) -> Result<Frame, IpcError> {
        match self.rx.recv_timeout(timeout) {
            Ok(frame) => Ok(frame),
            Err(RecvTimeoutError::Timeout) => {
                self.guard.end(IpcError::Timeout(what));
                Err(self.failure())
            }
            Err(RecvTimeoutError::Disconnected) => Err(self.failure()),
        }
    }

    /// A frame of the helper if one is waiting.
    ///
    /// # Errors
    /// Why the helper is gone.
    pub fn try_recv(&mut self) -> Result<Option<Frame>, IpcError> {
        match self.rx.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(TryRecvError::Empty) => match self.guard.reason() {
                Some(reason) => Err(reason),
                None => Ok(None),
            },
            Err(TryRecvError::Disconnected) => Err(self.failure()),
        }
    }

    /// Whether the helper still runs.
    ///
    /// # Errors
    /// Why the helper is gone.
    pub fn check(&self) -> Result<(), IpcError> {
        self.guard.reason().map_or(Ok(()), Err)
    }

    /// Ends the helper because its owner found it lying (a record that is not the protocol): the
    /// reason is kept and returned by every later call.
    pub fn condemn(&self, reason: IpcError) {
        self.guard.end(reason);
    }

    /// Says goodbye, waits [`Limits::exit_grace`] for the helper to exit and ends it after that.
    /// The exit code, if it exited by itself.
    #[must_use]
    pub fn shutdown(mut self) -> Option<i32> {
        let _ = self.send(kind::BYE, 0, &[]);
        // The pipe stays open while the helper leaves: closing it now would race the goodbye with
        // the end of the pipe, which the helper takes for the death of its parent.
        self.guard.reap(self.limits.exit_grace)
    }

    fn failure(&self) -> IpcError {
        if let Some(reason) = self.guard.reason() {
            return reason;
        }
        self.guard.exited();
        self.guard.reason().unwrap_or(IpcError::Exited {
            code: None,
            stderr: String::new(),
        })
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        *lock(&self.guard.stop) = true;
        self.guard.wake.notify_all();
        self.stdin = None;
        self.guard.kill();
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
        // The pipe readers end by themselves once the pipes close; they are not waited for (a
        // process that inherited the pipes could hold them open).
    }
}
