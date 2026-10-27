//! The workspace database: schema bookkeeping, moves that keep a note's identity,
//! deletions that only mark a row stale, and the foreign keys migration 3 added on top of both.
//!
//! Every assertion goes through SQL on the open connection, because what Task 10 relies on
//! is the state of the rows, not the return value of a helper. Rows for `notes`, `links`
//! and `items` are inserted here by hand: filling them in is Tasks 7-9's job, and this
//! module must not grow an API just to make its own tests shorter.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

// The hold times and the writer/holder handoff live in `tests/common/mod.rs`, which `tests/config.rs`
// uses for the registry as well, so a change to `BUSY_TIMEOUT_MS` moves both binaries together.
mod common;

use common::{contended, hold_write_lock, Contention, HOLD_MS, HOLD_PAST_TIMEOUT_MS};
use np_core::config::Config;
use np_core::db::{
    apply_move, mark_stale, note_counts, upsert_file, Db, DbError, Migration, NoteCounts,
    FK_ORPHANS_KEY, MIGRATIONS, NOTE_MERGE_KEY, PATHS_LOSSY_KEY,
};
use np_core::hash::{sha256_file, Snapshot};
use np_core::path_keys::RelPath;
use np_core::scan;
use tempfile::TempDir;

/// A snapshot as `walk` + [`np_core::hash::sha256_file`] would produce it: `case_key` is
/// the lowercased `rel`.
fn snap(rel: &str, hash: &str) -> Snapshot {
    Snapshot {
        rel_bytes: np_core::path_keys::rel_bytes_of_display(rel),
        case_bytes: np_core::path_keys::case_fold(rel.as_bytes()),
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
///
/// The extended code is what is asserted, not just `ConstraintViolation`: a foreign key reports the
/// same primary code, so matching only that would let an orphan row satisfy the test while proving
/// nothing about the CHECK. Every caller therefore names a parent that exists.
fn rejects(db: &Db, sql: &str, column: &str) {
    let err =
        db.0.execute(sql, [])
            .expect_err("a CHECK constraint has to refuse this value");
    assert!(
        matches!(
            err,
            rusqlite::Error::SqliteFailure(fault, _)
                if fault.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
        ),
        "expected a CHECK violation on {column}, got {err}"
    );
}

/// A note row for `rel` with the minimum the schema needs, returning its id.
///
/// The byte key is bound as bytes here exactly because [`a_string_bind_is_refused_from_every_key_column`]
/// shows what happens otherwise: a BLOB column accepts TEXT unless something refuses it, and this fixture
/// would then hand a note a key no reader can see.
/// The pair for a path spelled as text, which is what these fixtures hold.
fn path(rel: &str) -> RelPath {
    RelPath::from_display(rel)
}

fn insert_note(db: &Db, rel: &str, title: &str) -> i64 {
    db.0.execute(
        "INSERT INTO notes (rel_bytes, rel, title, title_source, encoding, word_count, body)
         VALUES (?1, ?2, ?3, 'filename', 'utf-8', 3, 'body text')",
        rusqlite::params![rel.as_bytes(), rel, title],
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
            // Migration 4's pair of byte-key indexes, the display-fold index it kept for link resolution,
            // and `files_rel` — new, and load-bearing: `rel` lost its unique constraint, so without this
            // index round 1's row resolution and `mark_stale` would scan the table once per file.
            "files_case_bytes",
            "files_case_key",
            "files_rel",
            "links_dst_note_id",
            "notes_rel",
            "notes_rel_bytes",
            "tags_tag",
        ]
    );

    // Column names in declaration order: Tasks 8-10 address them by name, so a rename here
    // breaks them rather than merely differing.
    for (table, declared) in [
        (
            "files",
            "id rel_bytes case_bytes rel case_key abs hash size mtime state last_seen",
        ),
        (
            "notes",
            "id rel_bytes rel title title_source encoding word_count body",
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

    // A note the child rows below can name. Migration 3 makes `note_id` a foreign key, so a made-up
    // id would be refused as an orphan and never reach the CHECK this section is about.
    upsert_file(&db, &snap("a.md", "h"), Path::new("/ws/a.md"), 1)
        .expect("test setup: registry row");
    let note = insert_note(&db, "a.md", "A");

    // The CHECK constraints are what keep a later milestone from writing a state this
    // program would read as neither live nor stale.
    rejects(
        &db,
        "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
         VALUES (x'612e6d64', x'612e6d64', 'a.md', 'a.md', '/ws/a.md', 'h', 1, 1, 'gone', 1)",
        "files.state",
    );
    rejects(
        &db,
        &format!(
            "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
             VALUES ({note}, NULL, 'html', 'x.md', 1, 'prose')"
        ),
        "links.kind",
    );
    rejects(
        &db,
        &format!(
            "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
             VALUES ({note}, NULL, 'md', 'x.md', 1, 'user')"
        ),
        "links.origin",
    );
    rejects(
        &db,
        &format!(
            "INSERT INTO items (note_id, text, line, state, marker)
             VALUES ({note}, 'task', 1, 'wip', NULL)"
        ),
        "items.state",
    );
}

/// The `src_note_id`, `dst_note_id` and `target` of every link row, oldest line first.
fn edges(db: &Db) -> Vec<(i64, Option<i64>, String)> {
    db.0.prepare("SELECT src_note_id, dst_note_id, target FROM links ORDER BY line")
        .expect("links are readable")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("link rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("link values")
}

/// Whether one statement is refused by a foreign key — the extended code again, so an orphan insert
/// cannot be mistaken for the CHECK or UNIQUE violation that shares its primary code.
fn refuses_orphan(db: &Db, sql: &str) {
    let err =
        db.0.execute(sql, [])
            .expect_err("a row naming no note has to be refused");
    assert!(
        matches!(
            &err,
            rusqlite::Error::SqliteFailure(fault, _)
                if fault.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY
        ),
        "expected a foreign-key violation from `{sql}`, got {err}"
    );
}

#[test]
fn a_child_row_that_names_no_note_is_refused() {
    // The invariant "one note row per file, and its children belong to that note" used to be held by
    // hand in `scan::delete_note_row`. One forgotten delete left a child whose dead `note_id` SQLite
    // would hand to the next INSERT, silently re-adopting it; migration 3 says the invariant in the
    // schema instead, so an orphan is refused where it is written.
    let db = Db::in_memory().expect("in-memory database");
    upsert_file(&db, &snap("a.md", "h1"), Path::new("/ws/a.md"), 1)
        .expect("test setup: registry row");
    let a = insert_note(&db, "a.md", "A");

    // Each shape an orphan takes: the three cascade children, an edge whose source note is gone, and
    // an edge whose destination was never a note. A NULL destination stays legal — that is the
    // unresolved state `note_counts` counts — so only the made-up id is refused.
    for sql in [
        "INSERT INTO headings (note_id, level, text, line) VALUES (999, 2, 'H', 1)".to_string(),
        "INSERT INTO tags (note_id, tag) VALUES (999, 'tag')".to_string(),
        "INSERT INTO items (note_id, text, line, state, marker) VALUES (999, 'task', 1, 'open', NULL)"
            .to_string(),
        "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
         VALUES (999, NULL, 'wiki', 'Nobody', 1, 'prose')"
            .to_string(),
        format!(
            "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
             VALUES ({a}, 999, 'wiki', 'Never', 2, 'prose')"
        ),
    ] {
        refuses_orphan(&db, &sql);
    }

    // The same rows against the note that does exist still go in: what is refused is the missing
    // parent, not the child table.
    db.0.execute(
        "INSERT INTO headings (note_id, level, text, line) VALUES (?1, 2, 'H', 1)",
        rusqlite::params![a],
    )
    .expect("a heading of a real note");
}

#[test]
fn deleting_a_note_row_cascades_to_its_children_and_nulls_inbound_edges() {
    // `scan::delete_note_row` writes these five statements by hand. Here only the parent row goes
    // away, which is what shows the schema now does the same thing on its own — and shows the one
    // direction it must not take: an edge pointing *into* the deleted note survives with a NULL
    // destination, because it is another note's prose and `unresolved_links` has to keep counting it.
    let db = Db::in_memory().expect("in-memory database");
    upsert_file(&db, &snap("a.md", "h1"), Path::new("/ws/a.md"), 1)
        .expect("test setup: registry row a");
    upsert_file(&db, &snap("b.md", "h2"), Path::new("/ws/b.md"), 1)
        .expect("test setup: registry row b");
    let a = insert_note(&db, "a.md", "A");
    let b = insert_note(&db, "b.md", "B");

    db.0.execute(
        "INSERT INTO headings (note_id, level, text, line) VALUES (?1, 2, 'H', 1)",
        rusqlite::params![a],
    )
    .expect("test setup: heading");
    db.0.execute(
        "INSERT INTO tags (note_id, tag) VALUES (?1, 'tag')",
        rusqlite::params![a],
    )
    .expect("test setup: tag");
    db.0.execute(
        "INSERT INTO items (note_id, text, line, state, marker) VALUES (?1, 'task', 2, 'open', NULL)",
        rusqlite::params![a],
    )
    .expect("test setup: item");
    // An approved outgoing edge on purpose: a foreign key is not conditional, so `ON DELETE CASCADE`
    // takes an approved edge with its source too. `delete_note_row` documents that consequence.
    db.0.execute(
        "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
         VALUES (?1, ?2, 'md', 'b.md', 3, 'approved')",
        rusqlite::params![a, b],
    )
    .expect("test setup: approved outgoing edge");
    db.0.execute(
        "INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
         VALUES (?1, ?2, 'wiki', 'A', 4, 'prose')",
        rusqlite::params![b, a],
    )
    .expect("test setup: inbound edge");

    db.0.execute("DELETE FROM notes WHERE id = ?1", rusqlite::params![a])
        .expect("delete the note row itself");

    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM headings"), 0);
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM tags"), 0);
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM items"), 0);
    assert_eq!(
        edges(&db),
        [(b, None, "A".to_string())],
        "the source's own edge cascades away; the inbound edge stays and loses only its destination"
    );

    // The NULLed edge is still unresolved rather than forgotten: this is the count a scan reports.
    let counts = note_counts(&db).expect("counts");
    assert_eq!((counts.links, counts.unresolved_links), (1, 1));
}

/// The schema as of version 2 — the first two `MIGRATIONS` entries copied out verbatim, and
/// deliberately **not** read from them. This fixture is history: migration 3 has to upgrade the shape an
/// M1 database holds on disk, so it must not move when the code under test does.
///
/// One file rather than one copy per test binary, because `crates/np-cli/tests/cli.rs` seeds the same
/// version-two file to drive the real binary: two verbatim copies of "version 2" are one silent edit away
/// from two different schemas, and a migration test upgrading a shape nobody shipped would still be
/// green. It is a *test* fixture — reading it into the library instead would put a downgrade path in
/// production code to serve a test.
const VERSION_TWO: &str = include_str!("fixtures/schema-v2.sql");

/// Write a version-two database at `path` holding the three kinds of row migration 3 has to tell
/// apart: children of two live notes, the children of one note whose file was reported gone
/// (`state = 'stale'`, spec §2), and five leftovers from the manual-cleanup era whose `note_id` names a
/// note no scan ever wrote.
///
/// The stale note is not decoration. An orphan clean that asked `files` who is still live, rather than
/// asking `notes` who ever existed, would treat it as one of the five and delete exactly the rows spec
/// §2 exists to protect — while passing every test written against a fixture whose parents are all
/// `'ok'`. [`a_stale_note_keeps_its_children_and_the_edge_that_points_at_it`] pins the difference.
fn seed_version_two(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("test setup: open the old file");
    conn.execute_batch(VERSION_TWO)
        .expect("test setup: version-two schema");
    conn.execute_batch(
        "INSERT INTO files (rel, case_key, abs, hash, size, mtime, state, last_seen) VALUES
            ('a.md', 'a.md', '/ws/a.md', 'ha', 1, 1, 'ok', 1),
            ('b.md', 'b.md', '/ws/b.md', 'hb', 1, 1, 'ok', 1),
            ('c.md', 'c.md', '/ws/c.md', 'hc', 1, 1, 'stale', 1);
         INSERT INTO notes (id, rel, title, title_source, encoding, word_count, body) VALUES
            (1, 'a.md', 'A', 'filename', 'utf-8', 3, 'a'),
            (2, 'b.md', 'B', 'filename', 'utf-8', 3, 'b'),
            (3, 'c.md', 'C', 'filename', 'utf-8', 3, 'c');
         -- Legitimate rows: three notes' own children, and edges between them. Note 3 owns `c.md`,
         -- whose file is gone; its rows are as much a note's as the other two.
         INSERT INTO headings (note_id, level, text, line) VALUES
            (1, 1, 'A heading', 1), (3, 1, 'C heading', 8);
         INSERT INTO tags (note_id, tag) VALUES (1, 'keep'), (2, 'both'), (3, 'old');
         INSERT INTO items (note_id, text, line, state, marker) VALUES
            (2, 'task', 2, 'open', '- [ ]'), (3, 'old task', 8, 'done', '- [x]');
         INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin) VALUES
            (1, 2, 'md', 'b.md', 3, 'prose'),
            -- Written while `c.md` was there. The stale note is this edge's destination rather than its
            -- source, so no rule about deleting an owner would be the thing that repairs it.
            (2, 3, 'wiki', 'C', 9, 'prose');
         -- Orphans: children of note 77, plus one edge whose destination is note 88. Neither id was
         -- ever a note row.
         INSERT INTO headings (note_id, level, text, line) VALUES (77, 2, 'Gone', 4);
         INSERT INTO tags (note_id, tag) VALUES (77, 'gone');
         INSERT INTO items (note_id, text, line, state, marker) VALUES (77, 'task', 5, 'open', NULL);
         INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin) VALUES
            (77, NULL, 'wiki', 'Nobody', 6, 'prose'),
            (1, 88, 'wiki', 'AlsoGone', 7, 'prose');",
    )
    .expect("test setup: seed the old rows");
}

/// Write a version-two database whose `tags` rows are written in an order the `tags_tag` index would
/// not return them in, for [`a_version_two_database_keeps_the_tag_rows_in_rowid_order`]: rowid order is
/// zeta, alpha, mid while reading the index hands back alpha, mid, zeta.
fn seed_tag_disorder(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("test setup: open the old file");
    conn.execute_batch(VERSION_TWO)
        .expect("test setup: version-two schema");
    conn.execute_batch(
        "INSERT INTO files (rel, case_key, abs, hash, size, mtime, state, last_seen) VALUES
            ('a.md', 'a.md', '/ws/a.md', 'ha', 1, 1, 'ok', 1);
         INSERT INTO notes (id, rel, title, title_source, encoding, word_count, body) VALUES
            (1, 'a.md', 'A', 'filename', 'utf-8', 3, 'a');
         INSERT INTO tags (note_id, tag) VALUES (1, 'zeta'), (1, 'alpha'), (1, 'mid');",
    )
    .expect("test setup: seed the tag rows");
}

/// The `meta` value a correct upgrade of [`seed_version_two`] leaves behind: one labelled count for
/// each orphan shape, and no more — the stale note's children must not have raised any of them.
const ONE_OF_EACH: &str = "headings=1,tags=1,items=1,links_src=1,links_dst_null=1";

/// The `meta` value [`FK_ORPHANS_KEY`] documents for a file with nothing to clean, and for one created
/// from scratch. Kept apart from a missing key, which means the file never upgraded.
const NOTHING_REMOVED: &str = "headings=0,tags=0,items=0,links_src=0,links_dst_null=0";

/// The orphan receipt as migration 3 wrote it, read back under the constant it writes under — so a
/// renamed key fails these tests instead of reading `NULL` and comparing equal to nothing.
fn receipt(db: &Db) -> String {
    scalar::<String>(
        db,
        &format!("SELECT value FROM meta WHERE key = '{FK_ORPHANS_KEY}'"),
    )
}

#[test]
fn a_version_two_database_upgrades_and_keeps_every_row_that_has_a_note() {
    // The upgrade path a real M1 workspace takes: an old file opened through the normal code path,
    // which may legitimately hold children left behind before the constraints existed.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_version_two(&path);

    let db = Db::open(&path).expect("the normal open migrates an old file");

    assert_eq!(
        scalar::<i64>(&db, "SELECT MAX(version) FROM schema_version"),
        MIGRATIONS.len() as i64,
        "the foreign-key migration is the newest recorded step"
    );
    // Nothing violates a key once the orphans are gone — and had anything been left behind, the
    // migration would have rolled itself back and this open would have failed.
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM pragma_foreign_key_check()"),
        0
    );

    // Every legitimate row is there, with its note still attached.
    assert_eq!(
        names(
            &db,
            "SELECT note_id || ':' || text FROM headings ORDER BY line"
        ),
        ["1:A heading", "3:C heading"]
    );
    assert_eq!(
        names(
            &db,
            "SELECT note_id || ':' || tag FROM tags ORDER BY note_id, tag"
        ),
        ["1:keep", "2:both", "3:old"]
    );
    assert_eq!(
        names(
            &db,
            "SELECT note_id || ':' || text FROM items ORDER BY line"
        ),
        ["2:task", "3:old task"]
    );
    // The edge into a note that never existed is NULLed rather than deleted: it is the source's prose,
    // and dropping it would quietly lower `unresolved_links`.
    assert_eq!(
        edges(&db),
        [
            (1, Some(2), "b.md".to_string()),
            (1, None, "AlsoGone".to_string()),
            (2, Some(3), "C".to_string())
        ],
        "the resolved edge keeps its destination and the dangling one loses only that"
    );

    // The orphans are gone, not re-attached to something convenient.
    for sql in [
        "SELECT COUNT(*) FROM headings WHERE note_id = 77",
        "SELECT COUNT(*) FROM tags WHERE note_id = 77",
        "SELECT COUNT(*) FROM items WHERE note_id = 77",
        "SELECT COUNT(*) FROM links WHERE src_note_id = 77",
        "SELECT COUNT(*) FROM links WHERE dst_note_id = 88",
    ] {
        assert_eq!(scalar::<i64>(&db, sql), 0, "{sql}");
    }

    // And what was removed is written down where a human can read it back.
    assert_eq!(
        receipt(&db),
        ONE_OF_EACH,
        "the cleanup reports what it deleted"
    );

    // The same value reaches the process that did the deleting through the handle, not a second read of
    // `meta` — and it has to survive **every** later step: migration 4 runs after this one, and a loop that
    // overwrote the receipt with the later step's nothing would leave the upgrading scan silent while
    // `meta` still held the record (`only_the_scan_that_removed_the_orphans_announces_them` in
    // `crates/np-cli/tests/cli.rs` covers the printed half of the same fact).
    assert_eq!(
        db.upgrade_orphans(),
        Some(ONE_OF_EACH),
        "the open that ran the cleanup still carries its receipt after the steps that followed"
    );

    // The constraint is live on the upgraded file, not merely declared.
    refuses_orphan(
        &db,
        "INSERT INTO headings (note_id, level, text, line) VALUES (77, 2, 'Again', 9)",
    );
}

#[test]
fn the_foreign_key_migration_does_not_run_a_second_time() {
    // Same bookkeeping `migrations_are_idempotent` pins for the SQL entries, for a migration that is
    // code rather than a script. A re-run cannot be caught by a leftover `headings_new` name: the
    // rebuild renames the staged table back into place, so nothing of that name survives to collide
    // with, and a second run would quietly rebuild all four tables again. What detects it is what a
    // re-run leaves behind — an extra `schema_version` row, and a receipt recomputed over an
    // already-clean file, which would read as zeros instead of the counts this upgrade earned.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_version_two(&path);
    Db::open(&path)
        .expect("first open upgrades")
        .0
        .close()
        .expect("close");

    let db = Db::open(&path).expect("second open applies nothing new");

    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM schema_version"),
        MIGRATIONS.len() as i64,
        "no migration is recorded twice"
    );
    let versions: Vec<i64> =
        db.0.prepare("SELECT version FROM schema_version ORDER BY version")
            .expect("schema_version is readable")
            .query_map([], |row| row.get(0))
            .expect("schema_version rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("schema_version values");
    assert_eq!(versions, (1..=MIGRATIONS.len() as i64).collect::<Vec<_>>());

    // A re-run would find no orphans left to remove and rewrite the receipt to zeros; the counts are
    // the ones the upgrade earned.
    assert_eq!(receipt(&db), ONE_OF_EACH);
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM links"), 3);
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM tags"), 3);
}

#[test]
fn a_stale_note_keeps_its_children_and_the_edge_that_points_at_it() {
    // Spec §2: a file that is gone is marked stale and nothing of its note is destroyed, so the note can
    // come back later as the same note. Migration 3 has to read "orphan" as "no `notes` row answers
    // this", not as "no live file answers this": switch the cleanup to filter on `files.state = 'ok'`
    // and every assertion below fails, which is exactly the history the spec protects.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_version_two(&path);

    let db = Db::open(&path).expect("the normal open migrates an old file");

    // The fixture really is the case: a stale registry row, and a note that still answers to it.
    assert_eq!(
        scalar::<String>(&db, "SELECT state FROM files WHERE rel = 'c.md'"),
        "stale",
        "the file is the one a scan reported gone"
    );
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM notes WHERE id = 3"),
        1
    );

    // Its children survived the rebuild with their note still named.
    assert_eq!(
        names(
            &db,
            "SELECT note_id || ':' || text FROM headings WHERE note_id = 3"
        ),
        ["3:C heading"]
    );
    assert_eq!(
        names(
            &db,
            "SELECT note_id || ':' || tag FROM tags WHERE note_id = 3"
        ),
        ["3:old"]
    );
    assert_eq!(
        names(
            &db,
            "SELECT note_id || ':' || text || ':' || state FROM items WHERE note_id = 3"
        ),
        ["3:old task:done"]
    );
    // And the edge written to it still points at it. `ON DELETE SET NULL` is for a target that stopped
    // being a note, which a stale one has not.
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM links WHERE dst_note_id = 3"),
        1,
        "the inbound edge keeps its destination"
    );

    // Absent from the receipt, which is what the equality asserts: a cleanup that took the stale note
    // for an orphan would report `headings=2,tags=2,items=2,…`, and one that deleted its inbound edge
    // instead of leaving it alone would report `links_src=2` or `links_dst_null=2`.
    assert_eq!(
        receipt(&db),
        ONE_OF_EACH,
        "nothing belonging to the stale note was counted as removed"
    );
}

