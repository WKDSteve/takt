// SPDX-License-Identifier: GPL-3.0-or-later
//! Everything the main program reads from a helper process, over arbitrary bytes: the frames of the control
//! pipe and their messages, the counters and records of the ring, the stream table and the packets of the
//! demuxer. The harness itself is `takt_demux::fuzzing`, where the ordinary tests run it too.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| takt_demux::fuzzing::read_untrusted(data));
