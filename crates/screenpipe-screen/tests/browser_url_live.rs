#![cfg(target_os = "windows")]

use screenpipe_screen::{BrowserUrlReader, WindowsCapture};
use url::Url;

/// The steady-state acceptance test. No `Ctrl+L`, no omnibox focus, no operator
/// interaction of any kind after the browser is put in front: the document
/// node answers with the committed URL on an ordinary page, which is exactly
/// what the retired omnibox path could not do.
async fn reads_the_live_browser_url(expected_app_key: &str) {
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (_, metadata) = WindowsCapture.capture_foreground().await.unwrap();
    assert_eq!(metadata.app_key, expected_app_key);

    let actual = BrowserUrlReader
        .read_for_foreground(&metadata)
        .unwrap()
        .unwrap();
    let expected = Url::parse("https://github.com/MooseGooseConsulting/screenpipe").unwrap();

    assert_eq!(
        actual.as_str().trim_end_matches('/'),
        expected.as_str().trim_end_matches('/')
    );
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Edge focused on the Goal 1 repository URL, omnibox unfocused"]
async fn reads_the_real_edge_document_url() {
    reads_the_live_browser_url("msedge.exe").await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Chrome focused on the Goal 1 repository URL, omnibox unfocused"]
async fn reads_the_real_chrome_document_url() {
    reads_the_live_browser_url("chrome.exe").await;
}
