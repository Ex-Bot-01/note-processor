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
pass forever. So CI does four things in one Linux job, per §6 of the parent design ("**One CI step per OS row** runs
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
via MRL truncation" (spec `:84`/`:234`/`:346`, `todo.md`), and draft v2 of this document recorded it as locked while noting
that the model's own `1_Pooling/config.json` says `embedding_dimension = 768`. The human has now chosen native width, so all
three documents agree on 768 and `todo.md` carries the amendment. What changed underneath is worth keeping in plain sight,
because it is the reason the number was ever contested:

- **Cost, restated honestly:** a row is 768 `f32` = **3072 bytes**, so ~18k chunks is ≈**55 MB** rather than the 37 MB the
  parent design priced. That is still small; it changes no architectural choice here.
- **Truncation stays available and gets cheaper, not dearer.** Google's model card for `embeddinggemma-2` states MRL support
  natively — "truncated embeddings across 128d, 256d, 512d, and 768d … up to a 6x reduction in vector storage costs with
  minimal impact on quality" — which *closes the open question draft v2 left for R6d*: the sub-vectors are trained, not an
  accident of slicing. So if 55 MB becomes a problem, narrowing is a **re-store of vectors already in the table** (slice to
  N, re-normalise) rather than a re-embed — no model run at all. It is not free: it changes model identity (§4), so it means
  a new `@n` and a migration over stored rows, and cosine comparisons must never mix widths. Order still matters if it ever
  happens: mean-pool → L2-normalise → slice → **re-normalise** (slicing first leaves a non-unit vector).
- **Window, now cited rather than reasoned about.** The card gives **8,192 tokens** of context for this model. Draft v2 said
  "nothing here reads a trained window"; it does now. Consequences: `embeddings.max_tokens`'s default of 2048 sits comfortably
  inside it, and `MAX_EMBEDDING_TOKENS = 32_768` **exceeds what the model can attend to** — recorded as a bound to tighten to
  8,192 in R6b2 alongside the enforcement test, not silently left generous. (`max_position_embeddings = 262144` in
  `config.json` is a RoPE/train-length property, not a usable embedding window; §5's refusal posture does not depend on which
  of the two numbers you read.)
- **Whole model means more than one file.** The card describes 740M parameters total — a 270M text backbone plus selectively
  loadable vision (170M) and audio (300M) encoders, all mapping into the same 768-d space. `ggml-org/embeddinggemma-2-GGUF`
  ships that split as `embeddinggemma-2-Q8_0.gguf` (309.9 MB) plus `mmproj-embeddinggemma-2-Q8_0.gguf` (554.8 MB). §12 covers
  what carrying the extra encoders costs, including one thing I could not make work: **no image→embedding path in llama.cpp's
  multimodal layer** as of `0ac1a7f`, so vision may be bytes we ship and never run. That needs your eye before R6b0.

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

Then a whole-branch review across R6a–R6d and merge only on the human's word.

**Three answers are gates on R6b0, not on R6d:** (i) stored width stays 512 via MRL unless the human amends `todo.md`
(§5); (ii) who downloads the artifact (§10); (iii) the pinned binding's llama.cpp revision supports
`gemma-embedding2` — checked by grep in R6b0, and worth a 30-second pre-check before any of it is written.

## 10. Open questions

**For the human:** **who downloads `embeddinggemma-2-Q8_0.gguf` (309.9 MB, ungated)?** I can fetch it on this host and
benchmark without touching your credentials; or you download it and point `np models install` at it. Needed before R6b0
rather than before the benchmark, so R6c is not designed entirely against a fake provider and made to meet reality last.

And one decision that is yours rather than mine, because §5 found a conflict between two approved things: the spec's
**512-d MRL** versus the model's **native 768**. My recommendation is to keep 512 as locked (it halves storage and RAM,
and the MRL question gets measured in R6d rather than argued now) — but if R6d shows truncated vectors diverging from
full ones more than expected, that is a decision to bring back to you with numbers, not one for a round to make quietly.

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

## 12. Fetching the model on first start, and the marker that says it is happening

Amended by the human 2026-10-29: *"it should be automatically downloaded in the first start. And while it downloads add a
flag or marker which we can later use to indicate the user that the model download is currently going on."* That reverses two
locked rules — "downloaded once by `np models install`, no API at index time" and "`install` stays local-file-only" (parent
§10.2) — so it gets its own section rather than a quiet edit elsewhere. `install --path --sha256` is **kept** as the offline
door: an air-gapped machine must still be able to install a model, and a download path that cannot fall back to a file the
human placed by hand is one refusal too many.

