use super::{
    apply_changes, known_rows, refresh_covered, report_refresh, report_refresh_gap, skipped_lines,
    strip_marked_removals, unindexable_record, unresolved_ambiguity, ChunkTally, UnknownPath,
    WritePass, NAME_CAP, PARSE_VERSION,
};
use crate::catalog::{HEADER, HEADER_CRLF};
use crate::change::{classify, Change};
use crate::config::MarkerConfig;
use crate::db::{self, upsert_file, Db};
use crate::hash::Snapshot;
use crate::path_keys::{self, RelPath};
use crate::scan::found::{is_unknown_path, under_unreadable, Found, Written};
use crate::scan::publish::{np_published, remove_corpses, remove_file};
use crate::scan::write::folded_row;
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

/// The accepted regression, as evidence rather than a paragraph: the ASCII byte fold cannot bridge a
/// Unicode-only case pair, so [`folded_row`] misses a rename that changes only a non-ASCII letter's case.
///
/// This lookup used to run on the Unicode display `case_key`, where `Ämli.md` and `ämli.md` are one
/// fold and the rename was followed onto the existing row. Matched on `case_bytes` it finds nothing, so the
/// caller upserts a second row — and [`crate::change::key_of`] is `case_bytes` under `case_sensitive =
/// false`, which means **on Windows this pair is the entire rename-following story**, not one corner of it.
/// Two seams keep this out of the integration tests: `folded_row` is private, and `scan::run` hardcodes
/// case sensitivity as `!cfg!(windows)`; the classify half of the same
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

// The protective half, on the platform that cannot refuse a real name: the two lists `crate::walk`
// reports are built by hand here, exactly as that walk builds them, because no Linux filesystem can put
// an unpaired surrogate in a file name. What they must never do is over-reach — every one of these tests
// names the paths that stay visible alongside the refused one.
//
// The refusal has two arms and they are not interchangeable: a refused **file** suppresses its own `rel`
// alone, and a refused **directory** suppresses its whole subtree by prefix. Getting either wrong costs
// a silent overwrite (too narrow) or a catalog that stops noticing real deletions (too wide).

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
/// bytes no `String` can — which is the whole point of the twin cases: `raw(b"caf\xE9")` and
/// `raw(b"caf\xE8")` are two paths whose `display` fields are one string.
fn raw(bytes: &[u8]) -> UnknownPath {
    UnknownPath {
        display: String::from_utf8_lossy(bytes).into_owned(),
        bytes: bytes.to_vec(),
    }
}

/// The premise every twin case below rests on, asserted instead of assumed: `left` and `right` are distinct
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
/// whose refusal entries are already lossy text. Two names keep that difference visible at the call site.
fn display_keyed_unknown(display: &str) -> UnknownPath {
    UnknownPath {
        display: display.to_string(),
        bytes: key(display),
    }
}

