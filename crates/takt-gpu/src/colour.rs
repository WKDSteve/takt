// SPDX-License-Identifier: GPL-3.0-or-later
//! The colour conversion of a layer, on the CPU: the numbers the shader
//! (`layer.wgsl`) gets as its uniform, and the reference the shader is
//! compared with ([`Conversion::apply`] does in `f32` what the shader does).
//!
//! # What a layer does with a texel
//!
//! 1. **Y'CbCr to R'G'B'** with one affine matrix ([`Conversion::ycc`]). It
//!    holds the scale of the samples (8 bit, 10 bit in the high or the low
//!    bits of a word), the range (limited: luma 16..=235 and chroma 16..=240
//!    of 8 bit, ITU-R BT.709-6 table 4; full: 0..=255) and the coefficients
//!    of the matrix ([`MatrixCoefficients`]). An RGB frame has the identity
//!    here.
//! 2. **To the primaries and the range of the target**, which is BT.709 SDR:
//!
//!    | source | what happens |
//!    |---|---|
//!    | SDR, BT.709 primaries | nothing: the R'G'B' is shown as it is |
//!    | SDR, other primaries | gamma 2.4 to light (BT.1886), the gamut matrix, gamma 2.4 back |
//!    | PQ (SMPTE ST 2084) | the EOTF to nits, tone mapping, the gamut matrix, gamma 2.4 |
//!    | HLG (ARIB STD-B67) | the inverse OETF and the OOTF of BT.2100 (1000 nits, gamma 1.2) to nits, then as PQ |
//!
//! 3. **Tone mapping** ([`ToneMap`]) is the EETF of Report ITU-R BT.2390
//!    (section 5.4.1, the Hermite spline in PQ space) on the largest of the
//!    three components, the other two scaled with it: no component ends above
//!    the peak of the target, and the hue stays.
//!
//! The result is gamma-encoded R'G'B' for an SDR display. A target whose
//! format encodes to sRGB itself gets it linearised with the sRGB curve; the
//! small difference between that curve and gamma 2.4 belongs to the colour
//! management of the display, which is a later phase.
//!
//! # BT.709, limited range, 8 bit: the numbers
//!
//! ```text
//! Y  = (Y8 - 16) / 219          0 .. 1
//! Cb = (U8 - 128) / 224        -0.5 .. 0.5
//! Cr = (V8 - 128) / 224
//! R = Y + 1.5748 Cr
//! G = Y - 0.1873 Cb - 0.4681 Cr
//! B = Y + 1.8556 Cb
//! ```
//!
//! with Kr = 0.2126 and Kb = 0.0722: 1.5748 = 2 (1 - Kr), 1.8556 = 2 (1 - Kb),
//! 0.1873 = 2 Kb (1 - Kb) / Kg and 0.4681 = 2 Kr (1 - Kr) / Kg, Kg = 0.7152.

use takt_core::{ColorDescription, ColorPrimaries, ColorRange, PixelFormat, TransferFunction};

/// The coefficients that turn Y'CbCr into R'G'B' (H.273 `MatrixCoefficients`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MatrixCoefficients {
    /// BT.709: Kr 0.2126, Kb 0.0722.
    Bt709,
    /// BT.601 (both line systems): Kr 0.299, Kb 0.114.
    Bt601,
    /// BT.2020, non-constant luminance: Kr 0.2627, Kb 0.0593.
    Bt2020Ncl,
}

impl MatrixCoefficients {
    /// The matrix a file with these primaries almost always uses; what a
    /// source falls back to when it does not name its matrix.
    #[must_use]
    pub const fn for_primaries(primaries: ColorPrimaries) -> Self {
        match primaries {
            ColorPrimaries::Bt601Ntsc | ColorPrimaries::Bt601Pal => Self::Bt601,
            ColorPrimaries::Bt2020 => Self::Bt2020Ncl,
            _ => Self::Bt709,
        }
    }

    /// (Kr, Kb).
    const fn kr_kb(self) -> (f64, f64) {
        match self {
            Self::Bt709 => (0.2126, 0.0722),
            Self::Bt601 => (0.299, 0.114),
            Self::Bt2020Ncl => (0.2627, 0.0593),
        }
    }
}

/// How the samples of a frame are to be read as colour: what
/// [`ColorDescription`] says, and the matrix, which it does not carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FrameColor {
    /// Primaries, transfer function and range.
    pub description: ColorDescription,
    /// The Y'CbCr matrix; unused for an RGB frame.
    pub matrix: MatrixCoefficients,
}

