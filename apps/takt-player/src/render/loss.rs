// SPDX-License-Identifier: GPL-3.0-or-later
//! A lost GPU device ends the window with its reason, not with a panic.
//!
//! wgpu loses a device for good when a call of the driver fails with "out of memory", "device
//! lost" (a reset of the GPU, a removed device) or an error it does not know (wgpu-core,
//! `Device::handle_hal_error`). After that every buffer it is asked for is refused without a word:
//! an error of a lost device goes to the lost callback, not to the error handler. egui-wgpu then
//! panics in the next frame that has something to draw, because `Queue::write_buffer_with` gives
//! it no staging buffer ("Failed to create staging buffer for index data"). That panic was all a
//! run under heavy GPU load left behind (task `takt/player-panik-egui-wgpu-staging-buffer`).
//!
//! eframe cannot make a new device for its window, so the player ends: [`DeviceLoss`] hears the
//! lost callback, and [`EndsOnLoss`] lets the app draw nothing more from the next frame on (egui
//! has no mesh then, and egui-wgpu asks for no staging buffer), says why on stderr and in the log
//! (`startlog`), and closes the window. A loss in the moment between the app and the paint of
//! the same frame still ends in that panic; the panic hook then names the loss as well
//! ([`recorded`]).

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use egui_wgpu::wgpu;

/// The longest reason that is kept, in characters: the message of a driver is foreign text.
const REASON_CHARS: usize = 200;

/// The first loss of a device in this process, for the panic hook and the exit code.
static RECORDED: OnceLock<String> = OnceLock::new();

/// Why the device of the window was lost, when it was; `None` while it is not.
#[must_use]
pub fn recorded() -> Option<&'static str> {
    RECORDED.get().map(String::as_str)
}

/// What the lost callback of wgpu says, as one short line.
fn reason_text(reason: wgpu::DeviceLostReason, message: &str) -> String {
    let message: String = message
        .split(['\r', '\n'])
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(REASON_CHARS)
        .collect();
    match (reason, message.is_empty()) {
        (wgpu::DeviceLostReason::Destroyed, true) => "destroyed".to_owned(),
        (wgpu::DeviceLostReason::Destroyed, false) => format!("destroyed: {message}"),
        (wgpu::DeviceLostReason::Unknown, true) => "no reason given".to_owned(),
        (wgpu::DeviceLostReason::Unknown, false) => message,
    }
}

/// Whether, and why, a device was lost; filled from the lost callback of wgpu, on whatever thread
/// wgpu calls it.
#[derive(Debug, Default)]
pub struct DeviceLoss {
    reason: Mutex<Option<String>>,
}

impl DeviceLoss {
    /// Listens to the lost callback of `device` (it replaces one set before).
    #[must_use]
    pub fn watch(device: &wgpu::Device) -> Arc<Self> {
        let loss = Arc::new(Self::default());
        let heard = Arc::clone(&loss);
        device.set_device_lost_callback(move |reason, message| {
            heard.record(reason_text(reason, &message));
        });
        loss
    }

    /// Keeps the first reason; a later one says nothing new.
    fn record(&self, reason: String) {
        let _ = RECORDED.set(reason.clone());
        let mut kept = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
        if kept.is_none() {
            *kept = Some(reason);
        }
    }

    /// Why the device was lost, `None` while it is not.
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        self.reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The message a lost device ends the player with.
#[must_use]
pub fn ending_text(reason: &str) -> String {
    format!(
        "the GPU device was lost ({reason}); the window cannot draw without it and closes \
         (more likely under heavy GPU load or with little free GPU memory)"
    )
}

/// An app that draws as long as its device lives and closes the window once it is lost.
pub struct EndsOnLoss<A> {
    app: A,
    loss: Option<Arc<DeviceLoss>>,
    /// The loss has been told and the window asked to close.
    told: bool,
    /// Tells the message of the end ([`tell`]; a test keeps it).
    tell: fn(&str),
}

/// Tells the end on stderr, in the log and, when nobody reads stderr, in a message window.
fn tell(text: &str) {
    eprintln!("takt-player: {text}");
    crate::startlog::report(text);
}

impl<A> EndsOnLoss<A> {
    /// `app`, ended when `loss` reports a lost device; `None` (no wgpu device) never ends it.
    pub const fn new(app: A, loss: Option<Arc<DeviceLoss>>) -> Self {
        Self {
            app,
            loss,
            told: false,
            tell,
        }
    }

    /// Why the device was lost, if it was.
    fn lost(&self) -> Option<String> {
        self.loss.as_ref().and_then(|loss| loss.reason())
    }

