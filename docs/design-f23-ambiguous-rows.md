# F23 — stop deleting the ambiguous row, and let content decide what it can

Status: design for `m6/ambiguous-rows`, **revised twice at the pre-implementation gate** — round one (`05cffe24`) blocked on
five P1s, round two (`d1a4cbd7`) said "correct three things first" and found a note-losing hole in the claim rule. Every
finding is incorporated below; §8 records what each changed. Backlog:
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
- **A claim must be scoped to the look-alike, not to the content.** `classify` matches a claim on **hash alone**
  (`claimants.get(added.hash)` over every unmatched known row), and the `present_hashes` guard cannot help: it is seeded
  from known rows whose *key* matches an unreadable listing, and a display-derived key never equals a real-bytes listing
  key. Left unscoped, a new unrelated path anywhere in the corpus with byte-identical content — a `# Title` stub, a blank
  template, a re-exported note, all common in a notes corpus — would claim the hidden row: `moved: 1`, the id re-homed onto
  somebody else's file, and nothing left tracking the file inside the obstruction. That is a worse wrong answer than today's.
  So: **an ambiguous row is admitted to `known` only when some listed, readable path spells itself exactly as the row does
  (same display text) and matches its stored hash; otherwise it is refused.** This is still a pre-drop and still leaves
  `classify` untouched.
  - **Corollary, and it removes machinery:** with display scoping plus `files.rel_bytes NOT NULL UNIQUE`, there is **at most
    one claimant per listed path**. So there is no multiplicity rule to write, no tally to keep and no tie-break to refuse —
    uniqueness is enforced by display equality plus the schema. (Two ambiguous rows cannot share a display spelling either,
    since the key is a function of the text.) An implementation that finds itself counting claimants has mis-scoped.

Read against the cases (row hash `H_r`, listed twin hash `H_t`):

| state | today | under this rule |
| --- | --- | --- |
| N′, content unchanged (`H_t == H_r`) | duplicate rows forever, version withheld | **resolves** by claim: `Moved`, re-keyed to real bytes, one live row at the original id, charge clears with the reason it was withheld |
| ambiguous row whose own path is listed and readable (real `U+FFFD` name) | charged every scan if a look-alike is blocked | **resolves** by match: identity pairing, no move, charge clears |
| M, twin differs (`H_t != H_r`) | `added: 1`, note survives, withheld | **unchanged.** No claim, no removal, still charged. The existing fence passes with its assertions untouched — that is invariant 2 and this design's over-reach test |
| unrelated readable path, byte-identical to the hidden note | `added: 1`, two rows for two files — correct | **unchanged**, because §2's scoping refuses admission: no display match, no claim. Without that clause this cell becomes `moved: 1` and a note is re-homed onto a stranger's file |
| N′, content changed while hidden | duplicate rows forever | unchanged (still duplicates) — the residue, §6 |
| M, twin byte-identical (`H_t == H_r`) | `added: 1`, note survives — **already correct: two files, two rows** | row claims the twin: `moved: 1` where nothing moved, one fewer catalog entry while blocked, and the id is re-homed onto a different physical file. Costs named in §5.4 |
| obstruction gone, no look-alike | row not ambiguous at all | unchanged: matched or removed on bytes like any other row |

## 3. The charge is a sum, and it discharges on resolution

`hidden_by_unknown` keeps `PARSE_VERSION` away; the backlog is explicit that mid-obstruction withholding is the safety, not
the bug. The count stays **two sources added together**:

1. rows hidden by the byte-keyed arm (a real path under an unreadable prefix) — **unchanged**, they resolve nothing; and
2. ambiguous rows this pass **did not resolve** — resolved meaning claimed *or* matched, §2.

Deriving source 2 from classification rather than from a pre-pass tally is what makes N′ stop paying once it has healed; a
charge that never clears is F19's bug wearing this milestone's clothes. Two precision rules, because the two sets are not
naturally disjoint:

