use chrono::{Duration, TimeZone, Utc};
use screenpipe_memory::{
    CadenceInput, CadenceRecord, CaptureGapSummary, MergeConfig, MergeDecision, Merger,
    ObservationSample, SplitReason, TextIdentity,
};

trait TestIngest {
    fn ingest(&mut self, sample: ObservationSample) -> MergeDecision;
}

impl TestIngest for Merger {
    fn ingest(&mut self, sample: ObservationSample) -> MergeDecision {
        self.ingest_with_metadata(
            sample,
            CadenceRecord::from_input(CadenceInput {
                input_idle: Duration::zero(),
                frame_stable_for: Duration::zero(),
                foreground_changed: false,
                frame_changed: false,
            }),
            CaptureGapSummary::default(),
        )
    }
}

/// Offset in seconds from a fixed base. Adds a duration rather than packing
/// the value into the seconds field, so callers are not limited to 0..59.
fn at(second: i64) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 3, 12, 0, 0).single().unwrap() + Duration::seconds(second)
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

/// Threshold used by the boundary tests below, which prove the split semantics
/// (`> idle_gap` splits, `== idle_gap` merges). This is deliberately not the
/// production value; `an_idle_window_sampled_at_max_backoff_keeps_merging`
/// covers the production relationship instead.
const TEST_IDLE_GAP_SECONDS: i64 = 30;

fn merger() -> Merger {
    Merger::new(MergeConfig {
        idle_gap: Duration::seconds(TEST_IDLE_GAP_SECONDS),
        scroll_overlap: 0.35,
    })
}

fn started(decision: MergeDecision, expected_reason: SplitReason) -> screenpipe_memory::OpenEvent {
    match decision {
        MergeDecision::Start { reason, event } => {
            assert_eq!(reason, expected_reason);
            event
        }
        MergeDecision::Merge { .. } => panic!("expected a start decision"),
    }
}

fn merged(decision: MergeDecision) -> screenpipe_memory::OpenEvent {
    match decision {
        MergeDecision::Merge { event } => event,
        MergeDecision::Start { .. } => panic!("expected a merge decision"),
    }
}

#[test]
fn first_sample_starts_an_initial_event() {
    let initial = sample(0, "notepad.exe", "notes", "goal one");
    let expected_hash = TextIdentity::from_ocr("goal one").exact_hash;
    let mut merger = merger();

    let event = started(merger.ingest(initial.clone()), SplitReason::Initial);

    assert_eq!(event.started_at, at(0));
    assert_eq!(event.ended_at, at(0));
    assert_eq!(event.latest, initial);
    assert_eq!(event.merge_hash, expected_hash.clone());
    assert_eq!(event.sample_count, 1);
    assert_eq!(event.hash_counts.get(&expected_hash), Some(&1));
}

#[test]
fn app_change_starts_a_new_event_before_other_checks() {
    let mut merger = merger();
    merger.ingest(sample(0, "notepad.exe", "notes", "same text"));

    let event = started(
        merger.ingest(sample(
            31,
            "msedge.exe",
            "browser",
            "completely different text",
        )),
        SplitReason::AppChange,
    );

    assert_eq!(event.latest.app_key, "msedge.exe");
    assert_eq!(event.sample_count, 1);
}

#[test]
fn normalized_window_title_change_starts_a_new_event() {
    let mut merger = merger();
    merger.ingest(sample(0, "notepad.exe", "Project A", "same text"));

    let event = started(
        merger.ingest(sample(
            31,
            "notepad.exe",
            "Project B",
            "completely different text",
        )),
        SplitReason::WindowTitleChange,
    );

    assert_eq!(event.latest.window_title, "Project B");
}

#[test]
fn cosmetic_window_title_case_and_whitespace_do_not_split() {
    let mut merger = merger();
    merger.ingest(sample(0, "notepad.exe", "  PROJECT\tA ", "same text"));

    let event = merged(merger.ingest(sample(1, "notepad.exe", "project a", "same text")));

    assert_eq!(event.sample_count, 2);
}

#[test]
fn a_thirty_one_second_gap_starts_a_new_event() {
    let mut merger = merger();
    merger.ingest(sample(0, "notepad.exe", "notes", "same text"));

    let event = started(
        merger.ingest(sample(
            31,
            "notepad.exe",
            "notes",
            "completely different text",
        )),
        SplitReason::IdleGap,
    );

    assert_eq!(event.started_at, at(31));
}

#[test]
fn exactly_thirty_seconds_still_merges() {
    let mut merger = merger();
    merger.ingest(sample(0, "notepad.exe", "notes", "same text"));

    let event = merged(merger.ingest(sample(30, "notepad.exe", "notes", "same text")));

    assert_eq!(event.sample_count, 2);
}

#[test]
fn unrelated_ocr_starts_a_text_hash_event() {
    let mut merger = merger();
    merger.ingest(sample(
        0,
        "notepad.exe",
        "notes",
        "one two three four five six seven",
    ));

    let event = started(
        merger.ingest(sample(
            1,
            "notepad.exe",
            "notes",
            "alpha beta gamma delta epsilon zeta eta",
        )),
        SplitReason::TextHashChange,
    );

    assert_eq!(event.sample_count, 1);
}

