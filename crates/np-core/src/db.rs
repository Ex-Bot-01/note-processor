//! One SQLite file per workspace: the registry of paths, plus the rows derived from them.
//!
//! Two rules decide the shape of everything here. A file that is gone is **marked stale and
//! never destroyed**: its row keeps `state = 'stale'` with every child row intact,
//! so the note can come back later and rejoin the live set as the same note. And a move
//! changes a path, not a note — [`apply_move`] writes one row of `files`, rewrites that note's display
//! `rel` beside it, and lets `notes.rel_bytes` ride its own cascade (migration 4 removed `notes.rel`'s foreign
//! key, so the display column is maintained here rather than by SQLite), while `headings`, `tags`, `links`
//! and `items`
//! stay put because they hang off `notes.id`, which never changes. That is how a rename
//! keeps a note's links.
//!
//! That hanging-off is stated in the schema as well as in the code: migration 3 gave every child
//! column a foreign key on `notes(id)`, so a row no note owns cannot be written at all. Each entry of
//! [`MIGRATIONS`] says what a deleted note does to the rows around it.
//!
//! There is no transaction helper on purpose: the scan wraps itself in one
//! [`rusqlite::Connection::unchecked_transaction`], which needs the public connection.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use thiserror::Error;

use crate::db::meta::stored_receipt;
use crate::db::meta::Carried;
use crate::db::migrations::migrate;

#[cfg(test)]
mod tests;

mod chunks;
mod embeddings;
mod links;
mod media;
mod meta;
mod migrations;
mod models;
mod notes;
mod upgrades;

pub use self::chunks::{
    chunk_digest, chunk_state, live_chunks, live_note_paths, write_chunks_for_note, ChunkWrite,
    LiveChunk,
};
pub use self::embeddings::{delete_vectors_for_chunk, upsert_vector, vectors_for, VectorStamp};
pub use self::links::{attachments, unresolved_targets, Attachments};
pub(crate) use self::links::{LIVE_EDGE, PROSE_ORIGIN};
pub use self::media::{
    link_media_path, media_by_hash, media_embeddings, media_path_check, media_paths, media_stamp,
    prune_unlinked_media, record_media, record_media_embedding, set_media_path_check,
    unlink_media_path, MediaRow, PathCheck,
};
pub use self::meta::{FK_ORPHANS_KEY, NOTE_MERGE_KEY, PATHS_LOSSY_KEY};
pub use self::migrations::{Migration, MIGRATIONS};
pub use self::models::{
    insert_model, member_stamp, model_files, model_row, model_rows, record_artifact_stamp,
    record_member_stamp, record_model_file, ModelFile, ModelRow,
};
pub use self::notes::{
    apply_move, insert_note, mark_stale, note_counts, note_ids, upsert_file, NoteCounts,
};
pub use self::upgrades::{merge_total, orphan_total};

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

/// The step of the wait described at [`Db::open`]'s journal-mode retry.
pub(crate) const FIRST_OPEN_RETRY_WAIT: Duration = Duration::from_millis(20);

/// How many times that wait is taken before the open gives up. A solo first open is milliseconds of DDL, so the ceiling is
/// orders of magnitude past the work it waits for; it exists because SQLite will not wait on our behalf for a journal-mode
/// change, not because building a schema is slow. An opener that still cannot convert after this reports what it reports
/// today — the bound is there so a workspace genuinely wedged by another process stays a reportable failure.
pub(crate) const FIRST_OPEN_RETRY_LIMIT: u32 = 20;

/// Whether SQLite said *someone else holds this right now* rather than refusing the statement itself. Only the first is
/// worth waiting on: a statement that was refused does not become acceptable with delay, and retrying one turns a clear
/// failure into a slow one.
fn is_transient_lock(source: &rusqlite::Error) -> bool {
    matches!(
        source,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::DatabaseBusy
    )
}

