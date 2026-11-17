// SPDX-License-Identifier: GPL-3.0-or-later
//! The seam the spike is about: a D3D12 texture of the decoder becomes a `wgpu::Texture`, through wgpu-hal.
#![allow(unsafe_code)]

use std::ffi::c_void;

use wgpu::hal::api::Dx12;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Graphics::Direct3D12::{ID3D12Fence, ID3D12Resource};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
use windows::core::Interface;

/// The `ID3D12Device*` behind a wgpu device of the DX12 backend, for FFmpeg to decode on.
pub fn raw_d3d12_device(device: &wgpu::Device) -> Result<*mut c_void, String> {
    // SAFETY: the handle is only read here; FFmpeg takes its own COM reference before the guard drops.
    let hal = unsafe { device.as_hal::<Dx12>() }.ok_or("the wgpu device is not on DX12")?;
    Ok(hal.raw_device().as_raw())
}

/// Wraps a texture the decoder wrote as a `wgpu::Texture`. No pixel is copied: wgpu gets a second COM
/// reference to the same resource.
pub fn import_texture(
    device: &wgpu::Device,
    resource: *mut c_void,
    format: wgpu::TextureFormat,
    size: wgpu::Extent3d,
) -> Result<wgpu::Texture, String> {
    // SAFETY: `resource` is a live ID3D12Resource* of a decoded frame; `cloned` takes a reference of our own.
    let resource = unsafe { ID3D12Resource::from_raw_borrowed(&resource) }
        .ok_or("the decoder handed out no texture")?
        .clone();
    // SAFETY: format, dimension and size are those of the resource (read from its description by the shim).
    let hal_texture = unsafe {
        wgpu::hal::dx12::Device::texture_from_raw(
            resource,
            format,
            wgpu::TextureDimension::D2,
            size,
            1,
            1,
        )
    };
    let desc = wgpu::TextureDescriptor {
        label: Some("decoder texture"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    // SAFETY: the resource was created on the device behind `device` (FFmpeg decodes on it), matches `desc`,
    // holds a decoded picture, and is in D3D12_RESOURCE_STATE_COMMON, which is what wgpu assumes for an
    // imported texture (FFmpeg returns its pictures to COMMON after every decode).
    Ok(unsafe { device.create_texture_from_hal::<Dx12>(hal_texture, &desc) })
}

/// Lets the queue of wgpu wait on the GPU for the decode of a frame, without blocking a CPU thread.
pub fn queue_wait_for_decode(
    queue: &wgpu::Queue,
    fence: *mut c_void,
    value: u64,
) -> Result<(), String> {
    // SAFETY: only `Wait` is called on the raw queue, which D3D12 allows from any thread.
    let hal = unsafe { queue.as_hal::<Dx12>() }.ok_or("the wgpu queue is not on DX12")?;
    // SAFETY: `fence` is the live ID3D12Fence* of the frame, which the caller still holds.
    let fence =
        unsafe { ID3D12Fence::from_raw_borrowed(&fence) }.ok_or("the frame has no fence")?;
    // SAFETY: fence and queue belong to the same device.
    unsafe { hal.as_raw().Wait(fence, value) }.map_err(|e| format!("ID3D12CommandQueue::Wait: {e}"))
}

/// Kernel plus user time of this process, in seconds.
pub fn process_cpu_seconds() -> f64 {
    let mut times = [FILETIME::default(); 4];
    let [creation, exit, kernel, user] = &mut times;
    // SAFETY: the pseudo handle of the own process is always valid, and the four places are writable.
    if unsafe { GetProcessTimes(GetCurrentProcess(), creation, exit, kernel, user) }.is_err() {
        return 0.0;
    }
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    (ticks(times[2]) + ticks(times[3])) as f64 / 1e7
}
