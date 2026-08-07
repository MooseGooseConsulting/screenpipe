use std::fmt;

use chrono::{DateTime, Utc};

#[derive(Clone, PartialEq, Eq)]
pub struct ObservationSample {
    pub captured_at: DateTime<Utc>,
    pub app_key: String,
    pub app_title: String,
    pub window_title: String,
    pub ocr_text: String,
    pub readable_text: String,
    pub browser_url: Option<String>,
    /// When the observation STOPPED, if it occupied a span rather than an
    /// instant. `None` means it was an instant and `captured_at` is both ends.
    ///
    /// Screen and clipboard samples are instants: a frame is read at a moment,
    /// a copy happens at a moment. An utterance is not - it runs for seconds,
    /// and the merger has to know that. Its idle-gap test measures
    /// `next.captured_at - open.ended_at`, so without this the "silence"
    /// between two utterances would include the length of the first one: a
    /// 30-second utterance followed by 35 seconds of quiet would read as a
    /// 65-second gap and split at a 60-second threshold that 35 seconds of
    /// silence never crossed.
    ///
    /// It also makes the durable window true. With it, `ended_at - started_at`
    /// is how long the speech actually ran.
    pub observed_until: Option<DateTime<Utc>>,
    /// Set only by the audio channel. `None` on every screen and clipboard
    /// sample, and the writer emits nothing for it then.
    pub audio: Option<AudioMeta>,
}

impl ObservationSample {
    /// The instant this observation stopped: its own end if it had one, and
    /// otherwise the instant it was captured.
    pub fn observed_until(&self) -> DateTime<Utc> {
        self.observed_until.unwrap_or(self.captured_at)
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
            // Not redacted: every field of it is a code or a model name, none
            // of which is content. Redacting it would hide the one thing worth
            // seeing in a debug line about an audio sample.
            .field("audio", &self.audio)
            .finish()
    }
}
