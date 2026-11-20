use std::collections::HashMap;

use rusqlite::{params, OptionalExtension};

use crate::change::Change;
use crate::chunk::chunk;
use crate::config::MarkerConfig;
use crate::db::{self, Db, DbError, PROSE_ORIGIN};

use crate::items::{extract_items, ItemCandidate};
use crate::links::{resolve, store_links};
use crate::parse::{body_start, parse_markdown, NoteDoc};
use crate::path_keys::RelPath;
use crate::scan::body_text;
use crate::scan::corpus::live_corpus;
use crate::scan::file_stem;
use crate::scan::found::is_unknown;
use crate::scan::found::owes_a_reparse;
use crate::scan::found::path_of;
use crate::scan::found::Found;
use crate::scan::found::Parsed;
use crate::scan::found::Written;
use crate::scan::found::ENCODING_LOSSY;
use crate::scan::found::ENCODING_UTF8;
use crate::scan::ScanError;

/// What one scan's chunk pass did, counted per live note.
///
/// It reaches no report and no serialised output: [`crate::scan::ScanReport`] stays the classification truth (and its
/// JSON shape is pinned by the CLI tests). The tally exists so `tests/scan.rs` can assert the warm-pass
/// property — a note whose rows already carry its digest costs **one indexed lookup and no cutting** — as
/// counted work rather than as a duration, which is the only honest way to state it on a shared machine.
/// A test that cannot see the work can only measure it, and a measurement of 2,000 small notes proves
/// nothing about 18k ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChunkTally {
    /// Live notes whose stored chunk rows were consulted — one indexed lookup each. A warm scan
    /// raises this and nothing else.
    pub consulted: usize,
    /// Notes actually handed to [`crate::chunk::chunk`], because no stored row carried their current
    /// digest (or a row lagged behind [`crate::chunk::CHUNKER_VERSION`]).
    ///
    /// Zero on a warm pass **for a corpus of notes that hold text**. A note with nothing to cut has no rows to
    /// carry a stamp, so `refresh_chunks` can never find it current and counts it every scan — one blank
    /// note in a vault makes `cut == 0` false forever. Read this number as "notes re-cut", never as "the warm
    /// path worked"; the warm-pass assertion belongs to [`ChunkTally::consulted`] plus unchanged row ids.
    pub cut: usize,
}

/// What the write pass carries to every note: the marker set that decides which lines become items, and the
/// tally of chunk work this scan did.
///
/// They travel together to `index_file` for every note in the corpus, so they are one parameter. Splitting them
/// into two made `apply_changes` and `index_file` eight-argument functions, which `clippy::too_many_arguments`
/// refuses at `-D warnings`; bundling is the fix rather than an `#[allow]`, because an argument list this long is
/// a fact about the design — every future per-note knob would otherwise add a ninth and tenth line here.
pub(crate) struct WritePass<'a> {
    pub(crate) markers: &'a MarkerConfig,
    /// Accumulated through the write pass so [`scan_counting_chunks`] can hand it to a test; the tally is not a
    /// field of [`Written`] because `Written` describes rows, and this counts work — the warm-pass property `tests/scan.rs` asserts.
    pub(crate) chunks: &'a mut ChunkTally,
}

