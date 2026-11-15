// SPDX-License-Identifier: GPL-3.0-or-later
//! Links libmpv: on Windows the import library `mpv.lib` in TAKT_LIBMPV_DIR, elsewhere the system's libmpv
//! (or the folder in TAKT_LIBMPV_DIR).
fn main() {
    println!("cargo:rerun-if-env-changed=TAKT_LIBMPV_DIR");
    if let Ok(dir) = std::env::var("TAKT_LIBMPV_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    }
    println!("cargo:rustc-link-lib=dylib=mpv");
}
