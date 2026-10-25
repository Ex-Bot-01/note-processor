//! One SQLite file per workspace: the registry of paths, plus the rows derived from them.
//!
//! Two rules decide the shape of everything here. A file that is gone is **marked stale and
//! never destroyed** (spec §2): its row keeps `state = 'stale'` with every child row intact,
//! so the note can come back later and rejoin the live set as the same note. And a move
//! changes a path, not a note — [`apply_move`] writes one row of `files` and lets
//! `notes.rel ON UPDATE CASCADE` follow it, while `headings`, `tags`, `links` and `items`
//! stay put because they hang off `notes.id`, which never changes. That is how a rename
//! keeps a note's links.
//!
//! That hanging-off is stated in the schema as well as in the code: migration 3 gave every child
//! column a foreign key on `notes(id)`, so a row no note owns cannot be written at all. Each entry of
//! [`MIGRATIONS`] says what a deleted note does to the rows around it.
//!
//! There is no transaction helper on purpose: Task 10 wraps a whole scan in one
//! [`rusqlite::Connection::unchecked_transaction`], which needs the public connection.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use thiserror::Error;

use crate::hash::Snapshot;

/// How long a connection waits for another connection's write lock before reporting `SQLITE_BUSY`, in
/// milliseconds. Set on every connection this crate opens — workspace database and registry alike.
///
/// WAL is chosen because two of them overlap (see [`Db::open`]), and WAL admits one writer at a time, so
/// the second one needs an answer that is not `SQLITE_BUSY`. SQLite's own default is no wait at all;
/// rusqlite raises that to 5000 ms for every connection it opens
/// (`rusqlite-0.40.2/src/inner_connection.rs:118`). Naming the value here keeps the wait this program's
/// own rather than the driver's.
///
/// What it has to cover is an overlapping scan's **whole write transaction**, not one statement: a scan
/// opens a single transaction and runs `apply_changes`, `store_note_links`, the catalog reads,
/// `catalog::render` and `set_index_list` inside it before committing (see `scan::scan`), so content
/// hashing is the only substantial cost that sits outside. A second writer arriving as that phase starts
/// has to outlast all of it: a value in the low hundreds of milliseconds gave up part-way through the
/// overlap WAL exists to tolerate, which is a worse answer than waiting.
///
/// 1800 ms is a budget for that phase rather than a measurement of one. It is bounded by the two numbers
/// around it: below rusqlite's 5000 ms, so a command waiting behind a genuinely wedged transaction still
/// reports in about two seconds instead of five, and far above the single-statement contention an ordinary
/// command meets. What it has *not* been checked against is a write phase over a corpus of several
/// thousand notes — no fixture that size exists locally. The two contention tests
/// (`crates/np-core/tests/db.rs`, `crates/np-core/tests/config.rs`) pin the value between those bounds;
/// they do not prove it is enough at scale. If a scan's write phase ever runs past it, that scan reports
/// `database is locked` under overlap and this constant is what moves.
pub(crate) const BUSY_TIMEOUT_MS: u32 = 1800;

