// SPDX-License-Identifier: GPL-3.0-or-later
//! Measures the compositor: several layers of 4K video into a 4K target, at
//! 60 pictures a second, and counts the pictures that were not done in time.
//!
//! ```text
//! cargo run --release -p takt-gpu --example composite-bench -- [options]
//!
//!   --layers N        number of layers (default 4)
//!   --seconds N       length of the run (default 30)
//!   --size WxH        size of the sources and of the target (default 3840x2160)
//!   --fps N           pictures a second without a window (default 60)
//!   --source KIND     imported (default under Windows): every picture is a texture another device
//!                     wrote, imported without a copy; upload: every picture is uploaded from system
//!                     memory, the fallback path
//!   --window          also present the target in a window of 1280x720 with vsync; the display
//!                     sets the pace then
//!   --fallback        the software adapter
//! ```
//!
//! The layers, bottom to top, repeated when there are more than four:
//!
//! 1. NV12, BT.709, the whole target.
//! 2. P010, BT.2020 PQ, tone mapped, the whole target at half opacity.
//! 3. NV12, BT.709, a quarter of the target in a corner (picture in picture).
//! 4. RGBA with alpha, the whole target: a graphic, uploaded once.
//!
//! Every video layer shows a new picture each time, out of a ring of three.
//! What is measured is the compositor, not a decoder: the pictures are made
//! before the clock starts.
//!
//! Without a window a picture is due every 1/fps seconds; it is late when the
//! GPU has not finished it by the time the next one is due. With a window,
//! `present` blocks until the display takes the picture; a picture is late
//! when more than one and a half refresh periods passed since the one before.

use std::sync::Arc;
use std::time::{Duration, Instant};

use takt_core::{
    ColorDescription, ColorPrimaries, ColorRange, FrameMeta, FramePool, PixelFormat, Timestamp,
    TransferFunction,
};
use takt_gpu::{Compositor, Gpu, GpuFrame, GpuOptions, Placement, PlanesView, readback, wgpu};

const RING: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    Imported,
    Upload,
}

struct Args {
    layers: usize,
    seconds: u64,
    size: (u32, u32),
    fps: u32,
    source: Source,
    window: bool,
    fallback: bool,
}

fn parse() -> Result<Args, String> {
    let mut args = Args {
        layers: 4,
        seconds: 30,
        size: (3840, 2160),
        fps: 60,
        source: if cfg!(windows) {
            Source::Imported
        } else {
            Source::Upload
        },
        window: false,
        fallback: false,
    };
    let mut list = std::env::args().skip(1);
    while let Some(arg) = list.next() {
        let mut value = || list.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--layers" => args.layers = value()?.parse().map_err(|e| format!("--layers: {e}"))?,
            "--seconds" => {
                args.seconds = value()?.parse().map_err(|e| format!("--seconds: {e}"))?
            }
            "--fps" => args.fps = value()?.parse().map_err(|e| format!("--fps: {e}"))?,
            "--size" => {
                let text = value()?;
                let (w, h) = text.split_once('x').ok_or("--size takes WxH")?;
                args.size = (
                    w.parse().map_err(|e| format!("--size: {e}"))?,
                    h.parse().map_err(|e| format!("--size: {e}"))?,
                );
            }
            "--source" => {
                args.source = match value()?.as_str() {
                    "imported" => Source::Imported,
                    "upload" => Source::Upload,
                    other => return Err(format!("--source: imported or upload, not {other}")),
                }
            }
            "--window" => args.window = true,
            "--fallback" => args.fallback = true,
            other => return Err(format!("unknown option {other}")),
        }
    }
    if args.layers == 0 || args.layers > 64 || args.seconds == 0 || args.fps == 0 {
        return Err("layers 1..=64, seconds and fps above 0".to_owned());
    }
    let (w, h) = args.size;
    if w < 16 || h < 16 || w % 2 != 0 || h % 2 != 0 || w > 8192 || h > 8192 {
        return Err("--size: even sides from 16 to 8192".to_owned());
    }
    if args.source == Source::Imported && !cfg!(windows) {
        return Err("--source imported is Windows only".to_owned());
    }
    Ok(args)
}

/// What a layer shows: its kind decides format, colour and place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Background,
    Hdr,
    Inset,
    Graphic,
}

impl Kind {
    fn of(layer: usize) -> Self {
        [Self::Background, Self::Hdr, Self::Inset, Self::Graphic][layer % 4]
    }

    fn format(self) -> PixelFormat {
        match self {
            Self::Background | Self::Inset => PixelFormat::Nv12,
            Self::Hdr => PixelFormat::P010,
            Self::Graphic => PixelFormat::Rgba8,
        }
    }

