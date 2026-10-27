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

`assert_planted_pair` takes a `&rusqlite::Connection`, which both seeders already hold (`db.0` in the v4 case, its own
connection in the v3 case), so no fixture has to open the file twice or reorder migrations.

**Why not one shared test function taking a flag.** The two fixtures exist to make *different* claims — one is about the
merge, one about migration 4 and 5 landing in a single open. A `fn check_merged_pair(db, is_v3: bool)` would either grow a
parameter per difference or start suppressing assertions to fit both, and the messages that say *why this one matters* ("the
edge crossed the rebuild and the merge together" is not the v4 claim) are the part worth keeping. Measurements shared, claims
separate.

**Why no macro.** A macro would preserve messages but hide the queries from `cargo doc`-adjacent reading and from grep, and
the drift it prevents is already prevented by having one query site.

## 4. Acceptance

1. Each of the five SQL blocks exists **once** in `crates/np-core/tests/db.rs`.
2. Both fixtures still assert every fact they assert today — no assertion is dropped to make the extraction fit. Verify by
   listing each test's `assert_eq!` count before and after; it must not fall.
3. Deleting any single assertion block turns red **exactly one** test, never both silently-green — checked per block.
4. Mutation: stop re-pointing incoming edges in migration 5 → **both** fixtures fail (they share the fact but keep their own
   claim). Mutation: delete the loser's outgoing-edge `INSERT` from *one* seeder → that seeder's `assert_planted_pair` fails,
   and it must fail even if every merge assertion is deleted. This is the mutation that F24 was filed for.
5. Gates as usual; goldens unmoved; suite total does not drop (it may rise by the premise assertions).

## 5. Explicitly out of scope

No change to migration 5, `merge_receipt`, or any production path. No new fixture — v3 and v4 already cover the two ways a
pair arrives. No attempt to unify the *other* merge fixtures (`seed_two_duplicate_groups…`, the nine-group cap case): they
plant different shapes and their assertions are about receipt arithmetic, not about what a fold leaves on disk.
