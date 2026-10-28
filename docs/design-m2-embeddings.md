# M2a — chunk and vector storage, with the model choice still open

Date: 2026-10-28. Branch: `m12/embeddings`. Spec: `docs/superpowers/specs/2026-10-23-note-processor-design.md`
§"Embeddings", §13 step 2. Roadmap: `docs/roadmap-2026-10-27.md` Track B M2. Backlog note that made the ordering
argument: `docs/backlog-m2-followups.md:189` — derived-data storage is "cheaper to build once now than to retrofit
after clustering stores derived data too."

## 1. What this milestone is, and what it deliberately is not

Track B's M2 is embeddings **and** clustering. Doing both in one step would put a clustering algorithm on top of
vectors produced by a model nobody has benchmarked on this machine — the spec refuses that itself: "Throughput is the
one real cost of that model and is **measured before we commit**" (§13 step 2). So M2 is cut in two, and this document
covers the first half.

**M2a (this design).** Chunking, vector storage, a provider seam with one offline implementation, staleness rules, and
the benchmark that decides the model. After it, `np` can answer "which chunks does this workspace have, which are
embedded, under which model" — and the catalog is byte-for-byte what it is today.

**M2b (next design, after the benchmark).** Clustering, `clusters` + membership, the review queue with evidence, the
`cluster` column in the catalog entry line, nearest neighbours for orphans.

**Not M2 at all.** Summaries/compression (`processed/`, roadmap M2b-of-Track-B), timelines (M3), daemon/API/watcher
(M4). No LLM call anywhere: spec §Constraints — "No LLM, no network in the indexing path."

## 2. The decision this design does not make for you

The spec chose `google/embeddinggemma-2`'s text tower and listed two fallbacks, all one config key apart. That choice
is a **download size and CPU-time** decision wearing a model name: ~540 MB of weights plus a candle-derived encoder
wrapper in-tree, versus 7 MB for `paraphrase-multilingual-MiniLM-L12-v2`, versus zero local cost with
`embeddings.provider = "openai-compatible"` (whose price is that your note text crosses the network and indexing needs
your server up). It also has a Windows dimension: the GGUF fallback "costs a C++ build on Windows", and I have never run
a build of this kind there, so by F18's rule I may not claim it works.

M2a therefore builds **the seam and the measurement**, not the model:

- `embeddings.provider = "none"` is the **default**. A fresh install with no config embeds nothing, downloads nothing,
  contacts nothing, and renders the identical catalog. This is not a stub to be filled in later: it is the shipping
  behaviour for anyone who has not chosen, and it is why M2a can merge before §2's question is answered.
- The benchmark (§6) runs on this host against whichever candidates I can build here, with its limits stated. Its
  numbers go into `docs/design-m2-model-choice.md`, which is where the model gets picked — with you in the loop, since
  it is your CPU and your bandwidth.

## 3. Schema: migration 7

Three tables, all keyed to `notes.id` and never to a path. That is deliberate and it is the F13 lesson applied forward:
a chunk that stored `rel_bytes` would inherit every lossy-name, rename and merge defect this project has spent six
milestones removing, and F19's merge already moves derived rows to the surviving id — which is exactly what chunks need.

```sql
CREATE TABLE chunks (
    id            INTEGER PRIMARY KEY,
    note_id       INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    ordinal       INTEGER NOT NULL,          -- 0-based within the note
    text          BLOB    NOT NULL,          -- raw UTF-8 bytes: never assume it round-trips as String
    byte_start    INTEGER NOT NULL,
    byte_end      INTEGER NOT NULL,
    source_hash   TEXT    NOT NULL,          -- sha256 of the note bytes this was cut from
    UNIQUE (note_id, ordinal)
);

CREATE TABLE embeddings (
    chunk_id      INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    model         TEXT    NOT NULL,          -- model id + version, verbatim from config/manifest
    dim           INTEGER NOT NULL,
    vector        BLOB    NOT NULL,          -- little-endian f32, exactly dim of them
    source_hash   TEXT    NOT NULL,          -- the chunk's, at embed time
    embedded_at   INTEGER NOT NULL,
    PRIMARY KEY (chunk_id, model)
);

CREATE TABLE models (
    id            TEXT    PRIMARY KEY,       -- e.g. embeddinggemma-2-text@1, or lexical-v1
    dim           INTEGER NOT NULL,
    sha256        TEXT,                      -- NULL for a provider with no weights
    prefix_doc    TEXT    NOT NULL DEFAULT '',
    prefix_query  TEXT    NOT NULL DEFAULT '',
    installed_at  INTEGER NOT NULL
);
```