/// The schema, oldest first. Each entry is applied once, inside its own transaction, and its
/// 1-based position is recorded in `schema_version`. A database whose recorded version is
/// ahead of this list is refused rather than trusted — a newer file read by an older binary
/// would otherwise be written to a schema this code does not know.
///
/// Three entries rather than one, split along the lines that matter (`files` + `notes` are the
/// registry and the note's identity; everything after them is derived from a note's contents
/// and can be rebuilt from the file; the third adds the keys between those two groups), so the
/// version bookkeeping has more than one row to prove itself with. Later milestones append an
/// entry here; they do not edit one — migration 3 exists precisely because a database on disk has
/// already applied the first two, and SQLite cannot add a constraint to a table that is there.
pub const MIGRATIONS: &[Migration] = &[
    // The registry, and one note row per markdown file. `rel` is the identity a scan keys on
    // and `case_key` the one a case-insensitive filesystem keys on, so both are constrained.
    Migration::Sql(
        r#"
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
    -- Mandated by the plan even though the UNIQUE constraint above already indexes this
    -- column; keeping both spellings means a later migration that changes one constraint
    -- still cannot slow down the case-folded lookup.
    CREATE INDEX files_case_key ON files(case_key);

    CREATE TABLE notes (
        id           INTEGER PRIMARY KEY,
        -- The cascade is what lets `apply_move` rewrite a single row and take the note with
        -- it; without it the update would fail while the child row still points at the old
        -- path.
        rel          TEXT REFERENCES files(rel) ON UPDATE CASCADE,
        title        TEXT,
        title_source TEXT,
        encoding     TEXT,
        word_count   INTEGER,
        body         TEXT
    );
    CREATE INDEX notes_rel ON notes(rel);
    "#,
    ),
    // Derived rows. Every one of them keys on `notes.id`, which is why a move never has to
    // touch them and why identity survives a rename — and why they are the tables migration 3 puts
    // a foreign key on.
    Migration::Sql(
        r#"
    CREATE TABLE headings (
        note_id INTEGER NOT NULL,
        level   INTEGER NOT NULL,
        text    TEXT    NOT NULL,
        line    INTEGER NOT NULL
    );

    CREATE TABLE tags (
        note_id INTEGER NOT NULL,
        tag     TEXT    NOT NULL
    );
    CREATE INDEX tags_tag ON tags(tag);

    CREATE TABLE links (
        src_note_id INTEGER NOT NULL,
        -- NULL is a link whose target is not in the registry yet, or never will be; it is
        -- counted as unresolved rather than dropped.
        dst_note_id INTEGER,
        kind        TEXT    NOT NULL CHECK (kind IN ('wiki','md','attachment')),
        target      TEXT    NOT NULL,
        line        INTEGER NOT NULL,
        origin      TEXT    NOT NULL CHECK (origin IN ('prose','approved'))
    );
    CREATE INDEX links_dst_note_id ON links(dst_note_id);

    CREATE TABLE items (
        note_id INTEGER NOT NULL,
        text    TEXT    NOT NULL,
        line    INTEGER NOT NULL,
        state   TEXT    NOT NULL CHECK (state IN ('open','done','paused')),
        -- The marker that decided `state`, spelled as the config wrote it; NULL when a
        -- keyword in the prose decided it rather than a list marker.
        marker  TEXT
    );

    CREATE TABLE meta (
        key   TEXT PRIMARY KEY,
        value TEXT
    );
    "#,
    ),
    // The child tables' note keys become real foreign keys. Not a script: SQLite has no `ALTER TABLE
    // ADD CONSTRAINT`, so each table is rebuilt, and the orphans an older build left behind have to be
    // counted and reported on the way through.
    Migration::Code(add_note_foreign_keys),
];

/// One step of the schema, as one entry of [`MIGRATIONS`].
#[derive(Clone, Copy)]
pub enum Migration {
    /// A SQL script, applied verbatim inside the step's own transaction.
    Sql(&'static str),
    /// A step that cannot be written as a script — a table rebuild, or a cleanup whose counts have to
    /// be reported — run inside the same kind of transaction. Returning an error rolls the whole step
    /// back and leaves the recorded version where it was.
    ///
    /// The connection is handed over with `foreign_keys` switched off, because a rebuild needs it that
    /// way; every migration step runs under that switch and enforcement is back on afterwards.
    Code(fn(&Connection) -> Result<(), DbError>),
}

/// What can fail while opening or writing the workspace database.
#[derive(Debug, Error)]
pub enum DbError {
    /// The database's directory could not be created. Like [`crate::hash::HashError::Read`],
    /// the message names the path it failed on: a report that cannot say which database broke
    /// is no use to whoever has to fix it. A database file that opens but rejects a statement
    /// arrives through [`DbError::Sqlite`] instead — SQLite's own text already quotes the file
    /// name (`unable to open database file: <path>`), so nothing re-wraps it.
    #[error("cannot create the database at {}: {source}", .path.display())]
    Io {
        /// The database path whose directory could not be made.
        path: PathBuf,
        /// Why it failed.
        source: io::Error,
    },
    /// SQLite rejected a statement. Constraint violations arrive unchanged, because Task 10
    /// reports the offending path under `skipped` and carries on with the next one.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A migration's `PRAGMA foreign_key_check` reported rows, so the file holds child rows no parent
    /// owns and its data cannot satisfy the constraints being added. The migration rolls back instead
    /// of recording itself: carrying on to write into a database whose own keys are broken would hide
    /// the damage rather than report it.
    #[error("foreign key check found {count} row(s) outside their parent's key, first offender: table {table} rowid {rowid} references {parent}")]
    ForeignKeys {
        /// How many rows the check reported.
        count: i64,
        /// The first row it reported.
        table: String,
        /// Rowid of that row, so whoever has to repair the data can find it.
        rowid: i64,
        /// The parent table its key names.
        parent: String,
    },
    /// The file was written by a newer version of this program, so its schema is unknown.
    #[error("database schema version {found} is newer than this build knows ({known})")]
    TooNew {
        /// The highest version the file recorded.
        found: i64,
        /// How many migrations this binary has.
        known: i64,
    },
    /// [`apply_move`] was asked to move a path the registry has no row for.
    #[error("no files row for {rel}")]
    MissingRel {
        /// The `rel` that should have been there.
        rel: String,
    },
    /// The stored `abs` does not end in its own `rel`, so the new absolute path cannot be
    /// derived. Inventing one would send every later read of that note to the wrong place.
    #[error("abs of {rel} is `{abs}`, which does not end with that rel")]
    BadAbs {
        /// The row whose `abs` is unusable.
        rel: String,
        /// The value stored in it.
        abs: String,
    },
}

/// An open workspace database.
///
/// The connection is public because Task 10 runs a whole scan inside one transaction, which
/// needs the [`Connection`] itself rather than a helper this module would have to grow.
#[derive(Debug)]
pub struct Db(pub Connection);

impl Db {
    /// Open `path`, creating its directories and applying any pending migration.
    ///
    /// Three pragmas are set before anything else, `busy_timeout` first so it also covers the
    /// migrations that run below: `journal_mode=WAL`, because a scan and an `np` command can overlap,
    /// `foreign_keys=ON`, because SQLite leaves constraints off by default per connection — without it a
    /// move would leave its notes behind instead of cascading, and orphan notes would be accepted — and
    /// `BUSY_TIMEOUT_MS` so the two of them that *do* overlap wait for each other instead of failing.
    /// (A code span, not a link: the constant is `pub(crate)` and rustdoc warns when public docs link
    /// to it.)
    pub fn open(path: &Path) -> Result<Db, DbError> {
        if let Some(parent) = path.parent() {
            // `Path::parent` of a bare file name is the empty path, which is not a directory
            // to create.
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|source| DbError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
        }
        let conn = Connection::open(path)?;
        // WAL is a file-database privilege: `:memory:` refuses it, so it is asked for here
        // and not in [`Db::in_memory`].
        conn.execute_batch(&format!(
            "PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};
             PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;"
        ))?;
        migrate(&conn)?;
        Ok(Db(conn))
    }

