#[test]
fn clipboard_watcher_cannot_be_reused_after_a_move() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/clipboard_watcher_not_copy.rs");
}
