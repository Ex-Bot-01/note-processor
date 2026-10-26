# F13 — `rel` / `abs` as lossy strings: design plan

Design only; no code changed. Built on `de360ac`. Every claim below cites a file I opened, and the
behaviour was run end-to-end with `np scan` over a throwaway workspace (`caf\xe8.md`, `caf\xe9.md`,
`plain.md` — both odd names decode to the same text `caf\uFFFD.md`).

## 1. The failure, concretely

Two paths that differ only in bytes UTF-8 cannot hold collapse into one spelling before the database
is involved:

1. **walk** — `normalize_rel` builds `rel` component by component with `to_string_lossy` and joins on
   `/` (`crates/np-core/src/walk.rs:168-176`); `record` then derives `case_key = rel.to_lowercase()`
   from that already-lossy text (`walk.rs:235-241`). `DiscoveredFile` admits this in its own docs
   (`walk.rs:47-53`) and keeps the byte-exact name only in `abs`. `sort_found` even carries an `abs`
   tie-break "for two different raw names that decode lossily to the same `rel`" (`walk.rs:252-261`),
   so the walk knowingly hands on both files.
2. **digest** — `read_one` hashes each file's real bytes (`crates/np-core/src/scan.rs:354-392`), so the
   two paths reach `classify` with distinct hashes and *identical* `rel` / `case_key`
   (`crates/np-core/src/hash.rs:26-33`).
3. **classify** — identity is `rel` (Unix) or `case_key` (Windows) — `key_of`, `crates/np-core/src/change.rs:164-169`
   — and `index_by_key` puts both snapshots in **one bucket** (`change.rs:172-187`). With one known row
   and two found snapshots, `knowns.iter().zip(founds.iter())` pairs one and drops the other into
   `unmatched_found` (`change.rs:96-113`), which is emitted as `Added`. Because `apply_changes` keys its
   pending map by `rel` (`scan.rs:488-490`), **both** found files read back the same change.
4. **upsert_file** — `abs.to_string_lossy()` is bound as TEXT (`crates/np-core/src/db.rs:653`) and
   `rel`/`case_key` are already lossy strings, so the second path writes the *same row*: the statement's
   own `ON CONFLICT (rel) DO UPDATE` (`db.rs:638-645`) turns the collision into a silent overwrite.
   `store_note` then resolves one note id from that shared `rel` (`scan.rs:735`, `scan.rs:824-831`,
   `LIMIT 1`) and rewrites it with the second file's title, tags, items and links.

Observed on `de360ac`: first scan reports `added: 3 / notes: 2 / skipped: []`; `files` holds **one** row
whose `hash` is the byte-largest path's digest (`caf\xe9.md`) and whose `abs` is `/ws/caf\uFFFD.md` — a
path that does not exist, so nothing downstream can reopen the file. The other note has no row at all:
its title, headings, tags, items and links were never written. Nothing reports it — `skipped` stays
empty because no constraint fired. **Which key fires:** on this ordinary path *neither* does; the upsert
absorbs it (verified against the real schema: two lossy upserts, one row). A key only fires where a
statement rewrites an identity outright — `apply_move` sets `rel`, `case_key` and `abs` together
(`db.rs:704-706`) — and when both would clash SQLite reports **`files.case_key` UNIQUE**
(`SQLITE_CONSTRAINT_UNIQUE`, 0x813), while a `rel`-only clash names `files.rel` (0x613,
`SQLITE_CONSTRAINT_PRIMARYKEY`). Both codes are swallowed by `is_identity_conflict` (`scan.rs:690-702`)
and land in `skipped`, spelled as the lossy `rel` — so the report cannot name the file it lost.

Two consequences beyond the lost row: the scan **never converges** (three consecutive scans each reported
`added: 2 / unchanged: 1`; `known_rows` reads one row back, `scan.rs:425-431`), and the lossy text is
**published**: `INDEX.md` gained `- [[…]](caf\uFFFD.md)` (bytes `EF BF BD` in a committed catalog line,
rendered by `catalog.rs:163-177` from `CatalogEntry::rel`).

## 2. Why history cannot be repaired

