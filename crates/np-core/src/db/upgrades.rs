use crate::db::DbError;

use rusqlite::{params, Connection, OptionalExtension};
use std::io;

use crate::db::meta::NOTE_MERGE_KEY;
use crate::db::meta::PATHS_LOSSY_KEY;
use crate::db::migrations::assert_replacement_complete;
use crate::db::migrations::copy_files_rows;
use crate::db::migrations::first_violation;

/// Echo the stored orphan receipt to `out` — [`std::io::stderr`] in production — unless there is
/// nothing to say.
///
/// `receipt` is the [`crate::db::FK_ORPHANS_KEY`] value exactly as the migration wrote it, so this line, the scan
/// report's field and `np scan`'s summary line render **one** string rather than recomputing the news
/// three ways; the total beside it comes from [`orphan_total`], the same reader the summary uses.
///
/// The sink is a parameter rather than a hard-wired `eprintln!` because stderr cannot be captured
/// in-process here: the standard library offers no way to replace the global handle, and this runs
/// inside [`Db::open`], before any test could redirect descriptor 2. Taking the sink lets the unit
/// tests in this module pin both halves of the promise — one line naming the total and the stored
/// counts for a real cleanup, silence for an all-zero one — so a regression in this visibility path
/// fails a test instead of going unnoticed; what they cannot pin is that the production call site
/// names stderr, nor that it runs after the commit (see [`apply_migrations`]).
///
/// A failed write is dropped rather than raised. It used to `.expect`, from inside the
/// migration's own transaction, so a workspace opened with descriptor 2 refusing writes had its whole
/// upgrade rolled back — every time, forever, over one line nobody could read. That panic never
/// protected the signal: the same counts are written into `meta` under [`crate::db::FK_ORPHANS_KEY`] in that same
/// transaction, and the scan whose own open did the deleting echoes them
/// ([`crate::ScanReport::upgrade_orphans`]). An unprinted line is a lost courtesy; a refused upgrade is
/// the worse silence, because it hides the deletions *and* leaves the file unupgraded.
///
/// **One `np scan` run prints this sentence twice**, once here on stderr and again as the `upgrade`
/// line of its stdout summary (`crates/np-cli/src/main.rs`). That is deliberate, not a double-report
/// bug: this copy belongs to the run that did the deleting and appears even for a command that prints
/// no report, while the summary line belongs to the report a person is reading — and neither has to
/// arrive, because `meta` holds the fact either way.
pub(crate) fn report_orphans(out: &mut dyn io::Write, receipt: &str) {
    // The total is read out of the stored receipt rather than carried alongside it, so the stderr copy
    // cannot disagree with the two stdout renderings of the same value. All zeros stays silent: every
    // workspace that predates this cleanup passes through here, and "found nothing" on an ordinary upgrade
    // is not news.
    let total = orphan_total(receipt);
    if total == 0 {
        return;
    }
    // The result is dropped on purpose, for the reason in this function's doc — this is the echo, and
    // the durable copy is committed in `meta` before this call is reached.
    let _ = writeln!(
        out,
        "note-processor: removed {} orphan row(s) while adding note foreign keys ({})",
        total, receipt,
    );
}

/// How many rows a stored [`crate::db::FK_ORPHANS_KEY`] receipt announces: the sum of its labelled counts.
///
/// The reader that pairs with `OrphanCleanup::value` (a code span, not a link: that type is private and
/// rustdoc warns when public docs link to it), for every caller left holding only the stored
/// string — the scan report, `np scan`'s summary line, and the stderr echo in `report_orphans` (a code
/// span, not a link: that function is private), each of which names the total beside the counts.
/// Every number in the value is summed whatever its label: the spelling is this module's own
/// and its labels are pinned by the tests around [`crate::db::FK_ORPHANS_KEY`], so there is nothing here to
/// validate — a fragment that will not parse as a count adds nothing rather than failing a report.
pub fn orphan_total(value: &str) -> usize {
    value
        .split(',')
        .filter_map(|part| match part.split_once('=') {
            Some((_, count)) => count.parse::<usize>().ok(),
            None => None,
        })
        .sum()
}

/// What [`add_note_foreign_keys`] had to remove, kept apart by shape because a person reading the
/// receipt wants to know which table an older build was leaving rows in.
pub(crate) struct OrphanCleanup {
    pub(crate) headings: usize,
    pub(crate) tags: usize,
    pub(crate) items: usize,
    /// Rows deleted: their source note no longer exists, so there is no sentence left to keep.
    pub(crate) links_src: usize,
    /// Rows NULLed rather than deleted: the edge stays, its destination goes.
    pub(crate) links_dst_null: usize,
}

