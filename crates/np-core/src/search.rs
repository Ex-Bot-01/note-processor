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
    /// Cosine as the vector list computed it, or `None` when no vector list vouched for this hit.
    ///
    /// **Why this sits beside [`Hit::score`] instead of replacing it.** In hybrid mode `score` is a fused rank sum (§4 of
    /// `docs/design-m3-hybrid-search.md`) and a reader who mistakes it for a similarity will draw conclusions about how close
    /// the match was — a claim the number cannot support. The raw evidence is therefore carried separately rather than
    /// recovered from context, and a hit that no vector list held says so with `None` instead of a plausible 0.0.
    pub cosine: Option<f64>,
    /// SQLite's own `bm25()` value for this note (negative-is-better, unremapped), or `None` when the keyword list did not
    /// hold it. Kept as returned because any rescaling of it would be a constant nobody measured.
    pub bm25: Option<f64>,
    /// 1-based position in the semantic list, when there was one.
    pub semantic_rank: Option<usize>,
    /// 1-based position in the keyword list, when there was one.
    pub keyword_rank: Option<usize>,
    /// A fragment of the note showing what matched, brackets around the hit term. `None` for a hit the keyword list did
    /// not produce — including every image, which has no prose to snippet.
    pub snippet: Option<String>,
    /// Whether this note contains every query word **whole**: `Some(true)` an exact hit, `Some(false)` one that only began
    /// with a queried word (**F49**), `None` one the keyword list never held.
    ///
    /// `None` is not `false`, and that distinction is the reason this is an `Option` rather than a bit: a vector hit has not
    /// failed a whole-word test, it was never in one. Under a query that asked for no prefixes every keyword hit is
    /// `Some(true)`, so this key never quietly changes meaning when a caller stops marking terms.
    pub matched_exactly: Option<bool>,
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
            cosine: Some(score),
            bm25: None,
            semantic_rank: None,
            keyword_rank: None,
            snippet: None,
            matched_exactly: None,
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
            cosine: Some(score),
            bm25: None,
            semantic_rank: None,
            keyword_rank: None,
            snippet: None,
            matched_exactly: None,
        }
    }

    /// One hit that **only** the keyword list produced.
    ///
    /// A separate constructor rather than [`Hit::note`] with a zeroed score, because `note` fills `cosine` with the number it
    /// was given and a note nobody embedded has no cosine to report — `Some(0.0)` would read as "we compared vectors and they
    /// were orthogonal", which is a different fact from "no vector list held this row" and the kind of plausible zero a reader
    /// acts on. The score starts at nothing because hybrid adds to it, and keyword-only mode overwrites it with SQLite's own.
    fn keyword_note(path: String) -> Hit {
        let mut hit = Self::note(0.0, path);
        hit.cosine = None;
        hit
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
        // **The empty list is reported where it is decided** (diagnostics design §1 point 4). This function is the only
        // place that knows the difference between "this model stores nothing" and "this model was not embedded"; the
        // second half is the error above, and the first is a state a reader acts on (`np embed`), so it goes out as
        // `no-vectors` at `warn` — a state to fix, not a fault. A caller that never reaches this line because its build
        // has no provider emits nothing here, and says so under its own rule (`provider-disabled`).
        tracing::warn!(
            target: crate::diagnostics::target::SEARCH,
            rule = crate::diagnostics::rule::NO_VECTORS,
            what = model,
            "no vectors are stored for this model, so the semantic half has nothing to rank"
        );
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

// --- keyword and hybrid ranking (`docs/design-m3-hybrid-search.md`) ------------------

/// Which lists a query may consult. The three spellings below are the whole vocabulary: `--mode`, `[search] mode` in config,
/// and `--json` all read these strings, so a fourth spelling appearing in one door and not another (F28's split-refusal
/// lesson) cannot happen while this stays the only definition. The serde derive renames to lowercase, which is the same set
/// [`std::str::FromStr`] accepts — one list, two doors, no drift possible; [`SearchMode::label`] is what both print.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    /// Cosine over stored vectors only — the behaviour before FTS5 existed.
    Semantic,
    /// SQLite `bm25()` over note titles and bodies only. Answers with no model installed, and never returns a figure.
    Keyword,
    /// Both lists, fused by rank (§4). The default the human chose on 2026-11-07.
    Hybrid,
}

