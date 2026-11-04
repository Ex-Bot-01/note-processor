//! `np search`: a read-only cosine top-k over both vector sources of one model
//! (`docs/design-m2-image-embeddings.md` §12).
//!
//! **This module writes nothing.** No migration, no `INSERT`, no `UPDATE`, no stamp — not even a benign-looking
//! `last_seen`. A command that cannot write cannot drift out of sync with the index it reads, which is the whole reason
//! §12 keeps it a proof of concept rather than folding ranking into `np scan`. Every statement below is a `SELECT`, and
//! [`no_write_statement_reaches_the_database`] pins the rule by running [`search`] against a connection whose write
//! calls are refused.
//!
//! **Ordering is part of the contract, not a side effect of the planner.** Hits come back by descending score, and a tie
//! breaks on `Hit::path` ascending — so a stable index answers the same query with the same list on every host and after
//! every re-embed. Ordering by rowid would do the opposite: re-embedding one note moves its `chunks.id`, and a hit that
//! was second could become first without a single score changing (§6's argument for the same rule on
//! [`Hit::source_note']).

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::params;

use crate::db::{self, Db, DbError};
use crate::embed::{EmbedError, Vector, COMPONENT_BYTES};
use crate::media;

/// What a hit is: a note's prose, or an image a note links (§12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    /// A note, scored by its best-matching chunk.
    Note,
    /// An image in `media`, scored by its one vector.
    Image,
}

impl HitKind {
    /// The one-word spelling [`search`] prints and `--json` emits, spelled once so text and JSON cannot disagree.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Image => "image",
        }
    }
}

impl std::fmt::Display for HitKind {
    /// The same word as [`HitKind::label`], so an error message and a report line cannot drift apart.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One ranked answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Cosine similarity in `[-1, 1]`, computed on the vectors **as stored** — nothing here re-normalises an index row.
    ///
    /// The text line prints this to four decimals; `--json` carries the full `f64`. Two spellings of one number is the
    /// same trade every report in this program makes: a reader wants an aligned column, a machine wants no rounding.
    pub score: f64,
    /// Which table the vector came from.
    pub kind: HitKind,
    /// Workspace-relative path, lossy-decoded for display (F20's rule: bytes are the identity, text is a rendering).
    ///
    /// For an image this is the lexicographically smallest `media_path.rel_bytes` holding those bytes, so one figure
    /// copied to three folders names one path and answers once (§6's content-identity rule).
    pub path: String,
    /// For an image hit, the note to open: the lexicographically smallest **live** note linking that path (§6).
    ///
    /// `None` for a note hit, and for an image whose linkers no live note row names — which happens when the only
    /// spelling of a figure's name in `links.target` is a lossy decode of bytes that never round-trip. Undercounting a
    /// linker is a reportable fact; inventing one is not.
    pub source_note: Option<String>,
    /// How many live notes link this image; always `0` for a note hit.
    pub linked_from: usize,
}

