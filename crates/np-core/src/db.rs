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

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use crate::hash::Snapshot;
use crate::path_keys;

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
/// Four entries rather than one, split along the lines that matter (`files` + `notes` are the
/// registry and the note's identity; everything after them is derived from a note's contents
/// and can be rebuilt from the file; the third adds the keys between those two groups; the fourth
/// re-keys the registry's own pair onto bytes), so the
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
    // `files` and `notes` gain byte keys (F13). Also not a script, for the same reason plus one more: the
    // ASCII-case fold of a byte key is Rust's, and SQL has no equivalent that a copy could call.
    Migration::Code(byte_path_keys),
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
    /// [`byte_path_keys`] was reached with foreign-key enforcement switched on. This is not about the data —
    /// it is the step refusing to run at all, because `DROP TABLE notes` under enforcement performs an
    /// implicit `DELETE FROM` that obeys every child constraint, and four of those children are
    /// `ON DELETE CASCADE`: the workspace's headings, tags, items and prose edges would go with the row and
    /// nothing downstream would report it. [`migrate`] switches enforcement off outside the transaction for
    /// exactly this reason; the guard is there so a later refactor cannot quietly hand the step a connection
    /// where that promise no longer holds.
    #[error("PRAGMA foreign_keys reads {value}; rebuilding `files` and `notes` needs it off, so nothing was written")]
    EnforcementOn {
        /// What the pragma answered (`1` is on).
        value: i64,
    },
    /// A rebuild's replacement table does not hold as many rows as the table it is about to replace, so the
    /// step refuses the `DROP`. A count cannot prove the *right* rows survived — that is what F13 §5's
    /// per-row key comparison is for — but it is the only assertion that can see a lost **attachment**: a
    /// `files` row with no note row has no child at all, so nothing cascades, `PRAGMA foreign_key_check`
    /// stays clean, and the drop would destroy the row with no error and no report line.
    #[error("refusing to drop {table}: it holds {old} row(s) but its replacement holds {staged}, so the copy lost a row")]
    CopyShrink {
        /// The table about to be dropped.
        table: &'static str,
        /// Rows in it at the moment of the check.
        old: i64,
        /// Rows in the staged replacement.
        staged: i64,
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
    // Saying it out loud as well: reading the receipt back needs sqlite3, and a person who just
    // watched an ordinary command delete rows deserves to be told they were deleted.
    report_orphans(&mut io::stderr(), &removed);
    Ok(())
}

