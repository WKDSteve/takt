// SPDX-License-Identifier: GPL-3.0-or-later
//! What the main program does with a record of the decode helper: check it, count it, and copy
//! its picture into a frame of the [`FramePool`].
//!
//! [`Intake`] is the part of the decoder that holds no process and no shared memory, only the
//! rules: so the tests and the fuzz harness ([`crate::fuzzing`]) run exactly the code that reads
//! the bytes of a real helper.
//!
//! # The pool
//!
//! Pictures land in frames of a [`FramePool`] of fixed size. The pool is made with the first
//! picture and made anew when the stream changes its size, format or colour; it never grows.
//! When every frame of it is out, the next picture stays in the ring: [`Intake::ready`] says so
//! and counts it ([`DecodeStats::pool_exhausted`]), and nothing is allocated in its place. The
//! helper then finds the ring full and stops taking packets, so the wait reaches back to the
//! demuxer by itself.

use std::time::Instant;

use takt_core::{FrameMeta, FramePool, PixelFormat, PooledFrame, Rational, Timestamp, VideoFrame};
use takt_ipc::ProtocolError;

use crate::proto::{FrameHead, Layout, Record, WireFormat, parse_record};

/// The counters of a decoder. Numbers only, cheap to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecodeStats {
    /// Packets sent to the helper.
    pub packets: u64,
    /// Pictures copied into a frame of the pool.
    pub frames: u64,
    /// Packets the decoder could not decode (the stream went on).
    pub errors: u64,
    /// Records from before a flush that were thrown away.
    pub stale: u64,
    /// Pictures from before the target of a jump that were not copied ([`Intake::skip_before`]).
    pub skipped: u64,
    /// Times a picture was ready and every frame of the pool was out. Counted once per wait, not
    /// once per look: it is the number of stalls, and the measure of a consumer that does not
    /// keep up.
    pub pool_exhausted: u64,
    /// Pools made: 1 for a stream that keeps its size, format and colour.
    pub pools: u64,
    /// Nanoseconds spent copying records out of the shared memory, in all: the first of the two
    /// copies a picture takes in this process (nothing is read in place there).
    pub read_nanos: u64,
    /// Nanoseconds spent copying pictures into frames of the pool, in all: the second copy.
    pub copy_nanos: u64,
    /// Bytes of pictures copied into frames, in all.
    pub copied_bytes: u64,
}

/// A decoded picture in a frame of the pool. Dropping it gives the frame back.
#[derive(Debug)]
pub struct Decoded {
    /// The picture. Its timestamp is the one of the packet, in the time base of the stream; the
    /// start of that base when [`Decoded::timed`] is false.
    pub frame: PooledFrame,
    /// Whether the stream gave the picture a timestamp.
    pub timed: bool,
    /// The picture is a keyframe.
    pub key: bool,
    /// The decoder marks the picture as damaged.
    pub corrupt: bool,
    /// Matrix coefficients (ITU-T H.273) for the way from Y'CbCr to R'G'B', 2 for unspecified.
    pub matrix: u8,
}

/// What a record turned out to be.
#[derive(Debug)]
pub enum Taken {
    /// A picture.
    Frame(Decoded),
    /// The decoder has handed out everything after a drain.
    End,
    /// Nothing to hand on: a count, an error that was counted, a record from before a flush.
    Nothing,
}

/// The rules of the reading side of a decoder.
#[derive(Debug)]
pub struct Intake {
    time_base: Rational,
    layout: Layout,
    pool_frames: usize,
    pool: Option<(FrameMeta, FramePool)>,
    /// A frame taken from the pool for the next picture.
    spare: Option<PooledFrame>,
    /// Whether the last look found the pool empty: the stall is counted once.
    starved: bool,
    serial: u32,
    sent: u64,
    consumed: u64,
    /// Pictures with a timestamp below this many ticks are counted and not copied.
    skip_before: Option<i64>,
    /// What the container says about the colour, for what the bitstream leaves unspecified:
    /// range, primaries, transfer, matrix.
    container: [u8; 4],
    stats: DecodeStats,
    last_error: Option<(i32, String)>,
}

