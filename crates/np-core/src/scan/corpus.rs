use std::collections::{BTreeMap, HashMap};

use rusqlite::params;

use crate::db::{Db, DbError, LIVE_EDGE, PROSE_ORIGIN};

use crate::links::file_name;

use crate::catalog::{self, CatalogEntry};
use crate::path_keys::RelPath;

/// `case_key` → every live row carrying that fold: the corpus [`crate::links::resolve`] places targets in.
///
/// Each entry is a [`RelPath`], so a placed target names the row it landed on rather than a spelling two
/// rows may share; matching still happens entirely on display text, which is the author's question.
///
/// A `Vec` rather than the one `rel` an earlier build kept, because `files.case_key` stopped being unique
/// with migration 4: `Ämli.md` beside `ämli.md` is two files whose display fold is one string, so
/// `insert` would have handed the fold to whichever row was read last and a link naming that fold would land
/// on it by iteration order. Listing both is what lets resolution see the ambiguity and decline, exactly as
/// [`crate::links`] already models an ambiguous basename. A fold with one owner still resolves.
pub(crate) fn live_corpus(db: &Db) -> Result<BTreeMap<String, Vec<RelPath>>, DbError> {
    let mut statement = db.0.prepare(
        "SELECT case_key, rel_bytes, rel FROM files WHERE state = 'ok'
         ORDER BY case_key, rel_bytes",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            RelPath::new(row.get::<_, Vec<u8>>(1)?, row.get::<_, String>(2)?),
        ))
    })?;
    let mut corpus: BTreeMap<String, Vec<RelPath>> = BTreeMap::new();
    for row in rows {
        let (case_key, path) = row?;
        corpus.entry(case_key).or_default().push(path);
    }
    Ok(corpus)
}

/// The catalog's rows: one per live note, with the link counts and tags it renders.
///
/// Live rows only, joined through `files`, so a stale note leaves the catalog while its rows stay in the
/// database. Tags come back in insertion order, which is the note's own order, because the
/// renderer prints them as written.
/// The join and the order are both on `rel_bytes`. On display `rel` two rows on each side of one
/// spelling is a cross product, so the catalog gained duplicate entries carrying wrong link counts; and for
/// twins that spelling is *identical*, so a display `ORDER BY` orders nothing and the rendered bytes fell to
/// whichever row SQLite happened to read.
pub(crate) fn catalog_entries(db: &Db) -> Result<Vec<CatalogEntry>, DbError> {
    let mut statement = db.0.prepare(
        "SELECT n.id, n.rel_bytes, n.rel, n.title, f.size
           FROM notes n JOIN files f ON f.rel_bytes = n.rel_bytes
          WHERE f.state = 'ok'
          ORDER BY n.rel_bytes",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(LiveNote {
            id: row.get::<_, i64>(0)?,
            rel_bytes: row.get::<_, Vec<u8>>(1)?,
            rel: row.get::<_, String>(2)?,
            title: row.get::<_, Option<String>>(3)?,
            size: row.get::<_, i64>(4)?.max(0) as u64,
        })
    })?;
    // Read out fully before the counts run, so no read is open across another statement.
    let mut live: Vec<LiveNote> = Vec::new();
    for row in rows {
        live.push(row?);
    }

    // Both counts ask for a live destination: `→n` / `←n` mean "edges landing on a live note", so an
    // edge into a note the catalog hides is not advertised as intact. The reader of such a count would
    // follow it to a note that is not in the catalog at all.
    let out = link_counts(
        db,
        &format!(
            "SELECT l.src_note_id, COUNT(*) FROM links l WHERE {LIVE_EDGE} GROUP BY l.src_note_id"
        ),
    )?;
    let inc = link_counts(
        db,
        &format!(
            "SELECT l.dst_note_id, COUNT(*) FROM links l WHERE {LIVE_EDGE} GROUP BY l.dst_note_id"
        ),
    )?;
    let tags = note_tags(db)?;

    Ok(live
        .into_iter()
        .map(|note| CatalogEntry {
            title: note
                .title
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| file_name(&note.rel).to_string()),
            size_human: catalog::size_human(note.size),
            tags: tags.get(&note.id).cloned().unwrap_or_default(),
            out: count_of(&out, note.id),
            inc: count_of(&inc, note.id),
            rel_bytes: note.rel_bytes,
            rel: note.rel,
        })
        .collect())
}