/// Write this scan's changes: every readable file's registry row refreshed, contents re-indexed where
/// they changed, gone paths marked stale.
///
/// Every readable file is upserted and not only the changed ones, because `classify` emits nothing for a
/// matched pair whose hash agrees — and without the refresh such a file would carry an old `mtime` in the
/// database and be re-hashed by every later scan forever.
///
/// A path whose identity is already held by another row — two files named `a.md` and `A.md` on a
/// case-sensitive filesystem, where `files.case_bytes` stays unique and nothing picks a winner — is
/// reported under `skipped` and left out of the counts instead of aborting the scan. So is a path
/// holding more than one note row ([`NoteRow::Duplicate`]), which no constraint can see.
///
/// The pending map is keyed on bytes, like every other identity in the pass: keyed on display
/// `rel`, two twins read back **the same change** from one slot whatever the columns are called.
///
/// `chunks` accumulates what the chunk pass did ([`ChunkTally`]). It is a parameter rather than a field of
/// [`Written`] because the tally has to reach a caller through [`scan_counting_chunks`] while `Written` stays
/// private, and one `&mut` says that more honestly than a second return path.
pub(crate) fn apply_changes(
    db: &Db,
    found: &[Found],
    changes: &[(RelPath, Change)],
    refresh: bool,
    now: i64,
    case_sensitive: bool,
    pass: &mut WritePass<'_>,
) -> Result<Written, ScanError> {
    let mut written = Written::default();
    let pending: HashMap<&[u8], &Change> = changes
        .iter()
        .map(|(path, change)| (path.rel_bytes.as_slice(), change))
        .collect();

    for readable in found {
        let rel = readable.snapshot.rel.as_str();
        let path = path_of(&readable.snapshot);
        // An unknown path is already named in the report and contributed only its marker to
        // `classify`; there is nothing here to write, and counting it either way would be a guess.
        if is_unknown(readable) {
            // The file was listed and then could not be read (or changed under the read). This is the state whose
            // absence from a report once read as "the note is gone": its rows are kept precisely because the next scan
            // may read it fine, and the diagnostic says which of the two happened to this path — a path, no body.
            tracing::debug!(
                target: crate::diagnostics::target::SCAN,
                rule = crate::diagnostics::rule::UNREADABLE,
                what = %rel,
                "the file could not be read as the walk listed it; its rows were left alone"
            );
            // Nothing to write, but a refresh has to remember a missed note *whose rows already exist*:
            // those still hold whatever an older build wrote. An attachment, or a note that never got a
            // row, is charged never — there is nothing stale to repair, so charging it would pin the
            // version forever and re-parse the whole corpus on every scan for nothing.
            if owes_a_reparse(db, &path, readable.is_markdown)? {
                written.uncovered += 1;
            }
            continue;
        }
        // An attachment: discovered, digested, tracked for changes, and never parsed into the catalog. Not a failure —
        // which is exactly why no sentence ever explained it, and why a person counting files against `INDEX.md` needs
        // the reason at `debug` rather than at `error`.
        if !readable.is_markdown {
            tracing::debug!(
                target: crate::diagnostics::target::SCAN,
                rule = crate::diagnostics::rule::NOT_MARKDOWN,
                what = %rel,
                "not markdown, so nothing was indexed from it"
            );
        }
        // `Removed` is keyed on a path that is not in `found`, so it cannot reach this lookup.
        let change = pending.get(readable.snapshot.rel_bytes.as_slice()).copied();
        match index_file(db, readable, change, refresh, now, case_sensitive, pass)? {
            Indexed::Taken => {
                tracing::debug!(
                    target: crate::diagnostics::target::SCAN,
                    rule = crate::diagnostics::rule::IDENTITY_TAKEN,
                    what = %rel,
                    "another path holds this note identity, so its rows were left as they were"
                );
                written.skipped.push(rel.to_string());
                // A note that lost an identity conflict keeps the rows it already had, so the refresh is
                // not finished — but only the *winner* of a collision has rows, and charging the loser
                // would hold the version back over nothing to repair.
                //
                // A refused **move** is judged by the path it came from, not the one it wanted. `classify`
                // suppresses the source's `Removed` because a move is never also a deletion, so that row
                // stays live with its pre-refresh data while the destination holds nothing at all: reading
                // `rel` here would report full coverage over a note left behind stale.
                let owed = match change {
                    Some(Change::Moved { from }) => owes_a_reparse(db, from, true)?,
                    _ => owes_a_reparse(db, &path, readable.is_markdown)?,
                };
                if owed {
                    written.uncovered += 1;
                }
            }
            // More than one note row sits on this path, so no note was re-parsed or rewritten (the registry
            // row may already have moved — see [`Indexed::Duplicate`]). Its rows exist and stay
            // whatever rules wrote them, which is exactly what a refresh has to remember.
            Indexed::Duplicate => {
                // Neither of these two rules has a case asserting on the event, and the reason is named rather than
                // left for a reviewer to hunt down. `Taken` needs an identity conflict on a path this scan is claiming:
                // `tests/scan.rs`'s lossy-twin cases do plant one, and assert on where the path ends up — which rows it
                // keeps, whether a refresh stays charged — not on what was emitted. `Duplicate` needs two note rows on
                // one key, and migration 5 folds such a pair the first time a database this program wrote is opened (see
                // [`Indexed::Duplicate`]), so only a foreign database reaches it. NOT COVERAGE in the gate's vocabulary:
                // what is untested is the one field added beside a decision those cases already pin.

                tracing::debug!(
                    target: crate::diagnostics::target::SCAN,
                    rule = crate::diagnostics::rule::DUPLICATE_ROWS,
                    what = %rel,
                    "more than one note row sits on this key, so none of them was rewritten"
                );
                written.skipped.push(rel.to_string());
                if owes_a_reparse(db, &path, readable.is_markdown)? {
                    written.uncovered += 1;
                }
            }
            Indexed::Done(links) => {
                if let Some(parsed) = links {
                    // `Some` here means the text was re-parsed, and a note `classify` did not call
                    // changed can only have been re-parsed because of `refresh`. An attachment yields
                    // `None`, so it is never counted. A path that had no note row before this scan does
                    // count: the refresh is writing its derived rows for the first time, which is work
                    // the receipt describes — hence "path(s)" in [`report_refresh`] rather than a
                    // promise that every one of them was already a note.
                    if refresh && matches!(change, None | Some(Change::Removed)) {
                        written.refreshed += 1;
                    }
                    written.parsed.push(parsed);
                }
                match change {
                    Some(Change::Added) => written.added += 1,
                    Some(Change::Modified) => written.modified += 1,
                    Some(Change::Moved { .. }) => written.moved += 1,
                    // The warm-pass answer for most of a vault, so it is the one scan rule at `trace`: turning it
                    // on is how someone finds out that the file they edited is *not* in this set.
                    None | Some(Change::Removed) => {
                        tracing::trace!(
                            target: crate::diagnostics::target::SCAN,
                            rule = crate::diagnostics::rule::UNCHANGED_CONTENT,
                            what = %rel,
                            "bytes match the stored digest, so nothing was re-parsed"
                        );
                        written.unchanged += 1;
                    }
                }
            }
        }
    }

    for (path, change) in changes {
        if matches!(change, Change::Removed) {
            db::mark_stale(db, &path.rel_bytes)?;
            written.removed += 1;
        }
    }
    Ok(written)
}

