use std::fmt;

use chrono::{DateTime, Utc};

#[derive(Clone, PartialEq, Eq)]
pub struct ObservationSample {
    pub captured_at: DateTime<Utc>,
    pub app_key: String,
    pub app_title: String,
    pub window_title: String,
    pub ocr_text: String,
    pub readable_text: String,
    pub browser_url: Option<String>,
}

impl fmt::Debug for ObservationSample {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObservationSample")
            .field("captured_at", &self.captured_at)
            .field("app_key", &self.app_key)
            .field("app_title", &self.app_title)
            .field("window_title", &self.window_title)
            .field("ocr_text", &"<redacted>")
            .field("readable_text", &"<redacted>")
            .field(
                "browser_url",
                &self.browser_url.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}
