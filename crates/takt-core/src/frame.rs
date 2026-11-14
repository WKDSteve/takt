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

/// Ausrichtung der Ebenen eines [`VideoFrame`] in Bytes: eine Cache-Zeile,
/// und genug fuer jede SIMD-Breite bis AVX-512.
pub const PLANE_ALIGN: usize = 64;

/// Eine Ebene: ihre Zeilen liegen ab `start` in `data`.
#[derive(Debug)]
struct Plane {
    /// `len + PLANE_ALIGN` Bytes; der Vorrat erlaubt, den Anfang auf die
    /// naechste ausgerichtete Adresse zu schieben. Wird nie in der Groesse
    /// geaendert, die Adresse bleibt also ueber die Lebenszeit dieselbe.
    data: Vec<u8>,
    start: usize,
    /// `stride * Zeilen`.
    len: usize,
    stride: usize,
    /// Genutzte Bytes je Zeile, ohne Padding.
    row: usize,
}

impl Plane {
    fn alloc(row: usize, rows: usize) -> Option<Self> {
        let stride = row.checked_next_multiple_of(PLANE_ALIGN)?;
        let len = stride.checked_mul(rows)?;
        let data = vec![0_u8; len.checked_add(PLANE_ALIGN)?];
        // `align_offset` darf laut Doku ablehnen (`usize::MAX`); dann beginnt
        // die Ebene unausgerichtet. Das kostet Verbraucher hoechstens Zeit,
        // nie Korrektheit: niemand darf sich fuer die Sicherheit darauf
        // verlassen.
        let offset = data.as_ptr().align_offset(PLANE_ALIGN);
        let start = if offset < PLANE_ALIGN { offset } else { 0 };
        Some(Self {
            data,
            start,
            len,
            stride,
            row,
        })
    }

    fn bytes(&self) -> &[u8] {
        &self.data[self.start..self.start + self.len]
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data[self.start..self.start + self.len]
    }
}

/// Ein Frame samt Bilddaten im Hauptspeicher.
///
/// # Ausrichtung
///
/// Jede Ebene beginnt an einer durch [`PLANE_ALIGN`] (64) teilbaren Adresse,
/// und ihre Zeilenlaenge ([`VideoFrame::stride`]) ist ein Vielfaches davon.
/// Ein Decoder oder Renderer mit SIMD-Schleifen (FFmpeg, mpv) kann damit
/// direkt in das Frame schreiben, ohne eigene Zwischenflaeche und ohne Kopie.
///
/// Die Folge fuer jeden Leser: **eine Zeile ist `stride` Bytes lang, nicht
/// `width` mal Bytes je Pixel.** Die genutzten Bytes nennt
/// [`VideoFrame::row_bytes`]; was dahinter bis zur naechsten Zeile liegt, ist
/// Padding mit unbestimmtem Inhalt (ein Frame aus dem Pool wird nicht
/// geloescht).
///
/// Ab Phase 1 tritt daneben eine Variante, die nur ein Handle auf eine
/// GPU-Textur haelt. Die Rueckgabemechanik ist fuer beide dieselbe.
#[derive(Debug)]
pub struct VideoFrame {
    meta: FrameMeta,
    /// Eine Ebene pro [`PixelFormat::planes`].
    planes: Vec<Plane>,
}

