//! One SQLite file per workspace: the registry of paths, plus the rows derived from them.
//!
//! Two rules decide the shape of everything here. A file that is gone is **marked stale and
//! never destroyed** (spec §2): its row keeps `state = 'stale'` with every child row intact,
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
//! There is no transaction helper on purpose: Task 10 wraps a whole scan in one
//! [`rusqlite::Connection::unchecked_transaction`], which needs the public connection.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use thiserror::Error;

use crate::chunk::{Chunk, CHUNKER_VERSION};
use crate::hash::Snapshot;
use crate::path_keys::{self, RelPath};

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
/// Eight entries rather than one, split along the lines that matter (`files` + `notes` are the
/// registry and the note's identity; everything after them is derived from a note's contents
/// and can be rebuilt from the file; the third adds the keys between those two groups; the fourth
/// re-keys the registry's own pair onto bytes; the fifth folds the duplicate note rows that key may
/// have inherited; the eighth remembers, per model, which artifact bytes had their digest checked), so the
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
    // The duplicate note rows version 3 allowed (F19). Not a script either: the survivor is chosen per key
    // and its incoming edges are re-pointed rather than dropped, which is a loop with a receipt, not a
    // statement.
    Migration::Code(merge_duplicate_notes),
    // Durable file identity (design-a4 §3): the OS's own answer to "is this the same file", so a note whose
    // bytes changed while its path was unreadable can still be rejoined to its row. Nullable and **not
    // backfilled**: identity is only readable by walking, so a row carries NULL until its path is next seen,
    // and every reader must treat that as unknown rather than as a mismatch. No CHECK (the bytes are opaque and
    // platform-shaped) and no index (nothing looks a row up by it; the scan reads the table in bulk).
    Migration::Sql(
        r#"
    ALTER TABLE files ADD COLUMN file_id BLOB;
    "#,
    ),
    // Chunk and vector storage (design-m2-embeddings §3): the first derived data that hangs off a note *id* rather
    // than a path, which is the F13 lesson applied forward — a chunk storing `rel_bytes` would inherit every
    // lossy-name, rename and merge defect six milestones spent removing.
    //
    // Four things about this shape are decisions rather than accidents:
    //
    // - **`models.sha256` is nullable by design.** Weights are never committed and `np models install` verifies the
    //   digest it fetched, but a provider with no weights — the default `embeddings.provider = "none"`, and every
    //   endpoint provider — has nothing to verify. NULL is therefore the common arm, so there is no
    //   `CHECK (typeof(sha256) = 'text')`: that CHECK would refuse the shipping configuration.
    // - **`embeddings.model` is a real foreign key**, and `models` is declared before its child so the key reads in
    //   file order. Without the reference an embedding row could name a model that was never installed, or one
    //   removed and reinstalled under the same id — exactly the orphan class migration 3 exists to end ("Migration 3
    //   gave every one of these five statements a twin in the schema", `scan.rs:1338-1340`) — and
    //   `PRAGMA foreign_key_check` reads *declared* constraints and ignores the pragma, which is what
    //   `first_violation`'s own comment says; a key that was never declared is a key that check can never see.
    //   Declaring it here costs nothing at write time and buys the one audit that reaches every row.
    // - **`vector` carries a length CHECK.** Little-endian f32 instead of the spec's q8 (`spec:85`) is a deliberate
    //   deviation — quantisation is a quality decision that belongs with the model benchmark, not smuggled in as
    //   storage trivia — and the CHECK keeps that deviation machine-enforced *now*, while the table is empty, so a
    //   later switch to q8 fails at write instead of becoming subtly wrong cosines no test would notice. `dim` is
    //   stored beside it so a mismatch is detectable at read time rather than producing garbage.
    // - **No index beyond the primary keys and `UNIQUE (note_id, ordinal)`**, because nothing looks a chunk up by
    //   anything but that pair. An index here would be written on every embed and paid for by every scan, to speed
    //   up a query no reader makes.
    Migration::Sql(
        r#"
    CREATE TABLE models (
        id            TEXT    PRIMARY KEY,
        dim           INTEGER NOT NULL,
        -- NULL is a provider with no weights, which is the common case rather than the broken one.
        sha256        TEXT,
        prefix_doc    TEXT    NOT NULL DEFAULT '',
        prefix_query  TEXT    NOT NULL DEFAULT '',
        installed_at  INTEGER NOT NULL
    );

    CREATE TABLE chunks (
        id            INTEGER PRIMARY KEY,
        note_id       INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
        ordinal       INTEGER NOT NULL,
        chunker       INTEGER NOT NULL,
        -- Raw UTF-8 bytes: `TEXT` would be a promise that note bodies are valid UTF-8, which F13 refuses to make.
        -- `typeof = 'blob'` too, not just the declared affinity: a Rust `String` bind arrives as TEXT and
        -- `length()` would then count characters, letting eight of them satisfy a 4*dim rule for dim=2.
        -- Migration 4 learned this the hard way with its key columns (F13 §4).
        text          BLOB    NOT NULL CHECK (typeof(text) = 'blob'),
        byte_start    INTEGER NOT NULL,
        byte_end      INTEGER NOT NULL,
        source_hash   TEXT    NOT NULL,
        UNIQUE (note_id, ordinal)
    );

    CREATE TABLE embeddings (
        chunk_id      INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
        model         TEXT    NOT NULL REFERENCES models(id) ON DELETE CASCADE,
        dim           INTEGER NOT NULL,
        -- Same trap one table over: without the typeof arm a TEXT bind of eight characters would satisfy
        -- `length(vector) = 4 * dim` at dim=2, and the failure would surface later as garbage cosines.
        vector        BLOB    NOT NULL CHECK (typeof(vector) = 'blob' AND length(vector) = 4 * dim),
        source_hash   TEXT    NOT NULL,
        embedded_at   INTEGER NOT NULL,
        -- Re-embedding under a second model must not destroy the first, so the pair is the key.
        PRIMARY KEY (chunk_id, model)
    );
    "#,
    ),
    // **Verify-on-change (D12, closed 2026-10-31).** Two nullable columns remembering which artifact bytes this row's
    // `sha256` was last checked against: the size and whole-second mtime of the file at that moment. Their only job is
    // to make re-verification cheap — R6d measured a full pass over the pinned artifact at **11.32 s** (26 MiB/s, our
    // own `hash::sha256_file`) against **~0.09 s** for llama.cpp's first read of every byte (§14), so hashing before
    // every embed would cost more than embedding a small workspace. A matching stat therefore skips the hash; a
    // different one, or no stamp at all, re-hashes and refuses on mismatch.
    //
    // Nullable and **not backfilled**, exactly like migration 6's `file_id`: a row installed by an older build carries
    // NULL and pays one full verification on its next embed, which is the honest cost of upgrading into a check rather
    // than a silent claim that it had always held. Both columns must be present to skip — a half-written stamp is read
    // as "unverified", never as "verified at size 0". No CHECK constraint: `sha256 IS NULL` (a provider with no
    // weights) legitimately has nothing to stamp, so the pair's meaning is enforced by [`record_artifact_stamp`]'s
    // caller rather than by SQL.
    Migration::Sql(
        r#"
    ALTER TABLE models ADD COLUMN verified_size INTEGER;
    ALTER TABLE models ADD COLUMN verified_mtime INTEGER;
    "#,
    ),
    // **A model can be more than one file (R7b, `docs/design-m2-local-embeddings.md` §12 rule 10).** The
    // `embeddinggemma-2` pair is a text encoder plus an `mmproj` vision/audio projector, and `models.sha256` holds
    // exactly one digest — the **primary** artifact's, which is what every existing reader (`verify_artifact`,
    // `install`'s comparison, `local_model`'s identity check) has always meant by it. Keeping that column meaning
    // *main* is the deliberate half of this migration: the alternative, re-pointing it at a pair, would silently
    // change what "the digest matches" asserts in code that cannot express "which file".
    //
    // §12 rule 10's asymmetry is why this is a child table and not more columns on `models`: **digests are cheap to
    // retrofit, names are not** — bytes already on disk live under `<models_dir>/<id>` with no room for a second
    // name. So the pair gets names here (`name = 'main'` is the primary, matching the artifact path) and any later
    // member arrives as a row rather than as a schema change.
    //
    // **One coupling to know before adding migration 10:** three test fixtures rewind a database to version four to
    // exercise the upgrade path (`tests/db.rs`, `tests/scan.rs`, `np-cli/tests/cli.rs`), and each undoes what later
    // entries created. A new table that they leave standing makes the next open fail with "table already exists" —
    // which is the fixture being right, not the migration being wrong.
    //
    // Not backfilled, like migration 6's `file_id` and migration 8's stamp: a model installed by an older build has
    // no recorded members until it is installed or fetched again, and readers treat "no rows" as *unknown*, never as
    // "this model has no files". No foreign key either — `models` has no delete path today, and adding an FK whose
    // cascade this migration cannot test on three OSes would be a claim, not a constraint (F18).
    Migration::Sql(
        r#"
    CREATE TABLE model_files (
        model       TEXT    NOT NULL,
        name        TEXT    NOT NULL,
        sha256      TEXT    NOT NULL CHECK (length(sha256) = 64),
        size        INTEGER NOT NULL CHECK (size >= 0),
        PRIMARY KEY (model, name)
    );
    "#,
    ),
    // 10. Media as a vector source of its own (`docs/design-m2-image-embeddings.md` §6): an image linked from a note
    // is not a chunk of prose, so it gets tables that can state what it actually is instead of a `chunks` row whose
    // `ordinal`, byte offsets and `source_hash` would have to lie about bytes no chunker ever read.
    //
    // **Content is the identity.** `media.hash` is unique, so one diagram copied to three paths embeds once and answers
    // once; `media_path` says where those bytes were seen, and re-pointing a path (a rename, or a file replaced in
    // place) changes no vector. Keying by path instead — the first draft — embedded the same figure per copy and gave
    // `np search` three hits for one thing.
    //
    // The CHECK families are borrowed, not invented: `length(hash) = 64` is migration 9's rule for `model_files.sha256`
    // (a truncated paste must not become content identity and then refuse the real file forever), `typeof(rel_bytes) =
    // 'blob'` beside declared affinity is migration 4's F13 lesson (affinity alone accepts a TEXT bind, and `length()`
    // would then count characters), and `typeof(vector) = 'blob' AND length(vector) = 4 * dim` is migration 7's fence on
    // `embeddings` — eight characters satisfy `length(vector) = 8` at dim 2 and reach a cosine as garbage.
    //
    // **The stamp columns are D12 carried to two more places.** `media.verified_size` / `verified_mtime` decide whether
    // the next run re-hashes an image or trusts one `stat`; adding them to `model_files` is what stops every image pass
    // from hashing all 554,821,056 projector bytes (§6's third change, ~21 s at D12's measured rate). Both pairs are
    // nullable and **not backfilled**: an older row reads as "never checked", which is the honest cost of upgrading into
    // a check, and a half pair reads as no pair at all (`media_stamp`, `member_stamp`) for the migration-8 reason.
    //
    // Same coupling as migration 9 warned about, now pointing forward: the eight rewind fixtures
    // (`tests/db.rs`, `tests/scan.rs`, `np-cli/tests/cli.rs`) delete version numbers and must undo what later entries
    // created, so each one drops these three tables — child first — beside `model_files`.
    Migration::Sql(
        r#"
    CREATE TABLE media (
        id             INTEGER PRIMARY KEY,
        -- Content identity: the same SHA-256 is one figure wherever it sits, however often it is copied (§7).
        hash           TEXT    NOT NULL UNIQUE CHECK (length(hash) = 64),
        size           INTEGER NOT NULL CHECK (size >= 0),
        mime           TEXT    NOT NULL,
        -- D12's stamp pair: one `stat` while the bytes have not moved, a full hash when they have.
        verified_size  INTEGER,
        verified_mtime INTEGER
    );

    -- Every workspace path those bytes were seen at. The last row going takes the media row with it (§6's lifecycle),
    -- which is what stops a deleted figure answering `np search` forever.
    CREATE TABLE media_path (
        rel_bytes   BLOB    NOT NULL CHECK (typeof(rel_bytes) = 'blob'),
        case_key    TEXT    NOT NULL,
        abs         TEXT    NOT NULL,
        media_id    INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
        PRIMARY KEY (rel_bytes)
    );

    CREATE TABLE media_embeddings (
        media_id      INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
        model         TEXT    NOT NULL REFERENCES models(id) ON DELETE CASCADE,
        dim           INTEGER NOT NULL,
        vector        BLOB    NOT NULL CHECK (typeof(vector) = 'blob' AND length(vector) = 4 * dim),
        source_hash   TEXT    NOT NULL,
        embedded_at   INTEGER NOT NULL,
        PRIMARY KEY (media_id, model)
    );

    ALTER TABLE model_files ADD COLUMN verified_size INTEGER;
    ALTER TABLE model_files ADD COLUMN verified_mtime INTEGER;
    "#,
    ),
];

