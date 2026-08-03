use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::cadence::CadenceRecord;
use crate::sample::ObservationSample;
use crate::text_hash::{TextIdentity, jaccard_overlap, normalize_text};

pub const MERGE_CONTRACT_VERSION: u32 = 1;

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
}

impl CaptureGap {
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::CaptureUnavailable => "capture_unavailable",
            Self::OcrUnavailable => "ocr_unavailable",
            Self::EmptyOcr => "empty_ocr",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CaptureGapSummary {
    pub capture_unavailable: u64,
    pub ocr_unavailable: u64,
    pub empty_ocr: u64,
}

impl CaptureGapSummary {
    pub fn record(&mut self, gap: CaptureGap) {
        let count = match gap {
            CaptureGap::CaptureUnavailable => &mut self.capture_unavailable,
            CaptureGap::OcrUnavailable => &mut self.ocr_unavailable,
            CaptureGap::EmptyOcr => &mut self.empty_ocr,
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
    pub hash_counts: BTreeMap<String, u64>,
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

        let reason = if sample.app_key != open.latest.app_key {
            Some(SplitReason::AppChange)
        } else if normalize_text(&sample.window_title) != normalize_text(&open.latest.window_title)
        {
            Some(SplitReason::WindowTitleChange)
        } else if sample.captured_at - open.ended_at > self.config.idle_gap {
            Some(SplitReason::IdleGap)
        } else {
            let previous_identity = TextIdentity::from_ocr(&open.latest.ocr_text);
            let resolved_merge_hash = if open.hash_counts.contains_key(&identity.exact_hash)
                || jaccard_overlap(&previous_identity.five_grams, &identity.five_grams)
                    >= self.config.scroll_overlap
            {
                open.merge_hash.clone()
            } else {
                identity.exact_hash.clone()
            };

            (resolved_merge_hash != open.merge_hash).then_some(SplitReason::TextHashChange)
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
        let hash_count = open.hash_counts.entry(identity.exact_hash).or_default();
        *hash_count = hash_count.saturating_add(1);
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
        let mut hash_counts = BTreeMap::new();
        hash_counts.insert(merge_hash.clone(), 1);
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
