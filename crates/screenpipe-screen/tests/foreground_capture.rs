#![cfg(target_os = "windows")]

use screenpipe_screen::WindowsCapture;
use xcap::Window;

mod common;
use common::{interactive_desktop_available, lock_foreground};

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
async fn captures_only_the_pinned_real_foreground_window_with_redacted_probe() {
    // Was `#[ignore]`d, which meant it never ran in the only session where it
    // is meaningful. It now probes for the capability instead: it runs by
    // default on an unlocked desktop, skips loudly when locked, and fails
    // outright when SCREEN_MEMORY_REQUIRE_INTERACTIVE says the capability must
    // be present.
    if !interactive_desktop_available(
        "captures_only_the_pinned_real_foreground_window_with_redacted_probe",
    ) {
        return;
    }

    let _foreground = lock_foreground();

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

    // The metadata must describe the window we pinned, not merely be non-empty.
    // `!is_empty()` passes just as well for a frame of the wrong window, which
    // is what this test previously settled for.
    assert_eq!(
        metadata.window_handle, before,
        "capture returned metadata for a different window than the pinned foreground HWND"
    );
    // Geometry must match the xcap candidate we identified as the foreground
    // window - this is what catches a transposed or wrongly-derived rectangle.
    let candidate = matching.first().copied().expect("pinned candidate");
    assert_eq!(
        (frame.width(), frame.height()),
        (
            candidate.width().expect("candidate width"),
            candidate.height().expect("candidate height")
        ),
        "captured frame geometry does not match the pinned window's own dimensions"
    );
    assert!(!metadata.app_key.trim().is_empty());
    assert!(!metadata.app_title.trim().is_empty());
    assert!(!metadata.window_title.trim().is_empty());
}