    fn color(self) -> ColorDescription {
        match self {
            Self::Hdr => ColorDescription {
                primaries: ColorPrimaries::Bt2020,
                transfer: TransferFunction::Pq,
                range: ColorRange::Limited,
            },
            Self::Graphic => ColorDescription {
                range: ColorRange::Full,
                ..Default::default()
            },
            Self::Background | Self::Inset => ColorDescription::default(),
        }
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "pixels of a target and a small layer index"
    )]
    fn placement(self, layer: usize, target: (u32, u32)) -> Placement {
        let (w, h) = (target.0 as f32, target.1 as f32);
        // Layers above the fourth are shifted a little, so they do not hide each other exactly.
        let shift = (layer / 4) as f32 * 0.03;
        match self {
            Self::Background | Self::Graphic => Placement::pixels(target, [0.0, 0.0, w, h], 1.0),
            Self::Hdr => Placement::pixels(target, [w * shift, h * shift, w, h], 0.5),
            Self::Inset => Placement::pixels(
                target,
                [w * (0.72 - shift), h * (0.72 - shift), w / 4.0, h / 4.0],
                1.0,
            ),
        }
    }
}

/// One picture in system memory, tight rows.
struct Planes {
    format: PixelFormat,
    size: (u32, u32),
    planes: Vec<(Vec<u8>, u32)>,
}

impl Planes {
    fn view(&self) -> PlanesView<'_> {
        let planes: Vec<(&[u8], u32)> = self.planes.iter().map(|(d, s)| (&d[..], *s)).collect();
        PlanesView::new(self.format, self.size.0, self.size.1, &planes)
            .expect("the planes were made for this size")
    }
}

/// A moving pattern: diagonal stripes whose phase is `step`.
fn picture(kind: Kind, size: (u32, u32), step: u32) -> Planes {
    let (w, h) = size;
    let wave = |x: u32, y: u32, speed: u32| ((x + y * 2 + step * speed) & 0xff).to_le_bytes()[0];
    match kind.format() {
        PixelFormat::Nv12 => {
            let mut y = vec![0_u8; (w * h) as usize];
            let mut uv = vec![0_u8; (w * h / 2) as usize];
            for row in 0..h {
                for col in 0..w {
                    y[(row * w + col) as usize] = 16 + wave(col, row, 37) / 2;
                }
            }
            for row in 0..h / 2 {
                for col in 0..w / 2 {
                    let at = (row * w + col * 2) as usize;
                    uv[at] = 64 + wave(col, row, 11) / 2;
                    uv[at + 1] = 64 + wave(row, col, 23) / 2;
                }
            }
            Planes {
                format: PixelFormat::Nv12,
                size,
                planes: vec![(y, w), (uv, w)],
            }
        }
        PixelFormat::P010 => {
            let mut y = vec![0_u8; (w * h * 2) as usize];
            let mut uv = vec![0_u8; (w * h) as usize];
            let put = |plane: &mut [u8], at: usize, code: u16| {
                plane[at..at + 2].copy_from_slice(&(code << 6).to_le_bytes());
            };
            for row in 0..h {
                for col in 0..w {
                    // Up to a signal of about 80 %: above the peak of the source in places.
                    let code = 64 + u16::from(wave(col, row, 29)) * 3;
                    put(&mut y, ((row * w + col) * 2) as usize, code);
                }
            }
            for row in 0..h / 2 {
                for col in 0..w / 2 {
                    let at = ((row * w + col * 2) * 2) as usize;
                    put(&mut uv, at, 384 + u16::from(wave(col, row, 7)));
                    put(&mut uv, at + 2, 384 + u16::from(wave(row, col, 13)));
                }
            }
            Planes {
                format: PixelFormat::P010,
                size,
                planes: vec![(y, w * 2), (uv, w * 2)],
            }
        }
        _ => {
            // A band across the lower third, half transparent at its edges, the rest transparent.
            let mut rgba = vec![0_u8; (w * h * 4) as usize];
            for row in h * 2 / 3..h * 5 / 6 {
                for col in 0..w {
                    let at = ((row * w + col) * 4) as usize;
                    let edge = row < h * 2 / 3 + 16 || row >= h * 5 / 6 - 16;
                    rgba[at..at + 4].copy_from_slice(&[
                        230,
                        wave(col, row, 0),
                        40,
                        if edge { 128 } else { 255 },
                    ]);
                }
            }
            Planes {
                format: PixelFormat::Rgba8,
                size,
                planes: vec![(rgba, w * 4)],
            }
        }
    }
}

