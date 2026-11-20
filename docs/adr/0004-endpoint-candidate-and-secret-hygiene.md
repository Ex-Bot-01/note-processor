# ADR 0004 — The endpoint is a candidate, and its address never enters the repository

**Status:** accepted. **Asked:** Whether a second embedding path exists, and how its secrets stay out of the tree.
**Recorded here:** 2026-11-20, moved verbatim from `todo.md`'s "Locked decisions" list; a ruling date quoted below is as it
was recorded there.

## Decision

Endpoint candidate is the same self-hosted OpenAI-compatible server this project is developed against, serving
Qwen3.8-Flash-Next. **Base URL and key never enter the repo** — runtime `NP_EMBED_BASE_URL` /
`NP_EMBED_API_KEY`, read only by the provider; benchmark provenance is redacted; nothing in CI authenticates
to it. Whether it serves `/v1/embeddings` at all is a measured question, asked first.
