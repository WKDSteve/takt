// SPDX-License-Identifier: GPL-3.0-or-later
//! libmpv behind a safe type: the C interface (`mpv/client.h` and the
//! software renderer of `mpv/render.h`, client API 2.x), only what the player
//! calls.
//!
//! The one module of the player with `unsafe` (KONZEPT, unsafe policy):
//! declarations and calls stay in here, [`Mpv`] and [`Surface`] are what the
//! rest of the program sees.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_double, c_int, c_void};
use std::fmt;
use std::marker::{PhantomData, PhantomPinned};
use std::ptr::{self, NonNull};

/// Opaque `mpv_handle`. The marker keeps it from being `Send`, `Sync` or
/// `Unpin` by accident (Nomicon pattern).
#[repr(C)]
struct Handle {
    _private: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Opaque `mpv_render_context`.
#[repr(C)]
struct RenderContext {
    _private: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// `mpv_render_param`: a type and a pointer whose meaning the type sets.
#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

/// `mpv_event`.
#[repr(C)]
struct RawEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

/// The head of `mpv_event_end_file`, the data of [`EVENT_END_FILE`]. Only
/// the first field is read, so fields a newer libmpv adds behind do not
/// matter.
#[repr(C)]
struct RawEndFile {
    reason: c_int,
    error: c_int,
}

const FORMAT_INT64: c_int = 4;
const FORMAT_DOUBLE: c_int = 5;

const EVENT_NONE: c_int = 0;
const EVENT_SHUTDOWN: c_int = 1;
const EVENT_END_FILE: c_int = 7;
const EVENT_FILE_LOADED: c_int = 8;

const END_FILE_REASON_ERROR: c_int = 4;

const RENDER_PARAM_INVALID: c_int = 0;
const RENDER_PARAM_API_TYPE: c_int = 1;
const RENDER_PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
const RENDER_PARAM_SW_SIZE: c_int = 17;
const RENDER_PARAM_SW_FORMAT: c_int = 18;
const RENDER_PARAM_SW_STRIDE: c_int = 19;
const RENDER_PARAM_SW_POINTER: c_int = 20;

/// `MPV_RENDER_UPDATE_FRAME`: a new frame waits to be rendered.
const RENDER_UPDATE_FRAME: u64 = 1;

/// `MPV_ERROR_NOMEM`, what a null handle of `mpv_create` is reported as.
const ERROR_NOMEM: c_int = -1;
/// `MPV_ERROR_INVALID_PARAMETER`, for a text that cannot be a C string.
const ERROR_INVALID_PARAMETER: c_int = -4;

/// What the surface is aligned to: mpv asks for 64 bytes for pointer and
/// stride, or it takes slower paths.
const ALIGN: usize = 64;

#[link(name = "mpv")]
unsafe extern "C" {
    /// Returns a new handle, or null on OOM or if `LC_NUMERIC` is not "C".
    safe fn mpv_create() -> *mut Handle;
    fn mpv_initialize(ctx: *mut Handle) -> c_int;
    fn mpv_terminate_destroy(ctx: *mut Handle);
    fn mpv_set_option_string(ctx: *mut Handle, name: *const c_char, data: *const c_char) -> c_int;
    /// `args` is an array of C strings that ends with a null pointer.
    fn mpv_command(ctx: *mut Handle, args: *const *const c_char) -> c_int;
    /// `data` points at the type `format` names (`i64`, `f64`).
    fn mpv_get_property(
        ctx: *mut Handle,
        name: *const c_char,
        format: c_int,
        data: *mut c_void,
    ) -> c_int;
    /// Never null; the event is valid until the next call on this handle.
    fn mpv_wait_event(ctx: *mut Handle, timeout: c_double) -> *mut RawEvent;
    /// A static string for any error code.
    safe fn mpv_error_string(error: c_int) -> *const c_char;
    /// `params` ends with an entry of kind [`RENDER_PARAM_INVALID`].
    fn mpv_render_context_create(
        res: *mut *mut RenderContext,
        mpv: *mut Handle,
        params: *mut RenderParam,
    ) -> c_int;
    fn mpv_render_context_update(ctx: *mut RenderContext) -> u64;
    fn mpv_render_context_render(ctx: *mut RenderContext, params: *mut RenderParam) -> c_int;
    fn mpv_render_context_free(ctx: *mut RenderContext);
}

/// A call of libmpv that failed: which one, and its error code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MpvError {
    step: &'static str,
    code: c_int,
}

impl fmt::Display for MpvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: mpv_error_string returns a static NUL-terminated string
        // for every code, also for unknown ones.
        let text = unsafe { CStr::from_ptr(mpv_error_string(self.code)) };
        write!(
            f,
            "{}: {} ({})",
            self.step,
            text.to_string_lossy(),
            self.code
        )
    }
}