    /// An empty database in memory, migrated the same way a file one is. For tests.
    pub fn in_memory() -> Result<Db, DbError> {
        let conn = Connection::open_in_memory()?;
        // The timeout is meaningless for a private `:memory:` database, which no second process can
        // lock, and is set anyway so that "every connection sets it" stays true of this crate.
        conn.execute_batch(&format!(
            "PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}; PRAGMA foreign_keys = ON;"
        ))?;
        migrate(&conn)?;
        Ok(Db(conn))
    }
}

/// Apply every migration whose version is above what the file recorded.
fn migrate(conn: &Connection) -> Result<(), DbError> {
    // The bookkeeping table is not itself a migration entry: it has to exist before the first
    // entry can be recorded in it.
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);")?;
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;
    if current > MIGRATIONS.len() as i64 {
        return Err(DbError::TooNew {
            found: current,
            known: MIGRATIONS.len() as i64,
        });
    }

    // Foreign-key enforcement is switched off for the whole loop and back on after it, whatever the
    // loop returns. A table rebuild cannot happen with it on — dropping a table fails while another one
    // references it, and `ALTER TABLE ... RENAME` rewrites the references it finds — and the pragma has
    // no effect inside a transaction (sqlite.org/lang_altertable.html, step 7 of the documented
    // procedure), so it cannot move into the one migration that needs it. Both open paths ask for
    // `foreign_keys = ON` before calling this, so restoring it keeps that promise to the caller even
    // when a migration fails.
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    let applied = apply_migrations(conn, current);
    let restored = conn.execute_batch("PRAGMA foreign_keys = ON");
    applied?;
    restored?;
    Ok(())
}

