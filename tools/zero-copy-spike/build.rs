// SPDX-License-Identifier: GPL-3.0-or-later
//! Compiles the C shim against the FFmpeg headers and links the shared FFmpeg in TAKT_FFMPEG_DIR
//! (a folder with `include/` and `lib/`, for example an LGPL shared build from BtbN).
fn main() {
    println!("cargo:rerun-if-env-changed=TAKT_FFMPEG_DIR");
    println!("cargo:rerun-if-changed=src/shim.c");
    let Ok(dir) = std::env::var("TAKT_FFMPEG_DIR") else {
        println!(
            "cargo:warning=TAKT_FFMPEG_DIR is not set: point it at a shared FFmpeg (include/, lib/)"
        );
        std::process::exit(1);
    };
    cc::Build::new()
        .file("src/shim.c")
        .include(format!("{dir}/include"))
        .warnings(true)
        .compile("spike_shim");
    println!("cargo:rustc-link-search=native={dir}/lib");
    for lib in ["avformat", "avcodec", "avutil", "d3d11", "dxgi"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
