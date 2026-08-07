#![cfg(target_os = "windows")]
//! The clipboard boundary against the real Windows clipboard.
//!
//! The exclusion formats are the mechanism that keeps a password manager's
//! copy out of the corpus, and a unit test cannot prove it. The unit tests
//! beside `capture_is_permitted` prove the DECISION; this proves the decision
//! is actually reached from what an application put on the real clipboard -
//! that the format names resolve to the ids Windows hands out, that the values
//! are read from the owning application's own memory, and that the text is
//! never returned when a refusal is present.
//!
//! A password manager cannot be installed to write those formats, so this test
//! registers and sets them itself, through the same `RegisterClipboardFormatW`
//! every other process uses. Format ids are window-station-wide, so the format
//! this test sets is byte-for-byte the format 1Password sets.
//!
//! # This test writes to the operator's clipboard
//!
//! There is no other way to put a synthetic format on it. The Unicode text
//! that was on the clipboard when the test started is written back at the end.
//! Non-text contents - an image, a file list - cannot be restored and are
//! lost. The alternative was `#[ignore]`, which would mean the refusal path is
//! never exercised at all, on a machine that records everything its operator
//! copies.
//!
//! # A locked workstation cannot open the clipboard at all
//!
//! Measured on this machine, session 1, `WinSta0\Default`, medium integrity,
//! with the workstation locked:
//!
//! ```text
//! WTSQuerySessionInformationW(WTSSessionInfoEx).SessionFlags -> 0  (LOCKED)
//! GetClipboardSequenceNumber()                               -> 1317 (readable)
//! OpenClipboard(hwnd)                       -> ERROR_ACCESS_DENIED, 40/40 tries
//! ```
//!
//! The sequence number stays readable while the text behind it does not, which
//! is what makes the production channel quiet rather than broken during a lock:
//! nothing is copied, so the counter never moves, so the clipboard is never
//! opened. This test needs to open it, so it gates on that capability and
//! skips - loudly, and only with the same opt-out the live-desktop tests use.

use std::sync::Mutex;

use screenpipe_screen::{CLIPBOARD_EXCLUSION_FORMATS, ClipboardRead, ClipboardWatcher};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::{PCWSTR, w};

const CF_UNICODETEXT: u32 = 13;

/// One clipboard per window station, and libtest runs a binary's tests
/// concurrently. Everything here happens inside one test function, but the
/// lock makes that a fact rather than a convention.
static CLIPBOARD: Mutex<()> = Mutex::new(());

/// A message-only window to own the clipboard.
///
/// Not optional. `OpenClipboard(NULL)` followed by `EmptyClipboard` leaves the
/// clipboard owner NULL, and Windows then fails every `SetClipboardData` - so
/// a test that wrote through a null owner would silently assert nothing. The
/// production reader opens with a null owner because it only ever READS.
///
/// `STATIC` is a system-global window class, which is what lets this skip
/// `RegisterClassW`, a window procedure, and a module handle.
struct OwnerWindow(HWND);

impl OwnerWindow {
    fn create() -> Self {
        let handle = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                PCWSTR::null(),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                None,
                None,
            )
        }
        .expect("create a message-only clipboard owner window");
        Self(handle)
    }
}

