#![cfg(target_os = "windows")]

use screenpipe_screen::{BrowserUrlReader, WindowsCapture};
use url::Url;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Edge focused on the Goal 1 repository URL"]
async fn reads_the_real_edge_address_bar_only() {
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (_, metadata) = WindowsCapture.capture_foreground().await.unwrap();
    assert_eq!(metadata.app_key, "msedge.exe");

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
