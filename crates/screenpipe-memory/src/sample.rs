use std::fmt;

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};

use crate::ObservationIdentity;

#[derive(Clone, PartialEq, Eq)]
pub struct ObservationSample {
    pub captured_at: DateTime<Utc>,
    pub app_key: String,
    pub app_title: String,
    pub window_title: String,
    pub ocr_text: String,
    pub readable_text: String,
    pub browser_url: Option<String>,
}

/// Additive context for channels whose observation is more than an instant.
///
/// [`ObservationSample`] keeps its original public field shape so downstream
/// struct literals remain source-compatible. New channels carry span and
/// source facts beside that stable sample through this envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationEnvelope {
    sample: ObservationSample,
    observed_until: DateTime<Utc>,
    audio: Option<AudioMeta>,
    identity: Option<ObservationIdentity>,
}

impl ObservationEnvelope {
    pub fn instant(sample: ObservationSample) -> Self {
        let observed_until = sample.captured_at;
        Self {
            sample,
            observed_until,
            audio: None,
            identity: None,
        }
    }

    /// Creates a policy-bound instant observation after validating its
    /// content-free identity. The source timestamp and capture timestamp must
    /// name the same observation.
    pub fn identified_instant(
        sample: ObservationSample,
        identity: ObservationIdentity,
    ) -> Result<Self> {
        identity.validate()?;
        ensure!(
            identity.observed_at == sample.captured_at,
            "observation identity timestamp must match the sample capture timestamp"
        );
        let observed_until = sample.captured_at;
        Ok(Self {
            sample,
            observed_until,
            audio: None,
            identity: Some(identity),
        })
    }

    pub fn spanning(
        sample: ObservationSample,
        observed_until: DateTime<Utc>,
        audio: AudioMeta,
    ) -> Self {
        Self {
            sample,
            observed_until,
            audio: Some(audio),
            identity: None,
        }
    }

    pub fn sample(&self) -> &ObservationSample {
        &self.sample
    }

    pub fn into_sample(self) -> ObservationSample {
        self.sample
    }

    pub fn observed_until(&self) -> DateTime<Utc> {
        self.observed_until
    }

    pub fn audio(&self) -> Option<&AudioMeta> {
        self.audio.as_ref()
    }

    pub fn identity(&self) -> Option<&ObservationIdentity> {
        self.identity.as_ref()
    }
}

impl From<ObservationSample> for ObservationEnvelope {
    fn from(sample: ObservationSample) -> Self {
        Self::instant(sample)
    }
}

/// What produced an audio observation, and how much to believe it.
///
/// This exists because none of it fits anywhere else: the transcript goes in
/// the text columns and the channel is recoverable from `app_key`, but "which
/// model said this, and did it think it was hearing speech at all" has no
/// column and is the difference between a transcript worth reading and one the
/// model invented over room tone.
///
/// Every field is a code or a model name. Nothing here is a device name, a
/// path, or any part of what was said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioMeta {
    /// `system_audio` or `microphone`.
    pub channel: &'static str,
    /// `console` or `communications` - the Windows endpoint role, not a
    /// device.
    pub device_category: &'static str,
    pub engine: &'static str,
    /// The model file's stem, e.g. `ggml-base.en`. Never its path.
    pub model: String,
    pub vad_engine: &'static str,
    pub vad_aggressiveness: &'static str,
    /// The language the model was told to assume, if it was told one.
    pub language: Option<String>,
    /// Mean no-speech probability across the kept segments, in parts per
    /// thousand.
    ///
    /// An integer rather than the `f32` whisper reports, so that
    /// `ObservationSample` stays `Eq` - which the merger, the runner, and their
    /// tests all rely on. Three digits is already more precision than a
    /// confidence heuristic deserves.
    pub avg_no_speech_permille: Option<u16>,
    /// Why the utterance ended: `silence`, `max_length`, or `stream_closed`.
    pub closed_by: &'static str,
}

impl fmt::Debug for ObservationSample {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObservationSample")
            .field("captured_at", &self.captured_at)
            .field("app_key", &self.app_key)
            .field("app_title", &self.app_title)
            .field("window_title", &self.window_title)
            .field("ocr_text", &"<redacted>")
            .field("readable_text", &"<redacted>")
            .field(
                "browser_url",
                &self.browser_url.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}
