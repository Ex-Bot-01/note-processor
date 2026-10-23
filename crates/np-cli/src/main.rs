//! `np` — command line entry point for note-processor.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Index a note workspace and regenerate its catalog.
    Scan {
        /// Workspace root to index.
        workspace: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { workspace } => {
            eprintln!("np scan {}: not implemented", workspace.display());
            ExitCode::from(2)
        }
    }
}
