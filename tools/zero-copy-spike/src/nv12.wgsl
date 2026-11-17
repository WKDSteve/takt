// SPDX-License-Identifier: GPL-3.0-or-later
// Luma and chroma plane of an NV12 or P010 picture to RGB (BT.709, limited range). `textureLoad` on purpose:
// both paths of the spike must produce the same bytes, a filter would only blur the comparison.
@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let xy = vec2<i32>(pos.xy);
    let y = textureLoad(luma, xy, 0).r;
    let uv = textureLoad(chroma, xy / 2, 0).rg;
    let c = vec3<f32>(y - 16.0 / 255.0, uv.x - 128.0 / 255.0, uv.y - 128.0 / 255.0);
    let rgb = vec3<f32>(
        1.16438 * c.x + 1.79274 * c.z,
        1.16438 * c.x - 0.21325 * c.y - 0.53291 * c.z,
        1.16438 * c.x + 2.11240 * c.y,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
