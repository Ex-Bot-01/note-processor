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

## F16 — no wide-character fixture in the golden corpus

- **Where:** repo-root `tests/golden/` (the golden directory is resolved at `crates/np-core/tests/catalog.rs:948`) contains no **two-column**
  title — em dash and umlaut are there and are single-width — so a width regression can only be caught by the unit
  tests in `tests/catalog.rs`, never by the byte-for-byte golden check. The shared corpus that would need the new note
  lives in `tests/fixtures/corpus`.
- **Why it is not done in F12:** adding a CJK or emoji title changes more than one file — it moves wrap points, can land
  in a different bucket and therefore change shard names, and both `cli` and `scan` goldens embed the generated file
  list. That is a deliberate regeneration across several fixtures and must be its own commit with its own review.
- **Fix:** add one wide-titled note to the shared corpus, regenerate every affected golden in a single commit, and
  confirm the independent verifier still reports `GOLDEN CHECK: pass` against the new bytes. **Done when:** the golden
  corpus contains at least one two-column title and CI is green on both runners.

## F17 — `continuation_indent` is accepted unclamped

- **Where:** `crates/np-core/src/config.rs:52` takes any `usize`, and `wrap` charges it as the prefix of every
  continuation line.
- **Risk:** pre-existing, not introduced by F12. An indent near or above `COLUMN_LIMIT` defeats the column budget on
  its own — every continuation line is over the limit before its first token — so a typo in `config.toml` silently
  produces one-token-per-line output with no warning.
- **Fix:** reject or clamp past a documented maximum (say 16) at config load, and say which in the error. **Done
  when:** an oversized indent fails `np scan` with a message naming the key instead of rendering oddly.

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
- **Fix:** an explicit decision, not a slow drift — either encode the wide units as WTF-8 so bytes exist to key
  on (changing `path_bytes`, `normalize_rel` and `rebase_abs` in one step, never `rel_bytes` alone), or say so
  in user-facing docs and make Windows refuse to *silently* overwrite: detect a display-text collision there and
  report it as `skipped` the way Unix does for byte collisions. **Do when:** on Windows, two names that differ
  only in unrepresentable units either key apart or are named in the report — never silently merged.

## F19 — a legacy duplicate note pair re-parses the whole corpus forever

- **Where:** `crates/np-core/src/scan.rs`'s `Indexed::Duplicate` arm charging `written.uncovered` on every
  scan, and `refresh_covered`, which therefore never returns true for such a workspace.
- **Risk:** deliberate at the level of F15 (recording the parse version beside a note that was not re-parsed is
  exactly F15's bug), but nothing heals it: no code merges two `notes` rows for one path, so every scan
  re-parses every note and prints the refresh-gap line. On a large corpus that is a permanent several-hundred-ms
  cost plus a warning nobody can act on, and the registry row keeps moving ahead of derived rows that never
  change (mixed-age database, `Indexed::Duplicate`'s doc).
- **Fix:** a migration or repair command that merges duplicate note rows for one `rel_bytes` — keeping the older
  `notes.id` so derived rows stay attached, re-pointing edges that named the loser, and reporting what it merged
  rather than choosing silently — after which the version records normally. Until then the cost is named in
  `refresh_covered`'s doc, not discovered. **Do when:** a workspace holding a legacy duplicate converges to
  `uncovered = 0` on the scan after the repair, and the repair names every pair it merged.

## F20 — two report-side filters still compare display text

- **Risk:** pre-existing shape, newly reachable. `scan.rs`'s `under_unreadable(&snapshot.rel, unreadable)` matches
  a lossy display spelling against a list of lossy display spellings, so with twin *directory* names one
  unreadable directory hides both twins from `known`: the readable twin can surface as a spurious `Added`, and
  `hidden_by_unreadable` inflates enough to hold back the parse version. Separately, `skipped.sort();
  skipped.dedup()` publishes one line for two byte-distinct refused files that share a display spelling — a
  reduced form of §1's "the report cannot name the file it lost".
- **Fix:** carry `rel_bytes` alongside the display spelling in both places (the unreadable set is built by the
  walk, which already has bytes) and key the dedup on bytes while printing display text. Neither changes catalog
  output, so goldens stay put; a fixture with twin directory names should pin both halves.
  **Do when:** a workspace with `caf\xe9/` and `caf\xe8/`, one unreadable, reports exactly the rows it should
  and names both refused paths on separate lines.

---

## Explicitly not in this backlog

Embeddings and clustering, compression into `processed/`, item timelines and tick extraction, the daemon, API,
watcher, scheduler, web UI and desktop shell remain separate milestones with their own specs — nothing here should
be mistaken for them, and no table or module in this file pre-implements them.
