//! Deterministic screen-memory domain and PostgreSQL writer.

mod cadence;
mod context;
mod merge;
mod observation;
pub mod policy;
mod postgres;
mod runner;
mod sample;
mod text_hash;

pub use cadence::{CadenceInput, CadencePolicy, CadenceRecord, MAX_CADENCE_INTERVAL_SECONDS};
pub use context::ContextModality;
pub use merge::{
    CaptureGap, CaptureGapSummary, EnvelopeMergeDecision, EventEnvelope, EventKind, HashLedger,
    MAX_EVENT_DURATION_SECONDS, MAX_EVENT_SAMPLES, MAX_TRACKED_HASHES, MERGE_CONTRACT_VERSION,
    MergeConfig, MergeDecision, MergeDecisionKind, Merger, OpenEvent, SplitReason,
};
pub use observation::{ObservationIdentity, ObservationOutcome, WindowKey};
pub use policy::{
    MemoryPolicyRepository, PgPolicyRepository, PolicyMutationRequest, PolicyRepository,
    PolicySnapshot,
};
pub use postgres::{
    PgEventReader, PgEventWriter, PgPreflight, SearchHit, SearchRequest, ensure_v3_schema,
};
pub use runner::{
    EventId, EventSink, ObservationRead, RunOutcome, Runner, SampleRead, SampleSource,
};
pub use sample::{AudioMeta, ObservationEnvelope, ObservationSample};
pub use text_hash::{TextIdentity, jaccard_overlap};
