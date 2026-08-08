//! Voice activity detection: where one utterance ends and the next begins.
//!
//! This is the boundary layer, not the transcriber. Whisper is given closed
//! utterances and never asked where speech starts, because asking a 74-million
//! parameter model to find silence costs several orders of magnitude more than
//! a signal test that was designed for exactly that job and runs on a phone.

use std::collections::VecDeque;

use chrono::{DateTime, Duration, Utc};
use webrtc_vad::{SampleRate, Vad, VadMode};

use crate::capture::SAMPLE_RATE_HZ;

/// Length of one frame handed to the detector.
///
/// Not a free parameter: the WebRTC module accepts 10, 20, or 30 ms and
/// nothing else. 20 ms is the middle one - fine enough that the 60 ms open
/// threshold below is three independent votes rather than two, coarse enough
/// that the per-frame cost stays negligible.
pub const FRAME_MS: usize = 20;

/// Samples in one frame at the capture rate. 320 at 16 kHz.
pub const FRAME_SAMPLES: usize = SAMPLE_RATE_HZ as usize * FRAME_MS / 1000;

/// Consecutive voiced frames before an utterance opens. 60 ms.
///
/// One voiced frame is not speech. A key press, a mouse click, a chair, a door:
/// all of them read as voice for a frame or two, and every one that gets
/// through costs a whisper inference and, worse, an event row containing
/// whatever the model hallucinated onto the noise.
const OPEN_AFTER_VOICED_FRAMES: usize = 3;

/// Silence before an open utterance closes. 600 ms.
///
/// Long enough to survive the pauses inside ordinary speech - between clauses,
/// between words before a proper noun - and short enough that the turn ends
/// while the person is still obviously finished. Below ~400 ms utterances
/// fragment mid-sentence; well above a second they run together and the
/// transcript loses the shape of the exchange.
const CLOSE_AFTER_SILENT_FRAMES: usize = 30;

/// Frames kept before the one that opened the utterance. 200 ms.
///
/// Speech is voiced only after its onset, so by the time three frames have
/// agreed, the first consonant is already 60 ms in the past - and it is
/// routinely the one that decides which word was said. This keeps a rolling
/// buffer so the utterance handed to whisper starts before the trigger did.
const PREROLL_FRAMES: usize = 10;

/// Longest single utterance. 30 seconds.
///
/// Whisper's own window: it processes 30 seconds at a time, so a longer
/// utterance is not transcribed more accurately, it is just chunked internally
/// with the seams hidden. Forcing the boundary here keeps it visible and keeps
/// one continuous speaker from holding audio in memory indefinitely.
pub const MAX_UTTERANCE_SECONDS: usize = 30;

const MAX_UTTERANCE_SAMPLES: usize = SAMPLE_RATE_HZ as usize * MAX_UTTERANCE_SECONDS;

/// How eagerly the detector calls a frame speech.
///
/// Exposed rather than hardcoded because the right setting depends on the room
/// this runs in, and there is no usage data to pick from yet - this channel has
/// never shipped. `Quality` is the WebRTC default and the least aggressive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VadAggressiveness {
    /// Least aggressive: more of the signal is called speech. Fewer clipped
    /// words, more inferences spent on noise.
    Quality,
    /// The middle setting.
    LowBitrate,
    /// More aggressive.
    Aggressive,
    /// Most aggressive: only confident speech gets through. Cheapest, and the
    /// one that loses quiet talkers.
    VeryAggressive,
}

impl VadAggressiveness {
    fn mode(self) -> VadMode {
        match self {
            Self::Quality => VadMode::Quality,
            Self::LowBitrate => VadMode::LowBitrate,
            Self::Aggressive => VadMode::Aggressive,
            Self::VeryAggressive => VadMode::VeryAggressive,
        }
    }

    /// Stable code, for `merge_meta` and for the CLI.
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::Quality => "quality",
            Self::LowBitrate => "low_bitrate",
            Self::Aggressive => "aggressive",
            Self::VeryAggressive => "very_aggressive",
        }
    }

    /// Parses the CLI spelling. Returns `None` for anything else.
    pub fn from_code(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "quality" => Some(Self::Quality),
            "low_bitrate" | "low-bitrate" => Some(Self::LowBitrate),
            "aggressive" => Some(Self::Aggressive),
            "very_aggressive" | "very-aggressive" => Some(Self::VeryAggressive),
            _ => None,
        }
    }
}

