//! The workspace database: schema bookkeeping, moves that keep a note's identity, and
//! deletions that only mark a row stale.
//!
//! Every assertion goes through SQL on the open connection, because what Task 10 relies on
//! is the state of the rows, not the return value of a helper. Rows for `notes`, `links`
//! and `items` are inserted here by hand: filling them in is Tasks 7-9's job, and this
//! module must not grow an API just to make its own tests shorter.

use std::fs;
use std::path::Path;
use std::time::Duration;

// The hold times and the writer/holder handoff live in `tests/common/mod.rs`, which `tests/config.rs`
// uses for the registry as well, so a change to `BUSY_TIMEOUT_MS` moves both binaries together.
mod common;

use common::{contended, hold_write_lock, Contention, HOLD_MS, HOLD_PAST_TIMEOUT_MS};
use np_core::db::{
    apply_move, mark_stale, note_counts, upsert_file, Db, DbError, NoteCounts, MIGRATIONS,
};
use np_core::hash::Snapshot;
use tempfile::TempDir;

/// A snapshot as `walk` + [`np_core::hash::sha256_file`] would produce it: `case_key` is
/// the lowercased `rel`.
fn snap(rel: &str, hash: &str) -> Snapshot {
    Snapshot {
        rel: rel.to_string(),
        case_key: rel.to_lowercase(),
        hash: hash.to_string(),
        size: 12,
        mtime: 1_000,
    }
}

/// The single value a probe query returns, with the query in the failure message.
fn scalar<T: rusqlite::types::FromSql>(db: &Db, sql: &str) -> T {
    db.0.query_row(sql, [], |row| row.get::<_, T>(0))
        .unwrap_or_else(|err| panic!("query failed: {sql}\n{err}"))
}

/// The names one single-column query returns, in the order it returned them.
fn names(db: &Db, sql: &str) -> Vec<String> {
    db.0.prepare(sql)
        .unwrap_or_else(|err| panic!("query failed: {sql}\n{err}"))
        .query_map([], |row| row.get::<_, String>(0))
        .expect("name rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("name values")
}

/// A table's column names, in declaration order.
fn column_names(db: &Db, table: &str) -> Vec<String> {
    db.0.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
        .expect("pragma_table_info is readable")
        .query_map(rusqlite::params![table], |row| row.get::<_, String>(0))
        .unwrap_or_else(|err| panic!("{table} columns: {err}"))
        .collect::<Result<Vec<_>, _>>()
        .expect("column names")
}

/// A statement that has to be refused by a CHECK constraint.
fn rejects(db: &Db, sql: &str, column: &str) {
    let err =
        db.0.execute(sql, [])
            .expect_err("a CHECK constraint has to refuse this value");
    assert!(
        matches!(
            err,
            rusqlite::Error::SqliteFailure(fault, _)
                if fault.code == rusqlite::ErrorCode::ConstraintViolation
        ),
        "expected a CHECK violation on {column}, got {err}"
    );
}

/// A note row for `rel` with the minimum the schema needs, returning its id.
fn insert_note(db: &Db, rel: &str, title: &str) -> i64 {
    db.0.execute(
        "INSERT INTO notes (rel, title, title_source, encoding, word_count, body)
         VALUES (?1, ?2, 'filename', 'utf-8', 3, 'body text')",
        rusqlite::params![rel, title],
    )
    .expect("test setup: note row");
    db.0.last_insert_rowid()
}

#[test]
fn migrations_are_idempotent() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    // A nested path so the same call is also shown to create the directories it needs.
    let path = tmp.path().join("workspaces").join("ws.sqlite");

    let first = Db::open(&path).expect("first open migrates");
    upsert_file(&first, &snap("a.md", "h1"), Path::new("/ws/a.md"), 7)
        .expect("test setup: one registry row");
    drop(first);
    let db = Db::open(&path).expect("second open applies nothing new");

    // One row per migration after two opens: the version bookkeeping neither re-applies
    // a migration nor counts the same one twice.
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM schema_version"),
        MIGRATIONS.len() as i64
    );
    // The recorded versions are 1..=len with no gaps or duplicates.
    let versions: Vec<i64> =
        db.0.prepare("SELECT version FROM schema_version ORDER BY version")
            .expect("schema_version is readable")
            .query_map([], |row| row.get(0))
            .expect("schema_version rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("schema_version values");
    assert_eq!(versions, (1..=MIGRATIONS.len() as i64).collect::<Vec<_>>());

    // Migration 1 created its tables and the second open neither recreated nor emptied them.
    // Without the written row, a version-bookkeeping bug that dropped and rebuilt a table would
    // look exactly as green as this test did before.
    assert_eq!(
        scalar::<String>(&db, "SELECT hash FROM files WHERE rel = 'a.md'"),
        "h1",
        "the row written before the reopen is still there"
    );
    assert!(path.exists(), "the database file is where it was asked for");
}

