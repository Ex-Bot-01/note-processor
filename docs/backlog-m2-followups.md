# M2 follow-up backlog (carried out of the M1 whole-branch review)

**How to read a card's status.** A card is closed when it carries a **Closed on …** paragraph; the absence of one used to mean "open", and on
2026-11-06 that reading proved wrong: work landed in hardening rounds whose commit messages named several cards at once, and the cards were
never updated. Twenty-two cards carry a **Closed on …** paragraph now: sixteen marked by the two audit passes of 2026-11-06 (eight each), four
written by the round that did the work (F26, F32, F33, F34), and two closed on the human's ruling of 2026-11-06 once merged (F25, F35). Each names the evidence that shows the fix is in the tree — a file:line that did
not exist when the card was filed, or a gate row that now passes. Cards closed some other way say it in their own words (“Resolved in …”,
“**shipped**” in the heading), and those words are theirs, not this convention. A card with neither has not been re-checked: treat it as open,
and add the evidence line when one closes rather than editing the prose away.

Audited 2026-11-06 (first pass): F1, F2, F3, F4, F5, F6, F8, F14 → closed below.

Audited 2026-11-06 (second pass): F7, F9, F10, F15, F18–F25. Eight of them carry a **Closed on** paragraph now — F7, F9, F10, F15, F19, F20,
F22, F24 — each quoting the file:line or the test that puts the fix in the tree, plus the command re-run to check it. Four stay open and say
what was checked and what is left: **F18** (needs a Windows box; nothing here can even cross-compile the arm it is about), **F21** (the panic
is gone; a person still cannot learn what an old upgrade deleted without `sqlite3`), **F23** (its “Still open — the residue” is narrower than
it reads, because A4 shipped after the card was written), **F25** (re-measured: the shape still comes off the renderer; it waits on a human
decision about `- [[title]]`, not on a measurement). **[Later the same day]** two of those waits ended: the human ruled on F25 (keep the
bullet, option A) and on F35 (refuse unknown keys everywhere); both were implemented on branches, merged with history into `main` at `4ec0794`
and `ec79e93`, and carry **Closed on** paragraphs below. The wait before the merge was the convention working, not stalling — a branch is not
the tree, and the same rule is what caught the miscount above. Not audited as far as this file knows:
**F28–F31**. Two cards are off the queue for
reasons an audit did not have to discover: F26 carries its own Closed on paragraph (2026-11-03), so it is closed rather than awaiting a check,
and F27 holds a deliberate do-not-implement stance that an audit does not overturn.

**[2026-11-07]** Of the four that stayed open above, **F23 now carries a Closed on paragraph**: its residue was driven through its own "do when" and turned out to be
one stale registry row rather than a lost note, and the human accepted option A (accept and document) over clearing such rows. **F25** and **F35** were already ruled
and closed below. Audited the same day: **F30 closed** — both halves of the storage measurement are in (`docs/measurement-f30-vector-surface.md`) and the human ruled
to **keep 768**, with the numbers that decided it written into the card; **F31 stays open and blocked**, needing about twenty labelled queries from a real vault or a
recorded decline, and it is also what keeps F30's ranking question unanswerable. Still open: **F18** (needs real Windows hardware), **F21** and **F28** (both
implemented; their closure paragraphs wait on a graded tree — see `docs/ops-actions-minutes.md` for what CI grades now and how to buy a full grade for one SHA).


These are not new features. They are the items the Milestone 1 reviews judged *correct enough to ship* but
worth fixing before the code carries embeddings, clustering or a UI on top of it. Each entry lists what was
decided in M1, the evidence, and how to tell a fix is done. Line references are as of `b719138` (merge of
`m1/core-catalog`).

Order of attack, round by round — each round is one task brief with its own review gate:

| Round | Items | Why this order |
| --- | --- | --- |
| R1 | F1, F2, F3 | test and doc hygiene; zero behavioural risk, unblocks honest `cargo doc` |
| R2a | F4, F6, F14 | small and independent: connection pragma, fingerprint tolerance, hermetic config tests |
| R2b | F5 | the one schema migration, kept alone because it rebuilds tables |
| R3 | F7, F8 | parsing semantics that change stored data, so they must land before M2 writes new rows |
| R3b | F15 | re-parse trigger: without it the shipped rules never reach existing databases |
| R4a | benchmark only | lands first so F9/F10 can be measured honestly (see the measurement protocol below) |
| R4b | F9, F10 | performance, invisible at 1.5k notes but wrong to carry into a watcher |
| R5 | F11, F12, F13 | naming/rendering/identity cleanups that M2's `processed/` writer would otherwise copy |

---

## F1 — chmod-based walk test leaks an unremovable temp dir on panic

- **Where:** `crates/np-core/tests/walk.rs:288` (`.expect` before the mode is restored at `:300`).
- **M1 decision:** accepted; a panic there only leaks a temp dir.
- **Fix:** restore the mode in a drop guard (RAII) so any failure path still chmods back, or restore before the
  assertions. **Done when:** a deliberately failing assertion inside that test leaves no read-only directory behind
  (`strace`-free check: re-run the test suite twice after `cargo test -p np-core --test walk`).

**Closed on 2026-11-06 by audit — the drop guard the card asked for is what the test now uses.** `crates/np-core/tests/walk.rs` reads the
directory's own mode first and comments it as “whatever was there is what the guard puts back”, so a failing assertion between the `chmod 0`
and the restore no longer leaks a read-only directory. The card's own “done when” (re-run the suite twice after a deliberate failure) was
satisfied by that structure rather than re-measured here; what is verified is that the mechanism exists at the site the card named.
## F2 — skipped configuration tests stay silent

- **Where:** `crates/np-core/tests/config.rs:157` returns early when a precondition is unmet; its sibling at `:145`
  prints `eprintln!("skipping…")`.
- **Fix:** same visible skip message, so a green CI run cannot hide an untested branch on some platform.
  **Done when:** every early return in that file either asserts or prints.

**Closed on 2026-11-06 by audit, in `cc8ee4a` (“hermetic and honest config tests”).** `grep -n 'return;' crates/np-core/tests/config.rs`
now finds no bare early return, so every unmet precondition in that file prints rather than exits quietly — the card's “done when”.
## F3 — rustdoc warnings

- **Where:** 9 remaining from `cargo doc -p np-core --no-deps`: `items.rs` ×5, `scan.rs:11` ×2, `walk.rs`
  (`long_prefix`), `parse.rs` (ambiguous link). Mostly `private_intra_doc_links`.
- **Fix:** plain code spans or fully-qualified paths; add `cargo doc -p np-core --no-deps --deny warnings` as a CI
  step so it cannot regress. **Done when:** the command is clean and CI enforces it.

**Closed on 2026-11-06 by audit — rustdoc warnings are now a gate failure, at three feature levels.** The gate list runs
`RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps` plus the `local-model` and `local-model,vision` variants, so a new
warning fails CI instead of accumulating. This round's own example: a public-to-private intra-doc link in F33 was caught by exactly that row.
## F4 — no `busy_timeout`, though WAL exists for concurrent access

- **Where:** `crates/np-core/src/db.rs:182` — that `execute_batch` sets `journal_mode = WAL` and `foreign_keys = ON`
  but no `busy_timeout`, while the rationale comment at `db.rs:149-152` says WAL exists because a scan and an `np`
  command can overlap; the registry DB opens without one too (`config.rs`).
- **M1 decision:** single-process in M1, so a busy error is merely ugly.
- **Fix:** `PRAGMA busy_timeout` on every connection (workspace DB and registry), value from config or a constant.
  **Done when:** a test opens two connections where the second writes while the first holds a transaction and the
  write succeeds after waiting rather than returning `SQLITE_BUSY`.

**Closed on 2026-11-06 by audit, in `cc8ee4a` (“busy_timeout”).** `crates/np-core/src/db.rs:538` and `:552` set
`PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}` before anything else, with the comment explaining it also covers the schema path; the report gained a
`busy-gap` line so a run that waited says so.
## F5 — child tables have no foreign keys

- **Where:** `crates/np-core/src/db.rs:68-101` — `headings`, `tags`, `links`, `items` carry `note_id` with no
  `REFERENCES notes(id)`; the invariant is held by hand in `scan.rs:517-528`.
- **Risk:** one forgotten delete re-attaches an orphan edge to a recycled rowid.
- **Fix:** a new migration adding FK constraints (SQLite needs table rebuild for existing tables), plus
  `ON DELETE CASCADE` where the child is meaningless without its note; keep the manual cleanup as defence in depth.
  **Done when:** a test inserts a child row with a dangling `note_id` and it fails, and every M1 scan test still
  passes unchanged.

**Closed on 2026-11-06 by audit.** `crates/np-core/src/db.rs` declares 15 `REFERENCES` clauses and turns enforcement on with
`PRAGMA foreign_keys = ON` (`:552`); the orphan-receipt machinery (`FK_ORPHANS_KEY`) exists precisely because those constraints can fire.
## F6 — published-file fingerprint rejects CRLF

- **Where:** `crates/np-core/src/scan.rs:1008-1018` (`np_published`) versus `catalog::HEADER`.
- **M1 decision:** fail safe — a shard whose line endings an editor rewrote is kept and reported in `skipped`
  forever rather than deleted on a normalized guess.
- **Fix:** accept `\r\n` between the two exact header lines (still no slurp, still byte-exact per line), so an
  externally reformatted shard is reclaimed instead of orphaned. **Done when:** the existing leak-path test in
  `crates/np-core/tests/scan.rs:690-782` is updated to assert deletion, and a file with *different* prose but a CRLF
  header still survives.
- **Accepted residual (R2a, not an oversight):** the two header lines are the whole identity signal, so a
  hand-written file that carries them under a generated name **is** deliberately deleted — authorship of the body
  is unknowable from disk, and rendering the corpus to compare bytes is the guess the fingerprint exists to avoid;
  `a_hand_written_file_carrying_our_header_under_a_generated_name_is_deleted` in
  `crates/np-core/tests/scan.rs` pins that behaviour.

**Closed on 2026-11-06 by audit, in `cc8ee4a` (“CRLF-tolerant catalog fingerprint”).** The fixture corpus carries
`crlf-note.md`, and CRLF handling is asserted in `tests/chunk.rs`, `tests/items.rs` and `tests/links.rs`.
## F7 — a leading horizontal rule reads as frontmatter

- **Where:** `crates/np-core/src/parse.rs:146-160`: any file whose first line is exactly `---` with another `---`
  later loses that region from headings, words and items.
- **Fix:** require at least one `key:` line in the block before treating it as frontmatter; add a fixture note that
  opens with a horizontal rule followed by prose. **Done when:** that note keeps its first section in `headings` and
  `word_count`, while every existing frontmatter test still passes.

**Closed on 2026-11-06 by audit, in `c2f68ff` (“require a key before reading frontmatter, fold tags by Unicode case”) — the card's rule is the
deciding test.** `crates/np-core/src/parse.rs:207` returns “not frontmatter” for a `---` region that holds content and no key
(`if !keyed && !blank_only`); the rule is written up as F7's own at `parse.rs:169-183`, and `key_line` (`:257`) is the whole test. The fixture
this card asked for exists — `tests/fixtures/notes/thematic-break-note.md` opens with `---` and then prose — and
`crates/np-core/tests/parse.rs:202 leading_rule_is_content_not_frontmatter` pins the “Done when”: `!note.frontmatter`, headings
`(2, "Erste Liste", 4)` and `(1, "Zweite Sektion", 10)`, the title won by that heading, and `note.words ==
lf.split_whitespace().count()`. Re-run here: `cargo test -p np-core --test parse` → **exit 0, 11 passed / 0 failed**, which is this case plus
every frontmatter case that had to keep passing.

## F8 — tag dedup folds ASCII case only

- **Where:** tag normalisation in `crates/np-core/src/parse.rs` (see the `#ÄNER-TAG` gap recorded in the M1 ledger).
- **Fix:** Unicode-aware case folding for dedup while preserving the first spelling for display. **Done when:** a
  note with `#ÄNER-TAG` and `#äner-tag` yields one tag, rendered as first seen.

**Closed on 2026-11-06 by audit — the fold is now full Unicode, not ASCII.** `crates/np-core/src/parse.rs:467-487` documents the
choice in the direction the card wanted (`str::to_lowercase` kept deliberately, with the `#ÄNER-TAG` example that `to_ascii_lowercase` would
leave beside it) and applies it at `:487`.
## F9 — per-statement re-preparation in the write path

- **Where:** `crates/np-core/src/db.rs:242`, `:308`, `:324` use `Connection::execute`; the scan path already uses
  `prepare_cached` (`scan.rs:650`).
- **Fix:** cached prepares for `upsert_file`, `apply_move`, `mark_stale`. **Done when:** behaviour is unchanged
  (all existing tests green) and a repeated-scan benchmark on the fixture corpus does not regress.

**Closed on 2026-11-06 by audit, in `14b2935` (“cache write-path prepares, resolve note ids once”) — all three statements are cached
prepares.** `upsert_file` at `crates/np-core/src/db.rs:1581`, `:1595`, `:1616`; `apply_move` at `:1743`, `:1761`, `:1780`; `mark_stale` at
`:1796`. The doc at `:1565-1572` says what the caching buys (one prepare per path, not three per file) and that no error — the identity
collisions included — reaches the caller differently. “Behaviour unchanged” re-measured here: `cargo test --workspace` → **exit 0, 38 suites,
0 failed**. The benchmark was re-run twice at `2c65525`, per §8: cold **500 / 497 ms**, warm **92 / 92 ms** — and those two figures are *not*
comparable to the rows in the table below, because a cold pass now also writes chunk rows (M2a `f752485`, three days after `14b2935`) and reads
file identity (`cbcb12a`); they record what head costs today, not an F9 delta. The table's own caveat survives this audit untouched: no figure
in it may be quoted as an improvement without an A/B/A repeat, and no A/B/A was run here.

## F10 — `note_id` looked up twice per note

- **Where:** `scan.rs:489` then again at `:517` / `:596`.
- **Fix:** resolve once per note and pass the id down. **Done when:** no duplicated lookup remains in the re-parse
  path and the scan tests are unchanged.

**Closed on 2026-11-06 by audit, in `14b2935` — one lookup per note, and the id travels instead of being read again.** `index_file` resolves
once at `crates/np-core/src/scan.rs:1422` (`let existing = match note_row(db, &path)?`) and hands that answer to `store_note`, whose
`existing: Option<i64>` parameter (`:1583`) is what removed the second read; `store_note`'s doc at `:1567-1571` names the single branch that
used to ask twice (its `rewritten || note_id(…)?.is_none()` short-circuits on `Added | Modified`). It returns the id it wrote under, so the
chunk pass hangs its rows off the same value rather than looking it up again, and `links.rs` resolves each path once through its own cache
(`crates/np-core/src/links.rs:512`, `:516`). “The scan tests are unchanged” re-measured here: `cargo test -p np-core --test scan --
--nocapture` → **exit 0, 74 passed / 0 failed with zero `NOT COVERAGE` lines** (host `pi`, euid 1000). The measurement protocol's limit is
quoted rather than smoothed over: nothing in the benchmark measures F10, because a warm pass never reaches the re-parse path.

## F11 — the generated-name rule lives in three places

- **Where:** the literal `"INDEX.md"` and the shard pattern are built in `catalog.rs:78`, `:103-105`, `:118`; a
  second const `CATALOG_ENTRY` holds the same name in `scan.rs:56`; a third spelling lives in `walk.rs:23-25`
  (`INDEX_STEM`, `INDEX_SHARD_PREFIX`) behind `is_generated_index` (`walk.rs:236`).
- **Fix:** one `pub mod index_names` (or equivalent) owning the entry-point name, the shard pattern, the header and
  the predicate; everyone else calls it. M2's `processed/` writer must not have to copy this rule.
  **Done when:** `rg '"INDEX' crates/` finds the literal only in that module and in tests.
- **Resolved in `84e3cf8`, with one deliberate deviation.** The entry-point name, shard pattern and predicate moved;
  `HEADER` stayed in `catalog.rs`, because it is rendered output rather than a name and its CRLF variant is decided by
  the renderer. The `Done when` check holds — no `"INDEX…"` literal survives outside `index_names.rs` and tests. M2's
  `processed/` writer takes the names from `index_names` and needs no header rule at all.

## F12 — wrapping counts characters, not display width

- **Where:** `crates/np-core/src/catalog.rs:170-176`.
- **M1 decision:** acceptable; output stays deterministic and §14 leaves the column rule to the builder.
- **Fix:** measure with display width (East Asian wide, combining marks, emoji) while keeping the wrap point a pure
  function of bytes so byte-stability holds. **Done when:** a CJK-titled note renders within 100 *columns* and the
  golden files are regenerated deliberately in their own commit.
- **Resolved in `42bf672` plus its review round.** Width comes from `unicode-width`, pinned exactly (`=0.2.2`) so a
  Unicode revision cannot move a wrap point silently, with the relied-upon widths asserted in `pinned_display_widths_of_
  the_rendered_shapes`. The review caught that the budget ignored the continuation indent — a wrapped entry reached 101
  visible columns against a 100-column promise — the indent is now charged before a token is appended to a
  continuation line, pinned at indents 0, 2 and 5. One exemption stays deliberately: a single token wider than the
  columns left on its line keeps a line of its own (spec §14 forbids breaking inside a path), so that line may reach
  `COLUMN_LIMIT + indent`; documented on `wrap` and pinned by
  `an_over_wide_token_on_a_continuation_line_reaches_the_documented_ceiling`.
  **No golden was regenerated**: every committed golden is ASCII or single-width, so the wrap points did not move; the
  wide-character fixture that *would* change bytes is filed as F16 rather than smuggled in here.

## Measurement protocol for the performance items (F9, F10)

A claim like "cached prepares speed up the write path" is worthless without a number taken the same way on both sides.
The first R4 attempt tried to measure before implementing and ran out of time having written no code, so the work is
split deliberately:

1. add an `#[ignore]`d release benchmark over a synthetic corpus of at least 2,000 files, in its own commit with no
   behaviour change, and record its numbers at that commit;
2. implement F9/F10, run the same benchmark on the same machine, and report both numbers side by side.

If the delta is inside run-to-run noise, say so. A flat result is a usable answer; an unmeasured improvement claim is not.

### Recorded results

Both sets from `cargo test --release -p np-core --test perf -- --ignored --nocapture` on the same machine, three
consecutive runs each, milliseconds per pass (cold = first pass over the corpus, warm = second pass over the identical
files).

| Build | Cold (ms) | Warm (ms) |
| --- | --- | --- |
| baseline `d2b55dc` — benchmark only, no F9/F10 | 395 / 410 / 401 | 57 / 58 / 58 |
| after F9 + F10 at `14b2935` | 370 / 380 / 371 | 41 / 42 / 41 |

What these numbers can and cannot carry:

- Of the three statements F9 caches, only `upsert_file` ever runs against this corpus — nothing moves and nothing is
  removed — so the two rows above cannot both be pure signal for the two changes. F10's saved lookup sits on the
  re-parse path, which a cold pass reaches as `Added` (where the old code also did exactly one lookup) and a warm pass
  never reaches at all; nothing here measures F10.
- Therefore no percentage is recorded here, and **no figure from this table may be quoted as an improvement until an
  A/B/A repeat** — baseline, change, baseline again, on this machine — confirms the gap survives a return to the first
  commit. The run-to-run spread inside a single row (395 against 410) is visible in the same table as the gap between
  rows, which is what that repeat is for.

## F15 — changing a parse rule never reaches notes that did not change

- **Where:** `crates/np-core/src/scan.rs:499-501` — a note is re-parsed only when classification says
  `Added | Modified`, or when its note row is missing.
- **Found by:** the R3 review (`R3 REVIEW: pass`, P1) while landing F7 and F8.
- **Consequence for a user who already ran M1:** headings, tags, `word_count` and the title keep their pre-R3 values
  forever. `INDEX.md` goes on showing a section that is no longer hidden and a tag spelled the old way — stable, so
  nothing looks broken — and then those lines change bytes when an unrelated edit touches the note. That is worse
  than an obvious failure, because the catalog is supposed to be trustworthy.
- **Fix:** store a `parse_version` constant in `meta`; at scan start compare it with the value this build writes.
  When the stored value is behind, force the rewrite path for every markdown note in that one scan and record the new
  version afterwards. A SQL migration cannot do this — migrations here are schema-only (`db.rs:53-56`) — and hashing
  must still run over raw bytes so forcing a re-parse does not itself count as a modification.
- **Done when:** a database written with `parse_version` behind (build it by hand, like the pre-migration fixture in
  `tests/db.rs`) shows every note's derived rows refreshed by one scan with no content change, `INDEX.md` reflects
  the current rules, and a second scan reports zero changes; plus a test that bumping the constant again re-triggers.
- **Priority:** must land before the first M2 release. Every future parse-rule change depends on this mechanism, so it
  is cheaper to build once now than to retrofit after clustering stores derived data too.

**Closed on 2026-11-06 by audit — `parse_version` exists, is compared at scan start, and forces exactly one rewrite.** The constant is
`crates/np-core/src/parse.rs:42`, the `meta` key `scan.rs:75`, the comparison `parse_version()` / `refresh_needed()` at `scan.rs:2016` /
`:2026`, and the write at `scan.rs:565`; migrations stayed schema-only, and a refresh is not a modification — the report stays
`(added, modified, moved, removed) == (0, 0, 0, 0)` in every case below. Each “Done when” clause is a test that ran green in this audit's scan
suite: `a_stale_parse_version_reparses_every_unchanged_note` (`tests/scan.rs:1691`: hand-built lagging database, derived rows equal to a
reference workspace, catalog equal, and `[[Real Heading]](thematic-break.md)` present in `INDEX.md`),
`the_refresh_is_one_shot_and_the_next_scan_writes_no_bytes` (`:2209`: the second scan reports zero changes, the catalog is byte-identical, no
child rowid moves), `a_second_rule_change_refreshes_once_and_the_scan_after_it_is_quiet` (`:2253`: the card's “bumping the constant again
re-triggers”), and `a_database_that_never_recorded_a_parse_version_is_refreshed_too` (`:2181`). The stderr announcement is pinned CLI-side at
`crates/np-cli/tests/cli.rs:506`.

## F14 — two configuration tests depend on ambient `$HOME`

- **Where:** `crates/np-core/tests/config.rs:89` and `:101`.
- **Evidence (found while doing F2, reproduced on `780ccf0`):** with the environment stripped —
  `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config` — both fail with
  `MissingField("data_dir")`, because `default_data_dir()` correctly returns `None` and the loader now refuses an
  empty data dir (the behaviour M1 deliberately chose). The product is right; the tests are not hermetic, and a
  machine without `$HOME` (or a stripped CI shell) turns a correct build red.
- **Fix:** give both tests an explicit temporary `HOME` / `XDG_CONFIG_HOME` (the way the CLI integration tests
  already isolate config) instead of inheriting the ambient environment. **Done when:**
  `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config` passes, and the normal run still passes.

**Closed on 2026-11-06 by audit, in `cc8ee4a`.** The card's own “done when” is now a standing gate row —
`env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config` — and it passes; the tests take an explicit temporary environment
(`TestEnv::install`) instead of inheriting `$HOME`.
## F13 — `rel` / `abs` stored as lossy strings

- **Where:** `crates/np-core/src/db.rs:256` (`to_string_lossy`), same family as the workspace-key digest fixed in M1
  (`config.rs:170`).
- **Risk:** two non-UTF-8 paths can collide in spelling; on Windows the lossy form is the only form.
- **Fix:** store raw bytes for `rel` (BLOB or TEXT with a byte-preserving encoding) on Unix and keep the display
  string derived; add a migration and a round-trip test using a latin-1 path name. **Done when:** two paths that
  differ only in invalid UTF-8 bytes get distinct rows and both resolve.

## F16 — no wide-character fixture in the golden corpus — **shipped**

- **Shipped (`m9/wide-goldens`, `docs/design-f16-wide-goldens.md`), and not the way this entry proposed.** The goldens do not
  come from `tests/fixtures/corpus`: `golden_render_matches_committed_bytes` renders `catalog()`'s hand-written list, so the
  regeneration this entry asked for would have renamed shard files (a CJK title sorts into `#`) and re-laid-out unrelated
  entries in one diff — spending the artifact behind "F12 moved zero committed bytes". Instead: a sibling directory
  `tests/golden-wide/`, one unsharded `INDEX.md`, five entries, and **no byte of `tests/golden/` touched**. The independent
  verifier gained an unsharded mode (it previously required every `INDEX.md` body line to be a shard link, so the new directory
  would have been read by no rule at all while printing `pass`) and a column-aware R4 with the product's own single-token
  exemption.
- **What made it coverage rather than decoration:** the first fixture did not detect the regression it was added for — measuring
  in code points left its bytes untouched, and only unit cases failed. Entry 5 became five `笔记 目录` pairs: 86 characters,
  106 columns, so the two measurements disagree about whether the line fits. With that, the mutation fails the wide golden test
  and leaves the M1 one green. Finding an entry like that is the item; adding CJK text is not.
- **Found on the way:** an over-wide token renders a physical line holding only the bullet — filed as F25 with its bytes.

- **Original report follows.**
- **Where:** repo-root `tests/golden/`
- **Where:** repo-root `tests/golden/` (the golden directory is resolved inside `golden_render_matches_committed_bytes`,
  `crates/np-core/tests/catalog.rs`) contains no **two-column**
  title — em dash and umlaut are there and are single-width — so a width regression can only be caught by the unit
  tests in `tests/catalog.rs`, never by the byte-for-byte golden check. The shared corpus that would need the new note
  lives in `tests/fixtures/corpus`.
- **Why it is not done in F12:** adding a CJK or emoji title changes more than one file — it moves wrap points, can land
  in a different bucket and therefore change shard names, and both `cli` and `scan` goldens embed the generated file
  list. That is a deliberate regeneration across several fixtures and must be its own commit with its own review.
- **Fix:** add one wide-titled note to the shared corpus, regenerate every affected golden in a single commit, and
  confirm the independent verifier still reports `GOLDEN CHECK: pass` against the new bytes. **Done when:** the golden
  corpus contains at least one two-column title and CI is green on both runners.

## F17 — `continuation_indent` is accepted unclamped — **shipped**

