# ADR 0009 — How work is done in this repository

**Status:** accepted. **Asked:** what keeps a round of agent-written changes honest.
**Recorded here:** 2026-11-20, moved from `todo.md`'s "Method" section. The conventions an agent follows when opening the tree
are in `AGENTS.md`; this is the method those conventions assume.

`superpowers:subagent-driven-development`: fresh implementer per bounded round, fresh scoped reviewer after each,
fix loop for Critical/Important, broad whole-branch review at the end, merge only on the human's word. The conventions an agent follows opening this tree cold — including design review
before any code — are in `AGENTS.md`, not here. Two writers never share a worktree. Reviewers have no shell — they must say "static
read" and list commands; the controller runs load-bearing claims himself. A subordinate's evidence outranks a
controller instruction: refuse and explain rather than fake coverage. No behaviour claim about a platform until
it was run there (F18's rule). Mutation testing means **production** mutations — deleting an assertion to see
what fails is not a check, because removal makes tests pass.
