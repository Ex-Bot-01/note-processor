# F22 — a scan's losses get a durable record, not just a line

Status: design for the first round of `m4/durable-records`. Backlog: `docs/backlog-m2-followups.md` F22 (and the
"this note appears twice" half of F23). Prior art to reuse, not reinvent: F21's orphan receipt (`meta` key + value
carried on the open handle + an always-present report field, echoed best-effort) and `PATHS_LOSSY_KEY`.

## 1. The defect

A refusal is a fact with no afterlife. When `np scan` declines to index a name the byte spelling cannot hold (F18), or
cannot enter a directory, the only evidence is one line on stdout or stderr at the moment it happened. Redirect the
summary away, ask a day later, or run under a service that keeps no log, and nothing in the workspace says any path
was ever unindexable — while a note is missing from the catalog permanently, until someone renames the file.

The single-list choice at `walk::UnknownPath`'s field docs is right for safety and wrong for diagnosis: an unreadable
directory and a name this build refuses print identically, so even the live line cannot tell a permission problem from
an unrepresentable name.

F23 contributes a second silent loss with the same shape: **one physical file can hold two live `files` rows, hence two
catalog entries**, and no signal today says "this note appears twice".

## 2. The rule

**Anything a scan loses is recorded durably in the workspace database, bounded, overwritten by the next scan, and never
load-bearing for a deletion decision.** Streams remain a courtesy (F21 §2). One record, one key, one mechanism — F21's.

The record is *current state*, not a history: it describes what this scan could not index and what the registry currently
duplicates. A scan that loses nothing writes an empty record, so a fixed workspace stops reporting the old problem.

## 3. Shape

One `meta` key, `UNINDEXABLE_KEY = "unindexable"`, value a compact string in the style of `FK_ORPHANS_KEY` (parseable by
a human with `sqlite3`, no new schema, no migration):

```
unreadable=2;unkeyable=1;duplicates=1;names=a/dir,b/dir,c/name.md
```

- **`unreadable`** — directories this scan did not read (permission denied, vanished mid-walk).
- **`unkeyable`** — names the byte spelling cannot hold (F18 refusals). Counted separately from `unreadable`, which is
  the diagnosis F22 exists to add.
- **`duplicates`** — the number of display paths holding more than one live (`state = 'ok'`) `files` row: F23's
  "this note appears twice", computed with one grouped query over `files`.
- **`names`** — up to eight display spellings, drawn first from `unkeyable`, then `unreadable`, then duplicated paths, so
  the most actionable entries survive the cap. Names are display text and may look alike for byte-distinct paths; say so
  in the field doc rather than promising a name that uniquely identifies a path.

In code:

- `crates/np-core/src/scan.rs` builds the record after the write pass from data it already has (`walked.unreadable`,
  `walked.unrepresentable`, plus one query for duplicates) and writes it **post-commit, best-effort** — a refused write
  is degraded, never fatal (R8's rule).
- `ScanReport` gains an additive, always-present field, e.g. `pub unindexable: Option<Unindexable>`, where
  `Unindexable { unreadable: usize, unkeyable: usize, duplicates: usize, names: Vec<String> }`. `None` when everything
  is zero, so the common report stays clean; JSON emits `null` in that case, exactly like `upgrade_orphans`, and the doc
  says so rather than claiming the key disappears.
- The stdout summary prints one line only when the field is `Some`: names counted by kind, not merged, because
  "3 paths skipped" hides that two of them are a permission problem nobody will fix by renaming anything.

## 4. Invariants this round must not break

1. **`skipped` keeps its current entries verbatim.** Adding causes means rewording strings that a dozen tests and any
   real user's `grep` depend on; the cause lives in the new record instead, as separate counts. Do not "improve" the
   report lines.
2. **No deletion decision reads the record.** It is output and diagnosis only. Unknown-is-not-absent stays decided by
   the walk's lists (`known_rows`), never by a `meta` value that a later build might rewrite.
3. **Catalog bytes do not move.** The catalog, its shards, and every file under `tests/golden/` stay bit-identical; the
   record is written after commit and touches no rendering path.
4. **No new stream dependence, no panic on a refusing descriptor 2 or 1** — writes to both are best-effort, matching R8
   and F21.
5. **`duplicates` must not be computed by the fallback's own logic.** It counts registry rows per display path; it says
   nothing about *why*, so it cannot drift into re-deciding F23's ambiguity.

## 5. Tests this round owes

- A scan with an unreadable directory: record written with `unreadable=1;unkeyable=0`, name present, report field `Some`
  with the same counts, stdout line printed once. Skip loudly (`NOT COVERAGE`) if `chmod 000` fails to deny on this
  runner — and re-check denial off disk, per F23's lesson that a green run is not proof a fixture ran.
- A scan where nothing is lost: field `None`, JSON key present and null, and an **existing** record from a previous scan
  is overwritten to empty — the fixed workspace stops reporting the old problem.
- Duplicates: seed two live `files` rows for one display path (the shape F23's harm fixture already produces) and assert
  `duplicates=1` plus that name in `names`. Do not re-derive F23's configuration here; reuse its fixture helper or point
  at it, so the two files cannot drift.
- Refusal counts: on Unix nothing is ever refused, so pin `unkeyable=0` from a hand-built walk list through the record
  builder rather than pretending to produce a refusal — and state that the non-zero case is Windows-only and unexecuted,
  in the same honest terms F18 uses.
- A `meta` write that fails: the scan still succeeds (best-effort), with an assertion that the failure did not become an
  error or a panic.

## 6. Gates

The standing list, no subsetting: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`;
`cargo test --workspace`; `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config`;
`RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps`; the cfg dead-code check; the golden verifier
→ `GOLDEN CHECK: pass`. CI green on Linux **and** Windows is part of "green"; nothing under `tests/golden/` is ever
regenerated.
