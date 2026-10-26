//! The one pass that turns a workspace folder into a registry, note rows and a catalog.
//!
//! [`scan`] is the only place in the crate that reads files, writes SQLite and renders the catalog at
//! once, so it owns the ordering rules the individual stages cannot express on their own:
//!
//! - **One transaction for the database, files written after it commits.** The registry row, the note
//!   row and everything derived from its text either all move to this scan's state or none of them do.
//!   `INDEX.md` cannot join a SQLite transaction, so the catalog is rendered inside that transaction,
//!   its name list is committed with it, and only then does the filesystem work start. A scan that dies
//!   halfway leaves the previous catalog readable instead of half-rewritten, and never leaves a delete
//!   list that lags the files on disk (see `set_index_list` and `write_index_files` for the order).
//! - **Unknown is not absent.** A directory the walk could not enter, and a file that vanished or was
//!   rewritten between the walk and the read, is named in [`ScanReport::skipped`] and keeps the row it
//!   already has (Review Focus 5). Reporting either as `Removed` would stale live notes because of a
//!   permission error or a race.
//! - **Parse rules reach notes that did not change.** Byte comparison cannot see a rule change, so
//!   `meta` records the parse rules version the rows were written with and a scan that finds a different
//!   value re-parses every markdown note once (see [`PARSE_VERSION_KEY`]). Classification stays the
//!   truth about the *files*: such a scan still reports its notes as unchanged.
//! - **The catalog reads live rows only.** Entries and [`ScanReport::notes`] come from
//!   `files.state = 'ok'`; [`crate::db::note_counts`] stays the unfiltered database truth, because a
//!   stale note is hidden from the catalog, not forgotten (spec §2).
//! - **`INDEX.md` and its shards are the only names this module creates, and it deletes only the ones
//!   that open with its own generated header** (spec §7): a name alone never licenses a removal. The
//!   names themselves come from [`index_names`], which owns their spelling; what this module owns is the
//!   fingerprint deciding which of them are ours to replace. Every write goes through a temporary file
//!   plus `rename`, and no note body is ever opened for writing.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use thiserror::Error;

use crate::catalog::{self, CatalogEntry};
use crate::change::{classify, Change};
use crate::config::{abs_path, Config, ConfigError, MarkerConfig, WorkspaceRegistry};
use crate::db::{self, Db, DbError, LIVE_EDGE, PROSE_ORIGIN};
use crate::hash::{sha256_bytes, sha256_file, Snapshot};
use crate::index_names;
use crate::items::{extract_items, ItemCandidate};
use crate::links::{resolve, store_links, RawLink};
use crate::parse::{body_start, parse_markdown, NoteDoc, PARSE_VERSION};
use crate::walk::{self, DiscoveredFile, WalkOptions};

