use crate::commands::common::load_config;
use crate::commands::common::writer_lock;
use np_core::db::orphan_total;
use np_core::scan;
use np_core::scan::ScanReport;
use std::error::Error;
use std::io;
use std::io::Write;
use std::path::Path;

/// Index `workspace` and print its report.
///
/// The write lock is taken before anything else and held for the whole command — see [`writer_lock`] for why
/// exclusion has to happen before the first statement rather than inside the first write.
///
/// Every failure leaves as one message on stderr and exit code 1 — the message is best effort, because a
/// descriptor that refuses writes must not cost the caller the exit code. A configuration that names no data dir
/// is reported as the missing value it is by [`load_config`], because
/// [`np_core::config::Config::load`] refuses rather than deriving `workspaces/<hash>.sqlite` next to the current
/// directory.
pub(crate) fn scan_workspace(workspace: &Path, json: bool) -> Result<(), Box<dyn Error>> {
    let cfg = load_config()?;
    // Held for the whole command, including the `INDEX.md` rendering that SQLite cannot roll back.
    let _writer =
        writer_lock(&cfg, workspace, "scan").map_err(|failure| failure.report_cli(json))?;
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
pub(crate) fn print_summary(report: &ScanReport) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "added      {}", report.added);
    let _ = writeln!(out, "modified   {}", report.modified);
    let _ = writeln!(out, "moved      {}", report.moved);
    let _ = writeln!(out, "removed    {}", report.removed);
    let _ = writeln!(out, "unchanged  {}", report.unchanged);
    // The upgrade's orphan receipt, echoed once by the run whose own `Db::open` did the deleting
    // (`ScanReport::upgrade_orphans` says why it is once and not always). Dropped when stdout refuses
    // writes (descriptor 1) like every other line here — with deliberate reason this time: the deletions are
    // already recorded in `meta` under `np_core::db::FK_ORPHANS_KEY`, so this line is the copy a person
    // gets to read, not the only place the fact lives.
    //
    // So the same sentence can appear **twice in one run** — once on stderr from the migration
    // (`np_core::db::report_orphans`) and once here. That duplication is the design, not a bug to file:
    // the stderr line belongs to the command that did the deleting and prints even when no report is
    // printed, this one belongs to the report a person is reading, and neither has to arrive for the fact
    // to survive.
    if let Some(receipt) = &report.upgrade_orphans {
        let _ = writeln!(
            out,
            "upgrade    removed {} orphan row(s) while adding note foreign keys ({})",
            orphan_total(receipt),
            receipt
        );
    }
    // What this run's own `Db::open` merged — the duplicate-note repair. One value, printed verbatim as
    // `meta` holds it under `np_core::db::NOTE_MERGE_KEY`, so this line and the durable record cannot drift
    // and no second count is computed here.
    //
    // Once per database rather than on every scan (`ScanReport::merged_notes` gives the reason), and dropped
    // when stdout refuses writes like every other line here: the merge itself is committed, and its receipt
    // is in `meta`. The same sentence can also appear on stderr from the migration
    // (`np_core::db::report_merges`), which is the deliberate duplication `report_orphans` documents.
    if let Some(receipt) = &report.merged_notes {
        let _ = writeln!(out, "merge      {receipt}");
    }
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
    // **Tracked but never searchable, named**. A vault holding `plain.txt` beside `note.md` answers
    // `added 5 / notes 1`, and the gap between those two numbers used to be a rule np applied silently: someone else's
    // product had to guess it and say it in its own words. Printed only when non-zero, because a vault of nothing but
    // `.md` has no explanation to give — and one label per scan that never needs printing is a line nobody reads.
    if report.attachments > 0 {
        let _ = write!(out, "attachments {}", report.attachments);
        if !report.attachment_examples.is_empty() {
            // The names come from the same predicate as the count (`db::ATTACHMENT_WHERE`), so this line cannot
            // describe a file the database has stopped counting. An ellipsis says the cap was reached; the number
            // above it stayed exact.
            let more = if i64::try_from(report.attachment_examples.len()).unwrap_or(i64::MAX)
                < report.attachments
            {
                ", \u{2026}"
            } else {
                ""
            };
            let _ = write!(
                out,
                " — tracked, never parsed: {}{more}",
                report.attachment_examples.join(", ")
            );
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "skipped    {}", report.skipped.len());
    for path in &report.skipped {
        let _ = writeln!(out, "  {path}");
    }
    // What that list actually **was**, counted by kind instead of merged: `skipped 2` hides that one
    // path is a permission problem nobody fixes by renaming and the other a name this build can never key.
    // Printed only when something was lost, and printed as the same string `meta` records under
    // `np_core::scan::UNINDEXABLE_KEY` — one value, two channels, so the line a person read yesterday can be
    // compared with the record they open today.
    //
    // One column wider than the labels above: `unindexable` is eleven letters, those are ten, and re-spacing
    // them to fit a new word would break the `grep` of anyone reading their own old logs. Dropped when stdout
    // refuses writes (descriptor 1) like every other line here — the durable copy is in `meta`, so this line
    // is the copy a person reads rather than the only place the fact lives.
    if let Some(losses) = &report.unindexable {
        let _ = writeln!(out, "unindexable {}", losses.value());
    }
    let _ = writeln!(out, "index      {}", report.index_files.join(", "));
    let _ = writeln!(out, "elapsed    {} ms", report.elapsed_ms);
}

// ---------------------------------------------------------------------------
// `np embed` — resolve a provider, embed what moved, report what happened.
//
// The command resolves *which* provider the configuration names and prints what came back; every decision
// about currency, batching, exit arms and the stamp rule lives in [`np_core::embed_run`]. That split is why
// there is no SQL here beyond `db.rs`'s own readers and no second digest.
// ---------------------------------------------------------------------------
