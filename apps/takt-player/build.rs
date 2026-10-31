// SPDX-License-Identifier: GPL-3.0-or-later
//! Locates the optional native libraries (libmpv, FFmpeg).
//!
//! Nothing is linked unless an environment variable names the directory that
//! holds the import library. Without one the player builds on `NullBackend`
//! alone, which keeps CI free of system dependencies. Setup per platform:
//! `docs/BUILD.md`.

use std::env;
use std::path::PathBuf;

/// One optional native library: where to look and which cfg to enable.
struct NativeLib {
    /// Environment variable naming the root directory.
    var: &'static str,
    /// Subdirectory below the root that holds the import libraries.
    lib_subdir: &'static str,
    /// cfg flag set when the library was found.
    cfg: &'static str,
}

const NATIVE_LIBS: [NativeLib; 2] = [
    // The mpv-dev archives keep everything flat in one directory.
    NativeLib {
        var: "TAKT_LIBMPV_DIR",
        lib_subdir: "",
        cfg: "takt_libmpv",
    },
    // Same variable and layout (`include/`, `lib/`) as ffmpeg-sys-next, so a
    // later FFmpeg binding reads the same setting.
    NativeLib {
        var: "FFMPEG_DIR",
        lib_subdir: "lib",
        cfg: "takt_ffmpeg",
    },
];

fn main() {
    for lib in NATIVE_LIBS {
        println!("cargo::rerun-if-env-changed={}", lib.var);
        println!("cargo::rustc-check-cfg=cfg({})", lib.cfg);

        let Some(root) = env::var_os(lib.var).filter(|v| !v.is_empty()) else {
            continue;
        };
        let mut dir = PathBuf::from(root);
        if !lib.lib_subdir.is_empty() {
            dir.push(lib.lib_subdir);
        }
        // A variable that is set but wrong must fail the build: silently
        // falling back to NullBackend would hide the misconfiguration.
        assert!(
            dir.is_dir(),
            "{} is set, but {} is not a directory (see docs/BUILD.md)",
            lib.var,
            dir.display()
        );
        println!("cargo::rerun-if-changed={}", dir.display());
        println!("cargo::rustc-link-search=native={}", dir.display());
        println!("cargo::rustc-cfg={}", lib.cfg);
    }
}