/// The UTF-8 byte order mark: dropped before decoding for parsing, kept in the bytes that are hashed.
const BOM: &[u8] = &[0xef, 0xbb, 0xbf];
/// `notes.encoding` for text that decoded without a single replacement character.
const ENCODING_UTF8: &str = "utf-8";
/// `notes.encoding` for bytes UTF-8 cannot hold, decoded with replacements instead of failing the scan
/// (Review Focus 1b).
const ENCODING_LOSSY: &str = "lossy";
/// `meta` key holding the index files this program has published, newline-separated. The previous list
/// is what lets a rescan delete the shards it no longer produces.
const INDEX_FILES_KEY: &str = "index_files";
/// `meta` key holding the value of [`crate::parse::PARSE_VERSION`] that this workspace's derived rows
/// were written with. Absent means "written before this mechanism existed", which is not a version to
/// compare against and therefore refreshes; see the comparison in `refresh_needed`.
pub const PARSE_VERSION_KEY: &str = "parse_version";
/// Suffix of the temporary file every catalog write goes through before its `rename`.
const TEMP_SUFFIX: &str = ".tmp";

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
    /// vanished or changed between walk and read, paths whose identity is already taken, a catalog
    /// temporary it could not remove (see `clear_temp_corpses`), and a root entry wearing a generated
    /// catalog name that this program did not publish — left on disk and named here instead of deleted.
    /// Sorted, deduplicated, spelled as `rel`.
    pub skipped: Vec<String>,
    /// Live note rows — the same number of entries the catalog holds (spec §2).
    pub notes: i64,
    /// Link rows with no note on the other end, attachments excluded: the "notes I could not place"
    /// count. [`crate::db::note_counts`]'s figure is the wider database question and does not match
    /// this one.
    pub unresolved_links: i64,
    /// The targets behind [`Self::unresolved_links`], spelled as their author wrote them, sorted and
    /// deduplicated — the list the CLI prints under that count ([`crate::db::unresolved_targets`] answers
    /// both from one predicate, so the number and the names beside it cannot disagree).
    pub unresolved_targets: Vec<String>,
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
    // Before the walk, not on the way out: a corpse the walk lists is a file this scan would index as an
    // attachment (see [`clear_temp_corpses`]). One that will not be removed is reported and retried by
    // the next scan; it does not end this one.
    let mut skipped = clear_temp_corpses(&root)?;
    let walked = walk::walk(&root, &WalkOptions::default())?;
    after_walk(&root);

    // A directory that could not be entered holds files that are unknown rather than absent: their
    // known rows stay out of the comparison (see [`known_rows`]) and the directory itself is what the
    // report names.
    skipped.extend(walked.unreadable.iter().cloned());
    let found = read_found(&walked.files, &mut skipped);
    let snapshots: Vec<Snapshot> = found.iter().map(|found| found.snapshot.clone()).collect();

    let db = Db::open(&cfg.workspace_db(&root))?;
    // Which rules wrote the rows on disk. Read before anything is written, so this one scan both does the
    // re-parse and records what it re-parsed with: `classify` compares bytes and knows nothing about the
    // rules that turn those bytes into headings, tags, items and links, so without this a note nobody has
    // edited since an older build ran would keep its old derived rows forever (F15).
    let stored = parse_version(&db)?;
    let refresh = refresh_needed(stored);
    let now = now_secs();
    let (known, hidden_by_unreadable) = known_rows(&db, &walked.unreadable)?;
    let changes = classify(&known, &snapshots, !cfg!(windows));

    let tx = db.0.unchecked_transaction().map_err(DbError::from)?;
    let written = apply_changes(
        &db,
        &found,
        &changes,
        refresh,
        now,
        !cfg!(windows),
        &cfg.markers,
    )?;
    // Decided here, before anything else writes, from what the write pass actually delivered.
    let record_version = refresh && refresh_covered(&written, hidden_by_unreadable);
    // Resolution waits for that pass so a link can land on a file that only appeared in this scan, and
    // comes after the stale-marking so nothing resolves onto a note that just left the live set.
    store_note_links(&db, &written.parsed)?;
    let entries = catalog_entries(&db)?;
    // Count and list are read from the same open transaction as one another, which is what lets the CLI
    // print the names directly under the number.
    let unresolved = unresolved_links(&db)?;
    let targets = db::unresolved_targets(&db)?;
    let previous = index_list(&db)?;
    // Rendering is pure, so the set this scan is about to publish is known here — and `meta` records it
    // inside the transaction rather than after the writes. A list that *leads* reality costs little,
    // because [`write_index_files`] treats a name already gone from disk as the state it wanted; a list
    // that *lags* it would be fatal, because a shard it fails to name would survive as an orphan nothing
    // ever deletes again. It cannot lag: [`superseded_names`] reads the root directory alongside `meta`,
    // so a kill between this commit and the writes below still leaves every stranded shard named.
    let rendered = catalog::render(&entries, &cfg.catalog);
    set_index_list(&db, &rendered)?;
    if record_version {
        // Recorded in the same transaction as the rows it describes: a scan that dies halfway has to
        // refresh again rather than leave half the corpus labelled as current. An up-to-date database
        // writes nothing here, so the steady-state scan pays one `SELECT` and no extra `INSERT`.
        set_meta(&db, PARSE_VERSION_KEY, &PARSE_VERSION.to_string())?;
    }
    tx.commit().map_err(DbError::from)?;
    // After the commit, so nothing is announced about a scan that rolled itself back — the same rule
    // [`crate::db::report_orphans`] follows. The report's own counts stay the classification truth; this
    // is the only place a person learns that a scan with nothing to report did real work.
    report_refresh(&mut io::stderr(), stored, written.refreshed);
    if refresh && !record_version {
        // Without this line a workspace with one unreadable directory would re-parse its whole corpus on
        // every scan in silence, which looks exactly like the version never being written by accident.
        report_refresh_gap(&mut io::stderr(), written.uncovered + hidden_by_unreadable);
    }

    // A root entry that wears one of our names but is not ours stays on disk and reaches the report, so
    // the user hears about the collision instead of losing the file.
    skipped.extend(write_index_files(&root, &rendered, &previous)?);
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
        unresolved_targets: targets,
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
    /// Notes this scan re-parsed **only** because the parse rules moved on — a file `classify` called
    /// unchanged. Deliberately absent from [`ScanReport`], whose counts stay the classification truth and
    /// whose JSON shape the CLI tests pin; [`report_refresh`] is where it becomes visible.
    refreshed: usize,
    /// Every note re-parsed here, for [`store_note_links`].
    parsed: Vec<Parsed>,
    /// Notes this scan owed a write and never delivered one: unreadable as the walk saw it, or lost to an
    /// identity conflict. Only meaningful during a refresh, where it decides whether the new rules may be
    /// recorded — see [`refresh_covered`].
    uncovered: usize,
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
    /// Whether the walk called this a note at all. [`apply_changes`] needs it to charge a missed path
    /// only when a parse was actually owed — see [`refresh_covered`].
    is_markdown: bool,
    /// Whether `text` arrived through lossy decoding.
    lossy: bool,
}

/// Whether this scan holds the file's contents, as opposed to knowing only that it saw the path.
fn is_unknown(found: &Found) -> bool {
    found.snapshot.hash.is_empty()
}

