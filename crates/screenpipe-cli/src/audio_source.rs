use std::fmt;
use std::sync::mpsc::{
    Receiver as SyncReceiver, Sender as UnboundedSyncSender, SyncSender, TrySendError,
    channel as sync_unbounded_channel, sync_channel,
};
use std::thread;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Duration;
use screenpipe_audio::{
    AudioCapture, CaptureError, Channel, DeviceCategory, ModelPath, Utterance, VadAggressiveness,
    VadSegmenter, WhisperEngine,
};
use screenpipe_memory::{
    AudioMeta, CadenceInput, CadenceRecord, CaptureGap, ObservationEnvelope, ObservationRead,
    ObservationSample, SampleRead, SampleSource,
};
use tokio::sync::mpsc::{Receiver, Sender, channel};

/// Utterances that may wait for the transcriber before any are dropped.
///
/// Transcription runs faster than real time on `base.en`, so this queue is
/// empty in the ordinary case and exists for the burst: the machine is busy,
/// two people are talking over each other, an inference takes a second longer
/// than the speech it describes. Eight utterances is roughly two minutes of
/// dialogue - past that the transcriber is not behind, it is broken, and
/// holding more audio would only turn a visible fault into a growing heap.
const TRANSCRIBE_QUEUE: usize = 8;

/// Finished observations waiting for the durable writer.
const WRITE_QUEUE: usize = 32;

/// Emitted when an utterance is dropped because transcription is behind.
///
/// Fixed text, like the clipboard channel's diagnostics. This line is printed
/// on a path holding a buffer of somebody's speech, and pinning it here is what
/// stops any of it - the length, the timestamps, a hash - being interpolated
/// into the message later.
const AUDIO_BACKLOG: &str = "event=audio_dropped reason=transcriber_backlog";

/// Emitted when whisper returned nothing it believed.
///
/// Not a fault and not an event: the operator coughed, a door closed, a fan
/// spun up. Counted, so a channel that is producing nothing but these is
/// visible, but it writes no row.
const AUDIO_NO_SPEECH: &str = "event=audio_discarded reason=no_speech";

/// Reported when the capture side is gone for good.
///
/// The one failure this channel cannot back off and retry through: the WASAPI
/// stream and the model both live on threads that have exited, and no amount of
/// waiting brings them back. The run loop matches on this to exit, so the
/// service wrapper restarts the process from a clean state instead of printing
/// the same error every second forever.
const CAPTURE_STOPPED: &str = "the audio capture threads stopped";

/// Terminal source state used by the run loop to restart the whole process.
///
/// This is deliberately a type rather than a message token. `anyhow` preserves
/// it through added context, so restart policy never depends on formatted error
/// text remaining unchanged.
#[derive(Debug)]
pub(crate) struct CaptureStopped;

impl fmt::Display for CaptureStopped {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(CAPTURE_STOPPED)
    }
}

impl std::error::Error for CaptureStopped {}

/// The message the capture side sends the async run loop.
enum Observed {
    Sample(Box<ObservationEnvelope>),
    /// Capture itself failed. The run loop counts these toward its gap ceiling,
    /// so a dead endpoint stops the channel rather than spinning on it.
    Gap(CaptureGap),
}

/// Everything the two worker threads need, resolved before either starts.
pub(crate) struct AudioConfig {
    pub(crate) channel: Channel,
    pub(crate) model: ModelPath,
    pub(crate) aggressiveness: VadAggressiveness,
    pub(crate) threads: i32,
    pub(crate) language: Option<String>,
}

/// Turns speech on one channel into observation samples.
///
/// Three threads, and the split is the whole design:
///
/// 1. **Capture** owns the WASAPI stream and the VAD. It must never block,
///    because the audio engine's buffer is finite and a stalled reader loses
///    audio with no record that it happened. It only ever hands closed
///    utterances to (2), and drops them - loudly - rather than wait.
/// 2. **Transcribe** owns the whisper model. This is the expensive thread,
///    hundreds of milliseconds per utterance, and it is why the audio channel
///    is a separate process from screen capture rather than another task
///    beside it.
/// 3. The **async run loop**, which owns the database and does the merging.
///
/// WASAPI objects are bound to the thread that initialized COM, so (1) is a
/// real OS thread and not a `spawn_blocking` task that the runtime may move.
pub(crate) struct AudioSampleSource {
    incoming: Receiver<Observed>,
}

