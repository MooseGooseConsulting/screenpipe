use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Duration, Utc};

use crate::cadence::CadenceRecord;
use crate::sample::ObservationSample;
use crate::text_hash::{TextIdentity, jaccard_overlap, normalize_text};

/// Bumped to 2 when the per-event hash ledger became bounded. Version 1 events
/// recorded every distinct OCR hash they ever saw; version 2 events record at
/// most `MAX_TRACKED_HASHES` and carry an eviction count, so `hashes_seen` is
/// no longer a complete census and must not be read as one.
pub const MERGE_CONTRACT_VERSION: u32 = 3;

/// Upper bound on distinct OCR hashes remembered inside one open event.
///
/// Unbounded, this was the worst defect in the merge path. Any window whose
/// text changes while staying above the five-gram overlap threshold - a log
/// tail, a build, a subtitled video, any UI with a clock - takes the merge
/// path on every sample and contributes a new 64-character key. At the 2s
/// cadence that is ~43,200 entries in 24 hours: several megabytes resident,
/// cloned twice per tick, and re-serialized in full into the `merge_meta`
/// JSONB on *every* write. The durable write volume is quadratic in sample
/// count; one event would rewrite a multi-megabyte JSONB every two seconds.
///
/// The cap also restores a behaviour that unbounded growth had quietly
/// removed. `contains` is what lets a previously-seen screen merge back in, so
/// with an unbounded ledger an event that had seen N screens would merge any
/// of those N back unconditionally, forever - `SplitReason::TextHashChange`
/// became progressively unreachable as the event aged.
pub const MAX_TRACKED_HASHES: usize = 64;

/// Bounded record of the OCR hashes seen inside one open event.
///
/// Eviction is by insertion order (oldest out first), which is what makes
/// "have I seen this screen recently" the question `contains` answers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HashLedger {
    counts: BTreeMap<String, u64>,
    order: VecDeque<String>,
    evicted: u64,
}

impl HashLedger {
    fn with_first(hash: String) -> Self {
        Self::from_hashes([hash])
    }

    /// Build a ledger by observing `hashes` in order. Eviction applies, so this
    /// cannot be used to fabricate a ledger larger than `MAX_TRACKED_HASHES`.
    pub fn from_hashes(hashes: impl IntoIterator<Item = String>) -> Self {
        let mut ledger = Self::default();
        for hash in hashes {
            ledger.record(hash);
        }
        ledger
    }

    /// True when this hash is still tracked. An evicted hash reads as unseen,
    /// which is the intended consequence of the bound.
    pub fn contains(&self, hash: &str) -> bool {
        self.counts.contains_key(hash)
    }

    fn record(&mut self, hash: String) {
        if let Some(count) = self.counts.get_mut(&hash) {
            *count = count.saturating_add(1);
            return;
        }
        if self.counts.len() >= MAX_TRACKED_HASHES
            && let Some(oldest) = self.order.pop_front()
        {
            self.counts.remove(&oldest);
            self.evicted = self.evicted.saturating_add(1);
        }
        self.order.push_back(hash.clone());
        self.counts.insert(hash, 1);
    }

    /// The tracked hashes and their observation counts.
    pub fn counts(&self) -> &BTreeMap<String, u64> {
        &self.counts
    }

    /// How many distinct hashes fell out of the window. Persisted alongside
    /// the counts so a reader can tell a complete census from a truncated one.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitReason {
    Initial,
    AppChange,
    WindowTitleChange,
    IdleGap,
    TextHashChange,
}

impl SplitReason {
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::AppChange => "app_change",
            Self::WindowTitleChange => "window_title_change",
            Self::IdleGap => "idle_gap",
            Self::TextHashChange => "text_hash_change",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeDecisionKind {
    Start,
    Merge,
}

impl MergeDecisionKind {
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Merge => "merge",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureGap {
    CaptureUnavailable,
    OcrUnavailable,
    EmptyOcr,
    /// The interactive desktop is locked or absent.
    ///
    /// Distinct from `CaptureUnavailable` on purpose. A locked workstation is
    /// an ordinary, indefinite state - a machine left overnight produces
    /// nothing else - whereas `CaptureUnavailable` means capture was attempted
    /// against a live desktop and failed. Collapsing the two made them
    /// indistinguishable to the run loop, so the ceiling that exists to catch a
    /// dead capture device would fire on a normal night's sleep instead.
    DesktopLocked,
}

impl CaptureGap {
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::CaptureUnavailable => "capture_unavailable",
            Self::OcrUnavailable => "ocr_unavailable",
            Self::EmptyOcr => "empty_ocr",
            Self::DesktopLocked => "desktop_locked",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureGapSummary {
    pub capture_unavailable: u64,
    pub ocr_unavailable: u64,
    pub empty_ocr: u64,
    pub desktop_locked: u64,
}

impl CaptureGapSummary {
    pub fn record(&mut self, gap: CaptureGap) {
        let count = match gap {
            CaptureGap::CaptureUnavailable => &mut self.capture_unavailable,
            CaptureGap::OcrUnavailable => &mut self.ocr_unavailable,
            CaptureGap::EmptyOcr => &mut self.empty_ocr,
            CaptureGap::DesktopLocked => &mut self.desktop_locked,
        };
        *count = count.saturating_add(1);
    }

