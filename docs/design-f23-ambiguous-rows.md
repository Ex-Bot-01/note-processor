# F23 — stop deleting the ambiguous row, and let content decide what it can

Status: design for `m6/ambiguous-rows`. Backlog: `docs/backlog-m2-followups.md` F23 (open by decision since M3, three
candidates costed there). Depends on F19's merge machinery only in spirit; touches no schema. Prior art: F20 §4 (the
fallback that hides these rows), F13 §7 (name a conflict, never pick a winner), `change::classify`'s hash pairing and its
`present_hashes` guard.

## 1. What the backlog established, and where it stops

Two workspaces hand `known_rows` **identical inputs** and need opposite answers:

- **N′** — the row's own file *is* the readable path. Today the fallback hides the row, the listed file pairs with nothing,
  indexes as `Added`, and the workspace ends with two live `files` rows and two `notes` rows for one physical file, two
  catalog entries, ids whose headings and links can diverge, and `PARSE_VERSION` withheld forever. No scan clears it.
- **M** — the row's own file is *inside* the blocked directory and the readable path is a different, unindexed twin. Here
  hiding is right: `added: 1`, the note survives, the version stays withheld.

The backlog's conclusion — "**a lossy key carries no byte spelling, so nothing computable at `known_rows` distinguishes
the two**" — is correct, and the narrow patch ("don't hide when the found set carries that display text") was probed and
rejected: it turns M into `added: 1, removed: 1`, staling a live note. This design does not contradict any of that. It
changes a different thing: **where the decision is made.**

`known_rows` genuinely cannot decide. But it currently decides by *deleting the row from the scan's view*, which also
removes it from `change::classify` — the one place in the pipeline that holds the other half of the information: **the
digest of the bytes on disk** for every path the walk could read. Rows carry `files.hash`. So the two cases are not
indistinguishable in general; they are indistinguishable to a predicate that is denied the comparison.

## 2. The rule

**Ambiguity stays exactly as wide as it is today. Its consequence changes from deletion to a mark: an ambiguous row may
not be removed, and it may be claimed by content.**

- **Same predicate.** `key_is_display_derived(rel, rel_bytes) && under_unreadable(rel, unreadable_display)` — unchanged,
  including its deliberate width (one `caf\u{FFFD}` entry hides the row whichever twin its bytes came from). No new
  heuristic for *who is ambiguous*.
- **Ambiguous rows stay in `known`.** They reach `classify` and can be paired by hash with a listed path, which produces
  the ordinary `Moved` and lets `apply_move` re-key `rel_bytes` from real bytes — the same route F20 §4 named as the only
  way out of the class.
- **An ambiguous row may never produce `Removed`.** That is the whole safety property F20 bought, kept word-for-word in
  effect: a false `Removed` stales a live note, drops it from the catalog, under-charges the gap, and records
  `PARSE_VERSION` over rows nobody re-parsed.
- **Refuse to pick when there is more than one candidate.** If two or more ambiguous rows share one display spelling, or
  if the hash claim is not unique (`added_count != 1`, or the hash still exists at a path that did not go away — the
  `present_hashes` guard), resolve none of them. F13 §7's rule: name the conflict, never guess.

Read against the two cases (row hash `H_r`, listed twin hash `H_t`):

| state | today | under this rule |
| --- | --- | --- |
| N′, content unchanged (`H_t == H_r`) | duplicate rows forever, version withheld | **resolves**: `Moved`, re-keyed to real bytes, one live row, and the charge clears with the reason it was withheld |
| M, twin differs (`H_t != H_r`) | `added: 1`, note survives, withheld | unchanged — no claim, no removal, still charged. **The existing fence must pass untouched.** |
| N′, content changed while hidden | duplicate rows forever | unchanged (still duplicates) — the residue, §5 |
| M, twin byte-identical (`H_t == H_r`) | `added: 1`, note survives | row claims the twin as a move; when the obstruction clears the real file indexes as its own row. Two files, two rows, correct by then; nothing is lost meanwhile because identical bytes render identically |
| obstruction gone, no look-alike | row not ambiguous at all | unchanged: matched or removed on bytes like any other row |

The residue row is why this is a step and not the fix. It is also the first state where **content agreement resolves the
case that matters most in practice**: a legacy workspace whose note has not been edited since it was migrated — which is
what "legacy duplicate" usually means, a corpus nobody touched because every scan re-parsed it (F19 §1).

## 3. The charge

`hidden_by_unknown` keeps `PARSE_VERSION` away, and the backlog is explicit that this is the safety, not the bug: *do not
write a "Do when" that demands recording the version mid-obstruction.* So:

**The count becomes "ambiguous rows this pass did not resolve", not "rows we deleted".** M keeps its charge; N′ loses it
only by resolving, which is the difference between healing and announcing. Implementation must derive the count from what
`classify` left unresolved rather than from a pre-pass tally, or the resolved case will keep paying a charge it has
discharged — and a charge that never clears is the F19 bug wearing this milestone's clothes.