impl VideoFrame {
    /// Legt ein Frame mit nullinitialisierten Ebenen an.
    ///
    /// Gibt `None` zurueck, wenn die Groessenangaben nicht zum Format passen.
    #[must_use]
    pub fn alloc(meta: FrameMeta) -> Option<Self> {
        let planes = meta.format.planes();
        let mut data = Vec::with_capacity(planes);

        let w = usize::try_from(meta.width).ok()?;
        let h = usize::try_from(meta.height).ok()?;
        if w == 0 || h == 0 {
            return None;
        }

        for plane in 0..planes {
            let (pw, ph) = plane_dims(meta.format, plane, w, h)?;
            let bytes_per_sample = usize::from(meta.format.bit_depth() > 8) + 1;
            let components = meta.format.components(plane);
            let row = pw.checked_mul(bytes_per_sample * components)?;
            data.push(Plane::alloc(row, ph)?);
        }

        Some(Self { meta, planes: data })
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

    /// Lesezugriff auf eine Ebene: `stride` mal Zeilen Bytes ab einer
    /// ausgerichteten Adresse (siehe [`VideoFrame`]).
    #[must_use]
    pub fn plane(&self, index: usize) -> Option<&[u8]> {
        self.planes.get(index).map(Plane::bytes)
    }

    /// Schreibzugriff auf eine Ebene, dieselben Bytes wie
    /// [`VideoFrame::plane`].
    #[must_use]
    pub fn plane_mut(&mut self, index: usize) -> Option<&mut [u8]> {
        self.planes.get_mut(index).map(Plane::bytes_mut)
    }

    /// Bytes von einer Zeile zur naechsten in der angegebenen Ebene: ein
    /// Vielfaches von [`PLANE_ALIGN`], also oft mehr als die Zeile nutzt.
    #[must_use]
    pub fn stride(&self, index: usize) -> Option<usize> {
        self.planes.get(index).map(|p| p.stride)
    }

    /// Genutzte Bytes je Zeile in der angegebenen Ebene (Breite der Ebene
    /// mal Bytes je Pixel), ohne das Padding bis [`VideoFrame::stride`].
    #[must_use]
    pub fn row_bytes(&self, index: usize) -> Option<usize> {
        self.planes.get(index).map(|p| p.row)
    }

    /// Gesamter Speicher der Ebenen in Bytes, samt Zeilen-Padding.
    #[must_use]
    pub fn byte_size(&self) -> usize {
        self.planes.iter().map(|p| p.len).sum()
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
        assert_eq!(f.row_bytes(1), Some(641));
        // 641 auf das naechste Vielfache von 64.
        assert_eq!(f.stride(1), Some(704));
        assert_eq!(f.plane(1).unwrap().len(), 704 * 361);
    }

    fn aligned(f: &VideoFrame, plane: usize) -> bool {
        f.plane(plane).unwrap().as_ptr().align_offset(PLANE_ALIGN) == 0
            && f.stride(plane).unwrap().is_multiple_of(PLANE_ALIGN)
    }

    #[test]
    fn nv12_ebenen_beginnen_ausgerichtet_mit_ausgerichteter_zeilenlaenge() {
        let mut m = meta(PixelFormat::Nv12);
        // Eine Breite, die kein Vielfaches von 64 ist, und eine ungerade.
        for (width, luma, chroma) in [
            (1920_usize, 1920, 1920),
            (1280, 1280, 1280),
            (1001, 1024, 1024),
        ] {
            m.width = u32::try_from(width).unwrap();
            let f = VideoFrame::alloc(m).unwrap();
            assert!(aligned(&f, 0) && aligned(&f, 1), "width {width}");
            assert_eq!(f.stride(0), Some(luma));
            assert_eq!(f.stride(1), Some(chroma));
            assert_eq!(f.row_bytes(0), Some(width));
            assert_eq!(f.row_bytes(1), Some(width.div_ceil(2) * 2));
            assert_eq!(f.plane(0).unwrap().len(), luma * 1080);
            assert_eq!(f.plane(1).unwrap().len(), chroma * 540);
        }
    }

    #[test]
    fn rgb0_ebene_beginnt_ausgerichtet_mit_ausgerichteter_zeilenlaenge() {
        let mut m = meta(PixelFormat::Rgb0);
        m.width = 1921;
        let mut f = VideoFrame::alloc(m).unwrap();
        assert!(aligned(&f, 0));
        assert_eq!(f.row_bytes(0), Some(7_684));
        assert_eq!(
            f.stride(0),
            Some(7_744),
            "1921 * 4 auf ein Vielfaches von 64"
        );
        assert_eq!(f.plane(0).unwrap().len(), 7_744 * 1080);
        assert_eq!(f.byte_size(), 7_744 * 1080);
        // Lese- und Schreibzugriff zeigen auf dieselben Bytes.
        let at = f.plane_mut(0).unwrap().as_ptr();
        assert_eq!(f.plane(0).unwrap().as_ptr(), at);
        assert!(f.row_bytes(1).is_none());
    }

    #[test]
    fn die_ausrichtung_ueberlebt_umzug_und_pool() {
        // Das Frame zieht um (in eine Box, in den Pool und zurueck); die
        // Bytes der Ebene bleiben, wo sie sind.
        let f = VideoFrame::alloc(meta(PixelFormat::Rgb0)).unwrap();
        let at = f.plane(0).unwrap().as_ptr();
        let moved = Box::new(f);
        assert_eq!(moved.plane(0).unwrap().as_ptr(), at);

        let mut pool = FramePool::new(meta(PixelFormat::Nv12), 1);
        let first = pool.acquire(Timestamp::from_millis(0)).unwrap();
        let at = first.get().plane(0).unwrap().as_ptr();
        assert!(aligned(first.get(), 0) && aligned(first.get(), 1));
        drop(first);
        let again = pool.acquire(Timestamp::from_millis(40)).unwrap();
        assert_eq!(again.get().plane(0).unwrap().as_ptr(), at);
        assert!(aligned(again.get(), 0) && aligned(again.get(), 1));
    }

    #[test]
    fn eine_groesse_die_ueberlaeuft_wird_abgelehnt() {
        let mut m = meta(PixelFormat::Rgb0);
        m.width = u32::MAX;
        m.height = u32::MAX;
        assert!(VideoFrame::alloc(m).is_none());
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
