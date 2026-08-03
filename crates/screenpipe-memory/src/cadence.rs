use chrono::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

impl Default for CadenceRecord {
    fn default() -> Self {
        Self::from_input(CadenceInput::default())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CadencePolicy;

impl CadencePolicy {
    pub fn next_interval(input: CadenceInput) -> Duration {
        if input.foreground_changed || input.frame_changed {
            return Duration::seconds(2);
        }

        let stable_for = input.input_idle.min(input.frame_stable_for);
        if stable_for >= Duration::minutes(10) {
            Duration::seconds(30)
        } else if stable_for >= Duration::minutes(2) {
            Duration::seconds(15)
        } else if stable_for >= Duration::seconds(30) {
            Duration::seconds(5)
        } else {
            Duration::seconds(2)
        }
    }
}
