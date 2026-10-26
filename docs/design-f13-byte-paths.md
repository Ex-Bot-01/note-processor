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
derived lossy display. Identity statements move to `rel_bytes`, display sites — the rendered target
(`catalog.rs:163-177`) and the report lines (`scan.rs:113-127`) — keep reading text and stay honest because they
were always display-only. Ordering is *not* in that set: `sort_key`'s tie-break moves to bytes with the rest of
the identity work (§3). Four things in
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
  filesystem that pair cannot both exist.
  **What that costs, said plainly instead of waved through.** The old fold did two jobs and the ASCII byte
  fold keeps only one. It keeps P3's `skipped` report (`a.md` beside `A.md` still collides) and it keeps
  rename-following for *ASCII* case changes; it does not keep rename-following for a non-ASCII one. On a
  case-insensitive filesystem `Ämli.md` → `ämli.md` alters no byte in `A`–`Z`, so the re-keyed `folded_row`
  — which matches `case_bytes` after this change (§3's list) — no longer finds the row that today's Unicode
  `case_key` finds, and no rename is attempted. If the content changed as well, `classify`'s hash pairing
  cannot bridge them either (different hashes mean no move claim, `change.rs:128-137`), so the note detaches:
  a freshly minted `notes.id` under the new spelling, the old row left behind and marked `stale` in that same pass
  (`mark_stale` runs inside `apply_changes`, `scan.rs:551-555`), and every edge into the old note reported unresolved
  until its sources re-parse. This plan accepts that regression rather than claiming the fold is free: there is
  no byte spelling of a Unicode case fold, so one column cannot both keep P3's ASCII guard and keep Unicode
  rename-following. §5 pins the detached-note behaviour as it will be, so a later fix has something to break.
- **The display fold stays Unicode, because it is a different fact.** `links::resolve` places a target by
  `case_key` with `target.to_lowercase()` (`links.rs:405-414`), which folds `ÄPFEL` onto `äpfel`; that is a
  question about how an author spells a name, not about which file it is, and it must not move with the
  column above. So `case_key` (TEXT, Unicode fold of display text, feeds link resolution) and `case_bytes`
  (BLOB, ASCII fold of OS bytes, identity guard) are **two facts with two names**, not two spellings of one
  fact — and both are derived in one place, the walker (`walk.rs:235-241`), under the `index_names` lesson
  from F11 (`crates/np-core/src/index_names.rs`): no call site hand-derives either, and neither is written
  without the other. That one place gets a name rather than staying a paragraph: `crates/np-core/src/path_keys.rs`,
  holding `rel_bytes` (OS bytes of the component list), `case_fold` (the ASCII byte fold), `display` (the lossy
  text) and `display_fold` (`str::to_lowercase`). The walker calls it, and migration 4's copy loop calls the same
  functions; §7 makes the module a round-1 deliverable because round 1 is the round that writes keys. `case_key UNIQUE` disappears with that division of labour — twins share a display fold by
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
  `case_bytes` in the same statement as the display pair, so the four columns cannot diverge — **and updating
  `notes.rel` for that row's bytes in the same step**, because migration 4 removes the foreign key that used to
  maintain it (§4). A rename that leaves `notes.rel` behind publishes the old target forever: `catalog_entries`
  renders `n.rel` (`scan.rs:866` → `catalog.rs:163-177`) and nothing else would notice. Say which value that write
  matches on: `notes.rel_bytes` has **already moved** by the time this statement runs — it rides `files`' own update
  — so the note is matched on the *new* bytes, not the old.
- **`rebase_abs`** (`db.rs:779-796`) — suffix surgery on the display `abs`. It stays text work, because its
  output is the display column; but it must run off the byte key's own row, and rebasing a *shared* display
  spelling hands one new `abs` to two files. What (b) changes about `abs`, said so that it cannot contradict
  §4's SQL: **the column stays and keeps being written** — migration 4 copies it verbatim (`db.rs:72`) and
  `upsert_file` binds lossy text into it exactly as today (`db.rs:653`). What stops being stored is the
  **byte-exact** absolute path: nothing persists root + bytes anywhere, so a caller that needs the real path
  recomputes it from `rel_bytes`, and `abs` becomes genuinely cosmetic instead of "display, plus the authority
  nobody reads".
- **`note_id`** (`scan.rs:824-831`) and the same statement inside `links::store_links` (`links.rs:504-520`)
  — `SELECT id FROM notes WHERE rel = ?1 LIMIT 1`. The `LIMIT 1` is what makes today's answer silently wrong
  instead of loud: with twins it hands one file's text to the other note, rewriting its title, tags, items
  and links. Fix: select by `rel_bytes`, and **drop the `LIMIT 1`** so a future duplicate fails with
  `ExecuteMultipleValues` rather than picking a row; §5 pins the one-note-per-file assumption that today's
  `LIMIT` hides.
- **`store_note`'s insert** (`scan.rs:769-773`) — `INSERT INTO notes (rel, title, title_source, encoding,
  word_count, body)`, and the **only writer of `notes.rel`** in the program. It is a writer rather than a reader,
  which is why it belongs on this list at all: under (b) that statement is what puts values into the note side of
  the identity, and leaving it alone means every note created after the switch carries no byte key — NULL in a
  column whose re-keyed join (`scan.rs:864-870`), foreign key + `ON UPDATE CASCADE` (§4) and re-keyed `note_id`
  all compare on bytes, so a keyless note row matches nothing and silently detaches from its file. That is F13
  reproduced by the fix, on every workspace, starting with the first new note. Fix: name `rel_bytes` in that
  column list and bind it **as bytes** (`Vec<u8>`, §4's `CHECK`) beside the display `rel`, and move the insert
  behind a bytes-typed call in `db.rs` so no `scan.rs` site can write a note key at all (§4's writer list).
- **the catalog join** (`scan.rs:864-870`) — `FROM notes n JOIN files f ON f.rel = n.rel`. Two rows on each
  side of one display spelling is a cross product, so the catalog gains duplicate entries carrying wrong
  link counts. Fix: join on `rel_bytes`; §5 covers the ordering this exposes.
- **the catalog's order** (`scan.rs:869`, `catalog.rs:132-139`) — `ORDER BY n.rel` and `sort_key`'s final
  tie-break `(bucket, lowercased title, rel)` are both display-keyed, and §5's ordering test needs byte keys in
  both places: for twins the display text is *identical*, so a display tie-break orders nothing and the rendered
  bytes fall to whichever row SQLite happened to read. Fix: `ORDER BY n.rel_bytes`, and the same byte key as
  `sort_key`'s last component — which requires something byte-typed to order on, because `CatalogEntry` is built
  from `rel` alone today (`scan.rs:904-913`). It gains `rel_bytes: Vec<u8>`, read by `sort_key` only and never
  rendered (`entry_line` prints `rel`, `catalog.rs:163-177`), so the goldens stay byte-identical. Construction
  sites to change: `scan.rs:904-913` **and** the full struct literal in `tests/catalog.rs:29-38`, which needs
  `rel_bytes: rel.as_bytes().to_vec()`; the other test entries build through `..entry(..)` and follow for free.
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
  `case_bytes` and return `rel_bytes`, so only a genuine ASCII-case twin is offered as a rename source — at the
  price named in the residual above: a non-ASCII case rename stops being followed once this matches bytes.
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

**Statement order, so the gate can actually run:** create `files_new`, copy it, drop `files`, rename
`files_new`→`files`; then create `notes_new`, copy it, drop `notes`, rename `notes_new`→`notes`; recreate
indexes; **then** run `PRAGMA foreign_key_check`. The gate must come after both renames — `notes_new`'s foreign
key names `files`, a table that does not exist between the drop and the rename, so checking earlier reports a
missing parent rather than a broken row.

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
`CREATE INDEX files_case_key ON files(case_key)` **and a new `CREATE INDEX files_rel ON files(rel);`** — round 1
resolves rows with `SELECT id FROM files WHERE rel = ?1` once per readable file (§7), and `mark_stale` matches the
same column (`db.rs:719`), so without an index every scan does a full table scan per file inside the 1800 ms
write-phase budget (`db.rs:36-50`) and no existing fixture is large enough to notice. Round 2 may drop it when the
display lookups go; the same shape for `notes` with `notes_rel_bytes` beside
the existing `notes_rel` (`db.rs:98`) — by then a lookup index only, since §3 moves the catalog's ordering onto
`rel_bytes` in round 2; keeping it costs nothing and dropping it would be a second change to reason about. Two things are
gone and one is new: `files.case_key UNIQUE` and `notes.rel`'s foreign key disappear — twins share a display
fold and a display spelling by definition, so uniqueness belongs to `case_bytes` alone (§3) — while
`case_key` itself stays, copied verbatim rather than re-derived, because SQLite's `LOWER()` is ASCII-only and
recomputing it in SQL would silently rewrite every non-ASCII name's fold, breaking the link resolution that
reads it. Read the `DROP`s before going further.

**Losing `notes.rel`'s foreign key loses its only writer.** Today `ON UPDATE CASCADE` (`db.rs:89`) is the sole
thing keeping `notes.rel` in step with a renamed file, and the catalog renders exactly that column
(`scan.rs:866`). Once migration 4 drops the constraint nothing maintains it, so round 1 must name a replacement
rather than let the column drift: re-keyed `apply_move` writes `notes.rel` for the row's own `rel_bytes` in the
same step that rewrites `files.rel` (§3). The alternative — `catalog_entries` selecting `f.rel` instead of `n.rel`
— keeps one fewer writable copy but moves a reader, which is round 2's business; round 1 takes the writer. §5 pins
a renamed note's catalog target and §6 lists it as a Done-when item, because a silently stale target is the same
class of bug this whole item exists to end.

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
louder half of the pair by comparison: `notes.rel REFERENCES files(rel)` carries no `ON DELETE` action, so
dropping a parent that still has note rows fails outright — **louder, not safe**, for the reason below.

**`DROP TABLE files` has the same shape, and neither drop can vouch for itself.** `files` is the parent, so it
fails only where a *note* row still points at it. An attachment — a `files` row with no note — has no child at
all, so nothing checks its survival: if the Rust copy loop loses one (a conversion that skipped a row, a bound
parameter that silently mismatched), `PRAGMA foreign_key_check` still passes afterwards, because there is no key
left to violate, and `DROP TABLE files` destroys the row with no error, no report line and no way to notice
except a catalog entry that never comes back. The same argument covers `notes`: an id whose children were never
written has nothing left to cascade. So the step asserts **before each drop** that the replacement table holds
exactly as many rows as the one it is about to destroy — `SELECT COUNT(*) FROM files` against
`SELECT COUNT(*) FROM files_new`, then the same pair for `notes` — and returns its own error naming both counts
when they differ, which rolls the step back. A count cannot prove that the *right* rows survived; that is what
§5's per-row key comparison is for. The count is what turns a silent shrink into an aborted migration.

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

**Two owners, both named: one for the folds, one for the binds.**

- **The folds belong to `crate::path_keys`** (`crates/np-core/src/path_keys.rs`, §3): `rel_bytes`, `case_fold`,
  `display`, `display_fold`. Nothing else in the crate defines a fold; the walker calls these and migration 4's
  copy loop calls the same ones, so a key is never derived twice under two rules. This module has to exist before
  any statement writes a key, which is why it is round 1's first deliverable (§7).
- **The binds belong to `crate::db`.** The writers of the three key columns are `upsert_file` (`db.rs:636`,
  `files.rel_bytes` + `case_bytes`), `apply_move` (`db.rs:685`, both, in the same statement as the display pair),
  **`store_note`'s insert** (`scan.rs:769-773`, `notes.rel_bytes`) — which today lives in `scan.rs` and moves
  behind a bytes-typed `db` call so no scan-side site can bind a key — and migration 4's own copy loop.
  `mark_stale` (`db.rs:718`) is **not** on that list: it writes only `state` (`db.rs:719`) and merely *takes* a
  byte key to match on. An earlier draft of this section counted it as a writer while missing the one above,
  which is exactly how a lookup gets mistaken for the statement that has to change.

That split is what constrains §3's shared list: the statements in it that live in `scan.rs` — `note_id`,
`folded_row`, `known_rows`, `catalog_entries`, `live_corpus` — move behind a bytes-typed call in `db.rs` or take
a `&[u8]` parameter, so "bind this key as TEXT" stops being a call any of them can write. The `CHECK` is the
backstop; the API shape is the design. §5 states the check for both halves of this claim, because "one module
owns it" and "no cast survives" are otherwise unfalsifiable sentences.

Byte-stability of the copy rests on the same two things R2b ruled on (`fix(db): copy
migration rows in rowid order…`, `3f86529`): `ORDER BY rowid` on every copy, so the rowids a rebuild hands
out cannot reorder `note_tags`' `ORDER BY note_id, rowid` (`scan.rs:936-938`) and thus published tag bytes;
and `notes.id` carried rather than re-minted, because every derived row hangs off it (`db.rs:1-8`). Only
`files` and `notes` are rebuilt — `tags` is untouched, so no tag rowid moves at all. A receipt belongs in
`meta` beside `fk_orphans` (`db.rs:341`): `paths_lossy = 'files=N,notes=M'`, counting the rows whose display
text holds U+FFFD, written even when zero, for the same reason F5's is ("looked and found none" must be
tellable apart from "never upgraded") — and read as §4.2 describes, because that count is the only way a
person can tell the first byte-keyed scan's expected churn from a regression.

Parse-version interaction: this is schema-only, like every entry here (`docs/backlog-m2-followups.md`,
F15: "a SQL migration cannot do this — migrations here are schema-only"), so `PARSE_VERSION` stays `1`
(`crates/np-core/src/parse.rs:42`) and `refresh_needed` must not fire (`scan.rs:1043-1050`). Derived rows
are not rewritten by migration 4: carry `hash`, `size`, `mtime`, `state` verbatim, and derive display text
with the *same* lossy rule so `classify` sees the same `rel` strings it saw before. That buys "the first
post-migration scan reports nothing changed" for **every** workspace — including one holding a non-UTF-8 name,
because migration 4 changes no comparison and no reader looks at the new columns yet (§7). §4.2 is what happens
in the round that does read them, and §6 carries both halves.

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

### 4.2 The churn, and which round owns it

**What the migrated key is.** Migration cannot invent bytes it never held (§2), so an existing row's
`rel_bytes` is the UTF-8 encoding of its stored display text. For an ASCII or valid-UTF-8 name those are the
OS bytes, and nothing moves: `classify` matches on the same spelling, `hash`/`size`/`mtime` were carried
verbatim, the scan reports every file unchanged, and the goldens are byte-identical.

**Which round owns it: not migration 4.** Migration 4 changes no comparison, so the scan immediately after it
reports **exactly what the previous scan reported**, in every workspace — including one whose real name is
`caf\xe9.md`, because `classify` still matches on display text (`change.rs:164-169`) and that text did not move.
"Exactly what it reported before", not "unchanged": a twin workspace was reporting `added: 2` before migration and
will keep reporting it after, because the bug is untouched until round 2.

**Round 1 is silent too, and for the same reason: it writes no OS bytes.** Its writers receive display strings
(§7), so a round-1 scan re-binds `rel_bytes` to the UTF-8 encoding of the display spelling — and for a migrated
lossy row that is the *same* byte sequence it already holds, since every readable file is upserted on every scan
(`scan.rs:471-473`, called at `scan.rs:605`). Nothing churns, nothing is repaired, and no reader compares keys in
any case. §5 pins that silence as a fact to test rather than a promise to keep.

**All of the churn therefore lands in round 2**, whether or not the workspace was scanned in between — there is
no route by which keys "arrive already correct", because nothing before round 2 ever writes OS bytes. The first
byte-keyed scan of a lossy workspace is where a migrated `63 61 66 EF BF BD 2E 6D 64` finally meets the walk's
`63 61 66 E9 2E 6D 64`, and that is the event the next paragraph describes.

**What the first byte-keyed scan of a lossy workspace reports.** A row migrated from `caf\uFFFD.md` holds
`63 61 66 EF BF BD 2E 6D 64`; the walk computes `63 61 66 E9 2E 6D 64`. Two byte keys, one file on disk,
identical display text. Because `classify` pairs unmatched snapshots **by hash** (`change.rs:70-76`, with the
claims resolved at `change.rs:122-149`), unchanged content means equal hashes and therefore a single
**`Moved { from }`**, not a `Removed` + `Added` pair. It is a *repeated* `Moved` until the byte key is actually
rewritten: `from` and `to` carry the same display spelling here, so a write pass that still expresses a rename
in display text renames the row onto itself, leaves `rel_bytes` holding the lossy value, and pairs the same two
keys again next scan — forever, which is §1's never-converges symptom manufactured rather than fixed.
§4.1's byte-carrying change list and §3's re-keyed `apply_move` are what stop the repeat. Where the content
changed as well, the hashes differ, no move claim exists (`change.rs:128-137`), and the shape is the original
**`Removed` + `Added`**. On §1's actual workspace — one row carrying `caf\xe9.md`'s digest, two real files on
disk — the first byte-keyed scan therefore reports one `Moved` (the path whose hash survives) plus one `Added`
(the twin nobody ever wrote), which is the swallowed path coming back that §2 said needs a rescan. Either shape
is honest churn rather than new loss, and it converges: a `Removed` row goes `stale`, keeps its children and
leaves the catalog (`catalog_entries` filters `f.state = 'ok'`, `scan.rs:864-870`), while the byte-keyed row is
live from this scan on.

Two consequences of that one-time event are worth naming rather than discovering. Where the shape is `Removed`
+ `Added`, the re-added note is a **fresh `notes.id`**, so edges pointing into the old row now name a stale file
and land in `unresolved_links` (`LIVE_EDGE`, `db.rs:805-807`) until their sources are re-parsed — which happens
when a source's content changes or when `PARSE_VERSION` moves, not on this scan — and its derived rows come from
a re-parse rather than from the carried-over ones, so a workspace that had hand-edited children on the stale row
keeps them in the database but publishes the re-derived set. Where the shape is `Moved`, the id survives — but
not by cascade: migration 4 removes `notes.rel`'s foreign key, so **`apply_move` itself** carries the note, by
writing `notes.rel` for the row's bytes (§3, §4). Nothing detaches — one more
reason §4.1's byte-carrying change list is not optional, since a display-keyed pass turns what should have been
the quiet shape into the loud one, on every scan.

**How it reads against the receipt.** This is what `paths_lossy = 'files=N,notes=M'` is for. It names the
workspaces holding rows whose display text contains U+FFFD, so a person can predict about `N` paths reporting a
`Moved` — or a `Removed` + `Added` pair where the content changed too — on the first byte-keyed scan, and
explain them afterwards; where the receipt reads `files=0`, a `Moved` for a path nobody renamed is a bug rather
than a migration artefact. That makes the receipt worth pinning in §5's migration test, not merely writing.

## 5. Test plan

Every bullet here that builds a name with `OsStrExt::from_bytes` — the round trip, both twins, the ASCII-case
half of the fold test, the twin ordering and resolution items — carries `#[cfg(unix)]`, as the existing non-UTF-8
tests already do (`crates/np-core/tests/walk.rs:144`, `crates/np-core/tests/config.rs:564`). Without it the same
test compiles on Windows into a name that cannot be built, and passes by asserting nothing.

