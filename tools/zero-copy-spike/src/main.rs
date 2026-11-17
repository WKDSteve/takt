// SPDX-License-Identifier: GPL-3.0-or-later
//! Spike `takt/spike-zero-copy-decoder-gpu-import`: FFmpeg decodes in hardware, and the picture reaches a wgpu
//! shader either without touching system memory (the D3D12 texture wrapped through wgpu-hal) or over system
//! memory (hwdownload plus upload). The texture is the decoder's own (`--decoder d3d12va`, on the device of
//! wgpu, no copy at all) or a shared one (`--decoder d3d11va`, one copy on the GPU). Headless: it renders into an offscreen target and prints
//! one line of numbers.
//!
//! ```text
//! zero-copy-spike <file> [--decoder d3d12va|d3d11va] [--mode zero-copy|cpu] [--sync strict|loose] [--frames N] [--fps F] [--verify]
//!                        [--validate] [--window]
//! zero-copy-spike --probe
//! ```
#![deny(unsafe_code)]

mod ffi;
mod interop;
mod window;

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    ZeroCopy,
    Cpu,
}

/// How rendering and decoding share a texture that is also a reference picture.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sync {
    /// The render of a picture is finished on the GPU before the decoder is fed again.
    Strict,
    /// The decoder runs on while the GPU may still sample the previous pictures.
    Loose,
}

struct Args {
    file: String,
    mode: Mode,
    d3d11: bool,
    sync: Sync,
    frames: u64,
    fps: Option<f64>,
    verify: bool,
    validate: bool,
    window: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        file: String::new(),
        mode: Mode::ZeroCopy,
        d3d11: false,
        sync: Sync::Strict,
        frames: u64::MAX,
        fps: None,
        verify: false,
        validate: false,
        window: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--probe" => return Ok(None),
            "--mode" => {
                args.mode = match value("--mode")?.as_str() {
                    "zero-copy" => Mode::ZeroCopy,
                    "cpu" => Mode::Cpu,
                    other => return Err(format!("unknown mode {other}")),
                }
            }
            "--decoder" => {
                args.d3d11 = match value("--decoder")?.as_str() {
                    "d3d12va" => false,
                    "d3d11va" => true,
                    other => return Err(format!("unknown decoder {other}")),
                }
            }
            "--sync" => {
                args.sync = match value("--sync")?.as_str() {
                    "strict" => Sync::Strict,
                    "loose" => Sync::Loose,
                    other => return Err(format!("unknown sync {other}")),
                }
            }
            "--frames" => {
                args.frames = value("--frames")?
                    .parse()
                    .map_err(|e| format!("--frames: {e}"))?;
            }
            "--fps" => {
                args.fps = Some(value("--fps")?.parse().map_err(|e| format!("--fps: {e}"))?);
            }
            "--verify" => args.verify = true,
            "--validate" => args.validate = true,
            "--window" => args.window = true,
            other if other.starts_with("--") => return Err(format!("unknown option {other}")),
            other => args.file = other.to_string(),
        }
    }
    if args.file.is_empty() {
        return Err(
            "usage: zero-copy-spike <file> [--decoder d3d12va|d3d11va] [--mode zero-copy|cpu] [--sync strict|loose] \
                    [--frames N] [--fps F] [--verify] [--validate] [--window] | --probe"
                .into(),
        );
    }
    Ok(Some(args))
}

/// What each backend of this machine offers for the import, without decoding anything.
fn probe() {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12 | wgpu::Backends::VULKAN,
        ..Default::default()
    });
    for adapter in instance.enumerate_adapters(wgpu::Backends::all()) {
        let info = adapter.get_info();
        let features = adapter.features();
        println!(
            "{:?} | {} | driver {} | NV12 {} | P010 {} | 16BIT_NORM {} | EXTERNAL_TEXTURE {}",
            info.backend,
            info.name,
            info.driver_info,
            features.contains(wgpu::Features::TEXTURE_FORMAT_NV12),
            features.contains(wgpu::Features::TEXTURE_FORMAT_P010),
            features.contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM),
            features.contains(wgpu::Features::EXTERNAL_TEXTURE),
        );
    }
}

struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    adapter_name: String,
}

fn open_gpu(validate: bool) -> Result<Gpu, String> {
    let flags = if validate {
        wgpu::InstanceFlags::VALIDATION | wgpu::InstanceFlags::DEBUG
    } else {
        wgpu::InstanceFlags::empty()
    };
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::DX12,
        flags,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .map_err(|e| format!("no DX12 adapter: {e}"))?;
    let wanted = wgpu::Features::TEXTURE_FORMAT_NV12
        | wgpu::Features::TEXTURE_FORMAT_P010
        | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;
    let missing = wanted - adapter.features();
    if !missing.is_empty() {
        return Err(format!("the adapter lacks {missing:?}"));
    }
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("zero-copy-spike"),
        required_features: wanted,
        ..Default::default()
    }))
    .map_err(|e| format!("request_device: {e}"))?;
    Ok(Gpu {
        adapter_name: adapter.get_info().name,
        instance,
        adapter,
        device,
        queue,
    })
}

struct Renderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
}

impl Renderer {
    fn new(device: &wgpu::Device, width: u32, height: u32, ten_bit: bool) -> Self {
        let shader = device.create_shader_module(wgpu::include_wgsl!("nv12.wgsl"));
        let plane = |binding| wgpu::BindGroupLayoutEntry {
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
            label: Some("planes"),
            entries: &[plane(0), plane(1)],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        // Ten bits of picture need more than eight bits of target, or the comparison measures rounding.
        let format = if ten_bit {
            wgpu::TextureFormat::Rgb10a2Unorm
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12 to rgb"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(format.into())],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            pipeline,
            layout,
            target,
            target_view,
        }
    }

    fn bind(
        &self,
        device: &wgpu::Device,
        luma: &wgpu::TextureView,
        chroma: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(luma),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(chroma),
                },
            ],
        })
    }

    fn draw(&self, encoder: &mut wgpu::CommandEncoder, planes: &wgpu::BindGroup) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.target_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, planes, &[]);
        pass.draw(0..3, 0..1);
    }

    /// Reads the target back and folds it into one number (FNV-1a), so two paths can be compared.
    fn checksum(&self, gpu: &Gpu) -> Result<u64, String> {
        let size = self.target.size();
        let row = size.width * 4; // 3840 * 4 is a multiple of 256; other widths are padded below
        let padded = row.div_ceil(256) * 256;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(padded) * u64::from(size.height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            self.target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: None,
                },
            },
            size,
        );
        gpu.queue.submit([encoder.finish()]);
        buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| format!("poll: {e}"))?;
        let data = buffer.slice(..).get_mapped_range();
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for line in data.chunks(padded as usize) {
            for byte in &line[..row as usize] {
                hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        Ok(hash)
    }
}

/// The decoder textures already wrapped, by resource pointer and array slice. The pool of the decoder is
/// small and fixed, so every wrap happens once.
#[derive(Default)]
struct Imported {
    by_resource: HashMap<(usize, u32), (wgpu::Texture, wgpu::BindGroup)>,
}

impl Imported {
    fn get(
        &mut self,
        gpu: &Gpu,
        renderer: &Renderer,
        frame: &ffi::Frame,
    ) -> Result<&(wgpu::Texture, wgpu::BindGroup), String> {
        let key = (frame.texture as usize, frame.subresource);
        if let std::collections::hash_map::Entry::Vacant(slot) = self.by_resource.entry(key) {
            let (format, luma_format, chroma_format) = plane_formats(frame.ten_bit, true);
            let size = wgpu::Extent3d {
                width: frame.res_width,
                height: frame.res_height,
                depth_or_array_layers: frame.array_size,
            };
            let texture = interop::import_texture(&gpu.device, frame.texture, format, size)?;
            let view = |aspect, format| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    format: Some(format),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    aspect,
                    base_array_layer: frame.subresource,
                    array_layer_count: Some(1),
                    ..Default::default()
                })
            };
            let bind = renderer.bind(
                &gpu.device,
                &view(wgpu::TextureAspect::Plane0, luma_format),
                &view(wgpu::TextureAspect::Plane1, chroma_format),
            );
            slot.insert((texture, bind));
        }
        Ok(&self.by_resource[&key])
    }
}

