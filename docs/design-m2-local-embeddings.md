# M2 local embeddings: running EmbeddingGemma-2 inside `np`

Status: **draft v2, revised after design review** (2026-10-29). The review verdict was *build-with-corrections*: seven
P1s, all of them corrections to this document rather than to the plan's shape. They are folded in below; §11 lists what
changed and which of the reviewer's claims I verified myself, because two of them — one about a locked decision, one
about a runtime claim — were load-bearing and one of mine was wrong.

Supersedes the "transport + benchmark" round planned in `docs/design-m2-embeddings.md` §9; that plan assumed an
OpenAI-compatible `/v1/embeddings` endpoint, and the human answered that it does not exist and is not wanted (§10 there).

Model facts below were read from Hugging Face's public API and GGUF metadata on 2026-10-29, unauthenticated; runtime
facts were read from a shallow clone of `ggml-org/llama.cpp` at `0ac1a7f` (2026-10-29) and the crates.io sparse index.
Cost figures are questions for §8's benchmark, never promises.

## 1. What changed, and what round this is

Embeddings are **local**. The application loads the model, runs it on CPU, and writes vectors — with no network at all.
The LLM API is for phase 2 (chatting with notes) and indexing must work fully without it. Network appears once: setup,
to obtain the artifact.

So round 4b is not "HTTP transport + benchmark". It is **a local inference provider + the benchmark that says whether
this machine can carry it**. Same discipline as every other round: a seam with tests, a refusal path, and a measured
number before any claim.

**Non-goals for this round:** semantic search UI, `cluster` columns, hybrid keyword+vector ranking, GPU, a resident
model server/daemon, incremental embedding during `np scan`, and anything at all in phase 2. This round ends with
`np embed <ws>` producing vectors — **verified against a reference only if one can be obtained (§7); otherwise reported
as self-consistent, not verified** — plus a throughput number judged against the threshold in §8.

## 2. The decision that matters: what actually runs the model

Three ways to execute EmbeddingGemma-2 in Rust. What is now *evidence* rather than recollection is marked ✓; what is
still a claim to check at pin time is marked ⚠.

| Option | Evidence-backed advantage | Real cost |
| --- | --- | --- |
| **(a) llama.cpp over GGUF**, via FFI bindings | ✓ the artifact declares `general.architecture = gemma-embedding2` (read from the first KB of `embeddinggemma-2-Q8_0.gguf`, which also reports `general.size_label = 271M`, `license = apache-2.0`); ✓ llama.cpp at `0ac1a7f` knows that architecture — `LLM_ARCH_GEMMA_EMBEDDING2` in `src/llama-arch.cpp`, build cases in `src/llama-model.cpp`, and the artifact pair is named in `tests/test-model-resolution.cpp`; ✓ pooling is *declared by the file* (`gemma-embedding2.pooling_type`) and read from metadata (`LLM_KV_POOLING_TYPE`), not guessed; ✓ a maintained Rust binding exists (`llama-cpp-2`, latest `0.1.160`, non-yanked, static build by default with `dynamic-link` available); ✓ tokenizer ships inside the artifact — no second implementation to keep in byte-agreement | A C/C++ build enters the dependency graph on three platforms under `--locked`. ⚠ support must exist **at the llama.cpp revision the chosen crate vendors**, not at upstream HEAD — checked in R6b0 by grepping the vendored source, not by trusting a changelog. FFI safety boundary we own. Upstream revision churn |
| **(b) ONNX Runtime** via `ort` | Prebuilt runtime, no model-code compile | Not "free": `ort` fetches prebuilt runtime binaries at build time — network plus reproducibility questions on three release OSes instead of a compiler. The published ONNX repo's `.onnx` files are 0.2–0.5 MB stubs with weights in external-data shards, and **`install` has no multi-file path today** (`Install { source, sha256 }` is one file, one digest, stored as `<dir>/<id>`). Tokenizer re-implemented in Rust; pooling and normalisation ours to get right |
| **(c) candle (pure Rust)** | No foreign toolchain, everything inspectable | ✓ `candle-transformers/src/models/mod.rs` on `main` lists `gemma`, `gemma2`, `gemma3`, `gemma4`, `paligemma`, `quantized_gemma3`, `recurrent_gemma` — **no `embedding_gemma2`**. We would write a 24-layer GQA decoder (`hidden_size = 512`, 4 heads / 2 KV, `head_dim = 256`), the sliding-window pattern and the projection to the stored width ourselves, with no reference implementation to check against |

**Recommendation: (a), behind a cargo feature.** The model's publisher ships the exact artifact, llama.cpp implements
that architecture today, and both alternatives move correctness risk into code we own — and this project's whole posture
is that derived data must be *provably* derived from the bytes.

