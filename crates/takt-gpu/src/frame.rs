// SPDX-License-Identifier: GPL-3.0-or-later
//! [`GpuFrame`]: what a decoder hands to the GPU. The seam between
//! `takt-decode` and this crate.
//!
//! A frame is a handle to a texture in video memory
//! ([`GpuFrame::Texture`]), written by a hardware decoder and imported
//! without a copy through system memory (the module `interop`, Windows). The
//! planes in
//! system memory ([`GpuFrame::Cpu`]) are the fallback: a software decoder,
//! the finished picture of libmpv, a machine without a hardware path. Both
//! end in the same shader and the same bind group layout.

use std::any::Any;
use std::fmt;

use takt_core::{PixelFormat, PooledFrame};

use crate::colour::FrameColor;
use crate::layout::{LayoutError, PlanesView};

/// The planar formats a hardware decoder writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlanarFormat {
    /// 8 bit, luma and interleaved chroma of half size.
    Nv12,
    /// 10 bit in the high bits of 16-bit words, else as NV12.
    P010,
}

impl PlanarFormat {
    /// The format of the texture as a whole.
    #[must_use]
    pub const fn texture_format(self) -> wgpu::TextureFormat {
        match self {
            Self::Nv12 => wgpu::TextureFormat::NV12,
            Self::P010 => wgpu::TextureFormat::P010,
        }
    }

    /// The formats of the views of the luma and of the chroma plane.
    #[must_use]
    pub const fn plane_formats(self) -> (wgpu::TextureFormat, wgpu::TextureFormat) {
        match self {
            Self::Nv12 => (wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm),
            Self::P010 => (
                wgpu::TextureFormat::R16Unorm,
                wgpu::TextureFormat::Rg16Unorm,
            ),
        }
    }

    /// The same layout as a pixel format of `takt-core`.
    #[must_use]
    pub const fn pixel_format(self) -> PixelFormat {
        match self {
            Self::Nv12 => PixelFormat::Nv12,
            Self::P010 => PixelFormat::P010,
        }
    }

    /// The feature a device needs for textures of this format.
    #[must_use]
    pub const fn feature(self) -> wgpu::Features {
        match self {
            Self::Nv12 => wgpu::Features::TEXTURE_FORMAT_NV12,
            Self::P010 => wgpu::Features::TEXTURE_FORMAT_P010,
        }
    }
}

/// When the texture of a frame holds its picture.
#[derive(Debug, Default)]
pub enum Ready {
    /// It does already.
    #[default]
    Now,
    /// When the fence reaches the value: the device that writes the picture
    /// signals it, and the queue of wgpu waits for it on the GPU, without
    /// blocking a thread.
    #[cfg(windows)]
    Fence {
        /// The fence, opened on the device of wgpu.
        fence: crate::interop::dx12::Fence,
        /// The value it has when the picture is complete.
        value: u64,
    },
}

/// Why a texture is not a frame.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The texture has another format than the frame names.
    Format,
    /// The texture cannot be bound for sampling, is not two-dimensional, has
    /// mip levels or is multisampled.
    Shape,
    /// The visible size is empty or larger than the texture.
    Visible,
    /// The array layer is not one of the texture.
    Layer,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Format => "the texture has another format than the frame names",
            Self::Shape => "the texture is not a plain 2D texture that can be sampled",
            Self::Visible => "the visible size is empty or larger than the texture",
            Self::Layer => "the texture has no such array layer",
        })
    }
}

impl std::error::Error for FrameError {}

/// A picture in a texture a decoder wrote.
///
/// The texture may be larger than the picture (the coded size, a slice of an
/// array): [`TextureFrame::visible`] is what is shown.
pub struct TextureFrame {
    texture: wgpu::Texture,
    layer: u32,
    format: PlanarFormat,
    visible: (u32, u32),
    color: FrameColor,
    ready: Ready,
    /// Dropped with the frame: the place of the picture in the pool of its
    /// owner.
    guard: Option<Box<dyn Any + Send + Sync>>,
}