impl From<ColorDescription> for FrameColor {
    /// With the matrix that goes with the primaries
    /// ([`MatrixCoefficients::for_primaries`]).
    fn from(description: ColorDescription) -> Self {
        Self {
            description,
            matrix: MatrixCoefficients::for_primaries(description.primaries),
        }
    }
}

/// The peaks tone mapping works between, in nits (cd/m²).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToneMap {
    /// The brightest the source means. Without metadata of the stream this
    /// is 1000 nits: the peak most HDR10 is mastered for, and the nominal
    /// peak of HLG.
    pub source_peak_nits: f32,
    /// The white of the SDR target, 203 nits (the HDR reference white of
    /// Report ITU-R BT.2408).
    pub target_peak_nits: f32,
}

impl Default for ToneMap {
    fn default() -> Self {
        Self {
            source_peak_nits: 1000.0,
            target_peak_nits: 203.0,
        }
    }
}

/// How the planes of a picture hold its samples; the shader branches on the
/// number ([`Samples::code`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Samples {
    /// Luma in plane 0, interleaved chroma in plane 1 (NV12, P010).
    TwoPlanes,
    /// Luma, Cb and Cr in a plane each.
    ThreePlanes,
    /// R'G'B' in plane 0, the fourth component is padding: opaque.
    Rgb,
    /// R'G'B' and straight alpha in plane 0.
    Rgba,
}

impl Samples {
    /// The samples of a pixel format, `None` for one no layer takes.
    #[must_use]
    pub const fn of(format: PixelFormat) -> Option<Self> {
        match format {
            PixelFormat::Nv12 | PixelFormat::P010 => Some(Self::TwoPlanes),
            PixelFormat::Yuv420P | PixelFormat::Yuv420P10 | PixelFormat::Yuv444P => {
                Some(Self::ThreePlanes)
            }
            PixelFormat::Rgb0 => Some(Self::Rgb),
            PixelFormat::Rgba8 => Some(Self::Rgba),
            _ => None,
        }
    }

    /// The number of this layout in the uniform of the shader.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::TwoPlanes => 0,
            Self::ThreePlanes => 1,
            Self::Rgb => 2,
            Self::Rgba => 3,
        }
    }

    /// Whether the samples are Y'CbCr.
    #[must_use]
    pub const fn is_ycc(self) -> bool {
        matches!(self, Self::TwoPlanes | Self::ThreePlanes)
    }
}

/// What step 2 of the module documentation does; the shader branches on the
/// number ([`Transfer::code`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transfer {
    /// SDR with the primaries of the target: shown as it is.
    AsItIs,
    /// SDR with other primaries: through light and the gamut matrix.
    Sdr,
    /// Perceptual quantizer, tone mapped.
    Pq,
    /// Hybrid log-gamma, tone mapped.
    Hlg,
}

impl Transfer {
    /// The number of this path in the uniform of the shader.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::AsItIs => 0,
            Self::Sdr => 1,
            Self::Pq => 2,
            Self::Hlg => 3,
        }
    }
}

/// Everything the shader needs to turn the texels of one picture into
/// R'G'B' for a BT.709 SDR target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Conversion {
    /// The rows of the affine matrix from the normalised samples
    /// (`y, cb, cr, 1`, each sample 0..=1 as a `Unorm` texture hands it out)
    /// to R'G'B'.
    pub ycc: [[f32; 4]; 3],
    /// The rows of the matrix from the light of the source primaries to the
    /// light of BT.709 (the fourth column is unused).
    pub gamut: [[f32; 4]; 3],
    /// The path of step 2.
    pub transfer: Transfer,
    /// The peaks of tone mapping.
    pub tone: ToneMap,
}

/// The exponent of an SDR display (BT.1886).
const SDR_GAMMA: f32 = 2.4;
/// The nominal peak of HLG in nits and the gamma of its OOTF there (BT.2100).
const HLG_PEAK_NITS: f32 = 1000.0;
const HLG_GAMMA: f32 = 1.2;
/// The luma coefficients of BT.2020, for the OOTF of HLG.
const HLG_LUMA: [f32; 3] = [0.2627, 0.6780, 0.0593];

// SMPTE ST 2084.
const PQ_M1: f32 = 0.159_301_76;
const PQ_M2: f32 = 78.843_75;
const PQ_C1: f32 = 0.835_937_5;
const PQ_C2: f32 = 18.851_563;
const PQ_C3: f32 = 18.6875;
/// The light a PQ signal of 1 means, in nits.
pub const PQ_PEAK_NITS: f32 = 10_000.0;

