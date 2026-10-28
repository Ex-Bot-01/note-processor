//! Change classification: what a fresh scan means for the files already in the registry.
//!
//! Two rules decide everything here. `size` + `mtime` is only the **cheap trigger** that
//! says whether to look closer; the content hash is the **identity** that says what
//! happened (spec §2). And a path whose contents could not be read is *unknown*, neither
//! present nor absent, so it produces no change at all — the caller reports it under
//! `skipped` and leaves its row untouched.

use std::collections::{HashMap, HashSet};

use crate::hash::Snapshot;
use crate::path_keys::RelPath;

/// What one path needs done to it, as reported by [`classify`].
#[derive(Debug, PartialEq)]
pub enum Change {
    /// A path nothing known matches — new file, or a previously unreadable file that can
    /// now be read.
    Added,
    /// The same identity at the same path with different contents.
    Modified,
    /// Contents we already had, under a new path. `from` is the old path, in both spellings:
    /// [`db::apply_move`](crate::db::apply_move) finds the row by its bytes and rewrites the display
    /// pair beside them, so a move named by text alone could not be carried out.
    Moved { from: RelPath },
    /// A known path that is gone.
    Removed,
}

/// Compare what was known against what is there now and say what changed.
///
/// Output is one entry per path, sorted by byte key ascending, where the key is the path as it reads
/// **now** — so a move is reported once, on its new path, and never also as a `Removed`. Each entry
/// names its path as a [`RelPath`], because this list carries an identity decision into the write pass:
/// text alongside for the report, bytes for everything that has to find the row (F13 §4.1).
///
/// Matching uses `case_sensitive` to pick the identity: `true` (Linux, macOS) keys on
/// `rel`, `false` (Windows) keys on `case_key` (spec §11). It is an argument rather than a
/// `#[cfg]` so both rules are testable on one machine; the call site passes `!cfg!(windows)`.
///
/// Input contract: each side holds **at most one snapshot per identity**, which is what
/// [`Snapshot::new`] over one [`crate::walk::DiscoveredFile`] gives a caller — two snapshots of one
/// path would be paired positionally and could report the pair as a move. Both lists come from the same
/// walk, so `case_bytes` is every entry's own `rel_bytes` ASCII-folded and `case_key` its own `rel`
/// lowercased; a snapshot that breaks either rule matches files it is not.
///
/// The decisions, in order:
///
/// - A snapshot with an empty `hash` is a read failure, not content. Found-side markers
///   are dropped, and they also stop their known counterpart from being reported `Removed`
///   — an unreadable file is unknown, not deleted. Known-side markers are dropped, so they
///   can neither produce a `Removed` nor lend their hash to a move; a readable file sitting
///   on such a path is therefore `Added`, which is what fills the empty row in.
///
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
///   one added path, the one sharing the most leading directories wins, tie-broken by byte key
///   ascending; the rest stay `Removed`.
pub fn classify(
    known: &[Snapshot],
    found: &[Snapshot],
    case_sensitive: bool,
) -> Vec<(RelPath, Change)> {
    classify_with_identities(known, found, case_sensitive, &FileIdentities::none())
}

