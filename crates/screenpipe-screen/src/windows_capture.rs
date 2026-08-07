use anyhow::{Context, Result, bail, ensure};
use xcap::Window;

use crate::windows_metadata::{foreground_window_handle, metadata_for_window};
use crate::{ForegroundMetadata, TransientFrame};

/// Foreground-only Windows Graphics Capture adapter retained from Screenpipe's
/// pinned xcap/WGC window path.
pub struct WindowsCapture;

impl WindowsCapture {
    pub fn preflight_interactive(&self) -> Result<()> {
        let active_session_id = unsafe { wts_get_active_console_session_id() };
        let process_id = unsafe { get_current_process_id() };
        let mut process_session_id = 0;
        let succeeded = unsafe { process_id_to_session_id(process_id, &mut process_session_id) };
        if succeeded == 0 {
            return Err(std::io::Error::last_os_error())
                .context("resolve current process Windows session");
        }
        validate_interactive_session(active_session_id, process_session_id)
    }

    pub fn foreground_window_handle(&self) -> Result<isize> {
        foreground_window_handle()
    }

    pub async fn capture_foreground(&self) -> Result<(TransientFrame, ForegroundMetadata)> {
        tokio::task::spawn_blocking(capture_foreground_blocking)
            .await
            .context("foreground capture worker failed")?
    }
}

fn validate_interactive_session(active_session_id: u32, process_session_id: u32) -> Result<()> {
    ensure!(
        active_session_id != u32::MAX,
        "Windows has no active console session"
    );
    ensure!(
        process_session_id == active_session_id,
        "process is not running in the active interactive Windows session"
    );
    Ok(())
}

#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "WTSGetActiveConsoleSessionId"]
    fn wts_get_active_console_session_id() -> u32;
    #[link_name = "GetCurrentProcessId"]
    fn get_current_process_id() -> u32;
    #[link_name = "ProcessIdToSessionId"]
    fn process_id_to_session_id(process_id: u32, session_id: *mut u32) -> i32;
}

fn capture_foreground_blocking() -> Result<(TransientFrame, ForegroundMetadata)> {
    let handle = foreground_window_handle()?;
    let window_id = handle as usize as u32;
    let windows = Window::all().context("enumerate capturable Windows windows")?;
    let candidate_count = windows.len();
    let focused_candidates = windows
        .iter()
        .filter(|window| window.is_focused().unwrap_or(false))
        .filter_map(|window| window.id().ok())
        .collect::<Vec<_>>();
    let window = windows
        .into_iter()
        .find(|window| window.id().ok() == Some(window_id))
        .with_context(|| {
            format!(
                "foreground HWND 0x{handle:x} is absent from {} capturable windows; xcap focused candidates: {focused_candidates:?}",
                candidate_count
            )
        })?;

    let process_id = window.pid().context("read foreground process id")?;
    let display_name = window
        .app_name()
        .context("read foreground application display name")?;
    let window_title = window.title().context("read foreground window title")?;
    if window_title.trim().is_empty() {
        bail!("foreground window title is empty");
    }
    let image = window
        .capture_image()
        .context("capture foreground window through Windows Graphics Capture")?;
    if foreground_window_handle()? != handle {
        bail!("foreground window changed during capture");
    }

    let width = image.width();
    let height = image.height();
    let stride = width
        .checked_mul(4)
        .context("foreground frame stride overflow")?;
    let rgba = image.into_raw();
    let mut bgra = Vec::with_capacity(rgba.len());
    for pixel in rgba.chunks_exact(4) {
        bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    let frame = TransientFrame::from_bgra(width, height, stride, bgra)?;
    let metadata = metadata_for_window(handle, process_id, display_name, window_title)?;
    Ok((frame, metadata))
}

#[cfg(test)]
mod session_tests {
    use super::validate_interactive_session;

    #[test]
    fn active_process_session_is_interactive_without_a_foreground_window() {
        validate_interactive_session(1, 1).unwrap();
    }

    #[test]
    fn missing_or_different_active_console_session_is_rejected() {
        // `(u32::MAX, 1)` is rejected by the session-equality guard alone, so
        // it does not reach the no-console-session guard at all - that guard
        // could be deleted outright and this test stayed green. The MAX/MAX
        // case is the only fixture that isolates it: the session ids match,
        // so only the sentinel check can reject it.
        let no_console = validate_interactive_session(u32::MAX, u32::MAX).unwrap_err();
        assert!(
            format!("{no_console:#}").contains("no active console session"),
            "expected the no-console-session guard to reject MAX/MAX, got: {no_console:#}"
        );

        let mismatched = validate_interactive_session(2, 1).unwrap_err();
        assert!(
            format!("{mismatched:#}").contains("not running in the active interactive"),
            "expected the session-mismatch guard, got: {mismatched:#}"
        );

        assert!(validate_interactive_session(u32::MAX, 1).is_err());
    }
}