The database never held the bytes. `files.rel` and `files.abs` are TEXT (`db.rs:70-72`) written through
`to_string_lossy`, which replaces every invalid byte with U+FFFD and discards which byte it was; two
inputs that collapse are indistinguishable afterwards, so a migration cannot tell `caf\xe8.md` from
`caf\xe9.md`, cannot restore the loser's `hash`, `size`, `mtime`, `body` or children (they were never
written), and cannot repair the one row it does have — its digest belongs to whichever file sorted last.

What a migration *can* do: carry every existing row forward without moving a byte, give each row a key
that later collisions can be measured against, and count the rows that look lossy (display text holding
U+FFFD) so a person knows which workspaces may still be hiding a twin. The missing note comes back only
by re-scanning: once identity is byte-exact, the swallowed path is simply `Added` again.

The precedent for doing it right is M1's workspace key: `Config::workspace_db` digests `path_bytes`,
"the raw OS bytes rather than a decoded string", precisely because "lossy decoding maps two different
paths onto one text" (`crates/np-core/src/config.rs:160-177`), with the byte/text split behind a
`#[cfg(unix)]` pair at `config.rs:214-226` and pinned by
`workspace_db_separates_a_non_utf8_path_from_its_lossy_spelling` (`crates/np-core/tests/config.rs:566-584`).
F13 is the same rule applied to the registry.

## 3. Options, with what each one breaks

Common to all three: `notes.rel REFERENCES files(rel) ON UPDATE CASCADE` (`db.rs:89`) is the **only**
child of `files` left — migration 3 re-pointed `headings`, `tags`, `items` and both `links` keys at
`notes.id` (`db.rs:519-559`), and `links.dst_note_id` is `ON DELETE SET NULL` (`db.rs:541`). So no option
re-points derived rows, and none touches the `SET NULL` semantics; each one only has to answer "what does
`notes` hang on".

**(a) `rel` becomes a BLOB keyed by an id.** Add `files.id INTEGER PRIMARY KEY`, make `notes.file_id
INTEGER REFERENCES files(id)` replace `notes.rel`. Cleanest identity model, and the FK is the cheapest to
reason about. Breaks: every query that resolves a note by path — `note_id` (`scan.rs:824-831`),
`links::store_links`' id map (`crates/np-core/src/links.rs:502-520`), `known_rows` (`scan.rs:427`),
`catalog_entries`' join (`scan.rs:867`), `LIVE_EDGE` (`db.rs:805-807`) — plus `apply_move`'s one-row update
and `rebase_abs`, which is text surgery on `abs` (`db.rs:779-796`). Also needs a display string for every
report line, so the lossy text comes back anyway as a second spelling.

**(b) A byte-preserving column alongside; `rel` demoted to display text.** Add `rel_bytes BLOB NOT NULL
UNIQUE` and `case_bytes BLOB NOT NULL UNIQUE`; make `notes.rel_bytes` the FK target; keep `rel`/`abs` as
derived lossy display. Identity queries move to `rel_bytes`, display sites (`catalog.rs:138`,
`scan.rs:113-127`) keep reading text and stay honest because they were always display-only. Breaks:
`case_key UNIQUE` must go — on a case-insensitive filesystem the fold still has to be unique, but the
fold of *bytes* is not defined for invalid UTF-8, so `case_bytes` is "fold the lossy display text, then
take bytes" and the surviving rule is exactly today's (`db.rs:669-670`: SQLite's `LOWER()` folds ASCII
only, which is why `apply_move` folds in Rust). Two spellings of one fact live in one row, so the pair
needs one owner — the `index_names` lesson from F11 (`crates/np-core/src/index_names.rs`) — otherwise a
writer updates one and not the other. Goldens do not move: display text for an ASCII/UTF-8 corpus is
byte-identical to today's, so `tests/golden/*` (paths printed at `tests/golden/INDEX-o-v.md:4-5`) and the
CLI assertions (`crates/np-cli/tests/cli.rs:175`, `:199-212`) stay as committed.