#[test]
fn a_freshly_created_database_records_an_empty_orphan_receipt() {
    // [`FK_ORPHANS_KEY`] promises the receipt is written even when every count is zero, so that "looked
    // and found none" reads differently from "this file never upgraded". A brand-new workspace is the
    // smallest case of that promise, and the one every fresh database takes.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");

    let db = Db::open(&path).expect("a new file migrates from scratch");

    assert_eq!(receipt(&db), NOTHING_REMOVED);
}

#[test]
fn a_receipt_that_cannot_be_written_rolls_the_cleanup_back() {
    // F21 §4.2: "never delete unannounced" is enforced by the transaction, not by the order the statements
    // happen to be written in — so this case refuses the `meta` write and then asks whether any row went.
    // Reading the code would let a refactor move the receipt past the commit, or soften its failure into a
    // warning, and keep every other test green; this assertion does not.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_version_two(&path);
    {
        let conn = rusqlite::Connection::open(&path).expect("test setup: reopen the fixture");
        // A `RAISE(ABORT)` trigger is the smallest way to fail **one** statement: migration 3's deletes, its
        // four table rebuilds and its `PRAGMA foreign_key_check` all still run, so a passing assertion below
        // cannot be the accident of some earlier refusal. A trigger rather than a read-only file because
        // `Db::open` opens its own connection, which no test gets to configure.
        conn.execute_batch(
            "CREATE TRIGGER refuse_receipts BEFORE INSERT ON meta
             BEGIN SELECT RAISE(ABORT, 'no receipts here'); END;",
        )
        .expect("test setup: refuse writes to `meta`");
    }

    let err =
        Db::open(&path).expect_err("a cleanup that cannot record its receipt must not commit");
    // Reaching the next line is the proof that this left as an error rather than a panic — which is what it
    // was before F21, where the only failing statement in this path was the stderr write's `.expect`.
    assert!(
        matches!(
            &err,
            DbError::Sqlite(rusqlite::Error::SqliteFailure(fault, _))
                if fault.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER
        ),
        "expected the receipt write itself to be refused, got {err}"
    );
    assert!(
        err.to_string().contains("no receipts here"),
        "the error has to name the refusal whoever would have to undo: {err}"
    );

    // Nothing was deleted. Read through a plain connection, because `Db::open` still refuses this file.
    let conn = rusqlite::Connection::open(&path).expect("the refused file is still readable");
    let one = |sql: &str| -> i64 {
        conn.query_row(sql, [], |row| row.get(0))
            .expect("fixture query reads a number")
    };
    for sql in [
        "SELECT COUNT(*) FROM headings WHERE note_id = 77",
        "SELECT COUNT(*) FROM tags WHERE note_id = 77",
        "SELECT COUNT(*) FROM items WHERE note_id = 77",
        "SELECT COUNT(*) FROM links WHERE src_note_id = 77",
        "SELECT COUNT(*) FROM links WHERE dst_note_id = 88",
    ] {
        assert_eq!(
            one(sql),
            1,
            "{sql}: the cleanup rolled back with its receipt"
        );
    }
    // And it rolled back completely: no receipt row, and a file still at version two — so the next run of a
    // real binary retries the upgrade rather than trusting a half-done one.
    assert_eq!(
        one(&format!(
            "SELECT COUNT(*) FROM meta WHERE key = '{FK_ORPHANS_KEY}'"
        )),
        0,
        "a refused receipt wrote nothing"
    );
    assert_eq!(
        one("SELECT MAX(version) FROM schema_version"),
        2,
        "the step recorded nothing, so the cleanup can be retried"
    );
}

#[test]
fn a_version_two_database_keeps_the_tag_rows_in_rowid_order() {
    // What this test *pins* is migration 3's `ORDER BY rowid` copy (see `rebuild_with_keys` in
    // `src/db.rs`), not a regression that could be happening today: while the copy is sorted, any
    // plan — even one reading `tags_tag` and fetching behind it — must emit the rows in rowid order,
    // so these assertions cannot fail on the current build. They exist for the future change that
    // drops the sort and meets a planner that then answers in another order (a new index, statistics
    // from `ANALYZE`); without them that would surface as every tagged entry line of `INDEX.md`
    // rewriting itself on someone's first scan after an upgrade, with no real change behind it
    // (see the byte-stability promise at `crate::catalog`'s top). The fixture is deliberately spelled
    // out-of-order (`zeta`, `alpha`, `mid`) so the pinned order is distinguishable from a sorted one.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_tag_disorder(&path);

    let db = Db::open(&path).expect("the normal open migrates an old file");

    assert_eq!(
        names(&db, "SELECT tag FROM tags ORDER BY rowid"),
        ["zeta", "alpha", "mid"],
        "the rebuilt table handed out its rowids in the order the rows were written"
    );
    // The exact read `note_tags` performs, so what is pinned is the catalog's order rather than a
    // query's.
    assert_eq!(
        names(&db, "SELECT tag FROM tags ORDER BY note_id, rowid"),
        ["zeta", "alpha", "mid"]
    );
}

#[test]
fn a_file_whose_data_cannot_satisfy_the_keys_refuses_to_upgrade() {
    // The check at the end of migration 3 is not decoration for the keys it adds: it reads the whole
    // database, so a row that broke migration 1's `notes.rel REFERENCES files(rel)` refuses the upgrade.
    // Such a note may not be deleted here — its file can come back, which is what marking a row stale is
    // for — and no parent may be invented for it, so the only honest answer is to say no.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_version_two(&path);
    {
        let conn = rusqlite::Connection::open(&path).expect("test setup: reopen the old file");
        // Enforcement has to be switched off to plant this row at all: the bundled SQLite is built with
        // `SQLITE_DEFAULT_FOREIGN_KEYS=1`, so even a bare connection refuses it. That is the point of the
        // case — only a file that arrived from outside this program can hold such a row.
        conn.execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("test setup: relax keys for the fixture");
        conn.execute(
            "INSERT INTO notes (id, rel, title, title_source, encoding, word_count, body)
             VALUES (77, 'never-was.md', 'Gone', 'filename', 'utf-8', 1, 'body')",
            [],
        )
        .expect("test setup: a note row with no files row");
    }

    let err = Db::open(&path).expect_err("a row the keys refuse has to be reported");
    assert!(
        matches!(
            &err,
            DbError::ForeignKeys { count, table, parent, .. } if *count == 1 && table == "notes" && parent == "files"
        ),
        "expected the orphan note to be named, got {err}"
    );

    // And refusing means refusing: the version stays where it was and every row is as it was written,
    // because the rebuild and the orphan removals happened inside the rolled-back transaction.
    // Read through a plain connection rather than `Db::open`, which would refuse the file again.
    let conn = rusqlite::Connection::open(&path).expect("the refused file is still readable");
    let one = |sql: &str| -> i64 {
        conn.query_row(sql, [], |row| row.get(0))
            .expect("fixture query reads a number")
    };
    assert_eq!(
        one("SELECT MAX(version) FROM schema_version"),
        2,
        "a migration that was refused records nothing"
    );
    assert_eq!(
        one("SELECT COUNT(*) FROM notes WHERE id = 77"),
        1,
        "and it removed nothing either"
    );
}

