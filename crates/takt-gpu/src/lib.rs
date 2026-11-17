// SPDX-License-Identifier: GPL-3.0-or-later
//! Takt: the picture on the GPU.
//!
//! A decoder hands out a [`GpuFrame`]: a texture in video memory
//! ([`GpuFrame::Texture`], imported without a copy through system memory) or,
//! as the fallback, planes in system memory ([`GpuFrame::Cpu`]). A
//! [`LayerSlot`] shows a frame in a rectangle of a target: it converts the
//! samples to the R'G'B' of a BT.709 SDR target (matrix, range, primaries),
//! tone maps HDR (PQ, HLG) down to it and blends the result over what is
//! below. A [`Compositor`] draws several layers into one target.
//!
//! # The seam to a decoder
//!
//! 1. The device exists first. The decoder is told what it needs to know
//!    about it ([`DeviceInfo`]): the adapter to decode on, and which formats
//!    can be drawn.
//! 2. The decoder returns frames ([`GpuFrame`]). A texture frame carries the
//!    visible size (the texture may be larger), how its samples are to be
//!    read as colour ([`FrameColor`]), when the texture holds the picture
//!    ([`Ready`]) and a guard that gives the picture back to its owner.
//! 3. The layer that shows a frame keeps it until the GPU is done with it
//!    ([`LayerSlot::before_pass`]).
//!
//! Whatever crosses this seam may come from a helper process: sizes, layers
//! and formats are checked against the texture and the limits of the device
//! before they are used ([`TextureFrame::new`], the module `interop`, [`PlanesView`]).
//!
//! # Platforms
//!
//! Drawing works wherever wgpu does. The import of a foreign texture
//! (the module `interop`, which exists under Windows only) is Windows on DX12
//! so far; elsewhere a decoder hands out
//! frames in system memory.

pub mod colour;
mod compositor;
mod device;
mod frame;
#[cfg(windows)]
pub mod interop;
mod layer;
mod layout;
pub mod readback;

pub use colour::{Conversion, FrameColor, MatrixCoefficients, ToneMap};
pub use compositor::Compositor;
pub use device::{
    DeviceInfo, Gpu, GpuOptions, OpenError, WANTED_FEATURES, preferred_backends, wanted_features,
};
pub use frame::{FrameError, GpuFrame, PlanarFormat, Ready, TextureFrame};
pub use layer::{LayerRenderer, LayerSlot, Placement, ShowError};
pub use layout::{LayoutError, PlaneShape, PlaneView, PlanesView, plane_shape};
/// The wgpu this crate draws with: a frame of this crate belongs to a device
/// of exactly this version.
pub use wgpu;
