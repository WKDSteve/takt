// SPDX-License-Identifier: GPL-3.0-or-later
//! The shader against its CPU reference, the reference pictures, the import
//! of a foreign texture against the upload of its planes, and four layers
//! against a blend computed by hand.
//!
//! The tests whose name begins with `gpu_` render on an adapter and are
//! ignored unless asked for:
//!
//! ```text
//! TAKT_GPU_TESTS=1 cargo test -p takt-gpu gpu -- --ignored
//! ```
//!
//! `TAKT_GPU_ADAPTER=fallback` asks for the software adapter (WARP of DX12
//! under Windows): the CI runners have no GPU, and WARP renders exactly
//! enough. The reference pictures in `tests/data` are compared with the CPU
//! reference in a test that runs everywhere.

use std::path::PathBuf;

use takt_core::{ColorDescription, ColorPrimaries, ColorRange, PixelFormat, TransferFunction};
use takt_gpu::colour::to_8bit;
use takt_gpu::{
    Compositor, Conversion, FrameColor, Gpu, GpuOptions, LayerRenderer, Placement, PlanesView,
    ToneMap, readback, wgpu,
};

/// The format of the target: not sRGB, so the bytes come back as drawn.
const TARGET: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Whether the GPU tests are asked for; says SKIP when not.
fn gpu_wanted() -> bool {
    if std::env::var("TAKT_GPU_TESTS").as_deref() == Ok("1") {
        return true;
    }
    println!("SKIP gpu test: set TAKT_GPU_TESTS=1 to render on a GPU adapter");
    false
}

fn gpu() -> Gpu {
    let options = GpuOptions {
        fallback_adapter: std::env::var("TAKT_GPU_ADAPTER").as_deref() == Ok("fallback"),
        ..Default::default()
    };
    let gpu = pollster::block_on(Gpu::open(options)).expect("a GPU adapter");
    let info = gpu.info();
    println!(
        "gpu test on {:?} ({:?}), NV12 {}, P010 {}, 16 bit {}",
        info.adapter, info.backend, info.nv12, info.p010, info.sixteen_bit
    );
    gpu
}

/// A picture in system memory: its format, size and planes with strides.
struct Picture {
    format: PixelFormat,
    size: (u32, u32),
    planes: Vec<(Vec<u8>, u32)>,
    color: FrameColor,
}

impl Picture {
    fn view(&self) -> PlanesView<'_> {
        let planes: Vec<(&[u8], u32)> = self.planes.iter().map(|(d, s)| (&d[..], *s)).collect();
        PlanesView::new(self.format, self.size.0, self.size.1, &planes).unwrap()
    }

    /// The same rows read as a picture of the top left `size` pixels.
    #[cfg(windows)]
    fn cropped(&self, size: (u32, u32)) -> PlanesView<'_> {
        let planes: Vec<(&[u8], u32)> = self.planes.iter().map(|(d, s)| (&d[..], *s)).collect();
        PlanesView::new(self.format, size.0, size.1, &planes).unwrap()
    }
}

fn color(primaries: ColorPrimaries, transfer: TransferFunction) -> FrameColor {
    ColorDescription {
        primaries,
        transfer,
        range: ColorRange::Limited,
    }
    .into()
}

/// The seven 75 % colour bars of BT.709 and black (two pixels each, so each
/// bar has its own chroma) above a grey wedge of eight steps, as NV12 in
/// planes with `pad` bytes of 255 after every row.
fn bars_bt709(pad: u32) -> Picture {
    const BARS: [(u8, u8, u8); 8] = [
        (180, 128, 128),
        (168, 44, 136),
        (145, 147, 44),
        (133, 63, 52),
        (63, 193, 204),
        (51, 109, 212),
        (28, 212, 120),
        (16, 128, 128),
    ];
    let (w, h) = (16_u32, 8_u32);
    let stride = w + pad;
    let mut y = vec![255_u8; (stride * h) as usize];
    let mut uv = vec![255_u8; (stride * h / 2) as usize];
    for row in 0..h {
        for col in 0..w {
            let value = if row < h / 2 {
                BARS[(col / 2) as usize].0
            } else {
                16 + u8::try_from(col / 2).unwrap() * 31 // 16 .. 233
            };
            y[(row * stride + col) as usize] = value;
        }
    }
    for row in 0..h / 2 {
        for pair in 0..w / 2 {
            let (u, v) = if row < h / 4 {
                let bar = BARS[pair as usize];
                (bar.1, bar.2)
            } else {
                (128, 128)
            };
            uv[(row * stride + pair * 2) as usize] = u;
            uv[(row * stride + pair * 2 + 1) as usize] = v;
        }
    }
    Picture {
        format: PixelFormat::Nv12,
        size: (w, h),
        planes: vec![(y, stride), (uv, stride)],
        color: color(ColorPrimaries::Bt709, TransferFunction::Bt709),
    }
}

/// R'G'B' (0..=1) to 10-bit limited Y'CbCr codes with the matrix of BT.2020
/// (non-constant luminance): the forward direction, written out here so the
/// test does not lean on the code it tests.
fn ycc10_bt2020(rgb: [f64; 3]) -> [u16; 3] {
    let (kr, kb) = (0.2627, 0.0593);
    let y = kr * rgb[0] + (1.0 - kr - kb) * rgb[1] + kb * rgb[2];
    let cb = (rgb[2] - y) / (2.0 * (1.0 - kb));
    let cr = (rgb[0] - y) / (2.0 * (1.0 - kr));
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "10-bit codes, 64..=960"
    )]
    let code = |v: f64| v.round() as u16;
    [
        code(64.0 + 876.0 * y),
        code(512.0 + 896.0 * cb),
        code(512.0 + 896.0 * cr),
    ]
}