- **Latin-1 round trip** *(round 2 — this is the byte-exact assertion round 1 cannot make)*. Build a name with
  `OsStrExt::from_bytes(b"caf\xe9.md")`, scan, and assert
  `files.rel_bytes` equals those exact bytes and that rebuilding a `PathBuf` from them opens the file —
  the registry's answer to `workspace_db_separates_a_non_utf8_path_from_its_lossy_spelling`.
- **Both paths resolve** *(round 2 — twins still collapse until readers key on bytes)*. `caf\xe8.md` +
  `caf\xe9.md`: two `files` rows, two `notes` rows, two catalog
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
  - **The residual that goes with it, pinned as behaviour.** A case-insensitive filesystem renaming `Ämli.md`
    onto `ämli.md` — no byte in `A`–`Z` changes — must be shown to miss `folded_row` (§3 re-keys it onto
    `case_bytes`), and the same pair with changed content must be shown to miss `classify`'s hash rule, so the
    note detaches: a fresh `notes.id`, the old row left behind and marked `stale` in that same pass (`Removed`
    runs inside the transaction), edges into it reported unresolved. Assert exactly that, so the accepted regression is evidence rather than a
    paragraph (§3's residual). Two seams make this awkward on purpose, and the test names them: `folded_row` is
    private (`scan.rs:704`), so this half lives in a `#[cfg(test)]` unit module beside it rather than in the
    integration tests; and the scan path hardcodes case sensitivity as `!cfg!(windows)` (`scan.rs:193`,
    `scan.rs:202`), so no Linux CI can drive it through `scan::run` — the `classify` half goes through
    `crates/np-core/tests/change.rs`, which already passes the flag by hand (`crates/np-core/tests/change.rs:10`).
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
  assumption stops being hidden by a `LIMIT` and starts being tested. Same query, one assertion weaker and more
  important: **no `notes` row has `rel_bytes IS NULL`** after any write pass — the new-note insert of
  `scan.rs:769-773` is the only thing that can produce one (§3), and a keyless note matches nothing in the
  re-keyed join or cascade. The column stays nullable because migration 4 copies rows it did not write and must
  not abort on them (`db.rs:85-92` allows a NULL `rel` today), which is precisely why the property is tested
  here instead of constrained in the schema.
- **Two claims about the code, each with a stated check.** "No `CAST` survives" and "one module owns both folds"
  are otherwise unfalsifiable, so both get a grep a person can run: (a)
  `grep -n "CAST(" crates/np-core/src/db.rs` matches only migration 4's own notes copy (`CAST(rel AS BLOB)`,
  §4) and nothing in the steady-state write path — `upsert_file`, `apply_move`, the moved note insert;
  (b) **no fold *definition over paths* lives outside `path_keys`**, which the grep answers with a named list
  rather than an empty set: `grep -rn "fn .*fold" crates/np-core/src/*.rs` returns `path_keys::case_fold` and
  `path_keys::display_fold` — the two definitions — plus exactly two known non-definitions, `items::folded_eq`
  (character equality while matching item text, nothing to do with paths) and `scan::folded_row` (a **reader** that
  looks a row up by `case_key`, which round 2 re-keys). Anything else in that output is a second definition and has
  to be folded into `path_keys`. Round 1 restated this check twice: the shipped spelling banned `to_lowercase`
  outside `path_keys`, which is unsatisfiable, because the display fold is legitimately computed in `links.rs`,
  `catalog.rs`, `parse.rs`, `items.rs` and `walk.rs` — §3 keeps that fold Unicode and inside the readers, so banning
  the token would have banned round 2's own code and contradicted §3 two pages up. The row-level assertion below is
  what actually binds the two columns; the grep only guards against a second definition appearing.
  `case_bytes = path_keys::case_fold(rel_bytes)` and `case_key = rel.to_lowercase()`, which is what "the same two
  functions wrote both columns" means when it is tested.
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
  written, and a file with a dangling `notes.rel_bytes` refuses to open. Six things this bullet has to cover:
  - **Pin the row counts of all four child tables** — `headings`, `tags`, `items`, `links` — before and after
    migration 4, not merely "a stale note keeps its children". Those are the four cascades §4's `DROP TABLE`
    hazard runs through, and a partial cascade that still leaves *some* children would pass the weaker claim.
  - **Pin the counts of the two rebuilt tables too**: `COUNT(*)` of `files` and of `notes`, old versus new,
    before each drop (§4). A child-table count cannot see a lost *attachment* row — it has no children, so
    nothing cascades and `PRAGMA foreign_key_check` stays clean while the drop destroys it. Include a fixture
    holding one attachment with no note row, which is exactly the row only this assertion can catch.
  - **Pin the refusal**: run the step with `PRAGMA foreign_keys = ON` and assert it errors out having touched
    nothing — child counts unchanged, `schema_version` unmoved, the file byte-identical if that is cheap to
    assert. This needs a named direct-call seam, because nothing reachable through `Db::open` can observe it:
    `migrate` forces the pragma off before the loop (`db.rs:301`) and the pragma has no effect inside a
    transaction (`db.rs:296-303`). The seam already exists — `MIGRATIONS` is a `pub const` (`db.rs:64`) of a
    `pub enum Migration` (`db.rs:153`), so the test destructures the fourth entry
    (`let Migration::Code(step) = MIGRATIONS[3]`, the step fn named `byte_path_keys` in §4) and calls it on a
    connection whose pragma the test set to `ON` outside any transaction.
  - **Pin that the `foreign_key_check` gate refuses, not merely passes.** §4's gate is a defence only if
    something can be turned away by it, so hand-build a version-3 fixture holding one `notes` row whose `rel`
    names no `files` row — writable with the pragma off, which is how orphans got in once enough for migration 3
    to count them (`FK_ORPHANS_KEY`, `db.rs:341`) — and assert the step returns its own error, leaves
    `schema_version` where it was and changes no count. A test that only ever runs the gate on clean data proves
    nothing about it.
  - **Pin that every key came from its own row**: after upgrade, each row's `rel_bytes` equals the bytes of
    that same row's `rel`, across several rows with different names. One bound parameter for `case_bytes`
    passes a one-row fixture and fails this.
  - **Pin the receipt** (`paths_lossy`) on a fixture holding one U+FFFD row as well as the zero case.