/// Whether skipping `rel` leaves **stale** derived rows behind — the only thing a refresh can repair.
///
/// A markdown path with no `notes` row has nothing to re-label. An unreadable new file is indexed as
/// `Added` on whichever scan can next read it, whatever the recorded version says, and the loser of a case
/// collision (`a.md` against `A.md`, one `case_key`) is refused at insert and holds no row of its own in the
/// normal case — two raw names that decode lossily to the same `rel` do produce a path refused over a row that
/// exists, and that one *is* charged, which is the safe direction. Charging either would hold
/// the rules version back forever over rows that do not exist, re-parsing the whole corpus on every scan
/// to repair nothing — the same mistake the attachment filter above already avoids, one level deeper.
fn owes_a_reparse(db: &Db, rel: &str, is_markdown: bool) -> Result<bool, DbError> {
    Ok(is_markdown && note_id(db, rel)?.is_some())
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
        is_markdown: file.is_markdown,
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
        is_markdown: file.is_markdown,
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
///
/// The count alongside the rows is how many live **note** rows that second filter dropped — nothing else
/// reports them, since they appear nowhere in `found` — which is how [`scan_with_inject`] knows a
/// parse-rule refresh never reached part of the corpus. Any row with no note behind it is excluded by the
/// `EXISTS` — attachments and never-yet-parsed paths alike: no parse rule can stale rows that do not exist,
/// so counting one would pin the version forever over work that is not owed.
fn known_rows(db: &Db, unreadable: &[String]) -> Result<(Vec<Snapshot>, usize), DbError> {
    let mut statement = db.0.prepare(
        "SELECT f.rel, f.case_key, f.hash, f.size, f.mtime,
                EXISTS (SELECT 1 FROM notes n WHERE n.rel = f.rel)
         FROM files f WHERE f.state = 'ok' ORDER BY f.rel",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            Snapshot {
                rel: row.get(0)?,
                case_key: row.get(1)?,
                hash: row.get(2)?,
                size: row.get::<_, i64>(3)?.max(0) as u64,
                mtime: row.get(4)?,
            },
            row.get::<_, bool>(5)?,
        ))
    })?;
    // Read out fully here, so the write pass never runs with a read open across it.
    let mut known = Vec::new();
    let mut hidden = 0_usize;
    for row in rows {
        let (snapshot, is_note) = row?;
        if under_unreadable(&snapshot.rel, unreadable) {
            if is_note {
                hidden += 1;
            }
        } else {
            known.push(snapshot);
        }
    }
    Ok((known, hidden))
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
    refresh: bool,
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
            // Nothing to write, but a refresh has to remember a missed note *whose rows already exist*:
            // those still hold whatever an older build wrote. An attachment, or a note that never got a
            // row, is charged never — there is nothing stale to repair, so charging it would pin the
            // version forever and re-parse the whole corpus on every scan for nothing.
            if owes_a_reparse(db, rel, readable.is_markdown)? {
                written.uncovered += 1;
            }
            continue;
        }
        // `Removed` is keyed on a path that is not in `found`, so it cannot reach this lookup.
        let change = pending.get(rel).copied();
        match index_file(db, readable, change, refresh, now, case_sensitive, markers)? {
            Indexed::Taken => {
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
                    _ => owes_a_reparse(db, rel, readable.is_markdown)?,
                };
                if owed {
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

/// Bring one found file's row up to date and re-index its contents when they changed, or when `refresh`
/// says the rules that wrote its derived rows are no longer this build's.
fn index_file(
    db: &Db,
    readable: &Found,
    change: Option<&Change>,
    refresh: bool,
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
        // `refresh` rewrites a note whose bytes did not change: the whole point of F15 is that its
        // derived rows were written by rules this build no longer has.
        let rewritten = refresh || matches!(change, Some(Change::Added | Change::Modified));
        // One id lookup per note, resolved here — after any move above has been applied, which is
        // what lets `notes.rel ON UPDATE CASCADE` have caught up with the path. The same answer
        // decides whether a note without a row is rebuilt (`is_none()` below) and whether
        // [`store_note`] updates or inserts; looking it up again inside `store_note` was F10.
        let existing = note_id(db, rel)?;
        if rewritten || existing.is_none() {
            let doc = parse_markdown(text, file_stem(rel));
            let links = doc.links.clone();
            let items = extract_items(text, markers);
            store_note(db, rel, existing, text, &doc, &items, readable.lossy)?;
            return Ok(Indexed::Done(Some((rel.to_string(), links))));
        }
    } else {
        // Nothing to parse, so this is an attachment. Any note row still sitting on this path is a
        // leftover from the rename that made it one (see [`delete_note_row`]).
        delete_note_row(db, rel)?;
    }
    Ok(Indexed::Done(None))
}

/// Delete the note row that a path this scan read as an attachment still carries.
///
/// A byte-identical rename that drops the extension (`note.md` → `note.txt`) is a `Moved`, and
/// [`db::apply_move`] lets `notes.rel` cascade onto the new path — but an attachment is never parsed, so
/// nothing overwrote that row. Left in place it renders as `- [[Title]](note.txt)` and is counted as a
/// live note, against "tracked but never catalogued" (spec §10), so the row goes with what hangs off it.
///
/// Only this note's `origin = 'prose'` edges go too: an approved edge is an input and not derived output
/// (spec §10). Edges pointing *in* lose their destination rather than being deleted, because they are
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
fn delete_note_row(db: &Db, rel: &str) -> Result<(), DbError> {
    let Some(id) = note_id(db, rel)? else {
        return Ok(());
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
    Ok(())
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
///
/// `existing` is the note row behind `rel` as [`index_file`] resolved it — `None` when there is none
/// yet. The id is passed in rather than looked up here because [`index_file`] needs the same answer
/// to decide whether to re-parse at all, and before F10 the duplicate existed on exactly one branch:
/// its `rewritten || note_id(db, rel)?.is_none()` short-circuits on `Added | Modified`, so a changed
/// note did its only lookup inside this function — only a row-less note, where `rewritten` is false
/// and `is_none()` alone decides, was asked the question twice.
///
/// No fresh read is required between that lookup and here: [`db::upsert_file`] runs *before* it, not
/// between, and writes `files` only, so nothing has touched `notes` in the meantime — and when
/// `existing` is `None`, [`db::insert_note`] mints the id itself, so none is needed after the write either —
/// and that call is where the note's byte key is written, not here (see its doc for why the insert lives in
/// `db`).
fn store_note(
    db: &Db,
    rel: &str,
    existing: Option<i64>,
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
            rel,
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

/// `case_key` → every live path carrying that fold: the corpus [`crate::links::resolve`] places targets in.
///
/// A `Vec` rather than the one `rel` an earlier build kept, because `files.case_key` stopped being unique
/// with migration 4 (F13 §3): `Ämli.md` beside `ämli.md` is two files whose display fold is one string, so
/// `insert` would have handed the fold to whichever row was read last and a link naming that fold would land
/// on it by iteration order. Listing both is what lets resolution see the ambiguity and decline, exactly as
/// [`crate::links`] already models an ambiguous basename. A fold with one owner still resolves.
fn live_corpus(db: &Db) -> Result<BTreeMap<String, Vec<String>>, DbError> {
    let mut statement =
        db.0.prepare("SELECT case_key, rel FROM files WHERE state = 'ok' ORDER BY case_key, rel")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut corpus: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        let (case_key, rel) = row?;
        corpus.entry(case_key).or_default().push(rel);
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
fn unresolved_links(db: &Db) -> Result<i64, DbError> {
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

/// The index files this workspace holds from the last scan, from [`INDEX_FILES_KEY`].
fn index_list(db: &Db) -> Result<Vec<String>, DbError> {
    let stored = meta_value(db, INDEX_FILES_KEY)?;
    // One name per line, and no names at all when the value is an empty list rather than absent.
    Ok(stored.map_or_else(Vec::new, |value| {
        value.lines().map(str::to_string).collect()
    }))
}

/// One `meta` value as this program wrote it: `None` when the key was never written or holds NULL.
///
/// The column is read as `Option<String>` deliberately. A row holding NULL is what anyone gets after
/// poking at the database by hand, and reading it as a plain `String` turns that into
/// `InvalidColumnType` — an error that fails the whole scan, contradicting what [`parse_version`]
/// promises two lines below its own name.
fn meta_value(db: &Db, key: &str) -> Result<Option<String>, DbError> {
    Ok(db
        .0
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// The one `meta` upsert this module writes, shared by the published index-file list and the parse rules
/// version, so neither invents its own conflict clause.
fn set_meta(db: &Db, key: &str, value: &str) -> Result<(), DbError> {
    db.0.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Record the index files this scan publishes, so a later scan knows which shards to delete.
///
/// Written inside the scan's transaction, before the files exist: `meta` describes the catalog this
/// program is responsible for, and the next scan tolerates naming a file that never arrived
/// ([`write_index_files`] ignores `NotFound`). Committing it afterwards cannot fail safely — a kill in
/// that window leaves a shard on disk that no delete list mentions.
fn set_index_list(db: &Db, rendered: &[(String, String)]) -> Result<(), DbError> {
    let value = rendered
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    set_meta(db, INDEX_FILES_KEY, &value)
}

/// The parse rules version this workspace recorded under [`PARSE_VERSION_KEY`].
///
/// `None` covers "the key is absent" and "the value is not a number this build can read" — a NULL, a
/// hand-edited file, a spelling from a future version. Both mean the same thing here: the stored rows
/// carry no label this build can trust, so they are rewritten and the truth recorded.
fn parse_version(db: &Db) -> Result<Option<u32>, DbError> {
    Ok(meta_value(db, PARSE_VERSION_KEY)?.and_then(|value| value.trim().parse().ok()))
}

/// Whether this scan has to re-parse every markdown note it reads.
///
/// Equality with [`PARSE_VERSION`] and nothing else means up to date. A database holding a *higher* value
/// is rewritten here too, and the recorded value moves back to what this build actually wrote: guessing
/// that newer rows are still valid would leave data labelled with rules this binary does not have, and one
/// full re-parse is cheap next to a catalog that quietly disagrees with the code that renders it.
fn refresh_needed(stored: Option<u32>) -> bool {
    stored != Some(PARSE_VERSION)
}

/// Whether a refresh reached every note it owed new derived rows.
///
/// Two ways to miss one, neither visible in [`ScanReport`]: a file that changed or became unreadable
/// between the walk and the read (`uncovered`, since the write pass skips its marker), and a live row
/// under a directory the walk could not enter (`hidden_by_unreadable`, dropped by [`known_rows`] so it
/// cannot be mistaken for deleted). Recording the version over either would label rows as current that
/// an older build wrote, permanently — the exact failure F15 exists to prevent, reintroduced by the fix.
/// Retrying is the cheaper fault: one subtree stays stale until it becomes readable again, and the scan
/// after that records the version once the corpus is covered.
///
/// Notes with existing rows only, on both sides. Three things are deliberately not charged, because no
/// parse rule can stale what does not exist: an unreadable image or mode-000 download folder, a markdown
/// path that has never had a `notes` row (it is indexed as `Added` whenever it next becomes readable), and
/// the loser of a case collision, which is refused at insert and normally holds no row of its own. Charging
/// any of them would re-parse the whole corpus on every scan forever and repair nothing.
///
/// What *can* still hold the version back indefinitely is stale data that exists and cannot be rewritten: a
/// note whose
/// rows are there but unreadable (a mode-000 `.md`, or one inside a directory nobody can enter), a live row
/// hidden behind such a directory, and a move refused by an identity conflict — judged by the path it came
/// from, since `classify` suppressed that path's `Removed` and its rows are still in the catalog. That loop
/// is worth paying for: those rows stay wrong until the obstruction clears, and the notice below names the
/// count so this is never silent.
fn refresh_covered(written: &Written, hidden_by_unreadable: usize) -> bool {
    written.uncovered == 0 && hidden_by_unreadable == 0
}

/// Say out loud that a scan re-parsed notes whose files did not change — unless there is nothing to say.
///
/// The report itself stays silent on purpose ([`ScanReport`] counts what classification decided, and its
/// JSON shape is pinned by the CLI tests), so without this line a rule change would reach a workspace and
/// rewrite every derived row in it while `np scan` printed `modified 0`. stderr only, for the same reason
/// the orphan receipt is: `--json` must keep stdout parseable.
///
/// The sink is a parameter because stderr cannot be redirected in-process — the standard library offers no
/// way to replace the global handle — so the tests in this module drive the formatting directly, exactly as
/// [`crate::db`] does for its receipt. What they cannot pin is that the production call site names stderr.
fn report_refresh(out: &mut dyn io::Write, stored: Option<u32>, notes: usize) {
    // Zero means the refresh cost nothing extra: a first scan re-parses nothing it was not going to parse
    // as added anyway. "Rules moved, nothing to do" is not news.
    if notes == 0 {
        return;
    }
    writeln!(
        out,
        "note-processor: re-parsed {notes} unchanged path(s) for parse rules v{PARSE_VERSION} (database held {})",
        stored.map_or_else(|| "nothing".to_string(), |version| format!("v{version}")),
    )
    .expect("printing the re-parse receipt to stderr");
}

/// Say out loud that a refresh could not label the corpus, so the next scan will try again.
///
/// stderr for the same reason as [`report_refresh`]: stdout stays machine-readable JSON. `missed` counts
/// both ways a note escapes — skipped during the write pass and hidden behind an unreadable directory.
fn report_refresh_gap(out: &mut dyn io::Write, missed: usize) {
    if missed == 0 {
        return;
    }
    writeln!(
        out,
        "note-processor: parse rules v{PARSE_VERSION} not recorded — {missed} note(s) were not re-parsed this scan; the next scan will retry"
    )
    .expect("printing the incomplete-refresh notice to stderr");
}

/// Write the rendered catalog and delete the shards this scan no longer produces.
///
/// The order is the whole contract, because one set of files cannot be published atomically:
///
/// 1. Every shard, each written to a temporary name and moved in with one `rename`, so a reader never
///    sees a half-written file (spec §7's whole-file regenerate).
/// 2. `INDEX.md` last. Until it moves, the old catalog still names only files that exist; from then on
///    the new one does too. Renaming it first is what let an entry point advertise shards that were
///    never written while superseded shards of another generation survived beside them.
/// 3. Superseded names, only once the entry point that no longer mentions them is live.
///
/// The names removed in step 3 come from [`superseded_names`] — the directory and `meta` together — rather
/// than from `meta` alone. A name already gone from disk is the state this call wanted, which is what
/// makes [`set_index_list`] safe to commit ahead of these writes.
///
/// Returns the root entries that wear a generated name without carrying this program's fingerprint: they
/// are left exactly as they are and reach [`ScanReport::skipped`], because spec §7 grants regeneration of
/// what np published and not removal of whatever is named like it.
fn write_index_files(
    root: &Path,
    rendered: &[(String, String)],
    previous: &[String],
) -> Result<Vec<String>, ScanError> {
    // Collected before anything is written: the deletion set is what disk and `meta` hold minus this
    // scan's generation, so reading the root first cannot see a file this call is about to publish — and
    // a root that cannot be listed fails the scan before it has touched anything.
    let superseded = superseded_names(root, rendered, previous)?;

    let entry_point = index_names::ENTRY;
    for (name, contents) in rendered.iter().filter(|(name, _)| *name != entry_point) {
        publish(root, name, contents)?;
    }
    for (name, contents) in rendered.iter().filter(|(name, _)| *name == entry_point) {
        publish(root, name, contents)?;
    }

    for name in superseded.delete {
        let path = root.join(&name);
        match fs::remove_file(&path) {
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(ScanError::Io { path, source }),
            Ok(()) => {}
        }
    }
    Ok(superseded.foreign)
}

/// The generated names this workspace holds that this scan does not publish: what `meta` lists and what
/// is actually sitting in the root, unioned.
///
/// `meta` alone is not enough. It is committed inside the scan's transaction (see [`set_index_list`]), so
/// a kill between that commit and the writes in [`write_index_files`] leaves it naming only the
/// generation that was never published; a superseded shard still on disk appears in no list, and nothing
/// would ever delete it. The listing sees reality instead, and the union covers both directions — which
/// is also what lets `meta` be committed ahead of the files.
///
/// Both sources are filtered by [`index_names::is_generated_index`], which only decides what is *worth
/// asking about*. What decides what is ours to remove is the file's own fingerprint: a candidate is deleted only
/// when it is a regular file opening with [`catalog::HEADER`] byte for byte, in either its LF or its CRLF
/// spelling (see [`np_published`]). A name that merely looks generated belongs to somebody else — a
/// hand-written `INDEX-old.md` in the root is never indexed, because [`walk`] excludes these names, so
/// deleting it would destroy a note no scan ever wrote (spec §7). Such a candidate, and a directory
/// wearing the name, are returned for the report instead: the user learns about the collision, and a
/// `remove_file` that could only answer `IsADirectory` never breaks every later scan of the workspace.
///
/// The fingerprint is read as the *whole* identity signal, and that has one accepted consequence: a file
/// carrying those two header lines under a generated name **is** deleted even when this program never
/// published it and a person wrote the prose. Authorship is not recoverable from anything else on disk —
/// separating an edited shard from a hand-written note would mean rendering the corpus and comparing
/// bytes, the guess the fingerprint exists to avoid — so the header decides, not who typed the body.
/// `a_hand_written_file_carrying_our_header_under_a_generated_name_is_deleted` pins that as intended
/// behaviour, and `docs/backlog-m2-followups.md` records it as an accepted residual.
///
/// A `meta` name no longer on disk appears in neither list. Its absence is already the state this call
/// wanted — which is exactly how the delete loop in [`write_index_files`] reads an
/// [`io::ErrorKind::NotFound`] — and naming it in the report would be a collision nobody can see.
fn superseded_names(
    root: &Path,
    rendered: &[(String, String)],
    previous: &[String],
) -> Result<Superseded, ScanError> {
    let entries = fs::read_dir(root).map_err(|source| ScanError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    // Every generated-looking name in the root, tagged with whether it is ours. The verdict is taken while
    // the listing runs, because `entry.file_type` is the cheapest answer to "file or directory" and a
    // second `stat` would race with whatever renamed the entry between the two calls.
    let mut present: Vec<(String, bool)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ScanError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !index_names::is_generated_index(&name) {
            continue;
        }
        // The file type is asked before the contents: a directory has no header to read, and letting
        // `File::open` answer for it would pretend a directory could be this program's output.
        let ours =
            entry.file_type().is_ok_and(|kind| kind.is_file()) && np_published(&entry.path());
        present.push((name, ours));
    }

    // The union the crash window needs: what `meta` names and what the root holds. A name in either list
    // is judged by what is actually on disk, never by which list produced it.
    let mut names: Vec<String> = present.iter().map(|(name, _)| name.clone()).collect();
    names.extend(
        previous
            .iter()
            .filter(|name| index_names::is_generated_index(name))
            .cloned(),
    );
    names.sort();
    names.dedup();

    let mut superseded = Superseded::default();
    for name in names {
        if rendered.iter().any(|(published, _)| published == &name) {
            continue;
        }
        match present.iter().find(|(found, _)| *found == name) {
            Some((_, true)) => superseded.delete.push(name),
            // On disk, but not ours: a foreign note or a directory wearing one of our names.
            Some((_, false)) => superseded.foreign.push(name),
            // Named by `meta` and already gone — the state this call wanted, which is how the delete loop
            // in [`write_index_files`] reads an [`io::ErrorKind::NotFound`]. Reporting it would name a
            // collision that is nothing but a shard this program never got round to writing.
            None => {}
        }
    }
    Ok(superseded)
}

/// The generated names one scan found in its root that it does not publish, split by who owns them.
#[derive(Default)]
struct Superseded {
    /// Files this program published in an earlier generation: safe to unlink.
    delete: Vec<String>,
    /// Entries wearing a generated name that this program did not publish: left on disk and reported.
    foreign: Vec<String>,
}

/// Whether `path` opens with the bytes [`catalog::render`] puts at the top of every file it publishes —
/// two header lines, byte for byte, in either the LF or the CRLF spelling. That is how a scan tells its
/// own output from a note that happens to be called `INDEX-something.md`.
///
/// Only those bytes are read, so a candidate's size never decides what a scan costs, and every failure —
/// unreadable, shorter than the header, an I/O error — answers "not ours". The bias is deliberate:
/// guessing wrong keeps a stale shard on disk rather than deleting a note.
///
/// CRLF is accepted because identity lives in the two header lines and nowhere else: a checkout, editor
/// or sync tool that rewrote the line endings of a shard np published leaves the catalog intact, and
/// refusing it would strand that shard in `skipped` forever. The same rule is why nothing after the
/// header is examined — prose cannot change who wrote the file — and why the match stays line-exact: any
/// other byte difference in those two lines, including a half-converted header, is not ours.
fn np_published(path: &Path) -> bool {
    let mut source = match fs::File::open(path) {
        Ok(source) => source,
        Err(_) => return false,
    };
    // The longer of the two accepted spellings sets the window. A file that ends earlier simply yields
    // fewer bytes, which is how a published shard whose header is its whole content still matches.
    let mut head = [0u8; catalog::HEADER_CRLF.len()];
    let read = match read_prefix(&mut source, &mut head) {
        Ok(read) => read,
        Err(_) => return false,
    };
    let head = &head[..read];
    head.starts_with(catalog::HEADER.as_bytes())
        || head.starts_with(catalog::HEADER_CRLF.as_bytes())
}

/// Fill `buf` from `source`, stopping at the end of the file. Returns how many bytes `buf` holds; an I/O
/// error is reported so the caller can answer its own question with "not ours".
fn read_prefix(source: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut read = 0;
    while read < buf.len() {
        match source.read(&mut buf[read..]) {
            // End of file: fewer bytes than asked for, which is an answer rather than a failure.
            Ok(0) => break,
            Ok(got) => read += got,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(read)
}

/// Move one rendered file into place through a temporary name, so a reader never sees it half-written
/// and a kill leaves either the old bytes or the new ones.
fn publish(root: &Path, name: &str, contents: &str) -> Result<(), ScanError> {
    let temp = root.join(format!("{name}{TEMP_SUFFIX}"));
    let target = root.join(name);
    fs::write(&temp, contents).map_err(|source| ScanError::Io {
        path: temp.clone(),
        source,
    })?;
    fs::rename(&temp, &target).map_err(|source| ScanError::Io {
        path: target,
        source,
    })?;
    Ok(())
}

/// Remove the `INDEX*.tmp` files a rename that died left behind.
///
/// A temporary name is not one [`index_names::is_generated_index`] recognises, so an orphaned corpse left by a
/// killed `rename` would be walked, hashed and catalogued-as-nothing as an ordinary attachment, and
/// nothing would ever clean it up. That is why [`scan`] calls this before the walk rather than where the
/// temporaries are made: cleaned up on the way out, the corpse would still be indexed once by its own
/// scan. Removal stops at directories — something named `INDEX.md.tmp` that is one belongs to somebody
/// else.
///
/// A corpse that will not go away (an immutable or locked file, a root this user may list but not write)
/// comes back in the returned list and reaches [`ScanReport::skipped`] instead of failing the scan: one
/// stuck temporary must not stop every later scan of a workspace whose notes read perfectly well. It is
/// indexed once by the scan that could not remove it — cleanup runs before the walk, so the file is there
/// when the walk looks — and the next scan tries again.
///
/// One limitation to know before someone widens this: cleanup is **root-only**, while
/// [`index_names::is_generated_index`] is applied by [`crate::walk`] to root-level names only (depth 1) —
/// so a `notes/INDEX.md.tmp`
/// would be indexed as an attachment forever. It is unreachable today because [`publish`] joins the root
/// alone and no other code makes these names.
fn clear_temp_corpses(root: &Path) -> Result<Vec<String>, ScanError> {
    let entries = fs::read_dir(root).map_err(|source| ScanError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    remove_corpses(entries, remove_file).map_err(|source| ScanError::Io {
        path: root.to_path_buf(),
        source,
    })
}

/// The loop behind [`clear_temp_corpses`]: every generated-looking temporary in the listing, removed or
/// reported. `remove` is [`remove_file`] in production and a failing stub in the test that pins the
/// tolerance rule, because an undeletable regular file needs filesystem privileges this suite does not
/// have. A failure to *list* stays an error — that is the workspace being unreadable, which no later scan
/// can fix either.
fn remove_corpses(
    entries: fs::ReadDir,
    remove: fn(&Path) -> io::Result<()>,
) -> Result<Vec<String>, io::Error> {
    let mut skipped = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(TEMP_SUFFIX) else {
            continue;
        };
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !index_names::is_generated_index(stem) || is_dir {
            continue;
        }
        if let Err(source) = remove(&entry.path()) {
            // Already gone is the state this call wanted; anything else is reported and retried.
            if source.kind() != io::ErrorKind::NotFound {
                skipped.push(name);
            }
        }
    }
    Ok(skipped)
}

/// [`fs::remove_file`] with its parameter fixed to `&Path`, so [`remove_corpses`] can accept it as a
/// function pointer beside a stub that always fails.
fn remove_file(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
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

#[cfg(test)]
mod tests {
    use super::{
        np_published, remove_corpses, remove_file, report_refresh, report_refresh_gap,
        PARSE_VERSION,
    };
    use crate::catalog::{HEADER, HEADER_CRLF};
    use std::fs;
    use std::io::{self, ErrorKind};
    use std::path::Path;

    /// A root holding one `INDEX.md.tmp` corpse and one file that merely looks similar.
    fn root_with(names: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().expect("test setup: tempdir");
        for name in names {
            fs::write(tmp.path().join(name), "stale\n").expect("test setup: plant a file");
        }
        tmp
    }

    /// The deletion fingerprint accepts exactly two spellings of the header and nothing else.
    #[test]
    fn the_published_fingerprint_is_ours_in_either_line_ending_only() {
        // Drift guard first: the accepted CRLF spelling is `HEADER` with Windows line endings and nothing
        // more, so rewording the header cannot leave one spelling silently unrecognised.
        assert_eq!(HEADER.replace('\n', "\r\n"), HEADER_CRLF);

        let root = tempfile::TempDir::new().expect("test setup: tempdir");
        let cases: [(&str, String, bool); 8] = [
            ("lf.md", format!("{HEADER}one entry\n"), true),
            // A published file with no entries is the header and nothing else.
            ("lf-bare.md", HEADER.to_string(), true),
            ("crlf.md", format!("{HEADER_CRLF}one entry\r\n"), true),
            ("crlf-bare.md", HEADER_CRLF.to_string(), true),
            // Neither accepted spelling, whatever a reader might guess from the bytes: a header
            // converted on one line only, a header with other words in it, too little of a header to
            // read, and an ordinary note.
            (
                "mixed.md",
                "# INDEX\r\n_generated by note-processor; do not edit_\nbody\n".to_string(),
                false,
            ),
            (
                "reworded.md",
                "# INDEX\r\n_generated by somebody else; do not edit_\r\nbody\r\n".to_string(),
                false,
            ),
            ("short.md", "# IND".to_string(), false),
            ("note.md", "# Meine Notizen\nbody\n".to_string(), false),
        ];

        for (name, body, ours) in &cases {
            let path = root.path().join(name);
            fs::write(&path, body).expect("test setup: write a candidate");
            assert_eq!(
                np_published(&path),
                *ours,
                "{name} opens {body:?}, which is {}ours",
                if *ours { "" } else { "not " }
            );
        }

        // A candidate that cannot be opened at all answers "not ours" rather than failing the scan.
        assert!(
            !np_published(&root.path().join("absent.md")),
            "an unreadable candidate is not a deletion licence"
        );
    }

    /// The refresh receipt: one line naming the count and both versions, silence when there is nothing to
    /// report. Pinned through a sink because stderr cannot be redirected in-process.
    #[test]
    fn the_refresh_receipt_names_the_count_and_both_versions() {
        let mut out = Vec::new();
        report_refresh(&mut out, Some(3), 12);
        assert_eq!(
            String::from_utf8(out).expect("the receipt is utf-8"),
            format!(
                "note-processor: re-parsed 12 unchanged path(s) for parse rules v{PARSE_VERSION} \
                 (database held v3)\n"
            ),
        );

        // A database written before the mechanism existed says so instead of claiming a version zero.
        let mut out = Vec::new();
        report_refresh(&mut out, None, 1);
        assert!(
            String::from_utf8_lossy(&out).contains("database held nothing"),
            "{out:?}"
        );

        // Every first scan of an empty-or-all-added workspace passes through here with nothing to say.
        let mut out = Vec::new();
        report_refresh(&mut out, None, 0);
        assert!(
            out.is_empty(),
            "a refresh that re-parsed nothing is not news: {out:?}"
        );
    }

    /// The incomplete-refresh notice: one line naming what was missed, silence when nothing was.
    #[test]
    fn an_incomplete_refresh_says_the_version_was_not_recorded() {
        let mut out = Vec::new();
        report_refresh_gap(&mut out, 3);
        assert_eq!(
            String::from_utf8(out).expect("the notice is utf-8"),
            format!(
                "note-processor: parse rules v{PARSE_VERSION} not recorded — 3 note(s) were not re-parsed this scan; the next scan will retry\n"
            ),
        );

        // A covering refresh has nothing to apologise for.
        let mut out = Vec::new();
        report_refresh_gap(&mut out, 0);
        assert!(
            out.is_empty(),
            "a covered refresh is not a problem: {out:?}"
        );
    }

    #[test]
    fn an_undeletable_corpse_is_reported_rather_than_fatal() {
        let root = root_with(&["INDEX.md.tmp"]);
        // The shape of an immutable or locked file: present, listed, and refusing to be unlinked.
        let refused: fn(&Path) -> io::Result<()> = |_| Err(io::Error::other("refused"));

        let skipped = remove_corpses(fs::read_dir(root.path()).expect("test: read root"), refused)
            .expect("a stuck corpse must not fail the scan");

        assert_eq!(
            skipped,
            vec!["INDEX.md.tmp".to_string()],
            "it is named for the report and retried next scan"
        );
    }

    #[test]
    fn a_corpse_that_is_already_gone_is_the_state_wanted() {
        let root = root_with(&["INDEX-1-2.md.tmp"]);
        let vanished: fn(&Path) -> io::Result<()> = |_| Err(io::Error::from(ErrorKind::NotFound));

        let skipped = remove_corpses(
            fs::read_dir(root.path()).expect("test: read root"),
            vanished,
        )
        .expect("a corpse that raced away is not a failure");

        assert!(skipped.is_empty(), "gone means done: {skipped:?}");
    }

    #[test]
    fn the_pass_touches_no_name_that_is_not_ours() {
        let root = root_with(&["INDEX-1-2.md.tmp", "notes.md", "INDEX-old.md"]);

        let skipped = remove_corpses(
            fs::read_dir(root.path()).expect("test: read root"),
            remove_file,
        )
        .expect("test: removal pass");

        assert!(skipped.is_empty(), "{skipped:?}");
        assert!(
            !root.path().join("INDEX-1-2.md.tmp").exists(),
            "ours to clean"
        );
        assert!(
            root.path().join("notes.md").is_file() && root.path().join("INDEX-old.md").is_file(),
            "a note is never this pass's to delete (spec §7)"
        );
    }
}
