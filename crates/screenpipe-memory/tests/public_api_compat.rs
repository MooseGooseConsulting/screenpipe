use chrono::{TimeZone, Utc};
use screenpipe_memory::ObservationSample;

// `ObservationSample` is publicly re-exported and its original fields are
// public. Keep an external-crate struct literal compiling so additive channel
// metadata cannot silently turn this public source contract into a constructor
// migration.
#[test]
fn original_public_struct_literal_remains_source_compatible() {
    let sample = ObservationSample {
        captured_at: Utc.with_ymd_and_hms(2026, 8, 8, 12, 0, 0).unwrap(),
        app_key: "example.exe".to_owned(),
        app_title: "Example".to_owned(),
        window_title: "Window".to_owned(),
        ocr_text: "content".to_owned(),
        readable_text: "content".to_owned(),
        browser_url: None,
    };

    assert_eq!(sample.app_key, "example.exe");
}
