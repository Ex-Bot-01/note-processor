//! Change classification: what a fresh scan means for the files we already know.
//!
//! Pure by design — every `Snapshot` is built by hand, so no test touches the
//! filesystem. `case_key` is always the lowercased `rel`, which is how `walk` fills it.

use np_core::change::{classify, Change};
use np_core::hash::Snapshot;

/// Linux and macOS semantics: the filesystem distinguishes case, so `rel` identifies a
/// file. This is what the scan passes as `case_sensitive`.
const CASE_SENSITIVE: bool = true;
/// Windows semantics: the filesystem folds case, so `case_key` identifies a file.
/// Passed explicitly rather than reached through `#[cfg]`, so it runs on this machine.
const CASE_INSENSITIVE: bool = false;

/// A snapshot as `walk` + `sha256_file` would produce it.
fn snap(rel: &str, hash: &str, size: u64, mtime: i64) -> Snapshot {
    Snapshot {
        rel: rel.to_string(),
        case_key: rel.to_lowercase(),
        hash: hash.to_string(),
        size,
        mtime,
    }
}

#[test]
fn unchanged_files_produce_no_change() {
    let known = vec![snap("a.md", "h1", 10, 100), snap("b/c.md", "h2", 20, 200)];

    let result = classify(&known, &known, CASE_SENSITIVE);
    assert!(result.is_empty(), "identical input: {result:?}");

    // A touched file is still the same note: `mtime` alone is not a change, because
    // identity is the hash and re-processing identical content breaks idempotency.
    let touched = vec![
        snap("a.md", "h1", 10, 9_999),
        snap("b/c.md", "h2", 20, 9_999),
    ];

    let result = classify(&known, &touched, CASE_SENSITIVE);
    assert!(result.is_empty(), "same hashes: {result:?}");
}

/// The cheap trigger is a decision to **not** look, so it can miss a change: two writes
/// inside the same second with equal length differ in content and still report nothing.
/// Pinned because the scan relies on skipping the hash here, and the trade-off must be a
/// stated one rather than a surprise.
#[test]
fn equal_size_and_mtime_skip_the_hash_entirely() {
    let known = vec![snap("a.md", "h1", 5, 100)];
    let found = vec![snap("a.md", "h2", 5, 100)];

    let result = classify(&known, &found, CASE_SENSITIVE);
    assert!(result.is_empty(), "size+mtime unchanged: {result:?}");
}

#[test]
fn same_hash_new_path_is_moved() {
    let known = vec![snap("a/x.md", "h1", 5, 100)];
    let found = vec![snap("b/x.md", "h1", 5, 100)];

    // One entry, on the new path, and no `Removed` next to it: identity is preserved.
    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![(
            "b/x.md".to_string(),
            Change::Moved {
                from: "a/x.md".to_string()
            }
        )]
    );
}

#[test]
fn case_only_rename_on_windows_is_not_a_move() {
    let known = vec![snap("Notes/x.md", "h1", 5, 100)];
    // Different `mtime`, so the cheap trigger lets the comparison reach the hashes:
    // passing must come from folding the key, not from skipping the check.
    let found = vec![snap("notes/x.md", "h1", 5, 101)];

    let result = classify(&known, &found, CASE_INSENSITIVE);
    assert!(result.is_empty(), "case-only rename: {result:?}");
}

#[test]
fn case_only_rename_on_a_case_sensitive_fs_is_a_move() {
    // Same input, opposite filesystem rule: two paths holding one content is exactly what
    // a move means. This test is what makes the `case_sensitive` argument discriminated
    // rather than decorative.
    let known = vec![snap("Notes/x.md", "h1", 5, 100)];
    let found = vec![snap("notes/x.md", "h1", 5, 101)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![(
            "notes/x.md".to_string(),
            Change::Moved {
                from: "Notes/x.md".to_string()
            }
        )]
    );
}

#[test]
fn content_change_is_modified() {
    let known = vec![snap("a.md", "h1", 5, 100)];
    let found = vec![snap("a.md", "h2", 7, 200)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![("a.md".to_string(), Change::Modified)]
    );
}

