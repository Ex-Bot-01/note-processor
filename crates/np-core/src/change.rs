//! Change classification: what a fresh scan means for the files already in the registry.
//!
//! Two rules decide everything here. `size` + `mtime` is only the **cheap trigger** that
//! says whether to look closer; the content hash is the **identity** that says what
//! happened (spec §2). And a path whose contents could not be read is *unknown*, neither
//! present nor absent, so it produces no change at all — the caller reports it under
//! `skipped` and leaves its row untouched.

use std::collections::{HashMap, HashSet};

use crate::hash::Snapshot;

/// What one path needs done to it, as reported by [`classify`].
#[derive(Debug, PartialEq)]
pub enum Change {
    /// A path nothing known matches — new file, or a previously unreadable file that can
    /// now be read.
    Added,
    /// The same identity at the same path with different contents.
    Modified,
    /// Contents we already had, under a new path. `from` is the old `rel`.
    Moved { from: String },
    /// A known path that is gone.
    Removed,
}

/// Compare what was known against what is there now and say what changed.
///
/// Output is one entry per path, sorted by `rel` ascending, where the key is the path as
/// it reads **now** — so a move is reported once, on its new path, and never also as a
/// `Removed`.
///
/// Matching uses `case_sensitive` to pick the identity: `true` (Linux, macOS) keys on
/// `rel`, `false` (Windows) keys on `case_key` (spec §11). It is an argument rather than a
/// `#[cfg]` so both rules are testable on one machine; the call site passes `!cfg!(windows)`.
///
/// The decisions, in order:
///
/// - A snapshot with an empty `hash` is a read failure, not content. Found-side markers
///   are dropped, and they also stop their known counterpart from being reported `Removed`
///   — an unreadable file is unknown, not deleted. Known-side markers are dropped, so they
///   can neither produce a `Removed` nor lend their hash to a move; a readable file sitting
///   on such a path is therefore `Added`, which is what fills the empty row in.
/// - A matched pair whose `size` **and** `mtime` both equal `found`'s emits nothing without
///   consulting the hashes at all.
/// - Otherwise the hashes decide: different hash at the same identity is `Modified`; equal
///   hash means the note itself did not change, so nothing is emitted even when the
///   spelling of `rel` did (a case-only rename on Windows) and even when `mtime` moved.
/// - Of what is left over, an added path becomes `Moved { from }` only when a removed path
///   carries **that same hash**, nothing still present carries it, and exactly one added
///   path carries it. Same content at two paths is two files, not a move of each other, and
///   a moved file whose contents also changed has two different hashes, so it stays
///   `Added` + `Removed` — the hash is the identity. When several removed paths could claim
///   one added path, the one sharing the most leading directories wins, tie-broken by `rel`
///   ascending; the rest stay `Removed`.
pub fn classify(
    known: &[Snapshot],
    found: &[Snapshot],
    case_sensitive: bool,
) -> Vec<(String, Change)> {
    // Read failures first: their paths must not appear in the output in any direction.
    let unreadable: HashSet<&str> = found
        .iter()
        .filter(|s| s.hash.is_empty())
        .map(|s| key_of(s, case_sensitive))
        .collect();
    let known_index = index_by_key(
        known
            .iter()
            .filter(|s| !s.hash.is_empty() && !unreadable.contains(key_of(s, case_sensitive))),
        case_sensitive,
    );
    let found_index = index_by_key(found.iter().filter(|s| !s.hash.is_empty()), case_sensitive);

    let mut changes: Vec<(String, Change)> = Vec::new();
    // Hashes the workspace still holds at a path that did not go away. A move may never
    // claim content that is still sitting where it was.
    let mut present_hashes: HashSet<&str> = HashSet::new();
    let mut unmatched_known: Vec<&Snapshot> = Vec::new();
    let mut unmatched_found: Vec<&Snapshot> = Vec::new();

    for (identity, knowns) in &known_index {
        let Some(founds) = found_index.get(*identity) else {
            unmatched_known.extend(knowns.iter().copied());
            continue;
        };
        // Same identity on both sides: pair them positionally. Only case-insensitive
        // matching can put several snapshots under one identity, and each bucket is in
        // `rel` order, so the pairing does not depend on the caller's input order.
        for (before, after) in knowns.iter().zip(founds.iter()) {
            present_hashes.insert(before.hash.as_str());
            if before.size == after.size && before.mtime == after.mtime {
                continue;
            }
            if before.hash != after.hash {
                changes.push((after.rel.clone(), Change::Modified));
            }
        }
        unmatched_known.extend(knowns.iter().skip(founds.len()).copied());
        unmatched_found.extend(founds.iter().skip(knowns.len()).copied());
    }
    for (identity, founds) in &found_index {
        if !known_index.contains_key(*identity) {
            unmatched_found.extend(founds.iter().copied());
        }
    }

    let claimants = group_by_hash(&unmatched_known);
    let added_count = count_by_hash(&unmatched_found);

    // Move claims are resolved in `rel` order so the outcome never follows hash order.
    unmatched_found.sort_by_key(|s| s.rel.as_str());
    let mut moved_from: HashSet<&str> = HashSet::new();
    for added in &unmatched_found {
        let hash = added.hash.as_str();
        let claim = match claimants.get(hash) {
            // Content that still exists somewhere else, or that more than one new path
            // shares, is a new file here — not the same note travelling.
            Some(group) if !present_hashes.contains(hash) && added_count.get(hash) == Some(&1) => {
                closest_to(group, added)
            }
            _ => None,
        };
        match claim {
            Some(before) => {
                moved_from.insert(before.rel.as_str());
                changes.push((
                    added.rel.clone(),
                    Change::Moved {
                        from: before.rel.clone(),
                    },
                ));
            }
            None => changes.push((added.rel.clone(), Change::Added)),
        }
    }

    for before in &unmatched_known {
        if !moved_from.contains(before.rel.as_str()) {
            changes.push((before.rel.clone(), Change::Removed));
        }
    }

    changes.sort_by(|a, b| a.0.cmp(&b.0));
    changes
}

