// SPDX-License-Identifier: GPL-3.0-or-later
//! The other side of the import: a D3D11 device on the adapter of wgpu that
//! writes pictures into shared textures and signals a shared fence.
//!
//! This is where a hardware decoder stands (D3D11VA decodes on such a device
//! and copies each picture into a shared texture on the GPU). Here the
//! pictures come from planes in system memory instead. That makes a texture
//! that is foreign to wgpu in every respect that matters for the import
//! (another device, another API, its own clock of completion) without a
//! decoder and without a file: the tests compare it with the upload of the
//! same planes, the measurement fills its layers with it, and the player can
//! show its test picture through it.
#![allow(unsafe_code)]

use std::os::windows::io::{AsHandle, BorrowedHandle, FromRawHandle, OwnedHandle};

use windows::Win32::Foundation::{HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_FLAG,
    D3D11_FENCE_FLAG_SHARED, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device,
    ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory4, IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

use super::ImportError;
use crate::frame::PlanarFormat;
use crate::layout::PlanesView;

/// `GENERIC_ALL`: the access of a shared handle.
const GENERIC_ALL: u32 = 0x1000_0000;

/// The largest side of a shared texture this module creates.
const MAX_SIDE: u32 = 16_384;

/// A D3D11 device that writes into shared textures.
#[derive(Debug)]
pub struct SharedSource {
    device: ID3D11Device5,
    context: ID3D11DeviceContext4,
    fence: ID3D11Fence,
    fence_handle: OwnedHandle,
    /// The value signalled last.
    value: u64,
    /// The rows of one picture in the layout D3D11 takes them in.
    scratch: Vec<u8>,
}

/// A texture both devices see, and the handle the other side opens it with.
#[derive(Debug)]
pub struct SharedTexture {
    texture: ID3D11Texture2D,
    handle: OwnedHandle,
    format: PlanarFormat,
    size: (u32, u32),
}

impl SharedTexture {
    /// The handle [`super::dx12::open_shared_texture`] takes.
    #[must_use]
    pub fn handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }

    /// The format.
    #[must_use]
    pub fn format(&self) -> PlanarFormat {
        self.format
    }

    /// Width and height.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        self.size
    }
}

/// Wraps a handle a `CreateSharedHandle` call returned.
fn owned(handle: HANDLE) -> OwnedHandle {
    // SAFETY: the handle was just created for this process by CreateSharedHandle, is valid and owned by
    // nobody else; OwnedHandle closes it with CloseHandle.
    unsafe { OwnedHandle::from_raw_handle(handle.0) }
}

