// SPDX-License-Identifier: GPL-3.0-or-later
//! The Windows calls of `takt-player --uninstall` (`super`): the question and the answer in a
//! window, the folders of the shell, the registry of the user, where a shortcut leads and which
//! programs run.
//!
//! They are declared here instead of pulling in a crate for them, as `console` does. Nothing
//! here is specific to takt: the names and places come from the caller.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// A handle of Windows (`HANDLE`, `HKEY`, `HWND`).
type Handle = *mut c_void;

/// A `GUID`.
#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

/// `PROCESSENTRY32W`: one line of the process list.
#[repr(C)]
struct ProcessEntry {
    size: u32,
    usage: u32,
    process_id: u32,
    default_heap_id: usize,
    module_id: u32,
    threads: u32,
    parent_process_id: u32,
    priority: i32,
    flags: u32,
    exe_file: [u16; 260],
}

/// The first entries every COM interface has (`IUnknown`).
#[repr(C)]
struct Unknown {
    query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// `IShellLinkW`, as far as it is called: `GetPath` is its first method.
#[repr(C)]
struct ShellLinkCalls {
    unknown: Unknown,
    get_path: unsafe extern "system" fn(*mut c_void, *mut u16, i32, *mut c_void, u32) -> i32,
}

/// `IPersistFile`, as far as it is called: `GetClassID` and `IsDirty` come before `Load`.
#[repr(C)]
struct PersistFileCalls {
    unknown: Unknown,
    get_class_id: usize,
    is_dirty: usize,
    load: unsafe extern "system" fn(*mut c_void, *const u16, u32) -> i32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(owner: Handle, text: *const u16, caption: *const u16, kind: u32) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn IsUserAnAdmin() -> i32;
    fn SHGetKnownFolderPath(id: *const Guid, flags: u32, token: Handle, path: *mut *mut u16)
    -> i32;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *const c_void, kind: u32) -> i32;
    fn CoUninitialize();
    fn CoCreateInstance(
        class: *const Guid,
        outer: *mut c_void,
        context: u32,
        interface: *const Guid,
        object: *mut *mut c_void,
    ) -> i32;
    fn CoTaskMemFree(memory: *const c_void);
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        key: Handle,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        size: *mut u32,
    ) -> i32;
    fn RegDeleteKeyValueW(key: Handle, subkey: *const u16, value: *const u16) -> i32;
    fn RegDeleteTreeW(key: Handle, subkey: *const u16) -> i32;
    #[cfg(test)]
    fn RegSetKeyValueW(
        key: Handle,
        subkey: *const u16,
        value: *const u16,
        kind: u32,
        data: *const c_void,
        size: u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry) -> i32;
    fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry) -> i32;
    fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
    fn QueryFullProcessImageNameW(
        process: Handle,
        flags: u32,
        name: *mut u16,
        size: *mut u32,
    ) -> i32;
    fn CloseHandle(handle: Handle) -> i32;
}

/// `HKEY_CURRENT_USER`: the number 0x8000_0001, sign-extended to the width of a pointer.
const CURRENT_USER: Handle = 0x8000_0001_u32.cast_signed() as isize as Handle;
/// `INVALID_HANDLE_VALUE`.
const NO_HANDLE: Handle = -1_isize as Handle;
const ERROR_FILE_NOT_FOUND: i32 = 2;
/// `RRF_RT_REG_SZ`: only a text is read.
const ONLY_TEXT: u32 = 0x2;
const MB_OK: u32 = 0x0;
const MB_YESNO: u32 = 0x4;
const MB_ICONERROR: u32 = 0x10;
const MB_ICONQUESTION: u32 = 0x20;
const MB_ICONINFORMATION: u32 = 0x40;
const MB_DEFBUTTON2: u32 = 0x100;
const MB_SETFOREGROUND: u32 = 0x1_0000;
const IDYES: i32 = 6;
const TH32CS_SNAPPROCESS: u32 = 0x2;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const COINIT_APARTMENTTHREADED: u32 = 0x2;
const CLSCTX_INPROC_SERVER: u32 = 0x1;
const STGM_READ: u32 = 0x0;
const FOLDERID_PROGRAMS: Guid = Guid(
    0xa77f_5d77,
    0x2e2b,
    0x44c3,
    [0xa6, 0xa2, 0xab, 0xa6, 0x01, 0x05, 0x4a, 0x51],
);
const FOLDERID_DESKTOP: Guid = Guid(
    0xb4bf_cc3a,
    0xdb2c,
    0x424c,
    [0xb0, 0x29, 0x7f, 0xe9, 0x9a, 0x87, 0xc6, 0x41],
);
/// The last eight bytes of the identifiers of OLE.
const OLE: [u8; 8] = [0xc0, 0, 0, 0, 0, 0, 0, 0x46];
static CLSID_SHELL_LINK: Guid = Guid(0x0002_1401, 0, 0, OLE);
static IID_SHELL_LINK_W: Guid = Guid(0x0002_14f9, 0, 0, OLE);
static IID_PERSIST_FILE: Guid = Guid(0x0000_010b, 0, 0, OLE);

