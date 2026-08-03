use anyhow::{Context, Result, bail};
use xcap::Window;

use crate::windows_metadata::{foreground_window_handle, metadata_for_window};
use crate::{ForegroundMetadata, TransientFrame};

/// Foreground-only Windows Graphics Capture adapter retained from Screenpipe's
/// pinned xcap/WGC window path.
pub struct WindowsCapture;

impl WindowsCapture {
    pub fn foreground_window_handle(&self) -> Result<isize> {
        foreground_window_handle()
    }

    pub async fn capture_foreground(&self) -> Result<(TransientFrame, ForegroundMetadata)> {
        tokio::task::spawn_blocking(capture_foreground_blocking)
            .await
            .context("foreground capture worker failed")?
    }
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
