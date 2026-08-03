#![cfg(target_os = "windows")]

use screenpipe_screen::WindowsCapture;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires an unlocked interactive Windows desktop"]
async fn captures_only_the_real_foreground_window() {
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (frame, metadata) = WindowsCapture.capture_foreground().await.unwrap();

    assert!(frame.width() > 0);
    assert!(frame.height() > 0);
    assert_ne!(metadata.window_handle, 0);
    assert!(!metadata.app_key.trim().is_empty());
    assert!(!metadata.app_title.trim().is_empty());
    assert!(!metadata.window_title.trim().is_empty());
}
