//! Media: content identity, the paths that hold it, and its vectors (migration 10).
//!
//! These read and write the three media tables and the two stamp pairs. Nothing here decides what an image *is* —
//! resolution lives in `crate::media`, encoding in `crate::vision` — so each function states only the row-level promise
//! its caller can rely on.

use crate::db::{Db, DbError};

use crate::db::models::stamp_pair;
use rusqlite::{params, OptionalExtension};
use std::path::Path;

/// One media row: content identity plus what was last seen about those bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaRow {
    /// Rowid, which is also the order [`media_embeddings`] returns in.
    pub id: i64,
    /// Lowercase hex sha256 of the bytes — the identity, so two paths holding one file share this row.
    pub hash: String,
    /// Exact byte length as last recorded.
    pub size: i64,
    /// What the bytes **are** (`image/png`, `image/jpeg`), from [`crate::media::image_kind_of_prefix`] rather than the
    /// filename — a file whose name lies is stored as what it is, because we measured the bytes to get here anyway.
    ///
    /// **Nothing reads this column yet.** The embed report counts figures and never names their format, so `mime` (like
    /// `size`) is recorded for whoever queries the database directly and for the unbuilt `np search` output, where a hit will
    /// name what it found. Claiming it feeds the report line would be a promise no code keeps.
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

