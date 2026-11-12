// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The audio path of Takt.
//!
//! For now this is the first building block of zone 1 audio: [`ring()`], the
//! wait-free single-producer single-consumer ring that carries interleaved
//! `f32` frames between the decoder side and the audio callback
//! (`docs/THREADS-UND-UHREN.md`, invariant 5: the audio thread only talks
//! through wait-free structures), and [`OutputCallback`], the body of the
//! audio callback that plays from it and drives the audio master clock. The
//! device itself is [`output`] behind the feature `cpal`.
//!
//! # Zone
//!
//! License zone 1 (permissive), free of `unsafe`.

pub mod callback;
pub mod histogram;
#[cfg(feature = "cpal")]
pub mod output;
pub mod ring;
pub mod sample;

pub use callback::{Gain, OutputCallback, OutputStats};
pub use histogram::{DurationHistogram, DurationSnapshot};
#[cfg(feature = "cpal")]
pub use output::{Output, OutputConfig, OutputError, Priority};
pub use ring::{Consumer, Producer, ring};
pub use sample::{DeviceSample, Dither};
