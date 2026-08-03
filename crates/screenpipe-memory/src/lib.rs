//! Deterministic screen-memory domain and PostgreSQL writer.

mod cadence;
mod merge;
mod sample;
mod text_hash;

pub use cadence::{CadenceInput, CadencePolicy};
pub use merge::{MergeConfig, MergeDecision, Merger, OpenEvent, SplitReason};
pub use sample::ObservationSample;
pub use text_hash::{TextIdentity, jaccard_overlap};
