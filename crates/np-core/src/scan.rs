//! The one pass that turns a workspace folder into a registry, note rows and a catalog.
//!
//! [`scan`] is the only place in the crate that reads files, writes SQLite and renders the catalog at
//! once, so it owns the ordering rules the individual stages cannot express on their own:
//!
//! - **One transaction for the database, files written after it commits.** The workspace row, the note
//!   row and everything derived from its text either all move to this scan's state or none of them do.
//!   (The registry line in `config.rs`'s database sits outside that set on purpose: it is written on a second
//!   connection after [`crate::db`] commits, so a registry failure exits 1 over work that is already durable,
//!   and the next scan reconciles it — the alternative was one transaction across two databases.)
//!   `INDEX.md` cannot join a SQLite transaction, so the catalog is rendered inside that transaction,
//!   its name list is committed with it, and only then does the filesystem work start. A scan that dies
//!   halfway leaves the previous catalog readable instead of half-rewritten, and never leaves a delete
//!   list that lags the files on disk (see `set_index_list` and `write_index_files` for the order).
//! - **Unknown is not absent.** A directory the walk could not enter, a file that vanished or was
//!   rewritten between the walk and the read, and a name whose byte spelling cannot be held (F18) are named
//!   in [`ScanReport::skipped`] and keep the row they already have (Review Focus 5). Reporting any of them
//!   as `Removed` would stale live notes because of a permission error, a race, or a name this program
//!   refuses to guess at.
//! - **Parse rules reach notes that did not change.** Byte comparison cannot see a rule change, so
//!   `meta` records the parse rules version the rows were written with and a scan that finds a different
//!   value re-parses every markdown note once (see [`PARSE_VERSION_KEY`]). Classification stays the
//!   truth about the *files*: such a scan still reports its notes as unchanged.
//! - **The catalog reads live rows only.** Entries and [`ScanReport::notes`] come from
//!   `files.state = 'ok'`; [`crate::db::note_counts`] stays the unfiltered database truth, because a
//!   stale note is hidden from the catalog, not forgotten (spec §2).
//! - **`INDEX.md` and its shards are the only names this module creates, and it deletes only the ones
//!   that open with its own generated header** (spec §7): a name alone never licenses a removal — with
//!   **one exception, our own temporaries**. A leftover `<generated>.tmp` from a killed write is cleared by
//!   `clear_temp_corpses` on the naming rule alone: that spelling is one nothing else produces, and a
//!   half-written temporary has no complete header to fingerprint. The F13 §1 review flagged the apparent
//!   contradiction; it is a naming-trust decision rather than an oversight, stated here so the rule above can
//!   be read literally. The
//!   names themselves come from [`index_names`], which owns their spelling; what this module owns is the
//!   fingerprint deciding which of them are ours to replace. Every write goes through a temporary file
//!   plus `rename`, and no note body is ever opened for writing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use thiserror::Error;

use crate::catalog::{self, CatalogEntry};
use crate::change::{classify_with_identities, Change, FileIdentities};
use crate::chunk::{chunk, CHUNKER_VERSION};
use crate::config::{abs_path, Config, ConfigError, MarkerConfig, WorkspaceRegistry};
use crate::db::{self, Db, DbError, LIVE_EDGE, PROSE_ORIGIN};
use crate::hash::{sha256_bytes, sha256_file, Snapshot};
use crate::index_names;
use crate::items::{extract_items, ItemCandidate};
use crate::links::{resolve, store_links, RawLink};
use crate::parse::{body_start, parse_markdown, NoteDoc, PARSE_VERSION};
use crate::path_keys::{os_bytes, RelPath};
use crate::walk::{self, DiscoveredFile, UnknownPath, WalkOptions};

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

/// `meta` key holding the unix seconds of the last scan that **committed** for this workspace.
pub const LAST_SCAN_KEY: &str = "last_scan_unix";
/// `meta` key holding the value of [`crate::parse::PARSE_VERSION`] that this workspace's derived rows
/// were written with. Absent means "written before this mechanism existed", which is not a version to
/// compare against and therefore refreshes; see the comparison in `refresh_needed`.
pub const PARSE_VERSION_KEY: &str = "parse_version";
/// `meta` key holding [`crate::chunk::CHUNKER_VERSION`] for the workspace's `chunks` rows — the value
/// design-m2-embeddings §5 calls `EMBED_VERSION`, spelled by its role rather than by a second number,
/// because the only definition it can describe is the chunker that cut those rows. Recording a second
/// constant beside `CHUNKER_VERSION` would let the two disagree in the file while claiming one fact.
///
/// **Chunker coverage only.** The record answers "did this build's chunking reach every note it owed", and
/// nothing else. Provider availability is not version debt (§5's P0, and the decision comment sits on
/// `record_chunk_version` where it is taken): with the default `embeddings.provider = "none"` the key is
/// recorded anyway, because a withheld key would be a coverage gap no user can clear.
pub const EMBED_VERSION_KEY: &str = "embed_version";
/// `meta` key holding what the **last scan** of this workspace could not index, beside what its registry
/// currently duplicates.
///
/// Beside [`crate::db::FK_ORPHANS_KEY`] and [`crate::db::PATHS_LOSSY_KEY`] for the same reason (F22 §1): a
/// refusal is a fact with no afterlife otherwise — one line on stdout or stderr at the moment it happened,
/// gone as soon as the terminal closes, while the note stays missing from the catalog until somebody
/// renames the file. There is no logger to write to and the catalog's bytes may not move, so `meta` is where
/// a person reading the workspace later has to be able to ask what was lost.
///
/// The value is `unreadable=N;unkeyable=N;duplicates=N;names=a,b`, written **even when every count is
/// zero**: this record describes the *last scan* rather than the history of the file, so a fixed workspace
/// has to overwrite an old complaint instead of leaving it standing there forever (F22 §2). `names` holds
/// up to eight display spellings joined by `,` — see [`Unindexable::names`] for their order — and a path
/// holding a comma reads as two entries, because this is a diagnosis for a human at an `sqlite3` prompt and
/// not a format to parse. Nothing decides anything with it: no deletion rule reads this key (F22 §4.2).
pub const UNINDEXABLE_KEY: &str = "unindexable";
/// Suffix of the temporary file every catalog write goes through before its `rename`.
const TEMP_SUFFIX: &str = ".tmp";
/// How many display spellings one record names, most actionable first (see [`Unindexable::names`]).
///
/// Bounded because the value is written on **every** scan: a workspace with an unreadable subtree under a
/// `chmod -R 000` would otherwise grow one `meta` row's text without limit, and a person reading it can act
/// on eight paths no better than on eight hundred.
const NAME_CAP: usize = 8;

/// A note re-parsed by one scan: its path and the links its text holds, which is everything
/// [`store_note_links`] needs once the whole corpus is up to date. The path travels in both spellings so the
/// id lookup that follows can go by bytes (F13 §4.1).
type Parsed = (RelPath, Vec<RawLink>);

/// The two spellings of one found file, as the write pass and the link pass both need them.
fn path_of(snapshot: &Snapshot) -> RelPath {
    RelPath::new(snapshot.rel_bytes.clone(), snapshot.rel.clone())
}

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
    /// vanished or changed between walk and read, paths whose identity is already taken, a name the byte
    /// spelling cannot hold (F18), a catalog temporary it could not remove (see `clear_temp_corpses`), and a root
    /// entry wearing a generated catalog name that this program did not publish — left on disk and named here
    /// instead of deleted.
    /// Sorted and deduplicated by the path's **bytes**, printed as display text (F20). Where the keys differ, two
    /// paths print two lines that look alike, because the report can name them no differently — they are not a
    /// duplicate to clean up later, and collapsing them would hide one lost path. Where the key is itself lossy —
    /// a Windows refusal, whose identity bytes come from `to_string_lossy` — two names still share a single line,
    /// which `crates/np-core/tests/walk.rs` pins. So this list is a floor on what the scan lost, never a count of
    /// it.
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
    /// What **this process's own** schema upgrade deleted, spelled exactly as the receipt it stored under
    /// [`crate::db::FK_ORPHANS_KEY`] — and `None` for every other scan.
    ///
    /// Two conditions make it `Some`: this open ran the orphan cleanup ([`crate::db::Db::upgrade_orphans`],
    /// not a later open of the same file), and that cleanup removed at least one row.
    ///
    /// **Once per process rather than on every scan.** The stored key is never cleared, so "the upgrade
    /// removed N rows" stays true for the life of the file; echoing it from then on would be a permanent
    /// one-line warning, and a permanent warning trains the reader to ignore it — which is how real
    /// signals die. The scan that did the deleting is the one run that needs saying.
    ///
    /// **And the key is never cleared after the echo.** Clearing would make this line load-bearing: a run
    /// whose stdout went to a closed pipe, or a `--json` nobody read, would take the fact with it. Left in
    /// place, the echo stays a convenience and `meta` stays the record. That is also where the two part
    /// ways by design: an all-zero receipt **is** still written, because it proves the check ran, while
    /// this field reports a **loss** and has nothing to report at zero — so a fresh workspace's first scan
    /// prints no line and its `--json` says `"upgrade_orphans": null`, even though its database now holds
    /// `headings=0,tags=0,…`.
    ///
    /// **Null, not absent.** The key is there on every scan report whatever the outcome, because one JSON
    /// shape is worth more to its one consumer than one fewer `null`: with the key suppressed at zero, a
    /// reader cannot tell "nothing was removed" apart from "this build stopped reporting it", and every
    /// reader grows a missing-key branch to answer a question `null` already answered.
    pub upgrade_orphans: Option<String>,
    /// What **this process's own** `Db::open` merged, spelled exactly as the receipt it stored under
    /// [`crate::db::NOTE_MERGE_KEY`] — and `None` for every other scan.
    ///
    /// The same two conditions as [`Self::upgrade_orphans`], for the same reasons: this open ran migration 5
    /// ([`crate::db::Db::merged_notes`], not a later open of the same file), and that merge folded at least
    /// one duplicate note row. A `merged=0` receipt stays in `meta` as the record that the check ran while
    /// this field reports work done, so an ordinary workspace's report says `null`.
    ///
    /// **Null, not absent**, for the reason [`Self::upgrade_orphans`] gives: one JSON shape for every scan.
    pub merged_notes: Option<String>,
    /// What this **file records** under [`crate::db::FK_ORPHANS_KEY`], written by this open or by any earlier one —
    /// `None` only when no such record exists.
    ///
    /// **The other half of F21, and the question it existed for.** [`Self::upgrade_orphans`] answers "what did this run
    /// delete" and is quiet from then on; someone asking a month later had to open `sqlite3`. This field is that record
    /// read back through `np scan`, so the durable receipt stops being readable only by whoever brings a database shell.
    /// It changes no announcement, clears nothing, and prints nothing new on stdout: an old fact stays available without
    /// any run becoming louder for it, which is the balance F21 §3 struck and this field keeps.
    ///
    /// **Null means no record, not a zero receipt.** A file created at the current schema carries
    /// `headings=0,tags=0,…` — proof the sweep ran — and that string reaches here verbatim; only a workspace whose file
    /// never went through migration 3 has nothing to report. Substituting `Some("")` would make an audited fact and an
    /// absent one read alike.
    ///
    /// **Null, not absent**, for the reason [`Self::upgrade_orphans`] gives: one JSON shape for every scan.
    pub recorded_upgrade_orphans: Option<String>,
    /// What this **file records** under [`crate::db::NOTE_MERGE_KEY`], same contract as
    /// [`Self::recorded_upgrade_orphans`]: written by any open, `None` only when the file holds no such record.
    pub recorded_merged_notes: Option<String>,
    /// What this scan could not index, and what the registry duplicates — counted by kind, which is the
    /// diagnosis [`ScanReport::skipped`] cannot give because it merges every cause into one list of paths
    /// (F22 §1). `None` when every count is zero, so a workspace that lost nothing keeps a clean report.
    ///
    /// **Null, not absent**, exactly like [`Self::upgrade_orphans`]: the JSON key is there on every scan
    /// whatever the outcome, because a reader must be able to tell "nothing was lost" apart from "this build
    /// stopped reporting it", and every reader would otherwise grow a missing-key branch to answer a question
    /// `null` already answered.
    ///
    /// The durable copy is `meta`'s [`UNINDEXABLE_KEY`] value, written after this scan's commit and overwritten
    /// by the next one. This field is not read back from it, and nothing that *decides* anything reads either:
    /// unknown-is-not-absent stays settled by the walk's own lists (F22 §4.2), so a refused record write
    /// degrades the diagnosis and never changes what is indexed.
    pub unindexable: Option<Unindexable>,
    /// Wall-clock milliseconds for the whole scan. Informational only — never in a generated file,
    /// where it would make every regeneration differ (spec §10).
    pub elapsed_ms: u128,
}

/// What one scan could not index, and what its registry duplicates — F22's diagnosis, counted by kind.
///
/// This exists because [`ScanReport::skipped`] cannot carry it: one merged list names an unreadable
/// directory and a name the byte spelling refuses identically, so even the live line cannot tell a
/// permission problem from an unkeyable name (F22 §1). The list itself keeps its entries **verbatim** —
/// causes arrive here, as separate counts, rather than by rewording strings a person's `grep` already
/// depends on.
///
/// Its durable copy is the [`UNINDEXABLE_KEY`] value this scan writes after its commit; [`Self::value`] is
/// that one spelling, so `meta`, `np scan`'s summary line and this struct's own fields describe one fact
/// rather than three recomputations of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unindexable {
    /// Directories whose contents this walk did not read: permission denied, or a directory that vanished
    /// mid-walk. Somebody's `chmod`, not anybody's filename.
    pub unreadable: usize,
    /// Names the byte spelling cannot hold (F18) — the count [`Self::unreadable`] refuses to be confused
    /// with, because this one is fixed by renaming a file. Windows-only in practice: `os_bytes` is exact on
    /// Unix, so no scan run there ever raises this above zero (see [`crate::walk::Walked::unrepresentable`]).
    pub unkeyable: usize,
    /// Display paths the registry holds **more than one live (`state = 'ok'`) row** for.
    ///
    /// **This is a question, not an accusation.** Two genuinely different files can share a display spelling —
    /// latin-1 `caf\xE9/note.md` beside `caf\xE8/note.md`, both readable, both correctly indexed — and they land
    /// here exactly as F23's one-file-two-rows does. It also counts attachments, which have no catalog entry at
    /// all. So read it as "look at these paths" and confirm with `sqlite3` before deleting anything (F22 §4.5).
    ///
    /// It used to promise to be "deliberately unexplained"; [`Self::ambiguous`] beside it now names one possible
    /// cause, so this count stays what it is — a pointer at paths — and the reader who wants the reason reads the
    /// other field.
    pub duplicates: usize,
    /// Notes whose registry row is **keyed from display text** and could not be settled this scan (F23): the walk
    /// could not read their own path, and no listed readable path both spells them exactly and matches their stored
    /// hash, so nothing could say which byte spelling the row belongs to.
    ///
    /// `N` is exactly the population that holds [`PARSE_VERSION_KEY`] back — this field is rendered *from* that
    /// count and never read back into a deletion or version decision, the same rule F21 argues for: ask a day later
    /// and the record has to say what the withheld version meant.
    pub ambiguous: usize,
    /// Up to eight display spellings, most actionable first: [`Self::unkeyable`] paths, then
    /// [`Self::duplicates`] ones, then [`Self::ambiguous`] notes, then [`Self::unreadable`] directories. A refused
    /// name and a duplicated path have no other channel, and an ambiguous note is named here because its only other
    /// signal is a version this workspace cannot record; an unreadable directory is also named in
    /// [`ScanReport::skipped`], so it gives up cap room first. One list, one cap — a second capped field would invite
    /// an unbounded value.
    ///
    /// Names are display text, so **a name here identifies no path**: two byte-distinct paths can share one
    /// lossy spelling (F20 §1) and then two entries look alike, exactly as two `skipped` lines do. It says
    /// where to look, not which twin.
    pub names: Vec<String>,
}

