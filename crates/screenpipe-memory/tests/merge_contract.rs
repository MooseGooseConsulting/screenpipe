use chrono::{Duration, TimeZone, Utc};
use screenpipe_memory::{
    CadenceInput, CadenceRecord, CaptureGap, CaptureGapSummary, EnvelopeMergeDecision,
    EventEnvelope, EventKind, MergeConfig, MergeDecision, MergeDecisionKind, Merger,
    ObservationEnvelope, ObservationSample, SplitReason, TextIdentity,
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

fn idle_cadence() -> CadenceRecord {
    CadenceRecord::from_input(CadenceInput {
        input_idle: Duration::zero(),
        frame_stable_for: Duration::zero(),
        foreground_changed: false,
        frame_changed: false,
    })
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
        kind: EventKind::Screen,
        idle_gap: Duration::seconds(TEST_IDLE_GAP_SECONDS),
        scroll_overlap: 0.35,
    })
}

fn started(decision: MergeDecision, expected_reason: SplitReason) -> screenpipe_memory::OpenEvent {
    match decision {
        MergeDecision::Start { reason, event } => {
            assert_eq!(reason, expected_reason);
            // The reason carried by the decision and the reason stored on the
            // event are two separate writes, and only the first was ever
            // asserted - so `start_reason` could be hardcoded to `Initial` for
            // every split and the whole suite stayed green. It is persisted
            // into `merge_meta`, so a wrong value is durable.
            assert_eq!(
                event.start_reason, expected_reason,
                "the event's stored start_reason must match the decision's reason"
            );
            assert_eq!(
                event.last_decision,
                MergeDecisionKind::Start,
                "a start decision must leave last_decision at Start"
            );
            assert_eq!(
                event.merge_contract_version,
                screenpipe_memory::MERGE_CONTRACT_VERSION
            );
            event
        }
        MergeDecision::Merge { .. } => panic!("expected a start decision"),
    }
}