#[test]
fn a_schema_newer_than_this_build_is_an_error() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    {
        let db = Db::open(&path).expect("test setup: migrate");
        // A database written by a later version of this program.
        db.0.execute("INSERT INTO schema_version (version) VALUES (99)", [])
            .expect("test setup: future version row");
    }

    let err = Db::open(&path).expect_err("a newer schema must not be opened silently");
    assert!(
        err.to_string().contains("99"),
        "the error has to name the version it cannot handle: {err}"
    );
}

#[test]
fn an_unusable_database_path_names_itself() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    // A regular file where the database's parent directory should be: `create_dir_all` cannot
    // work, and the failure is worthless to a report unless it says which path broke.
    let blocker = tmp.path().join("blocker");
    fs::write(&blocker, b"not a directory").expect("test setup: blocker file");
    let path = blocker.join("ws.sqlite");

    let err = Db::open(&path).expect_err("a file cannot hold a database directory");
    assert!(
        matches!(err, DbError::Io { .. }),
        "expected the io variant, got {err}"
    );
    assert!(
        err.to_string().contains(&path.display().to_string()),
        "the error names the database path: {err}"
    );
}

#[test]
fn the_schema_is_what_the_plan_mandates() {
    let db = Db::in_memory().expect("in-memory database");

    // The table list is the contract Tasks 8-10 write against, so it is pinned by name —
    // including `schema_version`, which is bookkeeping rather than a migration entry.
    let tables = names(
        &db,
        "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
    );
    assert_eq!(
        tables,
        [
            "files",
            "headings",
            "items",
            "links",
            "meta",
            "notes",
            "schema_version",
            "tags"
        ]
    );
    let indexes = names(
        &db,
        "SELECT name FROM sqlite_master WHERE type = 'index' AND sql IS NOT NULL ORDER BY name",
    );
    assert_eq!(
        indexes,
        [
            "files_case_key",
            "links_dst_note_id",
            "notes_rel",
            "tags_tag"
        ]
    );

    // Column names in declaration order: Tasks 8-10 address them by name, so a rename here
    // breaks them rather than merely differing.
    for (table, declared) in [
        ("files", "rel case_key abs hash size mtime state last_seen"),
        (
            "notes",
            "id rel title title_source encoding word_count body",
        ),
        ("headings", "note_id level text line"),
        ("tags", "note_id tag"),
        ("links", "src_note_id dst_note_id kind target line origin"),
        ("items", "note_id text line state marker"),
        ("meta", "key value"),
    ] {
        assert_eq!(
            column_names(&db, table),
            declared.split(' ').collect::<Vec<_>>(),
            "{table} columns"
        );
    }

    // The CHECK constraints are what keep a later milestone from writing a state this
    // program would read as neither live nor stale.
    rejects(
        &db,
        "INSERT INTO files (rel, case_key, abs, hash, size, mtime, state, last_seen)
         VALUES ('a.md', 'a.md', '/ws/a.md', 'h', 1, 1, 'gone', 1)",
        "files.state",
    );
    rejects(
        &db,
        "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
         VALUES (1, NULL, 'html', 'x.md', 1, 'prose')",
        "links.kind",
    );
    rejects(
        &db,
        "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
         VALUES (1, NULL, 'md', 'x.md', 1, 'user')",
        "links.origin",
    );
    rejects(
        &db,
        "INSERT INTO items (note_id, text, line, state, marker)
         VALUES (1, 'task', 1, 'wip', NULL)",
        "items.state",
    );
}

