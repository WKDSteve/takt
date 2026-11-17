// SPDX-License-Identifier: GPL-3.0-or-later
//! One layer on the GPU: a [`GpuFrame`] drawn with the shader `layer.wgsl`
//! into a rectangle of the target.
//!
//! [`LayerRenderer`] holds what all layers of one target format share (the
//! pipeline, the bind group layout, the sampler). A [`LayerSlot`] is one
//! layer: the textures of the picture it shows, its place and its opacity.
//!
//! A frame in system memory is uploaded when it is shown
//! ([`LayerSlot::show`]); the rows go to `write_texture` with their stride, so
//! a padded plane needs no repacking. A frame that is a texture already is
//! bound as it is: two views of its planes, no copy.

use std::fmt;
use std::sync::Arc;

use crate::colour::{Conversion, Samples, ToneMap};
use crate::frame::{GpuFrame, Ready, TextureFrame};
use crate::layout::{LayoutError, PlanesView};

/// Bytes of the uniform of `layer.wgsl`: ten `vec4`.
const UNIFORM_BYTES: usize = 160;

/// How many imported textures a slot keeps bound. The pool of a decoder is
/// small and fixed, so every texture is wrapped once; an entry holds the
/// texture alive, hence the bound.
const IMPORTED_MAX: usize = 16;

/// Where a layer is drawn and how opaque.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// In clip space: x and y of the top left corner, then width and height
    /// (the height runs downwards).
    pub rect: [f32; 4],
    /// 0 (invisible) to 1 (opaque).
    pub opacity: f32,
}

impl Placement {
    /// The whole viewport, opaque: what a player draws its one picture with.
    pub const FULL: Self = Self {
        rect: [-1.0, 1.0, 2.0, 2.0],
        opacity: 1.0,
    };

    /// The rectangle `x, y, width, height` in pixels of a target of `target`
    /// width and height, origin at the top left.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "the size of a target in pixels, far below 2^24"
    )]
    pub fn pixels(target: (u32, u32), rect: [f32; 4], opacity: f32) -> Self {
        let (w, h) = (target.0.max(1) as f32, target.1.max(1) as f32);
        Self {
            rect: [
                rect[0] / w * 2.0 - 1.0,
                1.0 - rect[1] / h * 2.0,
                rect[2] / w * 2.0,
                rect[3] / h * 2.0,
            ],
            opacity,
        }
    }
}

impl Default for Placement {
    fn default() -> Self {
        Self::FULL
    }
}

/// Why a frame could not be shown.
#[derive(Debug)]
pub enum ShowError {
    /// The planes of a frame in system memory do not hold its picture.
    Layout(LayoutError),
    /// The device lacks a feature the frame needs.
    Unsupported(&'static str),
    /// Waiting for the picture of an imported texture failed.
    #[cfg(windows)]
    Import(crate::interop::ImportError),
}

impl fmt::Display for ShowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Layout(e) => e.fmt(f),
            Self::Unsupported(what) => write!(f, "the device lacks {what}"),
            #[cfg(windows)]
            Self::Import(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ShowError {}

impl From<LayoutError> for ShowError {
    fn from(e: LayoutError) -> Self {
        Self::Layout(e)
    }
}

/// What every layer of one target format shares.
#[derive(Clone, Debug)]
pub struct LayerRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    srgb_target: bool,
}

impl LayerRenderer {
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
            label: Some("takt layer"),
            source: wgpu::ShaderSource::Wgsl(include_str!("layer.wgsl").into()),
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
            label: Some("takt layer"),
            entries: &[
                sampled(0),
                sampled(1),
                sampled(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(UNIFORM_BYTES as u64),
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("takt layer"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("takt layer"),
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
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // The shader hands out premultiplied colour; an opaque
                    // layer replaces what is below it.
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("takt layer"),
            mag_filter: filter,
            min_filter: filter,
            ..Default::default()
        });
        Self {
            pipeline,
            layout,
            sampler,
            srgb_target: target_format.is_srgb(),
        }
    }

    /// A layer of this renderer, showing nothing yet.
    #[must_use]
    pub fn slot(&self, device: &wgpu::Device) -> LayerSlot {
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("takt layer"),
            size: UNIFORM_BYTES as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        LayerSlot {
            renderer: self.clone(),
            uniform,
            placement: Placement::FULL,
            tone: ToneMap::default(),
            picture: None,
            uploaded: None,
            imported: Vec::new(),
            bound: None,
            current: None,
            replaced: Vec::new(),
            returning: Vec::new(),
        }
    }
}

/// What the uniform says about the picture, apart from its place.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PictureUniform {
    visible: (u32, u32),
    /// Whether the chroma planes have half the size.
    half_chroma: bool,
    samples: Samples,
    conversion: Conversion,
}

