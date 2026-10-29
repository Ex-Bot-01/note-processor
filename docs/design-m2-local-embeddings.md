# M2 local embeddings: running EmbeddingGemma-2 inside `np`

Status: **draft for design review** (2026-10-29). Supersedes the "transport + benchmark" round planned in
`docs/design-m2-embeddings.md` §9; that plan assumed an OpenAI-compatible `/v1/embeddings` endpoint and the human has
answered that it does not exist and is not wanted (§10 there).

Everything factual about the model below was read from Hugging Face's public API on 2026-10-29, unauthenticated, and is
cited as such. Everything about *cost* is a question this design's benchmark answers; no number in here is a promise.

## 1. What changed, and what round this is

Embeddings are **local**. The application loads the model, runs it on CPU, and writes vectors — with no network at all.
The LLM API is for phase 2 (chatting with notes) and indexing must work fully without it. Network appears once: setup,
to obtain the artifact.

So round 4b is not "HTTP transport + benchmark". It is **a local inference provider + the benchmark that says whether
this machine can carry it**. Same discipline as every other round: a seam with tests, a refusal path, and a measured
number before any claim.

**Non-goals for this round:** semantic search UI, `cluster` columns, hybrid keyword+vector ranking, GPU, a resident
model server/daemon, incremental embedding during `np scan`, and anything at all in phase 2. This round ends with
`np embed <ws>` producing verified vectors and a throughput number on the record.

## 2. The decision that matters: what actually runs the model

Three ways to execute EmbeddingGemma-2 in Rust. The evidence, then the recommendation.

| Option | What it buys | What it costs |
| --- | --- | --- |
| **(a) llama.cpp over GGUF**, through FFI bindings | `ggml-org/embeddinggemma-2-GGUF` **already publishes the text-only artifact** (`embeddinggemma-2-Q8_0.gguf`, 309.9 MB; BF16 558.0 MB) and upstream implements this architecture: GQA 4 heads / 2 KV, `head_dim = 256` ≠ `hidden_size / heads`, `sliding_window = 512` hybrid attention, SentencePiece vocab 262,144, mean pooling + L2 normalise built in (`LLAMA_POOLING_TYPE_MEAN`). Tokenizer included — no second implementation. | A C/C++ build in the dependency graph, on three platforms, under `--locked`. FFI safety boundary we must own. Memory control is by context/batch sizing we choose. Crate-version churn upstream. |
| **(b) ONNX Runtime** via the `ort` crate | Prebuilt binaries — no C++ compile of the model code; text graph + weights published by `onnx-community/embeddinggemma-2-ONNX`. | Weights ship as external-data shards we must locate and verify; tokenizer must be re-done in Rust (`tokenizers`) and kept byte-agreement with the model's own vocab; **pooling and normalisation are ours to implement**; two runtimes' behaviour to keep consistent. |
| **(c) candle (pure Rust)** | No foreign toolchain at all, everything inspectable. | `model_type = embedding_gemma2` is not implemented upstream. We would write a 24-layer GQA + sliding-window decoder and the 512 → 768 projection ourselves, and have **no reference implementation to check against** — the classic way to ship vectors that are quietly wrong. |

**Recommendation: (a), behind a cargo feature.** The model's own publisher ships the exact artifact and someone else has
already implemented the architecture; both alternatives move correctness risk into code we own, and this project's whole
posture is that derived data must be *provably* derived from the bytes.

Feature-gating (`np-core/local-model`) keeps default `cargo test --workspace` free of a C++ build — which protects every
gate in the list — but a feature nothing compiles rots, so: **one CI job builds and unit-tests the feature on Linux**
(no real model, no network; see §7), and release builds enable it. If review prefers otherwise, the alternative is
`default = ["local-model"]` and paying the compile everywhere.

The benchmark can falsify this choice. If llama.cpp's resident cost or throughput is unacceptable on this host, (b) is
the fallback and the seam below is what makes that a swap rather than a rewrite.

## 3. The seam, and what stays true either way

`np_core::embed::Embedder` already exists (`model_id`, `dim`, `embed(&[&str]) -> Vec<Vector>`). The local provider
implements it and nothing else in the tree learns that inference happened:

- **Loading is explicit and bounded.** A provider is constructed from an installed artifact path + `dim`; it holds the
  loaded model and releases it on drop. One load per process, never per note — a per-note load is the mistake this
  section exists to prevent.
- **`embed` takes a batch, returns one vector per text, in order**, each L2-normalised by the provider so callers cannot
  accidentally store un-normalised magnitudes and compare them against normalised ones.