impl Drop for OwnerWindow {
    fn drop(&mut self) {
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

struct Clipboard;

impl Clipboard {
    fn open(owner: HWND) -> Self {
        Self::try_open(owner).expect("the clipboard was open a moment ago and is now denied")
    }

    fn try_open(owner: HWND) -> Option<Self> {
        // The clipboard is briefly held by whichever application last wrote to
        // it, so a single attempt is genuinely flaky here in a way it is not in
        // the production path - which does not retry at all, and simply looks
        // again on its next tick.
        for attempt in 0_u64..25 {
            if unsafe { OpenClipboard(owner) }.is_ok() {
                return Some(Self);
            }
            std::thread::sleep(std::time::Duration::from_millis(20 * (attempt + 1).min(5)));
        }
        None
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

/// Set this to downgrade an unopenable clipboard from a failure to a skip.
///
/// Deliberately the same variable the live-desktop gate uses, and deliberately
/// the same polarity: failing is the default, because a skip that is invisible
/// is how a boundary that stopped working goes unnoticed.
const ALLOW_SKIP_ENV: &str = "SCREEN_MEMORY_ALLOW_LOCKED_SKIP";

/// Returns `false` when the caller must return early.
#[must_use]
fn clipboard_is_accessible(owner: &OwnerWindow, test_name: &str) -> bool {
    if Clipboard::try_open(owner.0).is_some() {
        return true;
    }
    // `probe_interactive_capability` is deliberately not consulted here: it
    // answers `Available` on a locked workstation - that is the defect its own
    // doc comment records - so a gate built on it would drive this test into a
    // failure it cannot avoid. `session_is_locked` is the signal that sees it.
    let cause = match screenpipe_screen::session_is_locked() {
        Some(true) => {
            "the workstation is locked, so the clipboard is denied to the default desktop"
        }
        Some(false) => "the workstation is unlocked, so another process is holding the clipboard",
        None => "the session lock state could not be determined",
    };
    let banner = format!("SKIPPED {test_name}: the clipboard cannot be opened - {cause}");
    let rule = "!".repeat(78);
    eprintln!("\n{rule}\n!!!! {banner}\n{rule}\n");
    println!("{banner}");
    assert!(
        std::env::var_os(ALLOW_SKIP_ENV).is_some(),
        "{banner}. Set {ALLOW_SKIP_ENV}=1 to downgrade this to a skip."
    );
    false
}

fn format_id(name: &str) -> u32 {
    let wide = name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let id = unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) };
    assert_ne!(id, 0, "could not register clipboard format {name}");
    id
}

/// Hand a block of bytes to the clipboard under `format`.
///
/// Ownership of the allocation passes to the system on success, so it must not
/// be freed here.
fn set_format(format: u32, bytes: &[u8]) {
    unsafe {
        let global: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).expect("GlobalAlloc");
        let pointer = GlobalLock(global);
        assert!(!pointer.is_null(), "GlobalLock");
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
        let _ = GlobalUnlock(global);
        SetClipboardData(format, HANDLE(global.0)).expect("SetClipboardData");
    }
}

fn text_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// Replace the clipboard with `text` plus any extra formats.
fn write_clipboard(owner: &OwnerWindow, text: &str, extra: &[(u32, Vec<u8>)]) {
    let _clipboard = Clipboard::open(owner.0);
    unsafe { EmptyClipboard() }.expect("EmptyClipboard");
    set_format(CF_UNICODETEXT, &text_bytes(text));
    for (format, bytes) in extra {
        set_format(*format, bytes);
    }
}

/// Whatever Unicode text is on the clipboard right now, read directly rather
/// than through the watcher - the watcher refuses to read its first poll, by
/// design, which is the very thing this test asserts.
fn current_text(owner: &OwnerWindow) -> Option<String> {
    let _clipboard = Clipboard::try_open(owner.0)?;
    unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.ok()?;
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT) }.ok()?;
    let global = HGLOBAL(handle.0);
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return None;
    }
    let units = unsafe { GlobalSize(global) } / size_of::<u16>();
    let wide = unsafe { std::slice::from_raw_parts(pointer as *const u16, units) };
    let end = wide.iter().position(|unit| *unit == 0).unwrap_or(units);
    let text = String::from_utf16_lossy(&wide[..end]);
    let _ = unsafe { GlobalUnlock(global) };
    Some(text)
}

