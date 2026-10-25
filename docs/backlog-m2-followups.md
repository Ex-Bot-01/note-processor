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
| R4 | F9, F10 | performance, invisible at 1.5k notes but wrong to carry into a watcher |
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

## F12 — wrapping counts characters, not display width

- **Where:** `crates/np-core/src/catalog.rs:170-176`.
- **M1 decision:** acceptable; output stays deterministic and §14 leaves the column rule to the builder.
- **Fix:** measure with display width (East Asian wide, combining marks, emoji) while keeping the wrap point a pure
  function of bytes so byte-stability holds. **Done when:** a CJK-titled note renders within 100 *columns* and the
  golden files are regenerated deliberately in their own commit.

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

---

## Explicitly not in this backlog

Embeddings and clustering, compression into `processed/`, item timelines and tick extraction, the daemon, API,
watcher, scheduler, web UI and desktop shell remain separate milestones with their own specs — nothing here should
be mistaken for them, and no table or module in this file pre-implements them.
