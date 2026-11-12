// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The audio path of Takt.
//!
//! For now this is the first building block of zone 1 audio: [`ring()`], the
//! wait-free single-producer single-consumer ring that carries interleaved
//! `f32` frames between the decoder side and the audio callback
//! (`docs/THREADS-UND-UHREN.md`, invariant 5: the audio thread only talks
//! through wait-free structures).
//!
//! # Zone
//!
//! License zone 1 (permissive), free of `unsafe`.

pub mod ring;

pub use ring::{Consumer, Producer, ring};
