// SPDX-License-Identifier: GPL-3.0-or-later
//! `--window`: the offscreen target also goes to a swapchain, so the run does what `mpv` does in its window
//! (present, vsync) and the two CPU numbers can stand next to each other.

use std::sync::Arc;
use std::time::Duration;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::pump_events::EventLoopExtPumpEvents;
use winit::window::{Window, WindowId};

const BLIT: &str = "
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
struct V { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> }
@vertex fn vs(@builtin(vertex_index) i: u32) -> V {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return V(vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0), vec2<f32>(p.x, 1.0 - p.y));
}
@fragment fn fs(v: V) -> @location(0) vec4<f32> { return textureSample(source, smp, v.uv); }
";

#[derive(Default)]
struct App {
    window: Option<Arc<Window>>,
    closed: bool,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attributes = Window::default_attributes()
                .with_title("zero-copy-spike")
                .with_inner_size(winit::dpi::PhysicalSize::new(1280, 720));
            self.window = event_loop.create_window(attributes).ok().map(Arc::new);
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            self.closed = true;
        }
    }
}

/// A window with a swapchain and the pass that scales the offscreen target into it.
pub struct Presenter {
    event_loop: EventLoop<()>,
    app: App,
    surface: wgpu::Surface<'static>,
    pipeline: wgpu::RenderPipeline,
    bind: Option<wgpu::BindGroup>,
    sampler: wgpu::Sampler,
}

impl Presenter {
    pub fn new(
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
    ) -> Result<Self, String> {
        let mut event_loop = EventLoop::new().map_err(|e| format!("event loop: {e}"))?;
        let mut app = App::default();
        let _ = event_loop.pump_app_events(Some(Duration::ZERO), &mut app);
        let window = app.window.clone().ok_or("no window")?;
        let size = window.inner_size();
        let surface = instance
            .create_surface(window)
            .map_err(|e| format!("create_surface: {e}"))?;
        let mut config = surface
            .get_default_config(adapter, size.width.max(1), size.height.max(1))
            .ok_or("the surface is not supported by the adapter")?;
        config.present_mode = wgpu::PresentMode::Fifo;
        surface.configure(device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(config.format.into())],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Ok(Self {
            event_loop,
            app,
            surface,
            pipeline,
            bind: None,
            sampler,
        })
    }

    /// Handles the window messages; `false` once the window was closed.
    pub fn pump(&mut self) -> bool {
        let _ = self
            .event_loop
            .pump_app_events(Some(Duration::ZERO), &mut self.app);
        !self.app.closed
    }

    /// Records the scaling pass into `encoder` and hands back the swapchain picture to present after the
    /// submit.
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::TextureView,
    ) -> Result<wgpu::SurfaceTexture, String> {
        let frame = self
            .surface
            .get_current_texture()
            .map_err(|e| format!("get_current_texture: {e}"))?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let bind = self.bind.get_or_insert_with(|| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(source),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            })
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations::default(),
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &*bind, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);
        Ok(frame)
    }
}