/// Whether one found file reached the registry, and what it yielded when it did.
enum Indexed {
    /// Its row is up to date, with the note's links when its text was re-parsed.
    Done(Option<Parsed>),
    /// Another path holds this identity, so nothing about this row was touched.
    Taken,
    /// More than one note row sits on this path's key — a legacy duplicate `notes.rel` never forbade.
    /// Picking one would rewrite that note's title, tags, items and links with the other's
    /// contents, so no note is re-parsed or rewritten and the file is named in `skipped`.
    ///
    /// What *is* already written by the time this is decided, and what a reader must not be promised away:
    /// for a renamed path `apply_move` has moved the `files` row and both notes rode its cascade, so their
    /// display `rel` changed; and `upsert_file` has refreshed `hash`, `size`, `mtime` and `state`. The
    /// registry row is therefore current while the two notes keep an older title, body, headings, tags,
    /// items and prose edges — a deliberately mixed-age database, because every alternative either merges
    /// two notes nobody asked to merge or stops the scan.
    ///
    /// This state is now **transient for our own databases**: migration 5
    /// (`crate::db::merge_duplicate_notes`, a code span: that step is private) folds such a pair onto its
    /// `MIN(id)` the first time the file is opened, so no scan of a database this program wrote reaches this
    /// arm twice. It stays permanent — and this reader stays load-bearing — only for a database another tool
    /// wrote, which can still arrive with a pair.
    Duplicate,
}

