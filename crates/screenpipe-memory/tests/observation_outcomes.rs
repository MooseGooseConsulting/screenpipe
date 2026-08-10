use anyhow::Result;
use async_trait::async_trait;
use chrono::{Duration, TimeZone, Utc};
use screenpipe_memory::{
    CadenceInput, CadenceRecord, EventId, EventKind, EventSink, MergeConfig, ObservationEnvelope,
    ObservationIdentity, ObservationOutcome, ObservationOutcomeDetails, ObservationRead,
    ObservationReason, ObservationSample, ObservationTiming, RunOutcome, Runner, SampleRead,
    SampleSource, SplitReason, WindowKey,
};

fn identity() -> ObservationIdentity {
    ObservationIdentity {
        source_id: "screen:primary".to_owned(),
        modality: "screen".to_owned(),
        machine_id: "icarus".to_owned(),
        observed_at: Utc.with_ymd_and_hms(2026, 8, 9, 12, 0, 0).unwrap(),
        producer: "screenpipe-cli".to_owned(),
        version: "0.2.0".to_owned(),
        policy_epoch: Some(7),
        window_key: WindowKey::None,
    }
}

fn timing() -> ObservationTiming {
    ObservationTiming {
        started_at: Utc.with_ymd_and_hms(2026, 8, 9, 11, 59, 59).unwrap(),
        finished_at: Utc.with_ymd_and_hms(2026, 8, 9, 12, 0, 0).unwrap(),
    }
}

fn available_outcome() -> ObservationOutcome {
    ObservationOutcome::Available(ObservationOutcomeDetails {
        identity: identity(),
        reason: ObservationReason::Available,
        timing: timing(),
    })
}

#[test]
fn window_key_parses_only_the_canonical_none_or_window_shape() {
    assert_eq!("none".parse::<WindowKey>().unwrap(), WindowKey::None);
    assert_eq!(
        "window(hwnd=42,window_generation=7)"
            .parse::<WindowKey>()
            .unwrap(),
        WindowKey::Window {
            hwnd: 42,
            window_generation: 7,
        }
    );
    assert!(
        "window(hwnd=0,window_generation=7)"
            .parse::<WindowKey>()
            .is_err()
    );
    assert!(
        "window(hwnd=042,window_generation=7)"
            .parse::<WindowKey>()
            .is_err()
    );
    assert!(
        "window(hwnd=42,window_generation=07)"
            .parse::<WindowKey>()
            .is_err()
    );
}

#[test]
fn available_outcome_rejects_an_identity_without_producer_or_version() {
    let mut missing_producer = identity();
    missing_producer.producer.clear();
    assert!(
        ObservationOutcome::Available(ObservationOutcomeDetails {
            identity: missing_producer,
            reason: ObservationReason::Available,
            timing: timing(),
        })
        .validate()
        .is_err()
    );

    let mut missing_version = identity();
    missing_version.version.clear();
    assert!(
        ObservationOutcome::Available(ObservationOutcomeDetails {
            identity: missing_version,
            reason: ObservationReason::Available,
            timing: timing(),
        })
        .validate()
        .is_err()
    );
}

#[test]
fn identity_validation_rejects_blank_required_fields_epochs_and_manual_zero_hwnd() {
    for field in ["source", "modality", "machine"] {
        let mut invalid = identity();
        match field {
            "source" => invalid.source_id.clear(),
            "modality" => invalid.modality.clear(),
            "machine" => invalid.machine_id.clear(),
            _ => unreachable!(),
        }
        assert!(invalid.validate().is_err(), "missing {field} must fail");
    }

    let mut missing_epoch = identity();
    missing_epoch.policy_epoch = None;
    assert!(missing_epoch.validate().is_err());

    let mut zero_epoch = identity();
    zero_epoch.policy_epoch = Some(0);
    assert!(zero_epoch.validate().is_err());

    let mut fake_window = identity();
    fake_window.window_key = WindowKey::Window {
        hwnd: 0,
        window_generation: 7,
    };
    assert!(fake_window.validate().is_err());
}

#[test]
fn outcomes_require_a_state_matched_typed_reason_and_ordered_content_free_timing() {
    assert!(available_outcome().validate().is_ok());

    let wrong_reason = ObservationOutcome::Available(ObservationOutcomeDetails {
        identity: identity(),
        reason: ObservationReason::Denied,
        timing: timing(),
    });
    assert!(wrong_reason.validate().is_err());

    let reversed_timing = ObservationOutcome::Available(ObservationOutcomeDetails {
        identity: identity(),
        reason: ObservationReason::Available,
        timing: ObservationTiming {
            started_at: Utc.with_ymd_and_hms(2026, 8, 9, 12, 0, 1).unwrap(),
            finished_at: Utc.with_ymd_and_hms(2026, 8, 9, 12, 0, 0).unwrap(),
        },
    });
    assert!(reversed_timing.validate().is_err());
}

