// SPDX-License-Identifier: GPL-3.0-or-later
//! Everything the main program reads from the decode helper, over arbitrary bytes: the head of a frame record,
//! the copy of its planes into a frame of the pool, the counts of the packet window, and what the helper reads of
//! its parent. The harness itself is `takt_decode::fuzzing`, where the ordinary tests run it too.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| takt_decode::fuzzing::read_untrusted(data));