- **Round 1's first scan is silent — and its keys are still lossy.** On a round-1 build (keys written, readers
  still display-keyed) over the `caf\uFFFD.md` row above: the scan reports every file unchanged — no `Moved`, no
  `Removed`, no `Added` — *and* `files.rel_bytes` holds `63 61 66 EF BF BD 2E 6D 64`, the bytes of the display
  spelling, **not** `63 61 66 E9 2E 6D 64`. Round 1 derives keys from display text and never from `abs` (§7), so this
  assertion is exactly as
  faithful as it can be; the byte-exact form belongs to the round-2 test below. The silence is still §4.2's claim
  that migration changes no comparison.
- **A rename carries the note's display target with it.** Round 1 removes `notes.rel`'s foreign key and its
  `ON UPDATE CASCADE`, which is the only thing that maintained that column today (`db.rs:89`), so rename one file
  on a round-1 build and assert three things: `files.rel` moved, `notes.rel` for those bytes moved with it, and
  `INDEX.md` renders the **new** target. If the writer is missed, the catalog keeps publishing the old path
  forever and nothing else in the suite notices — this is the pin that makes that impossible.
- **Migration, then scan.** On that hand-built database holding a row displayed as `caf\uFFFD.md` over a
  workspace whose real file is `caf\xe9.md`, driven straight into a round-2 build with no scan in between — the
  only route where a migrated lossy key is ever compared (§4.2): the first byte-keyed scan reports exactly one
  `Moved` and nothing else, because the content did not change and `classify` pairs by hash (`change.rs:70-76`);
  the receipt named that workspace beforehand; `notes.id` survives the move and keeps its children; the scan after
  that reports nothing changed, which is the assertion that the byte key was actually rewritten rather than
  re-derived every time. Then the same fixture with the file's **content** changed as well: there the first scan
  reports one `Removed` and one `Added`, the re-added note carries a fresh `notes.id`, the stale row keeps its
  children and leaves the catalog, and the edges into the old note appear in `unresolved_links` (§4.2). A third
  shape is worth pinning because it is §1's real workspace: one migrated row carrying `caf\xe9.md`'s digest with
  both twins on disk gives one `Moved` plus one `Added`, and two live rows by the second scan. Without these,
  §4.2's promise is a paragraph rather than a fact — and the `Removed`-only version of this test, which is what
  this plan first claimed, would fail on unchanged content.
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
- [ ] A rename updates `files.rel`, `notes.rel` for those bytes and the rendered catalog target together — with
      no foreign key left to do it for them (§4, §5).
