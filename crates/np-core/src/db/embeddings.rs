use crate::db::{Db, DbError};

use rusqlite::params;

/// Store one vector for one chunk under one model, idempotently.
///
/// **`chunk_text_digest` is the SHA-256 of that chunk's own text — never `files.hash`.** The two levels look
/// interchangeable (both `String`, both in scope during a scan) and are not: `chunks.source_hash` holds the note
/// digest, so after a `CHUNKER_VERSION` bump every row is restamped to an unchanged `files.hash` while its text may
/// have been re-cut — and a vector stamped with the note digest would then claim currency for bytes nobody has
/// embedded since. Pinned by `a_vector_stamped_with_the_note_digest_is_not_found_by_a_chunk_text_question`.
///
/// `INSERT … ON CONFLICT (chunk_id, model) DO UPDATE` rather than a plain `INSERT`, and the retry rule is the
/// reason: an embed pass *expects* its retry, so "this write already happened" is an ordinary event rather than a
/// bug. A plain insert into `PRIMARY KEY (chunk_id, model)` answers that retry with
/// `SQLITE_CONSTRAINT_PRIMARYKEY`, and because one `?` propagates out of the scan's write pass, the whole
/// scan's indexing work would roll back over one repeated vector — a provider problem turned into data
/// loss, which the rule forbids twice over: provider failure must not be data loss, and neither may our own
/// conflict handling be.
///
/// The conflict target is the **pair**, so re-embedding under a second model cannot disturb the first row —
/// Switching providers then costs a re-embed and nothing else — pinned at the schema by
/// `two_models_hold_vectors_for_one_chunk` and at the writer by `upsert_vector_keeps_both_models_rows`. `dim`, `source_hash` and `embedded_at` travel in the
/// update arm as well: a retry computed from chunk text that has since moved, or under a model whose answer
/// changed, must leave the row stating what it now holds rather than keeping the older facts beside a new
/// vector.
///
/// `bytes` is the little-endian f32 payload — [`crate::embed::Vector::to_bytes`] writes it — bound as bytes,
/// which is the only spelling migration 7's `CHECK (typeof(vector) = 'blob' …)` accepts. Nothing here
/// re-checks `bytes.len() == 4 * dim`: the `CHECK` does it at the write, and `Vector::from_bytes` is its
/// twin on the read side. An unknown `model` is refused by the foreign key rather than looked up here,
/// which is why `models` must be written before any vector naming it.
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
/// stays — keeping its ordinal and its `chunks.id`, which is what [`crate::db::write_chunks_for_note`] exists to
/// preserve — while its vectors do not. Deleting nothing is not an error: a chunk that was never embedded
/// has no rows, and "no vectors behind it" is a fact rather than a missing thing.
///
/// Separate from [`crate::db::write_chunks_for_note`] on purpose, because the two tables are written in different
/// transactions: chunk writes ride the scan's transaction, while a vector batch commits outside it.
pub fn delete_vectors_for_chunk(db: &Db, chunk_id: i64) -> Result<(), DbError> {
    db.0.prepare_cached("DELETE FROM embeddings WHERE chunk_id = ?1")?
        .execute(params![chunk_id])?;
    Ok(())
}

/// One stored vector's currency stamp under one model: the chunk it belongs to, and the digest of the bytes
/// that were handed to the model to make it.
///
/// Only those two fields, because they are the whole answer to "is this vector still current": the rule is
/// that `embeddings.source_hash` holds the digest of **the bytes the model read**, so currency is
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
/// — every read filters on an explicit model id, and for a long time no such reader existed:
/// `PRIMARY KEY (chunk_id, model)` lets two models hold a vector for one chunk, and comparing
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
