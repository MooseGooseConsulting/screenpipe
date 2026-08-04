//! Deterministic screen-memory domain and PostgreSQL writer.

mod cadence;
mod merge;
mod postgres;
mod runner;
mod sample;
mod text_hash;

pub use cadence::{CadenceInput, CadencePolicy, CadenceRecord};
pub use merge::{
    CaptureGap, CaptureGapSummary, MERGE_CONTRACT_VERSION, MergeConfig, MergeDecision,
    MergeDecisionKind, Merger, OpenEvent, SplitReason,
};
pub use postgres::PgEventWriter;
pub use runner::{EventId, EventSink, RunOutcome, Runner, SampleRead, SampleSource};
pub use sample::ObservationSample;
pub use text_hash::{TextIdentity, jaccard_overlap};
