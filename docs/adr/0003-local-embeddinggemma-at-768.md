# ADR 0003 — Embeddings come from a local EmbeddingGemma-2 at 768 dimensions

**Status:** accepted. **Asked:** Which model produces vectors, how it arrives, and what width is stored.
**Recorded here:** 2026-11-20, moved verbatim from `todo.md`'s "Locked decisions" list; a ruling date quoted below is as it
was recorded there.

## Decision

Embeddings: self-hosted `google/embeddinggemma-2` (Apache-2.0), **amended by the human 2026-10-29 on three points**:
**(a)** the **whole model**, not the text tower alone — vision and audio encoders are carried too, so the artifact set is
more than one file; **(b)** stored width is **768 with no MRL truncation** (the card says MRL is native at 128/256/512/768,
so narrowing later is a re-store of vectors we already hold, not a re-embed — but it changes model identity, §4);
**(c)** the artifact is **fetched automatically on first start**, with a marker that says a download is in progress so the
UI can say so later. `np models install --path/--sha256` stays as the offline door. mean-pool + L2-normalise, task
prefixes per the model card, sha256-pinned to digests recorded in the repo, recorded on every vector. Fallbacks in spec §9 are one config key each. **Benchmark order set
2026-10-28: EmbeddingGemma first**, then the endpoint candidate, MiniLM only if both stall (design §9).
