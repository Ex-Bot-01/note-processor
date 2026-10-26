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
(`db.rs:704-706`) — and whatever SQLite names the failing key to be, `is_identity_conflict`
(`scan.rs:690-702`) accepts a UNIQUE *or* a PRIMARYKEY extended code alike, so the path lands in `skipped`
spelled as the lossy `rel` and the report cannot name the file it lost. **Which constraint name and which
extended code SQLite reports is deliberately not claimed here.** No source in this repository says so, and
nothing downstream reads the difference — which is precisely why §5 turns it into a pinned test instead of
a sentence in a design document.

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
F13 is the same rule applied to the **workspace database** — the per-workspace file holding `files` and
`notes`.

**The other database, named so that it is out of scope on purpose.** This program owns two SQLite files and
migration 4 touches one of them. `Config::workspace_db` (`config.rs:160-177`) opens the workspace database
this plan re-keys. `WorkspaceRegistry::open` (`config.rs:309-321`) opens `<data_dir>/workspaces.sqlite`,
whose single table is `workspaces(workspace TEXT PRIMARY KEY, first_seen INTEGER, last_seen INTEGER)`, and
`WorkspaceRegistry::record` writes that key through `abs_path(workspace).to_string_lossy()`
(`config.rs:323-332`). It is the same family of bug: two workspaces whose paths differ only in bytes UTF-8
cannot hold share one row, their `first_seen`/`last_seen` merge, and `recent` (`config.rs:335-345`) lists
one workspace where the disk holds two. It is **not** part of this migration — no note path, catalog line or
derived row is computed from that column, so its failure loses a recency record rather than publishing
wrong bytes — and re-keying it means changing another table's primary key under rules this plan has not
costed. It goes to the backlog beside F13's Windows residual (§5) with the same shape of fix: a BLOB key
plus `CHECK(typeof(key) = 'blob')` (§4). One sentence here so that "the registry", which both databases are
called in passing (`db.rs:59` for `files` + `notes`, `config.rs:301` for `workspaces`), never reads as one
file.

## 3. Options, with what each one breaks

Common to all three: `notes.rel REFERENCES files(rel) ON UPDATE CASCADE` (`db.rs:89`) is the **only**
child of `files` left — migration 3 re-pointed `headings`, `tags`, `items` and both `links` keys at
`notes.id` (`db.rs:519-559`), and `links.dst_note_id` is `ON DELETE SET NULL` (`db.rs:541`). So no option
re-points derived rows, and none touches the `SET NULL` semantics; each one only has to answer "what does
`notes` hang on" — and in (b) that question is not optional. A foreign key's parent key has to be unique,
so demoting `files.rel` to an ordinary display column while `notes.rel` still references it leaves a schema
SQLite refuses to run DML against (`foreign key mismatch – c referencing p`, reproduced on the real pair of
shapes). Moving `notes` onto the byte key is part of option (b), not a refinement of it.

**(a) `rel` becomes a BLOB keyed by an id.** Add `files.id INTEGER PRIMARY KEY`, make `notes.file_id
INTEGER REFERENCES files(id)` replace `notes.rel`. Cleanest identity model, and the FK is the cheapest to
reason about. Breaks: **every statement in the shared list below**, and it inherits none of the relief (b)
gets from keeping a display column — each of them resolves a note by path today and would have to resolve
it by id instead: `note_id` (`scan.rs:824-831`) and the same query inside `links::store_links`'s id map
(`links.rs:504-520`), `known_rows` (`scan.rs:425-457`), `catalog_entries`' join (`scan.rs:864-870`),
`LIVE_EDGE` (`db.rs:805-807`) — plus `apply_move`'s one-row update and `rebase_abs`, which is text surgery
on `abs` (`db.rs:779-796`). Also needs a display string for every report line, so the lossy text comes back
anyway as a second spelling.

**(b) A byte-preserving column alongside; `rel` demoted to display text.** Add `rel_bytes BLOB NOT NULL
UNIQUE` and `case_bytes BLOB NOT NULL UNIQUE`; make `notes.rel_bytes` the FK target; keep `rel`/`abs` as
derived lossy display. Identity statements move to `rel_bytes`, display sites (`catalog.rs:138`,
`scan.rs:113-127`) keep reading text and stay honest because they were always display-only. Four things in
those three sentences are load-bearing, and the first review round found each of them understated.

- **A conflict arm names an identity, so the upsert's target moves too.** `upsert_file` reads
  `ON CONFLICT (rel) DO UPDATE` (`db.rs:640`). Leaving that arm keyed on the display column while a
  beautiful new `rel_bytes UNIQUE` sits beside it re-creates F13 with better furniture: twins carry
  *identical* display text, so file 2 hits the `DO UPDATE` arm, overwrites file 1's row and never reaches a
  constraint at all — no `skipped` entry appears, because nothing fails. The target becomes
  `ON CONFLICT (rel_bytes) DO UPDATE`, and no conflict arm may name `case_bytes`: an arm keyed on the fold
  would absorb file 1 into file 2 exactly as `rel` does today.