/// One step of the schema, as one entry of [`MIGRATIONS`].
#[derive(Clone, Copy)]
pub enum Migration {
    /// A SQL script, applied verbatim inside the step's own transaction.
    Sql(&'static str),
    /// A step that cannot be written as a script — a table rebuild, or a cleanup whose counts have to be
    /// reported — run inside the same kind of transaction. Saying those counts out loud is the runner's
    /// job and not the step's: `apply_migrations` (a code span; that function is private) echoes a cleanup
    /// only once its commit has landed, so nothing is announced about work that can still be rolled back.
    /// Returning an error rolls the whole step back and leaves the recorded version where it was.
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
///
/// The second field is what an upgrade tells *its own process* about itself ([`Db::upgrade_orphans`],
/// [`Db::merged_notes`]). It is not part of the file and it is not read back out of the file on demand: it
/// is this open's memory, so a second `Db::open` of the same database starts with nothing to announce.
#[derive(Debug)]
pub struct Db(
    /// The live connection, public for Task 10's whole-scan transaction.
    pub Connection,
    /// What this open's own migration steps wrote into `meta`, field by field. Read through
    /// [`Db::upgrade_orphans`] and [`Db::merged_notes`].
    Carried,
);

/// The receipts of the migration steps **this** open ran, one field per step that writes one.
///
/// A struct rather than two positional `Option<String>`s on [`Db`] because the two values are the same type
/// and mean different things: swapped, every test around either announcement would stay green while the
/// scan reported a merge as an orphan cleanup. Default is "this open migrated nothing", which is every
/// ordinary open.
#[derive(Debug, Default)]
struct Carried {
    /// The [`FK_ORPHANS_KEY`] receipt, when this open ran migration 3.
    orphans: Option<String>,
    /// The [`NOTE_MERGE_KEY`] receipt, when this open ran migration 5.
    merges: Option<String>,
}

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
    /// a scan that echoed it from then on would be a permanent one-line warning (F21 §3). Whether the
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
}

/// Apply every migration whose version is above what the file recorded, returning the receipts of the
/// steps *these* ran (see [`ORPHAN_CLEANUP_VERSION`], [`NOTE_MERGE_VERSION`] and [`Carried`]).
fn migrate(conn: &Connection) -> Result<Carried, DbError> {
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
    // The receipt travels out through `applied`, but the `?` stays where it is: enforcement has to go
    // back on before any migration error leaves this function, and a failed open keeps no handle that
    // could read the value anyway.
    let carried = applied?;
    restored?;
    Ok(carried)
}

/// The 1-based [`MIGRATIONS`] entry that removes orphan rows and writes their receipt — entry 3,
/// [`add_note_foreign_keys`].
///
/// Named because [`apply_migrations`] has to know which step's commit it reads a receipt back from.
/// Comparing the step *functions* would be the cleverer spelling and is not a dependable one (function
/// pointer comparison is environment-unpredictable), so the position is stated instead — and drift is
/// not silent: reading the key back answers `QueryReturnedNoRows` if the step that writes
/// [`FK_ORPHANS_KEY`] ever moves off this version, which every migration test in
/// `crates/np-core/tests/db.rs` would then trip over.
const ORPHAN_CLEANUP_VERSION: i64 = 3;

/// The 1-based [`MIGRATIONS`] entry that folds duplicate note rows together and writes its receipt — entry
/// 5, [`merge_duplicate_notes`].
///
/// Named for the same reason [`ORPHAN_CLEANUP_VERSION`] is, and with the same guard: reading
/// [`NOTE_MERGE_KEY`] back answers `QueryReturnedNoRows` if that step ever moves off this version or stops
/// recording its receipt, so the drift fails an open rather than silently losing the announcement.
const NOTE_MERGE_VERSION: i64 = 5;

/// Apply every migration above `from`, one transaction each, remembering a step's receipt when one of
/// those steps wrote it — and echoing each receipt **after** that step commits, never from inside it.
fn apply_migrations(conn: &Connection, from: i64) -> Result<Carried, DbError> {
    let mut carried = Carried::default();
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
        // Read back **inside** the step's own transaction, so this is exactly the value that step
        // wrote rather than a later reader's guess at it — and `query_row` has no answer for a
        // missing row, so a step reordered off [`ORPHAN_CLEANUP_VERSION`] or [`NOTE_MERGE_VERSION`], or one
        // stopped from recording its receipt, fails the open loudly instead of quietly losing the
        // announcement.
        let orphan_receipt = if version == ORPHAN_CLEANUP_VERSION {
            Some(read_receipt(&tx, FK_ORPHANS_KEY)?)
        } else {
            None
        };
        let merge_receipt = if version == NOTE_MERGE_VERSION {
            Some(read_receipt(&tx, NOTE_MERGE_KEY)?)
        } else {
            None
        };
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![version],
        )?;
        tx.commit()?;
        // The echo waits for the commit. A COMMIT can fail by itself — `SQLITE_FULL`, a WAL that
        // cannot be resumed — and take the deletes with it, so announcing from
        // [`add_note_foreign_keys`] used to say "removed N orphan row(s)" about rows that were still
        // there, and say it again on every retry: the false announcement F21 §3.1 refuses. Nothing is
        // printed anywhere before this line, so a step that rolled itself back — at any statement,
        // including this commit — is silent. That silence rests on call-site position rather than on
        // a test: no portable way to fail a COMMIT here is known (F21 §2 says so plainly), and the
        // durable receipt in `meta` is what the promise actually rests on. Both receipts are scoped to one
        // iteration, so no later step can re-echo an earlier one — and F19's merge follows the same rule for
        // the same reason: a merge announced before its commit is a merge that may not have happened.
        if let Some(value) = &orphan_receipt {
            report_orphans(&mut io::stderr(), value);
        }
        if let Some(value) = &merge_receipt {
            report_merges(&mut io::stderr(), value);
        }
        // At most one step can produce a receipt of each kind, so there is one value per field to hand back
        // — but a later step's `None` must not clear an earlier one, or the scan that did the work reports
        // nothing. Migration 4 and 5 run right after migration 3; these two pin that they do not swallow the
        // orphan receipt — `only_the_scan_that_removed_the_orphans_announces_them` (np-cli: the printed half)
        // and `a_version_two_database_upgrades_and_keeps_every_row_that_has_a_note` (this crate: the handle).
        carried.orphans = orphan_receipt.or(carried.orphans);
        carried.merges = merge_receipt.or(carried.merges);
    }
    Ok(carried)
}

/// One receipt value as its own step wrote it, read back from `meta` inside that step's transaction.
///
/// A missing row is an error and not a `None`: the step whose version [`apply_migrations`] matched promised
/// to write this key, so a silent absence means the receipt — and with it the announcement, and the proof
/// the check ran — never reached the file.
fn read_receipt(tx: &Transaction, key: &str) -> Result<String, DbError> {
    Ok(tx.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )?)
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

    // This `?` is the invariant rather than error handling for its own sake (F21 §2): the write shares
    // the transaction with the deletes above, so a receipt that cannot be written rolls the cleanup back
    // and **nothing is ever deleted unannounced**. `a_receipt_that_cannot_be_written_rolls_the_cleanup_back`
    // in `crates/np-core/tests/db.rs` refuses this one statement with a trigger and asserts the orphan rows
    // survived — reading statement order would let the next refactor move this line or soften its failure
    // and keep every other test green. Durable first, stream second: this step says nothing out loud
    // at all, because the write *is* the announcement's record. [`apply_migrations`] echoes it once
    // the commit has answered.
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![FK_ORPHANS_KEY, removed.value()],
    )?;
    Ok(())
}

/// Echo the stored orphan receipt to `out` — [`std::io::stderr`] in production — unless there is
/// nothing to say.
///
/// `receipt` is the [`FK_ORPHANS_KEY`] value exactly as the migration wrote it, so this line, the scan
/// report's field and `np scan`'s summary line render **one** string rather than recomputing the news
/// three ways; the total beside it comes from [`orphan_total`], the same reader the summary uses.
///
/// The sink is a parameter rather than a hard-wired `eprintln!` because stderr cannot be captured
/// in-process here: the standard library offers no way to replace the global handle, and this runs
/// inside [`Db::open`], before any test could redirect descriptor 2. Taking the sink lets the unit
/// tests in this module pin both halves of the promise — one line naming the total and the stored
/// counts for a real cleanup, silence for an all-zero one — so a regression in this visibility path
/// fails a test instead of going unnoticed; what they cannot pin is that the production call site
/// names stderr, nor that it runs after the commit (see [`apply_migrations`]).
///
/// A failed write is dropped rather than raised (F21 §2). It used to `.expect`, from inside the
/// migration's own transaction, so a workspace opened with descriptor 2 refusing writes had its whole
/// upgrade rolled back — every time, forever, over one line nobody could read. That panic never
/// protected the signal: the same counts are written into `meta` under [`FK_ORPHANS_KEY`] in that same
/// transaction, and the scan whose own open did the deleting echoes them
/// ([`crate::ScanReport::upgrade_orphans`]). An unprinted line is a lost courtesy; a refused upgrade is
/// the worse silence, because it hides the deletions *and* leaves the file unupgraded.
///
/// **One `np scan` run prints this sentence twice**, once here on stderr and again as the `upgrade`
/// line of its stdout summary (`crates/np-cli/src/main.rs`). That is deliberate, not a double-report
/// bug: this copy belongs to the run that did the deleting and appears even for a command that prints
/// no report, while the summary line belongs to the report a person is reading — and neither has to
/// arrive, because `meta` holds the fact either way.
fn report_orphans(out: &mut dyn io::Write, receipt: &str) {
    // The total is read out of the stored receipt rather than carried alongside it, so the stderr copy
    // cannot disagree with the two stdout renderings of the same value. All zeros stays silent: every
    // pre-F5 workspace passes through here, and "found nothing" on an ordinary upgrade is not news.
    let total = orphan_total(receipt);
    if total == 0 {
        return;
    }
    // The result is dropped on purpose, for the reason in this function's doc — this is the echo, and
    // the durable copy is committed in `meta` before this call is reached.
    let _ = writeln!(
        out,
        "note-processor: removed {} orphan row(s) while adding note foreign keys ({})",
        total, receipt,
    );
}