/// Write the orphan receipt to `out` — [`std::io::stderr`] in production — unless there is nothing
/// to say.
///
/// The sink is a parameter rather than a hard-wired `eprintln!` because stderr cannot be captured
/// in-process here: the standard library offers no way to replace the global handle, and this runs
/// inside [`Db::open`], before any test could redirect descriptor 2. Taking the sink lets the unit
/// tests in this module pin both halves of the promise — one line naming the total and the stored
/// counts for a real cleanup, silence for an all-zero one — so a regression in this visibility path
/// fails a test instead of going unnoticed; what they cannot pin is that the production call site
/// names stderr.
///
/// A failed write panics, which is what `eprintln!` did: losing the announcement to a closed
/// terminal quietly is the silence this function exists to avoid.
fn report_orphans(out: &mut dyn io::Write, removed: &OrphanCleanup) {
    // The position at the call site is deliberate — after the violation check in
    // [`add_note_foreign_keys`], so nothing is announced about a migration that rolled itself back.
    // All zeros stays silent: every pre-F5 workspace passes through here, and "found nothing" on an
    // ordinary upgrade is not news.
    if removed.total() == 0 {
        return;
    }
    writeln!(
        out,
        "note-processor: removed {} orphan row(s) while adding note foreign keys ({})",
        removed.total(),
        removed.value(),
    )
    .expect("printing the orphan receipt to stderr");
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
/// `ORDER BY note_id, rowid` and [`crate::catalog`] renders them in that first-seen order. Without the
/// sort the answer belongs to the planner: today a plain table scan happens to yield rowid order, but
/// any future plan that reaches the rows another way would hand the rowids out in that other order —
/// reading `tags_tag` and fetching each row behind it, say (that index carries `tag` and the rowid
/// only, so it cannot answer `SELECT note_id, tag` by itself), or a plan some `ANALYZE`-written
/// statistic prefers. The next scan would then rewrite every tagged entry line of `INDEX.md` with no
/// real change to report, against the byte-stability [`crate::catalog`] promises at its top. The sort
/// pins the outcome for determinism whatever a later planner decides; it is not a bet on today's plan.
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

/// `meta` key recording how many migrated rows carry a lossy display spelling.
///
/// Beside [`FK_ORPHANS_KEY`] for the same reason (F13 §4): a person reading a scan report afterwards has to
/// be able to tell "this file was upgraded and held two such paths" apart from "this file was never
/// upgraded", and there is no logger to write to. The value is `files=N,notes=M`, written **even when both
/// are zero** — the count's whole use is that it distinguishes looked-and-found-none from never-looked, and
/// the first byte-keyed scan's expected churn can only be read against it.
pub const PATHS_LOSSY_KEY: &str = "paths_lossy";

/// Migration 4 — `files` and `notes` are rebuilt so their identity lives in bytes (F13 §4).
///
/// Why bytes: a path is an OS byte string, `rel` was stored as TEXT, and SQLite stores anything that is not
/// valid UTF-8 as the replacement character. Two files named `caf\xE9.md` and `caf\xE8.md` therefore became
/// one row whose `rel` matched no file on disk — and because `abs` was a *derived* column, the real path was
/// not recoverable from the row either (F13 §1). The new `rel_bytes` / `case_bytes` pair is the identity;
/// `rel`, `case_key` and `abs` stay, become display text, and keep being written exactly as before, so every
/// reader that still matches on them keeps working until the round that moves them.
///
/// The shape follows [`add_note_foreign_keys`] — stage, copy, drop, rename, recreate indexes, gate on
/// `PRAGMA foreign_key_check` inside the transaction — with three differences this step cannot do without:
///
/// - **It reads `PRAGMA foreign_keys` and refuses when it is on** ([`DbError::EnforcementOn`]), rather than
///   assuming [`migrate`] turned it off. With enforcement on, `DROP TABLE notes` implicitly deletes the row,
///   which cascades into four child tables; migration 3's own rebuilds are why that promise exists 200 lines
///   away, and a step that destroys a workspace on someone else's assumption is not a step.
/// - **The `files` copy is a loop over rowids**, not an `INSERT … SELECT`: there is no `id` column to select
///   today (`rel TEXT PRIMARY KEY` on a rowid table is a unique index, not a rowid alias), and `case_bytes`
///   is a per-row Rust fold that one bound parameter would stamp identically onto every row. Every row's own
///   `rowid` becomes its new `id`, so addresses do not move.
/// - **A row count is asserted before each `DROP`** ([`DbError::CopyShrink`]), because the check below cannot
///   see a lost row that has no children.
///
/// Statement order matters twice over. Both renames happen **before** `PRAGMA foreign_key_check`, because
/// `notes_new`'s key names `files` — a table that does not exist between the drop and the rename, so an
/// earlier check reports a missing parent rather than a broken row. And indexes are recreated after both
/// renames, since dropping a table takes its indexes with it.
///
/// Byte stability, on top of [`rebuild_with_keys`]'s `ORDER BY rowid`: `notes.id` is carried, never re-minted,
/// because every derived row hangs off it. Only these two tables are rebuilt — `tags` is untouched, so no tag
/// rowid moves and no published byte in `INDEX.md` can shift.
fn byte_path_keys(conn: &Connection) -> Result<(), DbError> {
    // Before anything is written: this pragma cannot be changed inside a transaction, so the step cannot fix
    // what it finds — only refuse it. `PRAGMA foreign_keys;` answers through a query, not `execute_batch`,
    // which discards result rows.
    let enforcement: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    if enforcement != 0 {
        return Err(DbError::EnforcementOn { value: enforcement });
    }

    // `rel` and `abs` lose their constraints and keep their values: from here they are display text, derived
    // from the byte key rather than being it. `case_key` survives without `UNIQUE` — twins share a display
    // fold by definition, so uniqueness belongs to `case_bytes` alone — but keeps its index, because link
    // resolution still reads it.
    conn.execute_batch(
        "CREATE TABLE files_new (
            id         INTEGER PRIMARY KEY,
            rel_bytes  BLOB NOT NULL UNIQUE CHECK (typeof(rel_bytes)  = 'blob'),
            case_bytes BLOB NOT NULL UNIQUE CHECK (typeof(case_bytes) = 'blob'),
            rel        TEXT NOT NULL,
            case_key   TEXT NOT NULL,
            abs        TEXT NOT NULL,
            hash       TEXT NOT NULL,
            size       INTEGER NOT NULL,
            mtime      INTEGER NOT NULL,
            state      TEXT NOT NULL CHECK (state IN ('ok','stale')),
            last_seen  INTEGER NOT NULL
        );",
    )?;
    copy_files_rows(conn)?;
    assert_replacement_complete(conn, "files", "files_new")?;
    conn.execute_batch("DROP TABLE files; ALTER TABLE files_new RENAME TO files;")?;

    // The one copy that can stay a statement: its key comes from the row's own `rel`, so `CAST` is a per-row
    // value rather than one bind shared by every row — and casting to BLOB yields `typeof(...) = 'blob'`,
    // which the CHECK demands. Binding the same text as a Rust `&str` would not (F13 §4: a TEXT key and its
    // BLOB twin never compare equal, so one accidental string bind reopens F13 silently).
    //
    // `rel` stops being a foreign key. Two live notes may spell themselves identically once the lossy
    // spelling is admitted, and a key that two rows share cannot be a key; `notes.rel`'s `ON UPDATE CASCADE`
    // was the only thing maintaining that column, so [`apply_move`] takes over the job in round 1.
    conn.execute_batch(
        "CREATE TABLE notes_new (
            id           INTEGER PRIMARY KEY,
            rel_bytes    BLOB REFERENCES files(rel_bytes) ON UPDATE CASCADE
                         CHECK (typeof(rel_bytes) = 'blob'),
            rel          TEXT,
            title        TEXT,
            title_source TEXT,
            encoding     TEXT,
            word_count   INTEGER,
            body         TEXT
        );
         INSERT INTO notes_new (id, rel_bytes, rel, title, title_source, encoding, word_count, body)
         SELECT id, CAST(rel AS BLOB), rel, title, title_source, encoding, word_count, body
           FROM notes ORDER BY rowid;",
    )?;
    assert_replacement_complete(conn, "notes", "notes_new")?;
    conn.execute_batch("DROP TABLE notes; ALTER TABLE notes_new RENAME TO notes;")?;

    // Both tables are back under their own names, so the keys they name exist and the indexes land on the
    // live table. `files_rel` and `notes_rel` are lookup-only now — neither column is unique or a key any
    // more — and round 1 needs both: row resolution (`SELECT id FROM files WHERE rel = ?1`) and `mark_stale`
    // still ask about display text, once per file inside a scan's write phase.
    conn.execute_batch(
        "CREATE INDEX files_case_bytes ON files(case_bytes);
         CREATE INDEX files_case_key   ON files(case_key);
         CREATE INDEX files_rel        ON files(rel);
         CREATE INDEX notes_rel        ON notes(rel);
         CREATE INDEX notes_rel_bytes  ON notes(rel_bytes);",
    )?;

    if let Some((table, rowid, parent)) = first_violation(conn)? {
        // Nothing above is committed: the caller's transaction rolls back with this error, so the file keeps
        // its version-3 schema and its data. An orphan note is the case this catches — it was writable with
        // enforcement off, which is how orphans got in at all (see [`FK_ORPHANS_KEY`]) — and this step can
        // neither delete it (its file may come back; that is what `state = 'stale'` is for) nor invent a
        // parent for it.
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

    let receipt = format!(
        "files={},notes={}",
        lossy_rows(conn, "files")?,
        lossy_rows(conn, "notes")?
    );
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![PATHS_LOSSY_KEY, receipt],
    )?;
    Ok(())
}