- [ ] No collision is silent any more: either both rows exist, or the path appears in `skipped` under a
      spelling that identifies it (or the report says plainly that two paths share a display name).
- [ ] Byte identity travels the whole pipeline: `DiscoveredFile`, `Snapshot`, `key_of`/`index_by_key`,
      `classify`'s change list and `apply_changes`' pending map all key on bytes (§4.1). No structure between
      walk and write keys a file on its display text.
- [ ] Every statement in §3's shared list is re-keyed — `mark_stale`, `apply_move` (both its statements),
      `note_id` in `scan.rs` and in `links.rs`, the catalog join, `LIVE_EDGE`, `known_rows`' `EXISTS`,
      `folded_row`, `live_corpus` — and `upsert_file`'s conflict target is `rel_bytes`. One of them left
      display-keyed is the bug, not a subset of the fix.
- [ ] Every **writer** of a key writes it: `upsert_file` and `apply_move` bind `rel_bytes`/`case_bytes`, and
      `store_note`'s insert (`scan.rs:769-773`) names `notes.rel_bytes` and binds it as bytes beside the display
      `rel` (§3 — a note created without a key matches nothing in the re-keyed join, FK or cascade). After any
      write pass, no `notes` row has `rel_bytes IS NULL`.
- [ ] Every ordering site keys on bytes: `ORDER BY n.rel_bytes` (`scan.rs:869`) and `sort_key`'s last component
      (`catalog.rs:132-139`), which means `CatalogEntry` carries a byte-typed `rel_bytes` to order on
      (`scan.rs:904-913`) that nothing renders.
