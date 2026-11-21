//! The one pass that turns a workspace folder into a registry, note rows and a catalog.
//!
//! [`scan`] is the only place in the crate that reads files, writes SQLite and renders the catalog at
//! once, so it owns the ordering rules the individual stages cannot express on their own:
//!
//! - **One transaction for the database, files written after it commits.** The workspace row, the note
//!   row and everything derived from its text either all move to this scan's state or none of them do.
//!   The registry line in `config.rs`'s database sits outside that set on purpose: written on a second connection
//!   after [`crate::db`] commits, so a registry failure exits 1 over work that is already durable and the next scan
//!   reconciles it. `INDEX.md` cannot join a SQLite transaction, so the catalog is rendered inside that transaction,
//!   its name list is committed with it, and only then does the filesystem work start. A scan that dies
//!   halfway leaves the previous catalog readable instead of half-rewritten, and never leaves a delete
//!   list that lags the files on disk (see `set_index_list` and `write_index_files` for the order).
//! - **Unknown is not absent.** A directory the walk could not enter, a file that vanished or was
//!   rewritten between the walk and the read, and a name whose byte spelling cannot be held, are named in
//!   [`ScanReport::skipped`] and keep the row they already have. Reporting any of them
//!   as `Removed` would stale live notes because of a permission error, a race, or a name this program
//!   refuses to guess at.
//! - **Parse rules reach notes that did not change.** Byte comparison cannot see a rule change, so
//!   `meta` records the parse rules version the rows were written with and a scan that finds a different
//!   value re-parses every markdown note once (see [`PARSE_VERSION_KEY`]). Classification stays the
//!   truth about the *files*: such a scan still reports its notes as unchanged.
//! - **The catalog reads live rows only.** Entries and [`ScanReport::notes`] come from
//!   `files.state = 'ok'`; [`crate::db::note_counts`] stays the unfiltered database truth, because a
//!   stale note is hidden from the catalog, not forgotten.
//! - **`INDEX.md` and its shards are the only names this module creates, and it deletes only the ones
//!   that open with its own generated header**: a name alone never licenses a removal — with **one exception,
//!   our own temporaries**. A leftover `<generated>.tmp` from a killed write is cleared by `clear_temp_corpses`
//!   on the naming rule alone, because that spelling is one nothing else produces and a half-written temporary
//!   has no complete header to fingerprint — a naming-trust decision, stated so the rule above can be read
//!   literally. The
//!   names themselves come from [`crate::index_names`], which owns their spelling; what this module owns is the
//!   fingerprint deciding which of them are ours to replace. Every write goes through a temporary file
//!   plus `rename`, and no note body is ever opened for writing.

use std::io::{self};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use thiserror::Error;

use crate::catalog::{self};
use crate::change::classify_with_identities;
use crate::chunk::CHUNKER_VERSION;
use crate::config::{abs_path, Config, ConfigError, WorkspaceRegistry};
use crate::db::{self, Db, DbError};
use crate::hash::Snapshot;

use crate::parse::PARSE_VERSION;

use crate::scan::corpus::catalog_entries;
use crate::scan::corpus::unresolved_links;
use crate::scan::found::known_rows;
use crate::scan::found::read_found;
use crate::scan::found::skipped_lines;
use crate::scan::found::strip_marked_removals;
use crate::scan::found::unresolved_ambiguity;
use crate::scan::publish::clear_temp_corpses;
use crate::scan::publish::set_index_list;
use crate::scan::publish::write_index_files;
use crate::scan::publish::NAME_CAP;
use crate::scan::state::chunker_refresh_needed;
use crate::scan::state::chunker_version;
use crate::scan::state::duplicated_rels;
use crate::scan::state::index_list;
use crate::scan::state::parse_version;
use crate::scan::state::record_unindexable;
use crate::scan::state::refresh_covered;
use crate::scan::state::refresh_needed;
use crate::scan::state::report_chunk_gap;
use crate::scan::state::report_refresh;
use crate::scan::state::report_refresh_gap;
use crate::scan::state::set_meta;
use crate::scan::state::unix_now;
use crate::scan::write::apply_changes;
use crate::scan::write::store_note_links;
use crate::scan::write::WritePass;
use crate::walk::{self, UnknownPath, WalkOptions};

