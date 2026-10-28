# A4 — durable file identity: a third matching route that may only ever *add* a match

Design note, 2026-10-28. Roadmap: Track A item **A4** (`docs/roadmap-2026-10-27.md`). Branch: `m10/file-identity`.
Root cause shared by `docs/design-f23-ambiguous-rows.md` §6 and `docs/design-f20-bytes-in-reports.md`: a registry key derived
from display text cannot say which physical file it belongs to.

## 1. The state this fixes, in one paragraph

A row keyed from lossy display text sits under an unreadable prefix (F23 marks it instead of deleting it). While it is
unindexable, its note is edited. When the obstruction clears, F23's two resolution routes both need content: a claim needs
display equality **and** hash equality (`admits`), and a match needs the row's own key to be listed with a non-empty hash. The
edited file agrees on neither — so the row stays marked, charges `unresolved_ambiguity` on every scan forever, and its note is
re-indexed as a second live row under the real bytes. That is F19's bug in F23's clothes, and it is *documented* residue rather
than a surprise (`design-f23` §6). What would resolve it is not more text matching: it is knowing that the file on disk **is**
the file the row was written for.

## 2. What identity means here, per platform

| Platform | Source | Stored as |
| --- | --- | --- |
| Unix | `st_dev` and `st_ino` from the metadata the walk **already reads** (`walk.rs:433` calls `entry.metadata()` for size and mtime today) | 16 bytes, `dev` then `ino`, little-endian |
| Windows | *nothing, in this phase* | `NULL` |

**Windows stays inert, deliberately, and F18's rule is why.** NTFS has an equivalent (`VolumeSerialNumber` + `FileId` from
`GetFileInformationByHandleEx`), but reaching it means a syscall crate and — the part that actually decides — somebody running
the fixture on a Windows host and recording host, euid, head SHA and command. CI runs `windows-latest`, which proves today that
nothing regresses; it would not prove an identity claim, because a feature that silently reads `NULL` there passes every test
either way. So Phase 1 ships `None` on non-Unix, the code says so at the boundary, and the Windows work is its own item with its
own evidence. A database written on Unix keeps its ids; opened on Windows it simply stops gaining them, which is safe under §4's
rule.

## 3. Storage: one nullable column, backfilled by walking

Migration 6 adds `files.file_id BLOB` — nullable, no `CHECK`, no index. Nothing else changes: **no report field, no catalog
line, no golden byte** (§7 gate). There is no way to backfill it offline, because identity can only be read by stat-ing the file;
so a row carries `NULL` until the next scan that sees its path, and every route below treats `NULL` as "unknown", never as
"different". That is what makes the migration safe on a workspace that is not scanned for a week.

`Snapshot` gains the same field, so `classify` can see it without touching its signature more than the type requires.

## 4. The rule: identity may add a match and may never remove one

This is the load-bearing sentence of the design, and everything else follows from it.

- **Additive.** Identity pairs things that today end up as `Added` + `Removed`. It never vetoes an existing route: if the hash
  claim wants to re-home a row, disagreement of file ids does **not** stop it — because ids legitimately change across a copy,
  a restore, a `git worktree`-style duplication, an rsync, or a container rebuild, and refusing those would turn an improvement
  into data loss.
- **Uniqueness on both sides, or nothing.** A pair is eligible only when exactly one unmatched known row and exactly one
  unmatched found path carry that id, and no *other* listed path carries it either. Two paths sharing one id is a hard link
  (real: same file, two names) — the honest answer there is "identity cannot tell them apart", so identity abstains and today's
  text/content routes decide as before. Same for an id appearing twice among known rows.
- **Only between things nothing else claimed.** Identity runs after `classify`'s identity route and after hash claiming, over
  the leftovers. It is not allowed to break up a pairing another route already made.
- **Never authorises a deletion.** A row that identity does not recognise is treated exactly as it is today. The failure mode
  this feature must never produce is a `Removed` for a file that is on disk — the property F20 and F23 were built to protect.

### Where it plugs in (two places, both additive)