// ARIB STD-B67 / BT.2100.
const HLG_A: f32 = 0.178_832_77;
const HLG_B: f32 = 0.284_668_92;
const HLG_C: f32 = 0.559_910_7;

/// PQ signal 0..=1 to light 0..=1 of 10 000 nits.
#[must_use]
pub fn pq_eotf(signal: f32) -> f32 {
    let p = signal.max(0.0).powf(1.0 / PQ_M2);
    ((p - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * p)).powf(1.0 / PQ_M1)
}

/// Light 0..=1 of 10 000 nits to the PQ signal 0..=1.
#[must_use]
pub fn pq_inverse_eotf(light: f32) -> f32 {
    let p = light.max(0.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * p) / (1.0 + PQ_C3 * p)).powf(PQ_M2)
}

/// HLG signal 0..=1 to scene light 0..=1 (the inverse OETF of BT.2100).
#[must_use]
pub fn hlg_inverse_oetf(signal: f32) -> f32 {
    if signal <= 0.5 {
        signal * signal / 3.0
    } else {
        (((signal - HLG_C) / HLG_A).exp() + HLG_B) / 12.0
    }
}

/// The EETF of BT.2390 on the light of one pixel in nits: scales the three
/// components so that the largest lands where the curve puts it.
#[must_use]
pub fn tone_map(nits: [f32; 3], tone: ToneMap) -> [f32; 3] {
    if tone.source_peak_nits <= tone.target_peak_nits {
        return nits.map(|c| c.min(tone.target_peak_nits));
    }
    let largest = nits[0].max(nits[1]).max(nits[2]);
    if largest <= 0.0 {
        return [0.0; 3];
    }
    let top = pq_inverse_eotf(tone.source_peak_nits / PQ_PEAK_NITS);
    let e1 = (pq_inverse_eotf(largest / PQ_PEAK_NITS) / top).min(1.0);
    let max_lum = pq_inverse_eotf(tone.target_peak_nits / PQ_PEAK_NITS) / top;
    let knee = 1.5 * max_lum - 0.5;
    let mut e2 = e1;
    if e1 > knee {
        let t = (e1 - knee) / (1.0 - knee);
        let t2 = t * t;
        let t3 = t2 * t;
        e2 = (2.0 * t3 - 3.0 * t2 + 1.0) * knee
            + (t3 - 2.0 * t2 + t) * (1.0 - knee)
            + (-2.0 * t3 + 3.0 * t2) * max_lum;
    }
    let mapped = PQ_PEAK_NITS * pq_eotf(e2 * top);
    let scale = mapped / largest;
    nits.map(|c| c * scale)
}

fn dot3(row: [f32; 4], v: [f32; 3]) -> f32 {
    row[0] * v[0] + row[1] * v[1] + row[2] * v[2]
}

impl Conversion {
    /// The conversion of frames of `format` whose samples mean `color`.
    ///
    /// `None` for a format no layer takes ([`Samples::of`]).
    #[must_use]
    pub fn new(format: PixelFormat, color: FrameColor, tone: ToneMap) -> Option<Self> {
        let samples = Samples::of(format)?;
        let ycc = if samples.is_ycc() {
            ycc_rows(format, color.description.range, color.matrix)
        } else {
            [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
            ]
        };
        let primaries = color.description.primaries;
        let same_gamut = matches!(primaries, ColorPrimaries::Bt709 | ColorPrimaries::Unknown);
        let transfer = match color.description.transfer {
            TransferFunction::Pq => Transfer::Pq,
            TransferFunction::Hlg => Transfer::Hlg,
            // Linear is a state inside a compositor, not something a source
            // hands in today; it is read as SDR like an unknown transfer.
            _ if same_gamut => Transfer::AsItIs,
            _ => Transfer::Sdr,
        };
        Some(Self {
            ycc,
            gamut: gamut_rows(primaries),
            transfer,
            tone,
        })
    }

