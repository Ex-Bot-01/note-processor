// The helpers under test live in the child module that owns them now.
use super::upgrades::{orphan_total, report_merges, report_orphans, OrphanCleanup};
use crate::db::notes::rebase_abs;

#[test]
fn rebase_abs_swaps_the_rel_suffix() {
    assert_eq!(
        rebase_abs("/ws/notes/a.md", "notes/a.md", "notes/sub/b.md"),
        Some("/ws/notes/sub/b.md".to_string())
    );
    // A file at the workspace root: the root part of `abs` is everything before it.
    assert_eq!(
        rebase_abs("/ws/a.md", "a.md", "b.md"),
        Some("/ws/b.md".to_string())
    );
    // Windows separators, and a relative root spelled `.`.
    assert_eq!(
        rebase_abs("C:\\ws\\a.md", "a.md", "sub\\b.md"),
        Some("C:\\ws\\sub\\b.md".to_string())
    );
    assert_eq!(rebase_abs("a.md", "a.md", "b.md"), Some("b.md".to_string()));
    // The case that matters on Windows: `rel` is `/`-separated, `abs` is not, and the
    // file sits inside a directory. Answering None here would lose the note's history.
    assert_eq!(
        rebase_abs("C:\\ws\\notes\\a.md", "notes/a.md", "notes/b.md"),
        Some("C:\\ws\\notes\\b.md".to_string())
    );
    // A root-level file whose abs uses backslashes: the match happens on the second
    // spelling, so the answer keeps the native separator.
    assert_eq!(
        rebase_abs("C:\\ws\\a.md", "a.md", "b.md"),
        Some("C:\\ws\\b.md".to_string())
    );
}

#[test]
fn rebase_abs_accepts_either_separator_at_the_boundary() {
    // A Windows root spelled `C:/ws/` keeps its slash where the first component is joined,
    // while everything deeper arrives with backslashes, so the stored path mixes spellings.
    // Refusing this would answer `BadAbs` to every nested rename under such a root.
    assert_eq!(
        rebase_abs("C:/ws/notes\\a.md", "notes/a.md", "notes/b.md"),
        Some("C:/ws/notes\\b.md".to_string())
    );
    // The mirror case: a backslash root, slash-separated contents.
    assert_eq!(
        rebase_abs("C:\\ws/notes/a.md", "notes/a.md", "notes/sub/b.md"),
        Some("C:\\ws/notes/sub/b.md".to_string())
    );
}

#[test]
fn the_orphan_receipt_is_written_out_with_its_total_and_shapes() {
    // The stderr visibility path, pinned through the sink `report_orphans` writes to: a person
    // who watched an ordinary command delete rows has to be told. Capturing the real stderr
    // in-process is not possible — no stable API replaces the global handle, and the migration
    // runs inside `Db::open` before a test could redirect descriptor 2 — so the function takes
    // its sink for exactly this test. The input is the stored spelling, because that string is
    // what the caller has after the commit, and the total printed beside it is read back out of it.
    let mut out = Vec::new();
    report_orphans(
        &mut out,
        "headings=2,tags=1,items=0,links_src=3,links_dst_null=4",
    );
    assert_eq!(
        String::from_utf8(out).expect("the receipt is text"),
        "note-processor: removed 10 orphan row(s) while adding note foreign keys \
         (headings=2,tags=1,items=0,links_src=3,links_dst_null=4)\n"
    );
}

/// A writer that refuses every write the way `/dev/full` does — `ENOSPC`, not “closed”, which Rust's
/// handle reports as success (see the premise in `crates/np-cli/tests/cli.rs`).
struct Refusing;

impl std::io::Write for Refusing {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::from_raw_os_error(28))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_sink_that_refuses_writes_does_not_abort_the_upgrade() {
    // The upgrade-abort bug, at the unit level. This echo used to sit inside the migration's transaction, so a
    // panic here rolled the whole upgrade back — the file refused to open, every time, forever.
    // It now runs after the commit (see `apply_migrations`), where a panic would still abort a run
    // whose deletions are already durable. Either way the write is best effort, because its signal
    // has a durable home: `FK_ORPHANS_KEY` in `meta`. The production half of the promise — that the
    // call site names stderr and survives a descriptor 2 that refuses writes — is
    // `a_scan_upgrades_when_stderr_refuses_writes` in `crates/np-cli/tests/cli.rs`; no in-process
    // test can reach descriptor 2.
    report_orphans(
        &mut Refusing,
        "headings=1,tags=0,items=0,links_src=0,links_dst_null=0",
    );
}

