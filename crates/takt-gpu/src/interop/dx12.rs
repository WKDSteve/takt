// SPDX-License-Identifier: GPL-3.0-or-later
//! The D3D12 side of the import: a shared texture and a shared fence, opened
//! on the device of wgpu.
//!
//! The API, as read in the pinned sources (`wgpu-hal-27.0.4/src/dx12`,
//! `wgpu-27.0.1/src/api/device.rs`):
//!
//! * `dx12::Device::texture_from_raw` wraps an `ID3D12Resource` without
//!   touching it (one COM reference more).
//! * `wgpu::Device::create_texture_from_hal` makes an ordinary texture of it.
//!   wgpu-core enters it in the state `TextureUses::UNINITIALIZED`, which the
//!   DX12 side maps to `D3D12_RESOURCE_STATE_COMMON`: the state a shared
//!   texture is opened in.
//! * `dx12::Queue::as_raw` is the `ID3D12CommandQueue`; `Wait(fence, value)`
//!   on it lets the queue of wgpu wait on the GPU.
#![allow(unsafe_code)]

use std::os::windows::io::{AsRawHandle, BorrowedHandle};

use wgpu::hal::api::Dx12;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_DIMENSION_TEXTURE2D, ID3D12Fence, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010};

use super::ImportError;
use crate::frame::PlanarFormat;

