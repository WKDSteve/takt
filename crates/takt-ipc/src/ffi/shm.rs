// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Anonymous shared memory: a mapping without a name in any namespace, reachable only through the
//! handle a child process inherits (`CreateFileMapping` without a name on Windows, `memfd_create` on
//! Linux, a POSIX object unlinked right after its creation elsewhere).
#![allow(unsafe_code)]

use std::io;
use std::ptr::NonNull;

/// A mapping of anonymous shared memory. Unmapped and closed when dropped.
pub(crate) struct SharedMemory {
    ptr: NonNull<u8>,
    len: usize,
    handle: imp::Handle,
}

impl SharedMemory {
    /// New zeroed memory of `len` bytes.
    pub(crate) fn create(len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty mapping"));
        }
        let handle = imp::create(len)?;
        let ptr = imp::map(&handle, len)?;
        Ok(Self { ptr, len, handle })
    }

    /// Maps `len` bytes of the memory behind a handle this process inherited from its parent. `raw`
    /// is the number [`raw`](Self::raw) gave in the parent.
    ///
    /// Takes the handle over: it must be passed in once only.
    pub(crate) fn from_inherited(raw: u64, len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty mapping"));
        }
        let handle = imp::adopt(raw)?;
        let ptr = imp::map(&handle, len)?;
        Ok(Self { ptr, len, handle })
    }

    pub(crate) const fn ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// The number of the handle, as a child that inherited it would name it.
    pub(crate) fn raw(&self) -> u64 {
        imp::raw(&self.handle)
    }

    /// The handle itself, for the code that lets a child inherit it.
    pub(crate) const fn handle(&self) -> &imp::Handle {
        &self.handle
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        imp::unmap(self.ptr, self.len);
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::ptr::{self, NonNull};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        PAGE_READWRITE, UnmapViewOfFile,
    };

    pub(crate) type Handle = OwnedHandle;

    pub(super) fn create(len: usize) -> io::Result<Handle> {
        let len = len as u64;
        #[allow(clippy::cast_possible_truncation)]
        let (high, low) = ((len >> 32) as u32, len as u32);
        // SAFETY: no file (the paging file backs the memory), no security attributes (the handle is
        // not inheritable until a spawn asks for it), no name: none of the pointers is read.
        let raw = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                ptr::null(),
                PAGE_READWRITE,
                high,
                low,
                ptr::null(),
            )
        };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a handle this call just created and nobody else owns.
        Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
    }

    pub(super) fn adopt(raw: u64) -> io::Result<Handle> {
        let raw = usize::try_from(raw)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "handle out of range"))?;
        if raw == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "null handle"));
        }
        // SAFETY: the parent created this handle, made it inheritable for this process and names it
        // once on the command line; the caller passes it in once, so this is its only owner here. A
        // wrong number is caught by the mapping call that follows (not a mapping, too short).
        Ok(unsafe { OwnedHandle::from_raw_handle(ptr::without_provenance_mut(raw)) })
    }

    pub(super) fn map(handle: &Handle, len: usize) -> io::Result<NonNull<u8>> {
        // SAFETY: `handle` is a live handle; the call fails (null) when it is no mapping or the
        // mapping is shorter than `len`.
        let view = unsafe { MapViewOfFile(handle.as_raw_handle(), FILE_MAP_ALL_ACCESS, 0, 0, len) };
        NonNull::new(view.Value.cast::<u8>()).ok_or_else(io::Error::last_os_error)
    }

    pub(super) fn unmap(ptr: NonNull<u8>, _len: usize) {
        // SAFETY: `ptr` is the start of a view `map` returned, unmapped once (from Drop).
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: ptr.as_ptr().cast(),
            });
        }
    }

    pub(super) fn raw(handle: &Handle) -> u64 {
        let raw: RawHandle = handle.as_raw_handle();
        raw.addr() as u64
    }
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::ptr::{self, NonNull};

    pub(crate) type Handle = OwnedFd;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn anonymous() -> io::Result<OwnedFd> {
        // SAFETY: the name is a NUL-terminated literal (it only shows in /proc); the flag closes the
        // descriptor on exec unless a spawn clears it.
        let fd = unsafe { libc::memfd_create(c"takt-ipc".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a descriptor this call just created and nobody else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// POSIX shared memory, created under a name nobody can guess in time and unlinked before the
    /// call returns: after that only the descriptor reaches it.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn anonymous() -> io::Result<OwnedFd> {
        use std::ffi::CString;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static COUNTER: AtomicU32 = AtomicU32::new(0);
        for _ in 0..16 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos());
            let count = COUNTER.fetch_add(1, Ordering::Relaxed);
            // macOS allows 31 bytes for the name.
            let name = format!("/takt-{:x}-{count:x}-{nanos:x}", std::process::id());
            let name = CString::new(name)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name with NUL"))?;
            // Only these three: macOS answers any other flag (O_CLOEXEC too) with EINVAL. POSIX
            // sets close-on-exec on the descriptor by itself; it is set again below all the same.
            let flags = libc::O_CREAT | libc::O_EXCL | libc::O_RDWR;
            // SAFETY: `name` is NUL-terminated and lives across the call; O_EXCL makes the call
            // fail instead of opening an object somebody else made.
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            let fd = unsafe { libc::shm_open(name.as_ptr(), flags, 0o600 as libc::c_uint) };
            // SAFETY: as above.
            #[cfg(not(any(target_os = "macos", target_os = "ios")))]
            let fd = unsafe { libc::shm_open(name.as_ptr(), flags, 0o600 as libc::mode_t) };
            if fd < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(err);
            }
            // SAFETY: `fd` is a descriptor this call just created and nobody else owns.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            // SAFETY: `name` is NUL-terminated; removing the name leaves the object to the
            // descriptor.
            unsafe { libc::shm_unlink(name.as_ptr()) };
            // SAFETY: a call on a live descriptor that sets one flag.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                return Err(io::Error::last_os_error());
            }
            return Ok(fd);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free name for the shared memory",
        ))
    }

    pub(super) fn create(len: usize) -> io::Result<Handle> {
        let fd = anonymous()?;
        let size = libc::off_t::try_from(len)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mapping too large"))?;
        // SAFETY: `fd` is a live descriptor of a fresh object of size 0.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }

    pub(super) fn adopt(raw: u64) -> io::Result<Handle> {
        let raw = libc::c_int::try_from(raw)
            .ok()
            .filter(|fd| *fd > 2)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "descriptor out of range")
            })?;
        // SAFETY: the parent left this descriptor open for this process and names it once on the
        // command line; the caller passes it in once, so this is its only owner here. A wrong
        // number is caught by `fstat` in `map`.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    pub(super) fn map(handle: &Handle, len: usize) -> io::Result<NonNull<u8>> {
        // A mapping longer than its object faults on the first touch behind the end, so the size is
        // checked first.
        // SAFETY: `stat` is plain data the call fills in; `handle` is a live descriptor.
        let size = unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(handle.as_raw_fd(), &raw mut stat) != 0 {
                return Err(io::Error::last_os_error());
            }
            stat.st_size
        };
        if u64::try_from(size).unwrap_or(0) < len as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared memory is shorter than announced",
            ));
        }
        // SAFETY: a new shared mapping of a live descriptor at an address the system picks; the
        // length is inside the object (checked above).
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                handle.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        NonNull::new(ptr.cast::<u8>()).ok_or_else(|| io::Error::other("mapping at address 0"))
    }

    pub(super) fn unmap(ptr: NonNull<u8>, len: usize) {
        // SAFETY: `ptr` and `len` are a mapping `map` returned, unmapped once (from Drop).
        unsafe {
            libc::munmap(ptr.as_ptr().cast(), len);
        }
    }

    pub(super) fn raw(handle: &Handle) -> u64 {
        u64::try_from(handle.as_raw_fd()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mapping_is_zeroed_and_writable() {
        let mem = SharedMemory::create(64 * 1024).unwrap();
        assert_eq!(mem.len(), 64 * 1024);
        assert!(mem.raw() != 0);
        // SAFETY: the mapping is 64 KiB long, nobody else has it.
        unsafe {
            assert_eq!(*mem.ptr().as_ptr().add(65_535), 0);
            *mem.ptr().as_ptr() = 7;
            assert_eq!(*mem.ptr().as_ptr(), 7);
        }
    }

    #[test]
    fn an_empty_mapping_is_refused() {
        assert!(SharedMemory::create(0).is_err());
    }
}