/// One row of version-3 `files`, read out before any write happens.
///
/// Owned values rather than borrowed ones for that reason: SQLite refuses to start a write on a connection
/// that still has a row-producing statement mid-iteration, so the read has to finish — not merely pause —
/// before the first `INSERT` below. Same discipline as `scan::known_rows`.
struct FileRow {
    /// The address this row keeps: it becomes the new `id` column, which is a rowid alias.
    rowid: i64,
    rel: String,
    case_key: String,
    abs: String,
    hash: String,
    size: i64,
    mtime: i64,
    state: String,
    last_seen: i64,
}

/// Copy version-3 `files` into `files_new`, one row at a time, deriving both byte keys in Rust.
///
/// Ordered by `rowid` so the new `id`s are handed out in the same sequence the old rows held them — the
/// stability ruling F5's copy already follows (see [`rebuild_with_keys`] for why the order is load-bearing
/// rather than stylistic). `case_key` is copied, never recomputed: SQLite's `LOWER()` folds ASCII only, so
/// deriving it in SQL would rewrite every non-ASCII name's fold and break the link resolution that reads it.
fn copy_files_rows(conn: &Connection) -> Result<(), DbError> {
    let rows: Vec<FileRow> = {
        let mut statement = conn.prepare(
            "SELECT rowid, rel, case_key, abs, hash, size, mtime, state, last_seen
               FROM files ORDER BY rowid",
        )?;
        let mapped = statement.query_map([], |row| {
            Ok(FileRow {
                rowid: row.get::<_, i64>(0)?,
                rel: row.get::<_, String>(1)?,
                case_key: row.get::<_, String>(2)?,
                abs: row.get::<_, String>(3)?,
                hash: row.get::<_, String>(4)?,
                size: row.get::<_, i64>(5)?,
                mtime: row.get::<_, i64>(6)?,
                state: row.get::<_, String>(7)?,
                last_seen: row.get::<_, i64>(8)?,
            })
        })?;
        // Collected here, so the borrow of `statement` ends with this block and the writes below can begin.
        mapped.collect::<Result<Vec<_>, _>>()?
    };

    let mut insert = conn.prepare_cached(
        "INSERT INTO files_new (id, rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state,
                                last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for row in rows {
        // Both keys come from this row's own `rel`, through the one module that owns them (F13 §4: no
        // statement derives a key by hand). Round 1's keys are the bytes of the stored display spelling,
        // U+FFFD included — see [`crate::path_keys`].
        let rel_bytes = path_keys::rel_bytes(&row.rel);
        let case_bytes = path_keys::case_fold(&rel_bytes);
        insert.execute(params![
            row.rowid,
            rel_bytes,
            case_bytes,
            row.rel,
            row.case_key,
            row.abs,
            row.hash,
            row.size,
            row.mtime,
            row.state,
            row.last_seen
        ])?;
    }
    Ok(())
}

/// Refuse the swap unless the replacement holds exactly as many rows as the table it is about to replace.
fn assert_replacement_complete(
    conn: &Connection,
    table: &'static str,
    staged: &'static str,
) -> Result<(), DbError> {
    let old = count_rows(conn, table)?;
    let copied = count_rows(conn, staged)?;
    if old != copied {
        return Err(DbError::CopyShrink {
            table,
            old,
            staged: copied,
        });
    }
    Ok(())
}

/// `COUNT(*)` of one named table. Both names come from this module's own call sites, never from a caller.
fn count_rows(conn: &Connection, table: &str) -> Result<i64, DbError> {
    Ok(
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })?,
    )
}

