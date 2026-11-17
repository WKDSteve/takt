// SPDX-License-Identifier: GPL-3.0-or-later
//! Several layers into one target, back to front.
//!
//! Every layer converts its own picture to the R'G'B' of the target (BT.709
//! SDR) and is blended over what is below it with its alpha and opacity.
//! The blend works on the values of the target: on gamma-encoded values for
//! a `Unorm` target, in linear light for a target format that encodes to
//! sRGB itself.

use crate::layer::{LayerRenderer, LayerSlot};

/// Layers and the pass that draws them.
#[derive(Debug)]
pub struct Compositor {
    renderer: LayerRenderer,
    layers: Vec<LayerSlot>,
}

impl Compositor {
    /// A compositor of `layers` layers for targets of `target_format`;
    /// `filter` as for [`LayerRenderer::new`].
    #[must_use]
    pub fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        filter: wgpu::FilterMode,
        layers: usize,
    ) -> Self {
        let renderer = LayerRenderer::new(device, target_format, filter);
        let layers = (0..layers).map(|_| renderer.slot(device)).collect();
        Self { renderer, layers }
    }

    /// Adds a layer on top and returns its index.
    pub fn push_layer(&mut self, device: &wgpu::Device) -> usize {
        self.layers.push(self.renderer.slot(device));
        self.layers.len() - 1
    }

    /// The number of layers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.layers.len()
    }

    /// Whether there is no layer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Layer `index`, 0 is the bottom one.
    pub fn layer_mut(&mut self, index: usize) -> Option<&mut LayerSlot> {
        self.layers.get_mut(index)
    }

    /// Records one pass into `encoder` that clears `target` to `background`
    /// and draws every layer that has a picture, bottom first. The frames
    /// the layers no longer show start their way back first
    /// ([`LayerSlot::before_pass`]), so submit `encoder` once per call.
    pub fn render(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        queue: &wgpu::Queue,
        target: &wgpu::TextureView,
        background: wgpu::Color,
    ) {
        for layer in &mut self.layers {
            layer.before_pass(encoder, queue);
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("takt compositor"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(background),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        for layer in &self.layers {
            layer.draw(&mut pass);
        }
    }
}
