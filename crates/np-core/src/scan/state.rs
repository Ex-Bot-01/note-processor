use rusqlite::{params, OptionalExtension};
use std::io;

use crate::chunk::CHUNKER_VERSION;

use crate::db::{Db, DbError};

use crate::parse::PARSE_VERSION;

use crate::scan::found::Written;

/// `meta` key holding the index files this program has published, newline-separated. The previous list
/// is what lets a rescan delete the shards it no longer produces.
pub(crate) const INDEX_FILES_KEY: &str = "index_files";

/// `meta` key holding the unix seconds of the last scan that **committed** for this workspace.
pub const LAST_SCAN_KEY: &str = "last_scan_unix";

/// `meta` key holding the value of [`crate::parse::PARSE_VERSION`] that this workspace's derived rows
/// were written with. Absent means "written before this mechanism existed", which is not a version to
/// compare against and therefore refreshes; see the comparison in `refresh_needed`.
pub const PARSE_VERSION_KEY: &str = "parse_version";

/// `meta` key holding [`crate::chunk::CHUNKER_VERSION`] for the workspace's `chunks` rows — what other documents
/// call `EMBED_VERSION`. There is deliberately no second constant beside `CHUNKER_VERSION`: the only definition this
/// key could describe is the chunker that cut those rows, and two names for one fact eventually disagree in the file.
///
/// **Chunker coverage only.** The record answers "did this build's chunking reach every note it owed", and nothing
/// else. Provider availability is not version debt — the reasoning sits on `record_chunk_version`, where it is taken:
/// with the default `embeddings.provider = "none"` the key is
/// recorded anyway, because a withheld key would be a coverage gap no user can clear.
pub const EMBED_VERSION_KEY: &str = "embed_version";

/// `meta` key holding what the **last scan** of this workspace could not index, beside what its registry
/// currently duplicates.
///
/// Beside [`crate::db::FK_ORPHANS_KEY`] and [`crate::db::PATHS_LOSSY_KEY`] for the same reason: a refusal is a fact
/// with no afterlife otherwise — one line on stderr at the moment it happened, gone when the terminal closes, while
/// the note stays missing from the catalog until somebody renames the file. There is no logger to write to and the
/// catalog's bytes may not move, so `meta` is where a person reading the workspace later can ask what was lost.
///
/// The value is `unreadable=N;unkeyable=N;duplicates=N;names=a,b`, written **even when every count is
/// zero**: this record describes the *last scan* rather than the history of the file, so a fixed workspace
/// has to overwrite an old complaint instead of leaving it standing there forever. `names` holds
/// up to eight display spellings joined by `,` — see [`crate::scan::Unindexable::names`] for their order — and a path
/// holding a comma reads as two entries, because this is a diagnosis for a human at an `sqlite3` prompt and
/// not a format to parse. Nothing decides anything with it: no deletion rule reads this key.
pub const UNINDEXABLE_KEY: &str = "unindexable";

