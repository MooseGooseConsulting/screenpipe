//! WASAPI capture, in exactly one shape: mono 16 kHz signed 16-bit.
//!
//! That shape is not a preference. It is what the VAD module accepts and what
//! whisper.cpp resamples everything to internally, so asking the audio engine
//! for it means the conversion happens once, in Windows, instead of twice in
//! this process.

use std::collections::VecDeque;
use std::fmt;

use chrono::{DateTime, Duration, Utc};
use wasapi::{
    AudioCaptureClient, AudioClient, DeviceEnumerator, Direction, Handle, SampleType, StreamMode,
    WaveFormat, initialize_mta,
};

use crate::Channel;
use crate::device::{DeviceCategory, categorize_device};
use crate::vad::FRAME_SAMPLES;

/// The one sample rate this channel captures at.
///
/// whisper.cpp is trained at 16 kHz and resamples anything else; the WebRTC VAD
/// accepts 8/16/32/48 kHz. 16 kHz is the only rate that is native to both, and
/// asking WASAPI for it lets the audio engine do the resampling - which it is
/// doing anyway for the mix.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// Bytes in one VAD frame: 320 samples of 16-bit mono.
const FRAME_BYTES: usize = FRAME_SAMPLES * 2;

/// How long a read waits for the engine before it treats the interval as
/// silence. 2 seconds.
///
/// **A loopback stream delivers nothing at all while nothing is playing.** Not
/// zeroes - nothing: the event never signals and the wait times out. That is
/// the normal state of a laptop with no audio playing, so a timeout here is
/// silence and is reported as such, with a frame of zeroes, rather than as a
/// fault. Measured the hard way: the first live run of this channel declared
/// the stream stalled two seconds after it opened, on a machine where the audio
/// stack was working perfectly.
///
/// A genuinely broken endpoint - unplugged, disabled, taken exclusively by
/// another process - surfaces as an error from the capture client instead, and
/// that is still fatal. What this must never do is call a quiet room a fault:
/// hours of silence on loopback is ordinary, so any "no audio for too long"
/// ceiling would fire on normal use.
const READ_TIMEOUT_MS: u32 = 2_000;

/// Frames of manufactured silence one timed-out wait stands for.
const TIMEOUT_FRAMES: usize = READ_TIMEOUT_MS as usize / crate::vad::FRAME_MS;

/// Requested engine buffer, in 100 ns units. 100 ms.
///
/// Enough that an ordinary scheduling hiccup does not drop frames, short enough
/// that the timestamp correction below stays small.
const BUFFER_DURATION_HNS: i64 = 1_000_000;

/// What went wrong, in terms that name no device.
///
/// Windows exposes endpoint strings like "Microphone (Realtek High Definition
/// Audio)". They identify hardware in someone's house, they end up in a log the
/// service wrapper writes to disk, and they are not needed to act on any of
/// these.
#[derive(Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// No endpoint for this channel: no output device at all for loopback, no
    /// recording device for the microphone.
    NoDevice,
    /// The endpoint exists but would not open. Usually another process holds it
    /// in exclusive mode.
    DeviceUnavailable,
    /// The engine refused mono 16 kHz 16-bit even with conversion enabled.
    FormatUnsupported,
    /// The stream stopped delivering audio.
    StreamStalled,
}

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::NoDevice => "no audio endpoint is available for this channel",
            Self::DeviceUnavailable => {
                "the audio endpoint could not be opened; another process may hold it exclusively"
            }
            Self::FormatUnsupported => {
                "the audio engine would not provide mono 16 kHz 16-bit capture"
            }
            Self::StreamStalled => "the audio stream stopped delivering frames",
        };
        formatter.write_str(text)
    }
}

impl std::error::Error for CaptureError {}

/// One frame of audio, with the time it was captured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedFrame {
    pub captured_at: DateTime<Utc>,
    pub samples: Vec<i16>,
}

/// An open capture stream.
///
/// Not `Send`: WASAPI objects are bound to the thread that initialized COM, so
/// this is created and read on one dedicated thread and never moved.
pub struct AudioCapture {
    client: AudioClient,
    capture: AudioCaptureClient,
    event: Handle,
    /// Raw bytes read from the engine, not yet cut into frames.
    queued: VecDeque<u8>,
    category: DeviceCategory,
    channel: Channel,
}

impl AudioCapture {
    /// Opens the endpoint for `channel` and starts the stream.
    ///
    /// Loopback is the render device opened for capture - that is literally how
    /// WASAPI spells it, and it is why this channel can hear a call without
    /// touching the microphone.
    pub fn open(channel: Channel) -> Result<Self, CaptureError> {
        // Idempotent in practice: a second call on an already-initialized MTA
        // thread returns S_FALSE, which is not an error. A thread that has
        // already joined an STA fails here, and the device call below then
        // fails with DeviceUnavailable, which is the right thing to report to
        // an operator either way.
        let _ = initialize_mta();

        // The endpoint the operator hears is the RENDER default; the endpoint
        // they speak into is the CAPTURE default. The direction the client is
        // then initialized for is Capture in both cases - for loopback that
        // mismatch is exactly what asks WASAPI for the loopback stream.
        let device_direction = match channel {
            Channel::Loopback => Direction::Render,
            Channel::Microphone => Direction::Capture,
        };

        let enumerator = DeviceEnumerator::new().map_err(|_| CaptureError::NoDevice)?;
        let device = enumerator
            .get_default_device(&device_direction)
            .map_err(|_| CaptureError::NoDevice)?;
        let category = categorize_device(&enumerator, &device_direction, &device);

        let mut client = device
            .get_iaudioclient()
            .map_err(|_| CaptureError::DeviceUnavailable)?;

        // autoconvert is what makes one format work on every machine: the
        // engine mixes at whatever the device runs at and hands this stream the
        // mono 16 kHz it asked for. Without it, capture would have to carry a
        // resampler and a channel downmix for a channel that is off by default.
        let format = WaveFormat::new(16, 16, &SampleType::Int, SAMPLE_RATE_HZ as usize, 1, None);
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: BUFFER_DURATION_HNS,
        };
        client
            .initialize_client(&format, &Direction::Capture, &mode)
            .map_err(|_| CaptureError::FormatUnsupported)?;