impl OrphanCleanup {
    /// The stored spelling — the labels of [`crate::db::FK_ORPHANS_KEY`], in the order the tables are rebuilt.
    pub(crate) fn value(&self) -> String {
        format!(
            "headings={},tags={},items={},links_src={},links_dst_null={}",
            self.headings, self.tags, self.items, self.links_src, self.links_dst_null
        )
    }
}

/// Delete the rows of `table` whose `column` names no note, returning how many went.
///
/// Both names come from the call sites in this module and never from a caller or a file, so writing the
/// statement out with `format!` is safe. `NOT IN` is exact here because `notes.id` is an `INTEGER
/// PRIMARY KEY` and can never be NULL; against a nullable column the same predicate would match nothing
/// at all.
pub(crate) fn detach_orphans(
    conn: &Connection,
    table: &str,
    column: &str,
) -> Result<usize, DbError> {
    Ok(conn.execute(
        &format!("DELETE FROM {table} WHERE {column} NOT IN (SELECT id FROM notes)"),
        [],
    )?)
}

/// NULL the destinations that name no note, returning how many changed.
///
/// Not a delete: an edge whose target disappeared is still a sentence somebody wrote, and
/// [`note_counts`] counts a NULL destination as unresolved. Leaving them in place would fail the check
/// at the end of [`add_note_foreign_keys`], which is why they are repaired rather than kept.
pub(crate) fn unlink_destinations(conn: &Connection) -> Result<usize, DbError> {
    Ok(conn.execute(
        "UPDATE links SET dst_note_id = NULL
          WHERE dst_note_id IS NOT NULL AND dst_note_id NOT IN (SELECT id FROM notes)",
        [],
    )?)
}

