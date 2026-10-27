# F23 — stop deleting the ambiguous row, and let content decide what it can

Status: design for `m6/ambiguous-rows`, **revised after the pre-implementation review** (`05cffe24`, verdict BLOCK on five
P1s; every one is incorporated below, and §8 records what each one changed). Backlog:
`docs/backlog-m2-followups.md` F23 (open by decision since M3). Prior art: F20 §4 (the fallback that hides these rows),
F13 §4.1/§7 (bytes are identity; name a conflict, never pick — the rule as phrased here is restated in
`docs/design-f19-note-merge.md` and enforced by `db.rs`'s note identity-conflict rule), F22 (the bounded durable record),
`change::classify`'s hash pairing and its `present_hashes` guard.

## 1. What the backlog established, and where it stops

Two workspaces hand `known_rows` **identical inputs** and need opposite answers:

- **N′** — the row's own file *is* the readable path. Today the fallback hides the row, the listed file pairs with nothing,
  indexes as `Added`, and the workspace ends with two live `files` rows and two `notes` rows for one physical file, two
  catalog entries, ids whose headings and links can diverge, and `PARSE_VERSION` withheld forever. No scan clears it.
- **M** — the row's own file is *inside* the blocked directory and the readable path is a different, unindexed twin. Here
  hiding is right: `added: 1`, the note survives, the version stays withheld.

The backlog's conclusion — "**a lossy key carries no byte spelling, so nothing computable at `known_rows` distinguishes
the two**" — stands, and the narrow patch ("don't hide when the found set carries that display text") stays rejected: it
turns M into `added: 1, removed: 1`, staling a live note. What changes is **where the decision is made**. `known_rows`
cannot decide, yet it currently decides by deleting the row from the scan's view — which also removes it from
`change::classify`, the one place holding the other half of the information: the digest of every path the walk could read,
against the `files.hash` the row already carries. The cases are not indistinguishable; they are indistinguishable to a
predicate that is denied the comparison.

## 2. The rule

**Ambiguity stays exactly as wide as it is today. Its consequence changes from deletion to a mark: an ambiguous row may not
be removed, and it may be claimed — by content or by its own listed path.**

- **Same predicate for who is ambiguous:** `key_is_display_derived(rel, rel_bytes) && under_unreadable(rel,
  unreadable_display)`, including its deliberate width. No new heuristic about ambiguity itself.
- **Ambiguous rows stay in `known`** and reach `classify`.
- **An ambiguous row may never produce `Removed`** — F20's safety property kept intact: a false `Removed` stales a live
  note, drops it from the catalog, under-charges the gap, and records `PARSE_VERSION` over rows nobody re-parsed.
- **Two ways to resolve**, because `classify` emits *nothing* for an identity-matched pair whose `size` and `mtime` agree:
  - **claimed** — it produced `Moved { from } = its own key`, so `apply_move` re-keys `rel_bytes` from real bytes; or
  - **matched** — its own display path was listed this pass with a non-empty hash. This is the route for a directory whose
    name genuinely contains `U+FFFD` (permanent class member, and universal on Windows): its key already equals its OS
    bytes, it pairs by identity, and no move ever happens. A definition of "resolved" that required `Moved` would leave
    those rows charged every scan forever — §3's own forbidden outcome.
- **Refuse to pick when there is more than one candidate.** Multiplicity here is not two rows sharing a display spelling —
  `files.rel_bytes` is `NOT NULL UNIQUE`, and the key is a function of the display text, so that state cannot exist. The
  reachable state is **N ambiguous rows with equal content claiming one listed path** (two notes under one blocked
  directory whose bytes match a planted twin). There, `classify` would pick via `closest_to`; this design refuses:
  **resolve none of them.** Enforcement point is §4.

Read against the cases (row hash `H_r`, listed twin hash `H_t`):

| state | today | under this rule |
| --- | --- | --- |
| N′, content unchanged (`H_t == H_r`) | duplicate rows forever, version withheld | **resolves** by claim: `Moved`, re-keyed to real bytes, one live row at the original id, charge clears with the reason it was withheld |
| ambiguous row whose own path is listed and readable (real `U+FFFD` name) | charged every scan if a look-alike is blocked | **resolves** by match: identity pairing, no move, charge clears |
| M, twin differs (`H_t != H_r`) | `added: 1`, note survives, withheld | **unchanged.** No claim, no removal, still charged. The existing fence passes with its assertions untouched — that is invariant 2 and this design's over-reach test |
| N′, content changed while hidden | duplicate rows forever | unchanged (still duplicates) — the residue, §6 |
| M, twin byte-identical (`H_t == H_r`) | `added: 1`, note survives — **already correct: two files, two rows** | row claims the twin: `moved: 1` where nothing moved, one fewer catalog entry while blocked, and the id is re-homed onto a different physical file. Costs named in §5.4 |
| obstruction gone, no look-alike | row not ambiguous at all | unchanged: matched or removed on bytes like any other row |

## 3. The charge is a sum, and it discharges on resolution

`hidden_by_unknown` keeps `PARSE_VERSION` away; the backlog is explicit that mid-obstruction withholding is the safety, not
the bug. The count stays **two sources added together**:

1. rows hidden by the byte-keyed arm (a real path under an unreadable prefix) — **unchanged**, they resolve nothing; and
2. ambiguous rows this pass **did not resolve** — resolved meaning claimed *or* matched, §2.

Deriving source 2 from classification rather than from a pre-pass tally is what makes N′ stop paying once it has healed; a
charge that never clears is F19's bug wearing this milestone's clothes.

## 4. Where the mark lives (enforcement point)

`classify` keeps its signature and its semantics — it is `pub` with ~30 call sites in `tests/change.rs`, and teaching it a
new kind of row would move F23's ambiguity into the matching primitive every other caller inherits. So the mark is
**scan-side**, around an unmodified `classify`:

1. **Pre-drop the refused ones.** Ambiguous rows caught by the multiplicity rule are removed from `known` before
   `classify`, exactly as today, and they charge source 2. This is what makes "resolve none" true rather than aspirational:
   left in, `closest_to` would pick a winner among equal hashes and the uniqueness rule would be silently unenforced.
2. **Post-strip the unclaimed ones.** Ambiguous rows that were admitted and end with a `Removed` for their key have that
   change dropped after classification, before anything consumes it, and they charge source 2.

The two orders are observably different, which is why they are named: pre-dropping keeps today's behaviour for the refused
set; post-stripping is what the admitted set needs to survive. A `Snapshot` field is not required — the scan already knows
which keys it marked; if an implementation finds it needs one, that is a signal the seam moved and should be reported, not
pushed through.

**Uniqueness of a claim includes stale rows.** `known_rows` selects `state = 'ok'` only, but `upsert_file` resolves by
`rel_bytes` with **no state filter** and reactivates a stale row keeping its id — that is how a returning file rejoins its
note today. A `Moved` claimed onto a key that a *stale* row holds hits the `UNIQUE` in `apply_move`, comes back
`Indexed::Taken`, returns early, so the stale row is never reactivated and the path lands in `skipped` while
`owes_a_reparse` charges `written.uncovered` on every refresh — a stuck state nothing clears. Therefore: **a claim is
refused when any row, live or stale, already holds the target key**, and that refusal is pinned by a fixture (§5.5).

## 5. Tests this round owes

Two fixtures change because they pin the *behaviour being replaced*. They are named here so no implementer meets them as a
mystery, and each edit must be justified in its commit body — unlike the M fence, which is untouchable (invariant 2).

**5.1 Changed on purpose**
- `known_defect_a_readable_display_keyed_path_duplicates_its_own_row` → becomes the **convergence** fixture: one live
  `files` row for that path, one `notes` row at the original id, `rel_bytes` now real bytes, parse version recorded. F23's
  instruction to delete the harm fixture in the commit that fixes it is honoured for this half of the entry.
- `a_duplicated_note_from_f23_reaches_the_record` (the F22 record fixture) builds the same shape and asserts
  `"unreadable=1;unkeyable=0;duplicates=1"` with two live rows. Under this rule that scan converges, so the expectation
  becomes **`"unreadable=1;unkeyable=0;duplicates=0"`** with one live row: F22's duplicate counter stops firing on a pair
  that no longer exists — which is the record working, not regressing. Verified running for real on this runner (ext4,
  `denied()` true, no `NOT COVERAGE` printed), so the change is observed rather than predicted.

**5.2 Contract updates, not fences** — these assert that ambiguous rows stay *out* of `known`, which is the behaviour being
replaced: `a_display_keyed_row_falls_back_to_text_and_a_byte_keyed_row_does_not` (in-crate) and
`a_note_behind_a_refused_path_is_charged_to_the_refresh_that_missed_it` (its rows are planted through `snap()`, which keys
from display text, so they are ambiguous). Both must be updated to "in `known`, marked, unremovable". They are explicitly
**not** covered by invariant 2's stop signal.

**5.3 Untouchable** — `a_display_keyed_row_inside_an_unreadable_directory_survives_its_readable_twin`: passes with its
assertions **unedited**, or the design is wrong and the round stops and reports. Its twin deliberately carries a different
body; identical content would let hash pairing pair them and prove nothing.

**5.4 New: the byte-identical twin, with its cost named.** Two notes under one blocked directory, bytes matching a planted
twin → the row claims it. Assert what it buys (one catalog entry while blocked, no duplicate rows) **and what it costs**:
`moved: 1` where nothing moved, and because the row has left its own key, nothing tracks the file inside the obstruction —
a later deletion there is never reported, which is F20 §1's harm class, and other notes' links now resolve to the twin's
id. This configuration cannot be avoided (the two cases are identical to `known_rows` by construction), so it is documented
rather than described as free.

**5.5 New: multiplicity and collisions.**
- Two ambiguous rows with equal content, one claimant → **neither resolves**, both stay live, both charge source 2.
- A claim whose target key is held by a **stale** row → refused; the path is handled by the ordinary `upsert_file`
  reactivation route rather than ending in `skipped` with a permanent charge.

**5.6 Charge arithmetic.** One test where M still withholds `PARSE_VERSION`, one where healed N′ records it, plus one that
keeps source 1 charging (a byte-keyed row behind an unreadable prefix) so the sum cannot be read as a replacement. And the
debt `one_note_behind_an_unknown_path_is_charged_in_exactly_one_counter` explicitly leaves to F23: a disjointness unit beside
it, with its premise paragraph updated — the overlap was real today because `Indexed::Taken` charges
`written.uncovered` over an existing note row.

**5.7 Fixture honesty.** These depend on `chmod 000` and latin-1 directory names: re-check denial off disk, print
`NOT COVERAGE` rather than pass silently. **Baseline recorded on this runner:** scan suite 58 passed / 0 failed with
**zero** `NOT COVERAGE` lines, so every permission-dependent fixture executed here; the PR must name the validation runner
as F23's caveat requires.

**Mutations, each with a named assertion that must fail:** (1) let an ambiguous row produce `Removed` → the M fence fails;
(2) drop the claim route, keep the mark → healed N′ never converges; (3) resolve when two rows share one claimant → the
multiplicity fixture fails; (4) count ambiguity before classification instead of after → the charge test fails on the
resolved case; (5) require `Moved` for discharge → the identity-matched (`U+FFFD` name) fixture keeps charging forever;
(6) ignore stale rows in the claim check → the collision fixture ends in a permanent charge. Restore from `/tmp` copies,
never `git checkout --`.

## 6. Deliberately not in this round

- **Found-side suppression** (candidate ii): skipping a readable path whose display text matches an unresolved row. In M
  the twin is a genuinely different file that today gets indexed correctly, so suppression costs real coverage to buy
  tidiness; and it "changes what a scan is allowed to omit", which is its own spec decision. Under (ii) the M fence's
  `added: 1` legitimately becomes `added: 0` plus a named skip — an edit that belongs to a branch choosing (ii) on purpose.
- **A durable file identity** (candidate iv): store what the OS calls the file (`ino`/`dev` on Unix, volume serial + file
  index on NTFS) at every successful index and decide N′ against M by identity rather than content. It closes the residue
  including content-changed-while-hidden, and it is the right next step. Costs: a migration, platform code, files where
  identity is unavailable or recycled (exFAT/FAT without `FileIdInfo`, network mounts), and — per F18's standing lesson —
  no Windows claim until one has been run on Windows.
- **Cleaning up duplicates F23 already made.** F19's merge folds rows sharing one `rel_bytes`; these twins sit on different
  keys, so folding them means deciding which key is a lie. Deferred with the residue.

**Refused ambiguity must be visible.** §2 borrows "name the conflict" as a decision rule, and today the only observables
are a withheld version plus the gap line. So the refused set gets **one more field in F22's existing bounded `meta`
record** (`ambiguous=N;names=…`, same cap and ordering discipline) rather than a third mechanism — the same reasoning that
kept F22's duplicate count inside one key instead of inventing a channel.

## 7. Documentation step

Precedent is explicit (F18's entry was reworded to what shipped rather than closed as if solved). This round updates:
`docs/backlog-m2-followups.md` F23 — the half that now heals, the residue, candidate iv as the next step; and three
docstrings this change falsifies — `known_rows` and `display_spelled` ("**No scan clears that**"), and
`key_is_display_derived` ("Nothing else does").

## 8. What the design review changed

| finding | change |
| --- | --- |
| P1-A F22's record fixture breaks silently | §5.1 names it with its new expected value, verified running on this runner |
| P1-B "resolved = produced `Moved`" never discharges identity-matched rows | §2 adds the **match** route; §3 defines discharge as claim-or-match; mutation (5) added |
| P1-C "resolve none" unenforceable, enforcement point unnamed | §4 fixes it: pre-drop refused, post-strip admitted, `classify`'s signature untouched |
| P1-D a claim onto a stale row's key sticks forever | §4's uniqueness includes stale rows; §5.5 pins it; mutation (6) added |
| P1-E the byte-identical-twin cell understated its cost | §2's table corrected; §5.4 requires asserting the harm, not just the benefit |
| P2 two ambiguous rows cannot share a display spelling | §2/§4 restated as N rows / one claimant |
| P2 in-crate units that must change | §5.2 lists them and exempts them from invariant 2's stop signal |
| P2 the charge could be read as replaced | §3 writes it as a sum; §5.6 keeps source 1 pinned |
| P2 refused ambiguity invisible | §6: one more field in F22's bounded record |
| P2 no documentation step; citation nit | §7 added; F13 §7 citation pointed at the restatement that carries the rule |

## 9. Gates

The standing list, no subsetting: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`;
`cargo test --workspace`; `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config`;
`RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps`; the cfg dead-code check; the golden verifier →
`GOLDEN CHECK: pass`. CI green on Linux **and** Windows. This touches `known_rows`, which runs every scan, so re-run the
ignored release benchmark A/B/A against the merge base and record raw numbers **with what they cannot see**, in F19 §6's
style: `two_pass_scan_over_two_thousand_notes` has no unreadable directory and no display-derived key, so it measures only
the common path where the new code is one boolean test on rows that are not ambiguous.
