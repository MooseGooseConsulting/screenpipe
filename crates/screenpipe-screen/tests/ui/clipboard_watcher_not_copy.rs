use screenpipe_screen::ClipboardWatcher;

fn main() {
    let original = ClipboardWatcher::new();
    let moved = original;
    let _reused_after_move = original;
    let _ = moved;
}