/// How many rows of `table` display a path that some earlier read could not spell losslessly.
///
/// The test is U+FFFD in the display column, which is what a lossy read leaves behind. It counts rows whose
/// *display* carries the marker rather than rows whose key differs from something: after this migration the
/// two are the same fact, and the count is what makes the first byte-keyed scan's churn readable (F13 §4.2).
fn lossy_rows(conn: &Connection, table: &str) -> Result<i64, DbError> {
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE INSTR(rel, ?1) > 0"),
        params!["\u{fffd}"],
        |row| row.get(0),
    )?)
}

/// Record a file that is there now — or bring a known row back to `state = 'ok'`.
///
/// A path re-upserted after [`mark_stale`] rejoins the live set on the row it already has, so its `id` never
/// changes — which is what keeps the note's derived rows attached to it. `hash`, `size`, `mtime`, `abs` and
/// `last_seen` are refreshed from the snapshot every time, because a scan that saw a file is a sighting of it.
///
/// **Both byte keys are written here, and both come from `s.rel`, never from `abs`.** Migration 4 made
/// `rel_bytes` / `case_bytes` `NOT NULL UNIQUE` and dropped `case_key UNIQUE`, so a write that supplied no key
/// would fail with `SQLITE_CONSTRAINT_NOTNULL` on the first file of every scan, and the constraint that used
/// to report P3's case pair would be gone. Deriving them from `abs` instead is not an improvement yet: the
/// identity *readers* still match on is display text, so byte-exact keys written now would put new rows under
/// one rule and every read under another (F13 §7). One module owns both derivations — [`crate::path_keys`].
///
/// The row is resolved by `id`, in two statements, where this function used to carry one
/// `ON CONFLICT (rel) DO UPDATE` arm. That arm cannot survive the migration: `rel TEXT PRIMARY KEY` was the
/// only reason `ON CONFLICT (rel)` matched anything, migration 4 gives `rel` no unique constraint at all, and
/// SQLite answers a non-matching conflict target at *prepare* time — so an arm kept out of habit does not fail
/// on a lossy name, it fails on the first ordinary ASCII file and takes every scan down with it. Resolution
/// stays display-keyed on purpose: "which row spells this path" is the same question the readers still ask.
///
/// A `case_bytes` collision — two paths differing only in ASCII case on a case-sensitive filesystem —
/// surfaces as the constraint violation it is, exactly as `case_key UNIQUE` used to. Nothing picks a winner:
/// Task 10 reports such a path under `skipped` and keeps scanning. Names differing only in a *non-ASCII*
/// letter now get two rows, which is the behaviour change F13 §3 accepts out loud.
///
/// This and [`apply_move`] / [`mark_stale`] prepare through [`Connection::prepare_cached`], like the
/// scan path already does, because each runs once per *path* inside a scan's write loop rather than
/// once per run: this one for every file the walk found, [`apply_move`] for every moved path (a
/// case-only rename included) and [`mark_stale`] for every removed one — so an uncached
/// [`Connection::execute`] would re-compile these statements thousands of times over a large
/// workspace. The saving is not three prepares per file: a warm pass over unchanged files calls only
/// this function, since nothing moved and nothing went away, so exactly one prepare is saved per
/// file. Caching changes how a statement is obtained, not what it does: every error — the
/// identity collisions below included — reaches the caller exactly as it did before.
pub fn upsert_file(db: &Db, s: &Snapshot, abs: &Path, now: i64) -> Result<(), DbError> {
    let rel_bytes = path_keys::rel_bytes(&s.rel);
    let case_bytes = path_keys::case_fold(&rel_bytes);
    let existing: Option<i64> =
        db.0.prepare_cached("SELECT id FROM files WHERE rel = ?1")?
            .query_row(params![s.rel], |row| row.get(0))
            .optional()?;

    // `abs` is stored as TEXT, so a path that is not valid UTF-8 arrives here lossy.
    let abs_text = abs.to_string_lossy();
    // No filesystem sizes a file near `i64::MAX`, but the column is SQLite's INTEGER
    // and a bare `as` cast would wrap such a size into a negative one.
    let size = i64::try_from(s.size).unwrap_or(i64::MAX);

    match existing {
        Some(id) => {
            db.0.prepare_cached(
                "UPDATE files
                    SET rel_bytes = ?2, case_bytes = ?3, case_key = ?4, abs = ?5, hash = ?6, size = ?7,
                        mtime = ?8, state = 'ok', last_seen = ?9
                  WHERE id = ?1",
            )?
            .execute(params![
                id, rel_bytes, case_bytes, s.case_key, abs_text, s.hash, size, s.mtime, now
            ])?;
        }
        None => {
            db.0.prepare_cached(
                "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state,
                                    last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'ok', ?9)",
            )?
            .execute(params![
                rel_bytes, case_bytes, s.rel, s.case_key, abs_text, s.hash, size, s.mtime, now
            ])?;
        }
    }
    Ok(())
}

