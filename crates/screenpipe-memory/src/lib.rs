//! Deterministic screen-memory domain and PostgreSQL writer.

mod merge;
mod sample;
mod text_hash;

pub use merge::{MergeConfig, MergeDecision, Merger, OpenEvent, SplitReason};
pub use sample::ObservationSample;
pub use text_hash::{TextIdentity, jaccard_overlap};
