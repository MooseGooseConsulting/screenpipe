use crate::{
    XCapError, XCapResult,
    platform::{impl_monitor::ImplMonitor, impl_window::ImplWindow},
};

use image::RgbaImage;

fn check_capture_region(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    monitor_width: u32,
    monitor_height: u32,
) -> XCapResult<()> {
    // Validate region bounds
    if width > monitor_width
        || height > monitor_height
        || x + width > monitor_width
        || y + height > monitor_height
    {
        return Err(XCapError::InvalidCaptureRegion(format!(
            "Region ({x}, {y}, {width}, {height}) is outside monitor size ({monitor_width}, {monitor_height})"
        )));
    }
    Ok(())
}

#[cfg(feature = "wgc")]
pub(super) fn capture_monitor(
    monitor: &ImplMonitor,
    x: Option<u32>,
    y: Option<u32>,
    width: Option<u32>,
    height: Option<u32>,
) -> XCapResult<RgbaImage> {
    use super::wgc;
    if let (Some(x), Some(y), Some(width), Some(height)) = (x, y, width, height) {
        let monitor_width = monitor.width()?;
        let monitor_height = monitor.height()?;
        check_capture_region(x, y, width, height, monitor_width, monitor_height)?;
        wgc::capture_monitor(monitor.h_monitor, x, y, width, height)
    } else {
        wgc::capture_monitor(monitor.h_monitor, 0, 0, monitor.width()?, monitor.height()?)
    }
}

#[cfg(not(feature = "wgc"))]
pub(super) fn capture_monitor(
    monitor: &ImplMonitor,
    x: Option<u32>,
    y: Option<u32>,
    width: Option<u32>,
    height: Option<u32>,
) -> XCapResult<RgbaImage> {
    use super::gdi;

    let monitor_x = monitor.x()?;
    let monitor_y = monitor.y()?;
    let monitor_width = monitor.width()?;
    let monitor_height = monitor.height()?;

    if let (Some(x), Some(y), Some(width), Some(height)) = (x, y, width, height) {
        check_capture_region(x, y, width, height, monitor_width, monitor_height)?;

        // Calculate absolute coordinates
        let abs_x = monitor_x + x as i32;
        let abs_y = monitor_y + y as i32;

        gdi::capture_monitor(abs_x, abs_y, width as i32, height as i32)
    } else {
        gdi::capture_monitor(
            monitor_x,
            monitor_y,
            monitor_width as i32,
            monitor_height as i32,
        )
    }
}

#[cfg(feature = "wgc")]
pub(super) fn capture_window(window: &ImplWindow) -> XCapResult<RgbaImage> {
    use super::wgc;

    wgc::capture_window(window.hwnd)
}

#[cfg(not(feature = "wgc"))]
pub(super) fn capture_window(window: &ImplWindow) -> XCapResult<RgbaImage> {
    use super::gdi;

    gdi::capture_window(window.hwnd)
}
