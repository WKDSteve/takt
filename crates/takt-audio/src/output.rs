// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The device: an output stream of `cpal` that runs an [`OutputCallback`].
//!
//! # Which backend
//!
//! The default host of `cpal` on each platform: WASAPI in shared mode on
//! Windows, ALSA on Linux (PipeWire and PulseAudio through their ALSA plugin),
//! CoreAudio on macOS. Exclusive WASAPI (lower latency, no system mixer) and
//! the PipeWire host come later and need their own measurements.
//!
//! The stream has the rate and channel count of the ring, in the first
//! sample format of [`FORMATS`] the device takes: `f32` as it is in the ring,
//! else `i32` or `i16`, converted in the callback ([`crate::sample`], `i16`
//! with dither). A device that takes none of them, or not that rate, is
//! refused with [`OutputError::Unsupported`]; rate conversion belongs to a
//! resampling stage, not to the callback.
//!
//! # Thread priority
//!
//! `cpal` raises its audio thread itself when it is built with its feature
//! `realtime`: on Windows the MMCSS class "Audio" (not "Pro Audio") through
//! `audio_thread_priority`
//! (falling back to `THREAD_PRIORITY_TIME_CRITICAL`), on Linux `SCHED_FIFO`,
//! which needs `CAP_SYS_NICE` or an rtprio limit (`docs/BUILD.md`). On macOS
//! the CoreAudio thread is a real-time thread already. That feature pulls
//! `audio_thread_priority` (MPL-2.0) and so is not one of this zone 1 crate:
//! a zone 2 program turns it on in its own `Cargo.toml` and says so with
//! [`OutputConfig::realtime`].
//!
//! A refusal arrives as `RealtimeDenied` in the error callback, before the
//! first data callback; it is kept and read with [`Output::priority`] and
//! never stops the stream, which plays on at normal priority. A grant is not
//! reported by `cpal` (and ALSA skips the request for some device types), so
//! the best that can be said without a refusal is [`Priority::NoRefusal`].

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::callback::OutputCallback;

/// The sample formats an output is opened with, best first.
pub const FORMATS: [cpal::SampleFormat; 3] = [
    cpal::SampleFormat::F32,
    cpal::SampleFormat::I32,
    cpal::SampleFormat::I16,
];

/// How the output stream is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputConfig {
    /// Frames per second.
    pub sample_rate: u32,
    /// Frames per callback asked of the device. `None`: the device default.
    /// A size outside the range the device names, or a device that names
    /// none, gets the device default.
    pub buffer_frames: Option<u32>,
    /// The program builds `cpal` with its feature `realtime`, so `cpal` asks
    /// for real-time priority on its audio thread. Only a statement for
    /// [`Output::priority`]; this crate cannot see the features of `cpal`.
    pub realtime: bool,
}

impl Default for OutputConfig {
    /// 48 kHz with 480 frames per callback: 10 ms, half the extrapolation
    /// limit (20 ms) the player gives the audio master clock.
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            buffer_frames: Some(480),
            realtime: false,
        }
    }
}

/// Why no output could be opened.
#[derive(Debug)]
pub enum OutputError {
    /// The host has no default output device.
    NoDevice,
    /// The device takes none of [`FORMATS`] with this rate and channel count.
    Unsupported(String),
    /// `cpal` refused to build or start the stream.
    Stream(String),
}

impl fmt::Display for OutputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDevice => write!(f, "no default audio output device"),
            Self::Unsupported(why) => write!(f, "the audio device does not take the stream: {why}"),
            Self::Stream(why) => write!(f, "the audio stream did not start: {why}"),
        }
    }
}

impl std::error::Error for OutputError {}

/// What became of the request for real-time priority of the audio thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Priority {
    /// The program did not build `cpal` with `realtime`.
    NotRequested,
    /// `cpal` asked (where the device type allows it) and reported no
    /// refusal. Meaningful once the first callback ran.
    NoRefusal,
    /// The system refused; the stream plays on at normal priority.
    Refused(String),
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRequested => write!(f, "not requested"),
            Self::NoRefusal => write!(f, "requested, no refusal reported"),
            Self::Refused(why) => write!(f, "refused: {why}"),
        }
    }
}