impl Unindexable {
    /// The stored spelling under [`UNINDEXABLE_KEY`] — the string `meta` holds and `np scan` prints beside
    /// the label, so a person comparing the line they just read with the record a later run finds is
    /// comparing one format. Public for the reason [`crate::db::orphan_total`] is public: the CLI renders
    /// this record and must not invent a second spelling of it.
    pub fn value(&self) -> String {
        format!(
            "unreadable={};unkeyable={};duplicates={};ambiguous={};names={}",
            self.unreadable,
            self.unkeyable,
            self.duplicates,
            self.ambiguous,
            self.names.join(","),
        )
    }

    /// Whether this scan lost nothing at all, which is the case [`ScanReport::unindexable`] reports as
    /// `None` while `meta` still records it as zeros — "looked and found none" staying distinguishable from
    /// "this workspace was never scanned by a build that looks" (F22 §2).
    ///
    /// [`Self::ambiguous`] is deliberately absent. Marking a row needs an unreadable look-alike prefix
    /// (`under_unreadable` reads only that list), so `ambiguous > 0` implies `unreadable >= 1`, which already makes
    /// this false: the field can never be the sole reason a record exists, and adding it here would suggest it could.
    fn is_empty(&self) -> bool {
        self.unreadable == 0 && self.unkeyable == 0 && self.duplicates == 0
    }
}

/// The record one scan loses, from the two lists [`crate::walk`] reported and the registry's duplicated
/// display paths.
///
/// Takes the pieces rather than the database or the walk because all three are already in hand once the
/// write pass ends (F22 §3), and because a hand-built list is the only way to exercise the refusal arm on a
/// platform that refuses no name — see `scan::tests` for the case that does.
fn unindexable_record(
    unreadable: &[UnknownPath],
    unkeyable: &[UnknownPath],
    duplicates: &[String],
    ambiguous: &[String],
) -> Unindexable {
    // Ordered by kind rather than merged, and capped in that order: refused names and duplicated paths have no
    // other channel at all, an ambiguous note's only other signal is a withheld parse version, while an unreadable
    // directory is also named in `ScanReport::skipped`, so it gives up cap room first.
    let names = unkeyable
        .iter()
        .map(|path| path.display.clone())
        .chain(duplicates.iter().cloned())
        .chain(ambiguous.iter().cloned())
        .chain(unreadable.iter().map(|path| path.display.clone()))
        .take(NAME_CAP)
        .collect();
    Unindexable {
        unreadable: unreadable.len(),
        unkeyable: unkeyable.len(),
        duplicates: duplicates.len(),
        ambiguous: ambiguous.len(),
        names,
    }
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
    let mut chunks = ChunkTally::default();
    scan_inner(root, cfg, |_| {}, &mut chunks)
}

/// What one scan's chunk pass did, counted per live note.
///
/// It reaches no report and no serialised output: [`ScanReport`] stays the classification truth (and its
/// JSON shape is pinned by the CLI tests). The tally exists so `tests/scan.rs` can assert §5's warm-pass
/// property — a note whose rows already carry its digest costs **one indexed lookup and no cutting** — as
/// counted work rather than as a duration, which is the only honest way to state it on a shared machine.
/// A test that cannot see the work can only measure it, and a measurement of 2,000 small notes proves
/// nothing about 18k ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChunkTally {
    /// Live notes whose stored chunk rows were consulted — §5's one indexed lookup each. A warm scan
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
struct WritePass<'a> {
    markers: &'a MarkerConfig,
    /// Accumulated through the write pass so [`scan_counting_chunks`] can hand it to a test; the tally is not a
    /// field of [`Written`] because `Written` describes rows, and this counts work (§5's warm-pass property).
    chunks: &'a mut ChunkTally,
}

/// [`scan`] plus the chunk pass's tally, for `tests/scan.rs`.
///
/// Public and `doc(hidden)` for the same reason as [`scan_with_inject`]: nothing outside this crate should
/// build on it, and the property it exists to prove (nothing cut, nothing written, version still recorded)
/// is invisible from the report — the rows a warm scan leaves behind are byte-identical either way.
#[doc(hidden)]
pub fn scan_counting_chunks(
    root: &Path,
    cfg: &Config,
) -> Result<(ScanReport, ChunkTally), ScanError> {
    let mut chunks = ChunkTally::default();
    let report = scan_inner(root, cfg, |_| {}, &mut chunks)?;
    Ok((report, chunks))
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
    let mut chunks = ChunkTally::default();
    scan_inner(root, cfg, after_walk, &mut chunks)
}