impl fmt::Debug for TextureFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextureFrame")
            .field("format", &self.format)
            .field("visible", &self.visible)
            .field("layer", &self.layer)
            .field("color", &self.color)
            .field("ready", &self.ready)
            .field("guard", &self.guard.is_some())
            .finish_non_exhaustive()
    }
}

impl TextureFrame {
    /// The picture of `visible` width and height in layer 0 of `texture`,
    /// complete already.
    ///
    /// # Errors
    ///
    /// [`FrameError`] when the texture is not what the frame says: every
    /// number here may come from another process.
    pub fn new(
        texture: wgpu::Texture,
        format: PlanarFormat,
        visible: (u32, u32),
        color: FrameColor,
    ) -> Result<Self, FrameError> {
        if texture.format() != format.texture_format() {
            return Err(FrameError::Format);
        }
        if texture.dimension() != wgpu::TextureDimension::D2
            || texture.mip_level_count() != 1
            || texture.sample_count() != 1
            || !texture
                .usage()
                .contains(wgpu::TextureUsages::TEXTURE_BINDING)
        {
            return Err(FrameError::Shape);
        }
        if visible.0 == 0
            || visible.1 == 0
            || visible.0 > texture.width()
            || visible.1 > texture.height()
        {
            return Err(FrameError::Visible);
        }
        Ok(Self {
            texture,
            layer: 0,
            format,
            visible,
            color,
            ready: Ready::Now,
            guard: None,
        })
    }

    /// The picture is in array layer `layer` (a decoder that keeps its
    /// pictures in one texture array).
    ///
    /// # Errors
    ///
    /// [`FrameError::Layer`] when the texture has no such layer.
    pub fn in_layer(mut self, layer: u32) -> Result<Self, FrameError> {
        if layer >= self.texture.depth_or_array_layers() {
            return Err(FrameError::Layer);
        }
        self.layer = layer;
        Ok(self)
    }

    /// The picture is complete only when `ready` says so.
    #[must_use]
    pub fn ready_when(mut self, ready: Ready) -> Self {
        self.ready = ready;
        self
    }

    /// `guard` is dropped when the frame is: it gives the picture back to
    /// its owner (the pool of the decoder, the ring of shared textures).
    ///
    /// A layer drops a frame only after the GPU is done with it and the
    /// texture is back in the state an imported texture arrives in
    /// ([`crate::LayerSlot::before_pass`]).
    #[must_use]
    pub fn returned_by(mut self, guard: impl Any + Send + Sync) -> Self {
        self.guard = Some(Box::new(guard));
        self
    }

    /// The texture.
    #[must_use]
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// The array layer the picture is in.
    #[must_use]
    pub fn layer(&self) -> u32 {
        self.layer
    }

    /// The planar format.
    #[must_use]
    pub fn format(&self) -> PlanarFormat {
        self.format
    }

    /// Width and height of the picture.
    #[must_use]
    pub fn visible(&self) -> (u32, u32) {
        self.visible
    }

    /// How the samples are to be read as colour.
    #[must_use]
    pub fn color(&self) -> FrameColor {
        self.color
    }

    /// When the texture holds the picture.
    #[must_use]
    pub fn ready(&self) -> &Ready {
        &self.ready
    }
}

/// A frame on its way to the screen.
#[derive(Debug)]
pub enum GpuFrame {
    /// A texture in video memory, written by a decoder.
    Texture(TextureFrame),
    /// Planes in system memory, uploaded when shown: the fallback. The
    /// pooled frame itself, not a copy; it goes back to its pool when the
    /// last holder lets go.
    Cpu(PooledFrame),
}

impl GpuFrame {
    /// Takes a frame in system memory.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] for incomplete planes and [`LayoutError::Format`] for
    /// a format no layer takes (it is not converted to one that is). The
    /// frame goes back to its pool then.
    pub fn from_frame(frame: PooledFrame) -> Result<Self, LayoutError> {
        // The layout is checked once, here; the upload reads the same planes.
        PlanesView::from_frame(frame.get())?;
        Ok(Self::Cpu(frame))
    }

