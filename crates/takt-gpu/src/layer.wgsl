// SPDX-License-Identifier: GPL-3.0-or-later
//
// One layer: the planes of a picture to R'G'B' for a BT.709 SDR target, into a rectangle of the target, with an
// opacity. The numbers of the uniform and what they mean are in colour.rs, which is the CPU reference of this
// shader: `Conversion::apply` does the same in the same order.
//
// A quad of four vertices (triangle strip). The player draws it over the whole viewport egui-wgpu sets to the rect
// of its paint callback; a compositor places it with `rect`.

struct Layer {
    // Clip space: x and y of the top left corner, then width and height (the height runs downwards).
    rect: vec4<f32>,
    // The visible picture in texels of plane 0 (xy) and of the chroma planes (zw).
    visible: vec4<f32>,
    // The rows of the affine matrix from the samples (y, cb, cr, 1) to R'G'B'.
    ycc0: vec4<f32>,
    ycc1: vec4<f32>,
    ycc2: vec4<f32>,
    // The rows of the matrix from the light of the source primaries to the light of BT.709.
    gamut0: vec4<f32>,
    gamut1: vec4<f32>,
    gamut2: vec4<f32>,
    // x: opacity, y: peak of the source in nits, z: peak of the target in nits.
    light: vec4<f32>,
    // x: the layout of the samples, y: the transfer path, z: 1 when the target format encodes to sRGB itself.
    modes: vec4<u32>,
};

@group(0) @binding(0) var plane0: texture_2d<f32>;
@group(0) @binding(1) var plane1: texture_2d<f32>;
@group(0) @binding(2) var plane2: texture_2d<f32>;
@group(0) @binding(3) var picture_sampler: sampler;
@group(0) @binding(4) var<uniform> layer: Layer;

const SAMPLES_TWO_PLANES: u32 = 0u;
const SAMPLES_THREE_PLANES: u32 = 1u;
const SAMPLES_RGB: u32 = 2u;
const SAMPLES_RGBA: u32 = 3u;

const TRANSFER_SDR: u32 = 1u;
const TRANSFER_PQ: u32 = 2u;
const TRANSFER_HLG: u32 = 3u;

const SDR_GAMMA: f32 = 2.4;
const HLG_PEAK_NITS: f32 = 1000.0;
const HLG_GAMMA: f32 = 1.2;
const PQ_PEAK_NITS: f32 = 10000.0;

// SMPTE ST 2084.
const PQ_M1: f32 = 0.1593017578125;
const PQ_M2: f32 = 78.84375;
const PQ_C1: f32 = 0.8359375;
const PQ_C2: f32 = 18.8515625;
const PQ_C3: f32 = 18.6875;

// ARIB STD-B67 / BT.2100.
const HLG_A: f32 = 0.17883277;
const HLG_B: f32 = 0.28466892;
const HLG_C: f32 = 0.55991073;

