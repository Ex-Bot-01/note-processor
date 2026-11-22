//! Two processes opening a brand-new workspace at the same moment (#18).
//!
//! The shape is a real one: an app starts a writer child and a reader child against a folder nobody has indexed yet, and
//! both need a database before either can answer. What np guards against everywhere else — two connections overlapping on
//! *rows* — does not apply here, because the thing they collide on is the schema itself: `busy_timeout` makes a waiting
//! writer patient, and neither of them is a writer yet.
//!
//! The fault rate is why this file exists rather than a code comment. Measured on this repository's own tree before the
//! fix, 16 of 80 first opens returned an error to a caller that had done nothing wrong, at three different statements:
//! the `journal_mode = WAL` pragma (which answers `database is locked` in zero milliseconds, busy timeout set one statement
//! earlier and never consulted), a migration step (`table files already exists`), and the version row
//! (`UNIQUE constraint failed: schema_version.version`). All three are one fact — nothing made first creation exclusive —
//! and the third and second follow from reading the recorded version outside anything that would hold it.
//!
//! Rounds are repeated rather than run once because a race is a rate: at the observed ~1 fault in 5 opens, this many opens
//! miss a regression with probability near zero, and one round would miss it often.

use std::fs;
use std::path::PathBuf;
use std::sync::Barrier;

/// How many pairs of simultaneous openers to run. Each round is two opens, so the row count below is `ROUNDS * 2`.
const ROUNDS: usize = 60;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("np-first-open-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("test setup: scratch directory");
    dir
}

/// The version rows a single uncontended first open leaves behind — the baseline the raced databases are compared with, so
/// this file never hard-codes how many migrations this build carries.
fn baseline_versions() -> i64 {
    let dir = scratch("baseline");
    let db =
        np_core::db::Db::open(&dir.join("index.db")).expect("test setup: a solo first open works");
    drop(db);
    let conn =
        rusqlite::Connection::open(dir.join("index.db")).expect("test setup: read the baseline");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
        .expect("test setup: the bookkeeping table exists");
    drop(conn);
    fs::remove_dir_all(&dir).ok();
    n
}

/// **Two openers, one empty folder, no fault for either.** Both are legitimate: neither is doing anything the other should
/// have waited for, and an app that shows "sqlite: table files already exists" to someone who just opened a new folder is
/// reporting corruption in a database created milliseconds earlier.
#[test]
fn two_openers_of_a_new_workspace_both_get_a_schema() {
    let expected = baseline_versions();
    assert!(expected > 0, "test setup: a first open records versions");

    for round in 0..ROUNDS {
        let dir = scratch(&format!("race-{round}"));
        let path = dir.join("index.db");
        let barrier = Barrier::new(2);
        let outcomes: Vec<Result<(), String>> = std::thread::scope(|s| {
            let gate = &barrier;
            let openers: Vec<_> = (0..2)
                .map(|_| {
                    let path = path.clone();
                    s.spawn(move || {
                        gate.wait();
                        np_core::db::Db::open(&path)
                            .map(drop)
                            .map_err(|err| err.to_string())
                    })
                })
                .collect();
            openers
                .into_iter()
                .map(|h| h.join().expect("opener thread"))
                .collect()
        });

        for outcome in outcomes {
            assert_eq!(outcome, Ok(()), "round {round}: a first opener faulted");
        }
        // Both succeeded, so the folder holds exactly one complete schema — not two half-built ones, and not a schema whose
        // bookkeeping doubled while nobody was looking.
        let conn = rusqlite::Connection::open(&path).expect("round setup: open the raced database");
        let versions: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .expect("the bookkeeping table exists after a raced first open");
        let notes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'notes'",
                [],
                |r| r.get(0),
            )
            .expect("sqlite_master is readable");
        drop(conn);
        assert_eq!(
            versions, expected,
            "round {round}: the schema doubled or stopped short"
        );
        assert_eq!(
            notes, 1,
            "round {round}: the notes table is not there exactly once"
        );
        fs::remove_dir_all(&dir).ok();
    }
}

/// **The same folder opened twice in a row still agrees with itself.** The race fix must not become the only path that
/// produces a good schema: the ordinary sequential case is what every other suite in this crate depends on.
#[test]
fn a_second_opener_reads_the_schema_the_first_one_built() {
    let dir = scratch("sequential");
    let path: PathBuf = dir.join("index.db");
    {
        np_core::db::Db::open(&path).expect("first open builds the schema");
    }
    let db = np_core::db::Db::open(&path).expect("second open reads it");
    drop(db);
    let conn = rusqlite::Connection::open(&path).expect("open after two sequential opens");
    let versions: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
        .expect("the bookkeeping table exists");
    assert_eq!(
        versions,
        baseline_versions(),
        "a second open must add no migrations"
    );
    fs::remove_dir_all(&dir).ok();
}
