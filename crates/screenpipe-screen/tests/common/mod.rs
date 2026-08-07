//! Shared capability gate for live-desktop integration tests.
//!
//! `#[ignore]` is the wrong tool for a test that needs an unlocked interactive
//! desktop: it never runs in the session where it is meaningful, so it protects
//! nothing. These helpers let such a test run by default, skip *loudly* when
//! the capability is genuinely absent, and hard-fail instead of skipping when a
//! qualification run asserts the capability must be there.

#![allow(dead_code)]

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use screenpipe_screen::{InteractiveCapability, probe_interactive_capability};

fn foreground_lock_path() -> PathBuf {
    std::env::temp_dir().join("screenpipe-foreground-fixture.lock")
}

/// Exclusive claim on "the foreground window", held for the life of the guard.
///
/// There is only one foreground window per desktop, and every live capture test
/// pins its own fixture there. Cargo runs each integration-test binary in its
/// own process, so an in-process mutex cannot serialize them - two binaries
/// would steal focus from each other and both report a capture of the wrong
/// window. This lock is a file so it works across processes.
pub struct ForegroundLock {
    path: PathBuf,
    /// The token this guard wrote. `Drop` removes the file only if it still
    /// contains this token.
    token: String,
    /// False when the wait timed out and the lock was never acquired. Such a
    /// guard must not delete anything.
    owned: bool,
}

impl Drop for ForegroundLock {
    fn drop(&mut self) {
        if !self.owned {
            return;
        }
        // Release only OUR lock. The previous version deleted the file
        // unconditionally, so a guard fabricated on the timeout path - or one
        // whose lock had been reclaimed as stale by a waiter - would free a
        // lock another process was still holding, and the next waiter would
        // acquire while two fixtures were live.
        if std::fs::read_to_string(&self.path).ok().as_deref() == Some(self.token.as_str()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn lock_token() -> String {
    // Process id plus start instant: unique per test binary, and enough for
    // Drop to tell its own lock from a successor's.
    format!(
        "{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos()
    )
}

/// Blocks until no other live-desktop test holds the foreground.
///
/// A lock whose owning process is gone is reclaimed, so a crashed test cannot
/// wedge the suite forever.
pub fn lock_foreground() -> ForegroundLock {
    let path = foreground_lock_path();
    let token = lock_token();
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                use std::io::Write;
                let _ = file.write_all(token.as_bytes());
                drop(file);
                // The previous holder's fixture window is TopMost and may still
                // be tearing down. Launching into that race makes the new
                // fixture lose the foreground and the test time out, so let the
                // desktop settle before handing over.
                std::thread::sleep(Duration::from_millis(1500));
                return ForegroundLock {
                    path,
                    token,
                    owned: true,
                };
            }
            Err(_) => {
                // Staleness is "the owner is gone", not "the owner has held it
                // a while". The mtime is stamped once at creation and never
                // refreshed, so an age test meant a legitimately slow test -
                // merge_boundary_live budgets two 45s title waits plus three
                // captures and three OCR passes - had its lock stolen while it
                // was still running, putting two TopMost fixtures on the
                // desktop at once.
                if !lock_holder_is_alive(&path) {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if Instant::now() >= deadline {
                    // Better to run and risk interference than to hang the
                    // suite; the content assertions will fail loudly if the
                    // wrong window was captured. `owned: false` keeps this
                    // guard from deleting the real holder's lock on drop.
                    eprintln!("foreground lock wait timed out; proceeding without it");
                    return ForegroundLock {
                        path,
                        token,
                        owned: false,
                    };
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

/// True when the pid recorded in the lock file still names a live process.
fn lock_holder_is_alive(path: &PathBuf) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        // The file vanished between the failed create and this read; treat it
        // as gone so the caller retries the create.
        return false;
    };
    let Some(pid) = contents
        .split('-')
        .next()
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return false;
    };
    process_is_running(pid)
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => {
                let _ = CloseHandle(handle);
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(not(windows))]
fn process_is_running(_pid: u32) -> bool {
    true
}

/// Set this to downgrade a missing interactive desktop from a failure to a skip.
///
/// The default is deliberately the strict one - see
/// [`interactive_desktop_available`].
pub const ALLOW_SKIP_ENV: &str = "SCREEN_MEMORY_ALLOW_LOCKED_SKIP";

/// Returns `false` when the caller should return early.
///
/// # Why the default is to FAIL rather than skip
///
/// The first version of this gate printed a banner and returned `false`. That
/// was strictly weaker than the `#[ignore]` it replaced: libtest captures both
/// stdout and stderr per test and prints them only for *failing* tests, so
/// without `--nocapture` the banner is discarded and cargo prints a plain
/// `ok`. A skipped test became byte-for-byte indistinguishable from a passing
/// one - which is the exact class of lie these tests exist to remove, and
/// `#[ignore]` at least printed `ignored`.
///
/// It also had the escape hatch backwards. `SCREEN_MEMORY_REQUIRE_INTERACTIVE`
/// turned a skip into a failure, but nothing in the repository ever set it, so
/// the strict mode had never run: hardcoding the probe to `DesktopLocked` would
/// have made every live-desktop test return early and report `ok`.
///
/// So the polarity is inverted. A locked desktop fails loudly by default, and a
/// caller who genuinely wants to run the suite on a locked machine opts out
/// with `SCREEN_MEMORY_ALLOW_LOCKED_SKIP`. CI does not run these targets at
/// all, precisely because a skip there would be invisible.
#[must_use]
pub fn interactive_desktop_available(test_name: &str) -> bool {
    match probe_interactive_capability() {
        InteractiveCapability::Available => true,
        InteractiveCapability::Unavailable(reason) => {
            let banner = format!(
                "SKIPPED {test_name}: {} ({})",
                reason.describe(),
                reason.as_code()
            );
            let rule = "!".repeat(78);
            eprintln!("\n{rule}\n!!!! {banner}\n{rule}\n");
            println!("{banner}");
            assert!(
                std::env::var_os(ALLOW_SKIP_ENV).is_some(),
                "{banner} - this test needs an unlocked interactive desktop. \
                 Set {ALLOW_SKIP_ENV}=1 to downgrade this to a skip."
            );
            false
        }
    }
}