/// Where the pictures of one video layer come from.
enum Feed {
    /// A graphic, shown once.
    Still,
    /// Uploaded from system memory every time: the pictures and the pool
    /// their frames come from.
    Upload {
        pictures: Vec<Planes>,
        pool: FramePool,
    },
    /// Textures another device wrote, imported.
    #[cfg(windows)]
    Imported {
        textures: Vec<(wgpu::Texture, u64)>,
        format: takt_gpu::PlanarFormat,
        fence: takt_gpu::interop::dx12::Fence,
        color: takt_gpu::FrameColor,
        // The D3D11 textures stay alive as long as their D3D12 side is drawn.
        _shared: Vec<takt_gpu::interop::d3d11::SharedTexture>,
    },
}

fn upload_feed(kind: Kind, size: (u32, u32)) -> Feed {
    let color = kind.color();
    Feed::Upload {
        pictures: (0..RING)
            .map(|i| picture(kind, size, u32::try_from(i).unwrap_or(0)))
            .collect(),
        pool: FramePool::new(
            FrameMeta {
                width: size.0,
                height: size.1,
                format: kind.format(),
                primaries: color.primaries,
                transfer: color.transfer,
                range: color.range,
                pts: Timestamp::from_millis(0),
            },
            RING,
        ),
    }
}

#[cfg(windows)]
fn imported_feed(
    gpu: &Gpu,
    source: &mut takt_gpu::interop::d3d11::SharedSource,
    fence: &takt_gpu::interop::dx12::Fence,
    kind: Kind,
    size: (u32, u32),
) -> Result<Feed, String> {
    use takt_gpu::PlanarFormat;
    use takt_gpu::interop::dx12;
    let format = if kind.format() == PixelFormat::P010 {
        PlanarFormat::P010
    } else {
        PlanarFormat::Nv12
    };
    let mut textures = Vec::new();
    let mut shared = Vec::new();
    for step in 0..RING {
        let planes = picture(kind, size, u32::try_from(step).unwrap_or(0));
        let texture11 = source
            .texture(format, size.0, size.1)
            .map_err(|e| e.to_string())?;
        let texture = dx12::open_shared_texture(&gpu.device, texture11.handle(), format)
            .map_err(|e| e.to_string())?;
        let value = source
            .write(&texture11, &planes.view())
            .map_err(|e| e.to_string())?;
        textures.push((texture, value));
        shared.push(texture11);
    }
    Ok(Feed::Imported {
        textures,
        format,
        fence: fence.clone(),
        color: kind.color().into(),
        _shared: shared,
    })
}

/// Copies `planes` into a frame of the pool, row by row at the stride of
/// the frame: what a software decoder does when it writes its picture.
fn fill(frame: &mut takt_core::VideoFrame, planes: &Planes) {
    for (index, (data, stride)) in planes.planes.iter().enumerate() {
        let Some(target_stride) = frame.stride(index) else {
            continue;
        };
        let row = *stride as usize;
        if let Some(target) = frame.plane_mut(index) {
            for (to, from) in target.chunks_mut(target_stride).zip(data.chunks(row)) {
                to[..row].copy_from_slice(from);
            }
        }
    }
}

struct Stats {
    /// From the submission of a picture until the GPU was done with it.
    gpu: Vec<Duration>,
    /// From one picture to the next.
    interval: Vec<Duration>,
    late: u64,
    /// The longest time of making and submitting one picture on the CPU.
    cpu_worst: Duration,
    cpu_total: Duration,
}

fn percentile(sorted: &[Duration], per_mille: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    sorted[(sorted.len() - 1) * per_mille / 1000]
}

fn millis(d: Duration) -> String {
    format!("{:.2}", d.as_secs_f64() * 1000.0)
}