/// Bring one found file's row up to date and re-index its contents when they changed, or when `refresh`
/// says the rules that wrote its derived rows are no longer this build's.
///
/// A note's chunks are refreshed on the same pass and inside the same transaction — cheap, and atomic with
/// the note rows — see [`refresh_chunks`] for the order that work is decided in.
fn index_file(
    db: &Db,
    readable: &Found,
    change: Option<&Change>,
    refresh: bool,
    now: i64,
    case_sensitive: bool,
    pass: &mut WritePass<'_>,
) -> Result<Indexed, ScanError> {
    let snapshot = &readable.snapshot;
    let path = path_of(snapshot);
    match change {
        Some(Change::Moved { from }) => {
            // One row of `files` moves, and [`db::apply_move`] rewrites that note's own `rel` for it:
            // migration 4 removed `notes.rel`'s foreign key, so the display column has no cascade left to
            // ride. Identity is what survives a rename either way — `notes.rel_bytes` follows its parent —
            // which is what keeps a renamed note the same note with its links attached (see [`crate::db`]).
            if let Err(err) = db::apply_move(db, from, &path, now) {
                return conflict_or_err(err, Indexed::Taken);
            }
        }
        _ => {
            if !case_sensitive {
                // Identity first: on a case-insensitive filesystem the row may sit under another
                // spelling of the same name — a case-only rename emits no change at all — and upserting
                // over it would trip `files.case_bytes UNIQUE` instead of renaming the note that is still
                // the same note.
                if let Some(old) = folded_row(db, &snapshot.case_bytes, &snapshot.rel_bytes)? {
                    if let Err(err) = db::apply_move(db, &old, &path, now) {
                        return conflict_or_err(err, Indexed::Taken);
                    }
                }
            }
        }
    }

    // Whether this write replaced a different digest. `index_file` needs it below: a change kind cannot express
    // "same file, new path, new bytes", and that is exactly the case identity resolves.
    let hash_changed = match db::upsert_file(db, snapshot, &readable.abs, now) {
        Ok(changed) => changed,
        Err(err) => return conflict_or_err(err, Indexed::Taken),
    };

    // A move alone leaves the derived rows untouched, because they hang off `notes.id` and not off the
    // path. Missing rows are rebuilt whatever `classify` said: a file that changed extension into `.md`
    // while keeping its bytes is a `Moved`, and would otherwise stay unparsed forever.
    if let Some(text) = &readable.text {
        // `refresh` rewrites a note whose bytes did not change: the whole point is that its
        // derived rows were written by rules this build no longer has.
        // `hash_changed` is the content-based half of this test, and it is load-bearing rather than tidier.
        // A `Moved` was never in the list above because every move today implies equal bytes by construction —
        // but an identity-resolved move pairs a row with a file whose bytes **differ**. Without this term that
        // row would be re-keyed, handed fresh size/mtime/hash, keep its stale body and derived rows, be excluded
        // from the ambiguity charge, and let `PARSE_VERSION` be recorded as though it had been read; the next
        // scan short-circuits on size and mtime before comparing hashes, so nothing would ever notice. Keyed on
        // content rather than on a variant on purpose: any future route that pairs differing bytes lands in the
        // same hole.
        let rewritten =
            refresh || hash_changed || matches!(change, Some(Change::Added | Change::Modified));
        // One id lookup per note, resolved here — after any move above has been applied, which is
        // what lets [`db::apply_move`] have rewritten `notes.rel` onto the new path (migration 4 removed the
        // foreign key that used to do it). The same answer
        // decides whether a note without a row is rebuilt (`is_none()` below) and whether
        // [`store_note`] updates or inserts; a second lookup inside `store_note` is the duplicated read
        // this rule exists to stop.
        let existing = match note_row(db, &path)? {
            NoteRow::Missing => None,
            NoteRow::One(id) => Some(id),
            NoteRow::Duplicate => return Ok(Indexed::Duplicate),
        };
        if rewritten || existing.is_none() {
            let doc = parse_markdown(text, file_stem(&path.rel));
            let links = doc.links.clone();
            let items = extract_items(text, pass.markers);
            let id = store_note(db, &path, existing, text, &doc, &items, readable.lossy)?;
            refresh_chunks(db, id, text, &snapshot.hash, &mut *pass.chunks)?;
            return Ok(Indexed::Done(Some((path, links))));
        }
        // Reached only when a note row exists and nothing re-parsed it: `existing` is therefore `Some`,
        // because `rewritten || existing.is_none()` took every `None` above. The bytes did not move —
        // `hash_changed` sits inside `rewritten` (A4) — but the stored chunk rows can still lag the chunker,
        // which only [`db::chunk_state`] can see, so this arm consults it rather than assuming.
        if let Some(id) = existing {
            refresh_chunks(db, id, text, &snapshot.hash, &mut *pass.chunks)?;
        }
    } else {
        // Nothing to parse, so this is an attachment. Any note row still sitting on this path is a
        // leftover from the rename that made it one (see [`delete_note_row`]).
        if !delete_note_row(db, &path)? {
            return Ok(Indexed::Duplicate);
        }
    }
    Ok(Indexed::Done(None))
}