- **Precedence.** A row is marked **only if the byte-keyed arm did not already hide it**. The permanent class can satisfy
  both arms at once — a row whose key equals its OS bytes sitting under a genuinely unreadable prefix hits `is_unknown_path`
  *and* the fallback. Today's single `||` and single counter cannot double count; two passes over two sets can, and on
  Windows (where refusals are already lossy bytes) they would double charge universally.
- **Source 2 is defined positively over the marked set, not over stripped changes:** marked rows that produced neither
  `Moved { from } = its key` nor a listing of their own path with a non-empty hash, **minus** those whose own key appears in
  the found-side unreadable set — those are already charged by `written.uncovered`, which resolves by byte key. Deriving it
  from "did we strip a `Removed`" would charge zero times for a marked row that `classify` filtered out before ever emitting
  one, and an uncharged stale row is exactly the F15 class this project keeps paying to avoid.

**Which clause wins when they disagree.** Admission (§4's pre-drop) decides only whether a row enters `known`; discharge
(§3's match route) is decided independently, from what the walk listed. So a row can be **refused admission and still
discharge**: the permanent class — a name whose OS bytes really are its display text — has its own path listed under the very
key it stores, so `upsert_file` re-joins the file to its row even when content changed and no claim was possible, and nothing
stayed stale. That is not a loophole to close; charging there would bill a note for work the scan did. Pinned by
`a_permanent_class_row_whose_content_changed_while_a_twin_is_blocked_rejoins_its_own_row`, which is also the only fixture
that fails when §3's `matched` exclusion is deleted on its own.

## 4. Where the mark lives (enforcement point)

`classify` keeps its signature and its semantics — it is `pub` with ~30 call sites in `tests/change.rs`, and teaching it a
new kind of row would move F23's ambiguity into the matching primitive every other caller inherits. So the mark is
**scan-side**, around an unmodified `classify`:

1. **Pre-drop the unadmitted ones.** Ambiguous rows with no listed readable path that spells them exactly and matches their
   stored hash never enter `known` — today's behaviour, unchanged — and they charge source 2.
2. **Post-strip the admitted-but-unclaimed ones.** If such a row ends with a `Removed` for its key, that change is dropped
   after classification and before anything consumes it (`changes` has exactly one consumer, read twice inside
   `apply_changes`, so this is one filter between those reads), and the row charges source 2.

**The strip matches `Change::Removed` only.** The identity-matched route legitimately emits `Modified` keyed on the listed
path, whose `rel_bytes` equals the row's stored key; a strip written as "drop any change carrying a marked key" would delete
a legitimate re-parse and leave stale derived rows behind charging nothing. Traced against `classify`: in M and in
N′-changed the row is unmatched by identity, its hash matches no added path, and the final loop pushes exactly one `Removed`
whose bytes are the stored key — so `(Removed, key)` covers both.

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

**5.0 New first: the identity-matched row (no re-key at all).** Build it with existing helpers and **no rekey**: a directory
literally named `caf\xEF\xBF\xBD` holding a note, scanned (its key is naturally its own display bytes and `rel` contains
`U+FFFD`, so `key_is_display_derived` is true), then a sibling `caf\xe9` blocked — same display spelling, different bytes →
marked. Assert four things: no `Added` and no duplicate row; `PARSE_VERSION` recorded with **no `Moved` anywhere in the
report** (discharge by match); the note keeps its id *and* its key; and, once §3's precedence rule lands, that source 1 did
not also charge it this pass. This is the only fixture for §2's second resolution route, it is the class Windows lands in
universally, and it is where the precedence question bites — so it is built first.

**5.4 New: the byte-identical twin, with its cost named.** **One** note under the blocked directory (the fence fixture's
shape with an identical body) and a readable twin carrying the same bytes → one claimant, one added path, claim allowed. Assert what it buys (one catalog entry while blocked, no duplicate rows) **and what it costs**:
`moved: 1` where nothing moved, and because the row has left its own key, nothing tracks the file inside the obstruction —
a later deletion there is never reported, which is F20 §1's harm class, and other notes' links now resolve to the twin's
id. This configuration cannot be avoided (the two cases are identical to `known_rows` by construction), so it is documented
rather than described as free.

**5.5 New: strangers and collisions.**
- **The unrelated look-alike.** The hidden note's bytes reappear at an unconnected readable path (`notes/lookalike.md`).
  Assert `added: 1, moved: 0`, the row still live on its own key, and no catalog entry lost. This is the regression test for
  §2's scoping clause — the one assertion standing between this design and a note re-homed onto a stranger's file.
- A claim whose target key is held by a **stale** row → refused; the path is handled by the ordinary `upsert_file`
  reactivation route rather than ending in `skipped` with a permanent charge.

**5.6 Charge arithmetic.** One test where M still withholds `PARSE_VERSION`, one where healed N′ records it, plus one that
keeps source 1 charging (a byte-keyed row behind an unreadable prefix) so the sum cannot be read as a replacement. The debt
`one_note_behind_an_unknown_path_is_charged_in_exactly_one_counter` explicitly leaves to F23 gets a disjointness unit beside
it, with its premise paragraph updated — and it pins **per-configuration numbers** (`hidden = 1, uncovered = 0`; then
`hidden = 0, uncovered = 1`) rather than a blanket theorem: on Unix the two sources are disjoint for both classes, while on
Windows they overlap unless §3's precedence rule is in place. That asymmetry is the finding worth writing down.

**5.7 Fixture honesty.** These depend on `chmod 000` and latin-1 directory names: re-check denial off disk, print
`NOT COVERAGE` rather than pass silently. **Baseline measured here:** scan suite 58 passed / 0 failed with **zero**
`NOT COVERAGE` lines, so every permission-dependent fixture executed. "This runner" is not an identification — F23's caveat
exists because `chmod 000` does not deny a privileged user — so the PR body records **command, head SHA, host and euid**.

**5.8 Expected edits from the record field (§6), inventoried so no one meets them as breakage.** Adding a field to F22's
value rewrites it whole in nine places — `scan.rs:2580`, `scan.rs:2622`, `tests/scan.rs:3552, 3589, 3608, 3624, 3731, 3869`
and the `starts_with` at `tests/scan.rs:3919` — plus two in np-cli (`cli.rs:1215` whole string, `cli.rs:1237-1245` JSON
keys), with the stdout line assembled from it at `np-cli/src/main.rs:142`. Each is a value update, not a weakened assertion:
the strings get longer, none gets vaguer.

**Mutations, each with a named assertion that must fail:** (1) let an ambiguous row produce `Removed` → the M fence fails;
(2) drop the claim route, keep the mark → healed N′ never converges; **(3) remove §2's display scoping and let content alone
admit a row → the unrelated look-alike fixture must fail with `moved: 1`** — this is the mutation that guards the only
note-losing hole in the design; (4) count ambiguity before classification instead of after → the charge test fails on the
resolved case; (5) require `Moved` for discharge → §5.0's identity-matched fixture keeps charging forever; (6) ignore stale
rows in the claim check → the collision fixture ends in a permanent charge; (7) drop §3's precedence rule → §5.0's fixture
charges twice. Restore from `/tmp` copies,
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

**Unresolved ambiguity must be visible.** Today the only observables are a withheld version plus the gap line, so this gets
**one more field in F22's existing bounded `meta` record** rather than a third mechanism — F22's own reasoning for keeping
the duplicate count inside one key. Rules it must obey, all of them F22's:

- **`N` is exactly source 2** — unresolved ambiguity, the same population that holds `PARSE_VERSION` back — never "the
  refused set", or the record and the withheld version can disagree, which is F21's "ask a day later and there is nothing to
  say". Rendered *from* that population, never read back into deletion or version decisions.
- **One capped `names` list, not a second one** (`NAME_CAP = 8`), ordered duplicates → ambiguous → unreadable: duplicates
  have no other channel, ambiguity has this one, and an unreadable directory already appears in `skipped`. A second cap
  invites an unbounded value.
- **`is_empty()` stays as it is**, and the reason is recorded: ambiguity requires `under_unreadable`, so
  `ambiguous > 0 ⟹ unreadable ≥ 1 ⟹ is_empty() == false`; the field can never be the sole reason a record exists.
- **It steps on a stated contract, which §7 must amend:** `Unindexable::duplicates` promises to be "deliberately
  **unexplained**, because why a path holds two rows is F23's ambiguity and a count naming a cause would be re-deciding it
  here". A sibling `ambiguous` count *does* name a cause, so that docstring joins the falsified list.

## 7. Documentation step

Precedent is explicit (F18's entry was reworded to what shipped rather than closed as if solved). This round updates:
`docs/backlog-m2-followups.md` F23 — the half that now heals, the residue, candidate iv as the next step; and four
docstrings this change falsifies — `known_rows` and `display_spelled` ("**No scan clears that**"),
`key_is_display_derived` ("Nothing else does"), and `Unindexable::duplicates` (§6's new sibling names the cause it promised
not to).

## 8. What the two design reviews changed

Round one (`05cffe24`, BLOCK):

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

**Second review round** (`d1a4cbd7`, verdict "correct three things first, then build"):

| finding | change |
| --- | --- |
| P1-new the claim is scoped to content, not to the look-alike: `classify` matches on hash alone and `present_hashes` cannot fire for a display-derived key, so any unrelated byte-identical path re-homes the note | §2's scoping clause (admit only a listed readable path that spells the row exactly **and** matches its hash); new table row; mutation (3) rewritten to attack it; §5.5's look-alike fixture is the regression test |
| corollary: with scoping + `rel_bytes UNIQUE` there is at most one claimant, so the multiplicity machinery was unreachable code | §2/§4 multiplicity deleted; §5.5's first bullet replaced by the look-alike case |
| source 2 could double count (permanent class hits both arms; universal on Windows) or charge zero (`classify` filters rows out before emitting anything) | §3 adds precedence and a positive definition over the marked set, minus keys already charged by `written.uncovered` |
| the strip could swallow a legitimate `Modified` whose key equals the row's | §4: matches `Change::Removed` only, with the trace that shows `(Removed, key)` covers M and N′-changed |
| §5.4 built the state §4 refuses (two notes = multiplicity) | one note under the obstruction |
| mutation (5) named a fixture nobody owed | §5.0 adds the identity-matched `U+FFFD` fixture, first to build, four assertions |
| §6's field population, cap and `is_empty()` questions | §6: `N` = source 2 exactly; one capped list ordered duplicates → ambiguous → unreadable; `is_empty()` untouched with proof it cannot matter |
| the record field breaks nine pinned strings and a JSON shape | §5.8 inventories every site, and says why each is a lengthening rather than a weakening |
| "this runner" is not an identification | §5.7: PR body records command, head SHA, host and euid |

## 9. Gates

The standing list, no subsetting: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`;
`cargo test --workspace`; `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config`;
`RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps`; the cfg dead-code check; the golden verifier →
`GOLDEN CHECK: pass`. CI green on Linux **and** Windows. This touches `known_rows`, which runs every scan, so re-run the
ignored release benchmark A/B/A against the merge base and record raw numbers **with what they cannot see**, in F19 §6's
style: `two_pass_scan_over_two_thousand_notes` has no unreadable directory and no display-derived key, so it measures only
the common path where the new code is one boolean test on rows that are not ambiguous.

Runner identification, which §5.7 demands because `chmod 000` does not deny a privileged user: host `pi`, euid 1000, head
`ebaa17f`, commands `cargo test --workspace` (exit 0 with no `test result: FAILED` line anywhere) and
`cargo test -p np-core --test scan -- --nocapture` (zero `NOT COVERAGE` lines, so every permission-dependent fixture ran).

**Measured, A/B/A against `79bba56` (`two_pass_scan_over_two_thousand_notes --ignored`, host `pi`, euid 1000):** head cold
395/387/388 then 388/385/385 ms, warm 64/64/65 then 63/64/64; base cold 383/384/387/384, warm 64/64/66/70. The ranges
overlap and the second head block sits on the base numbers, so no cost is measurable at 2k notes — with the limit stated in
the same breath as the numbers: this corpus contains no unreadable directory and no display-derived key, so `admits` never
runs and the new code is one boolean test per row. It is evidence about the common path only.
