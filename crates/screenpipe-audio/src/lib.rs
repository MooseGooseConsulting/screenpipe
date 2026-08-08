//! Audio capture, voice-activity segmentation, and local transcription.
//!
//! Windows-only and unapologetically so, exactly like `screenpipe-screen`:
//! raw WASAPI, no cross-platform abstraction over a thing that only has to work
//! on one machine.
//!
//! # Off by default
//!
//! Nothing in this crate runs unless the operator asks for it by name. It is
//! not started by `screenpipe run`, it is not installed by
//! `screenpipe service install`, and building the binary does not enable it.
//! That is a deliberate posture and not an accident of wiring: a microphone
//! that records the room is a materially different thing from a recorder that
//! reads the screen its owner is already looking at, and it can capture people
//! who never agreed to be captured. See the README Audio channel section.

mod capture;
mod device;
mod transcribe;
mod vad;

pub use capture::{AudioCapture, CaptureError, CapturedFrame, SAMPLE_RATE_HZ};
pub use device::{DeviceCategory, describe_default_endpoint};
pub use transcribe::{ModelPath, Transcript, WhisperEngine, WhisperError};
pub use vad::{
    FRAME_MS, FRAME_SAMPLES, MAX_UTTERANCE_SECONDS, Utterance, UtteranceEnd, VadAggressiveness,
    VadSegmenter,
};

/// Which audio the operator asked to record.
///
/// Two values and no "both": each channel is captured, segmented, and merged as
/// its own independent stream (see `EventKind::Audio`), so "both" is two of
/// these running side by side rather than a third mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// What came back through the speakers - the far side of a call, a video,
    /// anything the machine played. Does NOT include the operator's own voice.
    Loopback,
    /// The default capture device. Records the room, including anyone in it.
    Microphone,
}

impl Channel {
    /// Stable code, persisted in `merge_meta.channel`.
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::Loopback => "system_audio",
            Self::Microphone => "microphone",
        }
    }

    /// The `apps.app_key` this channel's events are attributed to.
    ///
    /// A real `apps` row, unlike the clipboard channel's deliberate absence:
    /// there genuinely is a distinct source here, and naming it is what gives
    /// an audio event a title a person can recognise in a list of results.
    /// It is the channel, never a device name - Windows exposes strings like
    /// "Microphone (Realtek High Definition Audio)" and none of them are
    /// written anywhere.
    pub const fn app_key(self) -> &'static str {
        match self {
            Self::Loopback => "audio:loopback",
            Self::Microphone => "audio:microphone",
        }
    }

    /// The `apps.app_title`, which becomes the event's `title`.
    pub const fn app_title(self) -> &'static str {
        match self {
            Self::Loopback => "System Audio",
            Self::Microphone => "Microphone",
        }
    }
}