/// The LUID of the adapter behind `device` as its eight bytes, low part
/// first; `None` when the device is not on DX12.
#[must_use]
pub fn adapter_luid(device: &wgpu::Device) -> Option<[u8; 8]> {
    // SAFETY: the raw device is only asked for a value while the guard of wgpu holds it alive.
    let hal = unsafe { device.as_hal::<Dx12>() }?;
    // SAFETY: a call without arguments on a live ID3D12Device.
    let luid = unsafe { hal.raw_device().GetAdapterLuid() };
    let mut out = [0_u8; 8];
    out[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
    out[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
    Some(out)
}

/// Opens the shared texture behind `handle` (an NT handle of
/// `IDXGIResource1::CreateSharedHandle`) on the device of wgpu and wraps it
/// as a texture of wgpu. No pixel is copied: both devices see the same
/// memory.
///
/// The caller says only which format it expects. Size and array layers are
/// read from the resource and checked against the limits of the device.
///
/// # Errors
///
/// [`ImportError`] when the device is not on DX12 or lacks the format, when
/// the handle is not a texture of this adapter, or when the texture is not a
/// plain 2D texture of `format`.
pub fn open_shared_texture(
    device: &wgpu::Device,
    handle: BorrowedHandle<'_>,
    format: PlanarFormat,
) -> Result<wgpu::Texture, ImportError> {
    if !device.features().contains(format.feature()) {
        return Err(ImportError::Unsupported(match format {
            PlanarFormat::Nv12 => "NV12 textures (TEXTURE_FORMAT_NV12)",
            PlanarFormat::P010 => "P010 textures (TEXTURE_FORMAT_P010)",
        }));
    }
    let resource = {
        // SAFETY: the raw device is used for one call while the guard of wgpu holds it alive.
        let hal = unsafe { device.as_hal::<Dx12>() }.ok_or(ImportError::Backend)?;
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: `handle` is a live handle of this process for as long as it is borrowed; D3D12 looks it up
        // and fails for a handle that is not a shared resource of this adapter. The out pointer is a local.
        unsafe {
            hal.raw_device()
                .OpenSharedHandle(HANDLE(handle.as_raw_handle()), &raw mut resource)
        }
        .map_err(|e| ImportError::windows("ID3D12Device::OpenSharedHandle(texture)", &e))?;
        resource.ok_or(ImportError::Resource("missing"))?
    };
    // SAFETY: a call without arguments on the live resource just opened.
    let desc = unsafe { resource.GetDesc() };
    let expected = match format {
        PlanarFormat::Nv12 => DXGI_FORMAT_NV12,
        PlanarFormat::P010 => DXGI_FORMAT_P010,
    };
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D {
        return Err(ImportError::Resource("not a 2D texture"));
    }
    if desc.Format != expected {
        return Err(ImportError::Resource("of another format"));
    }
    if desc.MipLevels != 1 || desc.SampleDesc.Count != 1 {
        return Err(ImportError::Resource("mip-mapped or multisampled"));
    }
    let limits = device.limits();
    let width = u32::try_from(desc.Width).unwrap_or(u32::MAX);
    let layers = u32::from(desc.DepthOrArraySize);
    if width == 0
        || desc.Height == 0
        || width > limits.max_texture_dimension_2d
        || desc.Height > limits.max_texture_dimension_2d
        || layers == 0
        || layers > limits.max_texture_array_layers
    {
        return Err(ImportError::Size("outside the limits of the device"));
    }
    let size = wgpu::Extent3d {
        width,
        height: desc.Height,
        depth_or_array_layers: layers,
    };
    let texture_format = format.texture_format();
    // SAFETY: format, dimension, size, mip levels and sample count are those the resource itself reported.
    let hal_texture = unsafe {
        wgpu::hal::dx12::Device::texture_from_raw(
            resource,
            texture_format,
            wgpu::TextureDimension::D2,
            size,
            1,
            1,
        )
    };
    let descriptor = wgpu::TextureDescriptor {
        label: Some("takt imported texture"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: texture_format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    // SAFETY: the resource was opened on the device behind `device`, matches `descriptor` (checked above), and
    // a shared texture is opened in D3D12_RESOURCE_STATE_COMMON, which is what wgpu assumes for an imported
    // texture. Its content is defined whenever a frame says so (`Ready`).
    Ok(unsafe { device.create_texture_from_hal::<Dx12>(hal_texture, &descriptor) })
}

/// A fence another device signals, opened on the device of wgpu.
#[derive(Clone, Debug)]
pub struct Fence(ID3D12Fence);

/// Opens the shared fence behind `handle` (an NT handle of
/// `ID3D11Fence::CreateSharedHandle` or `ID3D12Device::CreateSharedHandle`).
///
/// # Errors
///
/// [`ImportError`] when the device is not on DX12 or the handle is not a
/// fence of this adapter.
pub fn open_shared_fence(
    device: &wgpu::Device,
    handle: BorrowedHandle<'_>,
) -> Result<Fence, ImportError> {
    // SAFETY: the raw device is used for one call while the guard of wgpu holds it alive.
    let hal = unsafe { device.as_hal::<Dx12>() }.ok_or(ImportError::Backend)?;
    let mut fence: Option<ID3D12Fence> = None;
    // SAFETY: as for the texture: a live borrowed handle, looked up by D3D12, and a local out pointer.
    unsafe {
        hal.raw_device()
            .OpenSharedHandle(HANDLE(handle.as_raw_handle()), &raw mut fence)
    }
    .map_err(|e| ImportError::windows("ID3D12Device::OpenSharedHandle(fence)", &e))?;
    fence.map(Fence).ok_or(ImportError::Resource("missing"))
}

impl Fence {
    /// The value the fence has reached.
    #[must_use]
    pub fn completed(&self) -> u64 {
        // SAFETY: a call without arguments on a live fence.
        unsafe { self.0.GetCompletedValue() }
    }

    /// Lets everything submitted to `queue` after this call wait on the GPU
    /// until the fence reaches `value`; no thread blocks. Nothing is queued
    /// when the fence is there already.
    ///
    /// A value the fence never reaches stalls the queue for good: whoever
    /// signals the fence must be trusted to do so, or to be gone together
    /// with this process.
    ///
    /// # Errors
    ///
    /// [`ImportError`] when the queue is not on DX12 or D3D12 refuses.
    pub fn wait_on(&self, queue: &wgpu::Queue, value: u64) -> Result<(), ImportError> {
        if self.completed() >= value {
            return Ok(());
        }
        // SAFETY: only `Wait` is called on the raw queue, which D3D12 allows from any thread.
        let hal = unsafe { queue.as_hal::<Dx12>() }.ok_or(ImportError::Backend)?;
        // SAFETY: the fence is alive (held by `self`) and belongs to the device of the queue: it was opened
        // on it.
        unsafe { hal.as_raw().Wait(&self.0, value) }
            .map_err(|e| ImportError::windows("ID3D12CommandQueue::Wait", &e))
    }
}