// ---------------------------------------------------------------------------------------------
// Migration 4 — the byte keys (F13 round 1). The fixture below is the shape a database holds *before*
// that step runs, so it is written out here rather than produced by the code under test.
// ---------------------------------------------------------------------------------------------

/// The schema as of version 3: migration 1's two registry tables exactly as they still stand at that
/// version, plus the four child tables in the shape migration 3 rebuilds them into (its own `DROP` and
/// `RENAME` leave nothing behind to read, so the rebuilt definitions are copied here). Copied verbatim and
/// deliberately **not** read from [`MIGRATIONS`] — like [`VERSION_TWO`] this fixture is history, and
/// migration 4 has to upgrade the shape a database on disk already holds.
const VERSION_THREE: &str = r#"
CREATE TABLE schema_version (version INTEGER PRIMARY KEY);

CREATE TABLE files (
    rel       TEXT    PRIMARY KEY,
    case_key  TEXT    NOT NULL UNIQUE,
    abs       TEXT    NOT NULL,
    hash      TEXT    NOT NULL,
    size      INTEGER NOT NULL,
    mtime     INTEGER NOT NULL,
    state     TEXT    NOT NULL CHECK (state IN ('ok','stale')),
    last_seen INTEGER NOT NULL
);
CREATE INDEX files_case_key ON files(case_key);

CREATE TABLE notes (
    id           INTEGER PRIMARY KEY,
    rel          TEXT REFERENCES files(rel) ON UPDATE CASCADE,
    title        TEXT,
    title_source TEXT,
    encoding     TEXT,
    word_count   INTEGER,
    body         TEXT
);
CREATE INDEX notes_rel ON notes(rel);

CREATE TABLE headings (
    note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    level   INTEGER NOT NULL,
    text    TEXT    NOT NULL,
    line    INTEGER NOT NULL
);

CREATE TABLE tags (
    note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    tag     TEXT NOT NULL
);
CREATE INDEX tags_tag ON tags(tag);

CREATE TABLE links (
    src_note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    dst_note_id INTEGER REFERENCES notes(id) ON DELETE SET NULL,
    kind        TEXT    NOT NULL CHECK (kind IN ('wiki','md','attachment')),
    target      TEXT    NOT NULL,
    line        INTEGER NOT NULL,
    origin      TEXT    NOT NULL CHECK (origin IN ('prose','approved'))
);
CREATE INDEX links_dst_note_id ON links(dst_note_id);

CREATE TABLE items (
    note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    text    TEXT NOT NULL,
    line    INTEGER NOT NULL,
    state   TEXT NOT NULL CHECK (state IN ('open','done','paused')),
    marker  TEXT
);

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT
);

INSERT INTO schema_version (version) VALUES (1), (2), (3);
"#;

/// Write a version-three database for [`a_version_three_database_upgrades_without_moving_anything`],
/// [`the_byte_key_migration_refuses_to_run_while_enforcement_is_on`] and
/// [`the_byte_key_gate_refuses_a_note_whose_file_row_is_gone`] (which adds its own orphan note row),
/// holding every shape migration 4 has to carry across without moving a byte:
///
/// - six `files` rows with different names — plain ASCII, a name that is valid UTF-8 but not ASCII, and a
///   name whose display fold needs Unicode (`ẞ`) — so one `case_bytes` value bound for the whole copy,
///   which passes a one-row fixture, cannot pass this one;
/// - an **attachment** row (`img/logo.png`) with no note at all. It has no child, so nothing cascades if
///   the copy loses it and `PRAGMA foreign_key_check` stays clean while the `DROP` destroys it: only the
///   old-versus-new `COUNT(*)` of `files` can see that row (F13 §4);
/// - one `state = 'stale'` row and a note whose children survive anyway, with **gaps in the note ids**
///   (`1, 2, 5, 7`) so a re-mint hands out `1, 2, 3, 4` and fails;
/// - **gaps in the `files` rowids** (`2, 3, 4, 9, 11, 13`) for the same reason: migration 4's copy carries
///   each rowid into the new `id` column, and consecutive rowids are exactly what letting SQLite re-mint
///   `1..=n` would produce, so a gapless fixture cannot tell the two apart;
/// - note 1 and the stale note 5 each have child rows in all four derived tables, which are the four
///   cascades `DROP TABLE notes` would ride with enforcement on.
fn seed_version_three(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("test setup: open the old file");
    conn.execute_batch(VERSION_THREE)
        .expect("test setup: version-three schema");
    conn.execute_batch(
        "-- The rowids are named and gapped on purpose: `files.id` is what round 2 resolves rows by, so a
         -- copy that dropped the explicit id and let SQLite mint 1..=n has to be caught here.
         INSERT INTO files (rowid, rel, case_key, abs, hash, size, mtime, state, last_seen) VALUES
            (2, 'a.md', 'a.md', '/ws/a.md', 'ha', 11, 101, 'ok', 901),
            (3, 'b.md', 'b.md', '/ws/b.md', 'hb', 12, 102, 'ok', 902),
            (4, 'c.md', 'c.md', '/ws/c.md', 'hc', 13, 103, 'stale', 903),
            (9, 'img/logo.png', 'img/logo.png', '/ws/img/logo.png', 'hp', 14, 104, 'ok', 904),
            (11, 'Straße/Ärger.md', 'straße/ärger.md', '/ws/Straße/Ärger.md', 'hs', 15, 105, 'ok', 905),
            (13, 'STRASSE-ẞ.md', 'strasse-ß.md', '/ws/STRASSE-ẞ.md', 'ht', 16, 106, 'ok', 906);
         INSERT INTO notes (id, rel, title, title_source, encoding, word_count, body) VALUES
            (1, 'a.md', 'A', 'filename', 'utf-8', 3, 'a'),
            (2, 'b.md', 'B', 'filename', 'utf-8', 3, 'b'),
            -- Note 5 belongs to the stale file and note 7 to the non-ASCII name. The ids are not
            -- consecutive on purpose: copying `id` rather than re-minting it is what keeps every derived
            -- row attached (F13 §4), and a fresh 1..=4 would otherwise look identical.
            (5, 'c.md', 'C', 'filename', 'utf-8', 3, 'c'),
            (7, 'Straße/Ärger.md', 'Ärger', 'filename', 'utf-8', 3, 's');
         -- Child rows: notes 1 and 5 each own a row in all four tables, note 7 has a heading, a tag and an
         -- inbound edge, and one edge is unresolved so a NULL destination crosses the migration too.
         INSERT INTO headings (note_id, level, text, line) VALUES
            (1, 2, 'A heading', 1), (7, 1, 'Überschrift', 3), (5, 2, 'C heading', 8);
         INSERT INTO tags (note_id, tag) VALUES (1, 'keep'), (2, 'both'), (5, 'old'), (7, 'straße');
         INSERT INTO items (note_id, text, line, state, marker) VALUES
            (1, 'task', 2, 'open', '- [ ]'), (5, 'old task', 8, 'done', '- [x]');
         INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin) VALUES
            (1, 2, 'md', 'b.md', 3, 'prose'),
            (1, NULL, 'wiki', 'Missing', 4, 'prose'),
            (2, 7, 'wiki', 'Ärger', 9, 'prose'),
            (5, 1, 'wiki', 'A', 10, 'prose');",
    )
    .expect("test setup: seed the old rows");
}

/// Write a version-three database holding one path whose display text carries U+FFFD — what a lossy read
/// leaves behind (F13 §1) — beside an ordinary one, with the lossy file's note row present. The pair for
/// [`the_lossy_receipt_counts_files_and_notes_in_both_shapes`]: this is the shape the receipt exists to name.
fn seed_lossy_version_three(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("test setup: open the old file");
    conn.execute_batch(VERSION_THREE)
        .expect("test setup: version-three schema");
    conn.execute_batch(
        "INSERT INTO files (rel, case_key, abs, hash, size, mtime, state, last_seen) VALUES
            ('caf\u{FFFD}.md', 'caf\u{FFFD}.md', '/ws/caf\u{FFFD}.md', 'hl', 9, 90, 'ok', 909),
            ('plain.md', 'plain.md', '/ws/plain.md', 'hq', 8, 80, 'ok', 908);
         INSERT INTO notes (id, rel, title, title_source, encoding, word_count, body) VALUES
            (4, 'caf\u{FFFD}.md', 'Cafe', 'filename', 'utf-8', 1, 'body');",
    )
    .expect("test setup: seed the lossy row");
}

/// Write a version-three database holding one row for `rel` whose `hash` and `size` are the ones really on
/// disk, plus that file's note row.
///
/// [`seed_lossy_version_three`] cannot be reused here: its digests are placeholders (`'hl'`), which a scan
/// would read as modified content. `mtime` stays an impossible `1` on purpose — the cheap trigger has to
/// *fail*, so the silence this fixture buys comes from the content hash matching, not from a size-and-mtime
/// shortcut.
/// Unix-only for the same reason as its two callers: a name whose bytes are not valid UTF-8 is built
/// with `OsStrExt::from_bytes`, so on Windows this fixture has no user — and an un-gated helper would be
/// dead code there, which `-D warnings` refuses. That is how eight CI runs came to fail on a green Linux
/// suite (F13 §5).
#[cfg(unix)]
fn seed_scanned_lossy_version_three(path: &Path, rel: &str, hash: &str, size: i64) {
    let conn = rusqlite::Connection::open(path).expect("test setup: open the old file");
    conn.execute_batch(VERSION_THREE)
        .expect("test setup: version-three schema");
    conn.execute(
        "INSERT INTO files (rel, case_key, abs, hash, size, mtime, state, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, 'ok', 90)",
        rusqlite::params![rel, rel.to_lowercase(), format!("/ws/{rel}"), hash, size],
    )
    .expect("test setup: seed the scanned lossy row");
    conn.execute(
        "INSERT INTO notes (id, rel, title, title_source, encoding, word_count, body)
         VALUES (4, ?1, 'Cafe', 'filename', 'utf-8', 1, 'body')",
        rusqlite::params![rel],
    )
    .expect("test setup: seed the scanned lossy note");
    // One derived row, so a test can tell "the note was carried" apart from "the note was re-minted and
    // re-parsed": children hang off `notes.id`, which a move must not touch.
    conn.execute(
        "INSERT INTO headings (note_id, level, text, line) VALUES (4, 1, 'Cafe', 1)",
        [],
    )
    .expect("test setup: seed the note's child row");
}

/// The tables whose rows migration 4 must neither lose nor duplicate — the two it rebuilds and the four
/// whose cascades [`np_core::db::MIGRATIONS`]' fourth step could otherwise ride.
const COUNTED_TABLES: &[&str] = &["files", "notes", "headings", "tags", "items", "links"];

/// `COUNT(*)` of each [`COUNTED_TABLES`] table, read from a bare connection so the number is what the
/// fixture wrote rather than something this program recomputed on the way in.
fn file_table_counts(path: &Path) -> Vec<i64> {
    let conn = rusqlite::Connection::open(path).expect("read the fixture's row counts");
    COUNTED_TABLES
        .iter()
        .map(|table| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or_else(|err| panic!("{table} count: {err}"))
        })
        .collect()
}

/// The `files` rows' identity pairs — display path beside the rowid that addressed the row — read from a
/// bare connection before the upgrade.
///
/// Migration 4's copy loop names the id it inserts, so this is the pair every row has to still answer with
/// afterwards (`rel || ':' || id`). No other assertion in the upgrade test can see it: a step that dropped
/// the id column and let SQLite hand out `1..=n` keeps every count, key, child row and column value intact.
fn file_row_ids(path: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).expect("read the fixture's file ids");
    let mut statement = conn
        .prepare("SELECT rel || ':' || rowid FROM files ORDER BY rowid")
        .expect("test setup: read the fixture's file ids");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("file id rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("file id values")
}

/// What each fixture row of `files` has to hold after the upgrade: its display fold and its byte key.
///
/// Both are spelled out instead of computed, because they *are* the assertion. `case_bytes` is the
/// ASCII-only fold of that row's own `rel` bytes (F13 §3): `Ä` keeps its capital, and `ẞ` keeps its own
/// bytes even though the copied `case_key` beside it folds onto `ß` — the two facts of §3 in one row, and
/// the row that fails a fold which reached for `str::to_lowercase`. Repeating them per row is what catches
/// one `case_bytes` bind shared by the whole copy: every name here differs from every other under the
/// ASCII fold, so a stamped-identical key collides or reads wrong.
const MIGRATED_KEYS: &[(&str, &str, &[u8])] = &[
    ("a.md", "a.md", b"a.md"),
    ("b.md", "b.md", b"b.md"),
    ("c.md", "c.md", b"c.md"),
    ("img/logo.png", "img/logo.png", b"img/logo.png"),
    // The ASCII `S` folds while the non-ASCII `Ä` does not, which is what `case_key` beside it — where
    // both folded — shows is a different fact.
    (
        "Straße/Ärger.md",
        "straße/ärger.md",
        "straße/Ärger.md".as_bytes(),
    ),
    // And here the ASCII letters folded while `ẞ` did not — even though `case_key` shows the Unicode fold
    // of the very same name.
    ("STRASSE-ẞ.md", "strasse-ß.md", b"strasse-\xe1\xba\x9e.md"),
];