    /// The R'G'B' (each 0..=1) of one texel, as the shader computes it for a
    /// target that does not encode to sRGB itself. `texel` holds the three
    /// samples as the textures hand them out: `y, cb, cr` or `r, g, b`.
    #[must_use]
    pub fn apply(&self, texel: [f32; 3]) -> [f32; 3] {
        let v = [texel[0], texel[1], texel[2]];
        let rgb = [
            dot3(self.ycc[0], v) + self.ycc[0][3],
            dot3(self.ycc[1], v) + self.ycc[1][3],
            dot3(self.ycc[2], v) + self.ycc[2][3],
        ]
        .map(|c| c.clamp(0.0, 1.0));
        match self.transfer {
            Transfer::AsItIs => rgb,
            Transfer::Sdr => self.encode_light(rgb.map(|c| c.powf(SDR_GAMMA))),
            Transfer::Pq => {
                let nits = rgb.map(|c| PQ_PEAK_NITS * pq_eotf(c));
                self.encode_nits(nits)
            }
            Transfer::Hlg => {
                let scene = rgb.map(hlg_inverse_oetf);
                let luma = HLG_LUMA[0] * scene[0] + HLG_LUMA[1] * scene[1] + HLG_LUMA[2] * scene[2];
                let gain = HLG_PEAK_NITS * luma.max(0.0).powf(HLG_GAMMA - 1.0);
                self.encode_nits(scene.map(|c| c * gain))
            }
        }
    }

    /// Tone maps light in nits and encodes it for the target.
    fn encode_nits(&self, nits: [f32; 3]) -> [f32; 3] {
        let mapped = tone_map(nits, self.tone);
        self.encode_light(mapped.map(|c| c / self.tone.target_peak_nits))
    }

    /// Light of the source primaries (1 is the white of the target) to the
    /// R'G'B' of the target.
    fn encode_light(&self, light: [f32; 3]) -> [f32; 3] {
        [
            dot3(self.gamut[0], light),
            dot3(self.gamut[1], light),
            dot3(self.gamut[2], light),
        ]
        .map(|c| c.clamp(0.0, 1.0).powf(1.0 / SDR_GAMMA))
    }
}

/// 0..=1 to 0..=255, rounded.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 0..=255 before the cast"
)]
pub fn to_8bit(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The affine matrix from normalised samples to R'G'B' (see the module
/// documentation), computed in `f64` and rounded once.
fn ycc_rows(format: PixelFormat, range: ColorRange, matrix: MatrixCoefficients) -> [[f32; 4]; 3] {
    // What a sample of 1.0 is as a code value, and the bit depth of the codes:
    // P010 keeps its 10 bits in the high bits of a word, the planar 10 bit
    // formats in the low bits.
    let (code_of_one, depth) = match format {
        PixelFormat::P010 => (65535.0 / 64.0, 10_i32),
        PixelFormat::Yuv420P10 => (65535.0, 10),
        _ => (255.0, 8),
    };
    let step = f64::from(1_i32 << (depth - 8));
    let (luma_zero, luma_span, chroma_zero, chroma_span) = match range {
        ColorRange::Limited => (16.0 * step, 219.0 * step, 128.0 * step, 224.0 * step),
        ColorRange::Full => {
            let top = f64::from((1_i32 << depth) - 1);
            (0.0, top, 128.0 * step, top)
        }
    };
    let (kr, kb) = matrix.kr_kb();
    let kg = 1.0 - kr - kb;
    // R'G'B' from Y (0..1) and Cb, Cr (-0.5..0.5).
    let from_ycc = [
        [1.0, 0.0, 2.0 * (1.0 - kr)],
        [
            1.0,
            -2.0 * kb * (1.0 - kb) / kg,
            -2.0 * kr * (1.0 - kr) / kg,
        ],
        [1.0, 2.0 * (1.0 - kb), 0.0],
    ];
    let scale = [
        code_of_one / luma_span,
        code_of_one / chroma_span,
        code_of_one / chroma_span,
    ];
    let zero = [
        luma_zero / luma_span,
        chroma_zero / chroma_span,
        chroma_zero / chroma_span,
    ];
    from_ycc.map(|row| {
        let offset = -(row[0] * zero[0] + row[1] * zero[1] + row[2] * zero[2]);
        narrow([
            row[0] * scale[0],
            row[1] * scale[1],
            row[2] * scale[2],
            offset,
        ])
    })
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "coefficients of a colour matrix, rounded to f32 once"
)]
fn narrow(row: [f64; 4]) -> [f32; 4] {
    row.map(|c| c as f32)
}

type Mat3 = [[f64; 3]; 3];

fn mul(a: Mat3, b: Mat3) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = (0..3).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    out
}

fn mul_vec(m: Mat3, v: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}