/// The pixels of an HDR test picture as 10-bit codes, 16 by 8: bars of
/// white, the three primaries and their mixes at the signal level `bar`
/// above a grey wedge of the signal levels `wedge`. Each bar and step is two
/// pixels wide and the chroma is constant over every 2x2 block.
fn hdr_codes(bar: f64, wedge: [f64; 8]) -> Vec<[u16; 3]> {
    const MIX: [[f64; 3]; 8] = [
        [1.0, 1.0, 1.0],
        [1.0, 1.0, 0.0],
        [0.0, 1.0, 1.0],
        [0.0, 1.0, 0.0],
        [1.0, 0.0, 1.0],
        [1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, 0.0],
    ];
    let mut out = Vec::new();
    for row in 0..8_usize {
        for col in 0..16_usize {
            let rgb = if row < 4 {
                MIX[col / 2].map(|c| c * bar)
            } else {
                [wedge[col / 2]; 3]
            };
            out.push(ycc10_bt2020(rgb));
        }
    }
    out
}

/// `codes` (16 by 8) as P010: the 10 bits in the high bits of little-endian
/// words, rows padded by `pad` bytes of 255.
fn p010(codes: &[[u16; 3]], pad: u32, color: FrameColor) -> Picture {
    let (w, h) = (16_u32, 8_u32);
    let stride = w * 2 + pad;
    let mut y = vec![255_u8; (stride * h) as usize];
    let mut uv = vec![255_u8; (stride * h / 2) as usize];
    let put = |plane: &mut [u8], at: u32, code: u16| {
        plane[at as usize..at as usize + 2].copy_from_slice(&(code << 6).to_le_bytes());
    };
    for row in 0..h {
        for col in 0..w {
            let code = codes[(row * w + col) as usize];
            put(&mut y, row * stride + col * 2, code[0]);
            if row % 2 == 0 && col % 2 == 0 {
                let at = (row / 2) * stride + (col / 2) * 4;
                put(&mut uv, at, code[1]);
                put(&mut uv, at + 2, code[2]);
            }
        }
    }
    Picture {
        format: PixelFormat::P010,
        size: (w, h),
        planes: vec![(y, stride), (uv, stride)],
        color,
    }
}

/// `codes` (16 by 8) as three planes with the 10 bits in the low bits.
fn yuv420p10(codes: &[[u16; 3]], color: FrameColor) -> Picture {
    let (w, h) = (16_u32, 8_u32);
    let mut planes = [
        vec![0_u8; (w * 2 * h) as usize],
        vec![0_u8; (w * h / 2) as usize],
        vec![0_u8; (w * h / 2) as usize],
    ];
    for row in 0..h {
        for col in 0..w {
            let code = codes[(row * w + col) as usize];
            let at = (row * w * 2 + col * 2) as usize;
            planes[0][at..at + 2].copy_from_slice(&code[0].to_le_bytes());
            if row % 2 == 0 && col % 2 == 0 {
                let at = ((row / 2) * w + (col / 2) * 2) as usize;
                planes[1][at..at + 2].copy_from_slice(&code[1].to_le_bytes());
                planes[2][at..at + 2].copy_from_slice(&code[2].to_le_bytes());
            }
        }
    }
    let [y, u, v] = planes;
    Picture {
        format: PixelFormat::Yuv420P10,
        size: (w, h),
        planes: vec![(y, w * 2), (u, w), (v, w)],
        color,
    }
}

/// PQ: bars at 203 nits (58 % of the signal), a wedge from black over 100,
/// 203 and 1000 nits up to 4000 nits, which is above the peak of the source.
fn hdr_pq() -> Vec<[u16; 3]> {
    hdr_codes(
        0.5807,
        [0.0, 0.15, 0.30, 0.5081, 0.5807, 0.68, 0.7518, 0.9026],
    )
}

/// HLG: bars at 75 % of the signal (the reference white), a wedge over the
/// whole signal.
fn hdr_hlg() -> Vec<[u16; 3]> {
    hdr_codes(0.75, [0.0, 0.10, 0.25, 0.38, 0.50, 0.75, 0.90, 1.0])
}

/// One sample of plane `plane` at `x, y` of that plane, 0..=1.
fn sample(view: &PlanesView<'_>, plane: usize, x: u32, y: u32, component: u32) -> f32 {
    let p = view.plane(plane).unwrap();
    let wide = matches!(
        p.shape.format,
        wgpu::TextureFormat::R16Unorm | wgpu::TextureFormat::Rg16Unorm
    );
    let bytes = if wide { 2 } else { 1 };
    let at = (y * p.stride + (x * (p.shape.texel_bytes() / bytes) + component) * bytes) as usize;
    if wide {
        f32::from(u16::from_le_bytes([p.data[at], p.data[at + 1]])) / 65535.0
    } else {
        f32::from(p.data[at]) / 255.0
    }
}

