// SPDX-License-Identifier: GPL-3.0-or-later
//! The device: which features it needs, how one is opened outside a window,
//! and what a decoder is told about it.
//!
//! The order at start is device first, decoder second: a hardware decoder
//! must work on the adapter of the device that draws, or its textures cannot
//! be imported. So the seam to `takt-decode` begins with [`DeviceInfo`] going
//! out, and [`crate::GpuFrame`] comes back.

use std::fmt;

/// The features this crate uses when the adapter has them: the planar
/// formats of imported decoder textures, and 16-bit normalised textures for
/// 10-bit frames from system memory. None of them is required; a path whose
/// feature is missing is refused where it is used.
pub const WANTED_FEATURES: wgpu::Features = wgpu::Features::TEXTURE_FORMAT_NV12
    .union(wgpu::Features::TEXTURE_FORMAT_P010)
    .union(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM);

/// What of [`WANTED_FEATURES`] an adapter with `available` features offers:
/// add this to the features of the device descriptor.
#[must_use]
pub fn wanted_features(available: wgpu::Features) -> wgpu::Features {
    WANTED_FEATURES.intersection(available)
}

/// The backends a device for video should be looked for on. Under Windows
/// this is DX12 alone: the import of a decoder texture is per backend, and
/// the hardware decoders there (D3D11VA, D3D12VA) share with D3D12.
#[must_use]
pub fn preferred_backends() -> wgpu::Backends {
    if cfg!(windows) {
        wgpu::Backends::DX12
    } else {
        wgpu::Backends::PRIMARY
    }
}

/// How [`Gpu::open`] looks for its adapter.
#[derive(Clone, Copy, Debug)]
pub struct GpuOptions {
    /// The backends to look on.
    pub backends: wgpu::Backends,
    /// Ask for the software adapter (WARP of DX12 under Windows): a machine
    /// without a GPU, such as a CI runner, renders with it.
    pub fallback_adapter: bool,
}

impl Default for GpuOptions {
    fn default() -> Self {
        Self {
            backends: preferred_backends(),
            fallback_adapter: false,
        }
    }
}

/// Why no device could be opened.
#[derive(Debug)]
pub struct OpenError(String);

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no GPU device: {}", self.0)
    }
}

impl std::error::Error for OpenError {}

/// A device of its own, for work without a window: tests, measurements, an
/// export. A window brings its device with it (eframe), which then only
/// needs [`wanted_features`].
#[derive(Debug)]
pub struct Gpu {
    /// The instance the adapter came from.
    pub instance: wgpu::Instance,
    /// The adapter.
    pub adapter: wgpu::Adapter,
    /// The device, with [`wanted_features`] of the adapter.
    pub device: wgpu::Device,
    /// Its queue.
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// Opens a device on the adapter `options` asks for.
    ///
    /// # Errors
    ///
    /// [`OpenError`] when there is no such adapter or it gives no device.
    pub async fn open(options: GpuOptions) -> Result<Self, OpenError> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: options.backends,
            ..Default::default()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: options.fallback_adapter,
                compatible_surface: None,
            })
            .await
            .map_err(|e| OpenError(e.to_string()))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("takt"),
                required_features: wanted_features(adapter.features()),
                ..Default::default()
            })
            .await
            .map_err(|e| OpenError(e.to_string()))?;
        Ok(Self {
            instance,
            adapter,
            device,
            queue,
        })
    }

    /// What a decoder needs to know about this device.
    #[must_use]
    pub fn info(&self) -> DeviceInfo {
        DeviceInfo::of(&self.adapter, &self.device)
    }
}

/// What a decoder is told about the device that will draw its frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The backend of the device.
    pub backend: wgpu::Backend,
    /// The name of the adapter, for a line in a log.
    pub adapter: String,
    /// The LUID of the adapter under Windows, as its eight bytes (low part
    /// first): a decoder opens its D3D11 device on this adapter, so that its
    /// textures can be shared. `None` on other backends than DX12.
    pub adapter_luid: Option<[u8; 8]>,
    /// Whether an imported NV12 texture can be drawn.
    pub nv12: bool,
    /// Whether an imported P010 texture can be drawn.
    pub p010: bool,
    /// Whether 10-bit frames from system memory can be drawn.
    pub sixteen_bit: bool,
}

impl DeviceInfo {
    /// The information about `device`, which was opened on `adapter`.
    #[must_use]
    pub fn of(adapter: &wgpu::Adapter, device: &wgpu::Device) -> Self {
        let info = adapter.get_info();
        let features = device.features();
        #[cfg(windows)]
        let adapter_luid = crate::interop::dx12::adapter_luid(device);
        #[cfg(not(windows))]
        let adapter_luid = None;
        Self {
            backend: info.backend,
            adapter: info.name,
            adapter_luid,
            nv12: features.contains(wgpu::Features::TEXTURE_FORMAT_NV12),
            p010: features.contains(wgpu::Features::TEXTURE_FORMAT_P010),
            sixteen_bit: features.contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_features_the_adapter_has_are_asked_for() {
        assert_eq!(
            wanted_features(wgpu::Features::empty()),
            wgpu::Features::empty()
        );
        let some = wgpu::Features::TEXTURE_FORMAT_NV12 | wgpu::Features::DEPTH_CLIP_CONTROL;
        assert_eq!(wanted_features(some), wgpu::Features::TEXTURE_FORMAT_NV12);
        assert_eq!(wanted_features(wgpu::Features::all()), WANTED_FEATURES);
    }

    #[test]
    fn video_is_drawn_on_dx12_under_windows() {
        let backends = preferred_backends();
        if cfg!(windows) {
            assert_eq!(backends, wgpu::Backends::DX12);
        } else {
            assert!(backends.contains(wgpu::Backends::VULKAN));
        }
        assert_eq!(GpuOptions::default().backends, backends);
    }
}