/// Why a search could not answer.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The query vector is not unit length.
    ///
    /// Refused rather than silently rescaled: a caller that forgot to normalise would otherwise get plausible-looking
    /// scores whose ranking depends on the query's magnitude, and no output line says so.
    #[error("query vector has length {norm} but search compares unit vectors; normalise it (a provider's own output is already unit length)")]
    NotUnitNorm {
        /// The Euclidean length that was actually measured.
        norm: f64,
    },
    /// No `models` row answers to that id.
    #[error("no model named \"{model}\" is installed in this workspace; run np models list to see the ids this workspace has")]
    ModelAbsent {
        /// The id that was asked for, verbatim.
        model: String,
    },
    /// The model indexes one kind and not the other, so a blended top-k would be a lie.
    ///
    /// §12 ranks notes and images in **one** list. Half an index can still do that — but it can only ever show one kind,
    /// which reads as "this workspace has no figures" when the truth is "nobody embedded them". Refusing names the id and
    /// the missing kind, per §8's rule that a refusal says what to change.
    #[error("model \"{model}\" has {present} vectors but no {missing} vectors; run np embed with a build that can read images (or embed the notes) before searching both kinds together")]
    HalfIndexed {
        /// The id that was asked for, verbatim.
        model: String,
        /// Which kind is present, so the message never says "nothing".
        present: HitKind,
        /// Which kind is missing.
        missing: HitKind,
    },
    /// A stored row's width disagrees with the query's.
    #[error("model \"{model}\" stores {stored}-dimensional vectors and the query is {query}-dimensional; re-embed under one model or search with the other")]
    DimensionMismatch {
        /// The id whose rows were read.
        model: String,
        /// Width of the stored rows.
        stored: usize,
        /// Width of the query.
        query: usize,
    },
    /// The query bytes are not a vector at all (a ragged buffer, or a width no `dim` can name).
    #[error("query vector: {0}")]
    QueryVector(#[from] EmbedError),
    /// A stored row cannot be decoded at the width its own `dim` declares.
    #[error("model \"{model}\" holds a {dim}-dimensional row that does not read as a vector: {reason}")]
    StoredRow {
        /// The id whose rows were read.
        model: String,
        /// The width the row declares.
        dim: usize,
        /// Why the bytes are not that width.
        reason: EmbedError,
    },
    /// The database could not be read.
    #[error("database: {0}")]
    Db(#[from] DbError),
}

/// Rank every vector this model stores against `query_vector` and return at most `limit` hits.
///
/// * **Read-only.** See the module note; nothing here can write.
/// * **One hit per note, its best chunk winning.** `embeddings` is keyed per chunk, so a note with six chunks would
///   otherwise fill a ten-hit list by itself. The score reported is the maximum over that note's live chunks — the chunk
///   identity is deliberately not reported (§12 shows a path, and §6 keeps a note's identity at its path).
/// * **Liveness fenced exactly like [`db::live_chunks`]**: `JOIN files f ON f.rel_bytes = n.rel_bytes WHERE f.state =
///   'ok'`. A deleted note keeps its rows and its vectors (D3), so an un-fenced read would rank a file no catalog can
///   show. Media rows need no fence of their own: [`db::prune_unlinked_media`] already took any row no live path holds.
/// * **Vectors compared as stored.** Stored rows are not re-normalised (the encoder wrote unit rows, and re-scaling
///   would hide a corrupt one); the query's length is asserted instead — see [`SearchError::NotUnitNorm`].
/// * **Empty is a fact.** Ok(empty) means this model stores no vectors of either kind; the caller says so and exits 0.
///   Half an index, or an id nobody installed, is a refusal.
/// * `limit` is a maximum: `0` returns nothing, more than the population returns everything.
///
/// # Ordering
///
/// Descending `score`, ties broken by `path` ascending. See the module note for why that is load-bearing.
pub fn search(
    conn: &Db,
    model: &str,
    query_vector: &[u8],
    limit: usize,
) -> Result<Vec<Hit>, SearchError> {
    let query = decode_query(conn, model, query_vector)?;

    let notes = note_hits(conn, model, &query)?;
    let images = image_hits(conn, model, &query)?;

    if notes.is_empty() && images.is_empty() {
        // Nothing at all under this id: an empty index is a fact (§12), and the caller prints it.
        return Ok(Vec::new());
    }
    if notes.is_empty() || images.is_empty() {
        let (present, missing) = if notes.is_empty() {
            (HitKind::Image, HitKind::Note)
        } else {
            (HitKind::Note, HitKind::Image)
        };
        return Err(SearchError::HalfIndexed {
            model: model.to_string(),
            present,
            missing,
        });
    }

    let mut hits = notes;
    hits.extend(images);
    // Descending score, then ascending path. `total_cmp` rather than `partial_cmp` because a score is never `NaN` by
    // construction (see [`cosine`]) and an `Option<Ordering>` in a sort key is a place for a silent `Equal`.
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
    });
    hits.truncate(limit);
    Ok(hits)
}

/// The query as components, after the checks that make a comparison meaningful.
fn decode_query(conn: &Db, model: &str, query_vector: &[u8]) -> Result<Vector, SearchError> {
    // A ragged buffer is [`EmbedError::Malformed`] rather than a width disagreement: it cannot describe *any* vector, so
    // blaming the index's dimensions would point at the wrong thing (`Vector::from_bytes` makes exactly that distinction).
    let dim = query_vector.len() / COMPONENT_BYTES;
    let query = Vector::from_bytes(query_vector, dim)?;
    let norm = norm_of(query.components());
    // 1e-3 is the tolerance the encoder seam uses for its own unit promise (`crate::vision`), so a caller that
    // normalised through either path passes both checks or neither.
    if (norm - 1.0).abs() > 1e-3 {
        return Err(SearchError::NotUnitNorm { norm });
    }
    // The `models` row is what makes an id meaningful at all; a search against an uninstalled id must name it rather
    // than report an empty index that belongs to no model.
    if db::model_row(conn, model)?.is_none() {
        return Err(SearchError::ModelAbsent {
            model: model.to_string(),
        });
    }
    Ok(query)
}