- [ ] All three key columns carry `CHECK(typeof(…) = 'blob')`, every bind of a key is `&[u8]`/`Vec<u8>`, and
      no `CAST` survives in the `files` write path — checked by the grep §5 states, not by rereading this file.
- [ ] Migration 4 is one `MIGRATIONS` entry, rebuilds only `files` and `notes`, copies `files` rowid-by-rowid
      in Rust with a per-row byte fold (no `INSERT … SELECT`, no single bound key), refuses to run when
      `PRAGMA foreign_keys` reads `1`, asserts old-vs-new `COUNT(*)` before **each** drop (both `files` and
      `notes`, so a lost attachment cannot be dropped away silently), pins all four child-table counts, gates on
      `PRAGMA foreign_key_check` — with a test that shows the gate refusing, not just passing — and writes its
      receipt.
- [ ] Migration 4 itself reports nothing changed in **every** workspace, flagged or not (§4.2); the churn §4.2
      describes belongs to the round that first compares byte keys, where a workspace with `N` flagged rows
      reports about `N` `Moved` paths (or `Removed` + `Added` where content changed too) and converges on the
      scan after.
- [ ] Catalog order is deterministic for twins, from the byte key in `ORDER BY` and `sort_key`, not from a
      rowid nobody chose.
- [ ] Every committed golden in `tests/golden/` is unchanged, byte for byte, in the same commit.
- [ ] One named module owns the (bytes, display) pair *and* both folds — `crate::path_keys`, holding
      `rel_bytes`/`case_fold` and `display`/`display_fold` (§4); no call site hand-derives display text from
      bytes or a fold from either, and §5's grep is the check that this stayed true.
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