- **Errors are typed and carry no secrets and no whole inputs**: artifact missing/unreadable (already `ModelError`),
  artifact not loadable by the runtime, token budget exceeded (§5), runtime error with a bounded message, dim mismatch
  (§6). None of them format a note's text into the message — a vault is private data and an error log outlives the run.
- **The `none` provider stays what it is**: default, refuses, holds nothing, contacts nothing.

## 4. Model identity: a vector must say which artifact made it

`models.id` is already the key that `embeddings.model` quotes, and `install` already takes `--dim --sha256
--prefix-doc --prefix-query`. Two rules follow, both cheap now and expensive later:

1. **Quantisation is part of identity.** A Q8_0 artifact does not produce bit-identical vectors to BF16 — same meaning,
   different bytes. So the id carries it: `embeddinggemma-2-text-q8_0@1`. Re-installing a different quantisation is a
   *different model row*, and its vectors coexist rather than masquerading as an update of the old ones. The `@1` is for
   prefix/pooling changes (§5), which invalidate every vector made under the old spelling.
2. **Prefixes are part of identity, not tuning.** Whatever task instruction goes in front of a document or a query is
   stored on the row and included in what `@n` means. Changing it without bumping is how you get a corpus where half the
   vectors were asked a different question than the other half.

`[embeddings] model = "…"` must name an installed row before anything embeds; naming something absent is a refusal with
the id in it, not a silent no-op (that is the `provider = "none"` behaviour and must not become the local one).

## 5. Tokens, prompts, and what "max length" means for us

From the model repo's own configs: pooling **mean**, `include_prompt: true`, output `last_hidden_state`, then an L2
normalise module; `embedding_dimension = 768`. So a row is 768 `f32` = **3072 bytes**, and the mean must be taken over
real tokens, not padding — which is exactly what llama.cpp's mean pooling does with an attention mask, and exactly the
kind of detail that is invisible until recall is bad.

The text tower reports `max_position_embeddings = 262144`, which is **not** a licence to embed a 200k-token file: quality
is trained for a window orders of magnitude smaller, and one oversized chunk would also blow the memory budget. The rule
this design proposes, in the project's existing style — refuse loudly rather than be quietly wrong:

