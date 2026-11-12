// SPDX-License-Identifier: GPL-3.0-or-later
//! The video surface: the picture of the source, drawn by [`gpu::Nv12Renderer`]
//! with a paint callback of `egui-wgpu` into the largest rectangle of its
//! aspect that fits, on black.

// The CPU reference of the shader, used by its tests.
#[cfg(test)]
pub mod colour;
pub mod gpu;

use std::sync::Arc;

use egui_wgpu::wgpu;
use takt_audio::DurationSnapshot;

use gpu::{Nv12Image, Nv12Renderer};

/// What lives in the callback resources of egui-wgpu: the renderer and the
/// picture it holds.
struct VideoResources {
    renderer: Nv12Renderer,
    generation: Option<u64>,
}

/// Puts the renderer into the resources of egui-wgpu; once, at start.
pub fn install(state: &egui_wgpu::RenderState) {
    let renderer = Nv12Renderer::new(&state.device, state.target_format, wgpu::FilterMode::Linear);
    state
        .renderer
        .write()
        .callback_resources
        .insert(VideoResources {
            renderer,
            generation: None,
        });
}

/// The upload times so far, `None` before [`install`].
#[must_use]
pub fn uploads(state: &egui_wgpu::RenderState) -> Option<DurationSnapshot> {
    state
        .renderer
        .read()
        .callback_resources
        .get::<VideoResources>()
        .map(|r| r.renderer.uploads())
}

/// A picture to show, numbered: a new number is uploaded once, the same one is
/// drawn again without an upload.
#[derive(Clone)]
pub struct Shown {
    /// The picture.
    pub image: Arc<Nv12Image>,
    /// Changes with every new picture.
    pub generation: u64,
}

struct VideoPaint(Shown);

impl egui_wgpu::CallbackTrait for VideoPaint {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(r) = resources.get_mut::<VideoResources>()
            && r.generation != Some(self.0.generation)
        {
            r.renderer.upload(device, queue, &self.0.image.view());
            r.generation = Some(self.0.generation);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        if let Some(r) = resources.get::<VideoResources>() {
            r.renderer.draw(pass);
        }
    }
}

/// Width to height of the surface until a source says otherwise.
pub const ASPECT: f32 = 16.0 / 9.0;

/// The largest rectangle of `aspect` that fits `available`, centred in it.
#[must_use]
pub fn fit(available: egui::Rect, aspect: f32) -> egui::Rect {
    let width = available.width().min(available.height() * aspect);
    let size = egui::vec2(width, width / aspect);
    egui::Rect::from_center_size(available.center(), size)
}

/// Draws `shown` (or an empty frame of the default aspect) into all the space
/// `ui` has left.
pub fn video_surface(ui: &mut egui::Ui, shown: Option<&Shown>) {
    let available = ui.available_rect_before_wrap();
    ui.allocate_rect(available, egui::Sense::hover());
    let painter = ui.painter_at(available);
    painter.rect_filled(available, 0.0, egui::Color32::BLACK);
    match shown {
        Some(shown) => {
            let (w, h) = shown.image.view().size();
            #[expect(clippy::cast_precision_loss, reason = "a display ratio, not time")]
            let aspect = w as f32 / h as f32;
            let surface = fit(available, aspect);
            painter.add(egui_wgpu::Callback::new_paint_callback(
                surface,
                VideoPaint(shown.clone()),
            ));
        }
        None => {
            painter.rect_stroke(
                fit(available, ASPECT),
                0.0,
                egui::Stroke::new(1.0_f32, ui.visuals().weak_text_color()),
                egui::StrokeKind::Inside,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_surface_keeps_its_shape_and_fits() {
        let wide = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 300.0));
        let r = fit(wide, ASPECT);
        assert!((r.height() - 300.0).abs() < 0.01);
        assert!((r.width() / r.height() - ASPECT).abs() < 0.001);
        assert!(wide.contains_rect(r));

        let tall = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(320.0, 900.0));
        let r = fit(tall, ASPECT);
        assert!((r.width() - 320.0).abs() < 0.01);
        assert!((r.center() - tall.center()).length() < 0.01);
    }
}