#[test]
fn available_envelope_retains_a_validated_policy_bound_identity_and_outcome() {
    let identity = identity();
    let sample = ObservationSample {
        captured_at: identity.observed_at,
        app_key: "screen:primary".to_owned(),
        app_title: "Screen".to_owned(),
        window_title: "Desktop".to_owned(),
        ocr_text: "content".to_owned(),
        readable_text: "content".to_owned(),
        browser_url: None,
    };

    let outcome = available_outcome();
    let envelope = ObservationEnvelope::available_instant(sample, outcome.clone()).unwrap();

    assert_eq!(envelope.identity(), Some(&identity));
    assert_eq!(envelope.outcome(), Some(&outcome));
}

struct OutcomeSource {
    outcome: Option<ObservationOutcome>,
}

#[async_trait]
impl SampleSource for OutcomeSource {
    async fn next_sample(&mut self) -> Result<SampleRead> {
        panic!("an outcome source must not be asked for content")
    }

    async fn next_observation(&mut self) -> Result<ObservationRead> {
        Ok(ObservationRead::Outcome(
            self.outcome.take().expect("one outcome"),
        ))
    }
}

struct NoopSink;

#[async_trait]
impl EventSink for NoopSink {
    async fn start(&self, _: &screenpipe_memory::OpenEvent, _: SplitReason) -> Result<EventId> {
        panic!("a content-free outcome must not start an event")
    }

    async fn merge(&self, _: &str, _: &screenpipe_memory::OpenEvent) -> Result<()> {
        panic!("a content-free outcome must not merge an event")
    }
}

#[tokio::test]
async fn runner_returns_content_free_outcomes_without_using_the_sink() {
    let outcome = ObservationOutcome::Denied(ObservationOutcomeDetails {
        identity: identity(),
        reason: ObservationReason::Denied,
        timing: timing(),
    });
    let mut source = OutcomeSource {
        outcome: Some(outcome.clone()),
    };
    let mut runner = Runner::new(MergeConfig {
        kind: EventKind::Screen,
        idle_gap: Duration::seconds(30),
        scroll_overlap: 0.35,
    });

    let result = runner.run_once(&mut source, &NoopSink).await.unwrap();

    assert_eq!(result, RunOutcome::Outcome { outcome });
}

struct UnboundSampleSource {
    sample: Option<ObservationSample>,
}

#[async_trait]
impl SampleSource for UnboundSampleSource {
    async fn next_sample(&mut self) -> Result<SampleRead> {
        panic!("this test supplies an envelope directly")
    }

    async fn next_observation(&mut self) -> Result<ObservationRead> {
        let sample = self.sample.take().expect("one sample");
        Ok(ObservationRead::Sample {
            observation: ObservationEnvelope::instant(sample),
            cadence: CadenceRecord::from_input(CadenceInput {
                input_idle: Duration::zero(),
                frame_stable_for: Duration::zero(),
                foreground_changed: false,
                frame_changed: false,
            }),
        })
    }
}

struct LegacySink;

#[async_trait]
impl EventSink for LegacySink {
    async fn start(&self, _: &screenpipe_memory::OpenEvent, _: SplitReason) -> Result<EventId> {
        EventId::try_from("legacy-event".to_owned())
    }

    async fn merge(&self, _: &str, _: &screenpipe_memory::OpenEvent) -> Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn runner_keeps_legacy_content_operational_during_outcome_migration() {
    let mut source = UnboundSampleSource {
        sample: Some(ObservationSample {
            captured_at: identity().observed_at,
            app_key: "screen:primary".to_owned(),
            app_title: "Screen".to_owned(),
            window_title: "Desktop".to_owned(),
            ocr_text: "content".to_owned(),
            readable_text: "content".to_owned(),
            browser_url: None,
        }),
    };
    let mut runner = Runner::new(MergeConfig {
        kind: EventKind::Screen,
        idle_gap: Duration::seconds(30),
        scroll_overlap: 0.35,
    });

    let result = runner.run_once(&mut source, &LegacySink).await.unwrap();

    assert!(matches!(result, RunOutcome::Started { .. }));
}

#[tokio::test]
async fn runner_rejects_content_free_available_outcome() {
    let mut source = OutcomeSource {
        outcome: Some(available_outcome()),
    };
    let mut runner = Runner::new(MergeConfig {
        kind: EventKind::Screen,
        idle_gap: Duration::seconds(30),
        scroll_overlap: 0.35,
    });

    let error = runner.run_once(&mut source, &NoopSink).await.unwrap_err();

    assert!(
        error
            .to_string()
            .contains("available outcome requires content")
    );
}