- **`case_bytes` folds bytes, not the display text.** What this plan first wrote — "the fold of *bytes* is
  not defined for invalid UTF-8" — is simply wrong, and acting on it would have shipped the bug. Folding
  the lossy text gives both twins the same bytes (`caf\xe8.md` and `caf\xe9.md` are both `caf\uFFFD.md`
  before any fold touches them), so a UNIQUE over *that* fold absorbs file 2 through `case_bytes` as
  surely as the `rel` arm does, and — with the conflict target fixed to `rel_bytes` — it absorbs it into
  `skipped` instead of silence, which is a louder bug and not a smaller one. `case_bytes` is therefore the
  **ASCII-case fold of `rel_bytes`**: every byte in `A`–`Z` lowered, every other byte copied unchanged,
  including every byte that is not valid UTF-8. Total, byte-stable, no decode anywhere in it.
  What it keeps: two paths differing only in ASCII case still share a `case_bytes`, so the
  case-insensitive-filesystem rule survives and ruling P3's `skipped` report for `a.md` beside `A.md`
  survives with it, and `folded_row` (`scan.rs:704-716`) still finds "the row under another spelling of the
  same name" that a rename has to follow. What it changes: the identity fold is no longer Unicode-folded,
  so `Äpfel.md` beside `äpfel.md` — two genuinely different files on a case-sensitive filesystem, which
  today collide on `case_key UNIQUE` and lose one of them to `skipped` — gets two rows. On a case-insensitive
  filesystem that pair cannot both exist, so nothing loosens where the fold exists to do work.
- **The display fold stays Unicode, because it is a different fact.** `links::resolve` places a target by
  `case_key` with `target.to_lowercase()` (`links.rs:405-414`), which folds `ÄPFEL` onto `äpfel`; that is a
  question about how an author spells a name, not about which file it is, and it must not move with the
  column above. So `case_key` (TEXT, Unicode fold of display text, feeds link resolution) and `case_bytes`
  (BLOB, ASCII fold of OS bytes, identity guard) are **two facts with two names**, not two spellings of one
  fact — and both are derived in one place, the walker (`walk.rs:235-241`), under the `index_names` lesson
  from F11 (`crates/np-core/src/index_names.rs`): no call site hand-derives either, and neither is written
  without the other. `case_key UNIQUE` disappears with that division of labour — twins share a display fold by
  definition — leaving `case_bytes` the only uniqueness on a fold, while `case_key` keeps its index for
  resolution's sake alone.
- **(b) inherits the shared list below, in full.** Once two live rows may share a display spelling, "find
  the row whose `rel` is this text" stops being a question with one answer; §3's list is that work, and it
  belongs to (b) exactly as much as to (a). Goldens do not move: display text for an ASCII/UTF-8 corpus is
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

**The statements keyed on display `rel` — shared by (a) and (b).** Each entry is a statement I opened, what
it does wrong once two live rows share a display spelling, and the fix. None of them is optional in either
option; a subset re-keyed is the bug moved, not fixed.

- **`mark_stale`** (`db.rs:718-720`) — `UPDATE files SET state = 'stale' WHERE rel = ?1`. Deleting one twin
  marks **both** stale, so the catalog loses a note that is sitting there readable and nothing reports it.
  Fix: key on `rel_bytes` and take bytes from the caller, which is what §4.1's pipeline change makes
  available.
- **`apply_move`** (`db.rs:685-712`) — reads `SELECT abs FROM files WHERE rel = ?1` (`db.rs:690`) and writes
  `UPDATE files SET rel = ?2, case_key = ?3, abs = ?4 … WHERE rel = ?1` (`db.rs:704-707`). With twins it
  renames both rows and drags **both notes** through `ON UPDATE CASCADE` (`db.rs:89`), so one rename
  silently re-keys a file no scan moved. Fix: look up and update by `rel_bytes`, writing `rel_bytes` and
  `case_bytes` in the same statement as the display pair, so the four columns cannot diverge.
- **`rebase_abs`** (`db.rs:779-796`) — suffix surgery on the display `abs`. It stays text work, because its
  output is the display column; but it must run off the byte key's own row, and rebasing a *shared* display
  spelling hands one new `abs` to two files. Under (b) the byte-exact absolute path stops being stored at
  all: it is the workspace root plus `rel_bytes`, which is what makes `abs` genuinely cosmetic instead of
  "display, plus the authority nobody reads".
- **`note_id`** (`scan.rs:824-831`) and the same statement inside `links::store_links` (`links.rs:504-520`)
  — `SELECT id FROM notes WHERE rel = ?1 LIMIT 1`. The `LIMIT 1` is what makes today's answer silently wrong
  instead of loud: with twins it hands one file's text to the other note, rewriting its title, tags, items
  and links. Fix: select by `rel_bytes`, and **drop the `LIMIT 1`** so a future duplicate fails with
  `ExecuteMultipleValues` rather than picking a row; §5 pins the one-note-per-file assumption that today's
  `LIMIT` hides.