/// Apply every migration above `from`, one transaction each.
fn apply_migrations(conn: &Connection, from: i64) -> Result<(), DbError> {
    for (index, migration) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version <= from {
            continue;
        }
        // One entry per transaction: DDL is transactional in SQLite, so a migration that
        // fails halfway leaves nothing behind and cannot be recorded as done. A code step gets the
        // same transaction, which is what lets it check its own work before anything is committed.
        let tx = conn.unchecked_transaction()?;
        match migration {
            Migration::Sql(sql) => tx.execute_batch(sql)?,
            Migration::Code(apply) => apply(&tx)?,
        }
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![version],
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// `meta` key recording what migration 3 removed to make its constraints possible.
///
/// `meta` is where this program already keeps the workspace bookkeeping a person may need in order to
/// explain itself — [`mod@crate::scan`] records the published index files there — so the receipt needs no
/// new channel: there is no logger to write to, and returning the counts from `Db::open` would mean
/// plumbing a migration detail through every caller that opens a database. The value is one labelled
/// count per shape, `headings=…,tags=…,items=…,links_src=…,links_dst_null=…`, written even when they
/// are all zero: "looked and found none" is worth telling apart from "this file never upgraded".
pub const FK_ORPHANS_KEY: &str = "fk_orphans";

/// Migration 3 — every child table gains a real foreign key on its note.
///
/// `headings`, `tags`, `links` and `items` carried their note's id as a bare integer. The invariant
/// "one note row per file, and its children belong to that note" was held by hand in
/// `scan::delete_note_row`, and one forgotten delete there left an orphan child whose dead id SQLite
/// hands to the next `INSERT` — a silent re-adoption. The shapes below follow what that code already
/// does, statement for statement:
///
/// - `headings.note_id`, `tags.note_id`, `items.note_id` → `REFERENCES notes(id) ON DELETE CASCADE`:
///   a heading with no note is nothing, and `delete_note_row` deletes them today.
/// - `links.src_note_id` → the same. An edge from a note that is gone is not an edge.
/// - `links.dst_note_id` → `ON DELETE SET NULL`, **not** CASCADE: dropping a target must not delete
///   the *source's* row, which is another note's prose. Keeping it with a NULL destination is what
///   `delete_note_row` does by hand and what keeps the edge inside `unresolved_links`.
///
/// The one place the schema is stricter than the hand-written cleanup is approved edges. That function
/// drops only this note's `origin = 'prose'` rows and keeps an approved one (spec §10), but a foreign
/// key cannot be conditional: either an approved edge goes with its source or the source row cannot be
/// deleted at all. It goes. M1 writes no approved edges — nothing calls `store_links` with that origin
/// outside tests — so nothing observable changes today, and `delete_note_row` says so where a reader
/// of that code will look.
///
/// SQLite has no `ALTER TABLE ADD CONSTRAINT`, so each table is rebuilt the documented way
/// (sqlite.org/lang_altertable.html): stage `<name>_new` with the constraints in its definition, copy
/// the rows across, drop the old table, rename the staged one back, recreate its indexes. Enforcement
/// is off for the duration — [`migrate`] switches it outside the transaction, the only place it has an
/// effect — and `PRAGMA foreign_key_check` runs **inside** that transaction, so a report rolls the
/// whole migration back instead of committing a file that cannot satisfy its own schema.
///
/// One consequence of running that check over the whole database: a file holding an orphan **note** — a
/// `notes.rel` no `files` row answers, which migration 1's key already forbids — refuses to open at all.
/// That row is not this step's to delete (its file may come back, which is what `state = 'stale'` exists
/// for) and not its to invent a parent for, so refusing the upgrade is the only honest answer left.
/// Nothing in M1 can produce one: a note row is written after its file is upserted, and `files` rows are
/// never deleted.
fn add_note_foreign_keys(conn: &Connection) -> Result<(), DbError> {
    // Orphans first: a database from the manual-cleanup era may hold children no note owns, and they
    // would fail the check below. Nothing with a real note is touched.
    let removed = OrphanCleanup {
        headings: detach_orphans(conn, "headings", "note_id")?,
        tags: detach_orphans(conn, "tags", "note_id")?,
        items: detach_orphans(conn, "items", "note_id")?,
        links_src: detach_orphans(conn, "links", "src_note_id")?,
        links_dst_null: unlink_destinations(conn)?,
    };

    for table in CHILD_TABLES {
        rebuild_with_keys(conn, table)?;
    }

    if let Some((table, rowid, parent)) = first_violation(conn)? {
        // The counts were computed and the tables rebuilt in this transaction; returning here discards
        // all of it, including the orphan removals, so the file is exactly as it was opened.
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_foreign_key_check()",
            [],
            |row| row.get(0),
        )?;
        return Err(DbError::ForeignKeys {
            count,
            table,
            rowid,
            parent,
        });
    }

    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![FK_ORPHANS_KEY, removed.value()],
    )?;
    if removed.total() > 0 {
        // Saying it out loud as well: reading the receipt back needs sqlite3, and a person who just
        // watched an ordinary command delete rows deserves to be told they were deleted. The position
        // is deliberate — after the violation check above, so nothing is announced about a migration
        // that rolled itself back. All zeros stays silent: every pre-F5 workspace passes through here,
        // and "found nothing" on an ordinary upgrade is not news.
        eprintln!(
            "note-processor: removed {} orphan row(s) while adding note foreign keys ({})",
            removed.total(),
            removed.value(),
        );
    }
    Ok(())
}