## 4. Invariants

1. **No ambiguous row is ever removed.** Pinned directly: after a scan of an M workspace, the row is still live with its
   body intact — not inferred from `removed == 0` alone.
2. **M does not move.** `a_display_keyed_row_inside_an_unreadable_directory_survives_its_readable_twin` passes with its
   assertions **unedited**. This is the regression test for over-reach, and it exists already; if it needs editing, stop
   and report — that is the same signal F19's stopped round was, and this entry has already had one controller-prescribed
   fix rejected on evidence.
3. **Resolution goes through `apply_move`, never a hand-written re-key.** Row identity, derived rows and `notes.rel` are
   that function's business (standing ruling); `classify` emits `Moved { from }` like any other move.
4. **Uniqueness or nothing.** One ambiguous row per display spelling, one claimant, one added path, no live copy of the
   hash elsewhere — otherwise no claim.
5. **Nothing under `tests/golden/` moves**, and the merge machinery from F19 is not repurposed here: this branch prevents
   new duplicates where content agrees; it does not clean up ones already made (§5).

## 5. Tests this round owes

- **The healed N′.** The existing harm fixture
  (`known_defect_a_readable_display_keyed_path_duplicates_its_own_row`) asserts the defect: two live `files` rows, two
  `notes` rows, version withheld. Replace it with a convergence fixture — one live row for that path, one note row at the
  *original* id, `rel_bytes` now the real bytes, and the parse version recorded — and say in the commit body that F23's
  "delete the harm fixture in the commit that fixes it" instruction is being honoured for this half of the entry.
- **The residue, fenced.** A variant whose note content changed while it was unindexable: hashes disagree, nothing
  resolves, and today's duplicate-pair outcome remains. Label it as F23-residual in the name and comment, state that it
  pins a *known wrong* behaviour, and require the fence to fail loudly if a future fix changes it silently — the pattern
  M3 established with `a_display_keyed_row_inside_an_unreadable_directory_survives_its_readable_twin`. Its twin must carry
  a **different body** from the hidden note or hash pairing makes it vacuous (that mistake is recorded in the backlog).
- **Two ambiguous rows, one display spelling.** Assert neither resolves and both stay live: uniqueness is an invariant,
  not an optimisation.
- **The charge follows resolution.** M still withholds `PARSE_VERSION`; the healed N′ records it. Both from the same
  helper so the pair cannot drift.
- **Fixture honesty.** These depend on `chmod 000` and latin-1 directory names: they must re-check denial off disk and
  print `NOT COVERAGE` rather than pass silently, and the report must say whether they ran here (`denied()` true) — a
  green CI run is not proof a fixture executed.

**Mutations, each with a named assertion that must fail:** (1) let an ambiguous row produce `Removed` → the M fence fails;
(2) drop the hash claim (keep the mark) → the healed-N′ fixture fails to converge; (3) resolve even when two rows share a
display spelling → the uniqueness fixture fails; (4) count ambiguity before pairing instead of after → the charge test
fails on the resolved case. Restore from `/tmp` copies, never `git checkout --`.

## 6. Deliberately not in this round

- **Found-side suppression** (backlog candidate ii): skipping a readable path whose display text matches an unresolved row,
  trading duplicate rows for a file left unindexed. It would close the residue row, and it is *not* free: in M the twin is
  a genuinely different file that today gets indexed correctly, so suppression costs real coverage to buy tidiness. It
  "changes what a scan is allowed to omit", which is a spec decision of its own — the backlog says so, and this round
  honours that rather than smuggling it in behind a hardening fix. Under (ii) the M fence's `added: 1` legitimately
  becomes `added: 0` plus a named skip; that edit belongs to a branch that chose (ii) on purpose.
- **A durable file identity** (new candidate iv): store what the OS calls the file — `ino`/`dev` on Unix, volume serial +
  file index on NTFS — at every successful index, then decide N′ against M by identity instead of by content. It resolves
  the residue too, including content changed while hidden, and it is the right next step. Cost: a migration, platform
  code, files where identity is unavailable or recycled (exFAT/FAT without `FileIdInfo`, network mounts), and — per F18's
  standing lesson — no Windows claim until one has been run on Windows.
- **Cleaning up duplicates F23 already made.** F19's merge folds rows on one `rel_bytes`; these twins sit on *different*
  keys, so folding them means deciding which key is a lie. Deferred with the residue.

## 7. Gates

The standing list, no subsetting: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`;
`cargo test --workspace`; `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config`;
`RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps`; the cfg dead-code check; the golden verifier →
`GOLDEN CHECK: pass`. CI green on Linux **and** Windows. This round touches `known_rows`, which runs every scan, so re-run
the ignored release benchmark in A/B/A form against the merge base and record raw numbers **with what they cannot see**, in
F19 §6's style: `two_pass_scan_over_two_thousand_notes` has no unreadable directory and no display-derived key, so it
measures only the common path where the new code is a boolean test on rows that are not ambiguous.