/// The CPU reference of every pixel (nearest chroma: pixel x, y of a 4:2:0
/// picture uses the pair x/2, y/2).
fn reference(view: &PlanesView<'_>, color: FrameColor) -> Vec<[u8; 3]> {
    let conversion = Conversion::new(view.format(), color, ToneMap::default()).unwrap();
    let (w, h) = view.size();
    let mut out = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let texel = match view.format() {
                PixelFormat::Nv12 | PixelFormat::P010 => [
                    sample(view, 0, x, y, 0),
                    sample(view, 1, x / 2, y / 2, 0),
                    sample(view, 1, x / 2, y / 2, 1),
                ],
                PixelFormat::Yuv420P10 => [
                    sample(view, 0, x, y, 0),
                    sample(view, 1, x / 2, y / 2, 0),
                    sample(view, 2, x / 2, y / 2, 0),
                ],
                _ => [
                    sample(view, 0, x, y, 0),
                    sample(view, 0, x, y, 1),
                    sample(view, 0, x, y, 2),
                ],
            };
            out.push(conversion.apply(texel).map(to_8bit));
        }
    }
    out
}

/// Draws `draw` into a target of `size` cleared to red and reads it back.
fn read_back(gpu: &Gpu, size: (u32, u32), draw: impl FnOnce(&mut wgpu::RenderPass<'_>)) -> Vec<u8> {
    let target = readback::target(&gpu.device, TARGET, size);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("gpu test"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::RED),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        draw(&mut pass);
    }
    gpu.queue.submit([encoder.finish()]);
    readback::read(&gpu.device, &gpu.queue, &target).unwrap()
}

/// Renders `view` at its own size, one texel a pixel.
fn render(
    gpu: &Gpu,
    view: &PlanesView<'_>,
    color: FrameColor,
    filter: wgpu::FilterMode,
) -> Vec<u8> {
    let renderer = LayerRenderer::new(&gpu.device, TARGET, filter);
    let mut slot = renderer.slot(&gpu.device);
    slot.show_planes(&gpu.device, &gpu.queue, view, color)
        .unwrap();
    read_back(gpu, view.size(), |pass| slot.draw(pass))
}

fn assert_close(name: &str, got: &[u8], want: &[[u8; 3]], tolerance: u8) {
    assert_eq!(got.len(), want.len() * 4, "{name}: size");
    for (i, (pixel, want)) in got.chunks(4).zip(want).enumerate() {
        for c in 0..3 {
            assert!(
                pixel[c].abs_diff(want[c]) <= tolerance,
                "{name}: pixel {i} channel {c}: {} is not {}",
                pixel[c],
                want[c]
            );
        }
        assert_eq!(pixel[3], 255, "{name}: pixel {i} is opaque");
    }
}

// ---------------------------------------------------------------------------
// Reference pictures
// ---------------------------------------------------------------------------

/// The reference pictures: a name, the picture, and what it shows.
fn reference_cases() -> Vec<(&'static str, Picture, &'static str)> {
    let bt2020 = |transfer| color(ColorPrimaries::Bt2020, transfer);
    let mut sd = bars_bt709(0);
    sd.color = color(ColorPrimaries::Bt601Pal, TransferFunction::Bt709);
    vec![
        (
            "bt709",
            bars_bt709(0),
            "BT.709 SDR, 8 bit limited: 75 % bars above a grey wedge, shown as it is",
        ),
        (
            "bt601-pal",
            sd,
            "the same codes read as BT.601 with the primaries of PAL: another matrix, mapped to BT.709",
        ),
        (
            "bt2020-pq",
            p010(&hdr_pq(), 0, bt2020(TransferFunction::Pq)),
            "BT.2020 PQ, 10 bit limited: bars at 203 nits above a wedge up to 4000 nits, tone mapped to SDR",
        ),
        (
            "bt2020-hlg",
            p010(&hdr_hlg(), 0, bt2020(TransferFunction::Hlg)),
            "BT.2020 HLG, 10 bit limited: bars at 75 % above a wedge over the whole signal, tone mapped to SDR",
        ),
    ]
}

fn reference_file(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(format!("reference-{name}.ppm"))
}

/// A plain PPM (P3): text, so a diff of a reference picture can be read.
fn ppm(size: (u32, u32), pixels: &[[u8; 3]], comment: &str) -> String {
    let mut out = format!("P3\n# {comment}\n{} {}\n255\n", size.0, size.1);
    for row in pixels.chunks(size.0 as usize) {
        let line: Vec<String> = row
            .iter()
            .map(|p| format!("{:3} {:3} {:3}", p[0], p[1], p[2]))
            .collect();
        out.push_str(&line.join("  "));
        out.push('\n');
    }
    out
}

fn read_ppm(text: &str) -> ((u32, u32), Vec<[u8; 3]>) {
    let mut numbers = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .flat_map(str::split_whitespace);
    assert_eq!(numbers.next(), Some("P3"));
    let mut next = || numbers.next().unwrap().parse::<u32>().unwrap();
    let size = (next(), next());
    assert_eq!(next(), 255);
    let pixels = (0..size.0 * size.1)
        .map(|_| [next(), next(), next()].map(|v| u8::try_from(v).unwrap()))
        .collect();
    (size, pixels)
}

fn load_reference(name: &str) -> ((u32, u32), Vec<[u8; 3]>) {
    let file = reference_file(name);
    let text = std::fs::read_to_string(&file)
        .unwrap_or_else(|e| panic!("{}: {e} (TAKT_GPU_BLESS=1 writes it)", file.display()));
    read_ppm(&text)
}