fn merged(decision: MergeDecision) -> screenpipe_memory::OpenEvent {
    match decision {
        MergeDecision::Merge { event } => {
            assert_eq!(
                event.last_decision,
                MergeDecisionKind::Merge,
                "a merge decision must leave last_decision at Merge"
            );
            event
        }
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
    assert_eq!(event.hash_counts.counts().get(&expected_hash), Some(&1));
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
fn a_title_change_splits_only_when_the_content_changed_too() {
    // A title change with DIFFERENT content is a real boundary and keeps its
    // own reason code, because "the title changed" describes what happened
    // better than "the text changed" does.
    let mut merger = merger();
    merger.ingest(sample(0, "notepad.exe", "Project A", "same text"));

    let event = started(
        merger.ingest(sample(
            2,
            "notepad.exe",
            "Project B",
            "completely different text",
        )),
        SplitReason::WindowTitleChange,
    );

    assert_eq!(event.latest.window_title, "Project B");
}

#[test]
fn a_title_change_over_unchanged_content_does_not_split() {
    // THE FIX THIS CONTRACT EXISTS FOR.
    //
    // On 784 real events from this machine, 690 - 88% - started because of a
    // title change, averaging 3.2 samples. Events that started from an app
    // change, an unambiguous real change of activity, averaged 24.1. The
    // activity was not that fragmented; the segmentation was, because a title
    // is a presentation surface and apps animate spinners, unsaved markers and
    // notification counts in it.
    //
    // Identical screen text, wholly different title. This must merge. The
    // braille glyphs are the real terminal spinner that caused it here.
    let mut merger = merger();
    let screen = "the same document, unchanged, while the title bar animates";
    merger.ingest(sample(0, "code.exe", "\u{2800} building - project", screen));

    let decision = merger.ingest(sample(2, "code.exe", "\u{2803} building - project", screen));

    let event = merged(decision);
    assert_eq!(
        event.sample_count, 2,
        "an animated title glyph must not open a new event"
    );
    assert_eq!(event.start_reason, SplitReason::Initial);
}

#[test]
fn a_title_change_over_scrolling_content_does_not_split() {
    // The same rule under the scroll-overlap path rather than the exact-hash
    // path: a document being read while a notification count ticks in the
    // title is one activity, not four.
    let mut merger = merger();
    let head = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron";
    merger.ingest(sample(0, "chrome.exe", "(1) Inbox - Mail", head));

    let decision = merger.ingest(sample(
        2,
        "chrome.exe",
        "(7) Inbox - Mail",
        &format!("{head} pi rho sigma"),
    ));

    let event = merged(decision);
    assert_eq!(
        event.sample_count, 2,
        "a notification count in the title must not open a new event"
    );
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
    assert_eq!(event.hash_counts.counts().get(&expected_hash), Some(&2));
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
    assert_eq!(event.hash_counts.counts().get(&first_hash), Some(&1));
    assert_eq!(event.hash_counts.counts().get(&second_hash), Some(&1));
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
fn capture_gaps_accumulate_per_kind_across_a_merge() {
    // Every other test in this file passes `CaptureGapSummary::default()`, so
    // `record` never ran and `add` never ran with a nonzero operand: the
    // three-way counter dispatch and the three saturating adds were entirely
    // unprotected here. The three counts are deliberately pairwise distinct -
    // with equal counts, swapping two counters is undetectable.
    let mut first = CaptureGapSummary::default();
    first.record(CaptureGap::CaptureUnavailable);
    first.record(CaptureGap::EmptyOcr);
    first.record(CaptureGap::EmptyOcr);
    assert_eq!(
        first,
        CaptureGapSummary {
            capture_unavailable: 1,
            ocr_unavailable: 0,
            empty_ocr: 2,
            desktop_locked: 0,
        },
        "record must credit each gap kind to its own counter"
    );

    let mut second = CaptureGapSummary::default();
    for _ in 0..3 {
        second.record(CaptureGap::OcrUnavailable);
    }

    let mut merger = merger();
    merger.ingest_with_metadata(
        sample(0, "notepad.exe", "notes", "same text"),
        idle_cadence(),
        first,
    );

    let event = merged(merger.ingest_with_metadata(
        sample(1, "notepad.exe", "notes", "same text"),
        idle_cadence(),
        second,
    ));

    assert_eq!(
        event.capture_gaps,
        CaptureGapSummary {
            capture_unavailable: 1,
            ocr_unavailable: 3,
            empty_ocr: 2,
            desktop_locked: 0,
        },
        "a merge must sum each gap counter with its own kind"
    );
}

#[test]
fn just_under_thirty_five_percent_overlap_starts_a_text_hash_event() {
    // The companion of `exactly_thirty_five_percent_overlap_merges`. Without a
    // case that sits just BELOW the threshold, the comparison can be scaled
    // arbitrarily far down (0.35 -> 0.00035) and every merge test still passes,
    // because the only "must split" case had overlap of exactly 0.0. That would
    // make scroll detection mean "merge on any single shared five-gram".
    //
    // 17 words -> 13 grams; 22 words -> 18 grams; grams starting w06..w13 are
    // shared -> 8. Union = 13 + 18 - 8 = 23, so overlap = 8/23 = 0.3478 < 0.35.
    // The second text is also a hash never seen before, so the `hash_counts`
    // short-circuit cannot mask the threshold.
    let first_text = "w01 w02 w03 w04 w05 w06 w07 w08 w09 w10 w11 w12 w13 w14 w15 w16 w17";
    let second_text =
        "w06 w07 w08 w09 w10 w11 w12 w13 w14 w15 w16 w17 w18 w19 w20 w21 w22 w23 w24 w25 w26 w27";
    let mut merger = merger();
    merger.ingest(sample(0, "notepad.exe", "notes", first_text));

    let event = started(
        merger.ingest(sample(1, "notepad.exe", "notes", second_text)),
        SplitReason::TextHashChange,
    );

    assert_eq!(event.sample_count, 1);
    assert_eq!(
        event.merge_hash,
        TextIdentity::from_ocr(second_text).exact_hash
    );
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
    assert_eq!(event.hash_counts.counts().get(&revisited_hash), Some(&2));
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
        kind: EventKind::Screen,
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

#[test]
fn a_long_lived_event_keeps_its_hash_ledger_bounded() {
    // Unbounded, this was the worst defect on the merge path. A window whose
    // text changes while staying above the overlap threshold - a log tail, a
    // build, a subtitled video, any UI with a clock - merges on every sample
    // and contributed a new 64-character key each time. At the 2s cadence that
    // is ~43,200 entries in 24 hours, cloned twice per tick and re-serialized
    // in full into the merge_meta JSONB on EVERY write: durable write volume
    // quadratic in sample count, for a single event.
    //
    // The fixture must actually take the merge path, so each sample shares
    // most of its five-grams with the previous one and only the tail differs.
    let mut merger = Merger::new(MergeConfig {
        kind: EventKind::Screen,
        idle_gap: Duration::seconds(120),
        scroll_overlap: 0.35,
    });
    let shared = "the quick brown fox jumps over the lazy dog while the tape keeps rolling along";

    let sample_total = screenpipe_memory::MAX_TRACKED_HASHES * 4;
    // The opening sample is always a Start; only the rest exercise merging.
    merger.ingest(sample(
        0,
        "chrome.exe",
        "Live log",
        &format!("{shared} counter 0"),
    ));
    let mut event = None;
    for index in 1..sample_total {
        let text = format!("{shared} counter {index}");
        event = Some(merged(merger.ingest(sample(
            index as i64,
            "chrome.exe",
            "Live log",
            &text,
        ))));
    }

    let event = event.expect("the overlapping samples must have merged");
    assert_eq!(
        event.sample_count as usize, sample_total,
        "the fixture stopped merging, so it no longer exercises ledger growth"
    );
    assert!(
        event.hash_counts.counts().len() <= screenpipe_memory::MAX_TRACKED_HASHES,
        "hash ledger grew to {} entries, past the {} bound",
        event.hash_counts.counts().len(),
        screenpipe_memory::MAX_TRACKED_HASHES
    );
    // Eviction must be recorded. Without it a truncated ledger is
    // indistinguishable from a short event to anything reading merge_meta.
    assert_eq!(
        event.hash_counts.evicted() as usize,
        sample_total - screenpipe_memory::MAX_TRACKED_HASHES,
        "every hash pushed out of the window must be counted"
    );
}

#[test]
fn an_evicted_hash_no_longer_forces_a_merge() {
    // `contains` is what lets a previously-seen screen merge back in. With an
    // unbounded ledger an event that had seen N screens merged any of those N
    // back unconditionally forever, so SplitReason::TextHashChange became
    // progressively unreachable as the event aged. Bounding the ledger is what
    // restores it, and this is the assertion that proves the bound has that
    // effect rather than merely capping memory.
    let mut merger = Merger::new(MergeConfig {
        kind: EventKind::Screen,
        idle_gap: Duration::seconds(120),
        scroll_overlap: 0.35,
    });
    let shared = "the quick brown fox jumps over the lazy dog while the tape keeps rolling along";
    let first_text = format!("{shared} counter 0");

    merger.ingest(sample(0, "chrome.exe", "Live log", &first_text));
    // Push the opening hash out of the window.
    for index in 1..=screenpipe_memory::MAX_TRACKED_HASHES {
        let text = format!("{shared} counter {index}");
        merger.ingest(sample(index as i64, "chrome.exe", "Live log", &text));
    }

    // Re-present the very first screen. It is byte-identical to a hash this
    // event has seen, but it is no longer tracked, so it must be treated as
    // new text rather than silently absorbed.
    let decision = merger.ingest(sample(
        (screenpipe_memory::MAX_TRACKED_HASHES + 1) as i64,
        "chrome.exe",
        "Live log",
        &first_text,
    ));
    assert!(
        matches!(decision, MergeDecision::Merge { .. }),
        "this fixture still overlaps, so it should merge on overlap alone - \
         if it split, the fixture no longer isolates the ledger"
    );
    assert!(
        !event_of(&decision).hash_counts.contains(&{
            let identity = TextIdentity::from_ocr(&format!("{shared} counter 1"));
            identity.exact_hash
        }),
        "the oldest evicted hash must not still be tracked"
    );
}

fn event_of(decision: &MergeDecision) -> &screenpipe_memory::OpenEvent {
    match decision {
        MergeDecision::Start { event, .. } | MergeDecision::Merge { event } => event,
    }
}

#[test]
fn an_unchanging_window_is_split_once_it_outlives_the_duration_ceiling() {
    // Every other split test in this file describes a CHANGE - of app, of
    // idleness, of text. An event whose app never changes, whose window never
    // goes idle and whose text keeps matching answers none of them and merges
    // for the length of the run. A dashboard, a video player, a clock, a
    // terminal tailing a log all behave exactly like this, so an unattended
    // week produced one row spanning the week: a week-old screen in `ocr_text`,
    // a duration that describes nothing, and a `sample_count` no reader can act
    // on.
    //
    // The idle gap here is deliberately enormous, so the only thing that can
    // end this event is the ceiling.
    let ceiling = screenpipe_memory::MAX_EVENT_DURATION_SECONDS;
    let mut merger = Merger::new(MergeConfig {
        kind: EventKind::Screen,
        idle_gap: Duration::seconds(ceiling * 10),
        scroll_overlap: 0.35,
    });
    let unchanged = "a dashboard nobody is looking at";
    merger.ingest(sample(0, "chrome.exe", "Dashboard", unchanged));

    // One second inside the ceiling still merges: the bound must not fire early
    // and fragment ordinary long sessions.
    let inside = merged(merger.ingest(sample(ceiling - 1, "chrome.exe", "Dashboard", unchanged)));
    assert_eq!(inside.sample_count, 2);
    assert_eq!(inside.started_at, at(0));

    let event = started(
        merger.ingest(sample(ceiling, "chrome.exe", "Dashboard", unchanged)),
        SplitReason::MaxDuration,
    );

    assert_eq!(event.started_at, at(ceiling));
    assert_eq!(event.sample_count, 1);
    assert!(
        event.start_reason.is_forced(),
        "a ceiling boundary must be distinguishable from an observed change"
    );
}

#[test]
fn an_unchanging_window_is_split_once_it_outgrows_the_sample_ceiling() {
    // The duration ceiling is measured on the wall clock, and the wall clock is
    // not something an unattended recorder can rely on: a suspend/resume, an
    // NTP correction, or a VM restored from a snapshot all leave it unreachable
    // while samples keep arriving. Every sample here shares one timestamp, so
    // the duration is permanently zero and only the sample ceiling can end the
    // event - which is exactly the frozen-clock case.
    let ceiling = screenpipe_memory::MAX_EVENT_SAMPLES;
    let mut merger = Merger::new(MergeConfig {
        kind: EventKind::Screen,
        idle_gap: Duration::seconds(30),
        scroll_overlap: 0.35,
    });
    let unchanged = "the same screen, and a clock that stopped";

    merger.ingest(sample(0, "code.exe", "Frozen", unchanged));
    for index in 1..ceiling {
        let event = merged(merger.ingest(sample(0, "code.exe", "Frozen", unchanged)));
        assert_eq!(
            event.sample_count,
            index + 1,
            "the fixture stopped merging, so it no longer reaches the ceiling"
        );
    }

    let event = started(
        merger.ingest(sample(0, "code.exe", "Frozen", unchanged)),
        SplitReason::MaxSamples,
    );

    assert_eq!(event.sample_count, 1);
    assert!(event.start_reason.is_forced());
}

#[test]
fn a_forced_boundary_never_outranks_an_observed_one() {
    // The ceilings are consulted last on purpose. If they ran first, an event
    // that reached the ceiling in the same sample that changed app would be
    // recorded as `max_duration` - blaming the recorder's own bound for a real
    // change of activity, and hiding the boundary a reader actually wants.
    let ceiling = screenpipe_memory::MAX_EVENT_DURATION_SECONDS;
    let mut merger = Merger::new(MergeConfig {
        kind: EventKind::Screen,
        idle_gap: Duration::seconds(ceiling * 10),
        scroll_overlap: 0.35,
    });
    merger.ingest(sample(0, "chrome.exe", "Dashboard", "unchanged screen"));

    let event = started(
        merger.ingest(sample(ceiling, "notepad.exe", "notes", "something else")),
        SplitReason::AppChange,
    );

    assert!(!event.start_reason.is_forced());
}

#[test]
fn every_split_reason_has_its_own_durable_code() {
    // These codes are written into `merge_meta.start_reason` and are the only
    // thing a reader has to tell one boundary from another. Two reasons sharing
    // a code is silent and durable.
    let codes = [
        SplitReason::Initial,
        SplitReason::AppChange,
        SplitReason::WindowTitleChange,
        SplitReason::IdleGap,
        SplitReason::TimestampRegression,
        SplitReason::TextHashChange,
        SplitReason::MaxDuration,
        SplitReason::MaxSamples,
    ]
    .map(SplitReason::as_code);

    let mut unique = codes.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), codes.len(), "duplicate split reason code");
    assert!(codes.iter().all(|code| !code.is_empty()));
}

// --- The clipboard contract ------------------------------------------------
//
// Same merger, same identifiers, same `merge_meta`; a different content test.
// The tests below pin both halves of that: what the clipboard rule DOES merge,
// and - the part that would otherwise rot silently - what it must not, because
// the screen rule would.

/// A clipboard capture as the channel actually produces one: text, and no app.
///
/// The empty app key is load-bearing. A clipboard capture is not attributed to
/// an application, so nothing here can accidentally exercise the app-change
/// split and pass for a content decision.
fn clipboard_sample(second: i64, text: &str) -> ObservationSample {
    ObservationSample {
        captured_at: at(second),
        app_key: String::new(),
        app_title: "Clipboard".to_owned(),
        window_title: String::new(),
        ocr_text: text.to_owned(),
        readable_text: text.to_owned(),
        browser_url: None,
    }
}

fn clipboard_merger() -> Merger {
    Merger::new(MergeConfig {
        kind: EventKind::Clipboard,
        idle_gap: Duration::seconds(TEST_IDLE_GAP_SECONDS),
        // Deliberately the production screen value. If the clipboard rule ever
        // consults it, these tests must fail rather than quietly agree.
        scroll_overlap: 0.35,
    })
}

#[test]
fn a_clipboard_event_records_its_kind_so_the_writer_cannot_mislabel_it() {
    let event = started(
        clipboard_merger().ingest(clipboard_sample(0, "the text that was copied")),
        SplitReason::Initial,
    );

    assert_eq!(event.kind, EventKind::Clipboard);
    assert_eq!(event.kind.as_code(), "clipboard");
    assert_eq!(EventKind::Screen.as_code(), "screen");
    assert_eq!(
        event.merge_contract_version,
        screenpipe_memory::MERGE_CONTRACT_VERSION
    );
}

#[test]
fn recopying_identical_text_merges_rather_than_opening_a_second_event() {
    // The clipboard sequence number changes on every copy, including a copy of
    // something already on the clipboard. Without this, hitting Ctrl-C twice -
    // or any application that re-asserts its own clipboard content - writes a
    // second row saying exactly what the first one said.
    let mut merger = clipboard_merger();
    started(
        merger.ingest(clipboard_sample(0, "one two three four five")),
        SplitReason::Initial,
    );

    let event = merged(merger.ingest(clipboard_sample(5, "one two three four five")));

    assert_eq!(event.sample_count, 2);
    assert_eq!(event.started_at, at(0));
    assert_eq!(event.ended_at, at(5));
}

#[test]
fn clipboard_text_identity_reuses_the_ocr_normalization() {
    // Same pipeline as the screen path: NFKC, case folding, whitespace
    // collapse. A re-copy that differs only in case or spacing is the same
    // text, and the hash the event carries is the one that pipeline produces.
    let mut merger = clipboard_merger();
    let started_event = started(
        merger.ingest(clipboard_sample(0, "Deploy The Release Notes")),
        SplitReason::Initial,
    );
    assert_eq!(
        started_event.latest_exact_ocr_hash,
        TextIdentity::from_ocr("Deploy The Release Notes").exact_hash
    );

    let event = merged(merger.ingest(clipboard_sample(2, "  deploy the\trelease   notes ")));

    assert_eq!(event.sample_count, 2);
}

#[test]
fn different_clipboard_text_closes_the_open_event_and_opens_a_new_one() {
    let mut merger = clipboard_merger();
    started(
        merger.ingest(clipboard_sample(0, "the first thing copied")),
        SplitReason::Initial,
    );

    let event = started(
        merger.ingest(clipboard_sample(2, "an entirely different thing")),
        SplitReason::TextHashChange,
    );

    assert_eq!(event.started_at, at(2));
    assert_eq!(event.sample_count, 1);
}

#[test]
fn overlapping_clipboard_text_still_splits_because_a_copy_is_not_a_scroll() {
    // The negative control for the whole kind. These two strings share every
    // five-gram but the last, so the SCREEN rule would merge them on scroll
    // overlap - they are the same material being scrolled past. Two clipboard
    // entries are not: they are two separate copies of two different things,
    // and merging them would silently lose the first one's text, since an
    // event keeps only its latest sample.
    let first = "the quick brown fox jumps over the lazy dog";
    let second = "the quick brown fox jumps over the lazy cat";
    let overlap = {
        let left = TextIdentity::from_ocr(first);
        let right = TextIdentity::from_ocr(second);
        screenpipe_memory::jaccard_overlap(&left.five_grams, &right.five_grams)
    };
    assert!(
        overlap >= 0.35,
        "the fixture no longer overlaps enough to be merged by the screen rule \
         ({overlap}), so it proves nothing about the clipboard rule"
    );

    let mut clipboard = clipboard_merger();
    started(
        clipboard.ingest(clipboard_sample(0, first)),
        SplitReason::Initial,
    );
    let split = started(
        clipboard.ingest(clipboard_sample(2, second)),
        SplitReason::TextHashChange,
    );
    assert_eq!(split.latest.ocr_text, second);

    // The same two texts on the screen path, to prove the difference is the
    // rule and not the fixture.
    let mut screen = merger();
    screen.ingest(sample(0, "notepad.exe", "notes", first));
    merged(screen.ingest(sample(2, "notepad.exe", "notes", second)));
}

#[test]
fn a_clipboard_event_is_closed_by_the_shared_idle_gap() {
    // The idle gap is not re-implemented for the clipboard: it is the same
    // threshold, applied to the same delta, so a copy made after a long silence
    // starts a new event even when it copies exactly what was there before.
    let mut merger = clipboard_merger();
    let text = "a link worth pasting twice";
    started(
        merger.ingest(clipboard_sample(0, text)),
        SplitReason::Initial,
    );

    // Exactly at the threshold still merges - `> idle_gap` splits, and the
    // boundary is shared with the screen contract above.
    let at_threshold = merged(merger.ingest(clipboard_sample(TEST_IDLE_GAP_SECONDS, text)));
    assert_eq!(at_threshold.sample_count, 2);

    let event = started(
        merger.ingest(clipboard_sample(TEST_IDLE_GAP_SECONDS * 2 + 1, text)),
        SplitReason::IdleGap,
    );

    assert_eq!(event.sample_count, 1);
    assert_eq!(event.started_at, at(TEST_IDLE_GAP_SECONDS * 2 + 1));
}

#[test]
fn a_clipboard_timestamp_regression_starts_a_new_valid_event_before_its_content_is_checked() {
    let mut merger = clipboard_merger();
    let copied = "synthetic clipboard regression fixture";
    started(
        merger.ingest(clipboard_sample(10, copied)),
        SplitReason::Initial,
    );

    let MergeDecision::Start { reason, event } = merger.ingest(clipboard_sample(5, copied)) else {
        panic!("timestamp regression must start a distinct clipboard event");
    };

    assert_eq!(reason.as_code(), "timestamp_regression");
    assert_eq!(event.started_at, at(5));
    assert_eq!(event.ended_at, at(5));
    assert_eq!(event.sample_count, 1);
}

// --- The audio contract ----------------------------------------------------
//
// The audio channel shares the discrete rule with the clipboard, but for a
// different reason and with a different consequence, so it is pinned
// separately: for a clipboard the rule dedupes a double Ctrl-C, and for audio
// it is what stands between a quiet room and a hundred identical rows of
// whisper's favourite hallucination.

/// One transcribed utterance, as the audio channel produces it.
///
/// The app key is NOT empty here, unlike the clipboard's. An audio event has a
/// real source worth naming - which of the two channels heard it - and naming
/// it is what gives the event a title a person can recognise.
fn audio_sample(second: i64, transcript: &str) -> ObservationEnvelope {
    audio_sample_lasting(second, 0, transcript)
}

/// An utterance that ran for `seconds` after it started.
fn audio_sample_lasting(second: i64, seconds: i64, transcript: &str) -> ObservationEnvelope {
    let sample = ObservationSample {
        captured_at: at(second),
        app_key: "audio:loopback".to_owned(),
        app_title: "System Audio".to_owned(),
        window_title: String::new(),
        ocr_text: transcript.to_owned(),
        readable_text: transcript.to_owned(),
        browser_url: None,
    };
    ObservationEnvelope::spanning(
        sample,
        at(second + seconds),
        screenpipe_memory::AudioMeta {
            channel: "system_audio",
            device_category: "communications",
            engine: "whisper-rs",
            model: "ggml-base.en".to_owned(),
            vad_engine: "webrtc-vad",
            vad_aggressiveness: "quality",
            language: Some("en".to_owned()),
            avg_no_speech_permille: Some(30),
            closed_by: "silence",
        },
    )
}

trait TestIngestEnvelope {
    fn ingest_envelope(&mut self, observation: ObservationEnvelope) -> EnvelopeMergeDecision;
}

impl TestIngestEnvelope for Merger {
    fn ingest_envelope(&mut self, observation: ObservationEnvelope) -> EnvelopeMergeDecision {
        self.ingest_envelope_with_metadata(
            observation,
            idle_cadence(),
            CaptureGapSummary::default(),
        )
    }
}

fn started_envelope(
    decision: EnvelopeMergeDecision,
    expected_reason: SplitReason,
) -> EventEnvelope {
    match decision {
        EnvelopeMergeDecision::Start { reason, event } => {
            assert_eq!(reason, expected_reason);
            assert_eq!(event.event().start_reason, expected_reason);
            event
        }
        EnvelopeMergeDecision::Merge { .. } => panic!("expected a start decision"),
    }
}

fn started_audio(
    decision: EnvelopeMergeDecision,
    expected_reason: SplitReason,
) -> screenpipe_memory::OpenEvent {
    started_envelope(decision, expected_reason).event().clone()
}

fn merged_audio(decision: EnvelopeMergeDecision) -> screenpipe_memory::OpenEvent {
    match decision {
        EnvelopeMergeDecision::Merge { event } => event.event().clone(),
        EnvelopeMergeDecision::Start { .. } => panic!("expected a merge decision"),
    }
}

fn audio_merger() -> Merger {
    Merger::new(MergeConfig {
        kind: EventKind::Audio,
        idle_gap: Duration::seconds(TEST_IDLE_GAP_SECONDS),
        scroll_overlap: 0.35,
    })
}

#[test]
fn an_audio_event_records_its_kind_so_the_writer_cannot_mislabel_it() {
    let mut merger = audio_merger();

    let event = started_audio(
        merger.ingest_envelope(audio_sample(0, "the deploy finished about ten minutes ago")),
        SplitReason::Initial,
    );

    assert_eq!(event.kind, EventKind::Audio);
    assert_eq!(event.kind.as_code(), "audio");
}

#[test]
fn every_distinct_utterance_becomes_its_own_event() {
    // Load-bearing, not incidental. The writer persists `latest.ocr_text` -
    // it REPLACES what the row held - so a rule that merged two different
    // transcripts would keep the second and silently lose the first. Splitting
    // is what makes every utterance durable and searchable.
    let mut merger = audio_merger();
    started_audio(
        merger.ingest_envelope(audio_sample(0, "did you see the review comments")),
        SplitReason::Initial,
    );

    let second = started_audio(
        merger.ingest_envelope(audio_sample(3, "yes, two of them were real")),
        SplitReason::TextHashChange,
    );

    assert_eq!(second.sample_count, 1);
    assert_eq!(second.latest.ocr_text, "yes, two of them were real");
}

#[test]
fn identical_transcripts_in_distinct_utterance_windows_start_distinct_events() {
    // The transcript is deliberately identical. These windows cannot belong
    // to one VAD utterance: the first is closed before the second begins. A
    // content-only identity would merge them and erase the first occurrence as
    // a separately searchable event.
    let mut merger = audio_merger();
    let first = started_audio(
        merger.ingest_envelope(audio_sample_lasting(0, 1, "Thank you.")),
        SplitReason::Initial,
    );

    let second = started_audio(
        merger.ingest_envelope(audio_sample_lasting(2, 1, "Thank you.")),
        SplitReason::TextHashChange,
    );

    assert_eq!(first.started_at, at(0));
    assert_eq!(first.ended_at, at(1));
    assert_eq!(second.started_at, at(2));
    assert_eq!(second.ended_at, at(3));
    assert_eq!(second.sample_count, 1);
    assert_ne!(first.merge_hash, second.merge_hash);
}

#[test]
fn identical_chunks_from_one_utterance_window_are_deduplicated() {
    // Guard the other side of the boundary: an upstream retry can hand the
    // same closed VAD utterance to the merger twice. The shared start/end span
    // is its identity, so this is one durable occurrence with two samples.
    let mut merger = audio_merger();
    let chunk = audio_sample_lasting(0, 1, "Thank you.");
    let first = started_audio(merger.ingest_envelope(chunk.clone()), SplitReason::Initial);

    let duplicate = merged_audio(merger.ingest_envelope(chunk));

    assert_eq!(duplicate.started_at, at(0));
    assert_eq!(duplicate.ended_at, at(1));
    assert_eq!(duplicate.sample_count, 2);
    assert_eq!(duplicate.merge_hash, first.merge_hash);
}

#[test]
fn a_silence_longer_than_the_idle_gap_starts_a_new_audio_event() {
    // Every separately closed VAD window is already a new event. The idle-gap
    // reason still has priority when the silence itself crosses the shared
    // threshold, so readers can distinguish an ordinary new utterance from a
    // long period with no speech.
    let mut merger = audio_merger();
    let line = "same thing said twice, an hour apart";
    started_audio(
        merger.ingest_envelope(audio_sample(0, line)),
        SplitReason::Initial,
    );

    let at_threshold = started_audio(
        merger.ingest_envelope(audio_sample(TEST_IDLE_GAP_SECONDS, line)),
        SplitReason::TextHashChange,
    );
    assert_eq!(at_threshold.sample_count, 1);

    let event = started_audio(
        merger.ingest_envelope(audio_sample(TEST_IDLE_GAP_SECONDS * 2 + 1, line)),
        SplitReason::IdleGap,
    );

    assert_eq!(event.sample_count, 1);
}

#[test]
fn the_audio_metadata_rides_on_the_event_for_the_writer_to_persist() {
    // None of it has a column: which model produced the transcript, and whether
    // that model thought it was hearing speech at all, is the difference
    // between a row worth reading and one invented over room tone.
    let mut merger = audio_merger();

    let event = started_envelope(
        merger.ingest_envelope(audio_sample(0, "an utterance with metadata")),
        SplitReason::Initial,
    );

    let meta = event.audio().expect("audio metadata");
    assert_eq!(meta.channel, "system_audio");
    assert_eq!(meta.device_category, "communications");
    assert_eq!(meta.model, "ggml-base.en");
    assert_eq!(meta.avg_no_speech_permille, Some(30));
    assert_eq!(meta.closed_by, "silence");
}

#[test]
fn a_screen_sample_carries_no_audio_metadata_at_all() {
    // Absent rather than empty: a screen row asserting anything about an audio
    // channel would have to be ignored by every reader of merge_meta.
    let mut merger = merger();

    let event = started_envelope(
        merger.ingest_envelope(sample(0, "notepad.exe", "notes", "ordinary screen text").into()),
        SplitReason::Initial,
    );

    assert!(event.audio().is_none());
}

#[test]
fn the_silence_is_measured_from_the_end_of_the_previous_utterance() {
    // The defect this pins: the merger measures `next.captured_at -
    // open.ended_at`, and an utterance's timestamp is when the speech STARTED.
    // Without the observation's end, a long sentence followed by a short pause
    // reads as one long gap - the sentence's own length folded into the
    // silence after it - and splits at a threshold the silence never crossed.
    let mut merger = audio_merger();
    let long_utterance = TEST_IDLE_GAP_SECONDS - 5;
    started_audio(
        merger.ingest_envelope(audio_sample_lasting(0, long_utterance, "a long sentence")),
        SplitReason::Initial,
    );

    // Speech resumes 10s after the previous turn ENDED. Total distance from its
    // start is long_utterance + 10, comfortably over the gap - which is exactly
    // what would have split it.
    let next_start = long_utterance + 10;
    assert!(
        next_start > TEST_IDLE_GAP_SECONDS,
        "the fixture must be one the old arithmetic would have split"
    );
    let event = started_audio(
        merger.ingest_envelope(audio_sample_lasting(next_start, 2, "a different sentence")),
        SplitReason::TextHashChange,
    );

    assert_eq!(
        event.start_reason,
        SplitReason::TextHashChange,
        "ten seconds of silence must not read as an idle gap"
    );
}

#[test]
fn an_audio_events_window_covers_the_speech_it_holds() {
    // `ended_at - started_at` is the durable answer to "how long did this run",
    // and for an instant-shaped observation it is zero. An utterance is not an
    // instant.
    let mut merger = audio_merger();

    let event = started_audio(
        merger.ingest_envelope(audio_sample_lasting(0, 7, "seven seconds of speech")),
        SplitReason::Initial,
    );

    assert_eq!(event.started_at, at(0));
    assert_eq!(event.ended_at, at(7));
}

#[test]
fn a_screen_events_window_is_still_the_instant_it_was_sampled() {
    // The span only exists for observations that have one. Nothing about the
    // screen channel's windows may move.
    let mut merger = merger();

    let event = started(
        merger.ingest(sample(0, "notepad.exe", "notes", "ordinary screen text")),
        SplitReason::Initial,
    );

    assert_eq!(event.started_at, event.ended_at);
    assert_eq!(event.ended_at, at(0));
}