impl SearchMode {
    /// The one spelling of each mode, shared by the CLI flag, the config value and `--json`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Keyword => "keyword",
            Self::Hybrid => "hybrid",
        }
    }
}

impl std::fmt::Display for SearchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// A mode name that is not one of the three. Named rather than swallowed: F35's rule is that an unrecognised config value is
/// refused, and a CLI flag deserves the same sentence as a config key.
#[derive(Debug, thiserror::Error)]
#[error("unknown search mode `{0}`: expected `hybrid`, `semantic` or `keyword`")]
pub struct UnknownSearchMode(pub String);

impl std::str::FromStr for SearchMode {
    type Err = UnknownSearchMode;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "hybrid" => Ok(Self::Hybrid),
            "semantic" => Ok(Self::Semantic),
            "keyword" => Ok(Self::Keyword),
            other => Err(UnknownSearchMode(other.to_string())),
        }
    }
}

/// One answer from the keyword list.
#[derive(Debug, Clone, PartialEq)]
pub struct KeywordHit {
    /// Display path of the live note (`notes.rel`), which is what a reader opens.
    pub path: String,
    /// `bm25(notes_fts)` exactly as SQLite returned it: **lower is better**, and corpus-relative. Not rescaled — any
    /// constant applied here would be one nobody measured (F31 has no labelled set to fit against).
    pub bm25: f64,
    /// A fragment of the body with the matched term in brackets; falls back to the title when the body holds no hit term.
    pub snippet: String,
    /// Whether this note contains **every** query word whole, rather than satisfying a marked (or `EveryTerm`) prefix.
    ///
    /// Computed by a second `MATCH` of [`Plan::exact_expression`] inside the same statement (**F49**), so it describes the
    /// same row set the hit came from rather than a re-query that could disagree with it. `true` whenever nothing in the
    /// query asked for word beginnings — the honest reading is "this one needed no prefix", not "this one was checked".
    pub matched_exactly: bool,
}

/// Why an answer came back from fewer lists than the mode asked for. Kept as an enum rather than a `String` so the CLI prints
/// one sentence per real cause instead of free text invented at the print site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankNote {
    /// The workspace holds no vectors at all, so hybrid's semantic half had nothing to read. A state, not an error: this is a
    /// machine that has not downloaded an artifact yet. Vectors under some *other* model id stay a refusal — see [`SearchError::NoVectorsForModel`].
    NoVectors,
    /// **F45 (A).** Configuration names no provider (`embeddings.provider = "none"`, or no `[embeddings]` section at all), so
    /// there is nothing that could turn the query into a vector. This is an instruction, not a gap: the answer came from FTS5,
    /// and the note says what would add the other half. It used to be a refusal at exit 1 with **zero bytes on stdout**, which
    /// a caller could not tell apart from "search is broken" while `--mode keyword` answered fine in the same state.
    ProviderDisabled,
    /// **F45.** Configuration names a model this workspace has never installed. Distinct from [`Self::ProviderDisabled`] because
    /// the reader's next command differs: one edits a file, the other downloads 309 MB.
    ModelNotInstalled,
    /// **F45.** A row exists and its bytes do not (deleted, or a moved data folder): the artifact this build would load is absent,
    /// and no fetch was permitted to put it back. Vectors from an earlier run may well be in the index — they are simply
    /// unusable without a model to embed the *query* with.
    ArtifactMissing,
    /// **F45/F38.** A fetch was allowed and attempted, and nothing was reached. `--json` carries `offline:true` beside this note:
    /// offline is a state, and it belongs to the machine-readable channel even when the run answered — search's exit code says
    /// "did you get an answer", which for a keyword list is yes.
    FetchOffline,
    /// **F45.** The artifact is missing, this build could fetch it, and policy said no: `[models] auto_fetch = false` or
    /// `NP_NO_AUTO_FETCH=1`. Named separately because "it did not download" and "it was not allowed to" read as the same bug to
    /// whoever set the switch and forgot.
    FetchDisabled,
    /// **F45's first-install case (ruled 2026-11-09).** The artifact arrived during this search. The answer is still keyword-only,
    /// because a model on disk writes no vectors by itself: `np embed` (or `np setup`) owes the index its rows. Saying so here is
    /// the difference between "search downloaded 309 MB for nothing" and a reader who knows what the next call is for.
    ArtifactFetched,
}