/// The reference pictures are what the CPU reference computes. They are in
/// the repository so that a change of the colour math shows up as a diff of
/// pictures, and is not absorbed by a test that compares the code with
/// itself. `TAKT_GPU_BLESS=1` writes them anew.
#[test]
fn the_reference_pictures_are_what_the_cpu_reference_computes() {
    let bless = std::env::var("TAKT_GPU_BLESS").as_deref() == Ok("1");
    for (name, picture, what) in reference_cases() {
        let computed = reference(&picture.view(), picture.color);
        if bless {
            std::fs::write(reference_file(name), ppm(picture.size, &computed, what)).unwrap();
            continue;
        }
        let (size, stored) = load_reference(name);
        assert_eq!(size, picture.size, "{name}");
        // One step for the last bit of `powf` on another platform.
        for (i, (got, want)) in computed.iter().zip(&stored).enumerate() {
            for c in 0..3 {
                assert!(
                    got[c].abs_diff(want[c]) <= 1,
                    "{name}: pixel {i} channel {c}: computed {} but the reference picture says {}",
                    got[c],
                    want[c]
                );
            }
        }
    }
}

/// Points of the reference pictures that follow from the standards, not from
/// the code: what a wrong constant would move.
#[test]
fn the_reference_pictures_hold_the_values_the_standards_give() {
    let at = |pixels: &[[u8; 3]], col: usize, row: usize| pixels[row * 16 + col];
    let near = |got: [u8; 3], want: [u8; 3], tolerance: u8, what: &str| {
        for c in 0..3 {
            assert!(
                got[c].abs_diff(want[c]) <= tolerance,
                "{what}: {got:?} is not {want:?}"
            );
        }
    };

    let (_, sdr) = load_reference("bt709");
    near(at(&sdr, 0, 0), [191, 191, 191], 2, "75 % white");
    near(at(&sdr, 10, 0), [191, 0, 0], 2, "75 % red");
    near(at(&sdr, 14, 0), [0, 0, 0], 0, "black");
    near(at(&sdr, 14, 4), [255, 255, 255], 2, "the top of the wedge");

    // PQ, tone mapped from 1000 to 203 nits. The curve of BT.2390 leaves everything below its knee as it is;
    // the knee is at 1.5 * 0.7724 - 0.5 = 0.6586 of the signal range of the source, about 88 nits. So 1 nit
    // (signal 0.15) and 10 nits (signal 0.30) come out as (nits / 203)^(1/2.4): 28 and 73 of 255.
    // 203 nits are above the knee. With T = 1/3 the spline gives 0.7387 of the range, a signal of 0.5553,
    // 158 nits: (158 / 203)^(1/2.4) = 0.90, 230 of 255. 1000 nits, the peak of the source, and 4000 nits above
    // it both end at white. The primaries of BT.2020 lie outside BT.709: red keeps no green and no blue.
    let (_, pq) = load_reference("bt2020-pq");
    near(at(&pq, 0, 4), [0, 0, 0], 0, "PQ black");
    near(at(&pq, 2, 4), [28, 28, 28], 2, "PQ 1 nit");
    near(at(&pq, 4, 4), [73, 73, 73], 2, "PQ 10 nits");
    near(at(&pq, 8, 4), [230, 230, 230], 2, "PQ 203 nits");
    near(at(&pq, 0, 0), [230, 230, 230], 2, "PQ 203 nits as a bar");
    near(at(&pq, 12, 4), [255, 255, 255], 0, "PQ 1000 nits");
    near(at(&pq, 14, 4), [255, 255, 255], 0, "PQ 4000 nits");
    let red = at(&pq, 10, 0);
    assert!(
        red[0] == 255 && red[1] == 0 && red[2] == 0,
        "PQ red: {red:?}"
    );
    // The wedge never falls, and grey stays grey.
    let (_, hlg) = load_reference("bt2020-hlg");
    for (name, picture) in [("pq", &pq), ("hlg", &hlg)] {
        for col in (2..16).step_by(2) {
            let (a, b) = (at(picture, col - 2, 4), at(picture, col, 4));
            assert!(b[0] >= a[0], "{name} wedge at {col}: {a:?} then {b:?}");
            assert!(
                b[0].abs_diff(b[1]) <= 1 && b[1].abs_diff(b[2]) <= 1,
                "{name}: grey"
            );
        }
    }
    // HLG on its nominal display of 1000 nits. A signal of 50 % is scene light 1/12, which the OOTF (gamma
    // 1.2) puts at 1000 * (1/12)^1.2 = 50.7 nits, below the knee: (50.7 / 203)^(1/2.4) = 0.56, 143 of 255.
    // A signal of 75 % is the reference white, 203 nits (BT.2408): the same 230 as 203 nits of PQ, by another
    // curve. 100 % is the peak.
    near(at(&hlg, 8, 4), [143, 143, 143], 2, "HLG 50 %");
    near(at(&hlg, 10, 4), [230, 230, 230], 2, "HLG 75 %");
    near(at(&hlg, 0, 0), [230, 230, 230], 2, "HLG 75 % as a bar");
    near(at(&hlg, 14, 4), [255, 255, 255], 0, "HLG 100 %");
    near(at(&hlg, 0, 4), [0, 0, 0], 0, "HLG black");
}

#[test]
#[ignore = "SKIP: renders on a GPU adapter; TAKT_GPU_TESTS=1 cargo test -p takt-gpu gpu -- --ignored (CI: Windows with WARP)"]
fn gpu_the_shader_draws_the_reference_pictures() {
    if !gpu_wanted() {
        return;
    }
    let gpu = gpu();
    let sixteen_bit = gpu.info().sixteen_bit;
    for (name, picture, _) in reference_cases() {
        if picture.format == PixelFormat::P010 && !sixteen_bit {
            println!("SKIP {name}: the adapter has no 16-bit normalised textures");
            continue;
        }
        let (_, want) = load_reference(name);
        let got = render(
            &gpu,
            &picture.view(),
            picture.color,
            wgpu::FilterMode::Nearest,
        );
        // 2 of 255 for the float path of the GPU against the rounding of the CPU reference; the HDR paths go
        // through three power functions and get one step more.
        let tolerance = if picture.color.description.is_hdr() {
            3
        } else {
            2
        };
        assert_close(name, &got, &want, tolerance);
    }
}

