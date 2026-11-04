# note-processor — handoff / todo

**Read this first.** The design is approved by the human (2026-10-23); M1 and the M2-hardening line are
shipped. Do not re-brainstorm approved designs — execute against the roadmap, or write a design and get it
reviewed before any code for something new.

Repo: `/home/agent/note-processor` · remote `https://github.com/Ex-Bot-01/note-processor` (private; `gh` is
authenticated as `Ex-Bot-01`, git identity repo-local). Because it is private, release assets 404 for
anonymous visitors — download while logged in.

1. **Design (approved):** `docs/superpowers/specs/2026-10-23-note-processor-design.md`
2. **Roadmap being executed:** `docs/roadmap-2026-10-27.md` (Track A = correctness residue, Track B = product)
3. **Current milestone design:** `docs/design-m2-embeddings.md` (M2a — chunk + vector storage; survived review)
4. **Backlog of known defects and deferrals:** `docs/backlog-m2-followups.md`
5. **Per-round working log:** `.superpowers/sdd/2026-10-23-note-processor-m1-core-catalog/m2-hardening-progress.md`

## What this product is (one paragraph)

A single-user, local-first markdown workspace for ~1,500 messy existing notes plus new **daily files**. The
human writes fast and out of order and cannot find things or tell what is still open. Two jobs: make the pile
navigable (deterministic `INDEX.md` catalog + graph of links/tags/clusters, no API in that path) and make
status visible (open/done/paused **item timelines**, latest written text wins; each new day opens with a fenced
TODO segment generated from the last *n* days by a self-hosted OpenAI-compatible endpoint). A chat panel answers
questions from the index/graph with `file:line` citations and shows **both** sides of any contradiction,
unresolved.

## Shipped (main) — R8 worker-thread policy (merged 2026-11-02: `e238aa1` measurement, `67cbe14` policy, `457f443` re-measurement)

* `np embed` stopped inheriting llama.cpp's compile-time four. `[embeddings] threads = "auto"` (default) resolves to
  `min(cpus, max(min(cpus, 4), floor(0.8 × cpus)))`: **twelve workers on this host**, two on a two-core laptop, never more
  than exist. `threads = 4` restores the old behaviour exactly; `threads = 0`, a negative, and anything above what C can be
  told are refused — by name, and now at a policy ceiling of 4,096 rather than at `i32::MAX`, because review found
  `threads = 100000000` parsing and reaching `n_threads`; the earlier revision also *clamped* such a value and llama.cpp
  aborted (`SIGABRT`). Design: `docs/design-m2-thread-policy.md`; measurement: §14's D13 subsection of the local-embeddings design.
* The run reports what it used — `threads 12 of 16 available (auto)`, `"workers"` in `--json`, `null` when no local model
  ran — and an artifact-gated case asserts the count reaching the context equals the count the report states, so "the
  report says twelve while the loader used four" is a test failure rather than a plausible-looking run.
* Also on main from the same pair of merges: the D13 measurement seam (`LoadOptions { threads }`, one process per cell in
  `crates/np-core/tests/probes/thread_sweep.sh`) and three committed sweep logs, so every worker-count number quoted in a
  design document traces to a row someone else can re-check.
* **Re-measured the same day** (`457f443`): the corpus pass under `auto` took **181.41 s / 180.97 s** against a same-day
  four-worker control of 456.43 s — **2.52×**, 40,642 / 40,741 chunks/hour, peak RSS unchanged at ~446 MiB and the same
  +1,556 KiB drift. So the speedup is not confined to the forward pass, and §8's evening rule keeps its verdicts (the
  20,000-note cohort was already inside the threshold at 1.24 h; it is 0.49 h now — headroom, not a new pass). Log:
  `crates/np-core/tests/probes/corpus-auto-2026-11-02.log`. Still four-worker on purpose: §14's cohort solve measures
  per-decode overhead rather than thread scaling, and `run_pass` pins four so the older rows keep meaning what they say.
  Gating that measurement also removed a pre-existing test-isolation race (`Config::load` without `ENV_LOCK`, since
  `ef8a382`), now fenced by `every_case_that_loads_a_config_holds_the_environment_lock`.

## Shipped (branch `r7c/image-embeddings`, not yet merged) — R7c image vectors + read-only `np search`