/// The whole sentence for a build that can fetch: which doors this reader can actually walk through. F32's rule, because a
/// `fetch`-less binary that names `np models fetch` sends them to a help screen that has never heard of it — the same reason
/// `embed_run::install_hint` is two functions rather than one. Two consts, one per feature shape, so the wording a build
/// prints is the wording that build can honour; `tests/cli.rs` pins each of them with the matching `cfg`.
#[cfg(feature = "fetch")]
const NO_VECTORS_SENTENCE: &str = concat!(
    "no vectors in this workspace: ranked by keyword only \u{2014} ",
    "`np models fetch` or `np embed` adds the semantic half"
);

/// The same advice where the fetch path is compiled out: install bytes you hold, then embed.
#[cfg(not(feature = "fetch"))]
const NO_VECTORS_SENTENCE: &str = concat!(
    "no vectors in this workspace: ranked by keyword only \u{2014} ",
    "`np embed` adds the semantic half once a model is installed"
);

// **F45's sentences, one pair per state that names a command.** Same rule `NO_VECTORS_SENTENCE` follows and for the same reason:
// a build that compiled the HTTP client out must not send its reader to `np models fetch`, so each of these exists twice and the
// wording a binary prints is wording that binary can honour. None of them names a host (§12 rule 8), and none of them tells the
// reader to run something that would not help: `ProviderDisabled` says *configure*, never *download*.
#[cfg(feature = "fetch")]
const PROVIDER_DISABLED_SENTENCE: &str = concat!(
    "embeddings are switched off (`embeddings.provider = \"none\"`): ranked by keyword only \u{2014} ",
    "set it to `\"local-model\"` with an `embeddings.model` to add the semantic half"
);
#[cfg(not(feature = "fetch"))]
const PROVIDER_DISABLED_SENTENCE: &str = concat!(
    "embeddings are switched off (`embeddings.provider = \"none\"`): ranked by keyword only \u{2014} ",
    "set it to `\"local-model\"` with an `embeddings.model`, then run `np embed`"
);

#[cfg(feature = "fetch")]
const MODEL_NOT_INSTALLED_SENTENCE: &str = concat!(
    "the configured model is not installed in this workspace: ranked by keyword only \u{2014} ",
    "`np setup` indexes and embeds in one call, or `np models fetch <vault> --id <id>` installs the bytes"
);
#[cfg(not(feature = "fetch"))]
const MODEL_NOT_INSTALLED_SENTENCE: &str = concat!(
    "the configured model is not installed in this workspace: ranked by keyword only \u{2014} ",
    "install it with `np models install <vault> --id <id> --path <file> --dim <width> --sha256 <digest>`, then `np embed`"
);

#[cfg(feature = "fetch")]
const ARTIFACT_MISSING_SENTENCE: &str = concat!(
    "the configured model's artifact is not on disk (automatic fetch is off, or this build cannot fetch): ",
    "ranked by keyword only \u{2014} `np models fetch <vault> --id <id>` puts the bytes back"
);
#[cfg(not(feature = "fetch"))]
const ARTIFACT_MISSING_SENTENCE: &str = concat!(
    "the configured model's artifact is not on disk: ranked by keyword only \u{2014} ",
    "`np models install <vault> --id <id> --path <file> --dim <width> --sha256 <digest>` puts the bytes back"
);

#[cfg(feature = "fetch")]
const FETCH_OFFLINE_SENTENCE: &str = concat!(
    "the pinned origin could not be reached, so nothing was fetched: ranked by keyword only \u{2014} ",
    "connect and run `np embed` (or `np setup`) to add the semantic half"
);
#[cfg(not(feature = "fetch"))]
const FETCH_OFFLINE_SENTENCE: &str = concat!(
    "the pinned origin could not be reached, so nothing was fetched: ranked by keyword only \u{2014} ",
    "this build cannot fetch, so place the artifact with `np models install` and run `np embed`"
);

#[cfg(feature = "fetch")]
const FETCH_DISABLED_SENTENCE: &str = concat!(
    "automatic fetching is switched off (`[models] auto_fetch = false`, or `NP_NO_AUTO_FETCH=1`): ranked by keyword only \u{2014} ",
    "unset it, or run `np setup` / `np models fetch <vault> --id <id>` when you want the semantic half"
);
#[cfg(not(feature = "fetch"))]
const FETCH_DISABLED_SENTENCE: &str = concat!(
    "this build has no fetch command: ranked by keyword only \u{2014} ",
    "install the artifact with `np models install` and run `np embed` to add the semantic half"
);

