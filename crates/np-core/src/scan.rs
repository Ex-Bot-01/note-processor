//! The one pass that turns a workspace folder into a registry, note rows and a catalog.
//!
//! [`scan`] is the only place in the crate that reads files, writes SQLite and renders the catalog at
//! once, so it owns the ordering rules the individual stages cannot express on their own:
//!
//! - **One transaction for the database, files written after it commits.** The registry row, the note
//!   row and everything derived from its text either all move to this scan's state or none of them do.
//!   `INDEX.md` cannot join a SQLite transaction, so the catalog is rendered from the committed state
//!   and then written; a scan that dies halfway leaves the previous catalog readable instead of
//!   half-rewritten.
//! - **Unknown is not absent.** A directory the walk could not enter, and a file that vanished or was
//!   rewritten between the walk and the read, is named in [`ScanReport::skipped`] and keeps the row it
//!   already has (Review Focus 5). Reporting either as `Removed` would stale live notes because of a
//!   permission error or a race.
//! - **The catalog reads live rows only.** Entries and [`ScanReport::notes`] come from
//!   `files.state = 'ok'`; [`crate::db::note_counts`] stays the unfiltered database truth, because a
//!   stale note is hidden from the catalog, not forgotten (spec §2).
//! - **`INDEX.md` and its shards are the only names this module creates or deletes** (spec §7), every
//!   write goes through a temporary file plus `rename`, and no note body is ever opened for writing.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use thiserror::Error;

use crate::catalog::{self, CatalogEntry};
use crate::change::{classify, Change};
use crate::config::{abs_path, Config, ConfigError, MarkerConfig, WorkspaceRegistry};
use crate::db::{self, Db, DbError};
use crate::hash::{sha256_bytes, sha256_file, Snapshot};
use crate::items::{extract_items, ItemCandidate};
use crate::links::{resolve, store_links, RawLink};
use crate::parse::{body_start, parse_markdown, NoteDoc};
use crate::walk::{self, DiscoveredFile, WalkOptions};

/// The UTF-8 byte order mark: dropped before decoding for parsing, kept in the bytes that are hashed.
const BOM: &[u8] = &[0xef, 0xbb, 0xbf];
/// `notes.encoding` for text that decoded without a single replacement character.
const ENCODING_UTF8: &str = "utf-8";
/// `notes.encoding` for bytes UTF-8 cannot hold, decoded with replacements instead of failing the scan
/// (Review Focus 1b).
const ENCODING_LOSSY: &str = "lossy";
/// `links.origin` for an edge the parser read out of a note, as opposed to one a person approved.
const PROSE_ORIGIN: &str = "prose";
/// `meta` key holding the index files currently on disk, newline-separated. The previous list is what
/// lets a rescan delete the shards it no longer produces.
const INDEX_FILES_KEY: &str = "index_files";

/// A note re-parsed by one scan: its path and the links its text holds, which is everything
/// [`store_note_links`] needs once the whole corpus is up to date.
type Parsed = (String, Vec<RawLink>);

