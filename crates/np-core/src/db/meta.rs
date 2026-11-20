use crate::db::DbError;

use rusqlite::{params, Connection, Transaction};

/// The receipts of the migration steps **this** open ran, one field per step that writes one.
///
/// A struct rather than two positional `Option<String>`s on [`Db`] because the two values are the same type
/// and mean different things: swapped, every test around either announcement would stay green while the
/// scan reported a merge as an orphan cleanup. Default is "this open migrated nothing", which is every
/// ordinary open.
#[derive(Debug, Default)]
pub(crate) struct Carried {
    /// The [`FK_ORPHANS_KEY`] receipt, when this open ran migration 3.
    pub(crate) orphans: Option<String>,
    /// The [`NOTE_MERGE_KEY`] receipt, when this open ran migration 5.
    pub(crate) merges: Option<String>,
}

/// One `meta` value, or `None` when the key was never written.
///
/// [`read_receipt`] is the migration-time reader and refuses to tolerate a missing key — inside a transaction, an
/// absent receipt means a step that should have written one did not, and that is a bug worth a failed open. This is the
/// opposite question: an absent key is an ordinary state of a file that never ran that step, so `QueryReturnedNoRows`
/// is an answer rather than a failure. Keeping both spellings is what stops one of them being "fixed" into the other.
pub(crate) fn stored_receipt(conn: &Connection, key: &str) -> Result<Option<String>, DbError> {
    match conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        // `Option<String>` on purpose: a `meta` row holding SQL NULL is a state this program already tolerates
        // elsewhere (`a_null_meta_value_is_read_as_no_label_rather_than_failing_the_scan`), and a receipt reader that
        // asked for `String` would turn one into a failed scan. Absent key and null value arrive as the same honest
        // answer — there is no record to report — while an empty string stays distinguishable from both.
        |row| row.get::<_, Option<String>>(0),
    ) {
        // A SQL NULL arrives here as `None` because the column is read as `Option<String>`, which is the whole point of
        // that choice: no second arm, and no InvalidColumnType on somebody's hand-edited row.
        Ok(value) => Ok(value),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(DbError::from(err)),
    }
}

/// One receipt value as its own step wrote it, read back from `meta` inside that step's transaction.
///
/// A missing row is an error and not a `None`: the step whose version [`apply_migrations`] matched promised
/// to write this key, so a silent absence means the receipt — and with it the announcement, and the proof
/// the check ran — never reached the file.
pub(crate) fn read_receipt(tx: &Transaction, key: &str) -> Result<String, DbError> {
    Ok(tx.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )?)
}

/// `meta` key recording what migration 3 removed to make its constraints possible.
///
/// `meta` is where this program already keeps the workspace bookkeeping a person may need in order to
/// explain itself — [`mod@crate::scan`] records the published index files there — so the receipt needs no
/// new channel: there is no logger to write to, and returning the counts from `Db::open` would mean
/// plumbing a migration detail through every caller that opens a database. The value is one labelled
/// count per shape, `headings=…,tags=…,items=…,links_src=…,links_dst_null=…`, written even when they
/// are all zero: "looked and found none" is worth telling apart from "this file never upgraded".
pub const FK_ORPHANS_KEY: &str = "fk_orphans";

/// `meta` key recording how many migrated rows carry a lossy display spelling.
///
/// Beside [`FK_ORPHANS_KEY`] for the same reason: a person reading a scan report afterwards has to
/// be able to tell "this file was upgraded and held two such paths" apart from "this file was never
/// upgraded", and there is no logger to write to. The value is `files=N,notes=M`, written **even when both
/// are zero** — the count's whole use is that it distinguishes looked-and-found-none from never-looked, and
/// the first byte-keyed scan's expected churn can only be read against it.
pub const PATHS_LOSSY_KEY: &str = "paths_lossy";

/// `meta` key recording what migration 5 merged, so the deletion has a record beside the one migration 3
/// keeps for its own.
///
/// Beside [`FK_ORPHANS_KEY`] and [`PATHS_LOSSY_KEY`] for the same reason: no logger exists, and a person
/// reading a scan report weeks later has to be able to tell "this file held a duplicate pair and it was
/// folded away" apart from "this file never had one". The value is `merged=N;paths=a.md,b.md`, written
/// **even when nothing was merged** — `merged=0` is the record that the check ran, and N
/// counts the note rows folded into a survivor rather than the number of paths, because a path holding three
/// rows lost two of them.
pub const NOTE_MERGE_KEY: &str = "note_merges";