const ARTIFACT_FETCHED_SENTENCE: &str = concat!(
    "the model artifact was installed by this search: ranked by keyword only \u{2014} ",
    "run `np embed` (or `np setup`) to write the vectors, then this query has its semantic half"
);

impl RankNote {
    /// The one sentence per cause. Spelled here rather than at the print site so text output and `--json` cannot disagree
    /// about what was missing, and a future third variant has to be worded once.
    pub fn label(&self) -> &'static str {
        match self {
            Self::NoVectors => NO_VECTORS_SENTENCE,
            Self::ProviderDisabled => PROVIDER_DISABLED_SENTENCE,
            Self::ModelNotInstalled => MODEL_NOT_INSTALLED_SENTENCE,
            Self::ArtifactMissing => ARTIFACT_MISSING_SENTENCE,
            // "the pinned origin" and no host (§12's rule 8): the transport's failure is reported as a state, the way
            // `embed_run`'s offline arm reports it, and one build's truth about fetching comes from the same const pair.
            Self::FetchOffline => FETCH_OFFLINE_SENTENCE,
            Self::FetchDisabled => FETCH_DISABLED_SENTENCE,
            Self::ArtifactFetched => ARTIFACT_FETCHED_SENTENCE,
        }
    }

    /// The stable machine token for `--json`'s `reason` key. Separate from [`Self::label`] on purpose: the sentence is prose a
    /// human reads and may reword, and a caller that branches on "why did I get keyword results" must not be branching on
    /// wording. Kebab-case, like every other token this program emits.
    pub fn token(&self) -> &'static str {
        match self {
            Self::NoVectors => "no-vectors",
            Self::ProviderDisabled => "provider-disabled",
            Self::ModelNotInstalled => "model-not-installed",
            Self::ArtifactMissing => "artifact-missing",
            Self::FetchOffline => "fetch-offline",
            Self::FetchDisabled => "fetch-disabled",
            Self::ArtifactFetched => "artifact-fetched",
        }
    }

    /// This note's token on the **diagnostics** side (design §1 point 4), which is search's `rule` field.
    ///
    /// One spelling differs from [`Self::token`], and it is worth the sentence: the result document says
    /// `fetch-offline` because that key names *which step of this command* declined, while a diagnostic says
    /// `offline` because an operator's alert matches one token for an outage across every command — `np embed`,
    /// `np models fetch` and `np search` mean the same disconnected machine. Result keys are frozen output, so the
    /// divergence lives in this mapping rather than in either document.
    #[must_use]
    pub fn diag_rule(&self) -> &'static str {
        match self {
            Self::NoVectors => crate::diagnostics::rule::NO_VECTORS,
            Self::ProviderDisabled => crate::diagnostics::rule::PROVIDER_DISABLED,
            Self::ModelNotInstalled => crate::diagnostics::rule::MODEL_NOT_INSTALLED,
            Self::ArtifactMissing => crate::diagnostics::rule::ARTIFACT_MISSING,
            Self::FetchOffline => crate::diagnostics::rule::OFFLINE,
            Self::FetchDisabled => crate::diagnostics::rule::FETCH_DISABLED,
            Self::ArtifactFetched => crate::diagnostics::rule::ARTIFACT_FETCHED,
        }
    }

    /// Report this note as a diagnostic, naming the model it is about where one is known.
    ///
    /// Called by whoever decided the note — np-cli's search path, at the point the note is final for this run — because
    /// §4 asks for one event per degraded state, and that is not what emitting inside each variant's
    /// construction site: a hybrid search can decide `NoVectors` twice over (its own branch, and [`ranked`]'s check) and
    /// a reader should see one record.
    ///
    /// `ArtifactFetched` is the one note that is not a degradation — an artifact arrived — so it reports at `info`. The
    /// rest say a half of the answer is missing, which is a state to fix: `warn` (§4).
    pub fn report(&self, model: Option<&str>) {
        let rule = self.diag_rule();
        let what = model.unwrap_or("the semantic half");
        // The sentence is this note's own label, so the event and the printed line cannot disagree about what happened;
        // no query text travels here (§12 of the diagnostics design), only the id and the rule.
        if matches!(self, Self::ArtifactFetched) {
            tracing::info!(
                target: crate::diagnostics::target::SEARCH,
                rule,
                what,
                "{}",
                self.label()
            );
        } else {
            tracing::warn!(
                target: crate::diagnostics::target::SEARCH,
                rule,
                what,
                "{}",
                self.label()
            );
        }
    }
}