/// What [`add_note_foreign_keys`] had to remove, kept apart by shape because a person reading the
/// receipt wants to know which table an older build was leaving rows in.
struct OrphanCleanup {
    headings: usize,
    tags: usize,
    items: usize,
    /// Rows deleted: their source note no longer exists, so there is no sentence left to keep.
    links_src: usize,
    /// Rows NULLed rather than deleted: the edge stays, its destination goes.
    links_dst_null: usize,
}

impl OrphanCleanup {
    /// Every row the cleanup changed, added up. What decides whether the receipt is worth printing.
    fn total(&self) -> usize {
        self.headings + self.tags + self.items + self.links_src + self.links_dst_null
    }

    /// The stored spelling — the labels of [`FK_ORPHANS_KEY`], in the order the tables are rebuilt.
    fn value(&self) -> String {
        format!(
            "headings={},tags={},items={},links_src={},links_dst_null={}",
            self.headings, self.tags, self.items, self.links_src, self.links_dst_null
        )
    }
}

/// Delete the rows of `table` whose `column` names no note, returning how many went.
///
/// Both names come from the call sites in this module and never from a caller or a file, so writing the
/// statement out with `format!` is safe. `NOT IN` is exact here because `notes.id` is an `INTEGER
/// PRIMARY KEY` and can never be NULL; against a nullable column the same predicate would match nothing
/// at all.
fn detach_orphans(conn: &Connection, table: &str, column: &str) -> Result<usize, DbError> {
    Ok(conn.execute(
        &format!("DELETE FROM {table} WHERE {column} NOT IN (SELECT id FROM notes)"),
        [],
    )?)
}

/// NULL the destinations that name no note, returning how many changed.
///
/// Not a delete: an edge whose target disappeared is still a sentence somebody wrote, and
/// [`note_counts`] counts a NULL destination as unresolved. Leaving them in place would fail the check
/// at the end of [`add_note_foreign_keys`], which is why they are repaired rather than kept.
fn unlink_destinations(conn: &Connection) -> Result<usize, DbError> {
    Ok(conn.execute(
        "UPDATE links SET dst_note_id = NULL
          WHERE dst_note_id IS NOT NULL AND dst_note_id NOT IN (SELECT id FROM notes)",
        [],
    )?)
}

/// One child table that [`add_note_foreign_keys`] rebuilds.
struct ChildTable {
    /// Its name, which is also the name the staged `<name>_new` table is renamed back to.
    name: &'static str,
    /// Every column in declaration order — copied out of migration 2 verbatim, with the new
    /// `REFERENCES` clause added to the one that names a note. Copying them instead of generating them
    /// means a rebuild cannot quietly drop a type, a NOT NULL or one of the CHECKs.
    columns: &'static str,
    /// The same columns as a comma-separated list for the copy. Naming them rather than using
    /// `SELECT *` means a column added to one side later cannot transpose values into the other.
    copied: &'static str,
    /// The table's `CREATE INDEX` statements, recreated after the rename because dropping a table
    /// drops its indexes with it. Empty when the table has none.
    indexes: &'static str,
}

const CHILD_TABLES: &[ChildTable] = &[
    ChildTable {
        name: "headings",
        columns: "note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
                  level INTEGER NOT NULL,
                  text  TEXT NOT NULL,
                  line  INTEGER NOT NULL",
        copied: "note_id, level, text, line",
        indexes: "",
    },
    ChildTable {
        name: "tags",
        columns: "note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
                  tag TEXT NOT NULL",
        copied: "note_id, tag",
        indexes: "CREATE INDEX tags_tag ON tags(tag);",
    },
    ChildTable {
        name: "links",
        // Two keys, two actions: the source's edge is its own prose and dies with it, the destination
        // is a reference to somebody else's note and merely becomes unresolved.
        columns: "src_note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
                  dst_note_id INTEGER REFERENCES notes(id) ON DELETE SET NULL,
                  kind   TEXT NOT NULL CHECK (kind IN ('wiki','md','attachment')),
                  target TEXT NOT NULL,
                  line   INTEGER NOT NULL,
                  origin TEXT NOT NULL CHECK (origin IN ('prose','approved'))",
        copied: "src_note_id, dst_note_id, kind, target, line, origin",
        indexes: "CREATE INDEX links_dst_note_id ON links(dst_note_id);",
    },
    ChildTable {
        name: "items",
        columns: "note_id INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
                  text  TEXT NOT NULL,
                  line  INTEGER NOT NULL,
                  state TEXT NOT NULL CHECK (state IN ('open','done','paused')),
                  marker TEXT",
        copied: "note_id, text, line, state, marker",
        indexes: "",
    },
];