- **the catalog join** (`scan.rs:864-870`) — `FROM notes n JOIN files f ON f.rel = n.rel`. Two rows on each
  side of one display spelling is a cross product, so the catalog gains duplicate entries carrying wrong
  link counts. Fix: join on `rel_bytes`; §5 covers the ordering this exposes.
- **`LIVE_EDGE`** (`db.rs:805-807`) — `notes dn JOIN files df ON df.rel = dn.rel …`, reached from
  `unresolved_targets` and from `scan::unresolved_links`. Same cross product, plus a stale twin answering
  for a live one, so the count and the list printed under it can disagree. Fix: join on `rel_bytes`.
- **`known_rows`' note test** (`scan.rs:428`) — `EXISTS (SELECT 1 FROM notes n WHERE n.rel = f.rel)`. With
  twins a note belonging to either file answers for both, and that boolean feeds the refresh-coverage
  decision (`refresh_covered`, `scan.rs:1070`), which decides whether `PARSE_VERSION` gets recorded.
  Fix: correlate on `rel_bytes`.
- **`folded_row`** (`scan.rs:704-716`) — `SELECT rel FROM files WHERE case_key = ?1 AND rel != ?2`. Once the
  display fold is shared by twins it returns *the twin* as "another spelling of the same name", and
  `index_file` (`scan.rs:590-600`) then calls `apply_move` between two files that both exist. Fix: match on
  `case_bytes` and return `rel_bytes`, so only a genuine ASCII-case twin is offered as a rename source.
- **`live_corpus`** (`scan.rs:845-857`) — a `BTreeMap<case_key, rel>` filled with `insert`, so the second
  twin silently replaces the first and the link corpus holds one candidate where the workspace has two.
  That breaks this section's own promise: a target naming the shared lossy spelling would resolve onto
  whichever row SQLite read last, which is the guessing `resolve` refuses. Fix: key the corpus by
  `rel_bytes` and index candidates by display fold as a *list*, the way `links::basename_index`
  (`links.rs:427-441`) already models ambiguity; `resolve_target`'s first step (`links.rs:405-414`) then
  sees two candidates and declines.

Link resolution under (a)/(b): a target naming the shared lossy spelling now has two byte-distinct
candidates, so it stays unresolved rather than guessing — consistent with `resolve`'s documented rule
(`links.rs:364-383`) and reported once in `unresolved_targets`. That promise is carried entirely by
`live_corpus` keeping both candidates (list above); without it the corpus collapses to one entry and the
target resolves by accident. The CLI JSON shape does **not** change in (b): `skipped` and
`unresolved_targets` stay arrays of display strings (`scan.rs:113-127`); only their contents gain the paths
that used to vanish silently.

## 4. Recommendation

**Option (b)**, as migration 4 via `Migration::Code(byte_path_keys)` appended to `MIGRATIONS`
(`db.rs:64`, `db.rs:148`), rebuilt with F5's pattern — stage, copy, drop, rename, recreate indexes
(`rebuild_with_keys`, `db.rs:578-590`) inside the step's own transaction with `foreign_keys` off
(`db.rs:301`) and a `PRAGMA foreign_key_check` gate before commit (`db.rs:393-402`). Unlike F5, this step has
to *check* that enforcement is off rather than assume it; see "The most dangerous statement" below.

```sql
-- files: byte keys are the identity, rel/abs become display text. There is deliberately no INSERT
-- here — the copy is a per-row loop, for the two reasons below.
CREATE TABLE files_new (
    id         INTEGER PRIMARY KEY,           -- rowid alias, carried from the old rowid
    rel_bytes  BLOB NOT NULL UNIQUE CHECK (typeof(rel_bytes)  = 'blob'), -- OS bytes of the rel -> identity
    case_bytes BLOB NOT NULL UNIQUE CHECK (typeof(case_bytes) = 'blob'), -- ASCII fold of rel_bytes
    rel        TEXT NOT NULL,                 -- display only, derived from rel_bytes
    case_key   TEXT NOT NULL,                 -- Unicode fold of rel, feeds link resolution; no UNIQUE now
    abs        TEXT NOT NULL,                 -- display only; the byte path is root + rel_bytes (§3)
    hash TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('ok','stale')), last_seen INTEGER NOT NULL
);

-- notes: keep the id (children hang off it), re-point the one child onto bytes. `rel` stays as display
-- text and stops being a foreign key — two live notes may share that spelling (§3).
CREATE TABLE notes_new (
    id INTEGER PRIMARY KEY,                    -- carried, never re-minted
    rel_bytes BLOB REFERENCES files(rel_bytes) ON UPDATE CASCADE
              CHECK (typeof(rel_bytes) = 'blob'),
    rel TEXT, title TEXT, title_source TEXT, encoding TEXT, word_count INTEGER, body TEXT
);
-- This copy *can* stay one statement, and it is the only one that can: its key comes from the row's own
-- `rel`, so CAST(rel AS BLOB) is a per-row value. It also satisfies the CHECK — CAST to BLOB yields
-- typeof 'blob' (verified), whereas binding a Rust `&str` would not (§4 below).
INSERT INTO notes_new (id, rel_bytes, rel, title, title_source, encoding, word_count, body)
SELECT id, CAST(rel AS BLOB), rel, title, title_source, encoding, word_count, body
  FROM notes ORDER BY rowid;
```