fn inverse(m: Mat3) -> Mat3 {
    let cofactor = |r: usize, c: usize| {
        let (r1, r2) = ((r + 1) % 3, (r + 2) % 3);
        let (c1, c2) = ((c + 1) % 3, (c + 2) % 3);
        m[r1][c1] * m[r2][c2] - m[r1][c2] * m[r2][c1]
    };
    let det = m[0][0] * cofactor(0, 0) + m[0][1] * cofactor(0, 1) + m[0][2] * cofactor(0, 2);
    let mut out = [[0.0; 3]; 3];
    for (i, row) in out.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            // The adjugate is the transpose of the cofactors.
            *cell = cofactor(j, i) / det;
        }
    }
    out
}

/// CIE xy of red, green, blue and white.
type Chromaticities = [(f64, f64); 4];

const D65: (f64, f64) = (0.3127, 0.3290);

fn chromaticities(primaries: ColorPrimaries) -> Chromaticities {
    match primaries {
        ColorPrimaries::Bt601Ntsc => [(0.630, 0.340), (0.310, 0.595), (0.155, 0.070), D65],
        ColorPrimaries::Bt601Pal => [(0.640, 0.330), (0.290, 0.600), (0.150, 0.060), D65],
        ColorPrimaries::Bt2020 => [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046), D65],
        ColorPrimaries::DciP3 => [
            (0.680, 0.320),
            (0.265, 0.690),
            (0.150, 0.060),
            (0.314, 0.351),
        ],
        ColorPrimaries::DisplayP3 => [(0.680, 0.320), (0.265, 0.690), (0.150, 0.060), D65],
        // BT.709, and what names no primaries.
        _ => [(0.640, 0.330), (0.300, 0.600), (0.150, 0.060), D65],
    }
}

fn xyz_of(xy: (f64, f64)) -> [f64; 3] {
    [xy.0 / xy.1, 1.0, (1.0 - xy.0 - xy.1) / xy.1]
}

/// The matrix from the light of these primaries to CIE XYZ.
fn rgb_to_xyz(c: Chromaticities) -> Mat3 {
    let [r, g, b] = [xyz_of(c[0]), xyz_of(c[1]), xyz_of(c[2])];
    let columns = [[r[0], g[0], b[0]], [r[1], g[1], b[1]], [r[2], g[2], b[2]]];
    let s = mul_vec(inverse(columns), xyz_of(c[3]));
    columns.map(|row| [row[0] * s[0], row[1] * s[1], row[2] * s[2]])
}

/// The Bradford adaptation from white `from` to white `to`, in XYZ.
fn adaptation(from: (f64, f64), to: (f64, f64)) -> Mat3 {
    const BRADFORD: Mat3 = [
        [0.8951, 0.2664, -0.1614],
        [-0.7502, 1.7135, 0.0367],
        [0.0389, -0.0685, 1.0296],
    ];
    let a = mul_vec(BRADFORD, xyz_of(from));
    let b = mul_vec(BRADFORD, xyz_of(to));
    let scale = [
        [b[0] / a[0], 0.0, 0.0],
        [0.0, b[1] / a[1], 0.0],
        [0.0, 0.0, b[2] / a[2]],
    ];
    mul(inverse(BRADFORD), mul(scale, BRADFORD))
}

