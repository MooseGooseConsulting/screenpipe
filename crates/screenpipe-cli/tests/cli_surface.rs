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
fn run_help_exposes_icarus_identity_defaults() {
    Command::cargo_bin("screenpipe")
        .unwrap()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--machine-slug"))
        .stdout(predicate::str::contains("icarus"))
        .stdout(predicate::str::contains("--display-name"))
        .stdout(predicate::str::contains("Icarus-Laptop"));
}

#[test]
fn run_requires_only_the_named_injected_database_variable() {
    Command::cargo_bin("screenpipe")
        .unwrap()
        .arg("run")
        .env_remove("SCREEN_MEMORY_DATABASE_URL")
        .env("DATABASE_URL", "sqlite://ambient-must-not-be-used.sqlite")
        .assert()
        .failure()
        .stderr(predicate::str::contains("SCREEN_MEMORY_DATABASE_URL"))
        .stderr(predicate::str::contains("run is not implemented").not())
        .stderr(predicate::str::contains("sqlite://").not());
}

#[test]
fn doctor_requires_the_named_injected_variable_without_rendering_values() {
    Command::cargo_bin("screenpipe")
        .unwrap()
        .arg("doctor")
        .env_remove("SCREEN_MEMORY_DATABASE_URL")
        .env(
            "DATABASE_URL",
            "postgresql://ambient-secret@example.invalid/db",
        )
        .assert()
        .failure()
        .stderr(predicate::str::contains("SCREEN_MEMORY_DATABASE_URL"))
        .stderr(predicate::str::contains("doctor is not implemented").not())
        .stderr(predicate::str::contains("ambient-secret").not());
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