#[test]
fn move_keeps_note_identity() {
    let db = Db::in_memory().expect("in-memory database");
    // An umlaut in the path is deliberate: `case_key` has to follow `walk`'s Rust
    // lowercasing, not SQLite's ASCII-only `LOWER()`, or a renamed umlaut filename would
    // get a case key no snapshot ever produces.
    let from = "Nötes/Ä.md";
    let to = "Nötes/Übersetzt.md";
    upsert_file(
        &db,
        &snap(from, "h1"),
        Path::new("/ws/Nötes/Ä.md"),
        1_700_000_000,
    )
    .expect("upsert a new file");
    let note_id = insert_note(&db, from, "Ä");

    db.0.execute(
        "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
         VALUES (?1, NULL, 'wiki', 'Ü', 4, 'prose')",
        rusqlite::params![note_id],
    )
    .expect("test setup: outgoing link");
    db.0.execute(
        "INSERT INTO headings (note_id, level, text, line) VALUES (?1, 2, 'Zwischen', 3)",
        rusqlite::params![note_id],
    )
    .expect("test setup: heading row");
    let link_before = link_row(&db);

    // An earlier scan reported this path gone. `classify` can only emit `Moved` for a path that
    // is there now, so the move has to bring the row back into the live set.
    mark_stale(&db, from).expect("mark the file stale first");
    assert_eq!(
        scalar::<String>(
            &db,
            &format!("SELECT state FROM files WHERE rel = '{from}'")
        ),
        "stale"
    );

    apply_move(
        &db,
        from,
        to,
        // A move is also a sighting, so `last_seen` moves with it.
        1_700_000_600,
    )
    .expect("move a known file");

    // Identity survives: the same `notes.id`, now readable under the new path.
    assert_eq!(
        scalar::<i64>(&db, &format!("SELECT id FROM notes WHERE rel = '{to}'")),
        note_id,
        "the note keeps its id and follows its file"
    );
    assert_eq!(
        scalar::<i64>(
            &db,
            &format!("SELECT COUNT(*) FROM notes WHERE rel = '{from}'")
        ),
        0,
        "no note row is left behind on the old path"
    );
    // The link row is untouched in every column, including its own rowid.
    assert_eq!(link_row(&db), link_before, "the link survives the rename");
    // Derived rows key on `note_id`, so they never needed rewriting.
    assert_eq!(
        scalar::<i64>(
            &db,
            &format!("SELECT COUNT(*) FROM headings WHERE note_id = {note_id}")
        ),
        1
    );

    // The registry row itself moved, and its derived columns moved with it.
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM files"), 1);
    assert_eq!(
        scalar::<String>(&db, &format!("SELECT state FROM files WHERE rel = '{to}'")),
        "ok",
        "a moved file is present, so it stops being stale"
    );
    assert_eq!(
        scalar::<String>(
            &db,
            &format!("SELECT case_key FROM files WHERE rel = '{to}'")
        ),
        to.to_lowercase(),
        "case_key is lowercased the same way walk.rs lowercases it"
    );
    assert_eq!(
        scalar::<String>(&db, &format!("SELECT abs FROM files WHERE rel = '{to}'")),
        "/ws/Nötes/Übersetzt.md",
        "abs is rebased onto the new rel"
    );
    assert_eq!(
        scalar::<i64>(
            &db,
            &format!("SELECT last_seen FROM files WHERE rel = '{to}'")
        ),
        1_700_000_600
    );
}

#[test]
fn apply_move_reports_rows_it_cannot_rewrite() {
    let db = Db::in_memory().expect("in-memory database");

    // A path the registry never saw: failing loudly beats a move that silently did nothing.
    let missing = apply_move(&db, "gone.md", "here.md", 1).expect_err("unknown rel is a bug");
    assert!(
        missing.to_string().contains("gone.md"),
        "the error names the path: {missing}"
    );

    // A row whose `abs` does not end in its own `rel` cannot be rebased, and guessing an
    // absolute path would put Task 10's reads at the wrong place.
    upsert_file(&db, &snap("a.md", "h1"), Path::new("/ws/a.md"), 1).expect("upsert");
    db.0.execute(
        "UPDATE files SET abs = '/somewhere/else/totally-different.md'",
        [],
    )
    .expect("test setup: unrelated abs");
    let err = apply_move(&db, "a.md", "b.md", 2).expect_err("an unrebasable abs is a bug");
    assert!(
        matches!(err, DbError::BadAbs { .. }),
        "expected BadAbs, got {err}"
    );
    assert_eq!(
        scalar::<String>(&db, "SELECT rel FROM files"),
        "a.md",
        "the failed move changed nothing"
    );
}