**Round 1 — schema plus the statements that write keys; no reader touched.** Migration 4 exactly as §4
describes it (both tables rebuilt with byte keys and their `CHECK`s, the rowid loop, the per-drop count
assertions, the receipt, the `PRAGMA foreign_keys` guard), **plus** `path_keys` (§4) and the three statements
that put values into key columns: `upsert_file`, `apply_move` and `store_note`'s insert (§3), each deriving both
keys through that one module and binding them as bytes. Reads stay display-keyed: **no *reader*** in §3's shared
list changes — `apply_move`'s two statements are writers, which is why they belong here — and every reader moves in
round 2, together with the question it asks.

**Round 1 must also replace `upsert_file`'s conflict arm.** Not by choice: migration 4 gives `rel` no
`PRIMARY KEY` and no `UNIQUE` — `rel TEXT PRIMARY KEY` (`db.rs:70`) is the only reason today's
`ON CONFLICT (rel) DO UPDATE` (`db.rs:640`) prepares at all — and only non-unique indexes are recreated, so the
clause fails with *ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint* and every scan dies on
the first file it writes. The replacement stays display-keyed, which is why it does not cross into round 2:
`SELECT id FROM files WHERE rel = ?1`, then `UPDATE … WHERE id = ?` when a row exists and a plain `INSERT` when
none does. Two statements resolved by `id` instead of an arm SQLite can no longer match; twins are still not
distinguished, because nothing reads the byte keys yet — exactly what round 1 cannot prove.