/// (planar format, luma plane, chroma plane); the planar format only exists for the import.
fn plane_formats(
    ten_bit: bool,
    planar: bool,
) -> (
    wgpu::TextureFormat,
    wgpu::TextureFormat,
    wgpu::TextureFormat,
) {
    use wgpu::TextureFormat as F;
    match (ten_bit, planar) {
        (false, true) => (F::NV12, F::R8Unorm, F::Rg8Unorm),
        (true, true) => (F::P010, F::R16Unorm, F::Rg16Unorm),
        (false, false) => (F::R8Unorm, F::R8Unorm, F::Rg8Unorm),
        (true, false) => (F::R16Unorm, F::R16Unorm, F::Rg16Unorm),
    }
}

/// The textures of the CPU path: two ordinary wgpu textures the downloaded planes are written into.
struct Uploaded {
    luma: wgpu::Texture,
    chroma: wgpu::Texture,
    bind: wgpu::BindGroup,
}

impl Uploaded {
    fn new(gpu: &Gpu, renderer: &Renderer, frame: &ffi::Frame) -> Self {
        let (_, luma_format, chroma_format) = plane_formats(frame.ten_bit, false);
        let make = |format, width, height| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("uploaded plane"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let luma = make(luma_format, frame.width, frame.height);
        let chroma = make(
            chroma_format,
            frame.width.div_ceil(2),
            frame.height.div_ceil(2),
        );
        let bind = renderer.bind(
            &gpu.device,
            &luma.create_view(&wgpu::TextureViewDescriptor::default()),
            &chroma.create_view(&wgpu::TextureViewDescriptor::default()),
        );
        Self { luma, chroma, bind }
    }

    fn write(&self, queue: &wgpu::Queue, planes: &ffi::Planes<'_>) {
        for (texture, data, stride) in [
            (&self.luma, planes.luma, planes.luma_stride),
            (&self.chroma, planes.chroma, planes.chroma_stride),
        ] {
            queue.write_texture(
                texture.as_image_copy(),
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: None,
                },
                texture.size(),
            );
        }
    }
}

#[derive(Default)]
struct Phases {
    decode: Duration,
    transfer: Duration,
    submit: Duration,
    wait: Duration,
}