/// [`classify`] with the OS's own opinion about which leftovers are one and the same file.
///
/// Identity **adds** matches and never removes one; see [`FileIdentities`].
pub fn classify_with_identities(
    known: &[Snapshot],
    found: &[Snapshot],
    case_sensitive: bool,
    identity: &FileIdentities,
) -> Vec<(RelPath, Change)> {
    // Read failures first: their paths must not appear in the output in any direction.
    let unreadable: HashSet<&[u8]> = found
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

    let mut changes: Vec<(RelPath, Change)> = Vec::new();
    // Hashes the workspace still holds at a path that did not go away. A move may never
    // claim content that is still sitting where it was — which includes both spellings of a
    // matched pair (its old bytes and the new ones written to the same path) and the known hash
    // behind a path that could only not be **read**, since unreadable content has not departed.
    let mut present_hashes: HashSet<&str> = known
        .iter()
        .filter(|s| !s.hash.is_empty() && unreadable.contains(key_of(s, case_sensitive)))
        .map(|s| s.hash.as_str())
        .collect();
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
            present_hashes.insert(after.hash.as_str());
            if before.size == after.size && before.mtime == after.mtime {
                continue;
            }
            if before.hash != after.hash {
                changes.push((path_of(after), Change::Modified));
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

    // Identity pairing (design-a4 §3), and it runs **here, over the leftovers only**, not as a first route.
    // A row whose own path was listed matched that path positionally above and is not eligible for anything; a
    // listed file already consumed by such a match must not be paired again, however uniquely its id reads —
    // pairing it would put two known rows on one file, which is the duplicate this whole item exists to remove.
    // So both sides have to still be unclaimed, and each pair consumes itself before any content claim is
    // counted: a paired-away row must not also lend its hash to some third path's `Moved`.
    let leftover_found: HashMap<&[u8], &Snapshot> = unmatched_found
        .iter()
        .map(|s| (s.rel_bytes.as_slice(), *s))
        .collect();
    // Byte-key order, so the outcome never follows hash-map iteration order.
    let mut pairing: Vec<&Snapshot> = unmatched_known
        .iter()
        .copied()
        .filter(|before| identity.eligible(before))
        .collect();
    pairing.sort_by_key(|s| s.rel_bytes.as_slice());
    let mut paired_known: Vec<&[u8]> = Vec::new();
    let mut paired_found: Vec<&[u8]> = Vec::new();
    for before in pairing {
        let Some(listed_key) = identity.listed_for(before) else {
            continue;
        };
        let Some(after) = leftover_found.get(listed_key).copied() else {
            continue; // consumed by an identity match, or never a leftover: nothing to pair with
        };
        changes.push((
            path_of(after),
            Change::Moved {
                from: path_of(before),
            },
        ));
        paired_known.push(before.rel_bytes.as_slice());
        paired_found.push(listed_key);
    }
    if !paired_known.is_empty() {
        unmatched_known.retain(|s| !paired_known.contains(&s.rel_bytes.as_slice()));
        unmatched_found.retain(|s| !paired_found.contains(&s.rel_bytes.as_slice()));
    }

    let claimants = group_by_hash(&unmatched_known);
    let added_count = count_by_hash(&unmatched_found);

    // Move claims are resolved in byte-key order so the outcome never follows hash order, and never
    // follows the display spelling two twins happen to share.
    unmatched_found.sort_by_key(|s| s.rel_bytes.as_slice());
    let mut moved_from: HashSet<&[u8]> = HashSet::new();
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
                moved_from.insert(before.rel_bytes.as_slice());
                changes.push((
                    path_of(added),
                    Change::Moved {
                        from: path_of(before),
                    },
                ));
            }
            None => changes.push((path_of(added), Change::Added)),
        }
    }

    for before in &unmatched_known {
        if !moved_from.contains(before.rel_bytes.as_slice()) {
            changes.push((path_of(before), Change::Removed));
        }
    }

    changes.sort_by(|a, b| a.0.rel_bytes.cmp(&b.0.rel_bytes));
    changes
}

/// The identity a path is matched on: its exact bytes where the filesystem keeps two names that differ
/// only in ASCII case, the ASCII fold of those bytes where it keeps one file.
///
/// Bytes, not display text (F13 §4.1): `caf\xe9.md` and `caf\xe8.md` spell themselves identically once
/// decoded, so a `&str` key here is where a twin pair used to die — one bucket, one positional pair,
/// one file dropped.
fn key_of(snapshot: &Snapshot, case_sensitive: bool) -> &[u8] {
    if case_sensitive {
        &snapshot.rel_bytes
    } else {
        &snapshot.case_bytes
    }
}

/// The two spellings of one snapshot, as the write pass needs them.
fn path_of(snapshot: &Snapshot) -> RelPath {
    RelPath::new(snapshot.rel_bytes.clone(), snapshot.rel.clone())
}

/// Snapshots grouped by identity, each group in byte-key ascending order.
fn index_by_key<'a>(
    snapshots: impl Iterator<Item = &'a Snapshot>,
    case_sensitive: bool,
) -> HashMap<&'a [u8], Vec<&'a Snapshot>> {
    let mut index: HashMap<&[u8], Vec<&Snapshot>> = HashMap::new();
    for snapshot in snapshots {
        index
            .entry(key_of(snapshot, case_sensitive))
            .or_default()
            .push(snapshot);
    }
    for group in index.values_mut() {
        group.sort_by_key(|s| s.rel_bytes.as_slice());
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
/// directories with it, tie-broken by byte key ascending so the pick is not hash order and not whichever
/// spelling sorts first once two rows read alike.
fn closest_to<'a>(candidates: &[&'a Snapshot], added: &Snapshot) -> Option<&'a Snapshot> {
    let mut best: Option<&'a Snapshot> = None;
    for candidate in candidates {
        let better = match best {
            None => true,
            Some(held) => {
                let mine = shared_dirs(&candidate.rel_bytes, &added.rel_bytes);
                let theirs = shared_dirs(&held.rel_bytes, &added.rel_bytes);
                mine > theirs || (mine == theirs && candidate.rel_bytes < held.rel_bytes)
            }
        };
        if better {
            best = Some(candidate);
        }
    }
    best
}

