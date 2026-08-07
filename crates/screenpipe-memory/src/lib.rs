//! Deterministic screen-memory domain and PostgreSQL writer.

mod cadence;
mod merge;
mod postgres;
mod runner;
mod sample;
mod text_hash;

pub use cadence::{CadenceInput, CadencePolicy, CadenceRecord, MAX_CADENCE_INTERVAL_SECONDS};
pub use merge::{
    CaptureGap, CaptureGapSummary, EventKind, HashLedger, MAX_EVENT_DURATION_SECONDS,
    MAX_EVENT_SAMPLES, MAX_TRACKED_HASHES, MERGE_CONTRACT_VERSION, MergeConfig, MergeDecision,
    MergeDecisionKind, Merger, OpenEvent, SplitReason,
};
pub use postgres::{
    MINIMUM_SERVER_VERSION_NUM, PgEventWriter, PgPreflight, SearchHit, SearchRequest,
};
pub use runner::{EventId, EventSink, RunOutcome, Runner, SampleRead, SampleSource};
pub use sample::{AudioMeta, ObservationSample};
pub use text_hash::{TextIdentity, jaccard_overlap};
