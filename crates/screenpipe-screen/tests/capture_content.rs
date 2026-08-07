#![cfg(target_os = "windows")]
//! Capture and OCR **content** correctness against a controlled live window.
//!
//! These tests are deliberately NOT `#[ignore]`d. An ignored test never runs in
//! the session where it is meaningful, so it protects nothing. Instead they
//! probe for the capability they need and skip *loudly* when the desktop is
//! locked - and hard-fail instead of skipping when
//! `SCREEN_MEMORY_REQUIRE_INTERACTIVE` is set, which is how a qualification run
//! turns "skipped" into "failed".
//!
//! Every assertion here is about content, not liveness. `width() > 0` and
//! `!app_key.is_empty()` pass just as well for a frame of the wrong window.

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use screenpipe_screen::{ForegroundMetadata, TransientFrame, WindowsCapture, WindowsOcr};

mod common;
use common::{interactive_desktop_available, lock_foreground};

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> *mut c_void;
    fn GetWindowThreadProcessId(window: *mut c_void, process_id: *mut u32) -> u32;
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

/// Resolve the foreground window's executable name **independently** of the
/// capture path. `capture_foreground` takes its process id from xcap's window
/// enumeration; this takes it straight from the HWND, so the two agreeing is
/// real evidence rather than a restatement.
fn independent_foreground_exe_name() -> Option<String> {
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

/// Distinct, OCR-unambiguous tokens. No characters that Windows OCR routinely
/// confuses (0/O, 1/l/I), and no dictionary words that a language model inside
/// the OCR engine might "correct" into something else.
const TOKENS: [&str; 3] = ["ZEBRAWALTZ", "QUARTZFJORD", "GOALONEVISIONSMOKE"];

/// A window whose width and height differ substantially, so a width/height
/// swap in the capture path is detectable.
const FIXTURE_WIDTH: u32 = 900;
const FIXTURE_HEIGHT: u32 = 560;

struct FixtureWindow {
    child: Child,
}

impl Drop for FixtureWindow {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Launch a PowerShell WinForms window rendering `TOKENS` as large black text
/// on white. Owning the fixture is what makes content assertions possible: we
/// know exactly what is on screen, so OCR can be checked against it.
fn launch_fixture_window() -> std::io::Result<FixtureWindow> {
    let script = format!(
        r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$form = New-Object System.Windows.Forms.Form
$form.Text = 'SCREENPIPE CAPTURE FIXTURE'
$form.StartPosition = 'CenterScreen'
$form.ClientSize = New-Object System.Drawing.Size({width}, {height})
$form.BackColor = [System.Drawing.Color]::White
$form.FormBorderStyle = 'FixedSingle'
$form.TopMost = $true
$label = New-Object System.Windows.Forms.Label
$label.Font = New-Object System.Drawing.Font('Segoe UI', 40, [System.Drawing.FontStyle]::Bold)
$label.ForeColor = [System.Drawing.Color]::Black
$label.AutoSize = $false
$label.Dock = 'Fill'
$label.TextAlign = 'MiddleCenter'
$label.Text = "{tokens}"
$form.Controls.Add($label)
$form.Add_Shown({{ $form.Activate(); $form.BringToFront() }})
[System.Windows.Forms.Application]::Run($form)
"#,
        width = FIXTURE_WIDTH,
        height = FIXTURE_HEIGHT,
        tokens = TOKENS.join("`n"),
    );

    let child = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-STA",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .spawn()?;
    Ok(FixtureWindow { child })
}

/// Wait until the fixture window owns the foreground, identified by the
/// executable that hosts it.
fn wait_for_fixture_foreground(timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(exe) = independent_foreground_exe_name()
            && exe == "powershell.exe"
        {
            // Give the window one more beat to finish painting its text.
            std::thread::sleep(Duration::from_millis(600));
            return Some(exe);
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    None
}

fn normalize_ocr(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase()
}

async fn capture_fixture() -> Option<(TransientFrame, ForegroundMetadata, String)> {
    let fixture = launch_fixture_window().expect("failed to launch the capture fixture window");
    let Some(expected_exe) = wait_for_fixture_foreground(Duration::from_secs(20)) else {
        drop(fixture);
        panic!(
            "the capture fixture never reached the foreground within 20s; \
             another window is holding focus"
        );
    };

    let result = WindowsCapture.capture_foreground().await;
    // Keep the fixture alive across the capture, then drop it deterministically.
    drop(fixture);

    let (frame, metadata) = result.expect("foreground capture of the pinned fixture failed");
    Some((frame, metadata, expected_exe))
}

#[tokio::test(flavor = "current_thread")]
async fn ocr_reads_the_exact_tokens_drawn_on_the_pinned_foreground_window() {
    if !interactive_desktop_available(
        "ocr_reads_the_exact_tokens_drawn_on_the_pinned_foreground_window",
    ) {
        return;
    }

    // One foreground window, many test binaries: serialize.
    let _foreground = lock_foreground();

    let Some((frame, _metadata, _exe)) = capture_fixture().await else {
        return;
    };

    let ocr_text = WindowsOcr
        .recognize(&frame)
        .await
        .expect("Windows OCR failed on the pinned fixture frame");
    let normalized = normalize_ocr(&ocr_text);

    for token in TOKENS {
        assert!(
            normalized.contains(token),
            "OCR did not read the token {token} that was drawn on screen. \
             Normalized OCR was: {normalized}"
        );
    }

    // A frame of the wrong window, or an empty frame, would fail the loop
    // above. This guards the opposite failure: OCR returning a giant blob that
    // happens to contain the tokens among unrelated screen content.
    assert!(
        normalized.len() < 400,
        "OCR returned far more text than the fixture drew, so the captured \
         frame probably was not the fixture window: {normalized}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn app_attribution_matches_the_real_foreground_process() {
    if !interactive_desktop_available("app_attribution_matches_the_real_foreground_process") {
        return;
    }

    // One foreground window, many test binaries: serialize.
    let _foreground = lock_foreground();

    let Some((_frame, metadata, expected_exe)) = capture_fixture().await else {
        return;
    };

    // `metadata.app_key` is derived from xcap's window enumeration; expected_exe
    // comes straight from GetWindowThreadProcessId on the foreground HWND. Two
    // independent paths must name the same process.
    assert_eq!(
        metadata.app_key, expected_exe,
        "capture attributed the frame to {} but the real foreground process was {}",
        metadata.app_key, expected_exe
    );
    assert!(
        metadata.window_title.contains("SCREENPIPE CAPTURE FIXTURE"),
        "captured window title was {:?}, not the fixture's",
        metadata.window_title
    );
    assert_ne!(metadata.window_handle, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn captured_frame_geometry_is_not_transposed() {
    if !interactive_desktop_available("captured_frame_geometry_is_not_transposed") {
        return;
    }

    // One foreground window, many test binaries: serialize.
    let _foreground = lock_foreground();

    let Some((frame, _metadata, _exe)) = capture_fixture().await else {
        return;
    };

    // The fixture is deliberately wider than it is tall. A width/height swap in
    // the WGC path - the exact class of bug that produced the "ROI out of
    // bounds" timeout - inverts this.
    assert!(
        frame.width() > frame.height(),
        "captured frame is {}x{} but the fixture window is {FIXTURE_WIDTH}x{FIXTURE_HEIGHT}; \
         width and height appear transposed",
        frame.width(),
        frame.height()
    );

    // Absolute pixel bounds are the wrong assertion here: WinForms `ClientSize`
    // is in logical units, so on a 200%-scaled display the real window is twice
    // the requested size. Aspect ratio is scale-invariant, which is exactly
    // what a transposition check needs.
    let fixture_aspect = f64::from(FIXTURE_WIDTH) / f64::from(FIXTURE_HEIGHT);
    let captured_aspect = f64::from(frame.width()) / f64::from(frame.height());
    assert!(
        (captured_aspect - fixture_aspect).abs() < 0.35,
        "captured frame is {}x{} (aspect {captured_aspect:.3}) but the fixture is \
         {FIXTURE_WIDTH}x{FIXTURE_HEIGHT} (aspect {fixture_aspect:.3}); window chrome \
         alone cannot account for that difference",
        frame.width(),
        frame.height()
    );

    // Deliberately no "smaller than the display" assertion here. This test
    // process is not per-monitor DPI aware, so GetSystemMetrics reports
    // virtualized logical pixels (1200x800) while the captured frame is in
    // physical pixels (1804x1184) - comparing them comes out backwards and
    // proves nothing. That the frame is the fixture window rather than a
    // whole-desktop grab is already established by the aspect-ratio assertion
    // above and by the OCR text-volume bound in
    // `ocr_reads_the_exact_tokens_drawn_on_the_pinned_foreground_window`.
}