#[test]
#[ignore = "SKIP: renders on a GPU adapter; TAKT_GPU_TESTS=1 cargo test -p takt-gpu gpu -- --ignored (CI: Windows with WARP)"]
fn gpu_padded_rows_and_three_planes_draw_the_same_picture() {
    if !gpu_wanted() {
        return;
    }
    let gpu = gpu();
    // Padded rows of NV12: the padding is not a pixel.
    let (tight, padded) = (bars_bt709(0), bars_bt709(13));
    let want = render(&gpu, &tight.view(), tight.color, wgpu::FilterMode::Nearest);
    let got = render(
        &gpu,
        &padded.view(),
        padded.color,
        wgpu::FilterMode::Nearest,
    );
    assert_eq!(got, want, "padded NV12");
    if !gpu.info().sixteen_bit {
        println!("SKIP 10 bit: the adapter has no 16-bit normalised textures");
        return;
    }
    // 10 bit in the high bits of two planes, padded, and in the low bits of three planes.
    let pq = color(ColorPrimaries::Bt2020, TransferFunction::Pq);
    let codes = hdr_pq();
    let two = p010(&codes, 0, pq);
    let want = render(&gpu, &two.view(), pq, wgpu::FilterMode::Nearest);
    let padded = p010(&codes, 14, pq);
    let got = render(&gpu, &padded.view(), pq, wgpu::FilterMode::Nearest);
    assert_eq!(got, want, "padded P010");
    let three = yuv420p10(&codes, pq);
    let got = render(&gpu, &three.view(), pq, wgpu::FilterMode::Nearest);
    let want: Vec<[u8; 3]> = want.chunks(4).map(|p| [p[0], p[1], p[2]]).collect();
    assert_close("three planes", &got, &want, 1);
}

/// Eight colours in two rows, in rows of `stride` bytes; the padding byte of
/// every pixel and the bytes after every row hold `filler`.
fn colours(stride: u32, filler: u8) -> (Picture, Vec<[u8; 3]>) {
    const COLOURS: [[u8; 3]; 8] = [
        [0, 0, 0],
        [255, 255, 255],
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [191, 191, 0],
        [16, 128, 235],
        [1, 2, 3],
    ];
    let mut pixels = vec![filler; (stride * 2) as usize];
    for (i, colour) in COLOURS.iter().enumerate() {
        let at = (i / 4) * stride as usize + (i % 4) * 4;
        pixels[at..at + 3].copy_from_slice(colour);
    }
    let picture = Picture {
        format: PixelFormat::Rgb0,
        size: (4, 2),
        planes: vec![(pixels, stride)],
        color: ColorDescription {
            range: ColorRange::Full,
            ..Default::default()
        }
        .into(),
    };
    (picture, COLOURS.to_vec())
}

#[test]
#[ignore = "SKIP: renders on a GPU adapter; TAKT_GPU_TESTS=1 cargo test -p takt-gpu gpu -- --ignored (CI: Windows with WARP)"]
fn gpu_rgb0_is_shown_as_it_is_and_opaque_whatever_the_padding_holds() {
    if !gpu_wanted() {
        return;
    }
    let gpu = gpu();
    // Tight rows with padding bytes of 0, and padded rows with 255: the padding byte must not become
    // transparency, the row padding not a pixel.
    for (name, stride, filler) in [("tight", 16_u32, 0_u8), ("padded", 64, 255)] {
        let (picture, want) = colours(stride, filler);
        let got = render(
            &gpu,
            &picture.view(),
            picture.color,
            wgpu::FilterMode::Nearest,
        );
        // No conversion on the way: at most one step of rounding. `assert_close` checks the alpha.
        assert_close(name, &got, &want, 1);
    }
}

// ---------------------------------------------------------------------------
// Compositing
// ---------------------------------------------------------------------------

/// A picture of one colour as `rgba8`, straight alpha.
fn plain(colour: [u8; 4]) -> Picture {
    Picture {
        format: PixelFormat::Rgba8,
        size: (2, 2),
        planes: vec![(colour.repeat(4), 8)],
        color: ColorDescription {
            range: ColorRange::Full,
            ..Default::default()
        }
        .into(),
    }
}

