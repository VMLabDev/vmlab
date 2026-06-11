//! CLI surface (PRD §12). The same binary also hosts the supervisor and lab
//! daemons via hidden subcommands, re-exec'd from the CLI as needed.

use clap::{Parser, Subcommand};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "vmlab", version, about = "Single-host VM lab orchestrator")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Validate the lab file with no side effects
    Validate,
    /// Internal: run the supervisor daemon in the foreground
    #[command(name = "__supervisord", hide = true)]
    Supervisord,
    /// Internal: run a lab daemon in the foreground
    #[command(name = "__labd", hide = true)]
    Labd {
        /// Lab name
        #[arg(long)]
        lab: String,
        /// Directory containing vmlab.wcl
        #[arg(long)]
        root: std::path::PathBuf,
    },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Validate => cmd_validate(),
        Command::Supervisord => todo_daemon("supervisor"),
        Command::Labd { .. } => todo_daemon("lab daemon"),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:?}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_validate() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let root = crate::paths::find_lab_root(&cwd)?;
    println!("lab root: {}", root.display());
    Ok(())
}

fn todo_daemon(which: &str) -> anyhow::Result<()> {
    anyhow::bail!("{which} not implemented yet")
}