/// The pipeline behind both public entry points, with the chunk tally it accumulates into.
fn scan_inner(
    root: &Path,
    cfg: &Config,
    after_walk: impl FnOnce(&Path),
    chunks: &mut ChunkTally,
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
    // Carried as [`UnknownPath`] pairs to the end of the scan, where they are deduped and ordered by bytes and
    // only then printed: a name that came off the disk keeps its real bytes all the way to the report (F20).
    let mut skipped: Vec<UnknownPath> = clear_temp_corpses(&root)?
        .into_iter()
        .map(UnknownPath::from_display)
        .collect();
    let walked = walk::walk(&root, &WalkOptions::default())?;
    after_walk(&root);

    // A directory whose contents the walk did not read holds files that are unknown rather than absent — it
    // could not be entered, or its **own name** was refused (F18), which hides the same subtree. Their known
    // rows stay out of the comparison (see [`known_rows`]) and the directory itself is what the report names.
    // A refused *file* name is the same rule about one path instead of a region: it is not indexed this scan,
    // its row keeps what it holds, and the path is named here too.
    skipped.extend(walked.unreadable.iter().cloned());
    skipped.extend(walked.unrepresentable.iter().cloned());
    let found = read_found(&walked.files, &mut skipped);
    let snapshots: Vec<Snapshot> = found.iter().map(|found| found.snapshot.clone()).collect();

    let db = Db::open(&cfg.workspace_db(&root))?;
    // Decided at the open, before anything else can decide it: only the process that ran the cleanup can
    // announce it, and a cleanup that removed nothing has no loss to announce — its receipt stays in
    // `meta` as the record that the check ran (see [`ScanReport::upgrade_orphans`]).
    let upgrade_orphans = db
        .upgrade_orphans()
        .filter(|receipt| db::orphan_total(receipt) > 0)
        .map(str::to_owned);
    // Decided the same way one line later: only the open that ran migration 5 can announce the merge, and a
    // receipt of `merged=0` proves the check ran without reporting any work.
    let merged_notes = db
        .merged_notes()
        .filter(|receipt| db::merge_total(receipt) > 0)
        .map(str::to_owned);
    // Which rules wrote the rows on disk. Read before anything is written, so this one scan both does the
    // re-parse and records what it re-parsed with: `classify` compares bytes and knows nothing about the
    // rules that turn those bytes into headings, tags, items and links, so without this a note nobody has
    // edited since an older build ran would keep its old derived rows forever (F15).
    let stored = parse_version(&db)?;
    let refresh = refresh_needed(stored);
    // The same question one definition over: are this workspace's `chunks` rows labelled with the chunker
    // this build cuts with. Read before the write pass for the same reason — this scan both does the work
    // and records what it did it under.
    let stored_chunk_version = chunker_version(&db)?;
    let chunk_refresh = chunker_refresh_needed(stored_chunk_version);
    let now = now_secs();
    let known = known_rows(&db, &walked.unreadable, &walked.unrepresentable, &snapshots)?;
    let changes =
        classify_with_identities(&known.rows, &snapshots, !cfg!(windows), &known.identity);
    // An ambiguous row is marked rather than deleted (F23 §2), which puts two decisions between
    // classification and the write pass: a `Removed` naming an admitted marked key is dropped (§4 — this is
    // the arm that used to live inside `known_rows` as a deletion, and F20's safety property says it must
    // never stale a note nobody re-read), and what is left unresolved becomes the second source of the
    // charge §3 sums.
    let changes = strip_marked_removals(changes, &known.marked);
    let ambiguous = unresolved_ambiguity(&known.marked, &changes, &snapshots);
    // The two sources are **added**, never one replacing the other: rows hidden by the byte-keyed arm
    // resolve nothing, and an unresolved marked row is a note this refresh did not reach either.
    let hidden = known.hidden + ambiguous.len();

    let tx = db.0.unchecked_transaction().map_err(DbError::from)?;
    let written = apply_changes(
        &db,
        &found,
        &changes,
        refresh,
        now,
        !cfg!(windows),
        // Markers and the chunk tally travel as one bundle; see [`WritePass`].
        &mut WritePass {
            markers: &cfg.markers,
            chunks,
        },
    )?;
    // Decided here, before anything else writes, from what the write pass actually delivered. One coverage
    // answer serves both versions, because §5 reuses `refresh_covered`'s two sources **unchanged**: a note
    // that escaped this scan escaped chunking too, and inventing a second coverage rule for chunks would be
    // a second thing to keep honest for no extra fact.
    let covered = refresh_covered(&written, hidden);
    let record_version = refresh && covered;
    // **Provider availability is not version debt** (design-m2-embeddings §5's P0). Nothing above or below
    // this line asks whether an embedder exists, and that is the whole decision: if recording waited for a
    // provider, then with `embeddings.provider = "none"` — the default (§2), what every install runs until
    // a human picks a model — the key would never be recorded, the coverage record would read as a gap
    // nobody can clear, and any later reader that treats a missing key as "the corpus was never cut" would
    // re-cut ~18k notes on every scan forever. Re-chunking fails for no external reason, so it is coverage
    // debt; whether a vector exists is a per-row fact (`chunks.source_hash`), reported as a bounded count,
    // never inferred from an absent meta key.
    let record_chunk_version = chunk_refresh && covered;
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
    // The freshness stamp rides in the same transaction as the catalog it describes (**the same reason
    // [`set_index_list`] is here rather than after the commit**): a scan that did not commit must not leave behind a
    // timestamp claiming it did. A reader asks "how old is this index?" and the only honest answer is the moment the rows
    // it describes became durable.
    set_meta(&db, LAST_SCAN_KEY, &unix_now().to_string())?;
    // Read here, inside the transaction and after every write of this scan, so the duplicate count answers
    // what this scan leaves behind rather than what it found (F23's duplicate is *created* by a write pass).
    let duplicates = duplicated_rels(&db)?;
    if record_version {
        // Recorded in the same transaction as the rows it describes: a scan that dies halfway has to
        // refresh again rather than leave half the corpus labelled as current. An up-to-date database
        // writes nothing here, so the steady-state scan pays one `SELECT` and no extra `INSERT`.
        set_meta(&db, PARSE_VERSION_KEY, &PARSE_VERSION.to_string())?;
    }
    if record_chunk_version {
        // Same transaction, same reason: a scan that died halfway must not leave the corpus labelled as
        // cut by this build. A workspace already carrying the number writes nothing here.
        set_meta(&db, EMBED_VERSION_KEY, &CHUNKER_VERSION.to_string())?;
    }
    tx.commit().map_err(DbError::from)?;
    // After the commit, so nothing is announced about a scan that rolled itself back — the same rule
    // [`crate::db::report_orphans`] follows. The report's own counts stay the classification truth; this
    // is the only place a person learns that a scan with nothing to report did real work.
    report_refresh(&mut io::stderr(), stored, written.refreshed);
    if refresh && !record_version {
        // Without this line a workspace with one unreadable directory would re-parse its whole corpus on
        // every scan in silence, which looks exactly like the version never being written by accident.
        report_refresh_gap(&mut io::stderr(), written.uncovered + hidden);
    }
    if chunk_refresh && !record_chunk_version {
        // The same silence is worse here than it is for parse rules. `report_refresh` above prints nothing when
        // this scan re-parsed nothing, which is exactly the shape of a chunker-only bump: the version is withheld
        // by an F18 refused name or an unmerged duplicate pair, and no line is written about it on any scan, so a
        // coverage gap nobody is told about is indistinguishable from a corpus that was never cut. §5's case for
        // recording anyway rests on that gap being bad; this line is what makes it visible.
        report_chunk_gap(&mut io::stderr(), written.uncovered + hidden);
    }

    // The durable record of what this scan could not index, written **after** the commit it describes — the
    // position F21's receipt echo established, and for the same reason: a scan that rolled itself back has
    // nothing to announce. Written on every scan, including one that lost nothing, because the record is
    // current state and an old value left standing is a false accusation (F22 §2).
    let losses = unindexable_record(
        &walked.unreadable,
        &walked.unrepresentable,
        &duplicates,
        &ambiguous,
    );
    let record = losses.value();
    record_unindexable(&db, &record);

    // A root entry that wears one of our names but is not ours stays on disk and reaches the report, so
    // the user hears about the collision instead of losing the file.
    skipped.extend(
        write_index_files(&root, &rendered, &previous)?
            .into_iter()
            .map(UnknownPath::from_display),
    );
    WorkspaceRegistry::open(&cfg.data_dir)?.record(&root, now)?;

    skipped.extend(written.skipped.into_iter().map(UnknownPath::from_display));
    let skipped = skipped_lines(skipped);
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
        upgrade_orphans,
        merged_notes,
        // Read from `meta` rather than from the handle, so a receipt written by an open in September reaches a scan in
        // December. A read of one indexed row each; a failure here is a database failure like any other in this path.
        recorded_upgrade_orphans: db.recorded_upgrade_orphans()?,
        recorded_merged_notes: db.recorded_merged_notes()?,
        // `None` when nothing was lost, so the common report stays clean while its JSON still carries the
        // key as `null` — one shape for every scan, which is what [`ScanReport::upgrade_orphans`] argues.
        unindexable: (!losses.is_empty()).then_some(losses),
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
fn owes_a_reparse(db: &Db, path: &RelPath, is_markdown: bool) -> Result<bool, DbError> {
    Ok(is_markdown && !matches!(note_row(db, path)?, NoteRow::Missing))
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
fn read_found(files: &[DiscoveredFile], skipped: &mut Vec<UnknownPath>) -> Vec<Found> {
    let found: Vec<Found> = files.iter().map(read_one).collect();
    skipped.extend(
        found
            .iter()
            .filter(|found| is_unknown(found))
            // Both spellings of the path, from the walk's own component walk: this file was listed, so its
            // bytes are as exact as a refused directory's, and deduping on them keeps two unreadable twins
            // from publishing one line (F20 §1).
            .map(|found| UnknownPath {
                display: found.snapshot.rel.clone(),
                bytes: found.snapshot.rel_bytes.clone(),
            }),
    );
    found
}

/// [`ScanReport::skipped`] from everything this scan collected: deduplicated by **bytes**, ordered by bytes,
/// then printed as display text.
///
/// Keying on bytes is what stops two refused (or unreadable, or unread-and-vanished) paths that share one lossy
/// display spelling from collapsing into a single line — the scan saw two paths and has to say so twice, however
/// alike the two lines look (F20 §1). Sorting by the same key makes the order total where display text ties.
/// A name that only ever existed as text arrives through [`UnknownPath::from_display`], whose bytes *are* that
/// text's UTF-8 bytes, so such entries dedup and order exactly as the strings used to.
fn skipped_lines(mut paths: Vec<UnknownPath>) -> Vec<String> {
    paths.sort_by(|a, b| a.bytes.cmp(&b.bytes));
    paths.dedup_by(|a, b| a.bytes == b.bytes);
    paths.into_iter().map(|path| path.display).collect()
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

/// The registry rows `classify` compares against: live rows, minus anything whose path this scan could not
/// look at — a subtree under an unreadable directory, or one refused name (F18). The filter is
/// [`is_unknown_path`], and it judges a row by its `rel_bytes`: a row whose *display* text happens to match an
/// unknown path while its bytes do not belongs to a subtree this scan did read, and hiding it is F20. The one
/// exception is a row whose stored key was itself derived from display text ([`key_is_display_derived`]): such a
/// row has no byte spelling left to be judged by, so it is compared text against text (F20 §4.2, "unknown is not
/// absent") — but since F23 that comparison **marks** the row instead of deleting it. An ambiguous row stays in
/// `known` when a listed readable path spells it exactly and matches its stored hash ([`admits`], §2's scoping),
/// and whether it resolves is decided afterwards from what `classify` did, not here.
///
/// Stale rows stay out because their absence was already reported; leaving them in would count one
/// deleted file as `removed` on every later scan. Dropping them is also what lets such a file come back
/// as `Added` onto the row it still has, which is how its note rejoins with its links attached —
/// [`crate::db`] keeps ids stable for exactly that.
///
/// The unreadable directories are the other half of the same caution: files below one were never seen,
/// so their rows must reach neither a `Removed` nor a move (Task 4 handoff), and leaving them in would
/// stale a whole subtree over one transient permission error. A refused name is the third piece of the same
/// rule ([`is_unknown_path`], F18): the file is there, but its identity could not be spelled, so its row
/// keeps what it has instead of being marked stale for want of a key. A refused **directory** needs no fourth
/// rule: [`crate::walk`] records it in `unreadable`, because an unspellable name makes every path below it
/// unkeyable too — the same hole an unreadable one cuts, by the same prefix.
///
/// The note test correlates on `rel_bytes`: with twins a note belonging to either file would otherwise
/// answer for both, and that boolean feeds the refresh-coverage decision, which decides whether
/// [`PARSE_VERSION_KEY`] gets recorded (F13 §3).
///
/// The count alongside the rows is how many live **note** rows that byte-keyed filter dropped — nothing else
/// reports them, since they appear nowhere in `found` — which is how [`scan_with_inject`] knows a
/// parse-rule refresh never reached part of the corpus. Any row with no note behind it is excluded by the
/// `EXISTS` — attachments and never-yet-parsed paths alike: no parse rule can stale rows that do not exist,
/// so counting one would pin the version forever over work that is not owed. F23's marked rows charge
/// **separately** ([`unresolved_ambiguity`], §3's source 2), and a row never charges both ([`known_rows`]'s
/// precedence: the byte arm answers first and marks nothing).
struct KnownRows {
    /// The rows `classify` may judge.
    rows: Vec<Snapshot>,
    /// Source 1 (§3): live note rows the **byte-keyed** arm hid, unchanged by F23.
    hidden: usize,
    /// Every ambiguous row this pass marked, admitted or refused, in byte-key ascending order — the
    /// population [`unresolved_ambiguity`] reads and §6's record field renders.
    marked: Vec<Marked>,
    /// The one identity tally computed from these rows and the listed paths, shared with
    /// [`crate::change::classify_with_identities`] so admission and pairing cannot disagree (design-a4 §3).
    identity: FileIdentities,
}

/// One row F23 **marked**: its key is display text ([`key_is_display_derived`]) and an unreadable path spells
/// that text, so `known_rows` cannot tell whether the file it describes is the readable look-alike or a file
/// behind the obstruction. It may never be removed, and it may be claimed — by content or by its own listed
/// path (§2).
struct Marked {
    /// The row's stored key: what a claim names (`Change::Moved { from }`) and what [`strip_marked_removals`]
    /// matches a `Removed` against.
    key: Vec<u8>,
    /// Display spelling, for the capped name list in F22's record (§6).
    rel: String,
    /// Whether [`admits`] let it reach `known`. A refused row is pre-dropped — today's behaviour — and so
    /// resolves nothing this pass.
    admitted: bool,
    /// Whether a note row hangs off it, for the reason [`known_rows`] gives: only a note can hold
    /// [`PARSE_VERSION_KEY`] back.
    is_note: bool,
}

fn known_rows(
    db: &Db,
    unreadable: &[UnknownPath],
    unrepresentable: &[UnknownPath],
    listed: &[Snapshot],
) -> Result<KnownRows, DbError> {
    // Both key columns come back, because this is the database side of a byte-keyed comparison: hand out
    // display text here and every non-UTF-8 row reads as removed-and-re-added on every scan forever, which is
    // §1's never-converges symptom manufactured by the fix (F13 §4.1).
    let mut statement = db.0.prepare(
        "SELECT f.rel_bytes, f.case_bytes, f.rel, f.case_key, f.hash, f.size, f.mtime,
                EXISTS (SELECT 1 FROM notes n WHERE n.rel_bytes = f.rel_bytes),
                f.file_id
         FROM files f WHERE f.state = 'ok' ORDER BY f.rel_bytes",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            Snapshot {
                rel_bytes: row.get(0)?,
                case_bytes: row.get(1)?,
                rel: row.get(2)?,
                case_key: row.get(3)?,
                hash: row.get(4)?,
                size: row.get::<_, i64>(5)?.max(0) as u64,
                mtime: row.get(6)?,
                // Nullable by design: a row written before migration 6, or any row on a platform that reports
                // nothing, reads as unknown. The routes that use it treat unknown as no opinion (design-a4 §3).
                file_id: row.get(8)?,
            },
            row.get::<_, bool>(7)?,
        ))
    })?;
    // The fallback's other side, built once per scan rather than per row: the same unknown paths spelled as
    // text, for the rows that have no other spelling to be read by. Empty in the common case, so a workspace with
    // nothing hidden pays nothing. Only the **unreadable** list gets this: see [`key_is_display_derived`] for why
    // the refused-name arm needs no text side.
    let unreadable_display = display_spelled(unreadable);
    // Read out fully here, so the write pass never runs with a read open across it.
    let mut hidden = 0_usize;
    // Every row that survived the byte-keyed hiding, in the byte-key order the query returned them, plus the
    // index of each **marked** one. Two passes are the point rather than a flourish: identity eligibility is a
    // fact about the whole population — "exactly one row and exactly one listed path carry this id" cannot be
    // answered row by row — so it is computed once from everything loaded, then consulted by admission here and
    // by pairing in `classify`. One tally, two readers, which is what keeps them from disagreeing about who is
    // eligible (design-a4 §3): a row admitted without a partner would have its `Removed` stripped and gain no
    // `Moved`, the permanent-duplicate stuck state this function's own comment warns about.
    let mut loaded: Vec<Snapshot> = Vec::new();
    // `(index, is_note)` for each marked row, in the query's byte-key order — which is the order `marked` must
    // come out in, because §5.7's determinism rule reads that vector when it renders the record.
    let mut candidates: Vec<(usize, bool)> = Vec::new();
    let mut is_marked: Vec<bool> = Vec::new();
    for row in rows {
        let (snapshot, is_note) = row?;
        // Bytes first: that is the identity, and for any row the walk keyed it this is the whole answer. First
        // for a second reason too — §3's **precedence**. The permanent class satisfies both arms at once (a row
        // whose key equals its OS bytes, sitting under a genuinely unreadable prefix), so marking before hiding
        // would charge it in source 1 *and* source 2; on Windows, where refusal bytes are already lossy, that
        // double charge would be universal. Today's single `||` could not double count, and neither may this.
        if is_unknown_path(&snapshot.rel_bytes, unreadable, unrepresentable) {
            if is_note {
                hidden += 1;
            }
            continue;
        }
        let ambiguous = key_is_display_derived(&snapshot.rel, &snapshot.rel_bytes)
            && under_unreadable(snapshot.rel.as_bytes(), &unreadable_display);
        if ambiguous {
            candidates.push((loaded.len(), is_note));
        }
        is_marked.push(ambiguous);
        loaded.push(snapshot);
    }

    // The scope lives here, at the one call site, rather than as a wider public API: every marked row is
    // display-derived by construction, so this is exactly F23's population and an ordinary rename's stored row is
    // never offered to identity pairing at all (design-a4 §3).
    let identity = FileIdentities::build(&loaded, listed, |row| {
        key_is_display_derived(&row.rel, &row.rel_bytes)
    });

    // Admission is decided before anything is dropped (§4): what no route can account for never enters `known`
    // — today's behaviour for the refused set, unchanged — and charges source 2.
    let mut admitted_row = vec![false; loaded.len()];
    let mut marked = Vec::with_capacity(candidates.len());
    for &(index, is_note) in &candidates {
        let snapshot = &loaded[index];
        // Content first, identity second, and the order is F23 §3's precedence carried forward rather than
        // taste: the byte arm of `key_is_display_derived` already decides what a row is, and a route that could
        // disagree with it would make which fact wins depend on which question got asked first.
        let admitted = admits(db, snapshot, listed)?
            || match identity.listed_for(snapshot) {
                // The key this pairing will **write** is the listed path's, not the row's — so that is the key
                // the stale-row rule tests, excluding this row itself. Without the guard a marked row could be
                // admitted while another row already holds the listed path's key: `classify` would pair it with
                // nothing (that path matched its own row and is no leftover), its `Removed` would still be
                // stripped as admitted, and it would sit in the table forever charging `unresolved_ambiguity`.
                Some(listed_key) => !key_taken(db, listed_key, &snapshot.rel_bytes)?,
                None => false,
            };
        admitted_row[index] = admitted;
        marked.push(Marked {
            key: snapshot.rel_bytes.clone(),
            rel: snapshot.rel.clone(),
            admitted,
            is_note,
        });
    }

    // Unmarked rows enter `known` as they always did; a marked one enters only if some route accounts for it.
    let known = loaded
        .into_iter()
        .zip(is_marked.iter().copied().zip(admitted_row))
        .filter(|(_row, (marked, admitted))| !*marked || *admitted)
        .map(|(row, _)| row)
        .collect();
    Ok(KnownRows {
        rows: known,
        hidden,
        marked,
        identity,
    })
}