/// Cosine of two rows, `0.0` when either has no length.
///
/// A zero-length stored row is not something the encoder writes — but a hand-written fixture or a half-written blob can,
/// and dividing by zero would put `NaN` into the sort key, where it compares unordered against everything and quietly
/// decides the order of the whole list.
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        // Callers check widths first; this arm keeps a future caller from getting a partial dot product instead.
        return 0.0;
    }
    let mut dot = 0f64;
    let mut norm_a = 0f64;
    let mut norm_b = 0f64;
    for (x, y) in a.iter().zip(b) {
        let x = f64::from(*x);
        let y = f64::from(*y);
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Euclidean length of a component list.
fn norm_of(components: &[f32]) -> f64 {
    let mut sum = 0f64;
    for c in components {
        sum += f64::from(*c) * f64::from(*c);
    }
    sum.sqrt()
}

/// One hit per live note with at least one vector under `model`, scored by its best chunk.
fn note_hits(conn: &Db, model: &str, query: &Vector) -> Result<Vec<Hit>, SearchError> {
    let mut statement = conn.0.prepare_cached(
        "SELECT n.rel_bytes, e.dim, e.vector
           FROM embeddings e
           JOIN chunks c ON c.id = e.chunk_id
           JOIN notes n ON n.id = c.note_id
           JOIN files f ON f.rel_bytes = n.rel_bytes
          WHERE e.model = ?1 AND f.state = 'ok'
          ORDER BY n.rel_bytes, c.id",
    ).map_err(DbError::from)?;
    let rows = statement.query_map(rusqlite::params![model], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    }).map_err(DbError::from)?;
    // Best score per path. A BTreeMap keyed by the display path is what makes the tie rule cheap to state: two live note
    // rows that lossy-decode identically (F13 allows it) share one display path and therefore one hit, which is the same
    // collapse §6's `Hit::path` already implies.
    let mut best: BTreeMap<String, f64> = BTreeMap::new();
    for row in rows {
        let (rel_bytes, dim, bytes) = row.map_err(DbError::from)?;
        let dim = usize::try_from(dim).unwrap_or(usize::MAX);
        if dim != query.dim() || bytes.len() != dim * COMPONENT_BYTES {
            return Err(SearchError::DimensionMismatch {
                model: model.to_string(),
                stored: dim,
                query: query.dim(),
            });
        }
        // The width check above is what makes this decode total; the arm stays because a row written by a future
        // migration is not this reader's problem to guess at.
        let stored = Vector::from_bytes(&bytes, dim).map_err(|err| SearchError::StoredRow {
            model: model.to_string(),
            dim,
            reason: err,
        })?;
        let score = cosine(query.components(), stored.components());
        let path = String::from_utf8_lossy(&rel_bytes).to_string();
        best.entry(path)
            .and_modify(|held| {
                if score > *held {
                    *held = score;
                }
            })
            .or_insert(score);
    }
    Ok(best
        .into_iter()
        .map(|(path, score)| Hit {
            score,
            kind: HitKind::Note,
            path,
            source_note: None,
            linked_from: 0,
        })
        .collect())
}

/// One hit per image this model has a vector for.
fn image_hits(conn: &Db, model: &str, query: &Vector) -> Result<Vec<Hit>, SearchError> {
    let linkers = linker_index(conn)?;
    let mut statement = conn.0.prepare_cached(
        "SELECT e.media_id, e.dim, e.vector
           FROM media_embeddings e
          WHERE e.model = ?1
          ORDER BY e.media_id",
    ).map_err(DbError::from)?;
    let rows = statement.query_map(rusqlite::params![model], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    }).map_err(DbError::from)?;
    let mut hits = Vec::new();
    for row in rows {
        let (media_id, dim, bytes) = row.map_err(DbError::from)?;
        let dim = usize::try_from(dim).unwrap_or(usize::MAX);
        if dim != query.dim() || bytes.len() != dim * COMPONENT_BYTES {
            return Err(SearchError::DimensionMismatch {
                model: model.to_string(),
                stored: dim,
                query: query.dim(),
            });
        }
        // The width check above is what makes this decode total; the arm stays because a row written by a future
        // migration is not this reader's problem to guess at.
        let stored = Vector::from_bytes(&bytes, dim).map_err(|err| SearchError::StoredRow {
            model: model.to_string(),
            dim,
            reason: err,
        })?;
        let Some((path, source_note, linked_from)) = media_report(conn, media_id, &linkers)? else {
            // A media row with no path row is a row `prune_unlinked_media` has not reached yet (a run that skipped its
            // sweep). There is no workspace path to name, so there is nothing a reader could open: report nothing rather
            // than a hit whose path column is empty.
            continue;
        };
        hits.push(Hit {
            score: cosine(query.components(), stored.components()),
            kind: HitKind::Image,
            path,
            source_note,
            linked_from,
        });
    }
    Ok(hits)
}

