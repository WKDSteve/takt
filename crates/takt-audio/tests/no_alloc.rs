// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Producer and consumer never allocate after `ring`: every call of the audio
//! path runs under `assert_no_alloc`, which aborts the test process on the
//! first allocation (in debug builds, which `cargo test` uses).

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use takt_audio::ring;

#[global_allocator]
static ALLOCATOR: AllocDisabler = AllocDisabler;

#[test]
fn push_pop_and_the_counters_do_not_allocate() {
    let (mut tx, mut rx) = ring(2, 5);
    let input = [0.25_f32; 16];
    let mut out = [0.0_f32; 16];
    assert_no_alloc(|| {
        for _ in 0..1_000 {
            // Eight frames into five: a wrap and an overflow on every round.
            tx.push(&input);
            let _ = tx.free_frames();
            let _ = rx.available_frames();
            rx.pop(&mut out[..6]);
            rx.pop(&mut out);
            let _ = tx.overflowed() + rx.overflowed();
        }
    });
    assert_eq!(tx.overflowed(), 3_000);
}

#[test]
fn the_callback_does_not_allocate_full_or_empty() {
    use std::sync::Arc;
    use takt_audio::OutputCallback;
    use takt_clock::{AudioMasterClock, ManualMonotonic, MonotonicSource};

    let time = Arc::new(ManualMonotonic::new());
    let clock = AudioMasterClock::new(
        48_000,
        Arc::clone(&time) as Arc<dyn MonotonicSource>,
        20_000_000,
    );
    let (mut tx, rx) = ring(2, 960);
    let mut callback = OutputCallback::new(
        rx,
        clock.writer().unwrap(),
        Arc::clone(&time) as Arc<dyn MonotonicSource>,
    );
    let stats = callback.stats();
    let mut out = [0.0_f32; 480 * 2];
    tx.push(&[0.5; 960 * 2]);
    // fill runs under assert_no_alloc itself; the outer guard also covers
    // the reads of the counters.
    assert_no_alloc(|| {
        for _ in 0..10 {
            time.advance(10_000_000);
            callback.fill(&mut out);
        }
        let _ = (stats.callbacks(), stats.xruns(), stats.silent_frames());
    });
    assert_eq!((stats.callbacks(), stats.xruns()), (10, 8));
}
