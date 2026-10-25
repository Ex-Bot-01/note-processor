//! R4a benchmark: an `#[ignore]`d release pass over a synthetic 2,000-note corpus.
//!
//! This is step 1 of the measurement protocol in `docs/backlog-m2-followups.md`: F9/F10 may only
//! claim a speedup against numbers taken *this* way, on *this* machine, before and after the
//! change. There is no behaviour under test here — the corpus is synthetic, the assertions are
//! sanity floors that cannot flake on timing, and everything informative is printed for a human
//! to compare across commits.
//!
//! Run it with `cargo test --release -p np-core --test perf -- --ignored --nocapture`, in the
//! style of the ignored timing tests in `tests/items.rs` and `tests/links.rs`.
//!
//! What is machine-dependent about the printed numbers, so a comparison stays honest:
//! - disk speed and fsync behaviour for the SQLite database and the 2,000 small writes of the
//!   first pass (a tmpfs `/tmp` versus a spinning disk moves these by an order of magnitude);
//! - CPU single-core speed — the scan is single-threaded;
//! - whether `TempDir` lands on the same volume as before (it follows `$TMPDIR`);
//! - ambient load from anything else running at the time.
//!
//! Therefore: no warm-up pass, no best-of-N, no assertion on any duration. The corpus is fully
//! deterministic (fixed seed arithmetic, no RNG), so both scans and both invocations see exactly
//! the same bytes; only the machine varies.

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use np_core::config::Config;
use np_core::scan;
use tempfile::TempDir;

/// Notes in the synthetic corpus. The protocol asks for at least 2,000.
const NOTES: usize = 2_000;
/// Subdirectories the notes are spread over, so link resolution and the walk see a real tree.
const DIRS: usize = 8;

/// The body of note `i`, byte-deterministic from `i` alone.
///
/// Shapes the parser has to work over: frontmatter `title:`, two headings, three `#tags`, two
/// wikilinks into the corpus (targets are spread by coprime multipliers so resolution is a hash
/// probe across all 2,000 basenames, not a neighbour lookup), and ~20 words of prose.
fn note_body(i: usize) -> String {
    let a = (i * 7 + 3) % NOTES;
    let b = (i * 13 + 11) % NOTES;
    let words = [
        "the vendor sent the contract in october and we agreed on quarterly invoices for the project",
        "notes from the reading group cover scheduling and budget with a short summary of risks",
        "a draft outline lists sections evidence methods results and two open questions about scope",
    ][i % 3];
    format!(
        "---\ntitle: Note {i:04}\n---\n\n## Section one\n\n{words} #alpha{} #beta{} \
         [[note-{a:04}]]\n\n## Section two\n\nFollow-up prose for note {i:04} with a second \
         reference to [[note-{b:04}]] #gamma{}\n",
        i % 5,
        i % 7,
        i % 11,
    )
}

/// A fresh tempdir holding `NOTES` markdown notes under `DIRS` subdirectories, plus a `Config`
/// whose data dir is a private directory inside the same tempdir (no real user data is touched).
///
/// The caller must keep the returned [`TempDir`] alive across the scans.
fn synthetic_workspace() -> (TempDir, PathBuf, Config) {
    let tmp = TempDir::new().expect("bench setup: tempdir");
    let root = tmp.path().join("ws");
    for i in 0..NOTES {
        let dir = root.join(format!("d{}", i % DIRS));
        fs::create_dir_all(&dir).expect("bench setup: create note directory");
        fs::write(dir.join(format!("note-{i:04}.md")), note_body(i))
            .expect("bench setup: write note");
    }
    let cfg = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    (tmp, root, cfg)
}

fn run_pass(label: &str, root: &std::path::Path, cfg: &Config) -> scan::ScanReport {
    let started = Instant::now();
    let report = scan::scan(root, cfg).expect("bench: scan must succeed");
    let elapsed = started.elapsed();
    // The numbers a human compares across commits. `notes` and `unresolved_links` are printed so
    // it is visible that link resolution did real work (4,000 wikilink edges over 2,000 notes).
    println!(
        "{label}: {} ms | added={} modified={} unchanged={} | notes={} unresolved={} elapsed_ms(field)={}",
        elapsed.as_millis(),
        report.added,
        report.modified,
        report.unchanged,
        report.notes,
        report.unresolved_links,
        report.elapsed_ms,
    );
    report
}

/// Two scans over a 2,000-note synthetic corpus: cold (everything added) and warm (nothing to do).
///
/// Ignored and release-only, in the style of the huge-input tests in `tests/items.rs` and
/// `tests/links.rs`: run it with
/// `cargo test --release -p np-core --test perf -- --ignored --nocapture`.
///
/// The only assertions are corpus sanity that cannot depend on machine speed: the cold pass adds
/// every note, the warm pass reports zero changes of any kind, and neither pass skips a file.
/// Durations are printed, never asserted — this is a measuring stick for F9/F10, not a gate.
#[test]
#[ignore = "2k-note benchmark; release-only (`cargo test --release -p np-core --test perf -- --ignored --nocapture`)"]
fn two_pass_scan_over_two_thousand_notes() {
    let (_tmp, root, cfg) = synthetic_workspace();

    let cold = run_pass("pass 1 (cold)", &root, &cfg);
    assert!(
        cold.skipped.is_empty(),
        "the synthetic corpus must be fully indexable, skipped={:?}",
        cold.skipped
    );
    assert_eq!(
        cold.added, NOTES,
        "a first scan over {} notes adds exactly that many",
        NOTES
    );

    let warm = run_pass("pass 2 (warm)", &root, &cfg);
    assert!(
        warm.skipped.is_empty(),
        "the second scan must skip nothing, skipped={:?}",
        warm.skipped
    );
    assert_eq!(
        (warm.added, warm.modified, warm.moved, warm.removed),
        (0, 0, 0, 0),
        "nothing on disk changed between the two passes"
    );
}
