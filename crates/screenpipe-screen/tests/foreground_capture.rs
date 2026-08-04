#![cfg(target_os = "windows")]

use screenpipe_screen::WindowsCapture;
use xcap::Window;

fn capture_outcome_category(error: &anyhow::Error) -> &'static str {
    let detail = format!("{error:#}");
    if detail.contains("timed out waiting on channel") {
        "timeout"
    } else if detail.contains("ROI out of bounds") {
        "roi_out_of_bounds"
    } else if detail.contains("is absent from") {
        "window_absent"
    } else if detail.contains("foreground window changed") {
        "foreground_changed"
    } else if detail.contains("HRESULT") || detail.contains("0x800") {
        "windows_error_redacted"
    } else {
        "other_redacted"
    }
}

fn app_category(window: Option<&Window>) -> &'static str {
    match window.and_then(|window| window.app_name().ok()) {
        Some(name) if name.eq_ignore_ascii_case("notepad++") => "notepadpp",
        Some(_) => "other",
        None => "unavailable",
    }
}

fn title_category(window: Option<&Window>) -> &'static str {
    match window.and_then(|window| window.title().ok()) {
        Some(title) if title.trim().is_empty() => "empty",
        Some(_) => "nonempty",
        None => "unavailable",
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires an unlocked interactive Windows desktop"]
async fn captures_only_the_pinned_real_foreground_window_with_redacted_probe() {
    std::thread::sleep(std::time::Duration::from_secs(2));
    let before = WindowsCapture.foreground_window_handle().unwrap();
    let windows = Window::all().unwrap();
    let candidate_count = windows.len();
    let matching = windows
        .iter()
        .filter(|window| window.id().ok() == Some(before as usize as u32))
        .collect::<Vec<_>>();
    let exact_match_count = matching.len();
    let focused_match_count = matching
        .iter()
        .filter(|window| window.is_focused().unwrap_or(false))
        .count();
    println!(
        "FOREGROUND_CAPTURE_PROBE phase=before hwnd={before} candidate_count={candidate_count} exact_match_count={exact_match_count} focused_match_count={focused_match_count} app_category={} title_category={}",
        app_category(matching.first().copied()),
        title_category(matching.first().copied()),
    );
    assert_eq!(
        exact_match_count, 1,
        "foreground HWND must have one xcap candidate"
    );
    assert_eq!(
        focused_match_count, 1,
        "foreground HWND must be the xcap-focused candidate before capture"
    );

    let result = WindowsCapture.capture_foreground().await;
    let after = WindowsCapture.foreground_window_handle().unwrap();
    assert_eq!(before, after, "foreground HWND changed during capture");

    let (frame, metadata) = match result {
        Ok(result) => {
            println!(
                "FOREGROUND_CAPTURE_PROBE phase=after hwnd={after} outcome=success width={} height={}",
                result.0.width(),
                result.0.height()
            );
            result
        }
        Err(error) => {
            println!(
                "FOREGROUND_CAPTURE_PROBE phase=after hwnd={after} outcome=error category={}",
                capture_outcome_category(&error)
            );
            panic!("foreground capture failed with a redacted outcome category");
        }
    };

    assert!(frame.width() > 0);
    assert!(frame.height() > 0);
    assert_ne!(metadata.window_handle, 0);
    assert!(!metadata.app_key.trim().is_empty());
    assert!(!metadata.app_title.trim().is_empty());
    assert!(!metadata.window_title.trim().is_empty());
}
