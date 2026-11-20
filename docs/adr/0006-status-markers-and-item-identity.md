# ADR 0006 — Status markers, and item identity that merges only on an anchor

**Status:** accepted. **Asked:** How open/done/paused is read without inventing structure the author did not write.
**Recorded here:** 2026-11-20, moved verbatim from `todo.md`'s "Locked decisions" list; a ruling date quoted below is as it
was recorded there.

## Decision

Status: **open is the default** (no marker), a tick means done; markers are config with German and English
defaults (spec §8.7). Item identity across files merges only on an anchor; fuzzy matches go to the review queue
as suggestions, never as assertions.