**(c) Reversible encoding inside the existing TEXT column** (percent- or `\xNN`-escaping the invalid
bytes). No schema change, no new column, and `notes.rel`/`case_key` keep working. Breaks the most for the
least: the stored spelling stops being the spelling an author types, so link resolution's exact-match step
(`links.rs:405-414`) and its basename index (`links.rs:427-441`) can never match a real name; `INDEX.md`
prints escapes (goldens move for any corpus holding such a name); `rebase_abs`'s suffix swap and the
`skipped` / `unresolved_targets` report lines (`scan.rs:113-127`, `crates/np-cli/src/main.rs:81-87`) all
show escapes to the user; and an escape scheme is ambiguous unless escaped-for-real input (`caf%E9.md`) is
also escaped, which changes ASCII paths' spelling too.

Link resolution under (a)/(b): a target naming the shared lossy spelling now has two byte-distinct
candidates, so it stays unresolved rather than guessing — consistent with `resolve`'s documented rule
(`links.rs:364-383`) and reported once in `unresolved_targets`. The CLI JSON shape does **not** change in
(b): `skipped` and `unresolved_targets` stay arrays of display strings (`scan.rs:113-127`); only their
contents gain the paths that used to vanish silently.

## 4. Recommendation

**Option (b)**, as migration 4 via `Migration::Code(byte_path_keys)` appended to `MIGRATIONS`
(`db.rs:64`, `db.rs:148`), rebuilt with F5's pattern — stage, copy, drop, rename, recreate indexes
(`rebuild_with_keys`, `db.rs:578-590`) inside the step's own transaction with `foreign_keys` off
(`db.rs:301`) and a `PRAGMA foreign_key_check` gate before commit (`db.rs:393-402`).

```sql
-- files: byte keys are the identity, rel/abs become display text.
CREATE TABLE files_new (
    id         INTEGER PRIMARY KEY,           -- rowid alias; nothing addresses it today
    rel_bytes  BLOB NOT NULL UNIQUE,          -- raw OS bytes of the rel  -> the identity
    case_bytes BLOB NOT NULL UNIQUE,          -- folded display bytes     -> one file per CI filesystem
    rel        TEXT NOT NULL,                 -- display only, derived from rel_bytes
    abs        TEXT NOT NULL,                 -- display only (byte-exact form lives in Rust)
    hash TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('ok','stale')), last_seen INTEGER NOT NULL
);
-- CAST is not decoration: a BLOB-affinity column stores an inserted TEXT value as TEXT, and a
-- column mixing TEXT and BLOB sorts the two classes apart (verified: 'zeta' before x'616C706861'),
-- which would split every ORDER BY on the key into two blocks.
INSERT INTO files_new (id, rel_bytes, case_bytes, rel, abs, hash, size, mtime, state, last_seen)
SELECT id, CAST(rel AS BLOB), CAST(? AS BLOB), rel, abs, hash, size, mtime, state, last_seen
  FROM files ORDER BY rowid;
DROP TABLE files; ALTER TABLE files_new RENAME TO files;
CREATE INDEX files_case_bytes ON files(case_bytes);

-- notes: keep the id (children hang off it), re-point the one child onto bytes.
CREATE TABLE notes_new (
    id INTEGER PRIMARY KEY,                    -- carried, never re-minted
    rel_bytes BLOB REFERENCES files(rel_bytes) ON UPDATE CASCADE,
    rel TEXT, title TEXT, title_source TEXT, encoding TEXT, word_count INTEGER, body TEXT
);
INSERT INTO notes_new (id, rel_bytes, rel, title, title_source, encoding, word_count, body)
SELECT id, CAST(rel AS BLOB), rel, title, title_source, encoding, word_count, body
  FROM notes ORDER BY rowid;
DROP TABLE notes; ALTER TABLE notes_new RENAME TO notes;
CREATE INDEX notes_rel_bytes ON notes(rel_bytes);
```

The `case_bytes` value cannot come from SQL: `lower()` folds ASCII only, which is the reason `apply_move`
folds in Rust (`db.rs:669-670`), so the copy loop reads each row and writes the same fold the walker uses
(`walk.rs:239`). Byte-stability of the copy rests on the same two things R2b ruled on (`fix(db): copy
migration rows in rowid order…`, `3f86529`): `ORDER BY rowid` on every copy, so the rowids a rebuild hands
out cannot reorder `note_tags`' `ORDER BY note_id, rowid` (`scan.rs:936-938`) and thus published tag bytes;
and `notes.id` carried rather than re-minted, because every derived row hangs off it (`db.rs:1-8`). Only
`files` and `notes` are rebuilt — `tags` is untouched, so no tag rowid moves at all. A receipt belongs in
`meta` beside `fk_orphans` (`db.rs:341`): `paths_lossy = 'files=N,notes=M'`, written even when zero, for
the same reason F5's is ("looked and found none" must be tellable apart from "never upgraded").

