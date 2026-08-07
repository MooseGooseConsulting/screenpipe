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
pub struct ForegroundLock(PathBuf);

impl Drop for ForegroundLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Blocks until no other live-desktop test holds the foreground.
///
/// A lock older than the timeout is treated as abandoned (a previous run was
/// killed) and reclaimed, so a crashed test cannot wedge the suite forever.
pub fn lock_foreground() -> ForegroundLock {
    let path = foreground_lock_path();
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => {
                // The previous holder's fixture window is TopMost and may still
                // be tearing down. Launching into that race makes the new
                // fixture lose the foreground and the test time out, so let the
                // desktop settle before handing over.
                std::thread::sleep(Duration::from_millis(1500));
                return ForegroundLock(path);
            }
            Err(_) => {
                let stale = std::fs::metadata(&path)
                    .and_then(|meta| meta.modified())
                    .map(|modified| {
                        modified.elapsed().unwrap_or(Duration::ZERO) > Duration::from_secs(120)
                    })
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if Instant::now() >= deadline {
                    // Better to run and risk interference than to hang the
                    // suite; the content assertions will fail loudly if the
                    // wrong window was captured.
                    eprintln!("foreground lock wait timed out; proceeding without it");
                    return ForegroundLock(path);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

/// Set this in a qualification run to turn a skip into a failure.
pub const REQUIRE_ENV: &str = "SCREEN_MEMORY_REQUIRE_INTERACTIVE";

/// Returns `false` when the caller should return early.
///
/// A silent early return is indistinguishable from a pass in `cargo test`
/// output, which is exactly the class of lie these tests exist to remove - so
/// the skip is printed to both streams and framed to be impossible to miss.
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
                std::env::var_os(REQUIRE_ENV).is_none(),
                "{banner} - but {REQUIRE_ENV} demands a real interactive desktop"
            );
            false
        }
    }
}