**Why the writers cannot wait for round 2.** Two reasons, either of them fatal to a schema-only round 1.
First, it would not run at all: `rel_bytes` and `case_bytes` are `NOT NULL UNIQUE` (§4) and `upsert_file`
(`db.rs:638-645`) supplies neither, so the first upsert after migration 4 fails with
`SQLITE_CONSTRAINT_NOTNULL` — which `is_identity_conflict` (`scan.rs:690-702`) does not accept as an identity
clash, so it is not a `skipped` entry but a scan that ends in error, on every workspace, from the first file it
writes. Second, migration 4 drops `files.case_key UNIQUE`, and that constraint is what rules P3's `skipped`
report today (`db.rs:72`, `scan.rs:688-702`); a round that removes it before anything writes `case_bytes` deletes
P3 outright, so `a.md` beside `A.md` would get two live rows and one note. Writing both keys in round 1 keeps the
guard at ASCII-fold strength from the moment the old one goes away.

What may not move in round 1 is **identity**, which is not the same as keeping the arm: resolving rows by
`rel_bytes` while `note_id`, the catalog join and `known_rows` still match display text creates exactly the
half-moved state this section refuses — two rows sharing one display spelling, one note between them, and a
`LIMIT 1` deciding which file owns it. Resolving by `id` through a display-keyed `SELECT` keeps writes and reads
answering the same question, "which row spells this path", until round 2 changes that question for everyone at
once.

