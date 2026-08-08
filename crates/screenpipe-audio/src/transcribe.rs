//! Local transcription. whisper.cpp, in this process, on the CPU.
//!
//! Nothing here reaches the network. That is the point of the whole channel:
//! the alternative to a local model is posting the operator's meetings, and
//! everyone else's voice in them, to somebody's API.

use std::fmt;
use std::path::{Path, PathBuf};

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::capture::SAMPLE_RATE_HZ;

/// Shortest audio worth transcribing. 250 ms.
///
/// Below this there is no word to find, and whisper does not return nothing
/// for near-nothing - it returns its most likely guess, which for a fragment
/// is a caption artefact ("[BLANK_AUDIO]", "Thank you.", a subtitle credit).
/// Refusing here is cheaper and more honest than filtering the output.
const MIN_TRANSCRIBABLE_SAMPLES: usize = SAMPLE_RATE_HZ as usize / 4;

/// Above this, whisper's own answer is that it heard no speech.
///
/// The model reports a per-segment no-speech probability, and a segment over
/// this is one it is telling us not to believe. Dropped rather than written,
/// because a durable row asserting words nobody said is worse than a silent
/// gap - it is wrong in the search index and wrong in the summaries built on
/// top of it.
const NO_SPEECH_CEILING: f32 = 0.6;

/// A model file on disk.
///
/// A newtype rather than a bare path because the failure it prevents is
/// specific: pointing this at a file that is not a ggml model produces a
/// whisper.cpp abort deep in C, not a Rust error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelPath(PathBuf);

impl ModelPath {
    /// Accepts a path that exists and is a file. Does not validate the format -
    /// only [`WhisperEngine::load`] can do that, by loading it.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, WhisperError> {
        let path = path.as_ref();
        if !path.is_file() {
            return Err(WhisperError::ModelMissing);
        }
        Ok(Self(path.to_path_buf()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// The model's file stem, for `merge_meta.model` - `ggml-base.en`.
    ///
    /// The stem and never the full path: the path can contain a Windows user
    /// name, and this string lands in a durable database row.
    pub fn label(&self) -> String {
        self.0
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_owned())
    }
}

/// What a transcription attempt produced.
#[derive(Clone, Debug, PartialEq)]
pub struct Transcript {
    /// Joined segment text, whitespace-collapsed. Empty when the model found
    /// nothing it believed.
    pub text: String,
    /// The raw segment text before collapsing, joined with single newlines.
    /// Persisted as `ocr_text` - the same "raw beside readable" split the
    /// screen channel writes.
    pub raw: String,
    /// Mean no-speech probability across the kept segments. `None` when none
    /// were kept.
    pub avg_no_speech_prob: Option<f32>,
    /// Segments the model returned, before the no-speech filter.
    pub segments: usize,
    /// Segments dropped for exceeding [`NO_SPEECH_CEILING`].
    pub dropped_segments: usize,
}

impl Transcript {
    /// True when nothing survived. The caller writes no event for these.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    fn nothing(segments: usize, dropped_segments: usize) -> Self {
        Self {
            text: String::new(),
            raw: String::new(),
            avg_no_speech_prob: None,
            segments,
            dropped_segments,
        }
    }
}

/// Failures worth telling the operator apart.
///
/// Deliberately carries no path, no model name, and no audio: this type is
/// printed to a log the service wrapper persists to disk.
#[derive(Debug, PartialEq, Eq)]
pub enum WhisperError {
    /// The model file is not where it was said to be.
    ModelMissing,
    /// whisper.cpp would not load it - wrong format, truncated download, or a
    /// model built for a different version.
    ModelUnreadable,
    /// The model loaded but inference failed.
    InferenceFailed,
}

impl fmt::Display for WhisperError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::ModelMissing => {
                "the whisper model file was not found; download a ggml model and pass --model"
            }
            Self::ModelUnreadable => {
                "the whisper model file could not be loaded; it is not a ggml model this build reads"
            }
            Self::InferenceFailed => "whisper could not transcribe the audio",
        };
        formatter.write_str(text)
    }
}

impl std::error::Error for WhisperError {}

/// A loaded model, reused for the life of the process.
///
/// Loading is the expensive part - hundreds of milliseconds and the model's
/// whole footprint in RAM - so it happens once at startup and never on the
/// path an utterance takes. That also means a bad model path fails before any
/// audio has been captured, rather than the first time somebody speaks.
pub struct WhisperEngine {
    context: WhisperContext,
    model: ModelPath,
    threads: i32,
    language: Option<String>,
}

