//! `np` — command line entry point for note-processor.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use np_core::config::Config;
use np_core::scan::{scan, ScanReport};

#[derive(Parser)]
// The binary is `np`, but the package is `np-cli`; without `name` clap would print the package name
// in `--version` and `--help`.
#[command(name = "np", version, about)]
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
        /// Print the report as JSON instead of a summary.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { workspace, json } => match scan_workspace(&workspace, json) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("np: {err}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Index `workspace` and print its report.
///
/// Every failure leaves as one message on stderr and exit code 1. A configuration that names no data
/// dir is reported as the missing value it is, because [`Config::load`] refuses rather than deriving
/// `workspaces/<hash>.sqlite` next to the current directory.
fn scan_workspace(workspace: &Path, json: bool) -> Result<(), Box<dyn Error>> {
    let cfg = match Config::load(None) {
        Ok(cfg) => cfg,
        Err(err) => return Err(format!("configuration error: {err}").into()),
    };
    let report = scan(workspace, &cfg)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_summary(&report);
    }
    Ok(())
}

/// The human-readable form of a report: one aligned line per counted field, then the details.
fn print_summary(report: &ScanReport) {
    println!("added      {}", report.added);
    println!("modified   {}", report.modified);
    println!("moved      {}", report.moved);
    println!("removed    {}", report.removed);
    println!("unchanged  {}", report.unchanged);
    println!("notes      {}", report.notes);
    print!("unresolved {}", report.unresolved_links);
    if report.unresolved_links > 0 {
        // An ambiguous target is left unresolved instead of guessed, so the number alone is not
        // actionable. Spelling the path is (`np_core::links::resolve` matches an exact rel first).
        print!(" — a target matched more than one file or none; spell its path to place it, e.g. [[notes/a.md]]");
    }
    println!();
    // The targets behind that count, spelled as their author wrote them. The scan reads both from one
    // transaction, so this list can never answer a different question than the number above it.
    for target in &report.unresolved_targets {
        println!("  {target}");
    }
    println!("skipped    {}", report.skipped.len());
    for path in &report.skipped {
        println!("  {path}");
    }
    println!("index      {}", report.index_files.join(", "));
    println!("elapsed    {} ms", report.elapsed_ms);
}