/// What each fixture row of `files` has to still hold, column by column: `rel, state, hash, size, mtime,
/// last_seen, abs`. Migration 4 copies these verbatim (F13 §4's "carry `hash`, `size`, `mtime`, `state`
/// verbatim"), and any value that moves here is a value the next scan reads as a change.
const MIGRATED_COLUMNS: &[(&str, &str, &str, i64, i64, i64, &str)] = &[
    ("a.md", "ok", "ha", 11, 101, 901, "/ws/a.md"),
    ("b.md", "ok", "hb", 12, 102, 902, "/ws/b.md"),
    ("c.md", "stale", "hc", 13, 103, 903, "/ws/c.md"),
    ("img/logo.png", "ok", "hp", 14, 104, 904, "/ws/img/logo.png"),
    (
        "Straße/Ärger.md",
        "ok",
        "hs",
        15,
        105,
        905,
        "/ws/Straße/Ärger.md",
    ),
    ("STRASSE-ẞ.md", "ok", "ht", 16, 106, 906, "/ws/STRASSE-ẞ.md"),
];

/// The `paths_lossy` receipt migration 4 wrote, read back under the constant it writes under — a renamed
/// key fails here rather than reading `NULL` and comparing equal to nothing.
fn lossy_receipt(db: &Db) -> String {
    scalar::<String>(
        db,
        &format!("SELECT value FROM meta WHERE key = '{PATHS_LOSSY_KEY}'"),
    )
}

#[test]
fn a_version_three_database_upgrades_without_moving_anything() {
    // Migration 4's promise is that it changes the *shape* and nothing else (F13 §4): every key comes from
    // its own row, no id is re-minted, no derived row cascades away, and no column value moves — because
    // `classify` still compares display text, so a value rewritten here would be a scan that reports files
    // it never saw change.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_version_three(&path);

    let before = file_table_counts(&path);
    let ids_before = file_row_ids(&path);
    assert_eq!(
        before,
        vec![6, 4, 3, 4, 2, 4],
        "files, notes, headings, tags, items, links: the fixture is what this test claims to have written"
    );
    assert_eq!(
        ids_before,
        [
            "a.md:2",
            "b.md:3",
            "c.md:4",
            "img/logo.png:9",
            "Straße/Ärger.md:11",
            "STRASSE-ẞ.md:13",
        ],
        "the fixture's rowids are gapped, so a re-minted 1..=6 cannot read as a preserved id"
    );

    let db = Db::open(&path).expect("the normal open runs migration 4");

    assert_eq!(
        scalar::<i64>(&db, "SELECT MAX(version) FROM schema_version"),
        MIGRATIONS.len() as i64,
        "the byte-key migration is the newest recorded step"
    );
    // The gate at the end of the step passed on a file that really did have rows in every table — and had
    // it not, this open would have failed rather than returned.
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM pragma_foreign_key_check()"),
        0
    );

    // Every key was derived from its own row. `rel_bytes` is the UTF-8 bytes of *that* row's display text
    // (round 1 writes a faithful copy, not the OS bytes — F13 §7), `case_bytes` that row's ASCII-only fold,
    // and `case_key` the value the fixture wrote rather than one recomputed by SQL's ASCII `LOWER()`.
    for (rel, case_key, case_bytes) in MIGRATED_KEYS {
        let (rel_bytes, stored_case_key, stored_case_bytes): (Vec<u8>, String, Vec<u8>) =
            db.0.query_row(
                "SELECT rel_bytes, case_key, case_bytes FROM files WHERE rel = ?1",
                rusqlite::params![*rel],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap_or_else(|err| panic!("{rel} has no byte key after the upgrade: {err}"));
        assert_eq!(
            rel_bytes,
            rel.as_bytes(),
            "{rel}: rel_bytes has to be the bytes of that row's own rel"
        );
        assert_eq!(
            stored_case_bytes, *case_bytes,
            "{rel}: case_bytes has to be the ASCII-only fold of those bytes"
        );
        assert_eq!(
            stored_case_key, *case_key,
            "{rel}: case_key is copied, not re-derived by an ASCII-only fold"
        );
    }

    // The ids the fixture wrote, each still naming its own path. `1, 2, 5, 7` rather than `1, 2, 3, 4`
    // because every derived row hangs off `notes.id`, and a re-mint silently re-parents them.
    assert_eq!(
        names(&db, "SELECT id || ':' || rel FROM notes ORDER BY id"),
        ["1:a.md", "2:b.md", "5:c.md", "7:Straße/Ärger.md",],
        "no note was re-minted and none changed its display path"
    );

    // The same claim for `files.id`, which the copy loop also names explicitly. Round 1 reads nothing by that
    // id, so a step that let SQLite re-mint `1..=n` would keep this whole test green — while round 2 resolves
    // every row through it (F13 §4). The gapped fixture rowids are what makes the pair discriminating.
    assert_eq!(
        names(&db, "SELECT rel || ':' || id FROM files ORDER BY id"),
        ids_before,
        "every `files` row keeps the id that addressed it before the upgrade"
    );

    // The four cascading children, still attached to the same notes. Counts alone would pass a migration
    // that lost one row and duplicated another.
    assert_eq!(
        names(
            &db,
            "SELECT note_id || ':' || text FROM headings ORDER BY line"
        ),
        ["1:A heading", "7:Überschrift", "5:C heading"]
    );
    assert_eq!(
        names(
            &db,
            "SELECT note_id || ':' || tag FROM tags ORDER BY note_id, rowid"
        ),
        ["1:keep", "2:both", "5:old", "7:straße"],
        "`tags` is not rebuilt, so its rowids — and the catalog's tag order — cannot move"
    );
    assert_eq!(
        names(
            &db,
            "SELECT note_id || ':' || text || ':' || state FROM items ORDER BY line"
        ),
        ["1:task:open", "5:old task:done"]
    );
    assert_eq!(
        edges(&db),
        [
            (1, Some(2), "b.md".to_string()),
            (1, None, "Missing".to_string()),
            (2, Some(7), "Ärger".to_string()),
            (5, Some(1), "A".to_string()),
        ],
        "no edge cascaded away and the unresolved one kept its NULL destination"
    );

    // The columns a scan compares, verbatim — including the stale row's `state`, which is what keeps that
    // note in the database at all.
    for (rel, state, hash, size, mtime, last_seen, abs) in MIGRATED_COLUMNS {
        let row: (String, String, i64, i64, i64, String) =
            db.0.query_row(
                "SELECT state, hash, size, mtime, last_seen, abs FROM files WHERE rel = ?1",
                rusqlite::params![*rel],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap_or_else(|err| panic!("{rel} is gone after the upgrade: {err}"));
        assert_eq!(
            row,
            (
                state.to_string(),
                hash.to_string(),
                *size,
                *mtime,
                *last_seen,
                abs.to_string()
            ),
            "{rel}: nothing in this row may move"
        );
    }

    // Nothing was lost or duplicated anywhere — the count that can see the attachment go, which no foreign
    // key can report because it has no children.
    let after: Vec<i64> = COUNTED_TABLES
        .iter()
        .map(|table| scalar::<i64>(&db, &format!("SELECT COUNT(*) FROM {table}")))
        .collect();
    assert_eq!(after, before, "every table holds what it held before");

    // And the receipt is there afterwards (its two shapes are [`the_lossy_receipt_counts_files_and_notes_in_both_shapes`]).
    assert_eq!(
        scalar::<i64>(
            &db,
            &format!("SELECT COUNT(*) FROM meta WHERE key = '{PATHS_LOSSY_KEY}'")
        ),
        1,
        "an upgraded file always carries its lossy-path receipt"
    );
}

#[test]
fn the_byte_key_migration_refuses_to_run_while_enforcement_is_on() {
    // `DROP TABLE notes` under enforcement performs an implicit `DELETE FROM`, and four child tables are
    // `ON DELETE CASCADE` — the whole workspace's headings, tags, items and prose edges would go with it
    // and nothing downstream would report a thing (F13 §4). The step cannot switch enforcement off for
    // itself, because the pragma has no effect inside a transaction, so it reads the pragma and refuses.
    //
    // `MIGRATIONS` is public and its `Code` variant carries the step function, which is the named seam §5
    // asks for: nothing reachable through `Db::open` can observe this guard, since `migrate` forces the
    // pragma off before the loop.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_version_three(&path);
    let bytes_before = fs::read(&path).expect("test setup: read the fixture");
    let counts_before = file_table_counts(&path);

    let Migration::Code(step) = MIGRATIONS[3] else {
        panic!("migration 4 is a code step, like migration 3");
    };
    let conn = rusqlite::Connection::open(&path).expect("reopen the version-three file");
    // Outside any transaction, which is the only place the pragma means anything — and therefore the
    // state a refactor of `migrate`, or another caller, could hand the step.
    conn.execute_batch("PRAGMA foreign_keys = ON")
        .expect("test setup: switch enforcement on");
    assert_eq!(
        conn.query_row::<i64, _, _>("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("read the pragma back"),
        1,
        "the fixture really is the case this guard refuses"
    );

    let err = step(&conn).expect_err("a step that can drop a workspace has to refuse enforcement");
    assert!(
        matches!(&err, DbError::EnforcementOn { value } if *value == 1),
        "expected the step's own error naming the pragma, got {err}"
    );
    assert!(
        err.to_string().contains("foreign_keys"),
        "the message has to say what was wrong: {err}"
    );

    // Refusing means refusing: not one byte of the file changed, so the version stayed at 3 and every row
    // is where the fixture left it.
    drop(conn);
    assert_eq!(
        fs::read(&path).expect("re-read the refused file"),
        bytes_before,
        "a refused migration writes nothing at all"
    );
    let conn = rusqlite::Connection::open(&path).expect("the refused file is still readable");
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT MAX(version) FROM schema_version", [], |row| row
            .get(0))
            .expect("schema_version is readable"),
        3,
        "the refused step recorded nothing"
    );
    drop(conn);
    assert_eq!(
        file_table_counts(&path),
        counts_before,
        "and cascaded nothing away either"
    );
}

#[test]
fn the_byte_key_gate_refuses_a_note_whose_file_row_is_gone() {
    // A gate is only a defence if something can be turned away by it (F13 §5), and every fixture that has
    // ever reached migration 4's `PRAGMA foreign_key_check` gave its notes parents — so the suite had never
    // seen it fire. An orphan note row is the case the step cannot repair: it may not delete the note (its
    // file may come back, which is what `state = 'stale'` is for) and may not invent a parent for it, so
    // refusing the upgrade is the only honest answer left.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_version_three(&path);
    {
        let conn = rusqlite::Connection::open(&path).expect("test setup: reopen the old file");
        // Enforcement has to be off to plant this at all — the bundled SQLite is built with
        // `SQLITE_DEFAULT_FOREIGN_KEYS=1`, so even a bare connection refuses it. That is the point of the
        // case: only a file that arrived from outside this program can hold such a row, which is how
        // migration 3 came to be counting orphans under [`FK_ORPHANS_KEY`] in the first place.
        conn.execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("test setup: relax keys for the fixture");
        conn.execute(
            "INSERT INTO notes (id, rel, title, title_source, encoding, word_count, body)
             VALUES (77, 'never-was.md', 'Gone', 'filename', 'utf-8', 1, 'body')",
            [],
        )
        .expect("test setup: a note row whose `rel` names no files row");
    }

    // The fixture is what this test claims to have written: six files, five notes — the fifth answered by
    // nothing — beside the child rows `seed_version_three` wrote. A file sitting at version 3 means only
    // migration 4 can run, so the error below cannot belong to an earlier step.
    let counts = file_table_counts(&path);
    assert_eq!(
        counts,
        vec![6, 5, 3, 4, 2, 4],
        "files, notes, headings, tags, items, links: the orphan is the fifth note"
    );

    let err =
        Db::open(&path).expect_err("a key the rebuilt table cannot resolve has to be refused");
    assert!(
        matches!(
            &err,
            DbError::ForeignKeys { count, table, parent, rowid }
                if *count == 1
                    && table == "notes"
                    && parent == "files"
                    && *rowid == 77
        ),
        "expected the gate to name the planted orphan (note id 77) and the parent it misses, got {err}"
    );

    // And refusing means refusing: the version stays at 3, `files` keeps its version-3 shape rather than
    // half a rebuild, and every table holds exactly what the fixture wrote. Read through a plain connection,
    // because `Db::open` refuses this file again.
    let conn = rusqlite::Connection::open(&path).expect("the refused file is still readable");
    let one = |sql: &str| -> i64 {
        conn.query_row(sql, [], |row| row.get(0))
            .expect("fixture query reads a number")
    };
    assert_eq!(
        one("SELECT MAX(version) FROM schema_version"),
        3,
        "a migration the gate refuses records nothing"
    );
    assert_eq!(
        one("SELECT COUNT(*) FROM pragma_table_info('files') WHERE name = 'rel_bytes'"),
        0,
        "and it leaves no half-applied rebuild behind"
    );
    drop(conn);
    assert_eq!(
        file_table_counts(&path),
        counts,
        "no row was deleted, moved or duplicated by the step that failed"
    );
}

/// A single `?1` bind of `'a.md'` — a Rust `&str`, which rusqlite hands to SQLite as TEXT — into one key
/// column, and the CHECK that has to refuse it.
///
/// This is the exact accident F13 cannot survive: BLOB is an affinity rather than a constraint, a TEXT key
/// conflicts with nothing and violates nothing beside its own `typeof` check, and two rows holding one
/// identity in two types that never compare equal is the original bug with better furniture (F13 §4).
fn refuses_string_bind(db: &Db, sql: &str, column: &str) {
    let err =
        db.0.execute(sql, rusqlite::params!["a.md"])
            .expect_err("a string bind into a byte key has to be refused at the write");
    assert!(
        matches!(
            &err,
            rusqlite::Error::SqliteFailure(fault, _)
                if fault.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK
        ),
        "expected the `typeof(…) = 'blob'` CHECK on {column}, got {err}"
    );
}

