// SPDX-License-Identifier: GPL-3.0-or-later
//
// NV12 (8-bit, BT.709, limited range) to R'G'B'. The constants and their
// derivation are in colour.rs, which is the CPU reference of this shader.
//
// One triangle covers the viewport; egui-wgpu sets the viewport to the rect of
// the paint callback, so the picture fills exactly that rect.

// The target format encodes to sRGB itself: hand it linear values then.
override srgb_target: bool = false;

@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma: texture_2d<f32>;
@group(0) @binding(2) var picture_sampler: sampler;

struct Varyings {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> Varyings {
    // (-1,-1), (3,-1), (-1,3): a triangle twice the size of the viewport.
    let x = f32((index << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(index & 2u) * 2.0 - 1.0;
    var out: Varyings;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    // Texture rows run downwards, clip space upwards.
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let low = c / 12.92;
    let high = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(high, low, c <= vec3<f32>(0.04045));
}

@fragment
fn fs_main(in: Varyings) -> @location(0) vec4<f32> {
    let y8 = textureSample(luma, picture_sampler, in.uv).r * 255.0;
    let uv8 = textureSample(chroma, picture_sampler, in.uv).rg * 255.0;
    let y = (y8 - 16.0) / 219.0;
    let cb = (uv8.x - 128.0) / 224.0;
    let cr = (uv8.y - 128.0) / 224.0;
    var rgb = clamp(
        vec3<f32>(
            y + 1.5748 * cr,
            y - 0.1873 * cb - 0.4681 * cr,
            y + 1.8556 * cb,
        ),
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    if srgb_target {
        rgb = srgb_to_linear(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}
