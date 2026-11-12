// SPDX-License-Identifier: GPL-3.0-or-later
//! The colour conversion of the video path on the CPU: the reference the
//! shader (`nv12.wgsl`) is compared with, and the place where its constants
//! are explained.
//!
//! # BT.709, limited range
//!
//! 8-bit video stores luma in 16..=235 and chroma in 16..=240 around 128
//! (ITU-R BT.709-6, table 4). Normalised:
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
//! The result is gamma-encoded R'G'B'; it is shown as is on a non-sRGB
//! target, and linearised with the sRGB curve when the target format encodes
//! itself (the BT.709 and sRGB curves differ slightly, which belongs to the
//! colour management of a later phase).

/// Normalised BT.709 limited range YUV of 8-bit samples to gamma-encoded RGB,
/// each 0..=1 (clamped, as the shader does).
#[must_use]
pub fn bt709_limited(y: u8, u: u8, v: u8) -> [f32; 3] {
    let y = (f32::from(y) - 16.0) / 219.0;
    let cb = (f32::from(u) - 128.0) / 224.0;
    let cr = (f32::from(v) - 128.0) / 224.0;
    [
        (y + 1.5748 * cr).clamp(0.0, 1.0),
        (y - 0.1873 * cb - 0.4681 * cr).clamp(0.0, 1.0),
        (y + 1.8556 * cb).clamp(0.0, 1.0),
    ]
}

/// The same as 8-bit values, rounded.
#[must_use]
pub fn bt709_limited_8bit(y: u8, u: u8, v: u8) -> [u8; 3] {
    bt709_limited(y, u, v).map(to_8bit)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_white_and_grey() {
        assert_eq!(bt709_limited_8bit(16, 128, 128), [0, 0, 0]);
        assert_eq!(bt709_limited_8bit(235, 128, 128), [255, 255, 255]);
        // Mid grey of the limited range, 126 = 16 + 219 / 2 (rounded).
        assert_eq!(bt709_limited_8bit(126, 128, 128), [128, 128, 128]);
        // Below black and above white are clipped, not wrapped.
        assert_eq!(bt709_limited_8bit(0, 128, 128), [0, 0, 0]);
        assert_eq!(bt709_limited_8bit(255, 128, 128), [255, 255, 255]);
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
            let got = bt709_limited_8bit(y, u, v);
            for (g, w) in got.iter().zip(want) {
                assert!(g.abs_diff(w) <= 2, "{y},{u},{v}: {got:?} is not {want:?}");
            }
        }
    }
}
