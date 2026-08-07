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
fn run_help_says_the_clipboard_channel_is_on_and_how_to_turn_it_off() {
    // The flag inverts, so the help text is the only place the DEFAULT is
    // visible to the person deciding whether to run this on their machine.
    // "clipboard text is recorded unless you say otherwise" is a consent
    // question, and it must not be discoverable only by reading the source.
    Command::cargo_bin("screenpipe")
        .unwrap()
        .args(["run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--no-clipboard"))
        .stdout(predicate::str::contains("ON by default"));
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

#[test]
fn audio_help_says_the_channel_is_off_and_what_turning_it_on_means() {
    // Same consent question as the clipboard flag above, with more at stake:
    // this channel can record people who are not the operator. The default and
    // the reason for it have to be readable from `--help`, not only from the
    // design document.
    Command::cargo_bin("screenpipe")
        .unwrap()
        .args(["audio", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("OFF"))
        .stdout(predicate::str::contains("audio-channel.md"));
}

#[test]
fn the_microphone_is_a_separate_decision_from_the_audio_channel() {
    // Loopback hears what came out of the speakers; the microphone hears the
    // room and everyone in it. Nothing may collapse those into one switch.
    Command::cargo_bin("screenpipe")
        .unwrap()
        .args(["audio", "run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--microphone"))
        .stdout(predicate::str::contains("hears the room"));
}

#[test]
fn the_audio_service_can_be_removed_by_a_build_that_cannot_record() {
    // The worst possible property for this channel would be "you installed it
    // with a special build, so you need that build to turn it off". `service`
    // is deliberately outside the feature gate; only its status is asserted
    // here, because install and uninstall touch the real Task Scheduler.
    Command::cargo_bin("screenpipe")
        .unwrap()
        .args(["audio", "service", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("MooseGoose Screen Memory Audio"));
}

#[test]
#[cfg(not(feature = "audio"))]
fn a_default_build_refuses_to_record_audio_and_says_how_to_get_one_that_can() {
    // The default binary has no whisper model, no capture path, and no way to
    // open a microphone. It should say so in terms someone can act on rather
    // than "unrecognized subcommand".
    Command::cargo_bin("screenpipe")
        .unwrap()
        .args(["audio", "run"])
        .env("SCREEN_MEMORY_DATABASE_URL", "postgresql://unused/unused")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--features audio"));
}

#[test]
#[cfg(not(feature = "audio"))]
fn a_default_build_refuses_to_install_a_service_it_cannot_run() {
    // Uninstall and status stay open in every build - see above - but install
    // is different: it would register a scheduled task running `audio run`
    // from a binary that refuses `audio run`, so the job would do nothing but
    // fail and restart every ten seconds with no visible symptom but a log
    // nobody has a reason to read.
    Command::cargo_bin("screenpipe")
        .unwrap()
        .args(["audio", "service", "install"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--features audio"))
        .stderr(predicate::str::contains(
            "Uninstall and status work from any build",
        ));
}