#[test]
fn the_exclusion_formats_stop_a_capture_before_the_text_is_read() {
    let _serialized = CLIPBOARD.lock().unwrap_or_else(|error| error.into_inner());
    let owner = OwnerWindow::create();
    if !clipboard_is_accessible(&owner, "the_exclusion_formats_stop_a_capture") {
        return;
    }
    let operators_text = current_text(&owner);

    let [exclude_monitor, can_include_history, can_upload_cloud] =
        CLIPBOARD_EXCLUSION_FORMATS.map(format_id);
    let mut watcher = ClipboardWatcher::new();

    // The first poll must not read, even though the clipboard just changed.
    // What is on it at startup is the operator's content from before this
    // process existed, and capturing it would re-record the same stale text on
    // every service restart.
    write_clipboard(&owner, "SCREENPIPE CLIPBOARD FIXTURE BASELINE", &[]);
    assert_eq!(
        watcher.poll(),
        ClipboardRead::Unchanged,
        "the first poll read the clipboard instead of learning where it was"
    );

    // Plain text is captured verbatim.
    let plain = "SCREENPIPE CLIPBOARD FIXTURE PLAIN TEXT";
    write_clipboard(&owner, plain, &[]);
    assert_eq!(watcher.poll(), ClipboardRead::Text(plain.to_owned()));

    // Nothing new: the sequence has not moved, so nothing is read again.
    assert_eq!(watcher.poll(), ClipboardRead::Unchanged);

    // Each refusal, alone, on a clipboard that also carries perfectly readable
    // text. Alone is what matters: tested together, deleting any single check
    // would still pass.
    let cases = [
        (
            "ExcludeClipboardContentFromMonitorProcessing",
            vec![(exclude_monitor, vec![0_u8; 4])],
        ),
        (
            "CanIncludeInClipboardHistory = 0",
            vec![(can_include_history, 0_u32.to_le_bytes().to_vec())],
        ),
        (
            "CanUploadToCloudClipboard = 0",
            vec![(can_upload_cloud, 0_u32.to_le_bytes().to_vec())],
        ),
        (
            "all three, as a password manager sets them",
            vec![
                (exclude_monitor, vec![0_u8; 4]),
                (can_include_history, 0_u32.to_le_bytes().to_vec()),
                (can_upload_cloud, 0_u32.to_le_bytes().to_vec()),
            ],
        ),
    ];

    for (index, (name, formats)) in cases.into_iter().enumerate() {
        // A distinct secret per case, so a stale `Text` from an earlier step
        // could never pass for a fresh one.
        let secret = format!("SCREENPIPE CLIPBOARD FIXTURE SECRET {index}");
        write_clipboard(&owner, &secret, &formats);

        let read = watcher.poll();

        assert_eq!(read, ClipboardRead::Excluded, "{name} did not exclude");
        assert!(
            !format!("{read:?}").contains("SECRET"),
            "{name} let the text through"
        );
    }

    // The permissive values are not a blanket refusal. Without this, a boundary
    // that refused everything would pass every case above.
    let allowed = "SCREENPIPE CLIPBOARD FIXTURE ALLOWED";
    write_clipboard(
        &owner,
        allowed,
        &[
            (can_include_history, 1_u32.to_le_bytes().to_vec()),
            (can_upload_cloud, 1_u32.to_le_bytes().to_vec()),
        ],
    );
    assert_eq!(watcher.poll(), ClipboardRead::Text(allowed.to_owned()));

    // A generation with no Unicode text in it is ignored rather than recorded
    // as an empty observation.
    {
        let _clipboard = Clipboard::open(owner.0);
        unsafe { EmptyClipboard() }.expect("EmptyClipboard");
        set_format(can_upload_cloud, &1_u32.to_le_bytes());
    }
    assert_eq!(watcher.poll(), ClipboardRead::NoText);

    // Neither is whitespace.
    write_clipboard(&owner, "   \t\r\n ", &[]);
    assert_eq!(watcher.poll(), ClipboardRead::NoText);

    // Give the operator their clipboard back.
    match operators_text {
        Some(text) if !text.is_empty() => write_clipboard(&owner, &text, &[]),
        _ => {
            let _clipboard = Clipboard::open(owner.0);
            unsafe { EmptyClipboard() }.expect("EmptyClipboard");
        }
    }
}
