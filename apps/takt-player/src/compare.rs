// SPDX-License-Identifier: GPL-3.0-or-later
//! The own pipeline against libmpv: the same file, the same timestamp, the same picture?
//!
//! libmpv is the reference oracle of the project (`takt-backend`): what the software path of
//! `takt-decode` decodes must be the picture mpv shows at that time. The two do not hand out the
//! same bytes: the own pipeline hands out the decoder's Y'CbCr, mpv a finished R'G'B' picture
//! (`rgb0`, converted by its software renderer with its own chroma upsampling and rounding). So
//! the test converts the decoded picture as BT.709 with interpolated chroma and
//! measures how far the two are apart, as a peak signal-to-noise ratio over all three channels.
//!
//! # The threshold
//!
//! Measured on 2026-11-17 (Windows 11, FFmpeg 9.0 LGPL build of BtbN, mpv 0.41.0-dev of
//! shinchiro), the lowest value of all 50 pictures of a file: 55.9 dB for H.264, HEVC and AV1 in
//! 8 bit, 51.7 dB in 10 bit (mpv dithers ten bits down to eight, the reference here rounds). The
//! numbers are in `docs/reviews/phase-1.md`. What a mistake looks like, measured by the second
//! test here: the picture next to the right one has 21.7 dB (the test clips move), the right
//! picture read with the matrix of BT.601 has 28.9 dB. [`MIN_PSNR_DB`] and [`MIN_PSNR_DB_10BIT`]
//! sit between the two groups, 7 to 10 dB below what was measured: room for another build of
//! mpv or FFmpeg, none for a wrong picture.
//!
//! # The same timestamp
//!
//! The own pipeline stamps a picture with the time its packet carries. The `MpvBackend` stamps
//! one with the position of mpv at the moment it rendered, which in a run on a busy machine can
//! be the time of a neighbour. So libmpv is asked the way that leaves no doubt: held, with an
//! exact jump to the time of a picture, and its position read once it has arrived. That position
//! must be the time asked for, and the picture it shows then is compared with the picture the own
//! pipeline stamps with exactly that time.
//!
//! Needs a build with libmpv and with FFmpeg (`TAKT_LIBMPV_DIR`, `FFMPEG_DIR`), and both helper
//! programs built in the same run: `cargo test -p takt-demux -p takt-decode -p takt-player`.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "sample values and their statistics, never a time"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use takt_backend::{SeekMode, VideoBackend};
use takt_clock::{Clock, ClockKind, SystemMonotonic};
use takt_core::{PixelFormat, Rational, Timestamp, VideoFrame};
use takt_decode::{BackendOptions, SoftwareBackend};

use crate::mpv::{Config, Early, MpvBackend, Sound, Sources};

/// The least a picture of the own pipeline must have against the picture libmpv shows at the
/// same time, in dB over R', G' and B' of all pixels.
const MIN_PSNR_DB: f64 = 45.0;

/// The least 10-bit material must have: mpv brings ten bits to eight on its way to `rgb0` with a
/// dither pattern, which the reference here does not imitate.
const MIN_PSNR_DB_10BIT: f64 = 42.0;

/// One frame of the test clips, in milliseconds.
const FRAME: i64 = 40;

/// The pictures of a clip that are compared: every third of its fifty and the last, so that
/// keyframes, pictures between them and both ends are among them.
fn compared_pictures() -> Vec<i64> {
    (0..50).step_by(3).chain([49]).collect()
}

fn clip(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// The directory the helper programs were built into: one above the test program.
fn helpers() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    for name in [DEMUX_HELPER, takt_decode::HELPER_NAME] {
        let helper = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        assert!(
            helper.is_file(),
            "{} is missing: build it in the same run (cargo test -p takt-demux -p takt-decode -p takt-player)",
            helper.display()
        );
    }
    dir
}

/// The name of the demux helper, which this crate does not link by itself.
const DEMUX_HELPER: &str = "takt-demux-helper";

/// A clock far beyond the end of every clip that lets nothing be dropped: every picture is due.
struct Late;