/// Rebuild one child table so its own definition carries the constraints.
///
/// Runs inside the caller's transaction with enforcement off, so neither the `DROP` nor the `RENAME`
/// can disturb another table's references, and a failure at any step leaves the original table where it
/// was.
///
/// The copy is ordered by `rowid`, and that order is load-bearing rather than stylistic. No code
/// addresses a child row *by* its rowid — rows are found through their note — but the rowids SQLite
/// hands out on the way in decide what [`crate::scan::note_tags`] returns, because it reads tags
/// `ORDER BY note_id, rowid` and [`crate::catalog`] renders them in that first-seen order. An unordered
/// copy leaves the choice to the planner: today a table scan happens to answer in rowid order, but a
/// plan that reads the covering `tags_tag` index — likelier once F9/F10 add indexes or an `ANALYZE`
/// writes statistics — would reassign the rowids in tag order and the next scan would rewrite every
/// tagged entry line of `INDEX.md` with no real change to report, which is the byte-stability
/// [`crate::catalog`] promises at its top.
fn rebuild_with_keys(conn: &Connection, table: &ChildTable) -> Result<(), DbError> {
    let staged = format!("{}_new", table.name);
    conn.execute_batch(&format!(
        "CREATE TABLE {staged} ({columns});
         INSERT INTO {staged} ({copied}) SELECT {copied} FROM {name} ORDER BY rowid;
         DROP TABLE {name};
         ALTER TABLE {staged} RENAME TO {name};
         {indexes}",
        columns = table.columns,
        copied = table.copied,
        name = table.name,
        indexes = table.indexes,
    ))?;
    Ok(())
}

/// The first row `PRAGMA foreign_key_check` reports, if it reports any: its table, rowid and the
/// parent its key names.
///
/// The check reads the constraints as declared and ignores the `foreign_keys` pragma, which is why it
/// can run inside the rebuild's own transaction — [`crate::links`] and `scan::delete_note_row` both
/// rely on enforcement being on for ordinary writes, but this is the one place that has to ask the
/// question while the pragma is off.
fn first_violation(conn: &Connection) -> Result<Option<(String, i64, String)>, DbError> {
    let mut statement = conn.prepare("SELECT * FROM pragma_foreign_key_check()")?;
    let mut rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

/// Record a file that is there now — or bring a known row back to `state = 'ok'`.
///
/// The row is keyed on `rel`, so a path re-upserted after [`mark_stale`] rejoins the live set
/// on the row it already has: its `rowid` never changes, which is what keeps the note's
/// derived rows attached to it. `hash`, `size`, `mtime`, `abs` and `last_seen` are refreshed
/// from the snapshot every time, because a scan that saw a file is a sighting of it.
///
/// A `case_key` collision — two paths differing only in case on a case-sensitive filesystem —
/// surfaces as the constraint violation it is. Nothing picks a winner: Task 10 reports such a
/// path under `skipped` and keeps scanning.
pub fn upsert_file(db: &Db, s: &Snapshot, abs: &Path, now: i64) -> Result<(), DbError> {
    db.0.execute(
        "INSERT INTO files (rel, case_key, abs, hash, size, mtime, state, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'ok', ?7)
         ON CONFLICT (rel) DO UPDATE SET
             case_key  = excluded.case_key,
             abs       = excluded.abs,
             hash      = excluded.hash,
             size      = excluded.size,
             mtime     = excluded.mtime,
             state     = 'ok',
             last_seen = excluded.last_seen",
        params![
            s.rel,
            s.case_key,
            // `abs` is stored as TEXT, so a path that is not valid UTF-8 arrives here lossy.
            abs.to_string_lossy(),
            s.hash,
            // No filesystem sizes a file near `i64::MAX`, but the column is SQLite's INTEGER
            // and a bare `as` cast would wrap such a size into a negative one.
            i64::try_from(s.size).unwrap_or(i64::MAX),
            s.mtime,
            now
        ],
    )?;
    Ok(())
}

/// Move a file's registry row from `from` to `to`, keeping everything that makes its note the
/// same note.
///
/// Only `files` is written: `notes.rel` follows through its `ON UPDATE CASCADE`, and the rows
/// derived from the note's content are keyed on `notes.id` and therefore not affected at all.
/// `case_key` is lowercased here in Rust, the way [`crate::walk`] builds it — SQLite's
/// `LOWER()` folds ASCII only and would give a renamed umlaut filename a case key no snapshot
/// ever produces. `abs` is rebased by swapping the old `rel` suffix for the new one under
/// whichever separator spelling the stored path uses — `rel` is always `/`-separated while `abs`
/// is native — so the workspace root keeps its exact spelling and a Windows move stays a Windows
/// path.
///
/// The row also comes back to `state = 'ok'`: `classify` reports `Moved` only for a path that is
/// there now, so a move is a sighting even when the file had been marked stale. It refreshes
/// `last_seen` and `state` but not `size`, `mtime` or `hash` — a caller that learned fresh
/// metadata with the move follows up with [`upsert_file`].
///
/// A missing `from` is an error rather than a silent no-op: the caller got it from a row that
/// was there a moment ago, and moving nothing would leave the scan reporting a move that did
/// not happen. A `to` that some other row already holds fails on the primary key, like any
/// other collision — two paths cannot be one note.
pub fn apply_move(db: &Db, from: &str, to: &str, now: i64) -> Result<(), DbError> {
    let abs: String =
        db.0.query_row(
            "SELECT abs FROM files WHERE rel = ?1",
            params![from],
            |row| row.get(0),
        )
        .map_err(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => DbError::MissingRel {
                rel: from.to_string(),
            },
            other => DbError::Sqlite(other),
        })?;
    let new_abs = rebase_abs(&abs, from, to).ok_or_else(|| DbError::BadAbs {
        rel: from.to_string(),
        abs,
    })?;

    db.0.execute(
        "UPDATE files
            SET rel = ?2, case_key = ?3, abs = ?4, last_seen = ?5, state = 'ok'
          WHERE rel = ?1",
        params![from, to, to.to_lowercase(), new_abs, now],
    )?;
    Ok(())
}

