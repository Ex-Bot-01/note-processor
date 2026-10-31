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
pass forever.

**Status, 2026-10-30.** R6b0 shipped steps 1–4 as the `feature-local-model` job — `ci.yml:84`, `:86`, `:95`, `:117` — on
**`ubuntu-latest` only**. The Windows row this paragraph promised ("one CI step per OS row") is **still owed**, and
deliberately so: `build.rs` refuses to build the feature off Linux (§14), so a Windows step today would record a written refusal
rather than a compile. What used to be true here — "`.github/workflows/ci.yml` has no `--features local-model` row at all" — is
history now, kept because §9's round table points at it (Scope B review P1-1 caught this paragraph still asserting it in the
present tense, four lines above the job that disproves it).

1. `cargo check --locked -p np-core --features local-model` — compile-only, **ubuntu today** (the Windows row is the owed half
   named above);
2. `cargo clippy --locked -p np-core --features local-model --all-targets -- -D warnings`;
3. `RUSTDOCFLAGS="--deny warnings" cargo doc --locked -p np-core --features local-model --no-deps`;
4. `cargo test --locked -p np-core --features local-model` where anything needing a real artifact prints a named
   `NOT COVERAGE: <test> — no artifact` line and returns (the F18 precedent), and the job greps that line into its log so
   "compiled, never ran" is written on the record instead of implied. **The first version of that step did not achieve it**
   (Scope B review P1-3): libtest captures a passing test's stdout and discards it, so measured under the exact CI command with no
   artifact the count was **0**, and 12 only with `-- --nocapture`. The step now passes `-- --nocapture` and prints the count.

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
  "nothing here reads a trained window"; it does now. Consequences then: `embeddings.max_tokens`'s default of 2048 sits comfortably
  inside it, and `MAX_EMBEDDING_TOKENS = 32_768` **exceeds what the model can attend to** — recorded as a bound to tighten to
  8,192 in R6b2 alongside the enforcement test, not silently left generous. (Both numbers have moved since, and neither moved the way this
  sentence predicted: decision B below sets the ceiling from the context we build rather than from the card.) (`max_position_embeddings = 262144` in
  `config.json` is a RoPE/train-length property, not a usable embedding window; §5's refusal posture does not depend on which
  of the two numbers you read.)
  **Annotated by R6b1 (2026-10-30), because this sentence is stale and I would rather mark it than let the next round re-derive
  it:** the tightening already landed — `crates/np-core/src/config.rs` declares `MAX_EMBEDDING_TOKENS: usize = 8_192`, which is
  *(annotated again the same day by decision B: that line now reads `4_096`, and the number stopped being a card fact at all — it is
  the width of the context we build, paired with `REQUESTED_CTX` by a compile-time assert. Read the sentence above as R6b1 wrote it,
  not as the current state: Scope B review P1-2 found this exact line convincing a merge reviewer that decision B had not landed.)*
  §9's pre-round row, not R6b2's. What R6b2 still owes is the enforcement test over prefix + text + specials, not the constant.
  The R6b1 brief repeated this document's 32 768-era framing as "conflicts with `MAX_EMBEDDING_TOKENS = 8_192`"; the code half of
  that was already true and the prose half was not.
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

**Token budget.** `max_position_embeddings = 262144` is not a licence to embed a 200k-token file. (That framing is draft-v2
reasoning: the parenthetical here used to say "nothing here reads a trained window", and it does now — `ModelFacts::n_ctx_train`,
measured in the table below. Scope B review P2-4 flagged the old and new claims sitting four lines apart.) The posture stays
refuse-loudly:

- `embeddings.max_tokens` (**default 1,024** since §5's decision B; 2,048 when R6a added it, together with its default-when-absent; note `EmbeddingsConfig`
  is `#[serde(deny_unknown_fields)]`, so before R6a writing that key was refused naming key and file, and R6a is the round that
  changed it — past tense now, because "today" outlived its round: Scope B review P2-3) applies to **what the model actually sees**: `prefix_doc` + chunk text + special tokens, since
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

### The artifact's own header, measured (R6b1, 2026-10-30)

Until R6b1 every number above was a citation from a model card or a `config.json`. `crates/np-core/src/local_model.rs` now reads
the shipped GGUF through the pinned runtime's own accessors, and both readings — parsing the file's KV section directly, and
loading it through llama.cpp v0.8.0 — agree. Artifact: `/home/agent/models/embeddinggemma-2-Q8_0.gguf`, **309,855,456 bytes**,
`sha256 = 2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135`, GGUF v3, 413 tensors, `general.file_type = 7` (Q8_0),
46 KV pairs.