impl std::fmt::Display for RankNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// What [`ranked`] produced: the list, and which halves of it are real.
#[derive(Debug, Clone)]
pub struct Answer {
    /// Fused, ordered, already truncated to the caller's limit.
    pub hits: Vec<Hit>,
    /// Whether a vector list was consulted at all. `false` is the report line "no vectors here", not a failure.
    pub semantic_used: bool,
    /// Whether the keyword list ran (it does not in [`SearchMode::Semantic`], and there is nothing to ask when the query has
    /// no word characters in it).
    pub keyword_used: bool,
    /// Whether any term was matched at the **start of a word** rather than as a whole word (**F49**): `true` for a marked
    /// `grand*`, or for any query under [`Prefix::EveryTerm`]. Beside `keyword_used` because "the keyword half ran" and "the
    /// keyword half widened your words" are two facts, and a client that prints the query as though the notes contained it is
    /// putting words on screen the bytes did not support.
    pub prefix_used: bool,
    /// Which words those were, in query order and deduplicated — the terms to name when labelling a hit, taken from
    /// [`Plan::prefix_terms`] rather than re-derived from the query text at the print site.
    pub prefix_terms: Vec<String>,
    /// The one honest sentence about a missing half, if any.
    pub note: Option<RankNote>,
}

impl Answer {
    /// Display paths in order — a test and CLI convenience that cannot disagree with [`Answer::hits`], because it reads them.
    pub fn paths(&self) -> Vec<&str> {
        self.hits.iter().map(|hit| hit.path.as_str()).collect()
    }
}

/// Whether this workspace holds any vectors at all, so a caller can decide **before loading a 310 MB model** that the
/// semantic half has nothing to read. Same question [`search`] asks internally, exposed because the CLI otherwise pays for a
/// runtime it will not use.
pub fn vectors_present(conn: &Db) -> Result<bool, SearchError> {
    any_vectors_exist(conn)
}

/// How many words one query is allowed to ask about. Beyond this the AND-joined expression costs more than it answers; the
/// cap exists so a pasted page of text cannot turn one search into a thousand phrase lookups.
const MAX_QUERY_TERMS: usize = 32;

/// How much of a query is allowed to match at the start of a word rather than as one whole word (**F49**).
///
/// Keyword search matches whole tokens, and that stance is kept: `runner` does not match `runners`, because a vault holds
/// identifiers and a search that quietly widened them would report notes the reader never asked about. What this adds is a
/// way to *ask* — the gap issue #6 named, where `grand` could not reach `grandma` by any route at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Prefix {
    /// Only a term the caller **marked** is a prefix: a trailing `*`, as in `np search . "grand*"`. The default, because a
    /// query nobody marked contains no prefix term and keeps matching exactly what it matched before — the README's
    /// "no stemming" rule still describes every unmarked word.
    #[default]
    Marked,
    /// **Every** term is a prefix, whether marked or not: `grand` matches `grandma`, `grandmother`, `Grand`. This is what
    /// `np search --prefix` and `search`'s `"prefix": true` ask for — the shape a front end puts behind a visible toggle, so
    /// a person can see that word beginnings are being matched rather than having it inferred from their typing.
    EveryTerm,
}

/// One word of a query, and whether its author asked for word beginnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// The word itself, with any marking `*` removed.
    pub text: String,
    /// Whether a `*` immediately followed it in the query text.
    pub marked: bool,
}

/// Split query text into [`Term`]s, which is where F49's one piece of syntax lives.
///
/// Word characters are alphanumerics and `_`, exactly as they were; every other character separates. A `*` immediately after
/// a word marks that term and is **not** a separator, which is what makes `grand*` mean "a word beginning with grand" while
/// `glob*` in a note about globs still needs the reader to know the trade (F49 §"what `*` costs"). Two consequences worth
/// stating because they are the whole cost of the feature:
///
/// * A trailing asterisk cannot be searched for. There is no escape spelling, and inventing one would put an FTS5 grammar in
///   the query surface — see [`plan`].
/// * A `*` anywhere else stays a separator, unchanged from before: `a*b`, `**`, and a leading `*` all split exactly as they
///   always did. Marking is only ever at the end of a term.
pub fn terms(query: &str) -> Vec<Term> {
    let mut out: Vec<Term> = Vec::new();
    let mut current = String::new();
    for c in query.chars() {
        if c.is_alphanumeric() || c == '_' {
            current.push(c);
        } else {
            if !current.is_empty() {
                out.push(Term {
                    text: std::mem::take(&mut current),
                    marked: c == '*',
                });
            }
            // A `*` that follows a separator, or a second one after a mark, is still just a separator: nothing to mark.
        }
    }
    if !current.is_empty() {
        out.push(Term {
            text: current,
            marked: false,
        });
    }
    out.truncate(MAX_QUERY_TERMS);
    out
}