/// The textures a frame in system memory is uploaded to.
#[derive(Debug)]
struct Uploaded {
    format: takt_core::PixelFormat,
    size: (u32, u32),
    planes: Vec<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
}

/// An imported texture, bound.
#[derive(Debug)]
struct Imported {
    texture: wgpu::Texture,
    layer: u32,
    bind_group: wgpu::BindGroup,
}

/// One layer: the picture it shows, where, and how opaque.
#[derive(Debug)]
pub struct LayerSlot {
    renderer: LayerRenderer,
    uniform: wgpu::Buffer,
    placement: Placement,
    tone: ToneMap,
    picture: Option<PictureUniform>,
    uploaded: Option<Uploaded>,
    imported: Vec<Imported>,
    bound: Option<wgpu::BindGroup>,
    /// The texture frame shown now.
    current: Option<Arc<GpuFrame>>,
    /// Texture frames another picture took the place of since the last
    /// [`LayerSlot::before_pass`].
    replaced: Vec<Arc<GpuFrame>>,
    /// Texture frames whose way back was recorded by the last
    /// [`LayerSlot::before_pass`]; they are let go once that work is done.
    returning: Vec<Arc<GpuFrame>>,
}

#[expect(
    clippy::cast_precision_loss,
    reason = "the size of a picture in pixels, far below 2^24"
)]
fn float(value: u32) -> f32 {
    value as f32
}

impl LayerSlot {
    /// Shows `frame` from the next draw on.
    ///
    /// A frame in system memory is uploaded here, into new textures when its
    /// size or format changed; the slot does not keep it. A texture frame is
    /// bound without a copy and kept until another picture takes its place
    /// (see [`LayerSlot::before_pass`]); when it carries a fence, the queue is
    /// told to wait for it.
    ///
    /// # Errors
    ///
    /// [`ShowError`]; the layer keeps showing what it showed.
    pub fn show(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &Arc<GpuFrame>,
    ) -> Result<(), ShowError> {
        let conversion =
            Conversion::new(frame.format(), frame.color(), self.tone).ok_or(LayoutError::Format)?;
        let samples = Samples::of(frame.format()).ok_or(LayoutError::Format)?;
        match frame.as_ref() {
            GpuFrame::Cpu(pooled) => {
                let view = PlanesView::from_frame(pooled.get())?;
                self.upload(device, queue, &view)?;
                if let Some(old) = self.current.take() {
                    self.replaced.push(old);
                }
            }
            GpuFrame::Texture(texture) => {
                self.bind(device, queue, texture)?;
                let same = self.current.as_ref().is_some_and(|c| Arc::ptr_eq(c, frame));
                if !same && let Some(old) = self.current.replace(Arc::clone(frame)) {
                    self.replaced.push(old);
                }
            }
        }
        self.picture = Some(PictureUniform {
            visible: frame.size(),
            half_chroma: frame.format().is_subsampled_420(),
            samples,
            conversion,
        });
        self.write_uniform(queue);
        Ok(())
    }

