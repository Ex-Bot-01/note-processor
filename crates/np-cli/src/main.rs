//! `np` — command line entry point for note-processor.

use std::error::Error;
use std::io::{self, Write};
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
                // Written through a handle with the result dropped, for the reason `np_core::scan` gives for
                // its own stderr receipt: `eprintln!` panics when descriptor 2 refuses the write, which would
                // replace a reportable failure with exit 101 and a panic message that cannot be printed
                // either. The exit code is the part the caller can still observe, so it stays FAILURE.
                let _ = writeln!(io::stderr(), "np: {err}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Index `workspace` and print its report.
///
/// Every failure leaves as one message on stderr and exit code 1 — the message is best effort, because a
/// descriptor that refuses writes must not cost the caller the exit code (see the call site in `main`). A
/// configuration that names no data dir is reported as the missing value it is, because [`Config::load`]
/// refuses rather than deriving `workspaces/<hash>.sqlite` next to the current directory.
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
///
/// Every line goes to one locked stdout handle with its result dropped, so a descriptor that refuses writes
/// (a closed pipe, `/dev/full`) cannot turn a scan whose work is committed into the panic `println!` would
/// raise — exit 101 over a summary nobody can read. The bytes on stdout are exactly what they were.
fn print_summary(report: &ScanReport) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "added      {}", report.added);
    let _ = writeln!(out, "modified   {}", report.modified);
    let _ = writeln!(out, "moved      {}", report.moved);
    let _ = writeln!(out, "removed    {}", report.removed);
    let _ = writeln!(out, "unchanged  {}", report.unchanged);
    let _ = writeln!(out, "notes      {}", report.notes);
    let _ = write!(out, "unresolved {}", report.unresolved_links);
    if report.unresolved_links > 0 {
        // An ambiguous target is left unresolved instead of guessed, so the number alone is not
        // actionable. Spelling the path usually is (`np_core::links::resolve` matches an exact rel first) —
        // except for a pair differing only in case, whose spellings fold identically and so place neither.
        let _ = write!(out, " — a target matched more than one file or none; spell its path to place it, e.g. [[notes/a.md]]");
    }
    let _ = writeln!(out);
    // The targets behind that count, spelled as their author wrote them. The scan reads both from one
    // transaction, so this list can never answer a different question than the number above it.
    for target in &report.unresolved_targets {
        let _ = writeln!(out, "  {target}");
    }
    let _ = writeln!(out, "skipped    {}", report.skipped.len());
    for path in &report.skipped {
        let _ = writeln!(out, "  {path}");
    }
    let _ = writeln!(out, "index      {}", report.index_files.join(", "));
    let _ = writeln!(out, "elapsed    {} ms", report.elapsed_ms);
}
