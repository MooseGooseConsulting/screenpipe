use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::sample::ObservationSample;
use crate::text_hash::{TextIdentity, jaccard_overlap, normalize_text};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitReason {
    Initial,
    AppChange,
    WindowTitleChange,
    IdleGap,
    TextHashChange,
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
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub latest: ObservationSample,
    pub merge_hash: String,
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

    pub fn ingest(&mut self, sample: ObservationSample) -> MergeDecision {
        let identity = TextIdentity::from_ocr(&sample.ocr_text);
        let Some(open) = self.open.as_ref() else {
            return self.start(sample, identity.exact_hash, SplitReason::Initial);
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
            return self.start(sample, identity.exact_hash, reason);
        }

        let open = self.open.as_mut().expect("open event checked above");
        open.ended_at = sample.captured_at;
        open.latest = sample;
        open.sample_count += 1;
        *open.hash_counts.entry(identity.exact_hash).or_default() += 1;
        MergeDecision::Merge {
            event: open.clone(),
        }
    }

    fn start(
        &mut self,
        sample: ObservationSample,
        merge_hash: String,
        reason: SplitReason,
    ) -> MergeDecision {
        let mut hash_counts = BTreeMap::new();
        hash_counts.insert(merge_hash.clone(), 1);
        let event = OpenEvent {
            started_at: sample.captured_at,
            ended_at: sample.captured_at,
            latest: sample,
            merge_hash,
            sample_count: 1,
            hash_counts,
        };
        self.open = Some(event.clone());
        MergeDecision::Start { reason, event }
    }
}