impl Clock for Late {
    fn kind(&self) -> ClockKind {
        ClockKind::Timeline
    }
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(3_600_000)
    }
    fn until_nanos(&self, _pts: Timestamp) -> i64 {
        -1
    }
    fn may_drop(&self) -> bool {
        false
    }
}

/// Y'CbCr to R'G'B' as BT.709 says it (Kr 0.2126, Kb 0.0722).
const BT709: [f32; 4] = [1.5748, 0.187_324, 0.468_124, 1.8556];
/// The same as BT.601 says it (Kr 0.299, Kb 0.114): the wrong one for the test clips.
const BT601: [f32; 4] = [1.402, 0.344_136, 0.714_136, 1.772];

/// A decoded picture as 8-bit planes without padding: Y', Cb, Cr, and its size.
struct Yuv {
    width: usize,
    height: usize,
    planes: [Vec<u8>; 3],
}

impl Yuv {
    fn from_frame(frame: &VideoFrame) -> Self {
        let meta = frame.meta();
        let wide = match meta.format {
            PixelFormat::Yuv420P => false,
            PixelFormat::Yuv420P10 => true,
            other => panic!("{other:?}"),
        };
        let planes = [0, 1, 2].map(|plane| {
            let (stride, row) = (
                frame.stride(plane).unwrap(),
                frame.row_bytes(plane).unwrap(),
            );
            let mut out = Vec::new();
            for line in frame.plane(plane).unwrap().chunks(stride) {
                if wide {
                    // Ten bits to eight, rounded.
                    out.extend(line[..row].chunks(2).map(|word| {
                        let value = u16::from_le_bytes([word[0], word[1]]);
                        ((value + 2) >> 2).min(255) as u8
                    }));
                } else {
                    out.extend_from_slice(&line[..row]);
                }
            }
            out
        });
        Self {
            width: meta.width as usize,
            height: meta.height as usize,
            planes,
        }
    }

    /// The chroma plane `plane` at the luma position (`x`, `y`), between its samples: 4:2:0 with
    /// the chroma sample at the left of its two columns and between its two rows (the siting of
    /// H.264, HEVC and AV1 unless a stream says otherwise).
    fn chroma(&self, plane: usize, x: usize, y: usize) -> f32 {
        let (cw, ch) = (self.width.div_ceil(2), self.height.div_ceil(2));
        let fx = x as f32 / 2.0;
        let fy = (y as f32 - 0.5) / 2.0;
        let at = |cx: usize, cy: usize| {
            f32::from(self.planes[plane][cy.min(ch - 1) * cw + cx.min(cw - 1)])
        };
        let (x0, y0) = (fx.floor().max(0.0), fy.floor().max(0.0));
        let (tx, ty) = ((fx - x0).clamp(0.0, 1.0), (fy - y0).clamp(0.0, 1.0));
        let (x0, y0) = (x0 as usize, y0 as usize);
        let top = at(x0, y0) * (1.0 - tx) + at(x0 + 1, y0) * tx;
        let bottom = at(x0, y0 + 1) * (1.0 - tx) + at(x0 + 1, y0 + 1) * tx;
        top * (1.0 - ty) + bottom * ty
    }

    /// The picture as R'G'B' bytes, limited range, with the chroma interpolated between its
    /// samples. `matrix` is what Cr gives to red, Cb and Cr take from green, and Cb gives to
    /// blue.
    fn to_rgb(&self, matrix: [f32; 4]) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.width * self.height * 3);
        for y in 0..self.height {
            for x in 0..self.width {
                let luma = (f32::from(self.planes[0][y * self.width + x]) - 16.0) / 219.0;
                let cb = (self.chroma(1, x, y) - 128.0) / 224.0;
                let cr = (self.chroma(2, x, y) - 128.0) / 224.0;
                let rgb = [
                    luma + matrix[0] * cr,
                    luma - matrix[1] * cb - matrix[2] * cr,
                    luma + matrix[3] * cb,
                ];
                out.extend(rgb.map(takt_gpu::colour::to_8bit));
            }
        }
        out
    }
}