#[test]
#[ignore = "SKIP: renders on a GPU adapter; TAKT_GPU_TESTS=1 cargo test -p takt-gpu gpu -- --ignored (CI: Windows with WARP)"]
fn gpu_four_layers_are_blended_bottom_to_top_in_their_rectangles() {
    if !gpu_wanted() {
        return;
    }
    let gpu = gpu();
    let size = (8_u32, 8_u32);
    // Bottom to top: opaque blue everywhere, red at half opacity on the left half, a green whose own alpha is
    // a quarter on the top half, opaque white in the middle four pixels.
    let layers: [([u8; 4], [f32; 4], f32); 4] = [
        ([0, 0, 255, 255], [0.0, 0.0, 8.0, 8.0], 1.0),
        ([255, 0, 0, 255], [0.0, 0.0, 4.0, 8.0], 0.5),
        ([0, 255, 0, 64], [0.0, 0.0, 8.0, 4.0], 1.0),
        ([255, 255, 255, 255], [3.0, 3.0, 2.0, 2.0], 1.0),
    ];
    let mut compositor = Compositor::new(&gpu.device, TARGET, wgpu::FilterMode::Nearest, 4);
    assert_eq!(compositor.len(), 4);
    for (index, (colour, rect, opacity)) in layers.iter().enumerate() {
        let picture = plain(*colour);
        let slot = compositor.layer_mut(index).unwrap();
        slot.show_planes(&gpu.device, &gpu.queue, &picture.view(), picture.color)
            .unwrap();
        slot.place(&gpu.queue, Placement::pixels(size, *rect, *opacity));
    }
    let target = readback::target(&gpu.device, TARGET, size);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    compositor.render(&mut encoder, &gpu.queue, &view, wgpu::Color::BLACK);
    gpu.queue.submit([encoder.finish()]);
    let got = readback::read(&gpu.device, &gpu.queue, &target).unwrap();

    // The same blend by hand: source over, on the values of the target.
    let mut want = vec![[0.0_f32; 3]; 64];
    for (colour, rect, opacity) in layers {
        let coverage = f32::from(colour[3]) / 255.0 * opacity;
        for y in 0..8_u16 {
            for x in 0..8_u16 {
                let (fx, fy) = (f32::from(x) + 0.5, f32::from(y) + 0.5);
                let inside = fx > rect[0]
                    && fx < rect[0] + rect[2]
                    && fy > rect[1]
                    && fy < rect[1] + rect[3];
                if inside {
                    let pixel = &mut want[usize::from(y) * 8 + usize::from(x)];
                    for c in 0..3 {
                        let source = f32::from(colour[c]) / 255.0;
                        pixel[c] = source * coverage + pixel[c] * (1.0 - coverage);
                    }
                }
            }
        }
    }
    let want: Vec<[u8; 3]> = want.iter().map(|p| p.map(to_8bit)).collect();
    assert_close("four layers", &got, &want, 2);
    // Spot checks, so the reference above cannot be wrong in the same way.
    let at = |x: usize, y: usize| &got[(y * 8 + x) * 4..(y * 8 + x) * 4 + 3];
    assert_eq!(at(7, 7), [0, 0, 255], "only the bottom layer");
    assert_eq!(at(3, 3), [255, 255, 255], "the top layer covers");
    let left = at(0, 7);
    assert!(
        left[0].abs_diff(128) <= 2 && left[2].abs_diff(128) <= 2,
        "{left:?}"
    );
}