/// `(display path, smallest live linker, linker count)` for one media row, or `None` when it holds no path.
///
/// Both the path and the note are chosen by **smallest bytes**, not by insertion order: `media_path` is keyed by
/// `rel_bytes` and `links` by note id, and either could come back in a different sequence after a rename or a re-scan.
fn media_report(
    conn: &Db,
    media_id: i64,
    linkers: &Linkers,
) -> Result<Option<(String, Option<String>, usize)>, SearchError> {
    let mut statement = conn.0.prepare_cached(
        "SELECT rel_bytes FROM media_path WHERE media_id = ?1 ORDER BY rel_bytes",
    ).map_err(DbError::from)?;
    let rows = statement
        .query_map(params![media_id], |row| row.get::<_, Vec<u8>>(0))
        .map_err(DbError::from)?;
    let mut paths: Vec<Vec<u8>> = Vec::new();
    for row in rows {
        paths.push(row.map_err(DbError::from)?);
    }
    // `ORDER BY rel_bytes` already did the work; `first` is the smallest, byte-for-byte.
    let Some(smallest) = paths.first() else {
        return Ok(None);
    };
    // Every path those bytes were seen at contributes its linkers: one figure copied to two folders, linked from three
    // notes, is one hit with `linked_from: 3` (§6), not two hits with 2 and 1.
    let mut notes: BTreeSet<Vec<u8>> = BTreeSet::new();
    for path in &paths {
        if let Some(holders) = linkers.get(path) {
            notes.extend(holders.iter().cloned());
        }
    }
    let source_note = notes
        .first()
        .map(|note| String::from_utf8_lossy(note).to_string());
    let linked_from = notes.len();
    Ok(Some((
        String::from_utf8_lossy(smallest).to_string(),
        source_note,
        linked_from,
    )))
}

/// Which live notes link each workspace figure path.
///
/// **Why this reads `links` rather than the files.** §6 rules `links` out as the *resolver* — its `target` is TEXT, so a
/// name that is not valid UTF-8 arrives already lossy-decoded, and it is not relative to the linking note's directory.
/// Neither half of that objection binds here: the second is fixed by re-running the one resolution rule the embed pass
/// uses ([`media::link_target_rel`], which takes the note's own `rel_bytes`), and the first only costs a linker for a
/// name whose bytes cannot round-trip through TEXT, which [`Hit::source_note`] documents as an undercount. The
/// alternatives were worse: re-reading every note body needs the workspace root, which this signature does not carry and
/// §12's read-only command should not need; a `note → media` edge would be an eleventh migration, which Task 6 forbids.
///
/// Live means what [`db::live_chunks`] means by it — `files.state = 'ok'`, joined on the byte key — and only `prose`
/// origins count, because an approved edge is a claim a human made about a note that may no longer exist.
fn linker_index(conn: &Db) -> Result<Linkers, SearchError> {
    let mut statement = conn.0.prepare_cached(
        "SELECT n.rel_bytes, l.target
           FROM links l
           JOIN notes n ON n.id = l.src_note_id
           JOIN files f ON f.rel_bytes = n.rel_bytes
          WHERE l.origin = ?1 AND l.kind = 'attachment' AND f.state = 'ok'
          ORDER BY n.rel_bytes, l.line",
    ).map_err(DbError::from)?;
    let rows = statement.query_map(params![db::PROSE_ORIGIN], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
    }).map_err(DbError::from)?;
    let style = media::PathStyle::native();
    let mut index: BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>> = BTreeMap::new();
    for row in rows {
        let (note_rel, target) = row.map_err(DbError::from)?;
        // The same rule the embed pass and its sweep apply, so "which note links this figure" has exactly one owner and
        // a hit cannot name a note whose link would not have embedded the figure in the first place.
        let Some(rel) = media::link_target_rel(style, &note_rel, target.as_bytes()) else {
            continue;
        };
        index.entry(rel).or_default().insert(note_rel);
    }
    Ok(index)
}

/// Every live note that links one workspace figure path, by path.
type Linkers = BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>;