/// Which leftovers the OS says are one and the same file — computed **once** per scan, from every row and every
/// listed path, and shared by both places that consult it (design-a4 §3, §4).
///
/// An identity is *eligible* when exactly one known row carries it and exactly one listed path carries it, and no
/// other listed path does. Anything else abstains: two rows on one id means a hard link or a corrupted registry,
/// and pairing either would duplicate a note; two paths on one id is the same hard link seen from the walk. A row
/// outside the caller's scope never enters it, which is what keeps an ordinary rename out of this feature's reach —
/// its old path still goes `Removed` + `Added`, with the catalog bytes unchanged.
///
/// **It only ever adds a match.** Disagreement — a row whose id matches nothing, or whose file now reports a
/// different id — is not evidence of absence: ids legitimately change across copies, restores, rsync and containers,
/// and a route that could veto would turn "this filesystem recycles inode numbers" into mass deletion. Unknown
/// (`None`) is likewise no opinion, which is what every non-Unix platform and every pre-migration row reports.
#[derive(Debug, Default)]
pub struct FileIdentities {
    /// Ids eligible for pairing: in scope on the row side and unique on both.
    eligible: HashSet<Vec<u8>>,
    /// The one listed path per eligible id, as its `rel_bytes`.
    listed: HashMap<Vec<u8>, Vec<u8>>,
}

impl FileIdentities {
    /// The empty opinion — what [`classify`] passes, meaning "no identity information anywhere". Every route
    /// then behaves exactly as it did before this feature existed, which is what lets that entry point stay a
    /// one-line delegate and keeps every existing caller's semantics asserted rather than restated.
    fn none() -> Self {
        Self::default()
    }

    /// Compute the tally once.
    ///
    /// `all_known` and `all_listed` are **every** row and every listed path, because uniqueness is a fact about
    /// the whole workspace — an id carried by a second row has to disqualify the pairing even when that row is out
    /// of scope. `in_scope` decides which rows may be paired at all, and belongs to the caller: the population this
    /// feature is scoped to (display-derived keys) is derived from a predicate in `scan.rs`, and moving it here
    /// would widen a public API to serve one call site (design-a4 §3).
    pub fn build(
        all_known: &[Snapshot],
        all_listed: &[Snapshot],
        in_scope: impl Fn(&Snapshot) -> bool,
    ) -> Self {
        let mut known_count: HashMap<&[u8], usize> = HashMap::new();
        for row in all_known {
            if let Some(id) = &row.file_id {
                *known_count.entry(id.as_slice()).or_default() += 1;
            }
        }
        let mut listed_count: HashMap<&[u8], usize> = HashMap::new();
        let mut listed_one: HashMap<&[u8], &[u8]> = HashMap::new();
        for file in all_listed {
            if let Some(id) = &file.file_id {
                *listed_count.entry(id.as_slice()).or_default() += 1;
                listed_one.insert(id.as_slice(), file.rel_bytes.as_slice());
            }
        }
        let mut out = FileIdentities::default();
        for row in all_known {
            let Some(id) = &row.file_id else { continue };
            if !in_scope(row) {
                continue;
            }
            // Two-sided uniqueness, checked once here so that admission and pairing cannot disagree about who is
            // eligible — the coupling design-a4 §3 names. A row admitted without a partner would have its
            // `Removed` stripped and gain no `Moved`: permanent duplicate plus permanent charge forever.
            if known_count.get(id.as_slice()) != Some(&1) {
                continue;
            }
            if listed_count.get(id.as_slice()) != Some(&1) {
                continue;
            }
            let Some(listed) = listed_one.get(id.as_slice()) else {
                continue;
            };
            out.eligible.insert(id.clone());
            out.listed.insert(id.clone(), listed.to_vec());
        }
        out
    }

    /// Whether this row's identity is eligible: in scope, one row, one path. This is the **one** question both
    /// plug-in points ask.
    pub fn eligible(&self, row: &Snapshot) -> bool {
        row.file_id
            .as_deref()
            .is_some_and(|id| self.eligible.contains(id))
    }

    /// The listed path this row pairs with, when it pairs with one.
    pub fn listed_for(&self, row: &Snapshot) -> Option<&[u8]> {
        if !self.eligible(row) {
            return None;
        }
        self.listed.get(row.file_id.as_deref()?).map(Vec::as_slice)
    }
}

/// How many leading directories two `/`-separated byte paths share. The file name never counts,
/// and a path at the workspace root shares none with anybody.
fn shared_dirs(a: &[u8], b: &[u8]) -> usize {
    let (Some(a_dir), Some(b_dir)) = (dirs_of(a), dirs_of(b)) else {
        return 0;
    };
    let mut shared = 0;
    for (x, y) in a_dir
        .split(|byte| *byte == b'/')
        .zip(b_dir.split(|byte| *byte == b'/'))
    {
        if x != y {
            break;
        }
        shared += 1;
    }
    shared
}

/// The directory part of a byte path — everything before its last `/`, or `None` when the name sits at
/// the workspace root and therefore shares no directory with anybody.
fn dirs_of(path: &[u8]) -> Option<&[u8]> {
    let last = path.iter().rposition(|byte| *byte == b'/')?;
    Some(&path[..last])
}