#[cfg(test)]
mod tests;

mod corpus;
mod found;
mod publish;
mod state;
mod write;

pub use self::state::{
    indexed, last_scan, EMBED_VERSION_KEY, LAST_SCAN_KEY, PARSE_VERSION_KEY, UNINDEXABLE_KEY,
};
pub use self::write::ChunkTally;

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

/// How many names [`ScanReport::attachment_examples`] may hold.
///
/// Five because it is what a summary line can carry without turning into a file listing, and because the number
/// beside it answers the question a caller actually asks. The count is never capped with the names.
pub const ATTACHMENT_EXAMPLE_LIMIT: usize = 5;

/// What one scan did, for the CLI to print and `--json` to serialise.
#[derive(Debug, Serialize)]
pub struct ScanReport {
    /// Paths that appeared since the last scan.
    pub added: usize,
    /// Paths whose contents changed.
    pub modified: usize,
    /// Paths whose contents travelled from another path in the same workspace.
    pub moved: usize,
    /// Paths that are gone; their rows are marked stale, never deleted.
    pub removed: usize,
    /// Paths the registry already had exactly as they are.
    pub unchanged: usize,
    /// Paths this scan could not index and left untouched: directories it could not read, files that
    /// vanished or changed between walk and read, paths whose identity is already taken, a name the byte
    /// spelling cannot hold, a catalog temporary it could not remove (see `clear_temp_corpses`), and a root
    /// entry wearing a generated catalog name that this program did not publish — left on disk and named here
    /// instead of deleted.
    /// Sorted and deduplicated by the path's **bytes**, printed as display text. Where the keys differ, two
    /// paths print two lines that look alike, because the report can name them no differently — they are not a
    /// duplicate to clean up later, and collapsing them would hide one lost path.
    ///
    /// **The next sentence in this doc comment was false for six days.** It said that where the key is itself lossy —
    /// "a Windows refusal, whose identity bytes come from `to_string_lossy`" — two names share a single line, and that
    /// `crates/np-core/tests/walk.rs` pins the behaviour. A later change replaced that byte spelling with
    /// [`crate::path_keys::wtf8`], so a Windows refusal's identity bytes carry the lone surrogate itself and two such
    /// names produce **two** lines here; `tests/walk.rs` now pins those six bytes instead, on the Windows row that first
    /// ran and caught the stale pair of them. A key that is genuinely lossy still exists — a row
    /// whose key migration 4 derived from display text, or an entry that only ever existed as text and reached the report
    /// through `UnknownPath::from_display` (private, so named rather than linked) — and *those* collapse, which is what the sentence
    /// should have been about.
    ///
    /// So this list is a floor on what the scan lost, never a count of it.
    pub skipped: Vec<String>,
    /// Live note rows — the same number of entries the catalog holds.
    pub notes: i64,
    /// Live tracked files that are **not** notes: every `.txt`, `.org`, `.png` and `.pdf` the walk
    /// found, hashed and listed but never parsed, so search cannot reach any of them.
    ///
    /// This exists because the distinction was otherwise invisible from outside. A front end that read `added: 5`
    /// and printed "5 files" was describing a vault whose writing its own product could only ever find in one of
    /// them, while `unindexable` stayed (correctly) `null` — nothing was lost, these files were simply never notes.
    /// With this key the sentence is np's to make and the caller's to print.
    ///
    /// `notes + attachments` is the count of live `files` rows: the two halves of what a scan tracks, with no third
    /// bucket. Unlike [`Self::added`] this describes the **workspace**, like [`Self::notes`], not this run's delta.
    pub attachments: i64,
    /// Up to [`ATTACHMENT_EXAMPLE_LIMIT`] of [`Self::attachments`], sorted by path bytes and printed as display
    /// text — names, not a sample chosen for variety. Capped because the list is "everything else in
    /// the folder": a vault holding 300 PDFs pays for five strings per scan rather than three hundred, and
    /// [`Self::attachments`] stays exact whatever the cap prints.
    pub attachment_examples: Vec<String>,
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
    /// **The question this field exists to answer.** [`Self::upgrade_orphans`] answers "what did this run
    /// delete" and is quiet from then on; someone asking a month later had to open `sqlite3`. This field is that record
    /// read back through `np scan`, so the durable receipt stops being readable only by whoever brings a database shell.
    /// It changes no announcement, clears nothing, and prints nothing new on stdout: an old fact stays available without
    /// any run becoming louder for it, which is the balance this field keeps.
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
    /// `None` when every count is zero, so a workspace that lost nothing keeps a clean report.
    ///
    /// **Null, not absent**, exactly like [`Self::upgrade_orphans`]: the JSON key is there on every scan
    /// whatever the outcome, because a reader must be able to tell "nothing was lost" apart from "this build
    /// stopped reporting it", and every reader would otherwise grow a missing-key branch to answer a question
    /// `null` already answered.
    ///
    /// The durable copy is `meta`'s [`UNINDEXABLE_KEY`] value, written after this scan's commit and overwritten
    /// by the next one. This field is not read back from it, and nothing that *decides* anything reads either:
    /// unknown-is-not-absent stays settled by the walk's own lists, so a refused record write
    /// degrades the diagnosis and never changes what is indexed.
    pub unindexable: Option<Unindexable>,
    /// Wall-clock milliseconds for the whole scan. Informational only — never in a generated file,
    /// where it would make every regeneration differ.
    pub elapsed_ms: u128,
}

