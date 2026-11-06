# F30 (first half) — what a stored vector actually costs

Measurement, not design. F30's card says the first thing it owes is index bytes for a corpus with figures; this measures the half that is
reachable from the shipped binary today and states precisely why the other half is not. No decision is taken here — the 512 question stays
the human's, which is what these numbers are for.

Measured 2026-11-06 on this host (16 CPUs, 7,419 MB RAM), `np` built `--features local-model`, artifact
`embeddinggemma-2-Q8_0.gguf` (309,855,456 B, sha256 `2188ac1d…`), a fresh data directory, and the repository's own fixture corpus at
`tests/fixtures/corpus/` — **16 notes**, which is a fixture, not anybody's vault. Two runs; the second differed only in having nothing to do.

## What was measured

| Quantity | Value | How |
| --- | --- | --- |
| Notes scanned / chunks / stored vectors | 16 / 15 / **15** | `select count(*)` over `notes`, `chunks`, `embeddings`. One note has nothing to cut, so it has no chunk and no row — the state `scan.rs` documents rather than a gap |
| Payload of one stored vector | **3,072 B** exactly | `sum(length(vector)) / count(*)` = 46,080 / 15, i.e. 4 × 768 as §1.2's locked decision says |
| Whole database, vectors present | **212,992 B** (52 pages × 4,096) | `pragma page_size`, `pragma page_count`, and the file size agrees |
| Same database, vectors deleted and `VACUUM`ed | **147,456 B** | run on a **copy** of the database; the measured original was never mutated |
| **Vector surface** | **65,536 B for 15 rows → 4,369 B per stored row** | the difference above, divided by rows: payload plus its index and page slack |
| `media_embeddings` | 0 rows, 0 B | see below — this is the half that is blocked |

Two runs: the first wrote the 15 rows (`embedded 15`), the second owed nothing (`current 15 embedded 0`). The byte counts above are from the
populated database and agree between runs, because the second run changed nothing; there is no spread to report on a deterministic rebuild of
the same corpus, and pretending otherwise would be theatre. What *was* measured twice with different results is stderr volume, in the last
section — that one genuinely varies, with work rather than with randomness.

## The half that is blocked, and by what

`media_embeddings` stayed empty because **the shipped binary cannot reach image code**: `np-cli` never calls
`LocalEmbedder::with_mmproj` (`grep -c with_mmproj crates/np-cli/src/main.rs` → 0), which is exactly F28. The projector path exists in
np-core and works — R7c encodes figures through it in `tests/local_vision.rs` — but no user-facing command supplies a projector file, so no
CLI run can produce an image row to weigh.

So the figure F30 asks for — bytes for a corpus *with* figures — is **not measured here**. What follows is arithmetic on the measurement
above, labelled as such and not offered as a number:

* A media row has the same shape as a note row: one 768-component `f32` vector in a table with the same kind of primary key. On that shape,
  each distinct figure costs about the **4,369 B** measured per note row, so a vault with 500 figures adds roughly 2.1 MB and one with 5,000
  roughly 21 MB — before whatever its own index overhead turns out to be, which is precisely the part arithmetic cannot see.
* Re-storing at 512 would cut payload from 3,072 B to 2,048 B per row (−33%), and on the measured surface that is roughly 1,024 B per row of
  the 4,369 — about **−23%**, not −33%, because the index and page slack do not shrink with the slice. Whether ranking survives the slice is a
  different question with no measurement behind it here (§8's rule applies: that number needs ≥2 runs and a spread, and there is no labelled
  set to rank against — F31).

Both bullets are arithmetic on one measured quantity. Neither replaces the measurement F30 still owes, which needs F28 resolved first: either
the CLI gains image support and this table gains real `media_embeddings` rows, or it stays library-only by decision and the question is
answered for CLI users by saying so.

## stderr volume, because F33's numbers were about a different command

The same runs measured something F33's tables did not cover: at the default `warn` level, an embed that **does work** wrote **1,670 bytes over
18 lines**, where a search that loads the model wrote 424 bytes over 4 lines. The difference is one line repeated —

```
init: embeddings required but some input tokens were not marked as outputs -> overriding
```

— which llama.cpp logs per batch (`src/llama-batch.cpp:212`, `LLAMA_LOG_WARN`), so warning volume scales with **work done**, not with the load.
The second run, which loaded nothing, wrote 0 bytes. README and F33's §5 said "424 bytes" as though it were the command's steady state; both
now say what was measured instead. Silencing that line means `log_level = "error"`, which also silences a real load failure — the trade F33
took deliberately, in the other direction, and still does.

## Not measured here, stated so it is not mistaken for covered

* `media_embeddings` bytes on any corpus (blocked on F28).
* Ranking quality at 512 or 256 — no labelled set exists (F31), and a set small enough to assert against could not support the claim.
* Anything about a real user's vault: this is 16 fixture notes, one of which produces no chunk. Per-row figures extrapolate; totals do not.
* FTS/index bytes for the *non-vector* tables (the 147,456 B baseline lumps them together with schema and row content on purpose — splitting
  them needs `dbstat`, which this rusqlite build does not enable).
* Windows page size and slack: CI runs the suites on `windows-latest`, never this measurement.