impl AudioSampleSource {
    /// Opens the stream and starts both workers.
    ///
    /// Blocks until the stream is actually open and the model is actually
    /// loaded. A bad model path, or an endpoint another process holds
    /// exclusively, fails here - while the operator is still watching - rather
    /// than on the first thing somebody says.
    ///
    /// The stream is opened INSIDE the capture thread, not here and moved. The
    /// WASAPI interfaces are COM objects bound to the thread that initialized
    /// COM and are not `Send`; the readiness channel is how the caller still
    /// learns whether the open succeeded.
    pub(crate) fn start(config: AudioConfig) -> Result<Self> {
        let (utterances_tx, utterances_rx) = sync_channel::<Utterance>(TRANSCRIBE_QUEUE);
        let (terminal_tx, terminal_rx) = sync_unbounded_channel::<Utterance>();
        let (observed_tx, observed_rx) = channel::<Observed>(WRITE_QUEUE);
        let (ready_tx, ready_rx) = sync_channel::<Result<DeviceCategory, CaptureError>>(1);

        let capture_reports = observed_tx.clone();
        let aggressiveness = config.aggressiveness;
        let audio_channel = config.channel;
        thread::Builder::new()
            .name("screenpipe-audio-capture".to_owned())
            .spawn(move || {
                let capture = match AudioCapture::open(audio_channel) {
                    Ok(capture) => {
                        let _ = ready_tx.send(Ok(capture.category()));
                        capture
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                capture_loop(
                    capture,
                    aggressiveness,
                    &utterances_tx,
                    &terminal_tx,
                    &capture_reports,
                );
            })?;

        let device_category = ready_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("the audio capture thread stopped before it opened"))??;

        let engine = WhisperEngine::load(
            config.model.clone(),
            config.threads,
            config.language.clone(),
        )?;

        println!(
            "event=audio_channel state=on channel={} device_category={} model={} vad={}",
            config.channel.as_code(),
            device_category.as_code(),
            engine.model_label(),
            config.aggressiveness.as_code()
        );

        let meta = AudioMetaTemplate {
            channel: config.channel.as_code(),
            device_category: device_category.as_code(),
            model: engine.model_label(),
            vad_aggressiveness: aggressiveness.as_code(),
            language: config.language.clone(),
        };
        let app_key = config.channel.app_key();
        let app_title = config.channel.app_title();
        thread::Builder::new()
            .name("screenpipe-audio-transcribe".to_owned())
            .spawn(move || {
                transcribe_loop(
                    engine,
                    meta,
                    app_key,
                    app_title,
                    &utterances_rx,
                    &terminal_rx,
                    &observed_tx,
                )
            })?;

        Ok(Self {
            incoming: observed_rx,
        })
    }
}

#[async_trait]
impl SampleSource for AudioSampleSource {
    async fn next_sample(&mut self) -> Result<SampleRead> {
        match self.incoming.recv().await {
            Some(Observed::Sample(sample)) => Ok(SampleRead::Sample {
                sample: sample.into_sample(),
                // Reported, not computed, exactly as the clipboard channel does
                // it. `CadencePolicy` answers a question about screen frames
                // and input idleness; neither describes an utterance. What is
                // true here is that the next sample arrives when somebody
                // speaks, which is not an interval, so this carries zero rather
                // than a number that would be read as one.
                cadence: CadenceRecord {
                    input: CadenceInput {
                        input_idle: Duration::zero(),
                        frame_stable_for: Duration::zero(),
                        foreground_changed: false,
                        frame_changed: false,
                    },
                    next_interval: Duration::zero(),
                },
            }),
            Some(Observed::Gap(gap)) => Ok(SampleRead::Gap(gap)),
            // Both workers are gone. Returning an error rather than parking
            // forever is what lets the run loop exit and the service wrapper
            // restart the process from a clean state.
            None => Err(CaptureStopped.into()),
        }
    }