Notes on the shape, each of which is a decision and not an accident:

- **`text BLOB`, `vector BLOB`.** Notes are arbitrary bytes; `TEXT` would be a promise that they are UTF-8. The vector
  is stored as raw little-endian f32 rather than serialised JSON: 512 dimensions is 2 KiB, and hex or JSON would triple
  it for no benefit. `dim` is stored so a mismatch is detectable at read time instead of producing garbage cosines.
- **`(chunk_id, model)` is the key.** Re-embedding with a second model must not destroy the first: switching providers
  then costs a re-embed and nothing else, which is what makes §2's "one config key" promise true. It also means every
  read must filter by model, so `vector_for(model)` takes it explicitly rather than defaulting to "whatever is there".
- **`source_hash` on both tables.** `chunks.source_hash` says which note bytes the chunk was cut from;
  `embeddings.source_hash` says which chunk text the vector was computed from. Two levels because a chunk can be
  unchanged while its note changed (a heading edit in paragraph 9 leaves chunk 2's text alone), and re-embedding only
  what actually moved is the whole difference between a usable and an unusable incremental scan.
- **`models.sha256` nullable.** Weights are never committed and `np models install` verifies the digest it fetched;
  a provider with no weights has nothing to verify. No `CHECK (typeof(sha256)='text')`: the NULL arm is the common one.
- **No FTS5 yet.** The spec wants it over note and summary text; summaries do not exist, and adding FTS now would put a
  second derived-data path in front of the same benchmark. It belongs with M2b or the search milestone, whichever comes
  first — recorded as an open item rather than silently dropped.

## 4. Chunking: deterministic first, model-shaped second

`chunk(note_text) -> Vec<Chunk>` is a pure function over bytes, and its contract is asserted before any model exists:

- **Deterministic and total.** Same bytes, same chunks, on every platform and every run: no HashMap, no time, no locale.
  Chunk boundaries are byte offsets into the note, and `byte_start`/`byte_end` must satisfy
  `text == &note_bytes[byte_start..byte_end]` — asserted per chunk, because an off-by-one here silently misaligns every
  citation to come, and a slice across a multi-byte character would panic on German umlauts, which is the corpus (§
  Constraints: mixed German and English).
- **Boundaries at block ends where possible.** Markdown blocks (headings, list items, fenced code) are the natural
  seams and the parser already finds them; a chunk that splits a fenced code block in half embeds garbage.
- **~350 tokens with overlap** is the spec's number, but token counts belong to a tokenizer M2a does not have. So the
  first implementation bounds chunks by **bytes** with a documented constant, and the tokenizer-aware cut lands with the
  chosen model as a **chunker version bump**, not a silent change: `models.id` carries the chunker in its version
  suffix so re-chunking invalidates vectors honestly. Getting this wrong is how you end up with half a corpus embedded
  under two different definitions of "chunk" and no way to tell which rows are comparable.

## 5. Staleness: reuse the F15 machinery, do not invent a second one

`PARSE_VERSION` already solves "derived rows were written by rules this build no longer has", including the part that
makes it safe: recording the version only when every owed note was actually re-parsed, and retrying next scan otherwise
(`docs/design-f22-durable-records.md`, F15's residue). Embedding needs the identical shape and gets it:

- A chunk is **current** when its `source_hash` equals the hash of the text sliced from the note's current bytes.
  `files.hash` already tells us whether a note changed at all, so an unchanged note costs one indexed lookup per scan —
  the warm-pass property that F13 §4.1 and A4's benchmark both protect.
- A new note or an edited note re-chunks, and only chunks whose own text moved get re-embedded.
- `EMBED_VERSION` (a `meta` key, same code path as `PARSE_VERSION`) covers chunker and prefix changes: bump it and the
  next scan re-chunks everything, with the same "don't record it if you could not finish it" rule.
- **A provider failure must not be a data-loss event.** If embedding is unavailable — no model installed, endpoint
  down, out of memory — the scan indexes the note, leaves its chunks unembedded, reports a bounded count, and does not
  record `EMBED_VERSION`. This is F21's durable-first rule and the `/dev/full` lesson: refusing work is fine, pretending
  it happened is not. The catalog still renders, because §2's whole point is that catalog quality does not depend on
  embeddings.

## 6. The benchmark, with its limits stated up front

The spec's gate is throughput, so the deliverable is a measurement and an honest statement of what it cannot tell us:

- A/B/A over a fixed synthetic corpus (~2k notes, ~18k chunks at the real size distribution), warm pass after cold, on
  this host (`pi`, euid 1000), recorded with CPU model, thread count and whether other work was running.
- Reported per candidate: seconds to embed the corpus, peak RSS, bytes downloaded, and the warm-scan cost when nothing
  changed (which must stay ~zero — that is the property users actually feel).
- **Stated limit:** this host is not your laptop, and a single machine's tokens/sec does not prove the model will feel
  fast on a fanless one. The claim it can support is "X is N× the cost of Y at the same quality question", which is
  what §2's decision needs.

Quality is deliberately out of scope here: measuring clustering quality before M2b has a consumer would be measuring
nothing. What goes into the model decision instead is throughput, size and Windows buildability — and if you want a
quality number too, that is a labelled-corpus task I would rather do against M2b's review queue, where there is a right
answer to compare against.

## 7. Tests

- **Chunker:** determinism (same bytes twice → identical offsets), the slice identity above on a note with umlauts and
  emoji, an empty note, a note that is one huge block, a note whose last line has no newline, and a mutation check:
  change one byte in paragraph 3 and assert exactly the chunks covering it changed (`docs/design-f24-merge-assertions.md`'
  rule — a premise asserted for one side of a pair is half an assertion).
- **Storage:** cascade when a note goes (F19's merge must move chunks and vectors to the survivor, which is the reason
  they hang off `notes.id` at all); two models coexisting on one chunk; `dim` mismatch refused at read.
- **Staleness:** unchanged note → no re-embed (asserted as "the provider was called zero times", through a test provider
  that counts calls, not as a timing claim); edited note with an untouched paragraph → only the covering chunks
  re-embedded; provider failure → `EMBED_VERSION` not recorded and the count reported.
- **Provider seam:** `"none"` embeds nothing and says so in the report; an unconfigured endpoint is refused **quietly**,
  not by panic — CI runs `cargo test --locked --release -- --ignored`, so a panicking authoring path poisons a green run
  (F16's lesson).
- **No network in tests.** The suite runs with proxy and `*_API_KEY` variables unset, and the endpoint provider's tests
  point at an unroutable address on `127.0.0.0/8` that is refused immediately. "No network in the indexing path" is a
  spec constraint, so it gets a mechanism rather than a comment.
- **Goldens:** M2a must move zero bytes in `tests/golden` and `tests/golden-wide`. No catalog field reads chunks yet, so
  an empty result there is not luck — it is the fence that says §1's "byte-for-byte identical" held.

## 8. Risks I can see now

1. **candle on Windows.** A Rust-only build probably works; the GGUF fallback needs C++. Nothing here claims either
   until one runs there (F18's rule), which is why §6 records buildability as a per-platform fact, not an assumption.
2. **Memory ceiling.** 18k × 512-d f32 ≈ 37 MB in RAM is fine; the danger is loading 540 MB of weights inside a scan on
   a small machine. The provider gets an explicit load/release boundary and the scan embeds in bounded batches.
3. **Chunker churn.** §4's byte-bounded first cut will be replaced by a tokenizer-aware one, and if that lands without a
   version bump it silently invalidates every vector. `EMBED_VERSION` plus `models.id` is the mitigation; the risk is
   someone treating it as ceremony.
4. **The `cluster` column reservation.** The catalog entry line already reserves it. M2a leaves it empty, which keeps
   goldens intact — but the moment M2b fills it, every golden changes at once. Better to face that in one reviewed
   milestone than to let it leak in per-note; recorded here so nobody "helpfully" starts filling it early.
