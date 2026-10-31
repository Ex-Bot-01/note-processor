# M2 follow-up backlog (carried out of the M1 whole-branch review)

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

## F2 — skipped configuration tests stay silent

- **Where:** `crates/np-core/tests/config.rs:157` returns early when a precondition is unmet; its sibling at `:145`
  prints `eprintln!("skipping…")`.
- **Fix:** same visible skip message, so a green CI run cannot hide an untested branch on some platform.
  **Done when:** every early return in that file either asserts or prints.

## F3 — rustdoc warnings

- **Where:** 9 remaining from `cargo doc -p np-core --no-deps`: `items.rs` ×5, `scan.rs:11` ×2, `walk.rs`
  (`long_prefix`), `parse.rs` (ambiguous link). Mostly `private_intra_doc_links`.
- **Fix:** plain code spans or fully-qualified paths; add `cargo doc -p np-core --no-deps --deny warnings` as a CI
  step so it cannot regress. **Done when:** the command is clean and CI enforces it.

## F4 — no `busy_timeout`, though WAL exists for concurrent access

- **Where:** `crates/np-core/src/db.rs:182` — that `execute_batch` sets `journal_mode = WAL` and `foreign_keys = ON`
  but no `busy_timeout`, while the rationale comment at `db.rs:149-152` says WAL exists because a scan and an `np`
  command can overlap; the registry DB opens without one too (`config.rs`).
- **M1 decision:** single-process in M1, so a busy error is merely ugly.
- **Fix:** `PRAGMA busy_timeout` on every connection (workspace DB and registry), value from config or a constant.
  **Done when:** a test opens two connections where the second writes while the first holds a transaction and the
  write succeeds after waiting rather than returning `SQLITE_BUSY`.

## F5 — child tables have no foreign keys

- **Where:** `crates/np-core/src/db.rs:68-101` — `headings`, `tags`, `links`, `items` carry `note_id` with no
  `REFERENCES notes(id)`; the invariant is held by hand in `scan.rs:517-528`.
- **Risk:** one forgotten delete re-attaches an orphan edge to a recycled rowid.
- **Fix:** a new migration adding FK constraints (SQLite needs table rebuild for existing tables), plus
  `ON DELETE CASCADE` where the child is meaningless without its note; keep the manual cleanup as defence in depth.
  **Done when:** a test inserts a child row with a dangling `note_id` and it fails, and every M1 scan test still
  passes unchanged.

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

## F7 — a leading horizontal rule reads as frontmatter

- **Where:** `crates/np-core/src/parse.rs:146-160`: any file whose first line is exactly `---` with another `---`
  later loses that region from headings, words and items.
- **Fix:** require at least one `key:` line in the block before treating it as frontmatter; add a fixture note that
  opens with a horizontal rule followed by prose. **Done when:** that note keeps its first section in `headings` and
  `word_count`, while every existing frontmatter test still passes.

## F8 — tag dedup folds ASCII case only

- **Where:** tag normalisation in `crates/np-core/src/parse.rs` (see the `#ÄNER-TAG` gap recorded in the M1 ledger).
- **Fix:** Unicode-aware case folding for dedup while preserving the first spelling for display. **Done when:** a
  note with `#ÄNER-TAG` and `#äner-tag` yields one tag, rendered as first seen.

## F9 — per-statement re-preparation in the write path

- **Where:** `crates/np-core/src/db.rs:242`, `:308`, `:324` use `Connection::execute`; the scan path already uses
  `prepare_cached` (`scan.rs:650`).
- **Fix:** cached prepares for `upsert_file`, `apply_move`, `mark_stale`. **Done when:** behaviour is unchanged
  (all existing tests green) and a repeated-scan benchmark on the fixture corpus does not regress.

## F10 — `note_id` looked up twice per note

- **Where:** `scan.rs:489` then again at `:517` / `:596`.
- **Fix:** resolve once per note and pass the id down. **Done when:** no duplicated lookup remains in the re-parse
  path and the scan tests are unchanged.

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
- **D12 — `models.sha256` is not re-verified before a load (R6c2 review P2-3, same open decision as
  `LocalModelProvider::new`'s doc).** A same-width artifact swap is therefore embedded and stamped under the row's id.
  Cost of closing it: **measured on 2026-10-31** (`local_window.rs`) at **11.32 s** for this artifact's 309,855,456
  bytes — 26 MiB/s, which is several times the cost of loading and embedding a small workspace's worth of notes. So
  "verify on every embed" is not free and should not be adopted by reflex; the options are verify-on-change (stat the
  file, re-hash when size or mtime moves), verify in `np models list` only, or hash lazily per tensor page.
  R6d's second round (2026-10-31, §14's A/B subsection) removed the last excuse for waiting: llama.cpp's mmap-first
  load reads **every byte** of the artifact on first use — `read_bytes` moves by 309,858,304 — and it takes about
  **0.1 s**. Reading is not the cost; our 26 MiB/s userspace hash loop is. So verify-on-change (stat, re-hash on size
  or mtime) is now the option with no measured downside beyond a stale-mtime edge case, and this item needs a decision
  rather than another measurement.
- **D13 — llama.cpp defaults to 4 threads on a 16-CPU host; nobody has measured what raising it buys.**
  R6d's OpenMP A/B (2026-10-31) found both build configurations printing `n_threads=4 n_threads_batch=4` from
  `llama_context_default_params()`, which is why the flag moved no throughput at all: ggml's own pthread pool and an
  OpenMP parallel-for were handed the same four workers. `local_model::load` accepts that default without comment.
  The unmeasured question is whether `llama_context_params.n_threads` set from `std::thread::available_parallelism`
  (or a config key) turns 157 tok/s into something worth waiting for on a big note — and what it costs in peak RSS
  and in being a well-behaved background process next to someone's editor. Filed rather than done because it changes
  runtime behaviour for every user of the feature, needs an A/B of its own (thread counts 1/2/4/nproc at both lengths,
  RSS included), and no measurement so far says the current 81–157 tok/s is unacceptable.