/// The longest path asked for in one piece (the long form of Windows paths).
const PATH_ROOM: usize = 32_768;

/// `text` as Windows takes it: UTF-16 with a closing NUL.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// The text in `data` up to its first NUL.
fn text(data: &[u16]) -> String {
    let len = data.iter().position(|&c| c == 0).unwrap_or(data.len());
    String::from_utf16_lossy(&data[..len])
}

/// What Windows calls the error `code`.
fn problem(code: i32) -> String {
    std::io::Error::from_raw_os_error(code).to_string()
}

/// Asks `question` in a window titled `title`; true when the answer is yes. "No" is the button
/// Enter takes.
pub fn ask(title: &str, question: &str) -> bool {
    let (title, question) = (wide(title), wide(question));
    let kind = MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_SETFOREGROUND;
    // SAFETY: both texts end with a NUL and live across the call; the box has no owner window.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            question.as_ptr(),
            title.as_ptr(),
            kind,
        ) == IDYES
    }
}

/// Says `text` in a window titled `title`, as a problem when `problem`.
pub fn tell(title: &str, text: &str, problem: bool) {
    let (title, text) = (wide(title), wide(text));
    let icon = if problem {
        MB_ICONERROR
    } else {
        MB_ICONINFORMATION
    };
    // SAFETY: both texts end with a NUL and live across the call; the box has no owner window.
    let _ = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | icon | MB_SETFOREGROUND,
        )
    };
}

/// Whether this process runs with the rights of an administrator.
#[must_use]
pub fn elevated() -> bool {
    // SAFETY: IsUserAnAdmin takes nothing and only reads the token of the process.
    unsafe { IsUserAnAdmin() != 0 }
}

/// A folder of the shell of the current user.
#[derive(Clone, Copy, Debug)]
pub enum Folder {
    /// Where the Start menu keeps its programs.
    Programs,
    /// The desktop.
    Desktop,
}

/// Where `folder` is for the current user (it may have been moved, as a synced profile moves the
/// desktop).
#[must_use]
pub fn folder(folder: Folder) -> Option<PathBuf> {
    let id = match folder {
        Folder::Programs => FOLDERID_PROGRAMS,
        Folder::Desktop => FOLDERID_DESKTOP,
    };
    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: `id` and `path` live across the call. On success Windows hands out a text that ends
    // with a NUL, which is read up to there before it is freed; it is freed exactly once, and
    // freeing null (after a failure) is allowed.
    unsafe {
        let found = SHGetKnownFolderPath(&raw const id, 0, std::ptr::null_mut(), &raw mut path);
        let folder = (found == 0 && !path.is_null()).then(|| {
            let mut len = 0;
            while *path.add(len) != 0 {
                len += 1;
            }
            PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(
                path, len,
            )))
        });
        CoTaskMemFree(path.cast());
        folder
    }
}

/// The text of the value `name` under `HKEY_CURRENT_USER\<key>` (`name` empty: its default
/// value); `None` when there is none or it is no text.
#[must_use]
pub fn registry_text(key: &str, name: &str) -> Option<String> {
    let (key, name) = (wide(key), wide(name));
    let mut bytes = 0u32;
    // SAFETY: the texts end with a NUL and live across the call; no data is asked for, so only
    // `bytes` is written.
    let asked = unsafe {
        RegGetValueW(
            CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            ONLY_TEXT,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut bytes,
        )
    };
    if asked != 0 {
        return None;
    }
    let mut data = vec![0u16; (bytes as usize).div_ceil(2) + 1];
    bytes = u32::try_from(data.len() * 2).ok()?;
    // SAFETY: `data` holds `bytes` bytes; the texts end with a NUL and live across the call.
    let read = unsafe {
        RegGetValueW(
            CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            ONLY_TEXT,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &raw mut bytes,
        )
    };
    (read == 0).then(|| text(&data))
}

/// Removes the value `name` under `HKEY_CURRENT_USER\<key>`; one that is not there is no error.
///
/// # Errors
///
/// What Windows said when the value is still there.
pub fn remove_registry_value(key: &str, name: &str) -> Result<(), String> {
    let (key, name) = (wide(key), wide(name));
    // SAFETY: both texts end with a NUL and live across the call.
    match unsafe { RegDeleteKeyValueW(CURRENT_USER, key.as_ptr(), name.as_ptr()) } {
        0 | ERROR_FILE_NOT_FOUND => Ok(()),
        code => Err(problem(code)),
    }
}

