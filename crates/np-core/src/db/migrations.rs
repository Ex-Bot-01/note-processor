use crate::db::DbError;
use crate::path_keys::{self};
use rusqlite::{params, Connection};
use std::io;

use crate::db::meta::read_receipt;
use crate::db::meta::Carried;
use crate::db::meta::FK_ORPHANS_KEY;
use crate::db::meta::NOTE_MERGE_KEY;
use crate::db::upgrades::byte_path_keys;
use crate::db::upgrades::detach_orphans;
use crate::db::upgrades::merge_duplicate_notes;
use crate::db::upgrades::report_merges;
use crate::db::upgrades::report_orphans;
use crate::db::upgrades::unlink_destinations;
use crate::db::upgrades::OrphanCleanup;

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
    // `files` and `notes` gain byte keys. Also not a script, for the same reason plus one more: the
    // ASCII-case fold of a byte key is Rust's, and SQL has no equivalent that a copy could call.
    Migration::Code(byte_path_keys),
    // The duplicate note rows version 3 allowed. Not a script either: the survivor is chosen per key
    // and its incoming edges are re-pointed rather than dropped, which is a loop with a receipt, not a
    // statement.
    Migration::Code(merge_duplicate_notes),
    // Durable file identity: the OS's own answer to "is this the same file", so a note whose
    // bytes changed while its path was unreadable can still be rejoined to its row. Nullable and **not
    // backfilled**: identity is only readable by walking, so a row carries NULL until its path is next seen,
    // and every reader must treat that as unknown rather than as a mismatch. No CHECK (the bytes are opaque and
    // platform-shaped) and no index (nothing looks a row up by it; the scan reads the table in bulk).
    Migration::Sql(
        r#"
    ALTER TABLE files ADD COLUMN file_id BLOB;
    "#,
    ),
    // Chunk and vector storage: the first derived data that hangs off a note *id* rather than a path, which is
    // the byte-key lesson applied forward — a chunk storing `rel_bytes` would inherit every lossy-name, rename
    // and merge defect the earlier migrations spent removing.
    //
    // Four things about this shape are decisions rather than accidents:
    //
    // - **`models.sha256` is nullable by design.** Weights are never committed and `np models install` verifies the
    //   digest it fetched, but a provider with no weights — the default `embeddings.provider = "none"`, and every
    //   endpoint provider — has nothing to verify. NULL is therefore the common arm, so there is no
    //   `CHECK (typeof(sha256) = 'text')`: that CHECK would refuse the shipping configuration.
    // - **`embeddings.model` is a real foreign key**, and `models` is declared before its child so the key reads in
    //   file order. Without the reference an embedding row could name a model that was never installed, or one
    //   removed and reinstalled under the same id — exactly the orphan class migration 3 exists to end, the one
    //   that gave every child statement a twin in the schema — and
    //   `PRAGMA foreign_key_check` reads *declared* constraints and ignores the pragma, which is what
    //   `first_violation`'s own comment says; a key that was never declared is a key that check can never see.
    //   Declaring it here costs nothing at write time and buys the one audit that reaches every row.
    // - **`vector` carries a length CHECK.** Little-endian f32 rather than quantised bytes is a deliberate
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
    // **Verify-on-change.** Two nullable columns remembering which artifact bytes this row's
    // `sha256` was last checked against: the size and whole-second mtime of the file at that moment. Their only job is
    // to make re-verification cheap — a full pass over the pinned artifact costs **11.32 s** (26 MiB/s, our own
    // `hash::sha256_file`) against **~0.09 s** for llama.cpp's first read of every byte — so hashing before
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
    // **A model can be more than one file.** The
    // `embeddinggemma-2` pair is a text encoder plus an `mmproj` vision/audio projector, and `models.sha256` holds
    // exactly one digest — the **primary** artifact's, which is what every existing reader (`verify_artifact`,
    // `install`'s comparison, `local_model`'s identity check) has always meant by it. Keeping that column meaning
    // *main* is the deliberate half of this migration: the alternative, re-pointing it at a pair, would silently
    // change what "the digest matches" asserts in code that cannot express "which file".
    //
    // The asymmetry is why this is a child table and not more columns on `models`: **digests are cheap to
    // retrofit, names are not** — bytes already on disk live under `<models_dir>/<id>` with no room for a second
    // name. So the pair gets names here (`name = 'main'` is the primary, matching the artifact path) and any later
    // member arrives as a row rather than as a schema change.
    //
    // **One coupling to know before adding a migration:** every rewind fixture rewinds a database to version four to
    // exercise the upgrade path (`tests/db.rs`, `tests/scan.rs`, `np-cli/tests/cli.rs` — eight sites as of migration
    // 10), and each undoes what later entries created. A new table that they leave standing makes the next open fail with "table already exists" —
    // which is the fixture being right, not the migration being wrong.
    //
    // Not backfilled, like migration 6's `file_id` and migration 8's stamp: a model installed by an older build has
    // no recorded members until it is installed or fetched again, and readers treat "no rows" as *unknown*, never as
    // "this model has no files". No foreign key either — `models` has no delete path today, and adding an FK whose
    // cascade this migration cannot test on three OSes would be a claim, not a constraint.
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
    // 10. Media as a vector source of its own: an image linked from a note
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
    // 'blob'` beside declared affinity is migration 4's lesson about affinity (alone it accepts a TEXT bind, and `length()`
    // would then count characters), and `typeof(vector) = 'blob' AND length(vector) = 4 * dim` is migration 7's fence on
    // `embeddings` — eight characters satisfy `length(vector) = 8` at dim 2 and reach a cosine as garbage.
    //
    // **Verify-on-change, carried to two more places.** `media_path.verified_size` / `verified_mtime` decide
    // whether the next run re-hashes an image or trusts one `stat`; adding them to `model_files` is what stops every
    // image pass from hashing all 554,821,056 projector bytes (~21 s at the measured rate above). Both
    // pairs are nullable and **not backfilled**: an older row reads as "never checked", which is the honest cost of
    // upgrading into a check, and a half pair reads as no pair at all (`media_path_check`, `member_stamp`) for the
    // migration-8 reason.
    //
    // **The image pair sits on `media_path`, not on `media`, and that placement is the whole point.** The rule is "one
    // `stat` when the bytes have not moved since the last check, a full hash when they have" — a statement about a
    // *file*, while `media` is keyed by content hash. A stamp on `media` could only be read after the hash was known,
    // which means after reading the file: the work the stamp exists to skip. So the pair travels with the path it
    // describes and joins to the hash rather than guarding it (`media_path_check`).
    //
    // Migration 10 is unreleased on this branch, so the DDL is edited in place rather than given a second entry — the same
    // call as dropping `media_path.case_key`, and every rewind fixture already drops these tables. (Migration 11 exists now:
    // The keyword index is a separate entry because it changes nothing here, and the reasoning above was about not
    // splitting one media shape across two versions, not about keeping the list at ten.)
    //
    // Same coupling as migration 9 warned about, now pointing forward: the eight rewind fixtures
    // (`tests/db.rs`, `tests/scan.rs`, `np-cli/tests/cli.rs`) delete version numbers and must undo what later entries
    // created, so each one drops these three tables — child first — beside `model_files`. The keyword index added its own
    // entry: the same fixtures also drop `notes_fts` and its three triggers, or re-running migration 11 fails on
    // `table notes_fts already exists`.
    Migration::Sql(
        r#"
    CREATE TABLE media (
        id             INTEGER PRIMARY KEY,
        -- Content identity: the same SHA-256 is one figure wherever it sits, however often it is copied (§7).
        hash           TEXT    NOT NULL UNIQUE CHECK (length(hash) = 64),
        size           INTEGER NOT NULL CHECK (size >= 0),
        mime           TEXT    NOT NULL
    );

    -- Every workspace path those bytes were seen at. Nothing here deletes a media row: the cascade below points the
    -- other way (parent to child), and SQLite has no "when the last child goes" constraint, so §6's lifecycle is kept by
    -- two accessors — [`unlink_media_path`] for a path that went away and [`prune_unlinked_media`] for media that never
    -- gained one. Both are needed: an embed that records bytes and then refuses to encode them leaves a row no unlink
    -- call can find, because there is no path left to unlink.
    --
    -- No foreign key to `files`, deliberately: `files` rows exist for images too, but a media path is a fact about a
    -- *link* in a note, and §6 keeps `links` as prose rather than a resolver — an FK here would make the registry, not
    -- the embed pass, decide when a figure stops being searchable. Re-pointing on a rename is therefore the embed pass's
    -- job (`link_media_path`'s upsert), and the row carries no case-folded twin of its key: `files.case_key` exists
    -- because the registry has to notice a case-twin *rename* on a case-insensitive filesystem, while this table's
    -- identity is a content hash — two spellings of one figure already resolve to one media row, which
    -- `case_twin_paths_holding_one_file_answer_once` pins. A column nothing reads would only add a NOT NULL to write.
    CREATE TABLE media_path (
        rel_bytes   BLOB    NOT NULL CHECK (typeof(rel_bytes) = 'blob'),
        abs         TEXT    NOT NULL,
        media_id    INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
        -- D12's stamp pair, on the **path**: one `stat` while this file has not moved, a full hash when it has. Kept
        -- here because a stamp that requires the content hash before it can be read skips nothing.
        verified_size  INTEGER,
        verified_mtime INTEGER,
        PRIMARY KEY (rel_bytes)
    );

    -- Both lifecycle accessors ask "does any path still hold this row?", which without an index on `media_id` is a full
    -- scan of `media_path` per media row — migration 4's `files_rel` argument, re-argued for the same reason.
    CREATE INDEX media_path_media_id ON media_path(media_id);

    CREATE TABLE media_embeddings (
        media_id      INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
        model         TEXT    NOT NULL REFERENCES models(id) ON DELETE CASCADE,
        -- `dim > 0` is not decoration: the length arm alone accepts `dim = 0` with an empty BLOB, and a zero-component
        -- row reads back as a vector of nothing — the garbage-cosine class the length arm exists to stop.
        dim           INTEGER NOT NULL CHECK (dim > 0),
        vector        BLOB    NOT NULL CHECK (typeof(vector) = 'blob' AND length(vector) = 4 * dim),
        source_hash   TEXT    NOT NULL,
        embedded_at   INTEGER NOT NULL,
        PRIMARY KEY (media_id, model)
    );

    ALTER TABLE model_files ADD COLUMN verified_size INTEGER;
    ALTER TABLE model_files ADD COLUMN verified_mtime INTEGER;
    "#,
    ),
    // **Migration 11 — the keyword half of search** (`docs/design-m3-hybrid-search.md`). External content: the index stores
    // terms pointing at `notes.id`, never a second copy of the prose, which is why the three triggers below are load-bearing
    // rather than an optimisation — a shadow table cannot notice a change on its own. They are installed **after** the
    // backfill so seeding cannot double-insert.
    //
    // `AFTER UPDATE OF title, body` deliberately omits `rel`: a move rewrites the path by cascade and changes no word, and
    // the keyword row keyed on `notes.id` survives it exactly as `headings` and `links` do. The delete trigger passes the
    // **old** text, which is what FTS5 needs to find the terms to drop — reading the new row there leaves ghost hits.
    //
    // No stemming (`porter` would fold `runners` onto `runner`, and this vault holds identifiers as often as prose);
    // `remove_diacritics 2` stays, because `café`/`cafe` is one spelling rather than two words.
    Migration::Sql(
        r#"
    CREATE VIRTUAL TABLE notes_fts USING fts5(
        title,
        body,
        content='notes',
        content_rowid='id',
        tokenize = 'unicode61 remove_diacritics 2'
    );

    INSERT INTO notes_fts (rowid, title, body)
    SELECT id, title, body FROM notes;

    CREATE TRIGGER notes_fts_ai AFTER INSERT ON notes BEGIN
        INSERT INTO notes_fts (rowid, title, body) VALUES (new.id, new.title, new.body);
    END;

    CREATE TRIGGER notes_fts_ad AFTER DELETE ON notes BEGIN
        INSERT INTO notes_fts (notes_fts, rowid, title, body)
        VALUES ('delete', old.id, old.title, old.body);
    END;

    CREATE TRIGGER notes_fts_au AFTER UPDATE OF title, body ON notes BEGIN
        INSERT INTO notes_fts (notes_fts, rowid, title, body)
        VALUES ('delete', old.id, old.title, old.body);
        INSERT INTO notes_fts (rowid, title, body) VALUES (new.id, new.title, new.body);
    END;
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

/// Apply every migration whose version is above what the file recorded, returning the receipts of the
/// steps *these* ran (see [`ORPHAN_CLEANUP_VERSION`], [`NOTE_MERGE_VERSION`] and [`Carried`]).
pub(crate) fn migrate(conn: &Connection) -> Result<Carried, DbError> {
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
    // references it, and `ALTER TABLE... RENAME` rewrites the references it finds — and the pragma has
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
        // there, and say it again on every retry: the false announcement this position exists to refuse.
        // Nothing is printed anywhere before this line, so a step that rolled itself back — at any statement,
        // including this commit — is silent. That silence rests on call-site position rather than on
        // a test: no portable way to fail a COMMIT here is known, and the
        // durable receipt in `meta` is what the promise actually rests on. Both receipts are scoped to one
        // iteration, so no later step can re-echo an earlier one — and the duplicate merge follows the same
        // rule for the same reason: a merge announced before its commit is a merge that may not have happened.
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
/// drops only this note's `origin = 'prose'` rows and keeps an approved one, but a foreign
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

    // This `?` is the invariant rather than error handling for its own sake: the write shares
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
pub(crate) fn first_violation(conn: &Connection) -> Result<Option<(String, i64, String)>, DbError> {
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
/// stability ruling the version-3 copy already follows (see [`rebuild_with_keys`] for why the order is
/// rather than stylistic). `case_key` is copied, never recomputed: SQLite's `LOWER()` folds ASCII only, so
/// deriving it in SQL would rewrite every non-ASCII name's fold and break the link resolution that reads it.
pub(crate) fn copy_files_rows(conn: &Connection) -> Result<(), DbError> {
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
        // Both keys come from this row's own `rel`, through the one module that owns them — no statement
        // derives a key by hand. The migrated keys are the bytes of the stored display spelling,
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
pub(crate) fn assert_replacement_complete(
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