impl Intake {
    /// The rules for a stream with the time base `time_base`, whose pictures go into pools of
    /// `pool_frames` frames (at least one) laid out as `layout`.
    #[must_use]
    pub fn new(time_base: Rational, layout: Layout, pool_frames: usize) -> Self {
        Self {
            time_base,
            layout,
            pool_frames: pool_frames.max(1),
            pool: None,
            spare: None,
            starved: false,
            serial: 0,
            sent: 0,
            consumed: 0,
            skip_before: None,
            container: [0, 2, 2, 2],
            stats: DecodeStats::default(),
            last_error: None,
        }
    }

    /// What the container says about the colour of the stream: FFmpeg's number of the range (1
    /// limited, 2 full, 0 unspecified) and the code points of ITU-T H.273 for primaries, transfer
    /// and matrix (2 is unspecified). A picture is read by what its bitstream says; where that
    /// says nothing, by this.
    pub const fn container_color(&mut self, range: u8, primaries: u8, transfer: u8, matrix: u8) {
        self.container = [range, primaries, transfer, matrix];
    }

    /// The counters.
    #[must_use]
    pub const fn stats(&self) -> DecodeStats {
        self.stats
    }

    /// The serial records must carry to be taken.
    #[must_use]
    pub const fn serial(&self) -> u32 {
        self.serial
    }

    /// Packets sent that the helper has not taken yet.
    #[must_use]
    pub const fn in_flight(&self) -> u64 {
        self.sent - self.consumed
    }

    /// Adds the time one record took to be copied out of the ring.
    pub const fn read_took(&mut self, nanos: u64) {
        self.stats.read_nanos = self.stats.read_nanos.saturating_add(nanos);
    }

    /// Counts a packet as sent.
    pub const fn sent(&mut self) {
        self.sent += 1;
        self.stats.packets += 1;
    }

    /// Starts a new serial (a flush) and returns it: records of the old one are thrown away from
    /// here on. A limit of [`skip_before`](Self::skip_before) ends with it.
    pub const fn flush(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.skip_before = None;
        self.serial
    }

    /// Pictures with a timestamp below `ticks` (in the time base of the stream) are counted and
    /// thrown away without being copied: what a decoder puts out between the keyframe a jump
    /// landed on and the target of the jump. `None` takes every picture again.
    pub const fn skip_before(&mut self, ticks: Option<i64>) {
        self.skip_before = ticks;
    }

    /// Frames of the current pool that are out, and its size; `(0, size)` before the first
    /// picture.
    #[must_use]
    pub fn pool(&self) -> (usize, usize) {
        let out = self.pool.as_ref().map_or(0, |(_, pool)| pool.in_flight());
        // The frame held for the next picture is not out yet.
        (
            out.saturating_sub(usize::from(self.spare.is_some())),
            self.pool_frames,
        )
    }

    /// The last error the decoder reported for a packet: the library's number and text.
    #[must_use]
    pub fn last_error(&self) -> Option<&(i32, String)> {
        self.last_error.as_ref()
    }

    /// Whether a frame is at hand for the next picture. `false` when every frame of the pool is
    /// out: the record then stays where it is, and the stall is counted. Before the first picture
    /// there is no pool to run dry.
    pub fn ready(&mut self) -> bool {
        let Some((_, pool)) = &mut self.pool else {
            return true;
        };
        if self.spare.is_none() {
            self.spare = pool.acquire(Timestamp::zero(self.time_base));
        }
        if self.spare.is_some() {
            self.starved = false;
            return true;
        }
        if !self.starved {
            self.starved = true;
            self.stats.pool_exhausted += 1;
        }
        false
    }

    /// Takes one record of the ring. Call [`ready`](Self::ready) first.
    ///
    /// # Errors
    /// A [`ProtocolError`] for a record that is not the protocol or counts that cannot be: the
    /// helper has lied and is to be ended.
    pub fn accept(&mut self, record: &[u8]) -> Result<Taken, ProtocolError> {
        let (serial, consumed, record) = parse_record(record)?;
        // The count runs through every record, also the ones from before a flush.
        if consumed < self.consumed || consumed > self.sent {
            return Err(ProtocolError::Invalid("packets taken"));
        }
        self.consumed = consumed;
        if serial != self.serial {
            self.stats.stale += 1;
            return Ok(Taken::Nothing);
        }
        match record {
            Record::Status => Ok(Taken::Nothing),
            Record::End => Ok(Taken::End),
            Record::Error(code, text) => {
                self.stats.errors += 1;
                self.last_error = Some((code, text));
                Ok(Taken::Nothing)
            }
            Record::Frame(head, planes) => self.frame(&head, planes),
        }
    }