    /// One frame: the app's, or nothing at all and the window closes. Says the reason once.
    fn frame(&mut self, ctx: &egui::Context, draw: impl FnOnce(&mut A, &egui::Context)) {
        let Some(reason) = self.lost() else {
            draw(&mut self.app, ctx);
            return;
        };
        if !self.told {
            self.told = true;
            (self.tell)(&ending_text(&reason));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl<A: eframe::App> eframe::App for EndsOnLoss<A> {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.frame(ctx, |app, ctx| app.update(ctx, frame));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An app of nothing, ended on `loss` without a word to stderr or the log.
    fn quiet(loss: Option<Arc<DeviceLoss>>) -> EndsOnLoss<()> {
        EndsOnLoss {
            tell: |_| {},
            ..EndsOnLoss::new((), loss)
        }
    }

    /// One frame of `ends` with a label as the app, and what it left: the shapes to draw and
    /// whether it asked the window to close.
    fn one_frame(ends: &mut EndsOnLoss<()>) -> (usize, bool) {
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            ends.frame(ctx, |(), ctx| {
                egui::CentralPanel::default().show(ctx, |ui| ui.label("a picture"));
            });
        });
        let closes = output
            .viewport_output
            .values()
            .any(|v| v.commands.contains(&egui::ViewportCommand::Close));
        (output.shapes.len(), closes)
    }

    #[test]
    fn after_a_lost_device_nothing_is_drawn_and_the_window_closes() {
        let loss = Arc::new(DeviceLoss::default());
        let mut ends = quiet(Some(Arc::clone(&loss)));
        let (shapes, closes) = one_frame(&mut ends);
        assert!(shapes > 0, "the app draws while the device lives");
        assert!(!closes);

        loss.record("Out of memory".to_owned());
        // No shape: egui-wgpu gets no mesh and asks for no staging buffer.
        assert_eq!(one_frame(&mut ends), (0, true));
        assert!(ends.told);
        assert_eq!(
            one_frame(&mut ends),
            (0, true),
            "and so in every frame after"
        );
    }

    #[test]
    fn without_a_device_the_app_is_never_ended() {
        let mut ends = quiet(None);
        let (shapes, closes) = one_frame(&mut ends);
        assert!(shapes > 0);
        assert!(!closes);
    }

    #[test]
    fn the_first_reason_is_kept_as_one_short_line() {
        let loss = DeviceLoss::default();
        loss.record(reason_text(
            wgpu::DeviceLostReason::Unknown,
            "\nOut of memory\nsecond line",
        ));
        loss.record("later".to_owned());
        assert_eq!(loss.reason().as_deref(), Some("Out of memory"));
        assert!(recorded().is_some());
        assert_eq!(
            reason_text(wgpu::DeviceLostReason::Destroyed, ""),
            "destroyed"
        );
        assert_eq!(
            reason_text(wgpu::DeviceLostReason::Unknown, "a\u{1b}[31mb"),
            "a[31mb"
        );
        assert_eq!(
            reason_text(wgpu::DeviceLostReason::Unknown, &"x".repeat(500))
                .chars()
                .count(),
            REASON_CHARS
        );
        assert!(ending_text("Out of memory").contains("(Out of memory)"));
    }

    /// Blocks on a future of wgpu: on native backends they are ready when polled.
    fn ready<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        loop {
            if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
            std::thread::yield_now();
        }
    }

    /// What egui-wgpu does with the frames of a lost device: a frame with something to draw ends
    /// in the panic of the task, a frame without a shape does not. On a real adapter, like the
    /// GPU tests of takt-gpu: `TAKT_GPU_TESTS=1 [TAKT_GPU_ADAPTER=fallback] cargo test -p
    /// takt-player lost_device -- --ignored`.
    #[test]
    #[ignore = "needs a GPU adapter (TAKT_GPU_TESTS=1)"]
    fn a_lost_device_panics_egui_wgpu_with_a_mesh_and_not_without_one() {
        if std::env::var("TAKT_GPU_TESTS").as_deref() != Ok("1") {
            println!("SKIP lost_device: set TAKT_GPU_TESTS=1 to run on a GPU adapter");
            return;
        }
        let gpu = ready(takt_gpu::Gpu::open(takt_gpu::GpuOptions {
            fallback_adapter: std::env::var("TAKT_GPU_ADAPTER").as_deref() == Ok("fallback"),
            ..Default::default()
        }))
        .expect("a GPU adapter");
        let (device, queue) = (&gpu.device, &gpu.queue);
        let loss = DeviceLoss::watch(device);
        let mut renderer = egui_wgpu::Renderer::new(
            device,
            wgpu::TextureFormat::Rgba8Unorm,
            egui_wgpu::RendererOptions::default(),
        );
        device.destroy();
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        assert_eq!(loss.reason().as_deref(), Some("destroyed"));

        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [320, 200],
            pixels_per_point: 1.0,
        };
        let mut paint = |loss: Option<Arc<DeviceLoss>>| {
            let mut ends = quiet(loss);
            let ctx = egui::Context::default();
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                ends.frame(ctx, |(), ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| ui.label("a picture"));
                });
            });
            let jobs = ctx.tessellate(output.shapes, 1.0);
            let mut encoder =
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                renderer.update_buffers(device, queue, &mut encoder, &jobs, &screen);
            }))
            .is_ok()
        };
        // The control: the app drawn on the lost device is the panic of the task.
        assert!(
            !paint(None),
            "egui-wgpu panics on a lost device with a mesh"
        );
        assert!(paint(Some(loss)), "no mesh, no staging buffer, no panic");
    }
}
