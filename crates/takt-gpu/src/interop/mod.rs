// SPDX-License-Identifier: GPL-3.0-or-later
//! Textures another device wrote, as textures of wgpu: the import behind
//! [`crate::GpuFrame::Texture`].
//!
//! wgpu has no import of a foreign handle in its own API (its
//! `ExternalTexture` takes textures of wgpu only), so this goes through
//! wgpu-hal: the native resource is wrapped (`texture_from_raw`) and handed
//! to wgpu (`create_texture_from_hal`). Everything behind that is ordinary
//! wgpu. The `unsafe` of it lives in the two modules here and nowhere else in
//! the crate.
//!
//! Windows only so far, on the DX12 backend:
//!
//! * [`dx12`]: opens a shared texture and a shared fence on the device of
//!   wgpu.
//! * [`d3d11`]: the other side, a D3D11 device on the same adapter that
//!   writes into shared textures. That is the place of a hardware decoder
//!   (D3D11VA); here it writes planes from system memory, which is how the
//!   import is tested without a decoder.
//!
//! A handle, a size, a format: whatever a caller passes in here may come from
//! a helper process. The texture is asked what it is (its own description,
//! which the driver answers) and checked against the limits of the device
//! before wgpu sees it.

use std::fmt;

pub mod d3d11;
pub mod dx12;

/// Why a texture or a fence could not be imported or written.
#[derive(Debug)]
pub enum ImportError {
    /// The device of wgpu is not on the DX12 backend.
    Backend,
    /// The device lacks the feature of the format.
    Unsupported(&'static str),
    /// The resource behind a handle is not what the caller said.
    Resource(&'static str),
    /// A size that cannot be shared or does not match.
    Size(&'static str),
    /// A call into Direct3D failed.
    Windows {
        /// The call.
        what: &'static str,
        /// Its HRESULT.
        code: i32,
        /// The text of the system for it.
        message: String,
    },
}

impl ImportError {
    fn windows(what: &'static str, error: &windows::core::Error) -> Self {
        Self::Windows {
            what,
            code: error.code().0,
            message: error.message(),
        }
    }
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend => write!(f, "the wgpu device is not on DX12"),
            Self::Unsupported(what) => write!(f, "the device lacks {what}"),
            Self::Resource(what) => write!(f, "the shared resource is {what}"),
            Self::Size(what) => write!(f, "size: {what}"),
            Self::Windows {
                what,
                code,
                message,
            } => write!(f, "{what} failed with {code:#010x}: {message}"),
        }
    }
}

impl std::error::Error for ImportError {}