/// Removes the key `HKEY_CURRENT_USER\<key>` with all below it; one that is not there is no
/// error.
///
/// # Errors
///
/// What Windows said when the key is still there, or that `key` names no key of its own.
pub fn remove_registry_key(key: &str) -> Result<(), String> {
    if key.trim_matches('\\').is_empty() {
        // An empty name would mean everything below HKEY_CURRENT_USER.
        return Err("no key named".to_owned());
    }
    let key = wide(key);
    // SAFETY: `key` ends with a NUL and lives across the call.
    match unsafe { RegDeleteTreeW(CURRENT_USER, key.as_ptr()) } {
        0 | ERROR_FILE_NOT_FOUND => Ok(()),
        code => Err(problem(code)),
    }
}

/// A COM object, released when dropped.
struct Object(*mut c_void);

impl Object {
    /// The calls of the object, read as the table `T`.
    ///
    /// # Safety
    ///
    /// The object must have the interface whose table `T` describes.
    unsafe fn calls<T>(&self) -> &T {
        // SAFETY: a COM object starts with the pointer to the table of its interface, which
        // lives as long as the object; the caller says the table is a `T`.
        unsafe { &**self.0.cast::<*const T>() }
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: every COM interface starts with the calls of IUnknown; the object came from
        // a successful call and is released only here.
        unsafe { (self.calls::<Unknown>().release)(self.0) };
    }
}

/// The program the shortcut `link` (a `.lnk` file) starts; `None` when it cannot be read or
/// names none.
#[must_use]
pub fn shortcut_target(link: &Path) -> Option<PathBuf> {
    let link: Vec<u16> = link.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: COM is set up on this thread for the length of the block and taken down only when
    // this call set it up; both objects are released (dropped at the end of `read`) before that.
    // Each object is called only through the table of the interface it was asked for. `link`
    // ends with a NUL, `path` holds the length it is called with, and no find data is asked for
    // (null).
    unsafe {
        let set_up = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED);
        let read = || {
            let mut raw = std::ptr::null_mut();
            let made = CoCreateInstance(
                &raw const CLSID_SHELL_LINK,
                std::ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                &raw const IID_SHELL_LINK_W,
                &raw mut raw,
            );
            if made < 0 || raw.is_null() {
                return None;
            }
            let shortcut = Object(raw);
            let mut raw = std::ptr::null_mut();
            let asked = (shortcut.calls::<Unknown>().query_interface)(
                shortcut.0,
                &raw const IID_PERSIST_FILE,
                &raw mut raw,
            );
            if asked < 0 || raw.is_null() {
                return None;
            }
            let file = Object(raw);
            if (file.calls::<PersistFileCalls>().load)(file.0, link.as_ptr(), STGM_READ) < 0 {
                return None;
            }
            let mut path = vec![0u16; PATH_ROOM];
            let room = i32::try_from(path.len()).ok()?;
            let got = (shortcut.calls::<ShellLinkCalls>().get_path)(
                shortcut.0,
                path.as_mut_ptr(),
                room,
                std::ptr::null_mut(),
                0,
            );
            let target = text(&path);
            // S_OK: a shortcut without a program answers S_FALSE.
            (got == 0 && !target.is_empty()).then(|| PathBuf::from(target))
        };
        let target = read();
        if set_up >= 0 {
            CoUninitialize();
        }
        target
    }
}

/// A handle closed when dropped.
struct Owned(Handle);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful Create/Open call and is closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The other processes of this session whose program is one of `names` (file names, letter case
/// does not count) and for which `ours` holds: the path of each and its process id. This process
/// is left out. A process that cannot be asked for its path is not ours to judge and left out.
#[must_use]
pub fn running(names: &[&str], ours: impl Fn(&Path) -> bool) -> Vec<(PathBuf, u32)> {
    let mut found = Vec::new();
    // SAFETY: a snapshot of the process list, read entry by entry into `entry`, which says its
    // own size and is otherwise zero (all its fields are numbers); every handle is closed by
    // `Owned`. Only the path of a program is asked for (PROCESS_QUERY_LIMITED_INFORMATION), into
    // `path` with its length in `len`, which comes back as the length written.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == NO_HANDLE {
            return found;
        }
        let snapshot = Owned(snapshot);
        let mut entry: ProcessEntry = std::mem::zeroed();
        entry.size = u32::try_from(size_of::<ProcessEntry>()).unwrap_or(0);
        let mut more = Process32FirstW(snapshot.0, &raw mut entry) != 0;
        while more {
            let pid = entry.process_id;
            let name = text(&entry.exe_file);
            let wanted =
                pid != std::process::id() && names.iter().any(|n| n.eq_ignore_ascii_case(&name));
            let process = if wanted {
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)
            } else {
                std::ptr::null_mut()
            };
            if !process.is_null() {
                let process = Owned(process);
                let mut path = vec![0u16; PATH_ROOM];
                let mut len = u32::try_from(path.len()).unwrap_or(0);
                if QueryFullProcessImageNameW(process.0, 0, path.as_mut_ptr(), &raw mut len) != 0 {
                    let path = PathBuf::from(String::from_utf16_lossy(&path[..len as usize]));
                    if ours(&path) {
                        found.push((path, pid));
                    }
                }
            }
            more = Process32NextW(snapshot.0, &raw mut entry) != 0;
        }
    }
    found
}

