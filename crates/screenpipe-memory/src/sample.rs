use chrono::{DateTime, Utc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationSample {
    pub captured_at: DateTime<Utc>,
    pub app_key: String,
    pub app_title: String,
    pub window_title: String,
    pub ocr_text: String,
    pub readable_text: String,
    pub browser_url: Option<String>,
}
