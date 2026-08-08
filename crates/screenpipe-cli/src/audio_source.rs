use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{
    Receiver as SyncReceiver, Sender as UnboundedSyncSender, SyncSender, TrySendError,
    channel as sync_unbounded_channel, sync_channel,
};
use std::thread::{self, JoinHandle};

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

/// Fixed diagnostics for worker failures. The worker threads can hold raw
/// audio, model paths, and OS error details, none of which belong in logs.
const AUDIO_CAPTURE_FAILURE: &str = "event=audio_capture_error category=capture_unavailable";
const AUDIO_TRANSCRIBE_FAILURE: &str = "event=audio_transcribe_error category=transcription";
const AUDIO_WORKER_SHUTDOWN_FAILURE: &str = "one or more audio workers panicked during shutdown";

fn capture_error_reason_code(error: &CaptureError) -> &'static str {
    match error {
        CaptureError::NoDevice => "no_device",
        CaptureError::DeviceUnavailable => "device_unavailable",
        CaptureError::FormatUnsupported => "format_unsupported",
        CaptureError::StreamStalled => "stream_stalled",
        CaptureError::StreamDiscontinuity => "stream_discontinuity",
    }
}

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

/// The one operation capture-loop ownership needs from WASAPI.
///
/// Keeping this private lets the loop run against a scripted source in tests
/// without changing the audio crate's public surface or opening hardware.
trait FrameSource {
    fn next_frame(&mut self) -> Result<screenpipe_audio::CapturedFrame, CaptureError>;
}

impl FrameSource for AudioCapture {
    fn next_frame(&mut self) -> Result<screenpipe_audio::CapturedFrame, CaptureError> {
        AudioCapture::next_frame(self)
    }
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
    shutdown: Arc<AtomicBool>,
    capture_worker: Option<JoinHandle<()>>,
    transcribe_worker: Option<JoinHandle<()>>,
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
        let shutdown = Arc::new(AtomicBool::new(false));