struct Varyings {
    @builtin(position) position: vec4<f32>,
    // 0..1 across the visible picture, y downwards.
    @location(0) at: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> Varyings {
    let corner = vec2<f32>(f32(index & 1u), f32((index >> 1u) & 1u));
    var out: Varyings;
    out.position = vec4<f32>(
        layer.rect.x + corner.x * layer.rect.z,
        layer.rect.y - corner.y * layer.rect.w,
        0.0,
        1.0,
    );
    out.at = corner;
    return out;
}

// Where `at` is in a plane whose visible part is `visible` texels of `size`. Clamped to the centres of the outer
// visible texels: a filter never reads what lies beside the picture in a texture larger than it.
fn coords(at: vec2<f32>, visible: vec2<f32>, size: vec2<u32>) -> vec2<f32> {
    let last = max(visible - vec2<f32>(0.5), vec2<f32>(0.5));
    return clamp(at * visible, vec2<f32>(0.5), last) / vec2<f32>(size);
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let low = c / 12.92;
    let high = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(high, low, c <= vec3<f32>(0.04045));
}

fn pq_eotf(signal: f32) -> f32 {
    let p = pow(max(signal, 0.0), 1.0 / PQ_M2);
    return pow(max(p - PQ_C1, 0.0) / (PQ_C2 - PQ_C3 * p), 1.0 / PQ_M1);
}

fn pq_inverse_eotf(light: f32) -> f32 {
    let p = pow(max(light, 0.0), PQ_M1);
    return pow((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p), PQ_M2);
}

fn hlg_inverse_oetf(signal: f32) -> f32 {
    if signal <= 0.5 {
        return signal * signal / 3.0;
    }
    return (exp((signal - HLG_C) / HLG_A) + HLG_B) / 12.0;
}

// The EETF of Report ITU-R BT.2390 on the largest component, the others scaled with it.
fn tone_map(nits: vec3<f32>, source_peak: f32, target_peak: f32) -> vec3<f32> {
    if source_peak <= target_peak {
        return min(nits, vec3<f32>(target_peak));
    }
    let largest = max(nits.r, max(nits.g, nits.b));
    if largest <= 0.0 {
        return vec3<f32>(0.0);
    }
    let top = pq_inverse_eotf(source_peak / PQ_PEAK_NITS);
    let e1 = min(pq_inverse_eotf(largest / PQ_PEAK_NITS) / top, 1.0);
    let max_lum = pq_inverse_eotf(target_peak / PQ_PEAK_NITS) / top;
    let knee = 1.5 * max_lum - 0.5;
    var e2 = e1;
    if e1 > knee {
        let t = (e1 - knee) / (1.0 - knee);
        let t2 = t * t;
        let t3 = t2 * t;
        e2 = (2.0 * t3 - 3.0 * t2 + 1.0) * knee
            + (t3 - 2.0 * t2 + t) * (1.0 - knee)
            + (-2.0 * t3 + 3.0 * t2) * max_lum;
    }
    let mapped = PQ_PEAK_NITS * pq_eotf(e2 * top);
    return nits * (mapped / largest);
}

// Light of the source primaries (1 is the white of the target) to the R'G'B' of the target.
fn to_target(light: vec3<f32>) -> vec3<f32> {
    let rgb = vec3<f32>(
        dot(layer.gamut0.xyz, light),
        dot(layer.gamut1.xyz, light),
        dot(layer.gamut2.xyz, light),
    );
    return pow(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / SDR_GAMMA));
}

fn from_nits(nits: vec3<f32>) -> vec3<f32> {
    let mapped = tone_map(nits, layer.light.y, layer.light.z);
    return to_target(mapped / layer.light.z);
}

@fragment
fn fs_main(in: Varyings) -> @location(0) vec4<f32> {
    let first = textureSample(plane0, picture_sampler, coords(in.at, layer.visible.xy, textureDimensions(plane0)));
    let second = textureSample(plane1, picture_sampler, coords(in.at, layer.visible.zw, textureDimensions(plane1)));
    let third = textureSample(plane2, picture_sampler, coords(in.at, layer.visible.zw, textureDimensions(plane2)));

    var samples = first.rgb;
    var alpha = 1.0;
    let kind = layer.modes.x;
    if kind == SAMPLES_TWO_PLANES {
        samples = vec3<f32>(first.r, second.r, second.g);
    } else if kind == SAMPLES_THREE_PLANES {
        samples = vec3<f32>(first.r, second.r, third.r);
    } else if kind == SAMPLES_RGBA {
        alpha = first.a;
    }

    let v = vec4<f32>(samples, 1.0);
    var rgb = clamp(
        vec3<f32>(dot(layer.ycc0, v), dot(layer.ycc1, v), dot(layer.ycc2, v)),
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );

    let transfer = layer.modes.y;
    if transfer == TRANSFER_SDR {
        rgb = to_target(pow(rgb, vec3<f32>(SDR_GAMMA)));
    } else if transfer == TRANSFER_PQ {
        rgb = from_nits(PQ_PEAK_NITS * vec3<f32>(pq_eotf(rgb.r), pq_eotf(rgb.g), pq_eotf(rgb.b)));
    } else if transfer == TRANSFER_HLG {
        let scene = vec3<f32>(hlg_inverse_oetf(rgb.r), hlg_inverse_oetf(rgb.g), hlg_inverse_oetf(rgb.b));
        let luma = dot(vec3<f32>(0.2627, 0.6780, 0.0593), scene);
        rgb = from_nits(scene * (HLG_PEAK_NITS * pow(max(luma, 0.0), HLG_GAMMA - 1.0)));
    }

    // The target format encodes to sRGB itself: hand it linear values then.
    if layer.modes.z == 1u {
        rgb = srgb_to_linear(rgb);
    }
    // Premultiplied: the pipeline blends with (one, one minus source alpha).
    let coverage = alpha * layer.light.x;
    return vec4<f32>(rgb * coverage, coverage);
}
