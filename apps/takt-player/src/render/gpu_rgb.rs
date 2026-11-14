// SPDX-License-Identifier: GPL-3.0-or-later
//! `rgb0` frames on the GPU: one `Rgba8Unorm` texture and the shader
//! `rgb0.wgsl`. The second texture path next to NV12 ([`super::gpu`]).
//!
//! This is the picture of the `MpvBackend`: the software renderer of mpv
//! hands out R'G'B' with the subtitles drawn in, and it goes to the GPU as it
//! is. It is not turned into NV12 on the way: that would be a second
//! conversion on the CPU and would halve the colour resolution of exactly the
//! parts that show it, the edges of subtitles and of drawn pictures.
//!
//! The fourth byte of a pixel is padding ([`takt_core::PixelFormat::Rgb0`]).
//! The texture has an alpha channel all the same, because no GPU has a
//! three-byte format; the shader never reads it.

use std::time::Instant;

use egui_wgpu::wgpu;
use takt_audio::{DurationHistogram, DurationSnapshot};
use takt_core::{PixelFormat, VideoFrame};

use super::gpu::{LayoutError, needed, texture, write_plane};

/// The rows of one `rgb0` picture, checked to be complete.
#[derive(Clone, Copy, Debug)]
pub struct Rgb0View<'a> {
    width: u32,
    height: u32,
    pixels: &'a [u8],
    stride: u32,
}

impl<'a> Rgb0View<'a> {
    /// The rows of a `width` by `height` picture, four bytes a pixel, with
    /// `stride` bytes from one row to the next.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] when the sizes do not hold the picture.
    pub fn new(
        width: u32,
        height: u32,
        pixels: &'a [u8],
        stride: u32,
    ) -> Result<Self, LayoutError> {
        if width == 0 || height == 0 {
            return Err(LayoutError::Empty);
        }
        let (w, h) = (width as usize, height as usize);
        let row = w.checked_mul(4).ok_or(LayoutError::Stride { plane: 0 })?;
        if (stride as usize) < row {
            return Err(LayoutError::Stride { plane: 0 });
        }
        if pixels.len() < needed(stride as usize, row, h) {
            return Err(LayoutError::Short { plane: 0 });
        }
        Ok(Self {
            width,
            height,
            pixels,
            stride,
        })
    }

    /// The rows of an `rgb0` frame.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] for another format or an incomplete plane.
    pub fn from_frame(frame: &'a VideoFrame) -> Result<Self, LayoutError> {
        let meta = frame.meta();
        if meta.format != PixelFormat::Rgb0 {
            return Err(LayoutError::Format);
        }
        let stride = frame
            .stride(0)
            .and_then(|s| u32::try_from(s).ok())
            .ok_or(LayoutError::Short { plane: 0 })?;
        Self::new(
            meta.width,
            meta.height,
            frame.plane(0).ok_or(LayoutError::Short { plane: 0 })?,
            stride,
        )
    }

    /// Width and height in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// An `rgb0` picture with rows of its own: what the paint callback carries
/// from the UI thread to the render pass.
#[derive(Debug)]
pub struct Rgb0Image {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    stride: u32,
}

impl Rgb0Image {
    /// A copy of the rows of `frame`.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] for another format or an incomplete plane.
    pub fn from_frame(frame: &VideoFrame) -> Result<Self, LayoutError> {
        let view = Rgb0View::from_frame(frame)?;
        Ok(Self {
            width: view.width,
            height: view.height,
            pixels: view.pixels.to_vec(),
            stride: view.stride,
        })
    }

    /// The rows as a view.
    #[must_use]
    pub fn view(&self) -> Rgb0View<'_> {
        Rgb0View {
            width: self.width,
            height: self.height,
            pixels: &self.pixels,
            stride: self.stride,
        }
    }
}

struct Textures {
    size: (u32, u32),
    picture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// Draws `rgb0` pictures with the shader `rgb0.wgsl`.
pub struct Rgb0Renderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    textures: Option<Textures>,
    uploads: DurationHistogram,
}