#[test]
fn a_string_bind_is_refused_from_every_key_column() {
    // One case per key column. `insert_note` and `upsert_file` bind bytes, so nothing else in the suite
    // would notice a CHECK quietly dropped from one of these three declarations — and the API shape is the
    // design's first defence, which makes this the backstop that has to be tested directly.
    let db = Db::in_memory().expect("in-memory database");
    upsert_file(&db, &snap("a.md", "h1"), Path::new("/ws/a.md"), 1)
        .expect("test setup: a registry row for the note's key to name");

    // `files.rel_bytes`: the TEXT `'a.md'` beside it is a *different* row's display text, and SQLite stores
    // no conflict — which is precisely how a string key would hide.
    refuses_string_bind(
        &db,
        "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
         VALUES (?1, x'622e6d64', 'b.md', 'b.md', '/ws/b.md', 'h2', 1, 1, 'ok', 1)",
        "files.rel_bytes",
    );
    // `files.case_bytes`, with a perfectly good BLOB in its sibling column.
    refuses_string_bind(
        &db,
        "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
         VALUES (x'622e6d64', ?1, 'b.md', 'b.md', '/ws/b.md', 'h2', 1, 1, 'ok', 1)",
        "files.case_bytes",
    );
    // `notes.rel_bytes`, naming the file row above so what refuses it is the CHECK and not the key.
    refuses_string_bind(
        &db,
        "INSERT INTO notes (rel_bytes, rel, title, title_source, encoding, word_count, body)
         VALUES (?1, 'a.md', 'A', 'filename', 'utf-8', 1, 'body')",
        "notes.rel_bytes",
    );

    // Refused means not written: neither a key nor a row was left half-typed.
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM files"), 1);
    assert_eq!(scalar::<i64>(&db, "SELECT COUNT(*) FROM notes"), 0);
}

#[test]
fn the_lossy_receipt_counts_files_and_notes_in_both_shapes() {
    // `paths_lossy` is the only way a person can tell the first byte-keyed scan's expected churn from a
    // regression (F13 §4.2), so both shapes have to be readable apart: a workspace with nothing flagged,
    // and one whose display text carries U+FFFD. The format is `files=<n>,notes=<n>`, the same shape as
    // migration 3's receipt — labels, in a fixed order, comma-separated.
    let tmp = TempDir::new().expect("test setup: tempdir");

    let clean = tmp.path().join("clean.sqlite");
    seed_version_three(&clean);
    let db = Db::open(&clean).expect("upgrade a workspace with nothing lossy");
    assert_eq!(
        lossy_receipt(&db),
        "files=0,notes=0",
        "zero is written down; its absence would mean the file never upgraded"
    );

    let lossy = tmp.path().join("lossy.sqlite");
    seed_lossy_version_three(&lossy);
    let db = Db::open(&lossy).expect("upgrade a workspace holding one lossy path");
    // One `files` row and its own note row, both displayed as `caf\u{FFFD}.md`; the plain file beside them
    // is not counted.
    assert_eq!(
        lossy_receipt(&db),
        "files=1,notes=1",
        "the count names both tables, so a flagged path is never read as an unflagged one"
    );
}

/// §4.2's churn, which is round 2's to own: the first **byte-keyed** scan of a workspace whose stored key
/// came from lossy display text.
///
/// The fixture is a version-3 database holding one row displayed as `caf\u{FFFD}.md` over a workspace whose
/// real file is `caf\xe9.md`, driven straight into this build with no scan in between — the only route by
/// which a migrated key is ever compared. Migration 4 could only copy what the row held, so it wrote
/// `63 61 66 EF BF BD 2E 6D 64`; the walk computes `63 61 66 E9 2E 6D 64`. Two byte keys, one file on disk,
/// identical display text.
///
/// Because `classify` pairs unmatched snapshots **by hash**, unchanged content means one `Moved` — not a
/// `Removed` + `Added` pair — and it is the *quiet* shape only because §4.1's change list carries bytes: a
/// pass that expressed the rename in display text would rename the row onto itself and pair the same two
/// keys again forever, which is §1's never-converges symptom manufactured by the fix. So the assertions are:
/// one `Moved` and nothing else, the real bytes in the key, the note carried with its id and its child rows,
/// and a following scan with nothing to report.
#[cfg(unix)]
#[test]
fn a_migrated_lossy_key_is_rewritten_by_the_first_byte_keyed_scan() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).expect("test setup: workspace directory");
    let on_disk = root.join(OsString::from_vec(b"caf\xe9.md".to_vec()));
    fs::write(&on_disk, "# Cafe\n").expect("test setup: write the non-UTF-8 named note");

    // The lossy spelling of that name — what a version-3 `rel` could hold, and what the row still displays.
    let display = "caf\u{fffd}.md";
    let cfg = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    let db_path = cfg.workspace_db(&root);
    fs::create_dir_all(
        db_path
            .parent()
            .expect("test setup: the workspace database sits in a directory"),
    )
    .expect("test setup: database directory");
    let hash = sha256_file(&on_disk).expect("test setup: hash the note the way a scan does");
    let size = fs::metadata(&on_disk)
        .expect("test setup: read the note's size")
        .len() as i64;
    seed_scanned_lossy_version_three(&db_path, display, &hash, size);
    assert_eq!(
        rusqlite::Connection::open(&db_path)
            .expect("test: read the fixture's version")
            .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("test: the fixture records its versions"),
        3,
        "premise: a version-three file, so the scan below runs migration 4 on its way in"
    );

    let report = scan(&root, &cfg).expect("the first byte-keyed scan of a lossy workspace");
    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!(
        (report.added, report.modified, report.moved, report.removed),
        (0, 0, 1, 0),
        "unchanged content under a second byte key is one `Moved`, and nothing else: {report:?}"
    );
    assert_eq!(
        report.unchanged, 0,
        "the row did change — its key moved: {report:?}"
    );

    let db = Db::open(&db_path).expect("test: reopen the migrated database");
    // The receipt named this workspace before any comparison ran (migration 4 writes it on the way in), which
    // is how a person tells this `Moved` from a regression: `files=1` predicts about one such path.
    assert_eq!(
        lossy_receipt(&db),
        "files=1,notes=1",
        "the flagged row is what the churn belongs to"
    );
    let key: Vec<u8> =
        db.0.query_row(
            "SELECT rel_bytes FROM files WHERE rel = ?1",
            rusqlite::params![display],
            |row| row.get(0),
        )
        .expect("the lossy row carries a byte key");
    assert_eq!(
        key,
        vec![0x63, 0x61, 0x66, 0xE9, 0x2E, 0x6D, 0x64],
        "the walk's real bytes replaced the bytes of the display spelling"
    );

    // Identity survived the rewrite rather than being re-minted: same `notes.id`, its child row still hung
    // off it, and the note now displays the path the catalog renders.
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM headings WHERE note_id = 4"),
        1,
        "the move carried the note and everything derived from it"
    );
    assert_eq!(
        scalar::<String>(&db, "SELECT title FROM notes WHERE id = 4"),
        "Cafe",
        "one note row, moved rather than replaced"
    );
    drop(db);

    // Convergence — the assertion that the key was actually **rewritten** rather than re-derived every pass.
    let again = scan(&root, &cfg).expect("test: second scan");
    assert_eq!(
        (
            again.added,
            again.modified,
            again.moved,
            again.removed,
            again.unchanged
        ),
        (0, 0, 0, 0, 1),
        "the churn is one-time: {again:?}"
    );
    assert!(again.skipped.is_empty(), "{again:?}");
}

/// §4.2's third shape, which is §1's actual workspace: one migrated row carrying `caf\xe9.md`'s digest with
/// **both** twins on disk. The row can only answer for one of them — the other was never written at all, its
/// title and tags and links simply lost — so the first byte-keyed scan reports one `Moved` (the path whose
/// hash survives) plus one `Added` (the twin nobody ever wrote). That is the swallowed path coming back, and
/// §2 says only a rescan can bring it.
#[cfg(unix)]
#[test]
fn a_migrated_row_over_two_twins_reports_one_move_and_brings_the_lost_note_back() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).expect("test setup: workspace directory");
    let e9 = root.join(OsString::from_vec(b"caf\xe9.md".to_vec()));
    let e8 = root.join(OsString::from_vec(b"caf\xe8.md".to_vec()));
    fs::write(&e9, "# Cafe nine\n").expect("test setup: write the twin the row knows");
    fs::write(&e8, "# Cafe eight\n").expect("test setup: write the twin that was never indexed");

    let cfg = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    let db_path = cfg.workspace_db(&root);
    fs::create_dir_all(db_path.parent().expect("test setup: database directory"))
        .expect("test setup: database directory");
    // The row holds the byte-largest path's digest, which is what F13 §1 observed on the real workspace.
    let hash = sha256_file(&e9).expect("test setup: hash the surviving twin");
    let size = fs::metadata(&e9)
        .expect("test setup: read the surviving twin")
        .len() as i64;
    seed_scanned_lossy_version_three(&db_path, "caf\u{fffd}.md", &hash, size);

    let report = scan(&root, &cfg).expect("the first byte-keyed scan over a twin pair");
    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!(
        (report.added, report.modified, report.moved, report.removed),
        (1, 0, 1, 0),
        "one path travels onto its real key, the other is indexed for the first time: {report:?}"
    );
    assert_eq!(
        report.notes, 2,
        "both notes are live by this scan: {report:?}"
    );

    let db = Db::open(&db_path).expect("test: reopen the workspace");
    assert_eq!(
        names(
            &db,
            "SELECT hex(rel_bytes) || '=' || title FROM files f JOIN notes n USING (rel_bytes) ORDER BY f.rel_bytes"
        ),
        [
            // The lower key is the twin that was never written: fresh row, fresh note, re-derived content.
            "636166E82E6D64=Cafe eight".to_string(),
            // The higher key is the migrated row, renamed onto its real bytes with its note carried — and
            // re-parsed, because a version-3 database holds no parse-rules label (F15), so this scan
            // refreshes every note it touches. Same `notes.id`, which is what "carried" means here.
            "636166E92E6D64=Cafe nine".to_string(),
        ],
        "two live rows by the first scan, each with its own note"
    );
    drop(db);

    let again = scan(&root, &cfg).expect("test: second scan");
    assert_eq!(
        (again.added, again.moved, again.removed, again.unchanged),
        (0, 0, 0, 2),
        "the churn is one-time and the pair converges: {again:?}"
    );
}

/// Extra rows for [`changed_content_under_a_migrated_lossy_key_mints_a_fresh_note_id`]: every child shape that
/// can hang off the lossy note (the helper's heading, plus a tag and an item here), and one edge pointing
/// **into** it.
///
/// The edge's source is a note whose `files` row is already `stale`, and that is the whole trick: a live source
/// would be re-parsed by this very scan — a version-3 database holds no parse-rules label, so F15 refreshes the
/// whole corpus — and its prose edges would be deleted and re-derived from its text, which is a different
/// mechanism than the one under test. A stale source is never re-parsed, so its row survives with
/// `dst_note_id` still set: exactly the shape `db::LIVE_EDGE` exists for, an edge that reports unresolved
/// **without** holding a NULL.
#[cfg(unix)]
fn seed_children_and_an_inbound_edge(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("test setup: open the seeded file");
    conn.execute_batch(
        "-- Note 4 is the lossy note `seed_scanned_lossy_version_three` wrote.
         INSERT INTO tags (note_id, tag) VALUES (4, 'carried');
         INSERT INTO items (note_id, text, line, state, marker)
              VALUES (4, 'a carried item', 2, 'open', '- [ ]');
         -- A path the row knows about that no longer exists: stale already, so this scan neither re-parses
         -- it nor rewrites its prose edges, and `known_rows` leaves it out of the comparison entirely.
         INSERT INTO files (rel, case_key, abs, hash, size, mtime, state, last_seen)
              VALUES ('gone.md', 'gone.md', '/ws/gone.md', 'hg', 7, 1, 'stale', 80);
         INSERT INTO notes (id, rel, title, title_source, encoding, word_count, body)
              VALUES (6, 'gone.md', 'Gone', 'filename', 'utf-8', 1, 'body');
         INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin)
              VALUES (6, 4, 'wiki', 'Cafe', 3, 'prose');",
    )
    .expect("test setup: seed the note's children and its inbound edge");
}

