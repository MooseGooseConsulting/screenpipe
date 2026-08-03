use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn help_exposes_only_goal_one_commands() {
    Command::cargo_bin("screenpipe")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("run"))
        .stdout(predicate::str::contains("doctor"))
        .stdout(predicate::str::contains("service"));
}

#[test]
#[cfg(windows)]
fn service_status_reads_the_native_task_and_process_state() {
    Command::cargo_bin("screenpipe")
        .unwrap()
        .args(["service", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("task="))
        .stdout(predicate::str::contains("process="));
}