/// The pixels of an `rgb0` frame as R'G'B' bytes without padding.
fn rgb_of(frame: &VideoFrame) -> Vec<u8> {
    assert_eq!(frame.meta().format, PixelFormat::Rgb0);
    let stride = frame.stride(0).unwrap();
    let width = frame.meta().width as usize;
    let mut out = Vec::with_capacity(width * frame.meta().height as usize * 3);
    for line in frame.plane(0).unwrap().chunks(stride) {
        for pixel in line[..width * 4].chunks(4) {
            out.extend_from_slice(&pixel[..3]);
        }
    }
    out
}

/// Peak signal-to-noise ratio of two pictures of 8-bit samples, in dB; 99 for equal pictures.
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let squares: u64 = a
        .iter()
        .zip(b)
        .map(|(a, b)| u64::from(a.abs_diff(*b)).pow(2))
        .sum();
    if squares == 0 {
        return 99.0;
    }
    let mse = squares as f64 / a.len() as f64;
    10.0 * (255.0_f64 * 255.0 / mse).log10()
}

fn millis(pts: Timestamp) -> i64 {
    pts.rescale(Rational::MILLIS).unwrap().ticks()
}

/// Every picture of `name` as the own pipeline decodes it: its time in milliseconds and its
/// R'G'B' bytes.
fn own_pictures(name: &str) -> Vec<(i64, Vec<u8>)> {
    own_decoded(name)
        .into_iter()
        .map(|(pts, picture)| (pts, picture.to_rgb(BT709)))
        .collect()
}

/// Every picture of `name` as the own pipeline decodes it: its time in milliseconds and its
/// planes.
fn own_decoded(name: &str) -> Vec<(i64, Yuv)> {
    let mut backend = SoftwareBackend::new(BackendOptions::in_dir(&helpers()));
    backend.open(&clip(name)).unwrap();
    let mut pictures = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !backend.is_eof() {
        assert!(Instant::now() < deadline, "{name}: the own pipeline hangs");
        match backend.next_frame(&Late).unwrap() {
            Some(frame) => {
                pictures.push((millis(frame.get().meta().pts), Yuv::from_frame(frame.get())));
            }
            None => std::thread::sleep(Duration::from_micros(200)),
        }
    }
    assert_eq!(backend.pipeline_stats().decode.errors, 0, "{name}");
    assert_eq!(backend.pipeline_stats().matrix, 1, "{name}: BT.709");
    pictures
}

