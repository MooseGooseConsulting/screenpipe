use anyhow::Result;
use async_trait::async_trait;
use chrono::{Duration, TimeZone, Utc};
use screenpipe_memory::{
    EventId, EventKind, EventSink, MergeConfig, ObservationEnvelope, ObservationIdentity,
    ObservationOutcome, ObservationRead, ObservationSample, RunOutcome, Runner, SampleRead,
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
}

#[test]
fn available_outcome_rejects_an_identity_without_producer_or_version() {
    let mut missing_producer = identity();
    missing_producer.producer.clear();
    assert!(
        ObservationOutcome::Available(missing_producer)
            .validate()
            .is_err()
    );

    let mut missing_version = identity();
    missing_version.version.clear();
    assert!(
        ObservationOutcome::Available(missing_version)
            .validate()
            .is_err()
    );
}

#[test]
fn identified_envelope_retains_a_validated_policy_bound_identity() {
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

    let envelope = ObservationEnvelope::identified_instant(sample, identity.clone()).unwrap();

    assert_eq!(envelope.identity(), Some(&identity));
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
    let outcome = ObservationOutcome::Denied(identity());
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
