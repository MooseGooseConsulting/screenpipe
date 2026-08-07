//! Prints what each lock signal reports right now.
//!
//! Exists because the signals disagree, and the only way to know which one is
//! right is to run it against a desktop whose real state you can see.
fn main() {
    println!(
        "probe_interactive_capability = {:?}",
        screenpipe_screen::probe_interactive_capability()
    );
    println!(
        "session_is_locked            = {:?}",
        screenpipe_screen::session_is_locked()
    );
}
