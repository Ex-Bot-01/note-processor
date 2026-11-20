use std::collections::HashSet;
use std::fs;

use std::path::PathBuf;

use rusqlite::params;

use crate::change::{Change, FileIdentities};

use crate::db::{Db, DbError};
use crate::hash::{sha256_bytes, sha256_file, Snapshot};

use crate::links::RawLink;

use crate::path_keys::RelPath;
use crate::scan::write::note_row;
use crate::scan::write::NoteRow;
use crate::walk;
use crate::walk::DiscoveredFile;
use crate::walk::UnknownPath;

/// The UTF-8 byte order mark: dropped before decoding for parsing, kept in the bytes that are hashed.
const BOM: &[u8] = &[0xef, 0xbb, 0xbf];

/// `notes.encoding` for text that decoded without a single replacement character.
pub(crate) const ENCODING_UTF8: &str = "utf-8";

/// `notes.encoding` for bytes UTF-8 cannot hold, decoded with replacements instead of failing the scan
/// so that one undecodable byte does not fail the whole scan.
pub(crate) const ENCODING_LOSSY: &str = "lossy";

/// A note re-parsed by one scan: its path and the links its text holds, which is everything
/// [`store_note_links`] needs once the whole corpus is up to date. The path travels in both spellings so the id
/// lookup that follows can go by bytes.
pub(crate) type Parsed = (RelPath, Vec<RawLink>);

/// The two spellings of one found file, as the write pass and the link pass both need them.
pub(super) fn path_of(snapshot: &Snapshot) -> RelPath {
    RelPath::new(snapshot.rel_bytes.clone(), snapshot.rel.clone())
}

/// What the write pass counted and collected.
#[derive(Default)]
pub(crate) struct Written {
    pub(crate) added: usize,
    pub(crate) modified: usize,
    pub(crate) moved: usize,
    pub(crate) removed: usize,
    pub(crate) unchanged: usize,
    /// Paths whose identity was already held by another row (see [`apply_changes`]).
    pub(crate) skipped: Vec<String>,
    /// Notes this scan re-parsed **only** because the parse rules moved on — a file `classify` called
    /// unchanged. Deliberately absent from [`ScanReport`], whose counts stay the classification truth and
    /// whose JSON shape the CLI tests pin; [`report_refresh`] is where it becomes visible.
    pub(crate) refreshed: usize,
    /// Every note re-parsed here, for [`store_note_links`].
    pub(crate) parsed: Vec<Parsed>,
    /// Notes this scan owed a write and never delivered one: unreadable as the walk saw it, or lost to an
    /// identity conflict. Only meaningful during a refresh, where it decides whether the new rules may be
    /// recorded — see [`refresh_covered`].
    pub(crate) uncovered: usize,
}

/// One discovered file as this scan saw it: identity from the walk, digest and note text from the read
/// — or the marker that says there is nothing to index this time.
pub(crate) struct Found {
    /// Identity as the walk recorded it, plus the digest of the bytes on disk. An **empty hash is the
    /// marker [`crate::change`] reads as unknown**: the file could not be read as the walk saw it, so
    /// this scan writes nothing about it and its row keeps what it has.
    pub(crate) snapshot: Snapshot,
    /// The real OS path, byte-exact even when `rel` had to be decoded lossily.
    pub(crate) abs: PathBuf,
    /// Decoded note text; `None` for a file that could not be read or is not markdown, since an
    /// attachment is hashed, tracked and linkable but never parsed.
    pub(crate) text: Option<String>,
    /// Whether the walk called this a note at all. [`apply_changes`] needs it to charge a missed path
    /// only when a parse was actually owed — see [`refresh_covered`].
    pub(crate) is_markdown: bool,
    /// Whether `text` arrived through lossy decoding.
    pub(crate) lossy: bool,
}