#[test]
fn the_stored_receipt_adds_up_to_the_total_the_announcement_names() {
    // The stored spelling is the one thing every reader holds — `Db::upgrade_orphans`, the scan
    // report, `np scan`'s summary line and the stderr echo all take the total from this function —
    // so it is pinned against a literal total rather than against a second sum over the same
    // struct: two functions reading one struct could agree while both miscounted the receipt.
    let removed = OrphanCleanup {
        headings: 2,
        tags: 1,
        items: 0,
        links_src: 3,
        links_dst_null: 4,
    };
    assert_eq!(orphan_total(&removed.value()), 10);
    // And an all-zero receipt is zero, which is what keeps the scan report's field `None`, and the
    // echo silent, for a cleanup that removed nothing (see
    // `crate::ScanReport::upgrade_orphans`).
    let untouched = OrphanCleanup {
        headings: 0,
        tags: 0,
        items: 0,
        links_src: 0,
        links_dst_null: 0,
    };
    assert_eq!(orphan_total(&untouched.value()), 0);
}

#[test]
fn an_upgrade_with_nothing_removed_writes_no_receipt() {
    // The other half of the promise: every workspace older than this migration passes through it, and
    // "found nothing" on an ordinary upgrade is not news. If this ever prints, every upgrade
    // shouts and the receipt stops meaning anything.
    let mut out = Vec::new();
    report_orphans(
        &mut out,
        "headings=0,tags=0,items=0,links_src=0,links_dst_null=0",
    );
    assert!(out.is_empty(), "all zeros has to stay silent");
}

#[test]
fn the_merge_receipt_is_echoed_once_rows_have_been_folded() {
    // The merge half of the stderr visibility path, pinned through the sink for the same reason
    // `report_orphans` is: this runs inside [`Db::open`], before any test could redirect descriptor 2. The
    // input is the stored spelling because that string is what [`apply_migrations`] holds after the commit,
    // and the count printed beside it is read back out of it — one value, three readers.
    let mut out = Vec::new();
    report_merges(&mut out, "merged=2;paths=a.md,b.md");
    assert_eq!(
        String::from_utf8(out).expect("the receipt is text"),
        "note-processor: merged 2 duplicate note row(s) into the oldest row for their path \
         (merged=2;paths=a.md,b.md)\n"
    );
}

#[test]
fn a_merge_that_folded_nothing_writes_no_line() {
    // The other half, and the only thing standing between this echo and a line on every fresh workspace:
    // `merged=0` still lands in `meta` as the record that the check ran, while stderr stays quiet. Delete
    // the zero guard in `report_merges` and this test is what fails — **and now a CLI case fails with it**:
    // `the_upgrading_scan_prints_the_merged_pair_on_stdout` asserts that an upgrade with nothing to fold says
    // nothing on stderr. The old claim here was "no integration test can see descriptor 2", which this repo
    // contradicts in three places — the child's stderr is read throughout `crates/np-cli/tests/cli.rs`, and
    // the orphan echo grew precisely this assertion — so what is true is narrower: no CLI case
    // asserted it **until now**.
    let mut out = Vec::new();
    report_merges(&mut out, "merged=0;paths=");
    assert!(out.is_empty(), "`merged=0` states no work done: {out:?}");
}

#[test]
fn rebase_abs_refuses_a_path_that_only_looks_like_a_suffix() {
    // `/ws/xa.md` ends in the characters `a.md` but not in that path component; and an
    // abs from another workspace has nothing to swap.
    assert_eq!(rebase_abs("/ws/xa.md", "a.md", "b.md"), None);
    assert_eq!(rebase_abs("/elsewhere/a.md", "notes/a.md", "b.md"), None);
    // The same trap in the Windows spelling.
    assert_eq!(rebase_abs("C:\\ws\\xa.md", "a.md", "b.md"), None);
}
