// SPDX-License-Identifier: GPL-3.0-or-later
//! The video surface: the picture of the source, drawn with a paint callback
//! of `egui-wgpu` into the largest rectangle of its aspect that fits, on
//! black.
//!
//! The drawing itself is `takt-gpu`: a frame is a [`GpuFrame`], and one layer
//! ([`takt_gpu::LayerSlot`]) converts it with the shader of that crate,
//! whatever its pixel format (NV12 of the test picture, `rgb0` of the
//! `MpvBackend`, later the frames of a decoder). A frame is never converted
//! on the CPU, and it is not copied either: the paint callback holds the
//! frame itself ([`Picture`]) and the layer uploads from its planes.

#[cfg(windows)]
pub mod import;
pub mod loss;

use std::sync::Arc;
use std::time::Instant;

use egui_wgpu::wgpu;
use takt_audio::{DurationHistogram, DurationSnapshot};
use takt_core::PooledFrame;
use takt_gpu::{GpuFrame, LayerRenderer, LayerSlot, LayoutError};

/// A frame on its way to a texture: the pooled frame itself, not a copy.
///
/// The frame goes back to its pool when the last holder lets go: the window
/// (until the next picture replaces it) and the paint callbacks of the pass
/// that draws it. A source therefore needs a pool of at least two frames, the
/// one shown and the one it fills next.
#[derive(Debug)]
pub struct Picture {
    frame: Arc<GpuFrame>,
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
        Ok(Self {
            frame: Arc::new(GpuFrame::from_frame(frame)?),
        })
    }

    /// A picture that is a frame for the GPU already (`--imported`).
    #[cfg(windows)]
    #[must_use]
    pub fn from_gpu(frame: Arc<GpuFrame>) -> Self {
        Self { frame }
    }

    /// Width and height in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        self.frame.size()
    }

    /// Whether the picture is finished R'G'B' (the `MpvBackend`), not Y'CbCr.
    fn is_rgb(&self) -> bool {
        self.frame.format().is_rgb()
    }
}

/// What lives in the callback resources of egui-wgpu: the layer the video is
/// drawn with, the number of the picture shown last, and the upload times of
/// the two kinds of picture.
struct VideoResources {
    slot: LayerSlot,
    generation: Option<u64>,
    uploads_ycc: DurationHistogram,
    uploads_rgb: DurationHistogram,
    /// Whether the last upload was an R'G'B' picture.
    last_rgb: bool,
    /// Why the last picture was not shown; said once, not every pass.
    fault: Option<String>,
}

/// Puts the layer into the resources of egui-wgpu; once, at start.
pub fn install(state: &egui_wgpu::RenderState) {
    let renderer = LayerRenderer::new(&state.device, state.target_format, wgpu::FilterMode::Linear);
    state
        .renderer
        .write()
        .callback_resources
        .insert(VideoResources {
            slot: renderer.slot(&state.device),
            generation: None,
            uploads_ycc: DurationHistogram::default(),
            uploads_rgb: DurationHistogram::default(),
            last_rgb: false,
            fault: None,
        });
}

/// The upload times of the kind of picture shown last (CPU side: the copy
/// into the staging memory of wgpu, not the transfer on the GPU); `None`
/// before [`install`].
#[must_use]
pub fn uploads(state: &egui_wgpu::RenderState) -> Option<DurationSnapshot> {
    state
        .renderer
        .read()
        .callback_resources
        .get::<VideoResources>()
        .map(|r| {
            if r.last_rgb {
                r.uploads_rgb.snapshot()
            } else {
                r.uploads_ycc.snapshot()
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
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(r) = resources.get_mut::<VideoResources>() {
            // The encoder of egui is submitted together with the pass that
            // draws the layer: frames the layer no longer shows start their
            // way back here.
            r.slot.before_pass(encoder, queue);
            if r.generation != Some(self.0.generation) {
                let picture = self.0.image.as_ref();
                let start = Instant::now();
                // The layout was checked in `Picture::from_frame`; what can
                // still fail is a format the device has no texture for, and
                // the layer keeps the picture before then.
                match r.slot.show(device, queue, &picture.frame) {
                    Ok(()) => {
                        let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                        r.last_rgb = picture.is_rgb();
                        if r.last_rgb {
                            r.uploads_rgb.record(nanos);
                        } else {
                            r.uploads_ycc.record(nanos);
                        }
                        r.fault = None;
                    }
                    Err(e) => {
                        if let Some(news) = fault_news(&mut r.fault, &e.to_string()) {
                            eprintln!("takt-player: picture not shown: {news}");
                        }
                    }
                }
                r.generation = Some(self.0.generation);
            }
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
            // The layer fills the viewport, which egui-wgpu set to the rect
            // of this callback.
            r.slot.draw(pass);
        }
    }
}

/// Remembers `fault` and hands it back when it is another one than the last:
/// a source that cannot be shown fails with every picture, and is named once.
fn fault_news<'a>(last: &mut Option<String>, fault: &'a str) -> Option<&'a str> {
    if last.as_deref() == Some(fault) {
        return None;
    }
    *last = Some(fault.to_owned());
    Some(fault)
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
    use takt_core::PixelFormat;

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

    fn planes(picture: &Picture) -> &takt_core::VideoFrame {
        match picture.frame.as_ref() {
            GpuFrame::Cpu(frame) => frame.get(),
            GpuFrame::Texture(_) => unreachable!("a frame from a pool is in system memory"),
        }
    }

    #[test]
    fn a_frame_goes_the_texture_path_of_its_format() {
        let nv12 = Picture::from_frame(frame(PixelFormat::Nv12)).unwrap();
        assert!(!nv12.is_rgb());
        let rgb0 = Picture::from_frame(frame(PixelFormat::Rgb0)).unwrap();
        assert!(rgb0.is_rgb());
        assert_eq!((nv12.size(), rgb0.size()), ((8, 4), (8, 4)));
        // What a software decoder hands out has a path as well.
        for format in [
            PixelFormat::Yuv420P,
            PixelFormat::Yuv420P10,
            PixelFormat::P010,
        ] {
            assert!(Picture::from_frame(frame(format)).is_ok(), "{format:?}");
        }
        // A format without a path is refused, not converted.
        assert_eq!(
            Picture::from_frame(frame(PixelFormat::Rgba16F)).unwrap_err(),
            LayoutError::Format
        );
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
        assert_eq!(shown.image.size(), (8, 4));
        assert_eq!(
            planes(&shown.image).plane(0).unwrap().as_ptr(),
            rows,
            "no copy: the rows of the pooled frame"
        );
        assert_eq!(&planes(&shown.image).plane(0).unwrap()[..4], &[9, 8, 7, 0]);

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
        let mut other = self::pool(PixelFormat::Rgba16F, 1);
        assert!(Picture::from_frame(other.acquire(stamp(0)).unwrap()).is_err());
        assert_eq!(other.in_flight(), 0);
    }

    #[test]
    fn a_picture_that_cannot_be_shown_is_named_once() {
        let mut last = None;
        assert_eq!(fault_news(&mut last, "no 16 bit"), Some("no 16 bit"));
        assert_eq!(fault_news(&mut last, "no 16 bit"), None);
        assert_eq!(fault_news(&mut last, "no NV12"), Some("no NV12"));
        // A picture that was shown in between makes the same fault news again.
        last = None;
        assert_eq!(fault_news(&mut last, "no NV12"), Some("no NV12"));
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