/// What one scan could not index, and what its registry duplicates, counted by kind.
///
/// This exists because [`ScanReport::skipped`] cannot carry it: one merged list names an unreadable
/// directory and a name the byte spelling refuses identically, so even the live line cannot tell a
/// permission problem from an unkeyable name. The list itself keeps its entries **verbatim** —
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
    /// Names the byte spelling cannot hold — the count [`Self::unreadable`] refuses to be confused
    /// with, because this one is fixed by renaming a file. Windows-only in practice: `os_bytes` is exact on
    /// Unix, so no scan run there ever raises this above zero (see [`crate::walk::Walked::unrepresentable`]).
    pub unkeyable: usize,
    /// Display paths the registry holds **more than one live (`state = 'ok'`) row** for.
    ///
    /// **This is a question, not an accusation.** Two genuinely different files can share a display spelling —
    /// latin-1 `caf\xE9/note.md` beside `caf\xE8/note.md`, both readable, both correctly indexed — and they land
    /// here exactly as the one-file-two-rows case does. It also counts attachments, which have no catalog entry at
    /// all. So read it as "look at these paths" and confirm with `sqlite3` before deleting anything.
    ///
    /// It used to promise to be "deliberately unexplained"; [`Self::ambiguous`] beside it now names one possible
    /// cause, so this count stays what it is — a pointer at paths — and the reader who wants the reason reads the
    /// other field.
    pub duplicates: usize,
    /// Notes whose registry row is **keyed from display text** and could not be settled this scan: the walk
    /// could not read their own path, and no listed readable path both spells them exactly and matches their stored
    /// hash, so nothing could say which byte spelling the row belongs to.
    ///
    /// `N` is exactly the population that holds [`PARSE_VERSION_KEY`] back — this field is rendered *from* that
    /// count and never read back into a deletion or version decision, the same rule this report follows elsewhere: ask a day later
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
    /// lossy spelling and then two entries look alike, exactly as two `skipped` lines do. It says
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
    /// "this workspace was never scanned by a build that looks."
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
/// write pass ends, and because a hand-built list is the only way to exercise the refusal arm on a
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
/// [`Config::workspace_db`]. See the module docs for the rules that hold it together, and
/// [`scan_with_inject`] for the seam its race test drives.
///
/// A scan does not fail because of one bad file: an unreadable directory, a note with latin-1 bytes, a
/// zero-byte file and a frontmatter-only note all index, and all get a catalog entry (
/// 4). What does fail is a workspace that cannot be opened at all, an unusable `data_dir`, and a catalog
/// file that cannot be written.
pub fn scan(root: &Path, cfg: &Config) -> Result<ScanReport, ScanError> {
    let mut chunks = ChunkTally::default();
    scan_inner(root, cfg, |_| {}, &mut chunks)
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
    // one folder is one workspace however it was typed.
    let root = abs_path(root);
    // **The phase reports itself** (diagnostics design point 7): one span for the whole pass, so every event inside it
    // inherits the workspace and its close says what the scan cost. The name is `scan` because that is the word the CLI,
    // `np setup`'s step line and the manual all use; the target is the published one rather than `module_path!()`, so
    // renaming this file cannot silently rename a filter somebody wrote.
    let _span = tracing::info_span!(
        target: crate::diagnostics::target::SCAN,
        "scan",
        workspace = %root.display()
    )
    .entered();
    // Before the walk, not on the way out: a corpse the walk lists is a file this scan would index as an
    // attachment (see [`clear_temp_corpses`]). One that will not be removed is reported and retried by
    // the next scan; it does not end this one.
    // Carried as [`UnknownPath`] pairs to the end of the scan, where they are deduped and ordered by bytes and
    // only then printed: a name that came off the disk keeps its real bytes all the way to the report.
    let mut skipped: Vec<UnknownPath> = clear_temp_corpses(&root)?
        .into_iter()
        .map(UnknownPath::from_display)
        .collect();
    let walked = walk::walk(&root, &WalkOptions::default())?;
    after_walk(&root);

    // A directory whose contents the walk did not read holds files that are unknown rather than absent — it
    // could not be entered, or its **own name** was refused, which hides the same subtree. Their known
    // rows stay out of the comparison (see [`known_rows`]) and the directory itself is what the report names.
    // A refused *file* name is the same rule about one path instead of a region: it is not indexed this scan,
    // its row keeps what it holds, and the path is named here too.
    skipped.extend(walked.unreadable.iter().cloned());
    skipped.extend(walked.unrepresentable.iter().cloned());
    // **One event per hidden subtree**. The report merges both lists into one `skipped`
    // array, which is the right shape for a summary line and the wrong one for a diagnosis: "could not enter" and
    // "cannot spell the name" have different remedies — one is a mount or a chmod, the other is a filename this build
    // will refuse forever. `what` is the display spelling, because these paths are exactly what the report prints.
    for dir in &walked.unreadable {
        tracing::debug!(
            target: crate::diagnostics::target::SCAN,
            rule = crate::diagnostics::rule::UNREADABLE,
            what = %dir.display,
            "directory contents were not read this scan"
        );
    }
    for refused in &walked.unrepresentable {
        tracing::debug!(
            target: crate::diagnostics::target::SCAN,
            rule = crate::diagnostics::rule::UNREPRESENTABLE_PATH,
            what = %refused.display,
            "the name cannot be keyed losslessly, so nothing under it is indexed"
        );
    }
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
    // edited since an older build ran would keep its old derived rows forever.
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
    // An ambiguous row is marked rather than deleted, which puts two decisions between
    // classification and the write pass: a `Removed` naming an admitted marked key is dropped (this is
    // the arm that used to live inside `known_rows` as a deletion, and the safety property says it must
    // never stale a note nobody re-read), and what is left unresolved becomes the second source of the
    // charge the counters above.
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
    // answer serves both versions, because the chunk rule reuses `refresh_covered`'s two sources **unchanged**:
    // a note
    // that escaped this scan escaped chunking too, and inventing a second coverage rule for chunks would be
    // a second thing to keep honest for no extra fact.
    let covered = refresh_covered(&written, hidden);
    let record_version = refresh && covered;
    // **Provider availability is not version debt.** Nothing above or below
    // this line asks whether an embedder exists, and that is the whole decision: if recording waited for a
    // provider, then with `embeddings.provider = "none"` — the default, what every install runs until
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
    // Read beside `notes`, from the same predicate as its names, so the two counts describe one walk.
    let attachments = db::attachments(&db, ATTACHMENT_EXAMPLE_LIMIT)?;
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
    // what this scan leaves behind rather than what it found: a duplicate pair is *created* by a write pass.
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
        // by a name this build refuses or an unmerged duplicate pair, and no line is written about it on any
        // scan, so a coverage gap nobody is told about is indistinguishable from a corpus that was never cut.
        // The case for recording anyway rests on that gap being bad; this line is what makes it visible.
        report_chunk_gap(&mut io::stderr(), written.uncovered + hidden);
    }

    // The durable record of what this scan could not index, written **after** the commit it describes for the
    // same reason the report lines above wait for it: a scan that rolled itself back has nothing to announce.
    // Written on every scan, including one that lost nothing, because the record is current state and an old
    // value left standing is a false accusation.
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
        attachments: attachments.count,
        attachment_examples: attachments.examples,
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
