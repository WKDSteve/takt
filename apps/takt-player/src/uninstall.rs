// SPDX-License-Identifier: GPL-3.0-or-later
//! `takt-player --uninstall`: the program removes the install it is part of, in its own process.
//!
//! This is what the entry in Settings > Apps runs. The work is `appkit-uninstall`'s (a crate of
//! the kit the author's other apps share, MIT): it takes away what `scripts/install.ps1` made,
//! with the checks of `scripts/uninstall.ps1` (which stays, for the terminal): the Start menu
//! entry and the desktop shortcut when they lead into this install, the files of a release, and
//! last the entry in Settings > Apps. It asks first, in a window; no shell and no other process
//! is started. The program cannot delete its own file and moves it to the folder for temporary
//! files instead.
//!
//! Here is only what is takt's own: [`PRODUCT`].

pub use appkit_uninstall::{Product, run};

/// takt: the player; it has no start with Windows.
pub const PRODUCT: Product = Product {
    name: "takt",
    program: "takt-player.exe",
    program_dir: "",
    link: "Takt.lnk",
    programs: &["takt-player.exe"],
    files: &[
        "takt-player.exe",
        "uninstall.ps1",
        "LICENSE",
        "README.md",
        "VERSION",
    ],
    folders: &[],
    run_value: None,
    env: "TAKT",
    close: "close its window",
    kept: "your media files",
    first: None,
};

#[cfg(test)]
mod tests {
    use super::PRODUCT;

    #[test]
    fn the_product_fits_together() {
        assert_eq!(PRODUCT.check(), Ok(()));
    }
}
