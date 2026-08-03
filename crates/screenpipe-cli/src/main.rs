use anyhow::bail;
use clap::{Parser, Subcommand};

mod service;

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
        Command::Service { action } => bail!("service {action:?} is not implemented"),
    }
}
