# Architecture decision records

One decision per file, numbered in the order they were taken. An ADR records a choice that has been closed and why it was
closed; it does not argue for change. `todo.md` links here instead of restating these, and anyone reading this tree should
treat what follows as settled unless a human reopens it.

| ADR | Decision | Ruling date quoted in the record |
| --- | --- | --- |
| [0001](0001-daemon-owns-everything.md) | The daemon owns everything; the window is a dumb client | — |
| [0002](0002-single-user-loopback-http.md) | One machine, one user, loopback HTTP with a token | — |
| [0003](0003-local-embeddinggemma-at-768.md) | Embeddings come from a local EmbeddingGemma-2 at 768 dimensions | 2026-10-29 |
| [0004](0004-endpoint-candidate-and-secret-hygiene.md) | The endpoint is a candidate, and its address never enters the repository | — |
| [0005](0005-four-writes-into-the-workspace.md) | Only four things may ever be written into a workspace | — |
| [0006](0006-status-markers-and-item-identity.md) | Status markers, and item identity that merges only on an anchor | — |
| [0007](0007-derived-data-with-no-importance-filter.md) | Derived data lives in the database and `INDEX.md`, with no importance filter | — |
| [0008](0008-frontend-stack.md) | The front end is React + Vite + TypeScript with CodeMirror 6 | — |
| [0009](0009-how-work-is-done.md) | Fresh implementer, fresh reviewer, merge only on the human's word | — |
