//! Clipboard text boundary.
//!
//! Reads Unicode text the operator copied, and nothing else. Three rules hold
//! this module together, and every one of them exists because the clipboard is
//! the most privacy-sensitive surface in this system:
//!
//! 1. Text only. `CF_UNICODETEXT` or nothing - an image, a file list, a rich
//!    document, a private application format are all ignored entirely.
//! 2. The exclusion formats decide first. If the application that owns the
//!    clipboard asked not to be recorded, this module never touches the text
//!    at all.
//! 3. Nothing here logs, formats, or `Debug`s the text. [`ClipboardRead`]
//!    carries it in one variant and `as_code` returns a fixed category for
//!    every variant including that one.

use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, IsClipboardFormatAvailable,
    OpenClipboard, RegisterClipboardFormatW,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::core::PCWSTR;

/// `CF_UNICODETEXT`. Declared here rather than pulled from
/// `Win32::System::Ole`, which is a large module this crate needs for nothing
/// else; the same local-constant pattern as `DESKTOP_SWITCHDESKTOP` and
/// `PROCESS_QUERY_LIMITED_INFORMATION` elsewhere in this crate.
const CF_UNICODETEXT: u32 = 13;

/// Maximum allocation inspected for one copied Unicode-text generation.
const MAX_CLIPBOARD_ALLOCATION_BYTES: usize = 16 * 1024 * 1024;

/// Set by an application that wants its clipboard content left alone by
/// monitors and recorders. Presence alone is the refusal - the format is
/// documented as carrying no meaningful value.
const EXCLUDE_FROM_MONITOR_PROCESSING: &str = "ExcludeClipboardContentFromMonitorProcessing";

/// `0` means "keep this out of clipboard history". Password managers set it.
const CAN_INCLUDE_IN_CLIPBOARD_HISTORY: &str = "CanIncludeInClipboardHistory";

/// `0` means "do not send this to the cloud clipboard". Password managers set
/// it alongside the history format.
const CAN_UPLOAD_TO_CLOUD_CLIPBOARD: &str = "CanUploadToCloudClipboard";

/// The three format names this boundary honours, in the order they are
/// checked. Public so a test can register the real names rather than a
/// hand-copied spelling of them - a typo here would silently disable the
/// refusal and no assertion against a private constant could see it.
pub const CLIPBOARD_EXCLUSION_FORMATS: [&str; 3] = [
    EXCLUDE_FROM_MONITOR_PROCESSING,
    CAN_INCLUDE_IN_CLIPBOARD_HISTORY,
    CAN_UPLOAD_TO_CLOUD_CLIPBOARD,
];

/// What one of the DWORD-valued permission formats says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FormatPermission {
    /// The format is not on the clipboard: the application said nothing.
    #[default]
    Absent,
    /// Present, carrying this DWORD. `0` is a refusal, anything else is
    /// permission.
    Value(u32),
    /// Present, but the value could not be read.
    ///
    /// Treated as a refusal by [`capture_is_permitted`]. An application put a
    /// permission format on the clipboard and we could not hear what it said;
    /// the only safe reading of that is the restrictive one.
    Unreadable,
}

/// What the exclusion formats say about one clipboard generation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClipboardExclusions {
    /// `ExcludeClipboardContentFromMonitorProcessing` is on the clipboard.
    pub exclude_from_monitor_processing: bool,
    pub can_include_in_history: FormatPermission,
    pub can_upload_to_cloud: FormatPermission,
}

/// Whether this clipboard generation may be captured at all.
///
/// A pure function on purpose. This is the single decision that keeps a
/// password out of the corpus, and it must be provable without a password
/// manager, a clipboard, or a desktop - the Win32 side above it only fills in
/// the struct.
pub fn capture_is_permitted(exclusions: ClipboardExclusions) -> bool {
    if exclusions.exclude_from_monitor_processing {
        return false;
    }
    permits(exclusions.can_include_in_history) && permits(exclusions.can_upload_to_cloud)
}

fn permits(permission: FormatPermission) -> bool {
    match permission {
        FormatPermission::Absent => true,
        FormatPermission::Value(value) => value != 0,
        FormatPermission::Unreadable => false,
    }
}

/// What one poll of the clipboard found.
///
/// `Text` is the only variant carrying content, and the only one the channel
/// turns into an event. The rest are categories, and `as_code` is what reaches
/// a log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardRead {
    /// The sequence number has not moved since the last poll, or this is the
    /// first poll and there is no "since" yet.
    Unchanged,
    /// A new generation carrying capturable Unicode text.
    Text(String),
    /// A new generation the exclusion formats forbid capturing. Nothing was
    /// read from it.
    Excluded,
    /// A new generation with no Unicode text in it: an image, files, or text
    /// that is entirely whitespace.
    NoText,
    /// The clipboard could not be opened or read this poll - most often
    /// because another process holds it open.
    ///
    /// The generation is deliberately NOT consumed, so the next poll sees it
    /// again and a transient lock costs a tick rather than an observation.
    Unavailable,
}