/// A closed run of speech, ready to transcribe.
#[derive(Clone, Debug, PartialEq)]
pub struct Utterance {
    /// When the first retained sample was captured, preroll included.
    pub started_at: DateTime<Utc>,
    /// When the last retained sample was captured.
    pub ended_at: DateTime<Utc>,
    /// Mono 16 kHz, normalized to [-1, 1] - the only shape whisper accepts.
    pub samples: Vec<f32>,
    /// Why the utterance closed. Persisted in `merge_meta`, because a run that
    /// hit the ceiling is a different object from one that ended in silence
    /// and a reader should not have to guess which they are looking at.
    pub closed_by: UtteranceEnd,
}

impl Utterance {
    /// Wall-clock length of the retained audio.
    pub fn duration(&self) -> Duration {
        self.ended_at - self.started_at
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UtteranceEnd {
    /// The speaker stopped for longer than the hangover.
    Silence,
    /// [`MAX_UTTERANCE_SECONDS`] was reached while speech was still going.
    MaxLength,
    /// Capture stopped - the stream ended or the process is shutting down -
    /// while an utterance was open.
    StreamClosed,
}

impl UtteranceEnd {
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::Silence => "silence",
            Self::MaxLength => "max_length",
            Self::StreamClosed => "stream_closed",
        }
    }
}

/// The per-frame speech decision, behind a trait.
///
/// Only so the state machine below can be tested. Its behaviour - when an
/// utterance opens, how much preroll survives, what closes it - is the part
/// worth pinning, and pinning it against a real detector would mean shipping
/// audio fixtures of a real person speaking into the repository, which the
/// no-raw-content rule forbids as squarely as it forbids screen text.
trait VoiceDetector {
    fn is_voice(&mut self, frame: &[i16]) -> bool;
}

struct WebRtcDetector {
    vad: Vad,
}

impl VoiceDetector for WebRtcDetector {
    fn is_voice(&mut self, frame: &[i16]) -> bool {
        // The error case is a malformed frame length, which cannot happen: the
        // segmenter only ever passes FRAME_SAMPLES. Treated as silence rather
        // than unwrapped, because a panic in the capture thread would take the
        // channel down over a frame.
        self.vad.is_voice_segment(frame).unwrap_or(false)
    }
}

/// Turns a stream of fixed-size frames into closed utterances.
///
/// A thin wrapper around a generic state machine, the same shape the clipboard
/// channel uses: the generic parameter exists only so the tests can drive the
/// boundary logic with a scripted detector, and keeping it off the public type
/// stops that testing seam from becoming part of this crate's API.
pub struct VadSegmenter {
    inner: Segmenter<WebRtcDetector>,
}

impl VadSegmenter {
    pub fn new(aggressiveness: VadAggressiveness) -> Self {
        Self {
            inner: Segmenter::with_detector(WebRtcDetector {
                vad: Vad::new_with_rate_and_mode(SampleRate::Rate16kHz, aggressiveness.mode()),
            }),
        }
    }

    /// True while an utterance is being accumulated.
    pub fn is_open(&self) -> bool {
        self.inner.is_open()
    }

    /// Feeds one frame. Returns an utterance on the frame that closes it.
    ///
    /// # Panics
    ///
    /// If `frame` is not [`FRAME_SAMPLES`] long. That is a programming error in
    /// the capture layer, not a runtime condition: the detector rejects any
    /// other length, so a wrong size here would silently mean "no speech,
    /// forever".
    pub fn push_frame(&mut self, frame: &[i16], captured_at: DateTime<Utc>) -> Option<Utterance> {
        self.inner.push_frame(frame, captured_at)
    }

    /// Closes whatever is open. Called when capture stops, so a turn that was
    /// still going at shutdown is transcribed rather than dropped.
    pub fn flush(&mut self) -> Option<Utterance> {
        self.inner.flush()
    }
}

struct Segmenter<D> {
    detector: D,
    /// Frames seen while closed, newest last, capped at [`PREROLL_FRAMES`].
    preroll: VecDeque<(DateTime<Utc>, Vec<i16>)>,
    /// Set once an utterance is open.
    open: Option<OpenUtterance>,
    consecutive_voiced: usize,
}

