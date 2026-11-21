# Archive: plans and roadmaps that have been executed

Nothing here is a task list. These documents did their work and stayed, because they record what was intended at the time —
which is the only way to notice later that something was dropped on purpose rather than by accident.

| Document | What it drove |
| --- | --- |
| [`roadmap-2026-10-27.md`](roadmap-2026-10-27.md) | Track A (correctness residue) and Track B (product). Both ran; what remains of them lives in [`../backlog-m2-followups.md`](../backlog-m2-followups.md) and [`../../todo.md`](../../todo.md) |
| [`plan-2026-10-28-state-and-roadmap.md`](plan-2026-10-28-state-and-roadmap.md) | The companion that sequenced the roadmap's state model |
| [`plans/`](plans/) | One implementation plan per round: M1 core and catalog, R7c image vectors, auto-fetch and setup, diagnostics, `np serve` progress |

## Why nothing here gets its links repaired

An archived plan quotes the paths it was written against, including generated ones that existed only while somebody was
following it — `tests/fixtures/corpus/INDEX.md` is produced by a `scan` into the fixture corpus, so no commit ever contained
it. Rewriting a step to satisfy a link checker would falsify the record of what that round was told to do.

So `.github/scripts/check-links.py` treats this directory as quotation rather than navigation and does not follow links out
of it. That is a deliberate hole: if a decision in here is still live, it belongs in [`../adr/README.md`](../adr/README.md)
or the backlog, where its links are checked like everyone else's.
