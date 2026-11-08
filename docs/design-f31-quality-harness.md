# F31: the quality harness exists; the measurement still needs a human

Written 2026-11-09. Companion to `.github/scripts/f31_eval.py` and to card **F31** in `docs/backlog-m2-followups.md`, which stays open after
this document — deliberately. Nothing here measures whether search is good. It makes that measurement one command, and it makes the
*arithmetic* of the measurement testable today, so the day labels exist nobody is also debugging a scoring function.

## What the harness does

```bash
.github/scripts/f31_eval.py <workspace> --labels my-labels.tsv \
    [--np target/release/np] [--modes keyword,semantic,hybrid] [--k 5] [--strict]
```

A label file is one query per line:

```text
# comment lines and blanks are ignored
what does the migration note say about rollback paths?   projects/migration.md   note
the diagram of the ingest pipeline                        img/ingest.png          image
```

For each mode it runs `np search <workspace> <query> --mode <m> --json`, finds where the labelled path landed in the ranked list, and prints
**recall@1, recall@k, MRR, how often an image outranked a labelled note, and how many answers never surfaced at all** — per mode, plus every
query's own row. `--strict` exits 3 when any labelled answer is absent from every mode's list, so a stale label file cannot quietly score well
on the queries that happen to be answerable.

## Three rules the reporting follows, and why each one exists

+ **A miss counts as zero; it is never dropped.** An MRR computed over only the answered queries *rises* when recall falls. That is not a
  hypothetical: the mutation that does it (`answered = [r for r in results if r.rank]`, divide by `len(answered)`) moves the self-test's planted
  figure from 0.458 to 0.611 — flattering, and wrong. `--self-test` pins both halves of that.
+ **Unsurfaced is its own column.** An answer that appears in no mode is a different fault from ranking badly: usually the label names a path
  the index does not hold (renamed, unindexed, or simply mistyped). Rolling it into recall@k hides a broken label file inside a mediocre score.
+ **Every metric is printed per mode, with the run's shape above it** — binary version, which modes ran, `k`, label-file path. Hybrid beating
  keyword by 0.02 over eight queries is noise wearing a decimal point; showing both columns is what lets a reader see that.

## What this cannot settle on its own

F31's claim is about *usefulness*: does search find the figure that answers a question a person actually asked. That needs labels from a real
vault, because the alternative — generating queries from the same corpus being searched — is circular by construction. A query lifted from note
X and scored against note X measures token overlap, not retrieval quality; it will look excellent on any keyword engine ever written.

**For that reason this commit ships no label file.** The plan asked for a starter set over a synthetic corpus; producing one would create a
number that means nothing in a file whose name implies it means something. When the human labels ~20 queries from their own notes — or records
that they would rather not, and why — F31 closes on that, and `f31_eval.py` is the thing that runs.

Making labels takes about ten minutes: pick twenty questions you actually wanted answered, write each one as you would type it, and name the file
that answers it (`np search <ws> <anything> --mode keyword --json | jq -r '.hits[].path'` lists candidate paths as the index spells them). Prefer
some queries whose answer is a figure — that is the case §14.4 cares about, and `img>note` in the table is the column that reports it.

## What is verified today

+ `.github/scripts/f31_eval.py --self-test` — recall@k, MRR, the image-outranks-note count, unsurfaced counting, both empty-list cases, and the
  exclude-the-misses mutant, against hand-computed numbers. It runs with no binary, no model and no workspace, which is why it can be a gate row:
  `gates.sh` calls it as `f31-selftest`.
+ End-to-end plumbing against the packaged `v0.2.2` Linux binary on a two-query file, one answerable and one not: `recall@1 0.500`, `MRR 0.500`,
  `unsurfaced 1`, exit 0 — and exit 3 with `--strict`. Those numbers calibrate the harness; they say nothing about search quality, which is the
  distinction the tool prints on every run so it cannot be quoted out of context by accident.
