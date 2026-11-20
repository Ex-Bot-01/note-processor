use crate::chunk::{Chunk, CHUNKER_VERSION};
use crate::db::{Db, DbError};

use rusqlite::params;

/// What one call to [`write_chunks_for_note`] touched, as counts of **rows touched**.
///
/// Each field counts rows whose stored content changed, not chunks that were looked at: a chunk already on
/// file exactly as this build would write it appears in none of the three numbers. That is the shape the warm
/// pass is about — an unchanged note answers [`ChunkWrite::default()`] — and a report claiming work it did not
/// do is the lie this module's own doc refuses.
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
/// reason: `embeddings.chunk_id` cascades off `chunks.id`, so the
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
/// *identifiable* rather than presumed stale, and `source_hash = note_digest` — the digest of the
/// **note** bytes the chunks were cut from. That is one level coarser than `embeddings.source_hash`, and the
/// two levels are by design: a heading edit in paragraph 9 leaves chunk 2's own text alone, and that fact
/// belongs to the vector row, never here. Stamping the digest onto rows whose text did not move is deliberate
/// rather than sloppiness: it is what makes the cheap path below reachable on the *next* scan, which is the
/// rule that an unchanged note costs one indexed lookup per scan. Its cost is one `UPDATE` per unchanged row on the
/// one scan that notices an edit, and it touches no vector — this function never writes `embeddings`.
///
/// `Chunk::text` travels as bytes (`&[u8]`), never as `String` or `&str`: a text bind arrives at SQLite as
/// TEXT, migration 7's `CHECK (typeof(text) = 'blob')` refuses it, and BLOB is an affinity rather than a
/// constraint — the split-type bug, kept armed by `a_string_bind_is_refused_from_chunks_text`.
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
/// That is a report claiming work it did not do, which is the lie this module's own doc refuses.
///
/// ## Transaction
///
/// None of its own: chunk writes belong to whichever transaction the caller is in, which for the scan is
/// its single write transaction, on the rule that chunk writes stay in the transaction — they are cheap and
/// must be atomic with the note rows. A failure mid-diff therefore rolls back alongside the note rows it
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
    /// Read back as bytes because the column is a BLOB holding note bytes: reading it into a
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
/// The reader the staleness rule needs. A caller compares a fresh note digest against the first element to
/// decide whether this note needs any chunk work at all, and compares each stored text against the chunk it
/// just cut to decide which ordinals moved; both answers come from one indexed pass over one note's rows.
///
/// - **Ordering.** Texts come back ascending by `ordinal`, which is [`crate::chunk`]'s own order. The
///   `ORDER BY` lives in the statement rather than being left to the planner, for the reason
///   `rebuild_with_keys` (a code span; that function is private) gives about rowid order.
/// - **The digest arm is `Some` only when there is one answer.** Every row must carry the same
///   `source_hash`, and every row must carry the current [`CHUNKER_VERSION`]; otherwise these rows support
///   no single statement and the caller has to re-chunk. Folding the chunker in follows from this signature
///   having no third slot — and rows cut by an older chunker must never read as current, since the version
///   stamp lives per row precisely so a bump invalidates them. A note with no rows has no digest either: nothing has
///   been cut from it yet, which is the same answer as "stale" and costs the caller one
///   [`write_chunks_for_note`] call.
/// - **A row this program cannot have written.** `chunks.ordinal` is a SQLite INTEGER, so a hand-edited file
///   could hold a value above `u32::MAX`. [`crate::chunk::chunk`] refuses to mint one, so the honest reading
///   is "not ours, not current": the digest arm comes back `None` and that row is reported at `u32::MAX`,
///   which keeps it visible instead of silently dropping a row from the list.
///
/// Texts are `Vec<u8>` because the column holds note bytes, not a promise about UTF-8.
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
/// This is the scan's per-note decision, and
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

/// One chunk the catalog can still show a reader: its id and its stored text.
///
/// The `files.state = 'ok'` join is the liveness fence that keeps a stale note out. A note whose file is gone keeps its row
/// with `state = 'stale'` and every child row intact — [`crate::db::notes::mark_stale`] says so in its own words — so an
/// un-fenced read over `chunks` is the first production reader that computes vectors for a deleted file's text
/// and reports them current. Joining through `notes.rel_bytes = files.rel_bytes` rather than `rel` because the
/// byte key is the identity: an ASCII-lossy display spelling could fold two paths into
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

/// Every live note's workspace-relative path, in path order.
///
/// **This is a second query with one predicate, not a second definition of "live".** [`live_chunks`] owns the
/// definition — `JOIN files f ON f.rel_bytes = n.rel_bytes WHERE f.state = 'ok'` — and this repeats that single
/// join and that single predicate so the media pass walks exactly the notes whose chunks the fence admitted. If
/// the fence ever widens or narrows, [`live_chunks`] is the owner a change has to be made in; a note counted live
/// here and stale there would embed images for a deleted file.
///
/// Ordered by `rel_bytes` so one run walks notes in the same order on every host and a report's refusal list is
/// reproducible. Bytes rather than strings because a non-UTF-8 note name is a live note.
pub fn live_note_paths(db: &Db) -> Result<Vec<Vec<u8>>, DbError> {
    let mut statement = db.0.prepare_cached(
        "SELECT n.rel_bytes
           FROM notes n
           JOIN files f ON f.rel_bytes = n.rel_bytes
          WHERE f.state = 'ok'
          ORDER BY n.rel_bytes",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut paths = Vec::new();
    for row in rows {
        paths.push(row?);
    }
    Ok(paths)
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
