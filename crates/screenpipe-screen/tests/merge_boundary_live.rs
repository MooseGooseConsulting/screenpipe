#![cfg(target_os = "windows")]
//! Merge boundaries must land where the screen ACTUALLY changed.
//!
//! Every other merge test feeds the merger hand-written samples, so it proves
//! the merger's arithmetic but says nothing about whether real captured
//! metadata drives a boundary at the right moment. This test captures two
//! genuinely different foreground windows through the real capture and OCR
//! path, feeds those real samples to a real `Merger`, and asserts the split
//! lands exactly at the switch - and nowhere else.

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use chrono::Utc;
use screenpipe_memory::{
    CadenceInput, CadenceRecord, CaptureGapSummary, MergeConfig, MergeDecision, Merger,
    ObservationSample, SplitReason,
};
use screenpipe_screen::{WindowsCapture, WindowsOcr};

mod common;
use common::{interactive_desktop_available, lock_foreground};

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> *mut c_void;
    fn GetWindowThreadProcessId(window: *mut c_void, process_id: *mut u32) -> u32;
    fn GetWindowTextW(window: *mut c_void, text: *mut u16, max_count: i32) -> i32;
}

/// Foreground window title, read straight from the window manager. Cheap enough
/// to poll, unlike a full WGC capture.
fn foreground_window_title() -> Option<String> {
    let window = unsafe { GetForegroundWindow() };
    if window.is_null() {
        return None;
    }
    let mut buffer = vec![0_u16; 1024];
    let length = unsafe { GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32) };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    Some(OsString::from_wide(&buffer).to_string_lossy().into_owned())
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> *mut c_void;
    fn CloseHandle(object: *mut c_void) -> i32;
    fn QueryFullProcessImageNameW(
        process: *mut c_void,
        flags: u32,
        filename: *mut u16,
        size: *mut u32,
    ) -> i32;
}

fn foreground_exe_name() -> Option<String> {
    let window = unsafe { GetForegroundWindow() };
    if window.is_null() {
        return None;
    }
    let mut process_id = 0_u32;
    if unsafe { GetWindowThreadProcessId(window, &mut process_id) } == 0 || process_id == 0 {
        return None;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return None;
    }
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) };
    unsafe {
        CloseHandle(process);
    }
    if ok == 0 || length == 0 {
        return None;
    }
    buffer.truncate(length as usize);
    let path = OsString::from_wide(&buffer).to_string_lossy().into_owned();
    Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_lowercase)
}

struct Fixture(Child);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A window with a caller-chosen title and body text, so two fixtures differ in
/// exactly the fields the merge contract splits on.
fn launch(title: &str, body: &str) -> std::io::Result<Fixture> {
    let script = format!(
        r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$form = New-Object System.Windows.Forms.Form
$form.Text = '{title}'
$form.StartPosition = 'CenterScreen'
$form.ClientSize = New-Object System.Drawing.Size(880, 520)
$form.BackColor = [System.Drawing.Color]::White
$form.TopMost = $true
$label = New-Object System.Windows.Forms.Label
$label.Font = New-Object System.Drawing.Font('Segoe UI', 40, [System.Drawing.FontStyle]::Bold)
$label.ForeColor = [System.Drawing.Color]::Black
$label.Dock = 'Fill'
$label.TextAlign = 'MiddleCenter'
$label.Text = '{body}'
$form.Controls.Add($label)
$form.Add_Shown({{ $form.Activate(); $form.BringToFront() }})
[System.Windows.Forms.Application]::Run($form)
"#
    );
    Ok(Fixture(
        Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-STA",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &script,
            ])
            .spawn()?,
    ))
}

