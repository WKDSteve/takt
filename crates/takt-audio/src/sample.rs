// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Sample formats of a device that takes no `f32`.
//!
//! The ring carries `f32`; a device that only takes integers gets them from
//! [`OutputCallback::fill_as`](crate::OutputCallback::fill_as), which converts
//! in the callback, without allocation. Rate conversion is not done here: a
//! device without the rate of the ring is refused, as before.
//!
//! # Dither
//!
//! `i16` has less resolution than the 24 bits of an `f32` mantissa, so plain
//! rounding would turn quiet signals into distortion that follows the signal.
//! [`Dither`] adds triangular (TPDF) noise of one LSB peak to each sample
//! before rounding: the error becomes a constant noise floor instead.
//! `i32` is finer than `f32`, there is nothing to dither.

/// A sample type an output device takes.
pub trait DeviceSample: Copy + Send + 'static {
    /// The value of silence.
    const SILENCE: Self;

    /// `x` in `[-1, 1]` (clipped outside) as this format; `dither` is the
    /// noise source of the stream.
    fn from_f32(x: f32, dither: &mut Dither) -> Self;
}

impl DeviceSample for f32 {
    const SILENCE: Self = 0.0;

    fn from_f32(x: f32, _: &mut Dither) -> Self {
        x
    }
}

impl DeviceSample for i16 {
    const SILENCE: Self = 0;

    fn from_f32(x: f32, dither: &mut Dither) -> Self {
        let scaled = x.clamp(-1.0, 1.0) * f32::from(i16::MAX) + dither.tpdf();
        // Clamped to the range of i16 first, so the cast cannot truncate.
        #[allow(clippy::cast_possible_truncation)]
        let sample = scaled
            .round()
            .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
        sample
    }
}

impl DeviceSample for i32 {
    const SILENCE: Self = 0;

    fn from_f32(x: f32, _: &mut Dither) -> Self {
        // In f64: i32::MAX is not exact in f32, and 2^31 would overflow.
        let scaled = f64::from(x.clamp(-1.0, 1.0)) * f64::from(i32::MAX);
        // In [-i32::MAX, i32::MAX] after the clamp above, so the cast cannot truncate.
        #[allow(clippy::cast_possible_truncation)]
        let sample = scaled.round() as i32;
        sample
    }
}

/// The noise source of the dither: a xorshift generator, cheap and without
/// allocation, good enough for noise (not for anything else).
#[derive(Clone, Debug)]
pub struct Dither {
    state: u32,
}

impl Default for Dither {
    fn default() -> Self {
        Self::new(0x9E37_79B9)
    }
}

impl Dither {
    /// A generator from `seed` (0 is replaced, xorshift would stay at 0).
    #[must_use]
    pub const fn new(seed: u32) -> Self {
        Self {
            state: if seed == 0 { 0x9E37_79B9 } else { seed },
        }
    }

    fn next_unit(&mut self) -> f32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        // The upper 24 bits are exact in an f32; the shift makes the cast lossless.
        #[allow(clippy::cast_precision_loss)]
        let unit = (x >> 8) as f32 / 16_777_216.0;
        unit
    }

    /// Triangular noise in `(-1, 1)`, the difference of two uniform values,
    /// in units of one LSB.
    pub fn tpdf(&mut self) -> f32 {
        self.next_unit() - self.next_unit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_scale_and_clipping_hit_the_ends_of_i16() {
        let mut d = Dither::default();
        for _ in 0..100 {
            assert!(i16::from_f32(1.0, &mut d) >= 32_766);
            assert!(i16::from_f32(4.0, &mut d) >= 32_766);
            assert!(i16::from_f32(-1.0, &mut d) <= -32_766);
            assert!(i16::from_f32(-4.0, &mut d) <= -32_766);
        }
    }

    #[test]
    fn dither_stays_within_one_lsb_and_averages_out() {
        let mut d = Dither::new(1);
        let mut sum = 0_i64;
        for _ in 0..100_000 {
            let s = i16::from_f32(0.0, &mut d);
            assert!((-1..=1).contains(&s), "{s}");
            sum += i64::from(s);
        }
        assert!(sum.abs() < 1_000, "mean of the dither is {sum} / 100000");
        // Half an LSB: without dither always 0 or always 1; with it, the mean is kept.
        let half = 0.5 / f32::from(i16::MAX);
        let ones = (0..100_000)
            .filter(|_| i16::from_f32(half, &mut d) == 1)
            .count();
        assert!((40_000..60_000).contains(&ones), "{ones}");
    }

    #[test]
    fn i32_spans_its_range_without_dither() {
        let mut d = Dither::default();
        assert_eq!(i32::from_f32(1.0, &mut d), i32::MAX);
        assert_eq!(i32::from_f32(-1.0, &mut d), -i32::MAX);
        assert_eq!(i32::from_f32(0.0, &mut d), 0);
        assert_eq!(i32::from_f32(0.5, &mut d), 1_073_741_824);
        assert_eq!(i32::from_f32(-9.0, &mut d), -i32::MAX);
    }

    #[test]
    fn f32_passes_through() {
        let mut d = Dither::default();
        assert_eq!(f32::from_f32(0.25, &mut d), 0.25);
    }
}
