//! Windows foreground capture, OCR, and clipboard boundary.

mod browser_url;
mod interactive;
mod types;
mod windows_capture;
mod windows_clipboard;
mod windows_last_input;
mod windows_metadata;
mod windows_ocr;

pub use browser_url::{BrowserUrlReader, UiaElementSnapshot, select_browser_url};
pub use interactive::{
    InteractiveCapability, NotInteractive, probe_interactive_capability, session_is_locked,
};
pub use types::{ForegroundMetadata, FrameFingerprint, TransientFrame};
pub use windows_capture::WindowsCapture;
pub use windows_clipboard::{
    CLIPBOARD_EXCLUSION_FORMATS, ClipboardExclusions, ClipboardRead, ClipboardWatcher,
    FormatPermission, capture_is_permitted,
};
pub use windows_last_input::WindowsLastInput;
pub use windows_ocr::WindowsOcr;