#[test]
fn missing_file_is_removed() {
    let known = vec![snap("a.md", "h1", 5, 100), snap("b.md", "h2", 6, 100)];
    let found = vec![snap("a.md", "h1", 5, 100)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![("b.md".to_string(), Change::Removed)]
    );
}

#[test]
fn new_file_is_added() {
    let known = vec![snap("a.md", "h1", 5, 100)];
    let found = vec![snap("a.md", "h1", 5, 100), snap("new.md", "h2", 9, 150)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![("new.md".to_string(), Change::Added)]
    );
}

#[test]
fn stale_hash_never_produced_when_read_fails() {
    // An empty `hash` is the caller's marker for "could not read this file": the path is
    // unknown, so it must produce no entry at all — neither Added/Modified nor Removed,
    // because the caller keeps the previous row and reports the path under `skipped`.
    let known = vec![snap("a.md", "h1", 5, 100), snap("b.md", "h2", 6, 100)];
    let found = vec![snap("a.md", "h1", 5, 100), snap("b.md", "", 0, 0)];

    let result = classify(&known, &found, CASE_SENSITIVE);
    assert!(result.is_empty(), "unreadable file: {result:?}");

    // A file that could not be read is not added either, even when nothing was known
    // about its path.
    let found = vec![
        snap("a.md", "h1", 5, 100),
        snap("b.md", "h2", 6, 100),
        snap("unreadable-new.md", "", 0, 0),
    ];

    let result = classify(&known, &found, CASE_SENSITIVE);
    assert!(result.is_empty(), "unreadable new file: {result:?}");
}

#[test]
fn unreadable_known_snapshot_never_produces_removed_or_moved() {
    // The other side of the same marker: a row whose stored hash is empty knows nothing,
    // so its disappearance is not evidence of a deletion.
    let known = vec![snap("a.md", "", 0, 0)];

    let result = classify(&known, &[], CASE_SENSITIVE);
    assert!(result.is_empty(), "unreadable known file: {result:?}");

    // And an added file cannot claim it as the source of a move.
    let found = vec![snap("b.md", "h9", 3, 50)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![("b.md".to_string(), Change::Added)]
    );
}

#[test]
fn a_file_that_became_readable_is_added() {
    // The registry row exists but holds no hash, because the last scan could not read the
    // file. Now it can, and there is no stored digest to compare against: `Added` is what
    // fills the empty row in.
    let known = vec![snap("a.md", "", 0, 0)];
    let found = vec![snap("a.md", "h1", 5, 100)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![("a.md".to_string(), Change::Added)]
    );
}

#[test]
fn duplicate_content_is_not_a_move() {
    // Two notes with byte-identical content, one of them renamed. The content still
    // exists at a path that never went away, so nothing may be reported as a move.
    let known = vec![snap("a.md", "same", 4, 10), snap("b.md", "same", 4, 11)];
    let found = vec![snap("a.md", "same", 4, 10), snap("c.md", "same", 4, 12)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![
            ("b.md".to_string(), Change::Removed),
            ("c.md".to_string(), Change::Added),
        ]
    );
}

#[test]
fn moved_and_edited_is_added_and_removed() {
    // A rename plus an edit: two different hashes, so the identity rule sees two files
    // where the human sees one note travelling. Only unchanged content may move.
    let known = vec![snap("old/x.md", "h1", 5, 10)];
    let found = vec![snap("new/x.md", "h2", 9, 20)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![
            ("new/x.md".to_string(), Change::Added),
            ("old/x.md".to_string(), Change::Removed),
        ]
    );
}

#[test]
fn two_new_paths_sharing_a_hash_are_not_moves() {
    // One known path, two new ones carrying its content: which new path is "the same
    // note" has no honest answer, so nothing moves.
    let known = vec![snap("old.md", "dup", 4, 10)];
    let found = vec![snap("p.md", "dup", 4, 20), snap("q.md", "dup", 4, 21)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![
            ("old.md".to_string(), Change::Removed),
            ("p.md".to_string(), Change::Added),
            ("q.md".to_string(), Change::Added),
        ]
    );
}

