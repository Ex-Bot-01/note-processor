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
| one **space-free** long CJK title, over 100 columns in a single token | the realistic shape of CJK text, which carries no spaces: there is no break point, so it takes the pinned exemption and its own line reaches `COLUMN_LIMIT + indent`. Row 1 pins wrapping *between* two-column tokens; this pins what happens when there is none — an off-by-one in `COLUMN_LIMIT - indent` shows up here first. |

Paths are ASCII except one (`notes/笔记.md`) so the rendered `rel` also carries two-column bytes into the golden. Worth naming
why row 5 is space-free rather than a sentence of words: my first instinct was a spaced CJK title, which wraps between tokens
and therefore measures the same code path as row 1 in a different alphabet. Real Japanese and Chinese titles run together, and
that is the case where the budget has no legal break — so the fixture needs both shapes, not two spellings of one. Titles are
written with `\u{…}` escapes where a stray editor normalisation would change the measured width — NFC versus NFD is exactly the
difference between zero columns and one for the combining case, and a fixture that silently re-normalises on someone's machine
tests nothing.

## 4. Decision D2 — how the new bytes come to exist

The goldens are produced by running the renderer **once**, then reviewed, then committed — never edited to make a test pass,
and never regenerated after a failure without an explanation of which rule the old bytes broke. Two independent checks happen
before anything is committed:

- the Python verifier (§5) reports `GOLDEN CHECK: pass` with the column-aware rule against the new files, **in an unsharded
   mode that has to be written first** — see §5;
- a mutation, run by the controller and recorded in the commit body: there is exactly one width decision in the renderer,
  `wrap`'s `current.width() + prefix + 1 + token.width()` (`src/catalog.rs:219`) — so replace those two `.width()` calls with
  `.chars().count()` and confirm **the wide golden test fails**. Score it by test name, not by suite: the unit cases in
  `tests/catalog.rs` assert widths directly, so "the M1 binary stayed green" would be measuring the wrong thing. What is
  expected is `golden_render_matches_committed_bytes` green and the new wide golden test red. A fixture that passes under its
  own regression is not coverage; if that mutation comes back green, this design's premise is wrong and the item stops here
  rather than shipping a decorative file.

## 5. Verifier hardening (tooling, not product code)

`verify_catalog_goldens.py` gains three things, and the first is not optional:

- **An unsharded mode.** Today the tool hard-codes the sharded shape: every body line of `INDEX.md` must match `LINK`
  (`verify_catalog_goldens.py:80-82`) and entry parsing, R4 and sorting run only `for name in shards` (:92). The new fixture
  renders with the default `max_entries_per_file`, which produces one `INDEX.md` **of entries**, so on an R4-only edit the new
  directory would either fail with "not a shard link" or — worse, if that check were merely relaxed — be read by no rule at
  all while still printing `pass`. So: a mode that parses `INDEX.md`'s body with `ENTRY`, applies R2/R3/R4/R6 and the
  trailing-newline and CR rules, and skips R5 (links) and R7 (bucket ranges) because neither exists in an unsharded file. Its
  expected file set is asserted by name, so an extra golden is a failure rather than an unnoticed file.
- **R4 becomes column-aware**: width measured with `unicodedata.east_asian_width` (`W`/`F` = 2, combining marks via
  `unicodedata.combining` = 0) against 100 — the rule R4 always meant, which char counting could not express. **With one
  exemption that mirrors the product's own:** a token wider than the columns left keeps a line of its own
  (`src/catalog.rs:194-198`, pinned by `tests/catalog.rs:635`), so a space-free CJK title legitimately reaches
  `COLUMN_LIMIT + indent`. R4 permits that width for a physical line consisting of a single token; forbidding it instead would
  mean banning space-free wide titles from the fixture, which would hide the case worth having.
- **Its own limit stated.** East-Asian-width plus `combining` cannot express zero-width format characters — ZWJ and ZWSP have
  category `Cf` and no width rule here — so R4 is documented as an upper bound for that class rather than a claim to model it.
  §7 already keeps ZWJ sequences out of the fixture.

It stays independent: it encodes the *rules* from the plan, not anything read out of `catalog.rs`. If the two disagree, that
is a finding about one of them, not a thing to reconcile by editing either file's expectations.

## 6. Acceptance

1. `tests/golden-wide/` exists, is compared byte-for-byte by a test next to the existing golden test, and contains at least one
   two-column title — the backlog's done-when.
2. `git diff main..HEAD -- tests/golden` is **empty**, and `GOLDEN CHECK: pass` covers both directories.
3. The char-counting mutation turns the wide golden test red and leaves the M1 one green; recorded with the command, head SHA,
   host and euid.
4. Suite total rises by exactly the new cases; every other gate as usual; CI green on Linux **and** Windows.
5. `.gitattributes` gains `/tests/golden-wide/** -text`, so the new bytes reach a Windows checkout exactly as committed the way
   `tests/golden/**` already does; and backlog F16 is rewritten to what shipped, as every closed item here has been.

**Why Windows CI is on the list, without overstating it:** this fixture is a pure render — nothing creates `notes/笔记.md`, no
filename reaches any filesystem (`tests/catalog.rs:3`: "Nothing here touches the filesystem except the committed goldens"). So
Windows green means *the committed bytes compare equal there too*, which is worth having and is all it means. Any NFC/NFD
question about names on demand belongs to a walk-based case, which this item does not pretend to be.

## 7. Out of scope

No change to `wrap`, bucketing or sharding; no change to `catalog()` or the M1 goldens; no attempt at
grapheme-cluster segmentation (a ZWJ emoji sequence is measured as its code points today, and this fixture documents that by
including one only if the measurement is stable — otherwise it stays out). `entry_line` is listed here as *unchanged* rather
than as a mutation target: it builds the string and measures nothing, which is why §4 names `wrap` alone.