impl std::error::Error for MpvError {}

fn check(step: &'static str, code: c_int) -> Result<(), MpvError> {
    if code < 0 {
        Err(MpvError { step, code })
    } else {
        Ok(())
    }
}

/// What the player hears from the event queue of mpv.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The queue is empty.
    None,
    /// The file is open and playback starts.
    FileLoaded,
    /// Playback of the file ended; `failed` when it ended with an error
    /// (the file could not be opened or decoded).
    EndFile {
        /// Ended by an error, not by its end or a stop.
        failed: bool,
    },
    /// The core shuts down.
    Shutdown,
    /// Any other event.
    Other,
}

/// The core handle; destroys the core when dropped.
struct Core(NonNull<Handle>);

impl Drop for Core {
    fn drop(&mut self) {
        // SAFETY: the handle came from mpv_create and is destroyed exactly
        // once, here. A render context of this core is freed before (see
        // the drop of `Mpv`), as render.h asks.
        unsafe { mpv_terminate_destroy(self.0.as_ptr()) }
    }
}

/// An mpv core that renders into memory ([`Surface`]) instead of a window.
///
/// Not `Send`: it stays on the thread that made it, which is all the player
/// needs (mpv itself would allow more).
pub struct Mpv {
    render: NonNull<RenderContext>,
    // Dropped after `render` was freed in `drop`.
    core: Core,
}