Both tables then take their old name back and get their indexes — `DROP TABLE files; ALTER TABLE files_new
RENAME TO files;`, then `CREATE INDEX files_case_bytes ON files(case_bytes);` beside a recreated
`CREATE INDEX files_case_key ON files(case_key);`; the same shape for `notes` with `notes_rel_bytes` beside
the existing `notes_rel` (`db.rs:98`), which keeps serving the catalog's display ordering. Two things are
gone and one is new: `files.case_key UNIQUE` and `notes.rel`'s foreign key disappear — twins share a display
fold and a display spelling by definition, so uniqueness belongs to `case_bytes` alone (§3) — while
`case_key` itself stays, copied verbatim rather than re-derived, because SQLite's `LOWER()` is ASCII-only and
recomputing it in SQL would silently rewrite every non-ASCII name's fold, breaking the link resolution that
reads it. Read the `DROP`s before going further.

**The `files` copy is a loop over rowids, not an `INSERT … SELECT`.** Two independent reasons, each of them
fatal to the one-statement version this plan first carried:

- **There is no `id` to select.** `SELECT id … FROM files` names a column that does not exist today
  (`db.rs:69-79`: the table is `rel TEXT PRIMARY KEY`, and a TEXT primary key on a rowid table is a unique
  index, not a rowid alias — no integer column named `id` exists until this step creates one). The loop
  reads `rowid` and supplies the new `id` itself, pinned to that same number so a rebuild cannot shuffle
  addresses that some later round may want.
- **`case_bytes` is per-row.** Binding one `?` for it — which is exactly what a single
  `INSERT … SELECT` forces — stamps every row with the same fold, so the second row of any workspace
  holding two files aborts the step on `case_bytes UNIQUE`. That failure is loud rather than silent, which
  is the only mercy in it. SQL's own `lower()` is no substitute either: it folds ASCII only against a text
  value (`db.rs:669-670`, the reason `apply_move` folds in Rust), while §3 needs one byte fold implemented
  once and shared with the walker (`walk.rs:235-241`).

So the step is: (1) `SELECT rowid, rel, case_key, abs, hash, size, mtime, state, last_seen FROM files ORDER
BY rowid`, read out fully before any write — the same no-read-open-across-a-write discipline
`scan::known_rows` follows (`scan.rs:446-457`); (2) per row, `rel_bytes` = that row's stored display text as
bytes (§4.2 says what that means), `case_bytes` = the ASCII-case fold of those bytes, both bound as `Vec<u8>`,
with `case_key` and every other column copied as they stand; (3)
`INSERT INTO files_new (id, rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
VALUES (?1 … ?11)` with `?1` = the row's `rowid`.

**The new `case_bytes UNIQUE` cannot refuse a database that was legal at version 3.** Any two paths equal
under the ASCII byte fold are also equal under Rust's `str::to_lowercase` — ASCII letters fold to the same
letters either way, and everything else is byte-identical between the two strings — and it is
`str::to_lowercase` that writes `case_key` (`walk.rs:239`) and rewrites it on a move (`db.rs:706`). A pair
that would collide on `case_bytes` therefore could not have satisfied `case_key UNIQUE` in the first place.
The one collision this step could meet is two rows whose display text differs only by an invalid byte, and
those are already a single row (§1).

**The most dangerous statement here is `DROP TABLE notes`, and it cannot protect itself from the inside.**
With foreign keys enabled, SQLite's `DROP TABLE` performs an implicit `DELETE FROM` on the table before
removing it, and that delete obeys every child constraint. Four tables are children of `notes.id`
(`db.rs:519-559`): `headings.note_id`, `tags.note_id`, `items.note_id` and `links.src_note_id` are all
`ON DELETE CASCADE`, so every heading, tag and item row in the workspace disappears and every edge whose
source was a note goes with it; `links.dst_note_id` is `ON DELETE SET NULL` (`db.rs:541`) — the milder shape
of the same event, an edge left standing with its destination erased. Verified out of band against these
exact child shapes: dropping `notes` with the pragma on took headings 3→0, tags 4→0, items 2→0 and links 2→0
while leaving `files` untouched; with it off, every row stayed. Nothing reports it. The next scan re-derives
nothing that it does not re-parse, and every published byte in `INDEX.md` moves — tags vanish from entry
lines (`catalog.rs:163-177`), link counts fall to zero — with no error anywhere. `DROP TABLE files` is the
loud half of the pair by comparison: `notes.rel REFERENCES files(rel)` carries no `ON DELETE` action, so
dropping a parent that still has note rows fails outright.

