use std::ffi::c_void;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::ForegroundMetadata;

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetForegroundWindow() -> *mut c_void;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
    fn CloseHandle(object: *mut c_void) -> i32;
    fn QueryFullProcessImageNameW(
        process: *mut c_void,
        flags: u32,
        filename: *mut u16,
        size: *mut u32,
    ) -> i32;
}

pub(crate) fn foreground_window_handle() -> Result<isize> {
    let handle = unsafe { GetForegroundWindow() };
    if handle.is_null() {
        bail!("no foreground window is available in the interactive session");
    }
    Ok(handle as isize)
}

pub(crate) fn metadata_for_window(
    handle: isize,
    process_id: u32,
    display_name: String,
    window_title: String,
) -> Result<ForegroundMetadata> {
    let process_path = process_image_path(process_id)?;
    let app_key = Path::new(&process_path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_lowercase)
        .filter(|name| !name.is_empty())
        .context("foreground process path has no executable name")?;
    let app_title = if display_name.trim().is_empty() {
        app_key.clone()
    } else {
        display_name
    };

    Ok(ForegroundMetadata {
        window_handle: handle,
        app_key,
        app_title,
        window_title,
        browser_url: None,
    })
}

fn process_image_path(process_id: u32) -> Result<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        bail!("cannot open foreground process {process_id} for metadata");
    }

    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    let query_result =
        unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length) };
    unsafe {
        CloseHandle(process);
    }
    if query_result == 0 || length == 0 {
        bail!("cannot read executable path for foreground process {process_id}");
    }
    buffer.truncate(length as usize);
    Ok(std::ffi::OsString::from_wide(&buffer)
        .to_string_lossy()
        .into_owned())
}