        let event = client
            .set_get_eventhandle()
            .map_err(|_| CaptureError::DeviceUnavailable)?;
        let capture = client
            .get_audiocaptureclient()
            .map_err(|_| CaptureError::DeviceUnavailable)?;
        client
            .start_stream()
            .map_err(|_| CaptureError::DeviceUnavailable)?;

        Ok(Self {
            client,
            capture,
            event,
            queued: VecDeque::with_capacity(FRAME_BYTES * 64),
            category,
            channel,
        })
    }

    /// Which endpoint role this stream is following.
    pub fn category(&self) -> DeviceCategory {
        self.category
    }

    pub fn channel(&self) -> Channel {
        self.channel
    }

    /// Blocks until one whole VAD frame is available, and returns it.
    ///
    /// The timestamp is corrected for what is still queued behind the frame, so
    /// a burst delivered by the engine in one wake-up carries the times the
    /// samples were actually captured rather than the time they were collected.
    /// Without that, every frame in a 100 ms buffer would claim the same
    /// instant, and the utterance boundaries built on those times would be off
    /// by up to a buffer.
    pub fn next_frame(&mut self) -> Result<CapturedFrame, CaptureError> {
        while self.queued.len() < FRAME_BYTES {
            self.fill()?;
        }

        let frame: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|_| {
                let low = self.queued.pop_front().unwrap_or(0);
                let high = self.queued.pop_front().unwrap_or(0);
                i16::from_le_bytes([low, high])
            })
            .collect();

        let behind = samples_to_duration(self.queued.len() / 2);
        let frame_span = samples_to_duration(FRAME_SAMPLES);
        Ok(CapturedFrame {
            captured_at: Utc::now() - behind - frame_span,
            samples: frame,
        })
    }

    fn fill(&mut self) -> Result<(), CaptureError> {
        if self.event.wait_for_event(READ_TIMEOUT_MS).is_err() {
            // Silence, not a stall. See READ_TIMEOUT_MS: a loopback stream
            // signals nothing while nothing is playing, so the frames the
            // detector needs to keep counting silence - and to close an
            // utterance that ended when the sound stopped - have to be
            // manufactured here.
            //
            // A whole timeout's worth of them, not one. The detector counts
            // FRAMES, so handing it a single 20 ms frame per two seconds of
            // wall clock would stretch the 600 ms hangover into a minute, and
            // an utterance that ended when the sound stopped would stay open
            // long after.
            self.queued
                .extend(std::iter::repeat_n(0u8, FRAME_BYTES * TIMEOUT_FRAMES));
            return Ok(());
        }
        self.capture
            .read_from_device_to_deque(&mut self.queued)
            .map_err(|_| CaptureError::StreamStalled)?;
        Ok(())
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        // Best effort. A stream left running would keep the endpoint busy for
        // the life of the process, which matters most in `doctor`, where the
        // whole point is to open it, prove it works, and give it straight back.
        let _ = self.client.stop_stream();
    }
}

/// Wall-clock length of a sample count at the capture rate.
fn samples_to_duration(samples: usize) -> Duration {
    Duration::microseconds((samples as i64 * 1_000_000) / i64::from(SAMPLE_RATE_HZ))
}

#[cfg(test)]
mod tests {
    use super::{CaptureError, FRAME_BYTES, SAMPLE_RATE_HZ, samples_to_duration};
    use crate::vad::{FRAME_MS, FRAME_SAMPLES};

    #[test]
    fn one_frame_is_two_bytes_per_sample() {
        assert_eq!(FRAME_BYTES, FRAME_SAMPLES * 2);
    }

    #[test]
    fn a_frames_worth_of_samples_lasts_a_frame() {
        // The timestamp correction in next_frame is only as good as this: get
        // it wrong and every utterance boundary is offset by the same error.
        assert_eq!(
            samples_to_duration(FRAME_SAMPLES),
            chrono::Duration::milliseconds(FRAME_MS as i64)
        );
        assert_eq!(
            samples_to_duration(SAMPLE_RATE_HZ as usize),
            chrono::Duration::seconds(1)
        );
        assert_eq!(samples_to_duration(0), chrono::Duration::zero());
    }

    #[test]
    fn capture_errors_name_no_device() {
        // Windows endpoint strings identify hardware in someone's house, and
        // these messages reach a log file kept on disk.
        for error in [
            CaptureError::NoDevice,
            CaptureError::DeviceUnavailable,
            CaptureError::FormatUnsupported,
            CaptureError::StreamStalled,
        ] {
            let message = error.to_string();
            assert!(!message.is_empty());
            assert!(
                !message.contains('(') && !message.contains('"'),
                "{message} looks like it quotes a device name"
            );
        }
    }
}
