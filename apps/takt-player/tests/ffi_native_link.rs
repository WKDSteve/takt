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
    use std::marker::{PhantomData, PhantomPinned};

    /// Opaque `mpv_handle` from `mpv/client.h`. The marker keeps it from
    /// being `Send`, `Sync` or `Unpin` by accident (Nomicon pattern).
    #[repr(C)]
    struct MpvHandle {
        _private: [u8; 0],
        _marker: PhantomData<(*mut u8, PhantomPinned)>,
    }

    #[link(name = "mpv")]
    unsafe extern "C" {
        /// No arguments, no preconditions, returns `(major << 16) | minor`.
        safe fn mpv_client_api_version() -> c_ulong;
        /// Returns a new handle, or null on OOM or if `LC_NUMERIC` is not
        /// "C"; neither case is undefined behaviour.
        safe fn mpv_create() -> *mut MpvHandle;
        fn mpv_set_property_string(
            ctx: *mut MpvHandle,
            name: *const c_char,
            data: *const c_char,
        ) -> c_int;
        fn mpv_initialize(ctx: *mut MpvHandle) -> c_int;
        fn mpv_get_property_string(ctx: *mut MpvHandle, name: *const c_char) -> *mut c_char;
        fn mpv_free(data: *mut c_void);
        fn mpv_terminate_destroy(ctx: *mut MpvHandle);
    }

    /// Options set before `mpv_initialize`. `config=no` is already the
    /// libmpv default and only made explicit (no mpv.conf, no `~~/scripts`);
    /// `load-scripts=no` is a second guard against user scripts; `ytdl=no`
    /// keeps the ytdl hook out. The other built-in scripts (stats, console,
    /// select, ...) still load but do nothing without a VO or a file; their
    /// `load-*` switches are left alone because a renamed option in a newer
    /// DLL would fail this test for reasons unrelated to the licence.
    const OPTIONS: [(&CStr, &CStr); 3] = [
        (c"config", c"no"),
        (c"load-scripts", c"no"),
        (c"ytdl", c"no"),
    ];

    /// The `mpv-configuration` property: the build options libmpv was
    /// configured with. The error names the step that failed.
    fn configuration() -> Result<String, String> {
        let ctx = mpv_create();
        if ctx.is_null() {
            return Err("mpv_create returned null".into());
        }
        // SAFETY: ctx is a live handle from mpv_create and is destroyed
        // exactly once at the end. All names and values are NUL-terminated
        // literals. A non-null property string is owned by the caller and
        // released with mpv_free after it was copied.
        unsafe {
            let mut result = Ok(());
            for (name, value) in OPTIONS {
                let rc = mpv_set_property_string(ctx, name.as_ptr(), value.as_ptr());
                if rc < 0 {
                    result = Err(format!("setting {name:?} failed with {rc}"));
                    break;
                }
            }
            if result.is_ok() {
                let rc = mpv_initialize(ctx);
                if rc < 0 {
                    result = Err(format!("mpv_initialize failed with {rc}"));
                }
            }
            let config = result.and_then(|()| {
                let raw = mpv_get_property_string(ctx, c"mpv-configuration".as_ptr());
                if raw.is_null() {
                    return Err("mpv-configuration is not available".into());
                }
                let config = CStr::from_ptr(raw).to_string_lossy().into_owned();
                mpv_free(raw.cast());
                Ok(config)
            });
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
        let config = configuration().unwrap_or_else(|e| panic!("libmpv setup: {e}"));
        // Decision of 2026-10-31 (KONZEPT): the prebuilt GPL libmpv is
        // accepted in zone 2. A switch to an LGPL build is welcome but must
        // show up in the docs, so this fails until they are updated. meson
        // accepts booleans in any case, hence the lowercase comparison. Only
        // the command-line option is visible here (docs/BUILD.md names the
        // limits of this check).
        assert!(
            !config.to_ascii_lowercase().contains("-dgpl=false"),
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