Design: `docs/design-m2-image-embeddings.md`. What landed, task by task, with the claim each one is allowed to make:

* **Migration 10** — `media` (keyed by SHA-256 of the bytes, not by path), `media_path` (every workspace path those bytes were
  seen at, carrying D12's size + whole-second-mtime stamp pair), `media_embeddings`, and `verified_size`/`verified_mtime` on
  `model_files` so verifying the 554 MB projector costs one `stat` until it moves. Nothing backfilled; the next `np embed`
  fills it.
* **Media resolution** (`crates/np-core/src/media.rs`) — reads a note's **raw bytes**, not `links.target`, resolves
  `![alt](target)` relative to that note's directory byte-exact, percent-decodes once, refuses any symlink that leaves the
  workspace, and judges format from the **bytes**: `.png`/`.jpg`/`.jpeg` by name, then a PNG signature or JPEG SOI marker.
  That closes **F26** — a `.png` full of BMP bytes now refuses as `unsupported-format` instead of embedding.
* **The vision seam** (`crates/np-core/src/vision.rs`, feature `vision`, default off) — `mtmd` behind our own FFI: six archives
  on the link line, not five, and `llama_set_causal_attn(false)` restored after every image decode so a text vector later in
  the same process is still a text vector. Pooled row is 768 floats arriving un-normalised; the seam normalises.
* **`np embed`'s images arm** — one line, six refusal classes as counts (`missing-target`, `outside-workspace`,
  `unsupported-format`, `undecodable`, `over-budget`, `vision-unavailable`), per-path detail in `--json`, no new exit code, and
  a stale-figure sweep that holds itself off when the pass could not read every note.
* **`np search <workspace> <query>`** — one cosine pass over `embeddings` and `media_embeddings` for one model id, one ranked
  list, `--limit` default 10, `--json` with five keys per hit. Writes nothing about the index; opening a first-run database
  still creates and migrates it, like every other command.
* **Install-time fetch** — `np models fetch` needs no config file: `[models] base_url` → `NP_MODELS_BASE_URL` → one baked
  origin (`fetch::resolve_origin`), and the default member set is what **this** binary can consume (`pins::members_for_build`),
  so a build without image support fetches the primary artifact only and refuses to be asked for `mmproj`.
* **Measured, written down where it can be re-checked** (design §14): one image costs **336** tokens at 1024×768 and **88** at
  512×384 — the patch grid, not the card's flat 280 — reproduced through the shipped seam twice on 2026-11-04; a 4096×3072 PNG
  costs **1,064** because the projector clamps its own grid, which is why `over-budget` cannot fire for a single image on this
  pair; the soft-token budget (70–1120) *is* honoured by this builder and nothing uses it.
* **CI**: the `feature-local-model` row's `NOT COVERAGE` floor moved **28 → 29** with `--test local_vision` on the link-proof
  command (measured twice, 2026-11-04). That row builds without `vision`, so the file contributes one line — its feature-off
  statement — and proves nothing about images; at `local-model,vision` with no artifact the same command prints **38**, all ten
  of them skips, because a runner has no projector.

**What this round does not claim, and where the debt is:** image encode **throughput**, peak RSS with the projector open, and
bit-identity of an image vector **across worker counts** are all unmeasured (design §14.4 says what each would take). Retrieval
quality is owed as **F31** — no labelled diagram set exists, so "it finds the right figure" is not said anywhere. SVG rasterising
is **F29**, Matryoshka truncation as a storage lever is **F30**, and **F28 stays open**: `np-cli` has no `vision` passthrough, so
every binary that ships counts figures and refuses them all as `vision-unavailable`. README's "Images in the index" section says
that plainly rather than letting a reader infer that `np embed` fills the image index.

## Shipped (main)

- **M1 core catalog** — `np-core` + `np scan <workspace>` producing a byte-stable `INDEX.md` (+ shards) and a
  SQLite registry: titles with provenance, headings, tags, checklist items, links resolved with evidence, word
  counts, encoding. Incremental scans; re-parse versioning that never records a version it could not satisfy.
- **M2 hardening line** — F13 byte-exact paths · F18 Windows refusal handling · F19 duplicate-note merge ·
  F20 raw bytes in reports · F21 durable upgrade announcements · F22 bounded durable records · F23 ambiguous
  rows marked rather than deleted · F24 per-fact merge assertions · F17 oversized indent refused, not clamped ·
  F16 wide golden coverage · A4 durable file identity (`files.file_id`, one eligibility tally shared by admission
  and pairing, re-parse whenever the stored digest moves).