    /// Width and height of the picture in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Texture(t) => t.visible,
            Self::Cpu(frame) => {
                let meta = frame.get().meta();
                (meta.width, meta.height)
            }
        }
    }

    /// The layout of the samples.
    #[must_use]
    pub fn format(&self) -> PixelFormat {
        match self {
            Self::Texture(t) => t.format.pixel_format(),
            Self::Cpu(frame) => frame.get().meta().format,
        }
    }

    /// How the samples are to be read as colour.
    #[must_use]
    pub fn color(&self) -> FrameColor {
        match self {
            Self::Texture(t) => t.color,
            Self::Cpu(frame) => frame.get().meta().color().into(),
        }
    }

    /// Whether the picture is in video memory already.
    #[must_use]
    pub fn is_texture(&self) -> bool {
        matches!(self, Self::Texture(_))
    }
}

impl From<TextureFrame> for GpuFrame {
    fn from(frame: TextureFrame) -> Self {
        Self::Texture(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use takt_core::{
        ColorPrimaries, ColorRange, FrameMeta, FramePool, Timestamp, TransferFunction,
    };

    fn pool(format: PixelFormat, capacity: usize) -> FramePool {
        FramePool::new(
            FrameMeta {
                width: 8,
                height: 4,
                format,
                primaries: ColorPrimaries::Bt2020,
                transfer: TransferFunction::Pq,
                range: ColorRange::Limited,
                pts: Timestamp::from_millis(0),
            },
            capacity,
        )
    }

    #[test]
    fn a_cpu_frame_is_the_pooled_frame_itself_and_goes_back_when_dropped() {
        let stamp = Timestamp::from_millis;
        let mut pool = pool(PixelFormat::P010, 2);
        let frame = pool.acquire(stamp(0)).unwrap();
        let rows = frame.get().plane(0).unwrap().as_ptr();
        let gpu = GpuFrame::from_frame(frame).unwrap();
        assert_eq!(gpu.size(), (8, 4));
        assert_eq!(gpu.format(), PixelFormat::P010);
        assert!(!gpu.is_texture());
        // The colour is the one of the frame, with the matrix of its primaries.
        let color = gpu.color();
        assert_eq!(color.description.transfer, TransferFunction::Pq);
        assert_eq!(color.matrix, crate::MatrixCoefficients::Bt2020Ncl);
        let GpuFrame::Cpu(inner) = &gpu else {
            unreachable!()
        };
        assert_eq!(inner.get().plane(0).unwrap().as_ptr(), rows, "no copy");
        assert_eq!(pool.in_flight(), 1);
        drop(gpu);
        assert_eq!(pool.in_flight(), 0);
    }

    #[test]
    fn a_format_without_a_path_is_refused_and_the_frame_goes_back() {
        let mut pool = pool(PixelFormat::Rgba16F, 1);
        let frame = pool.acquire(Timestamp::from_millis(0)).unwrap();
        assert_eq!(
            GpuFrame::from_frame(frame).map(|f| f.size()),
            Err(LayoutError::Format)
        );
        assert_eq!(pool.in_flight(), 0);
    }

    #[test]
    fn the_planar_formats_name_their_views_and_features() {
        use wgpu::TextureFormat as F;
        assert_eq!(PlanarFormat::Nv12.texture_format(), F::NV12);
        assert_eq!(
            PlanarFormat::Nv12.plane_formats(),
            (F::R8Unorm, F::Rg8Unorm)
        );
        assert_eq!(
            PlanarFormat::P010.plane_formats(),
            (F::R16Unorm, F::Rg16Unorm)
        );
        assert_eq!(PlanarFormat::P010.pixel_format(), PixelFormat::P010);
        assert_eq!(
            PlanarFormat::P010.feature(),
            wgpu::Features::TEXTURE_FORMAT_P010
        );
    }
}