/// The pictures libmpv shows of `name` at the times of `pictures` (numbers of pictures): for
/// each the position it arrived at, in milliseconds, and the R'G'B' bytes it shows there.
fn mpv_pictures(name: &str, pictures: &[i64]) -> Vec<(i64, Vec<u8>)> {
    let config = Config {
        sources: Sources::Files,
        sound: Sound::Silent,
        early: Early::HalfDisplayPeriod,
    };
    let mut backend = MpvBackend::new(config, Arc::new(SystemMonotonic::new())).unwrap();
    backend.open(&clip(name)).unwrap();
    // Held: the position read is where the jump landed, and the picture is the one of it.
    backend.set_paused(true).unwrap();
    let clock = backend.clock();
    let mut shown = Vec::new();
    for picture in pictures {
        // A little before the time of the picture: the first picture at or after the target is
        // the one an exact jump shows, and no rounding of a second decides which that is.
        let target = (picture * FRAME - FRAME / 4).max(0);
        backend
            .seek(Timestamp::from_millis(target), SeekMode::Exact)
            .unwrap();
        let asked = Instant::now();
        let mut rendered = None;
        let mut arrived: Option<(i64, Instant)> = None;
        loop {
            assert!(
                asked.elapsed() < Duration::from_secs(10),
                "{name}: libmpv did not arrive at picture {picture}"
            );
            // The newest picture since the jump was asked: the one of the jump.
            if let Some(frame) = backend.next_frame(clock.as_ref()).unwrap() {
                rendered = Some(rgb_of(frame.get()));
            }
            if arrived.is_none()
                && !backend.seeking()
                && let Some(position) = backend.position_millis()
            {
                arrived = Some((position, Instant::now()));
            }
            // A few passes more after it arrived: the picture of the jump is put down by the
            // render thread, not by the call that jumped.
            if let (Some((position, at)), Some(bytes)) = (arrived, &rendered)
                && at.elapsed() >= Duration::from_millis(80)
            {
                shown.push((position, bytes.clone()));
                break;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }
    shown
}

/// How the pictures of one file compare.
struct Compared {
    /// The lowest PSNR of a compared picture, in dB.
    lowest: f64,
    /// The mean PSNR, in dB.
    mean: f64,
    /// Pictures compared.
    pictures: usize,
}

/// Compares what both show of `name` at the same timestamps.
fn compare(name: &str) -> Compared {
    let own = own_pictures(name);
    assert_eq!(own.len(), 50, "{name}: every picture of the clip");
    let wanted = compared_pictures();
    let mpv = mpv_pictures(name, &wanted);
    assert_eq!(mpv.len(), wanted.len());
    let (mut lowest, mut sum) = (f64::MAX, 0.0);
    for (picture, (position, shown)) in wanted.iter().zip(&mpv) {
        // libmpv arrived at the time of the picture that was asked for ...
        assert_eq!(
            *position,
            picture * FRAME,
            "{name}: libmpv arrived at {position} ms for picture {picture}"
        );
        // ... and the own pipeline has a picture of exactly that time.
        let Some((_, decoded)) = own.iter().find(|(pts, _)| pts == position) else {
            panic!("{name}: the own pipeline has no picture at {position} ms");
        };
        assert_eq!(shown.len(), decoded.len(), "{name}: the same size");
        let db = psnr(decoded, shown);
        lowest = lowest.min(db);
        sum += db;
    }
    let mean = sum / mpv.len() as f64;
    println!(
        "{name}: {} pictures at the same timestamp, PSNR lowest {lowest:.2} dB, mean {mean:.2} dB",
        mpv.len()
    );
    Compared {
        lowest,
        mean,
        pictures: mpv.len(),
    }
}

#[test]
fn the_own_pipeline_shows_the_picture_libmpv_shows_at_the_same_time() {
    let _one_at_a_time = crate::session::OWN_PIPELINE_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (name, least) in [
        ("decode-h264.mp4", MIN_PSNR_DB),
        ("decode-hevc.mp4", MIN_PSNR_DB),
        ("decode-av1.mkv", MIN_PSNR_DB),
        ("decode-hevc-10bit.mp4", MIN_PSNR_DB_10BIT),
        ("decode-av1-10bit.mkv", MIN_PSNR_DB_10BIT),
        // The clip of the MpvBackend tests: no matrix named, both sides take BT.709.
    ] {
        let compared = compare(name);
        assert_eq!(compared.pictures, 18, "{name}");
        assert!(
            compared.lowest >= least,
            "{name}: a picture has {:.2} dB against libmpv, the threshold is {least} dB",
            compared.lowest
        );
        assert!(compared.mean >= compared.lowest);
    }
}

#[test]
fn a_picture_one_frame_off_or_of_the_wrong_matrix_fails_the_threshold() {
    let _one_at_a_time = crate::session::OWN_PIPELINE_TESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // What the threshold is for: the clip moves, so the picture next to the right one is far
    // away, and so is the right picture read with the matrix of BT.601.
    let own = own_decoded("decode-h264.mp4");
    let right = own[10].1.to_rgb(BT709);
    let neighbour = psnr(&right, &own[11].1.to_rgb(BT709));
    let wrong_matrix = psnr(&right, &own[10].1.to_rgb(BT601));
    println!("the picture after: {neighbour:.2} dB, the matrix of BT.601: {wrong_matrix:.2} dB");
    let least = MIN_PSNR_DB.min(MIN_PSNR_DB_10BIT);
    assert!(neighbour < least - 10.0, "{neighbour:.2} dB");
    assert!(wrong_matrix < least - 10.0, "{wrong_matrix:.2} dB");
    // The same picture with red and blue swapped is no match either.
    let swapped: Vec<u8> = right
        .chunks(3)
        .flat_map(|rgb| [rgb[2], rgb[1], rgb[0]])
        .collect();
    assert!(psnr(&right, &swapped) < least - 10.0);
    assert!((psnr(&right, &right) - 99.0).abs() < f64::EPSILON);
}