    fn add(self, other: Self) -> Self {
        Self {
            capture_unavailable: self
                .capture_unavailable
                .saturating_add(other.capture_unavailable),
            ocr_unavailable: self.ocr_unavailable.saturating_add(other.ocr_unavailable),
            empty_ocr: self.empty_ocr.saturating_add(other.empty_ocr),
            desktop_locked: self.desktop_locked.saturating_add(other.desktop_locked),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MergeDecision {
    Start {
        reason: SplitReason,
        event: OpenEvent,
    },
    Merge {
        event: OpenEvent,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MergeConfig {
    /// Must be strictly greater than the maximum cadence interval. The split
    /// test compares the delta between *consecutive samples*, so if this
    /// equals the slowest cadence the sleep alone reaches the threshold and
    /// ordinary capture and OCR overhead pushes every idle sample past it -
    /// fragmenting a quiet window into one-sample events, which is the exact
    /// opposite of what an idle gap is for. See `MAX_CADENCE_INTERVAL`.
    pub idle_gap: Duration,
    pub scroll_overlap: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenEvent {
    pub merge_contract_version: u32,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub latest: ObservationSample,
    pub merge_hash: String,
    pub latest_exact_ocr_hash: String,
    pub start_reason: SplitReason,
    pub last_decision: MergeDecisionKind,
    pub latest_cadence: CadenceRecord,
    pub capture_gaps: CaptureGapSummary,
    pub sample_count: u32,
    pub hash_counts: HashLedger,
}

#[derive(Clone, Debug)]
pub struct Merger {
    config: MergeConfig,
    open: Option<OpenEvent>,
}

impl Merger {
    pub fn new(config: MergeConfig) -> Self {
        Self { config, open: None }
    }

    pub fn ingest_with_metadata(
        &mut self,
        sample: ObservationSample,
        cadence: CadenceRecord,
        capture_gaps: CaptureGapSummary,
    ) -> MergeDecision {
        let identity = TextIdentity::from_ocr(&sample.ocr_text);
        let Some(open) = self.open.as_ref() else {
            return self.start(
                sample,
                identity.exact_hash,
                cadence,
                capture_gaps,
                SplitReason::Initial,
            );
        };

        // CONTENT DECIDES. The window title is a hint, not the authority.
        //
        // This used to test the title second, before content was consulted at
        // all, and a title change short-circuited straight to a split. A window
        // title is a presentation surface: apps put spinners, unsaved-change
        // markers, notification counts and download percentages in it. Every
        // one of those became a semantic event boundary.
        //
        // Measured on 784 real events from this machine: 690 of them - 88% -
        // started because of a title change, averaging 3.2 samples each. The
        // control was in the same table. Events that started from an app change
        // - an unambiguous, real change of activity - averaged 24.1 samples,
        // 7.5x longer. Nothing about the underlying activity was that
        // fragmented; the segmentation was. Content only ever got a vote 59
        // times, because the title check ran first and almost always fired.
        //
        // So the order is inverted. If the text is continuous - the same screen
        // seen before, or enough five-gram overlap to be a scroll - this is the
        // same activity and the title is decoration. A title change still
        // splits, but only when the content changed too, and it keeps its own
        // reason code because "the title changed" is the more informative
        // description of what happened.
        let reason = if sample.app_key != open.latest.app_key {
            Some(SplitReason::AppChange)
        } else if sample.captured_at - open.ended_at > self.config.idle_gap {
            Some(SplitReason::IdleGap)
        } else {
            let previous_identity = TextIdentity::from_ocr(&open.latest.ocr_text);
            let content_continues = open.hash_counts.contains(&identity.exact_hash)
                || jaccard_overlap(&previous_identity.five_grams, &identity.five_grams)
                    >= self.config.scroll_overlap;

            if content_continues {
                None
            } else if normalize_text(&sample.window_title)
                != normalize_text(&open.latest.window_title)
            {
                Some(SplitReason::WindowTitleChange)
            } else {
                Some(SplitReason::TextHashChange)
            }
        };

        if let Some(reason) = reason {
            return self.start(sample, identity.exact_hash, cadence, capture_gaps, reason);
        }

        let open = self.open.as_mut().expect("open event checked above");
        open.ended_at = sample.captured_at;
        open.latest = sample;
        open.latest_exact_ocr_hash = identity.exact_hash.clone();
        open.last_decision = MergeDecisionKind::Merge;
        open.latest_cadence = cadence;
        open.capture_gaps = open.capture_gaps.add(capture_gaps);
        open.sample_count = open.sample_count.saturating_add(1);
        open.hash_counts.record(identity.exact_hash);
        MergeDecision::Merge {
            event: open.clone(),
        }
    }

    fn start(
        &mut self,
        sample: ObservationSample,
        merge_hash: String,
        cadence: CadenceRecord,
        capture_gaps: CaptureGapSummary,
        reason: SplitReason,
    ) -> MergeDecision {
        let hash_counts = HashLedger::with_first(merge_hash.clone());
        let event = OpenEvent {
            merge_contract_version: MERGE_CONTRACT_VERSION,
            started_at: sample.captured_at,
            ended_at: sample.captured_at,
            latest: sample,
            latest_exact_ocr_hash: merge_hash.clone(),
            merge_hash,
            start_reason: reason,
            last_decision: MergeDecisionKind::Start,
            latest_cadence: cadence,
            capture_gaps,
            sample_count: 1,
            hash_counts,
        };
        self.open = Some(event.clone());
        MergeDecision::Start { reason, event }
    }
}