/// What can fail while scanning a workspace.
#[derive(Debug, Error)]
pub enum ScanError {
    /// The workspace root could not be read at all. Everything below the root is forgiving by design —
    /// an unreadable directory or file is `skipped`, not fatal — so this is the one walk failure that
    /// reaches a caller.
    #[error("cannot scan workspace: {0}")]
    Walk(#[from] walk::WalkError),
    /// Configuration made the scan impossible, such as a `data_dir` neither the config file nor the
    /// environment supplied ([`crate::config::ConfigError::MissingField`]). The message names the
    /// missing value, because that is what the caller has to go and set.
    #[error("configuration: {0}")]
    Config(#[from] ConfigError),
    /// The workspace database refused a read or a write. A constraint violation on one path's identity
    /// does not reach this variant: that path is `skipped` and the scan carries on.
    #[error("workspace database: {0}")]
    Db(#[from] DbError),
    /// A catalog file could not be written or removed. The message names the path that failed, since a
    /// report that cannot say which file broke is no use to whoever has to fix it.
    #[error("cannot write {}: {source}", .path.display())]
    Io {
        /// The index file or temporary file that could not be written.
        path: PathBuf,
        /// Why it failed.
        source: io::Error,
    },
}

/// What one scan did, for the CLI to print and `--json` to serialise.
#[derive(Debug, Serialize)]
pub struct ScanReport {
    /// Paths that appeared since the last scan.
    pub added: usize,
    /// Paths whose contents changed.
    pub modified: usize,
    /// Paths whose contents travelled from another path in the same workspace.
    pub moved: usize,
    /// Paths that are gone; their rows are marked stale, never deleted (spec §2).
    pub removed: usize,
    /// Paths the registry already had exactly as they are.
    pub unchanged: usize,
    /// Paths this scan could not index and left untouched: directories it could not read, files that
    /// vanished or changed between walk and read, and paths whose identity is already taken. Sorted,
    /// deduplicated, spelled as `rel`.
    pub skipped: Vec<String>,
    /// Live note rows — the same number of entries the catalog holds (spec §2).
    pub notes: i64,
    /// Link rows with no note on the other end, attachments excluded: the "notes I could not place"
    /// count. [`crate::db::note_counts`]'s figure is the wider database question and does not match
    /// this one.
    pub unresolved_links: i64,
    /// The catalog files written, `INDEX.md` first, then its shards.
    pub index_files: Vec<String>,
    /// Wall-clock milliseconds for the whole scan. Informational only — never in a generated file,
    /// where it would make every regeneration differ (spec §10).
    pub elapsed_ms: u128,
}

/// Index `root` into its workspace database and regenerate its catalog.
///
/// The pipeline is walk → read and hash → classify → parse → store → resolve links → items → render →
/// write index files, then [`WorkspaceRegistry::record`]; the database comes from
/// [`Config::workspace_db`] (spec §11). See the module docs for the rules that hold it together, and
/// [`scan_with_inject`] for the seam its race test drives.
///
/// A scan does not fail because of one bad file: an unreadable directory, a note with latin-1 bytes, a
/// zero-byte file and a frontmatter-only note all index, and all get a catalog entry (Review Focus 1b,
/// 4). What does fail is a workspace that cannot be opened at all, an unusable `data_dir`, and a catalog
/// file that cannot be written.
pub fn scan(root: &Path, cfg: &Config) -> Result<ScanReport, ScanError> {
    scan_with_inject(root, cfg, |_| {})
}

/// [`scan`] with a hook run immediately after the walk and before anything is read.
///
/// It exists for `tests/scan.rs`, which deletes or rewrites one file at exactly that moment to prove
/// that a file changed mid-scan is skipped rather than half-indexed ([`ScanReport::skipped`]). Public
/// only so an integration test can reach it; nothing outside this crate should build on it.
#[doc(hidden)]
pub fn scan_with_inject(
    root: &Path,
    cfg: &Config,
    after_walk: impl FnOnce(&Path),
) -> Result<ScanReport, ScanError> {
    let started = Instant::now();
    // An empty data dir would send every derived path next to the current directory — a `workspaces/`
    // folder beside wherever the command happened to be run, possibly inside the workspace itself.
    if cfg.data_dir.as_os_str().is_empty() {
        return Err(ConfigError::MissingField("data_dir").into());
    }
    // One absolute spelling for the walk, the database key, the stored `abs` and the registry row, so
    // one folder is one workspace however it was typed (spec §11).
    let root = abs_path(root);
    let walked = walk::walk(&root, &WalkOptions::default())?;
    after_walk(&root);

    // A directory that could not be entered holds files that are unknown rather than absent: their
    // known rows stay out of the comparison (see [`known_rows`]) and the directory itself is what the
    // report names.
    let mut skipped = walked.unreadable.clone();
    let found = read_found(&walked.files, &mut skipped);
    let snapshots: Vec<Snapshot> = found.iter().map(|found| found.snapshot.clone()).collect();

    let db = Db::open(&cfg.workspace_db(&root))?;
    let now = now_secs();
    let changes = classify(
        &known_rows(&db, &walked.unreadable)?,
        &snapshots,
        !cfg!(windows),
    );

    let tx = db.0.unchecked_transaction().map_err(DbError::from)?;
    let written = apply_changes(&db, &found, &changes, now, !cfg!(windows), &cfg.markers)?;
    // Resolution waits for that pass so a link can land on a file that only appeared in this scan, and
    // comes after the stale-marking so nothing resolves onto a note that just left the live set.
    store_note_links(&db, &written.parsed)?;
    let entries = catalog_entries(&db)?;
    let unresolved = unresolved_links(&db)?;
    let previous = index_list(&db)?;
    tx.commit().map_err(DbError::from)?;

    let rendered = catalog::render(&entries, &cfg.catalog);
    write_index_files(&root, &rendered, &previous)?;
    // Recorded only once the bytes are on disk: `meta` describes the catalog that exists.
    set_index_list(&db, &rendered)?;
    WorkspaceRegistry::open(&cfg.data_dir)?.record(&root, now)?;

    skipped.extend(written.skipped);
    skipped.sort();
    skipped.dedup();
    Ok(ScanReport {
        added: written.added,
        modified: written.modified,
        moved: written.moved,
        removed: written.removed,
        unchanged: written.unchanged,
        skipped,
        // The catalog's own row count is the live-note count: one query answers both questions, so the
        // report and the file it describes cannot drift apart.
        notes: entries.len() as i64,
        unresolved_links: unresolved,
        index_files: rendered.iter().map(|(name, _)| name.clone()).collect(),
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// What the write pass counted and collected.
#[derive(Default)]
struct Written {
    added: usize,
    modified: usize,
    moved: usize,
    removed: usize,
    unchanged: usize,
    /// Paths whose identity was already held by another row (see [`apply_changes`]).
    skipped: Vec<String>,
    /// Every note re-parsed here, for [`store_note_links`].
    parsed: Vec<Parsed>,
}

/// One discovered file as this scan saw it: identity from the walk, digest and note text from the read
/// — or the marker that says there is nothing to index this time.
struct Found {
    /// Identity as the walk recorded it, plus the digest of the bytes on disk. An **empty hash is the
    /// marker [`crate::change`] reads as unknown**: the file could not be read as the walk saw it, so
    /// this scan writes nothing about it and its row keeps what it has.
    snapshot: Snapshot,
    /// The real OS path, byte-exact even when `rel` had to be decoded lossily.
    abs: PathBuf,
    /// Decoded note text; `None` for a file that could not be read or is not markdown, since an
    /// attachment is hashed, tracked and linkable but never parsed (spec §10).
    text: Option<String>,
    /// Whether `text` arrived through lossy decoding.
    lossy: bool,
}

/// Whether this scan holds the file's contents, as opposed to knowing only that it saw the path.
fn is_unknown(found: &Found) -> bool {
    found.snapshot.hash.is_empty()
}

/// Read every discovered file, re-stating each one first, and name the unreadable ones in `skipped`.
///
/// The restatement is what closes the walk-to-read window (Review Focus 5): a file that has gone, or
/// whose size or modification second moved since the walk, is *unknown* to this scan. It is reported,
/// indexed as nothing, and keeps the row it has.
///
/// An unknown path still reaches [`crate::change::classify`] — as an empty-hash marker rather than not
/// at all. That is how such a file is kept from being counted `Removed`, and how the content its row
/// records is kept from being claimed as some other path's move (see [`crate::change`]).
fn read_found(files: &[DiscoveredFile], skipped: &mut Vec<String>) -> Vec<Found> {
    let found: Vec<Found> = files.iter().map(read_one).collect();
    skipped.extend(
        found
            .iter()
            .filter(|found| is_unknown(found))
            .map(|found| found.snapshot.rel.clone()),
    );
    found
}

/// One file's snapshot and note text, with an empty digest when it could not be read as the walk saw it.
///
/// Every read failure collapses into that marker on purpose: the caller has one answer for "unknown this
/// time", and it is the answer [`crate::change`] already speaks.
///
/// A markdown file is read once and digested from memory, because its bytes are held for parsing anyway;
/// anything else is digested in chunks by [`sha256_file`], so one large attachment never decides how much
/// memory a scan needs. Both spellings digest the raw bytes on disk — BOM included — because the digest
/// is the file's identity and not the text's (Review Focus 1b).
fn read_one(file: &DiscoveredFile) -> Found {
    let unknown = || Found {
        snapshot: Snapshot::new(file, String::new()),
        abs: file.abs.clone(),
        text: None,
        lossy: false,
    };
    let open = walk::to_open_path(&file.abs);
    // Still the same file the walk listed? One second of mtime is all this can compare, which is why
    // `size` is checked alongside it.
    let restated = fs::metadata(&open)
        .ok()
        .is_some_and(|meta| meta.len() == file.size && walk::epoch_secs(&meta) == file.mtime);
    if !restated {
        return unknown();
    }
    let (hash, text, lossy) = if file.is_markdown {
        match fs::read(&open) {
            Ok(bytes) => {
                let (text, lossy) = decode(&bytes);
                (sha256_bytes(&bytes), Some(text), lossy)
            }
            Err(_) => return unknown(),
        }
    } else {
        match sha256_file(&file.abs) {
            Ok(hash) => (hash, None, false),
            Err(_) => return unknown(),
        }
    };
    Found {
        snapshot: Snapshot::new(file, hash),
        abs: file.abs.clone(),
        text,
        lossy,
    }
}

/// Decode note bytes for parsing: a leading BOM is dropped, and bytes UTF-8 cannot hold become
/// replacement characters instead of a failed scan. `true` means the text is lossy.
///
/// The BOM has to go before [`parse_markdown`] sees the text, or a Windows export whose first line is
/// `\u{feff}---` loses its frontmatter block and with it its title (ruling from Task 6). Only the copy
/// used for parsing is stripped — see [`read_one`] for the bytes that are hashed.
fn decode(bytes: &[u8]) -> (String, bool) {
    let text = bytes.strip_prefix(BOM).unwrap_or(bytes);
    match std::str::from_utf8(text) {
        Ok(text) => (text.to_string(), false),
        Err(_) => (String::from_utf8_lossy(text).into_owned(), true),
    }
}

/// The registry rows `classify` compares against: live rows, minus anything under a directory the walk
/// could not read.
///
/// Stale rows stay out because their absence was already reported; leaving them in would count one
/// deleted file as `removed` on every later scan. Dropping them is also what lets such a file come back
/// as `Added` onto the row it still has, which is how its note rejoins with its links attached —
/// [`crate::db`] keeps ids stable for exactly that.
///
/// The unreadable directories are the other half of the same caution: files below one were never seen,
/// so their rows must reach neither a `Removed` nor a move (Task 4 handoff), and leaving them in would
/// stale a whole subtree over one transient permission error.
fn known_rows(db: &Db, unreadable: &[String]) -> Result<Vec<Snapshot>, DbError> {
    let mut statement = db.0.prepare(
        "SELECT rel, case_key, hash, size, mtime FROM files WHERE state = 'ok' ORDER BY rel",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(Snapshot {
            rel: row.get(0)?,
            case_key: row.get(1)?,
            hash: row.get(2)?,
            size: row.get::<_, i64>(3)?.max(0) as u64,
            mtime: row.get(4)?,
        })
    })?;
    // Read out fully here, so the write pass never runs with a read open across it.
    let mut known = Vec::new();
    for row in rows {
        let snapshot = row?;
        if !under_unreadable(&snapshot.rel, unreadable) {
            known.push(snapshot);
        }
    }
    Ok(known)
}

/// Whether `rel` sits inside a directory the walk could not read, which makes its absence from the found
/// list meaningless. `"."` is how [`crate::walk`] spells the workspace root itself, and that leaves
/// nothing knowable at all.
fn under_unreadable(rel: &str, unreadable: &[String]) -> bool {
    unreadable
        .iter()
        .any(|dir| dir == "." || rel.starts_with(&format!("{dir}/")))
}

/// Write this scan's changes: every readable file's registry row refreshed, contents re-indexed where
/// they changed, gone paths marked stale.
///
/// Every readable file is upserted and not only the changed ones, because `classify` emits nothing for a
/// matched pair whose hash agrees — and without the refresh such a file would carry an old `mtime` in the
/// database and be re-hashed by every later scan forever (Task 4 handoff).
///
/// A path whose identity is already held by another row — two files named `a.md` and `A.md` on a
/// case-sensitive filesystem, where `files.case_key` stays unique and nothing picks a winner — is
/// reported under `skipped` and left out of the counts instead of aborting the scan (ruling P3).
fn apply_changes(
    db: &Db,
    found: &[Found],
    changes: &[(String, Change)],
    now: i64,
    case_sensitive: bool,
    markers: &MarkerConfig,
) -> Result<Written, ScanError> {
    let mut written = Written::default();
    let pending: HashMap<&str, &Change> = changes
        .iter()
        .map(|(rel, change)| (rel.as_str(), change))
        .collect();

    for readable in found {
        let rel = readable.snapshot.rel.as_str();
        // An unknown path is already named in the report and contributed only its marker to
        // `classify`; there is nothing here to write, and counting it either way would be a guess.
        if is_unknown(readable) {
            continue;
        }
        // `Removed` is keyed on a path that is not in `found`, so it cannot reach this lookup.
        let change = pending.get(rel).copied();
        match index_file(db, readable, change, now, case_sensitive, markers)? {
            Indexed::Taken => written.skipped.push(rel.to_string()),
            Indexed::Done(links) => {
                if let Some(parsed) = links {
                    written.parsed.push(parsed);
                }
                match change {
                    Some(Change::Added) => written.added += 1,
                    Some(Change::Modified) => written.modified += 1,
                    Some(Change::Moved { .. }) => written.moved += 1,
                    None | Some(Change::Removed) => written.unchanged += 1,
                }
            }
        }
    }

    for (rel, change) in changes {
        if matches!(change, Change::Removed) {
            db::mark_stale(db, rel)?;
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
}

/// Bring one found file's row up to date and re-index its contents when they changed.
fn index_file(
    db: &Db,
    readable: &Found,
    change: Option<&Change>,
    now: i64,
    case_sensitive: bool,
    markers: &MarkerConfig,
) -> Result<Indexed, ScanError> {
    let snapshot = &readable.snapshot;
    let rel = snapshot.rel.as_str();
    match change {
        Some(Change::Moved { from }) => {
            // One row moves and `notes.rel` follows by cascade, which is what keeps a renamed note the
            // same note with its links attached (see [`crate::db`]).
            if let Err(err) = db::apply_move(db, from, rel, now) {
                return conflict_or_err(err, Indexed::Taken);
            }
        }
        _ => {
            if !case_sensitive {
                // Identity first: on a case-insensitive filesystem the row may sit under another
                // spelling of the same name — a case-only rename emits no change at all — and upserting
                // over it would trip `files.case_key UNIQUE` instead of renaming the note that is still
                // the same note (Task 4 handoff).
                if let Some(old) = folded_row(db, &snapshot.case_key, rel)? {
                    if let Err(err) = db::apply_move(db, &old, rel, now) {
                        return conflict_or_err(err, Indexed::Taken);
                    }
                }
            }
        }
    }

    if let Err(err) = db::upsert_file(db, snapshot, &readable.abs, now) {
        return conflict_or_err(err, Indexed::Taken);
    }

    // A move alone leaves the derived rows untouched, because they hang off `notes.id` and not off the
    // path. Missing rows are rebuilt whatever `classify` said: a file that changed extension into `.md`
    // while keeping its bytes is a `Moved`, and would otherwise stay unparsed forever.
    if let Some(text) = &readable.text {
        let rewritten = matches!(change, Some(Change::Added | Change::Modified));
        if rewritten || note_id(db, rel)?.is_none() {
            let doc = parse_markdown(text, file_stem(rel));
            let links = doc.links.clone();
            let items = extract_items(text, markers);
            store_note(db, rel, text, &doc, &items, readable.lossy)?;
            return Ok(Indexed::Done(Some((rel.to_string(), links))));
        }
    }
    Ok(Indexed::Done(None))
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
/// `files.rel` and `files.case_key` are both unique, so two paths can claim one note and no rule here may
/// pick a winner; SQLite reports that as a constraint violation on either statement.
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
fn folded_row(db: &Db, case_key: &str, rel: &str) -> Result<Option<String>, DbError> {
    Ok(db
        .0
        .query_row(
            "SELECT rel FROM files WHERE case_key = ?1 AND rel != ?2",
            params![case_key, rel],
            |row| row.get(0),
        )
        .optional()?)
}

/// Write one note's row and everything derived from its text.
///
/// An existing row is updated in place: a re-parse must not mint a new `notes.id`, because links,
/// headings, tags and items hang off it and M3's item identity has to survive a rescan. The child rows are
/// then replaced rather than appended — [`crate::links::store_links`] writes nothing on its own, so a scan
/// that appended would duplicate every row on every rescan and break byte-identical regeneration. Only
/// this note's `origin = 'prose'` links go; an approved edge is an input and not derived output (spec
/// §10), so a rescan must never undo one.
fn store_note(
    db: &Db,
    rel: &str,
    text: &str,
    doc: &NoteDoc,
    items: &[ItemCandidate],
    lossy: bool,
) -> Result<(), DbError> {
    let encoding = if lossy { ENCODING_LOSSY } else { ENCODING_UTF8 };
    // The body is what the author wrote below any recognised frontmatter block: the metadata above it is
    // already stored as title, tags and title source, and [`body_start`] is the shared decision about
    // where that block ends.
    let body = body_text(text, body_start(text));
    let words = i64::try_from(doc.words).unwrap_or(i64::MAX);

    let id = match note_id(db, rel)? {
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
        None => {
            db.0.execute(
                "INSERT INTO notes (rel, title, title_source, encoding, word_count, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    rel,
                    doc.title,
                    doc.title_source.as_str(),
                    encoding,
                    words,
                    body
                ],
            )?;
            db.0.last_insert_rowid()
        }
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
    Ok(())
}

/// The note row behind one `rel`, if it has one. Its id is what every derived row hangs off.
fn note_id(db: &Db, rel: &str) -> Result<Option<i64>, DbError> {
    Ok(db
        .0
        .prepare_cached("SELECT id FROM notes WHERE rel = ?1 LIMIT 1")?
        .query_row(params![rel], |row| row.get(0))
        .optional()?)
}

/// Place and store the links of every note this scan re-parsed.
fn store_note_links(db: &Db, parsed: &[Parsed]) -> Result<(), DbError> {
    if parsed.is_empty() {
        return Ok(());
    }
    let corpus = live_corpus(db)?;
    // Attachments belong in that corpus on purpose: a link may name an image, and the row it lands on has
    // no note behind it — which is exactly what [`ScanReport::unresolved_links`] is told to ignore.
    let resolved = resolve(&corpus, parsed);
    store_links(db, &resolved, PROSE_ORIGIN)
}

/// `case_key` → `rel` for every live file: the corpus [`crate::links::resolve`] places targets in.
fn live_corpus(db: &Db) -> Result<BTreeMap<String, String>, DbError> {
    let mut statement =
        db.0.prepare("SELECT case_key, rel FROM files WHERE state = 'ok' ORDER BY case_key, rel")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut corpus: BTreeMap<String, String> = BTreeMap::new();
    for row in rows {
        let (case_key, rel) = row?;
        corpus.insert(case_key, rel);
    }
    Ok(corpus)
}

/// The catalog's rows: one per live note, with the link counts and tags it renders.
///
/// Live rows only, joined through `files`, so a stale note leaves the catalog while its rows stay in the
/// database (spec §2). Tags come back in insertion order, which is the note's own order, because the
/// renderer prints them as written.
fn catalog_entries(db: &Db) -> Result<Vec<CatalogEntry>, DbError> {
    let mut statement = db.0.prepare(
        "SELECT n.id, n.rel, n.title, f.size
           FROM notes n JOIN files f ON f.rel = n.rel
          WHERE f.state = 'ok'
          ORDER BY n.rel",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?.max(0) as u64,
        ))
    })?;
    // Read out fully before the counts run, so no read is open across another statement.
    let mut live: Vec<(i64, String, Option<String>, u64)> = Vec::new();
    for row in rows {
        live.push(row?);
    }

    let out = link_counts(
        db,
        "SELECT src_note_id, COUNT(*) FROM links GROUP BY src_note_id",
    )?;
    let inc = link_counts(
        db,
        "SELECT dst_note_id, COUNT(*) FROM links
          WHERE dst_note_id IS NOT NULL GROUP BY dst_note_id",
    )?;
    let tags = note_tags(db)?;

    Ok(live
        .into_iter()
        .map(|(id, rel, title, size)| CatalogEntry {
            title: title
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| file_name(&rel).to_string()),
            size_human: catalog::size_human(size),
            tags: tags.get(&id).cloned().unwrap_or_default(),
            out: count_of(&out, id),
            inc: count_of(&inc, id),
            rel,
        })
        .collect())
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

/// Link rows with no note behind them, attachments excluded: the report's figure, and deliberately not
/// [`crate::db::note_counts`]'s. An embed of an image legitimately stores `dst_note_id NULL`, and counting
/// those would bury the "notes I could not place" signal (see [`crate::links::store_links`]).
fn unresolved_links(db: &Db) -> Result<i64, DbError> {
    Ok(db.0.query_row(
        "SELECT COUNT(*) FROM links WHERE dst_note_id IS NULL AND kind != 'attachment'",
        [],
        |row| row.get(0),
    )?)
}

/// The index files this workspace holds from the last scan, from [`INDEX_FILES_KEY`].
fn index_list(db: &Db) -> Result<Vec<String>, DbError> {
    let stored: Option<String> =
        db.0.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![INDEX_FILES_KEY],
            |row| row.get(0),
        )
        .optional()?;
    // One name per line, and no names at all when the value is an empty list rather than absent.
    Ok(stored.map_or_else(Vec::new, |value| {
        value.lines().map(str::to_string).collect()
    }))
}

/// Record the index files now on disk, so a later scan knows which shards to delete.
fn set_index_list(db: &Db, rendered: &[(String, String)]) -> Result<(), DbError> {
    let value = rendered
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    db.0.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![INDEX_FILES_KEY, value],
    )?;
    Ok(())
}

/// Write the rendered catalog and delete the shards this scan no longer produces.
///
/// Every file is written to a temporary name and moved into place with one `rename`, so a reader never
/// sees a half-written catalog (spec §7's whole-file regenerate) and a crash leaves either the old catalog
/// or the new one. Names that are no longer rendered are removed — but only names this program generates,
/// checked against [`walk::is_generated_index`] rather than trusted from `meta`, because a note body is
/// never this function's to delete (spec §7). A name already gone from disk is the state this call wanted.
fn write_index_files(
    root: &Path,
    rendered: &[(String, String)],
    previous: &[String],
) -> Result<(), ScanError> {
    for (name, contents) in rendered {
        let temp = root.join(format!("{name}.tmp"));
        let target = root.join(name);
        fs::write(&temp, contents).map_err(|source| ScanError::Io {
            path: temp.clone(),
            source,
        })?;
        fs::rename(&temp, &target).map_err(|source| ScanError::Io {
            path: target,
            source,
        })?;
    }

    for name in previous {
        let kept = rendered.iter().any(|(written, _)| written == name);
        if kept || !walk::is_generated_index(name) {
            continue;
        }
        let path = root.join(name);
        match fs::remove_file(&path) {
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(ScanError::Io { path, source }),
            Ok(()) => {}
        }
    }
    Ok(())
}

/// Everything from line `first` on, which is where [`body_start`] says the body begins.
fn body_text(text: &str, first: usize) -> &str {
    let mut offset = 0usize;
    for _ in 1..first {
        match text[offset..].find('\n') {
            Some(end) => offset += end + 1,
            // A file shorter than its own frontmatter block has no body.
            None => return "",
        }
    }
    &text[offset..]
}

/// The last `/`-separated component of a `rel`, used as a title when a note has none.
fn file_name(rel: &str) -> &str {
    rel.rsplit('/').next().unwrap_or(rel)
}

/// The file name without its extension — the last-resort title [`parse_markdown`] asks for. A leading dot
/// belongs to the name, and the walker excludes such files anyway.
fn file_stem(rel: &str) -> &str {
    let name = file_name(rel);
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

/// Whole seconds since the Unix epoch, for `files.last_seen` and the workspace registry. `0` when the
/// clock reads before the epoch, which is [`crate::walk::epoch_secs`]'s answer for the same file.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