/// Whether an ambiguous row is **admitted** to `known` — §2's scoping clause, and the one assertion standing
/// between F23 and a note re-homed onto a stranger's file.
///
/// `classify` matches a claim on **hash alone** (`claimants.get(added.hash)` over every unmatched known row),
/// and its `present_hashes` guard cannot help here: that set is seeded from known rows whose *key* matches an
/// unreadable listing, and a display-derived key never equals a real-bytes listing key. Left unscoped, any
/// unrelated readable path with byte-identical content — a `# Title` stub, a blank template, a re-exported note
/// — would claim the hidden row and re-home its id. So the scoping happens before classification: some listed
/// path must spell the row exactly (same display text), carry its stored hash, and be readable (an empty hash is
/// [`crate::change`]'s unknown marker).
///
/// The third test is §4's stale-row rule: a claim onto a key another row already holds hits the `UNIQUE` in
/// [`crate::db::apply_move`], comes back `Indexed::Taken`, returns early, and leaves the path in `skipped` while
/// `owes_a_reparse` charges `written.uncovered` on every refresh — a stuck state nothing clears. Rows of **any**
/// state count, because [`crate::db::upsert_file`] resolves by `rel_bytes` with no state filter and that is how a
/// returning file rejoins its note.
///
/// Testing "some candidate" rather than "the one `classify` will pick" is sound: a claim needs exactly one added
/// path carrying the hash (`added_count == Some(&1)`), so wherever two twins both qualify there is no claim to
/// refuse. With display scoping plus `files.rel_bytes UNIQUE` there is at most **one claimant per listed path**,
/// which is why no multiplicity rule, tally or tie-break exists to write (§2's corollary).
fn admits(db: &Db, row: &Snapshot, listed: &[Snapshot]) -> Result<bool, DbError> {
    for path in listed {
        if path.rel != row.rel || path.hash.is_empty() || path.hash != row.hash {
            continue;
        }
        if key_taken(db, &path.rel_bytes, &row.rel_bytes)? {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

/// Whether some row of **any** state — live or stale — besides the one keyed `except` already holds `key`.
fn key_taken(db: &Db, key: &[u8], except: &[u8]) -> Result<bool, DbError> {
    let taken =
        db.0.prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM files WHERE rel_bytes = ?1 AND rel_bytes <> ?2)",
        )?
        .query_row(params![key, except], |row| row.get::<_, bool>(0))?;
    Ok(taken)
}

/// §4's post-strip: drop every `Removed` naming an admitted marked key, after classification and before
/// anything consumes the list — `changes` has exactly one consumer ([`apply_changes`]), read twice inside it.
///
/// **`Change::Removed` only.** The identity-matched route legitimately emits `Modified` keyed on the listed
/// path, whose `rel_bytes` equal the row's stored key; a strip written as "drop any change carrying a marked
/// key" would delete that legitimate re-parse and leave stale derived rows behind charging nothing. Traced
/// against [`classify`]: in M and in N′-changed the row is unmatched by identity, its hash matches no added path,
/// and the final loop pushes exactly one `Removed` whose bytes are the stored key — so `(Removed, key)` covers
/// both.
fn strip_marked_removals(
    changes: Vec<(RelPath, Change)>,
    marked: &[Marked],
) -> Vec<(RelPath, Change)> {
    changes
        .into_iter()
        .filter(|(path, change)| {
            !(matches!(change, Change::Removed)
                && marked
                    .iter()
                    .any(|row| row.admitted && row.key == path.rel_bytes))
        })
        .collect()
}

/// §3's source 2: the marked rows this pass **did not resolve**, as their display spellings in byte-key order.
///
/// Resolution is §2's two routes, read from what classification actually delivered rather than tallied in a
/// pre-pass — which is what lets healed N′ stop paying once it has healed (a charge that never clears is F19's
/// bug wearing this milestone's clothes):
///
/// - **claimed** — it produced `Moved { from } = its own key`, so [`crate::db::apply_move`] re-keys `rel_bytes`
///   from real bytes; or
/// - **matched** — its own path was listed with a non-empty hash. Matched on the **key**, because that is what
///   `classify` pairs identity on: a listing that merely *spells* the row the same is M's look-alike, whose rows
///   are still unpaired. Without this route the permanent class — a name genuinely holding `U+FFFD`, and every
///   lossy Windows name — would charge every scan forever.
///
/// Then §3's exclusion: a marked row whose own key was listed **unreadable** is charged by `written.uncovered`
/// instead, which resolves by byte key. Deriving the count from "did we strip a `Removed`" instead would charge
/// zero times for a marked row `classify` filtered out before emitting anything — an uncharged stale row is
/// exactly the F15 class this project keeps paying to avoid.
fn unresolved_ambiguity(
    marked: &[Marked],
    changes: &[(RelPath, Change)],
    listed: &[Snapshot],
) -> Vec<String> {
    let claimed: HashSet<&[u8]> = changes
        .iter()
        .filter_map(|(_, change)| match change {
            Change::Moved { from } => Some(from.rel_bytes.as_slice()),
            _ => None,
        })
        .collect();
    let mut matched: HashSet<&[u8]> = HashSet::new();
    let mut unreadable: HashSet<&[u8]> = HashSet::new();
    for path in listed {
        if path.hash.is_empty() {
            unreadable.insert(path.rel_bytes.as_slice());
        } else {
            matched.insert(path.rel_bytes.as_slice());
        }
    }
    marked
        .iter()
        // Only a row with a note behind it can hold `PARSE_VERSION` back ([`known_rows`]'s reason).
        .filter(|row| row.is_note)
        .filter(|row| !unreadable.contains(row.key.as_slice()))
        .filter(|row| {
            !claimed.contains(row.key.as_slice()) && !matched.contains(row.key.as_slice())
        })
        .map(|row| row.rel.clone())
        .collect()
}

/// Whether `rel_bytes` sits inside a directory whose contents this walk never saw, which makes its absence from
/// the found list meaningless — either the walk could not enter it, or the directory's own name was refused
/// below ([`crate::walk::Walked::unreadable`] carries both shapes, F18). `"."` is how [`crate::walk`] spells the
/// workspace root itself, and that leaves nothing knowable at all.
///
/// Both sides of the comparison are **bytes**, and an entry only matches at a `/` boundary: `caf\xE9` hides
/// `caf\xE9/a.md` and nothing else — not its twin `caf\xE8/a.md`, whose display spelling is identical (F20 §1),
/// and not `caf\xE9x/a.md`, which is a different subtree.
///
/// An entry holding **no** bytes matches nothing. The walk spells the root `"."` for exactly that reason (see
/// [`crate::walk`]'s `unknown_path`), and it matters in both directions: an empty prefix would match every path
/// in the workspace, which is the one way this filter could hide the whole corpus rather than one subtree.
fn under_unreadable(rel_bytes: &[u8], unreadable: &[UnknownPath]) -> bool {
    unreadable.iter().any(|dir| {
        dir.bytes == b"."
            || (rel_bytes.len() > dir.bytes.len()
                && rel_bytes[..dir.bytes.len()] == dir.bytes[..]
                && rel_bytes[dir.bytes.len()] == b'/')
    })
}

/// Whether `rel_bytes` names a path this scan must not judge absent, from the two lists [`crate::walk`] reports.
///
/// They describe different shapes of "unknown" and so match differently: an unreadable **directory** hides a
/// whole subtree, hence [`under_unreadable`]'s prefix rule, while a refused name
/// ([`crate::walk::Walked::unrepresentable`], F18) is one file rather than a region, hence exact equality.
/// A refused **directory** belongs to the first list rather than the second
/// ([`crate::walk::Walked::unreadable`]), and that is not a convenience: its name is the first component of
/// every path below it, so none of those can be keyed either and the whole subtree needs the prefix rule.
/// Every arm here suppresses `Removed` for the same reason — the scan did not see that path, which is not
/// evidence it stopped existing.
///
/// Both arms compare **bytes** (F20), which is the whole point of [`UnknownPath`]. Matching display text instead
/// let one unreadable or refused directory hide its readable twin: `caf\xE9/` and `caf\xE8/` spell one
/// `caf\u{FFFD}/`, so rows belonging to a subtree this scan had read in full dropped out of `known` — a real
/// deletion inside the readable twin went unreported, a rename there produced a second live row instead of a
/// move, and one note could be charged in both `written.uncovered` and `hidden_by_unknown`.
///
/// Windows loses nothing by the switch and gains nothing from it: its key bytes *are* the lossy text (F18), so
/// byte equality there decides exactly what text equality decided. On Unix no *refused* name can reach either
/// list at all (`path_keys::name_is_lossless` is `true` for every byte a filename holds), while the prefix arm
/// still fires on any genuinely unreadable directory — and that is where the twin case above is reachable today.
fn is_unknown_path(
    rel_bytes: &[u8],
    unreadable: &[UnknownPath],
    unrepresentable: &[UnknownPath],
) -> bool {
    unrepresentable.iter().any(|path| path.bytes == rel_bytes)
        || under_unreadable(rel_bytes, unreadable)
}

/// Whether a row's stored key is the bytes of its own **display text** instead of the bytes of the path on
/// disk — the shape [`crate::db::copy_files_rows`] leaves behind, since migration 4 keyed every row through
/// [`crate::path_keys::rel_bytes_of_display`]. A note really named `caf\xE9/note.md` therefore sits under
/// `63 61 66 EF BF BD 2F 6E 6F 74 65 2E 6D 64`: the `U+FFFD` spelling of a name whose bytes no `String` holds
/// (`docs/design-f13-byte-paths.md` §4.2, and `crates/np-core/tests/db.rs` proves such rows ship).
///
/// Both halves of the test keep this to rows whose key **is** their own display text, which is what F20's byte
/// matching cannot judge: a key differing from its row's display bytes came off the disk, and a `rel` with no
/// `U+FFFD` in it has one spelling — either way [`is_unknown_path`] already decided the row exactly as it did
/// before this fallback existed.
///
/// Do not read that as a class only old databases hold. A name that genuinely contains `U+FFFD` (the bytes
/// `EF BF BD`, valid UTF-8, creatable on Linux today) satisfies both halves forever, because for it the key really
/// is the OS bytes; and on Windows every lossily-named row qualifies, since [`crate::path_keys::os_bytes`] has no
/// byte spelling for a UTF-16 name. In those cases the text arm is just the byte arm again — same bytes, same
/// answer — which is why widening here costs nothing they were not already paying.
///
/// A migrated row leaves the class when a scan sees its path **and** pairs with it — by identity, or since F23 by
/// content through [`known_rows`]' scoping, which routes the pair through [`crate::db::apply_move`] so `rel_bytes`
/// becomes the walk's bytes (F13 §4.2's one-time churn). Nothing else does: an unchanged row is refreshed in place and
/// keeps the key it has, so this fallback is not transitional for the cases above — say "until someone renames it", not
/// "until the next scan". What F23 changed is not membership but consequence: a row still in the class is now marked
/// rather than deleted, which is what lets content settle identity at all.
///
/// The refused-name arm gets no text side, deliberately: a refusal entry's own bytes are lossy wherever refusals
/// happen (Windows keys them from `to_string_lossy`, so they already equal a qualifying row's key and the byte arm
/// matches), and on Unix nothing is ever refused ([`crate::path_keys::name_is_lossless`] accepts every byte a Unix
/// name holds). A fallback there would be an untestable branch, not a safety net.
fn key_is_display_derived(rel: &str, rel_bytes: &[u8]) -> bool {
    rel.contains('\u{FFFD}') && rel_bytes == crate::path_keys::rel_bytes_of_display(rel)
}

/// The two unknown lists re-spelled for [`key_is_display_derived`] rows: each entry keeps its
/// [`UnknownPath::display`] and takes that text as its bytes, exactly as [`UnknownPath::from_display`] does for
/// a path that only ever existed as text.
///
/// Matching these against such a row's `rel` is deliberately **wider** than the byte comparison, and only for
/// those rows: one `caf\u{FFFD}` entry then hides it whichever twin its bytes came from. The asymmetry is the whole
/// argument — a false `Removed` stales a live note, drops it from the catalog, under-charges the refresh gap so
/// [`PARSE_VERSION_KEY`] is recorded over rows nobody re-parsed, and (F13 §4.2) brings the note back as a **second
/// `notes.id`** when the directory opens again.
///
/// **Do not call that over-hiding harmless.** When the row's own path is readable while an unreadable twin shares its
/// display spelling, hiding the row left the listed path with no row to pair with, so it indexed as a spurious `Added`:
/// two live `files` and `notes` rows for one file, with the version withheld forever. **F23 closed that half here** —
/// not by narrowing this predicate, which cannot tell the cases apart, but by marking the row instead of deleting it so
/// [`crate::change::classify`] can pair them on content: see `docs/design-f23-ambiguous-rows.md` §2 and
/// `a_healed_display_keyed_pair_leaves_one_live_row_and_records_the_version`.
///
/// What still stands, and is why the width stays: no predicate computable at this point separates the two cases, because
/// a display spelling has many byte twins and a lossy key records none of them. A row whose **content changed** while it
/// was unindexable has nothing to pair on, so its duplicate pair returns; and the case this fallback exists for — the
/// row's own file inside the obstruction, a different file readable outside it — must keep hiding, which is what
/// `a_display_keyed_row_inside_an_unreadable_directory_survives_its_readable_twin` fences. Backlog **F23** remains open
/// for the residue and names durable file identity as the way through it.
fn display_spelled(paths: &[UnknownPath]) -> Vec<UnknownPath> {
    paths
        .iter()
        .map(|path| UnknownPath::from_display(path.display.clone()))
        .collect()
}

/// Write this scan's changes: every readable file's registry row refreshed, contents re-indexed where
/// they changed, gone paths marked stale.
///
/// Every readable file is upserted and not only the changed ones, because `classify` emits nothing for a
/// matched pair whose hash agrees — and without the refresh such a file would carry an old `mtime` in the
/// database and be re-hashed by every later scan forever (Task 4 handoff).
///
/// A path whose identity is already held by another row — two files named `a.md` and `A.md` on a
/// case-sensitive filesystem, where `files.case_bytes` stays unique and nothing picks a winner — is
/// reported under `skipped` and left out of the counts instead of aborting the scan (ruling P3). So is a path
/// holding more than one note row ([`NoteRow::Duplicate`]), which no constraint can see.
///
/// The pending map is keyed on bytes, like every other identity in the pass (F13 §4.1): keyed on display
/// `rel`, two twins read back **the same change** from one slot whatever the columns are called.
///
/// `chunks` accumulates what the chunk pass did ([`ChunkTally`]). It is a parameter rather than a field of
/// [`Written`] because the tally has to reach a caller through [`scan_counting_chunks`] while `Written` stays
/// private, and one `&mut` says that more honestly than a second return path.
fn apply_changes(
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
            // Nothing to write, but a refresh has to remember a missed note *whose rows already exist*:
            // those still hold whatever an older build wrote. An attachment, or a note that never got a
            // row, is charged never — there is nothing stale to repair, so charging it would pin the
            // version forever and re-parse the whole corpus on every scan for nothing.
            if owes_a_reparse(db, &path, readable.is_markdown)? {
                written.uncovered += 1;
            }
            continue;
        }
        // `Removed` is keyed on a path that is not in `found`, so it cannot reach this lookup.
        let change = pending.get(readable.snapshot.rel_bytes.as_slice()).copied();
        match index_file(db, readable, change, refresh, now, case_sensitive, pass)? {
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
                    _ => owes_a_reparse(db, &path, readable.is_markdown)?,
                };
                if owed {
                    written.uncovered += 1;
                }
            }
            // More than one note row sits on this path, so no note was re-parsed or rewritten (F13 §7; the
            // registry row may already have moved — see [`Indexed::Duplicate`]). Its rows exist and stay
            // whatever rules wrote them, which is exactly what a refresh has to remember.
            Indexed::Duplicate => {
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
                    None | Some(Change::Removed) => written.unchanged += 1,
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
    /// More than one note row sits on this path's key — a legacy duplicate `notes.rel` never forbade
    /// (F13 §7). Picking one would rewrite that note's title, tags, items and links with the other's
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
/// A note's chunks are refreshed on the same pass and inside the same transaction (§5: cheap, and atomic
/// with the note rows) — see [`refresh_chunks`] for the order that work is decided in.
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
                // the same note (Task 4 handoff).
                if let Some(old) = folded_row(db, &snapshot.case_bytes, &snapshot.rel_bytes)? {
                    if let Err(err) = db::apply_move(db, &old, &path, now) {
                        return conflict_or_err(err, Indexed::Taken);
                    }
                }
            }
        }
    }

    // Whether this write replaced a different digest. `index_file` needs it below: a change kind cannot express
    // "same file, new path, new bytes", and that is exactly the case identity resolves (design-a4 §4a).
    let hash_changed = match db::upsert_file(db, snapshot, &readable.abs, now) {
        Ok(changed) => changed,
        Err(err) => return conflict_or_err(err, Indexed::Taken),
    };

    // A move alone leaves the derived rows untouched, because they hang off `notes.id` and not off the
    // path. Missing rows are rebuilt whatever `classify` said: a file that changed extension into `.md`
    // while keeping its bytes is a `Moved`, and would otherwise stay unparsed forever.
    if let Some(text) = &readable.text {
        // `refresh` rewrites a note whose bytes did not change: the whole point of F15 is that its
        // derived rows were written by rules this build no longer has.
        // `hash_changed` is the content-based half of this test, and it is load-bearing rather than tidier.
        // A `Moved` was never in the list above because every move today implies equal bytes by construction —
        // but an identity-resolved move pairs a row with a file whose bytes **differ**. Without this term that
        // row would be re-keyed, handed fresh size/mtime/hash, keep its stale body and derived rows, be excluded
        // from the ambiguity charge, and let `PARSE_VERSION` be recorded as though it had been read; the next
        // scan short-circuits on size and mtime before comparing hashes, so nothing would ever notice. Keyed on
        // content rather than on a variant on purpose: any future route that pairs differing bytes lands in the
        // same hole (design-a4 §4a).
        let rewritten =
            refresh || hash_changed || matches!(change, Some(Change::Added | Change::Modified));
        // One id lookup per note, resolved here — after any move above has been applied, which is
        // what lets [`db::apply_move`] have rewritten `notes.rel` onto the new path (migration 4 removed the
        // foreign key that used to do it). The same answer
        // decides whether a note without a row is rebuilt (`is_none()` below) and whether
        // [`store_note`] updates or inserts; looking it up again inside `store_note` was F10.
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
///
/// Returns `false` when the path carries **more than one** note row and nothing was deleted: choosing which
/// of two notes to destroy is the same guess F13 §7 refuses, so the caller reports the file instead.
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
/// ASCII byte fold and returning both spellings of the row it finds (F13 §3): once this matched the Unicode
/// display fold it handed back a *twin* as "another spelling of the same name", and [`index_file`] then moved
/// a row between two files that both exist. The accepted price is in [`crate::path_keys::case_fold`]'s doc:
/// a rename that changes only a non-ASCII letter's case stops being followed here.
fn folded_row(db: &Db, case_bytes: &[u8], rel_bytes: &[u8]) -> Result<Option<RelPath>, DbError> {
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
/// Returns the `notes.id` the row was written under — the id a re-parse keeps — because the chunk pass that
/// follows hangs its rows off it and must not look it up a second time (F10's rule about a duplicated read).
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
/// **The order is the whole point** (design-m2-embeddings §5): [`db::chunk_digest`] is consulted **first**, and
/// only a note whose rows carry no single digest — none at all, a digest other than `note_digest`, or a row
/// behind [`CHUNKER_VERSION`] — goes on to be cut. The stamp read deliberately excludes `chunks.text`: reading
/// the texts here would make a warm scan pull roughly the whole corpus out of SQLite and discard it, which is
/// not §5's "one indexed lookup" and is invisible to a fixture whose notes are one chunk each.
///
/// Nothing is re-read either: `text` is the string [`read_one`]
/// decoded once for the parser (leading BOM stripped, bytes UTF-8 cannot hold replaced), which is exactly the
/// domain §4 defines the offsets over. Cutting first and asking afterwards would slice every unchanged note in
/// the vault on every warm scan to arrive at rows that are already there — the failure mode §5 exists to
/// prevent — which is why [`ChunkTally::cut`] is asserted rather than timed.
///
/// `note_digest` is `files.hash`: the digest of the bytes on disk, BOM included ([`read_one`]). The offsets
/// stay over the decoded text (§4), and that is not a contradiction: decoding is a pure function of those
/// bytes, so one file digest still states exactly which decoded text these rows were cut from — and it is the
/// only digest this call can compare without opening the file again.
///
/// **A note with nothing to cut has no rows to stamp**, so [`db::chunk_digest`] answers `None` for it forever
/// and it pays one indexed lookup plus an empty cut on every scan. §5's warm-pass property is about the notes
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
    // is the scan's single write transaction — §5 requires chunk rows to be atomic with the note rows.
    db::write_chunks_for_note(db, note_id, &cut, note_digest)?;
    Ok(())
}

/// The note row one path owns, read by byte key.
#[derive(Debug)]
enum NoteRow {
    /// No note behind the path yet — an attachment, or a note this scan has not written.
    Missing,
    /// The normal case: exactly one row, whose id every derived row hangs off.
    One(i64),
    /// More than one row on the key. Legal at version 3 (`notes.rel` was never `UNIQUE`) and copied onto one
    /// `rel_bytes` by migration 4, so this is a population the reader has to name rather than pretend not to
    /// have (F13 §7).
    Duplicate,
}

/// How many note rows sit behind one path's byte key, and which one if there is a single answer.
///
/// **No `LIMIT 1`** (F13 §3): the old single-row read is what made a legacy duplicate silently *wrong* rather
/// than loud — it handed one file's title, tags, items and links to the other file's note. Refusing with an
/// error was the other wrong answer: it would make a workspace holding one old duplicate impossible to scan
/// ever again. So the count travels back to the caller, which reports the file under `skipped`, rewrites no
/// note, and carries on.
fn note_row(db: &Db, path: &RelPath) -> Result<NoteRow, DbError> {
    Ok(match db::note_ids(db, &path.rel_bytes)?.as_slice() {
        [] => NoteRow::Missing,
        [only] => NoteRow::One(*only),
        _ => NoteRow::Duplicate,
    })
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

/// `case_key` → every live row carrying that fold: the corpus [`crate::links::resolve`] places targets in.
///
/// Each entry is a [`RelPath`], so a placed target names the row it landed on rather than a spelling two
/// rows may share; matching still happens entirely on display text, which is the author's question (F13 §4.1).
///
/// A `Vec` rather than the one `rel` an earlier build kept, because `files.case_key` stopped being unique
/// with migration 4 (F13 §3): `Ämli.md` beside `ämli.md` is two files whose display fold is one string, so
/// `insert` would have handed the fold to whichever row was read last and a link naming that fold would land
/// on it by iteration order. Listing both is what lets resolution see the ambiguity and decline, exactly as
/// [`crate::links`] already models an ambiguous basename. A fold with one owner still resolves.
fn live_corpus(db: &Db) -> Result<BTreeMap<String, Vec<RelPath>>, DbError> {
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
/// database (spec §2). Tags come back in insertion order, which is the note's own order, because the
/// renderer prints them as written.
/// The join and the order are both on `rel_bytes` (F13 §3). On display `rel` two rows on each side of one
/// spelling is a cross product, so the catalog gained duplicate entries carrying wrong link counts; and for
/// twins that spelling is *identical*, so a display `ORDER BY` orders nothing and the rendered bytes fell to
/// whichever row SQLite happened to read.
fn catalog_entries(db: &Db) -> Result<Vec<CatalogEntry>, DbError> {
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
/// Whole seconds since the Unix epoch, for [`LAST_SCAN_KEY`]. A coarse clock is deliberate: the question this answers is
/// "has this vault been walked in the last few minutes?", and a stamp that implied sub-second precision would invite a
/// client to sort two workspaces by it.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        // A clock behind 1970 is not a workspace's problem to solve, and the stamp is not worth refusing a scan for:
        // record zero and let the reader see "never", which is what a pre-epoch value would mean anyway.
        .unwrap_or(0)
}

/// When this workspace was last scanned, in unix seconds — `None` when no scan has ever committed here, and also `None`
/// for a value that is not a number (a hand-edited row reads as "no stamp I can trust", never as a false timestamp).
pub fn last_scan(db: &Db) -> Result<Option<i64>, DbError> {
    Ok(meta_value(db, LAST_SCAN_KEY)?.and_then(|value| value.parse().ok()))
}

fn set_meta(db: &Db, key: &str, value: &str) -> Result<(), DbError> {
    db.0.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// The display paths the registry holds **more than one live row** for — counted, never explained (F22 §4.5).
///
/// Grouped by `rel`, the display text, because that is what a person notices: two byte-distinct paths sharing one
/// lossy spelling produce two alike-looking lines in `INDEX.md`. **That is also why the count cannot name a cause.**
/// Two different files whose names collide under lossy decoding are legitimate, correctly indexed, and
/// indistinguishable here from F23's duplicated note — and an attachment sharing a display spelling with a note
/// counts too, though it has no catalog entry at all. Narrowing this with `key_is_display_derived` would not help:
/// it flags both of F23's configurations identically (nothing separates them) while staying silent on Windows,
/// where every lossy key qualifies.
///
/// `state = 'ok'` keeps stale rows out; a note whose file went away is not a path that appears twice. Sorted by the
/// same text, so one workspace produces one record.
///
/// Collected in full before returning, like every other read here, so no statement is left mid-iteration
/// across a write.
fn duplicated_rels(db: &Db) -> Result<Vec<String>, DbError> {
    let mut statement = db.0.prepare(
        "SELECT rel FROM files WHERE state = 'ok' GROUP BY rel HAVING COUNT(*) > 1 ORDER BY rel",
    )?;
    let rels = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rels)
}

/// Record what this scan lost, **after the transaction commit and best-effort** (F22 §3).
///
/// "After the commit" is not "after the scan finished": publishing the index files and recording the workspace
/// row come later and can still fail the run. The record is therefore accurate about the *committed rows* it
/// describes, and a scan that dies after this point leaves a record of work that really did land — which is the
/// lesser error, since the alternative is describing nothing at all.
///
/// The result is dropped for the reason [`crate::db::report_orphans`] gives for its own writes: by the time
/// this runs the scan's rows and catalog are committed, so a `meta` write that refuses — a full disk, a lock another
/// process holds past the timeout, a trigger — costs this workspace its record and nothing else. Returning
/// `Err` would report a failed scan whose rows and catalog are already durable, and `.expect` would do worse
/// (F21 §2: that panic is exactly what F21 removed). The loss still reaches this run's report field and its
/// stdout line; only its afterlife is degraded.
fn record_unindexable(db: &Db, value: &str) {
    let _ = set_meta(db, UNINDEXABLE_KEY, value);
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

/// The chunker version this workspace recorded under [`EMBED_VERSION_KEY`].
///
/// `None` covers an absent key and a value this build cannot read, exactly as [`parse_version`] does: both
/// mean the rows carry no label worth trusting. Note what this number does **not** decide — which notes to
/// re-cut. That is per row, from `chunks.chunker` and `chunks.source_hash`, via [`db::chunk_state`]; this key
/// is the durable statement that the pass reached everything it owed.
fn chunker_version(db: &Db) -> Result<Option<i64>, DbError> {
    Ok(meta_value(db, EMBED_VERSION_KEY)?.and_then(|value| value.trim().parse().ok()))
}

/// Whether this scan has to record [`CHUNKER_VERSION`] afresh.
///
/// Equality and nothing else means up to date; a *higher* stored value is rewritten too, for the reason
/// [`refresh_needed`] gives. Unlike the parse comparison this asks no work of its own: a row behind the
/// chunker is stale on its own account ([`db::chunk_state`] reports it) whether or not the key lags.
fn chunker_refresh_needed(stored: Option<i64>) -> bool {
    stored != Some(CHUNKER_VERSION)
}

/// Whether a refresh reached every note it owed new derived rows.
///
/// Two ways to miss one, neither visible in [`ScanReport`]: a file that changed or became unreadable
/// between the walk and the read (`uncovered`, since the write pass skips its marker), and a live row this
/// scan could not look at — under a directory it could not enter, or carrying a name it refused (F18) —
/// which [`known_rows`] drops so it cannot be mistaken for deleted (`hidden_by_unknown`). Recording the
/// version over either would label rows as current that an older build wrote, permanently — the exact
/// failure F15 exists to prevent, reintroduced by the fix.
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
/// note whose rows are there but unreadable (a mode-000 `.md`, or one inside a directory nobody can enter), a
/// live row hidden behind such a directory, **a name this build refuses for good** (F18: while that spelling
/// stays unrepresentable no scan can index it — a refused *file* leaves its own rows owing, a refused
/// *directory* leaves everything below it owing — so they keep whatever an older build wrote and only renaming
/// the path clears the debt), a move refused by an identity conflict — judged by the path it came from, since
/// `classify` suppressed that path's `Removed` and its rows are still in the catalog — and **a duplicate note
/// pair** (`Indexed::Duplicate`). The unreadable ones clear when the obstruction does.
///
/// Of those, a refused name is the one that never clears on its own: no code renames a file, so such a
/// workspace re-parses its whole corpus on every scan and prints the gap line each time. That is chosen over
/// recording the version beside a note that was not re-parsed, which is F15's bug wearing a clean hat; the cost
/// is named here rather than discovered.
///
/// A duplicate pair no longer belongs to that class in a database this program wrote: **the next open heals
/// it.** Migration 5 (`crate::db::merge_duplicate_notes`) folds the pair onto its oldest row before the write
/// pass runs, so the charge is paid at most once more and then disappears with the reason it was withheld —
/// `a_merged_duplicate_pair_clears_the_charge_and_records_the_parse_version` in `crates/np-core/tests/scan.rs`
/// pins the clearing, and `an_unmerged_duplicate_pair_still_charges_the_refresh_and_withholds_the_version`
/// beside it pins what stays: a pair that arrives after that step has run — one another tool wrote into a
/// migrated file — is permanent again, because migration 5 does not take a second pass and this reader still
/// refuses to pick a winner. That loop is worth paying for: those rows stay wrong until the obstruction clears,
/// and the notice below names the count so this is never silent.
///
/// Read that healing narrowly, because it is the scope boundary of this paragraph: it covers
/// [`Indexed::Duplicate`] — **two note rows on one byte key** — and no other duplicate. F23's duplicate is another shape,
/// two live `files` **and** `notes` rows for one file behind a lossy display spelling, and F23 healed only the half where
/// content still agrees: a row whose **content changed while its path was unindexable** has nothing to pair on, so the
/// pair returns and this function keeps withholding the version — permanently for a migration-keyed row, whose stored key
/// matches no listing (`docs/design-f23-ambiguous-rows.md` §6 names durable file identity as the way through). "Duplicate
/// rows are healed" is therefore not a sentence to carry past here; "the ones this program can prove are one file are
/// healed" is.
fn refresh_covered(written: &Written, hidden_by_unknown: usize) -> bool {
    written.uncovered == 0 && hidden_by_unknown == 0
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
/// [`crate::db`] does for its receipt. What they cannot pin is that the production call site names stderr,
/// nor what a scan does when that descriptor will not accept the line: both are pinned by
/// `a_parse_rule_change_reports_the_re_parse_on_stderr` and
/// `a_scan_survives_a_stderr_that_refuses_every_write` in `crates/np-cli/tests/cli.rs`, which run the real
/// binary.
///
/// **A write that fails is dropped, here and in [`report_refresh_gap`], and that is deliberate.** Both are
/// called after `tx.commit()`: the rows, the note, the published index list and — when the corpus was fully
/// covered — the recorded rules version are already durable. Failing a scan because a line could not be
/// printed would turn work that succeeded into exit 101 or a `ScanError` over a courtesy notice, which is what
/// an unusable descriptor 2 means in practice: a detached terminal, a service running with stderr nowhere,
/// a device refusing writes. The cost of dropping it is one lost announcement about work already done, and the
/// next scan prints it again while the version still lags.
///
/// [`crate::db::report_orphans`] still unwraps on a refused write. It is left alone on purpose: its line is
/// printed inside the migration transaction, so nothing has been committed when it runs and a process that
/// dies there leaves the file as it was — a different decision from the one above, and not this change's to
/// revisit.
fn report_refresh(out: &mut dyn io::Write, stored: Option<u32>, notes: usize) {
    // Zero means the refresh cost nothing extra: a first scan re-parses nothing it was not going to parse
    // as added anyway. "Rules moved, nothing to do" is not news.
    if notes == 0 {
        return;
    }
    // Dropped, not unwrapped: see the note above on why a stderr that will not take this line is not a scan
    // failure.
    let _ = writeln!(
        out,
        "note-processor: re-parsed {notes} unchanged path(s) for parse rules v{PARSE_VERSION} (database held {})",
        stored.map_or_else(|| "nothing".to_string(), |version| format!("v{version}")),
    );
}

/// Say out loud that a refresh could not label the corpus, so the next scan will try again.
///
/// stderr for the same reason as [`report_refresh`]: stdout stays machine-readable JSON. `missed` counts
/// both ways a note escapes — skipped during the write pass and hidden behind an unreadable directory or
/// a refused name — and the write result is dropped for the reason spelled out on [`report_refresh`].
fn report_chunk_gap(out: &mut dyn io::Write, missed: usize) {
    if missed == 0 {
        return;
    }
    // Worded as its own fact rather than reusing the parse line: a reader who sees "parse rules" and has just
    // bumped a chunker would look in the wrong place, and the two gaps can disagree about which notes they name.
    let _ = writeln!(
        out,
        "note-processor: chunker v{CHUNKER_VERSION} not recorded — {missed} note(s) were not cut this scan; the next scan will retry"
    );
}

fn report_refresh_gap(out: &mut dyn io::Write, missed: usize) {
    if missed == 0 {
        return;
    }
    // Dropped for the same reason as [`report_refresh`]'s write.
    let _ = writeln!(
        out,
        "note-processor: parse rules v{PARSE_VERSION} not recorded — {missed} note(s) were not re-parsed this scan; the next scan will retry"
    );
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
/// The paths removed in step 3 come from [`superseded_names`] — the directory and `meta` together — rather
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

    // Every path here is the one the listing produced, never `root.join` of a display spelling: two root
    // entries can share one lossy spelling and only one of them is the file the verdict was about.
    for path in superseded.delete {
        match fs::remove_file(&path) {
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(ScanError::Io { path, source }),
            Ok(()) => {}
        }
    }
    Ok(superseded.foreign)
}

/// The generated names this workspace holds that this scan does not publish: what `meta` lists and what
/// is actually sitting in the root, unioned and judged **one directory entry at a time**.
///
/// Each entry is keyed by its file-name bytes ([`os_bytes`]), not by its display text. Two byte-distinct
/// entries whose names UTF-8 cannot hold — `INDEX-\xff.md` beside `INDEX-\xef\xbf\xbd.md` — lose to the same
/// `U+FFFD` spelling, and a verdict taken on that spelling answers for whichever one readdir happened to
/// reach first: it can unlink a header-less foreign file at its twin's bytes, or leave our own shard alive
/// in silence. That is F13 §1 on the one code path here that deletes files (see
/// `docs/design-f13-byte-paths.md`), so identity is compared as bytes end to end and the lossy text is kept
/// only for what is genuinely a display question — [`index_names::is_generated_index`] and the report line.
///
/// Which leaves one asymmetry to know about: **verdicts are per entry, report lines are not.** `foreign`
/// carries display text only — it was never a path the walk keyed — so the bytes [`ScanReport::skipped`] dedups
/// by are that text's own UTF-8 bytes, and two byte-distinct foreign twins still publish a single line. Read
/// that count as "names needing attention", never as "files on disk"; the paths the *walk* named do get one
/// line each (F20).
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
    // Every generated-looking entry in the root, tagged with whether it is ours. The verdict is taken while
    // the listing runs, because `entry.file_type` is the cheapest answer to "file or directory" and a
    // second `stat` would race with whatever renamed the entry between the two calls.
    let mut present: Vec<Listed> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ScanError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let display = name.to_string_lossy().into_owned();
        if !index_names::is_generated_index(&display) {
            continue;
        }
        // The file type is asked before the contents: a directory has no header to read, and letting
        // `File::open` answer for it would pretend a directory could be this program's output.
        let path = entry.path();
        let ours = entry.file_type().is_ok_and(|kind| kind.is_file()) && np_published(&path);
        present.push(Listed {
            name_bytes: os_bytes(&name),
            path,
            display,
            ours,
        });
    }

    // The union the crash window needs: what `meta` names and what the root holds. A key in either list
    // is judged by what is actually on disk, never by which list produced it. One sort and dedup on the
    // byte keys is also what keeps the deletion set free of duplicates: every entry below is one `root.join`
    // away from its name, so distinct name bytes mean distinct paths.
    let mut keys: Vec<Vec<u8>> = present
        .iter()
        .map(|listed| listed.name_bytes.clone())
        .collect();
    keys.extend(
        previous
            .iter()
            .filter(|name| index_names::is_generated_index(name))
            .map(|name| os_bytes(OsStr::new(name))),
    );
    keys.sort();
    keys.dedup();

    // What this scan publishes, spelled the same way. `publish` writes a name's bytes as the file name, so
    // a key equal to one of these *is* the file just written; an entry that merely displays like a published
    // name is a different file and stays a candidate under its own fingerprint.
    let published: Vec<Vec<u8>> = rendered
        .iter()
        .map(|(name, _)| os_bytes(OsStr::new(name)))
        .collect();

    let mut superseded = Superseded::default();
    for key in keys {
        if published.contains(&key) {
            continue;
        }
        match present.iter().find(|listed| listed.name_bytes == key) {
            // Ours, unlinked at the path this entry actually has.
            Some(listed) if listed.ours => superseded.delete.push(listed.path.clone()),
            // On disk, but not ours: a foreign note or a directory wearing one of our names. Reported as
            // display text because a report line is a display question (F13 §3).
            Some(listed) => superseded.foreign.push(listed.display.clone()),
            // Named by `meta` and already gone — the state this call wanted, which is how the delete loop
            // in [`write_index_files`] reads an [`io::ErrorKind::NotFound`]. Reporting it would name a
            // collision that is nothing but a shard this program never got round to writing.
            None => {}
        }
    }
    Ok(superseded)
}

/// One generated-looking root entry as the listing saw it: enough to judge it, enough to act on it.
struct Listed {
    /// Identity: the file name's OS bytes, which keep two entries sharing a lossy spelling apart. Windows
    /// has no byte spelling, so its key is the lossy text and twins stay collapsed there — F13 §5's
    /// residual, not a new one.
    name_bytes: Vec<u8>,
    /// What gets unlinked, straight from `entry.path()` — the bytes this entry really has on disk.
    path: PathBuf,
    /// The lossy display spelling: what [`index_names::is_generated_index`] reads and what the report names.
    display: String,
    /// Whether it is a regular file opening with [`catalog::HEADER`]: ours to remove.
    ours: bool,
}

/// The generated names one scan found in its root that it does not publish, split by who owns them.
#[derive(Default)]
struct Superseded {
    /// Files this program published in an earlier generation: safe to unlink. Real paths from the listing,
    /// because a display spelling can name a different entry than the one the verdict judged.
    delete: Vec<PathBuf>,
    /// Entries wearing a generated name that this program did not publish: left on disk and reported, in
    /// the display text a report line is written in.
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
        apply_changes, folded_row, is_unknown_path, known_rows, np_published, refresh_covered,
        remove_corpses, remove_file, report_refresh, report_refresh_gap, skipped_lines,
        strip_marked_removals, under_unreadable, unindexable_record, unresolved_ambiguity,
        ChunkTally, Found, UnknownPath, WritePass, Written, NAME_CAP, PARSE_VERSION,
    };
    use crate::catalog::{HEADER, HEADER_CRLF};
    use crate::change::{classify, Change};
    use crate::config::MarkerConfig;
    use crate::db::{self, upsert_file, Db};
    use crate::hash::Snapshot;
    use crate::path_keys::{self, RelPath};
    use std::fs;
    use std::io::{self, ErrorKind};
    use std::path::Path;

    /// A snapshot as [`crate::walk`] + `sha256_file` would produce it for a name that is valid UTF-8: both
    /// byte keys derived from the spelling's own bytes.
    fn snap(rel: &str) -> Snapshot {
        Snapshot {
            rel_bytes: path_keys::rel_bytes_of_display(rel),
            case_bytes: path_keys::case_fold(rel.as_bytes()),
            rel: rel.to_string(),
            case_key: path_keys::display_fold(rel),
            hash: "h".to_string(),
            size: 4,
            mtime: 10,
            file_id: None,
        }
    }

    /// §3's accepted regression, as evidence rather than a paragraph: the ASCII byte fold cannot bridge a
    /// Unicode-only case pair, so [`folded_row`] misses a rename that changes only a non-ASCII letter's case.
    ///
    /// Before round 2 this lookup ran on the Unicode display `case_key`, where `Ämli.md` and `ämli.md` are one
    /// fold and the rename was followed onto the existing row. Matched on `case_bytes` it finds nothing, so the
    /// caller upserts a second row — and [`crate::change::key_of`] is `case_bytes` under `case_sensitive =
    /// false`, which means **on Windows this pair is the entire rename-following story**, not one corner of it
    /// (`docs/backlog-m2-followups.md` F18). Two seams keep this out of the integration tests: `folded_row` is
    /// private, and `scan::run` hardcodes case sensitivity as `!cfg!(windows)`; the classify half of the same
    /// pair — the note detaching into `Added` + `Removed` — is pinned in `crates/np-core/tests/change.rs`.
    #[test]
    fn the_ascii_fold_cannot_follow_a_unicode_only_case_rename() {
        let db = Db::in_memory().expect("in-memory database");
        upsert_file(&db, &snap("Ämli.md"), Path::new("/ws/Ämli.md"), 1)
            .expect("test setup: store the row under its capital-Ä spelling");

        let renamed = snap("ämli.md");
        // The premises, spelled out: one Unicode fold, two ASCII folds. It is the first of these that used to
        // decide this lookup, and the second that decides it now.
        assert_eq!(renamed.case_key, path_keys::display_fold("Ämli.md"));
        assert_ne!(
            renamed.case_bytes,
            path_keys::case_fold("Ämli.md".as_bytes())
        );

        assert_eq!(
            folded_row(&db, &renamed.case_bytes, &renamed.rel_bytes).expect("the lookup runs"),
            None,
            "`Ämli.md` → `ämli.md` is a rename this lookup no longer follows"
        );

        // Control, so the answer above is about the pair rather than about the fixture: an ASCII-only rename
        // shares one `case_bytes` and is still followed onto the row it came from.
        upsert_file(&db, &snap("Aemli.md"), Path::new("/ws/Aemli.md"), 1)
            .expect("test setup: store the ASCII twin pair's row");
        let ascii = snap("aemli.md");
        assert_eq!(
            folded_row(&db, &ascii.case_bytes, &ascii.rel_bytes).expect("the lookup runs"),
            Some(RelPath::new(b"Aemli.md".to_vec(), "Aemli.md".to_string())),
            "an ASCII case rename is still followed"
        );
    }

    // F18's protective half, on the platform that cannot refuse a real name: the two lists `crate::walk`
    // reports are built by hand here, exactly as that walk builds them, because no Linux filesystem can put
    // an unpaired surrogate in a file name. What they must never do is over-reach — every one of these tests
    // names the paths that stay visible alongside the refused one.
    //
    // The refusal has two arms and they are not interchangeable: a refused **file** suppresses its own `rel`
    // alone, and a refused **directory** suppresses its whole subtree by prefix. Getting either wrong is the
    // silent overwrite F18 exists to stop (too narrow) or a catalog that stops noticing real deletions
    // (too wide).

    /// The exact arm: one refused file, and four neighbours that must keep being judged.
    #[test]
    fn a_refused_name_suppresses_only_its_own_rel() {
        let refused = [display_keyed_unknown("notes/caf\u{fffd}.md")];

        assert!(
            is_unknown_path(&key("notes/caf\u{fffd}.md"), &[], &refused),
            "the refused path itself is unknown to this scan"
        );

        // Each of these shares something with the refused name, which is exactly why equality is the rule:
        // widening it to a prefix would hide a live note from `classify`, from move resolution, and from the
        // stale-marking that keeps a removed file out of the catalog.
        for rel in [
            "notes/caf\u{fffd}.md.bak", // same spelling as a prefix, one more extension
            "notes/other.md",           // its sibling in the same directory
            "notes/sub/caf\u{fffd}.md", // the same file name one level down elsewhere
            "caf\u{fffd}.md",           // the same spelling at the root
        ] {
            assert!(
                !is_unknown_path(&key(rel), &[], &refused),
                "{rel} is another path"
            );
        }
    }

    /// The prefix arm, fed the way [`crate::walk`] now feeds it: a refused **directory** goes into the
    /// unreadable list, because its name is the first component of every path below it and none of those can
    /// be keyed either. Suppressed by exact equality instead, every row in that subtree would be reported
    /// `Removed` by the next scan.
    #[test]
    fn a_refused_directory_suppresses_every_row_below_it() {
        let dirs = [display_keyed_unknown("notes/caf\u{fffd}")];

        assert!(
            is_unknown_path(&key("notes/caf\u{fffd}/a.md"), &dirs, &[]),
            "a child of the refused directory"
        );
        assert!(
            is_unknown_path(&key("notes/caf\u{fffd}/deep/deeper/b.md"), &dirs, &[]),
            "at any depth: the walk never entered at all"
        );
        // Only what is *below* it. A directory whose name merely starts with the same bytes is a different
        // subtree, and the refused spelling itself is not suppressed — no file row can be keyed by a
        // directory's path, so there is nothing here that needs suppressing.
        assert!(
            !is_unknown_path(&key("notes/caf\u{fffd}x/a.md"), &dirs, &[]),
            "a sibling that shares the prefix bytes"
        );
        assert!(!is_unknown_path(&key("notes/other/a.md"), &dirs, &[]));
        assert!(!is_unknown_path(&key("notes/caf\u{fffd}"), &dirs, &[]));
    }

    /// The byte identity [`crate::walk`] would record for a path spelled as text — the faithful-copy rule
    /// ([`path_keys::rel_bytes_of_display`]), which is also exactly what a row written by migration 4 holds.
    fn key(rel: &str) -> Vec<u8> {
        path_keys::rel_bytes_of_display(rel)
    }

    /// An unknown path from its **raw identity bytes**, with the display text a lossy decode of those same
    /// bytes produces. Unlike [`display_keyed_unknown`], whose bytes are derived from its text, this one can hold
    /// bytes no `String` can — which is the whole subject of F20: `raw(b"caf\xE9")` and `raw(b"caf\xE8")` are two paths
    /// whose `display` fields are one string.
    fn raw(bytes: &[u8]) -> UnknownPath {
        UnknownPath {
            display: String::from_utf8_lossy(bytes).into_owned(),
            bytes: bytes.to_vec(),
        }
    }

    /// The premise every F20 case below rests on, asserted instead of assumed: `left` and `right` are distinct
    /// as identity and identical as display text. Put it in a helper because a fixture that quietly stopped
    /// producing one shared spelling would keep passing while proving nothing.
    fn assert_twin_bytes(left: &[u8], right: &[u8]) {
        assert_ne!(left, right, "twins have to be different paths");
        assert_eq!(
            String::from_utf8_lossy(left).into_owned(),
            String::from_utf8_lossy(right).into_owned(),
            "twins have to share one display spelling"
        );
    }

    /// An unknown path whose [`UnknownPath::bytes`] are the **stored key** a path gets when it only ever existed as
    /// text — [`path_keys::rel_bytes_of_display`], the rule migration 4 keyed every copied row through. That is why
    /// the [`key_is_display_derived`] fixtures build their entries this way: entry and row agree because both went
    /// through the key rule, not because either holds OS bytes.
    ///
    /// Deliberately not named like `walk::tests::unknown_from_text_bytes`, which takes a path's `str::as_bytes`
    /// with no key rule between. The two return the same bytes for every input today — `rel_bytes_of_display` *is*
    /// `as_bytes` — and promise different things: this one tracks the stored-key rule, that one tracks the platform
    /// whose refusal entries are already lossy text (F18). Two names keep that difference visible at the call site.
    fn display_keyed_unknown(display: &str) -> UnknownPath {
        UnknownPath {
            display: display.to_string(),
            bytes: key(display),
        }
    }

    /// A registry row for `rel` with one note behind it: the shape [`known_rows`] counts as **hidden**, and
    /// the only shape that can hold [`PARSE_VERSION`] back.
    fn plant_note(db: &Db, rel: &str) {
        upsert_file(db, &snap(rel), &Path::new("/ws").join(rel), 1)
            .expect("test setup: registry row");
        db::insert_note(
            db,
            &RelPath::from_display(rel),
            "t",
            "filename",
            "utf-8",
            3,
            "body",
        )
        .expect("test setup: note row");
    }

    /// The charge behind the suppression: a hidden note is one the refresh never re-parsed, so recording the
    /// new rules over it would label rows an older build wrote as current (F15). `known_rows` keeps the row out
    /// of `classify`'s reach **and** counts it here — one comparison, both halves of the promise.
    ///
    /// F23 §5.2 named this case as one whose contract was being replaced, on the reading that its rows are
    /// ambiguous (`snap` keys from display text, so they are). Running it showed the other half of §3: the entry
    /// [`crate::walk`] would record for this refusal is built by [`display_keyed_unknown`], whose **bytes** equal
    /// those rows' keys, so the byte-keyed arm answers first and precedence keeps them charged in source 1 and
    /// unmarked. That is pinned below rather than assumed: listing their own paths readable does not move them.
    #[test]
    fn a_note_behind_a_refused_path_is_charged_to_the_refresh_that_missed_it() {
        let db = Db::in_memory().expect("in-memory database");
        // The refused subtree: two notes at different depths, plus an attachment that owes no parse.
        plant_note(&db, "notes/caf\u{fffd}/a.md");
        plant_note(&db, "notes/caf\u{fffd}/deep/b.md");
        upsert_file(
            &db,
            &snap("notes/caf\u{fffd}/image.png"),
            Path::new("/ws/notes/caf\u{fffd}/image.png"),
            1,
        )
        .expect("test setup: an attachment row with no note behind it");
        // Two notes this scan does see, one of them in the same parent directory as the refused one.
        plant_note(&db, "notes/peer.md");
        plant_note(&db, "elsewhere/c.md");

        // A refused directory, recorded as [`crate::walk`] records it.
        let dirs = [display_keyed_unknown("notes/caf\u{fffd}")];
        let refused = known_rows(&db, &dirs, &[], &[]).expect("the rows read back");

        assert_eq!(
            refused
                .rows
                .iter()
                .map(|row| row.rel.as_str())
                .collect::<Vec<_>>(),
            ["elsewhere/c.md", "notes/peer.md"],
            "the refused subtree reaches neither `Removed` nor a move, and its neighbours still do"
        );
        assert_eq!(
            refused.hidden, 2,
            "one charge per hidden note — the attachment behind the same refusal owes nothing"
        );

        // §3's precedence, on the arm pair that overlaps for the permanent class. These rows are ambiguous by
        // predicate, and their own paths can be listed readable with a matching hash, yet the byte-keyed arm has
        // already answered: they stay charged **once**, in source 1, and are never marked. Mark-first would charge
        // each of them twice — universally on Windows, where refusal bytes are already lossy.
        let listed = [
            snap("notes/caf\u{fffd}/a.md"),
            snap("notes/caf\u{fffd}/deep/b.md"),
        ];
        let both_arms =
            known_rows(&db, &dirs, &[], &listed).expect("the same rows, with their paths listed");
        assert_eq!(
            both_arms.hidden,
            2,
            "source 1 still owns the charge: {:?}",
            both_arms
                .marked
                .iter()
                .map(|row| &row.rel)
                .collect::<Vec<_>>()
        );
        assert!(
            both_arms.marked.is_empty(),
            "a row the byte arm hid is not marked, so it cannot charge twice"
        );

        // The other arm, fed a refused *file*: exactly its own row disappears and is charged.
        let peer = known_rows(&db, &[], &[display_keyed_unknown("notes/peer.md")], &[])
            .expect("the rows read back again");
        assert_eq!(peer.hidden, 1, "one refused note, one charge");
        assert!(
            !peer.rows.iter().any(|row| row.rel == "notes/peer.md"),
            "the refused path is out of the comparison"
        );
        assert_eq!(
            peer.rows
                .iter()
                .map(|row| row.rel.as_str())
                .collect::<Vec<_>>(),
            [
                "elsewhere/c.md",
                "notes/caf\u{fffd}/a.md",
                "notes/caf\u{fffd}/deep/b.md",
                "notes/caf\u{fffd}/image.png",
            ],
            "read that directory as reachable and only the refused file itself disappears"
        );

        // And the charge is what withholds the version. Both halves of `refresh_covered` are pinned because
        // the notice the user sees prints their sum (`written.uncovered + hidden_by_unknown`).
        assert!(
            !refresh_covered(&Written::default(), peer.hidden),
            "a refresh that never reached a note may not label its rules current"
        );
        assert!(
            refresh_covered(&Written::default(), 0),
            "with nothing hidden, the version is recorded"
        );
        let missed_write = Written {
            uncovered: 1,
            ..Written::default()
        };
        assert!(
            !refresh_covered(&missed_write, 0),
            "the write-pass half still holds on its own"
        );
    }

    /// The one exception to byte matching, and its bound, in one comparison. A row whose key migration 4 derived
    /// from display text ([`key_is_display_derived`]) has no byte spelling to be judged by, so an unreadable
    /// directory whose bytes it cannot tell apart is **marked** — since F23 that is what the fallback does with
    /// it, instead of deleting it from the scan's view. With nothing listed the mark refuses admission (today's
    /// pre-drop) and charges source 2; with a listed readable path spelling the row exactly and carrying its
    /// stored hash the row is **in `known`, marked, and unremovable** — `classify` still calls it `Removed`, and
    /// [`strip_marked_removals`] is what stops that stale-ing a live note.
    ///
    /// The same display spelling keyed from **real** bytes is a row the walk re-keyed, and stays in `known` on
    /// its bytes: that is F20's twin fix surviving the fallback, asserted rather than promised. F23 §5.2 names
    /// this case as a contract update rather than a fence, and it is one: the assertions below used to read
    /// `hidden == 1`, which was the deletion being counted.
    #[test]
    fn a_display_keyed_row_falls_back_to_text_and_a_byte_keyed_row_does_not() {
        assert_twin_bytes(b"caf\xE9/note.md", b"caf\xE8/note.md");

        let db = Db::in_memory().expect("in-memory database");
        // The migrated row: a note really named `caf\xE9/note.md`, stored under the bytes of its lossy text.
        plant_note(&db, "caf\u{fffd}/note.md");
        // A row this build wrote: one display spelling, the walk's own bytes as its key.
        let walked = Snapshot {
            rel_bytes: b"caf\xE9/note.md".to_vec(),
            case_bytes: path_keys::case_fold(b"caf\xE9/note.md"),
            rel: "caf\u{fffd}/note.md".to_string(),
            case_key: path_keys::display_fold("caf\u{fffd}/note.md"),
            hash: "h".to_string(),
            size: 4,
            mtime: 10,
            file_id: None,
        };
        upsert_file(&db, &walked, Path::new("/ws/caf\u{fffd}/note.md"), 1)
            .expect("test setup: byte-keyed registry row");
        db::insert_note(
            &db,
            &RelPath::new(
                b"caf\xE9/note.md".to_vec(),
                "caf\u{fffd}/note.md".to_string(),
            ),
            "t",
            "filename",
            "utf-8",
            3,
            "body",
        )
        .expect("test setup: note behind the byte-keyed row");
        // A plain name, to show the fallback reaches no further than a lossy key can.
        plant_note(&db, "top.md");

        // The blocked directory is the **twin** (`caf\xE8`); `caf\xE9` was read in full by this walk.
        let dirs = [raw(b"caf\xE8")];
        let refused = known_rows(&db, &dirs, &[], &[]).expect("the rows read back");

        assert_eq!(
            refused
                .rows
                .iter()
                .map(|row| row.rel_bytes.as_slice())
                .collect::<Vec<_>>(),
            [b"caf\xE9/note.md".as_slice(), b"top.md".as_slice()],
            "the byte-keyed row and the plain row stay in the comparison; only the text-keyed row drops out"
        );
        assert_eq!(
            refused.hidden, 0,
            "the byte arm saw this row and answered `known`: source 1 charges nothing here"
        );
        let marks = refused
            .marked
            .iter()
            .map(|row| &row.rel)
            .collect::<Vec<_>>();
        assert_eq!(
            marks,
            ["caf\u{fffd}/note.md"],
            "and the fallback marked exactly one row"
        );
        assert!(
            !refused.marked[0].admitted,
            "no listed path spells it, so §2's scoping refuses admission and it never reaches `classify`"
        );
        assert_eq!(
            unresolved_ambiguity(&refused.marked, &[], &[]),
            ["caf\u{fffd}/note.md".to_string()],
            "the legacy note is still charged to a refresh that could not reach it — as source 2 now"
        );

        // The contract F23 replaces: a listed readable path that spells the row exactly **and** carries its
        // stored hash admits it. It pairs with nothing by identity — its key is display text, the listing's are
        // the walk's bytes — so `classify` still reaches for a `Removed`, and being marked is what makes that
        // change unremovable.
        let (_, lookalike) = twin_row(b"caf\xE8", "note.md");
        let listed = [lookalike];
        let admitted = known_rows(&db, &dirs, &[], &listed).expect("the rows read back once more");
        assert!(
            admitted
                .rows
                .iter()
                .any(|row| row.rel_bytes == b"caf\xEF\xBF\xBD/note.md".to_vec()),
            "an admitted ambiguous row is in `known`: {:?}",
            admitted.rows.iter().map(|row| &row.rel).collect::<Vec<_>>()
        );
        assert!(
            admitted.marked[0].admitted,
            "marked **and** admitted, which is the state §4's post-strip exists for"
        );

        let changes = classify(&admitted.rows, &listed, true);
        assert!(
            changes.iter().any(|(path, change)| {
                matches!(change, Change::Removed) && path.rel_bytes == b"caf\xEF\xBF\xBD/note.md"
            }),
            "premise: `classify` would stale it — {:?}",
            changes
                .iter()
                .map(|(path, change)| (path.rel.clone(), format!("{change:?}")))
                .collect::<Vec<_>>()
        );
        let kept = strip_marked_removals(changes, &admitted.marked);
        assert!(
            !kept.iter().any(|(path, change)| {
                matches!(change, Change::Removed) && path.rel_bytes == b"caf\xEF\xBF\xBD/note.md"
            }),
            "and the strip is what makes it unremovable: {:?}",
            kept.iter()
                .map(|(path, change)| (path.rel.clone(), format!("{change:?}")))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            unresolved_ambiguity(&admitted.marked, &kept, &listed),
            ["caf\u{fffd}/note.md".to_string()],
            "strip or no strip, its rows are still unpaired while a look-alike holds the content: it charges"
        );
    }

    // F20 §5: the two matchers over hand-built `UnknownPath`s whose **bytes** differ while their display text
    // does not. A Linux filesystem refuses no name, so the refused arm is hand-built here exactly as F18's tests
    // above build it — and the unreadable arm is hand-built for a reason F18 did not need: on Unix the twin pair
    // `caf\xE9` / `caf\xE8` is reachable today, and only raw bytes can spell it.

    /// Byte near-twins are different paths. Neither matcher may let `caf\xE9` cover `caf\xE8`, whose display
    /// spelling is identical — that single collapse is F20 §1: a readable subtree dropped out of `known`, so a
    /// deletion in it went unreported and a rename there produced a second live row instead of a move.
    #[test]
    fn a_byte_near_twin_is_not_the_unknown_path() {
        assert_twin_bytes(b"caf\xE9", b"caf\xE8");

        // The prefix arm: an unreadable directory covers its own subtree and nothing else.
        let dirs = [raw(b"caf\xE9")];
        assert!(under_unreadable(b"caf\xE9/low.md", &dirs), "its own child");
        assert!(
            !under_unreadable(b"caf\xE8/low.md", &dirs),
            "the twin's child shares the display spelling and stays a path this scan read"
        );
        assert!(
            !is_unknown_path(b"caf\xE8/low.md", &dirs, &[]),
            "`known_rows` asks [`is_unknown_path`], so the twin's row is not hidden from the comparison"
        );

        // The exact arm: one refused file, its byte-twin, and three neighbours that share something with it.
        // Exact equality is what keeps all four of these visible (F18), and bytes are what makes the first of
        // them the *right* one to suppress when two refused names spell alike.
        let files = [raw(b"caf\xE9.md")];
        assert!(
            is_unknown_path(b"caf\xE9.md", &[], &files),
            "the refused path itself is unknown to this scan"
        );
        for other in [
            b"caf\xE8.md".as_slice(),     // the twin: one display spelling, another path
            b"caf\xE9.md.bak".as_slice(), // the refused bytes as a prefix of a longer name
            b"other/caf\xE9.md".as_slice(), // the same name in another directory
            b"caf\xE9/x.md".as_slice(),   // below the twin spelling of its own directory
        ] {
            assert!(
                !is_unknown_path(other, &[], &files),
                "{} is another path",
                String::from_utf8_lossy(other)
            );
        }
    }

    /// The prefix rule ends at a `/` byte: `foo` names a **directory**, so it covers everything below it and
    /// nothing that merely starts with the same bytes. Drop the boundary check and one unreadable directory
    /// hides every path sharing its first letters — `foobar/x.md` among them, which this scan did read.
    #[test]
    fn an_unreadable_prefix_matches_only_at_a_slash_boundary() {
        let dirs = [raw(b"foo")];
        assert!(under_unreadable(b"foo/x.md", &dirs), "its own child");
        assert!(
            under_unreadable(b"foo/deep/deeper/x.md", &dirs),
            "at any depth: the walk never entered at all"
        );
        assert!(
            !under_unreadable(b"foobar/x.md", &dirs),
            "a sibling whose name merely starts with the same bytes"
        );
        assert!(
            !under_unreadable(b"foo", &dirs),
            "a directory is not below itself"
        );
        assert!(
            !is_unknown_path(b"foobar/x.md", &dirs, &[]),
            "and the boundary holds through the arm `known_rows` uses"
        );

        // A multi-component entry behaves the same way: the boundary is checked against its **last** byte.
        let dirs = [raw(b"notes/caf\xE9")];
        assert!(under_unreadable(b"notes/caf\xE9/deep/x.md", &dirs));
        assert!(!under_unreadable(b"notes/caf\xE8/x.md", &dirs));
        assert!(!under_unreadable(b"notes/caf\xE9x/x.md", &dirs));
    }

    /// `"."` is how [`crate::walk`] spells the workspace root, and it means **everything below**: a scan that
    /// could not read the root at all knows nothing about any path, so none may be judged absent. Both fields
    /// carry `"."` for that reason — see [`crate::walk`]'s `unknown_path`.
    #[test]
    fn the_root_entry_makes_every_path_below_it_unknown() {
        let dirs = [UnknownPath {
            display: ".".to_string(),
            bytes: b".".to_vec(),
        }];
        for rel in [
            b"a.md".as_slice(),
            b"deep/b.md".as_slice(),
            // A non-UTF-8 path too: `"."` is not a byte prefix of this one, and does not need to be.
            b"caf\xE9/deep/c.md".as_slice(),
        ] {
            assert!(
                under_unreadable(rel, &dirs),
                "{}: the root was unreadable, so nothing below it is knowable",
                String::from_utf8_lossy(rel)
            );
            assert!(
                is_unknown_path(rel, &dirs, &[]),
                "{} reaches the same answer through `known_rows`' arm",
                String::from_utf8_lossy(rel)
            );
        }
    }

    /// An entry holding **no** bytes matches nothing rather than everything. The walk never builds one — it
    /// spells the root `"."` (see [`crate::walk`]) — and an empty prefix is the single way this filter could
    /// hide a whole corpus instead of one subtree, so the degenerate case is pinned rather than trusted.
    #[test]
    fn an_entry_with_no_bytes_matches_nothing() {
        let dirs = [UnknownPath {
            display: String::new(),
            bytes: Vec::new(),
        }];
        for rel in [b"a.md".as_slice(), b"deep/b.md".as_slice()] {
            assert!(
                !under_unreadable(rel, &dirs),
                "an empty entry must not swallow {}",
                String::from_utf8_lossy(rel)
            );
            assert!(
                !is_unknown_path(rel, &dirs, &[]),
                "nor reach the comparison through the prefix arm"
            );
        }
        // The exact arm degenerates the same way: no row is keyed by an empty path, so nothing is suppressed.
        let files = [UnknownPath {
            display: String::new(),
            bytes: Vec::new(),
        }];
        assert!(!is_unknown_path(b"a.md", &[], &files));
    }

    /// [`ScanReport::skipped`] is sorted and deduplicated by **bytes**: two refused (or unreadable, or
    /// unread-and-vanished) paths sharing one lossy spelling publish two lines that look alike, because the
    /// scan saw two paths and can only name them alike (F20 §1, §3). One path reaching the report twice is
    /// still one line — the dedup survives the key change, it just stops being able to merge two paths.
    #[test]
    fn skipped_lines_dedups_by_bytes_and_not_by_display() {
        assert_twin_bytes(b"caf\xE8/note.md", b"caf\xE9/note.md");
        let shared = "caf\u{fffd}/note.md";

        assert_eq!(
            skipped_lines(vec![
                raw(b"caf\xE9/note.md"),
                raw(b"caf\xE8/note.md"),
                raw(b"caf\xE8/note.md"),
            ]),
            [shared, shared],
            "two paths, two lines, ordered by the byte key rather than by arrival"
        );

        assert_eq!(
            skipped_lines(vec![raw(b"caf\xE8/note.md"), raw(b"caf\xE8/note.md")]),
            [shared],
            "one path named twice is one line"
        );
    }

    /// F22 §3's arithmetic, over **hand-built** walk lists.
    ///
    /// A Linux filesystem refuses no name — `os_bytes` is exact — so the refusal arm has no end-to-end
    /// fixture and is counted here instead, exactly as the matcher cases above and in `walk::tests` build
    /// their refused entries. What this does **not** claim is that any real scan produces a non-zero
    /// `unkeyable`: only Windows refuses a name, and that arm stays unexecuted (F18's honest terms). The last
    /// block pins the consequence for every run on this platform: `unkeyable` is zero, always.
    #[test]
    fn the_record_counts_each_loss_by_kind_and_caps_its_names() {
        // Two refused names and one unreadable directory, all with distinct display text. The counts have to
        // stay apart, because "3 paths skipped" — the sentence `skipped` prints — is precisely what F22
        // exists to replace with a diagnosis.
        let refused = [
            display_keyed_unknown("caf\u{fffd}.md"),
            display_keyed_unknown("zebra.md"),
        ];
        let blocked = [raw(b"caf\xE9")];
        let duplicates = ["dup/a.md".to_string(), "dup/b.md".to_string()];
        let ambiguous = ["lost/note.md".to_string()];
        let record = unindexable_record(&blocked, &refused, &duplicates, &ambiguous);

        assert_eq!(
            (record.unreadable, record.unkeyable, record.duplicates),
            (1, 2, 2),
            "three kinds of loss, three counts — merged they would say only \"5\""
        );
        assert_eq!(
            record.ambiguous,
            1,
            "and F23's fourth kind: a note whose row could not be matched to a byte spelling this scan"
        );
        assert_eq!(
            record.names,
            [
                "caf\u{fffd}.md",
                "zebra.md",
                "dup/a.md",
                "dup/b.md",
                "lost/note.md",
                "caf\u{fffd}",
            ],
            // A refused name and a duplicated path have no other channel; an ambiguous note's only other signal is
            // a parse version its workspace cannot record; an unreadable directory is also named in
            // `ScanReport::skipped`, so it gives up the last cap slot.
            "unkeyable first, then duplicated paths, then ambiguous notes, then unreadable directories"
        );
        // The stored spelling, pinned whole — `meta`, `np scan`'s line and this struct read one format, so a
        // person comparing yesterday's terminal against today's record is comparing one string.
        assert_eq!(
            record.value(),
            "unreadable=1;unkeyable=2;duplicates=2;ambiguous=1;names=caf\u{fffd}.md,zebra.md,dup/a.md,dup/b.md,lost/note.md,caf\u{fffd}",
        );

        // The cap: five refused, four unreadable, three duplicated is twelve losses and eight names. What falls
        // off is the directories: a refused name and a duplicated path reach the person only through this list,
        // while an unreadable directory is named in `ScanReport::skipped` as well (F22 §3).
        let spelled = |prefix: &str, count: usize| -> Vec<UnknownPath> {
            (0..count)
                .map(|index| display_keyed_unknown(&format!("{prefix}{index}.md")))
                .collect()
        };
        let many_duplicates: Vec<String> = (0..3).map(|index| format!("dup{index}.md")).collect();
        let some_ambiguous: Vec<String> = (0..2).map(|index| format!("lost{index}.md")).collect();
        let capped = unindexable_record(
            &spelled("dir", 4),
            &spelled("no", 3),
            &many_duplicates,
            &some_ambiguous,
        );
        assert_eq!(
            (capped.unreadable, capped.unkeyable, capped.duplicates),
            (4, 3, 3),
            "the counts say twelve while the names say eight"
        );
        assert_eq!(
            capped.names.len(),
            NAME_CAP,
            "the value is bounded: it is rewritten on every scan, so an unbounded list would grow one \
             `meta` row without limit"
        );
        assert_eq!(
            &capped.names[..3],
            ["no0.md", "no1.md", "no2.md"],
            "every refused name survives before any directory's"
        );
        assert_eq!(
            &capped.names[3..6],
            ["dup0.md", "dup1.md", "dup2.md"],
            "then every duplicated path"
        );
        assert_eq!(
            &capped.names[6..],
            ["lost0.md", "lost1.md"],
            "then the ambiguous notes, and the four directories are what the cap cost — they are in `skipped` too"
        );

        // Nothing lost: `is_empty` is what makes the report field `None`, while the value is still the full
        // all-zero spelling — a fixed workspace has to overwrite an older complaint rather than leave it
        // standing there accusing somebody (F22 §2).
        let clean = unindexable_record(&[], &[], &[], &[]);
        assert!(clean.is_empty(), "nothing lost is nothing reported");
        assert_eq!(
            clean.value(),
            "unreadable=0;unkeyable=0;duplicates=0;ambiguous=0;names="
        );
        assert!(clean.names.is_empty());

        // And the shape every scan on this platform can actually reach: an unreadable directory, no refusal
        // of any kind. `walk::Walked::unrepresentable` is empty on Unix by construction, so a real run's
        // record reads `unkeyable=0` here for the same reason — the non-zero arm above is Windows-only and
        // unexecuted.
        let unix = unindexable_record(&blocked, &[], &[], &[]);
        assert_eq!(
            unix.unkeyable, 0,
            "Unix refuses no name, so no scan run here can raise this count"
        );
        assert_eq!(
            unix.ambiguous, 0,
            "and a workspace with nothing ambiguous records nothing — the field is rendered from the count that \
             holds the parse version back, not from a guess about it"
        );
        assert!(!unix.is_empty(), "one unreadable directory is still a loss");
    }

    /// §3's exclusion, pinned on the class it exists for. A row whose key already equals its OS bytes — a name
    /// genuinely holding `U+FFFD`, which is every lossy name on Windows — can be marked *and* listed unread: the walk
    /// saw the path, the read failed, and `written.uncovered` charges that. Source 2 must stand down for it, or one note
    /// pays twice and the gap line over-counts the way F20 §1 says it used to.
    #[test]
    fn a_marked_row_listed_but_unread_charges_exactly_one_counter() {
        assert_twin_bytes(b"caf\xE9/note.md", b"caf\xE8/note.md");

        let db = Db::in_memory().expect("in-memory database");
        // A note whose real directory name is the three bytes `EF BF BD`: its key is its own display text, so it is
        // a member of the permanent class, and no migration had to make it one.
        plant_note(&db, "caf\u{fffd}/note.md");
        let blocked = raw(b"caf\xE9");
        let known =
            known_rows(&db, std::slice::from_ref(&blocked), &[], &[]).expect("the rows read back");

        assert_eq!(
            known.hidden, 0,
            "the byte arm cannot see this row: its key is `EF BF BD`, the refusal is a raw `E9`"
        );
        assert_eq!(
            known.marked.iter().map(|row| &row.rel).collect::<Vec<_>>(),
            ["caf\u{fffd}/note.md"],
            "the look-alike prefix marks it, which is §2's width unchanged"
        );

        // The same path listed by this walk with an empty hash: it exists, and the read failed.
        let mut listed = twin_row(b"caf\xEF\xBF\xBD", "note.md").1;
        listed.hash.clear();
        assert_eq!(
            unresolved_ambiguity(&known.marked, &[], &[listed]),
            Vec::<String>::new(),
            "source 2 stands down — the read failure already charges this note in `written.uncovered`"
        );

        // With nothing listed at all, that same row is exactly source 2's population: charged once, not zero times.
        assert_eq!(
            unresolved_ambiguity(&known.marked, &[], &[]),
            ["caf\u{fffd}/note.md".to_string()],
            "hidden work charges in the counter that describes it"
        );
    }

    /// §4.4's answer, **counted** rather than assumed: one note behind one unknown path is charged in exactly
    /// one of the two counters the gap line adds together.
    ///
    /// They are disjoint for the rows both sides judge **by bytes**, which is what this test builds.
    /// `hidden_by_unknown` charges a live row this scan never looked at — a note under an unreadable directory,
    /// which the walk could not even list, so it never reaches `found` and cannot be charged twice.
    /// `written.uncovered` charges a path the walk *did* list whose contents this scan could not read. One row
    /// cannot be both: [`under_unreadable`]'s byte prefix only ever names a directory that was never entered, and
    /// [`is_unknown_path`]'s refused arm only ever names a name the walk would not key — neither is a listed path.
    ///
    /// The [`key_is_display_derived`] fallback used to sit outside that construction: it hid a row on **display** text,
    /// so a row it dropped could also be one whose own path was listed and could not be read — `written.uncovered`'s
    /// shape as well. F23 settled that in two places rather than leaving it to luck: `known_rows` asks the byte arm
    /// **first** and marks nothing it already hid (§3's precedence, which on Windows is the difference between one charge
    /// and a universal double charge), and [`unresolved_ambiguity`] skips any marked row whose key appears in the
    /// found-side unreadable set. [`a_marked_row_listed_but_unread_charges_exactly_one_counter`] pins that exclusion on
    /// the class it exists for; what remains here is the byte-keyed disjointness F20 established.
    ///
    /// Before F20 the two sets overlapped for byte-keyed rows too: a note in the readable twin was hidden by its
    /// unreadable twin's display spelling **and** charged as an unread write, so one note cost two charges and the
    /// refresh-gap line over-counted (F20 §1). `ScanReport` carries no gap count, which is why this arithmetic lives
    /// here rather than in `tests/scan.rs`.
    #[test]
    fn one_note_behind_an_unknown_path_is_charged_in_exactly_one_counter() {
        assert_twin_bytes(b"caf\xE9", b"caf\xE8");
        let db = Db::in_memory().expect("in-memory database");
        // The twin pair, stored the way the write pass stores a walk's finding: distinct identity bytes, one
        // display spelling `caf\u{FFFD}/low.md`.
        plant_twin_note(&db, b"caf\xE9", "low.md");
        plant_twin_note(&db, b"caf\xE8", "low.md");
        // The unreadable twin, spelled with the bytes the walk really saw.
        let dirs = [raw(b"caf\xE9")];

        // Case 1 — the fixture in `tests/scan.rs`: one directory the walk could not enter, its note behind it.
        let first = known_rows(&db, &dirs, &[], &[]).expect("the rows read back");
        assert_eq!(
            first.hidden, 1,
            "one note behind the unreadable twin: one charge"
        );
        assert_eq!(
            first
                .rows
                .iter()
                .map(|row| row.rel.as_str())
                .collect::<Vec<_>>(),
            ["caf\u{fffd}/low.md"],
            "and the only row left in the comparison is the **readable** twin's"
        );
        let written = apply_changes(
            &db,
            &[twin_found(b"caf\xE8", "low.md", true)],
            &[],
            true,
            1,
            true,
            &mut WritePass {
                markers: &MarkerConfig::default(),
                chunks: &mut ChunkTally::default(),
            },
        )
        .expect("the write pass runs over the twin this scan read");
        assert_eq!(
            written.uncovered, 0,
            "a note this scan read is not charged as an uncovered write"
        );
        assert_eq!(
            first.hidden + written.uncovered,
            1,
            "one note the refresh missed, one charge on the gap line"
        );

        // Case 2 — the overlap F20 closed. The readable twin's note cannot be read this scan either (gone, or
        // changed, between walk and read), so it reaches `found` as an unknown marker while its unreadable twin
        // sits in the list. Matched on display text it would be charged **twice**: hidden by a refusal this
        // scan never made, and missed by the write pass.
        let second = known_rows(&db, &dirs, &[], &[]).expect("the rows read back again");
        assert_eq!(
            second.hidden, 1,
            "only the genuinely hidden note is charged: the readable twin's row is still in `known`"
        );
        assert!(
            second
                .rows
                .iter()
                .any(|row| row.rel_bytes == b"caf\xE8/low.md".to_vec()),
            "the readable twin survives the comparison on bytes"
        );
        let written = apply_changes(
            &db,
            &[twin_found(b"caf\xE8", "low.md", false)],
            &[],
            true,
            1,
            true,
            &mut WritePass {
                markers: &MarkerConfig::default(),
                chunks: &mut ChunkTally::default(),
            },
        )
        .expect("the write pass runs over an unreadable twin");
        assert_eq!(
            written.uncovered, 1,
            "the note this scan could not read is charged once"
        );
        assert_eq!(
            second.hidden + written.uncovered,
            2,
            "two notes the refresh missed, two charges — display-text matching made this three (F20 §1)"
        );
    }

    /// One twin's path pair: identity bytes that UTF-8 cannot hold (`caf\xE9/low.md`) plus the single display
    /// spelling both twins decode to.
    fn twin_row(dir: &[u8], name: &str) -> (RelPath, Snapshot) {
        let whole = [dir, b"/", name.as_bytes()].concat();
        let path = RelPath::new(whole.clone(), String::from_utf8_lossy(&whole).into_owned());
        let snapshot = Snapshot {
            case_bytes: path_keys::case_fold(&whole),
            case_key: path_keys::display_fold(&path.rel),
            hash: "h".to_string(),
            mtime: 10,
            rel: path.rel.clone(),
            rel_bytes: whole,
            file_id: None,
            size: 4,
        };
        (path, snapshot)
    }

    /// A live row for one twin's note — file plus the note hanging off it, which is what makes it chargeable.
    fn plant_twin_note(db: &Db, dir: &[u8], name: &str) {
        let (path, snapshot) = twin_row(dir, name);
        upsert_file(db, &snapshot, &Path::new("/ws").join(&path.rel), 1)
            .expect("test setup: registry row for a twin");
        db::insert_note(db, &path, "t", "filename", "utf-8", 3, "body")
            .expect("test setup: note row for a twin");
    }

    /// The same twin as [`apply_changes`] sees it: text in hand when this scan read it, or the empty-digest
    /// marker a failed read leaves behind (see [`super::read_one`]).
    fn twin_found(dir: &[u8], name: &str, read: bool) -> Found {
        let (_, mut snapshot) = twin_row(dir, name);
        if !read {
            snapshot.hash.clear();
        }
        Found {
            abs: Path::new("/ws").join(&snapshot.rel),
            is_markdown: true,
            lossy: true,
            text: read.then(|| "# Twin\n".to_string()),
            snapshot,
        }
    }

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