/// What one query asks the keyword list for: the expression to run, the expression that would have asked for whole words,
/// and the terms whose match is therefore about word beginnings.
///
/// The query stays a bag of words rather than becoming FTS5 syntax — deliberately, and it is why this exists. A raw query
/// string would make `"`, `-`, `(` and `:` a supported input surface whose malformed forms return SQLite parse errors from
/// inside a library call, and exposing that grammar is a decision of its own (§8 of the design note). Quoting each token also
/// keeps an identifier like `GGML_DEFAULT_N_THREADS` — the reason hybrid became the default — out of the parser's way. F49
/// widens that by exactly one character: a term marked for prefix matching is emitted unquoted with a trailing `*`, which is
/// safe because a term contains nothing but word characters, so there is no metacharacter left to reach the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The conjunction to run: marked (or, under [`Prefix::EveryTerm`], every) term as `word*`, the rest as `"word"`.
    pub expression: String,
    /// The same conjunction with **no** prefix terms — whole words only. Run per row as a second `MATCH`, this is what tells a
    /// hit that contains the word from one that merely begins with it, in the same statement and therefore the same snapshot.
    pub exact_expression: String,
    /// The marked words, in query order, deduplicated: the terms this answer matched at a word beginning. A caller labels
    /// hits with these rather than printing the raw query as though the note contained it.
    pub prefix_terms: Vec<String>,
}

impl Plan {
    /// Whether anything in this query asked for word beginnings. `false` means [`Plan::expression`] and
    /// [`Plan::exact_expression`] are the same string, so no hit can be prefix-only.
    pub fn prefixes(&self) -> bool {
        !self.prefix_terms.is_empty()
    }
}

/// Plan one query under one [`Prefix`] policy, or `None` when it holds no word at all (and so has nothing to ask).
pub fn plan(query: &str, prefix: Prefix) -> Option<Plan> {
    let words = terms(query);
    if words.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let mut exact_parts = Vec::new();
    let mut prefix_terms: Vec<String> = Vec::new();
    for term in &words {
        // An FTS5 phrase is `"..."` with any inner quote doubled. A prefix term needs no quoting: `terms` keeps word
        // characters only, so there is nothing in it for the FTS5 parser to read as syntax.
        let quoted = format!("\"{}\"", term.text.replace('"', "\"\""));
        let as_prefix = match prefix {
            Prefix::Marked => term.marked,
            Prefix::EveryTerm => true,
        };
        if as_prefix {
            parts.push(format!("{}*", term.text));
            if !prefix_terms.iter().any(|seen| seen == &term.text) {
                prefix_terms.push(term.text.clone());
            }
        } else {
            parts.push(quoted.clone());
        }
        exact_parts.push(quoted);
    }
    Some(Plan {
        expression: parts.join(" AND "),
        exact_expression: exact_parts.join(" AND "),
        prefix_terms,
    })
}

/// The keyword list: live notes whose title or body contains every word of `query`, ordered by `bm25()` and then by path
/// **bytes** (F20's rule — the display text is a rendering, so it never decides an order on its own).
///
/// `prefix` decides how much of the query may match at the start of a word ([`Prefix`], **F49**). Each row also answers the
/// whole-word question in the same statement (`matched_exactly`), because a hit found by `grand*` and a hit that contains
/// `grand` are different facts and a client must be able to say which it is showing.
pub fn keyword(
    conn: &Db,
    query: &str,
    limit: usize,
    prefix: Prefix,
) -> Result<Vec<KeywordHit>, SearchError> {
    let Some(plan) = plan(query, prefix) else {
        return Ok(Vec::new());
    };
    keyword_planned(conn, &plan, limit)
}

