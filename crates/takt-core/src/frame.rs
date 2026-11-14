// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Video-Frames und ihre Eigentuemerschaft.
//!
//! # Warum hier schon ein Pool steht
//!
//! In Phase 0 liegen Frames noch im Hauptspeicher, und ein `Vec<u8>` pro
//! Frame waere voellig ausreichend. Der Pool steht trotzdem von Anfang an da,
//! weil sich die Eigentuemerschaft spaeter nicht mehr aendern laesst:
//!
//! Sobald der Hardware-Decoder Frames liefert, ist ein Frame kein Puffer
//! mehr, den man besitzt, sondern ein *Platz in einem begrenzten Ringpuffer
//! des Decoders*. Gibt der Renderer ihn nicht zurueck, laeuft der Decoder
//! leer und die Wiedergabe stockt. Haelt der Renderer ihn zu lange, greift
//! der Decoder auf einen Puffer zu, der noch angezeigt wird.
//!
//! Diese Rueckgabepflicht in eine bestehende Pipeline nachtraeglich
//! einzuziehen bedeutet, jede Stelle anzufassen, die ein Frame anfasst.
//! Deshalb gibt es sie ab dem ersten Commit — auch wenn sie noch nichts kostet.

use std::sync::{Arc, Mutex};

use crate::pixel::{ColorPrimaries, ColorRange, PixelFormat, TransferFunction};
use crate::time::Timestamp;

/// Beschreibung eines Frames ohne die Bilddaten selbst.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameMeta {
    /// Breite in Pixeln.
    pub width: u32,
    /// Hoehe in Pixeln.
    pub height: u32,
    /// Speicheranordnung.
    pub format: PixelFormat,
    /// Farbprimaerwerte.
    pub primaries: ColorPrimaries,
    /// Uebertragungsfunktion.
    pub transfer: TransferFunction,
    /// Wertebereich der Komponenten.
    pub range: ColorRange,
    /// Praesentationszeitstempel.
    pub pts: Timestamp,
}

impl FrameMeta {
    /// Ob dieses Frame einen HDR-Signalpfad beschreibt.
    #[must_use]
    pub const fn is_hdr(&self) -> bool {
        self.transfer.is_hdr()
    }
}

/// Ein Frame samt Bilddaten im Hauptspeicher.
///
/// Ab Phase 1 tritt daneben eine Variante, die nur ein Handle auf eine
/// GPU-Textur haelt. Die Rueckgabemechanik ist fuer beide dieselbe.
#[derive(Debug)]
pub struct VideoFrame {
    meta: FrameMeta,
    /// Eine Ebene pro [`PixelFormat::planes`].
    planes: Vec<Vec<u8>>,
    /// Bytes pro Zeile je Ebene. Kann groesser als `width` sein (Padding).
    strides: Vec<usize>,
}

impl VideoFrame {
    /// Legt ein Frame mit nullinitialisierten Ebenen an.
    ///
    /// Gibt `None` zurueck, wenn die Groessenangaben nicht zum Format passen.
    #[must_use]
    pub fn alloc(meta: FrameMeta) -> Option<Self> {
        let planes = meta.format.planes();
        let mut data = Vec::with_capacity(planes);
        let mut strides = Vec::with_capacity(planes);

        let w = usize::try_from(meta.width).ok()?;
        let h = usize::try_from(meta.height).ok()?;
        if w == 0 || h == 0 {
            return None;
        }

        for plane in 0..planes {
            let (pw, ph) = plane_dims(meta.format, plane, w, h)?;
            let bytes_per_sample = usize::from(meta.format.bit_depth() > 8) + 1;
            let components = meta.format.components(plane);
            let stride = pw * bytes_per_sample * components;
            data.push(vec![0_u8; stride * ph]);
            strides.push(stride);
        }

        Some(Self {
            meta,
            planes: data,
            strides,
        })
    }

    /// Beschreibung dieses Frames.
    #[must_use]
    pub const fn meta(&self) -> &FrameMeta {
        &self.meta
    }

    /// Setzt den Praesentationszeitstempel. Wird beim Wiederverwenden aus dem
    /// Pool aufgerufen.
    pub const fn set_pts(&mut self, pts: Timestamp) {
        self.meta.pts = pts;
    }

    /// Lesezugriff auf eine Ebene.
    #[must_use]
    pub fn plane(&self, index: usize) -> Option<&[u8]> {
        self.planes.get(index).map(Vec::as_slice)
    }