- **Releases** — `.github/workflows/release.yml`: a tag `v*` builds Windows/Linux/macOS and publishes a GitHub
  Release. `v0.1.0` is live: `np-0.1.0-x86_64-pc-windows-msvc.zip` (contains `np.exe`) plus Linux and macOS
  tarballs.

Quality bar in force on every merge: `cargo fmt --check`; `cargo clippy --all-targets -- -D warnings`;
`cargo test --workspace` counted by exit code and per-binary `FAILED` lines, never by summing `test result`
lines; config suite with `HOME`/`XDG_CONFIG_HOME` unset; `RUSTDOCFLAGS="--deny warnings" cargo doc`; the cfg
dead-code script; `verify_catalog_goldens.py` printing `GOLDEN CHECK: pass`; CI green on ubuntu **and**
windows-latest. **Goldens are never regenerated** — a diff under `tests/golden*` stops the work and gets
reported.

## Historical: M2a (branch `m12/embeddings`, merged 2026-10-30 at `9193732`)

Chunking and vector storage, deliberately without clustering: the spec gates the model on throughput ("measured
before we commit", §13 step 2), so M2 is cut into **M2a** (storage + chunker + provider seam + benchmark) and
**M2b** (clustering, `clusters`/membership, review queue with evidence, the reserved `cluster` column — which
will change every golden at once, by design, in one reviewed milestone).

Done on the branch: migration 7 (`models`, `chunks`, `embeddings`; FKs, `typeof='blob'` and length CHECKs pinned
by tests) · `chunk.rs` (pure over decoded text, `MAX_CHUNK_BYTES = 1800`, block seams, char-boundary safe, its
contract tested including a one-byte mutation case) · the review's P2 batch · **Round 3a**: `embed.rs` — `Vector`
(little-endian `f32`, refuses ragged and wrong-dim buffers), `Embedder`, `NoEmbeddingProvider` holding no client, URL or
key (pinned by `size_of == 0`) — and the `[embeddings]` config section (`provider` as an enum defaulting to `none`,
`deny_unknown_fields` on that one section).

**Shipped on `m12/embeddings`** (whole-branch review pending): Round 3b — chunk writes by per-row diff (never delete-and-reinsert, which would cascade away
vectors), vectors written with `ON CONFLICT (chunk_id, model) DO UPDATE`, and `EMBED_VERSION` covering **chunker
coverage only** (provider availability is a reported count, never version debt — see design §5's P0). Nothing reads
`embeddings.model` / `embeddings.dim` yet; the scan wiring is where they get read. Then the EmbeddingGemma benchmark.

## Locked decisions — do not revisit

- Shape: **daemon owns everything, Tauri window is a dumb client** (`np-core` / `npd` / `np-app`). Login
  autostart so today's file and TODO segment exist before the app opens. Linux **and** Windows.
- Single machine, single user. Loopback HTTP + token in config (port 8765) — no cloud, no accounts.
- Embeddings: self-hosted `google/embeddinggemma-2` (Apache-2.0), **amended by the human 2026-10-29 on three points**:
  **(a)** the **whole model**, not the text tower alone — vision and audio encoders are carried too, so the artifact set is
  more than one file; **(b)** stored width is **768 with no MRL truncation** (the card says MRL is native at 128/256/512/768,
  so narrowing later is a re-store of vectors we already hold, not a re-embed — but it changes model identity, §4);
  **(c)** the artifact is **fetched automatically on first start**, with a marker that says a download is in progress so the
  UI can say so later. `np models install --path/--sha256` stays as the offline door. mean-pool + L2-normalise, task
  prefixes per the model card, sha256-pinned to digests recorded in the repo, recorded on every vector. Fallbacks in spec §9 are one config key each. **Benchmark order set
  2026-10-28: EmbeddingGemma first**, then the endpoint candidate, MiniLM only if both stall (design §9).
- Endpoint candidate is the same self-hosted OpenAI-compatible server this project is developed against, serving
  Qwen3.8-Flash-Next. **Base URL and key never enter the repo** — runtime `NP_EMBED_BASE_URL` /
  `NP_EMBED_API_KEY`, read only by the provider; benchmark provenance is redacted; nothing in CI authenticates
  to it. Whether it serves `/v1/embeddings` at all is a measured question, asked first.
- Only four things may ever be written into the workspace: create today's daily file, replace the fenced
  `<!-- np:todo -->` region, append approved `[[wikilink]]` under `## Related`, regenerate `INDEX.md` (+ shards)
  and `processed/`. Note bodies are never touched by an LLM.
- Status: **open is the default** (no marker), a tick means done; markers are config with German and English
  defaults (spec §8.7). Item identity across files merges only on an anchor; fuzzy matches go to the review queue
  as suggestions, never as assertions.
- Derived data lives in the DB and `INDEX.md`, one catalog entry per note, **no importance filter**.
- Frontend: React + Vite + TypeScript + CodeMirror 6 (M5).

## Method (unchanged, and it is what keeps the defect count honest)

`superpowers:subagent-driven-development`: fresh implementer per bounded round, fresh scoped reviewer after each,
fix loop for Critical/Important, broad whole-branch review at the end, merge only on the human's word. **Design
review before any code.** Two writers never share a worktree. Reviewers have no shell — they must say "static
read" and list commands; the controller runs load-bearing claims himself. A subordinate's evidence outranks a
controller instruction: refuse and explain rather than fake coverage. No behaviour claim about a platform until
it was run there (F18's rule). Mutation testing means **production** mutations — deleting an assertion to see
what fails is not a check, because removal makes tests pass.

## Environment facts

- Rust lives in `~/.cargo/bin` — `export PATH="$HOME/.cargo/bin:$PATH"` before any cargo command.
- Node v22 + npm present (needed from M5). `git`, `gh` authenticated. Never put the GitHub token on a command
  line or in a file; `$GH_TOKEN` is already wired for `gh`.
- Subagent endpoints die around 30 minutes: keep rounds bounded, tell the implementer to `wip:`-commit as soon as
  it compiles, and resume rather than restart.

## Open items to ask the human about, at the milestone that needs them

- **M3:** where daily files live inside the workspace (`YYYY-MM-DD.md` at root vs `daily/`). Needed before M3's
  design, not before.
- **F18 residue:** Windows file identity (`VolumeSerialNumber` + `FileId`) needs real Windows hardware and a
  recorded run; CI proves only that the path is inert.
- **F25** (over-wide token renders a physical line holding only `-`) is filed and unadjudicated.
- **Owed measurements:** A4's A/B/A benchmark and whole-branch review were skipped on instruction at merge time;
  `typeof(vector)` is probed (`a_string_bind_is_refused_from_embeddings_vector`). Still open: the "chunks owed but unembedded" count in the scan report, and backlog D1–D10 from the M2a whole-branch review.

- **Answered by the human (2026-10-29): embeddings are local.** The self-hosted server does not serve
  `/v1/embeddings`; EmbeddingGemma-2 runs locally and does all embedding, the LLM API is used only to answer chat
  questions (phase 2), and network appears only at setup to download the artifact. `install` therefore stays
  local-file-only and no HTTP embedding transport will be written. Design: `docs/design-m2-local-embeddings.md`.
- **Before the first embed run, three backlog decisions:** D2 (is frontmatter part of the embedded text; do citations
  index the file or the body), D5 (does the embed round delete stale vectors or compare-and-skip — answered in
  `design-m2-local-embeddings.md` §6: only with their chunk row) and **D3 (a stale note's chunks must never read as
  current, which bites the moment `np embed` becomes the first production reader of `chunk_state`/`chunk_digest`)**. All
  three are cheap now and expensive after 18k vectors exist.
- **Runtime decided 2026-10-29 (you answered §13): use llama.cpp in the back and keep EmbeddingGemma-2**, accepting a bigger package.
  We build llama.cpp ourselves from a pinned, sha256-verified tarball (**v0.8.0**, `sha256 = 51b1e025…210ee`) behind the `local-model` feature
  and declare the C API by hand — the published Rust binding cannot be fed our source, vendors a revision without `gemma-embedding2`, and would
  add bindgen/libclang to every build. Measured here: builds in 23 s; ~13 MB of static archives against a 5.5 MB stripped `np` today.
  **Package ceiling: ≤ 40 MB stripped per platform**, printed by the release workflow from R6d. Evidence and reasoning:
  `docs/design-m2-local-embeddings.md` §14.

- **Direction set 2026-10-29: steer toward Prototype 1** (`docs/roadmap-2026-10-27.md` §3C) — `scan → fetch → embed → search`, CLI-only, on the
  real corpus. Rounds that don't put a queryable vector in front of the human sooner wait, and whatever waits gets named out loud.

- **R6b0's gate is answered, and it failed**: the pinned binding (`llama-cpp-sys-2` 0.1.160, newest published) vendors a llama.cpp that knows
  `LLM_ARCH_GEMMA_EMBEDDING` but **not** `GEMMA_EMBEDDING2`, `build.rs` has no source override (always cmake), and this host has no `cmake` and
  no root. Full evidence and the three shapes with prices: `docs/design-m2-local-embeddings.md` §13. **Waiting on the human**: prototype with
  EmbeddingGemma-300M today, or pay for a self-built llama.cpp to keep EmbeddingGemma-2.

- **R6b1 shipped (`local-model`, loader only — no tokenizing, no decode, no vectors):** `crates/np-core/src/local_model.rs` loads a GGUF through our
  own FFI into a `LoadedModel` (two handles, one `Drop` order: `llama_free` then `llama_model_free`) and reports what the artifact answers. The
  largest unknown in the prototype is now dead: **v0.8.0 loads `embeddinggemma-2-Q8_0.gguf` on this host** — `n_embd_out 768`, `n_embd_inp 512`,
  `n_ctx_train 262144`, `n_vocab 262144`, pooling MEAN read from metadata and not overridden, `add_bos` **and** `add_eos` both true (R6b2's token
  count budgets two specials). Cost of opening it here (§14's table has the method; `crates/np-core/tests/probes/model_load_probe.c` is the C half):
  **0.35 s warm / 0.44 s cold from our side**, peak RSS **419.4 MiB** in the bare C probe and **421.1 MiB** with loads serialized in Rust — **+1.7 MiB for
  us**, not zero. The ~524 MiB figure is the same suite on libtest's default threads and belongs to no single load; do not quote it as a per-load cost.
  Three load-and-drop cycles hold the same open-state RSS (±28 KiB) and do not crash — that is crash detection, **not** leak detection.
  The declared-vs-actual width debt is closed at the only place it can be: `load(path, configured_dim)` refuses against `llama_model_n_embd_out`, so
  declaring 512 against this file names both numbers before a context exists. `tests/local_load.rs` is artifact-gated on `NP_TEST_MODEL_PATH` and
  prints `NOT COVERAGE` without it — the CI feature row proves the refusals, not the 768.

- **R6b2 shipped (text in, one unit-length 768-d vector out):** `LoadedModel::embed_text(prefix, text, budget)` tokenizes with
  `add_special = true` and sizes its buffer from llama.cpp's own "you needed N slots" answer (`src/llama-vocab.cpp:4645-4647`, measured:
  a 2-slot buffer for a 17-token sentence returns `-17`), refuses over the configured **token** budget — first production use of
  `TokenBudgetExceeded`, and it reports tokens (a 23-byte sentence is **9**) — then refuses above the context window, decodes once with a
  borrowed `llama_batch_get_one`, copies the pooled row out of the buffer the context rewrites next decode, and L2-normalises, refusing a
  zero vector or a non-finite component instead of storing a row that silently scores 0.0 against everything. `LocalEmbedder`
  (`LoadedModel` + a row's two prefixes + budget) implements `Embedder` on the document path; `LocalModelProvider` still answers
  `RuntimeNotBuilt`, because R6c decides who holds ~420 MB and when.
  **The abort that was found first:** this artifact is an encoder — `create_memory` returns null for it, so `llama_decode` diverts to the
  encoder path (`src/llama-context.cpp:1845-1847`; measured: `llama_get_memory(ctx)` is `(nil)`) and *that* asserts `n_ubatch >= n_tokens`
  (`src/llama-context.cpp:1631`, not the decoder's `:1918`/`:1920`, which never run here). `GGML_ASSERT` → `GGML_ABORT` survives `NDEBUG`.
  At llama.cpp's default `n_ubatch = 512` a
  **600-token sequence killed the process (exit 134)** — reachable from an ordinary ~1,800-byte note, since `max_tokens` then accepted up to
  8,192. `load` now sets `n_ctx = n_batch = n_ubatch = 4096`, and a 633-token decode returns rc=0 with ‖v‖ = 1.0 instead.
  **Three numbers, none of them asserted** — only throughput is printed by a test (`decode_throughput_is_printed_not_asserted`); size comes
  from `cargo build --release` plus `nm`, and the RSS curve from `crates/np-core/tests/probes/decode_probe.c`: ~**130 tok/s** (543 tok in
  3.85 s, 2,163 tok in 18.0 s; 392% CPU because ggml runs its own thread pool — `llama_n_threads_batch` = 4 — so OpenMP-off is not
  single-threaded; R6d's A/B has since measured that OpenMP-on changes nothing at those four threads, §14), stripped release size **5.5 MiB → 11.0 MiB** with the feature (**+5.5 MiB** against
  §14's ≤ 40 MB ceiling; symbol counts must be taken on the *unstripped* binary, since `strip` removes the table and then "0 llama symbols"
  means nothing), and peak RSS **+0 / +47 / +109 / +218 / +458 MiB** above baseline at 133 / 513 / 1,026 / 2,052 / 4,085 tokens — the compute
  buffer is reserved at init and touched per decode, so long notes cost memory and ordinary chunks cost nothing.
  Semantic sanity is an ordering, not a score: `cos(cat, feline) = 0.8302` vs `cos(cat, revenue) = 0.5887`. Empty input is not an error —
  it embeds BOS+EOS (2 tokens) into a real unit vector, which is what the artifact does rather than a rule we invented.
  **§5's ceiling question is decided (2026-10-30, option B):** `MAX_EMBEDDING_TOKENS` **8,192 → 4,096**, because that is the width of the context
  this build creates and a ceiling above the window only produces configs refused later by a different sentence; default `embeddings.max_tokens`
  **2,048 → 1,024** (≈8 s per worst-case chunk at ~120–141 tok/s, interpolated between the measured points). Both stay revisitable — config numbers
  with no stored data behind them yet — and `local_model` now asserts `MAX_EMBEDDING_TOKENS <= REQUESTED_CTX` at compile time so the pair cannot drift.
  **Decision C, later on 2026-10-30:** the human asked for the 8,192 ceiling back, so `MAX_EMBEDDING_TOKENS` and `REQUESTED_CTX` moved to
  **8,192 together** — raising only the first would have re-created the config-refused-later-by-another-sentence defect B removed. Measured
  before asking for the wider window (`decode_probe nctx=8192`): +0 MiB at rest (133 tokens), and 7,999 tokens in **101 s** at peak
  **1,478 MiB (+1,058 MiB)** — so `DEFAULT_EMBEDDING_TOKENS` stays 1,024 and the interpolated "~63 s per 8k chunk" is replaced by that
  measurement (~79 tok/s). Still unmeasured: whether long-note *recall* improves above the window; we refuse there because one decode cannot
  hold more in this build, not because the model cannot represent it. Long-note retrieval is bounded by chunking either way.

* **R6c1 — `np embed <workspace>`, §6's four arms** ✅ *done 2026-10-31, branch `m14/embed-write`*
  (`crates/np-core/src/embed_run.rs` + argv coverage in `np-cli`). Currency is one chunk-level query fenced on
  `files.state = 'ok'`, and a vector counts as current only when it was written against **this** model id with a
  `source_hash` equal to SHA-256 of the prefixed text the provider receives — so changing `prefix_doc` in the database
  makes every stored vector stale, which §3's note about prefixes turning into lies needed for its first production
  reader. Over-budget chunks are refused and counted, never truncated; a `SQLITE_BUSY` batch stamps nothing and is
  reported as the gap it is. The writer takes an injected `&dyn Embedder`, so all of this is measured against a fake in
  the default suite (21 cases) and none of it waits on a 310 MB artifact.
  Review: both scoped reviewers timed out with nothing written, so the controller reviewed its own round under a stated
  conflict of interest and leaned on mutation testing instead — which found two surviving mutants worth naming: deleting
  `paired`'s **one-for-one length** arm left every case green (a provider answering N texts with N−1 vectors would pair
  silently, stamp the wrong vector onto chunks, and mark them current forever), and deleting its **per-vector width** arm
  likewise. Both are fenced now. Still owed: an independent read of this range by someone without that conflict.
* **R6c2 — wire the runtime that already works into the provider that refuses.** Verified by reading the call graph: no
  production path calls `local_model::load` or `LoadedModel::embed_text`, so `LocalModelProvider::embed` answers
  [`EmbedError::RuntimeNotBuilt`] in **every** build, feature flag or not — the working loader and forward pass sit behind a
  door nothing opens yet. R6c2 opens it (lifecycle: who holds ~420 MB at rest and for how long), then adds the artifact-gated
  §7 assertions that no fake can stand in for: batch invariance under a **non-empty** `prefix_doc` (§5 left that pairing
  untested on purpose), unit norms, and the accept-at-exactly-`window` boundary. Blocked on nothing but a window that can hold
  the work — the last five subagent rounds died at their deadlines.
* **R6c2 — wire the runtime that already works into the provider that refuses.** ✅ *wired 2026-10-31, branch
  `m15/runtime-wiring`* — `np-cli` resolves `local-model` to `local_model::LazyLocalEmbedder`, which loads llama.cpp on
  the first batch (identity answers come from the row, so an up-to-date workspace pays nothing) and mirrors a failed
  load so every batch refuses for the same typed reason. Measured against the real artifact through argv: 768 × float-32
  per row, ‖v‖ = 1 within 1e-3, second run `embedded 0 / current N`, prefix edit re-embeds everything — **but** the
  mutation that was supposed to prove the last claim refused to die, and the reason is a **P1**: `embed_run` prepends
  `prefix_doc` before stamping and calling `embed`, and `LoadedModel::embed_text` prepends it again, so documents reach
  llama.cpp prefixed twice and the token budget counts the doubled sequence. Both halves were right alone; nothing
  tested the pair.
  **Resolved the same day by A′**: `Embedder::model_inputs` is a required trait method — the provider answers what the
  model will read, the writer stamps that and hands `embed` raw text — with both `local_model` shapes spelling the
  dressing through one private function. Three mutations fenced (loaded embedder ignores prefix → argv fails; wired
  provider declares undressed inputs → staleness fails; the two declarations drift → new artifact-gated case in
  `local_embed.rs`). Real-artifact numbers after the fix: cos(same) 1.000000 vs cos(changed prefix) **0.895576**, one
  instruction applied, `local_embed` 13 passed (148 s). Next: R6d's corpus benchmark.
* **R6d — benchmark round.** *three rounds, 2026-10-31 (`m16/corpus-benchmark` + `m17/openmp-ab` + `m20/corpus-run`)*: `crates/np-core/tests/local_window.rs`
  (3 artifact-gated cases, all printing `NOT COVERAGE` without the file). Measured on this host against the pinned Q8_0:
  a `MAX_CHUNK_BYTES` chunk is **403 tokens in 2.61 s** (**155 tok/s**, ~3× §5's interpolated figure, which was drawn
  between much longer probe points); one near-ceiling chunk of **7,992 tokens** takes **98.6 s** at peak RSS
  **1,525,500 KiB**; the exact window edge is real — **8,192 tokens accepted in 100.1 s**, **8,193 refused before any
  decode**. That last one closes Scope B's P2-12, the surviving mutant `local_embed.rs` had recorded rather than paid
  for: mutating either `>` in `embed_text` (`n >= budget`, `n >= window`) now turns the acceptance into a refusal naming
  its own rule, and dies in 1.5 s because a refusal never reaches the decode. Also measured for D12: verifying this
  artifact by digest costs **11.32 s** per pass (26 MiB/s), which is why "re-hash on every embed" needs a decision
  rather than a reflex. The second round built the pinned tree twice (`-DGGML_OPENMP=OFF` vs `ON`, production option set
  otherwise, 36 s each at `-j8`) and A/B'd them through `crates/np-core/tests/probes/openmp_ab_probe.c` with **one process
  per measurement** — 5 processes per cell at 403 tokens, 3 at 7,992, OFF-then-ON back-to-back; the raw log is committed as
  `crates/np-core/tests/probes/openmp-ab-2026-10-31.log`. Medians taken across processes: **403 warm 156.3 vs 156.0 tok/s,
  403 cold 155.8 vs 156.0, 7,992 cold 81.4 vs 80.9, 7,992 warm 81.5 vs 81.1** — every gap smaller than the spread between
  two processes of the *same* build, on this one host with this one artifact, and `cpu_utilisation` measured at
  **396–400 %** on both sides (four cores busy either way; `n_threads=4 n_threads_batch=4`). ON's only measurable cost is a
  libgomp dependency plus **+180 to +292 kB** peak RSS. So `GGML_OPENMP=OFF` stays, and the thread count is filed as D13.
  The same runs answered the mmap question: an evicted artifact moves `read_bytes` by **309,858,304 bytes — more than the
  file's own 309,855,456, because that counter is page-granular** — inside a load window ≈ 0.09 s longer than a warm one
  (an upper bound: that window also builds the context), which is what D12 needed to hear about its own 11.32 s hash:
  reading is ~126× cheaper than our hash loop.
  The third round ran the corpus: `crates/np-core/tests/local_corpus.rs`, **2,048 notes / 2,048 chunks in one process**
  through `embed_run` + `LazyLocalEmbedder` (no C probe), RSS sampled between batches, run **twice end to end** because
  §8 forbids quoting a lucky number: **16,161 / 16,163 chunks/hour** (exact counts; **154.5 tok/s**, derived from a mean
  token count sampled from 64 of 2,048 chunks), steady wall **456.20 / 456.16 s**, cold start timed apart at **0.44 /
  0.46 s**. `VmRSS` drift across all 2,048 decodes **+1,436 / +1,552 KiB** (9 samples; peak ~447 MB, with host headroom
  printed): **no leak detected at this scale**, which is not the same sentence as "nothing leaks" — the fence fails above
  ~128 KiB retained per chunk, and nothing was measured past 2,048 decodes. Round 1's 1.5 GB peak remains a *length*
  effect. The next run over the same corpus costs **0.04 s** on both runs (`current = 2048`, no model load). Two cohorts
  separate the cost: **31.1 / 29.0 ms fixed per decode**, **5.531 / 5.548 ms/token** marginal — a two-point solution whose
  own residual proves nothing, validated instead against the corpus run's aggregate (predicts it to 0.6–1.3 %). That fixed
  part caps batching across texts at ~14 % of a short decode and ~2 % of a long one, measured at four threads; whether
  thread count turned out to be the bigger lever: measured on 2026-11-02 at **2.90× (short) / 3.60× (near-ceiling)** for
  sixteen workers against four — see §14's D13 subsection. §8's evening rule, applied with the
  bound stated (4 h): 20,000 notes = **1.24 h ✓** at short-note length and **7.89–7.91 h ✗** at 251 tokens — break-even
  ≈ 64,600 short notes, ≈ 10,100 long ones, both columns single-rate extrapolations good to ~1 %. Both runs' output is
  committed as `crates/np-core/tests/probes/corpus-run-2026-10-31.log`, with the spreads and the caveats beside the
  numbers. Still open in R6d: the batching **decision**, and now the worker-count one too — D13 is measured
  (`m22/thread-sweep`), so what waits on the human is whether any default changes, not whether to look.
* **D12 — verify-on-change (shipped on `m19/digest-verify-on-change`, 2026-10-31).** `np embed` now checks the artifact
  against `models.sha256` before it reads a note: one `stat` when the stamp holds, a full hash when size or mtime moved
  or when no stamp exists yet (migration 8 adds the two nullable columns and backfills nothing). A disagreement refuses
  with both digests and writes neither vectors nor a stamp. The report says what happened — `digest` re-checked and
  stamped / re-checked but unstampable (no readable mtime, or the index refused the write; the embed still proceeds, but
  nothing claims a stamp exists) / not re-read / nothing to verify — in text and in `--json` (`artifact_check`, present
  and null when no artifact was involved). Deliberately weaker than it sounds: size+mtime detects changed bytes, not a
  crafted mtime, and the printed sentence says so. Tests: `crates/np-core/tests/artifact_verify.rs` (10 cases, no
  artifact needed, so CI runs all of them), an argv refusal case mutation-checked against a neutered comparison, and the
  real-model argv case asserting the wordings across consecutive runs plus the populated `--json` shape. Reasoning: §7's
  verify-on-change subsection.
* **CI**: GitHub refuses to start jobs — no Actions budget. Until that changes, every gate below runs locally and its
  exit codes are the evidence; nothing here is claimed on CI's behalf.
