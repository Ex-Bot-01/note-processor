//! `np search`: a read-only cosine top-k over both vector sources of one model
//! (`docs/design-m2-image-embeddings.md` §12).
//!
//! **This module writes nothing.** No migration, no `INSERT`, no `UPDATE`, no stamp — not even a benign-looking
//! `last_seen`. A command that cannot write cannot drift out of sync with the index it reads, which is the whole reason
//! §12 keeps it a proof of concept rather than folding ranking into `np scan`. Every statement below is a `SELECT`, and
//! `searching_writes_nothing` in `tests/search.rs` pins the rule (a test name is not a documentation target, so it is
//! named in plain text rather than linked) by running [`search`] on a connection where every
//! write fails with `SQLITE_READONLY` (`PRAGMA query_only`).
//!
//! **Ordering is part of the contract, not a side effect of the planner.** Hits come back by descending score, and a tie
//! breaks on `Hit::path` ascending — so a stable index answers the same query with the same list on every host and after
//! every re-embed. Ordering by rowid would do the opposite: re-embedding one note moves its `chunks.id`, and a hit that
//! was second could become first without a single score changing (§6's argument for the same rule on
//! [`Hit::source_note`]).

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::params;

use crate::db::{self, Db, DbError};
use crate::embed::{EmbedError, Vector, COMPONENT_BYTES};
use crate::media;

/// What a hit is: a note's prose, or an image a note links (§12).
/// What a hit points at.
///
/// The derive order (`Note`, `Image`) is the search tie-break's last resort after path and stored bytes, so it is part of the
/// ordering contract rather than a coincidence of declaration — see [`search`]'s "Ordering" note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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
    /// The note to open: the lexicographically smallest **live** note linking that path (§6) — which is to say,
    /// [`Hit::linked_notes`]`[0]`, assembled by the one constructor that builds an image hit and never chosen
    /// independently. (That constructor is private on purpose, so this says what it is rather than linking to it.)
    ///
    /// `None` for a note hit, and for an image whose linkers no live note row names — which happens when the only
    /// spelling of a figure's name in `links.target` is a lossy decode of bytes that never round-trip. Undercounting a
    /// linker is a reportable fact; inventing one is not.
    pub source_note: Option<String>,
    /// Every live note linking this image, byte-ordered — the relation itself rather than a sample of it (F28).
    ///
    /// **Why this exists next to the two fields above.** §6 picked one note because a text line has one column, and
    /// `linked_from` counted the rest; together they answered "how many" and "open this", and discarded everything in
    /// between. A figure linked from four notes rendered as one name and the number 4, so the three nobody named were
    /// invisible to the reader trying to work out why the hit came back. This is the set; `source_note` is its head and
    /// `linked_from` its length, both built in one place so they cannot disagree, and
    /// `tests/search.rs`'s `a_hits_linker_fields_cannot_disagree_with_each_other` fails if that ever stops being true.
    ///
    /// Empty for a note hit and for an orphaned figure — never a one-element list holding a null.
    pub linked_notes: Vec<String>,
    /// How many live notes link this image, always `0` for a note hit: [`Hit::linked_notes`]`().len()`.
    pub linked_from: usize,
}

impl Hit {
    /// One hit for a figure, with its linker set. The only place `source_note`, `linked_notes` and `linked_from` are
    /// written together, which is what makes them one fact rather than three that happen to agree — the head and the
    /// length of a list cannot drift from it while there is one statement computing all three.
    fn image(score: f64, path: String, notes: &[Vec<u8>]) -> Hit {
        // Lossy-decoded on the way out for the same reason `path` is (F20: bytes are the identity, text is a rendering).
        let linked_notes: Vec<String> = notes
            .iter()
            .map(|note| String::from_utf8_lossy(note).to_string())
            .collect();
        let source_note = linked_notes.first().cloned();
        let linked_from = linked_notes.len();
        Hit {
            score,
            kind: HitKind::Image,
            path,
            source_note,
            linked_notes,
            linked_from,
        }
    }

