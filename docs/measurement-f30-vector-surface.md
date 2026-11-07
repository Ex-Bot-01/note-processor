# F30 — what a stored vector actually costs, text rows and figure rows

Measurement, not design. F30's card says the first thing it owes is index bytes for a corpus with figures. **Both halves are now measured:** the text half on
2026-11-06 from the shipped binary, the figure half on 2026-11-07 once F28 handed the CLI a projector (`docs/design-f28-image-embedding-in-np.md`). No decision is
taken here — the 512 question stays the human's, which is what these numbers are for. The figure half ends with the comparison this document thinks that decision
should be made against.

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

## The half that was blocked, and by what  *(superseded 2026-11-07 — see "The second half, measured" below; kept because it is what was true when written)*

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


## The second half, measured 2026-11-07 — a corpus with figures

F28 landed the hand-off this half was waiting on: `np-cli` calls `LocalEmbedder::with_mmproj` (`crates/np-cli/src/main.rs:693`,
one call site, reached only when the workspace has recorded a projector whose digest or stamp matches), so an ordinary
`np embed` now writes `media_embeddings` rows and they can be weighed. The paragraph above about the shipped binary being unable
to reach image code is **superseded**; it stayed in place because it is what was true on 2026-11-06, and the sentence that ages
should say when it aged.

Measured on this host with the release+`vision` binary (`--features local-model,vision`, `mtmd_*` symbols present), both real
artifacts installed through the CLI — main via `models install`, projector via `models install --member mmproj`, digests computed
from the files rather than typed. Two phases in **one** workspace, so the text population is the same in both measurements:

| | notes / chunks / vectors | whole database | how |
| --- | --- | --- | --- |
| phase 1 — text only | 20 notes → 20 rows, 0 media rows | **258,048 B** (63 pages × 4,096) | `np scan`, then `np embed` on a fresh data directory |
| phase 2 — same corpus, eight **distinct** PNGs appended to eight of those notes | 20 rows + **8 media rows** | **299,008 B** (73 pages) | figures 1,092–11,083 B on disk, sizes 24×24 through 80×80 |
| phase 2 copy, `media_embeddings` deleted and `VACUUM`ed | 20 rows, 0 media rows | **262,144 B** (64 pages) | run on a copy; the measured database was never mutated |
| phase 2 copy, both vector tables deleted and `VACUUM`ed | 0 rows | **180,224 B** (44 pages) | the non-vector baseline for this corpus |

Exact payload, straight from the table: `sum(length(vector)) / count(*)` = 24,576 / 8 = **3,072 B per media row**, identical to
the note rows' 61,440 / 20 = 3,072 B — one 768-component `f32` each, as §1.2's locked decision says, with no extra wide column
hiding in the media table.

**So a stored figure costs 4,608 B of database and a stored chunk 4,096 B.** Both are the vector surface — payload plus its index
and page slack — isolated by deletion-and-`VACUUM` on copies: (299,008 − 262,144) / 8 = **4,608 B per media row**, and
(262,144 − 180,224) / 20 = **4,096 B per note row**. The media row is about **12.5% dearer** than a text row, which arithmetic on
the payload alone could not have shown: same vector width, more periphery (its own table, its own key, its path and digest
columns). The first half measured 4,369 B per note row on a different corpus; 4,096 B here is the same quantity landing in the
same ballpark with a different page fill, and neither is a constant anyone should quote as *the* per-row cost.

`phase2 − phase1` (40,960 B) is **not** the media surface and is not quoted as such: appending a figure line also changes those
eight notes' bodies, so their chunks re-parse and re-encode. The deletion-and-`VACUUM` pair above is the clean isolation, which is
why the first half used it too.

### What the 512 arithmetic looks like with a measured slack constant

Slack per row is now a measured quantity rather than an unknown: 4,608 − 3,072 = **1,536 B** around a media vector, 4,096 − 3,072 =
**1,024 B** around a text vector. Re-storing at 512 keeps the slack and cuts the payload to 2,048 B:

| | 768 (measured) | 512 (arithmetic on measured slack) | saving |
| --- | --- | --- | --- |
| per figure | 4,608 B | ~3,584 B | **−22.2%** |
| per chunk | 4,096 B | ~3,072 B | −25.0% |
| a vault with 5,000 figures | ~22.0 MB of media surface | ~17.1 MB | **~4.9 MB** |

The first half guessed "roughly −23%" from the text-row ratio; the measured media-row figure is −22.2%, so the guess holds and is
now not a guess about the *ratio* — only about slack staying put when the payload shrinks, which nothing here has tested, because
nothing here can write a 512-wide row.

Put beside what else an image index needs, though: the projector artifact this feature loads is **554,821,056 B**. The ~4.9 MB a
5,000-figure vault would save at 512 is **0.9%** of that file. That is the comparison the decision should be made against, and it
is arithmetic too — but both sides of it are measured bytes, not estimates.

Re-running it: the script is deliberately **not** in the tree — it names this host's artifact paths and a scratch directory, and would only rot. The sequence is
above: build `--features local-model,vision`, `models install` the main file and `models install --member mmproj` the projector, embed a text-only corpus and weigh
it, append one distinct PNG to each of eight notes, scan, embed, weigh again, then delete-one-table-and-`VACUUM` on **copies**. The assertion that keeps a rerun honest
is `media_rows == 8`: a measurement taken from a corpus that is not the corpus described arrives wearing evidence.

### What this measurement nearly got wrong

The first attempt appended figures by a hand-written name list (`kafka`, `ledger`, …) while the corpus files were numbered from a
*different* list order, so four appends landed on real notes and four created brand-new ones — and the run reported
`images links 4 · embedded 4` on its way to four media rows. Nothing failed. The fix is in the fixture: figures are now keyed off
the filenames that actually exist, and it asserts both that the corpus did not gain files and that exactly eight notes carry a
figure before anything is measured. A size measurement taken from a corpus that isn't the corpus described is worse than no
measurement, because it arrives wearing evidence.

One timing observation, deliberately **not** a measurement: the phase-2 embed — 8 image encodes plus 8 text re-encodes with a
projector open — took 33.8 s wall on this host in one run. §8's rule (≥2 runs and a spread) is not satisfied by one run, so treat
it as "this did not take minutes", and nothing stronger.

### Still not measured

* Ranking quality at 512 or 256 — no labelled set exists (**F31**), and the storage case above is not an argument about relevance.
* Whether slack really stays put at a narrower slice: no code path writes a non-768 row, so the 512 column above is arithmetic.
* A real vault. Twenty fixture notes and eight synthetic 24×24-to-80×80 rasters; a vault's figures are photographs and screenshots
  at sizes this table has not seen, and per-row figures extrapolate while totals do not.
* Two figures with identical bytes linked from two notes (content-addressed dedupe) — the row count claim is F28's, pinned in tests,
  not weighed here.
* Windows page size and slack: never run there.

## Not measured here, stated so it is not mistaken for covered

* ~~`media_embeddings` bytes on any corpus (blocked on F28).~~ **Measured 2026-11-07** in the section above.
* Ranking quality at 512 or 256 — no labelled set exists (F31), and a set small enough to assert against could not support the claim.
* Anything about a real user's vault: this is 16 fixture notes, one of which produces no chunk. Per-row figures extrapolate; totals do not.
* FTS/index bytes for the *non-vector* tables (the 147,456 B baseline lumps them together with schema and row content on purpose — splitting
  them needs `dbstat`, which this rusqlite build does not enable).
* Windows page size and slack: CI runs the suites on `windows-latest`, never this measurement.
