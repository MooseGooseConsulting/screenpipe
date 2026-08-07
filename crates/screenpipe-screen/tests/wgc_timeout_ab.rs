#![cfg(target_os = "windows")]

use std::collections::BTreeMap;

use screenpipe_screen::WindowsCapture;

fn required_isize(name: &str) -> isize {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be set for the controlled WGC diagnostic"))
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be a decimal pointer-sized integer"))
}

fn attempt_count() -> usize {
    std::env::var("SCREENPIPE_WGC_AB_ITERATIONS")
        .map_or(Ok(10), |value| value.parse())
        .expect("SCREENPIPE_WGC_AB_ITERATIONS must be a positive integer")
}

fn outcome_category(error: &anyhow::Error) -> &'static str {
    let detail = format!("{error:#}");
    if detail.contains("timed out waiting on channel") {
        "timeout"
    } else if detail.contains("is absent from") {
        "window_absent"
    } else if detail.contains("foreground window changed") {
        "foreground_changed"
    } else if detail.contains("RgbaImage::from_raw") {
        "callback_rgba_conversion"
    } else if detail.contains("HRESULT") || detail.contains("0x800") {
        "windows_error_redacted"
    } else {
        "other_redacted"
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "same-HWND diagnostic; timeout A/B requires the separately hashed instrumented xcap 0.9.4 binary"]
async fn reports_repeated_capture_outcomes_for_one_explicit_foreground_hwnd() {
    let target_hwnd = required_isize("SCREENPIPE_WGC_TARGET_HWND");
    let iterations = attempt_count();
    assert!(iterations > 0, "diagnostic requires at least one attempt");

    let mut successes = 0usize;
    let mut errors = BTreeMap::<&'static str, usize>::new();

    for attempt in 1..=iterations {
        let before = WindowsCapture.foreground_window_handle().unwrap();
        assert_eq!(
            before, target_hwnd,
            "foreground HWND differs from the explicit target before capture"
        );
        let result = WindowsCapture.capture_foreground().await;
        let after = WindowsCapture.foreground_window_handle().unwrap();
        assert_eq!(
            after, target_hwnd,
            "foreground HWND differs from the explicit target after capture"
        );
        match result {
            Ok((frame, metadata)) => {
                assert_eq!(
                    metadata.window_handle, target_hwnd,
                    "foreground HWND changed during the controlled diagnostic"
                );
                successes += 1;
                println!(
                    "WGC_AB_ATTEMPT attempt={attempt} outcome=success width={} height={}",
                    frame.width(),
                    frame.height()
                );
            }
            Err(error) => {
                let category = outcome_category(&error);
                *errors.entry(category).or_default() += 1;
                println!("WGC_AB_ATTEMPT attempt={attempt} outcome=error category={category}");
            }
        }
    }

    println!(
        "WGC_AB_SUMMARY target_hwnd={target_hwnd} attempts={iterations} successes={successes} errors={errors:?}"
    );
}