impl WhisperEngine {
    /// Loads the model. Blocking and slow; call it once, off the capture path.
    pub fn load(
        model: ModelPath,
        threads: i32,
        language: Option<String>,
    ) -> Result<Self, WhisperError> {
        // whisper.cpp and GGML write to stdout and stderr directly, from C. On
        // load that is thirty lines of model banner; during inference it is
        // whatever the print switches below did not catch. This process's
        // stdout is a log file the service wrapper keeps on disk, so both are
        // routed into whisper-rs's hooks - and with neither the `log_backend`
        // nor the `tracing_backend` feature enabled, those hooks lead nowhere.
        // Idempotent, and the only call site.
        whisper_rs::install_logging_hooks();

        let context =
            WhisperContext::new_with_params(model.as_path(), WhisperContextParameters::default())
                .map_err(|_| WhisperError::ModelUnreadable)?;
        Ok(Self {
            context,
            model,
            threads: threads.max(1),
            language,
        })
    }

    /// The model's label for `merge_meta.model`.
    pub fn model_label(&self) -> String {
        self.model.label()
    }

    /// The language the model was told to assume, if any.
    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }

    /// Transcribes one closed utterance.
    ///
    /// `samples` must be mono 16 kHz in [-1, 1] - what [`crate::VadSegmenter`]
    /// produces. Blocking: this is the expensive call, and it is why the audio
    /// channel runs in its own process rather than beside screen capture.
    pub fn transcribe(&mut self, samples: &[f32]) -> Result<Transcript, WhisperError> {
        if samples.len() < MIN_TRANSCRIBABLE_SAMPLES {
            return Ok(Transcript::nothing(0, 0));
        }

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.threads);
        params.set_translate(false);
        if let Some(language) = self.language.as_deref() {
            params.set_language(Some(language));
        }
        // Every one of these prints to stdout from inside C. This process's
        // stdout is a log file the service wrapper keeps, and the thing being
        // printed is the transcript - which is to say, someone's speech. None
        // of it may go there.
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        // Each utterance is independent. Carrying decoder context across them
        // makes whisper repeat the previous turn when the current one is short
        // or noisy, which is exactly the hallucination this channel cannot
        // afford to write down as if someone had said it.
        params.set_no_context(true);
        params.set_suppress_blank(true);

        let mut state = self
            .context
            .create_state()
            .map_err(|_| WhisperError::InferenceFailed)?;
        state
            .full(params, samples)
            .map_err(|_| WhisperError::InferenceFailed)?;

        let count = state.full_n_segments();
        let segments = usize::try_from(count).unwrap_or(0);
        let candidates = (0..count).filter_map(|index| {
            let segment = state.get_segment(index)?;
            Some(SegmentCandidate {
                probability: segment.no_speech_probability(),
                text: segment
                    .to_str_lossy()
                    .map(|text| text.into_owned())
                    .map_err(|_| ()),
            })
        });
        Ok(transcript_from_segments(segments, candidates))
    }
}

struct SegmentCandidate {
    probability: f32,
    text: Result<String, ()>,
}

/// Applies the production no-speech acceptance boundary to Whisper segments.
///
/// The engine above supplies native segment data; this private seam lets the
/// boundary be tested with malformed FFI values without loading a model.
fn transcript_from_segments(
    segments: usize,
    candidates: impl IntoIterator<Item = SegmentCandidate>,
) -> Transcript {
    let mut kept: Vec<String> = Vec::new();
    let mut probabilities: Vec<f32> = Vec::new();
    let mut dropped = 0usize;
    for SegmentCandidate { probability, text } in candidates {
        let Ok(text) = text else {
            // Non-UTF-8 from the tokenizer. Counted as dropped rather than
            // guessed at.
            dropped += 1;
            continue;
        };
        let text = text.trim().to_owned();
        if text.is_empty() {
            continue;
        }
        if !is_credible_no_speech_probability(probability) {
            dropped += 1;
            continue;
        }
        kept.push(text);
        probabilities.push(probability);
    }

    if kept.is_empty() {
        return Transcript::nothing(segments, dropped);
    }

    let raw = kept.join("\n");
    let text = kept
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let average = probabilities.iter().sum::<f32>() / probabilities.len() as f32;
    Transcript {
        text,
        raw,
        avg_no_speech_prob: Some(average),
        segments,
        dropped_segments: dropped,
    }
}

