# ADR 0005 — Only four things may ever be written into a workspace

**Status:** accepted. **Asked:** What gives np the right to write inside someone's notes at all.
**Recorded here:** 2026-11-20, moved verbatim from `todo.md`'s "Locked decisions" list; a ruling date quoted below is as it
was recorded there.

## Decision

Only four things may ever be written into the workspace: create today's daily file, replace the fenced
`<!-- np:todo -->` region, append approved `[[wikilink]]` under `## Related`, regenerate `INDEX.md` (+ shards)
and `processed/`. Note bodies are never touched by an LLM.
