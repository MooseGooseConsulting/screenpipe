//! Shared v3 context-graph domain mappings.

/// Capture modalities persisted by the v3 context schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextModality {
    Screen,
    Browser,
    Clipboard,
    Audio,
}

impl ContextModality {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "screen" => Some(Self::Screen),
            "browser" => Some(Self::Browser),
            "clipboard" => Some(Self::Clipboard),
            "audio" => Some(Self::Audio),
            _ => None,
        }
    }

    /// Screen and browser preserve their existing capture default; clipboard
    /// and audio require an explicit grant before access.
    pub const fn default_consent(self) -> bool {
        matches!(self, Self::Screen | Self::Browser)
    }
}