/// Whether a native no-speech probability is both valid and credible.
///
/// The value crosses an FFI boundary. Rejecting malformed probabilities here
/// keeps NaN and out-of-range values from bypassing the ordinary ceiling and
/// reaching durable confidence metadata.
fn is_credible_no_speech_probability(probability: f32) -> bool {
    probability.is_finite()
        && (0.0..=1.0).contains(&probability)
        && probability <= NO_SPEECH_CEILING
}

#[cfg(test)]
mod tests {
    use super::{
        MIN_TRANSCRIBABLE_SAMPLES, ModelPath, NO_SPEECH_CEILING, SegmentCandidate, Transcript,
        WhisperError, transcript_from_segments,
    };
    use crate::capture::SAMPLE_RATE_HZ;

    #[test]
    fn a_missing_model_is_refused_before_anything_is_captured() {
        let error = ModelPath::new("C:/nowhere/ggml-base.en.bin").unwrap_err();
        assert_eq!(error, WhisperError::ModelMissing);
    }

    #[test]
    fn the_model_label_is_the_stem_and_never_the_path() {
        // The path can contain a Windows user name and this string is written
        // into a durable row.
        let fixture_dir = tempfile::tempdir().unwrap();
        let file = fixture_dir.path().join("ggml-base.en.bin");
        let canonical_temp_model = std::env::temp_dir().join("ggml-base.en.bin");
        assert_ne!(
            file, canonical_temp_model,
            "the fixture must not reuse the canonical temp model path"
        );
        std::fs::write(&file, b"not a real model").unwrap();
        let model = ModelPath::new(&file).unwrap();

        let label = model.label();

        assert_eq!(label, "ggml-base.en");
        assert!(!label.contains(std::path::MAIN_SEPARATOR));
        assert!(file.exists());
        assert!(!canonical_temp_model.starts_with(fixture_dir.path()));
    }

    #[test]
    fn the_minimum_is_a_quarter_second_of_audio() {
        // Whisper does not return nothing for near-nothing; it returns its best
        // guess, which for a fragment is a caption artefact.
        assert_eq!(MIN_TRANSCRIBABLE_SAMPLES, SAMPLE_RATE_HZ as usize / 4);
    }

    #[test]
    fn an_empty_transcript_reports_itself_empty() {
        let nothing = Transcript::nothing(3, 3);
        assert!(nothing.is_empty());
        assert_eq!(nothing.avg_no_speech_prob, None);
        assert_eq!((nothing.segments, nothing.dropped_segments), (3, 3));
    }

    #[test]
    fn segment_acceptance_enforces_the_complete_probability_boundary() {
        let next_above_ceiling = f32::from_bits(NO_SPEECH_CEILING.to_bits() + 1);
        let cases = [
            ("zero", 0.0, true),
            ("ceiling", NO_SPEECH_CEILING, true),
            ("interior", 0.2, true),
            ("above_ceiling", next_above_ceiling, false),
            ("negative", -f32::EPSILON, false),
            ("above_one", 1.0 + f32::EPSILON, false),
            ("positive_infinity", f32::INFINITY, false),
            ("negative_infinity", f32::NEG_INFINITY, false),
            ("nan", f32::NAN, false),
        ];

        for (category, probability, should_accept) in cases {
            let transcript = transcript_from_segments(
                1,
                [SegmentCandidate {
                    probability,
                    text: Ok("fixed".to_owned()),
                }],
            );

            assert_eq!(transcript.is_empty(), !should_accept, "category={category}");
            assert_eq!(
                transcript.dropped_segments == 0,
                should_accept,
                "category={category}"
            );
        }
    }

    #[test]
    fn error_messages_name_no_path_and_no_content() {
        // These reach a log file the service wrapper keeps on disk.
        for error in [
            WhisperError::ModelMissing,
            WhisperError::ModelUnreadable,
            WhisperError::InferenceFailed,
        ] {
            let message = error.to_string();
            assert!(
                !message.contains(':'),
                "{message} looks like it carries a path"
            );
            assert!(!message.is_empty());
        }
    }
}