/// Whether this scan holds the file's contents, as opposed to knowing only that it saw the path.
pub(crate) fn is_unknown(found: &Found) -> bool {
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
pub(crate) fn owes_a_reparse(db: &Db, path: &RelPath, is_markdown: bool) -> Result<bool, DbError> {
    Ok(is_markdown && !matches!(note_row(db, path)?, NoteRow::Missing))
}

/// Read every discovered file, re-stating each one first, and name the unreadable ones in `skipped`.
///
/// The restatement is what closes the walk-to-read window: a file that has gone, or
/// whose size or modification second moved since the walk, is *unknown* to this scan. It is reported,
/// indexed as nothing, and keeps the row it has.
///
/// An unknown path still reaches [`crate::change::classify`] — as an empty-hash marker rather than not
/// at all. That is how such a file is kept from being counted `Removed`, and how the content its row
/// records is kept from being claimed as some other path's move (see [`crate::change`]).
pub(crate) fn read_found(files: &[DiscoveredFile], skipped: &mut Vec<UnknownPath>) -> Vec<Found> {
    let found: Vec<Found> = files.iter().map(read_one).collect();
    skipped.extend(
        found
            .iter()
            .filter(|found| is_unknown(found))
            // Both spellings of the path, from the walk's own component walk: this file was listed, so its
            // bytes are as exact as a refused directory's, and deduping on them keeps two unreadable twins
            // from publishing one line.
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
/// alike the two lines look. Sorting by the same key makes the order total where display text ties.
/// A name that only ever existed as text arrives through [`UnknownPath::from_display`], whose bytes *are* that
/// text's UTF-8 bytes, so such entries dedup and order exactly as the strings used to.
pub(crate) fn skipped_lines(mut paths: Vec<UnknownPath>) -> Vec<String> {
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
/// is the file's identity and not the text's.
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
/// look at — a subtree under an unreadable directory, or one refused name. The filter is
/// [`is_unknown_path`], and it judges a row by its `rel_bytes`: a row whose *display* text happens to match an
/// unknown path while its bytes do not belongs to a subtree this scan did read, and hiding such a row is the
/// mistake this filter exists not to make. The one exception is a row whose stored key was itself derived from
/// display text ([`key_is_display_derived`]): such a row has no byte spelling left to be judged by, so it is
/// compared text against text — unknown is not absent — and that comparison **marks** the row rather than
/// deleting it. An ambiguous row stays in `known` when a listed readable path spells it exactly and matches its
/// stored hash, which is what [`admits`] decides; whether it resolves is settled afterwards from what
/// `classify` did, not here.
///
/// Stale rows stay out because their absence was already reported; leaving them in would count one
/// deleted file as `removed` on every later scan. Dropping them is also what lets such a file come back
/// as `Added` onto the row it still has, which is how its note rejoins with its links attached —
/// [`crate::db`] keeps ids stable for exactly that.
///
/// The unreadable directories are the other half of the same caution: files below one were never seen,
/// so their rows must reach neither a `Removed` nor a move, and leaving them in would
/// stale a whole subtree over one transient permission error. A refused name is the third piece of the same
/// rule ([`is_unknown_path`]): the file is there, but its identity could not be spelled, so its row
/// keeps what it has instead of being marked stale for want of a key. A refused **directory** needs no fourth
/// rule: [`crate::walk`] records it in `unreadable`, because an unspellable name makes every path below it
/// unkeyable too — the same hole an unreadable one cuts, by the same prefix.
///
/// The note test correlates on `rel_bytes`: with twins a note belonging to either file would otherwise
/// answer for both, and that boolean feeds the refresh-coverage decision, which decides whether
/// [`PARSE_VERSION_KEY`] gets recorded.
///
/// The count alongside the rows is how many live **note** rows that byte-keyed filter dropped — nothing else
/// reports them, since they appear nowhere in `found` — which is how [`scan_with_inject`] knows a
/// parse-rule refresh never reached part of the corpus. Any row with no note behind it is excluded by the
/// `EXISTS` — attachments and never-yet-parsed paths alike: no parse rule can stale rows that do not exist,
/// so counting one would pin the version forever over work that is not owed. Marked rows charge
/// **separately** ([`unresolved_ambiguity`]), and a row never charges both ([`known_rows`]'s
/// precedence: the byte arm answers first and marks nothing).
pub(crate) struct KnownRows {
    /// The rows `classify` may judge.
    pub(crate) rows: Vec<Snapshot>,
    /// Source 1 of the refresh's coverage charge: live note rows the **byte-keyed** arm hid.
    pub(crate) hidden: usize,
    /// Every ambiguous row this pass marked, admitted or refused, in byte-key ascending order — the
    /// population [`unresolved_ambiguity`] reads and the unindexable record renders.
    pub(crate) marked: Vec<Marked>,
    /// The one identity tally computed from these rows and the listed paths, shared with
    /// [`crate::change::classify_with_identities`] so admission and pairing cannot disagree.
    pub(crate) identity: FileIdentities,
}

/// One row `known_rows` **marked**: its key is display text ([`key_is_display_derived`]) and an unreadable path spells
/// that text, so `known_rows` cannot tell whether the file it describes is the readable look-alike or a file
/// behind the obstruction. It may never be removed, and it may be claimed — by content or by its own listed
/// path.
pub(crate) struct Marked {
    /// The row's stored key: what a claim names (`Change::Moved { from }`) and what [`strip_marked_removals`]
    /// matches a `Removed` against.
    key: Vec<u8>,
    /// Display spelling, for the capped name list in the unindexable record.
    pub(crate) rel: String,
    /// Whether [`admits`] let it reach `known`. A refused row is pre-dropped — today's behaviour — and so
    /// resolves nothing this pass.
    pub(crate) admitted: bool,
    /// Whether a note row hangs off it, for the reason [`known_rows`] gives: only a note can hold
    /// [`PARSE_VERSION_KEY`] back.
    is_note: bool,
}

pub(crate) fn known_rows(
    db: &Db,
    unreadable: &[UnknownPath],
    unrepresentable: &[UnknownPath],
    listed: &[Snapshot],
) -> Result<KnownRows, DbError> {
    // Both key columns come back, because this is the database side of a byte-keyed comparison: hand out
    // display text here and every non-UTF-8 row reads as removed-and-re-added on every scan forever — the
    // never-converges symptom manufactured by the fix.
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
                // nothing, reads as unknown. The routes that use it treat unknown as no opinion.
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
    // eligible: a row admitted without a partner would have its `Removed` stripped and gain no
    // `Moved`, the permanent-duplicate stuck state this function's own comment warns about.
    let mut loaded: Vec<Snapshot> = Vec::new();
    // `(index, is_note)` for each marked row, in the query's byte-key order — which is the order `marked` must
    // come out in, because the determinism rule reads that vector when it renders the record.
    let mut candidates: Vec<(usize, bool)> = Vec::new();
    let mut is_marked: Vec<bool> = Vec::new();
    for row in rows {
        let (snapshot, is_note) = row?;
        // Bytes first: that is the identity, and for any row the walk keyed it this is the whole answer. First
        // for a second reason too — **precedence**. The permanent class satisfies both arms at once (a row
        // whose key equals its OS bytes, sitting under a genuinely unreadable prefix), so marking before hiding
        // would charge it in the hidden count *and* the ambiguity tally; on Windows, where refusal bytes are lossy, that
        // double charge would be universal. Today's single `||` could not double count, and neither may this.
        if is_unknown_path(&snapshot.rel_bytes, unreadable, unrepresentable) {
            if is_note {
                // The row is live and hidden, not deleted — the distinction a reader of a short `INDEX.md` is trying
                // to make. The directory is reported as `unreadable`; this names the note it swallowed — the stored
                // path, never the note's text.
                tracing::debug!(
                    target: crate::diagnostics::target::SCAN,
                    rule = crate::diagnostics::rule::HIDDEN_BEHIND_UNREADABLE,
                    what = %snapshot.rel,
                    "a live row sits under a path this scan could not read, so it is hidden rather than removed"
                );
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
    // display-derived by construction, so this is exactly the ambiguous population and an ordinary rename's
    // stored row is never offered to identity pairing at all.
    let identity = FileIdentities::build(&loaded, listed, |row| {
        key_is_display_derived(&row.rel, &row.rel_bytes)
    });

    // Admission is decided before anything is dropped: what no route can account for never enters `known`
    // — today's behaviour for the refused set, unchanged — and charges source 2.
    let mut admitted_row = vec![false; loaded.len()];
    let mut marked = Vec::with_capacity(candidates.len());
    for &(index, is_note) in &candidates {
        let snapshot = &loaded[index];
        // Content first, identity second, and the order is a precedence rule rather than
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

    // The other half of "where did my note go": a row whose only spelling is lossy and which no route accounted for
    // stays out of the catalog. Reported per row at `debug` with its stored path, because the remedy is on disk — give
    // the file a name this build can key — and nothing else says which row is waiting for it.
    for marked_row in &marked {
        if !marked_row.admitted && marked_row.is_note {
            tracing::debug!(
                target: crate::diagnostics::target::SCAN,
                rule = crate::diagnostics::rule::UNRESOLVED_AMBIGUITY,
                what = %marked_row.rel,
                "no route accounted for this row's path this scan, so it stayed out of the catalog"
            );
        }
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

/// Whether an ambiguous row is **admitted** to `known` — the scoping rule that stands between marking such a
/// row and re-homing its note onto a stranger's file.
///
/// `classify` matches a claim on **hash alone** (`claimants.get(added.hash)` over every unmatched known row),
/// and its `present_hashes` guard cannot help here: that set is seeded from known rows whose *key* matches an
/// unreadable listing, and a display-derived key never equals a real-bytes listing key. Left unscoped, any
/// unrelated readable path with byte-identical content — a `# Title` stub, a blank template, a re-exported note
/// — would claim the hidden row and re-home its id. So the scoping happens before classification: some listed
/// path must spell the row exactly (same display text), carry its stored hash, and be readable (an empty hash is
/// [`crate::change`]'s unknown marker).
///
/// The third test is the stale-row rule: a claim onto a key another row already holds hits the `UNIQUE` in
/// [`crate::db::apply_move`], comes back `Indexed::Taken`, returns early, and leaves the path in `skipped` while
/// `owes_a_reparse` charges `written.uncovered` on every refresh — a stuck state nothing clears. Rows of **any**
/// state count, because [`crate::db::upsert_file`] resolves by `rel_bytes` with no state filter and that is how a
/// returning file rejoins its note.
///
/// Testing "some candidate" rather than "the one `classify` will pick" is sound: a claim needs exactly one added
/// path carrying the hash (`added_count == Some(&1)`), so wherever two twins both qualify there is no claim to
/// refuse. With display scoping plus `files.rel_bytes UNIQUE` there is at most **one claimant per listed path**,
/// which is why no multiplicity rule, tally or tie-break exists to write.
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

/// Post-strip: drop every `Removed` naming an admitted marked key, after classification and before
/// anything consumes the list — `changes` has exactly one consumer ([`apply_changes`]), read twice inside it.
///
/// **`Change::Removed` only.** The identity-matched route legitimately emits `Modified` keyed on the listed
/// path, whose `rel_bytes` equal the row's stored key; a strip written as "drop any change carrying a marked
/// key" would delete that legitimate re-parse and leave stale derived rows behind charging nothing. Traced
/// against [`classify`]: in M and in N′-changed the row is unmatched by identity, its hash matches no added path,
/// and the final loop pushes exactly one `Removed` whose bytes are the stored key — so `(Removed, key)` covers
/// both.
pub(crate) fn strip_marked_removals(
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

/// Source 2 of the refresh's coverage charge: the marked rows this pass **did not resolve**, as their display
/// spellings in byte-key order. Source 1 is `KnownRows::hidden`; a row is charged by one of the two, never both.
///
/// Resolution is the two routes below, read from what classification actually delivered rather than tallied in
/// a pre-pass — which is what lets a healed row stop paying once it has healed, because a charge that never
/// clears is the bug this mechanism exists not to reintroduce:
///
/// - **claimed** — it produced `Moved { from } = its own key`, so [`crate::db::apply_move`] re-keys `rel_bytes`
///   from real bytes; or
/// - **matched** — its own path was listed with a non-empty hash. Matched on the **key**, because that is what
///   `classify` pairs identity on: a listing that merely *spells* the row the same is M's look-alike, whose rows
///   are still unpaired. Without this route the permanent class — a name genuinely holding `U+FFFD`, and every
///   lossy Windows name — would charge every scan forever.
///
/// Then the exclusion: a marked row whose own key was listed **unreadable** is charged by `written.uncovered`
/// instead, which resolves by byte key. Deriving the count from "did we strip a `Removed`" instead would charge
/// zero times for a marked row `classify` filtered out before emitting anything — an uncharged stale row is
/// exactly the silent-gap class this project keeps paying to avoid.
pub(crate) fn unresolved_ambiguity(
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
/// below ([`crate::walk::Walked::unreadable`] carries both shapes). `"."` is how [`crate::walk`] spells the
/// workspace root itself, and that leaves nothing knowable at all.
///
/// Both sides of the comparison are **bytes**, and an entry only matches at a `/` boundary: `caf\xE9` hides
/// `caf\xE9/a.md` and nothing else — not its twin `caf\xE8/a.md`, whose display spelling is identical,
/// and not `caf\xE9x/a.md`, which is a different subtree.
///
/// An entry holding **no** bytes matches nothing. The walk spells the root `"."` for exactly that reason (see
/// [`crate::walk`]'s `unknown_path`), and it matters in both directions: an empty prefix would match every path
/// in the workspace, which is the one way this filter could hide the whole corpus rather than one subtree.
pub(super) fn under_unreadable(rel_bytes: &[u8], unreadable: &[UnknownPath]) -> bool {
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
/// ([`crate::walk::Walked::unrepresentable`]) is one file rather than a region, hence exact equality.
/// A refused **directory** belongs to the first list rather than the second
/// ([`crate::walk::Walked::unreadable`]), and that is not a convenience: its name is the first component of
/// every path below it, so none of those can be keyed either and the whole subtree needs the prefix rule.
/// Every arm here suppresses `Removed` for the same reason — the scan did not see that path, which is not
/// evidence it stopped existing.
///
/// Both arms compare **bytes**, which is the whole point of [`UnknownPath`]. Matching display text instead
/// let one unreadable or refused directory hide its readable twin: `caf\xE9/` and `caf\xE8/` spell one
/// `caf\u{FFFD}/`, so rows belonging to a subtree this scan had read in full dropped out of `known` — a real
/// deletion inside the readable twin went unreported, a rename there produced a second live row instead of a
/// move, and one note could be charged in both `written.uncovered` and `hidden_by_unknown`.
///
/// Windows loses nothing by the switch and gains nothing from it: its key bytes *are* the lossy text, so
/// byte equality there decides exactly what text equality decided. On Unix no *refused* name can reach either
/// list at all (`path_keys::name_is_lossless` is `true` for every byte a filename holds), while the prefix arm
/// still fires on any genuinely unreadable directory — and that is where the twin case above is reachable today.
pub(super) fn is_unknown_path(
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
/// `63 61 66 EF BF BD 2F 6E 6F 74 65 2E 6D 64`: the `U+FFFD` spelling of a name whose bytes no `String` holds;
/// `crates/np-core/tests/db.rs` proves such rows ship.
///
/// Both halves of the test keep this to rows whose key **is** their own display text, which is what byte
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
/// A migrated row leaves the class when a scan sees its path **and** pairs with it — by identity, or by
/// content through [`known_rows`]' scoping, which routes the pair through [`crate::db::apply_move`] so `rel_bytes`
/// becomes the walk's bytes, a churn paid once per row. Nothing else does: an unchanged row is refreshed in place and
/// keeps the key it has, so this fallback is not transitional for the cases above — say "until someone renames it", not
/// "until the next scan". What changed is not membership but consequence: a row still in the class is now marked
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
/// [`PARSE_VERSION_KEY`] is recorded over rows nobody re-parsed, and brings the note back as a **second
/// `notes.id`** when the directory opens again.
///
/// **Do not call that over-hiding harmless.** When the row's own path is readable while an unreadable twin shares its
/// display spelling, hiding the row left the listed path with no row to pair with, so it indexed as a spurious `Added`:
/// two live `files` and `notes` rows for one file, with the version withheld forever. **That half is closed here** —
/// not by narrowing this predicate, which cannot tell the cases apart, but by marking the row instead of deleting it so
/// [`crate::change::classify`] can pair them on content; that is what
/// `a_healed_display_keyed_pair_leaves_one_live_row_and_records_the_version` asserts.
///
/// What still stands, and is why the width stays: no predicate computable at this point separates the two cases, because
/// a display spelling has many byte twins and a lossy key records none of them. A row whose **content changed** while it
/// was unindexable has nothing to pair on, so its duplicate pair returns; and the case this fallback exists for — the
/// row's own file inside the obstruction, a different file readable outside it — must keep hiding, which is what
/// `a_display_keyed_row_inside_an_unreadable_directory_survives_its_readable_twin` fences. The residue stays an
/// open backlog item, where durable file identity is named as the way through it.
fn display_spelled(paths: &[UnknownPath]) -> Vec<UnknownPath> {
    paths
        .iter()
        .map(|path| UnknownPath::from_display(path.display.clone()))
        .collect()
}