/// The index files this workspace holds from the last scan, from [`INDEX_FILES_KEY`].
pub(crate) fn index_list(db: &Db) -> Result<Vec<String>, DbError> {
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
pub(crate) fn unix_now() -> i64 {
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

/// Whether this workspace has an index at all — the reading that tells **"nothing here matches"** apart from
/// **"nobody has ever looked"**.
///
/// Why a reader needs it: `search` answers an empty list in both worlds, and an application holding an empty
/// list can only say *no results*. A person who just pointed a GUI at a folder they have never indexed reads
/// that as a broken search; the one-click fix — index it — is invisible. The principle is settled for the
/// neighbouring state already: a degraded answer says which half was missing rather than going quiet. This is
/// the same fact about a different half: the workspace itself.
///
/// Two probes, in this order:
///
/// 1. [`LAST_SCAN_KEY`], written inside the scan's own transaction — so a scan that rolled back cannot leave a
///    stamp claiming it committed, and a scan that found **no notes at all** writes one: an empty vault is
///    indexed, which is not the same sentence as never scanned, and conflating them would send someone to
///    `np scan` a folder that has nothing to index.
/// 2. The [`crate::db`] `files` table, for a database written by an `np` older than the stamp. Those rows are
///    real and re-scanning them is work, so answering "not indexed" about them would be a lie with a cost.
///
/// **Reads only.** No schema write, no directory, no row: like `status.get`'s `last_scan`, this is asked on
/// paths that must not mint an index for a vault that does not exist (`search` already opens the database by
/// the time it asks, which is why probe 1 rather than "does the file exist" is the signal — an unscanned
/// search leaves an empty database behind, and a key that turns on file existence would answer `true` there).
pub fn indexed(db: &Db) -> Result<bool, DbError> {
    if last_scan(db)?.is_some() {
        return Ok(true);
    }
    // **Read the projected value, not rusqlite's `exists()`** — the same trap `search::any_vectors_exist`
    // documents: that helper answers "did the query return rows", and a `SELECT EXISTS(...)` projection always
    // returns one row, so it would answer `true` for every empty workspace.
    let mut statement = db.0.prepare_cached("SELECT EXISTS(SELECT 1 FROM files)")?;
    let rows: i64 = statement.query_row([], |row| row.get(0))?;
    Ok(rows != 0)
}

pub(crate) fn set_meta(db: &Db, key: &str, value: &str) -> Result<(), DbError> {
    db.0.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// The display paths the registry holds **more than one live row** for — counted, never explained.
///
/// Grouped by `rel`, the display text, because that is what a person notices: two byte-distinct paths sharing one
/// lossy spelling produce two alike-looking lines in `INDEX.md`. **That is also why the count cannot name a cause.**
/// Two different files whose names collide under lossy decoding are legitimate, correctly indexed, and
/// indistinguishable here from a duplicated note — and an attachment sharing a display spelling with a note
/// counts too, though it has no catalog entry at all. Narrowing this with `key_is_display_derived` would not help:
/// it flags both ambiguous configurations identically (nothing separates them) while staying silent on Windows,
/// where every lossy key qualifies.
///
/// `state = 'ok'` keeps stale rows out; a note whose file went away is not a path that appears twice. Sorted by the
/// same text, so one workspace produces one record.
///
/// Collected in full before returning, like every other read here, so no statement is left mid-iteration
/// across a write.
pub(crate) fn duplicated_rels(db: &Db) -> Result<Vec<String>, DbError> {
    let mut statement = db.0.prepare(
        "SELECT rel FROM files WHERE state = 'ok' GROUP BY rel HAVING COUNT(*) > 1 ORDER BY rel",
    )?;
    let rels = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rels)
}

/// Record what this scan lost, **after the transaction commit and best-effort**.
///
/// "After the commit" is not "after the scan finished": publishing the index files and recording the workspace
/// row come later and can still fail the run. The record is therefore accurate about the *committed rows* it
/// describes, and a scan that dies after this point leaves a record of work that really did land — which is the
/// lesser error, since the alternative is describing nothing at all.
///
/// The result is dropped for the reason [`crate::db::report_orphans`] gives for its own writes: by the time
/// this runs the scan's rows and catalog are committed, so a `meta` write that refuses — a full disk, a lock another
/// process holds past the timeout, a trigger — costs this workspace its record and nothing else. Returning
/// `Err` would report a failed scan whose rows and catalog are already durable, and `.expect` would do worse:
/// a panic here throws away a committed scan, which is the failure this call exists not to add. The loss
/// still reaches this run's report field and its stdout line; only its afterlife is degraded.
pub(crate) fn record_unindexable(db: &Db, value: &str) {
    let _ = set_meta(db, UNINDEXABLE_KEY, value);
}

/// The parse rules version this workspace recorded under [`PARSE_VERSION_KEY`].
///
/// `None` covers "the key is absent" and "the value is not a number this build can read" — a NULL, a
/// hand-edited file, a spelling from a future version. Both mean the same thing here: the stored rows
/// carry no label this build can trust, so they are rewritten and the truth recorded.
pub(crate) fn parse_version(db: &Db) -> Result<Option<u32>, DbError> {
    Ok(meta_value(db, PARSE_VERSION_KEY)?.and_then(|value| value.trim().parse().ok()))
}

/// Whether this scan has to re-parse every markdown note it reads.
///
/// Equality with [`PARSE_VERSION`] and nothing else means up to date. A database holding a *higher* value
/// is rewritten here too, and the recorded value moves back to what this build actually wrote: guessing
/// that newer rows are still valid would leave data labelled with rules this binary does not have, and one
/// full re-parse is cheap next to a catalog that quietly disagrees with the code that renders it.
pub(crate) fn refresh_needed(stored: Option<u32>) -> bool {
    stored != Some(PARSE_VERSION)
}

/// The chunker version this workspace recorded under [`EMBED_VERSION_KEY`].
///
/// `None` covers an absent key and a value this build cannot read, exactly as [`parse_version`] does: both
/// mean the rows carry no label worth trusting. Note what this number does **not** decide — which notes to
/// re-cut. That is per row, from `chunks.chunker` and `chunks.source_hash`, via [`db::chunk_state`]; this key
/// is the durable statement that the pass reached everything it owed.
pub(crate) fn chunker_version(db: &Db) -> Result<Option<i64>, DbError> {
    Ok(meta_value(db, EMBED_VERSION_KEY)?.and_then(|value| value.trim().parse().ok()))
}

/// Whether this scan has to record [`CHUNKER_VERSION`] afresh.
///
/// Equality and nothing else means up to date; a *higher* stored value is rewritten too, for the reason
/// [`refresh_needed`] gives. Unlike the parse comparison this asks no work of its own: a row behind the
/// chunker is stale on its own account ([`db::chunk_state`] reports it) whether or not the key lags.
pub(crate) fn chunker_refresh_needed(stored: Option<i64>) -> bool {
    stored != Some(CHUNKER_VERSION)
}

/// Whether a refresh reached every note it owed new derived rows.
///
/// Two ways to miss one, neither visible in [`ScanReport`]: a file that changed or became unreadable
/// between the walk and the read (`uncovered`, since the write pass skips its marker), and a live row this
/// scan could not look at — under a directory it could not enter, or carrying a name it refused —
/// which [`known_rows`] drops so it cannot be mistaken for deleted (`hidden_by_unknown`). Recording the
/// version over either would label rows as current that an older build wrote, permanently — the exact
/// failure this rule exists to prevent, reintroduced by the fix.
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
/// live row hidden behind such a directory, **a name this build refuses for good** (while that spelling
/// stays unrepresentable no scan can index it — a refused *file* leaves its own rows owing, a refused
/// *directory* leaves everything below it owing — so they keep whatever an older build wrote and only renaming
/// the path clears the debt), a move refused by an identity conflict — judged by the path it came from, since
/// `classify` suppressed that path's `Removed` and its rows are still in the catalog — and **a duplicate note
/// pair** (`Indexed::Duplicate`). The unreadable ones clear when the obstruction does.
///
/// Of those, a refused name is the one that never clears on its own: no code renames a file, so such a
/// workspace re-parses its whole corpus on every scan and prints the gap line each time. That is chosen over
/// recording the version beside a note that was not re-parsed, which is that bug wearing a clean hat; the cost
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
/// [`Indexed::Duplicate`] — **two note rows on one byte key** — and no other duplicate. The ambiguous
/// duplicate is another shape: two live `files` **and** `notes` rows for one file behind a lossy display
/// spelling, and only the half where content still agrees is healed — a row whose **content changed while its
/// path was unindexable** has nothing to pair on, so the pair returns and this function keeps withholding the
/// version — permanently for a migration-keyed row, whose stored key
/// matches no listing; durable file identity is the named way through. "Duplicate
/// rows are healed" is therefore not a sentence to carry past here; "the ones this program can prove are one file are
/// healed" is.
pub(crate) fn refresh_covered(written: &Written, hidden_by_unknown: usize) -> bool {
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
pub(crate) fn report_refresh(out: &mut dyn io::Write, stored: Option<u32>, notes: usize) {
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
pub(crate) fn report_chunk_gap(out: &mut dyn io::Write, missed: usize) {
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

pub(crate) fn report_refresh_gap(out: &mut dyn io::Write, missed: usize) {
    if missed == 0 {
        return;
    }
    // Dropped for the same reason as [`report_refresh`]'s write.
    let _ = writeln!(
        out,
        "note-processor: parse rules v{PARSE_VERSION} not recorded — {missed} note(s) were not re-parsed this scan; the next scan will retry"
    );
}