/// The matrix from the light of `primaries` to the light of BT.709.
fn gamut_rows(primaries: ColorPrimaries) -> [[f32; 4]; 3] {
    let source = chromaticities(primaries);
    let target = chromaticities(ColorPrimaries::Bt709);
    let mut to_xyz = rgb_to_xyz(source);
    if source[3] != target[3] {
        to_xyz = mul(adaptation(source[3], target[3]), to_xyz);
    }
    mul(inverse(rgb_to_xyz(target)), to_xyz).map(|row| narrow([row[0], row[1], row[2], 0.0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color(
        primaries: ColorPrimaries,
        transfer: TransferFunction,
        range: ColorRange,
    ) -> FrameColor {
        ColorDescription {
            primaries,
            transfer,
            range,
        }
        .into()
    }

    fn bt709() -> Conversion {
        Conversion::new(
            PixelFormat::Nv12,
            color(
                ColorPrimaries::Bt709,
                TransferFunction::Bt709,
                ColorRange::Limited,
            ),
            ToneMap::default(),
        )
        .unwrap()
    }

    fn bt709_8bit(y: u8, u: u8, v: u8) -> [u8; 3] {
        bt709()
            .apply([y, u, v].map(|c| f32::from(c) / 255.0))
            .map(to_8bit)
    }

    #[test]
    fn black_white_and_grey() {
        assert_eq!(bt709_8bit(16, 128, 128), [0, 0, 0]);
        assert_eq!(bt709_8bit(235, 128, 128), [255, 255, 255]);
        // Mid grey of the limited range, 126 = 16 + 219 / 2 (rounded).
        assert_eq!(bt709_8bit(126, 128, 128), [128, 128, 128]);
        // Below black and above white are clipped, not wrapped.
        assert_eq!(bt709_8bit(0, 128, 128), [0, 0, 0]);
        assert_eq!(bt709_8bit(255, 128, 128), [255, 255, 255]);
    }

    #[test]
    fn the_75_percent_colour_bars() {
        // The YCbCr values of 75 % bars in BT.709 limited range (EBU/SMPTE
        // test signals) and their R'G'B' of 191 = 0.75 * 255: one off from
        // rounding the 8-bit YCbCr is within the documented tolerance of 2.
        let bars: [((u8, u8, u8), [u8; 3]); 7] = [
            ((180, 128, 128), [191, 191, 191]), // white 75 %
            ((168, 44, 136), [191, 191, 0]),    // yellow
            ((145, 147, 44), [0, 191, 191]),    // cyan
            ((133, 63, 52), [0, 191, 0]),       // green
            ((63, 193, 204), [191, 0, 191]),    // magenta
            ((51, 109, 212), [191, 0, 0]),      // red
            ((28, 212, 120), [0, 0, 191]),      // blue
        ];
        for ((y, u, v), want) in bars {
            let got = bt709_8bit(y, u, v);
            for (g, w) in got.iter().zip(want) {
                assert!(g.abs_diff(w) <= 2, "{y},{u},{v}: {got:?} is not {want:?}");
            }
        }
    }

    #[test]
    fn the_matrix_of_bt709_limited_has_the_documented_numbers() {
        let rows = bt709().ycc;
        let near = |a: f32, b: f32| (a - b).abs() < 2e-4;
        // 255 / 219 and 255 / 224 times the coefficients of the module docs.
        assert!(near(rows[0][0], 255.0 / 219.0));
        assert!(near(rows[0][2], 1.5748 * 255.0 / 224.0));
        assert!(near(rows[1][1], -0.1873 * 255.0 / 224.0));
        assert!(near(rows[1][2], -0.4681 * 255.0 / 224.0));
        assert!(near(rows[2][1], 1.8556 * 255.0 / 224.0));
        assert!(near(rows[0][1], 0.0) && near(rows[2][2], 0.0));
    }

    #[test]
    fn ten_bits_read_the_same_colour_in_the_high_and_in_the_low_bits() {
        let c = color(
            ColorPrimaries::Bt709,
            TransferFunction::Bt709,
            ColorRange::Limited,
        );
        let high = Conversion::new(PixelFormat::P010, c, ToneMap::default()).unwrap();
        let low = Conversion::new(PixelFormat::Yuv420P10, c, ToneMap::default()).unwrap();
        // White, black and 75 % red as 10-bit codes (four times the 8-bit ones).
        for (codes, want) in [
            ([940_u16, 512, 512], [255_u8, 255, 255]),
            ([64, 512, 512], [0, 0, 0]),
            ([204, 436, 848], [191, 0, 0]),
        ] {
            let in_high = codes.map(|c| f32::from(c << 6) / 65535.0);
            let in_low = codes.map(|c| f32::from(c) / 65535.0);
            for got in [high.apply(in_high), low.apply(in_low)] {
                for (g, w) in got.map(to_8bit).iter().zip(want) {
                    assert!(g.abs_diff(w) <= 2, "{codes:?}: {got:?} is not {want:?}");
                }
            }
        }
    }

    #[test]
    fn full_range_uses_every_code() {
        let full = Conversion::new(
            PixelFormat::Yuv444P,
            color(
                ColorPrimaries::Bt709,
                TransferFunction::Bt709,
                ColorRange::Full,
            ),
            ToneMap::default(),
        )
        .unwrap();
        let at = |y: u8| full.apply([f32::from(y) / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        assert_eq!(at(0).map(to_8bit), [0, 0, 0]);
        assert_eq!(at(255).map(to_8bit), [255, 255, 255]);
        assert_eq!(at(16).map(to_8bit), [16, 16, 16]);
    }

    #[test]
    fn rgb_frames_pass_as_they_are() {
        let c = Conversion::new(
            PixelFormat::Rgb0,
            color(
                ColorPrimaries::Bt709,
                TransferFunction::Bt709,
                ColorRange::Full,
            ),
            ToneMap::default(),
        )
        .unwrap();
        assert_eq!(c.transfer, Transfer::AsItIs);
        assert_eq!(c.apply([0.25, 0.5, 1.0]), [0.25, 0.5, 1.0]);
        assert!(
            Conversion::new(
                PixelFormat::Rgba16F,
                FrameColor::from(ColorDescription::default()),
                ToneMap::default()
            )
            .is_none()
        );
    }

    #[test]
    fn the_matrix_follows_the_primaries_unless_the_source_names_one() {
        use MatrixCoefficients as M;
        assert_eq!(M::for_primaries(ColorPrimaries::Bt709), M::Bt709);
        assert_eq!(M::for_primaries(ColorPrimaries::Bt601Ntsc), M::Bt601);
        assert_eq!(M::for_primaries(ColorPrimaries::Bt601Pal), M::Bt601);
        assert_eq!(M::for_primaries(ColorPrimaries::Bt2020), M::Bt2020Ncl);
        assert_eq!(M::for_primaries(ColorPrimaries::Unknown), M::Bt709);
        // BT.601 red, 75 %: Y 81.. in 8 bit is (65, 100, 212) for 75 % red.
        let sd = Conversion::new(
            PixelFormat::Nv12,
            FrameColor {
                description: ColorDescription::default(),
                matrix: M::Bt601,
            },
            ToneMap::default(),
        )
        .unwrap();
        let got = sd
            .apply([65.0, 100.0, 212.0].map(|c: f32| c / 255.0))
            .map(to_8bit);
        for (g, w) in got.iter().zip([191_u8, 0, 0]) {
            assert!(g.abs_diff(w) <= 2, "{got:?}");
        }
    }

    #[test]
    fn the_gamut_matrix_from_bt2020_is_the_one_of_bt2087() {
        // Report ITU-R BT.2087, the matrix M2 inverted: BT.2020 to BT.709.
        let want = [
            [1.6605, -0.5876, -0.0728],
            [-0.1246, 1.1329, -0.0083],
            [-0.0182, -0.1006, 1.1187],
        ];
        let got = gamut_rows(ColorPrimaries::Bt2020);
        for (g, w) in got.iter().zip(want) {
            for (g, w) in g.iter().zip(w) {
                assert!((g - w).abs() < 2e-4, "{got:?}");
            }
        }
        // The same primaries are the identity, and white stays white for all.
        let same = gamut_rows(ColorPrimaries::Bt709);
        assert!((same[0][0] - 1.0).abs() < 1e-6 && same[0][1].abs() < 1e-6);
        for primaries in [
            ColorPrimaries::Bt601Ntsc,
            ColorPrimaries::Bt601Pal,
            ColorPrimaries::Bt2020,
            ColorPrimaries::DciP3,
            ColorPrimaries::DisplayP3,
        ] {
            for row in gamut_rows(primaries) {
                let white = row[0] + row[1] + row[2];
                assert!((white - 1.0).abs() < 1e-4, "{primaries:?}: {white}");
            }
        }
    }

    #[test]
    fn pq_has_the_values_of_st2084() {
        // Well-known points of the curve: 100 nits is about 51 % of the
        // signal, 1000 nits about 75 %, 10 000 nits is 100 %.
        for (nits, signal) in [
            (0.0_f32, 0.0_f32),
            (100.0, 0.5081),
            (203.0, 0.5807),
            (1000.0, 0.7518),
            (10_000.0, 1.0),
        ] {
            let encoded = pq_inverse_eotf(nits / PQ_PEAK_NITS);
            assert!((encoded - signal).abs() < 5e-4, "{nits} nits: {encoded}");
            let back = PQ_PEAK_NITS * pq_eotf(signal);
            assert!(
                (back - nits).abs() <= nits * 0.01 + 0.01,
                "{signal}: {back}"
            );
        }
    }

    #[test]
    fn hlg_has_the_values_of_bt2100() {
        assert!(hlg_inverse_oetf(0.0).abs() < 1e-7);
        // The two branches meet at a signal of 0.5, scene light 1/12.
        assert!((hlg_inverse_oetf(0.5) - 1.0 / 12.0).abs() < 1e-6);
        assert!((hlg_inverse_oetf(0.500_001) - 1.0 / 12.0).abs() < 1e-4);
        assert!((hlg_inverse_oetf(1.0) - 1.0).abs() < 1e-4);
        // BT.2408: a signal of 75 % is the reference white, 203 nits on a
        // display of 1000 nits.
        let scene = hlg_inverse_oetf(0.75);
        let nits = HLG_PEAK_NITS * scene.powf(HLG_GAMMA - 1.0) * scene;
        assert!((nits - 203.0).abs() < 2.0, "{nits}");
    }

    #[test]
    fn tone_mapping_keeps_the_dark_and_ends_at_the_peak_of_the_target() {
        let tone = ToneMap::default();
        // Below the knee nothing changes.
        let dark = tone_map([10.0, 5.0, 1.0], tone);
        for (g, w) in dark.iter().zip([10.0_f32, 5.0, 1.0]) {
            assert!((g - w).abs() < 0.05, "{dark:?}");
        }
        // The peak of the source lands on the peak of the target, and what
        // is brighter still clips there.
        for nits in [1000.0_f32, 4000.0, 10_000.0] {
            let top = tone_map([nits; 3], tone);
            assert!((top[0] - 203.0).abs() < 0.5, "{nits}: {top:?}");
        }
        // Monotonic in between, and never above the target.
        let mut last = 0.0;
        for step in 0..=100_u8 {
            let nits = f32::from(step) * 10.0;
            let out = tone_map([nits; 3], tone)[0];
            assert!(out + 1e-3 >= last && out <= 203.5, "{nits}: {out}");
            last = out;
        }
        // The largest component is mapped, the others keep their ratio.
        let red = tone_map([800.0, 400.0, 0.0], tone);
        assert!((red[1] / red[0] - 0.5).abs() < 1e-4 && red[2] == 0.0);
        // A source no brighter than the target is only clipped.
        let sdr = ToneMap {
            source_peak_nits: 100.0,
            target_peak_nits: 203.0,
        };
        assert_eq!(tone_map([50.0, 300.0, 0.0], sdr), [50.0, 203.0, 0.0]);
        assert_eq!(tone_map([0.0; 3], tone), [0.0; 3]);
    }

    #[test]
    fn the_transfer_of_a_source_picks_its_path() {
        let path = |primaries, transfer| {
            Conversion::new(
                PixelFormat::P010,
                color(primaries, transfer, ColorRange::Limited),
                ToneMap::default(),
            )
            .unwrap()
            .transfer
        };
        use ColorPrimaries as P;
        use TransferFunction as T;
        assert_eq!(path(P::Bt709, T::Bt709), Transfer::AsItIs);
        assert_eq!(path(P::Unknown, T::Unknown), Transfer::AsItIs);
        assert_eq!(path(P::Bt709, T::Srgb), Transfer::AsItIs);
        assert_eq!(path(P::Bt2020, T::Bt709), Transfer::Sdr);
        assert_eq!(path(P::Bt601Pal, T::Bt709), Transfer::Sdr);
        assert_eq!(path(P::Bt2020, T::Pq), Transfer::Pq);
        assert_eq!(path(P::Bt2020, T::Hlg), Transfer::Hlg);
        // PQ with BT.709 primaries is still HDR.
        assert_eq!(path(P::Bt709, T::Pq), Transfer::Pq);
    }

    #[test]
    fn hdr_white_and_grey_come_out_neutral_and_in_order() {
        let pq = Conversion::new(
            PixelFormat::P010,
            color(
                ColorPrimaries::Bt2020,
                TransferFunction::Pq,
                ColorRange::Full,
            ),
            ToneMap::default(),
        )
        .unwrap();
        let grey = |signal: f32| {
            // Full range 10 bit in the high bits: luma code = signal * 1023.
            let y = (signal * 1023.0).round() * 64.0 / 65535.0;
            let c = 512.0 * 64.0 / 65535.0;
            pq.apply([y, c, c])
        };
        let mut last = -1.0;
        for step in 0..=20_u8 {
            let out = grey(f32::from(step) / 20.0);
            assert!((out[0] - out[1]).abs() < 4e-3 && (out[1] - out[2]).abs() < 4e-3);
            assert!(out[0] + 1e-4 >= last, "step {step}: {out:?}");
            last = out[0];
        }
        // 1000 nits and more are the white of the target, black is black.
        assert!(grey(0.7518)[0] > 0.995);
        assert!(grey(1.0)[0] > 0.995);
        assert!(grey(0.0)[0] < 1e-3);
        // 100 nits of HDR stay 100 of 203 nits: (100/203)^(1/2.4).
        let want = (100.0_f32 / 203.0).powf(1.0 / 2.4);
        assert!((grey(0.5081)[0] - want).abs() < 0.01, "{:?}", grey(0.5081));
    }
}