Feature-gating (`np-core/local-model`) keeps default `cargo test --workspace` free of a C++ build, which protects every
gate in the standing list. A feature nothing compiles rots, and what rots silently is specific: clippy lints inside
`cfg(feature)`, rustdoc warnings in gated docs, and artifact-gated tests whose early return is indistinguishable from a
pass forever. **None of the four steps below exists yet** — `.github/workflows/ci.yml` has no `--features local-model` row
at all (verified: `grep -n "features" .github/workflows/ci.yml` → no match), so this paragraph is a promise with an owner
now rather than a description: **R6b0 adds it**, per §6 of the parent design ("**One CI step per OS row** runs
`cargo check --locked -p np-core --features local-model`"):

1. `cargo check --locked -p np-core --features local-model` — **on both ubuntu and windows rows**, compile-only;
2. `cargo clippy --locked -p np-core --features local-model --all-targets -- -D warnings`;
3. `RUSTDOCFLAGS="--deny warnings" cargo doc --locked -p np-core --features local-model --no-deps`;
4. `cargo test --locked -p np-core --features local-model` where anything needing a real artifact prints a named
   `NOT COVERAGE: <test> — no artifact` line and returns (the F18 precedent), and the job greps that line into its log so
   "compiled, never ran" is written on the record instead of implied.

**Release builds keep the feature OFF** until R6d lands and a Windows build of it has been recorded once in CI. Shipping
release binaries whose heavy path was compiled on one OS only would be F18's mistake with a different noun.

The benchmark can falsify this choice; if resident cost or throughput is unacceptable, (b) is the fallback and §3's seam
is what makes that a swap rather than a rewrite.

## 3. The seam, and what stays true either way

`np_core::embed::Embedder` already exists (`model_id`, `dim`, `embed(&[&str]) -> Vec<Vector>`). The local provider
implements it and nothing else in the tree learns that inference happened:

- **Constructed from the installed `models` row — not from a bare path.** `(id, dim, prefix_doc)` plus the artifact path.
  It prepends `prefix_doc` itself, and its `model_id()` *is* that row's `id`, so no second source of identity exists.
  This is forced by the shape of the trait: `embed(&self, texts: &[&str])` takes no prefix argument, so if the
  constructor does not carry the row, either the prefix is never applied or every caller re-implements it — and the
  parent design §3 already rules "a provider that ignores prefixes is a bug".
- **Loading is explicit and bounded.** One load per process, never per note; the provider holds the model and releases it
  on drop. A per-note load is the mistake this bullet exists to prevent.
- **`embed` takes a batch, returns one vector per text, in order**, each L2-normalised *as our asserted property* (§7's
  unit-norm test), not as a library default we hope was switched on.
- **Errors typed, secret-free, input-free**: artifact missing/unreadable (existing `ModelError`), artifact not loadable,
  token budget exceeded (§5), runtime error with a bounded message, dim mismatch (§6). None formats note text into the
  message — a vault is private and an error log outlives the run.
- **The `none` provider stays what it is**: default, refuses, holds nothing, contacts nothing.

## 4. Model identity: a vector must say which artifact made it

`models.id` is already the key `embeddings.model` quotes, and `install` already takes `--dim --sha256
--prefix-doc --prefix-query`. Two rules follow, both cheap now and expensive later:

1. **Quantisation is part of identity.** A Q8_0 artifact does not produce bit-identical vectors to BF16 — same meaning,
   different bytes. So the id carries it: `embeddinggemma-2-text-q8_0@1` (28 chars; `validate_id` accepts `[a-z0-9._@-]`,
   so it installs). Re-installing a different quantisation is a *different model row*, and its vectors coexist instead of
   masquerading as an update.
2. **Prefixes are part of identity, not tuning** — and `install` already enforces the shape of that rule: it refuses an
   existing id whose whole declared tuple differs (`dim`, `sha256`, and both prefixes), writing nothing. A prefix change
   therefore cannot drift under a live id; it forces a new id, which is what this rule wants.

**What `@n` means, stated completely** (vectors have no per-row analogue of `chunks.chunker`, so the version has to carry
it): `@n` changes whenever anything on our side would change vector bytes — **prefix, pooling mode, normalisation, and the
truncation in §5**. The current number lives in one constant in `np-core`, named in the design and asserted by a test that
installs two ids differing only in `@n`. Otherwise a later pooling fix is undetectable against 18k existing rows — the
F15/D1 class of defect.

`[embeddings] model = "…"` must name an installed row before anything embeds; naming something absent is a refusal with
the id in it, never a silent no-op (that is `provider = "none"`'s behaviour and must not become the local one).

## 5. Width, tokens, prompts: what a vector actually is

**Stored width is 768 — amended by the human on 2026-10-29.** The decision this section was written to protect was "512-d
via MRL truncation" (spec `:84`/`:234`/`:346`, `todo.md`), and draft v2 recorded it as locked while noting that the model's own
`1_Pooling/config.json` says `embedding_dimension = 768`. The human has chosen native width. Precise about what carries that
amendment, because a design doc claiming agreement it has not verified is the defect this document has now committed twice:
**`todo.md`'s locked list and this section carry it; the spec keeps its 512 text as history**, deliberately unedited (it records
what was decided on 2026-10-23), and every other site that said 512 — §9's gate, §10's question to the human, `crates/np-core/tests/embed.rs`'s
`LOCAL_DIM`, the parent design's pricing lines — is corrected or annotated rather than left to argue. What changed underneath is
worth keeping in plain sight, because it is why the number was ever contested:

- **Cost, restated honestly:** a row is 768 `f32` = **3072 bytes**, so ~18k chunks is ≈**55 MB** rather than the 37 MB the
  parent design priced. That is still small; it changes no architectural choice here.
- **Truncation stays available, and cheaper than a re-embed — with three preconditions that make it its own design.** Google's model card for `embeddinggemma-2` states MRL support
  natively — "truncated embeddings across 128d, 256d, 512d, and 768d … up to a 6x reduction in vector storage costs with
  minimal impact on quality" — which *closes the open question draft v2 left for R6d*: the sub-vectors are trained, not an
  accident of slicing. So if 55 MB becomes a problem, narrowing is a **re-store of vectors already in the table** (slice to
  N, re-normalise) rather than a re-embed — no model run at all. It is not free: it changes model identity (§4), so it means
  a new `@n` and a migration over stored rows, and cosine comparisons must never mix widths. Order still matters if it ever
  happens: mean-pool → L2-normalise → slice → **re-normalise** (slicing first leaves a non-unit vector). The preconditions,
  because "just re-store it" understates the work and R6c should not discover them under pressure:
  - it writes **new rows under a new id** (`PRIMARY KEY (chunk_id, model)` plus §4's identity rule leave no other option), so
    peak cost during the migration is wide *plus* narrow — ≈92 MB for 18k chunks — and the saving only lands when something
    deletes the old id's vectors, which §6 currently forbids ("no command deletes a `models` row today"). The migration and the
    bulk-delete rule have to be designed together.
  - the new row has **no artifact**, i.e. `sha256 IS NULL`, and today only `install` creates rows and it always writes a digest.
    So narrowing needs a row-creating path that is not `install`.
  - **No round between R6a and R6d performs this.** It gets its own design section when a real storage problem exists.
- **Window, now cited rather than reasoned about.** The card gives **8,192 tokens** of context for this model. Draft v2 said
  "nothing here reads a trained window"; it does now. Consequences: `embeddings.max_tokens`'s default of 2048 sits comfortably
  inside it, and `MAX_EMBEDDING_TOKENS = 32_768` **exceeds what the model can attend to** — recorded as a bound to tighten to
  8,192 in R6b2 alongside the enforcement test, not silently left generous. (`max_position_embeddings = 262144` in
  `config.json` is a RoPE/train-length property, not a usable embedding window; §5's refusal posture does not depend on which
  of the two numbers you read.)
- **Whole model = two files, and they do carry both encoders — measured, not assumed.** The card describes 740M parameters:
  a 270M text backbone plus selectively loadable vision (170M) and audio (300M) encoders into one 768-d space. In GGUF that is
  `embeddinggemma-2-Q8_0.gguf` (309.9 MB) **plus** `mmproj-embeddinggemma-2-Q8_0.gguf` (554.8 MB) = **864.7 MB**. A design
  review reasonably suspected the pair of being vision-only — an `mmproj` is usually a vision projector, and no file in that
  repo is named for audio. The mmproj's own header settles it: it declares **both** `clip.has_vision_encoder` and
  `clip.has_audio_encoder`, with `clip.audio.*` geometry alongside the vision keys (read from the served bytes, 2026-10-29).
  So the pair satisfies "the whole model"; no third download exists or is needed.
- **And a claim of mine was too strong — corrected the same way.** §12's first draft said there was "no image→embedding path
  in llama.cpp's multimodal layer" at `0ac1a7f`. What I had actually checked was one tool's headers. Checked properly:
  `mtmd_encode_chunk()` exists and returns embedding vectors (`mtmd.cpp`: `mtmd_encode_chunk_impl(..., std::vector<float> & out_embd)`),
  and the pinned binding exposes the surface — `llama-cpp-2 0.1.160` ships `src/mtmd.rs` with `MtmdContext::{support_vision,
  support_audio, encode_chunk}` and `MtmdBitmap::from_audio_data`. **What is still unproven** is the thing that actually matters
  to us: a *pooled 768-d embedding over mixed text-and-image tokens* for `LLM_ARCH_GEMMA_EMBEDDING2`, rather than image features
  injected into a generative run. That is a spike with a name (R7c below), not a doc assertion either way — and it is the honest
  reason to build the two-file plumbing before promising image search.

Pooling itself comes from the artifact: `pooling_mode = "mean"`, `include_prompt = true`, output `last_hidden_state`,
then a normalise module — and the GGUF declares `gemma-embedding2.pooling_type` for the runtime to read. The mean must be
over real tokens, not padding; that is what an attention mask is for, and it is invisible until recall is bad, which is
why §7 tests batch invariance rather than trusting it.

**Token budget.** `max_position_embeddings = 262144` is not a licence to embed a 200k-token file. (Reasoning, not
measurement: nothing here reads a trained window; the model card would be the citation and R6d should record one.) The
posture stays refuse-loudly:

- `embeddings.max_tokens` (**default 2048**, added in R6a together with its default-when-absent; note `EmbeddingsConfig`
  is `#[serde(deny_unknown_fields)]`, so today writing that key is refused naming key and file — that is the round that
  changes it) applies to **what the model actually sees**: `prefix_doc` + chunk text + special tokens, since
  `include_prompt = true` means the counted sequence is not just our bytes.
- An over-budget chunk is a **reported** condition, never a silent truncation: truncation stamps
  `source_hash = <the whole chunk's digest>` on a vector that read a prefix — a row claiming currency for bytes it never
  saw, which is this milestone's recurring failure.
- The chunker's `MAX_CHUNK_BYTES = 1800` makes overflow *unlikely* for prose (a token is at least one byte of the text it
  came from), but that is an observation with holes, not a theorem: SentencePiece/NMT normalisation can expand bytes, so
  the bound is on normalised bytes rather than stored ones; and prefix plus specials cost tokens that come from no chunk
  bytes at all. Keep the fence because it is cheap — **not** because the case is unreachable. R6d measures real
  bytes-per-token on this corpus and says which world we live in; moving the cap into the chunker instead would cost a
  `CHUNKER_VERSION` bump (re-cut ~18k rows *and* re-embed them) for a budget no measurement has yet shown to bite.

Query-time prefixes belong to phase 2; `install`'s `--prefix-doc/--prefix-query` are stored now so the row is complete
when that round needs them.

## 6. `np embed <workspace>`: what it writes, and what it must never do

A new command, deliberately **not** part of `np scan`: the scan wraps its whole write pass in one transaction
(`unchecked_transaction()` … `tx.commit()`), so unbounded model work inside it would leave a concurrent `np` waiting out
`busy_timeout` — which is what the parent design §5 forbids.

- **It reads chunking; it never writes it.** No `chunks` rows, no `EMBED_VERSION_KEY`. It is one reader of the scan's
  output, not a second path through it (the F19 ruling's shape).
- **Owed = per-row currency, liveness-fenced.** A chunk is owed when its text digest has no matching vector for the
  configured model id — and the query joins `files.state = 'ok'`, so **a stale note's chunks are never owed (backlog D3)**.
  Without that join this command is the first production reader to compute vectors for a deleted file's text and report
  them current; `db.rs`'s own doc says a gone file keeps `state = 'stale'` with every child row intact, so the rows are
  right there looking embeddable. R6c ships a fixture with one stale note.
- **Model work never runs inside a transaction.** Infer the batch, *then* open one to write it. Bounded transactions —
  one per batch, not one per corpus — so an interrupted run leaves a consistent database and the next resumes; idempotent
  through `upsert_vector`'s `ON CONFLICT`.
- **`SQLITE_BUSY` is a reported gap, never a silent skip**: non-zero exit, nothing stamped for that batch. With
  `BUSY_TIMEOUT_MS = 1800` and the scan holding its write lock across its pass, an embed running beside a scan will hit
  this; the test is that a scan still succeeds while an embed sits between batches.
- **Stamp rule (backlog D4 applied to the writer):** `embeddings.source_hash` is the digest of the bytes handed to the
  model — never `chunks.source_hash` re-read at write time, and never `files.hash`. That is what makes a concurrent
  re-cut produce *stale* rows rather than falsely current ones.
- **Read rule carried forward:** every read filters on an explicit model id (a `vectors_for(model)` helper; no such
  reader exists yet), because `PRIMARY KEY (chunk_id, model)` is the only thing preventing two models' vectors from being
  compared as if interchangeable.
- **Wrong-dim provider is refused before any write**, naming both numbers — today a mismatch surfaces as a raw `DbError`
  from a CHECK constraint mid-run.
- **Three exit states, not two.** (i) nothing owed → exit 0; (ii) configuration disabled — `provider = "none"`, or a
  `model` naming no installed row — refuses **before any write**, naming key and id, exit 1 labelled *configuration
  disabled*, because `EmbedError::Disabled` is documented as "the default state of a fresh install, reported as a count
  rather than as a fault" and must not become a fake failure; (iii) owed and not produced → non-zero, the only code path
  that means "gave up".
- **Report:** chunks seen / current / embedded / refused-for-budget / busy-gap / errors, plus the verification status line
  from §7. It does *not* replace the still-unimplemented "chunks owed with no vector" count in `ScanReport` (`todo.md`).

**Backlog D5, answered: vectors are deleted only with their chunk row.** Verified against the schema rather than asserted:
migration 7 declares `chunk_id INTEGER NOT NULL REFERENCES chunks(id) ON DELETE CASCADE`, every connection sets
`PRAGMA foreign_keys = ON`, and `deleting_a_note_takes_its_chunks_and_their_vectors` asserts the cascade at runtime on a
fresh database; `write_chunks_for_note`'s tail delete already calls that "the correct loss". So removal needs no explicit
caller. A changed chunk keeps its row id and is re-embedded into the same `(chunk_id, model)` slot via upsert. Two caveats
stated rather than glossed: `embeddings.model` also cascades from `models(id)` — pinned by
`deleting_a_model_takes_the_vectors_that_named_it` — so "nothing bulk-deletes vectors" means **no command deletes a
`models` row today**, and if some future `np models remove` appears, that is the bulk delete this rule forbids; and
migrations run with `PRAGMA foreign_keys = OFF`, so any future migration deleting parent rows must delete children by hand
(backlog D10).

## 7. Verification: what we can prove here, and what we must not imply

Determinism is not correctness — a wrong pooling implementation is perfectly deterministic. The primary checks are ones
that need no reference at all:

1. **Batch invariance:** `embed(&[t])` equals `embed(&[t₁ … t_k])` elementwise within tolerance for any member of the
   batch. This catches padding / attention-mask / sequence-id contamination — *the* failure mode of batched llama.cpp
   embedding. Against the fake provider this proves nothing; it only counts once a real model is loaded.
2. **Unit norm:** ‖v‖ = 1 ± ε for every produced vector — catches a missing normalise step that cosine-only tests cannot
   see, and makes §3's promise ours rather than the library's.
3. **Cross-quantisation agreement:** cosine between Q8_0 and BF16 vectors over a small fixture corpus (both artifacts
   ungated). A wrong pooling/tokenizer/architecture collapses that cosine long before it shows up in nearest neighbours a
   human eyeballs. Cost is real — BF16 is 558 MB of weights against ~2 GB free RAM here — so it runs as its own invocation
   and feasibility is a measurement, not an assumption.

**Optional reference path, honestly scoped:** expected vectors from an independent Python path are *possible* — the HF repo
is tagged `transformers`/`sentence-transformers` with `architectures = ["EmbeddingGemma2Model"]` — but Transformers does
not read GGUF, so it needs the **other** artifact (`model.safetensors`, 1488.9 MB bf16), on a host this design itself
calls memory-bound. It therefore starts with a free-RAM check and degrades to "skipped, and here is why" rather than
pretending to be the verification story. Pinned expectations, when obtained, are compared by cosine tolerance sized to
the quantisation gap measured in check 3.

**And the caveat must not live in a design doc:** when no reference golden exists, `np embed`'s own report prints
`self-consistent, not verified`. A caveat nobody reads at 2 a.m. is not a caveat.

Testing otherwise stays as before: fake-provider tests for determinism, ordering, budget refusal, dim mismatch, resume,
and "unchanged chunk → provider called zero times" run everywhere; the feature-gated runtime gets §2's four-step CI job
with no artifact and no network; real-model work is gated on a runtime env var with a clean early return (`NP_MODELS_DIR`),
**never `#[ignore]`**. No credentials anywhere: `ggml-org/embeddinggemma-2-GGUF` is ungated, so setup is one file
downloaded by a human and installed by path.

## 8. The benchmark, which is the point of the round

≥2 runs, report the spread, never a single lucky number. Measured on this host (16 cores, 8.9 GB free disk, ~2 GB of 7 GB
RAM free at writing — the binding constraint, and possibly why R6d wants a quieter machine):

- **load cost**: wall time to a usable provider; peak RSS during load;
- **steady throughput**: tokens/s and chunks/min over a batched pass, thread count recorded;
- **the curve that changes design, not just dates**: tokens/s *and* peak RSS as a function of input length (≈64 / 256 /
  512 / 1024 / 2048 tokens). This is what decides `max_tokens`, whether the batch API is honest about cost, and whether the
  next `CHUNKER_VERSION` should aim at *shorter* chunks. Corpus projection alone only tunes a date;
- **corpus projection with the threshold applied, not implied**: real corpus size × measured chunks/hour against the spec's
  rule — "**keep EmbeddingGemma 2 if a full first pass fits in one evening**" (spec §decision rule) — printed as arithmetic
  and shown passing or failing. Numbers without the threshold they were judged against are how a benchmark becomes a poster;
- **memory floor**: steady-state RSS, and whether two `np` processes coexist with the model resident;
- **quality sanity, not quality proof**: nearest neighbours for a few queries over the fixture corpus, printed for a human
  to read. Recall numbers need labels we do not have; claiming them would be exactly the unearned claim F18 made a rule about.

Plus §5's bytes-per-token measurement and §7 check 3's quantisation cosine distribution, both recorded in the same report:
a document with numbers, dates, host class ("16-core x86_64 Linux, CPU-only"), model id including quantisation, and no
paths or endpoints.

## 9. Rounds, in order, each independently reviewable

Every brief mandates a `wip:` commit at first compile — five workers this session died at ~30-minute endpoints, and the
ones with a checkpoint cost nothing.

1. **R6a — seam + feature.** `local-model` cargo feature, provider type that loads nothing yet but owns the API, error
   taxonomy, dim-mismatch refusal, `[embeddings] max_tokens` (default 2048) and config validation (`provider =
   "local-model"` requires a `model` naming an installed row), plus fake-provider tests including §3's "the prefix on the
   row reaches the model". No C++ in default builds; gates unchanged. *If the implementer is still writing error variants at
   25 minutes, config validation splits off.*
2. **R6b0 — dependency and link only.** Add `llama-cpp-2` (pinned, `--locked`), feature-gated, build it on ubuntu and
   Windows in CI, and **verify the vendored llama.cpp revision knows `gemma-embedding2`** by grepping the vendored source.
   Loads nothing. This round exists because a first compile that builds a C++ runtime can eat an implementer's whole
   window before any Rust type-checks.
3. **R6b1 — load → tokenize → batched forward.** Provider construction from a `models` row, artifact load, pooling and
   §5's pool→norm→slice→renorm chain, unit-norm assertion.
4. **R6b2 — budget + verification tests.** `max_tokens` enforcement over prefix+text+specials, batch invariance,
   cross-quantisation cosine, optional Python reference if RAM allows.
5. **R6c — `np embed <ws>`.** Currency query with the `files.state = 'ok'` fence, inference outside transactions, bounded
   writes, resume, three exit states, report + `--json` including the verification-status line, and a fixture proving an
   interrupted run leaves existing vectors intact.
6. **R6d — benchmark.** Harness, ≥2 runs, spread, the length curve, projection against the one-evening threshold,
   bytes-per-token, quantisation cosine distribution.

Then a whole-branch review across the shipped rounds, and merge only on the human's word.

| round | content | why here |
| --- | --- | --- |
| **pre-round (done in `b3f05d2`+)** | width residue cleanup; `MAX_EMBEDDING_TOKENS` 32 768 → **8,192** with its message and doc | one constant, no coupling; a bound above the model's window is a promise nobody can keep |
| **R6b0** | dependency + link only, vendored-revision grep, **the four-step CI job §2 promises**, **the pin table as data** (`const PINNED`, no network), and the reserved-name rule for fetch files (§12) | all offline; nothing dials out; survives a C++ build inside one window |
| **R6b1 / R6b2** | load → tokenize → forward; budget enforcement + verification | unchanged, since the ceiling fix already landed |
| **R6c** | `np embed <ws>` and §6's four labelled exit arms, reading a marker **read-only** ("artifact absent — run `np models fetch`") | the label exists before the writer does, so R6c never waits on the fetcher |
| **R6d** | benchmark, including whether an mmap-first load reads all 865 MB (which decides §12's digest cost) | measurement round |
| **R7a — marker only** *(shipped `ad9ed5a`)* | read/report the `.fetching.*` / `.part.*` names, `"fetching"` listing section, staleness *display* wording, tests that plant files. **No writer**: nothing can create a marker until R7b's first byte, which is why every test plants its own | fully offline-testable; this is the human-visible half of "tell me it is downloading" |
| **R7b — fetch** | HTTP GET → `.part` → streaming digest (incremental API in `hash.rs`) → pin compare → rename-if-absent → `db::insert_model`; two files, two digests; four named refusals; no resume, no retry, no marker reuse | the network round alone, after the marker exists |
| **R7c — multimodal spike (conditional)** | prove or kill a pooled 768-d embedding over mixed text+image tokens for this architecture through the pinned binding | only then does image indexing enter a roadmap; §5's open question |
| **M4** | the "first start" auto-fetch trigger, in `npd` | no other binary dials out unprompted (see §12) |

**Two answers gate R6b0** — width is settled (768, §5) and so is provenance (fetch, §12):

- the pinned binding's vendored llama.cpp revision supports `gemma-embedding2` — grep the vendored source in R6b0, not upstream HEAD;
- and free RAM for R6b2's cross-quantisation check, which may simply have to wait for a quieter machine rather than be assumed.

**R6a has shipped** (`local-model = []`, `embed::LocalModelProvider`, `[embeddings] max_tokens`), so the list below is what
remains — with §12's fetch work placed where review said it belongs, because "add a network client" next to "compile a C++
runtime" is how a round loses its endpoint window:

## 10. Open questions

**Answered by the human on 2026-10-29, and no longer open:** `np` fetches the artifact itself on first start (§12), so nobody
downloads by hand unless they choose the offline door; and stored width is **native 768**, not 512-via-MRL (§5). Both were
locked decisions that the human reversed, which is their right and this document's job to record — `todo.md` carries the
amendments, and §5 names exactly which documents still hold the old numbers as history.

What remains genuinely open for review or measurement, not for preference: whether a pooled multimodal embedding works at all
(§5, R7c), what an mmap-first load actually reads from disk (R6d, decides §12's digest cost), and whether 8,192 belongs beside
the provider rather than in a global constant (§5's own note on the MiniLM fallback).

**For review (folded into §2/§5/§7 above):** whether the `openai-compatible` spelling should be deleted or kept-parseable-
and-refused — resolved here in favour of **keeping it parseable and refusing at `Config::load`**, because deletion alone
would fail an existing config with serde's verbatim "unknown variant" text (which `ConfigError::Parse` passes through) and
stop `np scan` cold without naming a reason; the F17 precedent (`continuation_indent`) is exactly this shape. Cost stated so
it isn't discovered mid-round: `EmbeddingProvider`'s doc, the `every_provider_spelling_parses…` case list, and
`embed.rs`'s `Unavailable` doc all mention the spelling today. (Parent §10.3 assigned this decision to "§2 there"; it is
argued here in §10 and cross-referenced from §2.)

## 11. What the design review changed, and what I verified myself

Verdict was *build-with-corrections*. Corrections folded in: stored width (P1-1, **my draft contradicted a locked human
decision** — spec `:84`, `:234`, `:346` and `todo.md:72` all say 512-d MRL, and the shipped config fixture parses
`dim = 512`); provider built from the `models` row (P1-2); the D3 liveness fence in the owed-query (P1-3); inference-outside-
transaction plus a `SQLITE_BUSY` posture (P1-4); three exit states instead of two (P1-5); verification rewritten around
batch-invariance/unit-norm/cross-quantisation with the caveat printed by the command rather than only documented (P1-6);
per-OS feature compile and release-off (P1-7). Folded P2s: runtime claims replaced with evidence; `sliding_window_pattern`
replaces the word "hybrid"; the token-budget claim demoted from theorem to observation with its holes named; `max_tokens`
named as a new key on a strict struct and assigned to R6a; the second cascade qualified; stamp and read rules written down;
the one-evening threshold imported; ONNX's build-time download and missing multi-file install path stated; `@n` gloss
completed with where the number lives; the trained-window sentence marked as reasoning; §2/§10 cross-reference fixed.

Claims I checked rather than trusted (reviewer had no shell, and this repo has already been burned by an invented finding):

| Review claim | Verified by | Result |
| --- | --- | --- |
| stored dim contradicts a locked decision | `grep -n "MRL\|512-d" spec todo.md docs/design-m2-embeddings.md`, `grep dim = 512 tests/config.rs` | **true**, four independent sites — P1-1 stands and my draft was wrong |
| parent design already decided per-OS feature compile | `grep -n "per OS row" docs/design-m2-embeddings.md` | true (§6, "One CI step per OS row runs `cargo check --locked -p np-core --features local-model`") |
| cascade makes the D5 answer true | `grep REFERENCES chunks(id)` → `ON DELETE CASCADE`; tests `deleting_a_note_takes_its_chunks_and_their_vectors`, `deleting_a_model_takes_the_vectors_that_named_it` both present | true, including the second cascade caveat |
| embed-side readers have no liveness fence | `grep "FROM chunks WHERE note_id"` → three readers, none joining `files` | true (D3) |
| busy posture is unstated and real | `BUSY_TIMEOUT_MS: u32 = 1800`; scan's `unchecked_transaction()`/`tx.commit()` | true |
| disabled config is "a count rather than a fault" | `grep -n "count rather than as a fault" embed.rs` → the doc line on `EmbedError::Disabled` | true |
| llama.cpp support unverified | cloned `ggml-org/llama.cpp@0ac1a7f`; `LLM_ARCH_GEMMA_EMBEDDING2` in `src/llama-arch.cpp`, cases in `src/llama-model.cpp`, artifact named in `tests/test-model-resolution.cpp`; GGUF header declares `general.architecture = gemma-embedding2` and `gemma-embedding2.pooling_type` | **now verified at HEAD**, with the pin-time caveat kept (the crate's vendored revision, not upstream main) |
| candle lacks the architecture | `models/mod.rs` on `main`: gemma/2/3/4, paligemma, quantized_gemma3, recurrent variants — no `embedding_gemma2` | true; option (c) stays expensive |
| Transformers may not support the model | HF API: `architectures = ["EmbeddingGemma2Model"]`, tags include `transformers`, `sentence-transformers` | reviewer's open question resolves *yes*, but it needs the 1488.9 MB safetensors — so §7 keeps it optional |
| a maintained Rust binding exists | crates.io sparse index: `llama-cpp-2` `0.1.160`, not yanked, static build default | true; version pin and revision check are R6b0's job |

### Addendum — the §12 review (`d39e3e9f-…`), and two claims of mine it corrected

Verdict *build-with-corrections*: ten P1s, six P2s, no P0. The core was sound (sidecar-before-the-row is correct because
`install` inserts the row only after verification; keeping `install --path` as the offline door matches the amended lock;
refusing rather than fetching inside `np embed`). What the review caught in my text, all verified before accepting:

- **A model id can name the marker file.** `validate_id` allows `.` after the first character, so `alpha.fetching.json` is a
  legal id whose artifact path *is* row `alpha`'s marker path — writing a marker could overwrite a verified artifact. Fixed by
  leading-dot names (`.fetching.<id>.json`, `.part.<id>.<nonce>`), which the first-character rule makes unforgeable **by
  construction**. The same charset fact proved `nonce()`'s doc comment false ("a temp name can never be mistaken for a valid
  id") — corrected in code, not just here.
- **"A fifth `ArtifactState`" was wrong**, and the enum's own doc says why: four arms are mutually exclusive verdicts on one
  artifact, while fetching is orthogonal (no row at all on first start; a present row can be re-fetching). It becomes a
  listing-level `"fetching"` section beside `"unclaimed"`, with `LIST_KEYS` treated as the pinned protocol it is.
- **The marker and `.part` would have been reported as junk** — unclaimed entries — so a fetch was announced twice, and in the
  first-start case only as junk. Recognised names are now excluded from `unclaimed`.
- **"install's publish path exactly" meant copy+rename**, i.e. writing 310–865 MB twice on an 8.9 GB-free host; fetch keeps the
  *rename* discipline and copies nothing.
- **Clock-based staleness was uncheckable** in a repo that deliberately does not trust the clock (`now_secs()` clamps, `nonce()`
  contributes zeroes). Liveness becomes consent: refuse while a marker exists, print its fields, offer an explicit
  `--discard-marker`; never implicit deletion, never resume-by-guessing.
- **Rules 6 and 7 contradicted each other** about naming what was attempted (HTTP error strings carry URLs). Bounded:
  `source_kind` + repo/filename + status class, never host or full URL, with a leak test.

Two of its own questions were settled by measurement rather than argument: the mmproj carries **both** encoders (§5), and
`llama-cpp-2 0.1.160` does expose mtmd — which is how my "no image→embedding path" claim got corrected in both directions, one
too strong and one too narrow. The reviewer's suspicion that audio was unaccounted for in the GGUF pair was itself wrong, and
worth recording as a lesson in the same shape as mine: nobody should conclude what a file contains from its name.

## 12. Fetching the model on first start, and the marker that says it is happening

Amended by the human 2026-10-29: *"it should be automatically downloaded in the first start. And while it downloads add a
flag or marker which we can later use to indicate the user that the model download is currently on going."* That reverses two
locked rules — "downloaded once by `np models install`, no API at index time" and "`install` stays local-file-only" (parent
§10.2) — so it gets its own section rather than a quiet edit elsewhere. `install --path --sha256` is **kept** as the offline
door: an air-gapped machine must still be able to install a model, and a fetch path that cannot fall back to a file the human
placed by hand is one refusal too many.

This section is rewritten after review (`d39e3e9f-…`, ten P1s); §11's addendum records what was wrong in the first draft.

**Premises this amendment retires.** Three documents assert that nothing in `np` can make a network call, and they are true
today because of what does *not* exist — no HTTP crate anywhere (`grep -rn "reqwest|ureq|hyper|rustls" **/Cargo.toml` → no
match), plus a type-level fence (`NoEmbeddingProvider` holds no client; `tests/embed.rs` pins `size_of::<NoEmbeddingProvider>() == 0`)
and `models.rs`'s module doc ("Nothing here contacts anything: no HTTP client, no URL, no key"). Naming the retirement now,
rather than letting a round discover it:

- The **fetcher lives in `np-core::fetch`, behind its own cargo feature `fetch`, and that one is default-ON** — unlike
  `local-model`. A fetch path release builds compile out would defeat the amendment; the cost, stated plainly, is an HTTP+TLS
  dependency on three release OSes. `np scan` still cannot reach it: no call from `scan.rs`, asserted by a test that walks no
  client type, and the `NoEmbeddingProvider` size fence stays exactly as it is because indexing and embedding are where that
  promise lives.
- `models.rs`'s "nothing contacts anything" narrows to "**`install` and `list` contact nothing**; fetching is a separate module
  with its own rules", which is what will actually be true.

### The rules

1. **Who fetches, and when.** `np models fetch <workspace> --id <id>` is the real command and the only thing that dials out in
   this milestone. "First start" is satisfied in two halves: R7a/R7b ship the marker and the command, and **the automatic
   trigger belongs to the daemon (M4)** — no other binary starts a nine-hundred-megabyte download because it was invoked while
   a config named a model. `np scan` never fetches; `np embed` refuses with §6's `artifact absent — fetch first` arm rather
   than downloading inline (a nine-minute transfer inside a command that then writes rows is a transactional and UX mess at once).
2. **Names no id can forge.** Marker and temp files are `.fetching.<id>.json` and `.part.<id>.<nonce>` — leading dot, which
   `validate_id`'s first-character rule ("a lowercase letter or a digit") makes unrepresentable as a model id. The first draft's
   `<id>.fetching.json` was forgeable: `alpha.fetching.json` is a legal id whose artifact path *is* row `alpha`'s marker path, so
   writing a marker could overwrite a verified artifact. Recognised fetch names are **excluded from `unclaimed`** — that list keeps
   meaning "nothing this program recognises" — and the marker is rewritten atomically (temp + rename), which creates transient
   `.fetching.*` names for the same rule to cover. This also corrects `models::nonce`'s doc, which claimed a temp name could never
   look like an id: it can, because ids allow dots.
3. **`fetching` is not an `ArtifactState`.** The enum's own doc argues against a fifth arm — four mutually exclusive verdicts on one
   artifact — and fetching is orthogonal to all of them: on first start there is **no row at all**, and a present row can be
   re-fetching. So it is a listing-level section beside `unclaimed`: `{ "models": [...], "fetching": [...], "unclaimed": [...] }`.
   Adding the key is a pinned-protocol change and must fail `crates/np-cli/tests/models_cli.rs`'s `LIST_KEYS` fence, deliberately.
4. **Liveness is consent, not a clock.** The repo never trusts wall time for decisions (`now_secs()` clamps a pre-epoch clock to 0;
   `nonce()` contributes zeroes), and a resumed laptop moves it further than any heartbeat interval. So: if a marker exists when
   `fetch` starts, **refuse** — naming the path, `started_at`, `bytes_done/bytes_total`, and "if no fetch is running, delete this file
   and re-run". Time fields are *displayed*, never used to decide; a crash-restart neither resumes (that needs Range plus proof the
   first half came from the same upstream revision — otherwise we hash a splice and blame the human for our guess) nor discards
   silently: an explicit `--discard-marker` is the only door. The honest alternative that removes guessing entirely — an OS advisory
   lock held on the `.part` — is the same missing piece install design §4 already names, and is deliberately not smuggled in here.
5. **Publish by rename, never by copy.** Download to `.part.<id>.<nonce>` inside the models directory (same filesystem), hash it
   while streaming, then rename — and only into an **absent** target, refusing otherwise. `install`'s `publish_artifact` is
   copy+rename because its source lives elsewhere; reusing "exactly" that path would write 310 MB (or 865) twice on a host with
   8.9 GB free. What is shared is the discipline: temp name in the destination directory, atomic rename, never over existing bytes.
   Whether `fs::rename` replaces or errors when the target exists differs by platform in spirit; R7b pins the behaviour with a test
   on `windows-latest` rather than assuming either answer.
6. **Preflight is an admission check, not a promise.** Refuse before byte one when available bytes < Σ pinned `size_bytes` + margin —
   summed over **both** files (309.9 + 554.8, a factor of 2.8 against guessing one). It cannot prevent failure: free bytes are not
   quota, and ENOSPC can still arrive mid-file. Four named refusals, each its own variant, none of them an accusation it cannot
   support: `FetchInterrupted` (size/`bytes_done` short of total), `FetchDigestMismatch` (size right, digest different — "the published
   file is not the file this build pins: either it was re-uploaded upstream or something altered it in transit; verify with
   `np models hash`, or install a file you trust"), out-of-disk, and transport error. A pinned-digest refusal prints expected, found
   and size — nothing else — and never says "your download is corrupted" when the size matched.
7. **What may not survive the amendment is trust-by-URL.** The pin lives in the binary: `const PINNED: &[PinnedModel]` keyed by model
   id, holding `{id, member name, filename, size_bytes, sha256, read_on}` — what to accept, no host. Both files are pinned (text
   `2188ac1d…`, mmproj `c4a8a526…`, from public LFS metadata 2026-10-29). The **host is config**: `[models] base_url` with a documented
   default and an env override, so a mirror works and the origin is not a compile-time secret. One sentence to keep a future reader
   from "fixing" this: a public artifact host is **not** parent §9's secret class, which protects the self-hosted chat server's URL and
   key. Pin rot is real — nobody notices an upstream re-upload until first start refuses — so `[models] sha256 = "…"` overrides the pin
   with a loud line saying an unpinned digest was accepted; that is no weaker than today, where any human may pass `--sha256`. Silent
   fallback stays impossible.
8. **Refusals name a class, never a URL.** Rule 6's "name what was attempted" is bounded by rule 7: `source_kind`, repo and filename,
   HTTP status class — no host, no full URL, because client error strings carry them and this text lands in CI logs. R7b ships a test
   that no substring of a loopback base URL appears in any refusal (parent §7's `127.0.0.1:0` precedent).
9. **Streaming digests need one home.** `hash.rs` exposes `sha256_file`/`sha256_bytes` only; the fetcher needs an incremental hasher
   because it hashes while counting progress. That goes in `hash.rs` as the program's single SHA-256 definition with two callers — not a
   second one, and not a `sha2` dependency outside np-core (install design §3 refuses exactly that).
10. **N files, N digests — but names first.** Carrying the pair (§5) breaks: `Install { source, sha256 }`, artifact naming `<dir>/<id>`,
    the single `models.sha256` column and its schema fence, `ArtifactDisagrees`' one path, `install --path/--sha256` argv, and
    `LocalModelProvider::new(row, artifact: &Path, dim)` as already shipped. The smallest shape that holds both today's single file and a
    pair: keep one row per model with `models.sha256` = the **primary** artifact's digest (so every existing reader keeps its meaning), add
    migration 8 with a child table `model_files(model, name, sha256, size)` keyed `(model, name)` where `name = 'main'` is the primary;
    API `Install::file(name, path, sha256)`; CLI keeps `--path F --sha256 H` as `main` and adds `--path NAME=F --sha256 NAME=H`, refusing an
    unequal name set rather than zipping by order. **The asymmetry the first draft had backwards:** digests are cheap to retrofit (an
    additive child table), file *names* are not (bytes already on disk under `<dir>/<id>`). So R6b0 reserves the name space and the leading-dot
    rule; migration 8 lands with R7b when there is a second digest to store. If this round seems to want a migration, install design §6 says
    report the design gap — which is what this paragraph is.
11. **Per-workspace cost becomes automatic.** Install design §4 accepted "N copies for N workspaces" as a deliberate deviation because a
    human ran each `install`. First-start fetching makes it automatic: state in R7b that fetch is **per-workspace on demand** (the workspace
    that needs vectors fetches; nothing loops over configured workspaces dialing out), and the preflight runs per workspace.
12. **`bytes_total` may be unknown.** Chunked transfer or a missing `Content-Length` leaves it undefined: the field is optional, and
    `list` prints "size unknown" rather than inventing a denominator.

### What this buys, and what it does not claim

The marker answers the human's actual question — *is it downloading?* — in both cases that matter: no row yet (the listing-level section
carries it), and a row being re-fetched (row state plus a `fetching` line). And the pin table means a first start on a machine that has never
seen this model still refuses anything that does not hash to what the build expects. What it deliberately does not claim: that image or audio
embedding works. §5's open spike (R7c) owns that, and until it lands the encoders are carried because the human asked for the whole model —
with this section saying so out loud rather than letting 865 MB imply a capability nobody has demonstrated.


---

## 13. The runtime reality check (2026-10-29), which fails §9's third gate

Before briefing R6b0 I ran the pre-check that gate asked for — **grep the vendored source, not upstream HEAD**. It failed, and the way it
failed changes what "local embeddings" costs. All of this is measured on this host at 2026-10-29; nothing below is recalled from a changelog.

**What the pinned binding actually contains.** `llama-cpp-sys-2 0.1.160` vendors llama.cpp (40 MB of source, `LLAMA_VERSION` 0.6.x) and is
the **newest published version** (`index.crates.io/ll/am/llama-cpp-sys-2`, last entry). Grepping its vendored tree for the architecture this
model declares:

```
$ grep -rhoE "LLM_ARCH_GEMMA[A-Z0-9_]*" llama.cpp | sort -u
LLM_ARCH_GEMMA  LLM_ARCH_GEMMA2  LLM_ARCH_GEMMA3  LLM_ARCH_GEMMA3N  LLM_ARCH_GEMMA4
LLM_ARCH_GEMMA4_ASSISTANT  LLM_ARCH_GEMMA_EMBEDDING
```

`gemma-embedding2` is **absent**. Upstream at `0ac1a7f` has it (verified earlier by clone), so the gap is between upstream and the crate's
vendored revision — which means the pinned binding cannot resolve `general.architecture = gemma-embedding2` no matter what our code does. mtmd
*is* present (`mtmd_encode_chunk` ×2 in `tools/mtmd/mtmd.h`), so R7c is unaffected by this particular gap.

**There is no override knob.** `build.rs` imports `cmake::Config` and always builds its vendored copy; the env vars it reads are
`BUILD_DEBUG`, `LLAMA_LIB_PROFILE`, `LLAMA_BUILD_SHARED_LIBS`, `CMAKE_*`, backend SDK paths — nothing that points at a different llama.cpp
source tree. `dynamic-link` links against libraries you supply, which means building upstream yourself (still cmake) and owning that build for
three OSes.

**And this host cannot build C++ right now:** `cmake` is **missing**, there is no root (`sudo -n` → "interactive authentication is required"),
and the apt candidate can't be installed. The official Kitware tarball *is* reachable (HTTP 206 on a ranged GET), so provisioning is possible
without root — it just isn't free, and it must be written down rather than discovered mid-round. Resources measured now: **6.3 GB RAM free,
6.8 GB disk free on `/home`, 16 cores** — enough for one llama.cpp build plus a 310 MB artifact, not enough to be careless (the earlier "2 GB
free" note in the session record is stale; `free -m` says otherwise today).

**The consequence for width and window.** A model fact cannot be pinned globally while the model is undecided: §5's 8,192 is EmbeddingGemma-2's
window, and it now sits in a constant. Whatever shape is chosen below, the follow-up named in §5 — move the check beside the provider — stops
being a nicety and becomes part of the change.

### The three shapes, with their real prices

| | A. Prototype on EmbeddingGemma-**1** | B. Keep EmbeddingGemma-**2**, build llama.cpp ourselves | C. Wait for a binding release |
| --- | --- | --- | --- |
| What it is | `ggml-org/embeddinggemma-300M-GGUF` declares `gemma-embedding.*`, which the pinned binding **does** implement (verified in the vendored enum above; repo ungated, 377k downloads) | Pin a llama.cpp git revision that has `gemma-embedding2`, build it with cmake, link through `dynamic-link`; or vendor a patched `-sys` crate | Do nothing until `llama-cpp-sys-2` ships the newer vendored revision |
| Reaches a prototype | Fastest: no toolchain work, one dependency, one model file | Slowest by weeks: cmake provisioning locally (tarball, no root), plus a reproducible C++ build in CI on Linux/Windows/macOS | Unknown; R6b stalls at "cannot load the model" |
| Keeps your amendments | Text-only — **no vision, no audio**, so amendment (a) is deferred rather than met | Yes: whole model, pair of files, R7c spike stays live | Yes |
| Vector identity | New `@n`, and stored vectors are **not comparable** with EmbeddingGemma-2's; §5's narrowing migration must never mix them | as designed | as designed |
| Honest cost statement | We ship a retrieval prototype whose quality we can measure on your corpus today, and say plainly it is the 300M text model | We spend the next several rounds on build plumbing before one vector exists | We have a plan and no prototype |

**My recommendation for a prototype-first direction is A, with B kept as the upgrade path rather than abandoned**: the seam already takes width
and prefix from the installed row, so switching models later is a new pinned id plus a re-embed — the thing §5 already designed as a migration.
A gets `np search` in front of you while there is still a decision to make about quality; B buys the encoders nobody has yet demonstrated that we
can query. This is your call, not mine: it changes which model answers every question the app will ever ask your notes.