/// One live note as [`catalog_entries`] reads it, before its counts and tags are attached.
///
/// A name rather than a five-field tuple: the row now carries both spellings of its path, and which of them
/// renders (`rel`) and which only orders (`rel_bytes`) is the distinction worth keeping visible here.
struct LiveNote {
    /// The note row's id — what the link counts and tags are keyed on.
    id: i64,
    /// The byte key, for `ORDER BY` and for nothing a reader sees.
    rel_bytes: Vec<u8>,
    /// The display path, which is what the entry links to.
    rel: String,
    /// Stored title; `None` or empty means the entry takes its file name.
    title: Option<String>,
    /// File size in bytes, rendered through [`catalog::size_human`].
    size: u64,
}

/// One `SELECT note_id, COUNT(*)` pair into a lookup.
fn link_counts(db: &Db, sql: &str) -> Result<HashMap<i64, i64>, DbError> {
    let mut statement = db.0.prepare(sql)?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
    let mut counts: HashMap<i64, i64> = HashMap::new();
    for row in rows {
        let (id, count) = row?;
        counts.insert(id, count);
    }
    Ok(counts)
}

/// The stored count of a `COUNT(*)` row, as the `u32` [`CatalogEntry`] asks for. No note holds four
/// billion links; a larger number is clamped rather than wrapped.
fn count_of(counts: &HashMap<i64, i64>, id: i64) -> u32 {
    u32::try_from(counts.get(&id).copied().unwrap_or(0).max(0)).unwrap_or(u32::MAX)
}

/// Tags per note, in the order they were written.
fn note_tags(db: &Db) -> Result<HashMap<i64, Vec<String>>, DbError> {
    let mut statement =
        db.0.prepare("SELECT note_id, tag FROM tags ORDER BY note_id, rowid")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut tags: HashMap<i64, Vec<String>> = HashMap::new();
    for row in rows {
        let (id, tag) = row?;
        tags.entry(id).or_default().push(tag);
    }
    Ok(tags)
}

/// Link rows with no live note behind them, attachments excluded: the report's figure, and deliberately
/// not [`crate::db::note_counts`]'s. An embed of an image legitimately stores `dst_note_id NULL`, and
/// counting those would bury the "notes I could not place" signal (see [`crate::links::store_links`]).
///
/// A destination that exists but is stale counts here, because it is a note nobody can reach: links are
/// re-resolved only for notes this scan re-parsed, so an edge into a stale note keeps its old
/// `dst_note_id` until its source is edited. An edge into a note row that was actually *deleted* arrives
/// by the other door: migration 3's `ON DELETE SET NULL` empties it, and an empty destination counts here
/// as well. [`crate::db::unresolved_targets`] answers the list this scan
/// reports beside this number as [`ScanReport::unresolved_targets`], and both are read from one
/// transaction — that query asks the same question with the same [`LIVE_EDGE`] predicate and the same
/// [`PROSE_ORIGIN`] filter, neither spelling its own literal — an approved edge is an input, and once
/// M3 writes them the two figures must not diverge.
pub(crate) fn unresolved_links(db: &Db) -> Result<i64, DbError> {
    Ok(db.0.query_row(
        &format!(
            "SELECT COUNT(*) FROM links l
              WHERE l.origin = ?1 AND l.kind != 'attachment'
                AND NOT {LIVE_EDGE}"
        ),
        params![PROSE_ORIGIN],
        |row| row.get(0),
    )?)
}