/// A registry row for `rel` with one note behind it: the shape [`known_rows`] counts as **hidden**, and
/// the only shape that can hold [`PARSE_VERSION`] back.
fn plant_note(db: &Db, rel: &str) {
    upsert_file(db, &snap(rel), &Path::new("/ws").join(rel), 1).expect("test setup: registry row");
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
/// new rules over it would label rows an older build wrote as current. `known_rows` keeps the row out
/// of `classify`'s reach **and** counts it here — one comparison, both halves of the promise.
///
/// This case is worth pinning because the reading of it was argued rather than obvious: on paper its rows
/// are ambiguous (`snap` keys from display text, so they are). Running it showed the other half of the
/// precedence rule: the entry [`crate::walk`] would record for this refusal is built by
/// [`display_keyed_unknown`], whose **bytes** equal those rows' keys, so the byte-keyed arm answers first
/// and precedence keeps them charged in the hidden count and unmarked. That is pinned below rather than
/// assumed: listing their own paths readable does not move them.
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

    // Precedence, on the arm pair that overlaps for the permanent class. These rows are ambiguous by
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
/// directory whose bytes it cannot tell apart is **marked** — that is what the fallback now does with it,
/// instead of deleting it from the scan's view. With nothing listed the mark refuses admission (today's
/// pre-drop) and charges source 2; with a listed readable path spelling the row exactly and carrying its
/// stored hash the row is **in `known`, marked, and unremovable** — `classify` still calls it `Removed`, and
/// [`strip_marked_removals`] is what stops that stale-ing a live note.
///
/// The same display spelling keyed from **real** bytes is a row the walk re-keyed, and stays in `known` on
/// its bytes: that is the twin fix surviving the fallback, asserted rather than promised. This case is a
/// contract update rather than a fence, and it is one: the assertions below used to read
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

    // What marking buys: a listed readable path that spells the row exactly **and** carries its
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

// The two matchers over hand-built `UnknownPath`s whose **bytes** differ while their display text does
// not. A Linux filesystem refuses no name, so the refused arm is hand-built here exactly as the refusal
// tests above build it — and the unreadable arm is hand-built for a reason those did not need: on Unix
// the twin pair `caf\xE9` / `caf\xE8` is reachable today, and only raw bytes can spell it.

/// Byte near-twins are different paths. Neither matcher may let `caf\xE9` cover `caf\xE8`, whose display
/// spelling is identical — and that single collapse is what hid a readable subtree from `known`, so a
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
    // Exact equality is what keeps all four of these visible, and bytes are what makes the first of
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
/// scan saw two paths and can only name them alike. One path reaching the report twice is
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

/// The loss record's arithmetic, over **hand-built** walk lists.
///
/// A Linux filesystem refuses no name — `os_bytes` is exact — so the refusal arm has no end-to-end
/// fixture and is counted here instead, exactly as the matcher cases above and in `walk::tests` build
/// their refused entries. What this does **not** claim is that any real scan produces a non-zero
/// `unkeyable`: only Windows refuses a name, and that arm stays unexecuted here. The last
/// block pins the consequence for every run on this platform: `unkeyable` is zero, always.
#[test]
fn the_record_counts_each_loss_by_kind_and_caps_its_names() {
    // Two refused names and one unreadable directory, all with distinct display text. The counts have to
    // stay apart, because "3 paths skipped" — the sentence `skipped` prints — is precisely what the record
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
        record.ambiguous, 1,
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
    // while an unreadable directory is named in `ScanReport::skipped` as well.
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
    // standing there accusing somebody.
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

/// The exclusion, pinned on the class it exists for. A row whose key already equals its OS bytes — a name
/// genuinely holding `U+FFFD`, which is every lossy name on Windows — can be marked *and* listed unread: the walk
/// saw the path, the read failed, and `written.uncovered` charges that. Source 2 must stand down for it, or one note
/// pays twice and the gap line over-counts, which is what this exclusion exists to stop.
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

/// The answer, **counted** rather than assumed: one note behind one unknown path is charged in exactly
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
/// shape as well. That was settled in two places rather than left to luck: `known_rows` asks the byte arm
/// **first** and marks nothing it already hid (precedence, which on Windows is the difference between one charge
/// and a universal double charge), and [`unresolved_ambiguity`] skips any marked row whose key appears in the
/// found-side unreadable set. [`a_marked_row_listed_but_unread_charges_exactly_one_counter`] pins that exclusion on
/// the class it exists for; what remains here is the byte-keyed disjointness.
///
/// The two sets used to overlap for byte-keyed rows too: a note in the readable twin was hidden by its
/// unreadable twin's display spelling **and** charged as an unread write, so one note cost two charges and the
/// refresh-gap line over-counted. `ScanReport` carries no gap count, which is why this arithmetic lives
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

    // Case 2 — the overlap that byte matching closed. The readable twin's note cannot be read this scan
    // either (gone, or changed, between walk and read), so it reaches `found` as an unknown marker while
    // its unreadable twin
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
