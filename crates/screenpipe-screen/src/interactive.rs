//! Interactive-desktop capability probe.
//!
//! `WindowsCapture::preflight_interactive` proves only that this process runs
//! in the *active console session*. A locked workstation keeps the same session
//! id, so that check passes while the input desktop has switched to the
//! Winlogon secure desktop and no application window can be captured or read by
//! OCR. Capture tests that assert screen *content* are meaningless in that
//! state, and so is a capture tick.
//!
//! This probe distinguishes the two. It is deliberately library code rather
//! than test scaffolding: the runner needs the same distinction to record a
//! typed gap instead of an untyped capture error when the desktop is locked.

use std::ffi::c_void;

/// Why an interactive capture cannot produce meaningful screen content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotInteractive {
    /// No console session is attached (headless, or the session was detached).
    NoActiveConsoleSession,
    /// This process runs outside the active console session, so it addresses a
    /// different desktop than the human is using.
    WrongSession,
    /// The input desktop is not readable by this process. In practice: the
    /// workstation is locked, a secure-desktop UAC prompt is up, or a secure
    /// screensaver is active.
    DesktopLocked,
    /// The desktop is available but nothing holds the foreground.
    NoForegroundWindow,
}

impl NotInteractive {
    /// Stable, non-content category code. Safe to log and to persist as a gap.
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::NoActiveConsoleSession => "no_active_console_session",
            Self::WrongSession => "wrong_session",
            Self::DesktopLocked => "desktop_locked",
            Self::NoForegroundWindow => "no_foreground_window",
        }
    }

    /// Human-readable reason, for a loud test skip.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::NoActiveConsoleSession => "no active Windows console session is attached",
            Self::WrongSession => {
                "this process is not in the active console session, so it cannot see the user's desktop"
            }
            Self::DesktopLocked => {
                "the input desktop is not readable - the workstation is locked, or a secure desktop (UAC/screensaver) is in front"
            }
            Self::NoForegroundWindow => {
                "the desktop is unlocked but no window holds the foreground"
            }
        }
    }
}

/// Result of the interactive capability probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteractiveCapability {
    /// An unlocked interactive desktop with a foreground window is present.
    Available,
    Unavailable(NotInteractive),
}

impl InteractiveCapability {
    pub const fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }

    pub const fn unavailable_reason(self) -> Option<NotInteractive> {
        match self {
            Self::Available => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }
}

const DESKTOP_SWITCHDESKTOP: u32 = 0x0100;

#[link(name = "user32")]
unsafe extern "system" {
    fn OpenInputDesktop(flags: u32, inherit: i32, desired_access: u32) -> *mut c_void;
    fn CloseDesktop(desktop: *mut c_void) -> i32;
    fn GetForegroundWindow() -> *mut c_void;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn WTSGetActiveConsoleSessionId() -> u32;
    fn GetCurrentProcessId() -> u32;
    fn ProcessIdToSessionId(process_id: u32, session_id: *mut u32) -> i32;
}

/// Probe whether this process can capture meaningful screen content right now.
///
/// The order matters: session identity is checked before the desktop, because a
/// process in the wrong session would be told "locked" by `OpenInputDesktop`
/// even when the console user's desktop is perfectly usable.
pub fn probe_interactive_capability() -> InteractiveCapability {
    let active_session = unsafe { WTSGetActiveConsoleSessionId() };
    if active_session == u32::MAX {
        return InteractiveCapability::Unavailable(NotInteractive::NoActiveConsoleSession);
    }

    let mut process_session = 0_u32;
    let resolved = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut process_session) };
    if resolved == 0 || process_session != active_session {
        return InteractiveCapability::Unavailable(NotInteractive::WrongSession);
    }

    // A locked workstation switches the input desktop to Winlogon, which an
    // ordinary user process cannot open. This is the only reliable user-mode
    // lock signal that does not require a window-message hook.
    let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_SWITCHDESKTOP) };
    if desktop.is_null() {
        return InteractiveCapability::Unavailable(NotInteractive::DesktopLocked);
    }
    unsafe {
        CloseDesktop(desktop);
    }

    if unsafe { GetForegroundWindow() }.is_null() {
        return InteractiveCapability::Unavailable(NotInteractive::NoForegroundWindow);
    }

    InteractiveCapability::Available
}

#[cfg(test)]
mod tests {
    use super::{InteractiveCapability, NotInteractive, probe_interactive_capability};

    #[test]
    fn every_reason_has_a_distinct_stable_code() {
        let reasons = [
            NotInteractive::NoActiveConsoleSession,
            NotInteractive::WrongSession,
            NotInteractive::DesktopLocked,
            NotInteractive::NoForegroundWindow,
        ];
        let mut codes = reasons.map(NotInteractive::as_code).to_vec();
        codes.sort_unstable();
        let distinct = {
            let mut deduped = codes.clone();
            deduped.dedup();
            deduped.len()
        };
        assert_eq!(
            distinct,
            reasons.len(),
            "gap categories must not collide: {codes:?}"
        );
        assert!(codes.iter().all(|code| !code.is_empty()));
    }

    #[test]
    fn availability_and_reason_are_consistent() {
        assert!(InteractiveCapability::Available.is_available());
        assert_eq!(InteractiveCapability::Available.unavailable_reason(), None);
        for reason in [
            NotInteractive::NoActiveConsoleSession,
            NotInteractive::WrongSession,
            NotInteractive::DesktopLocked,
            NotInteractive::NoForegroundWindow,
        ] {
            let capability = InteractiveCapability::Unavailable(reason);
            assert!(!capability.is_available());
            assert_eq!(capability.unavailable_reason(), Some(reason));
        }
    }

    #[test]
    fn describe_never_leaks_window_or_user_content() {
        // The description is logged. It must stay a fixed category sentence -
        // no titles, paths, user names, or handles interpolated into it.
        for reason in [
            NotInteractive::NoActiveConsoleSession,
            NotInteractive::WrongSession,
            NotInteractive::DesktopLocked,
            NotInteractive::NoForegroundWindow,
        ] {
            let text = reason.describe();
            assert!(!text.is_empty());
            assert!(
                !text.contains('\\') && !text.contains("0x") && !text.contains('%'),
                "reason text must not carry a path, handle, or format placeholder: {text}"
            );
        }
    }

    #[test]
    fn probe_returns_a_decision_without_panicking() {
        // The probe runs against the real desktop, so its verdict depends on
        // session state. What must hold unconditionally is that it terminates
        // with a well-formed verdict rather than an OS panic, and that an
        // unavailable verdict always carries a reason.
        let capability = probe_interactive_capability();
        match capability {
            InteractiveCapability::Available => {
                assert_eq!(capability.unavailable_reason(), None);
            }
            InteractiveCapability::Unavailable(reason) => {
                assert!(!reason.as_code().is_empty());
                assert!(!reason.describe().is_empty());
            }
        }
    }
}
