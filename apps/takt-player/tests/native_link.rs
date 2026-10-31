// SPDX-License-Identifier: GPL-3.0-or-later
//! Link smoke tests for the optional native libraries.
//!
//! A module only exists when `build.rs` found its library (see
//! `docs/BUILD.md`). The tests prove that linking and loading work and that
//! the library meets the licensing rules; they do not exercise playback.

#[cfg(takt_libmpv)]
mod libmpv {
    use std::ffi::c_ulong;

    #[link(name = "mpv")]
    unsafe extern "C" {
        /// `mpv_client_api_version` from `mpv/client.h`: no arguments, no
        /// preconditions, returns `(major << 16) | minor`.
        safe fn mpv_client_api_version() -> c_ulong;
    }

    #[test]
    fn client_api_is_version_2() {
        let major = mpv_client_api_version() >> 16;
        assert_eq!(major, 2, "libmpv client API {major}.x, takt targets 2.x");
    }
}

// Checks only the separately linked FFmpeg from FFMPEG_DIR. The prebuilt
// libmpv carries its own statically linked FFmpeg, which this test cannot
// see (docs/BUILD.md, "Lizenz der vorgebauten libmpv").
#[cfg(takt_ffmpeg)]
mod ffmpeg {
    use std::ffi::{CStr, c_char};

    #[link(name = "avutil")]
    unsafe extern "C" {
        /// `avutil_license` from `libavutil/avutil.h`: returns a pointer to
        /// a static, NUL-terminated string.
        safe fn avutil_license() -> *const c_char;
    }

    #[test]
    fn build_is_lgpl() {
        // SAFETY: avutil_license returns a static NUL-terminated string that
        // lives for the whole process and is never written to.
        let license = unsafe { CStr::from_ptr(avutil_license()) }.to_string_lossy();
        // A GPL build of FFmpeg would turn the whole player into GPL-only
        // binaries with no way back (KONZEPT, license zones).
        assert!(
            license.starts_with("LGPL"),
            "FFmpeg build is licensed {license:?}, takt needs an LGPL build"
        );
    }
}