/// Writes the text `text` as the value `name` under `HKEY_CURRENT_USER\<key>`, for a test.
#[cfg(test)]
pub fn write_registry_text(key: &str, name: &str, text: &str) {
    /// `REG_SZ`.
    const TEXT: u32 = 1;
    let (key, name, data) = (wide(key), wide(name), wide(text));
    let size = u32::try_from(data.len() * 2).unwrap_or(0);
    // SAFETY: the texts end with a NUL and live across the call; `size` is the size of `data`.
    let written = unsafe {
        RegSetKeyValueW(
            CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            TEXT,
            data.as_ptr().cast(),
            size,
        )
    };
    assert_eq!(written, 0, "{}", problem(written));
}

/// A key below `HKEY_CURRENT_USER\Software` that belongs to one test and is removed with it:
/// never a key of the user.
#[cfg(test)]
pub struct TestPlace(String);

#[cfg(test)]
impl TestPlace {
    /// A key named after `what` and this process.
    pub fn new(what: &str) -> Self {
        Self(format!(
            r"Software\takt-player-test-{what}-{}",
            std::process::id()
        ))
    }

    /// The key, below `HKEY_CURRENT_USER`.
    pub fn key(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestPlace {
    fn drop(&mut self) {
        let _ = remove_registry_key(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use write_registry_text as write;

    #[test]
    fn registry_text_is_read_and_removed() {
        let place = TestPlace::new("uninstall-registry");
        let key = format!(r"{}\below\entry", place.key());
        assert_eq!(registry_text(&key, "Name"), None);
        write(&key, "Name", r"C:\Übung macht\takt");
        write(&key, "", "the default");
        assert_eq!(
            registry_text(&key, "Name").as_deref(),
            Some(r"C:\Übung macht\takt")
        );
        assert_eq!(registry_text(&key, "").as_deref(), Some("the default"));
        remove_registry_value(&key, "Name").unwrap();
        remove_registry_value(&key, "Name").unwrap();
        assert_eq!(registry_text(&key, "Name"), None);
        assert_eq!(registry_text(&key, "").as_deref(), Some("the default"));
        remove_registry_key(&format!(r"{}\below", place.key())).unwrap();
        assert_eq!(registry_text(&key, ""), None);
        remove_registry_key(&format!(r"{}\below", place.key())).unwrap();
    }

    #[test]
    fn a_key_without_a_name_is_never_removed() {
        assert!(remove_registry_key("").is_err());
        assert!(remove_registry_key(r"\").is_err());
    }

    #[test]
    fn the_shell_says_where_its_folders_are() {
        assert!(folder(Folder::Programs).is_some_and(|p| p.is_absolute()));
        assert!(folder(Folder::Desktop).is_some_and(|p| p.is_absolute()));
    }

    #[test]
    fn a_file_that_is_no_shortcut_leads_nowhere() {
        let file =
            std::env::temp_dir().join(format!("takt-player-no-link-{}.lnk", std::process::id()));
        std::fs::write(&file, b"not a shortcut").unwrap();
        assert_eq!(shortcut_target(&file), None);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(shortcut_target(&file), None);
    }

    #[test]
    fn this_process_is_not_among_the_running() {
        let me = std::env::current_exe().unwrap();
        let name = me.file_name().unwrap().to_str().unwrap().to_owned();
        let found = running(&[&name], |_| true);
        assert!(found.iter().all(|(_, pid)| *pid != std::process::id()));
    }

    #[test]
    fn the_process_list_has_the_size_windows_expects() {
        // 64 bit: 4 + 4 + 4 (+ 4 to align) + 8 + 5 * 4 + 260 * 2 (+ 4 to align).
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<ProcessEntry>(), 568);
    }
}
