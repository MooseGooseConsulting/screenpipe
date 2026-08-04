use std::ffi::OsString;

use anyhow::{Context, Result, ensure};
use chrono::Duration;
use clap::{Parser, Subcommand};
use screenpipe_memory::{EventSink, MergeConfig, PgEventWriter, RunOutcome, Runner, SampleSource};
use screenpipe_screen::{WindowsCapture, WindowsOcr};

use crate::service::{ServiceManager, ServiceRoot, ServiceStatus, WindowsTaskScheduler};

mod service;
mod windows_source;

use crate::windows_source::WindowsSampleSource;

const DATABASE_URL_ENV: &str = "SCREEN_MEMORY_DATABASE_URL";
const DEFAULT_MACHINE_SLUG: &str = "icarus";
const DEFAULT_DISPLAY_NAME: &str = "Icarus-Laptop";

#[derive(Debug, Parser)]
#[command(
    name = "screenpipe",
    about = "Headless OCR-first Windows screen-memory service"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run foreground capture in the current interactive session.
    Run {
        #[arg(long, default_value = DEFAULT_MACHINE_SLUG)]
        machine_slug: String,
        #[arg(long, default_value = DEFAULT_DISPLAY_NAME)]
        display_name: String,
    },
    /// Check capture, OCR, and PostgreSQL prerequisites.
    Doctor {
        #[arg(long, default_value = DEFAULT_MACHINE_SLUG)]
        machine_slug: String,
        #[arg(long, default_value = DEFAULT_DISPLAY_NAME)]
        display_name: String,
    },
    /// Manage the per-user headless service.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceAction {
    Install,
    Uninstall,
    Status,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Run {
            machine_slug,
            display_name,
        } => {
            let database_url = required_database_url(std::env::var_os(DATABASE_URL_ENV))?;
            run_capture(&database_url, &machine_slug, &display_name).await
        }
        Command::Doctor {
            machine_slug,
            display_name,
        } => {
            let database_url = required_database_url(std::env::var_os(DATABASE_URL_ENV))?;
            run_doctor(&database_url, &machine_slug, &display_name).await
        }
        Command::Service { action } => run_service_action(action),
    }
}

fn required_database_url(value: Option<OsString>) -> Result<String> {
    let value = value.with_context(|| format!("{DATABASE_URL_ENV} must be injected"))?;
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("{DATABASE_URL_ENV} must contain valid Unicode"))?;
    ensure!(!value.trim().is_empty(), "{DATABASE_URL_ENV} is blank");
    Ok(value)
}

fn default_runner() -> Runner {
    Runner::new(MergeConfig {
        idle_gap: Duration::seconds(30),
        scroll_overlap: 0.35,
    })
}

async fn run_iteration(
    runner: &mut Runner,
    source: &mut dyn SampleSource,
    sink: &dyn EventSink,
) -> Result<RunOutcome> {
    runner.run_once(source, sink).await
}

async fn run_capture(database_url: &str, machine_slug: &str, display_name: &str) -> Result<()> {
    let writer = PgEventWriter::connect(database_url, machine_slug, display_name).await?;
    let mut source = WindowsSampleSource::new();
    let mut runner = default_runner();
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    println!("event=runtime_ready machine_slug={machine_slug}");

    loop {
        tokio::select! {
            signal = &mut shutdown => {
                signal.context("listen for Ctrl-C")?;
                println!("event=shutdown reason=ctrl_c");
                return Ok(());
            }
            outcome = run_iteration(&mut runner, &mut source, &writer) => {
                print_run_outcome(&outcome?);
            }
        }
    }
}

fn print_run_outcome(outcome: &RunOutcome) {
    match outcome {
        RunOutcome::GapRecorded { gap } => {
            println!("event=capture_gap gap={}", gap.as_code());
        }
        RunOutcome::Started { event_id, reason } => {
            println!(
                "event=screen_started event_id={event_id} reason={}",
                reason.as_code()
            );
        }
        RunOutcome::Merged { event_id } => {
            println!("event=screen_merged event_id={event_id}");
        }
    }
}

