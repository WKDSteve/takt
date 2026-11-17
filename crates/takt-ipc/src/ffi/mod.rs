// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The `unsafe` of this crate, all of it: the calls of the operating system (an anonymous mapping, a
//! handle a child inherits, a job that ends the child with its parent) and the raw access to memory
//! another process writes at the same time. Everything outside this module is safe code on top of
//! [`Region`] and [`SharedMemory`].

pub(crate) mod region;
pub(crate) mod shm;

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(windows)]
pub(crate) mod windows;

pub use region::Region;
pub(crate) use shm::SharedMemory;