/// Wait until the production capture path itself reports the expected window as
/// foreground. Polling through `capture_foreground` rather than a separate xcap
/// enumeration means the test waits on exactly the same evidence the sample
/// source would use, so a fixture that is "up" by one measure but not the other
/// cannot produce a confusing failure.
async fn wait_for_title(expected_title: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut last_seen = String::new();
    while Instant::now() < deadline {
        if foreground_exe_name().as_deref() == Some("powershell.exe")
            && let Some(title) = foreground_window_title()
        {
            last_seen = title.clone();
            if title.contains(expected_title) {
                // One more beat so the label finishes painting before OCR.
                tokio::time::sleep(Duration::from_millis(700)).await;
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    eprintln!(
        "wait_for_title({expected_title}) timed out; last foreground title was {last_seen:?}"
    );
    false
}

fn idle_cadence() -> CadenceRecord {
    CadenceRecord::from_input(CadenceInput {
        input_idle: chrono::Duration::zero(),
        frame_stable_for: chrono::Duration::zero(),
        foreground_changed: false,
        frame_changed: false,
    })
}

/// Capture the current foreground window and turn it into a real
/// `ObservationSample`, exactly as the production sample source does.
async fn observe() -> ObservationSample {
    let (frame, metadata) = WindowsCapture
        .capture_foreground()
        .await
        .expect("foreground capture failed");
    let ocr_text = WindowsOcr
        .recognize(&frame)
        .await
        .expect("Windows OCR failed");
    ObservationSample {
        captured_at: Utc::now(),
        app_key: metadata.app_key,
        app_title: metadata.app_title,
        window_title: metadata.window_title,
        readable_text: ocr_text.clone(),
        ocr_text,
        browser_url: metadata.browser_url,
        audio: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn merge_boundary_lands_exactly_where_the_window_actually_changed() {
    if !interactive_desktop_available(
        "merge_boundary_lands_exactly_where_the_window_actually_changed",
    ) {
        return;
    }

    // One foreground window, many test binaries: serialize.
    let _foreground = lock_foreground();

    let mut merger = Merger::new(MergeConfig {
        kind: screenpipe_memory::EventKind::Screen,
        // Comfortably above the real capture interval so no idle-gap split can
        // be mistaken for a content boundary.
        idle_gap: chrono::Duration::seconds(600),
        scroll_overlap: 0.35,
    });

    // --- Window A -----------------------------------------------------------
    let first = launch("SCREENPIPE BOUNDARY ALPHA", "ZEBRAWALTZ").expect("launch alpha");
    assert!(
        wait_for_title("SCREENPIPE BOUNDARY ALPHA", Duration::from_secs(45)).await,
        "alpha fixture never became the pinned foreground window"
    );

    let alpha_one = observe().await;
    let decision = merger.ingest_with_metadata(
        alpha_one.clone(),
        idle_cadence(),
        CaptureGapSummary::default(),
    );
    assert!(
        matches!(
            decision,
            MergeDecision::Start {
                reason: SplitReason::Initial,
                ..
            }
        ),
        "the first real sample must open an initial event, got {decision:?}"
    );

    // A second sample of the SAME unchanged window must merge, not split.
    let alpha_two = observe().await;
    let decision =
        merger.ingest_with_metadata(alpha_two, idle_cadence(), CaptureGapSummary::default());
    let merged_event = match decision {
        MergeDecision::Merge { event } => event,
        MergeDecision::Start { reason, .. } => panic!(
            "an unchanged window split with reason {reason:?}; the merge boundary fired where \
             nothing changed on screen"
        ),
    };
    assert_eq!(
        merged_event.sample_count, 2,
        "two samples of one unchanged window must form a single two-sample event"
    );

    drop(first);

    // --- Window B -----------------------------------------------------------
    let second = launch("SCREENPIPE BOUNDARY BRAVO", "QUARTZFJORD").expect("launch bravo");
    assert!(
        wait_for_title("SCREENPIPE BOUNDARY BRAVO", Duration::from_secs(45)).await,
        "bravo fixture never became the pinned foreground window"
    );

    let bravo = observe().await;
    assert_ne!(
        bravo.window_title, alpha_one.window_title,
        "the two fixtures must present different window titles for this test to mean anything"
    );

    let decision =
        merger.ingest_with_metadata(bravo.clone(), idle_cadence(), CaptureGapSummary::default());
    drop(second);

    // The boundary must land HERE, at the real window change - and for the
    // right reason. Both fixtures are hosted by powershell.exe, so the app key
    // is identical and the honest reason is the window title change.
    let (reason, event) = match decision {
        MergeDecision::Start { reason, event } => (reason, event),
        MergeDecision::Merge { .. } => panic!(
            "the merger folded a genuinely different window into the previous event; \
             no boundary was detected where the screen actually changed"
        ),
    };
    assert_eq!(
        reason,
        SplitReason::WindowTitleChange,
        "expected the boundary to be attributed to the window title change"
    );
    assert_eq!(
        event.sample_count, 1,
        "the new event must start fresh at the boundary"
    );
    assert!(
        event.latest.window_title.contains("BRAVO"),
        "the new event must carry the new window's title, got {:?}",
        event.latest.window_title
    );
}
