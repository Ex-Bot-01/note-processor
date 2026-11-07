# F23's residue, measured — what is actually left when the obstruction clears

Status: **measurement, no code change.** It narrows what `docs/backlog-m2-followups.md` F23 calls "the residue" and asks for one
ruling: accept what is left and close, or spend on clearing it. Fixture:
`crates/np-core/tests/scan.rs::a_migration_keyed_row_with_no_identity_rejoins_its_path_and_leaves_a_stale_row`. Prior art: F23 §1–§4
(the scoping clause and the `matched` exclusion), `docs/design-a4-file-identity.md` §3 and §8 (identity may add a match, never remove
one; the boundary sentence), F13 §4 (migration 4's display-derived key), F22 §5 (`duplicates` counts **live** rows per display path).

## 1. The question, in the card's own words

> a workspace with `caf\xe9/` blocked beside `caf\xe8/`, holding one display-keyed row whose content changed while blocked, ends with
> exactly one live `files` row and one `notes` row for that path **once the obstruction is gone**

The shape is the oldest one in this area: a row written before byte keys existed, so its stored key is the **display text** of a path
whose real bytes differ (`caf\xe9/note.md` renders as `caf\u{fffd}/note.md`), and no `files.file_id` — nothing the OS can be asked
again. Its content changes while the path cannot be read. Then the obstruction goes away. Does the note come back, and what does the
database hold afterwards?

Two things had to be true for the fixture to ask that question rather than a different one. The legacy key has to be built by
**migration 4's own rule** — `path_keys::rel_bytes_of_display`, with `case_fold` and `display_fold` beside it — because an old build
knew nothing else; and the ambiguity twin (`caf\xe8/`) must appear only after the first scan, so no scan ever saw a pairable pair. The
content edit happens with no scan in between, which leaves exactly the state "changed while hidden" describes: at scan time the stored
hash is stale and the path unreadable.

## 2. What was measured

Row census by stored key, `state` in brackets. `pv` is `files.PARSE_VERSION` as recorded after that scan.

| | display key `caf\u{fffd}/note.md` | real key `caf\xe9/note.md` | twin `caf\xe8/note.md` | report |
| --- | --- | --- | --- | --- |
| while hidden | 1 [`ok`], pre-edit content | 0 rows | 1 [`ok`] | `added 1`, `unindexable{unreadable 1, duplicates 1, ambiguous 1}`, pv recorded |
| after clearing | 1 [`stale`], pre-edit content | **1 [`ok`], edited content** | 1 [`ok`] | `added 1 removed 1 skipped [] unindexable{duplicates 1}`, pv recorded |
| third scan | 1 [`stale`] | 1 [`ok`] | 1 [`ok`] | `0 0 0 0`, `skipped []` — settled, not stuck |

The catalog after clearing lists the edited note and its twin once each; the stale row appears nowhere in it. `duplicates: 1` is the
innocent shape F22 §5 already describes — two **live** rows under one display spelling, which is what two correctly-indexed twins look
like to that counter — and it is not caused by the residue: the stale row is excluded by the `state = 'ok'` filter.

So the card's "do when" is met: exactly one live `files` row and one `notes` row for the path, once the obstruction is gone. The
healing needs no identity, because the row had walked away from its own `rel_bytes` into its display spelling; the returning file found
its key unclaimed and indexed as new work (`upsert_file` resolves by `rel_bytes`, no state filter). What remains permanently is one
`stale` row under a key that spells no file, holding content the file no longer has.

## 3. The measurement that came out wrong first

The first fixture set `rel_bytes` and `rel` and left `case_bytes` folded from the **real** bytes. That scan refused the returning file
on `files.case_bytes UNIQUE`, and the end state looked like the worst version of F23: `(added, modified, moved, removed) = (0, 0, 0, 1)`,
`skipped: ["caf\u{fffd}/note.md"]`, **no row at all** for a note sitting there readable, `unindexable: None`, and the edited content
missing from the catalog — reproduced identically by a third scan, so it appeared to be a fixed point of stuck work.

It was an artifact of planting. No build this program has shipped can produce that state: `rel_bytes`, `case_bytes`, `rel` and
`case_key` are written together, and migration 4 derives all four from the same display string. A row whose `case_bytes` is folded from
bytes other than its own `rel_bytes` is not a legacy row; it is a fixture that invented a collision. Recorded here because the trap is
easy to fall into twice: **the four key columns move together, or the fixture stops measuring scans and starts measuring itself.**

## 4. What is left, stated without padding

- One `stale` registry row per affected path, forever, holding pre-edit content. Invisible in the catalog; visible only to someone who
  queries `files`. It is the row F23 refused to delete, for the reason F23 was opened.
- Nothing withheld: `PARSE_VERSION` is recorded on these scans, so a refresh is not pinned by this shape. (The card's claim that the
  version "stays withheld" belongs to the configuration where the row is *unresolvable*, not this one.)
- No user-visible report noise attributable to the residue: the `skipped` line disappears with the obstruction, and `duplicates` counts
  live rows.
- Not covered here: Windows file identity (F18's standing rule — no claim has been made or tested on that platform), and any runner where
  `chmod 000` fails to deny access or the filesystem refuses a latin-1 directory name prints `NOT COVERAGE` instead of passing quietly.
  This run: host `pi`, euid 1000, scan suite **78 passed / 0 failed** with zero `NOT COVERAGE` lines.

## 5. Options, with their costs

**(A) Accept and close the residue.** The note rejoins its path; the stale row is the safety F23 asked for, now with a test that says
what it costs. Cost: one dead row per affected path, and `duplicates`/catalog readers unaffected. Nothing to write beyond this document
and the card.

**(B) Clear the stale row once its real path is indexed.** Tempting, and it is F23's original question wearing a different hat: any rule
that deletes a display-derived row needs to know that no note is behind it. Here the pre-edit content exists nowhere else in the
database — the live row holds the post-edit bytes — so deletion would be safe *in this configuration*. It is not safe in the neighbour:
if the real path is later removed while a stale row still stands, that row is the last record that the note existed, which is what F23's
"don't delete the ambiguous row" protects. Cost: a rule about which stale rows are provably redundant, plus cases for the neighbour. This
is a spec change to what the registry may forget, not a hardening round.

**(C) Surface it.** A report line naming paths that carry an orphaned display-derived key — "this workspace has N rows keyed from text
that no longer spells any file". Cost: another field in `ScanReport`, whose JSON keys are pinned by CLI tests, buying visibility for a
state that is now known to be harmless. Only worth doing if (B) is declined *and* someone wants the number on screen.

**Recommendation: (A)**, with this fixture as the fence. The residue's harm turned out to be a dead row rather than a lost note, and both
alternatives spend the thing F23 was opened to protect. If (A) is taken, F23's remaining open item becomes only what was already outside
this measurement: Windows identity, which stays F18's blocker.

## 6. How to re-run it

`cargo test -p np-core --test scan` on a host where `denied()` is true; the case prints `NOT COVERAGE` and returns if the filesystem or
the mode bits decline, so a green run must be checked for that line before it is cited (F18's rule, and F23's runner caveat). The row
census in §2 came from temporary `eprintln!` dumps of the same fixture; the committed case asserts the numbers rather than printing them,
so re-measuring means adding the prints back.
