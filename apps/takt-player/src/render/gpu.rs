// SPDX-License-Identifier: GPL-3.0-or-later
//! NV12 frames on the GPU: two textures (Y as `R8Unorm`, the interleaved UV
//! of half size as `Rg8Unorm`) and the shader `nv12.wgsl`.
//!
//! The upload is a CPU path on purpose (Phase 0; zero copy from the decoder
//! is problem 1 of Phase 1 and this path stays its fallback). The rows go to
//! `write_texture` with their stride as `bytes_per_row`, so a padded plane is
//! uploaded as it is, without repacking. The time of each upload is counted.

use std::fmt;
use std::time::Instant;

use egui_wgpu::wgpu;
use takt_audio::{DurationHistogram, DurationSnapshot};
use takt_core::{PixelFormat, VideoFrame};

/// Why a set of planes is not an NV12 picture.
#[derive(Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// Width or height is zero.
    Empty,
    /// The stride of a plane is shorter than its rows.
    Stride {
        /// 0 for Y, 1 for UV.
        plane: usize,
    },
    /// A plane holds fewer bytes than its rows need.
    Short {
        /// 0 for Y, 1 for UV.
        plane: usize,
    },
    /// The frame is not NV12.
    Format,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty picture"),
            Self::Stride { plane } => write!(f, "plane {plane}: stride shorter than a row"),
            Self::Short { plane } => write!(f, "plane {plane}: fewer bytes than its rows"),
            Self::Format => write!(f, "not NV12"),
        }
    }
}

/// The planes of one NV12 picture, checked to be complete.
#[derive(Clone, Copy, Debug)]
pub struct Nv12View<'a> {
    width: u32,
    height: u32,
    y: &'a [u8],
    y_stride: u32,
    uv: &'a [u8],
    uv_stride: u32,
}

/// Bytes a plane of `rows` rows of `row` bytes at `stride` needs: the last
/// row needs no padding after it.
fn needed(stride: usize, row: usize, rows: usize) -> usize {
    stride * (rows - 1) + row
}

impl<'a> Nv12View<'a> {
    /// The planes of a `width` by `height` picture: Y with `y_stride` bytes
    /// per row, UV (interleaved, half width and height, rounded up) with
    /// `uv_stride`.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] when the sizes do not hold the picture.
    pub fn new(
        width: u32,
        height: u32,
        y: &'a [u8],
        y_stride: u32,
        uv: &'a [u8],
        uv_stride: u32,
    ) -> Result<Self, LayoutError> {
        if width == 0 || height == 0 {
            return Err(LayoutError::Empty);
        }
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        if (y_stride as usize) < w {
            return Err(LayoutError::Stride { plane: 0 });
        }
        if (uv_stride as usize) < cw * 2 {
            return Err(LayoutError::Stride { plane: 1 });
        }
        if y.len() < needed(y_stride as usize, w, h) {
            return Err(LayoutError::Short { plane: 0 });
        }
        if uv.len() < needed(uv_stride as usize, cw * 2, ch) {
            return Err(LayoutError::Short { plane: 1 });
        }
        Ok(Self {
            width,
            height,
            y,
            y_stride,
            uv,
            uv_stride,
        })
    }

    /// The planes of a decoded NV12 frame.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] for another format or incomplete planes.
    pub fn from_frame(frame: &'a VideoFrame) -> Result<Self, LayoutError> {
        let meta = frame.meta();
        if meta.format != PixelFormat::Nv12 {
            return Err(LayoutError::Format);
        }
        let stride = |plane| {
            frame
                .stride(plane)
                .and_then(|s| u32::try_from(s).ok())
                .ok_or(LayoutError::Short { plane })
        };
        Self::new(
            meta.width,
            meta.height,
            frame.plane(0).ok_or(LayoutError::Short { plane: 0 })?,
            stride(0)?,
            frame.plane(1).ok_or(LayoutError::Short { plane: 1 })?,
            stride(1)?,
        )
    }

    /// Width and height in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// An NV12 picture with planes of its own: what the paint callback carries
/// from the UI thread to the render pass (it must be `Send + Sync` and live on
/// its own).
#[derive(Debug)]
pub struct Nv12Image {
    width: u32,
    height: u32,
    y: Vec<u8>,
    y_stride: u32,
    uv: Vec<u8>,
    uv_stride: u32,
}

impl Nv12Image {
    /// A copy of the planes of `frame`.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] for another format or incomplete planes.
    pub fn from_frame(frame: &VideoFrame) -> Result<Self, LayoutError> {
        let view = Nv12View::from_frame(frame)?;
        Ok(Self {
            width: view.width,
            height: view.height,
            y: view.y.to_vec(),
            y_stride: view.y_stride,
            uv: view.uv.to_vec(),
            uv_stride: view.uv_stride,
        })
    }