/// Mark a path as gone without destroying anything.
///
/// The row and every child of it stay: the note keeps its headings, tags, links and items so
/// it can rejoin with its identity if the file comes back, and so the catalog can say "stale"
/// instead of forgetting it ever existed (spec §2). A path that is not in the registry changes
/// nothing, which is correct — there is no row to mark.
pub fn mark_stale(db: &Db, rel: &str) -> Result<(), DbError> {
    db.0.execute(
        "UPDATE files SET state = 'stale' WHERE rel = ?1",
        params![rel],
    )?;
    Ok(())
}

/// How much the database holds, for the scan report and the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteCounts {
    /// Note rows, stale files included.
    pub notes: i64,
    /// Link rows, resolved and unresolved alike.
    pub links: i64,
    /// Links whose `dst_note_id` is NULL: the target is not in the registry.
    pub unresolved_links: i64,
    /// Items waiting to be done.
    pub items_open: i64,
    /// Items closed with a done marker.
    pub items_done: i64,
    /// Items explicitly paused.
    pub items_paused: i64,
}

/// Count the rows a scan reports, as `COUNT(*)` queries — nothing is loaded into Rust, since
/// these are whole-table numbers over a catalog-sized database.
///
/// Stale files count too: marking one stale hides it from the catalog, it does not remove its
/// note.
pub fn note_counts(db: &Db) -> Result<NoteCounts, DbError> {
    Ok(NoteCounts {
        notes: count(&db.0, "SELECT COUNT(*) FROM notes")?,
        links: count(&db.0, "SELECT COUNT(*) FROM links")?,
        unresolved_links: count(
            &db.0,
            "SELECT COUNT(*) FROM links WHERE dst_note_id IS NULL",
        )?,
        items_open: count(&db.0, "SELECT COUNT(*) FROM items WHERE state = 'open'")?,
        items_done: count(&db.0, "SELECT COUNT(*) FROM items WHERE state = 'done'")?,
        items_paused: count(&db.0, "SELECT COUNT(*) FROM items WHERE state = 'paused'")?,
    })
}

/// One `COUNT(*)`-shaped query, as an `i64`.
fn count(conn: &Connection, sql: &str) -> Result<i64, DbError> {
    Ok(conn.query_row(sql, [], |row| row.get::<_, i64>(0))?)
}