The step cannot switch enforcement off to protect itself, because `migrate` sets
`PRAGMA foreign_keys = OFF` *outside* the transaction precisely for the reason that it has no effect inside
one (`db.rs:296-303`, `db.rs:301`) and a migration step runs inside it. So migration 4 must **read** the
pragma at the top of the step — `PRAGMA foreign_keys;` — and abort before touching a row when it answers
`1`, through its own error variant naming the pragma (not `DbError::ForeignKeys`, which means "this data
violates its keys"). Today `migrate` guarantees `0`; the guard exists so that a guarantee made 200 lines away
is not an assumption silently inherited by a refactor, or by a caller reaching the step some other way. It
must not lean on statement order either — running the noisy `DROP TABLE files` first is luck, not a defence.
§5 pins both halves: the refusal, and the child-table counts that prove nothing cascaded.

**A BLOB column does not make a bind be a BLOB, which is what the `CHECK`s are for.** BLOB is an affinity,
not a constraint: a column declared BLOB stores whatever type it is handed, and rusqlite binds a `&str` as
TEXT — the same default that put lossy text into `rel` in the first place (`db.rs:640-653`). A TEXT key and
its BLOB twin never compare equal and never conflict (verified: one table, one `UNIQUE` BLOB column,
`b"cafe.md"` and `"cafe.md"` stored side by side; `WHERE rel_bytes = ?` bound with bytes returns only the
BLOB row), so a single accidental string bind reopens F13 exactly as filed — no constraint fires, no report
line appears, and two rows hold one file's identity in two types that cannot see each other. It would also
split every `ORDER BY` on the key into two blocks, because SQLite orders BLOB before TEXT across types
(verified: `'zeta'` sorts before `x'616C706861'`). Hence three defences rather than the one `CAST` this plan
first leaned on: bytes bound from Rust, `CHECK(typeof(…) = 'blob')` on all three key columns so a string bind
fails at the write instead of at review, and no `CAST` in the `files` path at all — a cast is how a string
gets to look like a key.

**Which module owns every bind.** All three key columns are written from `crate::db` and nowhere else:
`upsert_file` (`db.rs:636`), `apply_move` (`db.rs:685`) and `mark_stale` (`db.rs:718`) take byte keys as
parameters, and migration 4's own copy loop is the fourth writer. That constrains §3's shared list: the
statements in it that live in `scan.rs` — `note_id`, `folded_row`, `known_rows`, `catalog_entries`,
`live_corpus` — move behind a bytes-typed call in `db.rs` or take a `&[u8]` parameter, so "bind this key as
TEXT" stops being a call any of them can write. The `CHECK` is the backstop; the API shape is the design.

Byte-stability of the copy rests on the same two things R2b ruled on (`fix(db): copy
migration rows in rowid order…`, `3f86529`): `ORDER BY rowid` on every copy, so the rowids a rebuild hands
out cannot reorder `note_tags`' `ORDER BY note_id, rowid` (`scan.rs:936-938`) and thus published tag bytes;
and `notes.id` carried rather than re-minted, because every derived row hangs off it (`db.rs:1-8`). Only
`files` and `notes` are rebuilt — `tags` is untouched, so no tag rowid moves at all. A receipt belongs in
`meta` beside `fk_orphans` (`db.rs:341`): `paths_lossy = 'files=N,notes=M'`, counting the rows whose display
text holds U+FFFD, written even when zero, for the same reason F5's is ("looked and found none" must be
tellable apart from "never upgraded") — and read as §4.2 describes, because that count is the only way a
person can tell next scan's expected churn from a regression.

Parse-version interaction: this is schema-only, like every entry here (`docs/backlog-m2-followups.md`,
F15: "a SQL migration cannot do this — migrations here are schema-only"), so `PARSE_VERSION` stays `1`
(`crates/np-core/src/parse.rs:42`) and `refresh_needed` must not fire (`scan.rs:1043-1050`). Derived rows
are not rewritten by migration 4: carry `hash`, `size`, `mtime`, `state` verbatim, and derive display text
with the *same* lossy rule so `classify` sees the same `rel` strings it saw before. That buys "the first
post-migration scan reports nothing changed" for a workspace with no non-UTF-8 name and for that workspace
only — §4.2 is what happens in the other case, and §6 carries both halves.

### 4.1 Byte identity has to travel walk → digest → classify → apply_changes

A `rel_bytes` column fixes nothing on its own: it only ever holds the bytes some caller handed it. Every
structure between the walk and the write keys a file on lossy text today, so twins collapse **before**
storage is reachable, and none of §6's first three items can pass while that is true. Each of these needs a
byte key, in the same change as the statement re-keying of §3:

