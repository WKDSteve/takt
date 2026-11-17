// SPDX-License-Identifier: GPL-3.0-or-later
//! Locates FFmpeg for the demux helper.
//!
//! Nothing is linked unless `FFMPEG_DIR` names the root of an FFmpeg with `include/` and `lib/`
//! (the same variable and layout `apps/takt-player/build.rs` reads; setup per platform:
//! `docs/BUILD.md`). Without it the crate builds all the same: the client side needs no FFmpeg,
//! and the helper answers every file with "built without FFmpeg". With it the cfg `takt_ffmpeg`
//! turns the bindings on and the helper links libavformat, libavcodec and libavutil dynamically.
//!
//! The bindings in `src/ffi.rs` read a few fields of FFmpeg's structures directly, so they are
//! written for one major version of each library. The build checks the headers for it and fails
//! with a message instead of producing a helper that reads the wrong bytes.

use std::env;
use std::path::{Path, PathBuf};

/// The library versions `src/ffi.rs` was written against (FFmpeg 9.0): header, macro, major.
const EXPECTED: [(&str, &str, u32); 3] = [
    (
        "libavformat/version_major.h",
        "LIBAVFORMAT_VERSION_MAJOR",
        63,
    ),
    ("libavcodec/version_major.h", "LIBAVCODEC_VERSION_MAJOR", 63),
    ("libavutil/version.h", "LIBAVUTIL_VERSION_MAJOR", 61),
];

/// The libraries the helper links, in the order the linker wants them.
const LIBS: [&str; 3] = ["avformat", "avcodec", "avutil"];

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

/// The value of `#define <name> <number>` in a header.
fn major_in(header: &Path, name: &str) -> Option<u32> {
    let text = std::fs::read_to_string(header).ok()?;
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("#define") && words.next() == Some(name))
            .then(|| words.next()?.parse().ok())
            .flatten()
    })
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=FFMPEG_DIR");
    println!("cargo::rustc-check-cfg=cfg(takt_ffmpeg)");

    let Some(root) = env::var_os("FFMPEG_DIR").filter(|v| !v.is_empty()) else {
        return;
    };
    // A variable that is set but wrong must fail the build: a helper silently built without
    // FFmpeg would hide the misconfiguration until the first file.
    let root = PathBuf::from(root);
    if !root.is_absolute() {
        println!(
            "cargo::error=FFMPEG_DIR must be an absolute path, got {} (see docs/BUILD.md)",
            root.display()
        );
        return;
    }
    let lib = root.join("lib");
    let Some(lib_str) = lib.to_str() else {
        println!("cargo::error=FFMPEG_DIR is not valid UTF-8");
        return;
    };
    for name in LIBS {
        let candidates = import_lib_names(name);
        if !candidates.iter().any(|f| lib.join(f).is_file()) {
            println!(
                "cargo::error=FFMPEG_DIR is set, but {lib_str} holds none of {} (see docs/BUILD.md)",
                candidates.join(", ")
            );
            return;
        }
    }
    for (header, name, expected) in EXPECTED {
        let path = root.join("include").join(header);
        match major_in(&path, name) {
            Some(major) if major == expected => {}
            Some(major) => {
                println!(
                    "cargo::error=FFMPEG_DIR holds {name} {major}, takt-demux is written for {expected} \
                     (FFmpeg 9.0); see crates/takt-demux/layout/README.md"
                );
                return;
            }
            None => {
                println!(
                    "cargo::error=FFMPEG_DIR is set, but {} does not state {name} (see docs/BUILD.md)",
                    path.display()
                );
                return;
            }
        }
    }
    println!("cargo::rustc-link-search=native={lib_str}");
    println!("cargo::rustc-cfg=takt_ffmpeg");
}