/// §4.2's churn shape that re-mints a note id — the one place in round 2 where churn can silently orphan
/// derived rows.
///
/// Same fixture as [`a_migrated_lossy_key_is_rewritten_by_the_first_byte_keyed_scan`] (a version-3 row displayed
/// as `caf\u{fffd}.md` over a real file named `caf\xe9.md`, driven straight into this build), with one addition:
/// the file's **content** is rewritten before the scan. Now the two byte keys hold two different hashes, so no
/// move claim exists and the shape is `Removed` + `Added` rather than `Moved`. That difference is what §4.2 names
/// as costly: the re-added note is a **fresh `notes.id`**, so edges into the old row now name a stale file and
/// land in `unresolved_links`, while the stale row itself keeps every derived row and simply leaves the catalog.
#[cfg(unix)]
#[test]
fn changed_content_under_a_migrated_lossy_key_mints_a_fresh_note_id() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).expect("test setup: workspace directory");
    let on_disk = root.join(OsString::from_vec(b"caf\xe9.md".to_vec()));
    fs::write(&on_disk, "# Cafe\n").expect("test setup: write the non-UTF-8 named note");

    let display = "caf\u{fffd}.md";
    let cfg = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    let db_path = cfg.workspace_db(&root);
    fs::create_dir_all(
        db_path
            .parent()
            .expect("test setup: the workspace database sits in a directory"),
    )
    .expect("test setup: database directory");
    // The row holds the digest of the content as it was *before* the edit, which is what makes this the loud
    // shape: `classify` pairs unmatched snapshots by hash and has nothing to pair here.
    let hash = sha256_file(&on_disk).expect("test setup: hash the note as the migration left it");
    let size = fs::metadata(&on_disk)
        .expect("test setup: read the note's size")
        .len() as i64;
    seed_scanned_lossy_version_three(&db_path, display, &hash, size);
    seed_children_and_an_inbound_edge(&db_path);

    fs::write(&on_disk, "the same note, rewritten after the migration\n")
        .expect("test setup: change the note's content");

    let report = scan(&root, &cfg).expect("the first byte-keyed scan of edited, migrated content");
    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!(
        (
            report.added,
            report.modified,
            report.moved,
            report.removed,
            report.unchanged
        ),
        (1, 0, 0, 1, 0),
        "two hashes under two keys is one `Removed` and one `Added`, never a move: {report:?}"
    );
    // One live note row — the fresh one. The stale row and the already-stale source of the inbound edge are
    // both out of the catalog, which counts exactly what this number counts.
    assert_eq!(report.notes, 1, "{report:?}");
    // §4.2's cost, stated as a number and a name: the edge into the old note is unresolved again.
    assert_eq!(report.unresolved_links, 1, "{report:?}");
    assert_eq!(report.unresolved_targets, vec!["Cafe".to_string()]);

    let db = Db::open(&db_path).expect("test: reopen the migrated database");
    // Which row is which: the migrated key survives as the stale one, still holding the bytes of the display
    // spelling; the walk's real bytes are the live row.
    assert_eq!(
        names(
            &db,
            "SELECT hex(rel_bytes) || '=' || state FROM files ORDER BY rel_bytes"
        ),
        [
            // 63 61 66 E9 2E 6D 64 — the real name, live from this scan on.
            "636166E92E6D64=ok".to_string(),
            "636166EFBFBD2E6D64=stale".to_string(),
            "676F6E652E6D64=stale".to_string(),
        ],
        "one file, two keys: the old one marked stale rather than deleted"
    );

    // Nothing detached when the row went stale: `mark_stale` writes `state` and no child row is touched.
    assert_eq!(
        names(
            &db,
            "SELECT 'headings=' || (SELECT COUNT(*) FROM headings WHERE note_id = 4)
                     || ' tags=' || (SELECT COUNT(*) FROM tags WHERE note_id = 4)
                     || ' items=' || (SELECT COUNT(*) FROM items WHERE note_id = 4)
                     || ' links=' || (SELECT COUNT(*) FROM links WHERE dst_note_id = 4)"
        ),
        ["headings=1 tags=1 items=1 links=1".to_string()],
        "the stale row keeps its children — they are history, not garbage"
    );
    // And the unresolved edge is unresolved because of `LIVE_EDGE`, not because anything NULLed it out: the
    // destination is still note 4, which is now behind a stale file.
    assert_eq!(
        names(
            &db,
            "SELECT src_note_id || '->' || COALESCE(CAST(dst_note_id AS TEXT), 'NULL')
                     || '/' || target FROM links"
        ),
        ["6->4/Cafe".to_string()],
        "the edge stands with its destination attached"
    );

    // The id is what changed. Read structurally rather than as a literal, because the number is SQLite's to hand
    // out; what matters is that it is not 4, and that this note's derived rows came from re-parsing the new
    // content (no heading in it) instead of riding over from the old row.
    let fresh_id = scalar::<i64>(
        &db,
        "SELECT n.id FROM notes n JOIN files f ON f.rel_bytes = n.rel_bytes WHERE f.state = 'ok'",
    );
    assert_ne!(
        fresh_id, 4,
        "the re-added note is a new row: §4.2's id re-mint"
    );
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM notes"),
        3,
        "old note, fresh note, and the stale source of the edge"
    );
    assert_eq!(
        names(
            &db,
            "SELECT n.title || '/' || n.title_source || '/h' || \
                    (SELECT COUNT(*) FROM headings h WHERE h.note_id = n.id) \
             FROM notes n JOIN files f ON f.rel_bytes = n.rel_bytes \
             WHERE f.state = 'ok'"
        ),
        // The stem is the whole display name minus the extension, replacement character included — the
        // display spelling of a name SQLite has no honest bytes for.
        ["caf\u{fffd}/filename/h0".to_string()],
        "re-derived, not carried: title from the file name, no heading rows"
    );

    // The catalog is what a person reads, so the detachment has to show there as well: the fresh note is
    // published under its re-derived title and the stale row's old title is gone from the page.
    let index = fs::read_to_string(root.join("INDEX.md")).expect("the scan publishes INDEX.md");
    assert!(
        index.contains("[[caf\u{fffd}]]"),
        "the re-added note: {index}"
    );
    assert!(
        !index.contains("[[Cafe]]"),
        "the stale row left the catalog: {index}"
    );
}

#[test]
fn upsert_file_keys_come_from_rel_and_never_from_abs() {
    // The key travels on the snapshot; the byte-exact `abs` this writer is still handed is display material
    // only (`abs.to_string_lossy()` into the `abs` column). Re-deriving a key from it would silently disagree
    // with the walk for any name that does not round-trip. This test hands `upsert_file` an `abs` sharing no
    // bytes with the snapshot's path, so it can fail for exactly one reason — and every other fixture here
    // spells its `abs` as `/ws/` + `rel`, where the two routes look identical.
    let db = Db::in_memory().expect("in-memory database");
    upsert_file(
        &db,
        &snap("a.md", "h1"),
        // Not `/ws/a.md`: an abs-derived key would read as a different path, not as the same one prefixed.
        Path::new("/ws/OTHER-name.md"),
        1,
    )
    .expect("upsert a row whose abs names some other file entirely");

    // Hex rather than bytes: a failure then reads `612E6D64` against `2F77732F4F544845522D6E616D652E6D64`,
    // which names the mistake without needing a decode table beside it.
    assert_eq!(
        scalar::<String>(&db, "SELECT hex(rel_bytes) FROM files"),
        "612E6D64",
        "rel_bytes is `a.md`'s own bytes; the writer may not take the key from the abs it is handed"
    );
    assert_eq!(
        scalar::<String>(&db, "SELECT hex(case_bytes) FROM files"),
        "612E6D64",
        "and the ASCII-case fold is that key's fold, not the abs's"
    );
}

/// Plant **two `notes` rows for one path** on a database whose migrations have already run.
///
/// This used to be a version-three fixture. Migration 5 (F19) merges any pair it can see at open time, so the
/// only way to meet a pair *as a pair* is to write one after the migrations ran — which is exactly the
/// population `docs/design-f19-note-merge.md` §3 says the reader exists for: a database another tool wrote.
///
/// The values are the ones the old fixture used, so the assertions below read as they always did: ids 4 and 9
/// (non-consecutive, because that is what a legacy file looks like) beside one `files` row whose `hash` matches
/// the note on disk and whose `mtime` is an impossible `1`. That mtime makes the cheap size-and-mtime trigger
/// fail so the digest is compared and `classify` calls the file unchanged: a note that is *not* rewritten is
/// what keeps property (1) below a statement about the reader rather than about `store_note`.
fn plant_duplicate_notes_after_migration(db: &Db, rel: &str, hash: &str, size: i64) {
    // A hex literal, not a bind: `rel_bytes` is a BLOB under a `typeof(rel_bytes) = 'blob'` CHECK, and a TEXT
    // bind is refused rather than quietly matching (F13 §4). `dup.md` is ASCII, so its case fold is itself.
    let key = "X'6475702E6D64'";
    db.0.execute(
        &format!(
            "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state,
                                last_seen)
             VALUES ({key}, {key}, ?1, ?2, ?3, ?4, ?5, 1, 'ok', 90)"
        ),
        rusqlite::params![rel, rel.to_lowercase(), format!("/ws/{rel}"), hash, size],
    )
    .expect("test setup: seed the duplicated-note file row");
    for (id, title, body) in [(4, "Dup first", "first"), (9, "Dup second", "second")] {
        db.0.execute(
            &format!(
                "INSERT INTO notes (id, rel_bytes, rel, title, title_source, encoding, word_count, body)
                 VALUES (?1, {key}, ?2, ?3, 'filename', 'utf-8', 1, ?4)"
            ),
            rusqlite::params![id, rel, title, body],
        )
        .expect("test setup: two note rows for one path");
    }
}

#[test]
fn two_note_rows_on_one_path_share_one_rel_bytes_so_a_note_id_lookup_cannot_be_a_bare_query_row() {
    // `notes.rel` was never UNIQUE, so one path can hold two note rows and `rel_bytes` carries no UNIQUE to
    // refuse it (F13 §7). This is the population every identity lookup now plans for: a bare `query_row` on
    // such a key is **not a wrong answer but a scan error** (`ExecuteMultipleValues`), and `LIMIT 1` is a
    // silent pick that rewrites one note's title, tags, items and links with the other file's. So
    // [`np_core::db::note_ids`] reads every id and the scan acts on the count.
    //
    // F19 amended *how the pair gets here*, not what is pinned about reading it. It used to arrive through
    // migration 4's mechanical key copy, and the test then pinned that no migration made an unannounced
    // identity call. Migration 5 is a different act: it merges such a pair onto `MIN(id)`, re-points incoming
    // edges instead of nulling them, and writes its receipt in the same transaction — an announced repair,
    // not the silent pick F13 refused (`docs/design-f19-note-merge.md` §2 and §4; the F19 section below
    // this test pins that merge). Three things stay pinned, and (1) and (3) are unchanged in substance:
    // (1) a pair that exists when a scan runs neither fails the scan nor gets a silent winner — the file is
    //     named in `skipped` and **no note is rewritten**;
    // (2) migration 5 does not chase a pair that appears after it ran, and nothing else folds one either: both
    //     rows survive on one key. That reachability is deliberate — §7 keeps `notes(rel_bytes)` free of a
    //     UNIQUE index so this defensive arm stays a state the reader can be shown rather than dead code
    //     maintained by hand;
    // (3) after a write pass no note row carries a NULL `rel_bytes`, so none can hide from the re-keyed join.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).expect("test setup: workspace directory");
    fs::write(root.join("dup.md"), "# Dup\n").expect("test setup: write the duplicated note");
    // No row of its own: this is the file whose note [`np_core::db::insert_note`] mints during the scan.
    fs::write(root.join("fresh.md"), "# Fresh\n").expect("test setup: write the unregistered note");

    let cfg = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    let db_path = cfg.workspace_db(&root);
    // Migrated to completion *before* the pair exists, so what follows is a database another tool wrote into.
    let db = Db::open(&db_path).expect("test setup: migrate the database, then plant beside it");
    let dup = root.join("dup.md");
    let hash = sha256_file(&dup).expect("test setup: hash the note the way a scan does");
    let size = fs::metadata(&dup)
        .expect("test setup: read the note's size")
        .len() as i64;
    plant_duplicate_notes_after_migration(&db, "dup.md", &hash, size);
    drop(db);
    assert_eq!(
        rusqlite::Connection::open(&db_path)
            .expect("test: read the fixture's note rows")
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE rel = 'dup.md'",
                [],
                |row| { row.get::<_, i64>(0) }
            )
            .expect("test: the fixture records its notes"),
        2,
        "premise: the current schema really does accept two rows for one path"
    );

    let report = scan(&root, &cfg).expect("a duplicate pair must not fail the scan");
    // (1) The duplicate is named, by the spelling a person can match against their workspace, and the scan
    // carried on to `fresh.md`. Nothing about `dup.md` was counted: it was refused, not indexed.
    assert_eq!(
        report.skipped,
        vec!["dup.md".to_string()],
        "the file holding two note rows is the thing this scan could not index: {report:?}"
    );
    assert_eq!(
        (report.added, report.modified, report.moved, report.removed),
        (1, 0, 0, 0),
        "premise: `fresh.md` has to reach the new-note insert for this to measure that writer too: {report:?}"
    );

    let db = Db::open(&db_path).expect("test: reopen the migrated database");
    // ... and neither of the two rows was rewritten. Had one been picked, its title would now be `Dup` —
    // what this file's own text says — because the scan parses the file, not the row it refused to touch.
    assert_eq!(
        names(
            &db,
            "SELECT id || ':' || title || ':' || body FROM notes ORDER BY id"
        ),
        [
            "4:Dup first:first".to_string(),
            "9:Dup second:second".to_string(),
            "10:Fresh:# Fresh\n".to_string(),
        ],
        "no note was rewritten, and the untouched pair still reads as it was planted"
    );
    // (1) Both rows, both ids, one key — and `fresh.md`'s brand-new note keyed the same way.
    assert_eq!(
        names(
            &db,
            "SELECT rel || ':' || id || ':' || hex(rel_bytes) FROM notes ORDER BY rel, id"
        ),
        [
            "dup.md:4:6475702E6D64",
            "dup.md:9:6475702E6D64",
            "fresh.md:10:66726573682E6D64",
        ],
        "both rows survive onto the one byte key, and the note this scan minted carries a key as well"
    );
    assert_eq!(
        scalar::<String>(&db, "SELECT hex(rel_bytes) FROM files WHERE rel = 'dup.md'"),
        "6475702E6D64",
        "the two notes hang off one `files` row's key, which is what makes the key non-unique by design"
    );
    // (2) The new half of the contract. Migration 5 ran on this file — and recorded that it merged nothing,
    // because the pair did not exist yet. It does not run again, so a pair written afterwards is never folded
    // quietly: it is named by every scan until whoever wrote it is dealt with (F19 §3).
    assert_eq!(
        scalar::<String>(
            &db,
            &format!("SELECT value FROM meta WHERE key = '{NOTE_MERGE_KEY}'")
        ),
        "merged=0;paths=".to_string(),
        "the merge is recorded as done and empty while a pair still stands: no second pass, no silent winner"
    );

    // (3) §5's weaker, more important half: no keyless note after a write pass. A NULL key matches nothing
    // the re-keyed join, cascade and lookup compare on bytes.
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM notes WHERE rel_bytes IS NULL"),
        0,
        "a note row with no byte key is F13 reproduced by the fix"
    );
}

// ---------------------------------------------------------------------------
// F19 — migration 5 merges the duplicate note rows version 3 allowed. No production API makes a pair
// any more (`insert_note` mints one row per path and `note_ids` reads them all), so every fixture below
// builds one with raw SQL, on a schema that is otherwise current.
// ---------------------------------------------------------------------------

/// The byte key of `dup.md`, as the hex literal SQLite needs for a `BLOB` column (F13 §4: a TEXT bind
/// would fail the `typeof(rel_bytes) = 'blob'` check rather than quietly match).
const DUP_BYTES: &str = "X'6475702E6D64'";
/// The byte key of `third.md`, the note that links to the loser.
const THIRD_BYTES: &str = "X'74686972642E6D64'";
/// The byte keys of the two paths in [`seed_two_duplicate_groups_at_version_four`]. `a.md` sorts first as a
/// key while being planted second, which is what makes its position in the receipt a statement about ordering.
const A_MD_BYTES: &str = "X'612E6D64'";
/// The byte key of `b.md` — later as a key, earlier everywhere else.
const B_MD_BYTES: &str = "X'622E6D64'";