Parse-version interaction: this is schema-only, like every entry here (`docs/backlog-m2-followups.md`,
F15: "a SQL migration cannot do this — migrations here are schema-only"), so `PARSE_VERSION` stays `1`
(`crates/np-core/src/parse.rs:42`) and `refresh_needed` must not fire (`scan.rs:1043-1050`). Derived rows
are not rewritten by migration 4, and the first post-migration scan must report nothing changed: carry
`hash`, `size`, `mtime`, `state` verbatim, and derive display text with the *same* lossy rule so
`classify` sees the same `rel` strings it saw before.

## 5. Test plan

- **Latin-1 round trip.** Build a name with `OsStrExt::from_bytes(b"caf\xe9.md")`, scan, and assert
  `files.rel_bytes` equals those exact bytes and that rebuilding a `PathBuf` from them opens the file —
  the registry's answer to `workspace_db_separates_a_non_utf8_path_from_its_lossy_spelling`.
- **Both paths resolve.** `caf\xe8.md` + `caf\xe9.md`: two `files` rows, two `notes` rows, two catalog
  entries (identical display text, byte-different targets), `skipped` still empty. Add the convergence
  assertion this bug fails today: a second scan reports `added: 0`.
- **Resolution stays honest.** A note linking `[[caf.md]]` with both twins present yields one
  `unresolved_targets` entry, not a guess; deleting one twin resolves it onto the survivor.
- **Goldens do not move by a byte.** `golden_render_matches_committed_bytes`
  (`crates/np-core/tests/catalog.rs:940-975`) passes with no golden regenerated, and the CLI corpus
  assertions (`crates/np-cli/tests/cli.rs:175`, `:199-212`) keep their numbers. Any diff there is a
  regression in this migration, not a regeneration (contrast F16, which regenerates deliberately).
- **Migration test** on a hand-built version-3 database, following the `VERSION_TWO` fixture pattern
  (`crates/np-core/tests/db.rs:394-459`, `seed_version_two` at `:471`): ASCII and UTF-8 rows byte-identical
  after upgrade, `notes.id` unchanged, a stale note keeps its children (R2b's second ruling), the orphan
  receipt is written, and a file with a dangling `notes.rel_bytes` refuses to open.
- **Windows stance, explicit.** Windows has no byte spelling — Rust hands out UTF-16 code units, and
  unpaired surrogates are the only lossy case (`config.rs:214-226` already says so). F13 therefore lands
  Unix fidelity only: `rel_bytes` is filled from `OsStrExt::as_bytes` on Unix and from the current lossy
  text elsewhere, and the Windows gap stays a documented residual. Any later Windows fix must change
  `path_bytes`, `normalize_rel` and `rebase_abs` together in one step (an WTF-8 encoding of the wide
  units), never `rel_bytes` alone — a half-moved Windows path would be written under one rule and read
  under another.

## 6. Done when

- [ ] Two paths differing only in invalid UTF-8 bytes get two `files` rows, two `notes` rows, two catalog
      entries, and both are openable from what the database stored.
- [ ] A scan over such a pair converges: second scan reports `added: 0`, `modified: 0`, `removed: 0`.
- [ ] No collision is silent any more: either both rows exist, or the path appears in `skipped` under a
      spelling that identifies it (or the report says plainly that two paths share a display name).
- [ ] Migration 4 is one `MIGRATIONS` entry, rebuilds only `files` and `notes`, copies with
      `ORDER BY rowid` and `CAST(… AS BLOB)`, gates on `PRAGMA foreign_key_check`, and writes its receipt.
- [ ] Every committed golden in `tests/golden/` is unchanged, byte for byte, in the same commit.
- [ ] One module owns the (bytes, display) pair; no call site hand-derives display text from bytes.
- [ ] Gates green: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
      `cargo test --workspace`, `RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps`.
- [ ] The Windows residual is written down in the backlog, not left as an implication.