async fn run_doctor(database_url: &str, machine_slug: &str, display_name: &str) -> Result<()> {
    println!("doctor secret=present variable={DATABASE_URL_ENV}");
    WindowsCapture
        .preflight_interactive()
        .context("verify interactive Windows session")?;
    println!("doctor interactive_windows=available");
    WindowsOcr
        .preflight()
        .context("verify Windows OCR engine")?;
    println!("doctor windows_ocr=available");

    let writer = PgEventWriter::connect(database_url, machine_slug, display_name)
        .await
        .context("verify PostgreSQL connection and machine identity")?;
    let report = writer.preflight().await?;
    ensure!(
        report.machine_slug == machine_slug && report.display_name == display_name,
        "PostgreSQL machine identity does not match requested identity"
    );
    // `preflight` returns Err unless the authoritative schema validated, so
    // reaching this line is itself the evidence that the schema is present.
    println!(
        "doctor postgres=available version={} schema=present machine_slug={} display_name={}",
        report.server_version, report.machine_slug, report.display_name
    );
    Ok(())
}

fn run_service_action(action: ServiceAction) -> anyhow::Result<()> {
    let service_root = ServiceRoot::current_user()?;
    let mut manager = ServiceManager::new(WindowsTaskScheduler);
    let status = match action {
        ServiceAction::Install => {
            let current_exe = std::env::current_exe().context("resolve running executable")?;
            manager.install(&service_root, &current_exe)?
        }
        ServiceAction::Uninstall => manager.uninstall(&service_root)?,
        ServiceAction::Status => manager.status(&service_root)?,
    };
    print_service_status(&status);
    Ok(())
}

fn print_service_status(status: &ServiceStatus) {
    println!(
        "task={} process={}",
        status.task_state,
        if status.process_running {
            "running"
        } else {
            "stopped"
        }
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::{Duration, TimeZone, Utc};
    use clap::Parser;
    use screenpipe_memory::{
        CadenceInput, CadenceRecord, EventId, EventSink, MergeConfig, ObservationSample, OpenEvent,
        RunOutcome, Runner, SampleRead, SampleSource, SplitReason,
    };

    use super::{Cli, Command, run_iteration};

    struct OneSample(Option<SampleRead>);

    #[async_trait]
    impl SampleSource for OneSample {
        async fn next_sample(&mut self) -> Result<SampleRead> {
            Ok(self.0.take().expect("one sample available"))
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        starts: Mutex<Vec<SplitReason>>,
    }

    #[async_trait]
    impl EventSink for RecordingSink {
        async fn start(&self, _event: &OpenEvent, reason: SplitReason) -> Result<EventId> {
            self.starts.lock().unwrap().push(reason);
            EventId::try_from("icarus_1".to_owned())
        }

        async fn merge(&self, _event_id: &str, _event: &OpenEvent) -> Result<()> {
            unreachable!("first sample cannot merge")
        }
    }

    #[test]
    fn run_command_defaults_to_the_icarus_machine_identity() {
        let cli = Cli::try_parse_from(["screenpipe", "run"]).unwrap();
        let Command::Run {
            machine_slug,
            display_name,
        } = cli.command
        else {
            panic!("run command expected");
        };
        assert_eq!(machine_slug, "icarus");
        assert_eq!(display_name, "Icarus-Laptop");
    }

    #[tokio::test]
    async fn run_iteration_wires_a_sample_through_runner_and_sink() {
        let captured_at = Utc.with_ymd_and_hms(2026, 8, 4, 12, 0, 0).single().unwrap();
        let cadence = CadenceRecord::from_input(CadenceInput {
            input_idle: Duration::zero(),
            frame_stable_for: Duration::zero(),
            foreground_changed: false,
            frame_changed: false,
        });
        let mut source = OneSample(Some(SampleRead::Sample {
            sample: ObservationSample {
                captured_at,
                app_key: "notepad.exe".to_owned(),
                app_title: "Notepad".to_owned(),
                window_title: "Goal 1".to_owned(),
                ocr_text: "runner wiring sample".to_owned(),
                readable_text: "runner wiring sample".to_owned(),
                browser_url: None,
            },
            cadence,
        }));
        let sink = RecordingSink::default();
        let mut runner = Runner::new(MergeConfig {
            idle_gap: Duration::seconds(30),
            scroll_overlap: 0.35,
        });

        let outcome = run_iteration(&mut runner, &mut source, &sink)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            RunOutcome::Started {
                event_id: "icarus_1".to_owned(),
                reason: SplitReason::Initial,
            }
        );
        assert_eq!(*sink.starts.lock().unwrap(), [SplitReason::Initial]);
    }
}
