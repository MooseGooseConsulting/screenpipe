use chrono::{Duration, TimeZone, Utc};
use screenpipe_memory::{
    CadenceInput, CadenceRecord, CaptureGapSummary, EventKind, MergeConfig, MergeDecision, Merger,
    ObservationSample, SampleRead,
};

const OCR_SECRET: &str = "OCR_SECRET_SENTINEL_7f41";
const READABLE_SECRET: &str = "READABLE_SECRET_SENTINEL_24ac";
const BROWSER_URL: &str =
    "https://browser.example.test/URL_PATH_SECRET_928b?token=URL_QUERY_SECRET_15de";

fn sensitive_sample() -> ObservationSample {
    ObservationSample {
        captured_at: Utc.with_ymd_and_hms(2026, 8, 3, 12, 0, 0).single().unwrap(),
        app_key: "chrome.exe".to_owned(),
        app_title: "Google Chrome".to_owned(),
        window_title: "Project notes".to_owned(),
        ocr_text: OCR_SECRET.to_owned(),
        readable_text: READABLE_SECRET.to_owned(),
        browser_url: Some(BROWSER_URL.to_owned()),
        observed_until: None,
        audio: None,
    }
}

fn cadence() -> CadenceRecord {
    CadenceRecord::from_input(CadenceInput {
        input_idle: Duration::zero(),
        frame_stable_for: Duration::zero(),
        foreground_changed: false,
        frame_changed: false,
    })
}

fn decision_with_sensitive_sample() -> MergeDecision {
    Merger::new(MergeConfig {
        kind: EventKind::Screen,
        idle_gap: Duration::seconds(30),
        scroll_overlap: 0.35,
    })
    .ingest_with_metadata(sensitive_sample(), cadence(), CaptureGapSummary::default())
}

fn assert_sample_secrets_absent(debug: &str) {
    assert!(!debug.contains(OCR_SECRET), "OCR text leaked: {debug}");
    assert!(
        !debug.contains(READABLE_SECRET),
        "readable text leaked: {debug}"
    );
    assert!(!debug.contains(BROWSER_URL), "browser URL leaked: {debug}");
    assert!(
        !debug.contains("URL_PATH_SECRET_928b"),
        "browser URL path leaked: {debug}"
    );
    assert!(
        !debug.contains("URL_QUERY_SECRET_15de"),
        "browser URL query leaked: {debug}"
    );
}

#[test]
fn observation_sample_debug_redacts_captured_text_and_browser_url() {
    let debug = format!("{:?}", sensitive_sample());

    assert_sample_secrets_absent(&debug);
    assert!(debug.contains("chrome.exe"));
    assert!(debug.contains("Project notes"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn open_event_debug_does_not_expose_latest_sample_secrets() {
    let decision = decision_with_sensitive_sample();
    let MergeDecision::Start { event, .. } = decision else {
        panic!("first sample should start an event");
    };
    let debug = format!("{event:?}");

    assert_sample_secrets_absent(&debug);
    assert!(debug.contains("sample_count: 1"));
    assert!(debug.contains("latest_exact_ocr_hash"));
}

#[test]
fn merge_decision_debug_does_not_expose_nested_sample_secrets() {
    let debug = format!("{:?}", decision_with_sensitive_sample());

    assert_sample_secrets_absent(&debug);
    assert!(debug.contains("Initial"));
    assert!(debug.contains("sample_count: 1"));
}

#[test]
fn sample_read_debug_does_not_expose_nested_sample_secrets() {
    let read = SampleRead::Sample {
        sample: sensitive_sample(),
        cadence: cadence(),
    };
    let debug = format!("{read:?}");

    assert_sample_secrets_absent(&debug);
    assert!(debug.contains("chrome.exe"));
    assert!(debug.contains("next_interval"));
}
