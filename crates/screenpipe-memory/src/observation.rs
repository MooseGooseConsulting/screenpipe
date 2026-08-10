use std::{fmt, str::FromStr};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A stable identity for a window, without retaining any window content.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WindowKey {
    None,
    Window { hwnd: u64, window_generation: u64 },
}

impl WindowKey {
    fn validate(&self) -> Result<()> {
        if let Self::Window { hwnd: 0, .. } = self {
            bail!("window key cannot use HWND 0; use none when no window is present");
        }
        Ok(())
    }
}

impl fmt::Display for WindowKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("none"),
            Self::Window {
                hwnd,
                window_generation,
            } => write!(
                formatter,
                "window(hwnd={hwnd},window_generation={window_generation})"
            ),
        }
    }
}

impl FromStr for WindowKey {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        if value == "none" {
            return Ok(Self::None);
        }

        let inner = value
            .strip_prefix("window(hwnd=")
            .and_then(|value| value.strip_suffix(')'))
            .ok_or_else(|| anyhow::anyhow!("invalid window key: {value}"))?;
        let (hwnd, window_generation) = inner
            .split_once(",window_generation=")
            .ok_or_else(|| anyhow::anyhow!("invalid window key: {value}"))?;
        let parsed = Self::Window {
            hwnd: hwnd
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid HWND in window key: {value}"))?,
            window_generation: window_generation
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid window generation in window key: {value}"))?,
        };
        parsed.validate()?;
        if parsed.to_string() != value {
            bail!("window key is not canonical: {value}");
        }
        Ok(parsed)
    }
}

/// Content-free facts that make an observation safe to route and audit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationIdentity {
    pub source_id: String,
    pub modality: String,
    pub machine_id: String,
    pub observed_at: DateTime<Utc>,
    pub producer: String,
    pub version: String,
    pub policy_epoch: Option<u64>,
    pub window_key: WindowKey,
}

impl ObservationIdentity {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("source", &self.source_id),
            ("modality", &self.modality),
            ("machine", &self.machine_id),
            ("producer", &self.producer),
            ("version", &self.version),
        ] {
            if value.trim().is_empty() {
                bail!("observation identity has an empty {name}");
            }
        }

        match self.policy_epoch {
            Some(0) | None => bail!("observation identity is missing a policy epoch"),
            Some(_) => self.window_key.validate(),
        }
    }
}

/// A content-free result for one policy-bound observation attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationOutcome {
    Available(ObservationIdentity),
    Absent(ObservationIdentity),
    Denied(ObservationIdentity),
    Failed(ObservationIdentity),
    TimedOut(ObservationIdentity),
    Cancelled(ObservationIdentity),
    Stale(ObservationIdentity),
}

impl ObservationOutcome {
    pub fn identity(&self) -> &ObservationIdentity {
        match self {
            Self::Available(identity)
            | Self::Absent(identity)
            | Self::Denied(identity)
            | Self::Failed(identity)
            | Self::TimedOut(identity)
            | Self::Cancelled(identity)
            | Self::Stale(identity) => identity,
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.identity().validate()
    }
}
