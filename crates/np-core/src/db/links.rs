use crate::db::{Db, DbError};

/// `links.origin` for an edge the parser read out of a note, as opposed to one a person approved.
/// Spelled once here because the count and the list beside it are two queries ([`unresolved_targets`]
/// here, `crate::scan::unresolved_links` there) plus the writer and the deleter in [`crate::scan`], and
/// an author-facing report that filtered one query by a literal `'prose'` and the other by a constant
/// would drift the moment M3 introduces a second origin.
pub(crate) const PROSE_ORIGIN: &str = "prose";

/// A link edge whose destination is a live note, spelled once because four queries have to agree on
/// it: the two counts `crate::scan::catalog_entries` renders, that module's `unresolved_links`, and
/// [`unresolved_targets`] here. The notes→files join is the point: an edge into a stale note is not a
/// healthy link. Migration 3 removed the other case this predicate used to catch — an edge pointing at a
/// note row that had been deleted, now impossible because `dst_note_id` is a foreign key that NULLs
/// itself — so the join stays for the stale case and the query is unchanged.
/// The join is on `rel_bytes`, not the display `rel`: once two live rows may spell themselves
/// identically, joining on that text is a cross product — one edge counted twice, and a stale twin able to
/// answer for a live one, so the count and the list printed under it could disagree.
pub(crate) const LIVE_EDGE: &str =
    "EXISTS (SELECT 1 FROM notes dn JOIN files df ON df.rel_bytes = dn.rel_bytes \
                                 WHERE dn.id = l.dst_note_id AND df.state = 'ok')";

/// The non-attachment link targets no live note answers: the "notes I could not place" list a scan
/// carries as [`crate::ScanReport::unresolved_targets`] and prints beside its
/// [`crate::ScanReport::unresolved_links`] count.
///
/// `note_counts` counts every NULL destination, because at database level an attachment with no
/// note row behind it is unresolved like a missing file; this is the author-facing question instead,
/// so attachments are left out and only notes are asked about. Stale sources count too — their rows
/// are history the report does not rewrite.
///
/// The predicate is `LIVE_EDGE` rather than `dst_note_id IS NULL` on purpose: an edge whose stored
/// destination went stale counts in the report's number without having a NULL in it, so it has to be
/// named here as well. A count and the list printed behind it that disagree would send the reader
/// looking for a note the list cannot show.
///
/// Sorted and deduplicated: one name written in three notes is one thing to fix.
pub fn unresolved_targets(db: &Db) -> Result<Vec<String>, DbError> {
    let mut statement = db.0.prepare(&format!(
        "SELECT DISTINCT target FROM links l
          WHERE l.origin = '{PROSE_ORIGIN}' AND l.kind != 'attachment'
            AND NOT {LIVE_EDGE}
          ORDER BY target",
    ))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut targets = Vec::new();
    for row in rows {
        targets.push(row?);
    }
    Ok(targets)
}

/// A live tracked file with no note row behind it: an **attachment**.
///
/// One const rather than two queries, for the reason [`LIVE_EDGE`] exists: [`attachments`] reads it twice, and a
/// count that disagrees with the names printed beside it sends the reader looking for a file the list cannot show.
const ATTACHMENT_WHERE: &str =
    "f.state = 'ok' AND NOT EXISTS (SELECT 1 FROM notes n WHERE n.rel_bytes = f.rel_bytes)";

/// What [`attachments`] found, spelled in two parts because the two answer different questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachments {
    /// Every live tracked file that is not a note. Exact, and never capped.
    pub count: i64,
    /// Up to the caller's limit of them, ordered by path **bytes** and printed as display text — so two
    /// byte-distinct twins can arrive as two lines that read alike, exactly as in
    /// [`crate::ScanReport::skipped`].
    pub examples: Vec<String>,
}

/// The files this workspace tracks that search will never reach: hashed, listed and linkable,
/// but parsed only when they are markdown.
///
/// This is the number a front end needs to say what happened in np's own words — "1 of 5 files became notes; 4
/// are attachments" — because `added` counts every file the walk found and `unindexable` stays `null`: nothing was
/// lost, it simply was never a note. The invariant worth stating is that [`Attachments::count`] plus the live note
/// count is the number of live `files` rows: the two halves of what a scan tracks, with no third bucket.
///
/// `example_limit` caps the names and not the count. Unresolved link targets are a to-do list an author works
/// through; this list is "everything else in the folder", which in a vault holding 300 PDFs is not something a
/// report should print in full on every scan.
pub fn attachments(db: &Db, example_limit: usize) -> Result<Attachments, DbError> {
    let count = db.0.query_row(
        &format!("SELECT COUNT(*) FROM files f WHERE {ATTACHMENT_WHERE}"),
        [],
        |row| row.get(0),
    )?;
    let mut statement = db.0.prepare(&format!(
        "SELECT f.rel FROM files f WHERE {ATTACHMENT_WHERE} ORDER BY f.rel_bytes LIMIT ?1"
    ))?;
    let rows = statement.query_map([i64::try_from(example_limit).unwrap_or(i64::MAX)], |row| {
        row.get::<_, String>(0)
    })?;
    let mut examples = Vec::new();
    for row in rows {
        examples.push(row?);
    }
    Ok(Attachments { count, examples })
}