| Header key | Measured value | What it decides here |
| --- | --- | --- |
| `general.architecture` | `gemma-embedding2` | the reason v0.8.0 is the pin; the tree really does resolve it — the load succeeds |
| `gemma-embedding2.embedding_length` | 512 | internal width, and what `llama_model_n_embd_inp` answers. **Not** the stored width |
| `gemma-embedding2.embedding_length_out` | **768** | what `llama_model_n_embd_out` answers, and therefore what a declared width is compared against (§6's refusal) |
| `gemma-embedding2.context_length` | **262,144** | `llama_model_n_ctx_train`. Was an open question; closed by decision B below — it is *not* our ceiling |
| `gemma-embedding2.pooling_type` | 1 (MEAN) | the context resolves `UNSPECIFIED` to this, so we read it and never override it; a context answering `NONE` is refused as "cannot produce a sentence vector" |
| `gemma-embedding2.attention.causal` | `false` (BOOL) | bidirectional, as an embedding model needs |
| `tokenizer.ggml.model` | `gemma4` | v0.8.0 knows the pre-tokenizer type; R6b2 gets tokens out of this vocabulary |
| `tokenizer.ggml.add_bos_token` / `add_eos_token` | **true / true** | R6b2's token count must budget **two** specials, not one — the case §5's "prefix + chunk + special tokens" line keeps threatening to get wrong |
| `tokenizer.ggml.tokens` | 262,144 entries | `llama_vocab_n_tokens`; valid ids are `0..262_144` |
| `gemma-embedding2.block_count` | 24 | sanity only (`n_layer`) |
| `general.size_label` | `271M` | the "300M" of older prose is the family, not this file (§13's price list) |

**Was an open question; closed below.** The artifact declares a trained window of **262,144 tokens** while the ceiling we refuse at
was **8,192** (taken from the model card's attention window). Three readings were possible: the card is right and `context_length` is a
RoPE train-length nobody can spend on one embedding; or the window really is usable and 8,192 leaves recall on the table for long notes;
or the number is aspirational and llama.cpp refuses or degrades long before it. R6b1 could not distinguish them — it loads the file and
never submits a token — so `ModelFacts::n_ctx_train` reports the artifact's number next to (not instead of) our own bound, which is still
true and still worth doing: a refusal that names the wrong limit wastes the reader's time.

### R6b2 measured what a token costs (2026-10-30) — the ceiling is now priced, not cited

R6b2 submits tokens, so the question above finally has evidence attached. On this host, `GGML_OPENMP = OFF`, one loaded model:

| Input | Tokens | Decode |
| --- | --- | --- |
| ~500-token note | 543 | **3.85 s** (141 tok/s) |
| ~2,000-token note | 2,163 | **18.0 s** (120 tok/s) |
| over the context window | > 8,192 (4,096 until decision C below) | refused before decoding — see below |

Three things follow, and they are separate decisions:

1. **The runtime's usable window is the window we ask for, not 262,144** — 4,096 when this was written, **8,192 since decision C**.
   `load` sets `n_ctx = n_batch = n_ubatch = REQUESTED_CTX`, because this
   artifact is an encoder — `create_memory` returns null for `LLM_ARCH_GEMMA_EMBEDDING2` (`src/llama-model.cpp:2825`; measured through our
   FFI, `llama_get_memory(ctx)` is `(nil)`) — so `llama_decode` diverts to the encoder path (`src/llama-context.cpp:1845-1847`) and *that*
   path asserts `n_ubatch >= n_tokens` at `src/llama-context.cpp:1631`. `GGML_ASSERT` is `GGML_ABORT` even under `NDEBUG`, so with llama.cpp's
   default 512 a 600-token sequence exits 134 rather than returning anything. (An earlier draft of this subsection blamed the decoder's
   asserts at `:1918`/`:1920`, which never run here — R6b2 review P1-1.) So today the honest ceiling is the window, and `embed_text` refuses
   between the two numbers — `TokenBudgetExceeded` over `embeddings.max_tokens`, "the sequence is longer than this context can hold"
   above the window. **Since decision B that band is empty for a validated config** — both numbers are 4,096 as of B and 8,192 as of C,
   and the compile-time assertion is what keeps them equal through a retune. The window refusal
   still earns its place twice over: an artifact trained narrower than `REQUESTED_CTX` makes `window()` the smaller bound, and a direct
   `embed_text` caller may pass any budget it likes — which is how `tests/local_embed.rs` reaches this arm. R6c should not design a report
   state for a config-reachable case (Scope B review P2-6). Raising `MAX_EMBEDDING_TOKENS` toward 262,144 means raising `REQUESTED_CTX` in
   the same change, and §14's curve says what that costs: **+458 MiB** of peak RSS at 4,085 tokens, **+1,058 MiB at 7,999** (measured
   2026-10-30 with `nctx=8192`), growing from there.
2. **Cost rises with length, and it is slow enough to matter.** Measured points: **543 tokens in 3.85 s (141 tok/s)** and **2,163 tokens in
   18.0 s (120 tok/s)**. Everything beyond them is **extrapolation on a falling rate**, not measurement — including the headline that
   `max_tokens = 8,192` would cost roughly a minute per chunk (~63 s at 130 tok/s, more if the decline continues). **That extrapolation has
   since been measured**: decision C's re-pin ran 7,999 tokens through an 8,192-wide context in **101 s (≈79 tok/s)** at peak RSS 1,478 MiB —
   the decline was steeper than interpolated, so quote the measurement and not the estimate. The two original points also price the
   default: 2,048 interpolates to a ~16-second worst case between them (it is not itself a measured length).
3. **Throughput, not the window, is what a corpus feels.** 5,000 notes × 500 tokens ≈ 2.5 M tokens ≈ **5.3 hours** at this rate,
   single-process. That is R6d's problem (OpenMP A/B, batching across texts in one decode — `embed_text` is deliberately one decode per
   text today), and it is the number worth optimising, because no ceiling choice changes it by more than a factor of a few.

### Decision C, later on 2026-10-30 — back to 8,192, with the window moved up to meet it

The human asked for the 8,192 ceiling back after the whole-branch review landed. Honouring that *coherently* meant moving two constants,
not one: a ceiling of 8,192 over a 4,096-wide context is exactly the config-that-lies decision B existed to remove (a `max_tokens = 6000`
would parse, validate, and then be refused by the window sentence). So `REQUESTED_CTX` went to 8,192 beside it, and the compile-time
assertion that pairs them stayed exactly as written — it is what makes "raise one" impossible without "raise the other".

| Key | Decision B | Decision C | Why |
| --- | --- | --- | --- |
| `MAX_EMBEDDING_TOKENS` | 4,096 | **8,192** | the human's call, and now a number the runtime can actually spend |
| `local_model::REQUESTED_CTX` | 4,096 | **8,192** | moves with the ceiling; measured before asking for it (below) |
| `DEFAULT_EMBEDDING_TOKENS` | 1,024 | **1,024** — unchanged | the ordinary chunk is still ~500 tokens (`MAX_CHUNK_BYTES = 1800`); raising the default would pay the tail cost on every note |

What was measured first, on this host, with `crates/np-core/tests/probes/decode_probe.c nctx=8192` (the argument and the built-width readback
printed in its output were added for this decision):

* **an 8,192-wide context costs nothing at rest**: a 133-token sequence peaks at 429,248 kB against a 429,528 kB model-open baseline — +0 MiB.
  §14's rule held: the compute buffer is touched per forward pass, not reserved per window;
* **a near-ceiling chunk is expensive**: 7,999 tokens, rc 0, `‖v‖` finite, peak **1,513,380 kB (1,478 MiB)**, **+1,058 MiB** over baseline
  (R6d re-measured this through the Rust API rather than a C probe: 7,992 tokens at **1,525,500 KiB**, and exactly 8,192 at
  **1,558,172 KiB** — same shape, +3 % for the wider sequence, which is what makes the number trustworthy across both paths),
  **101 s** wall. On an 8 GB machine that is the difference between indexing and swapping *while* a long chunk runs, which is why the default
  stayed at 1,024;
* **the built context matches the request**: `llama_n_ctx` = 8192 and `llama_n_ubatch` = 8192, the A1 review P1-1 readback — so the refusal
  added in that round stays silent here and fires only if a future build shrinks under us.

### Decision B, taken 2026-10-30 — the ceiling follows the build, not the card *(superseded the same day by decision C above; kept because the reasoning about drift is what C preserved)*

The human chose **option B** from the three priced above, with those numbers in front of them and "we can change it later if it is not
sufficient" attached. Concretely:

| Key | Before | After | Why this value |
| --- | --- | --- | --- |
| `MAX_EMBEDDING_TOKENS` (config ceiling) | 8,192 — the model card's attention window | **4,096** | equal to the context this build creates (`local_model::REQUESTED_CTX`). A ceiling above the window is a config that parses happily and is then refused at run time by a different sentence, so it is not generous but misleading. Length also costs RAM super-linearly here (**+458 MiB** over the C baseline at 4,085 tokens) on the class of machine this tool targets |
| `DEFAULT_EMBEDDING_TOKENS` | 2,048 — justified by chunk-byte arithmetic | **1,024** | ≈**8 s** of CPU per worst-case chunk at ~120–141 tok/s (interpolated between the two measured points; say so when quoting it), and a real corpus sits nearer the fast end. **Superseded for this shape on 2026-10-31 by a measurement of the shape itself** (`crates/np-core/tests/local_window.rs`): a `MAX_CHUNK_BYTES` chunk counts **403 tokens** and decodes in **2.61 s** — **155 tok/s**, about three times the interpolated figure, because that line was drawn between probe points at far longer sequences where attention cost dominates It still leaves room for `prefix_doc` + two specials over a 1,800-byte chunk, so the old justification becomes slack rather than headroom |

**Why this is revisitable rather than provisional.** Both are config-level numbers with no stored data behind them yet: raising the default is
one key per workspace, and raising the ceiling is a constant plus a re-measurement. What must not happen is drift between the ceiling and the
window, so `local_model` carries a compile-time assertion that `MAX_EMBEDDING_TOKENS <= REQUESTED_CTX` — outside `#[cfg(test)]`, like the FFI
layout pins, because a test-only check lets two constants in two files drift apart in a build that still compiles. Raising the ceiling means
raising `REQUESTED_CTX` in the same change and re-reading §14's RSS curve; that friction is the point.

**What it does not settle.** Whether 262,144-token embeddings would improve retrieval on long notes stays unmeasured, and the honest statement
is that we refuse above the window (8,192 today) because that is what one decode can hold here — not that the model cannot represent more. The
follow-up named
above also survives: this is a **global** constant doing a **per-model** job, and a second configured model (the spec's MiniLM, ~512 tokens)
needs the check moved beside the provider. And recall on long notes is bounded by *chunking*, not by this number: a 3,000-token note becomes
several embeddings whatever the ceiling says, so if long-note retrieval is the worry, the lever is chunk overlap and ranking, not `max_tokens`.

## 6. `np embed <workspace>`: what it writes, and what it must never do

A new command, deliberately **not** part of `np scan`: the scan wraps its whole write pass in one transaction
(`unchecked_transaction()` … `tx.commit()`), so unbounded model work inside it would leave a concurrent `np` waiting out
`busy_timeout` — which is what the parent design §5 forbids.

> **Shipped as (R6c1, 2026-10-31, `crates/np-core/src/embed_run.rs`).** Three things differ from the plan text above, and
> each is a decision rather than an oversight:
>
> * arm (iii) says **`np models install …`, not `np models fetch`**. §14's division of labour stands — fetching is R7b's
>   job and still does not exist — so naming the fetcher would send a reader to a command this build has never had. The
>   sentence names the one path that works (`models install` with its real flags) and says plainly that fetching is not
>   implemented here. When R7b lands, it renames nothing; it adds the missing option.
> * arm (iii) is evaluated **after** the currency read, so a workspace with nothing owed exits 0 whether or not the
>   artifact exists. That is exit state (i) as written above ("nothing owed → success") taking precedence over a complaint
>   about a file no work was waiting on; `np scan`'s warnings are unaffected either way.
> * **the token budget is enforced in §4's runtime, not here.** Nothing in `embed_run` counts tokens: pricing a sequence
>   needs the model's tokenizer, which lives behind `local-model`. The writer relays
>   [`EmbedError::TokenBudgetExceeded`](../crates/np-core/src/embed.rs), retries an over-budget batch member by member so
>   its batchmates still land, and counts what it refused. §5's "reported, never truncated" is unchanged; the check moved.
>
> **Amended 2026-10-31 (R6c2, A′) — who dresses a chunk.** The bullet above about the stamp covering "the exact bytes the
> provider receives" was written while `embed_run` applied `prefix_doc` itself. Wiring the real runtime made that wrong:
> [`LocalEmbedder`] applies the row's prefix too, so documents reached llama.cpp with the instruction **twice** and the
> stamp covered one copy (R6c2's P1). The seam now asks the provider: [`Embedder::model_inputs`] is a required trait
> method, `embed_run` stamps what it returns and hands [`Embedder::embed`] the raw chunk text. Consequences worth
> recording: a provider that changes its dressing invalidates its own stored vectors by construction rather than by
> convention; `prefix_query` stays out of it, because a query vector is not comparable with a stored one (§3) and is
> reachable only through `embed_as(TextKind::Query)`; and the two provider shapes in `local_model` declare their inputs
> through one private function, asserted equal against a real artifact so the rule cannot be spelled twice again.

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
with no artifact and no network; real-model work is gated on a runtime env var with a clean early return (**`NP_TEST_MODEL_PATH`**,
the name the tests and `ci.yml:104` actually read — this section said `NP_MODELS_DIR`, which nothing reads, so a human exporting the
named variable would have bought a green run and no coverage: Scope B review P2-10),
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
   taxonomy, dim-mismatch refusal, `[embeddings] max_tokens` (default 2048 then — decision B moved it to 1,024) and config validation (`provider =
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
| **R6b1** *(shipped `ac25c00`, fixes `4fc0aa9`)* | the pinned GGUF opens through our FFI and answers with its own header's numbers | width refusal, drop order, truncated-artifact refusal all tested |
| **R6b2** *(shipped `344692f`+`d1e8fad`+`2e15d33`, review fixes in `HEAD`)* | tokenize → forward pass → pool → L2-normalise; budget enforcement over the **token** count; the window made wide enough that the runtime returns a code instead of aborting | 12 artifact-gated cases, plus the three numbers §14 was missing: size +5.5 MiB, throughput ~130 tok/s, forward-pass RSS curve. Its review found two P0s — a dead buffer-resize path that decoded zeros on a short buffer, and RANK pooling read at the wrong width — both fixed; the window prose now cites the encoder assert that actually fires |
| **R6c** | `np embed <ws>` and §6's four labelled exit arms, reading a marker **read-only** ("artifact absent — run `np models fetch`") | the label exists before the writer does, so R6c never waits on the fetcher |
| **R6d** | benchmark, including whether an mmap-first load reads the whole artifact (which decides §12's digest cost) | **done 2026-10-31 across two rounds**: `local_window.rs` measures the default-budget chunk (403 tokens, 2.61 s, 155 tok/s), a near-ceiling one (7,992 tokens, 98.6 s, 81 tok/s, peak RSS 1,525,500 KiB) and the exact window edge (8,192 accepted in 100.1 s; 8,193 refused before any decode), plus the per-run digest price (309,855,456 bytes in **11.32 s**, 26 MiB/s — D12's cost, now measured rather than assumed). The second round built the pinned tree twice and ran eight A/B decodes: **`GGML_OPENMP=ON` buys nothing** (≤ 0.6 %, both builds default to `n_threads=4`), and **an mmap-first load does read every byte** — all 309,858,304 of them, for ≈ 0.1 s. §14's A/B subsection holds the table; what is left of R6d is a *corpus* run (thousands of decodes in one process) and any batching decision, which now waits on D13's thread-count measurement rather than on OpenMP |
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
(§5, R7c), what an mmap-first load actually reads from disk (R6d, decides §12's digest cost), and whether the token ceiling — 8,192 since
decision C, paired with `REQUESTED_CTX` — belongs beside the provider rather than in a global constant (§5's own note on the MiniLM fallback).

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

**The consequence for width and window.** A model fact cannot be pinned globally while the model is undecided. §5's ceiling used to be that kind
of pin (8,192, EmbeddingGemma-2's card) and since decision B it is a *build* fact instead — 8,192 today, the width of the context we create (C moved both together), checked
against `REQUESTED_CTX` at compile time. That removes one wrong authority without removing the real problem: whatever shape is chosen below, the
follow-up named in §5 — move the check beside the provider — stops being a nicety and becomes part of the change.

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


---

## 14. We own the llama.cpp build (decided 2026-10-29, measured the same day)

You answered the §13 question: **use llama.cpp in the back and keep EmbeddingGemma-2**, accepting a bigger package "as long as it is not
getting too big". That closes shape B — and the measurements below are why B turns out to be cheaper than §13 feared, and also why it is
**not** the pinned Rust binding.

### Identity of the vendored tree (A2 review P1-1, closed 2026-10-30)

A2's conditional BLOCK was: *the `.vendor` tree on disk cannot be shown to be the tarball the pin names* — its in-tree
`.vendor-stamp` predated `header_sha256`, `.vendor/.stamps/` did not exist although this script's comment said
provenance lived there, and the tree carries options (LLAMA_BUILD_UI / LLAMA_BUILD_APP at
`.vendor/llama.cpp-v0.8.0/CMakeLists.txt:136-139`) that a reviewer's memory of upstream v0.8.0 did not expect.
What is measured here, on 2026-10-30, and what each fact reaches:

| Check | Result | What it can and cannot certify |
| --- | --- | --- |
| `sha256sum .vendor/llama.cpp-v0.8.0/include/llama.h` | `c0854771d3760a41…` — equal to `third_party/llama-cpp.toml`'s `header_sha256` | Certifies the one file `src/llama_ffi.rs` mirrors. Says nothing about the other 20 MB of the tree |
| Every pinned option is consumed by this tree | Added `LLAMA_BUILD_BOGUS_OPTION = "ON"` to the pin → build refused with *"cmake accepted these options … but filed them as UNINITIALIZED"* (exit 101); pin restored, `cmp` clean, rebuild green | This is the check that answers "options that upstream v0.8.0 does not have": **if** `LLAMA_BUILD_UI` were unknown to this tree, it would trip the same refusal, and every feature-on build would fail. It does not, so those options exist in the tree we build. Cannot say whether that tree matches some other checkout of the tag |
| `./scripts/vendor-llama-cpp.sh` on the existing tree | prints *"already vendored … include/llama.h hashes to the pinned header_sha256; nothing fetched"*, exit 0, and now writes `.vendor/.stamps/v0.8.0.header-match` (contents: the digest, a timestamp, `note=no tarball was fetched in this run`) | The accept path leaves a record outside the tree that says exactly what was checked — A2 P2-9's missing `.stamps/` directory. Rewrites only when absent or holding a different digest, so a no-op run stays silent |
| The stale in-tree stamp | `.vendor/llama.cpp-v0.8.0/.vendor-stamp` deleted (nothing read it: `grep -rn vendor-stamp` over tracked files hits only the script's own historical comment) | Removes an artifact that looked like provenance and certified nothing |

What remains **unprovable on this host**: re-downloading the tarball and re-hashing it would need network egress, so the
tarball digest `51b1e025…` is asserted by the pin and by the vendor-time check that ran when this tree was unpacked —
not re-verified today. The honest summary is that build.rs's per-build header digest plus the option-understanding
refusal identify what we compile against; "this byte-for-byte is GitHub's v0.8.0" is a claim about the machine that ran
the vendor script, and CI on a fresh runner re-runs that path from scratch.

### Measured on this host, no root

All figures re-measured on 2026-10-30 on this host and reproducible: the Rust side is
`cargo test -p np-core --features local-model --test local_load -- --nocapture [--test-threads=1]`, the C side is
`crates/np-core/tests/probes/model_load_probe.c` (committed precisely so the "Rust costs nothing over C" sentence below
has a C number to be compared against; build line in that file's header). **Memory is MiB** — `VmHWM`/`VmRSS` from
`/proc/self/status`, which are KiB, divided by 1024.

| Fact | Value | How |
| --- | --- | --- |
| Load, warm page cache | **0.35 s** first call, 0.30 s on repeats | C probe and the Rust test agree to ~0.05 s; `llama_model_load_from_file` timed around one call |
| Load, artifact evicted from our page-cache view | **0.44 s** | the probe's `--drop-cache`: `posix_fadvise(POSIX_FADV_DONTNEED)` on the file first — no root needed to evict one file's pages, so this is "cold from our side"; the block layer may still hold them, and the honest reading is that the disk read costs ≈ 0.1 s |
| Peak RSS, bare C loader probe (warm) | **419.5 MiB** peak; **383.6 MiB** resident with the model open (5.0 MiB before) | `model_load_probe`, no Rust runtime in the process. This row read "5.0 MiB → 419.4 MiB with the model open" until 2026-10-30, which was a **peak** wearing a resident label: the probe printed `vmhwm_kb()` for both of its right-hand columns, so the two were always equal whatever happened (A2 review P1-3). Fixed to print `VmRSS` for "open" and `VmHWM` for "peak", and re-run against the artifact: resident 392,776 kB, peak 429,552 kB — a 36 MiB gap that had been invisible. The comparisons below are peak-against-peak and still stand; "resident with the model open" is the number to quote when asking what an idling `np embed` holds |
| Peak RSS, our test process, loads serialized | **421.1 MiB**; one load moves resident memory 127.8 → 407.4 MiB (+**279.6 MiB**) | `--test-threads=1`, printed per round with `--nocapture` |
| Rust over C, same artifact | **+1.7 MiB** | 421.1 − 419.4. Not "nothing": earlier prose here said the Rust side cost nothing, which was a rounding of an unattributed pair. It is ~0.4% of the load, and it is now a number rather than an adjective |
| The same suite on libtest's default threads | **523.8 MiB** peak — *not attributable to one load* | concurrent artifact cases share one process-wide `VmHWM`. Loads are serialized by a mutex in the test so the headline figure means something; an unattributed peak is a worse number than a serialized one |
| Three load-and-drop cycles, serialized | open-state RSS 407.5 MiB on all three rounds (±28 KiB), peak moves +164 KiB total, no crash | `loading_and_dropping_three_times_costs_the_same_each_time`. This detects double-free/UAF by crashing; it does **not** detect leaks (see the test's own doc) |
| Widening the window to 8,192 costs nothing at rest | **+0 MiB**: a 133-token sequence through an 8,192-wide context peaks at 429,248 kB against the 429,528 kB model-open baseline (2026-10-30) | `decode_probe nctx=8192`, which also prints `built n_ctx=8192 built n_ubatch=8192` — the readback A1 review P1-1 asked the Rust side to check. This row is why decision C could raise the window without raising the idle footprint |
| **A near-ceiling chunk, measured (decision C's price)** | 7,999 tokens: rc 0, finite pooled row, peak **1,478 MiB** (**+1,058 MiB** over baseline), **101 s** ≈ **79 tok/s** | same probe at `nctx=8192`. The falling rate continues (141 → 120 → 79 tok/s), so §5's interpolated "~63 s for an 8,192-token chunk" was optimistic by ~60% and the measurement supersedes it |
| Context on top of the model, nothing decoded | **+1.8 MiB** resident at `n_ctx = 4096` | same probe. The weights dominate, not the context — and this artifact has no KV cache to dominate anything (`create_memory` returns null), so what that 1.8 MiB buys is a compute buffer and a window number |
| **Corrected in R6b2: that +1.8 MiB was the whole story only because nothing had decoded.** Peak RSS above the C probe's own 419.4 MiB with the model open, by longest sequence run (`n_ctx = n_batch = n_ubatch = 4096`) | 133 tok **+0 MiB** · 513 **+47** · 1,026 **+109** · 2,052 **+218** · 4,085 **+458** | `crates/np-core/tests/probes/decode_probe.c` (committed, build line in its header — it also prints `memory=(nil)` and `n_threads=4`, the two facts behind the encoder-path and CPU-utilisation claims above); a compute buffer is *reserved* at context creation and *touched* per forward pass, so cost tracks the longest sequence actually run, not the ceiling asked for. This is why widening the window to escape the `n_ubatch` abort bought memory only for long notes, and it is the number R6d's peak-RSS headline should be read against. The 8,192 row above extends this curve: the shape holds at the wider window (+0 MiB at rest, ~+1 GiB near the ceiling) |
| **Package size, stripped** (`cargo build --release --bin np`, `strip`) | feature **off 5.5 MiB** (5,780,432 B) · feature **on 11.0 MiB** (11,554,872 B) · **+5.5 MiB**, against this section's ≤ **40 MB** ceiling | measured 2026-10-30 with `np-cli`'s `local-model` passthrough. **Why the C++ is retained is not that `np` calls it** — nothing in `np-cli` reaches `local_model`; retention follows from np-core's own public code referencing those symbols while `build.rs` emits a plain `-l static=llama` with no `--whole-archive`, so the linker pulls what the references name. That makes +5.5 MiB today's figure rather than a floor: an LTO or `codegen-units` change could reclaim part of it, and R6d re-measures on the release profile before this section's headroom is quoted again. Symbol counts come from the *unstripped* files (`nm --defined-only`: 252 `T llama_` and 1,276 `llama_`/`ggml_` total with the feature on, zero without) — a stripped binary has no symbol table, so counting on it says "0" either way and reads as "nothing was linked in" while 5.7 MB of C++ sits in the file |
| Throughput, `GGML_OPENMP = OFF`, this host | **141 tok/s** at 543 tokens, **120 tok/s** at 2,163 tokens (3.85 s and 18.0 s per decode; first decode of a run is no slower than a warm one) | `tests/local_embed.rs`'s throughput case prints it on every artifact run — the only one of R6b2's three numbers a test prints. `/usr/bin/time -v` on a 2,052-token pass reports **392% CPU**: with OpenMP off, ggml runs its own pthread pool (`ggml/src/ggml-cpu/ggml-cpu.c` — `ggml_threadpool_new_impl` at `:3324`, created per graph plan at `:3418`, with non-OpenMP worker state under `#ifndef GGML_USE_OPENMP` at `:509` and its barrier at `:2756`) and `llama_n_threads_batch` answers **4**. So "OpenMP off" is not "one core" — and R6d's A/B (below) found that turning OpenMP *on* changes nothing either, at the same four threads
| Independent cross-check of the FFI mirror | C prints `n_embd_out 768, n_embd_inp 512, n_ctx_train 262144, vocab 262144, pooling 1`; Rust's `ModelFacts` prints the same five numbers | two programs, one artifact: a wrong struct field in our mirror shows up as a disagreement rather than as a green suite |


Two readings worth writing down. First, **~420 MiB to open a model is a real prototype constraint** — it is not the 40 MB package ceiling
this section argued about, and on an 8 GB laptop with a browser open it is the difference between "indexes in the background" and "swap
begins". R6d's benchmark should treat peak RSS as a headline number beside tokens/sec, and §3's "one load per process, never per note"
went from a tidiness rule to the thing keeping the host responsive. Second, resident memory moves *with* the open model rather than with
the number of loads: it falls back to 104 MB after both frees — most of what the load touched is handed back, though not down to the ~5 MB
the probe started with — and the second and third loads peak where the first did. So a process that loads once pays once, and one that
loads repeatedly neither grows nor leaks, which is the behaviour §3's "one load per process" rule was written assuming.

Not claimed: any of this on Windows or macOS (§14's per-OS rule — the feature does not build there). The decode figures above are R6b2's,
measured through our own FFI on this host; what is still unmeasured is a *corpus* — thousands of decodes in one process, where the
per-sequence curve above meets §3's "one load per process" and becomes the number that decides whether `np embed` runs in `npd` or in a
foreground command.

### The OpenMP A/B, and what an mmap-first load reads (R6d, 2026-10-31)

The row above ended with "R6d's A/B settles whether the self-containment trade is worth its price". It does, and the price is zero.
The same pinned tree (`third_party/llama-cpp.toml`, tarball `51b1e025…`) was configured twice with the production option set and one
difference — `-DGGML_OPENMP=OFF` (what we ship) and `-DGGML_OPENMP=ON` — both built in 36 s at `-j8`, then eight runs, alternating A/B
on an otherwise idle host, through `crates/np-core/tests/probes/openmp_ab_probe.c` (committed; it prints which configuration ran by
looking for libgomp in `/proc/self/maps`, because `llama_version()` answers `0.8.0-dev` for both).

| Tokens per decode | `GGML_OPENMP=OFF` | `GGML_OPENMP=ON` | difference | peak RSS |
| --- | --- | --- | --- | --- |
| 403 (a production chunk), warm page cache, ×8 | **157.7 tok/s** (20.44 s) | 157.4 tok/s (20.48 s) | −0.2 % | 463,788 kB vs 464,252 kB |
| 403, artifact evicted from our page-cache view, ×8 | 156.7 tok/s (20.57 s) | 157.6 tok/s (20.46 s) | +0.6 % | 463,632 kB vs 464,000 kB |
| 7,992 (near the ceiling), cold, ×1 | 81.8 tok/s (97.68 s) | 81.9 tok/s (97.59 s) | +0.1 % | 1,500,720 kB vs 1,501,116 kB |
| 7,992, warm, ×1 | 82.0 tok/s (97.50 s) | 82.0 tok/s (97.43 s) | 0.0 % | 1,500,800 kB vs 1,500,940 kB |

Every difference is inside run-to-run noise, and peak RSS differs by under 0.5 MiB in a direction that flips between rows. **The reason is
printed on the `LOAD` line of both builds: `n_threads=4 n_threads_batch=4`.** With OpenMP off, ggml runs its own pthread pool at the same
count it would hand an OpenMP parallel-for, so the flag changes *who* owns the threads, not how many compute. That retires the trade this
section was holding its breath over: `GGML_OPENMP=OFF` stays, and with it the property that a build needs no libgomp at runtime — now
kept for a reason we measured instead of one we assumed.

**What the flag is *not* worth buying:** the ON build must link `-fopenmp`, so every binary would carry a libgomp dependency for 0 % of
throughput. And **the lever the A/B leaves standing is the thread count, not the threading library**: llama.cpp's default asked for 4
threads on this 16-CPU host (`nproc` = 16), which is why both builds decode at the same rate while §14's `/usr/bin/time -v` measured 392 %
CPU. Raising `llama_context_params.n_threads` is unmeasured territory and is filed in the backlog (D13) rather than adopted — a build
change has to wait for a number, which is how this section behaved about OpenMP.

**An mmap-first load does read the whole file.** The same runs compared `posix_fadvise(POSIX_FADV_DONTNEED)` on the artifact against a
warm page cache, reading `/proc/self/io`'s `read_bytes` across the load:

```
LOAD       0.34 s (warm)  /  0.43–0.44 s (evicted)     — identical in both build configurations
LOAD RSS   delta +386,964 … +387,288 kB either way      — the mapping is touched, not lazily skimmed
LOAD IO    read_bytes_delta = 0 bytes (warm)
LOAD IO    read_bytes_delta = 309,858,304 bytes (evicted) = every byte of the 295.5 MiB artifact
```

So `use_mmap` (llama.cpp's default) does not spare us the file: first use pulls all 309,855,456 bytes into the process, and it costs about
**0.1 s** — three orders of magnitude less than the **11.32 s** our own `sha256_file` takes over the same bytes (R6d, §9). That is the fact
D12 needed: verifying the digest on every run is expensive *because it hashes through a userspace loop at 26 MiB/s*, not because reading
the artifact is expensive. It reframes D12's options — hashing the way llama.cpp reads (or hashing once per `npd` lifetime instead of once
per `embed_run`) is where the 11 s lives.

### What this decides about §5's amendments

Keeping EmbeddingGemma-2 keeps amendment (a) alive: the two-file artifact pair stays in the plan, and R7c's spike now runs against v0.8.0's
`tools/mtmd` headers rather than a hypothetical. Native 768-d storage (§5) is unaffected — that was a width decision, not a runtime one.