        let capture_reports = observed_tx.clone();
        let capture_shutdown = Arc::clone(&shutdown);
        let aggressiveness = config.aggressiveness;
        let audio_channel = config.channel;
        let capture_worker = thread::Builder::new()
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
                    capture_shutdown.as_ref(),
                );
            })?;

        let device_category = match ready_rx.recv() {
            Ok(Ok(category)) => category,
            Ok(Err(error)) => {
                let _ = capture_worker.join();
                return Err(error.into());
            }
            Err(_) => {
                let _ = capture_worker.join();
                return Err(anyhow::anyhow!(
                    "the audio capture thread stopped before it opened"
                ));
            }
        };

        let engine = match WhisperEngine::load(
            config.model.clone(),
            config.threads,
            config.language.clone(),
        ) {
            Ok(engine) => engine,
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                let _ = capture_worker.join();
                return Err(error.into());
            }
        };

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
        let transcribe_worker = match thread::Builder::new()
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
            }) {
            Ok(worker) => worker,
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                let _ = capture_worker.join();
                return Err(error.into());
            }
        };

        Ok(Self {
            incoming: observed_rx,
            shutdown,
            capture_worker: Some(capture_worker),
            transcribe_worker: Some(transcribe_worker),
        })
    }

    /// Signals capture to flush its open utterance and close the worker inputs.
    pub(crate) fn request_shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// Rejects new worker output while preserving the observations already
    /// buffered for the foreground drain.
    pub(crate) fn stop_accepting_observations(&mut self) {
        self.incoming.close();
    }

    /// Joins every worker after its observed channel has closed, so joining
    /// cannot wait on a writer that the foreground drain has not processed.
    pub(crate) fn join_workers(&mut self) -> Result<()> {
        let mut worker_panicked = false;
        for worker in [&mut self.capture_worker, &mut self.transcribe_worker] {
            if let Some(worker) = worker.take() {
                let join_failed = worker.join().is_err();
                worker_panicked |= join_failed;
            }
        }
        if worker_panicked {
            Err(anyhow::anyhow!(AUDIO_WORKER_SHUTDOWN_FAILURE))
        } else {
            Ok(())
        }
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
fn capture_loop<C: FrameSource>(
    mut capture: C,
    aggressiveness: VadAggressiveness,
    utterances: &SyncSender<Utterance>,
    terminal: &UnboundedSyncSender<Utterance>,
    reports: &Sender<Observed>,
    shutdown: &AtomicBool,
) {
    let mut segmenter = VadSegmenter::new(aggressiveness);
    loop {
        if shutdown.load(Ordering::Acquire) {
            if let Some(utterance) = segmenter.flush() {
                preserve_terminal_utterance(terminal, utterance);
            }
            return;
        }
        let frame = match capture.next_frame() {
            Ok(frame) => frame,
            Err(error) => {
                let reason = capture_error_reason_code(&error);
                println!("{AUDIO_CAPTURE_FAILURE} reason={reason}");
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
                let _ = error;
                println!("{AUDIO_TRANSCRIBE_FAILURE}");
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
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::{
        AUDIO_BACKLOG, AUDIO_CAPTURE_FAILURE, AUDIO_NO_SPEECH, AUDIO_TRANSCRIBE_FAILURE,
        AudioSampleSource, CaptureError, CaptureStopped, FrameSource, TRANSCRIBE_QUEUE,
        WRITE_QUEUE, capture_error_reason_code, capture_loop, receive_utterance, to_permille,
    };
    use chrono::{TimeZone, Utc};
    use screenpipe_audio::{CapturedFrame, Utterance, UtteranceEnd, VadAggressiveness};

    struct ScriptedCapture {
        frames: std::collections::VecDeque<Result<CapturedFrame, CaptureError>>,
    }

    impl FrameSource for ScriptedCapture {
        fn next_frame(&mut self) -> Result<CapturedFrame, CaptureError> {
            self.frames
                .pop_front()
                .expect("the capture loop read beyond the scripted stream")
        }
    }

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
    fn terminal_capture_flush_bypasses_a_full_queue_without_reordering() {
        let (queued_tx, queued_rx) = std::sync::mpsc::sync_channel(1);
        let (terminal_tx, terminal_rx) = std::sync::mpsc::channel();
        let queued = synthetic_utterance(1);
        queued_tx.send(queued.clone()).unwrap();

        let started_at = Utc.with_ymd_and_hms(2026, 8, 8, 0, 0, 2).unwrap();
        let frames = (0..3)
            .map(|index| {
                Ok(CapturedFrame {
                    captured_at: started_at + chrono::Duration::milliseconds(index * 20),
                    samples: vec![i16::MAX; 320],
                })
            })
            .chain(std::iter::once(Err(CaptureError::StreamStalled)))
            .collect();
        let capture = ScriptedCapture { frames };
        let (reports_tx, mut reports_rx) = tokio::sync::mpsc::channel(1);

        capture_loop(
            capture,
            VadAggressiveness::Quality,
            &queued_tx,
            &terminal_tx,
            &reports_tx,
            &AtomicBool::new(false),
        );
        drop(queued_tx);
        drop(terminal_tx);

        assert_eq!(receive_utterance(&queued_rx, &terminal_rx), Some(queued));
        let terminal = receive_utterance(&queued_rx, &terminal_rx).expect("terminal utterance");
        assert_eq!(terminal.closed_by, UtteranceEnd::StreamClosed);
        assert_eq!(terminal.started_at, started_at);
        assert_eq!(receive_utterance(&queued_rx, &terminal_rx), None);
        assert!(matches!(
            reports_rx.try_recv(),
            Ok(super::Observed::Gap(
                screenpipe_memory::CaptureGap::CaptureUnavailable
            ))
        ));
    }

    struct ShutdownAfterFrames {
        frames: std::collections::VecDeque<CapturedFrame>,
        shutdown: Arc<AtomicBool>,
    }

    impl FrameSource for ShutdownAfterFrames {
        fn next_frame(&mut self) -> Result<CapturedFrame, CaptureError> {
            let frame = self.frames.pop_front().expect("a scripted frame");
            if self.frames.is_empty() {
                self.shutdown
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            Ok(frame)
        }
    }

    #[test]
    fn capture_shutdown_flushes_the_open_utterance_without_a_capture_gap() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let started_at = Utc.with_ymd_and_hms(2026, 8, 8, 0, 0, 2).unwrap();
        let capture = ShutdownAfterFrames {
            frames: (0..3)
                .map(|index| CapturedFrame {
                    captured_at: started_at + chrono::Duration::milliseconds(index * 20),
                    samples: vec![i16::MAX; 320],
                })
                .collect(),
            shutdown: Arc::clone(&shutdown),
        };
        let (queued_tx, _queued_rx) = std::sync::mpsc::sync_channel(1);
        let (terminal_tx, terminal_rx) = std::sync::mpsc::channel();
        let (reports_tx, mut reports_rx) = tokio::sync::mpsc::channel(1);

        capture_loop(
            capture,
            VadAggressiveness::Quality,
            &queued_tx,
            &terminal_tx,
            &reports_tx,
            shutdown.as_ref(),
        );

        let flushed = terminal_rx
            .recv()
            .expect("the in-flight utterance is flushed");
        assert_eq!(flushed.closed_by, UtteranceEnd::StreamClosed);
        assert!(
            reports_rx.try_recv().is_err(),
            "shutdown is not a capture gap"
        );
    }

    #[test]
    fn capture_shutdown_remains_typed_through_anyhow_context() {
        let error = anyhow::Error::new(CaptureStopped).context("audio source read failed");

        assert!(error.downcast_ref::<CaptureStopped>().is_some());
    }

    #[test]
    fn joining_workers_attempts_both_and_redacts_any_panic() {
        for (capture_panics, transcriber_panics) in [(true, false), (false, true), (true, true)] {
            let (observed_tx, incoming) = tokio::sync::mpsc::channel(1);
            drop(observed_tx);
            let capture_worker = std::thread::spawn(move || {
                assert!(!capture_panics, "private capture panic value");
            });
            let transcribe_worker = std::thread::spawn(move || {
                assert!(!transcriber_panics, "private transcriber panic value");
            });
            let mut source = AudioSampleSource {
                incoming,
                shutdown: Arc::new(AtomicBool::new(false)),
                capture_worker: Some(capture_worker),
                transcribe_worker: Some(transcribe_worker),
            };

            let error = source.join_workers().unwrap_err();

            assert!(
                source.capture_worker.is_none(),
                "capture join was not attempted"
            );
            assert!(
                source.transcribe_worker.is_none(),
                "transcriber join was not attempted after capture failed"
            );
            assert_eq!(
                error.to_string(),
                "one or more audio workers panicked during shutdown"
            );
            let rendered = format!("{error:?}");
            assert!(!rendered.contains("private capture"));
            assert!(!rendered.contains("private transcriber"));
        }
    }

    #[test]
    fn every_capture_error_has_a_stable_private_safe_reason_code() {
        let cases = [
            (CaptureError::NoDevice, "no_device"),
            (CaptureError::DeviceUnavailable, "device_unavailable"),
            (CaptureError::FormatUnsupported, "format_unsupported"),
            (CaptureError::StreamStalled, "stream_stalled"),
            (CaptureError::StreamDiscontinuity, "stream_discontinuity"),
        ];

        for (error, expected) in cases {
            let reason = capture_error_reason_code(&error);
            assert_eq!(reason, expected);
            assert!(
                reason
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
                "{reason} is not a fixed reason code"
            );
        }
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
        assert_eq!(
            AUDIO_CAPTURE_FAILURE,
            "event=audio_capture_error category=capture_unavailable"
        );
        assert_eq!(
            AUDIO_TRANSCRIBE_FAILURE,
            "event=audio_transcribe_error category=transcription"
        );
        for line in [
            AUDIO_BACKLOG,
            AUDIO_NO_SPEECH,
            AUDIO_CAPTURE_FAILURE,
            AUDIO_TRANSCRIBE_FAILURE,
        ] {
            assert!(!line.contains('{'), "{line} carries a format placeholder");
        }
    }
}
