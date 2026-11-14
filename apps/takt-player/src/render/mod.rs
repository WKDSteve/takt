// SPDX-License-Identifier: GPL-3.0-or-later
//! The video surface: the picture of the source, drawn with a paint callback
//! of `egui-wgpu` into the largest rectangle of its aspect that fits, on
//! black.
//!
//! There are two texture paths, chosen by the pixel format of the frame
//! ([`Picture::from_frame`]): NV12 through [`gpu::Nv12Renderer`] (the test
//! picture, later a decoder) and `rgb0` through [`gpu_rgb::Rgb0Renderer`]
//! (the finished picture of the `MpvBackend`). A frame is never converted
//! from one to the other on the CPU, and it is not copied either: the paint
//! callback holds the pooled frame itself ([`Picture`]) and uploads from its
//! planes.

// The CPU reference of the shader, used by its tests.
#[cfg(test)]
pub mod colour;
pub mod gpu;
pub mod gpu_rgb;

use std::sync::Arc;

use egui_wgpu::wgpu;
use takt_audio::DurationSnapshot;

use gpu::{LayoutError, Nv12Renderer, Nv12View};
use gpu_rgb::{Rgb0Renderer, Rgb0View};
use takt_core::{PixelFormat, PooledFrame};

/// The texture path of a picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    /// Two planes, 4:2:0, converted by the shader.
    Nv12,
    /// Finished R'G'B' with a padding byte, shown as it is.
    Rgb0,
}

/// A frame on its way to a texture: the pooled frame itself, not a copy.
///
/// The frame goes back to its pool when the last holder lets go: the window
/// (until the next picture replaces it) and the paint callbacks of the pass
/// that draws it. A source therefore needs a pool of at least two frames, the
/// one shown and the one it fills next.
#[derive(Debug)]
pub struct Picture {
    frame: PooledFrame,
    path: Path,
}

impl Picture {
    /// Takes `frame` for the texture path of its format.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] for incomplete planes, and [`LayoutError::Format`]
    /// for a format without a texture path (it is not converted to one that
    /// has). The frame goes back to its pool then.
    pub fn from_frame(frame: PooledFrame) -> Result<Self, LayoutError> {
        // The layout is checked once, here; the upload reads the same planes.
        let path = match frame.get().meta().format {
            PixelFormat::Nv12 => Nv12View::from_frame(frame.get()).map(|_| Path::Nv12),
            PixelFormat::Rgb0 => Rgb0View::from_frame(frame.get()).map(|_| Path::Rgb0),
            _ => Err(LayoutError::Format),
        }?;
        Ok(Self { frame, path })
    }

    /// Width and height in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        let meta = self.frame.get().meta();
        (meta.width, meta.height)
    }
}

/// What lives in the callback resources of egui-wgpu: a renderer per texture
/// path and the number of the picture uploaded last.
struct VideoResources {
    nv12: Nv12Renderer,
    rgb0: Rgb0Renderer,
    generation: Option<u64>,
    /// Whether the last upload went the `rgb0` path.
    last_rgb0: bool,
}

/// Puts the renderers into the resources of egui-wgpu; once, at start.
pub fn install(state: &egui_wgpu::RenderState) {
    let filter = wgpu::FilterMode::Linear;
    let nv12 = Nv12Renderer::new(&state.device, state.target_format, filter);
    let rgb0 = Rgb0Renderer::new(&state.device, state.target_format, filter);
    state
        .renderer
        .write()
        .callback_resources
        .insert(VideoResources {
            nv12,
            rgb0,
            generation: None,
            last_rgb0: false,
        });
}

/// The upload times of the texture path used last, `None` before
/// [`install`].
#[must_use]
pub fn uploads(state: &egui_wgpu::RenderState) -> Option<DurationSnapshot> {
    state
        .renderer
        .read()
        .callback_resources
        .get::<VideoResources>()
        .map(|r| {
            if r.last_rgb0 {
                r.rgb0.uploads()
            } else {
                r.nv12.uploads()
            }
        })
}