    /// Schreibzugriff auf eine Ebene.
    #[must_use]
    pub fn plane_mut(&mut self, index: usize) -> Option<&mut [u8]> {
        self.planes.get_mut(index).map(Vec::as_mut_slice)
    }

    /// Bytes pro Zeile in der angegebenen Ebene.
    #[must_use]
    pub fn stride(&self, index: usize) -> Option<usize> {
        self.strides.get(index).copied()
    }

    /// Gesamter belegter Speicher in Bytes.
    #[must_use]
    pub fn byte_size(&self) -> usize {
        self.planes.iter().map(Vec::len).sum()
    }
}

/// Abmessungen einer einzelnen Ebene.
fn plane_dims(
    format: PixelFormat,
    plane: usize,
    width: usize,
    height: usize,
) -> Option<(usize, usize)> {
    let sub = format.is_subsampled_420();
    Some(match (format, plane) {
        // Luma-Ebene: volle Auflaesung.
        (_, 0) => (width, height),
        // NV12/P010: eine verschachtelte Chroma-Ebene, halbe Groesse.
        (PixelFormat::Nv12 | PixelFormat::P010, 1) => (width.div_ceil(2), height.div_ceil(2)),
        // Planares YUV: zwei Chroma-Ebenen.
        (_, 1 | 2) if sub => (width.div_ceil(2), height.div_ceil(2)),
        (PixelFormat::Yuv444P, 1 | 2) => (width, height),
        _ => return None,
    })
}

/// Ein Frame, das bei `Drop` in seinen Pool zurueckkehrt.
///
/// Genau diese Mechanik braucht der Hardware-Pfad ab Phase 1. Sie steht hier
/// schon, damit die Aufrufseite sich nicht aendern muss.
#[derive(Debug)]
pub struct PooledFrame {
    frame: Option<VideoFrame>,
    pool: Arc<Mutex<Vec<VideoFrame>>>,
}

impl PooledFrame {
    /// Lesezugriff auf das Frame.
    ///
    /// # Panics
    /// Nur wenn das Frame bereits herausgenommen wurde, was intern nicht
    /// vorkommt.
    #[must_use]
    pub fn get(&self) -> &VideoFrame {
        self.frame.as_ref().expect("PooledFrame ohne Frame")
    }

    /// Schreibzugriff auf das Frame.
    ///
    /// # Panics
    /// Siehe [`PooledFrame::get`].
    #[must_use]
    pub fn get_mut(&mut self) -> &mut VideoFrame {
        self.frame.as_mut().expect("PooledFrame ohne Frame")
    }
}

impl Drop for PooledFrame {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            // Ein vergifteter Mutex darf die Wiedergabe nicht beenden; im
            // schlimmsten Fall verliert der Pool ein Frame und alloziert neu.
            if let Ok(mut pool) = self.pool.lock() {
                pool.push(frame);
            }
        }
    }
}

/// Ein Vorrat wiederverwendbarer Frames fester Geometrie.
///
/// Der Pool wachst bis `capacity` und gibt darueber hinaus `None` zurueck,
/// statt beliebig zu allozieren. Ein leerer Pool ist ein Signal — er bedeutet,
/// dass der Verbraucher nicht nachkommt, und genau das will man messen statt
/// es durch Nachallozieren zu verstecken.
#[derive(Debug)]
pub struct FramePool {
    free: Arc<Mutex<Vec<VideoFrame>>>,
    meta: FrameMeta,
    capacity: usize,
    live: usize,
}

impl FramePool {
    /// Legt einen leeren Pool fuer Frames der angegebenen Geometrie an.
    #[must_use]
    pub fn new(meta: FrameMeta, capacity: usize) -> Self {
        Self {
            free: Arc::new(Mutex::new(Vec::with_capacity(capacity))),
            meta,
            capacity,
            live: 0,
        }
    }

    /// Holt ein Frame aus dem Pool oder alloziert eines, solange die
    /// Kapazitaet nicht erschoepft ist.
    #[must_use]
    pub fn acquire(&mut self, pts: Timestamp) -> Option<PooledFrame> {
        let recycled = self.free.lock().ok().and_then(|mut f| f.pop());
        let mut frame = match recycled {
            Some(f) => f,
            None => {
                if self.live >= self.capacity {
                    return None;
                }
                self.live += 1;
                VideoFrame::alloc(self.meta)?
            }
        };
        frame.set_pts(pts);
        Some(PooledFrame {
            frame: Some(frame),
            pool: Arc::clone(&self.free),
        })
    }

