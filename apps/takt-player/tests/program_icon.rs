// SPDX-License-Identifier: GPL-3.0-or-later
//! The program carries the icon of the app (rule APP-11): the resource the icon generator writes
//! is linked in and holds what the `.ico` holds, so a `.res` that was left behind goes red here.
#![cfg(all(windows, target_env = "msvc"))]

const ICO: &[u8] = include_bytes!("../../../assets/icons/takt/takt.ico");

fn u32_at(bytes: &[u8], at: usize) -> usize {
    let word: [u8; 4] = bytes[at..at + 4].try_into().unwrap();
    usize::try_from(u32::from_le_bytes(word)).unwrap()
}

fn holds(whole: &[u8], part: &[u8]) -> bool {
    whole.windows(part.len()).any(|w| w == part)
}

#[test]
fn the_program_carries_the_images_and_the_group_of_the_icon() {
    let exe = std::fs::read(env!("CARGO_BIN_EXE_takt-player")).unwrap();
    let count = usize::from(u16::from_le_bytes([ICO[4], ICO[5]]));
    assert!(count > 0);
    // The group of the resource: the head of the `.ico`, then each entry with an id for its offset.
    let mut group = ICO[..6].to_vec();
    for i in 0..count {
        let entry = &ICO[6 + 16 * i..6 + 16 * (i + 1)];
        let (size, at) = (u32_at(entry, 8), u32_at(entry, 12));
        assert!(
            holds(&exe, &ICO[at..at + size]),
            "image {i} is not in the program"
        );
        group.extend_from_slice(&entry[..12]);
        group.extend_from_slice(&u16::try_from(i + 1).unwrap().to_le_bytes());
    }
    assert!(
        holds(&exe, &group),
        "the group of the icon is not in the program"
    );
}