/// Insert the note row for a file that has none yet, returning its freshly minted id.
///
/// This is the program's **only writer of `notes.rel_bytes`**, and it lives here rather than beside the rest
/// of [`crate::scan::store_note`] so that "bind this note key as TEXT" is not a call any scan-side site can
/// write (F13 §4). That matters more here than anywhere else in the schema: `notes.rel` is plain TEXT and stays
/// nullable, so an insert that named only the display column would succeed — leaving a note whose byte key is
/// NULL, which matches nothing the re-keyed join, cascade and lookup compare on bytes. A note row silently
/// detached from its file is F13 reproduced by the fix, starting with the first new note.
///
/// The id is minted here and returned because every derived row hangs off it; the update arm of `store_note`
/// stays in `scan.rs` because it writes no key at all.
pub fn insert_note(
    db: &Db,
    rel: &str,
    title: &str,
    title_source: &str,
    encoding: &str,
    word_count: i64,
    body: &str,
) -> Result<i64, DbError> {
    let rel_bytes = path_keys::rel_bytes(rel);
    db.0.prepare_cached(
        "INSERT INTO notes (rel_bytes, rel, title, title_source, encoding, word_count, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![
        rel_bytes,
        rel,
        title,
        title_source,
        encoding,
        word_count,
        body
    ])?;
    Ok(db.0.last_insert_rowid())
}