fn run(args: &Args) -> Result<(), String> {
    let gpu = open_gpu(args.validate)?;
    let raw_device = interop::raw_d3d12_device(&gpu.device)?;
    // Loose sync keeps up to three pictures alive on the GPU; the pool of the decoder has to know.
    let mut decoder = ffi::Decoder::open(&args.file, raw_device, args.d3d11, 6)?;

    let mut presenter = if args.window {
        Some(window::Presenter::new(
            &gpu.instance,
            &gpu.adapter,
            &gpu.device,
        )?)
    } else {
        None
    };
    let mut renderer: Option<Renderer> = None;
    let mut imported = Imported::default();
    let mut uploaded: Option<Uploaded> = None;
    let mut in_flight: VecDeque<ffi::Frame> = VecDeque::new();
    let mut phases = Phases::default();
    let mut checksums: Vec<u64> = Vec::new();
    let mut described = false;
    let mut frames = 0_u64;

    let cpu_start = interop::process_cpu_seconds();
    let start = Instant::now();
    while frames < args.frames && presenter.as_mut().is_none_or(window::Presenter::pump) {
        let t = Instant::now();
        let Some(frame) = decoder.next_frame(args.mode == Mode::ZeroCopy)? else {
            break;
        };
        phases.decode += t.elapsed();
        if !described {
            described = true;
            eprintln!(
                "decoder {} ({}) | {}x{} in a {}x{} resource, {} | array size {} | resource flags {:#x}",
                decoder.name(),
                if args.d3d11 { "d3d11va" } else { "d3d12va" },
                frame.width,
                frame.height,
                frame.res_width,
                frame.res_height,
                if frame.ten_bit { "P010" } else { "NV12" },
                frame.array_size,
                frame.res_flags,
            );
        }
        let renderer = renderer.get_or_insert_with(|| {
            Renderer::new(&gpu.device, frame.width, frame.height, frame.ten_bit)
        });
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());

        let t = Instant::now();
        match args.mode {
            Mode::ZeroCopy => {
                let (texture, bind) = imported.get(&gpu, renderer, &frame)?;
                interop::queue_wait_for_decode(&gpu.queue, frame.fence, frame.fence_value)?;
                renderer.draw(&mut encoder, bind);
                // Hand the texture back in the state the decoder expects for its reference pictures:
                // PRESENT is D3D12_RESOURCE_STATE_COMMON in wgpu-hal.
                encoder.transition_resources(
                    std::iter::empty(),
                    std::iter::once(wgpu::TextureTransition {
                        texture,
                        selector: None,
                        state: wgpu::TextureUses::PRESENT,
                    }),
                );
            }
            Mode::Cpu => {
                let planes = decoder.download(&frame)?;
                let textures =
                    uploaded.get_or_insert_with(|| Uploaded::new(&gpu, renderer, &frame));
                textures.write(&gpu.queue, &planes);
                renderer.draw(&mut encoder, &textures.bind);
            }
        }
        phases.transfer += t.elapsed();

        let t = Instant::now();
        let swapchain_frame = match presenter.as_mut() {
            Some(p) => Some(p.draw(&gpu.device, &mut encoder, &renderer.target_view)?),
            None => None,
        };
        gpu.queue.submit([encoder.finish()]);
        if let Some(frame) = swapchain_frame {
            frame.present();
        }
        phases.submit += t.elapsed();

        let t = Instant::now();
        if args.mode == Mode::ZeroCopy && args.sync == Sync::Loose {
            in_flight.push_back(frame);
            if in_flight.len() > 3 {
                in_flight.pop_front();
            }
            gpu.device
                .poll(wgpu::PollType::Poll)
                .map_err(|e| format!("poll: {e}"))?;
        } else {
            gpu.device
                .poll(wgpu::PollType::wait_indefinitely())
                .map_err(|e| format!("poll: {e}"))?;
            drop(frame);
        }
        phases.wait += t.elapsed();

        if args.verify && (frames < 3 || frames.is_multiple_of(60)) {
            gpu.device
                .poll(wgpu::PollType::wait_indefinitely())
                .map_err(|e| format!("poll: {e}"))?;
            checksums.push(renderer.checksum(&gpu)?);
        }
        frames += 1;
        if let Some(fps) = args.fps {
            let due = start + Duration::from_secs_f64(frames as f64 / fps);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
        }
    }
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("poll: {e}"))?;
    let wall = start.elapsed().as_secs_f64();
    let cpu = interop::process_cpu_seconds() - cpu_start;
    in_flight.clear();
    let per_frame = |d: Duration| d.as_secs_f64() * 1e3 / frames.max(1) as f64;
    let fps = frames as f64 / wall;
    println!(
        "decoder={} mode={} sync={} window={} adapter=\"{}\" frames={frames} wall_s={wall:.2} fps={fps:.1} cpu_s={cpu:.2} \
         cpu_pct_of_one_core={:.1} ms_per_frame: decode={:.3} transfer={:.3} submit={:.3} wait={:.3}",
        if args.d3d11 { "d3d11va" } else { "d3d12va" },
        if args.mode == Mode::ZeroCopy {
            "zero-copy"
        } else {
            "cpu"
        },
        if args.sync == Sync::Strict {
            "strict"
        } else {
            "loose"
        },
        args.window,
        gpu.adapter_name,
        cpu / wall * 100.0,
        per_frame(phases.decode),
        per_frame(phases.transfer),
        per_frame(phases.submit),
        per_frame(phases.wait),
    );
    if args.verify {
        let list: Vec<String> = checksums.iter().map(|c| format!("{c:016x}")).collect();
        println!("checksums={}", list.join(","));
    }
    Ok(())
}

/// Warnings and errors of wgpu (and with `--validate` of the D3D12 debug layer) to stderr.
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

fn main() -> std::process::ExitCode {
    if log::set_logger(&StderrLog).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
    let result = match parse_args() {
        Ok(Some(args)) => run(&args),
        Ok(None) => {
            probe();
            Ok(())
        }
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zero-copy-spike: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
