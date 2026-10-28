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

Listed in the order migration 7 declares them — `models`, then `chunks`, then `embeddings` — because that order is
itself a decision: a child's foreign key has to read in file order. Only **`models` before `embeddings`** is binding;
`chunks` could sit on either side of `models` without changing one constraint.

```sql
-- Declared before its child, so `embeddings.model`'s foreign key reads in file order.
CREATE TABLE models (
    id            TEXT    PRIMARY KEY,       -- e.g. embeddinggemma-2-text@1 — model identity ONLY (§4)
    dim           INTEGER NOT NULL,
    sha256        TEXT,                      -- NULL for a provider with no weights
    prefix_doc    TEXT    NOT NULL DEFAULT '',
    prefix_query  TEXT    NOT NULL DEFAULT '',
    installed_at  INTEGER NOT NULL
);

CREATE TABLE chunks (
    id            INTEGER PRIMARY KEY,
    note_id       INTEGER NOT NULL REFERENCES notes(id) ON DELETE CASCADE,
    ordinal       INTEGER NOT NULL,          -- 0-based within the note
    chunker       INTEGER NOT NULL,          -- which chunking code cut this row (§4)
    -- typeof, not just affinity: a Rust `String` bind arrives as TEXT and length() counts characters (F13 §4).
    text          BLOB    NOT NULL CHECK (typeof(text) = 'blob'),  -- raw UTF-8 bytes: no round trip as String
    byte_start    INTEGER NOT NULL,
    byte_end      INTEGER NOT NULL,
    source_hash   TEXT    NOT NULL,          -- sha256 of the note bytes this was cut from
    UNIQUE (note_id, ordinal)
);

CREATE TABLE embeddings (
    chunk_id      INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE,
    model         TEXT    NOT NULL REFERENCES models(id) ON DELETE CASCADE,  -- id + version, verbatim
    dim           INTEGER NOT NULL,
    -- The same trap one arm over: a TEXT bind of 4·dim characters passes `length(vector) = 4 * dim`, because
        -- length() counts characters on TEXT (F13 §4's type-split bug).
    vector        BLOB    NOT NULL CHECK (typeof(vector) = 'blob' AND length(vector) = 4 * dim),  -- little-endian f32
    source_hash   TEXT    NOT NULL,          -- the chunk's, at embed time
    embedded_at   INTEGER NOT NULL,
    PRIMARY KEY (chunk_id, model)
);

```

Notes on the shape, each of which is a decision and not an accident:

- **`text BLOB`, `vector BLOB`.** Notes are arbitrary bytes; `TEXT` would be a promise that they are UTF-8. The vector
  is stored as raw little-endian f32 rather than serialised JSON: 512 dimensions is 2 KiB, and hex or JSON would triple
  it for no benefit — **a deliberate deviation from the spec's "q8 ≈ 19 MB"** (`spec:85`, `spec:227`), accepted here
  because quantisation is a quality decision that belongs with the model benchmark and not smuggled in as storage
  trivia. `CHECK (typeof(vector) = 'blob' AND length(vector) = 4 * dim)` makes that deviation machine-enforced **now**, while the table is empty, so
  a later switch to q8 fails at write time instead of becoming subtly wrong cosines no test would notice. `dim` is stored
  so a mismatch is detectable at read time rather than producing garbage.
- **`(chunk_id, model)` is the key.** Re-embedding with a second model must not destroy the first: switching providers
  then costs a re-embed and nothing else, which is what makes §2's "one config key" promise true. It also means every
  read must filter by model, so the read helper this design calls `vector_for(model)` — **not written yet**, like every
  other provider-side name below — takes it explicitly rather than defaulting to "whatever is there".