    fn frame(&mut self, head: &FrameHead, planes: &[u8]) -> Result<Taken, ProtocolError> {
        if let (Some(limit), Some(pts)) = (self.skip_before, head.pts)
            && pts < limit
        {
            self.stats.skipped += 1;
            return Ok(Taken::Nothing);
        }
        // What the bitstream leaves open, the container may know.
        let stated = |frame: u8, container: u8| {
            if frame == 0 || frame == 2 {
                container
            } else {
                frame
            }
        };
        let head = &FrameHead {
            range: if head.range == 0 {
                self.container[0]
            } else {
                head.range
            },
            primaries: stated(head.primaries, self.container[1]),
            transfer: stated(head.transfer, self.container[2]),
            matrix: stated(head.matrix, self.container[3]),
            ..*head
        };
        let color = head.color();
        let meta = FrameMeta {
            width: head.width,
            height: head.height,
            format: head.format.pixel_format(self.layout),
            primaries: color.primaries,
            transfer: color.transfer,
            range: color.range,
            pts: Timestamp::zero(self.time_base),
        };
        if self.pool.as_ref().is_none_or(|(known, _)| *known != meta) {
            // The stream changed (or begins): a pool for the new pictures. Frames of the old one
            // that are still out go back to it and are freed with it.
            self.spare = None;
            self.pool = Some((meta, FramePool::new(meta, self.pool_frames)));
            self.stats.pools += 1;
        }
        let mut frame = match (self.spare.take(), &mut self.pool) {
            (Some(frame), _) => frame,
            (None, Some((_, pool))) => pool
                .acquire(meta.pts)
                // A new pool has a frame unless the geometry cannot be allocated; a known pool
                // has one because `ready` held it.
                .ok_or(ProtocolError::Invalid("no frame for the picture"))?,
            (None, None) => return Err(ProtocolError::Invalid("no frame for the picture")),
        };
        let started = Instant::now();
        copy_planes(head, planes, self.layout, frame.get_mut())?;
        self.stats.copy_nanos = self
            .stats
            .copy_nanos
            .saturating_add(u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        self.stats.copied_bytes = self.stats.copied_bytes.saturating_add(planes.len() as u64);
        self.stats.frames += 1;
        if let Some(ticks) = head.pts {
            frame
                .get_mut()
                .set_pts(Timestamp::new(ticks, self.time_base));
        }
        Ok(Taken::Frame(Decoded {
            frame,
            timed: head.pts.is_some(),
            key: head.key,
            corrupt: head.corrupt,
            matrix: head.matrix,
        }))
    }
}

/// The rows of one plane in a record: `rows` rows of `row` bytes, `stride` bytes apart.
fn rows(plane: &[u8], stride: usize, row: usize) -> impl Iterator<Item = &[u8]> {
    // The last row ends with its last pixel, so the last chunk is a row long and the ones before
    // it a stride; `row <= stride` was checked by the parser.
    plane.chunks(stride.max(1)).map(move |chunk| &chunk[..row])
}

/// Copies the planes of a record into `frame`, whose geometry must be the one `head` states for
/// `layout`.
///
/// # Errors
/// [`ProtocolError::Invalid`] when the frame is not the one for this picture.
pub fn copy_planes(
    head: &FrameHead,
    planes: &[u8],
    layout: Layout,
    frame: &mut VideoFrame,
) -> Result<(), ProtocolError> {
    let wrong = ProtocolError::Invalid("frame for another picture");
    let format = head.format.pixel_format(layout);
    let meta = *frame.meta();
    if meta.width != head.width || meta.height != head.height || meta.format != format {
        return Err(wrong);
    }
    if planes.len() != head.body_len() {
        return Err(ProtocolError::Invalid("length of the planes"));
    }
    let (luma, chroma) = planes.split_at(head.plane_len(0));
    let (cb, cr) = chroma.split_at(head.plane_len(1));
    let source = [luma, cb, cr];
    let stride = head.stride.map(|s| s as usize);
    let row = [0, 1, 2].map(|plane| head.format.plane(plane, head.width, head.height).0);
    let semi_planar = matches!(format, PixelFormat::Nv12 | PixelFormat::P010);
    let wide = head.format == WireFormat::Yuv420P10;

    let planes_to_copy = if semi_planar { 1 } else { 3 };
    for plane in 0..planes_to_copy {
        let to_stride = frame.stride(plane).ok_or(wrong.clone())?;
        if frame.row_bytes(plane) != Some(row[plane]) {
            return Err(wrong);
        }
        let to = frame.plane_mut(plane).ok_or(wrong.clone())?;
        for (from, to) in
            rows(source[plane], stride[plane], row[plane]).zip(to.chunks_mut(to_stride))
        {
            if semi_planar && wide {
                widen(from, &mut to[..row[plane]]);
            } else {
                to[..row[plane]].copy_from_slice(from);
            }
        }
    }
    if semi_planar {
        let to_stride = frame.stride(1).ok_or(wrong.clone())?;
        if frame.row_bytes(1) != Some(row[1] * 2) {
            return Err(wrong);
        }
        let to = frame.plane_mut(1).ok_or(wrong)?;
        let cb = rows(source[1], stride[1], row[1]);
        let cr = rows(source[2], stride[2], row[2]);
        for ((cb, cr), to) in cb.zip(cr).zip(to.chunks_mut(to_stride)) {
            if wide {
                interleave_wide(cb, cr, &mut to[..row[1] * 2]);
            } else {
                interleave(cb, cr, &mut to[..row[1] * 2]);
            }
        }
    }
    Ok(())
}

/// `cb` and `cr` side by side, sample by sample: the chroma plane of NV12.
fn interleave(cb: &[u8], cr: &[u8], to: &mut [u8]) {
    let (pairs, _) = to.as_chunks_mut::<2>();
    for ((pair, &cb), &cr) in pairs.iter_mut().zip(cb).zip(cr) {
        *pair = [cb, cr];
    }
}

/// A sample of 10 bits in the low bits of a little-endian word, as P010 holds it: in the high
/// bits. Bits above the tenth are not a sample (the bytes are foreign) and are cut off.
fn p010(sample: [u8; 2]) -> [u8; 2] {
    let value = u16::from_le_bytes(sample) & 0x03FF;
    (value << 6).to_le_bytes()
}

/// A row of 10-bit samples moved into the high bits: the luma plane of P010.
fn widen(from: &[u8], to: &mut [u8]) {
    let (to, _) = to.as_chunks_mut::<2>();
    let (from, _) = from.as_chunks::<2>();
    for (to, from) in to.iter_mut().zip(from) {
        *to = p010(*from);
    }
}

/// The chroma plane of P010: `cb` and `cr` side by side, each in the high bits of its word.
fn interleave_wide(cb: &[u8], cr: &[u8], to: &mut [u8]) {
    let (quads, _) = to.as_chunks_mut::<4>();
    let (cb, _) = cb.as_chunks::<2>();
    let (cr, _) = cr.as_chunks::<2>();
    for ((quad, cb), cr) in quads.iter_mut().zip(cb).zip(cr) {
        let (cb, cr) = (p010(*cb), p010(*cr));
        *quad = [cb[0], cb[1], cr[0], cr[1]];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{end_tail, error_tail, status_record};

    const BASE: Rational = Rational::lit(1, 25);

    /// A record of a `width` by `height` picture whose sample at plane `p`, row `y`, column `x`
    /// is `p * 100 + y * 10 + x`; strides are a few bytes more than a row, the padding is 255.
    fn picture(format: WireFormat, width: u32, height: u32, serial: u32, consumed: u64) -> Vec<u8> {
        let mut head = FrameHead {
            serial,
            consumed,
            pts: Some(i64::try_from(consumed).unwrap()),
            width,
            height,
            format,
            stride: [0; 3],
            range: 1,
            primaries: 1,
            transfer: 1,
            matrix: 1,
            key: consumed == 1,
            corrupt: false,
        };
        let mut body = Vec::new();
        for (plane, stride) in head.stride.iter_mut().enumerate() {
            let (row, rows) = format.plane(plane, width, height);
            *stride = u32::try_from(row + 3 + plane).unwrap();
            for y in 0..rows {
                for x in 0..row / format.sample_bytes() {
                    let value = u16::try_from(plane * 100 + y * 10 + x).unwrap();
                    if format.sample_bytes() == 2 {
                        body.extend_from_slice(&value.to_le_bytes());
                    } else {
                        body.push(u8::try_from(value).unwrap());
                    }
                }
                if y + 1 < rows {
                    body.resize(body.len() + 3 + plane, 255);
                }
            }
        }
        assert_eq!(body.len(), head.body_len());
        body.extend_from_slice(&head.tail());
        body
    }

    fn take(intake: &mut Intake, record: &[u8]) -> Decoded {
        assert!(intake.ready());
        match intake.accept(record).unwrap() {
            Taken::Frame(decoded) => decoded,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_planar_picture_lands_row_by_row_without_its_padding() {
        let mut intake = Intake::new(BASE, Layout::Planar, 2);
        intake.sent();
        let decoded = take(&mut intake, &picture(WireFormat::Yuv420P8, 5, 3, 0, 1));
        let frame = decoded.frame.get();
        assert_eq!(frame.meta().format, PixelFormat::Yuv420P);
        assert_eq!(frame.meta().pts, Timestamp::new(1, BASE));
        assert!(decoded.timed && decoded.key);
        assert_eq!(decoded.matrix, 1);
        let stride = frame.stride(0).unwrap();
        assert_eq!(&frame.plane(0).unwrap()[..5], &[0, 1, 2, 3, 4]);
        assert_eq!(
            &frame.plane(0).unwrap()[2 * stride..2 * stride + 5],
            &[20, 21, 22, 23, 24]
        );
        let stride = frame.stride(1).unwrap();
        assert_eq!(
            &frame.plane(1).unwrap()[stride..stride + 3],
            &[110, 111, 112]
        );
        assert_eq!(&frame.plane(2).unwrap()[..3], &[200, 201, 202]);
        assert_eq!(intake.stats().frames, 1);
        assert_eq!(intake.stats().pools, 1);
        // Luma 2 strides of 8 and a row of 5, chroma a stride (7, then 8) and a row of 3.
        assert_eq!(intake.stats().copied_bytes, (8 * 2 + 5) + (7 + 3) + (8 + 3));
    }

    #[test]
    fn a_semi_planar_picture_gets_its_chroma_interleaved() {
        let mut intake = Intake::new(BASE, Layout::SemiPlanar, 2);
        intake.sent();
        let decoded = take(&mut intake, &picture(WireFormat::Yuv420P8, 5, 3, 0, 1));
        let frame = decoded.frame.get();
        assert_eq!(frame.meta().format, PixelFormat::Nv12);
        assert_eq!(&frame.plane(0).unwrap()[..5], &[0, 1, 2, 3, 4]);
        let stride = frame.stride(1).unwrap();
        assert_eq!(
            &frame.plane(1).unwrap()[..6],
            &[100, 200, 101, 201, 102, 202]
        );
        assert_eq!(
            &frame.plane(1).unwrap()[stride..stride + 6],
            &[110, 210, 111, 211, 112, 212]
        );
    }

    #[test]
    fn ten_bits_move_into_the_high_bits_of_p010() {
        let mut intake = Intake::new(BASE, Layout::SemiPlanar, 2);
        intake.sent();
        let decoded = take(&mut intake, &picture(WireFormat::Yuv420P10, 4, 2, 0, 1));
        let frame = decoded.frame.get();
        assert_eq!(frame.meta().format, PixelFormat::P010);
        let word = |plane: &[u8], at: usize| u16::from_le_bytes([plane[at * 2], plane[at * 2 + 1]]);
        let luma = frame.plane(0).unwrap();
        assert_eq!(word(luma, 3), 3 << 6);
        let stride = frame.stride(0).unwrap();
        assert_eq!(word(&luma[stride..], 2), 12 << 6);
        let chroma = frame.plane(1).unwrap();
        assert_eq!(
            [
                word(chroma, 0),
                word(chroma, 1),
                word(chroma, 2),
                word(chroma, 3)
            ],
            [100 << 6, 200 << 6, 101 << 6, 201 << 6]
        );
        // Bits above the tenth are no sample.
        assert_eq!(p010([0xFF, 0xFF]), (0x03FFu16 << 6).to_le_bytes());

        // Planar keeps the words as they came.
        let mut planar = Intake::new(BASE, Layout::Planar, 1);
        planar.sent();
        let decoded = take(&mut planar, &picture(WireFormat::Yuv420P10, 4, 2, 0, 1));
        assert_eq!(decoded.frame.get().meta().format, PixelFormat::Yuv420P10);
        assert_eq!(word(decoded.frame.get().plane(0).unwrap(), 3), 3);
    }

    #[test]
    fn an_empty_pool_holds_the_picture_back_and_is_counted_once() {
        let mut intake = Intake::new(BASE, Layout::Planar, 2);
        for _ in 0..4 {
            intake.sent();
        }
        let first = take(&mut intake, &picture(WireFormat::Yuv420P8, 4, 4, 0, 1));
        let second = take(&mut intake, &picture(WireFormat::Yuv420P8, 4, 4, 0, 2));
        assert_eq!(intake.pool(), (2, 2));
        // Both frames are out: the next picture has to wait, however often it is asked for.
        for _ in 0..5 {
            assert!(!intake.ready());
        }
        assert_eq!(intake.stats().pool_exhausted, 1);
        assert_eq!(intake.stats().pools, 1, "no second pool, no new frame");
        let address = first.frame.get().plane(0).unwrap().as_ptr();
        drop(first);
        // The frame that came back is the one the next picture lands in.
        let third = take(&mut intake, &picture(WireFormat::Yuv420P8, 4, 4, 0, 3));
        assert_eq!(third.frame.get().plane(0).unwrap().as_ptr(), address);
        assert!(!intake.ready());
        assert_eq!(intake.stats().pool_exhausted, 2, "a second stall");
        drop(second);
        drop(third);
        assert!(intake.ready());
        assert_eq!(intake.pool(), (0, 2));
    }

    #[test]
    fn a_stream_that_changes_its_size_gets_a_new_pool() {
        let mut intake = Intake::new(BASE, Layout::Planar, 1);
        intake.sent();
        intake.sent();
        let small = take(&mut intake, &picture(WireFormat::Yuv420P8, 4, 4, 0, 1));
        // The one frame of the old pool is out, and the new picture is of another size: it gets
        // the first frame of a new pool.
        assert!(!intake.ready());
        let record = picture(WireFormat::Yuv420P8, 8, 6, 0, 2);
        let Taken::Frame(large) = intake.accept(&record).unwrap() else {
            panic!("no frame");
        };
        assert_eq!(large.frame.get().meta().width, 8);
        assert_eq!(small.frame.get().meta().width, 4);
        assert_eq!(intake.stats().pools, 2);
    }

    #[test]
    fn records_from_before_a_flush_are_thrown_away_but_counted() {
        let mut intake = Intake::new(BASE, Layout::Planar, 2);
        intake.sent();
        intake.sent();
        assert_eq!(intake.in_flight(), 2);
        assert_eq!(intake.flush(), 1);
        let stale = picture(WireFormat::Yuv420P8, 4, 4, 0, 1);
        assert!(matches!(intake.accept(&stale).unwrap(), Taken::Nothing));
        assert_eq!(
            intake.in_flight(),
            1,
            "the packet counts as taken all the same"
        );
        assert_eq!(intake.stats().stale, 1);
        assert_eq!(intake.stats().frames, 0);
        assert!(matches!(
            intake.accept(&status_record(1, 2)).unwrap(),
            Taken::Nothing
        ));
        assert_eq!(intake.in_flight(), 0);
        assert!(matches!(
            intake.accept(&end_tail(1, 2)).unwrap(),
            Taken::End
        ));
        let mut error = b"bad".to_vec();
        error.extend_from_slice(&error_tail(1, 2, -22));
        assert!(matches!(intake.accept(&error).unwrap(), Taken::Nothing));
        assert_eq!(intake.stats().errors, 1);
        assert_eq!(intake.last_error(), Some(&(-22, "bad".to_owned())));
    }

    #[test]
    fn pictures_before_the_target_of_a_jump_are_counted_and_not_copied() {
        let mut intake = Intake::new(BASE, Layout::Planar, 1);
        for _ in 0..3 {
            intake.sent();
        }
        intake.skip_before(Some(3));
        // Timestamps 1 and 2 lie before the target: no frame is used for them.
        for consumed in 1..=2 {
            let record = picture(WireFormat::Yuv420P8, 4, 4, 0, consumed);
            assert!(matches!(intake.accept(&record).unwrap(), Taken::Nothing));
        }
        assert_eq!(intake.stats().skipped, 2);
        assert_eq!(intake.stats().frames, 0);
        assert_eq!(intake.stats().pools, 0);
        let third = take(&mut intake, &picture(WireFormat::Yuv420P8, 4, 4, 0, 3));
        assert_eq!(third.frame.get().meta().pts, Timestamp::new(3, BASE));
        // A flush ends the limit.
        intake.flush();
        intake.skip_before(Some(9));
        intake.flush();
        intake.sent();
        drop(third);
        take(&mut intake, &picture(WireFormat::Yuv420P8, 4, 4, 2, 4));
    }

    #[test]
    fn the_container_says_what_the_bitstream_leaves_open() {
        let mut record = picture(WireFormat::Yuv420P8, 4, 4, 0, 1);
        // A bitstream that names its matrix and nothing else.
        let at = record.len() - crate::proto::FRAME_TAIL;
        record[at + 41..at + 45].copy_from_slice(&[0, 2, 2, 6]);
        let mut intake = Intake::new(BASE, Layout::Planar, 2);
        intake.sent();
        intake.sent();
        intake.container_color(2, 9, 16, 9);
        let decoded = take(&mut intake, &record);
        let meta = *decoded.frame.get().meta();
        assert_eq!(meta.primaries, takt_core::ColorPrimaries::Bt2020);
        assert_eq!(meta.transfer, takt_core::TransferFunction::Pq);
        assert_eq!(meta.range, takt_core::ColorRange::Full);
        assert_eq!(decoded.matrix, 6, "the bitstream named it");
        // A bitstream that names everything is believed.
        let named = picture(WireFormat::Yuv420P8, 4, 4, 0, 2);
        let decoded = take(&mut intake, &named);
        assert_eq!(
            decoded.frame.get().meta().primaries,
            takt_core::ColorPrimaries::Bt709
        );
        assert_eq!(
            decoded.frame.get().meta().range,
            takt_core::ColorRange::Limited
        );
    }

    #[test]
    fn a_helper_that_counts_packets_nobody_sent_has_lied() {
        let mut intake = Intake::new(BASE, Layout::Planar, 2);
        intake.sent();
        assert_eq!(
            intake.accept(&status_record(0, 2)).unwrap_err(),
            ProtocolError::Invalid("packets taken")
        );
        intake.accept(&status_record(0, 1)).unwrap();
        // The count never runs backwards.
        assert_eq!(
            intake.accept(&status_record(0, 0)).unwrap_err(),
            ProtocolError::Invalid("packets taken")
        );
    }

    #[test]
    fn a_frame_of_another_geometry_is_not_written_to() {
        let record = picture(WireFormat::Yuv420P8, 4, 4, 0, 1);
        let (_, _, Record::Frame(head, planes)) = parse_record(&record).unwrap() else {
            panic!("no frame");
        };
        let meta = |width, format| FrameMeta {
            width,
            height: 4,
            format,
            primaries: takt_core::ColorPrimaries::Bt709,
            transfer: takt_core::TransferFunction::Bt709,
            range: takt_core::ColorRange::Limited,
            pts: Timestamp::zero(BASE),
        };
        let mut other = VideoFrame::alloc(meta(6, PixelFormat::Yuv420P)).unwrap();
        assert!(copy_planes(&head, planes, Layout::Planar, &mut other).is_err());
        let mut nv12 = VideoFrame::alloc(meta(4, PixelFormat::Nv12)).unwrap();
        assert!(copy_planes(&head, planes, Layout::Planar, &mut nv12).is_err());
        assert!(copy_planes(&head, planes, Layout::SemiPlanar, &mut nv12).is_ok());
        assert!(copy_planes(&head, &planes[1..], Layout::SemiPlanar, &mut nv12).is_err());
    }
}