struct OpenUtterance {
    started_at: DateTime<Utc>,
    /// End of the last frame that carried voice. Trailing silence is dropped
    /// from the transcribed audio, so a hangover of room tone is not handed to
    /// the model as if it were part of the turn.
    last_voiced_at: DateTime<Utc>,
    samples: Vec<i16>,
    /// Samples up to and including the last voiced frame.
    voiced_len: usize,
    consecutive_silent: usize,
}

impl<D: VoiceDetector> Segmenter<D> {
    fn with_detector(detector: D) -> Self {
        Self {
            detector,
            preroll: VecDeque::with_capacity(PREROLL_FRAMES),
            open: None,
            consecutive_voiced: 0,
        }
    }

    fn is_open(&self) -> bool {
        self.open.is_some()
    }

    fn push_frame(&mut self, frame: &[i16], captured_at: DateTime<Utc>) -> Option<Utterance> {
        assert_eq!(
            frame.len(),
            FRAME_SAMPLES,
            "the VAD accepts exactly one frame size"
        );
        let voiced = self.detector.is_voice(frame);
        let frame_span = Duration::milliseconds(FRAME_MS as i64);
        let frame_end = captured_at + frame_span;

        let Some(open) = self.open.as_mut() else {
            self.preroll.push_back((captured_at, frame.to_vec()));
            if self.preroll.len() > PREROLL_FRAMES {
                self.preroll.pop_front();
            }
            self.consecutive_voiced = if voiced {
                self.consecutive_voiced + 1
            } else {
                0
            };
            if self.consecutive_voiced >= OPEN_AFTER_VOICED_FRAMES {
                self.consecutive_voiced = 0;
                self.open_utterance(frame_end);
            }
            return None;
        };

        open.samples.extend_from_slice(frame);
        if voiced {
            open.consecutive_silent = 0;
            open.last_voiced_at = frame_end;
            open.voiced_len = open.samples.len();
        } else {
            open.consecutive_silent += 1;
        }

        if open.consecutive_silent >= CLOSE_AFTER_SILENT_FRAMES {
            return Some(self.close(UtteranceEnd::Silence));
        }
        if open.samples.len() >= MAX_UTTERANCE_SAMPLES {
            return Some(self.close(UtteranceEnd::MaxLength));
        }
        None
    }

    fn flush(&mut self) -> Option<Utterance> {
        self.open.as_ref()?;
        Some(self.close(UtteranceEnd::StreamClosed))
    }

    fn open_utterance(&mut self, frame_end: DateTime<Utc>) {
        let mut samples = Vec::with_capacity(MAX_UTTERANCE_SAMPLES);
        let started_at = self.preroll.front().map(|(at, _)| *at).unwrap_or(frame_end);
        for (_, frame) in self.preroll.drain(..) {
            samples.extend_from_slice(&frame);
        }
        let voiced_len = samples.len();
        self.open = Some(OpenUtterance {
            started_at,
            last_voiced_at: frame_end,
            samples,
            voiced_len,
            consecutive_silent: 0,
        });
    }

    fn close(&mut self, closed_by: UtteranceEnd) -> Utterance {
        let open = self.open.take().expect("close called with nothing open");
        self.consecutive_voiced = 0;
        self.preroll.clear();
        // Trailing silence is cut. It is up to 600 ms of room tone, and whisper
        // is at its most inventive when given near-silence to explain.
        let mut samples = open.samples;
        samples.truncate(open.voiced_len);
        Utterance {
            started_at: open.started_at,
            ended_at: open.last_voiced_at,
            samples: samples.into_iter().map(pcm16_to_f32).collect(),
            closed_by,
        }
    }
}