/// The media row for one content hash, or `None` when nothing holds those bytes any more.
///
/// The reader a stamped path needs: [`media_path_check`] answers with the hash its pair vouches for, and this turns that
/// hash back into the row (and so into `media_stamp`'s currency question) **without reading the file**. Public because a
/// caller outside this module reaches it through that stamp; [`record_media`] reads back through the same statement.
pub fn media_by_hash(db: &Db, hash: &str) -> Result<Option<MediaRow>, DbError> {
    match read_media_by_hash(db, hash) {
        Ok(row) => Ok(Some(row)),
        // Absent is the answer a stamped path can act on; anything else (a locked database, a column that is not the
        // integer this wrote) stays an error rather than becoming "no such figure".
        Err(DbError::Sqlite(rusqlite::Error::QueryReturnedNoRows)) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Every path a media row is recorded at, in path order.
///
/// The lifecycle sweep's read: the embed pass knows which paths the live notes referenced and needs the ones
/// it did not. Ordered by `rel_bytes` so the sweep deletes in the same order on every host.
pub fn media_paths(db: &Db) -> Result<Vec<Vec<u8>>, DbError> {
    let mut statement =
        db.0.prepare_cached("SELECT rel_bytes FROM media_path ORDER BY rel_bytes")?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut paths = Vec::new();
    for row in rows {
        paths.push(row?);
    }
    Ok(paths)
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
/// `rel_bytes` is bound as bytes and never decoded — the column's `typeof` CHECK is what keeps a lossy spelling from
/// becoming the stored path, which is why this takes `&[u8]` where a `&str` would have compiled. `abs` is the
/// opposite case and is deliberately lossy: it is the display spelling a report prints, because a report shows
/// bytes as text, while `rel_bytes` stays the identity.
pub fn link_media_path(db: &Db, rel: &[u8], abs: &Path, media_id: i64) -> Result<(), DbError> {
    // What this path pointed at before the upsert, if anything. A figure replaced in place is the ordinary case, and
    // once its last path re-points elsewhere nothing references it — the same stranded-row shape as an unlink, so the
    // displaced row is swept here rather than left for `np search` to keep answering.
    // **Not `.ok()`**: a `SQLITE_BUSY` here would read as "nothing was displaced", the stranded row would keep its
    // vectors, and the run would report success. Only "no such row" is absence; everything else is a fault and travels
    // (`media_by_hash` follows the same rule).
    let displaced = match db
        .0
        .prepare_cached("SELECT media_id FROM media_path WHERE rel_bytes = ?1 AND media_id <> ?2")?
        .query_row(params![rel, media_id], |row| row.get::<_, i64>(0))
    {
        Ok(old) => Some(old),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(err) => return Err(err.into()),
    };
    // Re-pointing a path **clears its stamp pair** when the content changes, which is the load-bearing half of this
    // statement (`record_model_file` established the rule: a digest change nulls the check). Without it the pair survives
    // as a complete, confident record of bytes that are no longer there: `verified_size` and `verified_mtime` were measured
    // against the old content, and if the new file happens to share a size and a whole-second mtime — a rewritten
    // screenshot, an editor's atomic save, a copy that preserved length — the next pass reads no bytes at all and answers
    // search with a vector of the figure that used to live here. Same content re-linked (the ordinary second run) keeps its
    // pair, so the re-hash rule still costs exactly one `stat`.
    db.0.prepare_cached(
        "INSERT INTO media_path (rel_bytes, abs, media_id) VALUES (?1, ?2, ?3)
         ON CONFLICT (rel_bytes) DO UPDATE
            SET abs = excluded.abs,
                media_id = excluded.media_id,
                verified_size = CASE
                    WHEN media_path.media_id IS NOT excluded.media_id THEN NULL
                    ELSE media_path.verified_size
                END,
                verified_mtime = CASE
                    WHEN media_path.media_id IS NOT excluded.media_id THEN NULL
                    ELSE media_path.verified_mtime
                END",
    )?
    .execute(params![rel, abs.to_string_lossy(), media_id])?;
    if let Some(old) = displaced {
        // Same statement as `prune_unlinked_media`, scoped to the one row this call may have stranded.
        db.0.prepare_cached(
            "DELETE FROM media WHERE id = ?1
              AND NOT EXISTS (SELECT 1 FROM media_path WHERE media_id = media.id)",
        )?
        .execute(params![old])?;
    }
    Ok(())
}

/// Remove one path, reporting how many media rows went with it.
///
/// The count is the lifecycle rule made observable: a media row lives while at least one path holds those bytes,
/// so the last unlink deletes the media row and `ON DELETE CASCADE` takes its vectors. Returning the number rather than
/// a bool lets the embed report say "1 figure removed" without a second query, and says `0` for both "one of two paths
/// went" and "this path was never linked" — a deleted file is a fact about the world, not a corrupt database.
pub fn unlink_media_path(db: &Db, rel: &[u8]) -> Result<usize, DbError> {
    // One statement rather than a SELECT followed by a DELETE: with two statements, a link landing between them gives
    // two ways to be wrong — the debug assert firing, or `Ok(0)` returned after a row was deleted. `RETURNING` makes
    // "which media row did this path hold" and "is that path gone" the same instant.
    // Again: only `QueryReturnedNoRows` means "this path was not linked". A lock or a read-only file must not come back as
    // `Ok(0)` — that is the sweep believing it pruned when it pruned nothing, and the run then reporting a corpus with no
    // stale figures.
    let holder = match db
        .0
        .prepare_cached("DELETE FROM media_path WHERE rel_bytes = ?1 RETURNING media_id")?
        .query_row(params![rel], |row| row.get::<_, i64>(0))
    {
        Ok(media_id) => Some(media_id),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(err) => return Err(err.into()),
    };
    let Some(media_id) = holder else {
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

/// Delete every media row that no path holds any more, returning how many went.
///
/// [`unlink_media_path`] cannot be the whole lifecycle rule on its own: it works from a path, so a media row that never
/// gained one — bytes recorded and then refused by the encoder, or an embed interrupted between the two writes by a
/// provider that would not load — is invisible to it forever. The rule is "a media row lives while at least one
/// live path exists", and this is the accessor that makes that statement true rather than aspirational. `ON DELETE CASCADE`
/// takes the vectors, so a swept figure cannot answer `np search`.
///
/// Idempotent by construction: the second call finds nothing, which is what lets an embed pass run it unconditionally at
/// the end rather than only when it thinks something changed.
pub fn prune_unlinked_media(db: &Db) -> Result<usize, DbError> {
    let removed = db
        .0
        .prepare_cached(
            "DELETE FROM media WHERE NOT EXISTS (SELECT 1 FROM media_path WHERE media_id = media.id)",
        )?
        .execute([])?;
    Ok(removed)
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

/// What the vector one model made for one figure was built from — its *embedding currency*.
///
/// **The stamp is `media_embeddings.source_hash` itself; there is deliberately no second column and no setter.** "The
/// bytes this vector was made from" is one fact, and a second spelling of it (a `media_stamps` table, say) is a second
/// definition of currency that can disagree with the vector next to it. The writer is therefore [`record_media_embedding`],
/// which takes the digest as an argument and cannot store a vector without one — so a vector and its stamp always arrive
/// together or not at all.
///
/// `None` means no vector for this `(media_id, model)` pair, which is the only state that owes an encode. Two models may
/// hold vectors for one figure, hence the `model` argument: asking with the wrong id would call another model's work
/// current.
///
/// **Not to be confused with [`media_path_check`]**: this answers "is the *vector* current" (a digest, per model) and that
/// one answers "has the *file* moved" (size + mtime, per path).
pub fn media_stamp(db: &Db, media_id: i64, model: &str) -> Result<Option<String>, DbError> {
    let digest =
        db.0.prepare_cached(
            "SELECT source_hash FROM media_embeddings WHERE media_id = ?1 AND model = ?2",
        )?
        .query_row(params![media_id, model], |row| row.get::<_, String>(0))
        .optional()?;
    Ok(digest)
}

/// What the last check found at one media path: the pair that justified skipping the read, and the content it stands for.
///
/// `hash` travels with the pair because the pair is worthless on its own — the caller's question is "may I use these
/// bytes without reading them", and answering it needs both "nothing moved" and "these are the bytes that were there".
/// It comes from the joined `media` row, which is why the pair lives on `media_path` at all (see migration 10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathCheck {
    /// `media.size` as recorded when the check was written.
    pub size: i64,
    /// Whole-second mtime at that moment, from [`crate::walk`]`'s` D12 clock reading.
    pub mtime: i64,
    /// The content hash the path row points at, which the pair vouches for.
    pub hash: String,
}

/// The last recorded check for one media path, or `None` when it has none — never checked, or its media row is gone.
///
/// A half pair reads as no pair at all, which is migration 8's rule and the reason this returns a struct rather than two
/// columns: trusting one present column would let an embed reuse a hash for bytes it never checked. The join to `media`
/// means a path whose media row was pruned answers `None` too, so a swept figure cannot be revived from its stamp.
pub fn media_path_check(db: &Db, rel: &[u8]) -> Result<Option<PathCheck>, DbError> {
    let row =
        db.0.prepare_cached(
            "SELECT p.verified_size, p.verified_mtime, m.hash
               FROM media_path p JOIN media m ON m.id = p.media_id
              WHERE p.rel_bytes = ?1",
        )?
        .query_row(params![rel], |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .optional()?;
    // The pair rule comes from the same `stamp_pair` every other stamp reader uses; only the hash is this table's own.
    Ok(row.and_then(|(size, mtime, hash)| {
        stamp_pair(size, mtime).map(|(size, mtime)| PathCheck { size, mtime, hash })
    }))
}

/// Record the check that justifies reading no bytes at this path on the next run.
///
/// Written only **after** the bytes were digested and the path row was pointed at them, exactly like
/// [`record_artifact_stamp`], and a no-op for a path that is gone: a figure unlinked between hashing and this write is a
/// fact about the world, not a corrupt database. The caller reads `size` from the same `stat` it read `mtime` from, and
/// `mtime` from [`crate::walk`]`'s` D12 clock — one reading of the clock for both columns.
pub fn set_media_path_check(db: &Db, rel: &[u8], size: i64, mtime: i64) -> Result<(), DbError> {
    db.0.prepare_cached(
        "UPDATE media_path SET verified_size = ?2, verified_mtime = ?3 WHERE rel_bytes = ?1",
    )?
    .execute(params![rel, size, mtime])?;
    Ok(())
}