#[test]
fn stale_then_reappear_rejoins() {
    let db = Db::in_memory().expect("in-memory database");
    upsert_file(&db, &snap("a.md", "h1"), Path::new("/ws/a.md"), 10).expect("upsert");
    insert_note(&db, "a.md", "A");
    let rowid = scalar::<i64>(&db, "SELECT rowid FROM files WHERE rel = 'a.md'");

    mark_stale(&db, "a.md").expect("mark stale");
    assert_eq!(
        scalar::<String>(&db, "SELECT state FROM files WHERE rel = 'a.md'"),
        "stale"
    );
    // Nothing is destroyed: the note row and its file row are both still there.
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM notes"), 1);
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM files"), 1);

    // The same content at the same path re-joins the live set on the row that already
    // exists — a second row would give the note a second identity.
    upsert_file(&db, &snap("a.md", "h1"), Path::new("/ws/a.md"), 20).expect("re-upsert");
    assert_eq!(
        scalar::<String>(&db, "SELECT state FROM files WHERE rel = 'a.md'"),
        "ok"
    );
    assert_eq!(
        scalar::<i64>(&db, "SELECT rowid FROM files WHERE rel = 'a.md'"),
        rowid,
        "the reappearing file is the same row"
    );
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM files"), 1);
    assert_eq!(
        scalar::<i64>(&db, "SELECT last_seen FROM files WHERE rel = 'a.md'"),
        20
    );
}

#[test]
fn case_key_is_unique() {
    let db = Db::in_memory().expect("in-memory database");
    upsert_file(
        &db,
        &snap("Notes/A.md", "h1"),
        Path::new("/ws/Notes/A.md"),
        1,
    )
    .expect("upsert");

    // Two case-variant paths are two files on Linux but share a `case_key`. The collision
    // has to surface (Task 10 reports that path under `skipped`); overwriting the stored
    // row would hand one note's identity to the other.
    let err = upsert_file(
        &db,
        &snap("notes/a.md", "h2"),
        Path::new("/ws/notes/a.md"),
        2,
    )
    .expect_err("a colliding case_key must fail");
    assert!(
        matches!(
            &err,
            DbError::Sqlite(rusqlite::Error::SqliteFailure(fault, _))
                if fault.code == rusqlite::ErrorCode::ConstraintViolation
        ),
        "expected a constraint violation, got {err}"
    );

    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM files"), 1);
    assert_eq!(
        scalar::<String>(&db, "SELECT hash FROM files WHERE rel = 'Notes/A.md'"),
        "h1",
        "the stored row is not overwritten"
    );
}

#[test]
fn note_counts_tallies_rows_without_loading_them() {
    let db = Db::in_memory().expect("in-memory database");
    upsert_file(&db, &snap("a.md", "h1"), Path::new("/ws/a.md"), 1).expect("upsert a");
    upsert_file(&db, &snap("b.md", "h2"), Path::new("/ws/b.md"), 1).expect("upsert b");
    let a = insert_note(&db, "a.md", "A");
    let b = insert_note(&db, "b.md", "B");

    // One resolved link and one that points at nothing yet.
    db.0.execute(
        "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
         VALUES (?1, ?2, 'md', 'b.md', 1, 'approved')",
        rusqlite::params![a, b],
    )
    .expect("test setup: resolved link");
    db.0.execute(
        "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
         VALUES (?1, NULL, 'wiki', 'Fehlend', 2, 'prose')",
        rusqlite::params![a],
    )
    .expect("test setup: unresolved link");
    for (state, marker) in [("open", "- [ ]"), ("done", "- [x]"), ("paused", "paused")] {
        db.0.execute(
            "INSERT INTO items (note_id, text, line, state, marker) VALUES (?1, 'task', 3, ?2, ?3)",
            rusqlite::params![a, state, marker],
        )
        .expect("test setup: item row");
    }

    assert_eq!(
        note_counts(&db).expect("counts"),
        NoteCounts {
            notes: 2,
            links: 2,
            unresolved_links: 1,
            items_open: 1,
            items_done: 1,
            items_paused: 1,
        }
    );

    // A stale file is hidden from the catalog, not deleted, so its rows keep counting.
    mark_stale(&db, "b.md").expect("mark stale");
    assert_eq!(note_counts(&db).expect("counts after a delete").notes, 2);
}