**What round 1 proves on its own.** Every golden byte-identical; every migrated row's `rel_bytes` equals the
bytes of that row's own display `rel`, and **so does every row a round-1 scan writes** — the writers derive keys
through `path_keys` from display text, because their identity input is display text: `Snapshot` holds strings
(`hash.rs:25-38`) and `store_note(rel: &str)` (`scan.rs:736`) and `apply_move(&str, &str)` (`db.rs:685`) take
strings. One writer *does* have bytes in reach — `upsert_file(db, &Snapshot, abs: &Path)` (`db.rs:636`) is handed
the byte-exact path today and only uses it as `to_string_lossy` (`db.rs:652`) — so a rule is needed to keep round 1
honest: **round 1 derives both keys from `s.rel`, never from `abs`.** Deriving them from `abs` would quietly make
keys byte-exact for new and rewritten rows while readers still match display text, which is the half-moved state this
split refuses, and it would contradict §4.2 and §5's assertion that round-1 keys stay lossy. Round 1's keys are a
*faithful copy*, not OS fidelity; the
assertion that a row reads `63 61 66 E9 …` belongs to round 2 with §4.1's first three bullets. What round 1 does
prove: a version-3 file opens, passes `PRAGMA foreign_key_check`, keeps all four child-table counts plus
`COUNT(*)` of `files` and `notes` old-versus-new and every `notes.id`; the receipt is written even at zero; the
step refuses when enforcement is on, and refuses when the `foreign_key_check` gate finds a violation. Two
report-line facts belong here as well, and neither is "nothing": an ASCII-case pair (`a.md` beside `A.md`) still
lands in `skipped`, caught by `case_bytes UNIQUE` where `case_key UNIQUE` caught it before; and a Unicode-only
pair (`Ämli.md` beside `ämli.md`) **stops** being skipped and gets two rows, because the fold that caught it left
with the schema (§3's residual). A non-UTF-8 workspace reports nothing changed, because no reader compares the new
columns yet (§4.2) — and the word to avoid about that is *repaired*: after round 1 those rows still hold the bytes
of their own lossy spelling, and only round 2 gives them the path's real bytes.

**What round 1 cannot prove.** Nothing about identity. Twins still collapse into one row, because `classify`,
`known_rows` and the pending map still key on display text and the upsert still conflicts on `rel`. F13 is still
open at the end of round 1; that is what makes the split honest rather than a way of shipping half a fix twice.

**Round 2 — byte identity end to end.** §4.1's pipeline change plus every statement in §3's shared list,
in one commit: the walk emits bytes, the digest carries them, `classify` indexes on them, `apply_changes`
keys its pending map and its writes on them, `upsert_file`'s conflict target moves to `rel_bytes`, the catalog
orders on it (`ORDER BY n.rel_bytes` at `scan.rs:869`, `sort_key` and `CatalogEntry.rel_bytes`), and the display
fold stays only where authors look. Proves on its own: §6's twin items — two rows, two notes, two catalog
entries, both files openable from what was stored, the second scan reporting zero changes, twins landing in
`unresolved_targets` rather than resolving by accident, deterministic catalog order — and it owns §4.2's churn
for every workspace that reaches it without an intervening scan.

One hazard arrives with it, and round 2 must not discover it late: **`notes` can legitimately hold two rows for one
path.** `notes.rel` was never `UNIQUE` at version 3, so a legacy database may carry duplicate note rows for the same
file; migration 4 copies both onto the same `rel_bytes`, which has no `UNIQUE` there by design. Every identity lookup
round 2 writes must therefore say what it does with more than one row — an explicit `LIMIT 1` with the tie-break
named, or an aggregate — because a bare `query_row` on a two-row key is not a wrong answer but a scan error. §5 asks
for the two properties that make the population visible (`GROUP BY rel_bytes HAVING COUNT(*) > 1`, and no
`rel_bytes IS NULL` after a write pass) for exactly this reason.

**Which one reverts alone.** Round 2. Dropping it leaves round 1's keys being written and read by nobody — inert,
no data loss, goldens unmoved, exactly the state they were in the day round 1 shipped. Round 1 does not revert
once round 2 has run: by then the byte keys *are* the identity, and removing them is a second migration that has
to invent display text out of bytes it cannot un-lossy. That asymmetry is the argument for this order, not a
preference about which round gets the interesting work.

**What must not be split out.** Any subset of §3's shared list on its own: re-keying `note_id` while
`mark_stale` still matches display text moves the bug rather than fixing it, and a partially re-keyed database
is precisely the state in which two rows and one note disagree silently. Nor may round 1 write a key without
naming the module that derives it: `path_keys` (§4) lands in the same commit as the first statement that binds
`rel_bytes`/`case_bytes`, or round 2 will find two rules doing one job in two modules. And the conflict target
may not be moved ahead of its readers, for the reason given above.