/// [`keyword`] with the query already planned, so [`ranked`] runs one plan rather than planning twice.
fn keyword_planned(conn: &Db, plan: &Plan, limit: usize) -> Result<Vec<KeywordHit>, SearchError> {
    // **The whole-word test is a correlated subquery, not a second query.** SQLite refuses `MATCH` in a select list —
    // "unable to use function MATCH in the requested context", measured here — so the per-row question is asked where a
    // `MATCH` is legal: inside its own `SELECT … FROM notes_fts WHERE notes_fts MATCH ?3`, tied to the outer row by rowid.
    // The alternative was running the exact expression as a second statement and joining paths in Rust, which reads a
    // database a concurrent `np scan` may have moved between the two statements — and then `matched_exactly` would be a claim
    // about a row set that produced no hit at all. One statement cannot disagree with itself.
    let mut statement = conn
        .0
        .prepare_cached(
            "SELECT notes.rel,
                    bm25(notes_fts),
                    snippet(notes_fts, 1, '[', ']', ' … ', 12),
                    snippet(notes_fts, 0, '[', ']', ' … ', 8),
                    EXISTS (
                        SELECT 1 FROM notes_fts
                         WHERE notes_fts MATCH ?3 AND notes_fts.rowid = notes.id
                    )
             FROM notes_fts
             JOIN notes ON notes.id = notes_fts.rowid
             JOIN files ON files.rel = notes.rel
             WHERE notes_fts MATCH ?1 AND files.state = 'ok'
             ORDER BY bm25(notes_fts), notes.rel_bytes
             LIMIT ?2",
        )
        .map_err(DbError::from)?;
    let rows = statement
        .query_map(
            rusqlite::params![plan.expression, limit as i64, plan.exact_expression],
            |row| {
                let rel: String = row.get(0)?;
                let bm25_score: f64 = row.get(1)?;
                let body_snippet: String = row.get(2)?;
                let title_snippet: String = row.get(3)?;
                // SQLite answers a `MATCH` with an integer 0/1, not a boolean.
                let exact: i64 = row.get(4)?;
                Ok((rel, bm25_score, body_snippet, title_snippet, exact != 0))
            },
        )
        .map_err(DbError::from)?;
    let mut hits = Vec::new();
    for row in rows {
        let (path, bm25_score, body_snippet, title_snippet, matched_exactly) =
            row.map_err(DbError::from)?;
        // A body snippet with no bracket found nothing in the body: the match was in the title, so show the title.
        let snippet = if body_snippet.contains('[') {
            body_snippet
        } else {
            title_snippet
        };
        hits.push(KeywordHit {
            path,
            bm25: bm25_score,
            snippet,
            matched_exactly,
        });
    }
    Ok(hits)
}

/// The reciprocal-rank constant the fusion is defined with (§4). Not tuned here — there is no labelled set to tune against
/// (F31), and inventing a number this repository cannot measure would be the worse invention.
const RRF_K: f64 = 60.0;

fn rrf(rank: usize) -> f64 {
    1.0 / (RRF_K + rank as f64)
}