    /// One hit for a note. Takes no linker arguments at all: a note links things, it is not linked *by* things, so the
    /// three fields above are empty here by construction rather than by each caller remembering to zero them.
    fn note(score: f64, path: String) -> Hit {
        Hit {
            score,
            kind: HitKind::Note,
            path,
            source_note: None,
            linked_notes: Vec::new(),
            linked_from: 0,
        }
    }
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
    /// The configured model id holds no vectors while this database holds some under a **different** id.
    ///
    /// This is the mismatch §12's exit-1 refusal is for: a stale `[embeddings] model`, a typo, or an index built before a
    /// re-install. It is not "nothing indexed" — somebody did embed this workspace, and answering with an empty list would
    /// send the reader to `np embed` for nothing. The message names the configured id and what to change, never another
    /// model's artifact path or URL (§8's rule 8).
    #[error("no vectors indexed for model \"{model}\" while this index holds vectors under another model id; run np models list to see the ids, then np embed (or point [embeddings] model at the one that was embedded)")]
    NoVectorsForModel {
        /// The configured id that answered with nothing.
        model: String,
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
    #[error(
        "model \"{model}\" holds a {dim}-dimensional row that does not read as a vector: {reason}"
    )]
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
///   show. Media rows get no fence here, and the reason is worth stating exactly: [`db::prune_unlinked_media`] deletes media
///   rows that hold **no path row at all**, so a figure whose last linking note was deleted still has its path row and its
///   vector until an embed pass runs its sweep. A stale figure can therefore be ranked by this command; retiring it is the
///   writer's job (§6's lifecycle), and a read-only reader cannot run a sweep without breaking the rule above.
/// * **Vectors compared as stored.** Stored rows are not re-normalised (the encoder wrote unit rows, and re-scaling
///   would hide a corrupt one); the query's length is asserted instead — see [`SearchError::NotUnitNorm`].
/// * **Empty is a fact, and coverage is not a condition.** Search never embeds anything itself, so it answers from
///   whichever kinds this model has: note-only, image-only, or a blend of both. Ok(empty) means no vectors under this id
///   and none under any other either — the caller says "nothing indexed" and exits 0. Vectors exist under a **different**
///   id, which is the case a reader can act on, is [`SearchError::NoVectorsForModel`].
/// * `limit` is a maximum: `0` returns nothing, more than the population returns everything.
///
/// # Ordering
///
/// Descending `score`, then `path` ascending, then kind, then the stored path bytes — a total order with no fallback to row
/// arrival order. See the module note for why that is load-bearing.
pub fn search(
    conn: &Db,
    model: &str,
    query_vector: &[u8],
    limit: usize,
) -> Result<Vec<Hit>, SearchError> {
    let query = decode_query(query_vector)?;

    let notes = note_hits(conn, model, &query)?;
    let images = image_hits(conn, model, &query)?;

    if notes.is_empty() && images.is_empty() {
        // **Which kind is indexed is not a question search asks.** It never embeds anything itself, so it can only answer
        // from the half that is indexed — note-only, image-only, or both — and that is exactly why every [`Hit`] carries a
        // `kind`. What is still worth distinguishing is *whose* vectors they are: nothing indexed anywhere is a fact the
        // caller prints and exits 0 (§12), while an index that holds vectors under some other id means this command was
        // pointed at a model nobody embedded, which is a refusal.
        if any_vectors_exist(conn)? {
            return Err(SearchError::NoVectorsForModel {
                model: model.to_string(),
            });
        }
        return Ok(Vec::new());
    }

    let mut ranked = notes;
    ranked.extend(images);
    // Descending score, then ascending path, then kind, then the stored bytes. `total_cmp` rather than `partial_cmp` because
    // a score is never non-finite once [`stored_vector`] has decoded the row it came from (a `NaN` component compares
    // unordered, and an unordered head silently decides the order of everything under it); an `Option<Ordering>` in a sort
    // key would be a place for a silent `Equal`. The last two keys exist because the display path is lossy: two rows can
    // share one rendered path while being different files, and without a byte-level key their order is arrival order —
    // `a_tie_between_two_identical_hits_does_not_fall_back_to_row_order` kills that mutant. The **kind** key is the one part
    // of this comparator no case can reach: it decides only between a note and a figure at the same rendered path, which a
    // real corpus cannot produce (notes are `.md`, figures are png/jpg) and no fixture finds worth writing. It keeps the
    // order total; it is not tested, and says so rather than borrowing the neighbouring case's proof.
    ranked.sort_by(|a, b| {
        b.hit
            .score
            .total_cmp(&a.hit.score)
            .then_with(|| a.hit.path.cmp(&b.hit.path))
            .then_with(|| a.hit.kind.cmp(&b.hit.kind))
            .then_with(|| a.key.cmp(&b.key))
    });
    ranked.truncate(limit);
    Ok(ranked.into_iter().map(|ranked| ranked.hit).collect())
}