// ---------------------------------------------------------------------------
// The import of a foreign texture (Windows, DX12)
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod import {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use takt_gpu::interop::d3d11::{SharedSource, SharedTexture};
    use takt_gpu::interop::{ImportError, dx12};
    use takt_gpu::{GpuFrame, LayerSlot, PlanarFormat, Ready, TextureFrame};

    /// A picture of `format` whose every sample is different from its
    /// neighbours (a multiplicative hash of the position), in rows padded by
    /// `pad` bytes.
    fn noise(format: PlanarFormat, size: (u32, u32), pad: u32, seed: u32) -> Picture {
        let bytes = match format {
            PlanarFormat::Nv12 => 1,
            PlanarFormat::P010 => 2,
        };
        let stride = size.0 * bytes + pad;
        let fill = |rows: u32, plane: u32| {
            let mut data = vec![255_u8; (stride * rows) as usize];
            for row in 0..rows {
                for col in 0..size.0 {
                    let hash = (row * size.0 + col + seed)
                        .wrapping_mul(2_654_435_761)
                        .wrapping_add(plane * 97);
                    let at = (row * stride + col * bytes) as usize;
                    if bytes == 1 {
                        data[at] = hash.to_le_bytes()[2];
                    } else {
                        // Ten bits in the high bits of a word.
                        let code =
                            u16::from_le_bytes([hash.to_le_bytes()[1], hash.to_le_bytes()[2]]);
                        data[at..at + 2].copy_from_slice(&(code & 0xffc0).to_le_bytes());
                    }
                }
            }
            data
        };
        Picture {
            format: format.pixel_format(),
            size,
            planes: vec![(fill(size.1, 0), stride), (fill(size.1 / 2, 1), stride)],
            color: color(ColorPrimaries::Bt709, TransferFunction::Bt709),
        }
    }

    struct Foreign {
        source: SharedSource,
        fence: dx12::Fence,
    }

    impl Foreign {
        fn new(gpu: &Gpu) -> Self {
            let luid = gpu.info().adapter_luid.expect("a DX12 device has a LUID");
            let source = SharedSource::on_adapter(luid).unwrap();
            let fence = dx12::open_shared_fence(&gpu.device, source.fence_handle()).unwrap();
            Self { source, fence }
        }

        /// Writes `picture` into `shared` on the D3D11 device and wraps the
        /// texture the device of wgpu opened as a frame of `visible` size.
        fn frame(
            &mut self,
            shared: &SharedTexture,
            texture: &wgpu::Texture,
            picture: &Picture,
            visible: (u32, u32),
        ) -> TextureFrame {
            let value = self.source.write(shared, &picture.view()).unwrap();
            TextureFrame::new(texture.clone(), shared.format(), visible, picture.color)
                .unwrap()
                .ready_when(Ready::Fence {
                    fence: self.fence.clone(),
                    value,
                })
        }
    }

    fn render_frame(gpu: &Gpu, frame: TextureFrame, filter: wgpu::FilterMode) -> Vec<u8> {
        let frame = Arc::new(GpuFrame::from(frame));
        let renderer = LayerRenderer::new(&gpu.device, TARGET, filter);
        let mut slot = renderer.slot(&gpu.device);
        slot.show(&gpu.device, &gpu.queue, &frame).unwrap();
        read_back(gpu, frame.size(), |pass| slot.draw(pass))
    }

    fn formats(gpu: &Gpu) -> Vec<PlanarFormat> {
        let info = gpu.info();
        assert!(info.nv12, "DX12 always has NV12");
        let mut formats = vec![PlanarFormat::Nv12];
        if info.p010 && info.sixteen_bit {
            formats.push(PlanarFormat::P010);
        } else {
            println!("SKIP P010: the adapter lacks P010 or 16-bit normalised textures");
        }
        formats
    }

    /// The test the zero-copy spike ran by hand (`--verify`), kept: a texture
    /// another device wrote, imported, gives the very bytes the upload of
    /// the same planes gives.
    #[test]
    #[ignore = "SKIP: renders on a GPU adapter; TAKT_GPU_TESTS=1 cargo test -p takt-gpu gpu -- --ignored (CI: Windows with WARP)"]
    fn gpu_an_imported_texture_draws_the_bytes_the_upload_of_its_planes_draws() {
        if !gpu_wanted() {
            return;
        }
        let gpu = gpu();
        let mut foreign = Foreign::new(&gpu);
        for format in formats(&gpu) {
            for (size, pad) in [((64_u32, 36_u32), 0_u32), ((1920, 1080), 6)] {
                let shared = foreign.source.texture(format, size.0, size.1).unwrap();
                let texture =
                    dx12::open_shared_texture(&gpu.device, shared.handle(), format).unwrap();
                assert_eq!((texture.width(), texture.height()), size);
                assert_eq!(texture.format(), format.texture_format());
                // Two pictures after each other into the same texture: the second must replace the first.
                for seed in [1_u32, 77] {
                    let picture = noise(format, size, pad, seed);
                    for filter in [wgpu::FilterMode::Nearest, wgpu::FilterMode::Linear] {
                        let frame = foreign.frame(&shared, &texture, &picture, size);
                        let imported = render_frame(&gpu, frame, filter);
                        let uploaded = render(&gpu, &picture.view(), picture.color, filter);
                        assert_eq!(
                            readback::checksum(&imported),
                            readback::checksum(&uploaded),
                            "{format:?} {size:?} seed {seed} {filter:?}: import and upload differ"
                        );
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "SKIP: renders on a GPU adapter; TAKT_GPU_TESTS=1 cargo test -p takt-gpu gpu -- --ignored (CI: Windows with WARP)"]
    fn gpu_a_texture_larger_than_its_picture_shows_only_the_picture() {
        if !gpu_wanted() {
            return;
        }
        let gpu = gpu();
        let mut foreign = Foreign::new(&gpu);
        // The texture is 64 by 48, the picture its top left 40 by 30; what lies beside it is noise too.
        let (full, visible) = ((64_u32, 48_u32), (40_u32, 30_u32));
        let picture = noise(PlanarFormat::Nv12, full, 0, 5);
        let shared = foreign
            .source
            .texture(PlanarFormat::Nv12, full.0, full.1)
            .unwrap();
        let texture =
            dx12::open_shared_texture(&gpu.device, shared.handle(), PlanarFormat::Nv12).unwrap();
        for filter in [wgpu::FilterMode::Nearest, wgpu::FilterMode::Linear] {
            let frame = foreign.frame(&shared, &texture, &picture, visible);
            let imported = render_frame(&gpu, frame, filter);
            // The same rows uploaded as a picture of 40 by 30: its textures end where the picture ends.
            let uploaded = render(&gpu, &picture.cropped(visible), picture.color, filter);
            assert_eq!(imported.len(), uploaded.len());
            // The two paths divide by different texture sizes: one step for the last bit of a weight.
            let worst = imported
                .iter()
                .zip(&uploaded)
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap();
            assert!(worst <= 1, "{filter:?}: the paths differ by {worst}");
        }
        // A frame cannot claim more than the texture has.
        let frame = |visible| {
            TextureFrame::new(texture.clone(), PlanarFormat::Nv12, visible, picture.color)
                .map(|f| f.visible())
        };
        assert_eq!(frame((64, 48)), Ok((64, 48)));
        assert_eq!(frame((65, 48)), Err(takt_gpu::FrameError::Visible));
        assert_eq!(frame((0, 48)), Err(takt_gpu::FrameError::Visible));
        assert_eq!(
            TextureFrame::new(texture.clone(), PlanarFormat::P010, visible, picture.color)
                .map(|f| f.visible()),
            Err(takt_gpu::FrameError::Format)
        );
        assert_eq!(
            TextureFrame::new(texture.clone(), PlanarFormat::Nv12, visible, picture.color)
                .unwrap()
                .in_layer(1)
                .map(|f| f.layer()),
            Err(takt_gpu::FrameError::Layer)
        );
    }

    /// Counts its drops: the guard of a frame.
    struct Returned(Arc<AtomicUsize>);

    impl Drop for Returned {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn pass(gpu: &Gpu, slot: &mut LayerSlot, target: &wgpu::TextureView) {
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        slot.before_pass(&mut encoder, &gpu.queue);
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("gpu test"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations::default(),
                })],
                ..Default::default()
            });
            slot.draw(&mut pass);
        }
        gpu.queue.submit([encoder.finish()]);
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
    }

    #[test]
    #[ignore = "SKIP: renders on a GPU adapter; TAKT_GPU_TESTS=1 cargo test -p takt-gpu gpu -- --ignored (CI: Windows with WARP)"]
    fn gpu_a_frame_is_given_back_only_after_the_gpu_is_done_with_it() {
        if !gpu_wanted() {
            return;
        }
        let gpu = gpu();
        let mut foreign = Foreign::new(&gpu);
        let size = (64_u32, 36_u32);
        let returned = Arc::new(AtomicUsize::new(0));
        let mut frames = Vec::new();
        let mut shared = Vec::new();
        for seed in 0..2_u32 {
            let texture11 = foreign
                .source
                .texture(PlanarFormat::Nv12, size.0, size.1)
                .unwrap();
            let texture =
                dx12::open_shared_texture(&gpu.device, texture11.handle(), PlanarFormat::Nv12)
                    .unwrap();
            let picture = noise(PlanarFormat::Nv12, size, 0, seed);
            let frame = foreign
                .frame(&texture11, &texture, &picture, size)
                .returned_by(Returned(Arc::clone(&returned)));
            frames.push((Arc::new(GpuFrame::from(frame)), picture, texture));
            shared.push(texture11);
        }
        let renderer = LayerRenderer::new(&gpu.device, TARGET, wgpu::FilterMode::Nearest);
        let mut slot = renderer.slot(&gpu.device);
        let target = readback::target(&gpu.device, TARGET, size);
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let count = || returned.load(Ordering::SeqCst);

        // The first picture is shown and drawn; the caller lets go of it, the layer holds it.
        let (first, first_picture, first_texture) = frames.remove(0);
        slot.show(&gpu.device, &gpu.queue, &first).unwrap();
        // Showing the same frame again is not a replacement.
        slot.show(&gpu.device, &gpu.queue, &first).unwrap();
        drop(first);
        pass(&gpu, &mut slot, &view);
        assert_eq!((count(), slot.frames_on_their_way_back()), (0, 0));

        // The second picture takes its place. The pass that follows records the way back of the first
        // texture; the frame is still held while that work runs.
        let (second, ..) = frames.remove(0);
        slot.show(&gpu.device, &gpu.queue, &second).unwrap();
        assert_eq!((count(), slot.frames_on_their_way_back()), (0, 1));
        pass(&gpu, &mut slot, &view);
        assert_eq!((count(), slot.frames_on_their_way_back()), (0, 1));
        // The pass after that lets it go once the GPU is done.
        pass(&gpu, &mut slot, &view);
        assert_eq!((count(), slot.frames_on_their_way_back()), (1, 0));

        // Its owner writes the next picture into the texture that came back, and it draws right: the texture
        // was handed back in the state the other device expects.
        let third = noise(PlanarFormat::Nv12, size, 0, 9);
        assert_ne!(third.planes[0].0, first_picture.planes[0].0);
        let frame = foreign.frame(&shared[0], &first_texture, &third, size);
        let imported = render_frame(&gpu, frame, wgpu::FilterMode::Nearest);
        let uploaded = render(&gpu, &third.view(), third.color, wgpu::FilterMode::Nearest);
        assert_eq!(readback::checksum(&imported), readback::checksum(&uploaded));

        // A frame from system memory replaces a texture frame as well.
        slot.show_planes(&gpu.device, &gpu.queue, &third.view(), third.color)
            .unwrap();
        drop(second);
        pass(&gpu, &mut slot, &view);
        pass(&gpu, &mut slot, &view);
        assert_eq!((count(), slot.frames_on_their_way_back()), (2, 0));
    }

    #[test]
    #[ignore = "SKIP: renders on a GPU adapter; TAKT_GPU_TESTS=1 cargo test -p takt-gpu gpu -- --ignored (CI: Windows with WARP)"]
    fn gpu_a_handle_that_is_not_the_texture_it_is_said_to_be_is_refused() {
        if !gpu_wanted() {
            return;
        }
        let gpu = gpu();
        let mut foreign = Foreign::new(&gpu);
        let shared = foreign.source.texture(PlanarFormat::Nv12, 64, 36).unwrap();
        // The texture is NV12, the caller says P010.
        if gpu.info().p010 {
            let wrong = dx12::open_shared_texture(&gpu.device, shared.handle(), PlanarFormat::P010);
            assert!(
                matches!(wrong, Err(ImportError::Resource("of another format"))),
                "{wrong:?}"
            );
        }
        // The handle of the fence is not a texture, and the other way round.
        let fence_as_texture = dx12::open_shared_texture(
            &gpu.device,
            foreign.source.fence_handle(),
            PlanarFormat::Nv12,
        );
        assert!(
            matches!(fence_as_texture, Err(ImportError::Windows { .. })),
            "{fence_as_texture:?}"
        );
        let texture_as_fence = dx12::open_shared_fence(&gpu.device, shared.handle());
        assert!(
            matches!(texture_as_fence, Err(ImportError::Windows { .. })),
            "{texture_as_fence:?}"
        );
        // Planes that are not a picture of the texture are not written.
        let small = noise(PlanarFormat::Nv12, (32, 18), 0, 1);
        let refused = foreign.source.write(&shared, &small.view());
        assert!(matches!(refused, Err(ImportError::Size(_))), "{refused:?}");
        // Sizes a planar texture cannot have.
        for (w, h) in [(0, 36), (63, 36), (64, 35), (20_000, 36)] {
            let odd = foreign
                .source
                .texture(PlanarFormat::Nv12, w, h)
                .map(|t| t.size());
            assert!(matches!(odd, Err(ImportError::Size(_))), "{w}x{h}: {odd:?}");
        }
        // The fence counts the pictures written.
        let picture = noise(PlanarFormat::Nv12, (64, 36), 0, 1);
        let before = foreign.source.write(&shared, &picture.view()).unwrap();
        let after = foreign.source.write(&shared, &picture.view()).unwrap();
        assert_eq!(after, before + 1);
    }
}
