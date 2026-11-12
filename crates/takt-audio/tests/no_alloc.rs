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