/// A running output. Dropping it stops the stream.
pub struct Output {
    // Not Send on every platform: an output stays on the thread that opened it.
    _stream: cpal::Stream,
    buffer: cpal::BufferSize,
    format: cpal::SampleFormat,
    realtime: bool,
    refusal: Arc<Mutex<Option<String>>>,
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output")
            .field("buffer", &self.buffer)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl Output {
    /// Opens the default output device and starts `callback` on it.
    ///
    /// # Errors
    ///
    /// [`OutputError`] when there is no device, it takes no such stream, or
    /// the stream does not start.
    pub fn open(config: OutputConfig, callback: OutputCallback) -> Result<Self, OutputError> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or(OutputError::NoDevice)?;
        let channels = u16::try_from(callback.channels())
            .map_err(|_| OutputError::Unsupported("too many channels".into()))?;
        let ranges: Vec<_> = device
            .supported_output_configs()
            .map_err(|e| OutputError::Unsupported(e.to_string()))?
            .filter(|range| {
                range.channels() == channels
                    && range.min_sample_rate() <= config.sample_rate
                    && config.sample_rate <= range.max_sample_rate()
            })
            .collect();
        let range =
            pick(&ranges, cpal::SupportedStreamConfigRange::sample_format).ok_or_else(|| {
                OutputError::Unsupported(format!(
                    "f32, i32 or i16, {channels} channels, {} Hz",
                    config.sample_rate
                ))
            })?;
        let format = range.sample_format();
        let buffer = buffer_size(config.buffer_frames, range.buffer_size());
        let stream_config = cpal::StreamConfig {
            channels,
            sample_rate: config.sample_rate,
            buffer_size: buffer,
        };
        let mut callback = callback;
        let refusal = Arc::new(Mutex::new(None::<String>));
        let refused = Arc::clone(&refusal);
        // Called by cpal outside the data callback (a refusal of the priority
        // comes before the first one), so it may lock.
        let on_error = move |e: cpal::Error| {
            if e.kind() == cpal::ErrorKind::RealtimeDenied {
                *refused.lock().unwrap_or_else(PoisonError::into_inner) = Some(e.to_string());
            } else {
                eprintln!("takt-audio: {e}");
            }
        };
        let stream = match format {
            cpal::SampleFormat::I32 => device.build_output_stream(
                stream_config,
                move |out: &mut [i32], _: &cpal::OutputCallbackInfo| callback.fill_as(out),
                on_error,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                stream_config,
                move |out: &mut [i16], _: &cpal::OutputCallbackInfo| callback.fill_as(out),
                on_error,
                None,
            ),
            _ => device.build_output_stream(
                stream_config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| callback.fill(out),
                on_error,
                None,
            ),
        }
        .map_err(|e| OutputError::Stream(e.to_string()))?;
        stream
            .play()
            .map_err(|e| OutputError::Stream(e.to_string()))?;
        Ok(Self {
            _stream: stream,
            buffer,
            format,
            realtime: config.realtime,
            refusal,
        })
    }

    /// What became of the request for real-time priority.
    #[must_use]
    pub fn priority(&self) -> Priority {
        priority_of(
            self.realtime,
            &self.refusal.lock().unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// The buffer size the stream was opened with (`Default` when the device
    /// refused the fixed size).
    #[must_use]
    pub fn buffer(&self) -> cpal::BufferSize {
        self.buffer
    }

    /// The sample format of the stream, one of [`FORMATS`].
    #[must_use]
    pub fn format(&self) -> cpal::SampleFormat {
        self.format
    }
}

/// The first of `candidates` whose format comes first in [`FORMATS`].
fn pick<T>(candidates: &[T], format_of: impl Fn(&T) -> cpal::SampleFormat) -> Option<&T> {
    FORMATS
        .iter()
        .find_map(|&wanted| candidates.iter().find(|c| format_of(c) == wanted))
}

fn priority_of(realtime: bool, refusal: &Option<String>) -> Priority {
    match (realtime, refusal) {
        (false, _) => Priority::NotRequested,
        (true, None) => Priority::NoRefusal,
        (true, Some(why)) => Priority::Refused(why.clone()),
    }
}

/// The fixed size when the device names a range that holds it, else the
/// device default.
fn buffer_size(wanted: Option<u32>, range: &cpal::SupportedBufferSize) -> cpal::BufferSize {
    match (wanted, range) {
        (Some(frames), cpal::SupportedBufferSize::Range { min, max })
            if (*min..=*max).contains(&frames) =>
        {
            cpal::BufferSize::Fixed(frames)
        }
        _ => cpal::BufferSize::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_priority_says_only_what_is_known() {
        assert_eq!(
            priority_of(false, &Some("x".into())),
            Priority::NotRequested
        );
        assert_eq!(priority_of(true, &None), Priority::NoRefusal);
        assert_eq!(
            priority_of(true, &Some("no rtprio".into())).to_string(),
            "refused: no rtprio"
        );
    }

    #[test]
    fn the_best_format_the_device_takes_wins() {
        use cpal::SampleFormat::{F32, I16, I32, U8};
        assert_eq!(pick(&[I16, F32, I32], |f| *f), Some(&F32));
        assert_eq!(pick(&[I16, I32], |f| *f), Some(&I32));
        assert_eq!(pick(&[U8, I16], |f| *f), Some(&I16));
        assert_eq!(pick(&[U8], |f| *f), None);
    }

    #[test]
    fn a_fixed_size_only_inside_the_range_of_the_device() {
        let range = cpal::SupportedBufferSize::Range { min: 64, max: 4096 };
        assert_eq!(buffer_size(Some(480), &range), cpal::BufferSize::Fixed(480));
        assert_eq!(buffer_size(Some(8192), &range), cpal::BufferSize::Default);
        assert_eq!(buffer_size(None, &range), cpal::BufferSize::Default);
        assert_eq!(
            buffer_size(Some(480), &cpal::SupportedBufferSize::Unknown),
            cpal::BufferSize::Default
        );
    }
}