/// Plant F19 §5's shape and leave the file claiming version four, so the next open runs migration 5:
/// one `rel_bytes` holding two note rows (ids 4 and 9 — the winner is 4 by `MIN(id)`, and the ids are
/// non-consecutive because that is what a legacy file looks like), derived rows hanging off **both**, and
/// an incoming edge from a third note to each of them.
///
/// Planted on a current schema rather than copied from a version-three script: migration 5 writes no DDL,
/// only row moves, so "version four plus a pair" is exactly the file that step meets in the wild, and
/// deleting its recorded `schema_version` row reproduces that without a third hand-copied schema to keep
/// in step (this file already carries two of those, which is one too many to grow).
fn seed_duplicate_pair_at_version_four(path: &Path) {
    let db = Db::open(path).expect("test setup: create the current schema");
    db.0.execute_batch(
        &format!(
            "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
             VALUES ({DUP_BYTES}, {DUP_BYTES}, 'dup.md', 'dup.md', '/ws/dup.md', 'h1', 5, 1, 'ok', 9),
                    ({THIRD_BYTES}, {THIRD_BYTES}, 'third.md', 'third.md', '/ws/third.md', 'h2', 5, 1, 'ok', 9);
             INSERT INTO notes (id, rel_bytes, rel, title, title_source, encoding, word_count, body) VALUES
                (4,  {DUP_BYTES}, 'dup.md', 'Dup first',  'filename', 'utf-8', 1, 'first'),
                (9,  {DUP_BYTES}, 'dup.md', 'Dup second', 'filename', 'utf-8', 1, 'second'),
                (12, {THIRD_BYTES}, 'third.md', 'Third',  'filename', 'utf-8', 1, 'third');
             INSERT INTO headings (note_id, level, text, line) VALUES
                (4, 1, 'First heading', 1), (9, 2, 'Second heading', 3), (12, 1, 'Third heading', 1);
             INSERT INTO tags (note_id, tag) VALUES (4, 'first-tag'), (9, 'second-tag');
             INSERT INTO items (note_id, text, line, state, marker) VALUES
                (4, 'first task', 2, 'open', '- [ ]'), (9, 'second task', 4, 'done', '- [x]');
             INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin) VALUES
                (4, 12, 'wiki', 'Third', 3, 'prose'),
                (9, 12, 'wiki', 'Third', 5, 'prose'),
                (12, 4, 'wiki', 'Dup first', 2, 'prose'),
                (12, 9, 'wiki', 'Dup second', 3, 'prose');"
        ),
    )
    .expect("test setup: plant two note rows on one key, their derived rows and the edges around them");
    // Rewind past migration 5 — and past the receipt its step wrote while this file was still empty. A
    // genuine version-four file has no `note_merges` row at all, and leaving a `merged=0` one behind would
    // let that step take its upsert's update arm instead of recording itself, which is the difference
    // between "never checked" and "checked and found none" (F21 §2).
    db.0.execute_batch(&format!(
        "DELETE FROM meta WHERE key = '{NOTE_MERGE_KEY}';
         DELETE FROM schema_version WHERE version = 5;"
    ))
    .expect("test setup: rewind the file to version four");
    assert_eq!(
        scalar::<i64>(&db, "SELECT MAX(version) FROM schema_version"),
        4,
        "test setup: migration 5 has not run here"
    );
}

/// Plant the population real legacy workspaces actually arrive with — **version three**, holding two `notes`
/// rows for one `rel`, which was legal there because `notes.rel` carried no `UNIQUE` (F13 §7) — and leave the
/// file claiming version three so the next open runs migration 4 *and* migration 5 in one pass.
///
/// [`seed_duplicate_pair_at_version_four`] cannot stand in for this. It plants its pair on a schema migration 4
/// has already rebuilt, so nothing in it says that migration 4's `CAST(rel AS BLOB)` copy is what folds the two
/// rows onto one `rel_bytes`, nor that migration 5 then merges them in the same open. That order is
/// load-bearing rather than incidental: run the merge first and the pair is copied afterwards, into a file
/// already recorded as merged, with nothing left to heal it — which is how a reordered [`MIGRATIONS`] list
/// would lose somebody's note rows.
fn seed_duplicate_notes_at_version_three(path: &Path) {
    let conn = rusqlite::Connection::open(path).expect("test setup: open the old file");
    conn.execute_batch(VERSION_THREE)
        .expect("test setup: version-three schema");
    conn.execute_batch(
        "-- One `files` row per path (version three keyed those by `rel TEXT PRIMARY KEY`) and two note rows
         -- for `dup.md`, ids non-consecutive because that is what a legacy file looks like. Winner 4, loser 9.
         INSERT INTO files (rowid, rel, case_key, abs, hash, size, mtime, state, last_seen) VALUES
            (2, 'dup.md', 'dup.md', '/ws/dup.md', 'h1', 5, 1, 'ok', 9),
            (7, 'third.md', 'third.md', '/ws/third.md', 'h2', 5, 1, 'ok', 9);
         INSERT INTO notes (id, rel, title, title_source, encoding, word_count, body) VALUES
            (4, 'dup.md', 'Dup first',  'filename', 'utf-8', 1, 'first'),
            (9, 'dup.md', 'Dup second', 'filename', 'utf-8', 1, 'second'),
            (12, 'third.md', 'Third',   'filename', 'utf-8', 1, 'third');
         -- Derived rows on both halves of the pair, so 'the winner kept its corpus' is an assertion rather
         -- than a hope, and one edge into each of them from the third note.
         INSERT INTO headings (note_id, level, text, line) VALUES
            (4, 1, 'First heading', 1), (9, 2, 'Second heading', 3), (12, 1, 'Third heading', 1);
         INSERT INTO tags (note_id, tag) VALUES (4, 'first-tag'), (9, 'second-tag');
         INSERT INTO items (note_id, text, line, state, marker) VALUES
            (4, 'first task', 2, 'open', '- [ ]'), (9, 'second task', 4, 'done', '- [x]');
         INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin) VALUES
            (12, 4, 'wiki', 'Dup first', 2, 'prose'),
            (12, 9, 'wiki', 'Dup second', 3, 'prose');",
    )
    .expect("test setup: plant a version-three pair on one path");
    assert_eq!(
        conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| row
            .get::<_, i64>(0))
            .expect("test setup: read the fixture's version"),
        3,
        "test setup: migrations 4 and 5 both have to run on the open below"
    );
}

/// Plant a database holding **more than one** duplicate group and leave it claiming version four: `b.md` with
/// two note rows (ids 3 and 5) and `a.md` with **three** (ids 7, 9 and 11).
///
/// The shape is chosen so the receipt's two degrees of freedom cannot both be satisfied by an accident:
///
/// - **the count is rows folded, not paths** — three losers across two keys, so `merged` has to read 3 while
///   `paths` names 2. A group of three is the only way to tell those apart, and every other fixture here holds
///   exactly one pair, where both numbers would be 1;
/// - **the names are ordered by byte key** — `b.md` is planted first and carries the *lower* ids, so any receipt
///   built from arrival order or winner-id order reads `b.md,a.md`. Only `ORDER BY rel_bytes` answers `a.md,b.md`.
fn seed_two_duplicate_groups_at_version_four(path: &Path) {
    let db = Db::open(path).expect("test setup: create the current schema");
    db.0.execute_batch(
        &format!(
            "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
             VALUES ({B_MD_BYTES}, {B_MD_BYTES}, 'b.md', 'b.md', '/ws/b.md', 'h1', 5, 1, 'ok', 9),
                    ({A_MD_BYTES}, {A_MD_BYTES}, 'a.md', 'a.md', '/ws/a.md', 'h2', 5, 1, 'ok', 9);
             -- `b.md` first, and on the lower ids: see [`seed_two_duplicate_groups_at_version_four`]'s second
             -- bullet — the receipt must not be able to come out in this order.
             INSERT INTO notes (id, rel_bytes, rel, title, title_source, encoding, word_count, body) VALUES
                (3,  {B_MD_BYTES}, 'b.md', 'B first',  'filename', 'utf-8', 1, 'b1'),
                (5,  {B_MD_BYTES}, 'b.md', 'B second', 'filename', 'utf-8', 1, 'b2'),
                (7,  {A_MD_BYTES}, 'a.md', 'A first',  'filename', 'utf-8', 1, 'a1'),
                (9,  {A_MD_BYTES}, 'a.md', 'A second', 'filename', 'utf-8', 1, 'a2'),
                (11, {A_MD_BYTES}, 'a.md', 'A third',  'filename', 'utf-8', 1, 'a3');"
        ),
    )
    .expect("test setup: plant two duplicate groups, one of them a triple");
    db.0.execute_batch(&format!(
        "DELETE FROM meta WHERE key = '{NOTE_MERGE_KEY}';
         DELETE FROM schema_version WHERE version = 5;"
    ))
    .expect("test setup: rewind the file to version four");
}

/// The [`NOTE_MERGE_KEY`] receipt as the file holds it.
fn merge_receipt_of(db: &Db) -> String {
    scalar(
        db,
        &format!("SELECT value FROM meta WHERE key = '{NOTE_MERGE_KEY}'"),
    )
}

/// A workspace holding `dup.md` and `third.md` over a database [`seed_duplicate_pair_at_version_four`]
/// planted — the shape one `np scan` run therefore repairs as it opens.
fn merged_pair_workspace(tmp: &TempDir) -> (PathBuf, Config) {
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).expect("test setup: workspace directory");
    fs::write(root.join("dup.md"), "# Dup\n").expect("test setup: write dup.md");
    fs::write(root.join("third.md"), "# Third\n").expect("test setup: write third.md");
    let cfg = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };
    seed_duplicate_pair_at_version_four(&cfg.workspace_db(&root));
    (root, cfg)
}

#[test]
fn merging_a_duplicate_pair_keeps_the_oldest_id_and_moves_every_incoming_edge() {
    // F19 §4.2: row identity survives and link resolution survives, both asserted against the rows rather
    // than inferred from a report count.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_duplicate_pair_at_version_four(&path);

    let db = Db::open(&path).expect("the open runs migration 5");

    // One row remains, and it is the oldest: `MIN(id)`, the one derived rows already hang off.
    assert_eq!(
        names(
            &db,
            &format!(
                "SELECT CAST(id AS TEXT) FROM notes WHERE rel_bytes = {DUP_BYTES} ORDER BY id"
            )
        ),
        ["4".to_string()],
        "the survivor is MIN(id), not a silent pick or the newest row"
    );

    // The loser's derived rows are gone, and only the loser's. Asserted per shape because a merge that
    // deleted the *winner*'s children would otherwise be invisible here.
    for sql in [
        "SELECT COUNT(*) FROM headings WHERE note_id = 9",
        "SELECT COUNT(*) FROM tags WHERE note_id = 9",
        "SELECT COUNT(*) FROM items WHERE note_id = 9",
        "SELECT COUNT(*) FROM links WHERE src_note_id = 9",
    ] {
        assert_eq!(
            scalar::<i64>(&db, sql),
            0,
            "{sql}: the loser's own rows went with it"
        );
    }
    assert_eq!(
        names(&db, "SELECT text FROM headings WHERE note_id = 4"),
        ["First heading".to_string()],
        "the winner keeps the rows that were already hanging off it"
    );
    assert_eq!(
        names(&db, "SELECT tag FROM tags WHERE note_id = 4"),
        ["first-tag".to_string()],
        "and its tags, which is what a `MAX(id)` survivor would lose"
    );
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM items WHERE note_id = 4"),
        1
    );

    // The edge that mattered: another note's prose pointed at the loser. Its target still exists, under
    // the winner's id, so it is re-pointed — never nulled (F19 §2).
    assert_eq!(
        names(
            &db,
            "SELECT COALESCE(CAST(dst_note_id AS TEXT), 'NULL') FROM links
              WHERE src_note_id = 12 AND target = 'Dup second'"
        ),
        ["4".to_string()],
        "the incoming edge names the winner; a NULL here is a resolved link lost permanently"
    );
    assert_eq!(
        scalar::<i64>(
            &db,
            "SELECT COUNT(*) FROM links WHERE dst_note_id IS NOT NULL AND dst_note_id = 9"
        ),
        0,
        "no edge still claims a row that is gone"
    );
    // The third note's own prose is intact in both directions, and the winner's outgoing edge rode along
    // untouched: four edges went in, one (the loser's outgoing) came out.
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM links"),
        3,
        "only the loser's own outgoing edge disappeared"
    );

    // The receipt, read under the constant it is written under so a renamed key fails here.
    assert_eq!(merge_receipt_of(&db), "merged=1;paths=dup.md");
}

#[test]
fn the_receipt_counts_folded_rows_across_groups_and_names_paths_in_key_order() {
    // The receipt shape at more than its smallest point. Every other merge fixture holds one pair, where
    // `merged` and the name list are both a single 1 and cannot be told apart; here three rows fold out of two
    // keys, and the keys are planted in the opposite order to the one the receipt has to print.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_two_duplicate_groups_at_version_four(&path);

    let db = Db::open(&path).expect("the open runs migration 5 over both groups");

    assert_eq!(
        merge_receipt_of(&db),
        "merged=3;paths=a.md,b.md",
        "three rows folded, two paths named, names in byte-key order — not arrival order"
    );

    // The receipt's arithmetic checked against the rows it describes: one survivor per key, each the oldest on
    // that key, and nothing else left behind.
    assert_eq!(
        names(
            &db,
            &format!(
                "SELECT CAST(id AS TEXT) FROM notes WHERE rel_bytes = {A_MD_BYTES} ORDER BY id"
            )
        ),
        ["7".to_string()],
        "the triple left its MIN(id) standing"
    );
    assert_eq!(
        names(
            &db,
            &format!(
                "SELECT CAST(id AS TEXT) FROM notes WHERE rel_bytes = {B_MD_BYTES} ORDER BY id"
            )
        ),
        ["3".to_string()],
        "and the pair left its own"
    );
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM notes"),
        2,
        "five rows in, two out: `merged=3` is the difference, not a path count"
    );
}

