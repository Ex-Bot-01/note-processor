# F19 — merge the legacy duplicate note rows instead of paying for them forever

Status: design for the first round of `m5/note-merge`. Backlog: `docs/backlog-m2-followups.md` F19. Prior art:
migration 3's orphan cleanup (`add_note_foreign_keys`, its FK graph and `OrphanCleanup` receipt), F21's durable-first
announcement, F13 §7 (the reader that names duplicates rather than pretending not to have them).

## 1. The defect

Version 3 never put `UNIQUE` on `notes.rel`, so a path could hold more than one note row; migration 4 copied both onto
one `rel_bytes`. **Nothing merged them — as filed, against a build that had no exit**; migration 5 (§3) is the
exit this round adds, and everything below describes the build before it. The scan's reader sees the pair and
returns `Indexed::Duplicate`, which charges `written.uncovered` **on every scan**, so `refresh_covered` never
returns true: the parse version is withheld forever,
every note in the workspace re-parses each run (a permanent several-hundred-millisecond cost on a large corpus), the
refresh-gap line prints with nothing the user can do about it, and the registry row keeps advancing past derived rows
that never change — a mixed-age database by design.

The charge was the right call at the time: recording the parse version beside a note that was not re-parsed is exactly
F15's bug. What was missing is an exit.

## 2. The rule

**Merge the pair once, in the migration, keeping the oldest row; announce it durably before the delete commits.** That
is F21's rule applied to a deletion that already exists — and this program has one precedent for exactly this shape:
migration 3 deletes orphan rows, writes its receipt in the same transaction, and echoes best-effort after the commit.

Choosing the survivor is the whole risk, so it is stated rather than left to chance:

- **Winner = `MIN(id)` per `rel_bytes`.** The oldest row is the one derived rows already hang off (`headings`, `tags`,
  `items`, `links.src_note_id` all cascade from `notes.id`), so keeping it keeps the corpus attached. Content loses —
  see below — and that is acceptable *because* the merge forces a re-parse: the version is already withheld, so the very
  next scan rewrites title, body, headings, tags, items and links for every note from what is on disk. The row identity
  survives; the stored rendering does not have to.
- **Incoming edges are re-pointed, never nulled.** `UPDATE links SET dst_note_id = ?winner WHERE dst_note_id = ?loser`.
  Other notes' prose was not re-parsed and will not be, so nulling a target would lose a resolved link permanently — the
  distinction migration 3 already drew between `ON DELETE CASCADE` (an edge from a note that is gone) and
  `SET NULL` (a target that is gone). Here neither applies: the target still exists under another id.
- **Loser-derived rows are deleted by hand, not by the cascade.** The schema does declare `ON DELETE CASCADE` on every
  one of them and dropping the loser takes its headings, tags, items and outgoing edges with it — but that is the
  *outcome*, and it cannot be the mechanism. A `Migration::Code` step runs with `foreign_keys` enforcement switched off
  (`migrate` switches it outside the transaction, because a table rebuild needs it that way, and the pragma cannot be
  changed inside a transaction), and SQLite performs no `ON DELETE CASCADE` while enforcement is off. So deleting the
  loser's row would commit its headings, tags, items and outgoing edges as orphans into a file whose enforcement is back
  ON the moment the step commits — the silent re-adoption migration 3 exists to end. `merge_duplicate_notes` therefore
  deletes `headings`, `tags`, `items` and the loser's outgoing `links` explicitly, then the note row itself, in
  migration 3's shapes; the following refresh rebuilds them for the winner. Because nothing cascades behind those
  statements, the step ends with `PRAGMA foreign_key_check` (migrations 3 and 4 already run it): an explicit delete that
  is ever softened must fail the open, not leave a dangling `note_id` that no later scan re-checks. No table carries a
  unique constraint (`headings`, `tags`, `items`, `links` are plain), so re-pointing cannot collide — say so in the code
  comment so nobody adds defensive dedup that can never fire.

## 3. Shape

Migration 5, `merge_duplicate_notes`, inside its own transaction like every other step:

1. Select groups: `SELECT rel_bytes FROM notes WHERE rel_bytes IS NOT NULL GROUP BY rel_bytes HAVING
   COUNT(*) > 1`, ordered by `rel_bytes` so one database produces one receipt.

   The `WHERE` clause is not an optimisation, but say plainly what it defends today: **nothing can reach it.**
   Migration 4 rebuilt `notes.rel_bytes` with `CHECK (typeof(rel_bytes) = 'blob')`, and `typeof(NULL)` is
   `'null'`, so a keyless note row is unwritable in any file migration 4 has touched — by this program or by
   anything speaking plain SQL to it. Even the one in-program route fails: a version-three row with `rel IS NULL`
   would copy through `CAST(rel AS BLOB)` as NULL, and that CHECK refuses migration 4 itself. **That constraint is
   what protects keyless rows; this clause has no test because none can be written.** It stays for whoever changes
   that: if a later migration ever admits keyless rows, `GROUP BY` gathers **every** NULL into one group, two
   keyless notes are two unrelated files, and folding them deletes one of their note rows outright — so the clause
   must not be "simplified" away then, and the query here has to keep saying why.
2. For each group: re-point incoming edges from every other id to the winner; delete the loser's derived rows and
   outgoing edges, then its note row — by hand, per §2. Then `PRAGMA foreign_key_check` over the whole file, inside the
   same transaction, refusing the step through `DbError::ForeignKeys` exactly as migrations 3 and 4 do.

   Note what that costs, because §2 justifies the check only through softened deletes and this is wider: the pragma
   answers about the **whole file** and the step runs it whether or not it merged anything, so a workspace damaged by
   something with no bearing on a duplicate pair — a `headings` row whose `note_id` names nothing — now refuses to open
   at the four-to-five step where version four opened it happily. That is intended (the damage is real, an orphan id is
   re-adopted by the next `INSERT`, and no later scan re-checks those keys), and "unconditionally" is pinned twice:
   `the_merge_gate_refuses_damage_that_no_merge_could_have_caused` plants the damage on a file holding **no pair at
   all**, so wrapping the check in `if merged > 0` fails there, and `the_merge_gate_refuses_a_child_row_whose_note_is_gone`
   pins the same refusal beside a pair. It also has no in-program exit: §7 rules out the repair
   command that would be one, so the refusal text — table, rowid, parent — is itself the diagnosis.
