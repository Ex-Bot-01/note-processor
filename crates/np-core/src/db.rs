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
//! There is no transaction helper on purpose: Task 10 wraps a whole scan in one
//! [`rusqlite::Connection::unchecked_transaction`], which needs the public connection.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use thiserror::Error;

use crate::hash::Snapshot;

/// The schema, oldest first. Each entry is applied once, inside its own transaction, and its
/// 1-based position is recorded in `schema_version`. A database whose recorded version is
/// ahead of this list is refused rather than trusted — a newer file read by an older binary
/// would otherwise be written to a schema this code does not know.
///
/// Two entries rather than one, split along the line that matters (`files` + `notes` are the
/// registry and the note's identity; everything after them is derived from a note's contents
/// and can be rebuilt from the file), so the version bookkeeping has more than one row to
/// prove itself with. Later milestones append an entry here; they do not edit one.
pub const MIGRATIONS: &[&str] = &[
    // The registry, and one note row per markdown file. `rel` is the identity a scan keys on
    // and `case_key` the one a case-insensitive filesystem keys on, so both are constrained.
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
    // Derived rows. Every one of them keys on `notes.id`, which is why a move never has to
    // touch them and why identity survives a rename.
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
];

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
    /// Two pragmas are set before anything else: `journal_mode=WAL`, because a scan and an
    /// `np` command can overlap, and `foreign_keys=ON`, because SQLite leaves constraints off
    /// by default per connection — without it a move would leave its notes behind instead of
    /// cascading, and orphan notes would be accepted.
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
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        migrate(&conn)?;
        Ok(Db(conn))
    }

    /// An empty database in memory, migrated the same way a file one is. For tests.
    pub fn in_memory() -> Result<Db, DbError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
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

    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version <= current {
            continue;
        }
        // One entry per transaction: DDL is transactional in SQLite, so a migration that
        // fails halfway leaves nothing behind and cannot be recorded as done.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![version],
        )?;
        tx.commit()?;
    }
    Ok(())
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

/// A link edge whose destination is a live note, spelled once because four queries have to agree on
/// it: the two counts `crate::scan::catalog_entries` renders, that module's `unresolved_links`, and
/// [`unresolved_targets`] here. The notes→files join is the point — an edge into a stale note, or one
/// left pointing at a row that was deleted, is not a healthy link.
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
/// destination went stale or was deleted counts in the report's number, so it has to be named here as
/// well. A count and the list printed behind it that disagree would send the reader looking for a note
/// the list cannot show.
///
/// Sorted and deduplicated: one name written in three notes is one thing to fix.
pub fn unresolved_targets(db: &Db) -> Result<Vec<String>, DbError> {
    let mut statement = db.0.prepare(&format!(
        "SELECT DISTINCT target FROM links l
          WHERE l.origin = 'prose' AND l.kind != 'attachment'
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
