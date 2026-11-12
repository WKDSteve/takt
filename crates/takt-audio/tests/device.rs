// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The real device, only on request: `TAKT_DEVICE_TESTS=1` and the feature
//! `cpal`. CI has no audio output: there the test is listed as ignored with
//! its reason, and a run with `--include-ignored` still skips without the
//! variable.
//!
//!   TAKT_DEVICE_TESTS=1 cargo test -p takt-audio --features cpal --test device -- --ignored --nocapture
//!
//! It plays silence for half a second.

use std::sync::Arc;
use std::time::Duration;

use assert_no_alloc::AllocDisabler;

#[global_allocator]
static ALLOCATOR: AllocDisabler = AllocDisabler;

fn wanted() -> bool {
    if std::env::var("TAKT_DEVICE_TESTS").as_deref() == Ok("1") {
        return true;
    }
    println!("SKIP device test: set TAKT_DEVICE_TESTS=1 to play on the audio device");
    false
}

#[cfg(not(feature = "cpal"))]
#[test]
#[ignore = "SKIP: plays on the audio device; TAKT_DEVICE_TESTS=1 and --features cpal -- --ignored"]
fn the_device_plays_and_drives_the_clock() {
    if wanted() {
        panic!("TAKT_DEVICE_TESTS=1 needs the feature cpal (--features cpal)");
    }
}

#[cfg(feature = "cpal")]
#[test]
#[ignore = "SKIP: plays on the audio device; TAKT_DEVICE_TESTS=1 and --features cpal -- --ignored"]
fn the_device_plays_and_drives_the_clock() {
    use takt_audio::{Output, OutputCallback, OutputConfig, ring};
    use takt_clock::{AudioMasterClock, Clock, MonotonicSource, SystemMonotonic};

    if !wanted() {
        return;
    }
    let config = OutputConfig::default();
    let time: Arc<dyn MonotonicSource> = Arc::new(SystemMonotonic::new());
    let clock = AudioMasterClock::new(config.sample_rate, Arc::clone(&time), 20_000_000);
    let (mut tx, rx) = ring(2, 4_800);
    tx.push(&vec![0.0; 4_800 * 2]);
    let callback = OutputCallback::new(rx, clock.writer().unwrap(), time);
    let stats = callback.stats();
    let output = Output::open(config, callback).expect("an audio output device");
    std::thread::sleep(Duration::from_millis(500));
    let position = clock.now();
    let buffer = output.buffer();
    drop(output);

    println!(
        "buffer {:?}, {} callbacks, {} frames, {} xruns, position {position}",
        buffer,
        stats.callbacks(),
        stats.frames(),
        stats.xruns()
    );
    assert!(stats.callbacks() > 0, "the device never called back");
    assert!(stats.frames() > 0, "nothing was taken from the ring");
    // Half a second at 48 kHz, with room for the start of the stream.
    let ticks = position.ticks();
    assert!(
        (4_800..=48_000).contains(&ticks),
        "position {ticks} after 500 ms"
    );
}
