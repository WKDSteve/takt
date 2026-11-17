// SPDX-License-Identifier: GPL-3.0-or-later
//! `--imported`: the pictures of the source go to the GPU the way the
//! pictures of a hardware decoder will, without a decoder.
//!
//! A D3D11 device on the adapter of the window writes every frame into a
//! shared texture, and the layer of the video surface draws that texture
//! through the import of `takt-gpu` (`GpuFrame::Texture`, a fence, a guard
//! that gives the texture back). This is a check of that path in the real
//! window, with the swapchain and the encoder of egui around it: the first
//! picture is drawn both ways into a target of its own and the two results
//! are compared, and the overlay says how that went.
//!
//! Windows on DX12 only; NV12 frames of even size only. Whatever does not fit
//! goes the usual way (uploaded from system memory).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use egui_wgpu::wgpu;
use takt_core::{PixelFormat, PooledFrame};
use takt_gpu::interop::d3d11::{SharedSource, SharedTexture};
use takt_gpu::interop::dx12;
use takt_gpu::{GpuFrame, LayoutError, PlanarFormat, PlanesView, Ready, TextureFrame, readback};

/// Shared textures in the ring. A picture holds one while it is shown, the
/// layer up to two more on their way back, and the source fills the next.
const RING: usize = 6;

/// One shared texture: its D3D11 side, its D3D12 side as wgpu sees it, and
/// whether a frame holds it.
struct Shared {
    texture11: SharedTexture,
    texture: wgpu::Texture,
    free: Arc<AtomicBool>,
}

/// The guard of a frame: dropped when the GPU is done with the picture, it
/// hands the texture back to the ring.
struct Free(Arc<AtomicBool>);

impl Drop for Free {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// How the first imported picture compared with the upload of its planes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Verdict {
    /// No picture was imported yet.
    Pending,
    /// Both ways drew the same bytes.
    Same,
    /// They did not.
    Differs,
    /// The comparison itself failed.
    Unchecked(String),
}

/// Writes frames into shared textures and wraps them as frames of `takt-gpu`.
pub struct Importer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    source: SharedSource,
    fence: dx12::Fence,
    ring: Vec<Shared>,
    size: (u32, u32),
    verdict: Verdict,
    imported: u64,
    uploaded: u64,
    /// Why the last frame was uploaded instead.
    last_refusal: Option<String>,
}

impl Importer {
    /// An importer for the device of the window.
    ///
    /// # Errors
    ///
    /// A text for the user when the device is not on DX12 or has no NV12
    /// textures, or Direct3D refuses the second device.
    pub fn new(state: &egui_wgpu::RenderState) -> Result<Self, String> {
        if !state
            .device
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
        {
            return Err("the device has no NV12 textures".to_owned());
        }
        let luid = dx12::adapter_luid(&state.device).ok_or("the window does not draw with DX12")?;
        let source = SharedSource::on_adapter(luid).map_err(|e| e.to_string())?;
        let fence = dx12::open_shared_fence(&state.device, source.fence_handle())
            .map_err(|e| e.to_string())?;
        Ok(Self {
            device: state.device.clone(),
            queue: state.queue.clone(),
            source,
            fence,
            ring: Vec::new(),
            size: (0, 0),
            verdict: Verdict::Pending,
            imported: 0,
            uploaded: 0,
            last_refusal: None,
        })
    }

    /// `frame` as a frame for the GPU: imported when it can be, else the
    /// frame itself for the upload.
    ///
    /// # Errors
    ///
    /// [`LayoutError`] for a frame that goes the usual way and has no path
    /// there either.
    pub fn frame(&mut self, frame: PooledFrame) -> Result<Arc<GpuFrame>, LayoutError> {
        match self.import(&frame) {
            Ok(texture) => {
                let texture = Arc::new(GpuFrame::Texture(texture));
                if self.verdict == Verdict::Pending {
                    // The same picture both ways; the frame from the pool is
                    // not needed after that, the texture holds its picture.
                    self.verdict = self.compare(&texture, GpuFrame::Cpu(frame));
                }
                self.imported += 1;
                self.last_refusal = None;
                Ok(texture)
            }
            Err(why) => {
                self.uploaded += 1;
                self.last_refusal = Some(why);
                GpuFrame::from_frame(frame).map(Arc::new)
            }
        }
    }

