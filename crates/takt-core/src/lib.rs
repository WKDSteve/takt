// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Fundamenttypen der Takt-Medien-Suite: Zeitbasis, Frames, Audio-Puffer.
//!
//! # Die eine Regel
//!
//! **Zeit wird in diesem Projekt niemals als Gleitkommazahl dargestellt.**
//! Ein Zeitstempel ist immer ein [`Timestamp`]: ein ganzzahliger Tick-Wert
//! plus eine rationale Zeitbasis. Der Grund ist nicht Pedanterie, sondern
//! Erfahrung: `f64`-Sekunden akkumulieren bei 24000/1001 fps oder 44,1 kHz
//! Rundungsfehler, die sich nach Stunden Laufzeit als Drift zwischen Bild
//! und Ton zeigen — und dann nicht mehr rekonstruierbar sind.
//!
//! Gleitkomma ist ausschliesslich fuer die Anzeige erlaubt, siehe
//! [`Timestamp::as_secs_f64`], und diese Funktion darf niemals in eine
//! Rechnung zurueckfliessen.
//!
//! # Zone
//!
//! Dieses Crate liegt in Lizenz-Zone 1 (permissiv) und darf deshalb keine
//! Copyleft-Abhaengigkeiten einziehen. Es ist ausserdem frei von `unsafe`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod audio;
mod error;
mod frame;
mod pixel;
mod time;

pub use audio::{AudioBuffer, ChannelLayout, SampleFormat};
pub use error::{Error, Result};
pub use frame::{FrameMeta, FramePool, PLANE_ALIGN, PooledFrame, VideoFrame};
pub use pixel::{ColorDescription, ColorPrimaries, ColorRange, PixelFormat, TransferFunction};
pub use time::{Rational, Timestamp};