#[test]
fn a_version_three_file_jumps_four_to_five_in_one_open_and_its_pair_merges() {
    // The jump real legacy workspaces make. Every other F19 fixture plants its pair on a finished schema and
    // rewinds one version row, so none of them says that migration 4's copy is what puts two rows on one
    // `rel_bytes` **and** migration 5 folds them, in the same open (see
    // [`seed_duplicate_notes_at_version_three`]). Reorder those two steps and this test is what fails: the pair
    // arrives after the merge has run, into a file already recorded as merged, and nothing heals it.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_duplicate_notes_at_version_three(&path);

    let db = Db::open(&path).expect("one open runs migration 4 and then migration 5");

    // Both steps landed: the file is current, and the pair sits on one byte key — which is migration 4's doing
    // (`CAST(rel AS BLOB)` per row), and the precondition migration 5 merges on.
    assert_eq!(
        scalar::<i64>(&db, "SELECT MAX(version) FROM schema_version"),
        5
    );
    assert_eq!(
        scalar::<String>(&db, "SELECT hex(rel_bytes) FROM files WHERE rel = 'dup.md'"),
        "6475702E6D64",
        "migration 4 keyed the path, and both note rows copied onto that one key"
    );

    // One row remains at the lower id: `MIN(id)`, the row migration 4 carried across untouched.
    assert_eq!(
        names(
            &db,
            &format!(
                "SELECT CAST(id AS TEXT) FROM notes WHERE rel_bytes = {DUP_BYTES} ORDER BY id"
            )
        ),
        ["4".to_string()],
        "the merge ran on the pair migration 4 had just copied, keeping the oldest row"
    );

    // The winner's derived rows survived both steps — its heading, tag and item are the ones the fixture wrote
    // — and the loser's are gone rather than orphaned into the rebuilt tables.
    assert_eq!(
        names(&db, "SELECT text FROM headings WHERE note_id = 4"),
        ["First heading".to_string()]
    );
    assert_eq!(
        names(&db, "SELECT tag FROM tags WHERE note_id = 4"),
        ["first-tag".to_string()]
    );
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM items WHERE note_id = 4"),
        1
    );
    for sql in [
        "SELECT COUNT(*) FROM headings WHERE note_id = 9",
        "SELECT COUNT(*) FROM tags WHERE note_id = 9",
        "SELECT COUNT(*) FROM items WHERE note_id = 9",
        "SELECT COUNT(*) FROM links WHERE src_note_id = 9",
    ] {
        assert_eq!(scalar::<i64>(&db, sql), 0, "{sql}: the loser left nothing");
    }

    // And the third note's edge into the loser came through both steps and names the winner — a NULL here is a
    // resolved link lost permanently (F19 §2), and one naming 9 is an id that no longer exists.
    assert_eq!(
        names(
            &db,
            "SELECT COALESCE(CAST(dst_note_id AS TEXT), 'NULL') FROM links
              WHERE src_note_id = 12 AND target = 'Dup second'"
        ),
        ["4".to_string()],
        "the edge crossed the rebuild and the merge together"
    );

    assert_eq!(merge_receipt_of(&db), "merged=1;paths=dup.md");
}

#[test]
fn the_scan_that_merges_reports_the_string_it_stored() {
    // The report field and `meta` are one value, not two recomputations of it (F21 §3's rule, applied to
    // F19's deletion): whatever a person is told, they can compare against the record.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let (root, cfg) = merged_pair_workspace(&tmp);

    let report = scan(&root, &cfg).expect("a legacy pair must not fail the scan");

    let stored = merge_receipt_of(&Db::open(&cfg.workspace_db(&root)).expect("reopen"));
    assert_eq!(stored, "merged=1;paths=dup.md");
    assert_eq!(
        report.merged_notes.as_deref(),
        Some(stored.as_str()),
        "{report:?}"
    );
    // And the pair is gone by the time the report is written, so nothing about `dup.md` was refused.
    assert!(report.skipped.is_empty(), "{report:?}");

    let db = Db::open(&cfg.workspace_db(&root)).expect("reopen");
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM notes WHERE rel = 'dup.md'"),
        1,
        "one row per path once the merge has run"
    );
}

#[test]
fn a_second_open_announces_the_merge_nowhere_and_keeps_the_receipt() {
    // F21's once-per-process rule, so this cannot become a permanent one-line warning: the announcement is
    // this open's memory, while `meta` stays the record — and clearing it would make the line load-bearing.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_duplicate_pair_at_version_four(&path);

    let first = Db::open(&path).expect("the first open merges");
    assert_eq!(first.merged_notes(), Some("merged=1;paths=dup.md"));
    drop(first);

    let second = Db::open(&path).expect("the second open runs nothing new");
    assert_eq!(
        second.merged_notes(),
        None,
        "a merge announced on every open is a warning nobody can act on"
    );
    assert_eq!(merge_receipt_of(&second), "merged=1;paths=dup.md");
}

#[test]
fn a_merge_that_cannot_record_its_receipt_leaves_both_note_rows() {
    // F19 §4.1, pinned the way F21 §4.2 pins migration 3: refuse the `meta` write and ask whether any row
    // went. Statement order would let a refactor move the receipt past the deletes' commit, or soften its
    // failure into a warning, and keep every other test green.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_duplicate_pair_at_version_four(&path);
    {
        let conn = rusqlite::Connection::open(&path).expect("test setup: reopen the fixture");
        // Nothing has claimed `NOTE_MERGE_KEY` yet, so this is an ordinary `INSERT` and the trigger fires
        // — while migration 5's selects, re-points and deletes all still run.
        conn.execute_batch(
            "CREATE TRIGGER refuse_merge_receipt BEFORE INSERT ON meta
             BEGIN SELECT RAISE(ABORT, 'no merge receipt here'); END;",
        )
        .expect("test setup: refuse writes to `meta`");
    }

    let err = Db::open(&path).expect_err("a merge that cannot record its receipt must not commit");
    // Reaching this line is half the assertion: an error, not a panic.
    assert!(
        matches!(
            &err,
            DbError::Sqlite(rusqlite::Error::SqliteFailure(fault, _))
                if fault.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER
        ),
        "expected the receipt write itself to be refused, got {err}"
    );
    assert!(
        err.to_string().contains("no merge receipt here"),
        "the error has to name the refusal: {err}"
    );

    // Read through a plain connection: `Db::open` still refuses this file.
    let conn = rusqlite::Connection::open(&path).expect("the refused file is still readable");
    let one = |sql: &str| -> i64 {
        conn.query_row(sql, [], |row| row.get(0))
            .expect("fixture query reads a number")
    };
    assert_eq!(
        one("SELECT COUNT(*) FROM notes WHERE hex(rel_bytes) = '6475702E6D64'"),
        2,
        "both note rows survived a merge whose receipt was refused"
    );
    assert_eq!(
        one("SELECT COUNT(*) FROM headings WHERE note_id = 9"),
        1,
        "the loser's derived rows are still there: nothing was deleted unannounced"
    );
    assert_eq!(
        one("SELECT dst_note_id FROM links WHERE src_note_id = 12 AND target = 'Dup second'"),
        9,
        "and the incoming edge still names the row it was written against"
    );
    assert_eq!(
        one(&format!(
            "SELECT COUNT(*) FROM meta WHERE key = '{NOTE_MERGE_KEY}'"
        )),
        0,
        "a refused receipt wrote nothing"
    );
    assert_eq!(
        one("SELECT MAX(version) FROM schema_version"),
        4,
        "the step recorded nothing, so a later run can retry the merge"
    );
}

#[test]
fn a_database_with_nothing_to_merge_records_that_the_check_ran() {
    // `merged=0` is the record that migration 5 looked (F21 §2's lesson), while the report field reports
    // work done and therefore stays `None` — so "this file was never checked" and "nothing was found" stay
    // tellable apart from either channel alone.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).expect("test setup: workspace directory");
    fs::write(root.join("a.md"), "# A\n").expect("test setup: write a note");
    let cfg = Config {
        data_dir: tmp.path().join("data"),
        ..Config::default()
    };

    let report = scan(&root, &cfg).expect("a clean workspace scans");

    let db = Db::open(&cfg.workspace_db(&root)).expect("reopen");
    assert_eq!(merge_receipt_of(&db), "merged=0;paths=");
    assert!(
        report.merged_notes.is_none(),
        "`merged=0` states no work done: {report:?}"
    );
}

#[test]
fn the_merge_gate_refuses_a_child_row_whose_note_is_gone() {
    // Migration 5's `PRAGMA foreign_key_check`, seen from the only side that can reach it. Its deletes are
    // explicit rather than cascading — enforcement is off inside a code step (see `merge_duplicate_notes`) —
    // so this pragma is what turns "one of those deletes was softened" into a refused open instead of a
    // committed orphan idling until the next `INSERT` re-adopts its id. No later scan re-checks those keys.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = tmp.path().join("ws.sqlite");
    seed_duplicate_pair_at_version_four(&path);
    {
        let conn = rusqlite::Connection::open(&path).expect("test setup: reopen the fixture");
        // Enforcement off to plant this at all, exactly as in
        // `the_byte_key_gate_refuses_a_note_whose_file_row_is_gone`: only a writer outside this program leaves
        // a child row whose note is gone. The rowid is named so the error below can be pinned to *this* row.
        conn.execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("test setup: relax keys for the fixture");
        conn.execute(
            "INSERT INTO headings (rowid, note_id, level, text, line)
             VALUES (777, 999, 1, 'Dangling', 2)",
            [],
        )
        .expect("test setup: a heading row whose note never existed");
        assert_eq!(
            conn.query_row("SELECT MAX(version) FROM schema_version", [], |row| row
                .get::<_, i64>(0))
                .expect("fixture query reads a number"),
            4,
            "test setup: only migration 5 can run on the open below, so the refusal cannot belong to an earlier step"
        );
    }

    let err = Db::open(&path).expect_err("a child row outside its parent has to refuse the merge");
    assert!(
        matches!(
            &err,
            DbError::ForeignKeys { count, table, parent, rowid }
                if *count == 1
                    && table == "headings"
                    && parent == "notes"
                    && *rowid == 777
        ),
        "expected the check to name the dangling heading (rowid 777) and the parent it misses, got {err}"
    );

    // And refusing means refusing: the deletes roll back with the refusal, so the file keeps its pair and
    // records nothing — §4.1's "nothing is merged unannounced" from this side of the transaction.
    let conn = rusqlite::Connection::open(&path).expect("the refused file is still readable");
    let one = |sql: &str| -> i64 {
        conn.query_row(sql, [], |row| row.get(0))
            .expect("fixture query reads a number")
    };
    assert_eq!(
        one("SELECT COUNT(*) FROM notes WHERE hex(rel_bytes) = '6475702E6D64'"),
        2,
        "both note rows survived a step that refused its own work"
    );
    assert_eq!(
        one(&format!(
            "SELECT COUNT(*) FROM meta WHERE key = '{NOTE_MERGE_KEY}'"
        )),
        0,
        "a refused step wrote no receipt"
    );
    assert_eq!(
        one("SELECT MAX(version) FROM schema_version"),
        4,
        "the step recorded nothing, so a later run can retry the merge"
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
    mark_stale(&db, from.as_bytes()).expect("mark the file stale first");
    assert_eq!(
        scalar::<String>(
            &db,
            &format!("SELECT state FROM files WHERE rel = '{from}'")
        ),
        "stale"
    );

    apply_move(
        &db,
        &path(from),
        &path(to),
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
    // The two byte keys moved with the display pair (F13 §5: `case_bytes = case_fold(rel_bytes)`, both
    // written by the same statement). They are asserted on this row precisely because its name carries a
    // non-ASCII capital: a fold that reached for `str::to_lowercase` would rewrite `Ü` to `ü` here while
    // leaving every assertion above — including `case_key`, which *is* the Unicode fold — untouched.
    assert_eq!(
        scalar::<Vec<u8>>(
            &db,
            &format!("SELECT rel_bytes FROM files WHERE rel = '{to}'")
        ),
        to.as_bytes().to_vec(),
        "rel_bytes is the bytes of the new rel"
    );
    assert_eq!(
        scalar::<Vec<u8>>(
            &db,
            &format!("SELECT case_bytes FROM files WHERE rel = '{to}'")
        ),
        // The ASCII `N` folds; the `Ü` (\xc3\x9c) keeps its bytes. That is the whole difference between this
        // column and the `case_key` above, spelled out rather than recomputed.
        b"n\xc3\xb6tes/\xc3\x9cbersetzt.md".to_vec(),
        "case_bytes is the ASCII-only fold of those bytes"
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
    let missing =
        apply_move(&db, &path("gone.md"), &path("here.md"), 1).expect_err("unknown rel is a bug");
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
    let err =
        apply_move(&db, &path("a.md"), &path("b.md"), 2).expect_err("an unrebasable abs is a bug");
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

    mark_stale(&db, b"a.md").expect("mark stale");
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
    mark_stale(&db, b"b.md").expect("mark stale");
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
///
/// Both byte keys are spelled as `x'…'` literals because migration 4 made them `NOT NULL`, and this statement
/// runs on a raw connection where no Rust writer is in front of it. The hex is the ASCII spelling of `rel`,
/// which is all these fixtures name.
fn insert_file(rel: &str) -> String {
    let bytes = rel
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
         VALUES (x'{bytes}', x'{bytes}', '{rel}', '{rel}', '/ws/{rel}', 'hash', 1, 1, 'ok', 1)"
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
    // A diagnostic, not a discriminator: `contended` sleeps the whole `HOLD_MS` before handing the lock
    // back and `waited` starts before that sleep (`common::contended`), so any write that got past the
    // lock is over half the hold by construction. Nothing here goes red because `busy_timeout` went
    // missing — that is what `a_connection_with_no_busy_timeout_is_refused_the_same_write_at_once` and
    // the bounds in `the_busy_timeout_in_force_is_this_crates_and_not_the_drivers_default` are for. The
    // figure stays printed because one near zero would mean the harness stopped measuring at all.
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

    // `notes.rel_bytes REFERENCES files(rel_bytes)` is what makes an orphan note impossible now that migration 4
    // moved the key off display text; it is also the key `apply_move`'s cascade rides. Like the
    // `PRAGMA foreign_keys` readings in
    // `wal_and_foreign_keys_are_set_where_they_are_legal`, this fires on the bundled build's
    // default too, so it does not prove the pragma — only that the constraint is declared.
    let err = insert_note_err(&db, "never-seen.md");
    // The extended code, as [`refuses_orphan`] does it: `ConstraintViolation` alone is also what the
    // `typeof(rel_bytes) = 'blob'` CHECK answers with, so a bind that drifted back to TEXT would keep this
    // test green while it measured the CHECK instead of the key.
    assert!(
        matches!(
            &err,
            rusqlite::Error::SqliteFailure(fault, _)
                if fault.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY
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
    // The byte key is supplied so this still measures the **foreign key**: without it the insert would die on
    // `typeof(rel_bytes) = 'blob'` and the test would pass for the wrong reason.
    db.0.execute(
        "INSERT INTO notes (rel_bytes, rel, title, title_source, encoding, word_count, body)
         VALUES (?1, ?2, 'x', 'filename', 'utf-8', 1, 'body')",
        rusqlite::params![rel.as_bytes(), rel],
    )
    .expect_err("an orphan note must fail")
}
