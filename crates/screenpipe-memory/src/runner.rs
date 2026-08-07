use anyhow::{Context, Result, bail};
use async_trait::async_trait;

use crate::{
    CadenceRecord, CaptureGap, CaptureGapSummary, MergeDecision, Merger, ObservationSample,
    OpenEvent, SplitReason, TextIdentity,
};

// The `Sample` variant is ~216 bytes against `Gap`'s 1. Boxing to even that
// out would buy an allocation per capture on a path that produces at most one
// value every two seconds, and would put an indirection between the runner and
// the sample it immediately destructures. The size difference is real and
// deliberate, not an oversight.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SampleRead {
    Sample {
        sample: ObservationSample,
        cadence: CadenceRecord,
    },
    Gap(CaptureGap),
}

/// Capture boundary. Implementations must be movable to the runner task.
#[async_trait]
pub trait SampleSource: Send {
    async fn next_sample(&mut self) -> Result<SampleRead>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventId(String);

impl EventId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for EventId {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        if value.trim().is_empty() {
            bail!("event sink returned a blank start id");
        }
        Ok(Self(value))
    }
}

/// Durable event boundary. Implementations must be safe to share with the
/// runner task. Both operations must be atomic so retrying an error is safe.
/// `start` must allocate and insert in one transaction, return only a validated
/// ID for the durably inserted row, and leave no durable insert visible on
/// error.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn start(&self, event: &OpenEvent, reason: SplitReason) -> Result<EventId>;
    async fn merge(&self, event_id: &str, event: &OpenEvent) -> Result<()>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    GapRecorded {
        gap: CaptureGap,
    },
    Started {
        event_id: String,
        reason: SplitReason,
    },
    Merged {
        event_id: String,
    },
}

pub struct Runner {
    merger: Merger,
    current_event_id: Option<EventId>,
    pending_gaps: CaptureGapSummary,
    pending_sample: Option<(ObservationSample, CadenceRecord)>,
}

impl Runner {
    pub fn new(config: crate::MergeConfig) -> Self {
        Self {
            merger: Merger::new(config),
            current_event_id: None,
            pending_gaps: CaptureGapSummary::default(),
            pending_sample: None,
        }
    }

    pub fn current_event_id(&self) -> Option<&str> {
        self.current_event_id.as_ref().map(EventId::as_str)
    }

    pub fn pending_gaps(&self) -> CaptureGapSummary {
        self.pending_gaps
    }

    pub async fn run_once(
        &mut self,
        source: &mut dyn SampleSource,
        sink: &dyn EventSink,
    ) -> Result<RunOutcome> {
        let (sample, cadence) = if let Some(pending) = self.pending_sample.clone() {
            pending
        } else {
            let read = source.next_sample().await?;
            let pending = match read {
                SampleRead::Gap(gap) => return Ok(self.record_gap(gap)),
                SampleRead::Sample { sample, cadence } => (sample, cadence),
            };

            if TextIdentity::from_ocr(&pending.0.ocr_text)
                .normalized
                .is_empty()
            {
                return Ok(self.record_gap(CaptureGap::EmptyOcr));
            }

            self.pending_sample = Some(pending.clone());
            pending
        };

        let mut staged_merger = self.merger.clone();
        let decision = staged_merger.ingest_with_metadata(sample, cadence, self.pending_gaps);
        match decision {
            MergeDecision::Start { reason, event } => {
                let event_id = sink.start(&event, reason).await?;
                self.merger = staged_merger;
                self.current_event_id = Some(event_id.clone());
                self.pending_gaps = CaptureGapSummary::default();
                self.pending_sample = None;
                Ok(RunOutcome::Started {
                    event_id: event_id.as_str().to_owned(),
                    reason,
                })
            }
            MergeDecision::Merge { event } => {
                let event_id = self
                    .current_event_id
                    .clone()
                    .context("merger produced merge without a durable event id")?;
                sink.merge(event_id.as_str(), &event).await?;
                self.merger = staged_merger;
                self.pending_gaps = CaptureGapSummary::default();
                self.pending_sample = None;
                Ok(RunOutcome::Merged {
                    event_id: event_id.as_str().to_owned(),
                })
            }
        }
    }