    /// Uploads planes that are not a frame of a pool (a still picture, a
    /// test pattern), read as `color`.
    ///
    /// # Errors
    ///
    /// [`ShowError`]; the layer keeps showing what it showed.
    pub fn show_planes(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &PlanesView<'_>,
        color: crate::FrameColor,
    ) -> Result<(), ShowError> {
        let conversion =
            Conversion::new(view.format(), color, self.tone).ok_or(LayoutError::Format)?;
        let samples = Samples::of(view.format()).ok_or(LayoutError::Format)?;
        self.upload(device, queue, view)?;
        if let Some(old) = self.current.take() {
            self.replaced.push(old);
        }
        self.picture = Some(PictureUniform {
            visible: view.size(),
            half_chroma: view.format().is_subsampled_420(),
            samples,
            conversion,
        });
        self.write_uniform(queue);
        Ok(())
    }

    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &PlanesView<'_>,
    ) -> Result<(), ShowError> {
        let sixteen = view.planes().any(|p| {
            matches!(
                p.shape.format,
                wgpu::TextureFormat::R16Unorm | wgpu::TextureFormat::Rg16Unorm
            )
        });
        if sixteen
            && !device
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
        {
            return Err(ShowError::Unsupported(
                "16-bit normalised textures (TEXTURE_FORMAT_16BIT_NORM)",
            ));
        }
        let key = (view.format(), view.size());
        if self
            .uploaded
            .as_ref()
            .is_none_or(|u| (u.format, u.size) != key)
        {
            let planes: Vec<wgpu::Texture> = view
                .planes()
                .map(|plane| {
                    device.create_texture(&wgpu::TextureDescriptor {
                        label: Some("takt layer plane"),
                        size: wgpu::Extent3d {
                            width: plane.shape.width,
                            height: plane.shape.height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        // Never an sRGB format: the bytes are sampled as they
                        // are, and the shader decides what the target needs.
                        format: plane.shape.format,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                        view_formats: &[],
                    })
                })
                .collect();
            let views: Vec<wgpu::TextureView> = planes
                .iter()
                .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()))
                .collect();
            let Some(first) = views.first() else {
                return Err(LayoutError::Format.into());
            };
            // A format with fewer planes binds its last one again: the
            // shader does not read it.
            let second = views.get(1).unwrap_or(first);
            let third = views.get(2).unwrap_or(second);
            let bind_group = self.bind_group(device, [first, second, third]);
            self.uploaded = Some(Uploaded {
                format: key.0,
                size: key.1,
                planes,
                bind_group,
            });
        }
        if let Some(uploaded) = &self.uploaded {
            for (plane, texture) in view.planes().zip(&uploaded.planes) {
                queue.write_texture(
                    texture.as_image_copy(),
                    plane.data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(plane.stride),
                        rows_per_image: Some(plane.shape.height),
                    },
                    texture.size(),
                );
            }
            self.bound = Some(uploaded.bind_group.clone());
        }
        Ok(())
    }

    fn bind(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &TextureFrame,
    ) -> Result<(), ShowError> {
        match frame.ready() {
            Ready::Now => {}
            #[cfg(windows)]
            Ready::Fence { fence, value } => {
                fence.wait_on(queue, *value).map_err(ShowError::Import)?;
            }
        }
        #[cfg(not(windows))]
        let _ = queue;
        let found = self
            .imported
            .iter()
            .find(|i| i.texture == *frame.texture() && i.layer == frame.layer());
        let bind_group = if let Some(found) = found {
            found.bind_group.clone()
        } else {
            let (luma, chroma) = frame.format().plane_formats();
            let view = |aspect, format| {
                frame.texture().create_view(&wgpu::TextureViewDescriptor {
                    label: Some("takt layer imported plane"),
                    format: Some(format),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    aspect,
                    base_array_layer: frame.layer(),
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            };
            let luma = view(wgpu::TextureAspect::Plane0, luma);
            let chroma = view(wgpu::TextureAspect::Plane1, chroma);
            let bind_group = self.bind_group(device, [&luma, &chroma, &chroma]);
            if self.imported.len() >= IMPORTED_MAX {
                self.imported.remove(0);
            }
            self.imported.push(Imported {
                texture: frame.texture().clone(),
                layer: frame.layer(),
                bind_group: bind_group.clone(),
            });
            bind_group
        };
        self.bound = Some(bind_group);
        Ok(())
    }

    fn bind_group(
        &self,
        device: &wgpu::Device,
        planes: [&wgpu::TextureView; 3],
    ) -> wgpu::BindGroup {
        let plane = |binding: u32, view| wgpu::BindGroupEntry {
            binding,
            resource: wgpu::BindingResource::TextureView(view),
        };
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("takt layer"),
            layout: &self.renderer.layout,
            entries: &[
                plane(0, planes[0]),
                plane(1, planes[1]),
                plane(2, planes[2]),
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.renderer.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.uniform.as_entire_binding(),
                },
            ],
        })
    }

    /// Moves the layer; takes effect with the next draw.
    pub fn place(&mut self, queue: &wgpu::Queue, placement: Placement) {
        if self.placement != placement {
            self.placement = placement;
            self.write_uniform(queue);
        }
    }

    /// The peaks HDR pictures of this layer are tone mapped between, from
    /// the next [`LayerSlot::show`] on.
    pub fn set_tone_map(&mut self, tone: ToneMap) {
        self.tone = tone;
    }

    fn write_uniform(&self, queue: &wgpu::Queue) {
        if let Some(picture) = &self.picture {
            let bytes = uniform_bytes(&self.placement, picture, self.renderer.srgb_target);
            queue.write_buffer(&self.uniform, 0, &bytes);
        }
    }

    /// Gives the frames back whose pictures are no longer shown. Call it
    /// once for every submission, before the pass that draws the layer, with
    /// the encoder that is submitted together with that pass.
    ///
    /// A texture frame is not dropped the moment another picture takes its
    /// place: the GPU may still read it, and its owner expects the texture
    /// back in the state it handed it out in. So the first call after the
    /// replacement records the way back (to `D3D12_RESOURCE_STATE_COMMON`,
    /// which is `TextureUses::PRESENT` in wgpu-hal), and the call after that
    /// lets the frame go once that submission is done on the GPU. The guard
    /// of the frame therefore falls up to two submissions after its picture
    /// was replaced; a source needs that many pictures more in its pool.
    pub fn before_pass(&mut self, encoder: &mut wgpu::CommandEncoder, queue: &wgpu::Queue) {
        if !self.returning.is_empty() {
            let done = std::mem::take(&mut self.returning);
            queue.on_submitted_work_done(move || drop(done));
        }
        if self.replaced.is_empty() {
            return;
        }
        encoder.transition_resources(
            std::iter::empty(),
            self.replaced
                .iter()
                .filter_map(|frame| match frame.as_ref() {
                    GpuFrame::Texture(t) => Some(wgpu::TextureTransition {
                        texture: t.texture(),
                        selector: None,
                        state: wgpu::TextureUses::PRESENT,
                    }),
                    GpuFrame::Cpu(_) => None,
                }),
        );
        self.returning = std::mem::take(&mut self.replaced);
    }

    /// How many texture frames this layer still holds although it no longer
    /// shows them (see [`LayerSlot::before_pass`]).
    #[must_use]
    pub fn frames_on_their_way_back(&self) -> usize {
        self.replaced.len() + self.returning.len()
    }

    /// Draws the picture into its rectangle; nothing before the first
    /// [`LayerSlot::show`].
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if let Some(bind_group) = &self.bound {
            pass.set_pipeline(&self.renderer.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
    }

    /// Whether the layer has a picture to draw.
    #[must_use]
    pub fn has_picture(&self) -> bool {
        self.bound.is_some()
    }
}