/// Migration 4 — `files` and `notes` are rebuilt so their identity lives in bytes.
///
/// Why bytes: a path is an OS byte string, `rel` was stored as TEXT, and SQLite stores anything that is not
/// valid UTF-8 as the replacement character. Two files named `caf\xE9.md` and `caf\xE8.md` therefore became
/// one row whose `rel` matched no file on disk — and because `abs` was a *derived* column, the real path was
/// not recoverable from the row either. The new `rel_bytes` / `case_bytes` pair is the identity;
/// `rel`, `case_key` and `abs` stay, become display text, and keep being written exactly as before, so every
/// reader that still matches on them keeps working until the round that moves them.
///
/// The shape follows [`add_note_foreign_keys`] — stage, copy, drop, rename, recreate indexes, gate on
/// `PRAGMA foreign_key_check` inside the transaction — with three differences this step cannot do without:
///
/// - **It reads `PRAGMA foreign_keys` and refuses when it is on** ([`DbError::EnforcementOn`]), rather than
///   assuming [`migrate`] turned it off. With enforcement on, `DROP TABLE notes` implicitly deletes the row,
///   which cascades into four child tables; migration 3's own rebuilds are why that promise exists 200 lines
///   away, and a step that destroys a workspace on someone else's assumption is not a step.
/// - **The `files` copy is a loop over rowids**, not an `INSERT … SELECT`: there is no `id` column to select
///   today (`rel TEXT PRIMARY KEY` on a rowid table is a unique index, not a rowid alias), and `case_bytes`
///   is a per-row Rust fold that one bound parameter would stamp identically onto every row. Every row's own
///   `rowid` becomes its new `id`, so addresses do not move.
/// - **A row count is asserted before each `DROP`** ([`DbError::CopyShrink`]), because the check below cannot
///   see a lost row that has no children.
///
/// Statement order matters twice over. Both renames happen **before** `PRAGMA foreign_key_check`, because
/// `notes_new`'s key names `files` — a table that does not exist between the drop and the rename, so an
/// earlier check reports a missing parent rather than a broken row. And indexes are recreated after both
/// renames, since dropping a table takes its indexes with it.
///
/// Byte stability, on top of [`rebuild_with_keys`]'s `ORDER BY rowid`: `notes.id` is carried, never re-minted,
/// because every derived row hangs off it. Only these two tables are rebuilt — `tags` is untouched, so no tag
/// rowid moves and no published byte in `INDEX.md` can shift.
pub(crate) fn byte_path_keys(conn: &Connection) -> Result<(), DbError> {
    // Before anything is written: this pragma cannot be changed inside a transaction, so the step cannot fix
    // what it finds — only refuse it. `PRAGMA foreign_keys;` answers through a query, not `execute_batch`,
    // which discards result rows.
    let enforcement: i64 = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    if enforcement != 0 {
        return Err(DbError::EnforcementOn { value: enforcement });
    }

    // `rel` and `abs` lose their constraints and keep their values: from here they are display text, derived
    // from the byte key rather than being it. `case_key` survives without `UNIQUE` — twins share a display
    // fold by definition, so uniqueness belongs to `case_bytes` alone — but keeps its index, because link
    // resolution still reads it.
    conn.execute_batch(
        "CREATE TABLE files_new (
            id         INTEGER PRIMARY KEY,
            rel_bytes  BLOB NOT NULL UNIQUE CHECK (typeof(rel_bytes)  = 'blob'),
            case_bytes BLOB NOT NULL UNIQUE CHECK (typeof(case_bytes) = 'blob'),
            rel        TEXT NOT NULL,
            case_key   TEXT NOT NULL,
            abs        TEXT NOT NULL,
            hash       TEXT NOT NULL,
            size       INTEGER NOT NULL,
            mtime      INTEGER NOT NULL,
            state      TEXT NOT NULL CHECK (state IN ('ok','stale')),
            last_seen  INTEGER NOT NULL
        );",
    )?;
    copy_files_rows(conn)?;
    assert_replacement_complete(conn, "files", "files_new")?;
    conn.execute_batch("DROP TABLE files; ALTER TABLE files_new RENAME TO files;")?;

    // The one copy that can stay a statement: its key comes from the row's own `rel`, so `CAST` is a per-row
    // value rather than one bind shared by every row — and casting to BLOB yields `typeof(...) = 'blob'`,
    // which the CHECK demands. Binding the same text as a Rust `&str` would not: a TEXT key and its BLOB
    // twin never compare equal, so one accidental string bind quietly reopens the byte-key bug.
    //
    // `rel` stops being a foreign key. Two live notes may spell themselves identically once the lossy
    // spelling is admitted, and a key that two rows share cannot be a key; `notes.rel`'s `ON UPDATE CASCADE`
    // was the only thing maintaining that column, so [`apply_move`] takes over the job on this pass.
    conn.execute_batch(
        "CREATE TABLE notes_new (
            id           INTEGER PRIMARY KEY,
            rel_bytes    BLOB REFERENCES files(rel_bytes) ON UPDATE CASCADE
                         CHECK (typeof(rel_bytes) = 'blob'),
            rel          TEXT,
            title        TEXT,
            title_source TEXT,
            encoding     TEXT,
            word_count   INTEGER,
            body         TEXT
        );
         INSERT INTO notes_new (id, rel_bytes, rel, title, title_source, encoding, word_count, body)
         SELECT id, CAST(rel AS BLOB), rel, title, title_source, encoding, word_count, body
           FROM notes ORDER BY rowid;",
    )?;
    assert_replacement_complete(conn, "notes", "notes_new")?;
    conn.execute_batch("DROP TABLE notes; ALTER TABLE notes_new RENAME TO notes;")?;

    // Both tables are back under their own names, so the keys they name exist and the indexes land on the
    // live table. `files_rel` and `notes_rel` are lookup-only now — neither column is unique or a key any
    // more. The first upgrade pass needed both, because row resolution (`SELECT id FROM files WHERE rel = ?1`)
    // and `mark_stale` asked about display text once per file inside the write phase; the next pass moved those readers
    // to `rel_bytes`, so **no production statement queries either index any more** — they are maintained on
    // every insert and update and read by nothing. They stay because dropping an index from an existing
    // database is a second migration rather than a comment edit, and `the_schema_is_what_the_plan_mandates`
    // pins the shape; their removal belongs to whatever next has a reason to rebuild these tables.
    conn.execute_batch(
        "CREATE INDEX files_case_bytes ON files(case_bytes);
         CREATE INDEX files_case_key   ON files(case_key);
         CREATE INDEX files_rel        ON files(rel);
         CREATE INDEX notes_rel        ON notes(rel);
         CREATE INDEX notes_rel_bytes  ON notes(rel_bytes);",
    )?;

    if let Some((table, rowid, parent)) = first_violation(conn)? {
        // Nothing above is committed: the caller's transaction rolls back with this error, so the file keeps
        // its version-3 schema and its data. An orphan note is the case this catches — it was writable with
        // enforcement off, which is how orphans got in at all (see [`crate::db::FK_ORPHANS_KEY`]) — and this step can
        // neither delete it (its file may come back; that is what `state = 'stale'` is for) nor invent a
        // parent for it.
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_foreign_key_check()",
            [],
            |row| row.get(0),
        )?;
        return Err(DbError::ForeignKeys {
            count,
            table,
            rowid,
            parent,
        });
    }

    let receipt = format!(
        "files={},notes={}",
        lossy_rows(conn, "files")?,
        lossy_rows(conn, "notes")?
    );
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![PATHS_LOSSY_KEY, receipt],
    )?;
    Ok(())
}