/// Delete the note row that a path this scan read as an attachment still carries.
///
/// A byte-identical rename that drops the extension (`note.md` → `note.txt`) is a `Moved`, and
/// [`db::apply_move`] rewrites `notes.rel` onto the new path — but an attachment is never parsed, so
/// nothing overwrote that row. Left in place it renders as `- [[Title]](note.txt)` and is counted as a
/// live note, against "tracked but never catalogued", so the row goes with what hangs off it.
///
/// Only this note's `origin = 'prose'` edges go too: an approved edge is an input and not derived output
/// Edges pointing *in* lose their destination rather than being deleted, because they are
/// another note's prose and stay countable as unresolved.
///
/// Migration 3 gave every one of these five statements a twin in the schema — each child table cascades
/// off `notes(id)`, and `links.dst_note_id` is `ON DELETE SET NULL`. The hand-written ones stay: they
/// name in the code what is about to happen, and they hold whatever a connection's `foreign_keys` pragma
/// happens to say. Two places the schema now reaches further are worth knowing from here:
///
/// - The cascade takes **every** outgoing edge, so an approved one leaves with its source note. A foreign
///   key cannot be conditional, and an edge whose source row is gone has nothing to point at; M1 writes no
///   approved edges (`store_links` is called with `prose` only), so today the two agree exactly. If a later
///   milestone starts writing them, this is the decision to revisit.
/// - An orphan can no longer survive this function at all. SQLite hands a deleted rowid to the next
///   INSERT, so an inbound edge left pointing at a dead note would eventually claim an unrelated note as
///   its destination — which is precisely what `dst_note_id`'s foreign key now rules out.
///
/// Returns `false` when the path carries **more than one** note row and nothing was deleted: choosing which
/// of two notes to destroy is a guess this pass never makes, so the caller reports the file instead.
/// `true` means the path is now free of note rows — whether or not one was there, since "already none" is
/// the state this call wanted.
fn delete_note_row(db: &Db, path: &RelPath) -> Result<bool, DbError> {
    let note = note_row(db, path)?;
    // Two rows on one key: there is no honest choice about which to destroy.
    if matches!(note, NoteRow::Duplicate) {
        return Ok(false);
    }
    let Some(id) = (match note {
        NoteRow::One(id) => Some(id),
        _ => None,
    }) else {
        // No note row behind an attachment path, which is already the state wanted.
        return Ok(true);
    };
    db.0.execute("DELETE FROM headings WHERE note_id = ?1", params![id])?;
    db.0.execute("DELETE FROM tags WHERE note_id = ?1", params![id])?;
    db.0.execute("DELETE FROM items WHERE note_id = ?1", params![id])?;
    db.0.execute(
        "DELETE FROM links WHERE src_note_id = ?1 AND origin = ?2",
        params![id, PROSE_ORIGIN],
    )?;
    db.0.execute(
        "UPDATE links SET dst_note_id = NULL WHERE dst_note_id = ?1",
        params![id],
    )?;
    db.0.execute("DELETE FROM notes WHERE id = ?1", params![id])?;
    Ok(true)
}

/// Turn a constraint violation into "this path is skipped"; anything else stays an error.
fn conflict_or_err<T>(err: DbError, taken: T) -> Result<T, ScanError> {
    if is_identity_conflict(&err) {
        return Ok(taken);
    }
    Err(err.into())
}

/// Whether a failed write means another row already holds this path's identity — the one failure that is
/// a fact about the workspace rather than a broken scan.
///
/// `files.rel_bytes` and `files.case_bytes` are both unique — since migration 4 they, not the display
/// columns, carry identity — so two paths can claim one note and no rule here may pick a winner; SQLite
/// reports that as a constraint violation on either statement.
fn is_identity_conflict(err: &DbError) -> bool {
    matches!(
        err,
        DbError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
                && matches!(
                    failure.extended_code,
                    rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                        | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                )
    )
}

/// The `rel` of a live row holding `case_key` under a different spelling of the same path.
/// The row holding `case_bytes` under a different byte key — the one a rename has to follow. Matched on the
/// ASCII byte fold and returning both spellings of the row it finds: once this matched the Unicode
/// display fold it handed back a *twin* as "another spelling of the same name", and [`index_file`] then moved
/// a row between two files that both exist. The accepted price is in [`crate::path_keys::case_fold`]'s doc:
/// a rename that changes only a non-ASCII letter's case stops being followed here.
pub(crate) fn folded_row(
    db: &Db,
    case_bytes: &[u8],
    rel_bytes: &[u8],
) -> Result<Option<RelPath>, DbError> {
    Ok(db
        .0
        .query_row(
            "SELECT rel_bytes, rel FROM files WHERE case_bytes = ?1 AND rel_bytes != ?2",
            params![case_bytes, rel_bytes],
            |row| {
                Ok(RelPath::new(
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                ))
            },
        )
        .optional()?)
}