/// Whether anything in `err`'s source chain is SQLite reporting a database locked by another connection — the failure two
/// writers on one workspace end in once the busy timeout runs out. For a caller outside this crate that has to name that
/// failure without depending on SQLite's types.
pub fn is_database_busy(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut next: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(current) = next {
        let sqlite = current.downcast_ref::<rusqlite::Error>().or_else(|| {
            match current.downcast_ref::<DbError>() {
                Some(DbError::Sqlite(inner)) => Some(inner),
                _ => None,
            }
        });
        if let Some(rusqlite::Error::SqliteFailure(failure, _)) = sqlite {
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) {
                return true;
            }
        }
        next = current.source();
    }
    false
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
    /// SQLite rejected a statement. Constraint violations arrive unchanged, because the scan's
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
    /// `byte_path_keys` (this module's migration-4 step) was reached with foreign-key enforcement switched on.
    /// This is not about the data —
    /// it is the step refusing to run at all, because `DROP TABLE notes` under enforcement performs an
    /// implicit `DELETE FROM` that obeys every child constraint, and four of those children are
    /// `ON DELETE CASCADE`: the workspace's headings, tags, items and prose edges would go with the row and
    /// nothing downstream would report it. `migrate` switches enforcement off outside the transaction for
    /// exactly this reason; the guard is there so a later refactor cannot quietly hand the step a connection
    /// where that promise no longer holds.
    #[error("PRAGMA foreign_keys reads {value}; rebuilding `files` and `notes` needs it off, so nothing was written")]
    EnforcementOn {
        /// What the pragma answered (`1` is on).
        value: i64,
    },
    /// A rebuild's replacement table does not hold as many rows as the table it is about to replace, so the
    /// step refuses the `DROP`. A count cannot prove the *right* rows survived — that is what the per-row key
    /// comparison in this migration is for — but it is the only assertion that can see a lost **attachment**: a
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
/// The connection is public because a scan runs its whole write phase inside one transaction, which
/// needs the [`Connection`] itself rather than a helper this module would have to grow.
///
/// The second field is what an upgrade tells *its own process* about itself ([`Db::upgrade_orphans`],
/// [`Db::merged_notes`]). It is not part of the file and it is not read back out of the file on demand: it
/// is this open's memory, so a second `Db::open` of the same database starts with nothing to announce.
#[derive(Debug)]
pub struct Db(
    /// The live connection, public for the scan's whole-write transaction.
    pub Connection,
    /// What this open's own migration steps wrote into `meta`, field by field. Read through
    /// [`Db::upgrade_orphans`] and [`Db::merged_notes`].
    Carried,
);

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
        // The per-connection settings go first and on their own, because both take effect whatever another process is
        // doing with the file — which is precisely what the journal mode below cannot promise.
        conn.execute_batch(&format!(
            "PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}; PRAGMA foreign_keys = ON;"
        ))?;
        // WAL is a file-database privilege: `:memory:` refuses it, so it is asked for here
        // and not in [`Db::in_memory`].
        //
        // **The journal mode is the one statement here that cannot wait for its own turn.** SQLite answers a `journal_mode`
        // change made while another connection holds the file with `database is locked`, and it does not consult this
        // connection's busy timeout on the way: the pragma comes back immediately, with `busy_timeout` set one statement
        // earlier and never consulted. Nothing shows until a workspace is being *created*, where both openers are writers of
        // the schema rather than readers of one — so the loser of this statement used to be the first search of a brand-new
        // folder, told its database was locked milliseconds after it did not exist. Retrying waits for the condition the
        // pragma actually needs (the other open's transaction finished) instead of for a lock SQLite never offers; a solo
        // first open never takes a second trip, so only contention that already existed pays anything.
        let mut waits = 0u32;
        loop {
            match conn.execute_batch("PRAGMA journal_mode = WAL;") {
                Ok(()) => break,
                Err(source) if waits < FIRST_OPEN_RETRY_LIMIT && is_transient_lock(&source) => {
                    waits += 1;
                    std::thread::sleep(FIRST_OPEN_RETRY_WAIT);
                }
                Err(source) => return Err(source.into()),
            }
        }
        let carried = migrate(&conn)?;
        Ok(Db(conn, carried))
    }

    /// An empty database in memory, migrated the same way a file one is. For tests.
    pub fn in_memory() -> Result<Db, DbError> {
        let conn = Connection::open_in_memory()?;
        // The timeout is meaningless for a private `:memory:` database, which no second process can
        // lock, and is set anyway so that "every connection sets it" stays true of this crate.
        conn.execute_batch(&format!(
            "PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}; PRAGMA foreign_keys = ON;"
        ))?;
        let carried = migrate(&conn)?;
        Ok(Db(conn, carried))
    }
}

impl Db {
    /// The receipt this open wrote under [`FK_ORPHANS_KEY`], when — and only when — **this** open ran
    /// the orphan cleanup; `None` for every open that found the schema already migrated.
    ///
    /// Carried on the handle rather than read from `meta` by whoever feels like saying it: the stored
    /// receipt is never cleared, so "the upgrade removed N rows" stays true for the life of the file, and
    /// a scan that echoed it from then on would be a permanent one-line warning. Whether the
    /// value is worth printing is the reader's question — [`crate::ScanReport::upgrade_orphans`] answers
    /// it, and drops the all-zero case that proves only that the check ran.
    pub fn upgrade_orphans(&self) -> Option<&str> {
        self.1.orphans.as_deref()
    }

    /// The receipt this open wrote under [`NOTE_MERGE_KEY`], when — and only when — **this** open ran
    /// migration 5; `None` for every open that found the schema already migrated.
    ///
    /// Carried on the handle for the same reason [`Db::upgrade_orphans`] is, and with the same division of
    /// labour: the stored value is never cleared, so "merged N duplicate row(s)" stays true for the life of
    /// the file, while whether it is worth saying out loud is the reader's question —
    /// [`crate::ScanReport::merged_notes`] answers it and drops the `merged=0` case that proves only that
    /// the check ran.
    pub fn merged_notes(&self) -> Option<&str> {
        self.1.merges.as_deref()
    }

    /// The receipt this **file holds** under [`FK_ORPHANS_KEY`], whoever wrote it — including an open that only
    /// arrived at an already-migrated schema. `None` means no record exists, which is a different fact from the
    /// all-zero receipt and stays distinguishable from it.
    ///
    /// **Why this exists when [`Db::upgrade_orphans`] does.** The carried value answers "what did *this* process
    /// delete", and it is deliberately `None` forever after, because a permanent one-line warning trains a reader to
    /// ignore it. That left the other question — what did an upgrade delete here, months ago, which is the
    /// question someone asks when notes look shorter than they remember — answerable only with `sqlite3`. Reading
    /// `meta` is the same record through a different door: nothing echoes, nothing clears, and no run becomes louder
    /// for a fact it did not produce.
    pub fn recorded_upgrade_orphans(&self) -> Result<Option<String>, DbError> {
        stored_receipt(&self.0, FK_ORPHANS_KEY)
    }

    /// The receipt this **file holds** under [`NOTE_MERGE_KEY`], with the same contract and the same reason for
    /// existing as [`Db::recorded_upgrade_orphans`].
    pub fn recorded_merged_notes(&self) -> Result<Option<String>, DbError> {
        stored_receipt(&self.0, NOTE_MERGE_KEY)
    }
}