- **`source_hash` on both tables.** `chunks.source_hash` says which note bytes the chunk was cut from;
  `embeddings.source_hash` says which chunk text the vector was computed from. Two levels because a chunk can be
  unchanged while its note changed (a heading edit in paragraph 9 leaves chunk 2's text alone), and re-embedding only
  what actually moved is the whole difference between a usable and an unusable incremental scan.
- **`embeddings.model` is a real foreign key, and `models` is written before any vector naming it.** Without the
  reference an embedding row can name a model that was never installed, or one removed and reinstalled under the same
  id — exactly the orphan class migration 3 exists to end ("Migration 3 gave every one of these five statements a twin in
  the schema", `scan.rs:1338-1340`) — and `PRAGMA foreign_key_check` reads *declared* constraints (`first_violation`
  says as much in its own comment), so it could never see the damage.
- **Stored vectors are always document-prefixed.** The spec makes prefixes part of correctness: retrieval is asymmetric
  (query prefix on questions, document prefix on stored text), clustering symmetric. Both hold if what is *stored* is
  document-prefixed and only queries ever take `prefix_query`. That is a convention unless it is written down, so it is
  written down: `prefix_doc` describes every row of `embeddings`, and a provider that ignores prefixes is a bug.
- **`models.sha256` nullable.** Weights are never committed and `np models install` verifies the digest it fetched;
  a provider with no weights has nothing to verify. No `CHECK (typeof(sha256)='text')`: the NULL arm is the common one.
- **No FTS5 yet.** The spec wants it over note and summary text; summaries do not exist, and adding FTS now would put a
  second derived-data path in front of the same benchmark. It belongs with M2b or the search milestone, whichever comes
  first — recorded as an open item rather than silently dropped.

## 4. Chunking: deterministic first, model-shaped second

`chunk(note_text) -> Vec<Chunk>` is a pure function over bytes, and its contract is asserted before any model exists:

- **Deterministic and total.** Same bytes, same chunks, on every platform and every run: no HashMap, no time, no locale.
  Chunk boundaries are byte offsets with `text == &domain[byte_start..byte_end]`, asserted per chunk: an off-by-one
  silently misaligns every citation to come, and a slice through a multi-byte character would panic on the German umlauts
  this corpus is full of.
- **The domain is the decoded text, not the file.** `scan.rs:681-687` strips a leading BOM and replaces invalid UTF-8
  before anything is parsed, while `files.hash` digests the raw bytes on disk — "BOM included", in that file's own words.
  Offsets into the *file* would therefore mean something else for precisely the corpus F13/F18/F20/F23 refuse to key by:
  every BOM'd Windows export and every non-UTF-8 note. So offsets and `chunks.source_hash` are over the **decoded text
  that is actually embedded** — the same spelling as `notes.body`. The consequence is stated rather than buried: for a
  note whose `notes.encoding = 'lossy'` (already stored, no new column), byte offsets are *approximate* citations into
  the file, and M2b's evidence links must say so instead of pointing one character off.
- **`chunks.chunker` stamps which chunking code produced the row**, and staleness is per row (`chunker = CHUNKER_VERSION
  AND source_hash matches`) rather than a global verdict; `models.id` carries model identity only — one fact, one place.
  Without the stamp, §4's promise of "no half-embedded corpus under two definitions of chunk" is unenforceable: the only
  honest action after a bump would be re-chunk-everything, which is the very thing the mitigation was for.
- **Boundaries at blank-line seams where possible — and fence-blind.** What M2a actually does is cut at **blank lines**:
  a block is a maximal run of non-blank lines, whole blocks are merged while their span fits `MAX_CHUNK_BYTES`, and a
  block larger than the bound is cut back to the last whitespace seam before it. That is deliberately not `parse`'s
  notion of a block, so **a fenced code block containing a blank line can still be split in half** — and a chunk that
  splits a fenced block in half embeds garbage. Fence awareness belongs with the tokenizer-aware `CHUNKER_VERSION` bump
  below, because either one moves seams and therefore every row; recorded as an open item rather than silently dropped
  (`docs/backlog-m2-followups.md`).
- **~350 tokens with overlap** is the spec's number and M2a implements neither half of it. Token counts belong to a
  tokenizer this milestone does not have, so the first implementation bounds chunks by **bytes** with a documented
  constant (`MAX_CHUNK_BYTES`). And there is **no overlap yet**: v1's chunks are disjoint — `tests/chunk.rs` asserts
  `byte_start >= predecessor.byte_end` — so nothing is re-embedded twice and a seam that cuts a sentence in half stays
  cut. The tokenizer-aware cut lands with the chosen model as a `CHUNKER_VERSION` bump recorded per row, so old rows are
  *identifiable* rather than merely presumed stale; overlap is recorded beside the fence gap
  (`docs/backlog-m2-followups.md`) because adding it changes every offset.
- **CRLF is a re-embed amplifier, accepted knowingly.** Chunk text comes from stored note bytes, so a CRLF↔LF rewrite by
  an editor or sync client moves every offset and re-embeds the whole note — worst case all ~18k chunks of one language,
  against spec §"Linux **and** Windows: … line endings … differ". Normalising is *not* the fix: it would break the slice
  contract above and make citations point at text the file does not contain. The cost is written here so a Windows user's
  second scan is not filed as a bug.

## 5. Staleness: reuse the F15 machinery, do not invent a second one

`PARSE_VERSION` already solves "derived rows were written by rules this build no longer has", including the part that
makes it safe: recording the version only when every owed note was actually re-parsed, and retrying next scan otherwise
(`docs/design-f22-durable-records.md`, F15's residue). Embedding needs the identical shape and gets it:

- A chunk is **current** when its `source_hash` equals the hash of the text sliced from the note's current bytes.
  `files.hash` already tells us whether a note changed at all, so an unchanged note costs one indexed lookup per scan —
  the warm-pass property that F13 §4.1 and A4's benchmark both protect.
- A new note or an edited note re-chunks, and only chunks whose own text moved get re-embedded.
- **Two facts, split apart — this paragraph is the review's P0 and the one correction here that would have cost a
  week.** F15's `refresh_covered` (`scan.rs:1907-1909`) counts exactly two sources of unfinished work — a path that
  changed or went unread between walk and read, and a live row behind an unenterable directory or refused name — and the
  code refuses to charge anything else, because "Charging any of them would re-parse the whole corpus on every scan
  forever and repair nothing" (`scan.rs:1872-1873`). Re-parsing never fails for an *external* reason. Embedding does:
  no model installed, endpoint down, out of memory. Gate `EMBED_VERSION` — a name this design introduces, not code: it
  exists nowhere in the tree yet — on that and the **default** configuration
  (`embeddings.provider = "none"`, §2) withholds it on every scan forever — a full re-chunk of ~18k chunks each time,
  plus a gap line nobody can clear. So:
  - `EMBED_VERSION` covers **chunker-definition coverage only**, reusing `refresh_covered`'s two sources unchanged, and
    is recorded whenever chunking reached every note it owed;
  - **provider availability is not version debt.** "Owed but unembedded" is already self-describing per row through
    `chunks.source_hash`, so it is reported as a bounded count in F21/F22's durable-record shape and never inferred from
    a missing meta key.
- **Vectors are written `INSERT … ON CONFLICT (chunk_id, model) DO UPDATE`, and chunk replacement is a per-row diff.**
  F15 *expects* its retry to happen, and the derived-row pattern elsewhere is delete-then-insert (`scan.rs:1497-1503`);
  reusing that for chunks would cascade away every vector of an edited note, destroying exactly the "re-embed only what
  moved" property §3 calls the whole difference between a usable and an unusable incremental scan. A plain `INSERT` into
  `PRIMARY KEY (chunk_id, model)` makes the expected retry fail on a constraint error — and since one `?` propagates out
  of `apply_changes`, the whole scan's indexing work rolls back with it. Provider failure must not be data loss; neither
  may our own conflict handling be.
- **Embedding does not hold the scan's transaction open.** The write pass runs inside `unchecked_transaction()` and
  commits once (`scan.rs:432`, `:470`); unbounded model work inside it would leave a concurrent `np` waiting out
  `busy_timeout` to report `database is locked`. Chunk writes stay in the transaction — they are cheap and must be atomic
  with the note rows; vectors commit separately per batch, which is safe precisely because the bullet above no longer
  claims "recorded ⇒ embedded".
- **A provider failure must not be a data-loss event.** The scan indexes the note, leaves its chunks unembedded, reports
  the count, and the catalog still renders: refusing work is fine, pretending it happened is not (F21, and the
  `/dev/full` lesson).

## 6. The benchmark, with its limits stated up front

The spec's gate is throughput, so the deliverable is a measurement and an honest statement of what it cannot tell us:

- **A real corpus, not a synthetic one.** The spec's own gate wording is "on my own notes … retrieval spot-checks in
  German and English" (`spec:316`), and throughput is tokenizer- and language-dependent on the mixed de/en corpus
  (`spec:82`). A synthetic corpus cannot support even this section's modest claim. So: a fixed de/en-mixed slice derived
  from the real vault (~2k notes at the real size distribution), committed as a fixture or generated from a recorded
  seed — reported per language as well as blended, because one blended figure hides whichever language costs triple.
- Reported per candidate: chunks/hour, seconds to first useful result, peak RSS, weight bytes, and warm-scan cost when
  nothing changed (must stay ~zero — the property users actually feel). **≥2 runs with spread reported**: one
  unreplicated observation on a shared host is an anecdote. Endpoint candidates get their own table — "bytes downloaded"
  for `openai-compatible` measures your server rather than this CPU, and putting it in the same A/B/A column invites
  comparing incomparable things.
- **The decision rule is applied, not implied.** Spec `:317-318`: keep EmbeddingGemma 2 "if a full first pass fits in one
  evening, otherwise fall back per §9 and record the choice". The report therefore states the arithmetic — real corpus
  size × measured chunks/hour against an evening's hours — and shows each candidate passing or failing it. Numbers
  without the threshold they were judged against are how a benchmark becomes a poster.
- **Buildability is measured, not assumed.** One CI step per OS row runs `cargo check --locked -p np-core --features
  local-model`, because `cargo clippy --all-targets` never compiles a feature-gated heavy dependency. That answers the
  Windows question §8 asks with a build instead of hope.
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
- **Storage:** a runtime note delete cascades chunks and vectors with enforcement ON, asserted on a fresh database.
  **Not** "F19's merge moves them to the survivor" — that claim was wrong in three ways at once: `migrate` runs every
  step under `PRAGMA foreign_keys = OFF` (`migrate`, `db.rs:470`, restored at `:472`), migration 5 therefore deletes
  its children *by hand* precisely because "SQLite performs no `ON DELETE CASCADE` while it is off (verified)" — that
  sentence is `merge_duplicate_notes`'s own comment and the statements are its deletes at `db.rs:1222-1228`, and
  migration 5 runs **once** with no second pass (`scan.rs:1893-1896`) while runtime
  duplicates are refused rather than merged (`scan.rs:1357-1360`). Cascade also *deletes*; it never re-points. The honest
  statement is in §3: any future duplicate-healing must be taught `chunks`/`embeddings` explicitly, in step with the
  three hand-written child lists (`merge_duplicate_notes`'s deletes, `scan.rs:1368-1372`, `scan.rs:1497-1501`).
  Plus: two models
  coexisting on one chunk; a length/`dim` mismatch refused at write by the `CHECK`; an `embeddings` row naming an unknown
  model refused by the foreign key; and a Rust `String` bind into `chunks.text` refused by its `typeof` arm
  (`a_string_bind_is_refused_from_chunks_text`, on `refuses_string_bind`) — BLOB is an affinity, not a constraint.
- **Staleness:** unchanged note → no re-embed (asserted as "the provider was called zero times", through a test provider
  that counts calls, not as a timing claim); edited note with an untouched paragraph → only the covering chunks
  re-embedded; provider failure → `EMBED_VERSION` not recorded and the count reported.
- **Provider seam:** `"none"` embeds nothing and says so in the report; an unconfigured endpoint is refused **quietly**,
  not by panic — CI runs `cargo test --locked --release -- --ignored`, so a panicking authoring path poisons a green run
  (F16's lesson).
- **No network in the indexing path, fenced where a fence can actually see it.** The risk worth catching is not a test
  dialing out — it is someone wiring a default base URL into config, which env-stripping cannot notice. So: provider
  `"none"` **constructs no HTTP client at all**, asserted by the type (the none-provider holds no client field, so there
  is nothing to call), and endpoint tests point at a `TcpListener` bound on `127.0.0.1:0`, port read then dropped —
  refused deterministically on both OSes, where `127.0.0.0/8` is platform-shaped and CI runs Windows (`ci.yml:20`).
  Hermetic env isolation belongs in the CI job (F14's open defect, `backlog-m2-followups.md:191-200`), not in-process:
  mutating environment inside a test races cargo's parallel harness.
- **Anything needing the real model or endpoint is gated on an explicit runtime env var with a clean early return — never
  on `#[ignore]`.** CI runs `cargo test --locked --release -- --ignored` (`ci.yml:41`), which F16 already learned burns a
  green run; the spec's "opt-in, skipped by default, not in CI" bench and integration test (`spec:308-310`) are only
  opt-in if they check a variable at runtime.
- **Config refuses to be misspelled.** Every existing config struct is `#[serde(default)]` with no unknown-key refusal,
  and `MissingField` "cannot fire from `Config::load` today" (`config.rs:30-32`, `:56-57`) — so a hand-typed
  `provide = "openai-compatible"` would load as `"none"` in silence. In a project whose F-series is about not lying about
  status: `provider` is to be an **enum**, and the `[embeddings]` struct this design adds — **not written yet, like the
  provider itself; `Config` today holds `data_dir`, `catalog` and `markers` only** — is to carry
  `#[serde(deny_unknown_fields)]`, with one test asserting an unknown key there is refused naming key and file.
- Migration 7 updates `the_schema_is_what_the_plan_mandates`, which pins the table list by exact array and whose index
  assertion filters `sql IS NOT NULL`, so PK/UNIQUE autoindexes stay out of the expected list. Named rather than
  numbered: migration 7 moved every line below it in `tests/db.rs`, which is how the old numbers went stale.
- **Goldens:** M2a must move zero bytes in `tests/golden` and `tests/golden-wide`. No catalog field reads chunks yet, so
  an empty result there is not luck — it is the fence that says §1's "byte-for-byte identical" held.

## 8. Risks I can see now

1. **candle on Windows — stated in both halves.** M2a's own chunker, storage and staleness tests run on Windows from
   day one: CI already builds and tests the workspace on `windows-latest` (`ci.yml:20`, `:29-41`) and byte-exactness is
   engineered for (`.gitattributes:3`, fixtures exempted at `:5-7`). What CI does *not* cover is an optional heavy
   provider: a feature-gated `candle`/`ggml` dependency is invisible to `--all-targets` unless asked for, and the GGUF
   fallback additionally needs a C++ toolchain. §6's per-OS `cargo check --features local-model` is the measurement; until
   it runs, no claim about Windows model support is made here (F18's rule).
2. **Memory ceiling.** 18k × 512-d f32 ≈ 37 MB in RAM is fine; the danger is loading 540 MB of weights inside a scan on
   a small machine. The provider gets an explicit load/release boundary and the scan embeds in bounded batches.
3. **Chunker churn.** §4's byte-bounded first cut will be replaced by a tokenizer-aware one, and if that lands without a
   version bump it silently invalidates every vector. `EMBED_VERSION` plus `models.id` is the mitigation; the risk is
   someone treating it as ceremony.
4. **The `cluster` column reservation.** The catalog entry line already reserves it. M2a leaves it empty, which keeps
   goldens intact — but the moment M2b fills it, every golden changes at once. Better to face that in one reviewed
   milestone than to let it leak in per-note; recorded here so nobody "helpfully" starts filling it early.

## 9. Decisions from the human (2026-10-28), and how the endpoint stays out of the tree

**Benchmark order: `embeddinggemma-2` text tower first**, then the OpenAI-compatible endpoint candidate, then MiniLM
only if both stall. §6's one-evening arithmetic decides, not preference.

**Endpoint candidate:** the same self-hosted OpenAI-compatible server this project is developed against, serving the
Qwen3.8-Flash-Next model. Whether it exposes `/v1/embeddings` at all is a **measured question**, asked first: if it
serves only completions, that candidate is reported as unavailable rather than approximated by embedding through a chat
endpoint, which would silently produce vectors whose meaning nobody verified.

**Secret handling — a hard rule, not a preference.** The base URL and key never enter the repository in any form: not in
`config.toml` examples, not in fixtures, not in a benchmark report, not in a test failure message, not in a commit
message. They arrive at runtime as `NP_EMBED_BASE_URL` / `NP_EMBED_API_KEY` and are read only by the endpoint provider.
Consequences that are part of the design rather than tidiness:

- The benchmark writes **redacted** provenance — provider kind, model id, host category ("self-hosted loopback/LAN"),
  latency and throughput — never a URL, host, port or Authorization value. If a run cannot describe itself without the
  URL, it prints `<redacted>` and says what was withheld.
- Test failure output must not echo request lines: an error path that formats the full URL into a panic would leak into
  CI logs on the first flake, so the provider's error type carries the endpoint only as an opaque label.
- Nothing in CI authenticates to it. Per §7, real-endpoint work is gated on the runtime variable being present, which CI
  never sets — so a leaked key cannot be exercised by a public-facing job either.