/// How many rows a stored [`FK_ORPHANS_KEY`] receipt announces: the sum of its labelled counts.
///
/// The reader that pairs with `OrphanCleanup::value` (a code span, not a link: that type is private and
/// rustdoc warns when public docs link to it), for every caller left holding only the stored
/// string — the scan report, `np scan`'s summary line, and the stderr echo in `report_orphans` (a code
/// span, not a link: that function is private), each of which names the total beside the counts
/// (F21 §3). Every number in the value is summed whatever its label: the spelling is this module's own
/// and its labels are pinned by the tests around [`FK_ORPHANS_KEY`], so there is nothing here to
/// validate — a fragment that will not parse as a count adds nothing rather than failing a report.
pub fn orphan_total(value: &str) -> usize {
    value
        .split(',')
        .filter_map(|part| match part.split_once('=') {
            Some((_, count)) => count.parse::<usize>().ok(),
            None => None,
        })
        .sum()
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
    // more. Round 1 needed both, because row resolution (`SELECT id FROM files WHERE rel = ?1`) and
    // `mark_stale` asked about display text once per file inside the write phase; round 2 moved those readers
    // to `rel_bytes`, so **no production statement queries either index any more** — they are maintained on
    // every insert and update and read by nothing. They stay because dropping an index from an existing
    // database is a second migration rather than a comment edit, and `the_schema_is_what_the_plan_mandates`
    // pins the shape; their removal belongs to whatever next has a reason to rebuild these tables.
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
        let rel_bytes = path_keys::rel_bytes_of_display(&row.rel);
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

/// `meta` key recording what migration 5 merged, so the deletion has a record beside the one migration 3
/// keeps for its own.
///
/// Beside [`FK_ORPHANS_KEY`] and [`PATHS_LOSSY_KEY`] for the same reason: no logger exists, and a person
/// reading a scan report weeks later has to be able to tell "this file held a duplicate pair and it was
/// folded away" apart from "this file never had one". The value is `merged=N;paths=a.md,b.md`, written
/// **even when nothing was merged** — `merged=0` is the record that the check ran (F21 §2's lesson), and N
/// counts the note rows folded into a survivor rather than the number of paths, because a path holding three
/// rows lost two of them.
pub const NOTE_MERGE_KEY: &str = "note_merges";

/// How many display spellings one [`NOTE_MERGE_KEY`] receipt names — the same eight F22 caps its loss record
/// at, for the same reason: the count is the fact and the names are only where to look.
const MERGE_NAME_CAP: usize = 8;

/// Migration 5 — the duplicate note rows version 3 left behind are merged into one row each (F19).
///
/// Why a merge and not a permanent charge: `notes.rel` carried no `UNIQUE`, so one path could hold two note
/// rows, migration 4 copied both onto one `rel_bytes`, and every scan since has answered that key with
/// the scan's reader (`crate::scan::Indexed::Duplicate`, a code span: that enum is private) — charging
/// `uncovered` forever, withholding the parse version forever,
/// re-parsing the whole corpus on every run and printing a refresh gap nobody can act on (F19 §1). The charge
/// was right while there was no exit; this is the exit.
///
/// The rule, stated rather than left to chance (F19 §2):
///
/// - **Winner = `MIN(id)`.** The oldest row is the one derived rows already hang off, so keeping it keeps the
///   corpus attached. Content loses, and that is survivable *because* the version was already withheld: the
///   next scan rewrites title, body, headings, tags, items and edges for every note from what is on disk.
/// - **Incoming edges are re-pointed to the winner, never nulled.** Other notes' prose is not re-parsed, so a
///   NULL target would lose a resolved link permanently — the distinction migration 3 drew between CASCADE
///   (an edge from a note that is gone) and SET NULL (a target that is gone) does not apply when the target
///   still exists under another id.
/// - **Nothing needs deduplicating after the re-point.** No child table carries a unique constraint —
///   `headings`, `tags`, `items` are plain and `links` has none either (see migration 2) — so moving an edge
///   onto the winner cannot collide with anything, and two edges that now look alike are two mentions of one
///   note, which is what the writer would have stored anyway. A defensive dedup here can never fire.
///
/// **The deletes are by hand, not by cascade.** F19 §2 expected the loser's derived rows to go with its row,
/// and that is the outcome; the mechanism cannot be the schema's, because [`migrate`] runs every step with
/// `PRAGMA foreign_keys = OFF` — a pragma that cannot be changed inside a transaction — and SQLite performs
/// no `ON DELETE CASCADE` while it is off (verified: deleting a parent with the pragma off leaves its
/// cascading children standing). Deleting the loser without its children would commit orphan `headings`,
/// `tags`, `items` and `links.src_note_id` rows into a file whose enforcement is back ON the moment this step
/// commits — the silent re-adoption [`add_note_foreign_keys`] exists to end. So the six statements below are
/// migration 3's shapes, spelled out: the loser's own derived rows and outgoing edges go with it, its inbound
/// edges move to the winner.
///
/// Because those deletes are the only thing between a loser row and its children, the step closes with
/// `PRAGMA foreign_key_check` like migrations 3 and 4 — see the check's own comment for why it matters more here
/// than it does anywhere else in this file.
///
/// The receipt goes into `meta` **inside this transaction** (F19 §4.1): a merge that cannot announce itself
/// does not happen. [`apply_migrations`] echoes it after the commit, never from here.
fn merge_duplicate_notes(conn: &Connection) -> Result<(), DbError> {
    // Every read happens before the first write, and each query is fully collected rather than iterated:
    // SQLite refuses a write on a connection that still has a row-producing statement open (the same
    // discipline as [`copy_files_rows`]).
    let groups = duplicate_groups(conn)?;
    let merged = groups.iter().map(|group| group.losers.len()).sum::<usize>();
    for group in &groups {
        for loser in &group.losers {
            conn.execute(
                "UPDATE links SET dst_note_id = ?1 WHERE dst_note_id = ?2",
                params![group.winner, loser],
            )?;
            conn.execute("DELETE FROM headings WHERE note_id = ?1", params![loser])?;
            conn.execute("DELETE FROM tags WHERE note_id = ?1", params![loser])?;
            conn.execute("DELETE FROM items WHERE note_id = ?1", params![loser])?;
            // Every outgoing edge, `approved` included — the same ruling migration 3 wrote into the schema,
            // where a foreign key cannot be conditional either.
            conn.execute("DELETE FROM links WHERE src_note_id = ?1", params![loser])?;
            conn.execute("DELETE FROM notes WHERE id = ?1", params![loser])?;
        }
    }

    if let Some((table, rowid, parent)) = first_violation(conn)? {
        // Migrations 3 and 4 run this check because a *rebuild* could misplace a row. Here it is the only
        // defence against the deletes above being softened: a code step runs with enforcement off, so no
        // `ON DELETE CASCADE` fires behind them and a skipped or narrowed delete leaves a `note_id` pointing at
        // a row that is gone — into a file whose enforcement is back ON the moment this transaction commits,
        // where an id can be re-adopted by the next `INSERT`. No later scan re-checks those keys, so this is
        // the one place anything would notice. Returning rolls the merge and its receipt back together, which
        // leaves the file at version four to try again rather than committing a half-merged corpus.
        //
        // One consequence worth naming: the pragma answers about the **whole file** and this step runs it
        // unconditionally, so a workspace damaged by anything else — a child row whose note is simply gone —
        // now refuses to open at the four-to-five step where version four opened it, which is deliberate,
        // pinned by `the_merge_gate_refuses_damage_that_no_merge_could_have_caused` (a file with **no pair**, so
        // narrowing this check to `merged > 0` fails there) and by
        // `the_merge_gate_refuses_a_child_row_whose_note_is_gone` (the same damage beside a pair). Its only exit
        // is the message itself (table, rowid, parent) because §7 of `docs/design-f19-note-merge.md` rules out a
        // repair command. The check is not scoped to rows this merge touched.
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

    let paths: Vec<String> = groups.iter().map(|group| group.display.clone()).collect();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![NOTE_MERGE_KEY, merge_receipt(merged, &paths)],
    )?;
    Ok(())
}

/// One byte key that holds more than one note row: the survivor, the rows folded into it, and the name the
/// receipt gives them.
struct DuplicateNotes {
    /// The surviving row's display `rel`. Names in the receipt are display text and identify no path (F20 §1)
    /// — they say where to look.
    display: String,
    /// `MIN(id)`: the oldest row, which keeps its id and therefore its derived rows (F19 §2).
    winner: i64,
    /// Every other id on the key, ascending.
    losers: Vec<i64>,
}

/// Every `rel_bytes` holding more than one note row, with its winner and losers.
///
/// Ordered by the **byte key**, which is what makes one database produce one receipt; display spellings can
/// tie where bytes do not (F20 §1), the key cannot.
///
/// Rows whose key is NULL are left alone — and today none can exist. `notes.rel_bytes` carries
/// `CHECK (typeof(rel_bytes) = 'blob')` from migration 4 onward and `typeof(NULL)` is `'null'`, so a keyless row
/// is unwritable by this program or by anything speaking plain SQL to the file; **that constraint, not this
/// clause, is what protects those rows, which is why no test pins the clause and none can be written.** It stays
/// as defence in depth for whoever changes that: `GROUP BY` gathers **every** NULL into one group, two keyless
/// notes are two unrelated files, and folding them deletes one of their note rows outright. The same reasoning
/// is in `docs/design-f19-note-merge.md` §3 step 1.
fn duplicate_groups(conn: &Connection) -> Result<Vec<DuplicateNotes>, DbError> {
    let keys: Vec<Vec<u8>> = {
        let mut statement = conn.prepare(
            "SELECT rel_bytes FROM notes
              WHERE rel_bytes IS NOT NULL
              GROUP BY rel_bytes
             HAVING COUNT(*) > 1
             ORDER BY rel_bytes",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    let mut groups = Vec::with_capacity(keys.len());
    for rel_bytes in keys {
        let ids: Vec<i64> = {
            let mut statement =
                conn.prepare("SELECT id FROM notes WHERE rel_bytes = ?1 ORDER BY id")?;
            let rows = statement.query_map(params![&rel_bytes], |row| row.get::<_, i64>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        // `HAVING COUNT(*) > 1` already promised at least two ids here, so `losers` is never empty; the
        // pattern is total rather than an `expect`, because arriving with one id must merge nothing instead
        // of deleting somebody's only note row.
        let Some((winner, losers)) = ids.split_first() else {
            continue;
        };
        groups.push(DuplicateNotes {
            display: survivor_display(conn, *winner, &rel_bytes)?,
            winner: *winner,
            losers: losers.to_vec(),
        });
    }
    Ok(groups)
}

/// The spelling one merged path is named by in the receipt.
///
/// `notes.rel` is a display column and nullable (F13 §4), so a hand-made row can lack it; the byte key's own
/// lossy spelling is the fallback, because the receipt points at a path rather than identifying one. Without it
/// such a merge would record `merged=1;paths=` — a fold whose path cannot be named. Pinned by
/// `a_merged_pair_with_no_display_text_is_named_by_the_lossy_spelling_of_its_key` in `crates/np-core/tests/db.rs`.
fn survivor_display(conn: &Connection, winner: i64, rel_bytes: &[u8]) -> Result<String, DbError> {
    let stored: Option<Option<String>> = conn
        .query_row(
            "SELECT rel FROM notes WHERE id = ?1",
            params![winner],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    Ok(stored
        .flatten()
        .unwrap_or_else(|| String::from_utf8_lossy(rel_bytes).into_owned()))
}

/// The stored [`NOTE_MERGE_KEY`] spelling: `merged=N;paths=a.md,b.md`, the names capped at
/// [`MERGE_NAME_CAP`] and already in key order from [`duplicate_groups`].
fn merge_receipt(merged: usize, paths: &[String]) -> String {
    let named: Vec<&str> = paths
        .iter()
        .take(MERGE_NAME_CAP)
        .map(String::as_str)
        .collect();
    format!("merged={merged};paths={}", named.join(","))
}

/// How many note rows a stored [`NOTE_MERGE_KEY`] receipt says were merged away.
///
/// The reader that pairs with `merge_receipt` (a code span, not a link: that function is private), for
/// every caller left holding only the stored string — the scan report's field and the stderr echo here. Like
/// [`orphan_total`] it parses its own format rather than carrying a second number around: one value, three
/// readers, no way for them to disagree. A fragment that will not parse as a count adds nothing.
pub fn merge_total(value: &str) -> usize {
    value
        .split(';')
        .find_map(|part| part.strip_prefix("merged="))
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0)
}

/// Echo the stored merge receipt to `out` — [`std::io::stderr`] in production — unless nothing was merged.
///
/// The same shape, sink and dropped-write rule as [`report_orphans`], for the same reasons: stderr cannot be
/// captured in-process because this runs inside [`Db::open`] before a test could redirect descriptor 2, so the
/// sink is a parameter and this module's two tests pin both halves through it —
/// `the_merge_receipt_is_echoed_once_rows_have_been_folded` (one line naming the total beside the stored
/// receipt) and `a_merge_that_folded_nothing_writes_no_line` (silence at zero, which is what keeps every
/// ordinary command quiet while `merged=0` still lands in `meta` as proof the check ran). What they cannot pin
/// is that the production call site names stderr, nor that it runs after the commit — see [`apply_migrations`],
/// where the same no-portable-COMMIT-failure argument as F21 §2 applies. And a failed write must not roll back
/// (or abort) an upgrade whose deletions are durable in `meta` under [`NOTE_MERGE_KEY`].
fn report_merges(out: &mut dyn io::Write, receipt: &str) {
    let merged = merge_total(receipt);
    if merged == 0 {
        return;
    }
    let _ = writeln!(
        out,
        "note-processor: merged {} duplicate note row(s) into the oldest row for their path ({})",
        merged, receipt,
    );
}

/// Record a file that is there now — or bring a known row back to `state = 'ok'`.
///
/// A path re-upserted after [`mark_stale`] rejoins the live set on the row it already has, so its `id` never
/// changes — which is what keeps the note's derived rows attached to it. `hash`, `size`, `mtime`, `abs` and
/// `last_seen` are refreshed from the snapshot every time, because a scan that saw a file is a sighting of it.
///
/// **Both byte keys are written here, and both travel on the snapshot.** [`crate::path_keys`] derived them
/// from the walk's OS bytes and this statement binds what it is handed rather than inventing a rule of its
/// own. Migration 4 made `rel_bytes` / `case_bytes` `NOT NULL UNIQUE` and dropped `case_key UNIQUE`, so a write
/// that supplied no key would fail with `SQLITE_CONSTRAINT_NOTNULL` on the first file of every scan, and the
/// constraint that used to report P3's case pair would be gone. Deriving them from the *display* text instead
/// is the bug F13 exists to end: twins spell themselves identically, so the second one would resolve onto its
/// twin's row and overwrite it with no constraint firing at all.
///
/// The row is resolved by `id`, in two statements, where this function used to carry one
/// `ON CONFLICT (rel) DO UPDATE` arm. That arm cannot survive the migration: `rel TEXT PRIMARY KEY` was the
/// only reason `ON CONFLICT (rel)` matched anything, migration 4 gives `rel` no unique constraint at all, and
/// SQLite answers a non-matching conflict target at *prepare* time — so an arm kept out of habit does not fail
/// on a lossy name, it fails on the first ordinary ASCII file and takes every scan down with it. The lookup
/// that replaced it asks about `rel_bytes`, which is F13 §3's "the conflict target moves to `rel_bytes`" in
/// the shape this function can prepare: two twins now resolve to two different rows.
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
pub fn upsert_file(db: &Db, s: &Snapshot, abs: &Path, now: i64) -> Result<bool, DbError> {
    // The row's **previous digest travels out of this function** (design-a4 §3 and §4a). The caller decides
    // whether a note has to be re-parsed, and an identity-resolved move — same file, new key, different bytes —
    // is a case no `Change` variant names. Reading it here costs nothing, because this statement already runs
    // once per file, and the compare sits in the same transaction as the write, so no other writer can slip a
    // digest in between the two.
    let existing: Option<(i64, String)> =
        db.0.prepare_cached("SELECT id, hash FROM files WHERE rel_bytes = ?1")?
            .query_row(params![&s.rel_bytes], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;

    // `abs` is stored as TEXT, so a path that is not valid UTF-8 arrives here lossy.
    let abs_text = abs.to_string_lossy();
    // No filesystem sizes a file near `i64::MAX`, but the column is SQLite's INTEGER
    // and a bare `as` cast would wrap such a size into a negative one.
    let size = i64::try_from(s.size).unwrap_or(i64::MAX);

    // Written on both arms: identity is only as current as the last walk, and a file restored or recreated
    // under the same name deserves its new one rather than the stale id stored since before the restore.
    let hash_changed = match existing {
        Some((id, ref old_hash)) => {
            db.0.prepare_cached(
                "UPDATE files
                    SET rel_bytes = ?2, case_bytes = ?3, case_key = ?4, abs = ?5, hash = ?6, size = ?7,
                        mtime = ?8, state = 'ok', last_seen = ?9, file_id = ?10
                  WHERE id = ?1",
            )?
            .execute(params![
                id,
                &s.rel_bytes,
                &s.case_bytes,
                s.case_key,
                abs_text,
                s.hash,
                size,
                s.mtime,
                now,
                &s.file_id
            ])?;
            *old_hash != s.hash
        }
        None => {
            db.0.prepare_cached(
                "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state,
                                    last_seen, file_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'ok', ?9, ?10)",
            )?
            .execute(params![
                &s.rel_bytes,
                &s.case_bytes,
                s.rel,
                s.case_key,
                abs_text,
                s.hash,
                size,
                s.mtime,
                now,
                &s.file_id
            ])?;
            // A brand-new row reports **no change**. There is no previous digest to have changed, and the
            // caller's `Change::Added` already re-parses it — reporting true here would buy a second parse and
            // a lie about what was compared.
            false
        }
    };
    Ok(hash_changed)
}

/// Insert the note row for a file that has none yet, returning its freshly minted id.
///
/// This is the program's **only writer of `notes.rel_bytes`**, and it lives here rather than beside the rest
/// of `scan::store_note` so that "bind this note key as TEXT" is not a call any scan-side site can
/// write (F13 §4). That matters more here than anywhere else in the schema: `notes.rel` is plain TEXT and stays
/// nullable, so an insert that named only the display column would succeed — leaving a note whose byte key is
/// NULL, which matches nothing the re-keyed join, cascade and lookup compare on bytes. A note row silently
/// detached from its file is F13 reproduced by the fix, starting with the first new note.
///
/// The id is minted here and returned because every derived row hangs off it; the update arm of `store_note`
/// stays in `scan.rs` because it writes no key at all.
pub fn insert_note(
    db: &Db,
    path: &RelPath,
    title: &str,
    title_source: &str,
    encoding: &str,
    word_count: i64,
    body: &str,
) -> Result<i64, DbError> {
    db.0.prepare_cached(
        "INSERT INTO notes (rel_bytes, rel, title, title_source, encoding, word_count, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?
    .execute(params![
        &path.rel_bytes,
        path.rel,
        title,
        title_source,
        encoding,
        word_count,
        body
    ])?;
    Ok(db.0.last_insert_rowid())
}

/// Every `notes.id` stored under one byte key, ascending by id.
///
/// **This reads every row on purpose; there is no `LIMIT 1`.** `notes.rel_bytes` carries no `UNIQUE`: a
/// version-3 database could hold two note rows for one path (`notes.rel` was never unique) and migration 4
/// copies both onto one key (F13 §7). Handing back one answer from that population is either a silent pick,
/// which rewrites a note's whole derived set with another note's, or an `ExecuteMultipleValues` error, which
/// makes a legacy workspace impossible to scan forever. Both are refused by returning the count for the caller
/// to act on: one id is the normal path, none means "no row yet", and more than one is an identity conflict
/// the scan reports and leaves alone.
///
/// The key arrives as bytes because that is the only spelling with one answer per file; matching on display
/// text would let a twin's note answer for it (F13 §3).
pub fn note_ids(db: &Db, rel_bytes: &[u8]) -> Result<Vec<i64>, DbError> {
    let mut statement =
        db.0.prepare_cached("SELECT id FROM notes WHERE rel_bytes = ?1 ORDER BY id")?;
    let rows = statement.query_map(params![rel_bytes], |row| row.get::<_, i64>(0))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

/// Move a file's registry row from `from` to `to`, keeping everything that makes its note the
/// same note.
///
/// Both paths arrive as a [`RelPath`], because one statement has to write both spellings: the byte key is what
/// the row is matched and stored on, and the display pair is what the catalog renders. Neither may be derived
/// from the other here — a renamed lossy name does not round-trip through text.
///
/// The row is found by `id`, like [`upsert_file`]'s resolution, which was reached through the **byte** key:
/// `rel` stopped being unique in migration 4, so matching the lookup on it would rename every row sharing one
/// display spelling. Both byte keys are written in the same statement as the display pair, so the four columns
/// cannot diverge.
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
pub fn apply_move(db: &Db, from: &RelPath, to: &RelPath, now: i64) -> Result<(), DbError> {
    // The lookup keeps its own cached statement, and the `MissingRel` mapping below still reads the
    // same `QueryReturnedNoRows` it always did — caching the prepare changes how the statement is
    // made, not what a miss reports. It matches on bytes: two rows can spell one display `rel`, and a
    // rename keyed on that text would drag both notes through this statement.
    let (id, abs): (i64, String) =
        db.0.prepare_cached("SELECT id, abs FROM files WHERE rel_bytes = ?1")?
            .query_row(params![&from.rel_bytes], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => DbError::MissingRel {
                    rel: from.rel.clone(),
                },
                other => DbError::Sqlite(other),
            })?;
    // `rebase_abs` is display work — its output is the display `abs` column (F13 §3) — so it reads the two
    // display spellings, not the bytes.
    let new_abs = rebase_abs(&abs, &from.rel, &to.rel).ok_or_else(|| DbError::BadAbs {
        rel: from.rel.clone(),
        abs,
    })?;

    let to_case = path_keys::case_fold(&to.rel_bytes);
    db.0.prepare_cached(
        "UPDATE files
            SET rel_bytes = ?2, case_bytes = ?3, rel = ?4, case_key = ?5, abs = ?6, last_seen = ?7,
                state = 'ok'
          WHERE id = ?1",
    )?
    .execute(params![
        id,
        &to.rel_bytes,
        to_case,
        to.rel,
        path_keys::display_fold(&to.rel),
        new_abs,
        now
    ])?;

    // Matched on the **new** bytes: `notes.rel_bytes` rode the update above through its cascade, so the old
    // spelling no longer names this note. Display text is what is written, because `notes.rel` is a display
    // column and the catalog renders it verbatim.
    db.0.prepare_cached("UPDATE notes SET rel = ?2 WHERE rel_bytes = ?1")?
        .execute(params![&to.rel_bytes, to.rel])?;
    Ok(())
}

/// Mark a path as gone without destroying anything.
///
/// The row and every child of it stay: the note keeps its headings, tags, links and items so
/// it can rejoin with its identity if the file comes back, and so the catalog can say "stale"
/// instead of forgetting it ever existed (spec §2). A path that is not in the registry changes
/// nothing, which is correct — there is no row to mark.
///
/// Matched on `rel_bytes`, and taking bytes from the caller is what makes that possible (F13 §3): keyed on
/// display text, deleting one twin marked **both** stale, so the catalog lost a note that was sitting there
/// readable and nothing reported it.
pub fn mark_stale(db: &Db, rel_bytes: &[u8]) -> Result<(), DbError> {
    db.0.prepare_cached("UPDATE files SET state = 'stale' WHERE rel_bytes = ?1")?
        .execute(params![rel_bytes])?;
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
/// The join is on `rel_bytes`, not the display `rel` (F13 §3): once two live rows may spell themselves
/// identically, joining on that text is a cross product — one edge counted twice, and a stale twin able to
/// answer for a live one, so the count and the list printed under it could disagree.
pub(crate) const LIVE_EDGE: &str =
    "EXISTS (SELECT 1 FROM notes dn JOIN files df ON df.rel_bytes = dn.rel_bytes \
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

/// What one call to [`write_chunks_for_note`] touched, as counts of **rows touched**.
///
/// Each field counts rows whose stored content changed, not chunks that were looked at: a chunk already on
/// file exactly as this build would write it appears in none of the three numbers. That is the shape §5's
/// warm pass is about — an unchanged note answers [`ChunkWrite::default()`] — and a report claiming work it
/// did not do is the lie F21 §3 refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChunkWrite {
    /// Ordinals the note had no row for, so a fresh `chunks.id` was minted.
    pub inserted: usize,
    /// Rows rewritten in place — same `id`, new content. Includes a row whose text did not move but whose
    /// stored `source_hash` or `chunker` lagged behind what this call writes; see
    /// [`write_chunks_for_note`] for why that restamp is the cheap path's precondition rather than waste.
    pub updated: usize,
    /// Trailing rows deleted because the note holds fewer chunks than it did. Their vectors go with them
    /// through `ON DELETE CASCADE`, which is the only cascade this module provokes from here.
    pub deleted: usize,
}

/// Replace one note's chunk rows with `chunks`, by **per-ordinal diff**.
///
/// This is the program's only writer of `chunks`, and it diffs instead of delete-and-reinserting for one
/// reason (design-m2-embeddings §5's third bullet): `embeddings.chunk_id` cascades off `chunks.id`, so the
/// derived-row pattern used elsewhere (`crate::scan`'s delete-then-insert for headings, tags, items and
/// links) would cascade away every vector of an edited note. "Re-embed only what moved" therefore depends
/// on row *identity* surviving an edit, not merely on the final rows being right. Hence:
///
/// - rows are addressed by `UNIQUE (note_id, ordinal)` — the pair [`chunk_state`] reads back, and the
///   constraint `one_note_cannot_hold_two_chunks_at_the_same_ordinal` probes;
/// - an ordinal that already has a row is rewritten with `UPDATE … WHERE id = ?`, so its `id`, and every
///   vector hanging off it, is unchanged;
/// - ordinals the note no longer has (`ordinal >= chunks.len()`) are deleted, and only those.
///
/// Two facts go on every row this function writes: `chunker = CHUNKER_VERSION`, so a bump makes old rows
/// *identifiable* rather than presumed stale (§4), and `source_hash = note_digest` — the digest of the
/// **note** bytes the chunks were cut from. That is one level coarser than `embeddings.source_hash` (§3's
/// two levels: a heading edit in paragraph 9 leaves chunk 2's own text alone, and that fact belongs to the
/// vector row, never here). Stamping the digest onto rows whose text did not move is deliberate rather
/// than sloppiness: it is what makes the cheap path below reachable on the *next* scan, which is §5's "an
/// unchanged note costs one indexed lookup per scan". Its cost is one `UPDATE` per unchanged row on the
/// one scan that notices an edit, and it touches no vector — this function never writes `embeddings`.
///
/// `Chunk::text` travels as bytes (`&[u8]`), never as `String` or `&str`: a text bind arrives at SQLite as
/// TEXT, migration 7's `CHECK (typeof(text) = 'blob')` refuses it, and BLOB is an affinity rather than a
/// constraint — F13 §4's split-type bug, kept armed by `a_string_bind_is_refused_from_chunks_text`.
///
/// ## The cheap path
///
/// When the note holds exactly `chunks.len()` rows stamped with this digest *and* this chunker, **and no other
/// rows at all**, the stored set is what this call would write: nothing else writes `chunks`,
/// [`crate::chunk::chunk`] is deterministic, and a row's stamp means it was cut from bytes hashing to that
/// digest by this chunker. Two counts answer that in one indexed pass and nothing is written.
///
/// The second count is not paranoia. An **emptied** note — body cleared to blank lines, which [`crate::chunk::chunk`]
/// returns no chunks for — wants zero rows and can hold three stamped with an older digest; against the stamp
/// alone both sides read `0 == 0`, so a single-count predicate returns early and leaves those rows **and their
/// vectors** on the table permanently, every later scan re-deriving the same empty list and taking the same arm.
/// That is a report claiming work it did not do, which is the F21 §3 lie this module's own doc refuses.
///
/// ## Transaction
///
/// None of its own: chunk writes belong to whichever transaction the caller is in, which for the scan is
/// its single write transaction — §5: "Chunk writes stay in the transaction — they are cheap and must be
/// atomic with the note rows". A failure mid-diff therefore rolls back alongside the note rows it
/// accompanied instead of leaving a note half-rechunked.
pub fn write_chunks_for_note(
    db: &Db,
    note_id: i64,
    chunks: &[Chunk],
    note_digest: &str,
) -> Result<ChunkWrite, DbError> {
    // The row count as SQLite's INTEGER. Nothing here is near `i64::MAX` chunks, but a bare `as` would wrap
    // such a length into a negative number that matches nothing (the same reasoning as [`upsert_file`]'s size).
    let wanted = i64::try_from(chunks.len()).unwrap_or(i64::MAX);
    // Two counts, not one. `stamped` alone answers "does every row I would keep carry this stamp?" but says
    // nothing about rows that should be **gone**: an emptied note wants zero chunks and holds three, both
    // numbers read 0 == 0 against today's stamp, and a single-count predicate would return early with those
    // three rows — and their vectors — still on the table, forever, because every later scan derives the same
    // empty list and takes the same arm. `total` is what makes the cheap path a proof rather than a hope: skip
    // only when the stamped rows are as many as we want AND there are no others.
    let (stamped, total): (i64, i64) = db
        .0
        .prepare_cached(
            "SELECT COALESCE(SUM(CASE WHEN source_hash = ?2 AND chunker = ?3 THEN 1 ELSE 0 END), 0),
                    COUNT(*)
               FROM chunks WHERE note_id = ?1",
        )?
        .query_row(params![note_id, note_digest, CHUNKER_VERSION], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    if stamped == wanted && total == wanted {
        return Ok(ChunkWrite::default());
    }

    let stored = stored_chunks(db, note_id)?;
    let mut written = ChunkWrite {
        // The tail first. A chunk the note no longer has is gone, and its vectors cascading away with it is
        // the correct loss — unlike the cascade a blanket delete would cause for the rows that stay.
        deleted: db
            .0
            .prepare_cached("DELETE FROM chunks WHERE note_id = ?1 AND ordinal >= ?2")?
            .execute(params![note_id, wanted])?,
        inserted: 0,
        updated: 0,
    };

    for chunk in chunks {
        let ordinal = i64::from(chunk.ordinal);
        // Offsets are `usize` in Rust and `INTEGER` here; the same unreachable-but-refusing-to-wrap guard
        // as `wanted` above.
        let byte_start = i64::try_from(chunk.byte_start).unwrap_or(i64::MAX);
        let byte_end = i64::try_from(chunk.byte_end).unwrap_or(i64::MAX);
        // Bytes, not `&chunk.text`: see this function's doc for the CHECK that refuses a text bind.
        let text: &[u8] = chunk.text.as_bytes();

        // Binary search, not a scan: [`stored_chunks`] returns rows `ORDER BY ordinal`, which is what makes
        // the ordinals searchable and keeps a note with hundreds of chunks off quadratic time. Rows deleted
        // above all carry `ordinal >= wanted`, so nothing here can look up a row that was just removed.
        match stored.binary_search_by_key(&ordinal, |row| row.ordinal) {
            Ok(index) => {
                let row = &stored[index];
                if row.text != text
                    || row.byte_start != byte_start
                    || row.byte_end != byte_end
                    || row.chunker != CHUNKER_VERSION
                    || row.source_hash != note_digest
                {
                    // Matched on `id` — the whole point of the diff — with `note_id` in the predicate so a
                    // row read for another note could never be rewritten by a stale index position.
                    written.updated +=
                        db.0.prepare_cached(
                            "UPDATE chunks
                                SET chunker = ?3, text = ?4, byte_start = ?5, byte_end = ?6,
                                    source_hash = ?7
                              WHERE id = ?1 AND note_id = ?2",
                        )?
                        .execute(params![
                            row.id,
                            note_id,
                            CHUNKER_VERSION,
                            text,
                            byte_start,
                            byte_end,
                            note_digest
                        ])?;
                }
            }
            Err(_) => {
                // No row at this ordinal, so the insert cannot collide with `UNIQUE (note_id, ordinal)`.
                written.inserted +=
                    db.0.prepare_cached(
                        "INSERT INTO chunks
                             (note_id, ordinal, chunker, text, byte_start, byte_end, source_hash)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    )?
                    .execute(params![
                        note_id,
                        ordinal,
                        CHUNKER_VERSION,
                        text,
                        byte_start,
                        byte_end,
                        note_digest
                    ])?;
            }
        }
    }
    Ok(written)
}

/// One stored `chunks` row, in the shape [`write_chunks_for_note`]'s diff compares against.
struct StoredChunk {
    /// `chunks.id`, which the update matches on so a rewrite cannot mint a new row and cascade vectors.
    id: i64,
    ordinal: i64,
    chunker: i64,
    byte_start: i64,
    byte_end: i64,
    /// Read back as bytes because the column is a BLOB holding note bytes (F13 §4): reading it into a
    /// `String` would fail on exactly the lossy notes the column exists to hold.
    text: Vec<u8>,
    source_hash: String,
}

/// One note's chunk rows, ascending by ordinal.
///
/// The `ORDER BY` is load-bearing twice over: it is what lets [`write_chunks_for_note`] binary-search by
/// ordinal, and it is the reason [`chunk_state`] returns texts in [`crate::chunk`]'s own order rather than
/// in whatever order a later planner prefers — the same argument `rebuild_with_keys` (a code span; that
/// function is private) makes about rowids.
fn stored_chunks(db: &Db, note_id: i64) -> Result<Vec<StoredChunk>, DbError> {
    let mut statement = db.0.prepare_cached(
        "SELECT id, ordinal, chunker, byte_start, byte_end, text, source_hash
           FROM chunks WHERE note_id = ?1 ORDER BY ordinal",
    )?;
    let rows = statement.query_map(params![note_id], |row| {
        Ok(StoredChunk {
            id: row.get(0)?,
            ordinal: row.get(1)?,
            chunker: row.get(2)?,
            byte_start: row.get(3)?,
            byte_end: row.get(4)?,
            text: row.get(5)?,
            source_hash: row.get(6)?,
        })
    })?;
    let mut rows_out = Vec::new();
    for row in rows {
        rows_out.push(row?);
    }
    Ok(rows_out)
}

/// What one note's stored chunks say about themselves: the note digest they were cut from, and their text
/// by ordinal.
///
/// The reader §5's staleness rule needs. A caller compares a fresh note digest against the first element to
/// decide whether this note needs any chunk work at all, and compares each stored text against the chunk it
/// just cut to decide which ordinals moved; both answers come from one indexed pass over one note's rows.
///
/// - **Ordering.** Texts come back ascending by `ordinal`, which is [`crate::chunk`]'s own order. The
///   `ORDER BY` lives in the statement rather than being left to the planner, for the reason
///   `rebuild_with_keys` (a code span; that function is private) gives about rowid order.
/// - **The digest arm is `Some` only when there is one answer.** Every row must carry the same
///   `source_hash`, and every row must carry the current [`CHUNKER_VERSION`]; otherwise these rows support
///   no single statement and the caller has to re-chunk. Folding the chunker in follows from this signature
///   having no third slot — and rows cut by an older chunker must never read as current, since §4 stamps
///   per row precisely so a bump invalidates them. A note with no rows has no digest either: nothing has
///   been cut from it yet, which is the same answer as "stale" and costs the caller one
///   [`write_chunks_for_note`] call.
/// - **A row this program cannot have written.** `chunks.ordinal` is a SQLite INTEGER, so a hand-edited file
///   could hold a value above `u32::MAX`. [`crate::chunk::chunk`] refuses to mint one, so the honest reading
///   is "not ours, not current": the digest arm comes back `None` and that row is reported at `u32::MAX`,
///   which keeps it visible instead of silently dropping a row from the list.
///
/// Texts are `Vec<u8>` because the column holds note bytes, not a promise about UTF-8 (§3).
// The tuple stays spelled out: the caller destructures `(digest, texts)`, and a public alias for
// `Vec<(u32, Vec<u8>)>` would name one fact twice without saying anything the signature does not already.
// clippy's heuristic counts nesting rather than meaning.
#[allow(clippy::type_complexity)]
/// Still **no production caller**: the scan decides currency from [`chunk_digest`], which reads stamps only, and
/// this function's texts exist for the embed round that compares a chunk's stored bytes against what it would
/// re-embed. `None` means "no single digest describes these rows", which a caller must not read as "no chunks".
pub fn chunk_state(
    db: &Db,
    note_id: i64,
) -> Result<(Option<String>, Vec<(u32, Vec<u8>)>), DbError> {
    let rows = stored_chunks(db, note_id)?;
    // One pass decides both arms: a row that disagrees with the first row's stamp, or carries another
    // build's chunker, or cannot be addressed as a `u32` ordinal, sets `current` to false and stays in the
    // text list.
    let digest = rows.first().map(|row| row.source_hash.clone());
    let mut current = !rows.is_empty();
    let mut texts = Vec::with_capacity(rows.len());
    for row in rows {
        // `u32::MAX` rather than a wrap, and the ordinal is checked *before* it is narrowed: see this
        // function's doc. A row that cannot be addressed also decides the digest arm, so no caller can
        // mistake such a set for current.
        let addressable = u32::try_from(row.ordinal).is_ok();
        let ordinal = u32::try_from(row.ordinal).unwrap_or(u32::MAX);
        current &= addressable
            && row.chunker == CHUNKER_VERSION
            && Some(row.source_hash.as_str()) == digest.as_deref();
        texts.push((ordinal, row.text));
    }
    Ok((
        if current {
            // `digest` is `Some` here: `current` starts false for an empty row set.
            digest
        } else {
            None
        },
        texts,
    ))
}

/// The note digest one note's chunk rows agree on, **without reading their text**.
///
/// This is the scan's per-note decision (design §5: "an unchanged note costs one indexed lookup per scan"), and
/// the stamps are all that decision uses. [`chunk_state`] answers the same question while materialising a `Vec<u8>`
/// per chunk — on a warm scan over a real vault that is roughly the whole corpus read out of SQLite and thrown
/// away, which is a different cost class from an index probe and one the suite cannot see, because the perf fixture's
/// notes are ~350 bytes and cut one chunk each. Same rules as [`chunk_state`], same `None` arms: no rows, rows that
/// disagree, a row behind another build's chunker, or an ordinal too wide to address.
///
/// No `ORDER BY`: nothing here is searched by position, and the answer is order-independent.
pub fn chunk_digest(db: &Db, note_id: i64) -> Result<Option<String>, DbError> {
    let mut statement =
        db.0.prepare_cached("SELECT ordinal, chunker, source_hash FROM chunks WHERE note_id = ?1")?;
    let mut rows = statement.query_map(params![note_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    // Folded as it reads, because the first row's digest is the only candidate: any row that disagrees with it
    // decides `None`, so a caller never has to know how many rows disagreed.
    let mut digest: Option<String> = None;
    let mut current = true;
    for row in &mut rows {
        let (ordinal, chunker, source_hash) = row?;
        let addressable = u32::try_from(ordinal).is_ok();
        match &digest {
            None => digest = Some(source_hash),
            Some(first) => current &= first == &source_hash,
        }
        current &= addressable && chunker == CHUNKER_VERSION;
    }
    // An empty row set is `None` for the same reason [`chunk_state`] says: no rows is not "current".
    Ok(if current { digest } else { None })
}

/// Store one vector for one chunk under one model, idempotently.
///
/// **`chunk_text_digest` is the SHA-256 of that chunk's own text — never `files.hash`.** The two levels look
/// interchangeable (both `String`, both in scope during a scan) and are not: `chunks.source_hash` holds the note
/// digest, so after a `CHUNKER_VERSION` bump every row is restamped to an unchanged `files.hash` while its text may
/// have been re-cut — and a vector stamped with the note digest would then claim currency for bytes nobody has
/// embedded since. Pinned by `a_vector_stamped_with_the_note_digest_is_not_found_by_a_chunk_text_question`.
///
/// `INSERT … ON CONFLICT (chunk_id, model) DO UPDATE` rather than a plain `INSERT`, and §5's third bullet is
/// the reason: F15 *expects* its retry, so "this write already happened" is an ordinary event rather than a
/// bug. A plain insert into `PRIMARY KEY (chunk_id, model)` answers that retry with
/// `SQLITE_CONSTRAINT_PRIMARYKEY`, and because one `?` propagates out of the scan's write pass, the whole
/// scan's indexing work would roll back over one repeated vector — a provider problem turned into data
/// loss, which §5 forbids twice over ("Provider failure must not be data loss; neither may our own conflict
/// handling be").
///
/// The conflict target is the **pair**, so re-embedding under a second model cannot disturb the first row —
/// §3's "switching providers then costs a re-embed and nothing else", pinned at the schema by
/// `two_models_hold_vectors_for_one_chunk` and at the writer by `upsert_vector_keeps_both_models_rows`. `dim`, `source_hash` and `embedded_at` travel in the
/// update arm as well: a retry computed from chunk text that has since moved, or under a model whose answer
/// changed, must leave the row stating what it now holds rather than keeping the older facts beside a new
/// vector.
///
/// `bytes` is the little-endian f32 payload — [`crate::embed::Vector::to_bytes`] writes it — bound as bytes,
/// which is the only spelling migration 7's `CHECK (typeof(vector) = 'blob' …)` accepts. Nothing here
/// re-checks `bytes.len() == 4 * dim`: the `CHECK` does it at the write, and `Vector::from_bytes` is its
/// twin on the read side (§3). An unknown `model` is refused by the foreign key rather than looked up here,
/// which is why `models` must be written before any vector naming it (§3).
pub fn upsert_vector(
    db: &Db,
    chunk_id: i64,
    model: &str,
    dim: usize,
    bytes: &[u8],
    chunk_text_digest: &str,
    now: i64,
) -> Result<(), DbError> {
    // `models.dim` is an INTEGER; the guard is `upsert_file`'s, for a model that does not exist.
    let dim = i64::try_from(dim).unwrap_or(i64::MAX);
    db.0.prepare_cached(
        "INSERT INTO embeddings
             (chunk_id, model, dim, vector, source_hash, embedded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (chunk_id, model) DO UPDATE
            SET vector = excluded.vector, dim = excluded.dim,
                source_hash = excluded.source_hash, embedded_at = excluded.embedded_at",
    )?
    .execute(params![chunk_id, model, dim, bytes, chunk_text_digest, now])?;
    Ok(())
}

/// Drop every vector stored for one chunk, under every model, leaving the chunk row itself alone.
///
/// The cascade already covers the case where the **chunk** goes away. This is the other case: the chunk row
/// stays — keeping its ordinal and its `chunks.id`, which is what [`write_chunks_for_note`] exists to
/// preserve — while its vectors do not. Deleting nothing is not an error: a chunk that was never embedded
/// has no rows, and "no vectors behind it" is a fact rather than a missing thing.
///
/// Separate from [`write_chunks_for_note`] on purpose, because the two tables are written in different
/// transactions (§5: chunk writes ride the scan's transaction, vector batches commit per batch outside it).
pub fn delete_vectors_for_chunk(db: &Db, chunk_id: i64) -> Result<(), DbError> {
    db.0.prepare_cached("DELETE FROM embeddings WHERE chunk_id = ?1")?
        .execute(params![chunk_id])?;
    Ok(())
}

/// One stored vector's currency stamp under one model: the chunk it belongs to, and the digest of the bytes
/// that were handed to the model to make it.
///
/// Only those two fields, because they are the whole answer to "is this vector still current": §6's rule is
/// that `embeddings.source_hash` holds the digest of **the bytes the model read** (backlog D4), so currency is
/// one string comparison and needs neither the vector nor the timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorStamp {
    /// `embeddings.chunk_id` — the row whose text produced this vector.
    pub chunk_id: i64,
    /// `embeddings.source_hash`: digest of the prefixed text the model was given, written by
    /// [`crate::embed_run`]. Not `chunks.source_hash` (the note digest) and not `files.hash`.
    pub source_hash: String,
}

/// Every vector stored under one model id, ascending by chunk.
///
/// The `model` filter is not optional and that is the whole reason this reader exists
/// (`docs/design-m2-local-embeddings.md` §6: "every read filters on an explicit model id … no such reader
/// exists yet"): `PRIMARY KEY (chunk_id, model)` lets two models hold a vector for one chunk, and comparing
/// one model's stamps against another's vectors would call a stale row current — or the other way round,
/// re-embedding forever. A caller that wants "all vectors" can ask twice with two ids; there is no query here
/// that mixes them.
///
/// Ordered by `chunk_id` so a caller building a map gets one deterministic pass and a report whose counts do
/// not depend on the planner's mood.
pub fn vectors_for(db: &Db, model: &str) -> Result<Vec<VectorStamp>, DbError> {
    let mut statement = db.0.prepare_cached(
        "SELECT chunk_id, source_hash FROM embeddings WHERE model = ?1 ORDER BY chunk_id",
    )?;
    let rows = statement.query_map(params![model], |row| {
        Ok(VectorStamp {
            chunk_id: row.get(0)?,
            source_hash: row.get(1)?,
        })
    })?;
    let mut stamps = Vec::new();
    for row in rows {
        stamps.push(row?);
    }
    Ok(stamps)
}

/// One chunk the catalog can still show a reader: its id and its stored text.
///
/// The `files.state = 'ok'` join is §6's liveness fence (backlog D3). A note whose file is gone keeps its row
/// with `state = 'stale'` and every child row intact — [`mark_stale`] says so in its own words — so an
/// un-fenced read over `chunks` is the first production reader that computes vectors for a deleted file's text
/// and reports them current. Joining through `notes.rel_bytes = files.rel_bytes` rather than `rel` because the
/// byte key is the identity (F13 §4, migration 4): an ASCII-lossy display spelling could fold two paths into
/// one.
///
/// Ordered by `chunks.id`, which is the order [`crate::embed_run`] batches in — deterministic batching is what
/// makes a resumed run produce the same rows as an uninterrupted one.
pub fn live_chunks(db: &Db) -> Result<Vec<LiveChunk>, DbError> {
    let mut statement = db.0.prepare_cached(
        "SELECT c.id, c.text
           FROM chunks c
           JOIN notes n ON n.id = c.note_id
           JOIN files f ON f.rel_bytes = n.rel_bytes
          WHERE f.state = 'ok'
          ORDER BY c.id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(LiveChunk {
            id: row.get(0)?,
            text: row.get(1)?,
        })
    })?;
    let mut chunks = Vec::new();
    for row in rows {
        chunks.push(row?);
    }
    Ok(chunks)
}

/// One chunk row as [`live_chunks`] reads it.
pub struct LiveChunk {
    /// `chunks.id`, the key `embeddings.chunk_id` references.
    pub id: i64,
    /// The stored BLOB. Still bytes, for the reason `Owed::input` in [`crate::embed_run`] gives for its copy of
    /// these bytes: this column holds note text
    /// and makes no UTF-8 promise.
    pub text: Vec<u8>,
}

/// One row of `models` (migration 7), read as the columns hold them.
///
/// `dim` and `installed_at` are `i64` rather than `u16`/a timestamp type on purpose: SQLite's `INTEGER`
/// is untyped, so a hand-edited file can hold `-1`, `999999` or a value past `u64::MAX`'s neighbour in the
/// column this program wrote as `8` and `1730000000`. Narrowing on the way out would turn that into a panic,
/// a wrap, or a row that silently equals another ([`crate::models::install`] compares a *declared* dim with
/// the stored one to decide "already installed"), so the reader reports what is there and lets the caller
/// judge. A value not readable as an integer at all — TEXT in a column someone typed by hand — arrives as
/// [`DbError::Sqlite`], which is the same honest answer: it is reported, not guessed past.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRow {
    /// The identity vectors reference (`embeddings.model`'s foreign key), exactly as installed.
    pub id: String,
    /// Declared vector width. See this struct's note on `i64`.
    pub dim: i64,
    /// Lowercase-hex digest the artifact was verified against at install, or `None` for a provider with no
    /// local weights — migration 7's ordinary arm, not a missing value.
    pub sha256: Option<String>,
    /// Text prefix added to documents.
    pub prefix_doc: String,
    /// Text prefix added to queries.
    pub prefix_query: String,
    /// Whole seconds since the Unix epoch, recorded once at install and rewritten by nothing — not even a
    /// re-materialised artifact ([`crate::models::InstallOutcome::Restored]), because the row's claim about
    /// *when this id became known* does not change when its file comes back.
    pub installed_at: i64,
    /// Size in bytes of the artifact whose digest this row last verified, or `None` when no verification has run
    /// since this build was installed. Paired with [`Self::verified_mtime`]; see `local_model` (behind the `local-model` feature) for the rule
    /// that reads them and for what a match does and does not prove.
    pub verified_size: Option<i64>,
    /// Whole seconds since the Unix epoch of that same file's last modification. Read together with
    /// [`Self::verified_size`] — one of the two is treated as no stamp at all.
    pub verified_mtime: Option<i64>,
}

/// The `SELECT` column order every reader of `models` uses, so a row and a binder cannot disagree about
/// which column is which by spelling the list twice.
const MODEL_COLUMNS: &str =
    "id, dim, sha256, prefix_doc, prefix_query, installed_at, verified_size, verified_mtime";

/// Read the one row naming `id`, or `None` when no model of that identity has been installed.
///
/// The reads live here rather than in the command because np-cli has no `rusqlite` dependency and must not
/// grow one to answer a question about its own table (`docs/design-m2-models-install.md` §6), and because
/// `install`'s idempotence test is *this* row read against the *declared* tuple — a comparison no caller may
/// skip or partially perform.
pub fn model_row(db: &Db, id: &str) -> Result<Option<ModelRow>, DbError> {
    // The statement text is assembled per call rather than written out twice: `MODEL_COLUMNS` is the single
    // spelling of the column order that [`read_model_row`] binds by position, and these two reads are one row
    // per command, not a scan's per-file loop, so the allocation is not what `prepare_cached` exists to save.
    Ok(db
        .0
        .prepare_cached(&format!("SELECT {MODEL_COLUMNS} FROM models WHERE id = ?1"))?
        .query_row(params![id], read_model_row)
        .optional()?)
}

/// Every installed model, ordered by `id`.
///
/// Ordered because `np models list` output is compared by a human and by tests: without `ORDER BY` SQLite
/// answers in rowid order, which for a table whose primary key is TEXT is insertion order — so re-installing
/// two ids in the other sequence would reorder the listing.
pub fn model_rows(db: &Db) -> Result<Vec<ModelRow>, DbError> {
    let mut statement =
        db.0.prepare_cached(&format!("SELECT {MODEL_COLUMNS} FROM models ORDER BY id"))?;
    let rows = statement.query_map([], read_model_row)?;
    let mut rows_out = Vec::new();
    for row in rows {
        rows_out.push(row?);
    }
    Ok(rows_out)
}

/// One row of `models`, bound from the eight columns of [`MODEL_COLUMNS`].
fn read_model_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModelRow> {
    Ok(ModelRow {
        id: row.get(0)?,
        dim: row.get(1)?,
        sha256: row.get(2)?,
        prefix_doc: row.get(3)?,
        prefix_query: row.get(4)?,
        installed_at: row.get(5)?,
        verified_size: row.get(6)?,
        verified_mtime: row.get(7)?,
    })
}

/// Record which artifact bytes [`ModelRow::sha256`] was just checked against.
///
/// Written **only after a digest matched**, never before: a stamp that predates verification would let the next run
/// skip the hash it has not yet earned. Both halves arrive together because a reader treats a half stamp as no stamp,
/// and both are `NULL`-able in the schema only because an older row has neither.
///
/// The row need not exist for this to be a no-op rather than an error: a model deleted between the verification and
/// this write (a concurrent `np models remove`) is a fact about the world, not a corrupt database, and the digest was
/// already checked in this process.
pub fn record_artifact_stamp(db: &Db, id: &str, size: i64, mtime: i64) -> Result<(), DbError> {
    db.0.prepare_cached("UPDATE models SET verified_size = ?2, verified_mtime = ?3 WHERE id = ?1")?
        .execute(params![id, size, mtime])?;
    Ok(())
}

/// Insert an installed model, reporting whether this call was the one that wrote it.
///
/// `ON CONFLICT (id) DO NOTHING` rather than a plain `INSERT`, and the reason is concurrent installs of one
/// id (§6): two processes can both pass [`crate::models`]'s "is there already a row?" step, and if the loser
/// raised `SQLITE_CONSTRAINT_PRIMARYKEY` the human would read a bare constraint violation about a table they
/// have never opened. Returning `false` says what actually happened — somebody else installed this identity,
/// and [`crate::models::install`] turns that into the same full-tuple comparison it applies to its own
/// pre-flight read, so a *different* declaration is still refused rather than papered over.
///
/// `dim` and `now` arrive as `i64` because those are SQLite's `INTEGER` columns; the range rules on a
/// declared dim (`u16`, never zero) belong to the caller that parses human input, not to the writer
/// (`docs/design-m2-models-install.md` §2). `sha256` is `None` for a provider with no local weights —
/// migration 7's nullable arm.
pub fn insert_model(
    db: &Db,
    id: &str,
    dim: i64,
    sha256: Option<&str>,
    prefix_doc: &str,
    prefix_query: &str,
    now: i64,
) -> Result<bool, DbError> {
    let written =
        db.0.prepare_cached(
            "INSERT INTO models (id, dim, sha256, prefix_doc, prefix_query, installed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (id) DO NOTHING",
        )?
        .execute(params![id, dim, sha256, prefix_doc, prefix_query, now])?;
    Ok(written == 1)
}

/// One recorded member of a model's set of files (migration 9, §12 rule 10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFile {
    /// Which file of the id this is: `main` is the primary artifact whose digest also stands in `models.sha256`.
    pub name: String,
    /// Lowercase hex sha256 of those bytes, as recorded when they were written.
    pub sha256: String,
    /// Exact byte length.
    pub size: i64,
}

/// Record (or correct) one member of a model's file set.
///
/// An upsert rather than an insert because both `install` and a fetch can legitimately re-record the same name after
/// re-publishing the same bytes, and "the digest we hold for `main`" is one fact that must not fork into two rows or
/// fail with a constraint violation the caller cannot distinguish from a real conflict.
pub fn record_model_file(
    db: &Db,
    model: &str,
    name: &str,
    sha256: &str,
    size: i64,
) -> Result<(), DbError> {
    db.0.prepare_cached(
        "INSERT INTO model_files (model, name, sha256, size)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (model, name) DO UPDATE SET sha256 = excluded.sha256, size = excluded.size",
    )?
    .execute(params![model, name, sha256, size])?;
    Ok(())
}

/// Every recorded member of `model`, ordered by name.
///
/// **An empty vector means nothing was recorded, not that the model has no files** — a row installed before migration
/// 9 carries its primary digest in `models.sha256` alone. Callers that need the primary digest must fall back to that
/// column rather than reading "no rows" as "no artifact".
pub fn model_files(db: &Db, model: &str) -> Result<Vec<ModelFile>, DbError> {
    let mut stmt = db.0.prepare_cached(
        "SELECT name, sha256, size FROM model_files WHERE model = ?1 ORDER BY name",
    )?;
    let rows = stmt.query_map(params![model], |row| {
        Ok(ModelFile {
            name: row.get(0)?,
            sha256: row.get(1)?,
            size: row.get(2)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::{orphan_total, rebase_abs, report_merges, report_orphans, OrphanCleanup};

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
        // F21 §1, at the unit level. This echo used to sit inside the migration's transaction, so a
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
        // The other half of the promise: every pre-F5 workspace passes through the migration, and
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
        // and the count printed beside it is read back out of it — one value, three readers (F21 §3's rule).
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
        // F21 grew precisely this assertion for the orphan echo — so what is true is narrower: no CLI case
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
}

// ---------------------------------------------------------------------------
// Media: content identity, the paths that hold it, and its vectors (migration 10).
//
// These read and write the three media tables and the two stamp pairs. Nothing here decides what an image *is* —
// resolution lives in `crate::media`, encoding in `crate::vision` — so each function states only the row-level promise
// its caller can rely on.
// ---------------------------------------------------------------------------

/// One media row: content identity plus what was last seen about those bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRow {
    /// Rowid, which is also the order [`media_embeddings`] returns in.
    pub id: i64,
    /// Lowercase hex sha256 of the bytes — the identity, so two paths holding one file share this row.
    pub hash: String,
    /// Exact byte length as last recorded.
    pub size: i64,
    /// A mime type guessed from the extension (`image/png`, `image/jpeg`), stored for the report line and nothing else.
    pub mime: String,
}

/// Record these bytes, or read back the row that already holds them.
///
/// Insert-or-get rather than a plain insert, because one diagram linked from two notes is one ordinary next run, not a
/// constraint violation: `ON CONFLICT (hash) DO NOTHING` and a read-back make the second call return the first row's id.
/// The conflicting call's `size`/`mime` are deliberately **not** written over what is stored — the hash says these are
/// the same bytes, so a differing size would be a contradiction to report, not a field to refresh, and quietly
/// overwriting it would make the row describe bytes it may never have held.
pub fn record_media(db: &Db, hash: &str, size: i64, mime: &str) -> Result<MediaRow, DbError> {
    db.0.prepare_cached(
        "INSERT INTO media (hash, size, mime) VALUES (?1, ?2, ?3)
         ON CONFLICT (hash) DO NOTHING",
    )?
    .execute(params![hash, size, mime])?;
    read_media_by_hash(db, hash)
}

/// The media row for one hash, which [`record_media`] reads back after either arm of its upsert.
fn read_media_by_hash(db: &Db, hash: &str) -> Result<MediaRow, DbError> {
    db.0.prepare_cached("SELECT id, hash, size, mime FROM media WHERE hash = ?1")?
        .query_row(params![hash], |row| {
            Ok(MediaRow {
                id: row.get(0)?,
                hash: row.get(1)?,
                size: row.get(2)?,
                mime: row.get(3)?,
            })
        })
        .map_err(DbError::from)
}

/// Point one workspace path at one media row, re-pointing it if the path was already linked.
///
/// An upsert because a diagram replaced in place, or moved and re-added under the same name, is the normal case; a
/// `SQLITE_CONSTRAINT_PRIMARYKEY` out of the write pass would roll the whole embed pass back over one re-pointed row.
/// `rel_bytes` is bound as bytes and nothing else — the column's `typeof` CHECK is what keeps a lossy decode from
/// becoming the stored spelling (F13), which is why this takes `&[u8]` where a `&str` would have compiled.
pub fn link_media_path(
    db: &Db,
    rel: &[u8],
    case_key: &str,
    abs: &Path,
    media_id: i64,
) -> Result<(), DbError> {
    db.0.prepare_cached(
        "INSERT INTO media_path (rel_bytes, case_key, abs, media_id) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (rel_bytes) DO UPDATE
            SET case_key = excluded.case_key, abs = excluded.abs, media_id = excluded.media_id",
    )?
    .execute(params![rel, case_key, abs.to_string_lossy(), media_id])?;
    Ok(())
}

/// Remove one path, reporting how many media rows went with it.
///
/// The count is the lifecycle rule made observable (§6): a media row lives while at least one path holds those bytes,
/// so the last unlink deletes the media row and `ON DELETE CASCADE` takes its vectors. Returning the number rather than
/// a bool lets the embed report say "1 figure removed" without a second query, and says `0` for both "one of two paths
/// went" and "this path was never linked" — a deleted file is a fact about the world, not a corrupt database.
pub fn unlink_media_path(db: &Db, rel: &[u8]) -> Result<usize, DbError> {
    let holder =
        db.0.prepare_cached("SELECT media_id FROM media_path WHERE rel_bytes = ?1")?
            .query_row(params![rel], |row| row.get::<_, i64>(0))
            .ok();
    let removed_path =
        db.0.prepare_cached("DELETE FROM media_path WHERE rel_bytes = ?1")?
            .execute(params![rel])?;
    let Some(media_id) = holder else {
        debug_assert_eq!(removed_path, 0, "no holder means no row to delete");
        return Ok(0);
    };
    // `NOT EXISTS` rather than a count compare so the check and the delete are one statement: a second process linking
    // the same bytes between two queries must not leave an unreferenced media row standing.
    let removed_media =
        db.0.prepare_cached(
            "DELETE FROM media WHERE id = ?1
              AND NOT EXISTS (SELECT 1 FROM media_path WHERE media_id = media.id)",
        )?
        .execute(params![media_id])?;
    Ok(removed_media)
}

/// Store (or correct) the vector one model computed for one media row.
///
/// An upsert on `(media_id, model)` for migration 7's reason restated: re-embedding must replace a row rather than
/// append history nobody reads, and two models must be able to hold a vector for one figure at the same time.
/// `embedded_at` is the caller's clock, like every other timestamp this module writes — there is no SQL-side clock in
/// this crate, and a reader that cannot say when a row went stale is a reader an operator cannot audit.
pub fn record_media_embedding(
    db: &Db,
    media_id: i64,
    model: &str,
    dim: i64,
    vector: &[u8],
    source_hash: &str,
    now: i64,
) -> Result<(), DbError> {
    db.0.prepare_cached(
        "INSERT INTO media_embeddings (media_id, model, dim, vector, source_hash, embedded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (media_id, model) DO UPDATE
            SET dim = excluded.dim, vector = excluded.vector,
                source_hash = excluded.source_hash, embedded_at = excluded.embedded_at",
    )?
    .execute(params![media_id, model, dim, vector, source_hash, now])?;
    Ok(())
}

/// Every media vector stored under one model id, ascending by media id.
///
/// [`vectors_for`]'s sibling, and the same rule applies: the `model` filter is not optional, because
/// `PRIMARY KEY (media_id, model)` lets two models answer for one figure and mixing them is how one model's query
/// ranks another's answer. Ordered by media id so a resumed run produces the same ranking as an uninterrupted one.
pub fn media_embeddings(db: &Db, model: &str) -> Result<Vec<(i64, Vec<u8>)>, DbError> {
    let mut statement = db.0.prepare_cached(
        "SELECT media_id, vector FROM media_embeddings WHERE model = ?1 ORDER BY media_id",
    )?;
    let rows = statement.query_map(params![model], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Record the stamp that justified skipping a re-hash of one media row's bytes.
///
/// Written only **after** a digest matched, exactly like [`record_artifact_stamp`], and a no-op for a row that is gone:
/// a figure unlinked between verification and this write is a fact about the world, not a corrupt database.
pub fn set_media_stamp(db: &Db, media_id: i64, size: i64, mtime: i64) -> Result<(), DbError> {
    db.0.prepare_cached("UPDATE media SET verified_size = ?2, verified_mtime = ?3 WHERE id = ?1")?
        .execute(params![media_id, size, mtime])?;
    Ok(())
}

/// The stamp for one media row, or `None` when it has never been verified.
///
/// A half pair reads as no stamp at all — migration 8's rule, and the reason this returns a pair instead of two
/// columns: trusting one present column would let an embed skip hashing bytes it never checked.
pub fn media_stamp(db: &Db, media_id: i64) -> Result<Option<(i64, i64)>, DbError> {
    read_stamp_pair(
        db,
        "SELECT verified_size, verified_mtime FROM media WHERE id = ?1",
        params![media_id],
    )
}

/// Record the stamp that justified skipping a re-hash of one recorded model file.
///
/// The projector's half of §6's third change: without it, verifying the 554,821,056-byte `mmproj` costs a full read on
/// every image run. No-op for an unrecorded member, like [`record_artifact_stamp`] is for a missing model.
pub fn record_member_stamp(
    db: &Db,
    model: &str,
    name: &str,
    size: i64,
    mtime: i64,
) -> Result<(), DbError> {
    db.0.prepare_cached(
        "UPDATE model_files SET verified_size = ?3, verified_mtime = ?4
              WHERE model = ?1 AND name = ?2",
    )?
    .execute(params![model, name, size, mtime])?;
    Ok(())
}

/// The stamp for one recorded member, or `None` when it has never been verified.
///
/// Three states collapse here on purpose and one does not: "no member row" and "member with no stamp" both read `None`
/// because both mean *hash it*, while a stamp of `(0, 0)` reads `Some` because zeros are a size and an mtime that were
/// actually observed. Callers that need the first distinction ask [`model_files`] whether the member is recorded at all
/// — which is precisely what migration 9 could not express for an `install --path` model and this entry adds.
pub fn member_stamp(db: &Db, model: &str, name: &str) -> Result<Option<(i64, i64)>, DbError> {
    read_stamp_pair(
        db,
        "SELECT verified_size, verified_mtime FROM model_files WHERE model = ?1 AND name = ?2",
        params![model, name],
    )
}

/// One stamp pair, with the half-pair rule in a single place so `media` and `model_files` cannot drift apart.
fn read_stamp_pair(
    db: &Db,
    sql: &str,
    args: impl rusqlite::Params,
) -> Result<Option<(i64, i64)>, DbError> {
    let pair = db.0.prepare_cached(sql)?.query_row(args, |row| {
        Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?))
    });
    match pair {
        Ok((Some(size), Some(mtime))) => Ok(Some((size, mtime))),
        Ok(_) => Ok(None),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(DbError::from(err)),
    }
}