**What may not survive the amendment is trust-by-URL.** The pinned facts live in the binary, not in whatever the origin
serves today: repo, filename, size and SHA-256 are recorded at build time (the Q8_0 pair's digests were read from Hugging
Face's public LFS metadata on 2026-10-29 — `2188ac1d…` for the text file, `c4a8a526…` for the mmproj), and a fetched file is
accepted only when it hashes to the recorded value. An unverified download is never published, never renamed into place, and
never left behind as an artifact — same posture as `install`: refuse, name what was wrong, repair nothing.

The rules this section proposes, each of which R6b0/R6c must be able to show a test for:

1. **Who fetches.** `np models fetch <workspace> --id <id>` is the real command. "First start" means: when something needs
   vectors and the configured row's artifact is absent, it reports the state and (for the daemon/app start) kicks off one
   fetch; `np scan` does **not** dial out — indexing staying possible with a network down is spec §9's rule and the one worth
   dying for. `np embed` refuses rather than fetching inline: a nine-minute download inside a command that then writes rows is
   a transactional and UX mess at once.
2. **The marker, and why it cannot be a column.** A `models` row is inserted only after an artifact is verified and renamed
   into place (§4 of the install design), so there is no row to mark mid-download. The marker is therefore a sidecar in the
   model directory — `<id>.fetching.json`, holding `{nonce, started_at, bytes_done, bytes_total, source_kind}` — written
   before the first byte, updated periodically, and removed on both success and failure. `np models list` grows a fifth state,
   `fetching`, read from that file rather than inferred from a partial one.
3. **A stale marker is reported, never deleted silently.** If it is older than its own heartbeat interval with no writer
   advancing it, readers say "a fetch started at X appears to have been interrupted" and name the path. Deleting it would be
   repair; guessing that another process is mid-write would be a lie. `install`'s unclaimed-entry logic already draws this line
   and should be reused rather than rethought.
4. **One fetch per model id.** A second caller sees the marker and reports "already fetching" as its own exit state beside
   §6's three — not an error, not a queue.
5. **Partial bytes are invisible by construction.** Download to `<id>.<nonce>.part`, hash it streaming (the same pass that
   counts progress), then rename — `install`'s publish path exactly, so there is one mechanism for "this file became an
   artifact" instead of two with different failure modes.
6. **Preflight and honest failure.** Free disk checked before the first byte (a 310 MB write into a full filesystem is the
   worst time to discover it); offline, HTTP error, digest mismatch and out-of-disk each get their own refusal naming what was
   attempted and the manual `--path` alternative. No retry loop inside a user-facing command, no exponential backoff hidden in
   a scan.
7. **Secrets and logs.** The artifact is ungated, so no credential is involved; the origin URL still does not go into logs at
   info level or into any report, per parent §9's rule about hosts. `source_kind` in the marker is `"pinned-hf"`, not a URL.

**Open, and needing the human before R6b0:** whether "the whole model" means carrying the encoders *now*. The three shapes
are materially different, and only one of them is cheap:

| shape | bytes | what it buys today | what it costs |
| --- | --- | --- | --- |
| text GGUF now, encoders later when something indexes images | 310 MB | everything the notes need | nothing; a second fetch when the need is real |
| text + mmproj pair now | 865 MB | **possibly nothing**: at `0ac1a7f` I found no image→embedding path in llama.cpp's mtmd layer — it feeds images to generative models | 555 MB on disk, multi-file `install`/`fetch` support (one id, N digests), and a runtime feature we would have to verify exists before promising it |
| HF safetensors, all encoders | 1489 MB | the real multimodal graph, via Transformers/sentence-transformers | a Python runtime in the product, ~1.5 GB of weights and a RAM envelope this host (~2 GB free) may not survive, and no self-contained binary |

My recommendation is the first row with the second one's schema: **build `fetch` and `install` around N files with N pinned
digests now** — that is the part which is expensive to retrofit — and download only the text artifact until something in the
roadmap actually embeds an image. Say "carry both today" instead and I will, but then R6b0 starts by proving llama.cpp can
produce an image embedding for this architecture at all, because I could not find a way today and shipping 555 MB of unrunnable
weights to call it "the whole model" would be a claim, not a capability.

