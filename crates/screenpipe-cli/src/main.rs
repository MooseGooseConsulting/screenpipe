use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};

use crate::service::{ServiceManager, ServiceStatus, WindowsTaskScheduler};

mod service;
mod windows_source;

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
    Run,
    /// Check capture, OCR, and PostgreSQL prerequisites.
    Doctor,
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

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Run => bail!("run is not implemented"),
        Command::Doctor => bail!("doctor is not implemented"),
        Command::Service { action } => run_service_action(action),
    }
}

fn run_service_action(action: ServiceAction) -> anyhow::Result<()> {
    let local_app_data = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("LOCALAPPDATA is required for per-user service management")?;
    if !local_app_data.is_absolute() {
        bail!("LOCALAPPDATA must be an absolute path");
    }
    let mut manager = ServiceManager::new(WindowsTaskScheduler);
    let status = match action {
        ServiceAction::Install => {
            let current_exe = std::env::current_exe().context("resolve running executable")?;
            manager.install(&local_app_data, &current_exe)?
        }
        ServiceAction::Uninstall => manager.uninstall(&local_app_data)?,
        ServiceAction::Status => manager.status(&local_app_data)?,
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
