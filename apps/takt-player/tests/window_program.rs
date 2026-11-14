// SPDX-License-Identifier: GPL-3.0-or-later
//! The release build of the player is a window program: Windows opens no console window for it
//! when it is started from the Start menu. A debug build keeps its console (the output of a
//! development run). Read from the header of the program itself, so a lost attribute goes red
//! here in a release test run (`cargo test --release`) and in `scripts/test-release.ps1`.
#![cfg(windows)]

/// `IMAGE_SUBSYSTEM_WINDOWS_GUI` and `IMAGE_SUBSYSTEM_WINDOWS_CUI`.
const GUI: u16 = 2;
const CONSOLE: u16 = 3;

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// The subsystem field of a PE file: the optional header starts 24 bytes after the signature
/// `PE\0\0`, and the field is at offset 68 of it (the same in PE32 and PE32+).
fn subsystem(exe: &[u8]) -> u16 {
    assert_eq!(&exe[..2], b"MZ", "no program of Windows");
    let pe = usize::try_from(u32::from_le_bytes([
        exe[0x3c], exe[0x3d], exe[0x3e], exe[0x3f],
    ]))
    .unwrap();
    assert_eq!(&exe[pe..pe + 4], b"PE\0\0", "no PE signature");
    u16_at(exe, pe + 24 + 68)
}

#[test]
fn the_release_build_is_a_window_program_and_a_debug_build_keeps_its_console() {
    let exe = std::fs::read(env!("CARGO_BIN_EXE_takt-player")).unwrap();
    let want = if cfg!(debug_assertions) { CONSOLE } else { GUI };
    assert_eq!(subsystem(&exe), want);
}

#[test]
fn the_field_is_read_where_windows_keeps_it() {
    // A program of Windows that is known to be a console program, and one that is a window program.
    let root = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap());
    let cmd = std::fs::read(root.join("System32").join("cmd.exe")).unwrap();
    let explorer = std::fs::read(root.join("explorer.exe")).unwrap();
    assert_eq!(subsystem(&cmd), CONSOLE);
    assert_eq!(subsystem(&explorer), GUI);
}
