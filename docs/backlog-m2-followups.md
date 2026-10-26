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
- **Wider since F18, and worse in two specific ways** (whole-branch pass `f7639d0c`). A refused directory is pushed
  into the same `unreadable` list (`walk.rs:227`), so anything that lands in it — refused or genuinely unreadable —
  now hides a subtree from `known`. While such a sibling sits beside a **readable** directory of the same lossy
  spelling: (1) a real deletion inside the readable one is never reported `Removed`, and a rename there produces a
  **second live row instead of a move**, because the moved-away path stays unknown rather than becoming `Removed`;
  and (2) one note can be charged twice, in `written.uncovered` (`scan.rs:567`) *and* `hidden_by_unknown`
  (`scan.rs:486`), so the refresh-gap line over-counts. Bounded, not permanent: it self-heals the moment the
  refused or unreadable sibling is renamed away, and no phantom row survives that. This is the imprecision F18's own
  doc points at but does not own — F18 widened who enters these lists, it did not create the matching.
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
- **Fix:** reuse what F21 built rather than inventing a second mechanism — a bounded record in `meta`, the value
  carried on the open handle, and an always-present `ScanReport` field surfaced by `np scan`. (`np status` does not
  exist; `np scan` is this program's only command, as F21 §3 records.) Keep the single list but word the entry with its
  cause where the walk knows it — `skipped: unreadable directory` against `skipped: name this build cannot key`.
  Report text plus a `meta` key only, so catalog goldens stay put. **Do when:** a workspace scanned with stderr
  refusing writes still lets a later `np scan` name the refused path, and the two causes are distinguishable in the
  output.

## F23 — a display-text key cannot say which byte twin its row is for

- **Where:** `crates/np-core/src/scan.rs`'s `key_is_display_derived` fallback in `known_rows`, and the rows migration
  4 keyed through `path_keys::rel_bytes_of_display` (`db.rs`). A name genuinely holding `U+FFFD`, and every lossy
  Windows name, are in this class permanently; only a hash-paired `Moved` (so `apply_move` rewrites `rel_bytes`) lets
  a migrated row out.
- **Risk — two configurations, identical inputs to `known_rows`, opposite right answers.** Both start from one stored
  row keyed `63 61 66 EF BF BD 2F …` with display `caf\u{FFFD}/note.md`, and an unreadable directory whose display
  spelling is the same:
  - **N′ — the row's own file IS the readable path.** The fallback hides the row, so nothing pairs with the listed
    file: it indexes as `Added` and the workspace ends with **two live `files` rows and two `notes` rows for one
    physical file**, two catalog entries, and links/headings able to diverge between the ids.
    `hidden_by_unknown` also holds `PARSE_VERSION` back, so every scan re-parses the corpus — F15's class — until
    someone renames or removes the twin. **No scan clears either half.**
  - **M — the row's own file is inside the blocked directory** and the readable path is its unindexed twin. Here
    hiding the row is *correct*: the scan reports `added: 1`, the note survives, and the version stays withheld.
  Any predicate that stops hiding N′ (for instance "the walk listed a path with this display text") also stops hiding
  M, which reports `removed: 1`, stales a live note, drops it from the catalog and records `PARSE_VERSION` over rows
  nobody re-parsed. That is why the fallback stays as wide as it is: **a lossy key carries no byte spelling, so nothing
  computable at `known_rows` distinguishes the two.** The naive fix was tried, probed against both cases, and rejected.
- **Fix candidates, with their costs.** (i) Pair by content hash the way `apply_move` does — needs the blocked path to
  become readable at least once, so it repairs rather than prevents. (ii) Found-side suppression: when a listed path's
  display text matches a row hidden by the fallback, skip that path and name it in `skipped`, trading duplicate rows
  for a readable file left unindexed while its twin is unreadable — this changes what a scan is allowed to omit, so it
  needs its own spec rather than arriving inside a hardening round. (iii) A future schema step that refuses to carry
  display-derived keys at all: re-key every row from OS bytes or mark the workspace as needing a full re-index.
- **Do when:** both cases are pinned as fixtures — one naming the duplicate-row harm, one fencing M so that any fix
  reporting `removed: 1` there fails loudly — and a workspace with `caf\xe9/` (blocked) beside `caf\xe8/` holding one
  display-keyed row converges to exactly one live `files` row and one `notes` row while still recording the parse
  version. Until then, do not describe the fallback's over-hiding as churn that heals: it does not.
---

## Explicitly not in this backlog

Embeddings and clustering, compression into `processed/`, item timelines and tick extraction, the daemon, API,
watcher, scheduler, web UI and desktop shell remain separate milestones with their own specs — nothing here should
be mistaken for them, and no table or module in this file pre-implements them.