    fn import(&mut self, frame: &PooledFrame) -> Result<TextureFrame, String> {
        let meta = frame.get().meta();
        if meta.format != PixelFormat::Nv12 {
            return Err(format!("{:?} is not NV12", meta.format));
        }
        let view = PlanesView::from_frame(frame.get()).map_err(|e| e.to_string())?;
        let size = (meta.width, meta.height);
        if self.size != size {
            // Frames still out keep their textures alive; the ring starts anew.
            self.ring.clear();
            self.size = size;
        }
        if self.ring.is_empty() {
            for _ in 0..RING {
                let texture11 = self
                    .source
                    .texture(PlanarFormat::Nv12, size.0, size.1)
                    .map_err(|e| e.to_string())?;
                let texture =
                    dx12::open_shared_texture(&self.device, texture11.handle(), PlanarFormat::Nv12)
                        .map_err(|e| e.to_string())?;
                self.ring.push(Shared {
                    texture11,
                    texture,
                    free: Arc::new(AtomicBool::new(true)),
                });
            }
        }
        let shared = self
            .ring
            .iter()
            .find(|s| s.free.load(Ordering::Acquire))
            .ok_or("every shared texture is still in use")?;
        let value = self
            .source
            .write(&shared.texture11, &view)
            .map_err(|e| e.to_string())?;
        let texture = TextureFrame::new(
            shared.texture.clone(),
            PlanarFormat::Nv12,
            size,
            meta.color().into(),
        )
        .map_err(|e| e.to_string())?;
        shared.free.store(false, Ordering::Release);
        Ok(texture
            .ready_when(Ready::Fence {
                fence: self.fence.clone(),
                value,
            })
            .returned_by(Free(Arc::clone(&shared.free))))
    }

    fn compare(&self, imported: &Arc<GpuFrame>, uploaded: GpuFrame) -> Verdict {
        let uploaded = Arc::new(uploaded);
        let both = readback::frame_checksum(&self.device, &self.queue, imported).and_then(|a| {
            readback::frame_checksum(&self.device, &self.queue, &uploaded).map(|b| (a, b))
        });
        match both {
            Ok((a, b)) if a == b => Verdict::Same,
            Ok(_) => Verdict::Differs,
            Err(e) => Verdict::Unchecked(e.to_string()),
        }
    }

    /// What the overlay says about the path of the picture.
    #[must_use]
    pub fn status(&self) -> String {
        let verdict = match &self.verdict {
            Verdict::Pending => "nothing imported yet".to_owned(),
            Verdict::Same => "same bytes as the upload".to_owned(),
            Verdict::Differs => "DIFFERS from the upload".to_owned(),
            Verdict::Unchecked(why) => format!("not compared: {why}"),
        };
        let mut text = format!(
            "imported texture (D3D11 shared, NV12), {verdict}; {} imported, {} uploaded",
            self.imported, self.uploaded
        );
        if let Some(why) = &self.last_refusal {
            text.push_str(&format!(" ({why})"));
        }
        text
    }

    /// The verdict alone: a line that does not change once it is there, for
    /// a test that waits for it by name.
    #[must_use]
    pub fn verdict(&self) -> Option<&'static str> {
        match self.verdict {
            Verdict::Pending => None,
            Verdict::Same => Some("Imported picture: same bytes as the upload"),
            Verdict::Differs => Some("Imported picture: DIFFERS from the upload"),
            Verdict::Unchecked(_) => Some("Imported picture: not compared"),
        }
    }
}
