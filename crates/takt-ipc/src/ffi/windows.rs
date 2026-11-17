// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Windows: handles a child inherits, and the job that ends the child with its parent.
#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

/// `CREATE_NO_WINDOW`: a helper of a window program opens no console window.
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Lets child processes started from now on inherit `handle`, or stops that.
pub(crate) fn set_inheritable(handle: &impl AsRawHandle, inherit: bool) -> io::Result<()> {
    let flags = if inherit { HANDLE_FLAG_INHERIT } else { 0 };
    // SAFETY: `handle` is a live handle for the length of the borrow; the call changes one flag.
    let ok = unsafe { SetHandleInformation(handle.as_raw_handle(), HANDLE_FLAG_INHERIT, flags) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A job for one helper: when the last handle to the job closes (this process ends, however), the
/// system ends the helper. `memory` limits the memory the helper may commit; without
/// `child_processes` it cannot start a process of its own.
pub(crate) fn job(memory: Option<u64>, child_processes: bool) -> io::Result<OwnedHandle> {
    // SAFETY: no security attributes, no name: neither pointer is read.
    let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a handle this call just created and nobody else owns.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    // SAFETY: the structure is plain data for which all zeroes means "no limit".
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if !child_processes {
        info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        info.BasicLimitInformation.ActiveProcessLimit = 1;
    }
    if let Some(bytes) = memory {
        info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = usize::try_from(bytes).unwrap_or(usize::MAX);
    }
    let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
        .map_err(|_| io::Error::other("size of the job limits"))?;
    // SAFETY: `job` is a live job handle; `info` is the structure the class names, `size` its size,
    // and it lives across the call.
    let ok = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            size,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

/// Puts a process into a job.
pub(crate) fn assign(job: &OwnedHandle, process: &impl AsRawHandle) -> io::Result<()> {
    // SAFETY: both are live handles for the length of the borrows.
    let ok = unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The file behind a handle this process inherited from its parent. Takes the handle over: `raw`
/// must be passed in once only.
pub(crate) fn adopt_file(raw: u64) -> io::Result<File> {
    let raw = usize::try_from(raw)
        .ok()
        .filter(|raw| *raw != 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "handle out of range"))?;
    // SAFETY: the parent made this handle inheritable for this process and names it once on the
    // command line; the caller passes it in once, so the file is its only owner here. A number
    // that is no file handle makes the first read fail, nothing else.
    Ok(unsafe { File::from_raw_handle(ptr::without_provenance_mut(raw)) })
}