- **Where:** `crates/np-core/src/config.rs:52` took any `usize`, and `wrap` charged it as the prefix of every
  continuation line.
- **Risk:** pre-existing, not introduced by F12. An indent near or above `COLUMN_LIMIT` defeats the column budget on
  its own — every continuation line is over the limit before its first token — so a typo in `config.toml` silently
  produced one-token-per-line output with no warning. **Worse than the original report:** the value also reaches
  `" ".repeat(indent)` (`catalog.rs:230`) and an addition into the width budget (`catalog.rs:219`), so a merely
  enormous value attempts gigabytes of spaces per entry and one near `usize::MAX` overflows the addition and panics a
  debug build. Two failure modes, neither of them "odd output" — which is why the check sits at load.
- **Shipped (`m8/indent-clamp`, `docs/design-f17-indent-clamp.md`):** `MAX_CONTINUATION_INDENT = 16`, enforced at the end
  of `Config::load` so every source a value can arrive from is covered, including a future env override. **Rejected rather
  than clamped**, because substituting 16 for the reader's 4000 would render bytes that disagree with the config that
  produced them — the catalog is published output compared by fingerprint. The refusal travels as
  `Io(ErrorKind::InvalidData)` to keep the plan's mandated three-variant `ConfigError` intact; a new variant would amend a
  mandated shape for a message, and `Parse` would accuse valid TOML of being broken. Bound is inclusive, so 0 (flush
  continuation lines) stays legal; it is a product judgement and documented as one, since any finite ceiling removes the
  hazard and nothing derives 16 from `COLUMN_LIMIT`.
- **Pinned:** refusal names key, value and `max 16`, on both channels — `an_oversized_continuation_indent_is_refused_naming_key_value_and_ceiling`
  in `crates/np-core/tests/config.rs` and `an_oversized_continuation_indent_fails_the_scan_and_names_the_key` in
  `crates/np-cli/tests/cli.rs`, the last writing its fixture where the binary actually looks
  (`<config>/note-processor/config.toml`). Boundary pair beside each: 16 renders, 0 renders, absent key still defaults to
  2. Mutations confirm both directions — deleting the check fails the refusal tests, and `>` → `>=` fails the ceiling tests.

## F18 — Windows has no byte identity at all, and its suite cannot show it

