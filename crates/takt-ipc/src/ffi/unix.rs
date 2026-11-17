// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unix: descriptors a child keeps across `exec`, and the limit on its address space.
#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Makes the child of `command` keep the descriptors `keep` (std opens everything close-on-exec)
/// and, under Linux, limits its address space to `memory` bytes.
///
/// The descriptors must stay open in this process until the child is started.
pub(crate) fn prepare(command: &mut Command, keep: Vec<RawFd>, memory: Option<u64>) {
    let hook = move || -> io::Result<()> {
        for fd in &keep {
            // SAFETY: an async-signal-safe call on a descriptor number; a number that is not open
            // fails with EBADF, which ends the start of the child with that error.
            if unsafe { libc::fcntl(*fd, libc::F_SETFD, 0) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        limit_address_space(memory)
    };
    // SAFETY: the hook runs in the forked child before `exec`. It only makes async-signal-safe
    // calls (`fcntl`, `setrlimit`), reads a vector that was built before the fork, and does not
    // allocate: `last_os_error` stores the number of the error only.
    unsafe {
        command.pre_exec(hook);
    }
}

#[cfg(target_os = "linux")]
fn limit_address_space(memory: Option<u64>) -> io::Result<()> {
    let Some(bytes) = memory else {
        return Ok(());
    };
    let limit = libc::rlimit {
        rlim_cur: bytes as libc::rlim_t,
        rlim_max: bytes as libc::rlim_t,
    };
    // SAFETY: an async-signal-safe call with a pointer to a structure that lives across it.
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &raw const limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// No limit elsewhere: macOS does not enforce `RLIMIT_AS`.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
fn limit_address_space(_memory: Option<u64>) -> io::Result<()> {
    Ok(())
}

/// The file behind a descriptor this process inherited from its parent. Takes the descriptor over:
/// `raw` must be passed in once only.
pub(crate) fn adopt_file(raw: u64) -> io::Result<File> {
    let raw = RawFd::try_from(raw)
        .ok()
        .filter(|fd| *fd > 2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "descriptor out of range"))?;
    // SAFETY: the parent left this descriptor open for this process and names it once on the
    // command line; the caller passes it in once, so the file is its only owner here. A number
    // that is not open makes the first read fail, nothing else.
    Ok(unsafe { File::from_raw_fd(raw) })
}