fn run() -> Result<(), String> {
    let args = parse()?;
    let gpu = pollster::block_on(Gpu::open(GpuOptions {
        fallback_adapter: args.fallback,
        ..Default::default()
    }))
    .map_err(|e| e.to_string())?;
    let info = gpu.info();
    eprintln!(
        "adapter {} ({:?}) | NV12 {} P010 {} 16 bit {}",
        info.adapter, info.backend, info.nv12, info.p010, info.sixteen_bit
    );
    if !info.sixteen_bit {
        return Err("the adapter has no 16-bit normalised textures (the HDR layer)".to_owned());
    }

    let format = wgpu::TextureFormat::Rgba8Unorm;
    let target = readback::target(&gpu.device, format, args.size);
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut compositor =
        Compositor::new(&gpu.device, format, wgpu::FilterMode::Linear, args.layers);

    #[cfg(windows)]
    let mut foreign = if args.source == Source::Imported {
        let luid = info
            .adapter_luid
            .ok_or("--source imported needs the DX12 backend")?;
        let source =
            takt_gpu::interop::d3d11::SharedSource::on_adapter(luid).map_err(|e| e.to_string())?;
        let fence = takt_gpu::interop::dx12::open_shared_fence(&gpu.device, source.fence_handle())
            .map_err(|e| e.to_string())?;
        Some((source, fence))
    } else {
        None
    };

    eprintln!("making the pictures of {} layers ...", args.layers);
    let mut feeds = Vec::new();
    for layer in 0..args.layers {
        let kind = Kind::of(layer);
        let slot = compositor.layer_mut(layer).ok_or("no such layer")?;
        slot.place(&gpu.queue, kind.placement(layer, args.size));
        let feed = if kind == Kind::Graphic {
            let planes = picture(kind, args.size, 0);
            slot.show_planes(&gpu.device, &gpu.queue, &planes.view(), kind.color().into())
                .map_err(|e| e.to_string())?;
            Feed::Still
        } else {
            match args.source {
                Source::Upload => upload_feed(kind, args.size),
                #[cfg(windows)]
                Source::Imported => {
                    let (source, fence) = foreign.as_mut().ok_or("no foreign device")?;
                    imported_feed(&gpu, source, fence, kind, args.size)?
                }
                #[cfg(not(windows))]
                Source::Imported => return Err("--source imported is Windows only".to_owned()),
            }
        };
        feeds.push(feed);
    }

    let mut presenter = if args.window {
        Some(window::Presenter::new(&gpu)?)
    } else {
        None
    };

    let period = Duration::from_secs(1) / args.fps;
    let mut stats = Stats {
        gpu: Vec::new(),
        interval: Vec::new(),
        late: 0,
        cpu_worst: Duration::ZERO,
        cpu_total: Duration::ZERO,
    };
    eprintln!(
        "{} layers of {}x{}, source {:?}, {} for {} s ...",
        args.layers,
        args.size.0,
        args.size.1,
        args.source,
        if args.window {
            "in a window with vsync".to_owned()
        } else {
            format!("{} pictures a second without a window", args.fps)
        },
        args.seconds
    );

    // The first pictures compile pipelines and fill caches: they run before the clock starts.
    let warm_up = 30_u64;
    let wanted = if args.window {
        u64::MAX
    } else {
        args.seconds * u64::from(args.fps)
    };
    let mut start = Instant::now();
    let mut last_done = start;
    let mut n = 0_u64;
    let mut measured = 0_u64;
    let mut refresh = period;
    loop {
        if n == warm_up {
            start = Instant::now();
            last_done = start;
            if let Some(p) = &presenter {
                refresh = p.refresh_period().unwrap_or(period);
            }
        }
        let counting = n >= warm_up;
        if counting && (measured >= wanted || start.elapsed() >= Duration::from_secs(args.seconds))
        {
            break;
        }
        if let Some(p) = presenter.as_mut()
            && !p.pump()
        {
            break;
        }
        let due = start + period * u32::try_from(measured).unwrap_or(u32::MAX);
        if counting && presenter.is_none() {
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
        }

        let begin = Instant::now();
        let step = usize::try_from(n % RING as u64).unwrap_or(0);
        for (layer, feed) in feeds.iter_mut().enumerate() {
            let slot = compositor.layer_mut(layer).ok_or("no such layer")?;
            let frame = match feed {
                Feed::Still => continue,
                Feed::Upload { pictures, pool } => {
                    let mut frame = pool
                        .acquire(Timestamp::from_millis(0))
                        .ok_or("the pool of a layer ran dry")?;
                    fill(frame.get_mut(), &pictures[step]);
                    GpuFrame::from_frame(frame).map_err(|e| e.to_string())?
                }
                #[cfg(windows)]
                Feed::Imported {
                    textures,
                    format,
                    fence,
                    color,
                    ..
                } => {
                    let (texture, value) = &textures[step];
                    takt_gpu::TextureFrame::new(texture.clone(), *format, args.size, *color)
                        .map_err(|e| e.to_string())?
                        .ready_when(takt_gpu::Ready::Fence {
                            fence: fence.clone(),
                            value: *value,
                        })
                        .into()
                }
            };
            slot.show(&gpu.device, &gpu.queue, &Arc::new(frame))
                .map_err(|e| e.to_string())?;
        }
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        compositor.render(&mut encoder, &gpu.queue, &target_view, wgpu::Color::BLACK);
        let surface = match presenter.as_mut() {
            Some(p) => Some(p.draw(&gpu.device, &mut encoder, &target_view)?),
            None => None,
        };
        gpu.queue.submit([encoder.finish()]);
        let submitted = Instant::now();
        if let Some(surface) = surface {
            surface.present();
        }
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| e.to_string())?;
        let done = Instant::now();

        if counting {
            let cpu = submitted - begin;
            stats.cpu_total += cpu;
            stats.cpu_worst = stats.cpu_worst.max(cpu);
            stats.gpu.push(done - submitted);
            let interval = done - last_done;
            if measured > 0 {
                stats.interval.push(interval);
            }
            let late = if presenter.is_some() {
                measured > 0 && interval > refresh * 3 / 2
            } else {
                done > due + period
            };
            stats.late += u64::from(late);
            measured += 1;
        }
        last_done = done;
        n += 1;
    }
    let elapsed = start.elapsed();

    stats.gpu.sort_unstable();
    stats.interval.sort_unstable();
    let frames = stats.gpu.len();
    if frames == 0 {
        return Err("no picture was measured".to_owned());
    }
    #[expect(clippy::cast_precision_loss, reason = "a count of pictures for a rate")]
    let rate = frames as f64 / elapsed.as_secs_f64();
    println!(
        "layers={} size={}x{} source={:?} window={} pictures={} seconds={:.2} rate={:.2} late={} \
         gpu_ms_median={} gpu_ms_p99={} gpu_ms_max={} interval_ms_p99={} interval_ms_max={} \
         cpu_ms_mean={} cpu_ms_max={} refresh_ms={}",
        args.layers,
        args.size.0,
        args.size.1,
        args.source,
        args.window,
        frames,
        elapsed.as_secs_f64(),
        rate,
        stats.late,
        millis(percentile(&stats.gpu, 500)),
        millis(percentile(&stats.gpu, 990)),
        millis(percentile(&stats.gpu, 1000)),
        millis(percentile(&stats.interval, 990)),
        millis(percentile(&stats.interval, 1000)),
        millis(stats.cpu_total / u32::try_from(frames).unwrap_or(u32::MAX)),
        millis(stats.cpu_worst),
        millis(if args.window { refresh } else { period }),
    );
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("composite-bench: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `--window`: the target also goes to a swapchain with vsync, scaled.
mod window {
    use std::sync::Arc;
    use std::time::Duration;

    use takt_gpu::{Gpu, wgpu};
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
                    .with_title("takt-gpu composite-bench")
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

    /// A window with a swapchain and the pass that scales the target into it.
    pub struct Presenter {
        event_loop: EventLoop<()>,
        app: App,
        surface: wgpu::Surface<'static>,
        pipeline: wgpu::RenderPipeline,
        bind: Option<wgpu::BindGroup>,
        sampler: wgpu::Sampler,
    }

    impl Presenter {
        pub fn new(gpu: &Gpu) -> Result<Self, String> {
            let mut event_loop = EventLoop::new().map_err(|e| format!("event loop: {e}"))?;
            let mut app = App::default();
            let _ = event_loop.pump_app_events(Some(Duration::ZERO), &mut app);
            let window = app.window.clone().ok_or("no window")?;
            let size = window.inner_size();
            let surface = gpu
                .instance
                .create_surface(window)
                .map_err(|e| format!("create_surface: {e}"))?;
            let mut config = surface
                .get_default_config(&gpu.adapter, size.width.max(1), size.height.max(1))
                .ok_or("the surface is not supported by the adapter")?;
            config.present_mode = wgpu::PresentMode::Fifo;
            surface.configure(&gpu.device, &config);

            let shader = gpu
                .device
                .create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("blit"),
                    source: wgpu::ShaderSource::Wgsl(BLIT.into()),
                });
            let pipeline = gpu
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
            let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
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

        /// The refresh period of the monitor the window is on.
        pub fn refresh_period(&self) -> Option<Duration> {
            let millihertz = self
                .app
                .window
                .as_ref()?
                .current_monitor()?
                .refresh_rate_millihertz()?;
            (millihertz > 0).then(|| Duration::from_secs(1000) / millihertz)
        }

        /// Records the scaling pass into `encoder` and hands back the picture
        /// of the swapchain to present after the submit.
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
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &*bind, &[]);
            pass.draw(0..3, 0..1);
            drop(pass);
            Ok(frame)
        }
    }
}