fn uniform_bytes(
    placement: &Placement,
    picture: &PictureUniform,
    srgb_target: bool,
) -> [u8; UNIFORM_BYTES] {
    let (w, h) = (float(picture.visible.0), float(picture.visible.1));
    let chroma = if picture.half_chroma {
        (w / 2.0, h / 2.0)
    } else {
        (w, h)
    };
    let c = &picture.conversion;
    let floats: [[f32; 4]; 9] = [
        placement.rect,
        [w, h, chroma.0, chroma.1],
        c.ycc[0],
        c.ycc[1],
        c.ycc[2],
        c.gamut[0],
        c.gamut[1],
        c.gamut[2],
        [
            placement.opacity.clamp(0.0, 1.0),
            c.tone.source_peak_nits,
            c.tone.target_peak_nits,
            0.0,
        ],
    ];
    let modes: [u32; 4] = [
        picture.samples.code(),
        c.transfer.code(),
        u32::from(srgb_target),
        0,
    ];
    let mut out = [0_u8; UNIFORM_BYTES];
    let words = floats
        .iter()
        .flatten()
        .map(|f| f.to_le_bytes())
        .chain(modes.iter().map(|m| m.to_le_bytes()));
    for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(words) {
        *chunk = word;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use takt_core::{ColorDescription, PixelFormat};

    #[test]
    fn a_placement_in_pixels_is_a_rectangle_in_clip_space() {
        assert_eq!(
            Placement::pixels((200, 100), [0.0, 0.0, 200.0, 100.0], 1.0),
            Placement::FULL
        );
        let quarter = Placement::pixels((200, 100), [100.0, 50.0, 100.0, 50.0], 0.5);
        assert_eq!(quarter.rect, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(quarter.opacity, 0.5);
        assert_eq!(Placement::default(), Placement::FULL);
    }

    #[test]
    fn the_uniform_is_ten_vectors_in_the_order_of_the_shader() {
        let conversion = Conversion::new(
            PixelFormat::Nv12,
            ColorDescription::default().into(),
            ToneMap::default(),
        )
        .unwrap();
        let picture = PictureUniform {
            visible: (1920, 1080),
            half_chroma: true,
            samples: Samples::TwoPlanes,
            conversion,
        };
        let placement = Placement {
            rect: [-1.0, 1.0, 2.0, 2.0],
            opacity: 7.0,
        };
        let bytes = uniform_bytes(&placement, &picture, true);
        let float_at = |i: usize| f32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        let word_at = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(
            [float_at(0), float_at(1), float_at(2), float_at(3)],
            placement.rect
        );
        assert_eq!(
            [float_at(4), float_at(5), float_at(6), float_at(7)],
            [1920.0, 1080.0, 960.0, 540.0]
        );
        assert_eq!(float_at(8), conversion.ycc[0][0]);
        assert_eq!(float_at(19), conversion.ycc[2][3]);
        assert_eq!(float_at(20), conversion.gamut[0][0]);
        // The opacity is clamped, then the two peaks.
        assert_eq!(
            [float_at(32), float_at(33), float_at(34)],
            [1.0, 1000.0, 203.0]
        );
        assert_eq!([word_at(36), word_at(37), word_at(38)], [0, 0, 1]);
    }
}
