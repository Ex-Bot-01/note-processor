# F16 — wide-character goldens, added rather than regenerated

Design note, 2026-10-27. Backlog: `docs/backlog-m2-followups.md` §F16. Roadmap: Track A item **A3**. Branch:
`m9/wide-goldens`.

## 1. What the golden check actually renders today

`golden_render_matches_committed_bytes` (`crates/np-core/tests/catalog.rs:943`) renders a **hand-written entry list**
(`catalog()`, `catalog.rs:48`) with `config(4)` and compares the bytes to `tests/golden/`. The on-disk fixture corpus in
`tests/fixtures/corpus` is a separate thing used by the scan and CLI tests; the backlog's "the shared corpus that would need
the new note" points at it, but the goldens do not come from it. That matters for scoping: this item is about the renderer's
golden pair, not about the walk fixtures.

Those six entries hold an em dash and umlauts — both **single-width** — which is exactly why F12's fix from characters to
display columns moved zero golden bytes. So today a regression that reverts `wrap` to counting characters is caught only by
the unit cases in `tests/catalog.rs`, never by the byte-for-byte check, and the independent verifier cannot see it either: its
R4 rule counts **characters** per line (`verify_catalog_goldens.py`, "every line <= 100 chars"). A corpus of two-column text
would sail through both.

## 2. Decision D1 — add a second golden set; do not regenerate the M1 one

The backlog proposes adding one wide-titled note to the fixture list and regenerating every affected golden in its own commit.
That is a workable reading, and this design declines it for three reasons:

1. **Those bytes are evidence.** "F12 changed the measurement and moved zero committed bytes" is a claim about `tests/golden/`
   as it stands; the diff against M1 is empty at every merge and is quoted in merge messages. Regenerating them spends that
   artifact to buy what an additive directory buys anyway.
2. **The churn buries the point.** A CJK title sorts into the `#` bucket, `config(4)` shards by bucket, so adding one entry
   renames shard files and re-lays-out every other entry's line. The review then has to separate "wide-character bytes" from
   "shard-set churn" inside one diff — precisely the mixing that makes golden regeneration dangerous.
3. **Additive is checkable by the independent tool; regenerated is not.** A new directory can be validated from the rules
   alone. A modified file cannot be, because there is no prior expectation to validate against once the bytes are allowed to
   move.

So: a new directory `tests/golden-wide/` with its own entry list and its own test, plus one hardening of the verifier (§5).
`golden_render_matches_committed_bytes`, `catalog()` and every byte under `tests/golden/` stay untouched — verified by
`git diff main..HEAD -- tests/golden` being empty at merge.

**What this does not buy, said plainly:** the wide goldens do not exercise shard splitting, because the fixture renders with
the default `max_entries_per_file` and produces one `INDEX.md`. Shard names and ranges stay covered by the M1 set only. If a
future width change interacts with bucketing, that is a new fixture, not this one.

## 3. The fixture

Five entries in `wide_catalog()` (`crates/np-core/tests/catalog.rs`), chosen for what each measures rather than for volume:

| Title | What it pins |
| --- | --- |
| `笔记 目录` (CJK, all two-column) | the plain case: columns = 2 × chars, so a char-counting regression changes the break |
| `🚧 Roadmap — in progress` | emoji (two columns) beside an em dash and hyphen (one each), so a table swap is visible |
| `Café du Marché` built from `e` + U+0301, `o` + U+0301 | combining marks contribute **zero** columns: the opposite sign of the CJK case, and the one a naive "wide = 2" fix gets wrong |
| one long ASCII entry | an unambiguous wrap point in the same file, so the wide lines are compared against a known-good neighbour |
| `日本語 の タイトル で 折り返し を 起こす ほど 長い メモ` | wrapping *inside* two-column text: continuation indent plus CJK budget, which is where an off-by-one in `COLUMN_LIMIT - indent` shows up |

Paths are ASCII except one (`notes/笔记.md`) so the rendered `rel` also carries two-column bytes into the golden. Titles are
written with `\u{…}` escapes where a stray editor normalisation would change the measured width — NFC versus NFD is exactly the
difference between zero columns and one for the combining case, and a fixture that silently re-normalises on someone's machine
tests nothing.

## 4. Decision D2 — how the new bytes come to exist

The goldens are produced by running the renderer **once**, then reviewed, then committed — never edited to make a test pass,
and never regenerated after a failure without an explanation of which rule the old bytes broke. Two independent checks happen
before anything is committed:

- the Python verifier (§5) reports `GOLDEN CHECK: pass` with the column-aware rule against the new files;
- a mutation, run by the controller and recorded in the commit body: replace `.width()` with `.chars().count()` in
  `wrap`/`entry_line` and confirm **the wide golden test fails** while the M1 golden test stays green. A fixture that passes
  under its own regression is not coverage; if that mutation comes back green, this design's premise is wrong and the item
  stops here rather than shipping a decorative file.

## 5. Verifier hardening (tooling, not product code)

`verify_catalog_goldens.py` gains:

- **R4 becomes column-aware**: line width measured with `unicodedata.east_asian_width` (`W`/`F` = 2, combining marks via
  `unicodedata.combining` = 0) and checked against 100 — the rule R4 always meant, which char counting could not express.
- **the new directory** validated by the same rules, with its expected file set asserted so an unexpected extra golden is a
  failure rather than an unnoticed file.

It stays independent: it encodes the *rules* from the plan, not anything read out of `catalog.rs`. If the two disagree, that
is a finding about one of them, not a thing to reconcile by editing either file's expectations.

## 6. Acceptance

1. `tests/golden-wide/` exists, is compared byte-for-byte by a test next to the existing golden test, and contains at least one
   two-column title — the backlog's done-when.
2. `git diff main..HEAD -- tests/golden` is **empty**, and `GOLDEN CHECK: pass` covers both directories.
3. The char-counting mutation turns the wide golden test red and leaves the M1 one green; recorded with the command, head SHA,
   host and euid.
4. Suite total rises by exactly the new cases; every other gate as usual; CI green on Linux **and** Windows — the last one is
   load-bearing here, because a file name in the fixture (`notes/笔记.md`) goes through the walk path on a filesystem whose
   Unicode normalisation differs from this machine's.

## 7. Out of scope

No change to `wrap`, `entry_line`, bucketing or sharding; no change to `catalog()` or the M1 goldens; no attempt at
grapheme-cluster segmentation (a ZWJ emoji sequence is measured as its code points today, and this fixture documents that by
including one only if the measurement is stable — otherwise it stays out).
