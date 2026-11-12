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
//! The stream is `f32` at the rate and channel count of the ring. A device
//! that does not take that is refused with [`OutputError::Unsupported`];
//! conversion belongs to a resampling stage, not to the callback.

use std::fmt;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::callback::OutputCallback;

/// How the output stream is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputConfig {
    /// Frames per second.
    pub sample_rate: u32,
    /// Frames per callback asked of the device. `None`: the device default.
    /// A size outside the range the device names, or a device that names
    /// none, gets the device default.
    pub buffer_frames: Option<u32>,
}

impl Default for OutputConfig {
    /// 48 kHz with 480 frames per callback: 10 ms, half the extrapolation
    /// limit (20 ms) the player gives the audio master clock.
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            buffer_frames: Some(480),
        }
    }
}

/// Why no output could be opened.
#[derive(Debug)]
pub enum OutputError {
    /// The host has no default output device.
    NoDevice,
    /// The device takes no `f32` stream with this rate and channel count.
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

/// A running output. Dropping it stops the stream.
pub struct Output {
    // Not Send on every platform: an output stays on the thread that opened it.
    _stream: cpal::Stream,
    buffer: cpal::BufferSize,
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Output")
            .field("buffer", &self.buffer)
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
        let range = device
            .supported_output_configs()
            .map_err(|e| OutputError::Unsupported(e.to_string()))?
            .find(|range| {
                range.channels() == channels
                    && range.sample_format() == cpal::SampleFormat::F32
                    && range.min_sample_rate() <= config.sample_rate
                    && config.sample_rate <= range.max_sample_rate()
            })
            .ok_or_else(|| {
                OutputError::Unsupported(format!(
                    "f32, {channels} channels, {} Hz",
                    config.sample_rate
                ))
            })?;
        let buffer = buffer_size(config.buffer_frames, range.buffer_size());
        let stream_config = cpal::StreamConfig {
            channels,
            sample_rate: config.sample_rate,
            buffer_size: buffer,
        };
        let mut callback = callback;
        let stream = device
            .build_output_stream(
                stream_config,
                move |out: &mut [f32], _: &cpal::OutputCallbackInfo| callback.fill(out),
                // Called by cpal outside the real-time path.
                |e| eprintln!("takt-audio: {e}"),
                None,
            )
            .map_err(|e| OutputError::Stream(e.to_string()))?;
        stream
            .play()
            .map_err(|e| OutputError::Stream(e.to_string()))?;
        Ok(Self {
            _stream: stream,
            buffer,
        })
    }

    /// The buffer size the stream was opened with (`Default` when the device
    /// refused the fixed size).
    #[must_use]
    pub fn buffer(&self) -> cpal::BufferSize {
        self.buffer
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