- **`walk::DiscoveredFile`** (`walk.rs:46-59`) carries `rel: String`, `case_key: String` and the byte-exact
  `abs: PathBuf`. It gains the byte rel and the byte fold — on Unix `OsStrExt::as_bytes` of each component,
  joined by `/`, which is `normalize_rel`'s existing component walk (`walk.rs:168-176`) with the
  `to_string_lossy` taken out of the middle. `sort_found`'s tie-break (`walk.rs:252-261`) then compares byte
  rels instead of leaning on `abs` for the case §1 is about.
- **`scan::read_one`** (`scan.rs:354-392`) builds the snapshot from a discovered file and has to carry both
  forms across; `Found` (`scan.rs:290-303`) keeps its byte-exact `abs` unchanged.
- **`hash::Snapshot`** (`hash.rs:26-38`) — that pair of lossy strings *is* the identity today. It gains
  `rel_bytes`/`case_bytes`, and `rel` stays for display and report lines only.
- **`change::key_of`** (`change.rs:164-169`) and **`index_by_key`** (`change.rs:172-187`) — where a twin
  pair actually dies, in both directions: `key_of` returns `&str`, the index is
  `HashMap<&str, Vec<&Snapshot>>`, so both snapshots land in one bucket and `change.rs:96-113` pairs one and
  drops the other. Both move to byte keys, including the per-bucket `sort_by_key` on `rel`.
- **`classify`'s output** (`change.rs:63-67`) is `Vec<(String, Change)>`: a display string carrying an
  identity decision into the write pass, with `Change::Moved { from }` carrying a second one. The change list
  has to name the file by bytes — display text alongside, for the report — or `apply_changes` looks twins up
  in one slot whatever the columns are called.
- **`scan::apply_changes`' pending map** (`scan.rs:488-490`) — `HashMap<&str, &Change>` keyed on `rel`, which
  is §1.3's "both found files read back the same change" and survives any schema-only fix. Keyed on bytes.
- **`scan::known_rows`** (`scan.rs:425-457`) builds the database side of the comparison and must build
  byte-keyed snapshots from `rel_bytes`/`case_bytes`; if it kept handing out display text, §4.2's churn would
  be permanent instead of one-time.