/// 16-bit PCM to the normalized float whisper wants.
///
/// Divides by 32768 rather than 32767: the former is exact in binary and maps
/// the full i16 range into [-1, 1) without the asymmetry the latter introduces
/// at the positive extreme.
fn pcm16_to_f32(sample: i16) -> f32 {
    f32::from(sample) / 32768.0
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::{
        CLOSE_AFTER_SILENT_FRAMES, FRAME_MS, FRAME_SAMPLES, MAX_UTTERANCE_SECONDS,
        OPEN_AFTER_VOICED_FRAMES, PREROLL_FRAMES, Segmenter, Utterance, UtteranceEnd,
        VadAggressiveness, VoiceDetector, pcm16_to_f32,
    };

    /// A detector that says exactly what the test tells it to.
    struct Scripted {
        answers: Vec<bool>,
        index: usize,
    }

    impl Scripted {
        fn new(answers: Vec<bool>) -> Self {
            Self { answers, index: 0 }
        }
    }

    impl VoiceDetector for Scripted {
        fn is_voice(&mut self, _frame: &[i16]) -> bool {
            let answer = *self
                .answers
                .get(self.index)
                .expect("the segmenter consumed more frames than the script supplies");
            self.index += 1;
            answer
        }
    }

    /// Drives a whole script through, returning every utterance produced.
    ///
    /// Each frame carries a distinct sample value so the assembled audio can be
    /// checked for WHICH frames survived, not merely how many.
    fn run(answers: Vec<bool>) -> (Vec<Utterance>, Segmenter<Scripted>) {
        let start = Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).single().unwrap();
        let total = answers.len();
        let mut segmenter = Segmenter::with_detector(Scripted::new(answers));
        let mut produced = Vec::new();
        for index in 0..total {
            let frame = vec![i16::try_from(index % 1000).unwrap(); FRAME_SAMPLES];
            let at = start + chrono::Duration::milliseconds((index * FRAME_MS) as i64);
            if let Some(utterance) = segmenter.push_frame(&frame, at) {
                produced.push(utterance);
            }
        }
        (produced, segmenter)
    }

    fn voiced(count: usize) -> Vec<bool> {
        vec![true; count]
    }

    fn silent(count: usize) -> Vec<bool> {
        vec![false; count]
    }

    #[test]
    fn a_single_voiced_frame_does_not_open_an_utterance() {
        // A key press, a click, a chair. Each costs a whisper inference and an
        // event row of whatever the model invents to explain the noise.
        let (produced, segmenter) =
            run([silent(2), voiced(1), silent(CLOSE_AFTER_SILENT_FRAMES + 5)].concat());

        assert!(produced.is_empty());
        assert!(!segmenter.is_open());
    }

    #[test]
    fn speech_opens_only_after_the_agreed_number_of_frames() {
        let (_, segmenter) = run([voiced(OPEN_AFTER_VOICED_FRAMES - 1)].concat());
        assert!(!segmenter.is_open(), "one frame short must not open");

        let (_, segmenter) = run([voiced(OPEN_AFTER_VOICED_FRAMES)].concat());
        assert!(segmenter.is_open());
    }

    #[test]
    fn the_frames_before_the_trigger_are_kept() {
        // By the time three frames agree, the first consonant is 60 ms in the
        // past - and it is routinely the one that decides the word.
        let lead_in = PREROLL_FRAMES;
        let voiced_after_open = 5;
        let (produced, _) = run([
            silent(lead_in),
            voiced(OPEN_AFTER_VOICED_FRAMES + voiced_after_open),
            silent(CLOSE_AFTER_SILENT_FRAMES),
        ]
        .concat());

        let utterance = produced.first().expect("an utterance should have closed");
        let frames_kept = utterance.samples.len() / FRAME_SAMPLES;
        // The trigger frames are INSIDE the preroll window, not additional to
        // it: the buffer always holds the last PREROLL_FRAMES seen, and the
        // frames that opened the utterance are the newest of them.
        assert_eq!(
            frames_kept,
            PREROLL_FRAMES + voiced_after_open,
            "the whole preroll window plus everything voiced after the open"
        );

        // The retained audio must START BEFORE the trigger - that is the entire
        // point - so its first sample is the frame PREROLL_FRAMES - 1 back from
        // the one that opened it. Each frame in the script carries its own index
        // as its sample value, so this pins WHICH frames survived, not just how
        // many.
        let opening_frame = lead_in + OPEN_AFTER_VOICED_FRAMES - 1;
        let oldest_kept = opening_frame + 1 - PREROLL_FRAMES;
        assert!(
            oldest_kept < lead_in,
            "the kept audio must predate the speech"
        );
        assert!(
            (utterance.samples[0] - pcm16_to_f32(i16::try_from(oldest_kept).unwrap())).abs()
                < f32::EPSILON,
            "the utterance must begin at the oldest frame still in the preroll buffer"
        );
    }

    #[test]
    fn a_pause_shorter_than_the_hangover_stays_inside_one_utterance() {
        // Ordinary speech pauses between clauses. Splitting on them would turn
        // one sentence into four rows.
        let (produced, _) = run([
            voiced(OPEN_AFTER_VOICED_FRAMES + 10),
            silent(CLOSE_AFTER_SILENT_FRAMES - 1),
            voiced(10),
            silent(CLOSE_AFTER_SILENT_FRAMES),
        ]
        .concat());

        assert_eq!(produced.len(), 1, "one turn, not two");
        assert_eq!(produced[0].closed_by, UtteranceEnd::Silence);
    }

    #[test]
    fn trailing_silence_is_cut_from_the_transcribed_audio() {
        // Whisper is at its most inventive when handed near-silence to explain,
        // so the hangover is used to DECIDE the boundary and then discarded.
        let voiced_frames = OPEN_AFTER_VOICED_FRAMES + 10;
        let (produced, _) =
            run([voiced(voiced_frames), silent(CLOSE_AFTER_SILENT_FRAMES)].concat());

        let utterance = &produced[0];
        assert_eq!(
            utterance.samples.len() / FRAME_SAMPLES,
            voiced_frames,
            "no preroll was available and no hangover should survive"
        );
        assert_eq!(
            utterance.duration(),
            chrono::Duration::milliseconds((voiced_frames * FRAME_MS) as i64)
        );
    }

    #[test]
    fn continuous_speech_is_cut_at_the_ceiling_and_says_so() {
        let frames_in_ceiling = MAX_UTTERANCE_SECONDS * 1000 / FRAME_MS;
        let (produced, _) = run(voiced(frames_in_ceiling + 5));

        let utterance = produced.first().expect("the ceiling should have fired");
        assert_eq!(utterance.closed_by, UtteranceEnd::MaxLength);
        assert!(
            utterance.samples.len() <= frames_in_ceiling * FRAME_SAMPLES,
            "the ceiling bounds what is held in memory"
        );
    }

    #[test]
    fn a_turn_still_going_at_shutdown_is_flushed_rather_than_dropped() {
        let (produced, mut segmenter) = run(voiced(OPEN_AFTER_VOICED_FRAMES + 4));
        assert!(produced.is_empty());

        let flushed = segmenter.flush().expect("the open turn should be returned");

        assert_eq!(flushed.closed_by, UtteranceEnd::StreamClosed);
        assert!(!segmenter.is_open());
        assert!(segmenter.flush().is_none(), "flush must not repeat itself");
    }

    #[test]
    fn back_to_back_turns_are_separate_utterances() {
        let (produced, _) = run([
            voiced(OPEN_AFTER_VOICED_FRAMES + 5),
            silent(CLOSE_AFTER_SILENT_FRAMES),
            silent(3),
            voiced(OPEN_AFTER_VOICED_FRAMES + 5),
            silent(CLOSE_AFTER_SILENT_FRAMES),
        ]
        .concat());

        assert_eq!(produced.len(), 2);
        assert!(produced[0].ended_at <= produced[1].started_at);
    }

    #[test]
    fn the_frame_size_is_one_the_detector_accepts() {
        // The WebRTC module takes 10, 20 or 30 ms and nothing else, and it
        // reports a wrong length as an error the detector wrapper turns into
        // "silence" - which would look like a microphone that hears nothing.
        assert!(matches!(FRAME_MS, 10 | 20 | 30));
        assert_eq!(FRAME_SAMPLES, 320);
    }

    #[test]
    fn aggressiveness_round_trips_through_its_code() {
        for value in [
            VadAggressiveness::Quality,
            VadAggressiveness::LowBitrate,
            VadAggressiveness::Aggressive,
            VadAggressiveness::VeryAggressive,
        ] {
            assert_eq!(VadAggressiveness::from_code(value.as_code()), Some(value));
        }
        assert_eq!(VadAggressiveness::from_code("loud"), None);
    }

    #[test]
    fn pcm_conversion_is_symmetric_and_in_range() {
        assert_eq!(pcm16_to_f32(0), 0.0);
        assert_eq!(pcm16_to_f32(i16::MIN), -1.0);
        assert!(pcm16_to_f32(i16::MAX) < 1.0);
        assert_eq!(pcm16_to_f32(16_384), -pcm16_to_f32(-16_384));
    }
}