    async fn next_observation(&mut self) -> Result<ObservationRead> {
        match self.incoming.recv().await {
            Some(Observed::Sample(observation)) => Ok(ObservationRead::Sample {
                observation: *observation,
                cadence: CadenceRecord {
                    input: CadenceInput {
                        input_idle: Duration::zero(),
                        frame_stable_for: Duration::zero(),
                        foreground_changed: false,
                        frame_changed: false,
                    },
                    next_interval: Duration::zero(),
                },
            }),
            Some(Observed::Gap(gap)) => Ok(ObservationRead::Gap(gap)),
            None => Err(CaptureStopped.into()),
        }
    }
}

/// The parts of [`AudioMeta`] that are the same for every utterance in a run.
struct AudioMetaTemplate {
    channel: &'static str,
    device_category: &'static str,
    model: String,
    vad_aggressiveness: &'static str,
    language: Option<String>,
}

/// Thread 1: WASAPI to closed utterances. Never blocks on anything downstream.
fn capture_loop(
    mut capture: AudioCapture,
    aggressiveness: VadAggressiveness,
    utterances: &SyncSender<Utterance>,
    terminal: &UnboundedSyncSender<Utterance>,
    reports: &Sender<Observed>,
) {
    let mut segmenter = VadSegmenter::new(aggressiveness);
    loop {
        let frame = match capture.next_frame() {
            Ok(frame) => frame,
            Err(error) => {
                println!("event=audio_capture_error reason={error}");
                // The utterance in flight is worth more than the failure: flush
                // before reporting, so a stream that dies mid-sentence still
                // writes the sentence.
                if let Some(utterance) = segmenter.flush() {
                    preserve_terminal_utterance(terminal, utterance);
                }
                let _ = reports.blocking_send(Observed::Gap(CaptureGap::CaptureUnavailable));
                return;
            }
        };
        if let Some(utterance) = segmenter.push_frame(&frame.samples, frame.captured_at) {
            offer(utterances, utterance);
        }
    }
}

/// Preserves the one utterance closed by a terminal capture failure.
///
/// This side channel is unbounded but can contain at most one value: the
/// capture loop returns immediately after sending it. That makes the send
/// nonblocking even when the ordinary bounded queue is full, while the
/// transcriber still drains the ordinary queue first to preserve chronology.
fn preserve_terminal_utterance(terminal: &UnboundedSyncSender<Utterance>, utterance: Utterance) {
    let _ = terminal.send(utterance);
}

fn receive_utterance(
    utterances: &SyncReceiver<Utterance>,
    terminal: &SyncReceiver<Utterance>,
) -> Option<Utterance> {
    utterances.recv().ok().or_else(|| terminal.recv().ok())
}

/// Hands an utterance to the transcriber, or drops it and says so.
///
/// The one place in this channel that discards captured audio. It is a
/// `try_send` and not a `send` because the alternative is worse: blocking here
/// stops the WASAPI reader, and the audio engine's buffer then overruns
/// silently, losing audio that nothing counted.
fn offer(utterances: &SyncSender<Utterance>, utterance: Utterance) {
    match utterances.try_send(utterance) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => println!("{AUDIO_BACKLOG}"),
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// Thread 2: utterances to observations. Slow, and deliberately alone.
fn transcribe_loop(
    mut engine: WhisperEngine,
    meta: AudioMetaTemplate,
    app_key: &'static str,
    app_title: &'static str,
    utterances: &SyncReceiver<Utterance>,
    terminal: &SyncReceiver<Utterance>,
    observed: &Sender<Observed>,
) {
    while let Some(utterance) = receive_utterance(utterances, terminal) {
        let transcript = match engine.transcribe(&utterance.samples) {
            Ok(transcript) => transcript,
            Err(error) => {
                println!("event=audio_transcribe_error reason={error}");
                // The audio existed and could not be read - the same shape of
                // failure as an OCR pass that would not run, and counted the
                // same way so the run loop's ceiling still means something.
                if observed
                    .blocking_send(Observed::Gap(CaptureGap::OcrUnavailable))
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };

        if transcript.is_empty() {
            // Not a gap. A gap says capture was attempted and produced nothing,
            // which advances the run loop's abort ceiling; a quiet room is the
            // normal state of an audio channel and must not look like a fault.
            println!("{AUDIO_NO_SPEECH}");
            continue;
        }

        let sample = ObservationSample {
            captured_at: utterance.started_at,
            app_key: app_key.to_owned(),
            app_title: app_title.to_owned(),
            // Empty in v1. Attaching the foreground window would mean a live
            // channel between this process and the screen recorder, which does
            // not exist - and a guess here would put "Zoom" on an event that
            // was a video playing in a browser.
            window_title: String::new(),
            ocr_text: transcript.raw,
            readable_text: transcript.text,
            browser_url: None,
        };
        // An utterance occupies a span, and this is the only channel where
        // that is true. The envelope preserves the stable public sample shape
        // while carrying the end and audio source facts through the runner.
        let observation = ObservationEnvelope::spanning(
            sample,
            utterance.ended_at,
            AudioMeta {
                channel: meta.channel,
                device_category: meta.device_category,
                engine: "whisper-rs",
                model: meta.model.clone(),
                vad_engine: "webrtc-vad",
                vad_aggressiveness: meta.vad_aggressiveness,
                language: meta.language.clone(),
                avg_no_speech_permille: transcript.avg_no_speech_prob.map(to_permille),
                closed_by: utterance.closed_by.as_code(),
            },
        );

        if observed
            .blocking_send(Observed::Sample(Box::new(observation)))
            .is_err()
        {
            return;
        }
    }
}

/// A probability in [0, 1] as parts per thousand, clamped.
///
/// Clamped rather than trusted: this comes out of C, and a NaN or an
/// out-of-range value would otherwise become an arbitrary integer in a durable
/// row through the `as` cast.
fn to_permille(probability: f32) -> u16 {
    if !probability.is_finite() {
        return 0;
    }
    (probability.clamp(0.0, 1.0) * 1000.0).round() as u16
}

#[cfg(test)]
mod tests {
    use super::{
        AUDIO_BACKLOG, AUDIO_NO_SPEECH, CaptureStopped, TRANSCRIBE_QUEUE, WRITE_QUEUE,
        preserve_terminal_utterance, receive_utterance, to_permille,
    };
    use chrono::{TimeZone, Utc};
    use screenpipe_audio::{Utterance, UtteranceEnd};

    fn synthetic_utterance(second: u32) -> Utterance {
        let started_at = Utc.with_ymd_and_hms(2026, 8, 8, 0, 0, second).unwrap();
        Utterance {
            started_at,
            ended_at: started_at + chrono::Duration::milliseconds(20),
            samples: vec![0.0],
            closed_by: UtteranceEnd::StreamClosed,
        }
    }

    #[test]
    fn terminal_flush_bypasses_a_full_capture_queue_without_reordering() {
        let (queued_tx, queued_rx) = std::sync::mpsc::sync_channel(1);
        let (terminal_tx, terminal_rx) = std::sync::mpsc::channel();
        let queued = synthetic_utterance(1);
        let terminal = synthetic_utterance(2);
        queued_tx.send(queued.clone()).unwrap();

        preserve_terminal_utterance(&terminal_tx, terminal.clone());
        drop(queued_tx);
        drop(terminal_tx);

        assert_eq!(receive_utterance(&queued_rx, &terminal_rx), Some(queued));
        assert_eq!(receive_utterance(&queued_rx, &terminal_rx), Some(terminal));
        assert_eq!(receive_utterance(&queued_rx, &terminal_rx), None);
    }

    #[test]
    fn capture_shutdown_remains_typed_through_anyhow_context() {
        let error = anyhow::Error::new(CaptureStopped).context("audio source read failed");

        assert!(error.downcast_ref::<CaptureStopped>().is_some());
    }

    #[test]
    fn a_probability_becomes_parts_per_thousand() {
        assert_eq!(to_permille(0.0), 0);
        assert_eq!(to_permille(0.03), 30);
        assert_eq!(to_permille(1.0), 1000);
    }

    #[test]
    fn a_nonsense_probability_cannot_reach_a_durable_row() {
        // This value comes out of C. Without the clamp, `as u16` on a NaN or a
        // negative is an arbitrary number that would be stored and later read
        // as a confidence.
        assert_eq!(to_permille(f32::NAN), 0);
        assert_eq!(to_permille(f32::INFINITY), 0);
        assert_eq!(to_permille(-1.0), 0);
        assert_eq!(to_permille(9.5), 1000);
    }

    #[test]
    fn the_capture_queue_is_smaller_than_the_write_queue() {
        // Where audio is dropped must be the transcriber, never the writer: the
        // writer's failures are retried with the sample intact, and dropping
        // there would lose a transcript that had already been produced.
        const { assert!(TRANSCRIBE_QUEUE < WRITE_QUEUE) };
    }

    #[test]
    fn the_channel_diagnostics_carry_only_fixed_categories() {
        // These lines are printed by loops that hold a buffer of somebody's
        // speech. Pinning their exact text is what stops its length, its
        // timing, or a hash of it being interpolated into them later.
        assert_eq!(
            AUDIO_BACKLOG,
            "event=audio_dropped reason=transcriber_backlog"
        );
        assert_eq!(AUDIO_NO_SPEECH, "event=audio_discarded reason=no_speech");
        for line in [AUDIO_BACKLOG, AUDIO_NO_SPEECH] {
            assert!(!line.contains('{'), "{line} carries a format placeholder");
        }
    }
}