/// How many rows of `table` display a path that some earlier read could not spell losslessly.
///
/// The test is U+FFFD in the display column, which is what a lossy read leaves behind. It counts rows whose
/// *display* carries the marker rather than rows whose key differs from something: after this migration the
/// two are the same fact, and the count is what makes the first byte-keyed scan's churn readable.
fn lossy_rows(conn: &Connection, table: &str) -> Result<i64, DbError> {
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE INSTR(rel, ?1) > 0"),
        params!["\u{fffd}"],
        |row| row.get(0),
    )?)
}

/// How many display spellings one [`NOTE_MERGE_KEY`] receipt names — the same eight the loss record caps
/// at, for the same reason: the count is the fact and the names are only where to look.
const MERGE_NAME_CAP: usize = 8;

/// Migration 5 — the duplicate note rows version 3 left behind are merged into one row each.
///
/// Why a merge and not a permanent charge: `notes.rel` carried no `UNIQUE`, so one path could hold two note
/// rows, migration 4 copied both onto one `rel_bytes`, and every scan since has answered that key with
/// the scan's reader (`crate::scan::Indexed::Duplicate`, a code span: that enum is private) — charging
/// `uncovered` forever, withholding the parse version forever,
/// re-parsing the whole corpus on every run and printing a refresh gap nobody can act on. The charge
/// was right while there was no exit; this is the exit.
///
/// The rule, stated rather than left to chance:
///
/// - **Winner = `MIN(id)`.** The oldest row is the one derived rows already hang off, so keeping it keeps the
///   corpus attached. Content loses, and that is survivable *because* the version was already withheld: the
///   next scan rewrites title, body, headings, tags, items and edges for every note from what is on disk.
/// - **Incoming edges are re-pointed to the winner, never nulled.** Other notes' prose is not re-parsed, so a
///   NULL target would lose a resolved link permanently — the distinction migration 3 drew between CASCADE
///   (an edge from a note that is gone) and SET NULL (a target that is gone) does not apply when the target
///   still exists under another id.
/// - **Nothing needs deduplicating after the re-point.** No child table carries a unique constraint —
///   `headings`, `tags`, `items` are plain and `links` has none either (see migration 2) — so moving an edge
///   onto the winner cannot collide with anything, and two edges that now look alike are two mentions of one
///   note, which is what the writer would have stored anyway. A defensive dedup here can never fire.
///
/// **The deletes are by hand, not by cascade.** The expectation was that the loser's derived rows go with its row,
/// and that is the outcome; the mechanism cannot be the schema's, because [`migrate`] runs every step with
/// `PRAGMA foreign_keys = OFF` — a pragma that cannot be changed inside a transaction — and SQLite performs
/// no `ON DELETE CASCADE` while it is off (verified: deleting a parent with the pragma off leaves its
/// cascading children standing). Deleting the loser without its children would commit orphan `headings`,
/// `tags`, `items` and `links.src_note_id` rows into a file whose enforcement is back ON the moment this step
/// commits — the silent re-adoption [`add_note_foreign_keys`] exists to end. So the six statements below are
/// migration 3's shapes, spelled out: the loser's own derived rows and outgoing edges go with it, its inbound
/// edges move to the winner.
///
/// Because those deletes are the only thing between a loser row and its children, the step closes with
/// `PRAGMA foreign_key_check` like migrations 3 and 4 — see the check's own comment for why it matters more here
/// than it does anywhere else in this file.
///
/// The receipt goes into `meta` **inside this transaction**: a merge that cannot announce itself
/// does not happen. [`apply_migrations`] echoes it after the commit, never from here.
pub(crate) fn merge_duplicate_notes(conn: &Connection) -> Result<(), DbError> {
    // Every read happens before the first write, and each query is fully collected rather than iterated:
    // SQLite refuses a write on a connection that still has a row-producing statement open (the same
    // discipline as [`copy_files_rows`]).
    let groups = duplicate_groups(conn)?;
    let merged = groups.iter().map(|group| group.losers.len()).sum::<usize>();
    for group in &groups {
        for loser in &group.losers {
            conn.execute(
                "UPDATE links SET dst_note_id = ?1 WHERE dst_note_id = ?2",
                params![group.winner, loser],
            )?;
            conn.execute("DELETE FROM headings WHERE note_id = ?1", params![loser])?;
            conn.execute("DELETE FROM tags WHERE note_id = ?1", params![loser])?;
            conn.execute("DELETE FROM items WHERE note_id = ?1", params![loser])?;
            // Every outgoing edge, `approved` included — the same ruling migration 3 wrote into the schema,
            // where a foreign key cannot be conditional either.
            conn.execute("DELETE FROM links WHERE src_note_id = ?1", params![loser])?;
            conn.execute("DELETE FROM notes WHERE id = ?1", params![loser])?;
        }
    }

    if let Some((table, rowid, parent)) = first_violation(conn)? {
        // Migrations 3 and 4 run this check because a *rebuild* could misplace a row. Here it is the only
        // defence against the deletes above being softened: a code step runs with enforcement off, so no
        // `ON DELETE CASCADE` fires behind them and a skipped or narrowed delete leaves a `note_id` pointing at
        // a row that is gone — into a file whose enforcement is back ON the moment this transaction commits,
        // where an id can be re-adopted by the next `INSERT`. No later scan re-checks those keys, so this is
        // the one place anything would notice. Returning rolls the merge and its receipt back together, which
        // leaves the file at version four to try again rather than committing a half-merged corpus.
        //
        // One consequence worth naming: the pragma answers about the **whole file** and this step runs it
        // unconditionally, so a workspace damaged by anything else — a child row whose note is simply gone —
        // now refuses to open at the four-to-five step where version four opened it, which is deliberate,
        // pinned by `the_merge_gate_refuses_damage_that_no_merge_could_have_caused` (a file with **no pair**, so
        // narrowing this check to `merged > 0` fails there) and by
        // `the_merge_gate_refuses_a_child_row_whose_note_is_gone` (the same damage beside a pair). Its only exit
        // is the message itself (table, rowid, parent), because nothing downstream can repair one: there is
        // no repair command for this damage. The check is not scoped to rows this merge touched.
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_foreign_key_check()",
            [],
            |row| row.get(0),
        )?;
        return Err(DbError::ForeignKeys {
            count,
            table,
            rowid,
            parent,
        });
    }

    let paths: Vec<String> = groups.iter().map(|group| group.display.clone()).collect();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![NOTE_MERGE_KEY, merge_receipt(merged, &paths)],
    )?;
    Ok(())
}