impl ClipboardRead {
    /// Fixed category code. Never derived from the content, so it is safe to
    /// log for every variant.
    pub const fn as_code(&self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Text(_) => "text",
            Self::Excluded => "excluded",
            Self::NoText => "no_text",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Polls `GetClipboardSequenceNumber` and reads the text when it moves.
///
/// # Why polling, and not `AddClipboardFormatListener`
///
/// The listener API delivers `WM_CLIPBOARDUPDATE` to a window, which means a
/// message pump on a dedicated thread and a hidden window to own it. This
/// process has neither, and the run loop it would have to feed is an async
/// loop that already ticks on a cadence. `GetClipboardSequenceNumber` is a
/// counter read that needs no window, no pump, and no thread affinity, and the
/// only thing the listener would buy is knowing about copies that happened
/// between two ticks - which this channel does not want anyway, since it
/// records what the clipboard HELD, not every transition it passed through.
///
/// # A locked workstation
///
/// Measured on this machine with the screen locked: the sequence number is
/// still readable, and `OpenClipboard` is refused with `ERROR_ACCESS_DENIED`
/// on every attempt. That combination is why this polls the counter first and
/// opens the clipboard second. Nothing is copied while a machine is locked, so
/// the counter does not move, so the clipboard is never opened and a locked
/// night produces no work and no diagnostics at all.
///
/// A copy made in the seconds before the lock is the one case that reaches the
/// refused `OpenClipboard`. It comes back [`ClipboardRead::Unavailable`],
/// which does NOT consume the generation, so the observation survives the lock
/// and is captured on the first poll after the machine is unlocked.
#[derive(Clone, Debug, Default)]
pub struct ClipboardWatcher {
    last_sequence: Option<u32>,
}

impl ClipboardWatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the clipboard if, and only if, it has changed since the last poll.
    pub fn poll(&mut self) -> ClipboardRead {
        let sequence = unsafe { GetClipboardSequenceNumber() };
        let previous = self.last_sequence.replace(sequence);
        if !should_read(sequence, previous) {
            return ClipboardRead::Unchanged;
        }

        match read_current_generation() {
            Ok(read) => read,
            Err(_) => {
                // Put the sequence back so this generation is retried rather
                // than skipped. The error itself is dropped here on purpose:
                // it comes from a boundary holding the clipboard's contents,
                // and the caller gets a fixed category instead.
                self.last_sequence = previous;
                ClipboardRead::Unavailable
            }
        }
    }
}

/// Whether a poll should read the clipboard's contents.
///
/// The first poll of a process never reads. The clipboard at startup holds
/// whatever was copied before this channel existed - possibly hours before,
/// possibly by a program that has since exited - and capturing it would mean
/// every service restart re-recorded the same stale content as a new
/// observation. The first poll learns where the clipboard is; only changes
/// observed while running are captured.
fn should_read(sequence: u32, previous: Option<u32>) -> bool {
    matches!(previous, Some(previous) if previous != sequence)
}

/// Holds the clipboard open for as long as it is alive.
///
/// A guard rather than paired calls because everything between the open and
/// the close touches clipboard contents, and an early return that skipped
/// `CloseClipboard` would leave the clipboard locked against every other
/// application on the desktop.
struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> Result<Self> {
        // `HWND(null)` associates the clipboard with the current task rather
        // than with a window. This process has no window to offer.
        unsafe { OpenClipboard(HWND(std::ptr::null_mut())) }
            .context("open the Windows clipboard")?;
        Ok(Self)
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

fn read_current_generation() -> Result<ClipboardRead> {
    // Registered before the clipboard is opened: if the format ids cannot be
    // resolved, the exclusion check cannot be made, and a generation whose
    // permissions are unknowable must not be read.
    let formats = exclusion_format_ids()?;
    let _clipboard = ClipboardGuard::open()?;

    if !capture_is_permitted(read_exclusions(&formats)) {
        return Ok(ClipboardRead::Excluded);
    }

    // Only now is the text touched at all.
    let Some(text) = read_unicode_text()? else {
        return Ok(ClipboardRead::NoText);
    };
    if text.trim().is_empty() {
        return Ok(ClipboardRead::NoText);
    }
    Ok(ClipboardRead::Text(text))
}

struct ExclusionFormatIds {
    exclude_from_monitor_processing: u32,
    can_include_in_history: u32,
    can_upload_to_cloud: u32,
}

fn exclusion_format_ids() -> Result<ExclusionFormatIds> {
    let [exclude, history, cloud] = CLIPBOARD_EXCLUSION_FORMATS;
    Ok(ExclusionFormatIds {
        exclude_from_monitor_processing: register_format(exclude)?,
        can_include_in_history: register_format(history)?,
        can_upload_to_cloud: register_format(cloud)?,
    })
}

/// Resolve a clipboard format name to its id, registering it if no application
/// has yet.
///
/// Registration is process-independent and idempotent: the id is the same one
/// the password manager's own `RegisterClipboardFormat` call returns.
fn register_format(name: &str) -> Result<u32> {
    let wide = name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let id = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
    if id == 0 {
        bail!("cannot register clipboard format {name}");
    }
    Ok(id)
}

/// Requires the clipboard to be open.
fn read_exclusions(formats: &ExclusionFormatIds) -> ClipboardExclusions {
    ClipboardExclusions {
        exclude_from_monitor_processing: format_is_available(
            formats.exclude_from_monitor_processing,
        ),
        can_include_in_history: read_permission(formats.can_include_in_history),
        can_upload_to_cloud: read_permission(formats.can_upload_to_cloud),
    }
}

fn format_is_available(format: u32) -> bool {
    unsafe { IsClipboardFormatAvailable(format) }.is_ok()
}

fn read_permission(format: u32) -> FormatPermission {
    if !format_is_available(format) {
        return FormatPermission::Absent;
    }
    match read_global_dword(format) {
        Some(value) => FormatPermission::Value(value),
        None => FormatPermission::Unreadable,
    }
}

fn read_global_dword(format: u32) -> Option<u32> {
    let handle = unsafe { GetClipboardData(format) }.ok()?;
    let global = HGLOBAL(handle.0);
    let locked = LockedGlobal::lock(global)?;
    if locked.size < size_of::<u32>() {
        return None;
    }
    // Unaligned: the clipboard's memory is whatever the owning application
    // allocated, and nothing promises a DWORD-aligned block.
    Some(unsafe { std::ptr::read_unaligned(locked.pointer as *const u32) })
}

/// Requires the clipboard to be open. `Ok(None)` means the clipboard holds no
/// Unicode text at all, which is not an error - it is an image, or files.
fn read_unicode_text() -> Result<Option<String>> {
    if !format_is_available(CF_UNICODETEXT) {
        return Ok(None);
    }
    let handle: HANDLE =
        unsafe { GetClipboardData(CF_UNICODETEXT) }.context("read clipboard Unicode text")?;
    let global = HGLOBAL(handle.0);
    // Check the advertised allocation before taking a lock or allocating a
    // Rust string. The NUL terminator cannot make a hostile over-allocation
    // safe: `GlobalSize` describes the whole Win32 block we would scan.
    if unsafe { GlobalSize(global) } > MAX_CLIPBOARD_ALLOCATION_BYTES {
        return Ok(None);
    }
    let Some(locked) = LockedGlobal::lock(global) else {
        bail!("cannot lock the clipboard's text buffer");
    };

    // `GlobalSize` is the ALLOCATION, which may be larger than the string, and
    // the string is NUL-terminated inside it. Bounded by the allocation and
    // stopped at the first NUL: a buffer that is not terminated must not read
    // past the block, and a buffer with slack must not carry it into the text.
    let units = locked.size / size_of::<u16>();
    let wide = unsafe { std::slice::from_raw_parts(locked.pointer as *const u16, units) };
    let end = wide.iter().position(|unit| *unit == 0).unwrap_or(units);
    Ok(Some(
        std::ffi::OsString::from_wide(&wide[..end])
            .to_string_lossy()
            .into_owned(),
    ))
}

struct LockedGlobal {
    global: HGLOBAL,
    pointer: *mut core::ffi::c_void,
    size: usize,
}

impl LockedGlobal {
    fn lock(global: HGLOBAL) -> Option<Self> {
        let pointer = unsafe { GlobalLock(global) };
        if pointer.is_null() {
            return None;
        }
        let size = unsafe { GlobalSize(global) };
        if size == 0 {
            let _ = unsafe { GlobalUnlock(global) };
            return None;
        }
        Some(Self {
            global,
            pointer,
            size,
        })
    }
}

impl Drop for LockedGlobal {
    fn drop(&mut self) {
        // `GlobalUnlock` reports failure when the lock count reaches zero,
        // which is the successful case here, so its result says nothing worth
        // reading.
        let _ = unsafe { GlobalUnlock(self.global) };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CLIPBOARD_EXCLUSION_FORMATS, ClipboardExclusions, ClipboardRead, FormatPermission,
        capture_is_permitted, should_read,
    };

    #[test]
    fn a_clipboard_nobody_refused_is_captured() {
        assert!(capture_is_permitted(ClipboardExclusions::default()));
        assert!(capture_is_permitted(ClipboardExclusions {
            exclude_from_monitor_processing: false,
            can_include_in_history: FormatPermission::Value(1),
            can_upload_to_cloud: FormatPermission::Value(1),
        }));
    }

    #[test]
    fn every_refusal_alone_is_enough_to_stop_a_capture() {
        // Each format is tested in isolation, with the other two saying yes.
        // Tested together, deleting any single check would still pass.
        let refusals = [
            ClipboardExclusions {
                exclude_from_monitor_processing: true,
                can_include_in_history: FormatPermission::Value(1),
                can_upload_to_cloud: FormatPermission::Value(1),
            },
            ClipboardExclusions {
                exclude_from_monitor_processing: false,
                can_include_in_history: FormatPermission::Value(0),
                can_upload_to_cloud: FormatPermission::Value(1),
            },
            ClipboardExclusions {
                exclude_from_monitor_processing: false,
                can_include_in_history: FormatPermission::Value(1),
                can_upload_to_cloud: FormatPermission::Value(0),
            },
        ];

        for refusal in refusals {
            assert!(
                !capture_is_permitted(refusal),
                "a refusal was overruled: {refusal:?}"
            );
        }
    }

    #[test]
    fn a_permission_that_cannot_be_read_is_a_refusal() {
        // An application put a permission format on the clipboard and we could
        // not hear what it said. Reading that as consent is how a password
        // ends up in the corpus.
        for exclusions in [
            ClipboardExclusions {
                can_include_in_history: FormatPermission::Unreadable,
                ..Default::default()
            },
            ClipboardExclusions {
                can_upload_to_cloud: FormatPermission::Unreadable,
                ..Default::default()
            },
        ] {
            assert!(
                !capture_is_permitted(exclusions),
                "an unreadable permission was treated as consent: {exclusions:?}"
            );
        }
    }

    #[test]
    fn only_zero_refuses_among_the_dword_values() {
        // The formats are documented as 0/1, but the value is whatever the
        // owning application wrote. Anything other than zero is permission.
        for value in [1_u32, 2, u32::MAX] {
            assert!(capture_is_permitted(ClipboardExclusions {
                can_include_in_history: FormatPermission::Value(value),
                can_upload_to_cloud: FormatPermission::Value(value),
                ..Default::default()
            }));
        }
    }

    #[test]
    fn the_first_poll_learns_the_sequence_without_reading_the_clipboard() {
        // Reading on the first poll would re-record whatever was on the
        // clipboard before this process started - on every service restart.
        assert!(!should_read(7, None));
        assert!(!should_read(0, None));
        // Unchanged is unchanged, however many times it is polled.
        assert!(!should_read(7, Some(7)));
        // A move in either direction is a new generation. The counter is
        // per-window-station and can be observed to jump or wrap.
        assert!(should_read(8, Some(7)));
        assert!(should_read(7, Some(8)));
    }

    #[test]
    fn every_read_outcome_has_its_own_fixed_category_and_none_carries_content() {
        let secret = "CLIPBOARD_SECRET_SENTINEL_4b2f";
        let reads = [
            ClipboardRead::Unchanged,
            ClipboardRead::Text(secret.to_owned()),
            ClipboardRead::Excluded,
            ClipboardRead::NoText,
            ClipboardRead::Unavailable,
        ];

        let codes = reads.each_ref().map(ClipboardRead::as_code);
        let mut unique = codes.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "duplicate clipboard read code");
        for code in codes {
            assert!(!code.is_empty());
            assert!(
                !code.contains(secret),
                "a read category carried clipboard content: {code}"
            );
        }
    }

    #[test]
    fn the_exclusion_format_names_are_the_ones_windows_documents() {
        // These strings are the entire refusal mechanism. A typo in any of them
        // registers a format no application will ever set, so the refusal
        // becomes unreachable and nothing else in the system can tell.
        assert_eq!(
            CLIPBOARD_EXCLUSION_FORMATS,
            [
                "ExcludeClipboardContentFromMonitorProcessing",
                "CanIncludeInClipboardHistory",
                "CanUploadToCloudClipboard",
            ]
        );
    }
}
