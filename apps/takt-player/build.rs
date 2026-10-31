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
    /// Library name as passed to the linker (`mpv` -> `mpv.lib`, `libmpv.so`).
    link_name: &'static str,
    /// cfg flag set when the library was found.
    cfg: &'static str,
}

// Order matters: the linker takes the first search path that holds a library.
// FFmpeg goes first so that a system FFmpeg next to libmpv (e.g. /usr/lib)
// cannot shadow the build FFMPEG_DIR points at.
const NATIVE_LIBS: [NativeLib; 2] = [
    // Same variable and layout (`include/`, `lib/`) as ffmpeg-sys-next, so a
    // later FFmpeg binding reads the same setting.
    NativeLib {
        var: "FFMPEG_DIR",
        lib_subdir: "lib",
        link_name: "avutil",
        cfg: "takt_ffmpeg",
    },
    // The mpv-dev archives keep everything flat in one directory.
    NativeLib {
        var: "TAKT_LIBMPV_DIR",
        lib_subdir: "",
        link_name: "mpv",
        cfg: "takt_libmpv",
    },
];

/// File names the linker accepts for `name` on the current target.
fn import_lib_names(name: &str) -> Vec<String> {
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    match (os.as_str(), target_env.as_str()) {
        ("windows", "msvc") => vec![format!("{name}.lib")],
        ("windows", _) => vec![format!("lib{name}.dll.a"), format!("{name}.lib")],
        ("macos", _) => vec![format!("lib{name}.dylib")],
        _ => vec![format!("lib{name}.so")],
    }
}

fn main() {
    for lib in NATIVE_LIBS {
        println!("cargo::rerun-if-env-changed={}", lib.var);
        println!("cargo::rustc-check-cfg=cfg({})", lib.cfg);

        let Some(root) = env::var_os(lib.var).filter(|v| !v.is_empty()) else {
            continue;
        };
        // A variable that is set but wrong must fail the build: silently
        // falling back to NullBackend would hide the misconfiguration.
        let mut dir = PathBuf::from(root);
        // The build script runs in the package directory, rustc in the
        // workspace root; a relative path would mean two different places.
        if !dir.is_absolute() {
            println!(
                "cargo::error={} must be an absolute path, got {} (see docs/BUILD.md)",
                lib.var,
                dir.display()
            );
            continue;
        }
        if !lib.lib_subdir.is_empty() {
            dir.push(lib.lib_subdir);
        }
        let Some(dir_str) = dir.to_str() else {
            println!("cargo::error={} is not valid UTF-8", lib.var);
            continue;
        };
        let candidates = import_lib_names(lib.link_name);
        if !candidates.iter().any(|f| dir.join(f).is_file()) {
            println!(
                "cargo::error={} is set, but {dir_str} holds none of {} (see docs/BUILD.md)",
                lib.var,
                candidates.join(", ")
            );
            continue;
        }
        // No rerun-if-changed on the directory: cargo would scan the whole
        // tree (all of /usr/lib on Linux), and the output only depends on
        // the variable.
        println!("cargo::rustc-link-search=native={dir_str}");
        println!("cargo::rustc-cfg={}", lib.cfg);
    }
}