    /// Wie viele Frames derzeit ausgegeben sind.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        let free = self.free.lock().map_or(0, |f| f.len());
        self.live.saturating_sub(free)
    }

    /// Obergrenze gleichzeitig existierender Frames.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Timestamp;

    fn meta(format: PixelFormat) -> FrameMeta {
        FrameMeta {
            width: 1920,
            height: 1080,
            format,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt709,
            range: ColorRange::Limited,
            pts: Timestamp::from_millis(0),
        }
    }

    #[test]
    fn nv12_hat_zwei_ebenen_mit_halbem_chroma() {
        let f = VideoFrame::alloc(meta(PixelFormat::Nv12)).unwrap();
        assert_eq!(f.stride(0), Some(1920));
        // Chroma: 960 Pixel mal 2 Komponenten.
        assert_eq!(f.stride(1), Some(1920));
        assert_eq!(f.plane(0).unwrap().len(), 1920 * 1080);
        assert_eq!(f.plane(1).unwrap().len(), 1920 * 540);
        assert!(f.plane(2).is_none());
        // 1,5 Bytes pro Pixel bei 4:2:0.
        assert_eq!(f.byte_size(), 1920 * 1080 * 3 / 2);
    }

    #[test]
    fn rgb0_hat_eine_ebene_mit_vier_bytes_je_pixel() {
        let mut f = VideoFrame::alloc(meta(PixelFormat::Rgb0)).unwrap();
        assert_eq!(f.stride(0), Some(1920 * 4));
        assert_eq!(f.plane(0).unwrap().len(), 1920 * 1080 * 4);
        assert!(f.plane(1).is_none());
        assert!(f.stride(1).is_none());
        assert_eq!(f.byte_size(), 1920 * 1080 * 4);
        // Dieselbe Groesse wie Rgba8: nur die Bedeutung des vierten Bytes
        // unterscheidet die beiden.
        let rgba = VideoFrame::alloc(meta(PixelFormat::Rgba8)).unwrap();
        assert_eq!(f.byte_size(), rgba.byte_size());
        f.plane_mut(0).unwrap()[4..8].copy_from_slice(&[10, 20, 30, 0]);
        assert_eq!(&f.plane(0).unwrap()[4..7], &[10, 20, 30]);
    }

    #[test]
    fn ein_pool_fuer_rgb0_gibt_rgb0_aus() {
        let mut pool = FramePool::new(meta(PixelFormat::Rgb0), 1);
        let frame = pool.acquire(Timestamp::from_millis(40)).unwrap();
        assert_eq!(frame.get().meta().format, PixelFormat::Rgb0);
        assert_eq!(frame.get().stride(0), Some(1920 * 4));
    }

    #[test]
    fn p010_belegt_doppelt_so_viel_wie_nv12() {
        let nv12 = VideoFrame::alloc(meta(PixelFormat::Nv12)).unwrap();
        let p010 = VideoFrame::alloc(meta(PixelFormat::P010)).unwrap();
        assert_eq!(p010.byte_size(), nv12.byte_size() * 2);
    }

    #[test]
    fn ungerade_abmessungen_runden_chroma_auf() {
        let mut m = meta(PixelFormat::Yuv420P);
        m.width = 1281;
        m.height = 721;
        let f = VideoFrame::alloc(m).unwrap();
        assert_eq!(f.stride(1), Some(641));
        assert_eq!(f.plane(1).unwrap().len(), 641 * 361);
    }

    #[test]
    fn nullgroesse_wird_abgelehnt() {
        let mut m = meta(PixelFormat::Nv12);
        m.width = 0;
        assert!(VideoFrame::alloc(m).is_none());
    }

    #[test]
    fn pool_recycelt_statt_zu_allozieren() {
        let mut pool = FramePool::new(meta(PixelFormat::Nv12), 2);
        assert_eq!(pool.in_flight(), 0);

        let a = pool.acquire(Timestamp::from_millis(0)).unwrap();
        let b = pool.acquire(Timestamp::from_millis(20)).unwrap();
        assert_eq!(pool.in_flight(), 2);

        // Kapazitaet erschoepft: kein stilles Nachallozieren.
        assert!(pool.acquire(Timestamp::from_millis(40)).is_none());

        drop(a);
        assert_eq!(pool.in_flight(), 1);

        // Das zurueckgegebene Frame wird wiederverwendet.
        let c = pool.acquire(Timestamp::from_millis(40)).unwrap();
        assert_eq!(c.get().meta().pts, Timestamp::from_millis(40));
        assert_eq!(pool.in_flight(), 2);

        drop(b);
        drop(c);
        assert_eq!(pool.in_flight(), 0);
    }
}