    /// The planes as a view.
    #[must_use]
    pub fn view(&self) -> Nv12View<'_> {
        Nv12View {
            width: self.width,
            height: self.height,
            y: &self.y,
            y_stride: self.y_stride,
            uv: &self.uv,
            uv_stride: self.uv_stride,
        }
    }
}

struct Textures {
    size: (u32, u32),
    luma: wgpu::Texture,
    chroma: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// Draws NV12 pictures with the shader `nv12.wgsl`.
pub struct Nv12Renderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    textures: Option<Textures>,
    uploads: DurationHistogram,
}

fn texture(
    device: &wgpu::Device,
    label: &str,
    size: (u32, u32),
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    data: &[u8],
    stride: u32,
    size: (u32, u32),
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(stride),
            rows_per_image: Some(size.1),
        },
        wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
    );
}

impl Nv12Renderer {
    /// A renderer for a target of `target_format`. `filter` is `Linear` on
    /// screen (scaling, chroma upsampling) and `Nearest` where every pixel
    /// must be one texel (the comparison with the CPU reference).
    #[must_use]
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        filter: wgpu::FilterMode,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12"),
            source: wgpu::ShaderSource::Wgsl(include_str!("nv12.wgsl").into()),
        });
        let sampled = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12"),
            entries: &[
                sampled(0),
                sampled(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let srgb = if target_format.is_srgb() { 1.0 } else { 0.0 };
        let constants = [("srgb_target", srgb)];
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12"),
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
            label: Some("nv12"),
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

    /// Uploads `picture`, with new textures when its size changed.
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, picture: &Nv12View<'_>) {
        let start = Instant::now();
        let size = picture.size();
        let chroma_size = (size.0.div_ceil(2), size.1.div_ceil(2));
        if self.textures.as_ref().is_none_or(|t| t.size != size) {
            let luma = texture(device, "nv12 luma", size, wgpu::TextureFormat::R8Unorm);
            let chroma = texture(
                device,
                "nv12 chroma",
                chroma_size,
                wgpu::TextureFormat::Rg8Unorm,
            );
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("nv12"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(
                            &luma.create_view(&Default::default()),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(
                            &chroma.create_view(&Default::default()),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.textures = Some(Textures {
                size,
                luma,
                chroma,
                bind_group,
            });
        }
        if let Some(t) = &self.textures {
            write_plane(queue, &t.luma, picture.y, picture.y_stride, size);
            write_plane(queue, &t.chroma, picture.uv, picture.uv_stride, chroma_size);
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

    /// The time of the uploads so far (CPU side: the copy into the staging
    /// memory of wgpu, not the transfer on the GPU).
    #[must_use]
    pub fn uploads(&self) -> DurationSnapshot {
        self.uploads.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::colour::bt709_limited_8bit;

    #[test]
    fn a_view_checks_strides_and_lengths() {
        let y = vec![0_u8; 16 * 4];
        let uv = vec![0_u8; 8 * 2];
        assert!(
            Nv12View::new(4, 4, &y, 16, &uv, 8).is_ok(),
            "padded rows are fine"
        );
        assert_eq!(
            Nv12View::new(0, 4, &y, 4, &uv, 4).unwrap_err(),
            LayoutError::Empty
        );
        assert_eq!(
            Nv12View::new(4, 4, &y, 3, &uv, 4).unwrap_err(),
            LayoutError::Stride { plane: 0 }
        );
        assert_eq!(
            Nv12View::new(4, 4, &y, 4, &uv, 3).unwrap_err(),
            LayoutError::Stride { plane: 1 }
        );
        assert_eq!(
            Nv12View::new(4, 4, &y[..10], 4, &uv, 4).unwrap_err(),
            LayoutError::Short { plane: 0 }
        );
        // Odd sizes round the chroma up: 5x3 needs 3x2 UV pairs.
        let uv_small = vec![0_u8; 6 + 5];
        assert_eq!(
            Nv12View::new(5, 3, &y, 5, &uv_small, 6).unwrap_err(),
            LayoutError::Short { plane: 1 }
        );
        // The last row needs no padding after it.
        let y_tight = vec![0_u8; 16 * 3 + 4];
        assert!(Nv12View::new(4, 4, &y_tight, 16, &uv, 4).is_ok());
    }

    /// A test picture: the seven 75 % colour bars (two pixels each, so each
    /// bar has its own chroma) above a grey wedge of eight steps, in planes
    /// with `pad` bytes of 255 after every row.
    fn bars(pad: u32) -> Picture {
        const BARS: [(u8, u8, u8); 8] = [
            (180, 128, 128),
            (168, 44, 136),
            (145, 147, 44),
            (133, 63, 52),
            (63, 193, 204),
            (51, 109, 212),
            (28, 212, 120),
            (16, 128, 128),
        ];
        let (w, h) = (16_u32, 8_u32);
        let (ys, uvs) = (w + pad, w + pad);
        let mut y = vec![255_u8; (ys * h) as usize];
        let mut uv = vec![255_u8; (uvs * h / 2) as usize];
        for row in 0..h {
            for col in 0..w {
                let value = if row < h / 2 {
                    BARS[(col / 2) as usize].0
                } else {
                    16 + u8::try_from(col / 2).unwrap() * 31 // 16 .. 233
                };
                y[(row * ys + col) as usize] = value;
            }
        }
        for row in 0..h / 2 {
            for pair in 0..w / 2 {
                let (u, v) = if row < h / 4 {
                    let bar = BARS[pair as usize];
                    (bar.1, bar.2)
                } else {
                    (128, 128)
                };
                uv[(row * uvs + pair * 2) as usize] = u;
                uv[(row * uvs + pair * 2 + 1) as usize] = v;
            }
        }
        (w, h, y, ys, uv, uvs)
    }

    type Picture = (u32, u32, Vec<u8>, u32, Vec<u8>, u32);

    fn view_of(p: &Picture) -> Nv12View<'_> {
        Nv12View::new(p.0, p.1, &p.2, p.3, &p.4, p.5).unwrap()
    }

    /// The adapter of the test. `TAKT_GPU_ADAPTER=fallback` asks for the software adapter (WARP of
    /// DX12 on Windows): the CI runners have no GPU, and WARP renders the shader exactly enough.
    fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::default();
        let options = wgpu::RequestAdapterOptions {
            force_fallback_adapter: std::env::var("TAKT_GPU_ADAPTER").as_deref() == Ok("fallback"),
            ..Default::default()
        };
        let adapter = pollster::block_on(instance.request_adapter(&options)).ok()?;
        println!("gpu test on {:?}", adapter.get_info().name);
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
    }

    /// Renders `picture` at its own size into an `Rgba8Unorm` target and
    /// reads it back, four bytes per pixel.
    fn render(device: &wgpu::Device, queue: &wgpu::Queue, picture: &Nv12View<'_>) -> Vec<u8> {
        let (w, h) = picture.size();
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut renderer = Nv12Renderer::new(device, format, wgpu::FilterMode::Nearest);
        renderer.upload(device, queue, picture);
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(row * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nv12 test"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::RED),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            renderer.draw(&mut pass);
        }
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
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
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            let start = (y * row) as usize;
            out.extend_from_slice(&data[start..start + (w * 4) as usize]);
        }
        out
    }

    /// The CPU reference of every pixel (nearest chroma: pixel x, y uses the
    /// pair x/2, y/2).
    fn reference(picture: &Nv12View<'_>) -> Vec<[u8; 3]> {
        let (w, h) = picture.size();
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let luma = picture.y[(y * picture.y_stride + x) as usize];
                let at = ((y / 2) * picture.uv_stride + (x / 2) * 2) as usize;
                out.push(bt709_limited_8bit(luma, picture.uv[at], picture.uv[at + 1]));
            }
        }
        out
    }

    fn gpu_wanted() -> bool {
        if std::env::var("TAKT_GPU_TESTS").as_deref() == Ok("1") {
            return true;
        }
        println!("SKIP gpu test: set TAKT_GPU_TESTS=1 to render on a GPU adapter");
        false
    }

    #[test]
    #[ignore = "SKIP: renders on a GPU adapter; TAKT_GPU_TESTS=1 cargo test -p takt-player gpu -- --ignored (CI: Windows with WARP)"]
    fn the_shader_matches_the_cpu_reference_also_for_padded_rows() {
        if !gpu_wanted() {
            return;
        }
        let (device, queue) = gpu().expect("a GPU adapter");
        let tight = bars(0);
        let padded = bars(13);
        let (tight_view, padded_view) = (view_of(&tight), view_of(&padded));
        let want = reference(&tight_view);
        // Tolerance: 2 of 255 per channel, for the float path of the GPU
        // against the rounding of the CPU reference.
        for (name, picture) in [("tight", &tight_view), ("padded", &padded_view)] {
            let got = render(&device, &queue, picture);
            for (i, (pixel, want)) in got.chunks(4).zip(&want).enumerate() {
                for c in 0..3 {
                    assert!(
                        pixel[c].abs_diff(want[c]) <= 2,
                        "{name}: pixel {i} channel {c}: {} is not {}",
                        pixel[c],
                        want[c]
                    );
                }
            }
        }
    }
}