impl SharedSource {
    /// A D3D11 device on the adapter with `luid`
    /// ([`crate::DeviceInfo::adapter_luid`]), with a fence the device of
    /// wgpu can open.
    ///
    /// # Errors
    ///
    /// [`ImportError::Windows`] when the adapter is gone or Direct3D refuses
    /// (shared fences need Windows 10 1703).
    pub fn on_adapter(luid: [u8; 8]) -> Result<Self, ImportError> {
        let [a, b, c, d, e, f, g, h] = luid;
        let luid = LUID {
            LowPart: u32::from_le_bytes([a, b, c, d]),
            HighPart: i32::from_le_bytes([e, f, g, h]),
        };
        // SAFETY: plain creation calls with local out pointers; every result is checked.
        let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }
            .map_err(|e| ImportError::windows("CreateDXGIFactory1", &e))?;
        // SAFETY: as above.
        let adapter: IDXGIAdapter = unsafe { factory.EnumAdapterByLuid(luid) }
            .map_err(|e| ImportError::windows("IDXGIFactory4::EnumAdapterByLuid", &e))?;
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        // SAFETY: the adapter is alive, the out pointers are locals, and no software rasterizer is named.
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                Some(&raw mut context),
            )
        }
        .map_err(|e| ImportError::windows("D3D11CreateDevice", &e))?;
        let (Some(device), Some(context)) = (device, context) else {
            return Err(ImportError::Resource("missing"));
        };
        let device: ID3D11Device5 = device
            .cast()
            .map_err(|e| ImportError::windows("ID3D11Device5 (Windows 10 1703)", &e))?;
        let context: ID3D11DeviceContext4 = context
            .cast()
            .map_err(|e| ImportError::windows("ID3D11DeviceContext4 (Windows 10 1703)", &e))?;
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: a creation call on the live device with a local out pointer.
        unsafe { device.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &raw mut fence) }
            .map_err(|e| ImportError::windows("ID3D11Device5::CreateFence", &e))?;
        let fence = fence.ok_or(ImportError::Resource("missing"))?;
        // SAFETY: no security attributes and no name; the handle returned is owned by this process.
        let handle = unsafe { fence.CreateSharedHandle(None, GENERIC_ALL, PCWSTR::null()) }
            .map_err(|e| ImportError::windows("ID3D11Fence::CreateSharedHandle", &e))?;
        Ok(Self {
            device,
            context,
            fence,
            fence_handle: owned(handle),
            value: 0,
            scratch: Vec::new(),
        })
    }

    /// The handle [`super::dx12::open_shared_fence`] takes.
    #[must_use]
    pub fn fence_handle(&self) -> BorrowedHandle<'_> {
        self.fence_handle.as_handle()
    }

    /// A shared texture of `format`, `width` by `height` (both even, as the
    /// planar formats of Direct3D ask).
    ///
    /// # Errors
    ///
    /// [`ImportError::Size`] for an odd, empty or huge size, and
    /// [`ImportError::Windows`] when Direct3D refuses (an adapter without
    /// the format).
    pub fn texture(
        &self,
        format: PlanarFormat,
        width: u32,
        height: u32,
    ) -> Result<SharedTexture, ImportError> {
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            return Err(ImportError::Size("empty or larger than 16384"));
        }
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(ImportError::Size("a planar texture has even sides"));
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: match format {
                PlanarFormat::Nv12 => DXGI_FORMAT_NV12,
                PlanarFormat::P010 => DXGI_FORMAT_P010,
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0).cast_unsigned(),
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                .cast_unsigned(),
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: the description is a complete local, there is no initial data, the out pointer is a local.
        unsafe {
            self.device
                .CreateTexture2D(&raw const desc, None, Some(&raw mut texture))
        }
        .map_err(|e| ImportError::windows("ID3D11Device::CreateTexture2D", &e))?;
        let texture = texture.ok_or(ImportError::Resource("missing"))?;
        let resource: IDXGIResource1 = texture
            .cast()
            .map_err(|e| ImportError::windows("IDXGIResource1", &e))?;
        // SAFETY: no security attributes and no name; the handle returned is owned by this process.
        let handle = unsafe { resource.CreateSharedHandle(None, GENERIC_ALL, PCWSTR::null()) }
            .map_err(|e| ImportError::windows("IDXGIResource1::CreateSharedHandle", &e))?;
        Ok(SharedTexture {
            texture,
            handle: owned(handle),
            format,
            size: (width, height),
        })
    }

    /// Writes the picture of `planes` into `texture` and signals the fence.
    /// Returns the value the fence has when the texture holds the picture:
    /// what a frame carries as [`crate::Ready::Fence`].
    ///
    /// The other device must not be reading the texture meanwhile, and must
    /// have left it in `D3D12_RESOURCE_STATE_COMMON`: that is what the guard
    /// of a frame is for.
    ///
    /// # Errors
    ///
    /// [`ImportError::Size`] when the planes are not a picture of the
    /// texture's format and size.
    pub fn write(
        &mut self,
        texture: &SharedTexture,
        planes: &PlanesView<'_>,
    ) -> Result<u64, ImportError> {
        if planes.format() != texture.format.pixel_format() || planes.size() != texture.size {
            return Err(ImportError::Size(
                "the planes are not a picture of this texture",
            ));
        }
        let (Some(luma), Some(chroma)) = (planes.plane(0), planes.plane(1)) else {
            return Err(ImportError::Size("a planar picture has two planes"));
        };
        // Direct3D takes a planar picture as one block: the rows of the luma plane, then the rows of the
        // chroma plane, all at the same pitch. The sides are even, so a chroma row is as long as a luma row.
        let pitch = luma.shape.width as usize * luma.shape.texel_bytes() as usize;
        let rows = luma.shape.height as usize + chroma.shape.height as usize;
        self.scratch.clear();
        self.scratch.reserve(pitch * rows);
        for plane in [luma, chroma] {
            for row in plane
                .data
                .chunks(plane.stride as usize)
                .take(plane.shape.height as usize)
            {
                let Some(row) = row.get(..pitch) else {
                    return Err(ImportError::Size("a row shorter than the picture"));
                };
                self.scratch.extend_from_slice(row);
            }
        }
        if self.scratch.len() != pitch * rows {
            return Err(ImportError::Size("fewer rows than the picture"));
        }
        let pitch = u32::try_from(pitch).map_err(|_| ImportError::Size("a row too long"))?;
        self.value += 1;
        // SAFETY: the texture is alive and was created by this device; `scratch` holds `pitch * rows` bytes,
        // which is the whole of subresource 0 of a planar texture of this size (checked above), and Direct3D
        // copies them before the call returns.
        unsafe {
            self.context.UpdateSubresource(
                &texture.texture,
                0,
                None,
                self.scratch.as_ptr().cast(),
                pitch,
                0,
            );
        }
        // SAFETY: the fence belongs to this device; `Flush` takes no arguments.
        unsafe {
            self.context
                .Signal(&self.fence, self.value)
                .map_err(|e| ImportError::windows("ID3D11DeviceContext4::Signal", &e))?;
            self.context.Flush();
        }
        Ok(self.value)
    }
}
