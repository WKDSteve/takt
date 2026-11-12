// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The ring between two threads: every sample arrives once, in order.

use std::time::{Duration, Instant};

use takt_audio::ring;

/// `n` as an `f32`, exact because it stays below 2^24.
#[expect(clippy::cast_precision_loss, reason = "checked to be exact")]
fn exact(n: usize) -> f32 {
    assert!(n < 1 << 24, "{n} is not exact in f32");
    n as f32
}

#[test]
fn a_million_frames_arrive_once_and_in_order() {
    // Each sample is its own index, exact in f32 up to 2^24 (two million
    // samples here). A lost sample shows as a gap, a doubled one as a step back,
    // a torn frame as a channel that does not follow the one before it.
    const FRAMES: usize = 1_000_000;
    const CHANNELS: usize = 2;
    // A prime capacity: the runs split at another place on almost every push.
    const CAPACITY: usize = 1_021;

    let (mut tx, mut rx) = ring(CHANNELS, CAPACITY);
    let deadline = Instant::now() + Duration::from_secs(120);

    let producer = std::thread::spawn(move || {
        let mut chunk = [0.0_f32; 97 * CHANNELS];
        let mut sent = 0_usize;
        let mut size = 1_usize;
        while sent < FRAMES {
            assert!(Instant::now() < deadline, "producer stuck at frame {sent}");
            // Only what fits, so this test loses nothing on purpose; the
            // overflow path has a test of its own.
            let frames = size.min(FRAMES - sent).min(tx.free_frames());
            if frames == 0 {
                std::thread::yield_now();
                continue;
            }
            for (i, sample) in chunk[..frames * CHANNELS].iter_mut().enumerate() {
                *sample = exact(sent * CHANNELS + i);
            }
            assert_eq!(tx.push(&chunk[..frames * CHANNELS]), frames);
            sent += frames;
            size = size % 97 + 1;
        }
        tx.overflowed()
    });

    let mut out = [0.0_f32; 61 * CHANNELS];
    let mut received = 0_usize;
    let mut size = 1_usize;
    while received < FRAMES {
        assert!(
            Instant::now() < deadline,
            "consumer stuck at frame {received}"
        );
        let frames = rx.pop(&mut out[..size * CHANNELS]);
        if frames == 0 {
            if producer.is_finished() && rx.available_frames() == 0 {
                // A producer that panicked: its join below says why.
                break;
            }
            std::thread::yield_now();
            continue;
        }
        for (i, &sample) in out[..frames * CHANNELS].iter().enumerate() {
            let expected = exact(received * CHANNELS + i);
            assert_eq!(sample, expected, "sample {expected} came out as {sample}");
        }
        received += frames;
        size = size % 61 + 1;
    }

    assert_eq!(producer.join().unwrap(), 0, "nothing was dropped");
    assert_eq!(rx.available_frames(), 0, "nothing came twice");
}