/// Write one note's row and everything derived from its text.
///
/// An existing row is updated in place: a re-parse must not mint a new `notes.id`, because links,
/// headings, tags and items hang off it and M3's item identity has to survive a rescan. The child rows are
/// then replaced rather than appended — [`crate::links::store_links`] writes nothing on its own, so a scan
/// that appended would duplicate every row on every rescan and break byte-identical regeneration. Only
/// this note's `origin = 'prose'` links go; an approved edge is an input and not derived output, so a
/// rescan must never undo one.
///
/// `existing` is the note row behind `rel` as [`index_file`] resolved it — `None` when there is none
/// yet. The id is passed in rather than looked up here because [`index_file`] needs the same answer
/// to decide whether to re-parse at all, and the duplicate used to exist on exactly one branch:
/// its `rewritten || note_id(db, rel)?.is_none()` short-circuits on `Added | Modified`, so a changed
/// note did its only lookup inside this function — only a row-less note, where `rewritten` is false
/// and `is_none()` alone decides, was asked the question twice.
///
/// No fresh read is required between that lookup and here: [`db::upsert_file`] runs *before* it, not
/// between, and writes `files` only, so nothing has touched `notes` in the meantime — and when
/// `existing` is `None`, [`db::insert_note`] mints the id itself, so none is needed after the write either —
/// and that call is where the note's byte key is written, not here (see its doc for why the insert lives in
/// `db`).
/// Returns the `notes.id` the row was written under — the id a re-parse keeps — because the chunk pass that
/// follows hangs its rows off it and must not look it up a second time.
fn store_note(
    db: &Db,
    path: &RelPath,
    existing: Option<i64>,
    text: &str,
    doc: &NoteDoc,
    items: &[ItemCandidate],
    lossy: bool,
) -> Result<i64, DbError> {
    let encoding = if lossy { ENCODING_LOSSY } else { ENCODING_UTF8 };
    // The body is what the author wrote below any recognised frontmatter block: the metadata above it is
    // already stored as title, tags and title source, and [`body_start`] is the shared decision about
    // where that block ends.
    let body = body_text(text, body_start(text));
    let words = i64::try_from(doc.words).unwrap_or(i64::MAX);

    let id = match existing {
        Some(id) => {
            db.0.execute(
                "UPDATE notes
                    SET title = ?2, title_source = ?3, encoding = ?4, word_count = ?5, body = ?6
                  WHERE id = ?1",
                params![
                    id,
                    doc.title,
                    doc.title_source.as_str(),
                    encoding,
                    words,
                    body
                ],
            )?;
            id
        }
        None => db::insert_note(
            db,
            path,
            &doc.title,
            doc.title_source.as_str(),
            encoding,
            words,
            body,
        )?,
    };

    db.0.execute("DELETE FROM headings WHERE note_id = ?1", params![id])?;
    db.0.execute("DELETE FROM tags WHERE note_id = ?1", params![id])?;
    db.0.execute("DELETE FROM items WHERE note_id = ?1", params![id])?;
    db.0.execute(
        "DELETE FROM links WHERE src_note_id = ?1 AND origin = ?2",
        params![id, PROSE_ORIGIN],
    )?;

    for heading in &doc.headings {
        db.0.prepare_cached(
            "INSERT INTO headings (note_id, level, text, line) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![
            id,
            i64::from(heading.level),
            heading.text,
            i64::try_from(heading.line).unwrap_or(i64::MAX)
        ])?;
    }
    for tag in &doc.tags {
        db.0.prepare_cached("INSERT INTO tags (note_id, tag) VALUES (?1, ?2)")?
            .execute(params![id, tag])?;
    }
    for item in items {
        db.0.prepare_cached(
            "INSERT INTO items (note_id, text, line, state, marker) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![
            id,
            item.text,
            i64::try_from(item.line).unwrap_or(i64::MAX),
            item.state.as_str(),
            item.marker
        ])?;
    }
    Ok(id)
}

