# F18 — stop Windows overwriting a twin, without pretending it has byte identity

**Status:** design for the implementation round. Scope decision first, because the honest options are not equal.

## 1. What is actually broken

On non-Unix, `path_keys::os_bytes` (`crates/np-core/src/path_keys.rs:136`) spells a name as
`to_string_lossy().into_owned().into_bytes()`. Rust hands Windows names as UTF-16 code units; a name holding an
unpaired surrogate has no `char`, so the lossy conversion replaces it with U+FFFD — and two *different* OS names can
land on one byte key. Round 2 then makes that destructive rather than merely lossy:

`SELECT id FROM files WHERE rel_bytes = ?1` finds the twin's row, `UPDATE` overwrites it, no constraint fires,
nothing reaches `skipped`. One note silently becomes another. That is F13 §1's failure with better furniture, and a
green `windows-latest` job cannot see it because nothing in that suite can build an unpaired surrogate through the
filesystem API from a test.

## 2. The decision: refuse, don't guess

**Do not implement WTF-8 here.** Carrying wide units end to end would mean changing `path_bytes`, `normalize_rel`,
`rebase_abs`, every stored key and every migrated row in one step — a milestone with its own migration, not a
hardening item. Recorded so nobody reads this document as half-implementing it.

What must stop now is the *silent overwrite*. So: **a path whose OS name cannot survive the byte spelling
losslessly is not indexed at all.** It is named in `skipped` with its display spelling and its existing row is left
untouched — the same "unknown is not absent" rule the walk already applies to a directory it cannot enter
(`Walked::unreadable`). Losing one unrepresentable note from the catalog is a visible, bounded loss; silently
rewriting a different note's title, tags, items and links is not.

Consequences worth stating before writing code:

- **Windows keeps no byte identity.** This change does not give it one; it makes the absence loud instead of
  destructive. `docs/backlog-m2-followups.md` F18 stays open after this round, with its wording narrowed to
  "no byte identity on Windows" and this refusal as the shipped mitigation.
- **Unix behaviour must not move.** There `os_bytes` is exact, so the predicate is always false; goldens and every
  existing test stay untouched. Any Unix-visible change in this round is a bug.

## 3. Where the code changes

1. **A pure, testable predicate** in `path_keys`: something like
   `fn has_unpaired_surrogate(units: &[u16]) -> bool` — a high surrogate not immediately followed by a low one, or a
   lone low surrogate. Pure over `u16`, so it is tested on every platform with literal code units even though only
   Windows can produce them from a real path.
2. **`fn name_is_lossless(name: &OsStr) -> bool`**: the Unix arm returns `true` unconditionally (bytes are exact, and
   saying so in code is cheaper than proving it later); the Windows arm walks `encode_wide()` through the predicate.
3. **The walk skips and names.** In `walk.rs`, a file whose *own* component fails `name_is_lossless` is excluded from
   `Walked::files` and recorded so the scan can report it — reuse the unreadable channel's shape rather than inventing
   a second "don't treat as absent" path, because the required semantics are identical: **keep the row, publish no
   change, name the path.** If reusing `unreadable` would mislabel the reason in the report, add a sibling list and
   have both suppress `Removed`; say in the doc which one means what.
4. **Report text**: one line per affected display spelling, consistent with existing `skipped` entries; do not invent
   a Windows-only report channel.

## 4. Tests this must ship with

- **Predicate units (all platforms):** `"caf\u{D800}"` → unrepresentable; `"a\u{D800}\u{DC00}b"` → fine (valid pair);
  `"\u{DC00}"` alone → unrepresentable; two highs in a row → the first is unpaired; empty and pure-BMP strings →
  fine. This is the only part of F18 that CI can actually execute, so it carries the weight.
- **The Unix no-op, pinned:** `name_is_lossless` returns true for every name the Unix walk can produce, including raw
  non-UTF-8 bytes (`OsStrExt::from_bytes(b"caf\xe9.md")`) — proof that the mitigation cannot accidentally skip real
  Unix files. A silent skip on Unix would be worse than the bug being fixed.
- **Walk-level behaviour through a platform-neutral seam:** the walk should call the predicate on an `OsStr`, so the
  test can hand it a name built from bytes on Unix (always kept) and assert the exclusion list stays empty there.
- **`#[cfg(windows)]` integration test, written even though CI cannot create such a file** — document in its comment
  that it is the shape to run manually on a Windows box, and that its absence from Linux CI is exactly why the
  predicate units exist. Do not let a `cfg(windows)` test count as coverage of the logic.

## 5. Done when

Two Windows names differing only in unpaired surrogates produce **no** overwrite: neither is indexed, both are named
in the report, and any pre-existing row keeps its note and children. On Unix, every existing test still passes with
`GOLDEN CHECK: pass` and no path is newly skipped. F18's backlog entry is reworded to what shipped (refusal) and what
remains (byte identity), rather than closed as if solved.