/// Move a file's registry row from `from` to `to`, keeping everything that makes its note the
/// same note.
///
/// The row is found by `id`, like [`upsert_file`]'s resolution: `rel` stopped being unique in migration 4, so
/// matching the update on it would rename every row sharing one display spelling — and before the migration
/// there was no `id` to match on. Both byte keys are written in the same statement as the display pair, so the
/// four columns cannot diverge.
///
/// Three things follow the path. `notes.rel_bytes` rides `files`' own update through its
/// `ON UPDATE CASCADE`, which is what lets the second statement below match on the **new** bytes; `notes.rel`
/// is rewritten by that second statement, because migration 4 removed the foreign key whose cascade used to be
/// the only thing maintaining it and the catalog renders exactly that column (`crate::scan::catalog_entries`
/// → `crate::catalog`) — a rename that left it behind would publish the old target forever and nothing else in
/// the suite would notice; and the rows derived from the note's content are keyed on `notes.id`, which does not
/// move, so they need nothing at all.
///
/// `case_key` is lowercased here through [`crate::path_keys`], the way [`crate::walk`] builds it — SQLite's
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
/// not happen. A `to` whose bytes some other row already holds fails on `rel_bytes UNIQUE` / its implicit
/// primary key, like any other collision — two paths cannot be one note.
pub fn apply_move(db: &Db, from: &str, to: &str, now: i64) -> Result<(), DbError> {
    // The lookup keeps its own cached statement, and the `MissingRel` mapping below still reads the
    // same `QueryReturnedNoRows` it always did — caching the prepare changes how the statement is
    // made, not what a miss reports.
    let (id, abs): (i64, String) =
        db.0.prepare_cached("SELECT id, abs FROM files WHERE rel = ?1")?
            .query_row(params![from], |row| Ok((row.get(0)?, row.get(1)?)))
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

    let to_bytes = path_keys::rel_bytes(to);
    let to_case = path_keys::case_fold(&to_bytes);
    db.0.prepare_cached(
        "UPDATE files
            SET rel_bytes = ?2, case_bytes = ?3, rel = ?4, case_key = ?5, abs = ?6, last_seen = ?7,
                state = 'ok'
          WHERE id = ?1",
    )?
    .execute(params![
        id,
        to_bytes,
        to_case,
        to,
        path_keys::display_fold(to),
        new_abs,
        now
    ])?;

    // Matched on the **new** bytes: `notes.rel_bytes` rode the update above through its cascade, so the old
    // spelling no longer names this note. Display text is what is written, because `notes.rel` is a display
    // column and the catalog renders it verbatim.
    db.0.prepare_cached("UPDATE notes SET rel = ?2 WHERE rel_bytes = ?1")?
        .execute(params![to_bytes, to])?;
    Ok(())
}

/// Mark a path as gone without destroying anything.
///
/// The row and every child of it stay: the note keeps its headings, tags, links and items so
/// it can rejoin with its identity if the file comes back, and so the catalog can say "stale"
/// instead of forgetting it ever existed (spec §2). A path that is not in the registry changes
/// nothing, which is correct — there is no row to mark.
pub fn mark_stale(db: &Db, rel: &str) -> Result<(), DbError> {
    db.0.prepare_cached("UPDATE files SET state = 'stale' WHERE rel = ?1")?
        .execute(params![rel])?;
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
    use super::{rebase_abs, report_orphans, OrphanCleanup};

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
        // its sink for exactly this test.
        let mut out = Vec::new();
        report_orphans(
            &mut out,
            &OrphanCleanup {
                headings: 2,
                tags: 1,
                items: 0,
                links_src: 3,
                links_dst_null: 4,
            },
        );
        assert_eq!(
            String::from_utf8(out).expect("the receipt is text"),
            "note-processor: removed 10 orphan row(s) while adding note foreign keys \
             (headings=2,tags=1,items=0,links_src=3,links_dst_null=4)\n"
        );
    }

    #[test]
    fn an_upgrade_with_nothing_removed_writes_no_receipt() {
        // The other half of the promise: every pre-F5 workspace passes through the migration, and
        // "found nothing" on an ordinary upgrade is not news. If this ever prints, every upgrade
        // shouts and the receipt stops meaning anything.
        let mut out = Vec::new();
        report_orphans(
            &mut out,
            &OrphanCleanup {
                headings: 0,
                tags: 0,
                items: 0,
                links_src: 0,
                links_dst_null: 0,
            },
        );
        assert!(out.is_empty(), "all zeros has to stay silent");
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