#[test]
fn wal_and_foreign_keys_are_set_where_they_are_legal() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let db = Db::open(&tmp.path().join("ws.sqlite")).expect("open a file database");
    assert_eq!(
        scalar::<String>(&db, "PRAGMA journal_mode").to_lowercase(),
        "wal",
        "a file database runs in WAL"
    );
    // Caveat for whoever reads a green run as proof: the bundled SQLite is compiled with
    // `-DSQLITE_DEFAULT_FOREIGN_KEYS=1` (libsqlite3-sys build.rs), so `foreign_keys` already
    // reads 1 on a connection this program never configured — deleting the pragma line from
    // `Db::open`/`Db::in_memory` would not fail a single test here. The pragma stays because a
    // non-bundled SQLite defaults it to OFF, and then a move would quietly leave its notes on
    // the old path.
    assert_eq!(
        scalar::<i64>(&db, "PRAGMA foreign_keys"),
        1,
        "foreign keys are enforced, or a move would orphan its notes"
    );

    // `:memory:` rejects WAL; asking for it there would fail every in-memory test.
    let db = Db::in_memory().expect("open an in-memory database");
    assert_eq!(
        scalar::<String>(&db, "PRAGMA journal_mode").to_lowercase(),
        "memory"
    );
    assert_eq!(scalar::<i64>(&db, "PRAGMA foreign_keys"), 1);
}

/// A `files` row insert needing no child table — these cases measure locks, not the schema.
fn insert_file(rel: &str) -> String {
    format!(
        "INSERT INTO files (rel, case_key, abs, hash, size, mtime, state, last_seen)
         VALUES ('{rel}', '{rel}', '/ws/{rel}', 'hash', 1, 1, 'ok', 1)"
    )
}

/// Run `sql` on `blocked` while `holder` still holds a write transaction, then commit the holder.
///
/// The waiting statement cannot run on this thread: only this thread can hand over the lock it is
/// waiting for, so the connection travels to the writer and its outcome comes back. See
/// [`common::contended`] for why the two timings it reports are split.
fn write_while_locked(
    holder: &Db,
    blocked: Db,
    sql: String,
    hold: Duration,
) -> Contention<rusqlite::Result<usize>> {
    contended(
        hold,
        move || blocked.0.execute(&sql, []),
        || {
            holder
                .0
                .execute("COMMIT", [])
                .expect("test: release the write lock");
        },
    )
}

/// The setup both contention tests share: two open connections, then a write transaction on one of
/// them. Opening both first means nothing below races on a schema write — only on the row.
fn locked_pair(path: &Path) -> Db {
    let holder = Db::open(path).expect("open the first connection");
    hold_write_lock(
        |sql| holder.0.execute(sql, []).map(|_| ()),
        &insert_file("a.md"),
    );
    holder
}

#[test]
fn a_second_writer_waits_for_the_held_transaction_instead_of_failing_busy() {
    // `journal_mode = WAL` is justified by two np runs overlapping one workspace (see `Db::open`), and
    // WAL lets one writer in at a time. Without `busy_timeout` the later write is refused the instant it
    // asks, so an ordinary command fails on a lock that clears a moment later.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    let holder = locked_pair(&path);
    let blocked = Db::open(&path).expect("open a second connection while the file is unlocked");

    let contention = write_while_locked(
        &holder,
        blocked,
        insert_file("b.md"),
        Duration::from_millis(HOLD_MS),
    );

    contention
        .outcome
        .expect("the second write has to wait for the lock, not fail with SQLITE_BUSY");
    assert!(
        contention.waited >= Duration::from_millis(HOLD_MS / 2),
        "the statement returned after {:?}, so it never waited for the held lock",
        contention.waited
    );
    assert_eq!(
        names(&holder, "SELECT rel FROM files ORDER BY rel"),
        ["a.md", "b.md"],
        "both writes land once the first transaction commits"
    );
}