#[test]
fn exact_hash_match_merges_and_updates_latest_fields() {
    let mut first = sample(0, "notepad.exe", "notes", "same text");
    first.browser_url = Some("https://example.test/first".to_owned());
    let mut second = sample(1, "notepad.exe", "notes", " SAME\tTEXT ");
    second.readable_text = "latest readable text".to_owned();
    second.browser_url = Some("https://example.test/latest".to_owned());
    let expected_hash = TextIdentity::from_ocr("same text").exact_hash;
    let mut merger = merger();
    merger.ingest(first);

    let event = merged(merger.ingest(second.clone()));

    assert_eq!(event.started_at, at(0));
    assert_eq!(event.ended_at, at(1));
    assert_eq!(event.latest, second);
    assert_eq!(event.sample_count, 2);
    assert_eq!(event.hash_counts.get(&expected_hash), Some(&2));
}

#[test]
fn overlapping_scrolling_text_reuses_the_stable_merge_hash() {
    let first_text = "one two three four five six seven eight nine ten";
    let second_text = "three four five six seven eight nine ten eleven twelve";
    let first_hash = TextIdentity::from_ocr(first_text).exact_hash;
    let second_hash = TextIdentity::from_ocr(second_text).exact_hash;
    let mut merger = merger();
    merger.ingest(sample(0, "notepad.exe", "notes", first_text));

    let event = merged(merger.ingest(sample(1, "notepad.exe", "notes", second_text)));

    assert_eq!(event.started_at, at(0));
    assert_eq!(event.merge_hash, first_hash.clone());
    assert_eq!(event.latest.ocr_text, second_text);
    assert_eq!(event.sample_count, 2);
    assert_eq!(event.hash_counts.get(&first_hash), Some(&1));
    assert_eq!(event.hash_counts.get(&second_hash), Some(&1));
}

#[test]
fn exactly_thirty_five_percent_overlap_merges() {
    let first_text = "w01 w02 w03 w04 w05 w06 w07 w08 w09 w10 w11 w12 w13 w14 w15 w16 w17";
    let second_text = "w07 w08 w09 w10 w11 w12 w13 w14 w15 w16 w17 w18 w19 w20 w21 w22 w23 w24";
    let mut merger = merger();
    merger.ingest(sample(0, "notepad.exe", "notes", first_text));

    let event = merged(merger.ingest(sample(1, "notepad.exe", "notes", second_text)));

    assert_eq!(event.sample_count, 2);
}

#[test]
fn a_previously_seen_exact_hash_merges_after_the_viewport_moves_away() {
    let samples = [
        "w01 w02 w03 w04 w05 w06 w07 w08 w09 w10",
        "w03 w04 w05 w06 w07 w08 w09 w10 w11 w12",
        "w05 w06 w07 w08 w09 w10 w11 w12 w13 w14",
        "w07 w08 w09 w10 w11 w12 w13 w14 w15 w16",
    ];
    let revisited_hash = TextIdentity::from_ocr(samples[1]).exact_hash;
    let mut merger = merger();
    for (second, text) in samples.into_iter().enumerate() {
        merger.ingest(sample(second as i64, "notepad.exe", "notes", text));
    }

    let event = merged(merger.ingest(sample(4, "notepad.exe", "notes", samples[1])));

    assert_eq!(event.sample_count, 5);
    assert_eq!(event.hash_counts.get(&revisited_hash), Some(&2));
}

#[test]
fn an_idle_window_sampled_at_max_backoff_keeps_merging() {
    // At max backoff the cadence sleeps MAX_CADENCE_INTERVAL_SECONDS, and real
    // capture plus OCR overhead pushes the delta between consecutive samples
    // past it. An idle gap equal to the slowest cadence therefore split on
    // every idle sample, turning a quiet window into a run of one-sample
    // events and collapsing the merge ratio Goal 1 measures. The production
    // idle gap must stay strictly above the slowest cadence.
    let mut merger = Merger::new(MergeConfig {
        idle_gap: Duration::seconds(screenpipe_memory::MAX_CADENCE_INTERVAL_SECONDS * 2),
        scroll_overlap: 0.35,
    });
    let unchanged = "unchanged idle window contents";
    let mut second = 0;
    started(
        merger.ingest(sample(second, "chrome.exe", "Idle", unchanged)),
        SplitReason::Initial,
    );

    // Ten consecutive max-backoff samples, each a second of overhead late.
    let mut event = None;
    for _ in 0..10 {
        second += screenpipe_memory::MAX_CADENCE_INTERVAL_SECONDS + 1;
        event = Some(merged(merger.ingest(sample(
            second,
            "chrome.exe",
            "Idle",
            unchanged,
        ))));
    }

    let event = event.expect("idle window must have merged");
    assert_eq!(
        event.sample_count, 11,
        "an unchanged idle window must merge into one event, not fragment"
    );
    assert_eq!(event.started_at, at(0));
}