#[test]
fn two_claimants_for_one_path_prefer_the_closest_directory() {
    // Two known paths hold `dup`, one found path holds it now. The claimant sharing more
    // directories with the new path wins — here that is the alphabetically *later* one, so
    // the distance rule cannot be an accident of the tie-break.
    let known = vec![
        snap("alpha/y.md", "dup", 4, 11),
        snap("notes/deep/x.md", "dup", 4, 10),
    ];
    let found = vec![snap("notes/new/x.md", "dup", 4, 12)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![
            ("alpha/y.md".to_string(), Change::Removed),
            (
                "notes/new/x.md".to_string(),
                Change::Moved {
                    from: "notes/deep/x.md".to_string()
                }
            ),
        ]
    );

    // Equally distant claimants are broken by `rel` ascending, so the result does not
    // depend on the order the caller happened to supply.
    let known = vec![
        snap("notes/b/x.md", "dup", 4, 10),
        snap("notes/a/x.md", "dup", 4, 11),
    ];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![
            ("notes/b/x.md".to_string(), Change::Removed),
            (
                "notes/new/x.md".to_string(),
                Change::Moved {
                    from: "notes/a/x.md".to_string()
                }
            ),
        ]
    );
}

#[test]
fn snapshots_sharing_a_folded_key_are_paired_in_rel_order() {
    // A case-insensitive key can hold two snapshots when a workspace was written on a
    // case-sensitive filesystem. Each side is paired in `rel` order, and the two sides are
    // supplied in different orders — which is exactly what the scan produces, since one
    // list comes from SQLite and the other from the walk.
    let known = vec![snap("a.md", "h2", 7, 20), snap("A.md", "h1", 5, 10)];
    let found = vec![snap("A.md", "h1", 5, 10), snap("a.md", "h3", 9, 30)];

    assert_eq!(
        classify(&known, &found, CASE_INSENSITIVE),
        vec![("a.md".to_string(), Change::Modified)]
    );
}

#[test]
fn one_entry_per_path_and_sorted_by_rel() {
    // Inputs deliberately out of order, so the sort is doing the work.
    let known = vec![
        snap("z.md", "h1", 1, 1),
        snap("m.md", "h2", 2, 2),
        snap("a.md", "h3", 3, 3),
    ];
    let found = vec![
        snap("m.md", "h2-new", 2, 9),
        snap("n.md", "h4", 4, 4),
        snap("a.md", "h3", 3, 3),
    ];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![
            ("m.md".to_string(), Change::Modified),
            ("n.md".to_string(), Change::Added),
            ("z.md".to_string(), Change::Removed),
        ]
    );
}

#[test]
fn a_matched_pair_protects_its_new_hash_from_a_move_claim() {
    // `a.md` was edited into content `g2`, and `c.md` is a copy of that same new content while
    // `b.md` (also `g2`) disappears. Content `g2` is demonstrably still in the workspace, so
    // `c.md` is a second file holding it — not `b.md` travelling, which would rewrite a live
    // row's `rel` and give one note two paths to the same bytes.
    let known = vec![snap("a.md", "g1", 10, 100), snap("b.md", "g2", 20, 200)];
    let found = vec![snap("a.md", "g2", 30, 300), snap("c.md", "g2", 30, 300)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![
            ("a.md".to_string(), Change::Modified),
            ("b.md".to_string(), Change::Removed),
            ("c.md".to_string(), Change::Added),
        ]
    );
}

#[test]
fn an_unreadable_path_protects_its_known_hash_from_a_move_claim() {
    // `a.md` could not be read this time (empty hash marker), `b.md` is gone and `c.md` carries
    // the hash all three used to share. `a.md`'s content is unknown, not absent, so nothing may
    // claim to have moved from `b.md`: `c.md` is added and `b.md` stays out of the output as a
    // move source.
    let known = vec![snap("a.md", "dup", 10, 100), snap("b.md", "dup", 10, 100)];
    let found = vec![snap("a.md", "", 0, 0), snap("c.md", "dup", 10, 300)];

    assert_eq!(
        classify(&known, &found, CASE_SENSITIVE),
        vec![
            ("b.md".to_string(), Change::Removed),
            ("c.md".to_string(), Change::Added),
        ]
    );
}
