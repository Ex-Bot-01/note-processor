# F20 — carry the bytes into the report-side filters

Status: design for round 1 of `m3/report-fidelity`. Backlog: `docs/backlog-m2-followups.md` F20 (widened by F18's
refusal list). Prior art: `docs/design-f13-byte-paths.md` §4.1 (one component walk produces bytes and display text
together — never a second lossy pass), F18 `docs/design-f18-windows-refusal.md`.

## 1. The defect

Two report-side filters compare **display text** where the rest of the program compares bytes:

- `scan::under_unreadable` matches a snapshot's `rel` against `Walked::unreadable`, and both sides went through
  `to_string_lossy`. Two directories whose names differ only in bytes that fold to one `U+FFFD` (`café/` written
  latin-1 beside `caf` + `è`) produce **one** display spelling, so an unreadable or refused twin hides the readable
  one from `known_rows`. Consequences, in order of harm: a real deletion inside the readable directory is never
  reported `Removed`; a rename there produces a **second live row instead of a move**; and one note can be charged
  in both `written.uncovered` and `hidden_by_unknown`, so the refresh-gap line over-counts.
- `scan::is_unknown_path`'s refused-name arm compares display text for the same reason, with the same twin effect
  (F18 accepted that deliberately *on Windows*, where the key bytes are the lossy ones; on Unix the byte key is
  exact and the comparison is simply wrong).
- `skipped.sort(); skipped.dedup()` publishes **one** line for two byte-distinct refused paths that share a display
  spelling — the report cannot name the file it lost.

This is reachable on **Linux today**: non-UTF-8 filenames are ordinary there. F18 widened who enters these lists; it
did not create the matching, and this document closes the matching.

## 2. The rule

**A path that is unknown is carried as bytes plus display text, from the walk that knows the bytes to the filter
that needs them.** Nothing re-derives one from the other on the way. `path_keys::normalize_rel` already produces
`(rel_bytes, rel_display)` in a single component walk; today both list builders throw the bytes away two lines later.

Rejected alternative: keep `Vec<String>` and add parallel `*_bytes` vectors. Two lists that must stay index-aligned
are one `.push()` in one branch away from silently mismatching every entry after it, and no type or test can see it
happen. One struct per path, two fields.

## 3. Shape

In `crates/np-core/src/walk.rs`:

```rust
/// A path this scan cannot judge absent, spelled twice: `display` is what the report prints, `bytes` is what a
/// caller matches on. Both come from one component walk (`path_keys::normalize_rel`), so a path cannot be matched
/// by one spelling and named by another (F13 §4.1).
pub struct UnknownPath {
    pub display: String,
    pub bytes: Vec<u8>,
}
```

- `Walked::unreadable: Vec<UnknownPath>` and `Walked::unrepresentable: Vec<UnknownPath>`. Field docs keep their
  current meaning ("contents this walk did not read" / "name the byte key cannot hold") and gain one clause each:
  matching is by `bytes`, printing is by `display`.
- `unreadable_rel` and `entry_rel` become `unreadable_path` / `entry_path` returning `UnknownPath`. The root case
  keeps its special spelling: `display == "."` **and** `bytes == b"."`, because `"."` means "everything below the
  root" to both matchers. A path that normalises to empty must not produce an empty prefix, which would match every
  path in the workspace — pin that.
- `scan::under_unreadable(bytes, &[UnknownPath])`: true when an entry is `"."` or `rel_bytes` starts with
  `entry.bytes` followed by a `/` byte. `scan::is_unknown_path(rel_bytes, …)`: exact equality on `bytes` for the
  refused arm. Neither function takes `&str` any more.
- `ScanReport::skipped` stays `Vec<String>` — it is output, and its consumers (`np-cli`, the CLI tests) print it.
  Inside `scan.rs`, dedup keys on bytes and sorting orders by bytes, then maps to `display` for the report. Two
  byte-distinct paths with one display spelling therefore print **two identical-looking lines**, which is honest:
  the program saw two paths and can only name them alike.

## 4. Invariants this round must not break

1. **Unix behaviour that is correct today stays bit-identical.** Every golden file, every catalog byte. The change
   is in a comparison most fixtures never reach; if a golden moves, stop and report rather than regenerating.
2. **Unknown is not absent.** No path may become `Removed` because its directory was unreadable or refused — that is
   the invariant F13 §1 and F18 built, and byte matching narrows who is unknown, so the risk is *over*-reporting
   deletions. The existing mode-000-directory and vanished-directory tests must pass unchanged in their assertions.
   **The exception round 1 had to add** (review `d2cbf5de`): a row whose stored key *is* its display text has no byte
   spelling left to match, so it is compared text against text — migration 4 keyed every row through
   `rel_bytes_of_display`, and without the fallback a live note behind an unreadable directory went back to being
   reported `Removed`. Over-hiding such a row is churn; under-hiding it is data loss. The class is **not** only
   migrated rows: a name genuinely holding `U+FFFD`, and every Windows lossy name, qualify permanently — there the
   text arm and the byte arm agree anyway. A migrated row leaves the class when a scan pairs its path by hash so
   `apply_move` rewrites `rel_bytes`; an unchanged row keeps its key forever, so no doc may call this transitional.
3. **No new fold or key definition outside `path_keys`.** This round consumes `rel_bytes`; it defines no spelling of
   its own. If a builder needs bytes it calls `normalize_rel`, never `to_string_lossy().into_bytes()`.
4. **Charge once.** After the fix, one note behind one unknown path is charged in exactly one of
   `written.uncovered` / `hidden_by_unknown`. If byte matching does not by itself remove the double charge, find the
   second charge and pin the behaviour with a test that says which one is authoritative — do not silently leave an
   over-counting gap line.
5. **Public API change is allowed** (`Walked`'s two fields); `ScanReport` and catalog output are not.

## 5. Tests this round owes

- The F20 fixture from the backlog: a workspace holding `café/` (latin-1 bytes) and `cafè/` (latin-1 bytes) — one
  unreadable via mode `000`, both holding notes — asserting the readable directory's rows are **not** hidden, that a
  deletion inside it is reported, and that the note behind the unreadable one is charged once. If the platform
  refuses to create such names, skip loudly rather than silently (`eprintln!` + return), matching F18's rule.
- A unit test on `under_unreadable` / `is_unknown_path` over hand-built `UnknownPath`s: near-twin bytes must not
  match; a `/` boundary must be respected (`foo` does not make `foobar/x.md` unknown); `"."` matches everything;
  and an entry whose `bytes` are empty matches nothing rather than everything.
- A report test: two refused paths with one display spelling produce two `skipped` lines, deduped by bytes.
- Mutation checks the implementer must run and report: revert `under_unreadable` to display-text comparison (the
  twin fixture must fail); revert the `/` boundary check (the `foobar` case must fail). Restore from a `/tmp` copy,
  never `git checkout --`.

## 6. Gates

The standing list, no subsetting: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`;
`cargo test --workspace`; `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config`;
`RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps`;
`python3 .superpowers/sdd/2026-10-23-note-processor-m1-core-catalog/check_cfg_dead_code.py`;
`python3 .superpowers/sdd/2026-10-23-note-processor-m1-core-catalog/verify_catalog_goldens.py` → `GOLDEN CHECK: pass`.
CI green on Linux **and** Windows is part of "green", and nothing under `tests/golden/` is ever regenerated.