/// A hit plus the one key its order needs that [`Hit`] does not carry: the path as stored, before it was rendered.
struct Ranked {
    hit: Hit,
    /// Raw `rel_bytes` / `media_path.rel_bytes`. Kept beside the hit rather than folded into it because two distinct byte
    /// paths can render to one string, and then this is the only thing that separates them.
    key: Vec<u8>,
}

/// The query as components, after the check that makes a comparison meaningful.
fn decode_query(query_vector: &[u8]) -> Result<Vector, SearchError> {
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
    Ok(query)
}

/// Whether this database holds a single vector under **any** model id.
///
/// Two `EXISTS` probes rather than a join or a count: each stops at the first row, and "somebody embedded this
/// workspace, just not under the id you asked with" is all the caller needs to know.
fn any_vectors_exist(conn: &Db) -> Result<bool, SearchError> {
    // **Read the projected value; do not use rusqlite's `exists()` here.** That helper answers "did this query return one or
    // more *rows*" (`statement.rs`: `rows.next()?.is_some()`), and a `SELECT EXISTS(...) OR EXISTS(...)` projection returns
    // exactly one row even when both subqueries are false — so it answered true for every empty index and an unembedded
    // workspace was refused instead of reporting "nothing indexed". Reading the integer is the question actually being asked.
    let mut held = conn
        .0
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM embeddings) OR EXISTS(SELECT 1 FROM media_embeddings)",
        )
        .map_err(DbError::from)?;
    let value: i64 = held
        .query_row([], |row| row.get::<_, i64>(0))
        .map_err(DbError::from)?;
    Ok(value != 0)
}

