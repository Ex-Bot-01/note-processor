use crate::db::{Db, DbError};
use crate::hash::Snapshot;
use crate::path_keys::{self, RelPath};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

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
/// is exactly the bug the byte key exists to prevent: twins spell themselves identically, so the second one would resolve onto its
/// twin's row and overwrite it with no constraint firing at all.
///
/// The row is resolved by `id`, in two statements, where this function used to carry one
/// `ON CONFLICT (rel) DO UPDATE` arm. That arm cannot survive the migration: `rel TEXT PRIMARY KEY` was the
/// only reason `ON CONFLICT (rel)` matched anything, migration 4 gives `rel` no unique constraint at all, and
/// SQLite answers a non-matching conflict target at *prepare* time — so an arm kept out of habit does not fail
/// on a lossy name, it fails on the first ordinary ASCII file and takes every scan down with it. The lookup
/// that replaced it asks about `rel_bytes` — the conflict target moved to the byte key — in the shape this
/// function can prepare: two twins now resolve to two different rows.
///
/// A `case_bytes` collision — two paths differing only in ASCII case on a case-sensitive filesystem —
/// surfaces as the constraint violation it is, exactly as `case_key UNIQUE` used to. Nothing picks a winner:
/// the scan reports such a path under `skipped` and keeps scanning. Names differing only in a *non-ASCII*
/// letter now get two rows, which is the behaviour change the byte-key migration accepted out loud.
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
    // The row's **previous digest travels out of this function**. The caller decides
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
/// write. That matters more here than anywhere else in the schema: `notes.rel` is plain TEXT and stays
/// nullable, so an insert that named only the display column would succeed — leaving a note whose byte key is
/// NULL, which matches nothing the re-keyed join, cascade and lookup compare on bytes. A note row silently
/// detached from its file is the bug this migration exists to end, reproduced by the fix, starting with the
/// first new note.
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
/// copies both onto one key. Handing back one answer from that population is either a silent pick,
/// which rewrites a note's whole derived set with another note's, or an `ExecuteMultipleValues` error, which
/// makes a legacy workspace impossible to scan forever. Both are refused by returning the count for the caller
/// to act on: one id is the normal path, none means "no row yet", and more than one is an identity conflict
/// the scan reports and leaves alone.
///
/// The key arrives as bytes because that is the only spelling with one answer per file; matching on display
/// text would let a twin's note answer for it.
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
    // `rebase_abs` is display work — its output is the display `abs` column — so it reads the two
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
/// instead of forgetting it ever existed. A path that is not in the registry changes
/// nothing, which is correct — there is no row to mark.
///
/// Matched on `rel_bytes`, and taking bytes from the caller is what makes that possible: keyed on
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
pub(crate) fn rebase_abs(abs: &str, from: &str, to: &str) -> Option<String> {
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