/// Rank one query against the lists `mode` allows, and fuse them.
///
/// `query_vector` is optional because **the caller may have no way to produce one**: a workspace with no vectors, or a
/// keyword-only mode, should not have to embed anything. Absent vector input simply means no semantic list — hybrid answers
/// from keywords and sets [`RankNote::NoVectors`] when the workspace holds no vectors at all; [`SearchMode::Semantic`]
/// answers empty rather than inventing a ranking it cannot compute.
///
/// Ordering is the existing contract extended, not replaced: fused score descending, then display path ascending, then kind,
/// then the raw evidence as bit patterns — so an equal-score pair still has exactly one order on every host and after every
/// re-embed ([`search`] explains why row order may never be a tie-break).
pub fn ranked(
    conn: &Db,
    mode: SearchMode,
    model: Option<&str>,
    query_vector: Option<&[u8]>,
    query_text: &str,
    limit: usize,
    prefix: Prefix,
) -> Result<Answer, SearchError> {
    // The ranking phase (diagnostics design point 7). No workspace field: this function is handed a connection, and the
    // caller that knows the vault — np-cli, or a `np serve` request span — tags everything nested inside it anyway.
    let _span = tracing::info_span!(target: crate::diagnostics::target::SEARCH, "search").entered();
    let mut semantic: Vec<Hit> = Vec::new();
    let mut semantic_used = false;
    if mode != SearchMode::Keyword {
        if let (Some(model), Some(vector)) = (model, query_vector) {
            // **Ask whether there are any vectors before calling it a half of the answer.** Hybrid skips the read entirely
            // when the workspace holds none — that is the machine with no artifact yet, and its report says so. Semantic mode
            // still runs the query, because "no vectors at all" is an empty answer there while "vectors under another model
            // id" is a refusal, and only asking tells those two apart.
            let present = any_vectors_exist(conn)?;
            if mode != SearchMode::Hybrid || present {
                semantic = search(conn, model, vector, limit)?;
                semantic_used = present;
            }
        }
    }

    // Planned once and used twice: for whether the half ran at all, and for the terms it widened. A query with no word in it
    // has no plan, which is the same `keyword_used == false` as before F49 — prefix mode cannot conjure a keyword half out of
    // a query that asks for nothing.
    let plan = if mode == SearchMode::Semantic {
        None
    } else {
        plan(query_text, prefix)
    };
    let keyword_used = plan.is_some();
    // **Which halves actually ran**, at the level someone reaches for when nothing failed but the answer is not what they
    // expected. No `rule`: there is no refusal here, and the states that do refuse already report `no-vectors` or
    // `provider-disabled` where they are decided — repeating a token on a routine per-query line would hand a reader two
    // records to reconcile for one fact.
    tracing::debug!(
        target: crate::diagnostics::target::SEARCH,
        "mode {} ranked {}",
        mode.label(),
        match (semantic_used, keyword_used) {
            (true, true) => "both halves",
            (true, false) => "the vector list only",
            (false, true) => "the keyword list only",
            (false, false) => "nothing: neither half had anything to rank",
        }
    );
    let keywords = match &plan {
        Some(plan) => keyword_planned(conn, plan, limit)?,
        None => Vec::new(),
    };

    // Keyed by (kind, path): a note can sit in both lists, and an image can only ever be in the semantic one.
    let mut merged: BTreeMap<(HitKind, String), Hit> = BTreeMap::new();
    for (index, mut hit) in semantic.into_iter().enumerate() {
        hit.semantic_rank = Some(index + 1);
        hit.score = if mode == SearchMode::Hybrid {
            rrf(index + 1)
        } else {
            hit.cosine.unwrap_or(hit.score)
        };
        merged.insert((hit.kind, hit.path.clone()), hit);
    }
    for (index, found) in keywords.into_iter().enumerate() {
        let key = (HitKind::Note, found.path.clone());
        let entry = merged
            .entry(key)
            .or_insert_with(|| Hit::keyword_note(found.path.clone()));
        if mode == SearchMode::Hybrid {
            entry.score += rrf(index + 1);
        } else {
            // Keyword-only mode: the number that orders the list is SQLite's own, negated so better still means larger.
            entry.score = -found.bm25;
        }
        entry.keyword_rank = Some(index + 1);
        entry.bm25 = Some(found.bm25);
        entry.snippet = Some(found.snippet);
        // Only the keyword list can answer this, so a hit it never held keeps `None`: an image found by cosine has no notion
        // of a whole word, and `false` would claim it failed a test that was never run on it.
        entry.matched_exactly = Some(found.matched_exactly);
    }

    let mut hits: Vec<Hit> = merged.into_values().collect();
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| raw_key(left).cmp(&raw_key(right)))
    });
    hits.truncate(limit);

    let note = if mode == SearchMode::Hybrid && !semantic_used && !any_vectors_exist(conn)? {
        Some(RankNote::NoVectors)
    } else {
        None
    };
    // `semantic_used` is false in two different situations and only one of them deserves the printed sentence: no vectors
    // at all (a state, worth saying) versus a keyword-only mode that never asked (nothing to say).
    Ok(Answer {
        hits,
        semantic_used,
        keyword_used,
        prefix_used: plan.as_ref().is_some_and(Plan::prefixes),
        prefix_terms: plan.map(|plan| plan.prefix_terms).unwrap_or_default(),
        note,
    })
}

/// The last-resort ordering key: the raw evidence, as bit patterns, so `None` sorts before any number and two hits with equal
/// scores still have one order. `total_cmp` is not used here because this compares a tuple of `Option<u64>`, which already is
/// a total order.
fn raw_key(hit: &Hit) -> (Option<u64>, Option<u64>) {
    (
        hit.cosine.map(|value| value.to_bits()),
        hit.bm25.map(|value| value.to_bits()),
    )
}