    fn record_gap(&mut self, gap: CaptureGap) -> RunOutcome {
        self.pending_gaps.record(gap);
        RunOutcome::GapRecorded { gap }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use anyhow::{Result, anyhow};
    use async_trait::async_trait;
    use chrono::{Duration, TimeZone, Utc};

    use crate::{
        CadenceInput, CadenceRecord, CaptureGap, CaptureGapSummary, EventId, EventKind, EventSink,
        MERGE_CONTRACT_VERSION, MergeConfig, MergeDecisionKind, ObservationSample, OpenEvent,
        RunOutcome, Runner, SampleRead, SampleSource, SplitReason, TextIdentity,
    };

    fn at(second: i64) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 3, 12, 0, second as u32)
            .single()
            .unwrap()
    }

    fn sample(second: i64, app_key: &str, window_title: &str, ocr_text: &str) -> ObservationSample {
        ObservationSample {
            captured_at: at(second),
            app_key: app_key.to_owned(),
            app_title: app_key.trim_end_matches(".exe").to_owned(),
            window_title: window_title.to_owned(),
            ocr_text: ocr_text.to_owned(),
            readable_text: ocr_text.to_owned(),
            browser_url: None,
        }
    }

    fn cadence(seconds: i64) -> CadenceRecord {
        CadenceRecord {
            input: CadenceInput {
                input_idle: Duration::seconds(seconds),
                frame_stable_for: Duration::seconds(seconds),
                foreground_changed: false,
                frame_changed: false,
            },
            next_interval: Duration::seconds(if seconds >= 30 { 5 } else { 2 }),
        }
    }

    fn sample_read(second: i64, app_key: &str, window_title: &str, ocr_text: &str) -> SampleRead {
        SampleRead::Sample {
            sample: sample(second, app_key, window_title, ocr_text),
            cadence: cadence(second),
        }
    }

    fn runner() -> Runner {
        Runner::new(MergeConfig {
            kind: EventKind::Screen,
            idle_gap: Duration::seconds(30),
            scroll_overlap: 0.35,
        })
    }

    struct MemorySource {
        reads: VecDeque<Result<SampleRead>>,
        read_count: usize,
    }

    impl MemorySource {
        fn new(reads: impl IntoIterator<Item = Result<SampleRead>>) -> Self {
            Self {
                reads: reads.into_iter().collect(),
                read_count: 0,
            }
        }

        fn read_count(&self) -> usize {
            self.read_count
        }

        fn remaining(&self) -> usize {
            self.reads.len()
        }
    }

    #[async_trait]
    impl SampleSource for MemorySource {
        async fn next_sample(&mut self) -> Result<SampleRead> {
            self.read_count += 1;
            self.reads
                .pop_front()
                .expect("test source should have another read")
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    enum SinkCall {
        Start {
            event: OpenEvent,
            reason: SplitReason,
        },
        Merge {
            event_id: String,
            event: OpenEvent,
        },
    }

    struct RecordingSink {
        calls: Mutex<Vec<SinkCall>>,
        start_results: Mutex<VecDeque<Result<String>>>,
        merge_results: Mutex<VecDeque<Result<()>>>,
    }

    impl RecordingSink {
        fn new(
            start_results: impl IntoIterator<Item = Result<String>>,
            merge_results: impl IntoIterator<Item = Result<()>>,
        ) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                start_results: Mutex::new(start_results.into_iter().collect()),
                merge_results: Mutex::new(merge_results.into_iter().collect()),
            }
        }

        fn calls(&self) -> Vec<SinkCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl EventSink for RecordingSink {
        async fn start(&self, event: &OpenEvent, reason: SplitReason) -> Result<EventId> {
            self.calls.lock().unwrap().push(SinkCall::Start {
                event: event.clone(),
                reason,
            });
            let raw_id = self
                .start_results
                .lock()
                .unwrap()
                .pop_front()
                .expect("test sink should have a start result")?;
            EventId::try_from(raw_id)
        }

        async fn merge(&self, event_id: &str, event: &OpenEvent) -> Result<()> {
            self.calls.lock().unwrap().push(SinkCall::Merge {
                event_id: event_id.to_owned(),
                event: event.clone(),
            });
            self.merge_results
                .lock()
                .unwrap()
                .pop_front()
                .expect("test sink should have a merge result")
        }
    }

    fn started_event(call: &SinkCall) -> &OpenEvent {
        match call {
            SinkCall::Start { event, .. } => event,
            SinkCall::Merge { .. } => panic!("expected a start call"),
        }
    }

    fn merged_event(call: &SinkCall) -> (&str, &OpenEvent) {
        match call {
            SinkCall::Merge { event_id, event } => (event_id, event),
            SinkCall::Start { .. } => panic!("expected a merge call"),
        }
    }

    #[tokio::test]
    async fn first_sample_starts_then_exact_match_merges_using_returned_id() {
        let mut source = MemorySource::new([
            Ok(sample_read(0, "notepad.exe", "notes", "same text")),
            Ok(sample_read(1, "notepad.exe", "notes", " SAME\tTEXT ")),
        ]);
        let sink = RecordingSink::new([Ok("event-123".to_owned())], [Ok(())]);
        let mut runner = runner();

        assert_eq!(
            runner.run_once(&mut source, &sink).await.unwrap(),
            RunOutcome::Started {
                event_id: "event-123".to_owned(),
                reason: SplitReason::Initial,
            }
        );
        assert_eq!(
            runner.run_once(&mut source, &sink).await.unwrap(),
            RunOutcome::Merged {
                event_id: "event-123".to_owned(),
            }
        );

        let calls = sink.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(merged_event(&calls[1]).0, "event-123");
        assert_eq!(merged_event(&calls[1]).1.sample_count, 2);
    }

    #[tokio::test]
    async fn split_replaces_the_durable_id_for_later_merges() {
        let mut source = MemorySource::new([
            Ok(sample_read(0, "notepad.exe", "notes", "same text")),
            Ok(sample_read(1, "msedge.exe", "browser", "web text")),
            Ok(sample_read(2, "msedge.exe", "browser", "web text")),
        ]);
        let sink = RecordingSink::new(
            [Ok("event-old".to_owned()), Ok("event-new".to_owned())],
            [Ok(())],
        );
        let mut runner = runner();

        runner.run_once(&mut source, &sink).await.unwrap();
        let split = runner.run_once(&mut source, &sink).await.unwrap();
        runner.run_once(&mut source, &sink).await.unwrap();

        assert_eq!(
            split,
            RunOutcome::Started {
                event_id: "event-new".to_owned(),
                reason: SplitReason::AppChange,
            }
        );
        let calls = sink.calls();
        assert_eq!(merged_event(&calls[2]).0, "event-new");
    }

    #[tokio::test]
    async fn gap_before_first_sample_attaches_to_the_successful_start() {
        let mut source = MemorySource::new([
            Ok(SampleRead::Gap(CaptureGap::CaptureUnavailable)),
            Ok(sample_read(1, "notepad.exe", "notes", "same text")),
        ]);
        let sink = RecordingSink::new([Ok("event-1".to_owned())], []);
        let mut runner = runner();

        assert_eq!(
            runner.run_once(&mut source, &sink).await.unwrap(),
            RunOutcome::GapRecorded {
                gap: CaptureGap::CaptureUnavailable,
            }
        );
        runner.run_once(&mut source, &sink).await.unwrap();

        let calls = sink.calls();
        assert_eq!(
            started_event(&calls[0]).capture_gaps,
            CaptureGapSummary {
                capture_unavailable: 1,
                ocr_unavailable: 0,
                empty_ocr: 0,
                desktop_locked: 0,
            }
        );
        assert_eq!(runner.pending_gaps(), CaptureGapSummary::default());
    }

    #[tokio::test]
    async fn gap_with_open_event_attaches_to_the_next_successful_merge() {
        let mut source = MemorySource::new([
            Ok(sample_read(0, "notepad.exe", "notes", "same text")),
            Ok(SampleRead::Gap(CaptureGap::OcrUnavailable)),
            Ok(sample_read(2, "notepad.exe", "notes", "same text")),
        ]);
        let sink = RecordingSink::new([Ok("event-1".to_owned())], [Ok(())]);
        let mut runner = runner();

        runner.run_once(&mut source, &sink).await.unwrap();
        runner.run_once(&mut source, &sink).await.unwrap();
        runner.run_once(&mut source, &sink).await.unwrap();

        let calls = sink.calls();
        assert_eq!(
            merged_event(&calls[1]).1.capture_gaps,
            CaptureGapSummary {
                capture_unavailable: 0,
                ocr_unavailable: 1,
                empty_ocr: 0,
                desktop_locked: 0,
            }
        );
        assert_eq!(runner.pending_gaps(), CaptureGapSummary::default());
    }

    #[tokio::test]
    async fn failed_initial_start_retries_the_exact_sample_without_reading_the_next_item() {
        let mut source = MemorySource::new([
            Ok(SampleRead::Gap(CaptureGap::CaptureUnavailable)),
            Ok(sample_read(0, "notepad.exe", "notes", "first")),
            Ok(sample_read(1, "notepad.exe", "notes", "second")),
        ]);
        let sink = RecordingSink::new([Err(anyhow!("start failed")), Ok("event-2".to_owned())], []);
        let mut runner = runner();

        runner.run_once(&mut source, &sink).await.unwrap();
        assert!(runner.run_once(&mut source, &sink).await.is_err());
        assert_eq!(runner.pending_gaps().capture_unavailable, 1);
        assert_eq!((source.read_count(), source.remaining()), (2, 1));
        let retry = runner.run_once(&mut source, &sink).await.unwrap();

        assert_eq!(
            retry,
            RunOutcome::Started {
                event_id: "event-2".to_owned(),
                reason: SplitReason::Initial,
            }
        );
        assert_eq!(runner.current_event_id(), Some("event-2"));
        let calls = sink.calls();
        assert_eq!(started_event(&calls[0]), started_event(&calls[1]));
        assert_eq!(
            calls
                .iter()
                .map(|call| match call {
                    SinkCall::Start { reason, .. } => *reason,
                    SinkCall::Merge { .. } => panic!("failed start must not make merge possible"),
                })
                .collect::<Vec<_>>(),
            vec![SplitReason::Initial, SplitReason::Initial]
        );
        assert_eq!(started_event(&calls[1]).capture_gaps.capture_unavailable, 1);
        assert_eq!(started_event(&calls[1]).latest.ocr_text, "first");
        assert_eq!(started_event(&calls[1]).latest_cadence, cadence(0));
        assert_eq!((source.read_count(), source.remaining()), (2, 1));
        assert_eq!(runner.pending_gaps(), CaptureGapSummary::default());
    }

    #[tokio::test]
    async fn failed_merge_retries_the_exact_sample_without_reading_the_next_item() {
        let mut source = MemorySource::new([
            Ok(sample_read(0, "notepad.exe", "notes", "same text")),
            Ok(SampleRead::Gap(CaptureGap::OcrUnavailable)),
            Ok(sample_read(2, "notepad.exe", "notes", "same text")),
            Ok(sample_read(3, "notepad.exe", "notes", "same text")),
        ]);
        let sink = RecordingSink::new(
            [Ok("event-1".to_owned())],
            [Err(anyhow!("merge failed")), Ok(())],
        );
        let mut runner = runner();

        runner.run_once(&mut source, &sink).await.unwrap();
        runner.run_once(&mut source, &sink).await.unwrap();
        assert!(runner.run_once(&mut source, &sink).await.is_err());
        assert_eq!(runner.current_event_id(), Some("event-1"));
        assert_eq!(runner.pending_gaps().ocr_unavailable, 1);
        assert_eq!((source.read_count(), source.remaining()), (3, 1));
        runner.run_once(&mut source, &sink).await.unwrap();

        let calls = sink.calls();
        let (failed_id, failed_event) = merged_event(&calls[1]);
        let (retry_id, retry_event) = merged_event(&calls[2]);
        assert_eq!((failed_id, retry_id), ("event-1", "event-1"));
        assert_eq!(failed_event, retry_event);
        assert_eq!(
            (failed_event.sample_count, retry_event.sample_count),
            (2, 2)
        );
        assert_eq!(retry_event.capture_gaps.ocr_unavailable, 1);
        assert_eq!(retry_event.latest.ocr_text, "same text");
        assert_eq!(retry_event.latest_cadence, cadence(2));
        assert_eq!((source.read_count(), source.remaining()), (3, 1));
        assert_eq!(runner.pending_gaps(), CaptureGapSummary::default());
    }

    #[tokio::test]
    async fn failed_split_retries_the_exact_split_without_reading_the_next_item() {
        let mut source = MemorySource::new([
            Ok(sample_read(0, "notepad.exe", "notes", "same text")),
            Ok(SampleRead::Gap(CaptureGap::OcrUnavailable)),
            Ok(sample_read(1, "msedge.exe", "browser", "web text")),
            Ok(sample_read(2, "notepad.exe", "notes", "same text")),
        ]);
        let sink = RecordingSink::new(
            [
                Ok("event-old".to_owned()),
                Err(anyhow!("split failed")),
                Ok("event-new".to_owned()),
            ],
            [],
        );
        let mut runner = runner();

        runner.run_once(&mut source, &sink).await.unwrap();
        runner.run_once(&mut source, &sink).await.unwrap();
        assert!(runner.run_once(&mut source, &sink).await.is_err());
        assert_eq!(runner.pending_gaps().ocr_unavailable, 1);
        assert_eq!((source.read_count(), source.remaining()), (3, 1));
        let retry = runner.run_once(&mut source, &sink).await.unwrap();

        assert_eq!(
            retry,
            RunOutcome::Started {
                event_id: "event-new".to_owned(),
                reason: SplitReason::AppChange,
            }
        );
        assert_eq!(runner.current_event_id(), Some("event-new"));
        let calls = sink.calls();
        assert_eq!(calls[1], calls[2]);
        assert_eq!(started_event(&calls[2]).latest.app_key, "msedge.exe");
        assert_eq!(started_event(&calls[2]).latest.ocr_text, "web text");
        assert_eq!(started_event(&calls[2]).latest_cadence, cadence(1));
        assert_eq!(started_event(&calls[2]).capture_gaps.ocr_unavailable, 1);
        assert_eq!((source.read_count(), source.remaining()), (3, 1));
        assert_eq!(runner.pending_gaps(), CaptureGapSummary::default());
    }

    #[tokio::test]
    async fn fatal_source_error_is_returned_without_touching_sink_or_runner_state() {
        let mut source = MemorySource::new([Err(anyhow!("capture boundary crashed"))]);
        let sink = RecordingSink::new([], []);
        let mut runner = runner();

        let error = runner.run_once(&mut source, &sink).await.unwrap_err();

        assert_eq!(error.to_string(), "capture boundary crashed");
        assert!(sink.calls().is_empty());
        assert_eq!(runner.current_event_id(), None);
        assert_eq!(runner.pending_gaps(), CaptureGapSummary::default());
    }

    #[tokio::test]
    async fn invalid_start_id_retries_the_exact_sample_without_reading_the_next_item() {
        let mut source = MemorySource::new([
            Ok(SampleRead::Gap(CaptureGap::CaptureUnavailable)),
            Ok(sample_read(0, "notepad.exe", "notes", "first")),
            Ok(sample_read(1, "notepad.exe", "notes", "second")),
        ]);
        let sink = RecordingSink::new([Ok(" \t".to_owned()), Ok("event-good".to_owned())], []);
        let mut runner = runner();

        runner.run_once(&mut source, &sink).await.unwrap();
        let error = runner.run_once(&mut source, &sink).await.unwrap_err();
        assert_eq!(error.to_string(), "event sink returned a blank start id");
        assert_eq!(runner.current_event_id(), None);
        assert_eq!(runner.pending_gaps().capture_unavailable, 1);
        assert_eq!((source.read_count(), source.remaining()), (2, 1));
        let retry = runner.run_once(&mut source, &sink).await.unwrap();

        assert_eq!(
            retry,
            RunOutcome::Started {
                event_id: "event-good".to_owned(),
                reason: SplitReason::Initial,
            }
        );
        let calls = sink.calls();
        assert_eq!(started_event(&calls[0]), started_event(&calls[1]));
        assert_eq!(started_event(&calls[1]).latest.ocr_text, "first");
        assert_eq!(started_event(&calls[1]).latest_cadence, cadence(0));
        assert_eq!(started_event(&calls[1]).capture_gaps.capture_unavailable, 1);
        assert_eq!(runner.pending_gaps(), CaptureGapSummary::default());
        assert_eq!((source.read_count(), source.remaining()), (2, 1));
    }

    #[tokio::test]
    async fn metadata_tracks_start_reason_decision_exact_hash_cadence_gaps_and_latest_url() {
        let mut first = sample(
            0,
            "notepad.exe",
            "notes",
            "one two three four five six seven eight nine ten",
        );
        first.browser_url = Some("https://example.test/first".to_owned());
        let mut second = sample(
            2,
            "notepad.exe",
            "notes",
            "three four five six seven eight nine ten eleven twelve",
        );
        second.browser_url = Some("https://example.test/latest".to_owned());
        let second_exact_hash = TextIdentity::from_ocr(&second.ocr_text).exact_hash;
        let mut source = MemorySource::new([
            Ok(SampleRead::Gap(CaptureGap::CaptureUnavailable)),
            Ok(SampleRead::Sample {
                sample: first,
                cadence: cadence(1),
            }),
            Ok(SampleRead::Gap(CaptureGap::EmptyOcr)),
            Ok(SampleRead::Sample {
                sample: second.clone(),
                cadence: cadence(30),
            }),
        ]);
        let sink = RecordingSink::new([Ok("event-1".to_owned())], [Ok(())]);
        let mut runner = runner();

        for _ in 0..4 {
            runner.run_once(&mut source, &sink).await.unwrap();
        }

        let calls = sink.calls();
        let start = started_event(&calls[0]);
        assert_eq!(start.merge_contract_version, MERGE_CONTRACT_VERSION);
        assert_eq!(start.start_reason, SplitReason::Initial);
        assert_eq!(start.last_decision, MergeDecisionKind::Start);
        assert_eq!(start.latest_cadence, cadence(1));
        assert_eq!(start.capture_gaps.capture_unavailable, 1);

        let merged = merged_event(&calls[1]).1;
        assert_eq!(merged.merge_contract_version, MERGE_CONTRACT_VERSION);
        assert_eq!(merged.start_reason, SplitReason::Initial);
        assert_eq!(merged.last_decision, MergeDecisionKind::Merge);
        assert_eq!(merged.latest_exact_ocr_hash, second_exact_hash);
        assert_ne!(merged.latest_exact_ocr_hash, merged.merge_hash);
        assert_eq!(merged.latest_cadence, cadence(30));
        assert_eq!(
            merged.capture_gaps,
            CaptureGapSummary {
                capture_unavailable: 1,
                ocr_unavailable: 0,
                empty_ocr: 1,
                desktop_locked: 0,
            }
        );
        assert_eq!(merged.latest, second);
    }

    #[tokio::test]
    async fn empty_ocr_sample_becomes_a_typed_gap_and_never_starts_an_event() {
        let mut source =
            MemorySource::new([Ok(sample_read(0, "notepad.exe", "notes", " \t\r\n "))]);
        let sink = RecordingSink::new([], []);
        let mut runner = runner();

        let outcome = runner.run_once(&mut source, &sink).await.unwrap();

        assert_eq!(
            outcome,
            RunOutcome::GapRecorded {
                gap: CaptureGap::EmptyOcr,
            }
        );
        assert!(sink.calls().is_empty());
        assert_eq!(runner.current_event_id(), None);
        assert_eq!(runner.pending_gaps().empty_ocr, 1);
    }
}
