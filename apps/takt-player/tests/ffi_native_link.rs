// SPDX-License-Identifier: GPL-3.0-or-later
//! Link smoke tests for the optional native libraries.
//!
//! A module only exists when `build.rs` found its library (see
//! `docs/BUILD.md`). The tests prove that linking and loading work and that
//! the library matches the documented licensing state; they do not exercise
//! playback. The `ffi` in the file name is what the unsafe policy (KONZEPT,
//! decisions of 2026-10-30) requires for `unsafe` code.
#![allow(unsafe_code)]

#[cfg(takt_libmpv)]
mod libmpv {
    use std::ffi::{CStr, c_char, c_int, c_ulong, c_void};

    /// Opaque `mpv_handle` from `mpv/client.h`.
    #[repr(C)]
    struct MpvHandle {
        _private: [u8; 0],
    }

    #[link(name = "mpv")]
    unsafe extern "C" {
        /// No arguments, no preconditions, returns `(major << 16) | minor`.
        safe fn mpv_client_api_version() -> c_ulong;
        /// No preconditions; returns a new handle or null.
        safe fn mpv_create() -> *mut MpvHandle;
        fn mpv_set_option_string(
            ctx: *mut MpvHandle,
            name: *const c_char,
            data: *const c_char,
        ) -> c_int;
        fn mpv_initialize(ctx: *mut MpvHandle) -> c_int;
        fn mpv_get_property_string(ctx: *mut MpvHandle, name: *const c_char) -> *mut c_char;
        fn mpv_free(data: *mut c_void);
        fn mpv_terminate_destroy(ctx: *mut MpvHandle);
    }

    /// The `mpv-configuration` property: the build options libmpv was
    /// configured with, or `None` if the handle could not be set up.
    fn configuration() -> Option<String> {
        let ctx = mpv_create();
        if ctx.is_null() {
            return None;
        }
        // SAFETY: ctx is a live handle from mpv_create and is destroyed
        // exactly once at the end. The names are NUL-terminated literals.
        // A non-null property string is owned by the caller and released
        // with mpv_free after it was copied.
        unsafe {
            // config=no: never read the user's mpv.conf or load their scripts.
            let ok = mpv_set_option_string(ctx, c"config".as_ptr(), c"no".as_ptr()) >= 0
                && mpv_initialize(ctx) >= 0;
            let mut config = None;
            if ok {
                let raw = mpv_get_property_string(ctx, c"mpv-configuration".as_ptr());
                if !raw.is_null() {
                    config = Some(CStr::from_ptr(raw).to_string_lossy().into_owned());
                    mpv_free(raw.cast());
                }
            }
            mpv_terminate_destroy(ctx);
            config
        }
    }

    #[test]
    fn client_api_is_version_2() {
        let major = mpv_client_api_version() >> 16;
        assert_eq!(major, 2, "libmpv client API {major}.x, takt targets 2.x");
    }

    #[test]
    fn license_state_matches_build_md() {
        let config = configuration().expect("libmpv did not report mpv-configuration");
        // Decision of 2026-10-31 (KONZEPT): the prebuilt GPL libmpv is
        // accepted in zone 2. A switch to an LGPL build is welcome but must
        // show up in the docs, so this fails until they are updated.
        assert!(
            !config.contains("-Dgpl=false"),
            "libmpv is an LGPL build (-Dgpl=false): update docs/BUILD.md, \
             section \"Lizenz der vorgebauten libmpv\", then this test"
        );
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