#[test]
fn a_connection_with_no_busy_timeout_is_refused_the_same_write_at_once() {
    // The control for the test above. Without it, a green run would prove nothing: this is the same
    // contention with the timeout switched off on the waiting connection, and it has to come back
    // `SQLITE_BUSY` immediately — which is exactly what `BUSY_TIMEOUT_MS` exists to prevent.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    let holder = locked_pair(&path);
    let blocked = Db::open(&path).expect("open a second connection while the file is unlocked");
    blocked
        .0
        .execute_batch("PRAGMA busy_timeout = 0;")
        .expect("test: switch the timeout off");

    let contention = write_while_locked(
        &holder,
        blocked,
        insert_file("b.md"),
        Duration::from_millis(HOLD_MS),
    );

    let err = contention
        .outcome
        .expect_err("with no timeout set, a locked write must be refused");
    assert!(
        matches!(
            &err,
            rusqlite::Error::SqliteFailure(fault, _)
                if fault.code == rusqlite::ErrorCode::DatabaseBusy
        ),
        "expected SQLITE_BUSY, got {err}"
    );
    // `inside_call`, not `waited`: this is the one bound that asks how long SQLite itself hesitated,
    // and `waited` deliberately includes the handoff to the holder plus its whole hold.
    assert!(
        contention.inside_call < Duration::from_millis(HOLD_MS),
        "the refusal is immediate, not a wait: {:?}",
        contention.inside_call
    );
}

#[test]
fn the_busy_timeout_in_force_is_this_crates_and_not_the_drivers_default() {
    // "A write that waits and wins" alone would prove nothing about `BUSY_TIMEOUT_MS`: rusqlite calls
    // `sqlite3_busy_timeout(db, 5000)` on every connection it opens, so deleting the pragma would leave
    // the patient test above green. The hold sits between the two values — `HOLD_PAST_TIMEOUT_MS` is
    // 3000 ms, past this crate's 1800 ms and well inside those five seconds — which keeps the case a
    // discriminator rather than a tautology: our connection gives up with `SQLITE_BUSY` while the holder
    // still has the transaction, and a connection left on the driver default would have waited it out
    // and succeeded. Were the hold ever raised past 5000 ms, or our timeout lowered to where any hold
    // refuses, this test would prove nothing and should be moved rather than loosened.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    let holder = locked_pair(&path);
    let blocked = Db::open(&path).expect("open a second connection while the file is unlocked");

    let contention = write_while_locked(
        &holder,
        blocked,
        insert_file("b.md"),
        Duration::from_millis(HOLD_PAST_TIMEOUT_MS),
    );

    let err = contention
        .outcome
        .expect_err("a lock held past the configured timeout has to be refused eventually");
    assert!(
        matches!(
            &err,
            rusqlite::Error::SqliteFailure(fault, _)
                if fault.code == rusqlite::ErrorCode::DatabaseBusy
        ),
        "expected SQLITE_BUSY, got {err}"
    );
    // Half our timeout below, the driver's default above: the refusal has to land near 1800 ms, which
    // is neither "refused at once" (the control case) nor "waited out a 5 s timeout".
    assert!(
        contention.inside_call >= Duration::from_millis(900),
        "the write gave up after {:?}, which is not the ~1800 ms this crate asks for",
        contention.inside_call
    );
    assert!(
        contention.inside_call < Duration::from_millis(4500),
        "the write waited {:?}, i.e. something nearer to rusqlite's own 5 s default than to ours",
        contention.inside_call
    );
}

#[test]
fn a_note_row_has_to_name_a_known_file() {
    let db = Db::in_memory().expect("in-memory database");

    // `notes.rel REFERENCES files(rel)` is what makes an orphan note impossible; the same
    // key is what `apply_move` cascades along. Like the `PRAGMA foreign_keys` readings in
    // `wal_and_foreign_keys_are_set_where_they_are_legal`, this fires on the bundled build's
    // default too, so it does not prove the pragma — only that the constraint is declared.
    let err = insert_note_err(&db, "never-seen.md");
    assert!(
        matches!(
            &err,
            rusqlite::Error::SqliteFailure(fault, _)
                if fault.code == rusqlite::ErrorCode::ConstraintViolation
        ),
        "expected the foreign key to fire, got {err}"
    );
}

/// The id of the one `links` row, every column of it plus its rowid.
fn link_row(db: &Db) -> (i64, i64, Option<i64>, String, String, i64, String) {
    db.0.query_row(
        "SELECT rowid, src_note_id, dst_note_id, kind, target, line, origin FROM links",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )
    .expect("exactly one link row")
}

/// A note insert that is expected to fail, so the test can assert on the driver error.
fn insert_note_err(db: &Db, rel: &str) -> rusqlite::Error {
    db.0.execute(
        "INSERT INTO notes (rel, title, title_source, encoding, word_count, body)
         VALUES (?1, 'x', 'filename', 'utf-8', 1, 'body')",
        rusqlite::params![rel],
    )
    .expect_err("an orphan note must fail")
}