/// The identity a path is matched on: its exact spelling where the filesystem keeps two
/// names that differ only in case, its folded spelling where it keeps one file.
fn key_of(snapshot: &Snapshot, case_sensitive: bool) -> &str {
    if case_sensitive {
        &snapshot.rel
    } else {
        &snapshot.case_key
    }
}

/// Snapshots grouped by identity, each group in `rel` ascending order.
fn index_by_key<'a>(
    snapshots: impl Iterator<Item = &'a Snapshot>,
    case_sensitive: bool,
) -> HashMap<&'a str, Vec<&'a Snapshot>> {
    let mut index: HashMap<&str, Vec<&Snapshot>> = HashMap::new();
    for snapshot in snapshots {
        index
            .entry(key_of(snapshot, case_sensitive))
            .or_default()
            .push(snapshot);
    }
    for group in index.values_mut() {
        group.sort_by_key(|s| s.rel.as_str());
    }
    index
}

/// Snapshots grouped by their content hash.
fn group_by_hash<'a>(snapshots: &[&'a Snapshot]) -> HashMap<&'a str, Vec<&'a Snapshot>> {
    let mut groups: HashMap<&str, Vec<&Snapshot>> = HashMap::new();
    for snapshot in snapshots {
        groups
            .entry(snapshot.hash.as_str())
            .or_default()
            .push(*snapshot);
    }
    groups
}

/// How many snapshots carry each content hash.
fn count_by_hash<'a>(snapshots: &[&'a Snapshot]) -> HashMap<&'a str, usize> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for snapshot in snapshots {
        *counts.entry(snapshot.hash.as_str()).or_default() += 1;
    }
    counts
}

/// The candidate most plausibly the same file as `added`: the one sharing the most leading
/// directories with it, tie-broken by `rel` ascending so the pick is not hash order.
fn closest_to<'a>(candidates: &[&'a Snapshot], added: &Snapshot) -> Option<&'a Snapshot> {
    let mut best: Option<&'a Snapshot> = None;
    for candidate in candidates {
        let better = match best {
            None => true,
            Some(held) => {
                let mine = shared_dirs(&candidate.rel, &added.rel);
                let theirs = shared_dirs(&held.rel, &added.rel);
                mine > theirs || (mine == theirs && candidate.rel < held.rel)
            }
        };
        if better {
            best = Some(candidate);
        }
    }
    best
}

/// How many leading directories two `/`-separated paths share. The file name never counts,
/// and a path at the workspace root shares none with anybody.
fn shared_dirs(a: &str, b: &str) -> usize {
    let Some((a_dir, _)) = a.rsplit_once('/') else {
        return 0;
    };
    let Some((b_dir, _)) = b.rsplit_once('/') else {
        return 0;
    };
    let mut shared = 0;
    for (x, y) in a_dir.split('/').zip(b_dir.split('/')) {
        if x != y {
            break;
        }
        shared += 1;
    }
    shared
}
