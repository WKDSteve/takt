// SPDX-License-Identifier: GPL-3.0-or-later
//! The FFI to the C shim (`shim.c`): demux and D3D12VA decode through FFmpeg.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};

#[repr(C)]
struct RawFrame {
    frame: *mut c_void,
    texture: *mut c_void,
    fence: *mut c_void,
    fence_value: u64,
    subresource: c_int,
    array_size: c_int,
    width: c_int,
    height: c_int,
    res_width: c_int,
    res_height: c_int,
    ten_bit: c_int,
    res_flags: u32,
    pts: i64,
}

#[repr(C)]
struct RawPlanes {
    data: [*const u8; 2],
    linesize: [c_int; 2],
}

unsafe extern "C" {
    fn spike_open(
        path: *const c_char,
        d3d12_device: *mut c_void,
        d3d11: c_int,
        extra_frames: c_int,
        err: *mut c_char,
        errlen: c_int,
    ) -> *mut c_void;
    fn spike_decoder_name(s: *const c_void) -> *const c_char;
    fn spike_next(s: *mut c_void, want_texture: c_int, out: *mut RawFrame) -> c_int;
    fn spike_download(s: *mut c_void, frame: *mut c_void, out: *mut RawPlanes) -> c_int;
    fn spike_frame_free(frame: *mut c_void);
    fn spike_close(s: *mut c_void);
}

/// A demuxer and a hardware decoder whose pictures land on the D3D12 device of the caller.
pub struct Decoder {
    raw: *mut c_void,
}

/// One decoded picture as a D3D12 texture. With D3D12VA it is the decoder's own, which the decoder does not
/// write to again while this value lives; with D3D11VA it is one of a few shared textures of the shim.
pub struct Frame {
    raw: *mut c_void,
    /// `ID3D12Resource*`, borrowed from the frame.
    pub texture: *mut c_void,
    /// `ID3D12Fence*`, borrowed from the frame; the decode is done when it reaches `fence_value`.
    pub fence: *mut c_void,
    pub fence_value: u64,
    pub subresource: u32,
    pub array_size: u32,
    pub width: u32,
    pub height: u32,
    pub res_width: u32,
    pub res_height: u32,
    pub ten_bit: bool,
    pub res_flags: u32,
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: `raw` is the AVFrame `spike_next` handed out, freed exactly once.
        unsafe { spike_frame_free(self.raw) }
    }
}

/// The two planes of a downloaded picture, valid until the next download.
pub struct Planes<'a> {
    pub luma: &'a [u8],
    pub luma_stride: u32,
    pub chroma: &'a [u8],
    pub chroma_stride: u32,
}

impl Decoder {
    /// `d3d12_device` is an `ID3D12Device*`; the shim takes its own reference. `d3d11` decodes with D3D11VA
    /// and copies each picture on the GPU into a texture shared with that device, instead of decoding with
    /// D3D12VA on it.
    pub fn open(
        path: &str,
        d3d12_device: *mut c_void,
        d3d11: bool,
        extra_frames: u32,
    ) -> Result<Self, String> {
        let c_path = CString::new(path).map_err(|_| "path contains NUL".to_string())?;
        let mut err = [0 as c_char; 256];
        // SAFETY: the path is NUL-terminated, the device pointer is a live ID3D12Device of the caller, and the
        // shim writes at most `errlen` bytes including the terminator.
        let raw = unsafe {
            spike_open(
                c_path.as_ptr(),
                d3d12_device,
                c_int::from(d3d11),
                extra_frames as c_int,
                err.as_mut_ptr(),
                err.len() as c_int,
            )
        };
        if raw.is_null() {
            // SAFETY: the shim terminated the buffer (snprintf).
            let msg = unsafe { CStr::from_ptr(err.as_ptr()) };
            return Err(msg.to_string_lossy().into_owned());
        }
        Ok(Self { raw })
    }

    pub fn name(&self) -> String {
        // SAFETY: the name is a static string of libavcodec.
        unsafe { CStr::from_ptr(spike_decoder_name(self.raw)) }
            .to_string_lossy()
            .into_owned()
    }

    /// The next picture in presentation order, `None` at the end of the stream. Without `want_texture` the
    /// texture fields of a D3D11VA frame stay empty (no GPU copy happens).
    pub fn next_frame(&mut self, want_texture: bool) -> Result<Option<Frame>, String> {
        // SAFETY: RawFrame is plain data; all-zero is a valid value.
        let mut out: RawFrame = unsafe { std::mem::zeroed() };
        // SAFETY: `raw` is a live Spike and `out` a valid place to write.
        match unsafe { spike_next(self.raw, c_int::from(want_texture), &mut out) } {
            0 => Ok(Some(Frame {
                raw: out.frame,
                texture: out.texture,
                fence: out.fence,
                fence_value: out.fence_value,
                subresource: out.subresource as u32,
                array_size: out.array_size as u32,
                width: out.width as u32,
                height: out.height as u32,
                res_width: out.res_width as u32,
                res_height: out.res_height as u32,
                ten_bit: out.ten_bit != 0,
                res_flags: out.res_flags,
            })),
            1 => Ok(None),
            code => Err(format!("decode failed (AVERROR {code})")),
        }
    }

    /// The CPU detour (hwdownload): copies the picture into system memory.
    pub fn download(&mut self, frame: &Frame) -> Result<Planes<'_>, String> {
        // SAFETY: RawPlanes is plain data; all-zero is a valid value.
        let mut out: RawPlanes = unsafe { std::mem::zeroed() };
        // SAFETY: both pointers are live; the shim fills `out` on success.
        let code = unsafe { spike_download(self.raw, frame.raw, &mut out) };
        if code < 0 {
            return Err(format!("av_hwframe_transfer_data failed (AVERROR {code})"));
        }
        let (luma_stride, chroma_stride) = (out.linesize[0] as usize, out.linesize[1] as usize);
        let (luma_rows, chroma_rows) = (frame.height as usize, frame.height.div_ceil(2) as usize);
        // SAFETY: FFmpeg allocated `linesize * rows` bytes for each plane of the software frame, which lives
        // in the Spike until the next download; the borrow of `self` keeps that call away.
        let (luma, chroma) = unsafe {
            (
                std::slice::from_raw_parts(out.data[0], luma_stride * luma_rows),
                std::slice::from_raw_parts(out.data[1], chroma_stride * chroma_rows),
            )
        };
        Ok(Planes {
            luma,
            luma_stride: luma_stride as u32,
            chroma,
            chroma_stride: chroma_stride as u32,
        })
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `spike_open` and is closed exactly once.
        unsafe { spike_close(self.raw) }
    }
}