/// Cut one note into chunks when its stored rows no longer describe its bytes.
///
/// **The order is the whole point**: [`db::chunk_digest`] is consulted **first**, and
/// only a note whose rows carry no single digest — none at all, a digest other than `note_digest`, or a row
/// behind [`CHUNKER_VERSION`] — goes on to be cut. The stamp read deliberately excludes `chunks.text`: reading
/// the texts here would make a warm scan pull roughly the whole corpus out of SQLite and discard it, which is
/// not the one indexed lookup a warm pass should cost, and invisible to a fixture whose notes are one chunk
/// each.
///
/// Nothing is re-read either: `text` is the string [`read_one`]
/// decoded once for the parser (leading BOM stripped, bytes UTF-8 cannot hold replaced), which is exactly the
/// domain the chunk offsets are defined over. Cutting first and asking afterwards would slice every unchanged
/// note in the vault on every warm scan to arrive at rows that are already there — the failure mode this order
/// exists to prevent — which is why [`ChunkTally::cut`] is asserted rather than timed.
///
/// `note_digest` is `files.hash`: the digest of the bytes on disk, BOM included ([`read_one`]). The offsets
/// stay over the decoded text, and that is not a contradiction: decoding is a pure function of those
/// bytes, so one file digest still states exactly which decoded text these rows were cut from — and it is the
/// only digest this call can compare without opening the file again.
///
/// **A note with nothing to cut has no rows to stamp**, so [`db::chunk_digest`] answers `None` for it forever
/// and it pays one indexed lookup plus an empty cut on every scan. The warm-pass property is about the notes
/// that have rows; a blank note cuts nothing, writes nothing, and stays cheap in the way that matters.
fn refresh_chunks(
    db: &Db,
    note_id: i64,
    text: &str,
    note_digest: &str,
    chunks: &mut ChunkTally,
) -> Result<(), DbError> {
    chunks.consulted += 1;
    // Stamps only, never texts: see this function's doc for the cost that decides. `None` means "these rows
    // describe no single set of bytes" — never "this note has none", which is why an emptied note still reaches
    // the write below and loses its rows.
    let stamped = db::chunk_digest(db, note_id)?;
    if stamped.as_deref() == Some(note_digest) {
        return Ok(());
    }
    chunks.cut += 1;
    let cut = chunk(text);
    // No transaction of its own: `db::write_chunks_for_note` writes through the caller's handle, which here
    // is the scan's single write transaction, and chunk rows have to be atomic with the note rows.
    db::write_chunks_for_note(db, note_id, &cut, note_digest)?;
    Ok(())
}

/// The note row one path owns, read by byte key.
#[derive(Debug)]
pub(crate) enum NoteRow {
    /// No note behind the path yet — an attachment, or a note this scan has not written.
    Missing,
    /// The normal case: exactly one row, whose id every derived row hangs off.
    One(i64),
    /// More than one row on the key. Legal at version 3 (`notes.rel` was never `UNIQUE`) and copied onto one
    /// `rel_bytes` by migration 4, so this is a population the reader has to name rather than pretend not to
    /// have.
    Duplicate,
}

/// How many note rows sit behind one path's byte key, and which one if there is a single answer.
///
/// **No `LIMIT 1`**: the old single-row read is what made a legacy duplicate silently *wrong* rather
/// than loud — it handed one file's title, tags, items and links to the other file's note. Refusing with an
/// error was the other wrong answer: it would make a workspace holding one old duplicate impossible to scan
/// ever again. So the count travels back to the caller, which reports the file under `skipped`, rewrites no
/// note, and carries on.
pub(crate) fn note_row(db: &Db, path: &RelPath) -> Result<NoteRow, DbError> {
    Ok(match db::note_ids(db, &path.rel_bytes)?.as_slice() {
        [] => NoteRow::Missing,
        [only] => NoteRow::One(*only),
        _ => NoteRow::Duplicate,
    })
}

/// Place and store the links of every note this scan re-parsed.
pub(crate) fn store_note_links(db: &Db, parsed: &[Parsed]) -> Result<(), DbError> {
    if parsed.is_empty() {
        return Ok(());
    }
    let corpus = live_corpus(db)?;
    // Attachments belong in that corpus on purpose: a link may name an image, and the row it lands on has
    // no note behind it — which is exactly what [`ScanReport::unresolved_links`] is told to ignore.
    let resolved = resolve(&corpus, parsed);
    store_links(db, &resolved, PROSE_ORIGIN)
}