impl Mpv {
    /// A core with `options` set before it starts (`mpv_set_option_string`,
    /// the names of the mpv manual without `--`), and the software renderer.
    ///
    /// # Errors
    ///
    /// [`MpvError`] with the call that failed: an unknown option, a value
    /// mpv refuses, or a libmpv built without the software renderer.
    pub fn new(options: &[(&CStr, &CStr)]) -> Result<Self, MpvError> {
        let core = Core(NonNull::new(mpv_create()).ok_or(MpvError {
            step: "mpv_create",
            code: ERROR_NOMEM,
        })?);
        let handle = core.0.as_ptr();
        for (name, value) in options {
            // SAFETY: the handle is live (owned by `core`), name and value
            // are NUL-terminated and outlive the call; mpv copies them.
            let code = unsafe { mpv_set_option_string(handle, name.as_ptr(), value.as_ptr()) };
            check("mpv_set_option_string", code)?;
        }
        // SAFETY: the handle is live and not yet initialized.
        check("mpv_initialize", unsafe { mpv_initialize(handle) })?;

        let mut params = [
            RenderParam {
                kind: RENDER_PARAM_API_TYPE,
                data: c"sw".as_ptr().cast_mut().cast(),
            },
            RenderParam {
                kind: RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        let mut render = ptr::null_mut();
        // SAFETY: the handle is live and initialized; `params` ends with the
        // INVALID entry and its API type is a static C string mpv only
        // reads; `render` is a valid place for the result.
        let code =
            unsafe { mpv_render_context_create(&raw mut render, handle, params.as_mut_ptr()) };
        check("mpv_render_context_create", code)?;
        let render = NonNull::new(render).ok_or(MpvError {
            step: "mpv_render_context_create",
            code: ERROR_NOMEM,
        })?;
        Ok(Self { render, core })
    }

    /// Starts playing `source`: a path, or anything else mpv opens. The
    /// answer comes as [`Event::FileLoaded`] or [`Event::EndFile`].
    ///
    /// # Errors
    ///
    /// [`MpvError`] when the text holds a NUL byte or mpv refuses the command.
    pub fn load(&mut self, source: &str) -> Result<(), MpvError> {
        let source = CString::new(source).map_err(|_| MpvError {
            step: "loadfile",
            code: ERROR_INVALID_PARAMETER,
        })?;
        let args = [c"loadfile".as_ptr(), source.as_ptr(), ptr::null()];
        // SAFETY: the handle is live; `args` holds two NUL-terminated
        // strings that outlive the call and ends with a null pointer.
        check("loadfile", unsafe {
            mpv_command(self.core.0.as_ptr(), args.as_ptr())
        })
    }

    /// The next event of the queue, without waiting.
    pub fn poll(&mut self) -> Event {
        // SAFETY: the handle is live. mpv_wait_event never returns null and
        // the event stays valid until the next call on this handle; it is
        // read here and not kept (`&mut self` rules out a second call
        // meanwhile). The data of an END_FILE event is an
        // mpv_event_end_file, whose head `RawEndFile` mirrors.
        unsafe {
            let event = &*mpv_wait_event(self.core.0.as_ptr(), 0.0);
            match event.event_id {
                EVENT_NONE => Event::None,
                EVENT_FILE_LOADED => Event::FileLoaded,
                EVENT_SHUTDOWN => Event::Shutdown,
                EVENT_END_FILE => {
                    let end = event.data.cast::<RawEndFile>();
                    Event::EndFile {
                        failed: !end.is_null() && (*end).reason == END_FILE_REASON_ERROR,
                    }
                }
                _ => Event::Other,
            }
        }
    }

    /// Whether a new frame waits to be rendered (`mpv_render_context_update`).
    pub fn frame_ready(&mut self) -> bool {
        // SAFETY: the render context is live until `drop`.
        let flags = unsafe { mpv_render_context_update(self.render.as_ptr()) };
        flags & RENDER_UPDATE_FRAME != 0
    }

    /// Renders the current frame into `surface` (scaled to its size, `rgb0`)
    /// without waiting for the time of the frame.
    ///
    /// # Errors
    ///
    /// [`MpvError`] when mpv refuses the surface.
    pub fn render(&mut self, surface: &mut Surface) -> Result<(), MpvError> {
        let mut size = [surface.width, surface.height];
        let mut stride = surface.stride;
        let mut no_wait: c_int = 0;
        let pixels = surface.pixels_mut();
        debug_assert!(pixels.len() >= stride * usize::try_from(size[1]).unwrap_or(0));
        let mut params = [
            RenderParam {
                kind: RENDER_PARAM_SW_SIZE,
                data: size.as_mut_ptr().cast(),
            },
            RenderParam {
                kind: RENDER_PARAM_SW_FORMAT,
                data: c"rgb0".as_ptr().cast_mut().cast(),
            },
            RenderParam {
                kind: RENDER_PARAM_SW_STRIDE,
                data: (&raw mut stride).cast(),
            },
            RenderParam {
                kind: RENDER_PARAM_SW_POINTER,
                data: pixels.as_mut_ptr().cast(),
            },
            RenderParam {
                kind: RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: (&raw mut no_wait).cast(),
            },
            RenderParam {
                kind: RENDER_PARAM_INVALID,
                data: ptr::null_mut(),
            },
        ];
        // SAFETY: the render context is live. Every parameter points at a
        // value of the type render.h names for it (int[2], char*, size_t*,
        // void*, int*) that lives until the call returns; the format is a
        // static string mpv only reads. The pixel pointer is valid for
        // writes of stride * height bytes (`Surface::new` allocates exactly
        // that from the aligned start, checked above) and is borrowed
        // exclusively through `surface`; mpv writes no further.
        let code = unsafe { mpv_render_context_render(self.render.as_ptr(), params.as_mut_ptr()) };
        check("mpv_render_context_render", code)
    }

    /// A property as a floating point number, `None` while mpv has none
    /// (no file, no video).
    pub fn double(&mut self, name: &CStr) -> Option<f64> {
        let mut value = 0.0_f64;
        // SAFETY: the handle is live, the name NUL-terminated, and `value`
        // is the double FORMAT_DOUBLE asks for.
        let code = unsafe {
            mpv_get_property(
                self.core.0.as_ptr(),
                name.as_ptr(),
                FORMAT_DOUBLE,
                (&raw mut value).cast(),
            )
        };
        (code >= 0).then_some(value)
    }

    /// A property as an integer, `None` while mpv has none.
    pub fn int(&mut self, name: &CStr) -> Option<i64> {
        let mut value = 0_i64;
        // SAFETY: as in `double`, with the int64_t FORMAT_INT64 asks for.
        let code = unsafe {
            mpv_get_property(
                self.core.0.as_ptr(),
                name.as_ptr(),
                FORMAT_INT64,
                (&raw mut value).cast(),
            )
        };
        (code >= 0).then_some(value)
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // SAFETY: the render context came from mpv_render_context_create and
        // is freed exactly once, before the core (`core` drops after this).
        unsafe { mpv_render_context_free(self.render.as_ptr()) }
    }
}

/// A picture in memory that mpv renders into: `rgb0`, four bytes a pixel,
/// rows `stride` bytes apart, start and stride aligned to 64 bytes.
pub struct Surface {
    width: c_int,
    height: c_int,
    stride: usize,
    /// Where the aligned picture starts in `data`.
    start: usize,
    data: Vec<u8>,
}

impl Surface {
    /// A black surface, `None` for a size of zero or one beyond 8K
    /// (8192 x 4320).
    ///
    /// The size comes from the file. Without the bound a file that only
    /// claims a huge picture makes the player allocate gigabytes before a
    /// single frame is decoded.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        const MAX_WIDTH: u32 = 8_192;
        const MAX_HEIGHT: u32 = 4_320;
        if width == 0 || height == 0 || width > MAX_WIDTH || height > MAX_HEIGHT {
            return None;
        }
        let stride = (usize::try_from(width).ok()? * 4).next_multiple_of(ALIGN);
        let len = stride * usize::try_from(height).ok()?;
        // Room to move the start to the next multiple of 64.
        let data = vec![0_u8; len + ALIGN];
        let start = data.as_ptr().align_offset(ALIGN);
        // align_offset may refuse (usize::MAX); then the picture starts
        // unaligned, which mpv takes on a slower path.
        let start = if start < ALIGN { start } else { 0 };
        Some(Self {
            width: c_int::try_from(width).ok()?,
            height: c_int::try_from(height).ok()?,
            stride,
            start,
            data,
        })
    }

    /// Bytes from one row to the next.
    #[cfg(test)]
    fn stride(&self) -> usize {
        self.stride
    }

    /// The rows of the picture, `stride * height` bytes.
    pub fn pixels(&self) -> &[u8] {
        &self.data[self.start..self.start + self.len()]
    }

    fn pixels_mut(&mut self) -> &mut [u8] {
        let (start, len) = (self.start, self.len());
        &mut self.data[start..start + len]
    }

    fn len(&self) -> usize {
        self.stride * usize::try_from(self.height).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_surface_is_aligned_and_holds_its_rows() {
        let surface = Surface::new(1921, 1080).unwrap();
        assert_eq!(surface.stride(), 7_744, "1921 * 4 up to a multiple of 64");
        assert_eq!(surface.pixels().len(), 7_744 * 1080);
        assert_eq!(surface.pixels().as_ptr().align_offset(ALIGN), 0);
        assert!(Surface::new(0, 1080).is_none());
        assert!(Surface::new(1920, 0).is_none());
        assert!(Surface::new(8_193, 1).is_none());
        assert!(Surface::new(1, 4_321).is_none());
        assert!(Surface::new(8_192, 4_320).is_some());
    }

    #[test]
    fn an_error_names_the_call_and_the_text_of_mpv() {
        let error = MpvError {
            step: "loadfile",
            code: ERROR_INVALID_PARAMETER,
        };
        assert_eq!(error.to_string(), "loadfile: invalid parameter (-4)");
        assert!(check("x", 0).is_ok());
        assert_eq!(check("x", -1).unwrap_err().code, -1);
    }

    #[test]
    fn an_unknown_option_is_refused_before_the_core_starts() {
        let error = Mpv::new(&[(c"no-such-option-of-takt", c"1")])
            .err()
            .unwrap();
        assert_eq!(error.step, "mpv_set_option_string");
    }

    #[test]
    fn a_text_with_a_nul_byte_is_no_source() {
        let mut mpv =
            Mpv::new(&[(c"config", c"no"), (c"vo", c"libmpv"), (c"ao", c"null")]).unwrap();
        assert_eq!(mpv.load("a\0b").unwrap_err().code, ERROR_INVALID_PARAMETER);
        assert_eq!(mpv.double(c"time-pos"), None, "no file, no position");
        assert!(!mpv.frame_ready());
    }
}
