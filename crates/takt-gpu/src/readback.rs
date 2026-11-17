// SPDX-License-Identifier: GPL-3.0-or-later
//! A target read back into system memory, and a checksum of it: how two
//! paths to the same picture are compared (the import of a texture against
//! the upload of its planes), in the tests and in a running program.
//!
//! This blocks until the GPU is done and copies the whole target; it is for
//! checks, not for the path of a frame.

use std::fmt;
use std::sync::Arc;
use std::sync::mpsc;

use crate::frame::GpuFrame;
use crate::layer::{LayerRenderer, ShowError};

/// Why a target could not be read back.
#[derive(Debug)]
pub enum ReadError {
    /// The texture has no `COPY_SRC` usage or not four bytes a pixel.
    Texture,
    /// The device was lost or the buffer could not be mapped.
    Device(String),
    /// The frame to check could not be shown.
    Show(ShowError),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Texture => write!(f, "the texture cannot be read back as four bytes a pixel"),
            Self::Device(why) => write!(f, "reading back failed: {why}"),
            Self::Show(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ReadError {}

/// A target for [`read`]: `width` by `height`, drawn into and copied from.
#[must_use]
pub fn target(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    size: (u32, u32),
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("takt target"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

/// The bytes of `texture` (a format of four bytes a pixel), row after row
/// without padding. Waits for everything submitted so far.
///
/// # Errors
///
/// [`ReadError`].
pub fn read(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
) -> Result<Vec<u8>, ReadError> {
    let four_bytes = texture.format().block_copy_size(None) == Some(4);
    if !four_bytes || !texture.usage().contains(wgpu::TextureUsages::COPY_SRC) {
        return Err(ReadError::Texture);
    }
    let (w, h) = (texture.width(), texture.height());
    let row = w * 4;
    let padded = row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("takt readback"),
        size: u64::from(padded) * u64::from(h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([encoder.finish()]);
    let (sender, receiver) = mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            // The receiver is gone only when this function returned early.
            let _ = sender.send(result);
        });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| ReadError::Device(e.to_string()))?;
    receiver
        .recv()
        .map_err(|e| ReadError::Device(e.to_string()))?
        .map_err(|e| ReadError::Device(e.to_string()))?;
    let data = buffer.slice(..).get_mapped_range();
    let mut out = Vec::with_capacity(row as usize * h as usize);
    for line in data.chunks(padded as usize) {
        out.extend_from_slice(line.get(..row as usize).unwrap_or(line));
    }
    Ok(out)
}

/// Draws `frame` at its own size, one texel a pixel, into a target of its
/// own and returns the checksum of what came out: two frames that hold the
/// same picture give the same number, whichever way they reached the GPU.
///
/// # Errors
///
/// [`ReadError`] when the frame cannot be shown or the target not read.
pub fn frame_checksum(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    frame: &Arc<GpuFrame>,
) -> Result<u64, ReadError> {
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let renderer = LayerRenderer::new(device, format, wgpu::FilterMode::Nearest);
    let mut slot = renderer.slot(device);
    slot.show(device, queue, frame).map_err(ReadError::Show)?;
    let texture = target(device, format, frame.size());
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("takt frame checksum"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations::default(),
            })],
            ..Default::default()
        });
        slot.draw(&mut pass);
    }
    queue.submit([encoder.finish()]);
    Ok(checksum(&read(device, queue, &texture)?))
}

/// FNV-1a over `bytes`: one number for a picture, so two of them can be
/// compared and named in a line of text.
#[must_use]
pub fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checksum_is_fnv_1a() {
        // The test vectors of the FNV reference.
        assert_eq!(checksum(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(checksum(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(checksum(b"foobar"), 0x8594_4171_f739_67e8);
        assert_ne!(checksum(&[0, 1]), checksum(&[1, 0]));
    }
}