1. **`scan.rs::admits`** — a marked row is admitted when a listed readable path carries its stored id uniquely. Today admission
   needs display equality plus hash agreement; identity is an alternative route to the same yes/no, not a replacement, so a row
   whose file was copied (new id, same bytes) still admits by content exactly as F23 shipped.
2. **`classify`** — after the existing routes, pair remaining unmatched known rows with unmatched found paths on unique equal
   ids and emit `Moved { from }`. The consequence matters more than the mechanism: a `Moved` carries the row's id, so headings,
   tags, items and links stay attached instead of being rebuilt from scratch — which is precisely what the residue case is
   costing today.

## 5. Hazards, named rather than waved at

- **Id reuse.** A deleted file's inode can come back on a brand-new, unrelated file. If that new file is unmatched and the old
  row is unmatched, identity turns `(Added, Removed)` into `Moved`. That is the wrong answer about history and the better answer
  about data: the alternative deletes a note's derived rows to re-create them from the same bytes. Recorded here so nobody
  discovers it in a year and calls it a bug; §6 pins that identity only ever runs on leftovers, which bounds the blast radius.
- **Hard links.** Handled by abstention (§4), not by detection of "is this a link".
- **Network and stacked filesystems.** NFS with `noac`, fuse overlays and some container graphs report ids that are not stable
  across mounts or change between scans. Both directions of error are possible; §4's asymmetry (may add, may not remove) is what
  keeps the bad direction from costing data.
- **Same id, different file after a restore.** Same shape as id reuse. The mitigation is the same: identity never vetoes.

## 6. Tests

1. **The residue becomes convergence.** F23's documented case — marked row, note edited while blocked, obstruction clears — ends
   with one live row on the real bytes, `moved = 1`, the same `notes.id`, derived rows intact, and `PARSE_VERSION` recorded
   instead of withheld. This is the fixture the whole item exists for; it goes into `crates/np-core/tests/scan.rs` next to
   F23's fixtures and its doc comment names the residue it closes.
2. **The route needs two scans.** Identity is only learnable while walking, so the fixture plants the file, scans (id recorded),
   blocks + edits, unblocks, scans again. Asserting the intermediate state (`file_id` non-NULL after scan one) keeps the fixture
   honest about why it converges rather than appearing to.
3. **Non-uniqueness abstains.** Two listed paths with one id (hard link where the filesystem supports it, otherwise planted by
   writing identical metadata through a helper that re-checks the link off disk and prints `NOT COVERAGE` if refused) → no
   identity pairing; behaviour is byte-identical to today's.
4. **Id disagreement never vetoes.** Known row id `A`, found path same bytes at a new key with id `B` → still `Moved` by the hash
   route, exactly as F23 shipped. This test is the §4 rule made executable; without it, a future "tightening" that treats ids as
   authoritative would silently delete notes on the next restore.
5. **`NULL` means unknown.** A row with no stored id and a listed path with no id (the non-Unix case, forced by constructing the
   snapshots) resolve exactly as before — this is also what Windows CI is evidence for: green there means inert, not working.
6. Existing F23 fixtures, including the M fence and `a_permanent_class_row_…`, stay green unedited. If one of them moves, this
   design removed a match somewhere and must be re-read before code continues.

## 7. Cost, and what the benchmark cannot see

`walk` reads identity from metadata it already fetches; `classify` compares small byte strings for leftovers only. The scan-side
read gains one column per known row. A/B/A against `main` at 2k notes is required, with its usual limit stated: that corpus has
no unreadable directory and no marked row, so it measures the common path where identity is read and compared but never decides
anything.

## 8. Out of scope (each is its own item, none is smuggled in here)

Windows/NTFS identity (§2, blocked on a real run); using identity to detect moves that today cost a re-parse when content is
unchanged (a performance nicety, not correctness); stale-row resurrection; any report or catalog surface for the id; and
retiring F23's display-equality clause — **explicitly not**: identity is additive, so `admits` keeps its text-and-hash route and
the residue only stops being permanent where an id was recorded before the obstruction.
