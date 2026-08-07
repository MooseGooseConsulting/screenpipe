use chrono::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CadenceInput {
    pub input_idle: Duration,
    pub frame_stable_for: Duration,
    pub foreground_changed: bool,
    pub frame_changed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CadenceRecord {
    pub input: CadenceInput,
    pub next_interval: Duration,
}

impl CadenceRecord {
    pub fn from_input(input: CadenceInput) -> Self {
        Self {
            input,
            next_interval: CadencePolicy::next_interval(input),
        }
    }
}

/// The slowest interval `CadencePolicy` will ever select. `MergeConfig.idle_gap`
/// must stay strictly greater than this, or the idle backoff itself trips the
/// idle-gap split on every sample.
pub const MAX_CADENCE_INTERVAL_SECONDS: i64 = 30;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CadencePolicy;

impl CadencePolicy {
    pub fn next_interval(input: CadenceInput) -> Duration {
        if input.foreground_changed || input.frame_changed {
            return Duration::seconds(2);
        }

        let stable_for = input.input_idle.min(input.frame_stable_for);
        if stable_for >= Duration::minutes(10) {
            Duration::seconds(MAX_CADENCE_INTERVAL_SECONDS)
        } else if stable_for >= Duration::minutes(2) {
            Duration::seconds(15)
        } else if stable_for >= Duration::seconds(30) {
            Duration::seconds(5)
        } else {
            Duration::seconds(2)
        }
    }
}
