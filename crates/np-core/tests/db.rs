//! The workspace database: schema bookkeeping, moves that keep a note's identity, and
//! deletions that only mark a row stale.
//!
//! Every assertion goes through SQL on the open connection, because what Task 10 relies on
//! is the state of the rows, not the return value of a helper. Rows for `notes`, `links`
//! and `items` are inserted here by hand: filling them in is Tasks 7-9's job, and this
//! module must not grow an API just to make its own tests shorter.

use std::path::Path;

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

    drop(Db::open(&path).expect("first open migrates"));
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

    // Migration 1 created its tables and the second open did not recreate or empty them.
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM files"), 0);
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

#[test]
fn a_note_row_has_to_name_a_known_file() {
    let db = Db::in_memory().expect("in-memory database");

    // `notes.rel REFERENCES files(rel)` is what makes an orphan note impossible; the same
    // key is what `apply_move` cascades along.
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
