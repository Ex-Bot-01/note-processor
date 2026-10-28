# F24 — measure the merged pair once, let each fixture keep its own claim

Design note, 2026-10-27. Backlog: `docs/backlog-m2-followups.md` §F24. Branch: `m7/f24-merge-assertions`. Test-only change;
no production code is touched, and `tests/golden/` stays untouched by construction.

## 1. What is duplicated, exactly

`merging_a_duplicate_pair_keeps_the_oldest_id_and_moves_every_incoming_edge` (v4 fixture) and
`a_version_three_file_jumps_four_to_five_in_one_open_and_its_pair_merges` (v3→v5 fixture) ask the same eight questions of
the same planted shape — both seeders use winner 4, loser 9, third note 12, path `dup.md`:

| Fact | Query (identical text in both) |
| --- | --- |
| survivor ids on the key | `SELECT CAST(id AS TEXT) FROM notes WHERE rel_bytes = {DUP_BYTES} ORDER BY id` |
| winner's heading / tag / item | three queries on `note_id = 4` |
| loser left nothing | four-query loop on `note_id = 9` / `src_note_id = 9` |
| incoming edge re-pointed | `links WHERE src_note_id = 12 AND target = 'Dup second'` → `dst_note_id` |
| receipt | `merge_receipt_of(&db)` |

Not duplicated, and staying where it is: the v4 fixture's *"no edge still claims a row that is gone"* (`dst_note_id = 9`) and
its total-edge count (3 planted-minus-loser's-outgoing vs the v3 fixture's 2), and the v3 fixture's `MAX(version) = 5` plus
its `hex(rel_bytes)` premise. Those are different claims about different fixtures.

## 2. The risk F24 names, and why dedup alone does not close it

The recorded incident: the v3 copy of `SELECT COUNT(*) FROM links WHERE src_note_id = 9` could never fail, because only the
v4 seeder planted the loser's outgoing edge. One fixture gained a row; the other kept reading as coverage. Note what that is —
**not** duplicated assertion text, but an *unasserted precondition*. Extracting the assertion text would have left it
exactly where it was.

**Current state, so nobody hunts a hole that has been closed:** both seeders plant that edge today
(`seed_duplicate_pair_at_version_four` at `db.rs:2162`, `seed_duplicate_notes_at_version_three` at `db.rs:2220`, the latter
with a comment naming this exact failure). Nothing is broken right now. Which is precisely why the second half of F24 belongs
to this item rather than to "already fixed": the row is there because a human remembered to put it there, and no assertion
would notice its removal.

So F24 has two halves, and only the second one is interesting:

1. **Measure once.** A helper reads the facts; each test asserts them.
2. **Assert the planting.** Each seeder proves, off its own connection before the open, that the population it claims to
   plant is present. If a future edit drops the loser's outgoing edge from either fixture, that seeder fails at the point of
   the edit instead of silently converting a merge assertion into a tautology.

## 3. Shape

```rust
/// The facts a merged pair leaves behind, read once so both fixtures ask them the same way (§1). This struct
/// deliberately holds no verdicts: every field is a measurement, and each test compares it with its own message.
struct MergedPairFacts {
    survivor_ids: Vec<String>,
    winner_headings: Vec<String>,
    winner_tags: Vec<String>,
    winner_items: i64,
    /// (the SQL as written, rows left) for each of the four shapes the loser owned.
    loser_residue: Vec<(&'static str, i64)>,
    incoming_edge_target: String,
    receipt: String,
}

fn merged_pair_facts(db: &Db) -> MergedPairFacts;
```

and in each seeder, before it returns:

```rust
assert_planted_pair(&conn);   // two rows on `dup.md` (4 live-shape, 9), one edge out of 9,
                              // one edge from 12 into 9, winner's heading+tag+item present
```

**Both halves of the pair are asserted, per shape.** The first draft checked the winner's corpus as a summed three and said
nothing about the loser's own derived rows — which left three of the four residue zeros both fixtures assert satisfiable by a
pair that was never planted. That is the same class of hole as §2, one row away, and it arrived from me rather than from the
data. So the helper compares two vectors of `(query, count)`: the winner's heading/tag/item at one each, and the loser's
heading/tag/item at one each. Winner queries come in two spellings on purpose — `SELECT text …` for the verdict, which asks
*which* heading survived, and `SELECT COUNT(*) …` for the premise, which asks only whether one was planted.

`assert_planted_pair` takes a `&rusqlite::Connection`, which both seeders already hold (`db.0` in the v4 case, its own
connection in the v3 case), so no fixture has to open the file twice or reorder migrations. **It queries `rel = 'dup.md'`,
never `rel_bytes`:** version three's `notes` has no key column at all (`db.rs:898-907`), so a shared premise check must stay
inside the intersection of the two schemas — the same constraint that makes `ids on the key` a *post-merge* fact in §1 rather
than a planted one.

**Why not one shared test function taking a flag.** The two fixtures exist to make *different* claims — one is about the
merge, one about migration 4 and 5 landing in a single open. A `fn check_merged_pair(db, is_v3: bool)` would either grow a
parameter per difference or start suppressing assertions to fit both, and the messages that say *why this one matters* ("the
edge crossed the rebuild and the merge together" is not the v4 claim) are the part worth keeping. Measurements shared, claims
separate.

**Why no macro.** A macro would preserve messages but hide the queries from `cargo doc`-adjacent reading and from grep, and
the drift it prevents is already prevented by having one query site.

## 4. Acceptance

1. The five SQL blocks are reached from **one call site each**. Not "the text appears once in the file": a third
   near-identical incoming-edge query lives in `a_merge_that_cannot_record_its_receipt_leaves_both_note_rows`
   (`db.rs:2745`) and asserts the *pre*-merge state (`dst_note_id` still 9). That one must stay, and it is a different claim,
   not a missed duplicate.
2. Both fixtures still assert every fact they assert today — no assertion is dropped to make the extraction fit. Verify by
   listing each test's `assert_eq!` count before and after; it must not fall (baseline: 9 and 9, seeders 1 and 1).
3. **Per measured fact, break the production behaviour and watch who goes red** — removal proves nothing, since deleting an
   assertion makes tests pass. Migration 5 mutations to run: stop re-pointing incoming edges → both fixtures red; skip the
   loser's derived-row deletes → both red; keep the loser row (no fold) → both red plus the receipt count.
4. The premise mutation F24 exists for: delete the loser's outgoing-edge `INSERT` from **one** seeder (both plant it today,
   `db.rs:2162` and `db.rs:2220`) → that seeder's `assert_planted_pair` fails, and it must fail even with every merge
   assertion deleted. Before this change the same edit leaves the suite green, which is the hole.
5. Gates as usual; goldens unmoved; suite total does not drop (it may rise by the premise assertions).

## 5. Explicitly out of scope

No change to migration 5, `merge_receipt`, or any production path. No new fixture — v3 and v4 already cover the two ways a
pair arrives. No attempt to unify the *other* merge fixtures (`seed_two_duplicate_groups…`, the nine-group cap case): they
plant different shapes and their assertions are about receipt arithmetic, not about what a fold leaves on disk.