- **Where:** `crates/np-core/src/path_keys.rs`'s `os_bytes`, the `#[cfg(not(unix))]` arm spelling bytes as
  `to_string_lossy().into_owned().into_bytes()`, plus every `#[cfg(unix)]` test F13 added (`tests/scan.rs`'
  Latin-1 round trip, twins and resolution-over-twins; `tests/db.rs`'s two migrated-lossy cases).
- **Risk:** on Windows `rel_bytes == rel.as_bytes()` by construction, so F13's promise is not weaker there — it
  is absent. Two names differing only where Rust cannot represent the unit collapse to one key **through**
  round 2's own lookup: `SELECT id FROM files WHERE rel_bytes = ?1` finds the twin's row and `UPDATE`s over it,
  no constraint fires, nothing reaches `skipped`. Round 2 also moved that platform's identity fold from Unicode
  `case_key` to ASCII `case_bytes`, so a non-ASCII case rename is no longer followed either. A green
  `windows-latest` job proves none of this either way: nothing in that suite can build a colliding name.
- **Shipped (`01d35a2`, `c3303ec`, `docs/design-f18-windows-refusal.md`): within one workspace, the silent overwrite
  is closed on Windows.** Every component of an indexed path must survive the byte spelling, so a refused **file**
  names itself in `skipped` and keeps its row (`Walked::unrepresentable`, matched by exact path) while a refused
  **directory** is never entered and makes its whole subtree unknown by prefix (`Walked::unreadable`, F20's prefix
  rule). Nothing at any relative depth is indexed under a lossy key, so no lookup can resolve one note onto
  another's row. The predicate is pure over `u16`, so Linux CI executes the surrogate rule and both arms' behaviour
  through an injected-predicate seam; the Windows arm itself is only type-checked (`cargo check --target
  x86_64-pc-windows-msvc`) and its integration tests say in writing that CI compiles them out.
- **What that claim does *not* cover.** No byte identity: two such names are still one file to this program, now
  with neither indexed instead of one silently rewriting the other. The **workspace root's own name** is never
  refused — it is relative to nothing — so two roots differing only in unpaired surrogates still hash to one
  database (`config::path_bytes`' non-Unix arm, noted there). And a refused path holds `PARSE_VERSION` back for as
  long as it stays unrepresentable, which on such a workspace means permanently: every scan re-parses and prints
  the refresh-gap line rather than labelling rows current that this build did not write.
- **Still open:** (1) real byte identity on Windows, which means encoding the wide units as WTF-8 and changing
  `path_bytes`, `normalize_rel`, `rebase_abs`, every stored key and a migration **in one step** — never `rel_bytes`
  alone, or a path is written under one rule and read under another; (2) round 2's move from Unicode `case_key` to
  ASCII `case_bytes` for the identity fold, which stopped non-ASCII case renames being followed on Windows at all
  (§5 pins the regression; following it would need a Unicode-folded byte key, which is exactly what §3 refuses for
  Unix); (3) **nothing here has been executed on the platform it is about** — `tests/walk.rs`'s two
  `#[cfg(windows)]` tests (the file twin and the never-entered directory) run in CI's `windows-latest` job, but no
  CI filesystem can create such a name, so each prints `NOT COVERAGE` and returns unless a human runs
  `cargo test -p np-core --test walk -- --nocapture` on a Windows box. **Do when:** two Windows names differing
  only in unpaired surrogates key apart *and* are both indexed — not merely reported — and a `Ämli.md` → `ämli.md`
  rename on Windows still carries the note.

**Audited 2026-11-06 — still open, and every item on its “Still open” list still holds.** Checked rather than assumed:
`crates/np-core/src/path_keys.rs:143-145` is still `to_string_lossy().into_owned().into_bytes()`, so Windows has no byte identity; no WTF-8
exists anywhere under `crates/`; the identity fold is still ASCII `case_bytes`. The shipped half re-checks out — a refused file names itself in
`Walked::unrepresentable`, a refused directory goes to `unreadable` with the prefix rule (`walk.rs:214-279`), and CI's native Windows job ran
and passed at this commit (run `42171010611`: `Test (windows-latest)` success beside both ubuntu rows). **What could not be re-measured here,
and the card should say so:** `cargo check -p np-core --target x86_64-pc-windows-msvc --all-targets` exits 101 on this host today, because
`ring` and `libsqlite3-sys` run build scripts needing MSVC's `lib.exe` and no Windows C toolchain is installed (`x86_64-pc-windows-gnu` is not
installed either). CI's compile is therefore the only Windows-compile evidence this machine can reach — which sharpens the card's own point: a
Linux host cannot even type-check the arm it describes. Related, and consistent with design-a4 §6.5: A4's `file_identity` returns `None` off
Unix (`walk.rs:501-506`), so file identity is *inert* on Windows by design, not supported.


**First interactive run on real Windows hardware — 2026-11-07.** Supplied by the operator as an artifact bundle (`np-win-run.zip`:
`REPORT.md`, four `.ps1` scripts, five logs, and a read-only `db-inspect.log`); the tree tested was an unpacked GitHub ZIP, so it had no
`.git` and **its commit cannot be named** — future stages should run from `git clone`. Host: Windows 11 `10.0.26100`, Windows PowerShell
`5.1.26100.9711`, MSVC Build Tools 17.14.44, rustup 1.29.3. Binary from
`cargo build --release --locked -p np-cli` in 54.75 s; `np 0.2.0`; four subcommands.

What passed there for the first time anywhere:

* **The default build compiles and runs on MSVC.** Until this run, the only Windows-compile evidence was CI's hosted job — and
  `cargo check --target x86_64-pc-windows-msvc` exits 101 on this Linux box (`ring` and `libsqlite3-sys` need MSVC `lib.exe`), so the card's
  own claim that CI is unreachable evidence was correct and is now superseded.
* **All eleven migrations applied natively**, FTS5 shadow tables present, WAL sidecars cleaned, `notes.encoding = "utf-8"`,
  `title_source = heading`, headings captured, and every integrity counter zero: `fk_orphans = headings=0,tags=0,items=0,links_src=0,links_dst_null=0`,
  `paths_lossy = files=0,notes=0`, `unindexable = unreadable=0`.
* **The F36 headline property holds with nothing installed**: `search <ws> "broker migration"` answered `kafka.md` at exit 0 with the one-line
  caveat on stderr; `--mode keyword` answered with no caveat; `--mode semantic` refused with F32's sentence at exit 1; `--mode fuzzy` gave clap's
  usage error listing the three names at exit 2.
* **A path with spaces and non-ASCII** (`C:\Users\chris\Meine Notizen äöü`) indexed `added 1`, then `unchanged 1` — so Windows path keying survived
  that shape. It is *not* the surrogate collision F18 wants, so the card stays open.

Two findings came out of it, and neither is Windows-specific behaviour:

* **A sentence with a hole in it.** With no `[embeddings]` section and no vectors, `np search <ws> "anything"` printed
  `nothing indexed for  by hybrid` — `{model}` interpolated the empty string (double space), about an index the keyword half had just consulted.
  Reproduced on Linux in minutes; fixed with three arms (`keyword` names the query; no-vectors hybrid says the workspace holds no vectors; a real
  id keeps the old sentence), pinned by `an_empty_result_names_the_query_and_never_an_absent_model` and, for the third arm,
  `real_model::an_unembedded_workspace_names_its_model_in_the_empty_result`. The Windows box found in one evening what six feature-matrix CI rows had
  never surfaced, because no CI row ever ran the binary against an empty vault.
* **The `%LOCALAPPDATA%` surprise is a real product question** — see **F37**.

Still open on this card, unchanged: byte identity for names differing only in unpaired surrogates (WTF-8 and a migration), the ASCII-only identity
fold that stopped following `Ämli.md` → `ämli.md` renames on Windows, and a human-run `cargo test -p np-core --test walk` that does *not* print
`NOT COVERAGE`. Not yet run on that machine: Stage 4's config-strictness check, the full suite (Stage 5), inference with an artifact (Stage 6),
and the rename experiments (Stage 7).

## F19 — a legacy duplicate note pair re-parses the whole corpus forever

- **Where:** `crates/np-core/src/scan.rs`'s `Indexed::Duplicate` arm charging `written.uncovered` on every
  scan, and `refresh_covered`, which therefore never returns true for such a workspace.
- **Risk (as filed, against a build that had no exit):** deliberate at the level of F15 (recording the parse
  version beside a note that was not re-parsed is exactly F15's bug), but nothing healed it: no code merged two
  `notes` rows for one path, so every scan re-parsed every note and printed the refresh-gap line. On a large
  corpus that was a permanent several-hundred-ms cost plus a warning nobody can act on, and the registry row
  kept moving ahead of derived rows that never changed (mixed-age database, `Indexed::Duplicate`'s doc).
- **Fix:** a migration that merges duplicate note rows for one `rel_bytes` — keeping the older `notes.id` so
  derived rows stay attached, re-pointing edges that named the loser, and reporting what it merged rather than
  choosing silently — after which the version records normally. **A `np repair` command was considered and ruled
  out** (design §7): [this program's only command is `np scan` — historical as of round 4a: `np models install|hash|list` now exist, and none of them is a second path through migration or merge logic, which is what this ruling forbids], whose open already runs every schema fact, and a
  user-facing second path through the same logic is a second thing to keep in agreement with the first. So do not
  build one on the strength of this entry — the entry is closed, and migration 5 at open is the merge path.
  **Do when:** a workspace holding a legacy duplicate converges to `uncovered = 0` on the scan after the repair,
  and the repair names every pair it merged. **Met** by `a_merged_duplicate_pair_clears_the_charge_and_records_
  the_parse_version` and `the_scan_that_merges_reports_the_string_it_stored`.
- **Shipped (`9821dbb`, `4f0f5b3`, `e4495c3`, `7212e56`, `docs/design-f19-note-merge.md`): migration 5 folds the
  pair at open, and announces it in the transaction that deletes.** Migration 5 (`merge_duplicate_notes`) groups
  by `rel_bytes` — never by display text, so it cannot reach across F23's display twins — keeps `MIN(id)` because
  that is the row derived rows already hang off, re-points `links.dst_note_id` onto the winner instead of nulling
  it (other notes' prose is not re-parsed, so a NULL target is a resolved link lost permanently), and deletes the
  loser's `headings`, `tags`, `items` and outgoing edges **by hand**: enforcement is off inside a code step, so no
  `ON DELETE CASCADE` fires behind it, and `PRAGMA foreign_key_check` in the same transaction is what turns a
  softened delete into a refused open. The receipt lands in `meta` under `note_merges` — `merged=N;paths=…`, names
  capped at eight and ordered by byte key — written even at zero, because `merged=0` is the record that the check
  ran; only the open that merged echoes it, on stderr and as `ScanReport::merged_notes`, so stdout, `--json` and
  the durable record are one string (F21 §3's rule again). Convergence is asserted rather than hoped for, and
  `Indexed::Duplicate` survives because a pair another tool writes *after* migration 5 has run is still permanent:
  there is no second pass and no `UNIQUE` index on `notes(rel_bytes)` (design §7 keeps that choice deliberately
  open so F13 §7's reader stays testable), so the reader still names such a pair instead of picking a winner.
  **One pin this entry cannot have yet:** migration 5's `WHERE rel_bytes IS NOT NULL` arm has no fixture, and a run
  that went looking for one established why — `notes.rel_bytes` carries `CHECK (typeof(rel_bytes) = 'blob')`, and
  `typeof(NULL)` is `'null'`, so no writer can leave a note row keyless in any file migration 4 touched. That CHECK
  is the protection; the clause is defence in depth, and **F-5's keyless-row pin becomes possible only when some
  future migration admits keyless note rows.** What is pinned instead is the reachable half of the same shape: a
  merged survivor with no `rel` at all (`a_merged_pair_with_no_display_text_is_named_by_the_lossy_spelling_of_its_key`).

**Closed on 2026-11-06 by audit — the entry said “the entry is closed”; now it carries the evidence.** Migration 5 is registered at
`crates/np-core/src/db.rs:161` (`Migration::Code(merge_duplicate_notes)`) and defined at `:1350`; the receipt lands in `meta` under
`note_merges` (`:1305`) and reaches a person through `Db::merged_notes` (`:580`) → `ScanReport::merged_notes`; `PRAGMA foreign_key_check`
runs inside that same transaction (`:781`, `:1020`). Both “Met by” tests passed in this audit's runs:
`a_merged_duplicate_pair_clears_the_charge_and_records_the_parse_version` (`tests/scan.rs:4975`) and
`the_scan_that_merges_reports_the_string_it_stored` (`tests/db.rs:3997`). The card's note about the unfixtureable `WHERE rel_bytes IS NOT NULL`
arm is still true: `notes.rel_bytes` carries `CHECK (typeof(rel_bytes) = 'blob')` at `db.rs:348`, and `typeof(NULL)` is `'null'`.

## F20 — two report-side filters still compare display text

- **Risk:** pre-existing shape, newly reachable. `scan.rs`'s `under_unreadable(&snapshot.rel, unreadable)` matches
  a lossy display spelling against a list of lossy display spellings, so with twin *directory* names one
  unreadable directory hides both twins from `known`: the readable twin can surface as a spurious `Added`, and
  `hidden_by_unreadable` inflates enough to hold back the parse version. Separately, `skipped.sort();
  skipped.dedup()` publishes one line for two byte-distinct refused files that share a display spelling — a
  reduced form of §1's "the report cannot name the file it lost".
- **Wider since F18, and worse in two specific ways** (whole-branch pass `f7639d0c`). A refused directory is pushed
  into the same `unreadable` list (`walk.rs:227`), so anything that lands in it — refused or genuinely unreadable —
  now hides a subtree from `known`. While such a sibling sits beside a **readable** directory of the same lossy
  spelling: (1) a real deletion inside the readable one is never reported `Removed`, and a rename there produces a
  **second live row instead of a move**, because the moved-away path stays unknown rather than becoming `Removed`;
  and (2) one note can be charged twice, in `written.uncovered` (`scan.rs:567`) *and* `hidden_by_unknown`
  (`scan.rs:486`), so the refresh-gap line over-counts. Bounded **for rows the walk keyed from real bytes**: for those
  it self-heals the moment the refused or unreadable sibling is renamed away, and no phantom row survives. It does
  **not** heal for a row whose stored key is display text — see F23, which is the same obstruction with no scan-driven
  exit at all. This is the imprecision F18's own doc points at but does not own: F18 widened who enters these lists, it
  did not create the matching.
- **Fix:** carry `rel_bytes` alongside the display spelling in both places (the unreadable set is built by the
  walk, which already has bytes) and key the dedup on bytes while printing display text. Neither changes catalog
  output, so goldens stay put; a fixture with twin directory names should pin both halves.
  **Do when:** a workspace with `caf\xe9/` and `caf\xe8/`, one unreadable, reports exactly the rows it should,
  names both refused paths on separate lines, and counts the note behind them once — not in both
  `written.uncovered` and `hidden_by_unknown`.
- **Shipped (`bc8f05c`, `aa74a04`, `7acd75f`, `86ee207`, `docs/design-f20-bytes-in-reports.md`).** An unknown path now
  travels as `walk::UnknownPath { display, bytes }` from the walk that has the OS bytes to the filter that needs
  them: `under_unreadable` matches a byte prefix plus a real `/`, `is_unknown_path`'s refused arm compares exact
  bytes, and `skipped` sorts and dedups by bytes while printing display text — so two byte-distinct paths publish two
  look-alike lines. The fixture exists and runs here (`caf\xe9/` blocked beside readable `caf\xe8/`, real `chmod
  000`, no skip taken): the readable twin's deletion is reported, a rename there is one `Moved` with no `Added` twin,
  and the hidden note is charged exactly once — the double charge F20 feared turns out not to survive byte matching,
  and the arithmetic that used to sum three now sums two.
- **One exception this round had to add, and it is not legacy-only.** Migration 4 keyed every row through
  `rel_bytes_of_display`, so a migrated row's stored key *is* its lossy text: byte-only matching hid nothing from it
  and reported a live note `removed: 1` while its directory was unreadable (review `d2cbf5de`). Such rows are now
  also compared text-against-text, which hides the row instead of falsely deleting it. A name genuinely holding
  `U+FFFD`, and every lossy Windows name, qualify permanently; a migrated row leaves the class only when a scan pairs
  it by hash so `apply_move` rewrites `rel_bytes`, never merely because a scan ran. The refused-name side of that
  fallback was deleted as unable to ever fire (Windows refusal entries are already lossy-keyed; Unix refuses nothing),
  with the mutation confirming the guarantee never lived there.
- **Over-hiding is NOT always harmless churn, and this entry said it was.** When a display-keyed row's own path is
  readable while an unreadable twin shares its display spelling, hiding the row leaves the listed path with nothing to
  pair against: it indexes as a spurious `Added`, so **one file gets two live `files` rows and two `notes` rows**, and
  `hidden_by_unknown` holds `PARSE_VERSION` back until someone renames or removes the twin — no scan clears it. The
  alternative is worse (a false `Removed` stales a live note and later returns it as a second id), so the wider
  matching stays; **F23** owns the ambiguity, which no predicate at `known_rows` can resolve.
- **What this does not claim:** on Windows, where identity bytes come from `to_string_lossy`, two refused names still
  share one `skipped` line — so that list is a floor on what a scan lost, never a count. That is stated by a
  `#[cfg(windows)]` test in `tests/walk.rs` which **no CI run can execute**: it prints `NOT COVERAGE` and returns,
  because no filesystem there will hold an unpaired surrogate either.

**Closed on 2026-11-06 by audit — bytes travel all the way to the matcher, and the fixture ran here instead of early-returning.**
`walk::UnknownPath` carries `display` + `bytes` (`crates/np-core/src/walk.rs:99-113`) with a doc that forbids handing a text-only entry to a
byte-keyed matcher; `under_unreadable` compares a byte prefix plus a real `/` (`scan.rs:1117-1124`); `skipped` is sorted and deduplicated by
bytes and printed as display text (`scan.rs:167-173`, built at `:722`, pinned by `tests/scan.rs:3116`). The runner caveat F23's entry demands
was satisfied when re-measured: `cargo test -p np-core --test scan -- --nocapture` → **exit 0, 74 passed / 0 failed, zero `NOT COVERAGE`
lines**, host `pi`, euid 1000 — so `an_unreadable_directory_does_not_hide_its_readable_twin` (`tests/scan.rs:2876`) and
`two_unreadable_twins_publish_two_report_lines_that_look_alike` (`:3063`) executed, as did the single-charge arithmetic case
(`src/scan.rs:3327`) and both display-keyed fences (`tests/scan.rs:3161`, `:3306`). `GOLDEN CHECK: pass`, so no catalog byte moved. **What
stays open belongs to F23, not here:** the display-keyed over-hiding that no predicate at `known_rows` can resolve, and Windows' lossy-keyed
floor on one `skipped` line.

## F21 — a refusing stderr could block the first upgrade of an orphan-holding workspace

- **Was:** `db.rs`'s `report_orphans` did `.expect` on its write to descriptor 2, from inside migration 3's
  transaction. A pre-F5 workspace holding orphans and opened with stderr refusing writes (`|& head`, a detached
  service, `/dev/full`) panicked mid-migration and never upgraded — the panic guarding a duplicate of the receipt
  written two statements earlier, in the same transaction.
- **Shipped (`0b6a071`, `a775b90`, `docs/design-f21-durable-upgrade-announcement.md`): durable first, echoed second.**
  The `meta` receipt is the announcement of record and the deletion cannot commit without it — pinned by forcing that
  write to fail (`a_receipt_that_cannot_be_written_rolls_the_cleanup_back`), not by trusting statement order. The
  stderr echo dropped its `expect` and moved **after `tx.commit()`**, so a failed COMMIT can no longer announce rows
  that then rolled back; `Db::open` carries the stored string to `ScanReport::upgrade_orphans`, and stdout, `--json`
  and the echo all render that one value. The field is `Some` only when the cleanup removed more than zero rows, so a
  fresh workspace announces nothing while the all-zero receipt still lands in `meta` as proof the check ran.
- **What it does not claim.** No test pins the post-commit position of the echo: a second connection's write lock, a
  read-only file, a `RAISE(ABORT)` trigger and a page limit each refuse an *earlier* statement rather than the commit,
  so no portable COMMIT failure exists to build one on — that guarantee is call-site position plus the durable
  receipt. And the announcement is once-per-process by design: if stdout refused too, the fact survives only in
  `meta`, which is what "no stream is load-bearing" means here. **Do when:** a person can learn what an old upgrade
  deleted without opening `sqlite3` — that is F22's durable-announcement mechanism applied to receipts, not a new
  echo.

**Audited 2026-11-06 — the panic is gone and verified; the card's own Do-when is not met, so it stays open.** In the tree and under test: the
receipt is written before anything deletes, pinned by `a_receipt_that_cannot_be_written_rolls_the_cleanup_back` (`tests/db.rs:1911`, passed);
the echo dropped its `.expect` and moved after the commit (`src/db.rs:676` → `:688`); the stored string reaches
`ScanReport::upgrade_orphans` (`scan.rs:209`). Two CLI cases ran green here: `an_error_is_reported_rather_than_panicked_when_stderr_refuses_it`
(`crates/np-cli/tests/cli.rs:701`) and `a_scan_upgrades_when_stderr_refuses_writes`. The remaining ask is unmet as written:
`upgrade_orphans` is `Some` only on the open that ran the cleanup (`scan.rs:189-193`), so somebody asking today what an upgrade deleted last
month still needs `sqlite3`. F22's shape — a bounded `meta` record whose one string is printed on stdout and in `--json` — now exists for
refusals; applying that shape to upgrade receipts is the rest of this card, and it needs no new exit code and no second echo.

**Second half landed 2026-11-07 — the record is readable without `sqlite3`, and no run got louder for it.** `Db::recorded_upgrade_orphans` and
`Db::recorded_merged_notes` read `meta` on any open, and `ScanReport` carries both as `recorded_upgrade_orphans` / `recorded_merged_notes`. The announcement
stays exactly where F21 put it — once per process, on the run that did the deleting — because a permanent one-line warning is how real signals die; what
changed is that the *record* now reaches anyone who asks later, through `np scan --json` rather than a database shell. Nothing echoes twice, nothing clears,
and stdout gained no line: `only_the_scan_that_removed_the_orphans_announces_them` still asserts the printed report says nothing about orphans, alongside the
new assertion that the same workspace's JSON carries the receipt byte for byte on every scan after it.

Three np-core cases and one CLI extension: a migration-5 merge reported by its own run and then carried by the next (`merged=1;paths=dup.md`, while
`merged_notes` is `null`); a fresh file answering with the zero receipts that prove the checks ran, rather than nulls or empty strings; and a hand-poked SQL
`NULL` in `meta.value` reading as no record instead of failing every later scan — the same defect class `a_null_meta_value_is_read_as_no_label_rather_than_failing_the_scan`
retired for the other reader, which my first draft re-imported through this new door and that case caught. One mutant died twice: reading the carried handle
instead of `meta` fails the CLI case immediately and, once the np-core case gained its orphan-receipt assertion, fails there too — which is worth recording
because that assertion was missing at first, and a durability test that only checks the field this open also wrote proves nothing.

**Still not closed, on the same ground as F28: CI cannot grade it.** Actions has been out of included minutes since `2026-11-07T01:57Z`
(`docs/ops-actions-minutes.md`). What is graded locally: 17 gate rows exit 0, workspace tests green, `GOLDEN CHECK: pass`, plus the CLI case through argv.
The closure paragraph goes in when a run that has steps grades `main`. **No new exit code and no second echo**, as this card's own "Do when" required.

## F22 — a refusal exists only as one report line, with no reason attached

- **Where:** `scan.rs` puts refused names in `report.skipped` and nowhere durable — the `meta` receipt carries the
  parse version and orphan counts, not refusals — while `walk::UnknownPath`'s field docs record no cause (the text
  that used to sit at `walk.rs:97-99` moved there in F20), so a directory that was **refused** and a directory nobody
  may enter print identically.
- **Risk:** on Windows a refusal means a note is missing from the catalog, permanently, until someone renames the
  file. The only evidence is one line on stdout or stderr at the moment it happened: redirect the summary away, or
  ask a day later, and `np status` has nothing to say — it cannot report that any path is unindexable. F18 §3.3
  allowed a sibling list carrying reasons; the implementation chose one list because the caller's duty is identical
  either way, which is true for *safety* and false for *diagnosis*.
- **Also unreported, and this is the entry that should carry it:** F23's duplicate state — one physical file with two
  live `files` rows, hence **two catalog entries** — has no signal at all. What a scan does print today is honest but
  indirect: a `skipped` line naming the obstructed directory, and the refresh-gap warning on stderr every run because
  `refresh_needed` never goes false; the at-risk population is already durable in `meta` under `PATHS_LOSSY_KEY`. None
  of that says "this note appears twice". Fold it into the same bounded `meta` record rather than inventing a third
  mechanism.
- **Fix:** reuse what F21 built rather than inventing a second mechanism — a bounded record in `meta`, the value
  carried on the open handle, and an always-present `ScanReport` field surfaced by `np scan`. (`np status` does not
  exist; `np scan` is this program's only command, as F21 §3 records — historical as of round 4a, which added `np models install|hash|list`; see design-m2-models-install §6.) Keep the single list but word the entry with its
  cause where the walk knows it — `skipped: unreadable directory` against `skipped: name this build cannot key`.
  Report text plus a `meta` key only, so catalog goldens stay put. **Do when:** a workspace scanned with stderr
  refusing writes still lets a later `np scan` name the refused path, and the two causes are distinguishable in the
  output.

**Closed on 2026-11-06 by audit — one bounded record, counts carrying causes, the same string on both channels.** The `meta` key `unindexable`
(`scan.rs:101`) holds `unreadable=N;unkeyable=N;duplicates=N;ambiguous=N;names=…`, written **after** the commit and best-effort
(`record_unindexable`, `scan.rs:1986-1993`: a refused `meta` write costs the record, never the scan — F21's lesson), rendered verbatim by
`np scan` (`crates/np-cli/src/main.rs:314-323`) and as an object in `--json`. The two causes are distinguishable, which was half the card, and
F23's duplicate state is folded into this record instead of growing a third mechanism (`duplicated_rels`, `scan.rs:1969`). Cases that ran green
here: `an_unreadable_directory_is_recorded_as_a_loss_of_that_exact_kind` (`tests/scan.rs:4484`),
`a_scan_that_loses_nothing_overwrites_the_record_an_earlier_scan_left` (`:4539`),
`the_duplicate_count_names_a_display_path_with_two_live_rows_and_not_one_with_a_stale_row` (`:4635`),
`a_refused_record_write_leaves_the_scan_successful` (`:4723`), `names_order_a_duplicated_path_before_an_unreadable_directory` (`:4792`),
`a_healed_pair_leaves_no_duplicate_in_the_record_but_still_names_the_loss` (`:4852`), and CLI-side
`an_unreadable_directory_prints_one_record_line` (`crates/np-cli/tests/cli.rs:1318`), which asserts stdout, `--json` and the stored value are
one string. **Two limits, stated rather than smoothed over.** (1) The card's sentence names *stderr* specifically; what is pinned is that the
record depends on no stream at all (written with `let _ = set_meta`, after the commit, whatever stdout and stderr do) plus F21's cases that a
refusing stderr neither panics nor moves the exit code — no single case combines “stderr refused” with “the next scan named the path”. (2)
Nothing drives an F23-shaped row to `duplicates=1` any more, because F23 healed the shape that used to produce it; that gap is recorded in the
test's own doc (`tests/scan.rs:4620-4634`) and left open on purpose.

## F23 — a display-text key cannot say which byte twin its row is for

- **Half shipped.** The row is no longer deleted from the scan's view: `known_rows` marks it, an ambiguous row may never
  produce `Removed`, and it may be claimed by content through the ordinary `Moved` route so `apply_move` re-keys
  `rel_bytes` from real bytes. A legacy workspace whose note has **not** been edited since migration now converges: one
  live `files` row, one `notes` row at the id it already had, and `PARSE_VERSION` recorded — pinned by
  `a_healed_display_keyed_pair_leaves_one_live_row_and_records_the_version`, which replaced the fixture that used to pin
  the defect (this entry instructed whoever fixed it to delete that fixture rather than let it harden into a guard on the
  bug; it was replaced instead, so the same test now pins the healing). A row whose key already equals its OS bytes — a
  name genuinely holding `U+FFFD`, and every lossy name on Windows — discharges by **match**, with no move at all:
  `a_row_keyed_from_its_own_lossy_bytes_discharges_by_match_without_a_move`. The durable record gained `ambiguous=N`,
  rendered from exactly the population that holds the version back, so a person reading `meta` tomorrow learns what a
  withheld version meant. **Shipped on `m6/ambiguous-rows` (`347dc2c`..`ebaa17f`; the branch was rewritten to reword two `wip:` subjects, so any
  earlier SHA quoted for it no longer resolves), `docs/design-f23-ambiguous-rows.md`).**
- **The claim is scoped to the look-alike, and that is load-bearing.** `classify` matches a move on **hash alone**, and its
  `present_hashes` guard cannot fire for a display-derived key (that set is seeded from known rows whose *key* matches an
  unreadable listing). So admission requires a listed readable path that spells the row exactly **and** matches its stored
  hash. Without that clause any readable path anywhere with byte-identical content — a `# Title` stub, a blank template, a
  re-exported note — claims the hidden row: `moved: 1`, the id homed on somebody else's file. Mutation proves it: delete
  the scoping test and `an_unrelated_look_alike_carrying_the_hidden_bytes_is_added_and_never_claims_them` reports
  `moved: 1`.
- **Still open — the residue.** A row whose **content changed while it was unindexable** has no hash agreement, nothing
  claims it, and the duplicate pair returns exactly as before. Two live rows for one file, version withheld, until someone
  renames or removes the twin. This is not a corner: the obstruction can outlast the edit.
- **Why the naive rule stays rejected.** N′ (the row's own file is the readable path) and M (the row's file is inside the
  blocked directory, the readable path being a different file that displays identically) hand `known_rows` identical
  inputs and need opposite answers. Any predicate there that stops hiding N′ also stops hiding M, which reports
  `removed: 1`, stales a live note, drops it from the catalog and records `PARSE_VERSION` over rows nobody re-parsed. The
  fence for that is `a_display_keyed_row_inside_an_unreadable_directory_survives_its_readable_twin`; its twin carries a
  **different body** deliberately, because identical content lets `classify` pair them and the fixture proves nothing.
- **Fix candidates for the residue, with their costs.** (i) **Durable file identity** — store what the OS calls the file
  (`ino`/`dev` on Unix, volume serial + file index on NTFS) at every successful index and decide N′ against M by identity
  instead of content. It resolves the residue including content-changed-while-hidden, and it is the right next step. Costs:
  a migration, platform code, files where identity is unavailable or recycled (exFAT/FAT without `FileIdInfo`, network
  mounts), and no Windows claim until one has been run on Windows (F18's standing lesson). (ii) **Found-side suppression**
  — skip a readable path whose display text matches an unresolved row and name it in `skipped`: no duplicate rows, but a
  genuinely different file goes unindexed, which costs real coverage to buy tidiness, and it changes what a scan is allowed
  to omit — its own spec, not a hardening round. Under (ii) the fence's `added: 1` legitimately becomes `added: 0` plus a
  named skip; that edit must be chosen on purpose and justified, never made silently. (iii) A schema step refusing
  display-derived keys — re-key from OS bytes or flag for full re-index — costs a migration plus one complete re-parse of
  every flagged workspace and must say what happens to rows F23 already duplicated.
- **Costs this round accepted, not hid.** In the byte-identical-twin configuration the row claims the twin: `moved: 1`
  where nothing moved, one fewer catalog entry while the obstruction stands, and — because the row has left its own key —
  a later deletion inside the obstruction is never reported. Pinned with its harm named by
  `a_byte_identical_twin_is_claimed_and_the_file_it_leaves_behind_goes_unreported`. Unavoidable: the two configurations are
  identical to `known_rows` by construction.
- **Runner caveat:** the fixtures here early-return with `NOT COVERAGE` when the filesystem refuses latin-1 directory names
  or `chmod 000` fails to deny access (a privileged runner), so a green CI run is not proof they executed. Validate any F23
  change on a runner where `denied()` is true and name it — command, head SHA, host and euid — in the PR. Baseline for this
  round: scan suite green with **zero** `NOT COVERAGE` lines on host `pi`, euid 1000.
- **Do when (rest of it):** a workspace with `caf\xe9/` blocked beside `caf\xe8/`, holding one display-keyed row whose
  content changed while blocked, ends with exactly one live `files` row and one `notes` row for that path **once the
  obstruction is gone**. While the twin is blocked some charge to the hidden count remains correct and `PARSE_VERSION`
  stays withheld — that withholding is the safety, not the bug.

**Audited 2026-11-06 — still open, but “Still open — the residue” above is narrower than it reads, and A4 is why.**
`docs/design-a4-file-identity.md` shipped as `cbcb12a` (2026-10-28), after this card was written: the OS identity (`st_dev`/`st_ino`,
`walk.rs:490-497`) is stored on every successful index (`files.file_id`, `db.rs:169`) and may *add* a match, never remove one. So fix candidate
(i) is no longer a candidate — it is shipped on Unix — and the permanent-class case has healed:
`a_permanent_class_row_whose_content_changed_while_a_twin_is_blocked_rejoins_its_own_row` (`tests/scan.rs:3652`) passes here, in a run with
**zero `NOT COVERAGE` lines** (host `pi`, euid 1000). What the residue means now, precisely, is a **migration-keyed** row whose stored key is
display text, whose real path is byte-distinct, and whose identity is unknown — pinned by
`an_unknown_identity_leaves_that_row_refused_and_discharged_exactly_as_before` (`:3788`, which sets `file_id = NULL` at `:3808` to keep the
pre-A4 state asserted), and named as such by the healed test's own doc (`:3647-3650`). Design-a4 §8 states the boundary this card should carry:
“the residue only stops being permanent where an id was recorded before the obstruction”. A4's two debts ride along and are already on the
record (roadmap §A4): no A/B/A benchmark, no whole-branch review. Windows identity remains F18's blocker.

**Measured 2026-11-07 — the residue is smaller than this card says, and one sentence here is wrong.** `docs/design-f23-residue-measured.md` builds the
Do-when's fixture faithfully (migration 4's own `rel_bytes_of_display` + `case_fold` + `display_fold`, so the legacy key is what an old build wrote rather
than a shape that merely resembles one) and measures it. **The note comes back:** once the obstruction clears, the edited content indexes at its real bytes
(`added 1, removed 1`, `skipped []`), the catalog lists it once beside its twin, a third scan has nothing pending, and `PARSE_VERSION` is recorded — so
"PARSE_VERSION stays withheld" describes the configuration where the row is *unresolvable*, not this one. What remains permanently is **one `stale` row under
the display spelling**, holding pre-edit content that exists nowhere else: F23's refused deletion, still standing, invisible in the catalog and excluded from
`duplicates` by the `state = 'ok'` filter. The Do-when ("exactly one live `files` row and one `notes` row for that path once the obstruction is gone") is
therefore **met**, and "the duplicate pair returns" needs correcting: it is one live row and one stale one, not two live rows. Pinned by
`a_migration_keyed_row_with_no_identity_rejoins_its_path_and_leaves_a_stale_row` (`tests/scan.rs`, 78 passed / 0 failed, **zero `NOT COVERAGE` lines**, host
`pi`, euid 1000).

**The measurement that came out wrong first, because the lesson is worth more than the finding.** The first fixture re-keyed `rel_bytes` and `rel` and left
`case_bytes` folded from the real bytes. The scan then refused the returning file on `files.case_bytes UNIQUE`: `(0, 0, 0, 1)`, one line in `skipped`, no row
at all for a readable note, absence from the catalog, identical on a third scan — indistinguishable from "the note is unindexable forever", and I would have
reported exactly that. No shipped build can produce that state: the four key columns are written together. So the invariant to carry into any F23 fixture is
**`rel_bytes`, `case_bytes`, `rel` and `case_key` move together, or the fixture measures its own planting.**

**Decision owed** (options and costs in §5 of the document): **(A)** accept the stale row and close the residue — recommended, since the harm turned out to be
a dead row rather than a lost note; **(B)** clear such rows once their path is indexed, which is F23's original deletion question again and unsafe in the
neighbouring configuration where the stale row is the last record that a note existed; **(C)** report the count. Under (A) this card's only remaining open item
is Windows file identity, which stays F18's blocker. Nothing was implemented for this round beyond the test and the document: the semantics question is the one
this card has always said must be chosen on purpose.
**Closed on 2026-11-07 by measuring the "do when" and finding one dead row where the card expected a lost note — option A, accepted by the human.** The residue is
now driven end to end (`a_migration_keyed_row_with_no_identity_rejoins_its_path_and_leaves_a_stale_row`, `tests/scan.rs`) with the legacy key built by migration 4's
own rule rather than an impression of it, and the answer is in `docs/design-f23-residue-measured.md`: once the obstruction clears, the note **rejoins its real path**
with the content written while it was unreadable (`added 1, removed 1`, nothing skipped), the catalog lists it once beside its twin, a third scan has nothing pending,
and `PARSE_VERSION` is recorded. The permanent remainder is one `stale` row under the display spelling holding pre-edit content found nowhere else — this card's
refused deletion, still standing and still doing the job it was kept for, invisible in the catalog and excluded from `duplicates` by the `state = 'ok'` filter. So the
"Do when" above is **met**, and two of its sentences needed correcting: "the duplicate pair returns" describes one live row and one stale one, not two live rows; and
"`PARSE_VERSION` stays withheld" belongs to the configuration where the row cannot be resolved at all, not to this one.

**What was ruled.** Of the three candidates in §5 of the document — (A) accept the stale row and close, (B) clear such rows once their path is indexed, (C) report the
count — the human took **A**. B is this card's original question in a new hat: deleting a display-derived row needs proof that no note is behind it, and in the
neighbouring configuration that stale row *is* the last record that a note ever existed, which is the harm F23 was opened to stop. C buys visibility for a state now
known not to cost anything, at the price of another pinned `ScanReport` field. No code changed for this closure: the deliverable was the measurement and the wording,
which is why nothing here waits on a capability.

**How this was verified, and why that is not a CI claim.** `cargo test -p np-core --test scan` → **78 passed / 0 failed**, with **zero `NOT COVERAGE` lines**, on host
`pi`, euid 1000 — the runner condition this card's own caveat demands be named, because these fixtures build their obstruction with `chmod 000` and latin-1 directory
names and print `NOT COVERAGE` instead of passing quietly where the filesystem or a privileged account declines. Full local gates: 17 rows exit 0, workspace **693
passed / 0 failed**, `GOLDEN CHECK: pass`. Actions minutes are exhausted (`docs/ops-actions-minutes.md`), so no run graded this tree and the last CI-graded tree stays
`5e368ac`. That is disclosed rather than papered over, and it is also the weaker argument here than it usually is: by this card's own caveat a green CI run is *not*
proof these cases executed, so naming the host and the euid carries more than a job that might have skipped them. The closure rests on measurement plus text, as F29's
did, and says so in the same breath.

**What this leaves open, elsewhere.** Windows file identity — whether `VolumeSerialNumber` + `FileId` behaves as design-a4 §5 expects — stays **F18**'s blocker, and no
claim about it is made here. Fix candidate (i) above ("durable file identity", listed here as a future step) was overtaken by A4 (`cbcb12a`) and retired in the audit
paragraph before this one; this closure retires nothing else. The invariant worth keeping from the measurement that came out wrong first (§3 of the document):
`rel_bytes`, `case_bytes`, `rel` and `case_key` move together in a fixture, or the fixture invents a `files.case_bytes UNIQUE` collision no shipped build can produce
and starts measuring its own planting.

## F24 — the two merge tests carry the same assertions twice

- **Where:** `crates/np-core/tests/db.rs` —
  `merging_a_duplicate_pair_keeps_the_oldest_id_and_moves_every_incoming_edge` and
  `a_version_three_file_jumps_four_to_five_in_one_open_and_its_pair_merges` repeat five blocks verbatim: the
  survivor is `MIN(id)`, the winner's heading/tag/item survived, the four-shape loop over what the loser left,
  the re-pointed incoming edge, and the receipt string.
- **Risk:** drift, in the direction that hides a hole rather than showing one. This round closed exactly that: the
  version-three copy of `SELECT COUNT(*) FROM links WHERE src_note_id = 9` could never fail because only the
  version-four fixture planted the loser's outgoing edge — one fixture gained a row, the other's assertion went on
  reading as coverage.
- **Fix:** extract an `assert_merged_pair_folded(&db)` helper, the way `crates/np-cli/tests/cli.rs` already has
  `assert_only_the_orphans_went`. Deliberately **not** done in this round: keeping each test's own failure messages
  ("the edge crossed the rebuild and the merge together" is not the claim the version-four case makes) costs most
  of the saving, so the honest extraction is under twenty lines and would flatten those messages to buy it.
  **Do when:** the next change to migration 5's delete list — take it then, with both fixtures in front of you.

**Closed on 2026-11-06 by audit, in `eea29cd`/`af1d504`, with a deviation from the fix this card proposed.** The card asked for an
`assert_merged_pair_folded(&db)` helper; what shipped is shared *measurement* with unshared verdicts — `MergedPairFacts` (`tests/db.rs:3632`)
and `merged_pair_facts` (`:3644`) read the survivor ids, the winner's heading/tag/item, the loser's four shapes each paired with the query that
measured them, the re-pointed edge and the receipt, while each fixture keeps its own assertions and its own failure messages. The deviation is
argued in `docs/design-f24-merge-assertions.md` and in the commit message: the card's incident was an **unasserted precondition**, and
extracting assertion text would have left that hole exactly where it was — so both seeders now end in `assert_planted_pair` (`:3674`), and
deleting one seeder's loser-edge `INSERT` fails at setup naming the query. Both fixtures pass here:
`merging_a_duplicate_pair_keeps_the_oldest_id_and_moves_every_incoming_edge` (`:3740`) and
`a_version_three_file_jumps_four_to_five_in_one_open_and_its_pair_merges` (`:3933`). The card's trigger — “take it at the next change to
migration 5's delete list” — never had to be waited for.

## F25 — an over-wide token leaves a physical line holding nothing but the bullet

- **Where:** `crates/np-core/src/catalog.rs`, `wrap`'s single-token exemption ("The one exemption", near `COLUMN_LIMIT`).
  Found while building F16's wide goldens, not introduced by it: any entry whose second token is wider than the columns left
  renders like this — the bytes below came off the renderer, with a 55-ideograph CJK title (110 columns in one token):

  ```text
  -
    [[目目目目…目]](plans/cjk-unbroken.md)
    · 3 B · #long · →0 ←0
  ```

  The bullet gets a line to itself, then the over-wide token, then the rest of the entry on a third line.
- **Why it may be wrong:** R2's rule — and every human reading `INDEX.md` — expects an entry to *start* with `- [[title]]`. A
  line containing only `-` is not an entry, and in a Markdown viewer it renders as an empty list item followed by two orphan
  lines. The exemption itself is right (spec §14 forbids breaking inside a path, so a too-wide token must keep its own line);
  what is questionable is that the bullet has already been emitted by then, so the break lands before it instead of after it.
- **Fix (undecided):** either attach the bullet to the over-wide token (`- [[目…]](…)` on one line, whatever its width) or
  document the current shape as intended and teach the golden verifier's R2 to accept a bare-bullet first physical line. The
  second option is only honest after somebody adjudicates it — until then `verify_catalog_goldens.py` keeps rejecting the
  shape, which is why F16's fixture uses a space-free CJK title that stays **inside** the budget rather than one that does not.
- **Do when:** someone decides whether `- [[…]]` must begin every entry. Then either fix `wrap` (a golden-affecting change:
  no current golden exercises this shape, so only the new fixture and unit cases move) or amend R2 and add a golden that pins
  the accepted shape deliberately.

**Audited 2026-11-06 — still open, re-measured, and waiting on a human decision rather than on a measurement.** Reproduced at `2c65525` with a
throwaway probe against the public renderer (`catalog::render`, one entry, 55 ideographs in the title = 110 columns in one token,
`continuation_indent = 2`), which printed exactly:

```text
-
  [[目目…目]](plans/cjk-unbroken.md)
  · 3 B · #long · →0 ←0
```

The probe file was deleted afterwards and `git status --porcelain` returned to 0. The verifier still rejects the shape, so the card's claim
that this cannot be closed by quietly adding a golden is true: `verify_catalog_goldens.py`'s entry loop requires every first physical line to
start with `- `, and a bare `-` fails that check before any entry regex runs. **What it needs:** someone decides whether an entry must begin
`- [[title]]`. Option A moves the bullet into the over-wide token (a golden-affecting decision, though no committed golden exercises the shape);
option B keeps the renderer, amends R2, and adds a deliberate golden. Neither is an audit's call. F16's fixture still avoids the shape on
purpose (`docs/design-f16-wide-goldens.md` §3), so nothing pins today's behaviour.

**Closed on 2026-11-06 — option A, by the human's ruling, merged into `main` at `4ec0794`** (branch `f25/bullet-stays-with-entry`: `03cb04a` the fix,
`0d7bf65` falsifiability and the boundary case, `9abb5b4` an unrelated stale-README correction). The card's framing was that someone must decide
whether an entry begins with its bullet; the ruling is that it does, because `- ` at the start of a physical line is what makes that line parseable as
an entry — titles are optional in this catalog, the marker is not.

- **The mechanism** (`crates/np-core/src/catalog.rs`, `wrap`): a break is refused before token index 1, so the widest second token travels with its
  bullet instead of being pushed off it. Its doc comment states both the boundary (the old code broke when the second token exceeded 98 columns) and
  what the fix accepts in exchange — a link of exactly 99 or 100 columns now yields a first line of 101 or 102, which §14 permits because that line
  holds a single unsplittable token, and which `verify_catalog_goldens.py`'s `width_ok` allows on the same condition.
- **Two cases, each able to lose.** `an_over_wide_second_token_keeps_the_bullet_on_its_line` asserts an absolute width of 139 rather than "the entry is
  unchanged", because an identity assertion passes on any renderer; `the_bullet_rule_bites_at_ninety_nine_columns_of_link` is the boundary case, and it
  is the only thing that kills a "break when the token would fit alone" mutant. Mutants run against production: `index > 1` → `index > 0` and
  `index > 1` → `index > 2` each fail both cases; the fits-alone mutant fails only the boundary case.
- **End to end, not just unit:** the built binary over a scratch vault containing a 110-column CJK title produced an `INDEX.md` with
  `grep -c '^-$'` = **0** and both entries starting `- [[`.
- **R2 was not amended and no golden was touched.** The card's warning that this could not be closed by relaxing the verifier is why option B stayed
  unchosen; `GOLDEN CHECK: pass` on the merged tree, with `git status --porcelain` at 0 afterwards. Merged-tree gates: 14 rows exit 0, `cargo test
  --workspace` 669 passed / 0 failed, CI run `42223491720` success on all three jobs.

## F26 — the image decoder accepts more formats than §7's extension list promises

Found reviewing R7c Task 4 on 2026-11-03, and **measured rather than inferred**: a BMP payload handed to
`mtmd_helper_bitmap_init_from_buf` came back a live bitmap and then a vector (80 image tokens) — reached through the
artifact suite with the bytes under a `.png` name. `stb_image`, already inside `libmtmd`, decodes bmp, tga, gif, psd,
pnm and qoi as well as png and jpeg. The webp and video arms need ffmpeg (`mtmd-helper.cpp:417-427`), which is **not
installed on this host**, so whether they accept anything here is unmeasured (F18) — the claim in this entry is only
about what was observed.

What bounds the corpus today is §7's extension gate in `crate::media`, not the decoder. So a mislabelled file embeds,
and search would report `kind=image` beside a path whose bytes are none of the three formats R7c promised (the human's
scope decision: png/jpg/jpeg). Nothing is corrupted and no vector is wrong — the pixels are real — but the promise and
the artefact disagree.

Two ways out, and Task 5's discovery pass is where the choice belongs: sniff content in discovery (PNG signature, JPEG
SOI marker; everything else refuses as `unsupported-format`, a class §8 already defines and `media.rs` already emits),
or widen the published promise to what this decoder actually takes. The first costs one read of a few bytes per media
file — the file is opened for hashing anyway — and keeps the promise small.

**Closed on 2026-11-03 by the first option.** `media::image_kind_of_prefix` accepts a PNG signature or a JPEG SOI marker and
nothing else, and the embed pass classifies from the bytes it then digests and encodes — one read, so the judged, digested and
encoded bytes are literally the same slice. A `.png` whose payload is a BMP now refuses as `unsupported-format`
(`tests/embed_media.rs::a_remote_or_inline_target_is_counted_and_never_fetched` and the content-gate case in that file;
`tests/media_resolve.rs::a_mislabelled_file_resolves_and_then_fails_the_content_check` pins the division between placing a path
and judging bytes).

## F27 — a figure rewritten in place, at the same length and whole-second mtime, keeps its old vector

Filed while reviewing R7c Task 5b on 2026-11-03. §7 chose D12's stamp for image files, and that stamp is **size plus
whole-second mtime**. An editor that rewrites a figure in place — an atomic save, a re-export of the same diagram at the same
resolution, a copy that preserved length — can leave both fields identical while every pixel changes. With a current vector
already stored, the pass reads nothing and the old vector keeps answering `np search`.

This is not a bug in the pass: it is what the chosen stamp means, and closing it means hashing every figure on every run,
which is the cost §7 traded away. What the pass *does* guarantee, and now tests, is that **anything it encodes is digested from
the bytes it encoded** (`tests/embed_media.rs::a_figure_encoded_after_a_refusal_is_digested_from_the_bytes_it_encodes`), so a
fooled stamp can delay an encode but never mislabel one. The blind spot itself is pinned by
`a_same_size_same_mtime_overwrite_of_a_current_figure_is_not_detected`, which will fail loudly if someone closes the gap and
forgets to update this entry.

Ways out, if search accuracy starts to matter more than embed time: compare mtime with sub-second precision (needs `statx`
or a dependency — the reason D12 rounded in the first place), store the digest per path rather than trusting the pair (which
is a full read anyway), or watch for writes and mark dirty paths out of band. None is a small change; none is needed until a
corpus demonstrates the loss.

## Carried out of the M2a chunker review (2026-10-28)

Two behaviours `crates/np-core/src/chunk.rs` defers in its own comments, recorded here rather than left in a docstring —
the same treatment design-m2-embeddings §3 gave "No FTS5 yet". Both change where chunks start and end, so both belong to
one `CHUNKER_VERSION` bump rather than to a quiet patch.

- **No overlap between chunks yet.** `chunk` cuts at blank lines and its chunks are disjoint — `tests/chunk.rs` asserts
  `byte_start >= predecessor.byte_end` — so the spec's "~350 tokens ±overlap" is not implemented, and a seam that cuts a
  sentence in half stays cut. **Do when:** the tokenizer-aware cut ships overlap and every row says which chunker wrote
  it (`docs/design-m2-embeddings.md` §4).
- **The chunker is fence-blind.** A "block" here is a maximal run of non-blank lines, not `crate::parse`'s notion of one,
  so a fenced code block holding a blank line can be split in half — the shape §4 says embeds garbage. **Do when:** a
  note with a blank line inside a fence keeps that fence inside one chunk.

## Explicitly not in this backlog

Embeddings and clustering, compression into `processed/`, item timelines and tick extraction, the daemon, API,
watcher, scheduler, web UI and desktop shell remain separate milestones with their own specs — nothing here should
be mistaken for them, and no table or module in this file pre-implements them.


## Carried out of the M2a whole-branch review (2026-10-29)

Found across the seams between rounds, where per-round review structurally cannot look. D2 and D5 are **decisions to
make before round 4b embeds anything** — after that they cost vectors, not just rows.

- **D1 — no version stamp covers `decode`, yet decode output is the chunker's domain.** Bumping `PARSE_VERSION` does
  *not* re-cut chunks: `EMBED_VERSION` counts chunker coverage only. Either fold decode into that key's stated scope or
  add a decode component to chunk currency. Until then a decoder change silently leaves every chunk describing bytes no
  current build produces.
- **D2 — frontmatter is embedded, and offsets index the file rather than `notes.body`.** Fixture
  `frontmatter_is_part_of_the_text_being_cut` states where we stand. Decide before 4b: strip metadata from the embed
  domain (costs a `CHUNKER_VERSION` bump = re-cut every row) or define citations against the file and say so in M2b.
- **D3 — a stale note's chunks and vectors read as current.** `chunk_digest`/`chunk_state` take a `note_id` and never
  join `files`, and `mark_stale` leaves `files.hash` at the last-known digest, so every chunk of a deleted file answers
  "current". The catalog has exactly this liveness fence; these readers do not. Fix: join `files.state = 'ok'` on the
  embed-side read, with a stale-note fixture.
- **D4 — `embeddings.source_hash`'s level is documented and tested but unenforced.** Both levels are `String`, both in
  scope during a scan; passing `files.hash` compiles and would claim currency after a chunker bump. Pinned by
  `a_vector_stamped_with_the_note_digest_is_not_found_by_a_chunk_text_question`; still worth a newtype when the embed
  round writes real vectors.
- **D5 — nothing decides whether the embed round deletes or compares stale vectors.** When a chunk's text changes its
  row keeps its id and its old vector survives with the old `embeddings.source_hash` (by design), but
  `delete_vectors_for_chunk` has no production caller and no rule. Round 4b's brief must name which rule it is.
- **D6 — no size or throughput number for chunk writes.** `chunks.text` stores the decoded corpus a second time
  (`notes.body` already holds it), all of it inside the scan's single write transaction, whose `BUSY_TIMEOUT_MS = 1800`
  doc says the write phase "has not been checked against a write phase over a large vault". Print chunk rows and
  database bytes in `tests/perf.rs`; record cold/warm at realistic note sizes in the backlog.
- **D7 — `install` copies up to ~540 MB before it can discover the workspace database is busy**, and then says
  "model database: …" naming neither the path nor the competing command; the copy survives as an unclaimed file only
  discoverable by running `list`. Add a variant that names the workspace DB and says re-run.
- **D8 — `models::access()` (absolute-for-OS-access spelling) is applied to install's writes, artifact reads and hash**;
  still worth the long-path unit case on Windows so a future call site cannot regress silently.
- **D9 — chunk-gap stderr line: asserted now** (`an_incomplete_chunk_pass_reports_its_own_gap_on_stderr`, unix like its
  sibling). Recorded because the reason given for not writing it ("no capture seam") was false.
- **D10 — migration 5's hand-written child-row lists are chunk-safe only by declaration order.** It deletes children by
  hand and does not list `chunks`/`embeddings`, which is correct solely because it runs before migration 7 creates them.
  Any future duplicate healing must add both tables or it will orphan rows on a legacy database.
- **D11 — "cannot stat the artifact" is answered as §6's *artifact absent* (R6c2 review P2-2).**
  `embed::artifact_is_readable` folds every `fs::metadata` failure into `EmbedError::ArtifactUnreadable`, and `np-cli`
  maps that variant to the absent arm — whose advice is "install the artifact". For a file that exists but cannot be
  stat'd (no search permission on the parent directory, a symlink loop) that sentence sends someone to re-install a
  file they need to `chmod`. The fix is small: map only `ErrorKind::NotFound` to the absent arm and let other kinds
  surface as the OS failure they are. Deferred because proving it needs an argv case that manipulates directory
  permissions — Linux-only evidence, and F18 says we do not ship a platform claim we have not run.
- **D12 — CLOSED 2026-10-31: verify-on-change shipped (`local_model::verify_artifact`, migration 8).**
  The decision taken was option 1 — stat the artifact and re-hash only when size or whole-second mtime moved — because
  R6d priced both sides of it: 11.32 s to hash the pinned artifact, ~0.09 s for llama.cpp to page in every byte, so a
  `stat` on the common path and one honest hash per change. What it catches is corruption (interrupted copy, partial
  restore, replaced file); what it cannot catch is a rewrite by someone who also sets the mtime, and `np embed`'s new
  `digest` line says "detects changed bytes, not a crafted mtime" instead of claiming verification. Two decisions inside
  it are worth keeping near the closure: migration 8's columns are **not backfilled** (an old row pays one hash rather
  than inheriting credit it never earned), and `np models install` deliberately does **not** stamp on its
  `AlreadyInstalled` path, because §7 case 5 promises a repeated install rewrites no column. Full reasoning, the report
  wording, the fourth report arm (`VerifiedUnstampable`, so a run that recorded nothing never claims it did) and where
  each rule is tested: `docs/design-m2-local-embeddings.md` §7's verify-on-change subsection.
- **D13 — llama.cpp defaults to 4 threads on a 16-CPU host; nobody has measured what raising it buys.**
  R6d's OpenMP A/B (2026-10-31) found both build configurations printing `n_threads=4 n_threads_batch=4` from
  `llama_context_default_params()`, which is why the flag moved no throughput at all: ggml's own pthread pool and an
  OpenMP parallel-for were handed the same four workers. `local_model::load` accepts that default without comment.
  The unmeasured question is whether `llama_context_params.n_threads` set from `std::thread::available_parallelism`
  (or a config key) turns 157 tok/s into something worth waiting for on a big note — and what it costs in peak RSS
  and in being a well-behaved background process next to someone's editor. Filed rather than done because it changes
  runtime behaviour for every user of the feature, needs an A/B of its own (thread counts 1/2/4/nproc at both lengths,
  RSS included), and no measurement so far says the current 81–157 tok/s is unacceptable.
  **Addendum from R6d round 3 (2026-10-31): batching has a measured ceiling; D13 does not.** Two cohorts in one
  process, run twice, put the cost shape at **29.0–31.1 ms fixed per decode** and **5.531–5.548 ms/token marginal**
  (≈180 tok/s as token count grows), so batching texts into a single decode could recover at most ~14 % of a short-note
  decode and ~2 % of a long one — measured at the current four threads. The stronger-sounding version of that sentence,
  "thread count is therefore the bigger lever", is *not* something this measured: raising `n_threads` has never been
  swept here, and the one threading intervention actually tested (OpenMP on/off) moved throughput by nothing, both
  configurations handing ggml four workers. What the cohorts license is narrower and still worth writing down — at four
  threads, decodes between 33 and 251 tokens fit ~30 ms fixed plus ~5.6 ms/token, so batching's ceiling is small and the
  thread sweep is the next measurement worth taking. Numbers and scope: §14's corpus subsection and
  `crates/np-core/tests/probes/corpus-run-2026-10-31.log`. The authorisation question
  is unchanged — this still changes runtime behaviour for every user, and the sweep (1/2/4/nproc × both lengths × RSS)
  has not been run.
  **Closed as a measurement, open as a decision (2026-11-02).** The sweep ran twice — 1/2/4/6/8/12/16/**32** workers on a
  375-token chunk and 2/4/8/16 on a 7,970-token one, one process per cell, plus three restricted-cpuset cells.
  **All sixteen workers on this host are 2.90× the default at a realistic chunk and 3.60× at a near-window one, for
  +6.6 MiB of peak RSS** (and no measurable difference at the long length); thirty-two workers — twice the machine's own
  count — collapse to **91.4 tok/s while burning 1,553 % CPU**, worse than four on throughput and better than nothing on
  cost, so a count above `available_parallelism()` is not an option any policy should offer (the optimum between 16 and
  32 was not probed, and nothing above 16 workers has been measured anywhere); and the vectors are **bit-identical
  across every worker count compared** (`max |Δ| = 0`, including at 32 workers), which retires the reproducibility
  objection this entry was written around. Numbers, harness and scope: §14's "D13: the
  worker count, measured". What remains is not a measurement: whether any user's default should change (and if so to
  what — see the floor argument in that section: a bare 80 % is one worker on a two-core laptop), and a re-pin caveat,
  since bit-identity is an observation about this llama.cpp and must be re-checked with
  `crates/np-core/tests/probes/thread_vectors_compare.py` after one.
  **Decided 2026-11-02 (option (c)), and the follow-up ran the same day.** `[embeddings] threads = "auto"` resolves to
  `min(cpus, max(min(cpus, 4), floor(0.8 × cpus)))` — twelve workers on this host, never more than exist, never fewer than
  four where four exist — with an explicit count honoured as written and reported by `np embed`. Design:
  `docs/design-m2-thread-policy.md`. The corpus was re-measured under it with a same-day four-worker control: 456.43 s →
  **181.41 / 180.97 s**, 16,153 → **40,642 / 40,741 chunks/hour** (**2.52×**), peak RSS unchanged (−0.2 %) and the same
  +1,556 KiB drift, so the speedup is not confined to the forward pass. Log:
  `crates/np-core/tests/probes/corpus-auto-2026-11-02.log`; the older four-worker rows above stay quoted as what they were
  taken at, because `local_corpus.rs`'s `run_pass` still pins four. §14's cohort solve (31.1 ms fixed + 5.53 ms/token) is
  deliberately **still four-worker** — it measures per-decode overhead, which is a different question from thread scaling.

## F28 — the shipped binary cannot reach image code, so `np embed` never embeds an image

**Found 2026-11-04 while measuring R7c Task 7's package sizes.** The plan asked for a fourth
figure — `local-model,vision` — beside R7b's three shipped sizes, "where the fifth archive shows
up". It cannot be built: `crates/np-cli/Cargo.toml` declares `local-model = ["np-core/local-model"]`
and `fetch = ["np-core/fetch"]`, and has **no `vision` passthrough**, so
`cargo build --release --bin np -p np-cli --features local-model,vision` fails at feature resolution
with *"the package 'np-cli' does not contain this feature: vision"* (exit 101, measured).

**What that means in practice, stated plainly.** `np_core::vision` is reachable only when the
`vision` feature is on, and only np-core builds can turn it on. So in every `np` binary — including a
`--features local-model` one — `Embedder::supports_images()` answers `false`, §11's images arm counts
figures and refuses them all as `vision-unavailable`, and `np search` ranks whatever image vectors
already exist but cannot make a new one. The whole R7c pipeline is exercised end to end in
`crates/np-core/tests/local_vision.rs` and `tests/embed_run.rs`, never through the command line.

**Why this was not caught earlier.** Every gate on this branch runs the feature pair against
`-p np-core`, so a green `--features local-model,vision` row says the *library* builds that way. The
one place that would have failed — a CLI build with the same pair — is not in any gate list, because
the plan's Step 5 phrased the measurement as if the shape already existed.

**The decision owed, which is a product question, not a bug.** Either (a) add `vision =
["np-core/vision"]` to np-cli and let an operator build a binary that embeds images — at the cost §10
already predicts: a fifth archive, cmake's `LLAMA_BUILD_MTMD`, and a larger artifact; or (b) keep
image embeddings library-only for now and say so in README's `np search` section, where a reader
today would otherwise conclude that `np embed` fills the image index. Under (b) the honest wording is
"image vectors come from a build made with `--features local-model,vision`; the shipped binary records
figures and refuses to encode them".

**Do not resolve this by adding the passthrough as a side effect of a docs commit.** It changes what a
downloaded binary links, which is exactly the kind of size-and-dependency claim §14 exists to keep
measured. If (a) is chosen: re-measure all four shapes with `strip`, in the same session, and record
the projector's 554,821,056 bytes as a *fetched* cost separate from the binary's own.

**Still open after R7c Task 8 (2026-11-04).** README now carries option (b)'s wording — its "Images in the index" section
states that every figure lands as `vision-unavailable` in a binary built today, and the `np search` row says the command ranks
image vectors *that exist* rather than implying `np embed` filled them. That is documentation of the gap, not its resolution:
the decision about what a shipped binary links is still owed, and no `vision` passthrough was added here.

**Ruled 2026-11-06 by the human — option (a), plus a requirement this card did not contain.** "`np` should also embed images",
and: an image embedding needs **its own separate vector, linked to the place where an image is mentioned or placed in notes**.
Measured against `main` = `64a6b73`, the ruling's first half is already shipped and its second half is not, which changes what
this card is asking for:

- **"its own separate vector" — exists.** `media_embeddings` keys a vector by `(media_id, model)` with `dim > 0` and
  `length(vector) = 4 * dim` (`crates/np-core/src/db.rs:362-375`), one row per figure per model. Image vectors are not folded into
  text chunks, and nothing about the ruling asks them to be.
- **"linked to the place where an image is mentioned or placed" — only partly exists, at the wrong grain.** What links a figure to
  somewhere is `media_path`: every workspace *path* whose bytes hash to that figure (`db.rs:347-359`). It answers "which files hold
  these bytes", not "where a note placed this figure". And `np search` does not even report the set: `media_report` returns the
  **lexicographically smallest** stored path and one smallest live linker note (`search.rs:432-447`, documented at `:67`), so a
  figure used in twelve notes reports one of them by sort order. The code says why it stopped there — "a `note → media` edge would be
  an eleventh migration, which Task 6 forbids" (`search.rs:480`).

So F28 is now two pieces, and the second is larger than the feature flag this card was written about:

1. **Capability.** `vision = ["np-core/vision"]` in `crates/np-cli/Cargo.toml`, so a shipped binary can encode a figure at all — with
   §14's rule intact: re-measure every archive shape with `strip` in one session, name the projector's 554,821,056 bytes as a *fetched*
   cost beside the binary's own, and keep the ~40 MB ceiling claim honest or correct it.
2. **Placement.** An occurrence edge (migration 11) from a note to the figure it references, so a hit can name the note that placed it
   instead of one chosen by byte order — which changes what a media hit renders as, and therefore its goldens, its README section, and
   `media_report`'s contract.

**Steps ① and ② landed on `main` (2026-11-06).** ① `8317a1b`: "this build has no image code" and "this workspace has no projector" are separate
refusal classes (`ImageBlocker` on the provider, `projector-absent` emitted by the image pass). ② `736a66d` (commit `94def8f`): np-cli declares
`vision`, so `cargo check -p np-cli --features local-model,vision` — the command this card measured as impossible at filing — now compiles; the CLI
hands over a projector only if this database recorded it and its digest or stamp matches (`local_model::projector_artifact`), refusing the run when a
recorded file's bytes moved; and CI gained `Feature local-model,vision (ubuntu-latest)`, the row whose absence let this card stay open through five
days of green runs. Evidence: branch run `42258831178`, four jobs success, its vision step reporting 15 passed / 0 failed with the artifact-backed cases
printing `NOT COVERAGE`; merged-tree gates 17 rows exit 0 including the CLI-with-pair check, workspace 670 passed / 0 failed, `GOLDEN CHECK: pass`.
Three mutants killed by three distinct cases (skip the digest comparison, resolve by path existence, ignore the stamp pair), none needing a model file.
**Still open in this card** (as filed): `np models install --member`, the five archive shapes against §14's ceiling, and the placement half. All three are
closed below — the card closes at the end of this section once the merged tree is graded.

**Step ③ landed the same day** (commit `b082272`, merged with history at `5374dc1`; branch CI `42324417379` green on all four jobs, merged-tree gates 17 rows exit 0, workspace 680 passed / 0 failed, `GOLDEN CHECK: pass`). `np models install --member <main|mmproj>`: the member name keys `model_files`, the file lands at `<id>.mmproj`
(`member_artifact_name`), and the model row is not written — asserted as a whole-row comparison, because a projector digest copied into `models.sha256`
would leave every file readable and make each later verification compare the wrong bytes. Three refusals precede any read: an invented member names
itself and offers what is installable (`models::INSTALLABLE_MEMBERS`, one list for the parser and the write path so they cannot drift); `--dim` beside a
member that has no width refuses instead of being ignored; and `--member mmproj` where no model is installed answers about the missing model **before**
touching the file — the row check sits ahead of the source hash, deliberately out of step with `install`, whose row comparison needs that hash. The
default binary in this run has no `fetch` feature at all, which is the point: eleven shipped-binary cases (refusal order, both JSON shapes, repeat =
`already installed`, `unclaimed 0`, `models.sha256` still the main artifact's digest) ran on a machine that dialled nothing. np-core gained five cases
and np-cli five; mutants killed: skip the row check, record under the `main` member name (two tests), accept an unverified existing projector, drop the
`--dim` guard, drop the member-set check — the last meaning `--member audio` silently installs a projector, which is exactly the drift the shared list
prevents. **One equivalent mutant was found and removed rather than kept**: skipping the member write when the recorded digest already matched decided
nothing (`record_model_file`'s CASE clears a stamp only when the digest changes), so the branch is gone and the rule lives in one place.

**Step ④ — sizes — measured the same day.** Five release+`strip` shapes on one host: no features **6,080,592 B**, default (`fetch`) **8,223,056 B**,
`fetch + local-model` **14,018,104 B**, `fetch + local-model + vision` **15,957,688 B**, `--no-default-features --features fetch` **8,223,056 B**. The image
code therefore costs **+1,939,584 B** over the text-only C++ build, attributed by symbol rather than by feature name (`nm --defined-only`: 0 `mtmd_*` /
0 `clip_*` without `vision`, 686 / 928 with it), and §14's ≤ 40 MB ceiling survives the vision shape with ~24.8 MiB of headroom — so the sentence stays
true and gains the row that tested it, in `docs/design-m2-local-embeddings.md` §14, next to the finding it retires. Two things this measurement refuses to
claim: that equal sizes mean one artifact (`--no-default-features --features fetch` matches `default` at 8,223,056 B while differing in 2,192,920 of those
bytes), and that the table describes what a user downloads to encode figures (the projector is **554,821,056 B**, ~35× the largest binary in it).

**Step ⑤ — placement — landed the same day, and it needed no migration.** `Hit` gains `linked_notes`: every live note linking a figure, byte-ordered,
assembled once inside one constructor whose head becomes `source_note` and whose length becomes `linked_from`. The card's own wording ("an occurrence edge
(migration 11) from a note to the figure it references") was corrected earlier the same day by the ruling that the link is figure → note, so what was
missing was never an edge — it was that `media_report` already assembled the whole set and then reduced it to one name and a count before any caller could
read it. Four np-core cases: all three linkers named in byte order with insertion order still wrong on purpose; the three fields unable to disagree across
a corpus holding a two-linker figure, an orphan and note hits (the loop asserts the corpus contained both shapes, or it would prove nothing by emptiness);
linkers gathered from every path one figure is stored at; and a deleted linker leaving the list as well as the count. Three mutants killed — return only
the first linker, take `source_note` from the end of the list, count "any" instead of length — each dying on several cases including pre-existing ones.
Two fixture bugs of this file's own documented species were caught by asserting non-emptiness: both planted media rows under the path a link *spells*
rather than the one it resolves to, and `linked_notes` came back empty where the count had been quietly satisfiable at 0.

**End-to-end, with real artifacts and no mocks:** a vision build (`--features local-model,vision`), both gguf files installed offline through step ③'s
`install --member`, one 463-byte PNG linked from two notes, then `np embed` (image encoded) and `np search`. The JSON hit carries
`linked_from: 2`, `linked_notes: ["notes/a.md","notes/b.md"]`, `source_note: "notes/a.md"`; the text line reads
`0.5162 image img/fig.png (from notes/a.md +1 more)`. Before this step that second note existed only as a number in a key nobody reads on a terminal.
The artifact-gated CLI case that pins `--json`'s key set was re-run with `NP_TEST_MODEL_PATH` set — 1 passed, not `NOT COVERAGE` — so the six-key schema is
graded by a real model rather than asserted.

**Merged with history at `d05a70a` (commit `f6c9eb9`), and every demand of this card is now met — but it does not close today, because CI cannot grade it.**
GitHub Actions has failed all four jobs on every SHA since 2026-11-07T01:57Z with **zero steps created** (this merge's run is `42357863387`, four failures in
~2 s), so "green CI" is not available as evidence for anything, and this repository closes cards on graded trees rather than on local runs. What *is*
graded on the merged tree: 17 gate rows exit 0; workspace **684 passed / 0 failed**; `GOLDEN CHECK: pass`; and the two artifact-backed suites actually
running — `np-cli`'s `real_model` module with `NP_TEST_MODEL_PATH` set (**5 passed / 0 failed**, including the six-key hit schema) and `np-core`'s
`local_vision` with both ggufs present (**15 passed / 0 failed**, no `NOT COVERAGE`). The closure paragraph goes in when a run that *has steps* grades
`main`; until then this card stays open with its work done, which is the honest state and reads differently from "nothing left".

**A gap this step exposed without causing — closed the same day.** `np models list` printed one line per model and never mentioned members, so nothing told
you what the workspace recorded (true of fetched projectors since migration 9, hence a reporting gap rather than an install one), and the only confirmation
was watching `np embed` stop reporting `projector-absent`. `ListedModel` now carries `members`: name, claimed file name, present-or-absent — **by stat, never
by hashing**, because `list` is the cheap question and byte verification already has a home at use time, where 554 MB is being read anyway. Text gains
`files mmproj:present` only when there is a member to name; `--json` gains an eighth per-row key holding `[]` for a single-file model, since empty is a fact
and absence is not. The main artifact stays out of the list: its verdict is the row's own `state`, which does hash and can say `mismatch`, and one file with
two answers of different strength is how readers end up trusting the weaker one. Four np-core cases and two CLI ones; mutants killed for listing `main` as a
member, treating any `metadata()` success as present (a directory in the member's place is absent — the claim that needed a case rather than a doc sentence),
and never populating the set at all. The third mutant initially "survived" only because `cargo fmt` had rewrapped the line the patch searched for, so the
mutation never applied — checked before believing it, the same trap as an earlier no-diff mutant this round.

**Ruled in the same turn: the link is figure → note.** Not a coordinate inside the note — so the edge is one row per (note, figure), with no line,
heading or chunk column. That settles the shape question this card ended on and leaves two to measure rather than debate: what the `vision`
passthrough costs a shipped binary (§14's ~40 MB sentence has to survive it or be corrected), and what a media hit should render now that a figure
has more than one honest note to name — `media_report`'s "smallest path, one linker" contract was chosen because there was no edge to ask, and once
there is, choosing by byte order becomes an arbitrary answer to a question the database can now answer properly.

## F29 — every `.svg` in the vault refuses as `unsupported-format`, and a diagram library is mostly SVG

Filed 2026-11-04 by R7c Task 8 so that §8's deferred format has a number instead of living inside one design bullet. The human
asked for ".png, .jpg and .svg" on 2026-11-02 and dropped SVG to "only .png and .jpg for today" once told it is not raster and
no option turns it on.

- **Where the refusal lives:** `crate::media::mime_of` accepts `png`, `jpg`, `jpeg` — case-folded, extension only — and nothing
  else, so an `.svg` link refuses before a byte is opened. F26's content gate made this narrower, not wider:
  `image_kind_of_prefix` then requires a PNG signature or a JPEG start-of-image marker, so a mislabelled file also lands here.
- **Why it will be noticed:** every `.svg`, `.drawio`, `.excalidraw` and `.pdf` link already resolves today as
  `links.kind = 'attachment'`, so a vault of exported diagrams prints an `unsupported-format` count on its first image-aware
  `np embed` — the exact corpus class this round was built for.
- **Fix shape (its own round, per §8: "not a list edit"):** a pure-Rust rasteriser behind the `vision` feature, rendering at a
  fixed longest side with external references and DTDs disabled — an SVG is a file that asks to fetch things — plus the
  dependency's measured package-size delta beside R7b's three shapes, and §8's rule that cost is measured per encode still
  applies to whatever pixel count the renderer chooses.
- **Done when:** either that ships with a measured size figure and an SVG case in `tests/embed_media.rs` (embedded, not merely
  refused), or someone decides vector formats stay out permanently and README says so as a product limit rather than leaving it
  readable only as a format accident.

**Ruled 2026-11-06: the second branch of "done when", on the assistant's recommendation — not implemented yet.** Vector formats stay out,
and README is to say so as a product limit rather than leaving `.svg` refusal readable as a format accident. The ruling follows F28's: now
that a shipped binary is to encode figures, this card becomes the question "what will it encode", and the answer is raster formats — because
the fix shape above needs a rasteriser that must disable external references and DTDs ("an SVG is a file that asks to fetch things"), which is
a dependency and an attack surface added for a capability nobody has asked for yet. Not closed: the README wording does not exist until someone
writes it, and `unsupported-format` will keep counting diagrams on the first image-aware `np embed` until it does.

**Closed on 2026-11-07, on the second branch of its own "done when"** — vector formats stay out permanently, and README says so as a product limit. The
wording is in `README.md`, "Images in the index": a paragraph naming `.svg`, `.drawio`, `.excalidraw` and `.pdf` as out *by decision*, giving the reason a
rasteriser would cost (a decoder that must disable external references and DTDs — an SVG is a file that asks to fetch things — plus a dependency, an attack
surface, and a per-encode measurement §8 requires before anything ships), and stating the answer to "what will `np` encode" as **PNG and JPEG, extension
and content signature both**. The `unsupported-format` row in the class table now points there instead of implying a re-export is all that stands between a
diagram vault and image search. The paragraph also says what a refused figure does *not* cost: the note's own prose is indexed and searchable as always, and
the link is recorded in `links` like any other attachment — which is the card's own opening observation ("every `.svg` … already resolves today as
`links.kind = 'attachment'`") turned into something a reader can rely on rather than a surprise count.

**Closed without CI grading, deliberately, and that choice is stated rather than skipped past.** CI has been out of included minutes since
`2026-11-07T01:57Z` (`docs/ops-actions-minutes.md`), and this card's "done when" is satisfied by text a reader can open — no code path changed, so there is
no behaviour for a runner to grade. What was run on the tree: 17 local gate rows exit 0 and `GOLDEN CHECK: pass`. The one *factual* claim added here that a
test could hold — that an unsupported attachment link still lands in `links` as an `attachment` row — is already pinned elsewhere (F26's content gate cases
in `crates/np-core/tests/embed_media.rs`, and the resolver cases in `tests/media.rs`), so no new assertion was invented to make a docs commit look like work.

**A stale sentence this closure retired on the way.** README's "Does not" row still claimed "no image encoding in a shipped binary yet (F28)" — untrue since
step ② merged at `736a66d`. It now says image encoding needs a `--features local-model,vision` build plus an installed projector, that release binaries are
text-only by default, and that they say so per figure rather than skipping silently; the row also gained the F29 limit, so "does not" lists vector formats
explicitly instead of leaving them to be inferred from a refusal label.

## F30 — Matryoshka truncation is the storage lever for a big image index, and it has never been exercised

Filed 2026-11-04 by R7c Task 8. Rows are stored at the model's native **768** (`docs/design-m2-image-embeddings.md` §1.2,
and the human's locked decision (b) in `todo.md`: "768 with no MRL truncation"). The card supports 768 / 512 / 256 / 128 and two
rules that are easy to get wrong: a sliced unit vector must be **re-normalised** — skipping it "degrades ranking quality
silently", in the card's words — and query and corpus **must share a dimension**.

The lever is real because a second vector source doubles the vector surface on an image-heavy vault: 4 × 768 bytes per stored
row in `embeddings` and again in `media_embeddings`, one row per chunk and per distinct figure respectively. How much that costs
on a real corpus is **not measured** — no index-size figure for either table exists in this repository, which is the first thing
this entry owes.

- **Done when:** index bytes are printed for a corpus with figures (D6's sibling measurement, still open), and someone decides
  whether to re-store at 512.

**Half closed on 2026-11-06 by `docs/measurement-f30-vector-surface.md`.** Measured on the fixture corpus (16 notes → 15 chunks → 15 rows):
payload **3,072 B per row** exactly (4 × 768), whole database **212,992 B**, and **147,456 B** with the vectors deleted and `VACUUM`ed on a
copy — so the vector surface is **65,536 B over 15 rows, 4,369 B per stored row** including index and page slack. The card's "no index-size
figure for either table exists" is now true of `media_embeddings` only.

The corpus-with-figures half is **blocked on F28, not on effort**: `np-cli` never calls `with_mmproj` (grep count 0), so no shipped command can
produce an image row to weigh. The doc does the arithmetic on the measured per-row figure and labels it arithmetic — roughly −23% at 512 rather
than the −33% the payload alone suggests, because index slack does not shrink with the slice — and lists ranking quality at 512 as unmeasured,
which needs F31's labelled set. What remains here is the human's decision, now with numbers attached. That is a re-store of vectors already held — no re-encode — but it changes model identity
  (`docs/design-m2-local-embeddings.md` §4), so it needs its own migration and `models` row, a re-normalisation on write, and a
  refusal rather than a blend when a query's width disagrees with the stored one.

**Second half measured 2026-11-07 — the blocker was this card's own dependency, and F28 cleared it.** `np-cli` now calls `LocalEmbedder::with_mmproj`
(`main.rs:693`, one call site, reached only when the workspace recorded a projector whose digest or stamp matches), so an ordinary `np embed` writes
`media_embeddings` rows and they can be weighed. Numbers, on a 20-note corpus with **eight distinct PNGs** (1,092–11,083 B on disk, 24×24 through 80×80), release+
`vision` binary, both artifacts installed through the CLI:

* payload per stored vector: **3,072 B in both tables**, exact (`sum(length(vector))/count(*)` = 24,576/8 and 61,440/20) — one 768-component `f32`, no wide column
  hiding in the media table;
* **vector surface per figure row: 4,608 B. Per text chunk row: 4,096 B.** Isolated by deleting one table and `VACUUM`ing a *copy* (299,008 → 262,144 → 180,224 B),
  the same trick as the first half; a media row is about **12.5% dearer** than a text row, which payload arithmetic cannot see — same vector width, more periphery;
* `phase2 − phase1` (40,960 B) is *not* quoted as the media surface: appending a figure line re-parses and re-encodes those eight notes' chunks, so that delta mixes
  two things. The deletion pair above is the clean isolation;
* slack per row is therefore measured — 1,536 B around a media vector, 1,024 B around a text one — which turns the first half's guessed "roughly −23% at 512" into
  **−22.2%** on the media row (4,608 → ~3,584) and −25.0% on a chunk. Still arithmetic in one respect: nothing can write a non-768 row, so "slack stays put when the
  payload shrinks" is untested.

**The comparison this document thinks the decision should be made against:** a 5,000-figure vault holds ~22.0 MB of media surface at 768 and would save **~4.9 MB**
at 512 — **0.9% of the 554,821,056 B projector file** that has to be downloaded and read for any of those rows to exist. That is not an argument that 512 is worthless;
it is an argument about which side of the ledger a migration, a model-identity change (`docs/design-m2-local-embeddings.md` §4), a re-normalisation on write and a
width-disagreement refusal are worth buying against.

**Recommendation: keep 768 and close this card by decision.** The saving is under 1% of the artifact the feature already requires, the quality question cannot be
answered without F31's labelled set, and the cost is not a config value but a migration plus identity rules for vectors already stored. If the human agrees, the card
closes with that ruling rather than with code; if not, the numbers above are what the re-store proposal starts from.

**Nearly got this wrong on the fixture, recorded because nothing failed.** The first attempt appended figures by a hand-written name list while the corpus files were
numbered from a different order: four appends hit real notes, four created new ones, and the run reported `images links 4 · embedded 4` cheerfully on its way to four
media rows. The fixture now keys off filenames that exist and asserts both that the corpus gained no files and that exactly eight notes carry a figure *before*
measuring. One timing observation, not a measurement (§8 wants ≥2 runs and a spread): phase-2 embed — 8 image encodes plus 8 text re-encodes with a projector open —
took 33.8 s wall in one run on this host.

**Closed on 2026-11-07 by the human's ruling on the recommendation this card asked for: keep 768, no re-store.** The measurement that made the question answerable is
`docs/measurement-f30-vector-surface.md`, both halves now in — payload **3,072 B per vector** in `embeddings` and `media_embeddings` alike, and vector surface of
**4,608 B per stored figure against 4,096 B per stored chunk**, measured by deleting one table and `VACUUM`ing a copy rather than inferred from payload. Slack came out
at 1,536 B around a media vector and 1,024 B around a text one, which is what turns "−33% at 512" (payload arithmetic) into **−22.2%** on the row that actually gets
stored.

The ruling rests on two numbers beside each other: a 5,000-figure vault holds ~22.0 MB of media surface at 768 and would save **~4.9 MB** at 512 — **0.9% of the
554,821,056 B projector file** that must be downloaded and read before any figure row can exist — while the cost of re-storing is not a config value but a migration,
a model-identity change (`docs/design-m2-local-embeddings.md` §4), a re-normalisation on write and a refusal when a query's width disagrees with the stored one. Bought
against a ranking question that **cannot** be answered before F31's labelled set exists. Paying a migration to buy an unmeasurable quality risk for 0.9% of an artifact
the feature already requires is not a trade this repository takes.

**What closing here does *not* claim:** that 512 would rank worse, or better — no labelled set exists (**F31**), and the card's "re-normalise or degrade silently" rule
and its "query and corpus must share a dimension" rule stay unimplemented and unexercised, now by decision rather than by oversight. If F31's set ever arrives and says
something surprising about width, this closure is the paragraph to reopen, and §5 of the measurement document holds the arithmetic it would start from (including the
one thing still untested: that slack stays put when the payload shrinks — nothing here can write a non-768 row).

Verified as measurement, not as CI grading: 17 local gate rows exit 0, workspace **693 passed / 0 failed**, `GOLDEN CHECK: pass`, and the figure half was produced by
the real release+`vision` binary against both artifacts installed through `np models install [--member mmproj]`. Actions minutes are exhausted
(`docs/ops-actions-minutes.md`), so nothing about this tree was graded remotely; the last CI-graded tree remains `5e368ac`. No code changed — this card's remaining
deliverable was a decision, and the card says so rather than inventing work.

## F31 — nothing can be said about whether search finds the right diagram, because there is no labelled set

Filed 2026-11-04 by R7c Task 8, as the retrieval-quality half of design §14.4. What exists is a **sanity check**: four generated
shapes and three sentences, where each caption beats an unrelated sentence against its own image and the best-matching image is
the right one (spike 2026-11-02; the red-square pair re-measured through the shipped seam twice on 2026-11-04, identical to six
decimals — design §14.3). That establishes one geometry, not usefulness, and §12's single ranked list is honest only if a reader
knows the difference.

- **What it would take:** a labelled set from the human's own vault — some number of queries, each naming which figure answers
  it — run through `np search --json`, reported as rank@1 / rank@5 **per kind**, plus how often an image outranks the note that
  explains it. Measurements and their spread, not assertions: a labelled set small enough to assert against is the same set that
  cannot support a claim.
- **Blocks:** any sentence beginning "search finds the right diagram", in a design doc, a README or a report, until numbers
  exist. `np search` proves the plumbing reaches both kinds; that is the whole claim on record today.
- **Done when:** §14 gains a measured subsection with the set's size and provenance, or records explicitly that the measurement
  was declined and why.

**The harness landed 2026-11-09; the card stays open on purpose.** `.github/scripts/f31_eval.py` takes a TSV of
`query<TAB>expected-path[<TAB>kind]`, runs every search mode through `--json`, and prints recall@1 / recall@k / MRR / how often an image
outranked the labelled note / how many answers never surfaced — per mode, with the run's shape above it, and `--strict` exiting 3 when a label
names something the index does not hold. Its arithmetic is gate-graded (`f31-selftest` in `gates.sh`: hand-computed numbers, plus the
"exclude the misses" mutant that would make MRR rise as recall falls). What is *not* here is a label file: queries generated from the same corpus
being searched measure token overlap, not retrieval, and would put a meaningless number in a file whose name implies it means something. The ten
minutes needed are described in `docs/design-f31-quality-harness.md`. **The ruling this card waits on is still yours**: ~20 labelled queries from
your own vault, or a sentence saying you would rather not and why — either closes it, and only the first one buys a quality claim.

## F32 — `np search` blames `embeddings.provider = "none"` on a workspace whose config says `local-model`

Filed 2026-11-05 while writing the README's troubleshooting table, which had to document the message rather than the cause.
Measured here with `[embeddings] provider = "local-model"`, `model = "embeddinggemma-2"` in the config and a **fresh data
directory** (so no `models` row for this workspace): `np search` exits 1 with
`np: embedding disabled: embeddings.provider = "none"` — a sentence about a configuration state the user does not have. The
same command with a row installed and its artifact file deleted prints the accurate one, `no embedding provider for this
workspace: its model artifact is absent, so the query cannot be embedded`.

- **Why:** `embed_provider` answers the *no row* arm with `Some(NoEmbeddingProvider)`, so `search_workspace`'s dedicated
  artifact-absent sentence — written for exactly this situation in R7c Task 6 — never fires, and the generic `Disabled`
  message from the trait arrives instead. The two no-provider arms (no row, no bytes) resolve differently and therefore
  report differently, which is the part worth fixing rather than rewording.
- **What it would take:** carry the resolution reason out of `embed_provider` as a value (the way `embed`'s report already
  carries its detail), have `search` print the cause it was given instead of the provider's generic refusal, and pin each arm
  with a CLI case asserting the exact sentence — one with no row, one with a row and no file, one default build. A test that
  only asserts "mentions embeddings" is what let this through: the present assertion passes on both messages.
- **Blocks:** README's claim (design §6, repeated there) that a refusal names what to change, for this arm; and any support
  answer that tells a user to edit their provider because their provider is disabled.

**Closed on 2026-11-05 by carrying the reason out of resolution.** `embed_provider` answers with a `ProviderState`
(`Ready` / `Placeholder { embedder, reason }` / `ArtifactAbsent`) and the placeholder carries a new
`np_core::embed_run::NoProvider`; `search_workspace` matches it with no `_` arm, so a future state fails to compile rather
than borrowing another arm's sentence. The remedy text reuses `install_hint`, which already exists in two feature-gated halves —
the design's first draft invented prose naming `np models fetch` in a build that compiles it out, and review caught that before
code. Merged at `18d0f5a`; CI run `42110909478` is green on all three jobs with the new cases executed on `windows-latest`.
Design: `docs/design-f32-search-refusal-arms.md`, including the five-arm matrix measured in three build shapes and the one
mutant that survived because it is equivalent.

## F33 — a model load writes ~1,500 lines of llama.cpp progress to stderr

Filed 2026-11-05 by the same README pass. Measured on an `np embed` run that encoded one chunk: **stdout 12 lines** (the
report), **stderr 1,553 lines** — `create_tensor:` 749, `llama_graph_n_input_tensors:` 567, `print_info:` 65, plus the loader's
metadata dump. A run with nothing owed writes **0** stderr lines, so the volume tracks real encodes and hides in exactly the
runs a user cares about.

- **Why:** our FFI never calls `llama_log_set`, so llama.cpp keeps its own default verbosity and streams to the process's
  stderr. Nothing of ours is mixed in — stdout stays clean, which is what makes this cosmetic rather than corrupting — but
  `np embed 2>&1 | tee run.log` buries the report under a thousand tensor names, and `--json` consumers that merge the two
  streams get garbage.
- **What it would take:** one `llama_log_set` at context creation with a level derived from configuration rather than a
  constant (default error-only), plus a visible escape hatch — an env var or flag — because `tests/probes/thread_sweep.sh` and
  the R6d benchmark work read some of these lines. Pin it with a CLI case asserting stderr is empty after a successful encode;
  that case needs both artifact variables, so it must print `NOT COVERAGE` rather than pass silently without them.
- **Blocks:** nothing correctness-wise. It costs a line in README ("stdout is the report; stderr is llama.cpp's") and any
  wrapper that treats merged output as machine-readable.
- **Also measured 2026-11-05, on the search side** (while measuring F32 with a `local-model` build): `np search <vault> alpha`
  against an installed artifact opens with `llama_model_loader: loaded meta data with 46 key-value pairs and 413 tensors …`
  and the metadata dump — so the volume is not confined to `np embed`; **any command that resolves a lazy provider and then
  encodes** pays it, which makes the escape hatch in §"what it would take" more urgent than an embed-only reading suggested.

**Closed on 2026-11-06 by a level the user chose.** `llama_log_set` is declared and installed once, from np-cli's `embed_provider`, behind
`local-model`; the names, the gate and the precedence live in ungated `np_core::loglevel` because `[embeddings] log_level` is config vocabulary
and np-cli builds np-core with `default-features = false`. np-core installs nothing on its own — the sink is process-global, so quieting a host
program's stderr stays the host program's decision. Default `warn`; `NP_LLAMA_LOG_LEVEL` fills the key when the file leaves it unset and does
**not** override a file that named one (this card's first draft argued the reverse while citing §12 rule 7, which says the opposite).

Two corrections to this card, kept rather than edited away. The volume was understated: measured against the pre-F33 binary, `np search` wrote
**145,816 bytes over 1,553 lines**, not ~2,985 / ~120 — twice, identical. And the claim above that the thread sweep and the R6d benchmark "read
some of these lines" is **false**: both committed probe logs contain zero llama.cpp loader lines (`grep -c` = 0 over 190 and 82 lines), because
the sweep sends each cell's combined output to a scratch file it never commits and parses only its own tags. So the escape hatch exists for a
human diagnosing a load failure, which is a weaker justification than this card offered — and the design carries the weaker one.

Merged at `62ddcfb`; CI runs `42124160287` and `42128684675` green on all three jobs; measured ladder `off` 0 B / `error` 0 B / `warn` 424 B /
`info` 11,691 B / `debug` 145,816 B — the last byte-identical to the pre-F33 binary, which is what shows the gate adds nothing. Design:
`docs/design-f33-llama-log-level.md`.

## F34 — the fetch disk preflight cannot answer on Windows, so rule 6 silently does not happen there

Filed 2026-11-05 from run `42061786030`, job `127365138173`: `preflight_refuses_before_byte_one_when_the_disk_cannot_hold_the_pair`
failed with `expected DiskShortfall, got Interrupted { …, expected: 9223372036854775807 }`.

**This is not a first-run discovery; it is four days of unread CI.** Re-measured job by job the same day: the Windows job executed on
every push until 2026-10-29T13:11 UTC, created zero steps from 2026-10-29T14:39 through 2026-10-31T20:23, and has executed on every
push since 2026-11-01T20:08. The run at `a816b17` — the commit that *added* this test, R7b C2, pushed 2026-11-01T21:21 — already
failed it (`41351611843`, `15 passed; 1 failed`), and **the next 43 CI runs all failed the Windows job** (counted before this round's
own pushes turned them green). Every one of those merges
rested on green ubuntu rows while three documents still asserted that CI could not run. The failure is old; only the reading is new.

- **Cause:** `available_bytes` is `statvfs` under `#[cfg(unix)]` and a literal `None` elsewhere, and the preflight refuses only on
  `Some(_)`. The report already says "free space not checked on this platform" rather than claiming a pass, so the product has been
  honest; what was missing is an answer.
- **What it would take:** `GetDiskFreeSpaceExW` behind `#[cfg(windows)]` with a hand-written binding (no new dependency), the same
  climb-to-an-existing-ancestor rule the unix half uses, and the admission decision split into a pure function so all three of its
  arms are testable on every host. Design: `docs/design-f34-windows-disk-preflight.md`, including what CI proves and what it cannot.
- **Blocks:** any sentence claiming §12 rule 6 protects a Windows fetch, and keeping `Test (windows-latest)` red — which now matters,
  because the runner works and a red main is a real signal rather than an absent one.
- **Done when:** the un-gated preflight test passes on the Windows runner alongside unit tests for the answer, the climb and the refusal
  to answer, `Test (windows-latest)` is green again on `main`, and macOS claims nothing beyond "it compiles the unix `statvfs` arm; its
  number has never been read here" (F18). The first draft of this criterion asked that macOS "still claim nothing beyond cannot say",
  which would have a future reader restore a false sentence: macOS is `cfg(unix)` and has been answering all along.
- **What the round also caught** (2026-11-05, head `9de3b2d`): locally green on all 15 gate rows, 651 tests passing, and Windows
  **clippy** still failed it — `unused doc comment` on a `///` above the `extern` block. No Linux command compiles that block, so no
  local gate can see that lint; "push the branch before asking for a merge" is what retired it.

**Closed on 2026-11-05 by the full fix.** Three arms (`unix` / `windows` / neither), Windows answering from a hand-declared
`GetDiskFreeSpaceExW`, and `preflight(needed, Option<available>)` split out as a pure function so all three outcomes are testable on
any host. Merged at `b2840f9`; run `42090000404` is green on all three jobs, which ends the 43-run streak, and the Windows Test step
executes six `fetch::tests` including the four `#[cfg(windows)]` ones — a plausible answer, out-parameter order graded against the
production arm, the climb out of an absent directory, and the NUL refusal. macOS still claims only the build fact: it compiles the
`statvfs` arm, and nobody here has read its number.

**Root cause of the zero-step window this card turned on (found 2026-11-07).** The card's own measurement — "created zero steps from
2026-10-29T14:39 through 2026-10-31T20:23" — was reported as an unexplained outage, and the same shape returned on 2026-11-07. Measured from the Actions
API: this private repository spent **2,028 billed minutes in October** (stopping at the moment CI went zero-step) and **2,137 in the first 6.4 days of
November**, crossing the allowance at `2026-11-07T01:57:05Z` — seven seconds before the first zero-step failure. The account ran out of included minutes;
nothing about the tree was involved, and a zero-step job spends nothing. Numbers, method and the options for spending less are in
`docs/ops-actions-minutes.md`. **This does not reopen F34**, which was closed on its own evidence (the Windows preflight answering through
`GetDiskFreeSpaceExW`, green on the runner at `b2840f9`). What it changes is what "CI is red" means operationally: the first question is whether the jobs
have steps, and this card exists because four days of *executed-but-red* Windows runs were read as absent — a mistake in the opposite direction from
treating an unprovisioned runner as a defect.

**Amended 2026-11-08: F38's auto-fetch inherits this preflight, on every platform, and Windows answered in the field the same week.**
`np embed`/`np setup` reaching for a missing pinned artifact go through `fetch::fetch`, which is where rule 6's free-space check lives,
so the default-on download is space-checked exactly as the explicit command was — and a shortfall is a *refusal*, loud and specific,
never classified as offline (F38 §5's table: an answered error stays an answer). Observed on Windows on 2026-11-07 with the v0.2.1
cross-built binary, outside any test suite: `np models fetch` printed `free space 285.8 GB before the fetch`, transferred 295.5 MB and
verified `2188ac1d…09135`. That is the Windows arm of this rule working on a real volume, one release after it closed on runner
evidence at `b2840f9`.

## F35 — a misspelled or invented **top-level** config section is ignored, while a bad key inside `[embeddings]` is refused

Filed 2026-11-05 while measuring F32, by catching myself doing it. `Config` (`crates/np-core/src/config.rs:543-545`) is
`#[serde(default)]` with **no** `deny_unknown_fields`, unlike `EmbeddingsConfig`, `ModelsConfig`, `CatalogConfig` and
`PathsConfig`-style nested structs that all carry it — a deliberate-looking difference the file's own comments justify for the
nested ones ("a misspelled key here would silently do nothing") but never state for the top level.

Measured with the shipped binary on one config file, two variants:

```toml
[embeddings]
provider = "local-model"
typo_key = 3          # → np: configuration error: config parse error in …/config.toml: TOML parse error at line 6, column 1
```

```toml
[paths]               # no such section exists in this schema
data_dir = "/nonexistent-if-honoured-search-refuses-here"
```

The first refuses naming file and line. The second is **accepted and ignored**: the run went on to use `$XDG_DATA_HOME`, and
nothing said a word about the section the author believed they were configuring. My own scratch config carried exactly this
invented `[paths]` section for several measured runs before the mismatch showed up in `find`.

- **Why it matters more than one wasted afternoon:** `data_dir` decides *which workspace database and which artifact
  directory* a command touches. A user who writes `[paths] data_dir = …`, or `[embedding]` instead of `[embeddings]`, gets a
  working command pointing somewhere they did not choose — the failure shape §3's "name what to change" rule exists to
  prevent, reached from the other side.
- **What it would take:** decide, then say so in one place. Either (a) `deny_unknown_fields` on `Config` too, which makes every
  unknown top-level key a parse error naming file and line — consistent with the nested structs, but it also means an older
  binary refuses a config written for a newer one, which is a compatibility stance someone must own; or (b) keep leniency and
  state it in README's configuration section ("only these sections are read; anything else in the file is ignored"), which is
  honest but leaves the trap armed. This card deliberately does not pick: (a) reverses a possible forward-compatibility
  decision, and that is the human's call, not a cleanup riding along with something else.
- **Blocks:** nothing correctness-wise; it blocks any sentence claiming "a typo in `config.toml` fails loudly" without naming
  which levels of the file that covers (the thread-policy design at §84 says exactly that about `[embeddings]`, where it is true).

**Closed on 2026-11-06 — merged into `main` at `ec79e93`** (branch `f35/refuse-unknown-sections`: `c99ee6b` the change, `2e50127` the rulings recorded).
Option **(a)**, by the human's ruling, with option
(b)'s honesty explicitly declined: "only these sections are read; anything else is ignored" would document the trap instead of disarming it.
`Config`, `CatalogConfig` and `MarkerConfig` now carry `deny_unknown_fields` beside the two that always did, so one sentence covers the file:
*a key this build does not know is refused, naming the file, the line and the key as written*. Decision, cost (forward compatibility for
sections invented after this binary was built) and measurements: `docs/design-f35-unknown-keys.md`. Branch `f35/refuse-unknown-sections`,
commit `c99ee6b`; CI run 42213055546.

Two things the card got wrong, recorded rather than smoothed over:

- Its premise that `CatalogConfig` "already carries it". Measured at `main` = `64a6b73`: `grep -n 'deny_unknown_fields'` finds the attribute
  on exactly two structs — `ModelsConfig` (`config.rs:173-174`) and `EmbeddingsConfig` (`:450-451`). Nothing else refused anything, which is
  a wider gap than the card describes and makes the `[paths]` case the general one rather than an edge of it.
- Its framing as *top-level* only. The root was the hole that mattered, but `[catalog]` and `[markers]` were lenient too, so a stray key there
  vanished quietly as well; all three containers are strict now and each has a case that fails if its own attribute is removed.

Evidence to re-run after merge: `cargo test -p np-core --test config` (54 cases, 0 failed), whose new ones are
`a_stray_key_at_the_top_level_is_refused_naming_key_and_file`, `an_invented_section_is_refused_naming_the_section_and_its_line`,
`every_section_refuses_an_unknown_key`, `an_unknown_key_inside_a_section_is_the_one_named_when_the_root_has_one_too`,
`a_file_of_real_keys_still_loads_every_section`, plus `strictness_no_longer_stops_at_a_section_boundary` — which is
`the_models_strictness_is_scoped_to_the_models_section`'s fixture with the demand inverted, alongside
`an_unknown_key_outside_embeddings_still_loads`'s rewritten twin. Reversing a stance that three tests pinned is why this card asked for a human
ruling rather than taking it, and both pins are still in the file saying what they used to mean. One of our own fixtures was living off the
leniency: `an_unconfigured_fetch_says_it_is_using_the_compiled_in_origin` wrote an invented `[workspace]` section to mean "a config that says
nothing", and now writes an empty file — caught by the suite before a user met it.

On the merged tree: the 14-row gate list exits 0 (`cargo test --workspace` 669 passed / 0 failed, `GOLDEN CHECK: pass` with no golden touched,
rustdoc `--deny warnings` on all three feature sets), and CI run `42223491720` is success on all three jobs. One gate-list correction came out of this
round and belongs to every future one: the `local-model` rows need `CMAKE=$HOME/tools/cmake/bin/cmake`, or `crates/np-core/build.rs:125` panics and
three rows fail for a reason that reads like a code defect.

---

## F36 — `np search` can only rank by meaning, so the first query anyone types in a vault has no answer

**Implemented on 2026-11-08; closure withheld.** Merged into `main` at `0fa2f0b` (branch `f36/hybrid-search`: `e872c66` the change,
`7ce7335` the rulings and measurements). CI still cannot grade anything: run `42394486534` on the merged tree completed as a failure with
**zero steps on every job** — `Decide the matrix` failed before step one and the other three reported `skipped`, which is the account-out-of-
included-minutes signature recorded in `docs/ops-actions-minutes.md`, not a code defect (count `[.steps[]?] | length` before believing red).
So this card carries local gate evidence and no graded run, and says so. Design and rulings: `docs/design-m3-hybrid-search.md`.

### What was asked, by whom

Two rulings from the human on 2026-11-07, recorded rather than inferred: **hybrid becomes the default**, and **the mode is
user-configurable** across embeddings-only, FTS5-only, and both. A third question — what `np search` does on first start, when
config names a model that has never been fetched — was answered with this assistant's recommendation and adopted: **nothing dials
out**. A command that needs vectors refuses and names `fetch`/`install`; the keyword half works with no model at all.

### Why the default was the interesting part

Before this change there was no FTS table anywhere (`grep -c fts5 crates/np-core/src/db.rs` → 0), while `build.rs` already compiles
the bundled SQLite with `-DSQLITE_ENABLE_FTS5`. So `np search ~/Notes "GGML_DEFAULT_N_THREADS"` — a query whose target is one exact
string — needed ~310 MB of artifact, an embed pass, and a cosine comparison that ranks identifiers no better than prose. Hybrid by
default means **search answers on a machine that has never downloaded anything**, which is also what makes the PoC demonstrable on
Windows and macOS, where feature builds refuse to compile inference at all (`build.rs`, F18).

### What shipped

- **Migration 11** — `notes_fts`, an FTS5 **external-content** index over `notes(title, body)` with
  `tokenize = 'unicode61 remove_diacritics 2'`, backfilled once, then three triggers (`AFTER INSERT`/`DELETE`/`UPDATE OF title, body`)
  keep it current. External content is why the triggers are load-bearing rather than an optimisation: a shadow table cannot notice
  a change on its own. `UPDATE OF` deliberately omits `rel`, so a move changes no word and the row survives exactly as `headings`
  and `links` do. The delete trigger passes the **old** text, which is what FTS5 needs to find terms to drop.
- **Three modes, named once** in `search::SearchMode` — `hybrid` (default), `semantic`, `keyword` — with one `FromStr`, one
  `label()`, one serde rename feeding config, and a clap `ValueEnum` bridged by `SearchModeFlag::to_core`. F28's split-refusal lesson:
  one list of names behind every door, so a fourth spelling cannot enter through argv and not through TOML.
- **Fusion by reciprocal rank fusion**, `k = 60`, un-tuned and stated as un-tuned: there is no labelled set (F31), so inventing a
  constant this repository cannot measure would be the worse invention. Keyword-only mode orders by SQLite's own `bm25()` negated, so
  "higher first" holds in every mode.
- **Query text is data, not grammar.** `match_expression` splits on non-word characters, doubles inner quotes, wraps each token as an
  FTS5 phrase and joins with `AND`, capped at 32 terms. `"kafka`, `-foo OR (bar): "baz*"` and `":()*-` are all answerable queries;
  none reaches SQLite's parser as syntax.
- **Two evidence channels.** `Hit` gained `cosine`, `bm25`, `semantic_rank`, `keyword_rank`, `snippet`; `--json`'s document became
  `{hits, mode, note, vectors}` and each hit carries the same **eleven** keys in every mode, null where a half did not run. The text
  channel prints `[cos … bm25 …]` beside the score and a `snippet()` excerpt, and small magnitudes print as `1.97e-6` rather than
  `-0.0000`.
- **`vectors_present` is a question about the database, not about config.** Hybrid skips the vector read entirely when the workspace
  holds none; `semantic` still runs it, because "no vectors at all" (empty answer) and "vectors under another model id" (refusal) are
  different states and only asking tells them apart.

### What this changed for cards that were already closed

- **F32's refusals survive, but they have to be asked for.** Four pinned CLI cases (`search_with_embedding_disabled_refuses_and_names_the_setting`,
  `search_with_no_row_names_the_missing_row_and_says_what_to_run`, `searching_without_the_runtime_names_the_model_that_stayed_owed`,
  `search_with_the_artifact_file_missing_says_the_query_cannot_be_embedded`) now pass `--mode semantic`. Under the new default those four
  workspaces answer from keywords and exit 0, which is the point of the change — so each case asks for the one list that genuinely cannot run.
  The sentences themselves are untouched, and a fifth case (`config_can_owe_the_semantic_half_alone_and_says_so`) reaches the same refusal
  through `[search] mode = "semantic"` instead of argv.
- **The schema pin grew by five names**: `notes_fts` plus FTS5's four shadow tables (`_config`, `_data`, `_docsize`, `_idx`), pinned by name so
  they read as one index's implementation rather than four forgotten tables.
- **Rewinding past migration 11 is now part of three test files.** A fixture that stamps `schema_version` back to 4 must delete what later
  entries created — the rule `tests/db.rs` already followed for migrations 6, 7 and 9. Six rewind sites across `tests/db.rs`, `tests/scan.rs`
  and `np-cli/tests/cli.rs` now drop the three triggers and `notes_fts`; without that, the next open fails on `table notes_fts already exists`.
  The const is duplicated in those three files because integration tests cannot share a private helper; each copy names the other two and
  points at `db::MIGRATIONS` as the thing to keep up with. **A fourth migration will need the same edit**, and that is the debt this round
  accepted knowingly.

### Evidence

New cases: `crates/np-core/tests/search_modes.rs` (9) — `a_keyword_query_finds_an_exact_identifier_with_no_model_installed`,
`diacritics_fold_and_word_endings_do_not`, `the_keyword_index_follows_an_edit_and_survives_a_move`,
`a_hit_found_by_both_lists_outranks_one_found_by_either_alone`, `keyword_mode_returns_no_images_and_hybrid_still_does`,
`no_vectors_is_a_state_and_the_report_says_which_half_answered`, `mode_names_parse_refuse_and_round_trip`,
`fts5_operators_are_words_not_grammar`, `a_query_beyond_the_term_cap_searches_its_first_terms`.
Config (2): `an_absent_search_section_means_hybrid`, `an_invented_search_mode_is_refused_and_names_the_three`, plus `[search]` rows added to
`every_section_refuses_an_unknown_key` and `a_file_of_real_keys_still_loads_every_section`.
CLI (5): `search_without_a_model_answers_from_keywords_and_says_the_half_that_ran`,
`search_json_names_the_mode_that_ran_and_the_half_that_was_missing`, `keyword_mode_answers_where_the_semantic_half_would_refuse`,
`config_can_owe_the_semantic_half_alone_and_says_so`, `an_invented_mode_refuses_with_claps_exit_two`.

Three production mutations, each applied and diffed before its result was believed:

| Mutation | Killed by |
| --- | --- |
| hybrid overwrites the fused score with the keyword rank (`score += rrf` → `score = rrf`) | `a_hit_found_by_both_lists_outranks_one_found_by_either_alone` |
| the raw query reaches `MATCH` as FTS5 grammar | `fts5_operators_are_words_not_grammar` (2 cases fail) |
| the update trigger deletes the old row and never adds the new one | `the_keyword_index_follows_an_edit_and_survives_a_move`, on "and the new word is findable in the same row" |

Gates on the working tree: **all 17 rows exit 0** (`fmt`, four clippy rows across default / `local-model` / `local-model,vision` /
`--no-default-features`, workspace tests, three feature test rows, hermetic config row, rustdoc `--deny warnings` on three feature sets,
`check_cfg_dead_code.py`, `check_attr_displacement.py`, `verify_catalog_goldens.py` → `GOLDEN CHECK: pass`, no golden touched).
Artifact-enabled: `cargo test -p np-cli --features local-model` with `NP_TEST_MODEL_PATH` set is **40 passed / 0 failed**, which includes
`searching_a_real_index_answers_in_both_channels` — re-pinned to the four top-level keys and eleven hit keys, and to `note: null`,
`vectors: true` on a healthy run.

Cost, measured the same day (full table in §6 of the design note): on a 500-note vault holding 556,210 B of prose, migration 11 adds
**217,088 bytes — 39% of the prose it indexes, 12.2% of the file**, and `notes_fts_data` is smaller than the `chunks` table beside it. A full
re-parse of those 500 notes costs **431 ms without the three triggers and 515 ms with them (+84 ms, +19.5%, ≈0.17 ms per note)**; an ordinary
scan re-parses only what changed. The measurement also corrected another card: `docs/measurement-f30-vector-surface.md` asserted that splitting
per-table bytes needed `dbstat`, "which this rusqlite build does not enable" — the bundled SQLite is built with `SQLITE_ENABLE_DBSTAT_VTAB` and
this round's numbers came out of `dbstat`. The claim is struck through in place, because a measurement note wrong about whether its instrument
exists is worth leaving visible.

Two real runs are quoted in `README.md` rather than paraphrased: a keyword-only JSON document from a vault with no model
(`mode: "keyword"`, `note: null`, `vectors: false`, cosine null), and a hybrid text listing from the same vault after `np embed` on a
`--features local-model` build, where the note both lists ranked first prints `[cos 0.8143 bm25 -1.97e-6]` and the vector-only hit prints
`[cos 0.7090]` with no keyword evidence at all.

### What this card does not claim

- **No retrieval-quality claim.** Hybrid is a recall argument, not a precision one: nothing here says hybrid finds *better* results than
  `semantic`, because that needs the labelled set F31 still owes. The fusion constant is un-tuned for the same reason.
- **No Windows or macOS measurement.** "search now works where inference cannot be built" is a statement about which code paths need a
  runtime, not a run on those platforms (F18).
- **`bm25()` is SQLite's number, unremapped** — negative, better-is-more-negative, magnitude meaningless on a two-note vault. `score`
  carries its negation so ordering reads consistently; nobody should read either as a probability.
- **No stemming, and no `OR` mode.** `runner` does not find `runners` (measured, and asserted); a query whose words never co-occur returns
  nothing rather than some. Both are choices (§8 of the design note) that a future card may reverse with data this one does not have.

---

## F37 — on Windows the index lands loose in `%LOCALAPPDATA%`, not in a `note-processor` folder

**Opened 2026-11-07 by the first interactive Windows run. Ruled on 2026-11-08: option A — nest under `%LOCALAPPDATA%\note-processor` with a
one-release read-fallback. Implementation is owed; the ruling is not.**

Implementation must ship as one change, because half of it is worse than none: (1) `default_data_dir()` appends `note-processor` on Windows;
(2) on open, if the new location holds no registry and the old one does, **read from the old one** and print where it went, so a vault never
reports `added N` because its index moved — that false report is the worst possible first experience and is the entire reason for the fallback;
(3) README's path table and every `%LOCALAPPDATA%` mention follow; (4) the Windows-only arms are exercised through an injectable seam so Linux CI
runs both locations, since CI cannot create a real `%LOCALAPPDATA%` and this host cannot compile the Windows arm (`ring`, `libsqlite3-sys` need MSVC
`lib.exe`). Verification lands on the operator's Windows box — `run-stage5-7.ps1` already captures the current layout into its log, which is the
before-picture this migration will be checked against.

### The fact

`default_data_dir()` (`crates/np-core/src/config.rs:1059-1062`) returns `%LOCALAPPDATA%` **untouched** on Windows —
`dir_from(env_path("LOCALAPPDATA"), None, None)` appends nothing — while `config_file_path()` (line 960) does append
`note-processor`. So a Windows user ends up with:

```text
C:\Users\<user>\AppData\Local\workspaces\e51b164fc7471791.sqlite     the index for one vault (measured: 167,936 B)
C:\Users\<user>\AppData\Local\workspaces.sqlite                       the workspace registry (measured: 12,288 B)
C:\Users\<user>\AppData\Roaming\note-processor\config.toml           config, correctly nested
```

Observed directly on Windows 11: `dir "$env:LOCALAPPDATA\note-processor\workspaces"` returned `PathNotFound` and the operator concluded the tool
had written nothing. It had — two directories earlier. README is consistent with the binary (`<data_dir>` is `%LOCALAPPDATA%`), so this is not a
documentation slip; it is the layout.

### Why it is worth changing

* **Two generically-named entries** (`workspaces`, `workspaces.sqlite`) sit in the directory every application shares. Anything else may create
  either name; nothing here can tell its own files from theirs, and the registry file has no marker a human would recognise as ours.
* **The two halves disagree.** Config nests under `%APPDATA%\note-processor`, data does not nest at all — so "where does `np` keep things" has two
  answers on one platform, and the guide-writing mistake above was downstream of exactly that.
* Linux and macOS land in `~/.local/share`, where an unqualified `workspaces/` is far less exposed. So this is a Windows-only shape, which is also why
  six CI rows and every measurement in this repository never noticed it.

### Options, with their cost

| | Change | Cost | Notes |
| --- | --- | --- | --- |
| **A** | `default_data_dir()` returns `%LOCALAPPDATA%\note-processor` on Windows | breaks any existing Windows index: the DB is keyed by path hash, so a moved directory reads as an empty workspace until it is re-scanned. Needs a one-time relocation (or a read-fallback to the old location for one release) **and** a README rewrite | cleanest end state; matches config; the fallback keeps it from being a data-loss trap |
| **B** | Leave the layout, name the files distinctly (`note-processor-workspaces.sqlite`, `note-processor/`) | still a migration for existing files; two names in a shared directory is still not containment | half-measure |
| **C** | Document it loudly and change nothing | free | leaves the confusion that produced this card, and Windows is about to get more users from the F36 keyword-search story |

**Recommendation: A with a read-fallback for one release.** The population affected today is tiny — the binaries that could produce those paths are
weeks old, and `v0.1.0`'s shipped asset had no `scan`-adjacent persistence to speak of beyond that same layout — so this is cheap now and expensive
after Windows users arrive. Do not do it silently: a workspace that suddenly reports `added N` because its index moved elsewhere is the worst possible
first experience, which is what the fallback exists to prevent.

### Implemented 2026-11-09 (this commit) — option A, as ruled, in one change

**(1) The nesting.** `default_data_dir()` on Windows now returns `%LOCALAPPDATA%\note-processor`. Nothing else moved: Unix keeps
`$XDG_DATA_HOME` / `$HOME/.local/share` exactly as it was, which is asserted rather than assumed (`the_unix_shape_is_unchanged_by_the_seam`)
because "we fixed Windows" turning into "and relocated everybody else's index" is the obvious way to make this worse.

**(2) The fallback.** `resolve_data_dir(explicit, platform, lookup)` answers both halves of the layout question in one **pure** function:
does the config file's `data_dir` win (it always does — an instruction, not a place we moved), and if we are on the OS default, is our
index still sitting at the pre-nesting location? When it is, this release reads and writes *there* and prints one line on stderr:

```text
np: reading your index from C:\Users\ana\AppData\Local — where it was written before this release nested its data folder. nothing to do: it stays there until a later release moves it
```

Reading and writing the old place as a unit is deliberate: a registry in one directory and per-workspace databases in another is a worse
bug than the layout this card filed. Either half counts as "ours" out there — the registry file, or the `workspaces/` directory beside it,
since an interrupted first run can leave one without the other.

**(3) The seam, and why it is a value rather than a `cfg`.** CI is Linux; this development host cannot compile the Windows arm at all
(`ring`, `libsqlite3-sys` need MSVC `lib.exe`). A layout fix whose Windows half never executes is a layout fix nobody verified — which is
precisely how the shape described above survived six CI rows. So `DataDirPlatform` is chosen by `cfg` in production and *passed in*
everywhere else, and `tests/config.rs` resolves both layouts on one machine. The test harness's own expectation
(`TestEnv::data_dir_of`) was made platform-aware rather than derived from the code under test: a test that computes its expectation from
the implementation cannot fail.

**(4) README** follows — the `<data_dir>` paragraph now names the nested path and explains the fallback, and the same edit corrected a
sentence **F38** had quietly invalidated: `np models fetch` is no longer "the only command in the program that opens a socket".

**Evidence.** `cargo test -p np-core --test config` **68 / 0**; workspace suite green. Mutations planted, diffed, and killed by named
tests: fallback disabled → `an_index_left_where_it_was_before_f37_is_still_read_from_there`,
`an_empty_legacy_location_vetoes_nothing_and_a_half_written_one_still_counts`, `an_operator_chosen_directory_wins_even_when_the_legacy_index_is_sitting_there`;
nesting removed on the Windows shape → `the_windows_data_dir_nests_the_program_folder_like_config_already_did` (+ the same third test);
explicit directory ignored → `an_explicit_data_dir_in_the_file_is_never_second_guessed` and that third test again. The full gate list ran
against this tree; the verdict is in the commit message beside it, since a card that cites its own green before the log exists is how F34
got read wrong for four days.

**Field-run result, round five (2026-11-08, `v0.2.2_test_windows.zip`) — the layout works, and it found a bug in its own announcement.**

| what the log shows | reading |
| --- | --- |
| every new workspace database landed in `%LOCALAPPDATA%\workspaces` (43 → 45 files), nested folder absent | the fallback chose the old directory and **read and wrote it as a unit**, which is the design |
| `scan` on an existing vault: `unchanged 1`, then `added 0` on the repeat | no false `added N` about notes already indexed — the lie this bridge exists to prevent did not happen |
| **no** `reading your index from …` line anywhere, in a log whose every command merges stderr into the capture | **the announcement never fired.** A silent migration instead of a stated one |

The fallback was never wrong; the question asked next to it was. `Config::load` decided whether the operator had *declared* a directory by
testing if merged `cfg.data_dir` was non-empty — and [`Config::default`] resolves the OS default, fallback included, so that field is never
empty and every Windows run looked like an instruction. Declaring it explicit dropped `legacy_data_dir`, the only trigger for the note. On real
Windows the effective path stayed correct because the default had already resolved to the same place, which is why six CI rows and 68 local tests
missed it: **nothing on a Unix host can distinguish the two states.** Fixed by asking the file (`declared_data_dir`, a lenient one-field probe)
and pushed through `load_with_platform`, F37's own "a value, not a `cfg`" argument carried up to the entry point — so Linux now grades the Windows
shape end to end. Restoring the old question fails the new test with the field shape in its diff. Shipped in **v0.2.3**.

Two notes for whoever closes this card. First, the operator's script asserts `reading your index from` appears *if* a run fell back; on their
machine it could not have, so that line was correctly reporting a real gap rather than a script bug — the difference between this and round four's
false CHECKs is that the assertion was aimed at something that should have happened. Second: **the fallback still has never been exercised against
a nested-first install** (a machine where the nested folder holds the registry and the old one also exists). Stage 9b manufactures that shape; it
skipped itself on their box because a real legacy directory is present, which is the right call but leaves the case open.

**Closed on evidence, pending CI only — round six (2026-11-08, `test_win_0.2.3.zip`).** v0.2.3 on the operator's machine printed the sentence
that v0.2.2 owed:

```text
np: reading your index from C:\Users\chris\AppData\Local — where it was written before this release nested its data folder. nothing to do: …
```

…with `added 0` on a vault whose index predates the nesting, and **zero `CHECK` lines in the whole run log** for the first time in six rounds.

The operator then wrote their own probe (`probe-f37-nested.ps1`, `probe-stream-split.ps1`) that reached the case my Stage 9b must skip: it points
`LOCALAPPDATA` at a scratch directory, so nothing touches the real 45-index root, and builds each layout from nothing.

| state built in the sandbox | what `np 0.2.3` did | why it matters |
| --- | --- | --- |
| **A** — empty data root, first scan | nested under `note-processor\workspaces`, registry beside it, and **no fallback line** | a fresh install gets the new layout *and* is not falsely told its index was found in an old place |
| **B** — that same index moved to the pre-F37 shape | reads legacy, announces it, `added 0`, **no second index appeared nested** | the false-`added N` report this bridge exists to prevent, prevented; and no split-brain |
| **C** — nested folder *and* legacy index both present | still reads legacy, still announces | the rule is "nested wins when it holds **our** registry", not "when it exists" — the distinction a directory check would get wrong |
| **D** — same state C, captured with `Start-Process -RedirectStandardOutput/-RedirectStandardError` | notice on **stderr only**; `search --json` and `setup --json` stdout parsed as one document on its own handle | §"stdout carries exactly one JSON document" proven on Windows under real handle redirection — my `cmd /c 2>&1` capture merges the channels and structurally could not show this |

Their probe is folded into the operator script as **Stage 9c** (sandboxed A/B/C) and **Stage 10** (stream separation), so a future run covers
nested-first without anyone hand-writing it — and Stage 9b's refusal to move live data stays, because that refusal was correct.

**What this card still owes.**

+ **Windows, in the field.** Stage 9 of the operator script (sha256 `db75c546…494345`, uploaded on v0.2.1) scans a fresh vault twice and
  prints which root holds `*.sqlite`, then requires the second scan to report `added      0`. That line is the test: it means the run found
  the index it had just written, wherever it happened to be. Written on Linux, never executed by PowerShell.
+ **Removing the fallback is not a deletion.** One release from now, deleting `legacy_data_dir_override` would hand every upgraded machine
  exactly the false `added N` report this bridge exists to prevent. The removal has to *ship with* a relocation (copy the registry and
  `workspaces/` into the nested folder by rename, verify, then stop reading the old path) or an equally honest refusal. Recorded here so
  whoever deletes it is not surprised by their own test suite going green while users see `added 214`.
+ CI grading, in common with F36/F38/F40: no workflow run has had steps since the included minutes ran out.

## F38 — `np embed` should fetch the pinned model itself, so an application gets vectors from one call

**Opened 2026-11-07 as a question, ruled 2026-11-08 by the human partner: default ON.** The reason is in the ruling's own words — the
consumer of this program is increasingly another program, and what it wants is *for vectors to simply exist*, not a sequence of three
commands whose exit codes it has to interpret. Design `docs/design-f38-auto-fetch.md`, plan
`docs/superpowers/plans/2026-11-08-auto-fetch-and-setup.md`; this card is the record of what shipped and what it still owes.

**Default-on is defensible only because of what can be fetched, not because of who asked.** One id (`embeddinggemma-2`), from one
origin that is compiled in unless `[models] base_url` replaces it, verified against the pinned digest before a single byte is put in
place. An id this build does not pin is refused **before a transport is chosen** — no URL, mirror or config string can name a download
target — and a binary built without `local-model`, or without `fetch`, cannot reach the trigger at all: those are compile-time gates,
so "the default distribution does not dial out" is a build fact rather than a behaviour. Three switches refuse it (`--no-auto-fetch`,
`[models] auto_fetch = false`, `NP_NO_AUTO_FETCH=1` — the environment outranks the file in that one direction, so a daemon can be
silenced without editing anything), and every transfer announces id, members, byte count and origin *kind* before byte one.

### What shipped, in which commit

| piece | commit | what it is |
| --- | --- | --- |
| `[models] auto_fetch` (default true) + kill switch | `9f00d7f` | file key, env veto consulted per call, argv may open a door that only the environment bolts |
| offline classification | `200f0f6` | `ClassKind` + `FetchError::is_offline()`; **only** a transport failure counts — 404/403/5xx/redirect/digest stay loud refusals, and TLS is deliberately *not* offline so a captive portal cannot suppress vectors (my call, overturnable) |
| the trigger | `783e173`, cfg fix `e72a582` | inside provider resolution, before any transaction; pinned id + allowed + artifact or row missing; `np search` passes `false` unconditionally |
| the machine contract | `aa204e0` | `fetched` / `offline` in every embed document; offline became a **state** (`FetchStep::Offline` → … → `EmbedOutcome::Offline`) rather than an error string, and arm (vi) moved to the top of `run` after measurement showed it reporting `configuration disabled` for an outage |
| `np setup` | `2c56b9a` | index → fetch → embed as one command; offline exits 0 with `vectors:false, offline:true`; TTY `[y/N]` courtesy that can only veto; reuses `embed_report`, so there is one implementation of the embed half |
| locks and the amendment trail | this commit | `NP_NO_AUTO_FETCH=1` for CI and `gates.sh`; README first-run paragraph; §7 of `docs/design-m3-hybrid-search.md` and §12 rule 1 of `docs/design-m2-local-embeddings.md` amended beside the rulings they change |

### Evidence

+ **Gates: 19 rows, all exit 0, `NOT COVERAGE` 0**, with both real artifacts exported — `/home/agent/tmp/gates-f38t6b`, tree
  `2c56b9a` whose only difference from the committed tree here is documentation and CI environment. No row was skipped.
+ Tests that carry the claims: `embed_fetches_a_missing_pinned_model_and_refuses_honestly_when_there_is_no_connection`,
  `the_kill_switch_leaves_the_old_refusal_word_for_word`, `an_unpinned_id_is_refused_before_any_transport_is_chosen`,
  `a_build_without_the_runtime_does_not_fetch_a_model_it_cannot_load`, `argv_and_the_environment_each_outrank_the_file_in_one_direction`,
  `embed_json_says_it_was_offline_and_that_nothing_was_fetched`, `only_a_transport_failure_can_be_offline`,
  `offline_is_its_own_arm_and_does_no_work`, `setup_offline_indexes_anyway_and_says_vectors_were_skipped`,
  `setup_with_fetching_switched_off_indexes_and_refuses_without_blaming_the_network`, `setup_with_an_installed_artifact_ends_with_vectors`
  (real inference), and in `models_cli`: `embed_dials_out_for_a_missing_pinned_model_and_keeps_an_answer_an_answer`,
  `an_unpinned_id_never_opens_a_connection_during_embed`, `search_never_dials_out_even_when_a_fetch_would_be_permitted` — the last three
  read a **stub origin's request log**, so "it dialled out", "it did not", and "an answer is not an outage" are socket facts, not wording.
+ Mutations planted and diffed, each failing a named test: offline routed back to an error; search's auto-fetch flag flipped to `true`
  (which *survived* the first version of its test — hybrid mode never resolves a provider on a vector-less workspace — and fails the
  corrected one run with `--mode semantic`); setup returning Err when offline; `vectors` derived from the scan half.
+ **One real-origin run, outside the gate list** (2026-11-09): fresh vault → `np setup --json` fetched 309,855,456 B in ~19 s, verified
  `2188ac1d…09135`, embedded, exited 0 with `fetched.origin = "compiled-in-pin"` and no `http` substring anywhere in the document.
  Numbers and the exact document are in `docs/design-f38-auto-fetch.md` §5b.

### What it still owes

+ **`--help` told the opposite of the ruling, and now has a test (2026-11-09).** `np embed --help` said fetching "is off unless you
  switch it on" — true of the option-A draft, false of what shipped after *"I would [have] the default as true"* — and `np models fetch --help`
  still called itself "the only command in this program that dials out". Both were fixed by reading the released binary's own `--help`, not the
  code: the code was right. `embed_and_models_fetch_help_match_the_behaviour_f38_shipped` now pins the two off-switch names, the phrase "on by
  default", and the absence of the pre-ruling sentence; it asserts over whitespace-flattened text because clap wraps at terminal width and a test
  that breaks on a narrow console gets deleted rather than fixed. Mutated by restoring the old sentence: FAILED as it must. The design doc's title
  ("an opt-in automatic fetch") carries a dated amendment for the same reason.
+ **What the release build shape actually prints, measured 2026-11-09 on the packaged v0.2.2 Linux binary** — recorded here because it is what the
  Windows asset will be asked to reproduce, and because two of these three sentences are not the ones you would guess from the code:
  | state | `np embed` says | exit | `--json` |
  | --- | --- | --- | --- |
  | fresh vault, no config at all | `configuration disabled: embeddings.provider = "none"` | 1 | `outcome.configuration-disabled{key:"embeddings.provider",id:"none"}`, `errors 0`, `offline:false`, `fetched:null` |
  | fresh vault, `[embeddings] provider="local-model" model="embeddinggemma-2"`, nothing installed | `configuration disabled: embeddings.model = "embeddinggemma-2"` — **not** `runtime not built`, and **no fetch is even attempted**: the trigger is compiled out with `local-model`, so arm (ii) answers first | 1 | same shape, `key:"embeddings.model"` |
  | after `np models install`/`fetch` puts real bytes in place | `runtime not built: … install one built with \`--features local-model\`, or search by keyword (\`np search --mode keyword\`) — \`embeddinggemma-2\` stays owed` | 1 | `outcome.runtime-not-built{id}`, `errors 0` (**F40**'s contract, holding) |
  So on a release asset the offline path **cannot be observed at all**, and any test that expects to see it there is testing a build shape that does
  not exist. The Windows script's Stage 8 was rewritten against this table the same day.

+ **CI grading.** No workflow run has had steps since the included minutes ran out at `2026-11-07T01:57:05Z`; the last CI-graded tree is
  still `5e368ac`. Local gates are not closure (`docs/ops-actions-minutes.md`), so **this card stays open** and F36/F40's closures stay
  withheld for the same reason.
+ **Windows, in the field.** Stage 8 of the operator script (sha256 `feadc2c7…508a7c`, uploaded on v0.2.1) asks the four questions this
  card can only ask on Linux — including that a release asset must *not* speak of fetching at all. Written here, never executed by
  PowerShell: if a line throws rather than printing `CHECK`, the bug is in my script.
+ **F41**, raised by F38's own kill-switch path: refusing to fetch leaves a fresh workspace told `configuration disabled`, with no remedy
  line. Ruling owed there.
+ ~~A shape test for `fetched`'s JSON keys~~ — **done the same day** (`dc52776`): `the_fetched_record_spells_its_keys_as_the_contract_does`
  serialises the record and pins both key sets, plus the three spellings a host could arrive under. The member field is
  `digest-verified`, kebab-case like every other key in the report; §5b of the design doc records that §5's sketch said otherwise.

## F39 — an application-control policy on the operator's Windows box refuses to run anything cargo just built, and "build it from source" stops being an option

**Filed 2026-11-08 from the operator's own transcript** (`error.md`, verbatim below). `cargo build --release --locked -p np-cli` on
Windows 11 stops at *build-script execution*, twice over, on two unrelated crates:

```
error: failed to run custom build command for `icu_normalizer_data v2.4.0`
Caused by:  could not execute process `…\target\release\build\icu_normalizer_data-f1572f0d67db13fc\build-script-build` (never executed)
Caused by:  Eine Anwendungssteuerungsrichtlinie hat diese Datei blockiert. (os error 4551)
error: failed to run custom build command for `thiserror v2.0.21`      … same refusal, same shape
```

German message = *"An application control policy has blocked this file."*

**This is not a code defect and must not be chased as one.** There is no rustc error, no linker error, no `build.rs` panic and no
`np` source in the path: cargo compiled fine and then asked Windows to *run* a freshly written `.exe`, and Windows said no. Two
unrelated crates fail identically, and the blocked files were created on that disk seconds earlier — nothing was downloaded, so this is
not Mark-of-the-Web either. A zero-line compile failure is not a defect; check what the OS was asked to do before believing red.

**Why it belongs in this repository at all:** while hosted CI minutes are exhausted, our entire cross-platform story is *"build from
source"* — README tells Windows users `cargo install --git …`. On a machine with this policy that sentence is not an instruction, it is a
dead end, and the only remaining supported path to `np` on Windows is a **prebuilt release asset we cannot produce** (no CI minutes, no
second machine). F39 is therefore what turns a Windows asset from a nicety into the only door: roughly one user in the field cannot use
the alternative.

| cause | how to tell it apart | what it allows |
| --- | --- | --- |
| **Smart App Control** on | `Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy' -Name VerifiedAndReputablePolicyState` → `2` | nothing; SAC honours no exclusions and Microsoft's rule is that it cannot be turned off once enabled without reinstalling |
| **WDAC / Device Guard** policy in force | `Get-CimInstance Win32_DeviceGuard -Namespace root\Microsoft\Windows\DeviceGuard` → non-zero `UsermodeCodeIntegrityPolicyEnforcementStatus`; `citool.exe /lp` (admin) prints the rules | an signed-script path, or a policy exception from whoever owns the policy |
| **Defender ASR rule** `d4f940ab-401b-4efc-aadc-ad5f3ff50688` ("block executables unless they meet a prevalence, age, or trusted-list criterion") | `Get-MpPreference \| Select-Object AttackSurfaceReductionRules_Ids, AttackSurfaceReductionRules_Actions` — this is the classic compiler-breaker on a home PC | an exclusion for the dev folder: `Add-MpPreference -AttackSurfaceReductionOnlyExclusions "D:\test_project\note-processor-main\target"` (admin) |
| **Third-party AV app control** (ESET/Bitdefender/Kaspersky "enhanced"/"aggressive" tiers) | which product owns `Get-MpComputerStatus`'s reporting, and its own block log in the same minute as the failure | a per-folder allowance inside that product; Defender exclusions will not satisfy it |
| path- or volume-scoped policy | build a throwaway crate on `C:` (`cargo new hello; cargo build`) — if that succeeds, the trigger is the `D:` location, not the toolchain | move the workspace to the unpoliced volume |

**Evidence still obtainable while the build is blocked.** A binary built *before* the policy tightened still runs: Stages 4b, 6 and 7 of
the Windows guide need no compiler, only `np.exe`, so `run-stage5-7.ps1 -SkipTests` keeps the config-strictness, fetch/embed/search and
rename-identity evidence flowing. Only Stage 5 (`cargo test --workspace`) genuinely needs a working toolchain — check
`Test-Path D:\test_project\note-processor-main\target\release\np.exe` before concluding the machine is useless for testing.

**Done when** either (a) the build is graded on a Windows box and this card names which policy owned the refusal — in which case README's
source-build instruction gains that caveat and the fix; or (b) we ship a Windows asset, at which point the card closes as "the source path
is not universally available, so the binary is the supported one". Either way README gets an honest line about app-control-blocked machines
rather than silently assuming everyone can compile.

**Answered in part on 2026-11-08, by the operator's own hand.** The decisive experiment was not a build — it was whether a *downloaded*
binary runs where a freshly compiled one cannot. `np.exe` from the v0.2.0 release asset (cross-built on Linux, unsigned, sha256
`d36f3e3b…44c270`, downloaded over HTTPS and `Unblock-File`d) printed its version on the same machine that refused to run cargo's build
scripts. So the policy governs **compiler-spawned children**, not every unknown executable, and a prebuilt binary is a real door rather
than a consolation prize: no certificate purchase, no exclusion, no reinstall, nothing changed in Windows.

What that settles for distribution, while CI minutes are gone:

* the cross-built `-gnu` asset (`docs/ops-cross-build-windows-gnu.md`) is a supported way to put `np` on a locked-down Windows box —
  with its honest limits stated beside it: no inference feature, unsigned, and carrying no gate result;
* Stage 5 (`cargo test --workspace`) stays impossible on that machine, so any claim about the suite "on Windows" still needs CI or another
  host. A binary answers questions about runtime behaviour; it does not grade a tree;
* **which product owns the policy is still unnamed** — Defender's ASR rule `d4f940ab…`, third-party AV app control and WDAC all remain
  candidates, and the ladder above is what names it. That is worth ten minutes on a quiet afternoon because it decides whether *building*
  becomes possible there too, and until someone runs it this card stays open.

## F40 — with the artifact present and no runtime compiled in, `np embed` reports a *report-integrity* failure instead of saying "this build cannot embed"

**Filed 2026-11-08 from the first Windows run with a real artifact** (`out.zip`, log `run-20261107-225434.log`), reproduced on Linux the
same evening — so this is not a Windows oddity, it is what `np` does.

The operator fetched `embeddinggemma-2` (295.5 MB, digest verified) and ran `np embed` with the release asset, which is compiled without
the inference feature:

```
model      embeddinggemma-2
outcome    owed and not produced
detail     1 chunk(s) or batch(es) failed: the local embedding runtime is not built yet: model `embeddinggemma-2` … produced nothing
seen       1        current 0   embedded 0   errors 1        exit=1
```

The same binary with the artifact **absent** takes a different, calmer path — `outcome configuration disabled`, `errors 0`, refusing before
any work. So the difference is the presence of the artifact, and the worse branch is the one a real user meets: they installed a model
correctly, and are told the program owed vectors and failed to produce them.

**Why the wording is a defect and not cosmetics.** [`EmbedOutcome::OwedAndNotProduced`](../crates/np-core/src/embed_run.rs) is defined in
`embed_run.rs` as §6's *report-integrity* state: the accounting says work was owed and nothing arrived, which points at our chunking or our
stamping. Here it points at a compile-time feature. Three consequences:

* an operator reads "possible data problem in my vault" when the truth is "wrong binary — install one with `local-model`, or search by
  keyword";
* `errors 1` counts as a failure a monitor would page on, and nothing about this run can be fixed by retrying;
* it **does work before refusing** (`seen 1`), so the refusal arrives after scanning and opening the database rather than before them.

Nothing was written — `current 0 embedded 0`, no rows stamped — so there is no corruption to clean up. This is about the answer, not the state.

**The fix has one shape:** provider resolution answers "can this process embed at all?" **before** any batch runs, and the answer
`RuntimeNotBuilt` carries its own outcome — same exit 1, sentence naming the feature and the two ways forward — with both artifact states
taking that path identically. That is also a prerequisite for **F38**: `docs/design-f38-auto-fetch.md` §5 promises auto-fetch will *not* fire
on `RuntimeNotBuilt`, and today there is no such state to test against, only a per-batch error string. So F40 lands before or with F38's
Task 3, not after it.

**Done when:** the artifact-present/no-runtime case exits 1 naming the feature without the words "owed" or "errors", `seen` stays 0 because
nothing was scanned for work, and a test pins that both artifact states produce the same outcome — asserter: two cases, one outcome enum, no
string-matching on a per-batch error.

---

## F18 — round three (2026-11-08): Windows graded four things for the first time, and one of this card's claims is false

Same bundle (`out.zip`, two logs; binary `np.exe` sha256 `d36f3e3b…44c270`, the cross-built release asset — **no `.git`, so the tree is
unnamed**, which is now a standing caveat on every row below).

**Passed, first time ever on Windows:** `[search] modee` refused naming file, line 2, column 1 and the key (**F35**); a valid
`[search] mode = "keyword"` obeyed with no caveat; hybrid `--json` carried `cosine: null`, `bm25`, `vectors: false` and the F36 note at exit 0.

**F34's Windows arm answered in the field for the first time:** `np models fetch` printed `free space 285.8 GB before the fetch`, fetched
295.5 MB from the compiled-in origin, verified sha256 `2188ac1d…09135`, and `models list` then reported `present dim 768`. Nothing about
rule 6 is theoretical on that platform any more.

**This card's §5 claim is falsified.** It asserted the Windows identity fold is ASCII-only, so a rename `Ämli.md → ämli.md` would not be
followed. Measured: `moved 1`, and the regenerated `INDEX.md` lists `ämli.md`. The ASCII-only fold is still real — `path_keys.rs:112`
says `u8::to_ascii_lowercase` "is the whole rule: it changes only 0x41..=0x5A" — but it is not the whole *story*: the rename pairing caught
the unmatched path and reported a move. So the honest statement is "the identity key does not fold non-ASCII case, and the move-detection
heuristic rescues that case in practice", which is a materially weaker claim than the one this card made — and better news for users.

**And the ASCII case behaves differently, correctly.** `Note.md → note.md` reported `unchanged 1`, not `moved 1`: on a case-insensitive
volume the identity keys are equal after the ASCII fold, so nothing moved as far as identity is concerned. The `INDEX.md` in the same bundle
lists `note.md` — the disk spelling — so no stale display path was left behind. My own test script asserted `moved 1` here and printed a
CHECK; that expectation was wrong for Windows and has been corrected, which is worth stating because it is the second round in a row where a
script expectation, not the binary, produced the red line.

**Still unproven:** byte identity under unpaired-surrogate collisions, and the full interactive test suite (Stage 5) — that needs a working
toolchain, which this machine does not have (**F39**). Also new: `%LOCALAPPDATA%\workspaces` held **40** `*.sqlite` files at 167,936 B after
two runs, because every fresh vault path mints a workspace that nothing ever forgets. Not F18's subject; recorded here so whoever picks up
workspace lifecycle does not have to rediscover it, and noted in **F37**'s population estimate: the layout is loose *and* growing.

**Fixed on 2026-11-08 (`e361e03`), with the fifth arm §6's amendment now records.** np-cli resolves a runtime-less build to
`Resolution::RuntimeNotBuilt`, and `embed_run` answers it **before the currency read**: label `runtime not built`, exit 1,
`chunks_seen 0`, `errors 0`, both remedies named (`--features local-model`, and `np search --mode keyword` from **F36**). Arm (iv) is
untouched — hand the run a provider that refuses every batch and it still reports "owed", and in a `local-model` build unloadable bytes
genuinely are owed-and-not-produced after real work. The two cases now live in two `#[cfg]`-separated tests instead of one test choosing
its assertion with `cfg!`.

Evidence, all re-run rather than inherited: np-core `--test embed_run` **26 / 0**; np-cli default **37 / 0** including the new
`embed_without_a_runtime_refuses_before_reading_a_note`; np-cli with `local-model` **42 / 0**; and with the real 310 MB artifact exported,
the same suite **42 / 0** plus **39 / 0** in `models_cli`, no `NOT COVERAGE` line anywhere. Full local gate list at `e361e03`, `dirty=0`:
**17 rows exit 0**, two artifact rows SKIPPED by unset env, `NOT COVERAGE` count **0** (`/home/agent/tmp/gates-f40`). Mutation — mapping
`ProviderState::Ready` back to `Resolution::Provider` in a default build — FAILS the new test and passes again after revert; the core arm (v)
test asserts the label *before* the counts, because deleting the early return leaves every count at zero and would otherwise pass on numbers.

**What this card still owes:** CI has not graded any of it (out of included minutes since 2026-11-07), so no "Closed" line here — same
withholding as **F21**, **F28** and **F36**. And the published `v0.2.0` Windows asset still answers arm (iv) for this case: it was built at
`4c5200f`, before this fix, so its sha256 (`d36f3e3b…44c270`) is a promise about the old sentence too. Republishing under the same name would
make the release notes' digest wrong, so the fix ships with the next asset — say the word and I will build one rather than let a stale binary
be the only Windows download on the page.

## F42 — a workspace can hold a pinned model whose declared prefixes differ from the pin, and only `fetch` ever notices

Filed 2026-11-08 from round four's Windows log (`prefix_doc "" prefix_query ""` under an id whose pin declares two prefixes), reproduced on Linux
the same evening. Not a corruption bug: a **silent quality drift with no repair path**.

**The state.** `np models install --id embeddinggemma-2 --path … --dim 768 --sha256 …` without `--prefix-doc`/`--prefix-query` succeeds and records
empty prefixes for an id this build *pins*, where the pin declares `"represent this sentence for retrieval: "` / `"query for note search: "`. The
flags default to empty — documented as "Empty when omitted" — and nothing compares them against the pin, because `install` exists for artifacts
the human holds and cannot assume the pin describes them.

**What happens next, measured on Linux with a real runtime (`np 0.2.x`, feature `local-model`, one note):**

| step | result |
| --- | --- |
| `models list` | `embeddinggemma-2 present dim 768 sha256 2188ac1deca4 … prefix_doc "" prefix_query ""` — the drift is visible if you look |
| `np embed` | `outcome nothing owed`, `embedded 1`, `errors 0`. **No warning.** Vectors written with no instruction prefix |
| `np search "broker migration"` | a hit, `cos 0.8120` — versus `cos 0.8013` for the same note and same bytes installed **with** the pinned prefixes |

That pair of cosines is deliberately *not* offered as a quality number: one note, one query, and a higher score on the un-prefixed side proves
nothing (the asymmetry between document and query prefixes is what matters, and it shows up with distractors — which is F31's labelled set, not
this table). What it does establish is that **the two states produce different vectors**, silently.

**Why it sticks.** Once the row exists, both doors close on the correct declaration: `models install` refuses to change what an installed model
means (no `--force`, by design — replacing an identity invalidates every vector), and `models fetch` compares the stored declaration with the pin
and returns `IdentityDisagrees` **before transferring a byte**. So the honest commands refuse, and no command repairs it. The only way out today is
SQL against the workspace database, which is not something to tell a user who did nothing wrong but omitted two flags.

- **Option A — say something.** When embedding a **pinned** id whose row declares prefixes different from the pin's, print one line (and a JSON
  field) naming the mismatch, and keep going. Cheap, no new state; costs a warning that is arguably noise for people who installed deliberately.
- **Option B — a repair command.** `np models redeclare <ws> --id …` that verifies the artifact hashes to the pin, rewrites the declaration, and
  drops that model's vectors in the same transaction so they are recomputed under the meaning now on file. Honest about invalidation; new surface.
- **Option C — leave it** and document the two flags as load-bearing. Costs nothing here and leaves someone's search quietly worse.

**Blocks:** nothing mechanically. It weakens any sentence of the form "vectors for a pinned model mean what the pin says they mean" — which is a
sentence §12 leans on when arguing that a default-on fetch is safe.

**Done when:** a ruling is recorded and either the warning, the command, or the documentation carries a dated test; and `models list`'s empty
prefixes under a pinned id are no longer a state a careful user can enter without a word of warning.

**Ruled 2026-12-08: option A**, on the implementer's recommendation, and implemented in the same change. **A correction first, because this
card's premise was wrong.** It says the drift is "empty prefixes … where the pin declares `represent this sentence for retrieval:` /
`query for note search:`". `pins::PINNED_IDENTITIES` has declared **empty** prefixes since it was introduced (`864ef95`, and at `v0.2.3` —
checked with `git show v0.2.3:crates/np-core/src/pins.rs` on 2026-12-08), deliberately: no instruction convention has been measured in this
repository, and prefixes are part of an id's identity, so a measured convention must arrive as a new `@n` id rather than an edit. So the
round-four row (`prefix_doc "" prefix_query ""`) **agreed** with the pin, and the two cosines on this card compared a pin-conformant install
against one carrying prefixes the pin does *not* declare. Different vector bytes, yes — which was the point — but the card described the
direction backwards, and an implementation written to it would have warned about the conformant case.

**What shipped.** One predicate, `pins::prefix_drift(id, stored_doc, stored_query)`, because three commands want the same fact and only one of
them can refuse: `fetch` already returns `IdentityDisagrees` on this comparison before transferring a byte, and now agrees with the warning by
reading the same function. The notice prints on **stderr in both channels** (so stdout stays one word or one document) at three moments —
`models install` (before any vector exists: the actionable moment), `embed`, and `setup` (both of which reach rows that arrived another way: an
older build, a copied vault). `--json` carries `prefix_drift: {doc, query}` **only when there is drift**, so the key's presence is the signal
and no existing document shape changed. The two halves are reported separately and deliberately: the document prefix is what every stored vector
was computed under; the query prefix affects only future queries. Nothing about the outcome changes — that is what makes this A and not a
refusal wearing a warning's clothes.

Measured on a debug build, pinned id, one workspace: install with `--prefix-doc "represent this sentence: "` prints
``np: note: `embeddinggemma-2` is recorded with a document prefix this build does not declare for that id — its vectors mean something
different from a fetched model's, and no command rewrites them (`np models list` shows the declaration on file)`` and emits
`"prefix_drift": {"doc": true, "query": false}`; `embed --json` in that state prints the same line *and* still reports its own outcome
(`runtime-not-built`), with the key beside it. Omitting both flags — this build's conformant case, and the one round four recorded — prints
nothing at all.

Tests: `installing_a_pinned_model_with_an_undeclared_prefix_says_so`, `installing_a_pinned_model_without_prefixes_stays_silent`, and
`an_unpinned_id_is_never_told_it_disagrees_with_a_pin` (np-cli `models_cli`, 2026-12-08) plus
`embedding_a_pinned_model_with_undeclared_prefixes_notes_the_drift_and_still_reports_its_own_outcome` (np-cli `cli`). Two mutants, one per
failure mode a warning can have: making `prefix_drift` always return `None` kills the install and embed cases; making it always claim a
document drift kills the silent case. Neither mutant survives, which is the whole reason the silent case has its own test — a warning that
fires on every pinned install becomes noise, and nobody reads it.

**Still open, honestly:** option B (`models redeclare`) does not exist. The notice says "no command rewrites them" and points at `np models
list`, which is true but leaves the repair as SQL. That is acceptable while drift means *non-conformant prefixes the user chose*; it stops
being acceptable the day this build ships a pin with non-empty prefixes, because then an ordinary omitted flag produces exactly the state this
card describes and there is still no command to fix it. Revisit B at that id change.

## F41 — a fresh workspace with fetching switched off is refused as *configuration disabled*, and the sentence names no remedy

**Filed 2026-11-08, found while writing `np setup`'s tests (F38).** Not fixed by that commit; deliberately not fixed there, because
the wording below is pinned in fourteen places and changing it is a ruling, not a drive-by.

The state: `[embeddings] provider = "local-model"`, `model = "embeddinggemma-2"`, and nothing installed in this workspace yet — the
ordinary first run on an air-gapped machine, or any run under `NP_NO_AUTO_FETCH=1`. What comes back, on both channels, exit 1:

```
np: configuration disabled: embeddings.model = "embeddinggemma-2"
```

**Why it reads wrong.** The configuration is correct. It names a provider and an id that this very build pins; what is missing is a
310 MB file. `configuration disabled` points the reader at a key they already set, and no line tells them to place a file — while the
neighbouring arms do: arm (iii) (`artifact-absent`) carries [`install_hint`](../crates/np-core/src/embed_run.rs) with both remedies
spelled out, and F38's `offline` arm names re-running and keyword search. The one state where a person most needs the command line is
the one that omits it.

**Where it comes from.** `embed_run::run`'s arm (ii) re-reads the `models` row after np-cli already looked: no row ⇒
`EmbedOutcome::ConfigurationDisabled { key: "embeddings.model", id }` (`crates/np-core/src/embed_run.rs`, arm (ii)). That pre-empts
np-cli's own `NoProvider::RowMissing`, which *does* carry the install hint — so the program contains a correct sentence for this state
and prints a different one. F32 chose the "this is a configuration state" label on purpose (§6 of that design), and 14 assertions across
`crates/np-cli/tests/cli.rs` (from line 1619) pin both channels; `setup_with_fetching_switched_off_…` pins it for `np setup` too.

**Measured again on 2026-11-09, in the release-binary shape.** A default-feature binary (what the releases ship) on a fresh vault answers
the same `configuration disabled` — even though the true blocker there is the build shape: no inference was compiled in, and installing a
model will not change the outcome. With an artifact actually installed, the same command says F40's `runtime not built` and names both
remedies. So arm (ii) does not merely omit a hint; it can also **precede a more specific diagnosis**, which is the F40 mistake one level up
and the reason this card is worth ruling on rather than leaving. Both cases are asserted in
`setup_on_a_build_without_inference_indexes_and_names_the_feature`.

**The question for whoever rules on it:** now that F38 made fetching the default, is "no row" still best described as disabled
configuration — or is it the artifact-absent state seen from before the first install? Three options, cheapest first:

- **A.** Keep the label; append `install_hint(workspace)` to arm (ii)'s detail. One line plus test updates at the fourteen sites; no new
  state, and the reader gets a command in both cases.
- **B.** Give the state its own outcome (`model-not-installed`) with the hint as its payload, and let `configuration disabled` mean only
  "no provider configured" — cleaner taxonomy, bigger diff, and F32's card would need an amendment paragraph.
- **C.** Leave it. Defensible if one holds that a workspace where nothing was ever installed *is* unconfigured; the cost is that the
  air-gapped first run — the exact audience for the kill switch — has to find `np models install` from the README.

**Done when:** a ruling is recorded here, and a fresh-workspace refusal either names a remedy or this card says why it should not. Until
then the wording stays as pinned, and `setup_with_fetching_switched_off_indexes_and_refuses_without_blaming_the_network` asserts today's
sentence so that a future change fails loudly rather than silently.

**Ruled 2026-11-09 by the operator: option A.** "For the rest I would use your recommendation", and the recommendation was A — keep F32's
label, append a remedy. Implemented as `EmbedOutcome::ConfigurationDisabled { hint }`, `Some(install_hint(workspace))` at arm (ii)'s row-missing
half only, `None` for `provider = "none"` (a switch thrown deliberately is not a missing file) and for a key that names nothing at all. The field
is `skip_serializing_if = "Option::is_none"`, so **the machine-readable shape of every existing refusal is byte-identical**; the fourteen pins
from line 1619 of `crates/np-cli/tests/cli.rs` still pass because they assert `contains`, which is what made A cheap.

Measured on a built default-feature binary (the shape `cargo install np-cli` produces), fresh vault, correct config, `NP_NO_AUTO_FETCH=1`:

```
np: configuration disabled: embeddings.model = "embeddinggemma-2" — fetch it with `np models fetch vault --id <id>`, or install bytes you
already hold with `np models install vault --id <id> --path <file> --dim <width> --sha256 <digest>` — and note that this binary links no
embedding runtime, so `np embed` will still refuse until you run one built with `--features local-model`
```

**One clause beyond the letter of A, stated because it was not in the ruling.** `fetch` is default-on and `local-model` is default-off, so the
build a person gets from `cargo install` can obey the hint — download 309 MB, digest-verify it — and then meet F40's arm (v) anyway. A remedy
that ends in the same refusal is worse than no remedy, because the reader has spent bandwidth to learn nothing. So `install_hint` now appends
`runtime_caveat()` when the runtime is not compiled in, which also reaches arm (iii) and `np search`'s reuse of the same wording (one function,
three callers, one truth). If you would rather have the shorter sentence, delete that one function body — nothing else reads it.

**What A does not fix**, and I did not touch it because it is a ruling about outranking rather than wording: arm (ii) still *precedes* arm (v),
so on a no-inference build with nothing installed the label remains `configuration disabled` rather than F40's `runtime not built`. The clause
above makes the sentence honest; option B (`model-not-installed` as its own outcome, and possibly reordering) would make the taxonomy honest.
Recommending B stay un-taken until something else forces arm order again — every reorder is a claim about which fact outranks which, and F40
already spent that argument once.

**Falsified by mutation, not by reading.** Dropping the hint append (`hint: Some(...)` → `hint: None`) fails
`a_model_naming_no_row_names_the_command_that_fixes_it` (np-core) **and** `embed_naming_an_uninstalled_model_names_the_command_that_fixes_it`
(np-cli). Making `runtime_caveat` always empty fails `an_install_hint_says_when_this_binary_cannot_embed_anyway` and the np-cli pin. The
`local-model` half of that test asserts the *opposite* (no clause when the runtime is compiled in) and is graded by a different gate row, so both
directions have an owner. `provider_none_still_refuses_without_an_install_hint` exists because a mutant that hints on *every* configuration
refusal passes the first test: the two halves of arm (ii) are separate assertions, not one assertion about "arm (ii)".

**Still owed:** CI grading, as always — this is graded by `.github/scripts/gates.sh` locally.

## F43 — ship the inference runtime inside `np`, so a downloaded asset can do semantic search

**Filed 2026-11-08 by the operator's question "I thought llama.cpp is shipped with my software?"** It wasn't, and the answer to why matters
more than the fact: not a missing toolchain but a refusal at `crates/np-core/build.rs:125`, "`local-model` is built and tested on Linux only",
citing §14's rule that no platform gets a claim without a measurement on it. Three layers of honesty had stacked up into a product gap: the
feature was never attempted cross, `build.rs` refused it, and README:108 described the consequence as if it were a design choice.

**The ruling.** Ship the runtime inside `np` (the operator's ask was "download the llama.cpp parts"; measured, that split buys nothing —
stripped, the runtime costs **+2.18 MB** against a 310 MB model download), keep the model out of the asset, and make the model download the
one step a user takes. `local-model` is now compiled into both release shapes.

**Done, with evidence.**
- Cross-build works: llama.cpp v0.8.0 built for `x86_64-w64-mingw32` from the pinned tree; all 30 FFI functions present in the archives; an
  exe linked against them imports system DLLs only. Recipe and the four non-obvious facts: `docs/ops-cross-build-windows-gnu.md`.
- `build.rs` admits `windows`/`gnu` only behind `NP_LLAMA_CMAKE_TOOLCHAIN`, refuses a posix-threaded compiler out of cmake's own records,
  and keeps refusing macOS and MSVC. A cross build gets its own build dir and stamp lines because a warm host cache was demonstrably being
  certified to a Windows build.
- `.github/scripts/cross-build-windows.sh` builds `local-model,fetch`, verifies the runtime is in the artifact, strips, and prints both hashes:
  zip 6,907,477 B (`bc3fde7e…d5bd524c`), np.exe 16,253,440 B (`1359286f…979a7de7b`).
- Semantic search verified end to end on the Linux sibling of this shape: "who reconciled the ledger" ranks the ledger note at cosine 0.8358
  above the pasta recipe at 0.5704, `vectors:true` in `--json`, and F38's default-on pinned fetch fired by itself (295.5 MB, digest-checked)
  when the model was absent — which is now observable on a release asset for the first time.

**Owed before this can be called shipped.**
1. **A run on real Windows.** Nothing here has executed `llama_model_load_from_file` on Windows; there is no Wine on this host. The claim
   "semantic search works on Windows" is F18-shaped until someone's machine says so.
2. **README and the release notes for the *next* asset** must stop saying release binaries cannot embed (README:108 stays true of v0.2.3 and
   stops being true at the next cut — do not edit it retroactively).
3. **F42 becomes load-bearing**: with vectors reachable from a downloaded exe, an id installed without `--prefix-doc/--prefix-query` is no
   longer a curiosity. Its A/B/C ruling is now blocking, not backlog.
4. **MSVC parity** stays unmeasured and refused; `-gnu-cross` naming is where that difference is visible.

**2026-11-08 ~18:15 UTC — owed item 1 discharged, on someone else's hardware.** The operator ran `v0.3.0-rc.1`'s `np.exe`
(`997ce21f…66c2ffc5`, 16,254,464 B) on Windows 10.0.26300.0 with PowerShell 7.6.9 and sent the log back. Measured, from their run:

* `np embed` → exit 0, `embedded 2`, `errors 0`, `threads 12 of 16 available (auto)`, artifact digest re-checked ok at 295.5 MB.
  **The inference runtime loads and produces vectors on Windows.** That is the sentence this card existed to earn.
* Hybrid search → cosine **0.8033** on `kafka.md` (the note that answers the query), second hit 0.7276, `mode:"hybrid"`, `vectors:true`.
  Semantic-only run → top hit **0.7396**, correct note. Linux scores 0.85/0.80 for the same queries: same shape of answer, no Windows-specific
  numerical collapse.
* llama.cpp's own chatter (`load: control-looking token …`, `init: embeddings required but some input tokens were not marked as outputs`) and
  the F37 fallback sentence both stayed on **stderr** while stdout carried exactly one JSON document under real per-handle redirection — for
  hybrid *and* `--mode semantic`. The stdout-purity contract holds with a chatty C library linked in.
* F38's offline arm became observable on a downloaded asset for the first time: dead origin → exit 1, `offline:true`, no host name anywhere.

Still owed: item 2 (README at the next cut), item 3 (**F42's ruling is blocking**), item 4 (MSVC parity — this was mingw-built and unsigned).

## F44 — two `np` writers on one vault: one of them fails in 5 ms, and `--json` prints nothing at all

**Filed 2026-11-08 from the measurement the operator asked for** ("we definitely need to test this: 2 instances try to access the same vault.
Maybe we make a flag or something like a lock?"). Measured on Linux with the packaged v0.2.3 binary, 60-note vault, `XDG_DATA_HOME` redirected:

| pair | result |
| --- | --- |
| `scan` + `scan` | **one fails every time** (5/5 rounds): exit 1, `np: workspace database: sqlite: database is locked`, after **5–6 ms** |
| `search` + `search` | both succeed, identical output |
| `scan` + `search --json` | both succeed, 10 hits each, WAL doing its job |
| `scan` on two different vaults | both succeed |
| immediate retry of the loser | succeeds, exit 0 |

Three things worth naming before anyone fixes it:

* **`busy_timeout` is not protecting this path.** `db.rs` sets `PRAGMA busy_timeout = 1800`, and a timeout wait would take ~1.8 s; the loser
  returns in 5 ms. That signature is SQLite refusing to *wait* — a journal-mode change or a deferred transaction upgrading to write while
  another reader holds the file — not contention that a longer timeout would absorb. So "raise the timeout" is not the fix, and anyone who
  tries it will "fix" nothing and believe they did.
* **The failure is invisible to the machine-readable contract.** The loser writes **zero bytes to stdout** and one line to stderr. §"stdout
  carries exactly one JSON document" is broken for this case, and `fetched`/`offline` (F38's lesson: a state, not an error string) has no
  sibling for "someone else is working in this vault". An app backend that fans out invocations gets an empty pipe and an exit code.
* **A lock flag is probably the wrong first move.** `scan` renders `INDEX.md` *inside* its write transaction (documented in `scan.rs`), so two
  writers are not merely colliding on SQLite — they are racing over files SQLite cannot roll back. The shape worth designing is a coarse
  advisory lock around mutating commands with a bounded wait and an honest refusal (`locked`, in JSON, naming the other workspace), not a
  `--lock` toggle that leaves the default unsafe. That is a design conversation, which is why this is a card rather than a commit.

**Not claimed:** anything about Windows/macOS locking (measured on Linux only), and nothing about two writers where one is `embed` — the
release binary has no runtime, so `embed` refuses before touching the database. Reproduce with: two concurrent `np scan <vault>` in a loop,
capturing exit codes and stdout byte counts separately.

**Ruled 2026-12-08: the coarse advisory lock, default on, no flag** (the implementer's recommendation, adopted). Implemented the same day.

**What shipped.** `np_core::lock::WorkspaceLock` ([`crates/np-core/src/lock.rs`](../crates/np-core/src/lock.rs)) is a `BEGIN IMMEDIATE`
transaction on `<data-dir>/<workspace-key>.lock.db`, opened and left uncommitted for the life of the command, released by `Drop`. Five commands
take it before their first statement — `scan`, `embed`, `setup`, `models install`, `models fetch` — through one np-cli helper, `writer_lock`, and
no reader does. A second writer waits [`WorkspaceLock::DEFAULT_WAIT`](crates/np-core/src/lock.rs) (2 s, bounded) and then refuses as a **state**:
exit 1, one sentence on stderr naming the way around it, and under `--json` one document `{"command":…,"locked":true,"waited_ms":2000}`. That
document is the half this card was filed for: the loser used to write zero bytes.

**Why a second file rather than the workspace database.** Taking the write lock on `<key>.db` would be one fewer file and would read better in
the error. It cannot work: `scan` and `embed` write through their own connections, so the holder would contend with *itself* and refuse its own
work after 2 s. A side file excludes processes without contending with the work being protected. **Why not a lock file with a PID and a
timestamp in it:** that design's failure mode is the stale lock, and the whole reason to use SQLite here is that its write lock is released by the
operating system when a process dies — so there is nothing to detect, expire, or delete. **Why the rollback journal stays on:** a held-but-unwritten
transaction creates no `-wal`/`-shm` pair, so the cost is one zero-byte file per workspace that has ever been written to, sitting beside the
database it belongs to (copy a workspace's data and its lock travels with it; two workspaces never contend).

**Two ordering decisions worth their own sentences.** `writer_lock` runs §3 step 0 (`require_workspace_dir`) *before* deriving a lock name,
because taking a lock creates a file and a typo'd vault path must not leave a data directory and a `.lock.db` under a hash no scan will ever read
— the failure §3 step 0 exists to prevent, which this change would have re-opened through a new door. And `models fetch` takes the lock *before*
`--discard-marker`, refining that flag's documented "happens first, depends on nothing": what it must not do is delete a marker while another
process is mid-fetch in the same workspace, which is exactly the situation the lock detects.

**Measured on Linux** (debug binary, 30-note vault): five concurrent `np scan --json` all exit 0 with `added 0` and no child printed SQLite's
sentence — the bounded wait absorbing ordinary overlap, which is what it is for. Deterministic refusal is measured by holding the lock from a
separate process (POSIX record locks are per *process*, so an in-process second connection would not contend and a test written that way would
pass whether or not exclusion works).

**Tests** (`crates/np-cli/tests/cli.rs`, 2026-12-08): `a_second_writer_refuses_as_a_state_and_still_prints_one_document` (all five mutating
commands, plus text mode printing nothing on stdout), `readers_answer_while_a_writer_holds_the_workspace`,
`one_workspace_being_written_does_not_stop_another_being_written` (second vault *inside the same data directory*, so it proves scoping rather than
unrelated paths), `concurrent_writers_never_emit_the_raw_sqlite_error`; and in np-core, `acquiring_and_dropping_leaves_the_workspace_lockable_again`
plus `a_wait_is_a_bound_and_not_a_suggestion` (child-process holder; asserts the wait was honoured *and* stayed bounded). Two mutants, one per
direction: skipping `BEGIN IMMEDIATE` kills the refusal test and the bounded-wait test; making `search` take the lock kills the readers test.

**Still not claimed, and it needs the operator's machine:** that this excludes anything on **Windows**. SQLite uses `LockFileEx` there rather
than POSIX record locks, and the semantics are believed equivalent but unmeasured — Stage 13 of the harness is the two-writer test that would
measure it. Also unmeasured: two genuinely long overlapping runs (`embed` against a real corpus while `scan` waits), which the fixture corpus is
too small to produce, and anything on macOS.

## F45 — `np search` refuses outright when the index has vectors but the config has no provider, instead of answering by keyword

**Filed 2026-11-08 from round seven.** The operator tripped it with my own harness: Stages 6, 8 and 11 delete the config on their way out, so
the state *"workspace database holds embeddings, `config.toml` says `embeddings.provider = "none"`"* is produced by every run of the test script
and reached by real users whenever the data folder and the config file stop travelling together — moved profile, second machine, model
uninstalled, `NP_NO_AUTO_FETCH` box with a colleague's vault copied in.

Measured matrix (vault with an index **and** vectors, query `broker`, no config on disk):

| invocation | exit | what came back |
| --- | --- | --- |
| `np search <v> broker` (default = hybrid) | 1 | `np: embedding disabled: embeddings.provider = "none" — set it to "local-model" and name an installed model with embeddings.model, then run np embed` |
| `np search <v> broker --json` | 1 | nothing on stdout; the same sentence on stderr |
| `np search <v> broker --mode semantic` | 1 | same refusal |
| `np search <v> broker --mode keyword` | **0** | hits, `mode:"keyword"`, `vectors:true` |

So the keyword half demonstrably works and is not what failed. The default mode — which is the one an application calls without knowing whether
this vault has ever been embedded — hands back a configuration error instead of results. F36's promise reads the other way: hybrid was specified
as *both halves, degrading with a note* (`vectors:false` plus `note`) when there are no vectors, and that fallback fires correctly when the index
has **no** vectors (`crates/np-cli/src/main.rs:820`: `if semantic_wanted && (mode != SearchMode::Hybrid || present)` — hybrid-without-vectors
skips the provider entirely). The uncovered corner is precisely *hybrid + vectors present + provider unbuildable*: `present` is true, so we enter
the provider branch and a `ProviderState::Placeholder` becomes a hard refusal.

**Why I think it is a real defect rather than a defensible stance.** The refusal is right for `--mode semantic` — the user asked for something
this machine cannot do. For the default mode the user asked "find me notes about this", and there is an answer available in FTS5. A caller that
did not ask for semantics gets exit 1 and zero bytes, which is indistinguishable from "search is broken" — and `--json` printing nothing means
the machine-readable channel carries no explanation either (same failure shape as F44's silent loser).

**Options.**
* **A — degrade with a note (my recommendation).** hybrid + vectors + provider unbuildable → keyword results, `vectors:false`, and
  `note:"embeddings present but embeddings.provider is none: …"`. Keeps F36's contract ("hybrid never fails because one half is missing")
  consistent regardless of *why* the semantic half is missing. Cost: a user who wanted semantics silently gets keyword — mitigated by the note,
  and by `--mode semantic` still refusing loudly.
* **B — keep refusing, fix the machine-readable half.** Refusal stays, but `--json` emits `{hits:[], mode:"hybrid", vectors:false, error:{…}}`
  at exit 1 so a caller can branch. Cheaper, and honest about "you asked me for hybrid and I cannot do hybrid".
* **C — leave it.** Costs an app integrating `np search --json` a special case for a state the harness itself creates on every run.

**Owed:** ruling, then tests pinning *both* halves (the fallback results AND the note text), because F36's existing assertions cover only
"no vectors at all". Stage 12 of `run-stage5-7.ps1` measures today's behaviour and labels it `F45 (open)` so a ruling shows up as a diff.

## F47 — a runtime-bearing release ships MIT code and the project has no license of its own

Filed 2026-12-08 while writing `THIRD_PARTY_NOTICES.md`, prompted by §14's decision to own the llama.cpp build. Two separate obligations, one
card because they land in the same change:

**The third-party half is done.** Since `v0.3.0` every published asset statically links `libllama.a`, `libggml.a`, `libggml-cpu.a` and
`libggml-base.a` from llama.cpp `v0.8.0` — MIT, `Copyright (c) 2023-2026 The ggml authors`. MIT asks for the copyright line and license text to
accompany the distribution, so `THIRD_PARTY_NOTICES.md` carries them verbatim, names which archives are linked and which parts of the pinned tree
are not (`common/` with its bundled nlohmann/json, the server's cpp-httplib, `gguf-py/`, examples — a notice file that lists everything in a
vendored tree teaches readers to assume linkage that is not there), and both packaging scripts now copy it into the tarball/zip and refuse to
publish an asset whose listing lacks it. Verified by reading the archive back, not by trusting the write.

**The project's own license is missing, and it is a release blocker.** `ls LICENSE*` at the root returns nothing, which means every published
asset — including three releases' worth of downloads — has been distributed under no stated terms. That is not a cosmetic gap: without a license
the default is "all rights reserved", so a downloader has no permission to use, modify or redistribute `np`, and any contributor's patch arrives
without a grant that makes shipping it safe. Naming a license is the owner's decision rather than a contributor's edit, which is why this is a
card and not a commit.

Options, cheapest first: **A — MIT**, matching llama.cpp and the crate ecosystem, one file, no CLA theatre; **B — Apache-2.0**, if patent
exposure from a future commercial user matters more than brevity (and note it is dual-license-compatible with most of the dependency tree);
**C — AGPL-3.0**, which would deliberately make hosted deployments contribute back — and would be a statement about the project's direction, not
a neutral choice, since it interacts with MIT-licensed static linkage in ways that need care.

**Done when:** the owner picks one; `LICENSE` exists at the root and is named in `README.md`'s licensing section (which currently says plainly
that none exists) and in `THIRD_PARTY_NOTICES.md`; and a release is cut from that tree. Until then no release should be called final, and this
card is the reason printed in the release notes rather than a footnote here.

**Also owed by the same file:** generated Rust crate notices (`cargo about`) attached beside the C/C++ notice. Hand-maintaining ~200 entries would
be a list that is wrong more often than right, so the item is tooling — and the reason it is written down rather than dropped is that a copyleft
crate in the tree would need stronger treatment than disclosure, which is exactly what a generated report catches and prose does not.

## F46 — `np` creates an empty `<id>.models` directory for a workspace with nothing installed, and tooling reads it as "a model is here"

**Filed 2026-11-08 from round seven.** Observed on Windows at 18:15:18 during a plain `np scan`: a sibling directory
`<workspace-id>.models/`, 0 entries, next to the workspace sqlite. Nothing was fetched and nothing was installed; `np models list` correctly
reports none. The directory is created by `crates/np-core/src/models.rs:1582` (`fs::create_dir_all(...)` on the models dir) which runs on paths
that only *want* to know whether something is installed.

Why it matters more than an empty folder: my F37 evidence line printed ``.models dirs : N  (a fetched model lands beside the index)`` and counted
stubs, so a machine with no model at all could read as "N model directories present". Anyone debugging a "why is there no model" report has to
distinguish real installs from stubs by looking inside — which is exactly the check the directory name was supposed to save them.

**Options.** A: create the directory lazily, only where something is written into it (`fetch`, `install`), so its existence means what its name
says. B: keep creating it and document that emptiness is normal, then fix consumers (my script now counts `.models` directories *containing
files* and prints both numbers). A is the smaller lie; B is the smaller change. **Owed:** ruling plus, either way, a test that `scan` on a fresh
workspace does not leave a directory claiming a model exists — or a doc line saying it may.

**Ruled and shipped 2026-11-09 — option A, plus the operator's amendment.** *"For the F45: A, but when the program has it first install on a
machine it tries to install the huggingface model, if it has no internet connection then it falls back."* That sentence supersedes F38 §5's
"`np search` never fetches", so it is written into both design documents rather than left in a commit message: `docs/design-m3-hybrid-search.md`
§5 and §7 (the superseded bullet kept verbatim above its replacement), and `docs/design-f38-auto-fetch.md`'s two "never fetches" lines.

**The contract, as shipped.** Hybrid never refuses for a missing semantic half; it answers from FTS5 and says which half was absent — prose on
stderr, stable token under `--json`'s new `reason`, plus `fetched` and `offline` carrying F38's meanings so one reader handles both commands.
Seven tokens (`provider-disabled`, `model-not-installed`, `artifact-missing`, `fetch-offline`, `fetch-disabled`, `artifact-fetched`,
`no-vectors`), each with its own sentence, each naming only commands this build has (F32's rule). **`--mode semantic` keeps every refusal it had**
and does not fetch: degrading is what you do when you can still answer. Search writes no vectors and no catalog; the artifact cache is the only
thing it may create.

**Fetch conditions, because a read dialling out needs defending.** Hybrid mode + configuration naming a `local-model` provider + an id **this
build pins** + runtime and HTTP client compiled in + `AutoFetch::Deferred.allowed(&cfg)` — the same call `np embed` makes, so `[models]
auto_fetch = false` and `NP_NO_AUTO_FETCH=1` outrank search exactly as they outrank embed.

**Measured on a release-shaped binary (`--features local-model,fetch`), 2026-11-09.** Fresh vault, pinned id configured, never embedded:

| state | exit | what came back |
| --- | --- | --- |
| dead origin (`NP_MODELS_BASE_URL` → closed port) | **0** | `offline:true`, `reason:"fetch-offline"`, one keyword hit, no host in either channel |
| `NP_NO_AUTO_FETCH=1` | 0 | `offline:false`, `reason:"fetch-disabled"`, sentence names the switch |
| real origin | 0 | 30.7 s; `fetched:{id, members:[{bytes:309855456, digest-verified:true}], origin:"compiled-in-pin"}`, `reason:"artifact-fetched"` |
| …then `np embed`, then `--mode semantic` | 0 | cosine **0.6859** on the right note, `reason:null`, `vectors:true` |

The last row is what makes the amendment worth its bandwidth: the download was not decorative, and the note that says "run `np embed`" is the
difference between a caller that knows what to do next and one that wonders why a search moved 310 MB.

**What the new tests caught while I wrote them — both bugs were mine, both are in the code as comments.**

* Gating *semantic* resolution on `present` turned F32's refusal ("vectors exist under another id") into silence and made F40's
  `runtime is not built` disappear. Caught by `search_with_embedding_disabled_refuses_and_names_the_setting` and
  `searching_without_the_runtime_names_the_model_that_stayed_owed`. The rule now sits in the code: **semantic always embeds the query** — that
  call is what surfaces provider-level refusals — while hybrid may skip it when there is nothing to compare against.
* Passing `hybrid` straight into `embed_provider` as "may fetch" **bypassed the kill switch**: with `NP_NO_AUTO_FETCH=1`, search reported
  `offline:true` about a network it had no business touching. Caught by
  `search_with_fetching_switched_off_says_so_instead_of_blaming_the_network` on the `local-model,fetch` row only — a default-features run
  compiles the trigger out and would have "passed". That asymmetry (which feature row grades which claim) is why the gate list keeps both rows.

**Falsified as a vocabulary, not per-case.** `every_rank_note_says_what_arrived_and_never_names_a_host` asserts across all seven notes: unique
tokens, **unique sentences** (two states sharing prose is one state wearing two names), every sentence naming the half that *did* answer, no
`http` anywhere (§12 rule 8), and — feature-gated both ways — a fetch-less build never advertises `np models fetch` while a build that can fetch
advertises it somewhere. `a_note_token_is_not_its_sentence` exists because the obvious shortcut (make `reason` return the sentence) passes every
other assertion here.

**Also updated:** `an_empty_result_names_the_query_and_never_an_absent_model`, `search_json_names_the_mode_that_ran_and_the_half_that_was_missing`
(key set is now seven, and each new key is asserted by value rather than only listed), and
`search_without_a_model_answers_from_keywords_and_says_the_half_that_ran` — which now pins both notes apart, including that an unconfigured
workspace must **not** be told to fetch. Stage 12 of `run-stage5-7.ps1` still measures the state on a real machine and labels it `F45 (open)`;
its line changes to the degraded wording on the next Windows run, which is the diff this card asked for.

**Still owed:** CI grading (local rows: default 42 np-cli cases, `local-model,fetch` 56 with real artifacts), and the operator's Windows run to
confirm the first-install fetch behaves the same on a machine whose network stack they actually have.