impl Rgb0Renderer {
    /// A renderer for a target of `target_format`; `filter` as for
    /// [`super::gpu::Nv12Renderer::new`].
    #[must_use]
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        filter: wgpu::FilterMode,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rgb0"),
            source: wgpu::ShaderSource::Wgsl(include_str!("rgb0.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rgb0"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rgb0"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let srgb = if target_format.is_srgb() { 1.0 } else { 0.0 };
        let constants = [("srgb_target", srgb)];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rgb0"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions {
                    constants: &constants,
                    ..Default::default()
                },
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rgb0"),
            mag_filter: filter,
            min_filter: filter,
            ..Default::default()
        });
        Self {
            pipeline,
            layout,
            sampler,
            textures: None,
            uploads: DurationHistogram::default(),
        }
    }

    /// Uploads `picture`, with a new texture when its size changed.
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, picture: &Rgb0View<'_>) {
        let start = Instant::now();
        let size = picture.size();
        if self.textures.as_ref().is_none_or(|t| t.size != size) {
            // Not an sRGB format: the bytes are sampled as they are, and the
            // shader decides what the target needs.
            let texture = texture(device, "rgb0", size, wgpu::TextureFormat::Rgba8Unorm);
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rgb0"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(
                            &texture.create_view(&Default::default()),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.textures = Some(Textures {
                size,
                picture: texture,
                bind_group,
            });
        }
        if let Some(t) = &self.textures {
            write_plane(queue, &t.picture, picture.pixels, picture.stride, size);
        }
        let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.uploads.record(nanos);
    }

    /// Draws the last uploaded picture over the whole viewport of `pass`;
    /// nothing before the first upload.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(t) = &self.textures {
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &t.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
    }

    /// The time of the uploads so far (CPU side, as for NV12).
    #[must_use]
    pub fn uploads(&self) -> DurationSnapshot {
        self.uploads.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::gpu::testing;
    use takt_core::{ColorPrimaries, ColorRange, FrameMeta, Timestamp, TransferFunction};

    #[test]
    fn a_view_checks_stride_and_length() {
        let pixels = vec![0_u8; 64 * 3];
        assert!(
            Rgb0View::new(4, 3, &pixels, 64).is_ok(),
            "padded rows are fine"
        );
        assert_eq!(
            Rgb0View::new(0, 3, &pixels, 16).unwrap_err(),
            LayoutError::Empty
        );
        assert_eq!(
            Rgb0View::new(4, 3, &pixels, 15).unwrap_err(),
            LayoutError::Stride { plane: 0 }
        );
        assert_eq!(
            Rgb0View::new(4, 3, &pixels[..64 * 2 + 15], 64).unwrap_err(),
            LayoutError::Short { plane: 0 }
        );
        // The last row needs no padding after it.
        assert!(Rgb0View::new(4, 3, &pixels[..64 * 2 + 16], 64).is_ok());
    }

    fn frame(format: PixelFormat) -> VideoFrame {
        VideoFrame::alloc(FrameMeta {
            width: 6,
            height: 2,
            format,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt709,
            range: ColorRange::Full,
            pts: Timestamp::from_millis(0),
        })
        .unwrap()
    }

    #[test]
    fn an_image_copies_an_rgb0_frame_and_refuses_another_format() {
        let mut rgb = frame(PixelFormat::Rgb0);
        rgb.plane_mut(0).unwrap()[..4].copy_from_slice(&[1, 2, 3, 0]);
        let image = Rgb0Image::from_frame(&rgb).unwrap();
        assert_eq!(image.view().size(), (6, 2));
        assert_eq!(&image.view().pixels[..4], &[1, 2, 3, 0]);
        assert_eq!(image.view().stride, 24);
        // The same bytes with another meaning are not taken by guessing.
        assert_eq!(
            Rgb0Image::from_frame(&frame(PixelFormat::Rgba8)).unwrap_err(),
            LayoutError::Format
        );
        assert_eq!(
            Rgb0Image::from_frame(&frame(PixelFormat::Nv12)).unwrap_err(),
            LayoutError::Format
        );
    }

    /// Eight colours in two rows, in rows of `stride` bytes; the padding
    /// byte of every pixel and the bytes after every row hold `filler`.
    fn colours(stride: u32, filler: u8) -> (Vec<u8>, Vec<[u8; 3]>) {
        const COLOURS: [[u8; 3]; 8] = [
            [0, 0, 0],
            [255, 255, 255],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [191, 191, 0],
            [16, 128, 235],
            [1, 2, 3],
        ];
        let mut pixels = vec![filler; (stride * 2) as usize];
        for (i, colour) in COLOURS.iter().enumerate() {
            let at = (i / 4) * stride as usize + (i % 4) * 4;
            pixels[at..at + 3].copy_from_slice(colour);
        }
        (pixels, COLOURS.to_vec())
    }

    #[test]
    #[ignore = "SKIP: renders on a GPU adapter; TAKT_GPU_TESTS=1 cargo test -p takt-player gpu -- --ignored (CI: Windows with WARP)"]
    fn the_shader_shows_rgb0_as_it_is_and_opaque_whatever_the_padding_holds() {
        if !testing::gpu_wanted() {
            return;
        }
        let (device, queue) = testing::gpu().expect("a GPU adapter");
        // Tight rows with padding bytes of 0, and padded rows with 255: the
        // padding byte must not become transparency, the row padding not a
        // pixel.
        for (name, stride, filler) in [("tight", 16_u32, 0_u8), ("padded", 64, 255)] {
            let (pixels, want) = colours(stride, filler);
            let view = Rgb0View::new(4, 2, &pixels, stride).unwrap();
            let mut renderer =
                Rgb0Renderer::new(&device, testing::TARGET_FORMAT, wgpu::FilterMode::Nearest);
            renderer.upload(&device, &queue, &view);
            let got = testing::read_back(&device, &queue, view.size(), |pass| renderer.draw(pass));
            assert_eq!(got.len(), 4 * 2 * 4);
            for (i, (pixel, want)) in got.chunks(4).zip(&want).enumerate() {
                // No conversion on the way: at most one step of rounding.
                for c in 0..3 {
                    assert!(
                        pixel[c].abs_diff(want[c]) <= 1,
                        "{name}: pixel {i} channel {c}: {} is not {}",
                        pixel[c],
                        want[c]
                    );
                }
                assert_eq!(pixel[3], 255, "{name}: pixel {i} is opaque");
            }
            assert_eq!(renderer.uploads().total(), 1);
        }
    }
}