/// A stored row decoded, with its components proven finite.
///
/// The extra step over [`Vector::from_bytes`] is not paranoia about the encoder: the schema's `CHECK` on a vector column
/// tests byte length and `typeof`, never value, so a row can hold a `NaN` or an infinity and read as a perfectly shaped
/// vector. Ranking one would put a non-finite score in the list — see [`search`]'s ordering note — and "which row is corrupt"
/// is a fact the reader needs now rather than an ordering accident to explain later, so it is refused as
/// [`SearchError::StoredRow`] naming the model and width.
fn stored_vector(bytes: &[u8], dim: usize, model: &str) -> Result<Vector, SearchError> {
    let stored = Vector::from_bytes(bytes, dim).map_err(|err| SearchError::StoredRow {
        model: model.to_string(),
        dim,
        reason: err,
    })?;
    if let Some(index) = stored
        .components()
        .iter()
        .position(|component| !component.is_finite())
    {
        return Err(SearchError::StoredRow {
            model: model.to_string(),
            dim,
            reason: EmbedError::NonFiniteComponent { index },
        });
    }
    Ok(stored)
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
fn note_hits(conn: &Db, model: &str, query: &Vector) -> Result<Vec<Ranked>, SearchError> {
    let mut statement = conn
        .0
        .prepare_cached(
            "SELECT n.rel_bytes, e.dim, e.vector
           FROM embeddings e
           JOIN chunks c ON c.id = e.chunk_id
           JOIN notes n ON n.id = c.note_id
           JOIN files f ON f.rel_bytes = n.rel_bytes
          WHERE e.model = ?1 AND f.state = 'ok'
          ORDER BY n.rel_bytes, c.id",
        )
        .map_err(DbError::from)?;
    let rows = statement
        .query_map(rusqlite::params![model], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(DbError::from)?;
    // Best score per note, keyed by the **stored bytes** of its path. Keying on the rendered form was the first version, and
    // it is wrong in a way the display hides: two files whose names differ only in bytes that are not valid UTF-8 (F13 lets
    // them exist) lossy-decode to one string, so two unrelated notes would fold into one hit reporting the higher of their
    // two scores — a merged row no note owns.
    let mut best: BTreeMap<Vec<u8>, f64> = BTreeMap::new();
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
        let stored = stored_vector(&bytes, dim, model)?;
        let score = cosine(query.components(), stored.components());
        best.entry(rel_bytes)
            .and_modify(|held| {
                if score > *held {
                    *held = score;
                }
            })
            .or_insert(score);
    }
    Ok(best
        .into_iter()
        .map(|(key, score)| Ranked {
            hit: Hit::note(score, String::from_utf8_lossy(&key).to_string()),
            key,
        })
        .collect())
}

/// One hit per image this model has a vector for.
fn image_hits(conn: &Db, model: &str, query: &Vector) -> Result<Vec<Ranked>, SearchError> {
    let linkers = linker_index(conn)?;
    let mut statement = conn
        .0
        .prepare_cached(
            "SELECT e.media_id, e.dim, e.vector
           FROM media_embeddings e
          WHERE e.model = ?1
          ORDER BY e.media_id",
        )
        .map_err(DbError::from)?;
    let rows = statement
        .query_map(rusqlite::params![model], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(DbError::from)?;
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
        let stored = stored_vector(&bytes, dim, model)?;
        let Some((key, notes)) = media_report(conn, media_id, &linkers)? else {
            // A media row with no path row is a row `prune_unlinked_media` has not reached yet (a run that skipped its
            // sweep). There is no workspace path to name, so there is nothing a reader could open: report nothing rather
            // than a hit whose path column is empty.
            continue;
        };
        hits.push(Ranked {
            hit: Hit::image(
                cosine(query.components(), stored.components()),
                String::from_utf8_lossy(&key).to_string(),
                &notes,
            ),
            key,
        });
    }
    Ok(hits)
}

/// `(smallest stored path, every live linker)` for one media row, or `None` when it holds no path.
///
/// The linkers come back as the **whole set**, byte-ordered, and it is up to [`Hit::image`] to decide what a reader sees:
/// the head becomes `source_note`, the length becomes `linked_from`. This function used to do that narrowing itself —
/// returning one note and a count — which is where F28's information went missing: the set was assembled here, complete,
/// and then reduced before any caller could use it.
///
/// Both the path and the notes are chosen by **smallest bytes**, not by insertion order: `media_path` is keyed by
/// `rel_bytes` and `links` by note id, and either could come back in a different sequence after a rename or a re-scan.
/// The path comes back as bytes rather than rendered text because [`search`] needs the un-lossy form to break ties.
fn media_report(
    conn: &Db,
    media_id: i64,
    linkers: &Linkers,
) -> Result<Option<MediaReport>, SearchError> {
    let mut statement = conn
        .0
        .prepare_cached("SELECT rel_bytes FROM media_path WHERE media_id = ?1 ORDER BY rel_bytes")
        .map_err(DbError::from)?;
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
    // `notes` is a `BTreeSet`, so this is already byte-ordered — the same order `source_note` was chosen from, which is
    // what lets the head of this list be the old answer rather than a new decision.
    Ok(Some((smallest.clone(), notes.into_iter().collect())))
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
    let mut statement = conn
        .0
        .prepare_cached(
            "SELECT n.rel_bytes, l.target
           FROM links l
           JOIN notes n ON n.id = l.src_note_id
           JOIN files f ON f.rel_bytes = n.rel_bytes
          WHERE l.origin = ?1 AND l.kind = 'attachment' AND f.state = 'ok'
          ORDER BY n.rel_bytes, l.line",
        )
        .map_err(DbError::from)?;
    let rows = statement
        .query_map(params![db::PROSE_ORIGIN], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(DbError::from)?;
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

/// What one media row reports: its smallest **stored** path bytes, and every live note that links it, byte-ordered.
/// A name for this pair because [`media_report`]'s bare return type trips `clippy::type_complexity`, and a reader of that
/// signature deserves the words anyway.
type MediaReport = (Vec<u8>, Vec<Vec<u8>>);