Deliberately **not** moved: `links::resolve`'s corpus keys (`links.rs:364-383`, `links.rs:405-414`) and the
report strings (`scan.rs:113-127`). Author-facing spelling is display text by design — a wikilink names a
title, not a byte sequence — and §3 already says what happens when two files share one spelling. The only
thing that changes there is `live_corpus` keeping both candidates (§3's list), so ambiguity is detected
rather than resolved by whoever was read last.

### 4.2 What the first scan after migration 4 reports

**What the migrated key is.** Migration cannot invent bytes it never held (§2), so an existing row's
`rel_bytes` is the UTF-8 encoding of its stored display text. For an ASCII or valid-UTF-8 name those are the
OS bytes, and nothing moves: `classify` matches on the same spelling, `hash`/`size`/`mtime` were carried
verbatim, the scan reports every file unchanged, and the goldens are byte-identical.

**Where a non-UTF-8 name exists, expect churn.** A row stored as `caf\uFFFD.md` migrates to bytes
`63 61 66 EF BF BD 2E 6D 64`. The next walk computes the real OS bytes, `63 61 66 E9 2E 6D 64`. Those are
different byte keys, so `classify` sees a path that vanished and a path that appeared: **one `Removed`, one
`Added`**, not "nothing changed". That is the swallowed twin coming back, which §2 already said needs a
rescan; it is honest churn rather than new loss, and it converges — the migrated row goes `stale`, keeps its
children and leaves the catalog (`catalog_entries` filters `f.state = 'ok'`, `scan.rs:864-870`), while the
byte-keyed row is live from this scan on.

Two consequences of that one-time event are worth naming rather than discovering. The re-added note is a
**fresh `notes.id`**, so edges pointing into the old row now name a stale file and land in
`unresolved_links` (`LIVE_EDGE`, `db.rs:805-807`) until their sources are re-parsed — which happens when a
source's content changes or when `PARSE_VERSION` moves, not on this scan. And the derived rows come from a
re-parse rather than from the carried-over ones, so a workspace that had hand-edited children on the stale
row keeps them in the database but publishes the re-derived set.

**How it reads against the receipt.** This is what `paths_lossy = 'files=N,notes=M'` is for. It names the
workspaces holding rows whose display text contains U+FFFD, so a person can predict about `N` such
Removed/Added pairs before scanning and explain them afterwards; where the receipt reads `files=0`, a
`Removed` in the next scan is a bug rather than a migration artefact. That makes the receipt worth pinning in
§5's migration test, not merely writing.

## 5. Test plan

- **Latin-1 round trip.** Build a name with `OsStrExt::from_bytes(b"caf\xe9.md")`, scan, and assert
  `files.rel_bytes` equals those exact bytes and that rebuilding a `PathBuf` from them opens the file —
  the registry's answer to `workspace_db_separates_a_non_utf8_path_from_its_lossy_spelling`.
- **Both paths resolve.** `caf\xe8.md` + `caf\xe9.md`: two `files` rows, two `notes` rows, two catalog
  entries — whose display text *and* displayed target are identical, which is what the ordering item below
  exists for — and `skipped` still empty. Add the convergence assertion this bug fails today: a second scan
  reports `added: 0`.
- **Byte identity in the pipeline, not just in the schema.** A `change.rs` unit test over two snapshots whose
  byte keys differ while their display `rel` is identical must yield two identities and two changes, not one
  pair plus a dropped file; and `apply_changes` must write both rows for them. This fails on today's
  `key_of`/`index_by_key`/pending-map whatever the database looks like (§4.1), which is the point.
- **The two folds, each pinned.** `a.md` beside `A.md` on a case-sensitive filesystem still produces one row
  and one `skipped` entry — the ASCII byte fold keeps ruling P3 — while `Ämli.md` beside `ämli.md` produces
  *two* rows, which is the behaviour change §3 accepts out loud. And a link `[[ÄMLI]]` still resolves onto
  `Ämli.md`, because the display fold stays Unicode.
- **Catalog order for twins is pinned, not incidental.** Two live twins with the same title give the rendered
  order twice and compare bytes: it must be identical, and must come from the byte key in `ORDER BY` and
  `sort_key` (`catalog.rs:82`, `catalog.rs:133-139`) rather than from whichever rowid SQLite happened to
  assign. A third scan reproducing the same bytes is the assertion this plan's byte-stability promise needs;
  today's tie-break `(bucket, lowercased title, rel)` (`catalog.rs:133-139`) ties on both entries.
- **Keys are typed.** One test per key column — `files.rel_bytes`, `files.case_bytes`, `notes.rel_bytes` —
  binding a `&str` and asserting `CHECK(typeof(…) = 'blob')` refuses it. The regression this plan cannot
  afford is a TEXT twin of a BLOB key: it conflicts with nothing and reports nothing (§4).
- **One note row per file, pinned.** After a scan, and again after a move, no `notes.rel_bytes` carries two
  rows (`GROUP BY rel_bytes HAVING COUNT(*) > 1` is empty). §3 removes `note_id`'s `LIMIT 1`, so this
  assumption stops being hidden by a `LIMIT` and starts being tested.
- **Resolution stays honest.** A note linking `[[caf.md]]` with both twins present yields one
  `unresolved_targets` entry, not a guess; deleting one twin resolves it onto the survivor. This is the test
  that fails if `live_corpus` still collapses twins into one map entry (§3's list).
- **Goldens do not move by a byte.** `golden_render_matches_committed_bytes`
  (`crates/np-core/tests/catalog.rs:940-975`) passes with no golden regenerated, and the CLI corpus
  assertions (`crates/np-cli/tests/cli.rs:175`, `:199-212`) keep their numbers. Any diff there is a
  regression in this migration, not a regeneration (contrast F16, which regenerates deliberately).
- **Migration test** on a hand-built version-3 database, following the `VERSION_TWO` fixture pattern
  (`crates/np-core/tests/db.rs:394-459`, `seed_version_two` at `:471`): ASCII and UTF-8 rows byte-identical
  after upgrade, `notes.id` unchanged, a stale note keeps its children (R2b's second ruling), the receipt is
  written, and a file with a dangling `notes.rel_bytes` refuses to open. Four things this bullet used to leave
  out:
  - **Pin the row counts of all four child tables** — `headings`, `tags`, `items`, `links` — before and after
    migration 4, not merely "a stale note keeps its children". Those are the four cascades §4's `DROP TABLE`
    hazard runs through, and a partial cascade that still leaves *some* children would pass the weaker claim.
  - **Pin the refusal**: run the step with `PRAGMA foreign_keys = ON` and assert it errors out having touched
    nothing — child counts unchanged, `schema_version` unmoved, the file byte-identical if that is cheap to
    assert.
  - **Pin that every key came from its own row**: after upgrade, each row's `rel_bytes` equals the bytes of
    that same row's `rel`, across several rows with different names. One bound parameter for `case_bytes`
    passes a one-row fixture and fails this.
  - **Pin the receipt** (`paths_lossy`) on a fixture holding one U+FFFD row as well as the zero case.
- **Migration, then scan.** On that hand-built database holding a row displayed as `caf\uFFFD.md` over a
  workspace whose real file is `caf\xe9.md`: the first scan reports exactly one `Removed` and one `Added`
  (§4.2), the receipt names that workspace beforehand, the stale row keeps its children and leaves the
  catalog, and the scan after that reports nothing changed. Without this test §4.2's promise is a paragraph
  rather than a fact.
- **What SQLite calls an identity clash.** One test capturing the constraint name in the message *and* the
  extended code rusqlite surfaces for both an `upsert_file` clash and an `apply_move` clash, so the answer is
  evidence in a test file instead of a sentence here. Not load-bearing — `is_identity_conflict`
  (`scan.rs:690-702`) accepts UNIQUE and PRIMARYKEY alike, which is exactly why §1 stopped asserting it.
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
- [ ] Byte identity travels the whole pipeline: `DiscoveredFile`, `Snapshot`, `key_of`/`index_by_key`,
      `classify`'s change list and `apply_changes`' pending map all key on bytes (§4.1). No structure between
      walk and write keys a file on its display text.
- [ ] Every statement in §3's shared list is re-keyed — `mark_stale`, `apply_move` (both its statements),
      `note_id` in `scan.rs` and in `links.rs`, the catalog join, `LIVE_EDGE`, `known_rows`' `EXISTS`,
      `folded_row`, `live_corpus` — and `upsert_file`'s conflict target is `rel_bytes`. One of them left
      display-keyed is the bug, not a subset of the fix.
- [ ] All three key columns carry `CHECK(typeof(…) = 'blob')`, every bind of a key is `&[u8]`/`Vec<u8>`, and
      no `CAST` survives in the `files` write path.
- [ ] Migration 4 is one `MIGRATIONS` entry, rebuilds only `files` and `notes`, copies `files` rowid-by-rowid
      in Rust with a per-row byte fold (no `INSERT … SELECT`, no single bound key), refuses to run when
      `PRAGMA foreign_keys` reads `1`, pins all four child-table counts, gates on `PRAGMA foreign_key_check`,
      and writes its receipt.
- [ ] A workspace with no non-UTF-8 name reports nothing changed after migration 4; one with `N` flagged rows
      reports exactly the churn §4.2 predicts and converges on the scan after.
- [ ] Catalog order is deterministic for twins, from the byte key in `ORDER BY` and `sort_key`, not from a
      rowid nobody chose.
- [ ] Every committed golden in `tests/golden/` is unchanged, byte for byte, in the same commit.
- [ ] One module owns the (bytes, display) pair *and* both folds — `rel_bytes`/`case_bytes` and
      `rel`/`case_key`; no call site hand-derives display text from bytes or a fold from either.
- [ ] The `workspaces.workspace` key of the registry database is written down in the backlog as its own item
      (§2), not folded into this one.
- [ ] Gates green: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
      `cargo test --workspace`, `RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps`.
- [ ] The Windows residual is written down in the backlog, not left as an implication.

## 7. Implementation split

**Yes — two rounds, and the order is schema first, pipeline second.** The tempting framing ("byte identity
through the pipeline first, schema keys and migration second") is the half that cannot work. `known_rows`
(`scan.rs:425-457`) builds the `known` side of every comparison *from the database*, so if `classify` keys on
bytes while the stored key is still display text, every non-UTF-8 row reads as removed-and-re-added on every
scan forever — §1's never-converges symptom, manufactured rather than fixed. That is the same half-moved rule
this plan already refuses on Windows (§5): a key written under one rule and read under another.

**Round 1 — schema only; nothing reads the new keys.** Migration 4 exactly as §4 describes it: both tables
rebuilt with byte keys and their `CHECK`s, the rowid loop, the receipt, the `PRAGMA foreign_keys` guard — and
no query changed to use any of it. Proves on its own: every golden byte-identical; every migrated row's
`rel_bytes` equals the bytes of that row's own display `rel`; a version-3 file opens, passes
`PRAGMA foreign_key_check`, keeps all four child-table counts and every `notes.id`; the receipt is written
even at zero; the step refuses when enforcement is on. It changes no report line, because nothing reads the
columns yet.

**Round 2 — byte identity end to end.** §4.1's pipeline change plus every statement in §3's shared list,
in one commit: the walk emits bytes, the digest carries them, `classify` indexes on them, `apply_changes`
keys its pending map and its writes on them, and the display fold stays only where authors look. Proves on its
own: §6's twin items — two rows, two notes, two catalog entries, both files openable from what was stored,
the second scan reporting zero changes, twins landing in `unresolved_targets` rather than resolving by
accident, deterministic catalog order.

**Which one reverts alone.** Round 2. Dropping it leaves round 1's columns sitting unread, exactly as they
were the day round 1 shipped — inert, no data loss, goldens unmoved. Round 1 does not revert once round 2 has
run: by then the byte keys *are* the identity, and removing them is a second migration that has to invent
display text out of bytes it cannot un-lossy. That asymmetry is the argument for this order, not a preference
about which round gets the interesting work.

**What must not be split out.** Any subset of §3's shared list on its own: re-keying `note_id` while
`mark_stale` still matches display text moves the bug rather than fixing it, and a partially re-keyed database
is precisely the state in which two rows and one note disagree silently. Nor may round 1 define the byte fold
without naming its owner — `case_bytes` is derived beside `walk`'s display fold from the moment it exists,
or round 2 will find two rules doing one job in two modules.