3. Write a receipt into `meta` under `NOTE_MERGE_KEY = "note_merges"` **in the same transaction**, in the compact style
   of `FK_ORPHANS_KEY`: `merged=N;paths=a.md,b.md` with at most eight display spellings, ordered by path so one database
   produces one receipt. Zero merges still writes `merged=0`, because the receipt proves the check ran (F21 §2's lesson).
4. Echo best-effort **after** the commit — never inside the transaction, which is F21's Important finding.

Then:

- `Db::open` carries the stored value for the process that performed the merge; `ScanReport::merged_notes:
  Option<String>` is `Some` only when this open merged at least one pair, exactly like `upgrade_orphans`, and both
  stdout and `--json` render that one value.
- `Indexed::Duplicate` **stays**. Migration 5 makes it unreachable for anything this program writes — once a key holds
  one row, the reader takes `NoteRow::One` and every later write updates it — but a database produced by another tool
  could still arrive with a pair, and F13 §7's reader must keep naming that instead of guessing. Its doc gains the
  sentence: this state is now transient for our own databases, permanent only for ones we did not write.

## 4. Invariants

1. **No merge commits without its receipt.** Same transaction, same rule as migration 3; pinned by forcing the `meta`
   write to fail and asserting both note rows survive — not by trusting statement order.
2. **Row identity survives, link resolution survives.** The winner keeps its id; no `dst_note_id` that pointed at a
   loser is left null. Both asserted directly against the database, not inferred from a report count.
3. **The charge clears.** After the merge scan, the next scan reports `uncovered == 0` and records the parse version —
   F19's "Do when", which is the difference between a repair and a warning with better typography.
4. **No new public surface beyond the additive report field.** No `np repair` command: a user-facing command means a
   second path through this logic, and two paths drift. The migration runs at open, which is where every other schema
   fact is established.
5. **Catalog bytes do not move.** Goldens stay bit-identical; the merge changes which id holds derived rows, and the
   following refresh re-renders from disk, so `INDEX.md` content is unchanged for any fixture that does not contain a
   duplicate pair.

## 5. Tests this round owes

- A workspace whose database holds two note rows for one `rel_bytes`, with derived rows on both and **an incoming link
  from a third note to the loser**: after open, one row remains at `MIN(id)`, the loser's derived rows are gone, the
  incoming edge names the winner (not null), the receipt reads `merged=1;paths=…`, and the report field carries the same
  string. Build it with raw SQL in `np-core/tests/db.rs` — no production API makes a pair any more.
- Convergence: the scan after such an open reports `uncovered == 0` and records the parse version, where before the
  merge it withheld both forever. This is the test that says the defect is gone rather than documented.
- Announcement once per process, like F21: a second open reports `None`, while the receipt stays in `meta`.
- Receipt-write failure: both note rows survive and the failure surfaces as an error, not a panic.
- Zero merges still writes `merged=0` (the check ran) and leaves the report field `None`.
- F13 §7's existing duplicate-reader tests must keep passing **unchanged** — they pin the reader that names a pair this
  migration no longer allows in our own databases, and if one of them has to change, that is a signal the design is wrong:
  stop and report.

**A second thing was settled on the controller's decision rather than assumed:** §3 step 1 used to justify its
NULL clause as load-bearing ("a hand-written row can lack it"). A run tried to build the fixture that would prove
it and could not — the CHECK above rejects a NULL key, with `PRAGMA foreign_keys = OFF` or without it. The claim is
corrected here rather than quietly dropped, and the reachable half of the same shape is pinned instead: a merged
pair whose survivor has no `rel` is named by its key's lossy spelling
(`a_merged_pair_with_no_display_text_is_named_by_the_lossy_spelling_of_its_key`).

**One bullet of §5 was overridden, on the controller's decision, and it is recorded here rather than hidden.**
`two_note_rows_on_one_path_share_one_rel_bytes_so_a_note_id_lookup_cannot_be_a_bare_query_row` did change. What moved was
the half that made the pair reachable at all: it seeded a version-three database holding the pair and let migration 4 carry
it in, so once migration 5 existed the fixture no longer described what its name says — the merge folded the pair before any
scan read it. The seed now opens the file to completion first and plants the pair beside it, which is the population that
test is actually about: a database another tool wrote into, after this program's one pass. Its assertions were re-pinned in
place (same names, same three properties, plus a new one: the stored receipt reads `merged=0` while the pair still stands,
which is the proof no second pass ran), and `seed_duplicate_notes_version_three` was deleted with it because nothing else
used it.

The durable half of F13 §7 — the half the stop condition was protecting — is untouched and still pinned twice over:
**a read must never pick a winner silently.** Migration 5 is not such a pick. It runs once, at open, on a rule stated in
advance (`MIN(id)`), it re-points the edges that named the loser rather than dropping them, and it writes its receipt in the
same transaction so the deletion cannot commit unannounced — an announced repair, of exactly the shape migration 3 set with
its orphan cleanup. What F13 refused was a `LIMIT 1` in the reader: a silent, per-query, unrecorded choice that rewrites one
note with another file's contents. The reader still refuses; `an_unmerged_duplicate_pair_still_charges_the_refresh_and_
withholds_the_version` pins that it does.

## 6. Gates

The standing list, no subsetting: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`;
`cargo test --workspace`; `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config`;
`RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps`; the cfg dead-code check; the golden verifier
→ `GOLDEN CHECK: pass`. CI green on Linux **and** Windows is part of "green"; nothing under `tests/golden/` is ever
regenerated. Because this round touches a hot path only at open time, re-run the ignored release benchmark in A/B/A form
against the merge base and report raw numbers — no percentage from a single sample.

## 7. Deliberately not in this round

- **A `UNIQUE` index on `notes(rel_bytes)`.** It would make the state unrepresentable, which is tempting, but it also
  makes F13 §7's reader untestable through any API and turns a defensive arm into dead code maintained by hand. The
  backlog keeps this as a possible later schema step, with the legacy fixtures moved to raw-SQL construction first.
- **A user-facing repair command** (invariant 4). One cost of that ruling deserves a line here rather than living only in
  the code comment: migration 5's whole-file `PRAGMA foreign_key_check` refuses damage it did not cause (see §3 step 2),
  and such a file therefore has no way back inside this program — the message names the offending table, rowid and parent,
  and fixing it means `sqlite3`. A repair command would have been the exit; two paths through the merge were judged worse
  than no exit at all, and that trade is being made knowingly.
- **Merging across display twins** — two byte-distinct paths that share a lossy spelling are not duplicates of each
  other; that is F23's ambiguity, and this migration groups by `rel_bytes` precisely so it cannot reach into it.