/// One byte key that holds more than one note row: the survivor, the rows folded into it, and the name the
/// receipt gives them.
struct DuplicateNotes {
    /// The surviving row's display `rel`. Names in the receipt are display text and identify no path
    /// — they say where to look.
    display: String,
    /// `MIN(id)`: the oldest row, which keeps its id and therefore its derived rows.
    winner: i64,
    /// Every other id on the key, ascending.
    losers: Vec<i64>,
}

/// Every `rel_bytes` holding more than one note row, with its winner and losers.
///
/// Ordered by the **byte key**, which is what makes one database produce one receipt; display spellings can
/// tie where bytes do not, the key cannot.
///
/// Rows whose key is NULL are left alone — and today none can exist. `notes.rel_bytes` carries
/// `CHECK (typeof(rel_bytes) = 'blob')` from migration 4 onward and `typeof(NULL)` is `'null'`, so a keyless row
/// is unwritable by this program or by anything speaking plain SQL to the file; **that constraint, not this
/// clause, is what protects those rows, which is why no test pins the clause and none can be written.** It stays
/// as defence in depth for whoever changes that: `GROUP BY` gathers **every** NULL into one group, two keyless
/// notes are two unrelated files, and folding them deletes one of their note rows outright.
fn duplicate_groups(conn: &Connection) -> Result<Vec<DuplicateNotes>, DbError> {
    let keys: Vec<Vec<u8>> = {
        let mut statement = conn.prepare(
            "SELECT rel_bytes FROM notes
              WHERE rel_bytes IS NOT NULL
              GROUP BY rel_bytes
             HAVING COUNT(*) > 1
             ORDER BY rel_bytes",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    let mut groups = Vec::with_capacity(keys.len());
    for rel_bytes in keys {
        let ids: Vec<i64> = {
            let mut statement =
                conn.prepare("SELECT id FROM notes WHERE rel_bytes = ?1 ORDER BY id")?;
            let rows = statement.query_map(params![&rel_bytes], |row| row.get::<_, i64>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        // `HAVING COUNT(*) > 1` already promised at least two ids here, so `losers` is never empty; the
        // pattern is total rather than an `expect`, because arriving with one id must merge nothing instead
        // of deleting somebody's only note row.
        let Some((winner, losers)) = ids.split_first() else {
            continue;
        };
        groups.push(DuplicateNotes {
            display: survivor_display(conn, *winner, &rel_bytes)?,
            winner: *winner,
            losers: losers.to_vec(),
        });
    }
    Ok(groups)
}

/// The spelling one merged path is named by in the receipt.
///
/// `notes.rel` is a display column and nullable, so a hand-made row can lack it; the byte key's own
/// lossy spelling is the fallback, because the receipt points at a path rather than identifying one. Without it
/// such a merge would record `merged=1;paths=` — a fold whose path cannot be named. Pinned by
/// `a_merged_pair_with_no_display_text_is_named_by_the_lossy_spelling_of_its_key` in `crates/np-core/tests/db.rs`.
fn survivor_display(conn: &Connection, winner: i64, rel_bytes: &[u8]) -> Result<String, DbError> {
    let stored: Option<Option<String>> = conn
        .query_row(
            "SELECT rel FROM notes WHERE id = ?1",
            params![winner],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    Ok(stored
        .flatten()
        .unwrap_or_else(|| String::from_utf8_lossy(rel_bytes).into_owned()))
}

/// The stored [`NOTE_MERGE_KEY`] spelling: `merged=N;paths=a.md,b.md`, the names capped at
/// [`MERGE_NAME_CAP`] and already in key order from [`duplicate_groups`].
fn merge_receipt(merged: usize, paths: &[String]) -> String {
    let named: Vec<&str> = paths
        .iter()
        .take(MERGE_NAME_CAP)
        .map(String::as_str)
        .collect();
    format!("merged={merged};paths={}", named.join(","))
}

/// How many note rows a stored [`NOTE_MERGE_KEY`] receipt says were merged away.
///
/// The reader that pairs with `merge_receipt` (a code span, not a link: that function is private), for
/// every caller left holding only the stored string — the scan report's field and the stderr echo here. Like
/// [`orphan_total`] it parses its own format rather than carrying a second number around: one value, three
/// readers, no way for them to disagree. A fragment that will not parse as a count adds nothing.
pub fn merge_total(value: &str) -> usize {
    value
        .split(';')
        .find_map(|part| part.strip_prefix("merged="))
        .and_then(|count| count.parse::<usize>().ok())
        .unwrap_or(0)
}

/// Echo the stored merge receipt to `out` — [`std::io::stderr`] in production — unless nothing was merged.
///
/// The same shape, sink and dropped-write rule as [`report_orphans`], for the same reasons: stderr cannot be
/// captured in-process because this runs inside [`Db::open`] before a test could redirect descriptor 2, so the
/// sink is a parameter and this module's two tests pin both halves through it —
/// `the_merge_receipt_is_echoed_once_rows_have_been_folded` (one line naming the total beside the stored
/// receipt) and `a_merge_that_folded_nothing_writes_no_line` (silence at zero, which is what keeps every
/// ordinary command quiet while `merged=0` still lands in `meta` as proof the check ran). What they cannot pin
/// is that the production call site names stderr, nor that it runs after the commit — see [`apply_migrations`],
/// where the same no-portable-COMMIT-failure argument applies. And a failed write must not roll back
/// (or abort) an upgrade whose deletions are durable in `meta` under [`NOTE_MERGE_KEY`].
pub(crate) fn report_merges(out: &mut dyn io::Write, receipt: &str) {
    let merged = merge_total(receipt);
    if merged == 0 {
        return;
    }
    let _ = writeln!(
        out,
        "note-processor: merged {} duplicate note row(s) into the oldest row for their path ({})",
        merged, receipt,
    );
}