- `embeddings.max_tokens` (default **2048**, counted with the model's own tokenizer) is the budget for one embedding.
- A chunk over budget is a **reported** condition, never a silent truncation: truncation writes a vector stamped
  `source_hash = <the whole chunk's digest>` while it only read a prefix of it — that row then claims currency for bytes
  it never saw, which is the failure mode this whole milestone keeps circling.
- The cheap fix belongs to the chunker, not the embedder: if `CHUNKER_VERSION`'s size cap already guarantees fewer than
  2048 tokens for any chunk at that byte bound (a token is ≥1 byte, so a byte cap of N cannot exceed N tokens), the
  over-budget case is unreachable by construction and the check above is a fence, not a workflow. **Measure the actual
  bytes-per-token ratio on the corpus** in the benchmark and record which of the two we are living in.

Query-time prefixes belong to phase 2; `install`'s `--prefix-doc/--prefix-query` are stored now so the artifact row is
complete when that round needs them.

## 6. Where vectors get written, and what happens to stale ones

A new command, **`np embed <workspace>`** — deliberately *not* part of `np scan`:

- it consults per-row currency (`chunks.source_hash` vs the chunk text's digest vs `embeddings.source_hash`) and does
  only the work owed;
- it writes in **bounded transactions** (one per batch, not one per corpus), so an interrupted run leaves a consistent
  database and the next run resumes where it stopped — idempotent through `upsert_vector`'s `ON CONFLICT`;
- it reports: chunks seen / already current / embedded this run / refused for budget / provider errors, and prints the
  same kind of honest gap line the scan prints when it could not finish;
- exit code is non-zero if anything was owed and not produced, so a script can tell "nothing to do" from "gave up".

**Backlog D5, answered here:** *vectors are deleted only with their chunk row.* A changed chunk keeps its row id, gets
re-embedded into the same `(chunk_id, model)` slot via upsert; a removed chunk's vectors go by the existing FK cascade;
nothing ever bulk-deletes vectors. That matches F23's mark-don't-delete posture and means an interrupted embed round can
never have destroyed vectors it did not intend to.

**Wrong-dim provider debt, fixed here:** if the provider's `dim` disagrees with `models.dim`/config, refuse before any
write, naming both numbers — today a mismatch surfaces as a raw `DbError` from a CHECK constraint mid-run.

## 7. How this is tested without a model, and how the real one is used

Default CI must stay what it is: no C++ inference, no downloads, no network. So:

- **Fake provider in tests** (already the pattern): determinism, ordering, batch splitting, budget refusal, dim
  mismatch, resume-after-interrupt, "unchanged chunk → provider called zero times". These run everywhere and are the real
  behavioural spec.
- **The feature-gated runtime gets Linux CI compile + unit tests only** — load a ~1 MB fixture GGUF? No: no fake GGUF is
  valid for a real architecture, so CI compiles the feature and runs the tests that need no artifact, and anything
  touching a real model returns early. That is a stated gap, not a hidden one.
- **Real-model work is gated on a runtime env var with a clean early return, never `#[ignore]`** (standing rule):
  `NP_MODELS_DIR` pointing at an installed artifact enables the reference-vector test; absent means it returns and says
  so in its name output.
- **Correctness needs a reference, and this is the honest weak point.** Determinism is not correctness: a wrong pooling
  implementation is perfectly deterministic. The plan is to pin a small fixture corpus (a dozen chunks of real-ish
  Markdown) with expected vectors produced by an *independent* path — sentence-transformers/transformers on the same
  artifact, run once by whoever sets up the model — stored as a golden fixture with cosine-similarity tolerance for the
  quantisation. If that reference cannot be obtained, this round must say "vectors are self-consistent, not verified" in
  every place it reports them, rather than implying more.

No credentials are involved anywhere: `ggml-org/embeddinggemma-2-GGUF` is ungated (verified via the public API), so the
human downloads one file with no account and installs it by path. Secrets rule from §9 of the embeddings design still
applies to phase 2.

## 8. The benchmark, which is the point of the round

Same discipline as planned before: **≥2 runs, report the spread**, never a single lucky number. Measured on this host
(16 cores, 8.9 GB free disk, ~2 GB RAM free at the time of writing — that last one is the binding constraint and may be
why the benchmark needs a quiet machine):

- **load cost**: wall time to make a usable provider from the artifact; peak RSS while loading;
- **steady throughput**: tokens/s and chunks/min over a batched pass, CPU threads recorded;
- **corpus projection**: chunks-per-vault estimate (the perf fixture's 2000 notes → measured chunk count) × measured
  rate = minutes for a first index of ~18k chunks, stated with the arithmetic visible;
- **memory floor**: RSS during steady state, and whether two `np` processes can coexist with the model resident;
- **quality sanity, not quality proof**: nearest neighbours for a few queries over the fixture corpus, printed for a
  human to read. Recall numbers need labels we do not have; claiming them would be exactly the kind of unearned claim
  F18 made a rule about.

The report goes in the repo as a document with numbers and dates, host class ("16-core x86_64 Linux, CPU-only"), model
id including quantisation, and no paths or endpoints.

## 9. Rounds, in order, each independently reviewable

1. **R6a — seam + feature.** `local-model` cargo feature, provider type that loads nothing yet but owns the API, error
   taxonomy, dim-mismatch refusal, config validation (`provider = "local-model"` requires a `model` naming an installed
   row), and the fake-provider behavioural tests. No C++ in default builds; gates unchanged.
2. **R6b — real runtime behind the seam.** llama.cpp binding, artifact load, batched embed, pooling/normalise contract,
   budget check. Reference-vector fixture if obtainable (§7). Feature-gated CI job added here.
3. **R6c — `np embed <ws>`.** Currency query, bounded write transactions, resume, report + `--json`, exit codes, the
   D5 rule stated in docs and asserted by a test that an interrupted run leaves vectors intact.
4. **R6d — benchmark.** Harness, ≥2 runs, spread, projection arithmetic, and the byte-per-token measurement that decides
   whether §5's over-budget path is reachable at all.

Then a whole-branch review across R6a–R6d (the seam-to-report kind), and merge only on the human's word.

## 10. Open questions for review — and one for the human

For review: is (a) the right runtime given the CI cost, or does the C++ dependency argue for (b)? Is `max_tokens` as a
fence-with-refusal the right posture versus capping in the chunker (which costs a `CHUNKER_VERSION` bump and re-cuts
18k rows)? Should `embeddings.provider = "openai-compatible"` be **deleted** from config parsing now — my recommendation
is yes, refuse it with a message naming what changed, because a value that parses and implements nothing is the exact
defect shape this project keeps paying for? And is one Linux feature job enough to keep the feature honest?

For the human: **who downloads the artifact.** I can fetch `embeddinggemma-2-Q8_0.gguf` (309.9 MB, ungated) from this
host and benchmark it; or you download it yourself and point `np models install` at it. The first makes the benchmark
runnable today without touching your credentials; say which you prefer before R6d.
