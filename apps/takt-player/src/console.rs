// SPDX-License-Identifier: GPL-3.0-or-later
//! The console of the terminal the player was started from.
//!
//! A release build for Windows is a window program (`windows_subsystem` in `main.rs`): started
//! from the Start menu it gets no console window. Started from a terminal it would then write
//! `--keys`, the lines of `--headless` and its errors nowhere, so it attaches itself to the
//! console of its parent first. This is the one call of the Windows API the player makes itself;
//! it is declared here instead of pulling in a crate for it.
#![allow(unsafe_code)]

/// `ATTACH_PARENT_PROCESS`: the console of the process that started this one.
#[cfg(windows)]
const ATTACH_PARENT_PROCESS: u32 = u32::MAX;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn AttachConsole(process_id: u32) -> i32;
    fn GetConsoleWindow() -> *mut core::ffi::c_void;
}

/// `MB_OK | MB_ICONERROR`: one button, the icon of an error.
#[cfg(windows)]
const MB_ICONERROR: u32 = 0x10;

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(
        owner: *mut core::ffi::c_void,
        text: *const u16,
        caption: *const u16,
        kind: u32,
    ) -> i32;
}

/// Whether the process has a console to write to: its own (a debug build) or the one of the
/// terminal it was attached to. Started from the Start menu, a release build has none.
#[cfg(windows)]
pub fn has_console() -> bool {
    // SAFETY: GetConsoleWindow takes nothing and only returns a handle (null without a console).
    !unsafe { GetConsoleWindow() }.is_null()
}

/// Elsewhere stderr is where it always was.
#[cfg(not(windows))]
pub fn has_console() -> bool {
    true
}

/// Shows `text` in a message window of the system and waits until it is closed.
#[cfg(windows)]
pub fn show_error(text: &str) {
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(text), wide("Takt"));
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that live until the call returns;
    // a null owner is allowed and means the window has no parent.
    let _ = unsafe {
        MessageBoxW(
            core::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONERROR,
        )
    };
}

/// Elsewhere there is no message window: stderr has the text already.
#[cfg(not(windows))]
pub fn show_error(_text: &str) {}

/// Lets `print!` and `eprintln!` reach the terminal the player was started from.
///
/// Without a parent console (a double click, a link) this does nothing, and so it does in a
/// build that has a console of its own (a debug build). Output that is redirected to a file or
/// a pipe keeps going there: the handles the parent passed on are not replaced.
#[cfg(windows)]
pub fn attach_parent() {
    // SAFETY: AttachConsole takes no pointer and only changes which console the process writes
    // to. It fails, harmlessly, when the parent has no console or this process has one already.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// Elsewhere a program has no console of its own to choose.
#[cfg(not(windows))]
pub fn attach_parent() {}

#[cfg(test)]
mod tests {
    #[test]
    fn attaching_twice_or_without_a_parent_console_is_harmless() {
        super::attach_parent();
        super::attach_parent();
    }
}
