use crate::db::{Db, DbError};

use rusqlite::{params, OptionalExtension};

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
/// grow one to answer a question about its own table, and because
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
/// id: two processes can both pass [`crate::models`]'s "is there already a row?" step, and if the loser
/// raised `SQLITE_CONSTRAINT_PRIMARYKEY` the human would read a bare constraint violation about a table they
/// have never opened. Returning `false` says what actually happened — somebody else installed this identity,
/// and [`crate::models::install`] turns that into the same full-tuple comparison it applies to its own
/// pre-flight read, so a *different* declaration is still refused rather than papered over.
///
/// `dim` and `now` arrive as `i64` because those are SQLite's `INTEGER` columns; the range rules on a
/// declared dim (`u16`, never zero) belong to the caller that parses human input, not to the writer
/// itself. `sha256` is `None` for a provider with no local weights —
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

/// One recorded member of a model's set of files.
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
         ON CONFLICT (model, name) DO UPDATE
            -- Clearing the stamp pair is the load-bearing half. Re-recording a *different* digest — a fetch that found
            -- new bytes for this member — would otherwise leave `verified_size`/`verified_mtime` describing bytes that
            -- are no longer on disk, and D12's verify-on-change would skip hashing a 554,821,056-byte projector it has
            -- never checked.
            SET sha256 = excluded.sha256, size = excluded.size,
                verified_size = NULL, verified_mtime = NULL
            -- …but only when the declaration actually changed. `install` and a repeat fetch legitimately re-record the
            -- same bytes (§12 rule 10), and clearing then would throw away a stamp that is still true and cost a full
            -- 554 MB hash on the next image run for no reason. No row matches the WHERE on an identical re-record, so
            -- the statement becomes a no-op rather than a wipe.
            WHERE model_files.sha256 IS NOT excluded.sha256 OR model_files.size IS NOT excluded.size",
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

/// Record the stamp that justified skipping a re-hash of one recorded model file.
///
/// The projector's half of verify-on-change: without it, verifying the 554,821,056-byte `mmproj` costs a full read on
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
/// because both mean *hash it*, while a stamp of `(0, 0)` reads `Some` because it is a complete pair: zeros are what a
/// real `stat` reports for an empty file with a zero mtime, and this reader's job is "was a pair written", not "were the
/// values plausible" (nothing here stores provenance, so plausibility is not something it could check). Callers that need the first distinction ask [`model_files`] whether the member is recorded at all
/// — which is precisely what migration 9 could not express for an `install --path` model and this entry adds.
pub fn member_stamp(db: &Db, model: &str, name: &str) -> Result<Option<(i64, i64)>, DbError> {
    read_stamp_pair(
        db,
        "SELECT verified_size, verified_mtime FROM model_files WHERE model = ?1 AND name = ?2",
        params![model, name],
    )
}

/// One stamp pair, with the half-pair rule in a single place so `media_path` and `model_files` cannot drift apart.
///
/// The rule itself is [`stamp_pair`], which is also what [`media_path_check`] applies to its own three-column read — the
/// alternative was a second copy of "half a pair is no check" in that function, which is precisely the drift this helper was
/// written to prevent (and re-reading the row for a shared query would cost an indexed lookup per figure per run).
///
/// No `typeof` guard here, unlike the blob columns this file insists on: these two have INTEGER affinity, so SQLite
/// converts `'40960'` to the integer 40960 **on write** and a text stamp cannot exist to be misread — measured, in
/// `a_stamp_column_holds_only_an_integer`. The guard that matters is the pair rule below.
fn read_stamp_pair(
    db: &Db,
    sql: &str,
    args: impl rusqlite::Params,
) -> Result<Option<(i64, i64)>, DbError> {
    let pair = db.0.prepare_cached(sql)?.query_row(args, |row| {
        Ok(stamp_pair(
            row.get::<_, Option<i64>>(0)?,
            row.get::<_, Option<i64>>(1)?,
        ))
    });
    match pair {
        Ok(pair) => Ok(pair),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(DbError::from(err)),
    }
}

/// D12's half-pair rule: **one column present is not a check.**
///
/// Both stamp columns are written by one statement, so a row with only one of them was edited by hand or written by a build
/// that has since changed; treating it as a verification would skip a read on the strength of an incomplete record. Every
/// reader of a pair answers through this function, which is what lets `media_path` and `model_files` mean the same thing by
/// "unchecked".
pub(crate) fn stamp_pair(size: Option<i64>, mtime: Option<i64>) -> Option<(i64, i64)> {
    match (size, mtime) {
        (Some(size), Some(mtime)) => Some((size, mtime)),
        _ => None,
    }
}