/// A picture to show, numbered: a new number is uploaded once, the same one is
/// drawn again without an upload.
#[derive(Clone)]
pub struct Shown {
    /// The picture.
    pub image: Arc<Picture>,
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
            let picture = self.0.image.as_ref();
            // The views were built once in `Picture::from_frame`; a frame
            // does not change after that, so they are there again.
            match picture.path {
                Path::Nv12 => {
                    if let Ok(view) = Nv12View::from_frame(picture.frame.get()) {
                        r.nv12.upload(device, queue, &view);
                    }
                }
                Path::Rgb0 => {
                    if let Ok(view) = Rgb0View::from_frame(picture.frame.get()) {
                        r.rgb0.upload(device, queue, &view);
                    }
                }
            }
            r.last_rgb0 = picture.path == Path::Rgb0;
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
            // The renderer of the picture this callback carries: `prepare`
            // uploaded it there.
            match self.0.image.path {
                Path::Nv12 => r.nv12.draw(pass),
                Path::Rgb0 => r.rgb0.draw(pass),
            }
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
            let (w, h) = shown.image.size();
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

    fn pool(format: PixelFormat, capacity: usize) -> takt_core::FramePool {
        use takt_core::{ColorPrimaries, ColorRange, FrameMeta, Timestamp, TransferFunction};
        takt_core::FramePool::new(
            FrameMeta {
                width: 8,
                height: 4,
                format,
                primaries: ColorPrimaries::Bt709,
                transfer: TransferFunction::Bt709,
                range: ColorRange::Limited,
                pts: Timestamp::from_millis(0),
            },
            capacity,
        )
    }

    fn frame(format: PixelFormat) -> PooledFrame {
        pool(format, 1)
            .acquire(takt_core::Timestamp::from_millis(0))
            .unwrap()
    }

    #[test]
    fn a_frame_goes_the_texture_path_of_its_format() {
        let nv12 = Picture::from_frame(frame(PixelFormat::Nv12)).unwrap();
        assert_eq!(nv12.path, Path::Nv12);
        let rgb0 = Picture::from_frame(frame(PixelFormat::Rgb0)).unwrap();
        assert_eq!(rgb0.path, Path::Rgb0);
        assert_eq!((nv12.size(), rgb0.size()), ((8, 4), (8, 4)));
        // A format without a path is refused, not converted.
        for format in [PixelFormat::Yuv420P, PixelFormat::Rgba8, PixelFormat::P010] {
            assert_eq!(
                Picture::from_frame(frame(format)).unwrap_err(),
                LayoutError::Format
            );
        }
    }

    #[test]
    fn a_picture_is_the_frame_itself_and_gives_it_back_when_the_last_holder_goes() {
        let stamp = takt_core::Timestamp::from_millis;
        let mut pool = pool(PixelFormat::Rgb0, 2);
        let mut frame = pool.acquire(stamp(0)).unwrap();
        frame.get_mut().plane_mut(0).unwrap()[..4].copy_from_slice(&[9, 8, 7, 0]);
        let rows = frame.get().plane(0).unwrap().as_ptr();

        // What the window keeps and what a paint callback carries.
        let shown = Shown {
            image: Arc::new(Picture::from_frame(frame).unwrap()),
            generation: 0,
        };
        let callback = shown.clone();
        let view = Rgb0View::from_frame(shown.image.frame.get()).unwrap();
        assert_eq!(view.size(), (8, 4));
        assert_eq!(
            shown.image.frame.get().plane(0).unwrap().as_ptr(),
            rows,
            "no copy: the rows of the pooled frame"
        );
        assert_eq!(
            &shown.image.frame.get().plane(0).unwrap()[..4],
            &[9, 8, 7, 0]
        );

        // The source fills its next frame while this one is shown: two out.
        let next = pool.acquire(stamp(40)).unwrap();
        assert_eq!(pool.in_flight(), 2);
        assert!(pool.acquire(stamp(80)).is_none(), "the pool is the bound");
        // The window moves on; the callback of the pass still holds the frame.
        drop(shown);
        assert_eq!(pool.in_flight(), 2);
        drop(callback);
        assert_eq!(pool.in_flight(), 1);
        drop(next);
        assert_eq!(pool.in_flight(), 0);

        // A frame that is refused goes back as well.
        let mut other = self::pool(PixelFormat::Rgba8, 1);
        assert!(Picture::from_frame(other.acquire(stamp(0)).unwrap()).is_err());
        assert_eq!(other.in_flight(), 0);
    }

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