/// `abs` with its trailing `from` replaced by `to`, or `None` when `abs` does not end in
/// `from` along a separator.
///
/// The suffix is tried under both separator spellings, because the two paths are spelled
/// differently by design: `rel` is always `/`-separated ([`crate::walk`] joins with `/`) while
/// `abs` is the native path the walker was handed. Matching only `/` would fail every Windows
/// move of a file inside a directory — `C:\ws\notes\a.md` does not end with `notes/a.md` — and
/// the replacement is re-spelled with whichever separator matched, so the new path stays native.
///
/// The separator check is what stops `/ws/xa.md` from being read as ending in `a.md`. The boundary
/// itself may be spelled either way, because a root the user wrote with one separator keeps it
/// where the first component was joined while deeper components carry the platform's own: a stored
/// `C:/ws/notes\a.md` is the same file under `C:/ws/` as `C:\ws\notes\a.md`, and answering `BadAbs`
/// for the mixed spelling would lose a nested rename's history.
fn rebase_abs(abs: &str, from: &str, to: &str) -> Option<String> {
    for separator in ["/", "\\"] {
        let suffix = from.replace('/', separator);
        let Some(root) = abs.strip_suffix(suffix.as_str()) else {
            continue;
        };
        if root.is_empty() || root.ends_with(['/', '\\']) {
            return Some(format!("{root}{}", to.replace('/', separator)));
        }
    }
    None
}

/// `links.origin` for an edge the parser read out of a note, as opposed to one a person approved.
/// Spelled once here because the count and the list beside it are two queries ([`unresolved_targets`]
/// here, `crate::scan::unresolved_links` there) plus the writer and the deleter in [`crate::scan`], and
/// an author-facing report that filtered one query by a literal `'prose'` and the other by a constant
/// would drift the moment M3 introduces a second origin.
pub(crate) const PROSE_ORIGIN: &str = "prose";

/// A link edge whose destination is a live note, spelled once because four queries have to agree on
/// it: the two counts `crate::scan::catalog_entries` renders, that module's `unresolved_links`, and
/// [`unresolved_targets`] here. The notes→files join is the point: an edge into a stale note is not a
/// healthy link. Migration 3 removed the other case this predicate used to catch — an edge pointing at a
/// note row that had been deleted, now impossible because `dst_note_id` is a foreign key that NULLs
/// itself — so the join stays for the stale case and the query is unchanged.
pub(crate) const LIVE_EDGE: &str =
    "EXISTS (SELECT 1 FROM notes dn JOIN files df ON df.rel = dn.rel \
                                 WHERE dn.id = l.dst_note_id AND df.state = 'ok')";

/// The non-attachment link targets no live note answers: the "notes I could not place" list a scan
/// carries as [`crate::ScanReport::unresolved_targets`] and prints beside its
/// [`crate::ScanReport::unresolved_links`] count.
///
/// `note_counts` counts every NULL destination, because at database level an attachment with no
/// note row behind it is unresolved like a missing file; this is the author-facing question instead,
/// so attachments are left out and only notes are asked about. Stale sources count too — their rows
/// are history the report does not rewrite.
///
/// The predicate is `LIVE_EDGE` rather than `dst_note_id IS NULL` on purpose: an edge whose stored
/// destination went stale counts in the report's number without having a NULL in it, so it has to be
/// named here as well. A count and the list printed behind it that disagree would send the reader
/// looking for a note the list cannot show.
///
/// Sorted and deduplicated: one name written in three notes is one thing to fix.
pub fn unresolved_targets(db: &Db) -> Result<Vec<String>, DbError> {
    let mut statement = db.0.prepare(&format!(
        "SELECT DISTINCT target FROM links l
          WHERE l.origin = '{PROSE_ORIGIN}' AND l.kind != 'attachment'
            AND NOT {LIVE_EDGE}
          ORDER BY target",
    ))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut targets = Vec::new();
    for row in rows {
        targets.push(row?);
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::rebase_abs;

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
    fn rebase_abs_refuses_a_path_that_only_looks_like_a_suffix() {
        // `/ws/xa.md` ends in the characters `a.md` but not in that path component; and an
        // abs from another workspace has nothing to swap.
        assert_eq!(rebase_abs("/ws/xa.md", "a.md", "b.md"), None);
        assert_eq!(rebase_abs("/elsewhere/a.md", "notes/a.md", "b.md"), None);
        // The same trap in the Windows spelling.
        assert_eq!(rebase_abs("C:\\ws\\xa.md", "a.md", "b.md"), None);
    }
}
