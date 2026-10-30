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

## In progress — M2a (branch `m12/embeddings`)

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
  **600-token sequence killed the process (exit 134)** — reachable from an ordinary ~1,800-byte note, since `max_tokens` accepts up to
  8,192. `load` now sets `n_ctx = n_batch = n_ubatch = 4096`, and a 633-token decode returns rc=0 with ‖v‖ = 1.0 instead.
  **Three numbers, none of them asserted** — only throughput is printed by a test (`decode_throughput_is_printed_not_asserted`); size comes
  from `cargo build --release` plus `nm`, and the RSS curve from `crates/np-core/tests/probes/decode_probe.c`: ~**130 tok/s** (543 tok in
  3.85 s, 2,163 tok in 18.0 s; 392% CPU because ggml runs its own thread pool — `llama_n_threads_batch` = 4 — so OpenMP-off is not
  single-threaded, and R6d still owes the A/B), stripped release size **5.5 MiB → 11.0 MiB** with the feature (**+5.5 MiB** against
  §14's ≤ 40 MB ceiling; symbol counts must be taken on the *unstripped* binary, since `strip` removes the table and then "0 llama symbols"
  means nothing), and peak RSS **+0 / +47 / +109 / +218 / +458 MiB** above baseline at 133 / 513 / 1,026 / 2,052 / 4,085 tokens — the compute
  buffer is reserved at init and touched per decode, so long notes cost memory and ordinary chunks cost nothing.
  Semantic sanity is an ordering, not a score: `cos(cat, feline) = 0.8302` vs `cos(cat, revenue) = 0.5887`. Empty input is not an error —
  it embeds BOS+EOS (2 tokens) into a real unit vector, which is what the artifact does rather than a rule we invented.
  **§5's ceiling question now has a price attached and still needs the human:** extrapolating the measured rate (which falls with length),
  `max_tokens = 8192` is roughly a **minute per chunk** — an input the shipped code refuses above 4,096 anyway — while the default 2,048 is a
  measured ~16 s; and the runtime window is 4,096 today, so anything above that is refused until
  `REQUESTED_CTX` moves — which §14's curve says costs +458 MiB by 4,085 tokens. Still owed: R6c wiring config → provider → `embeddings`
  rows, and R6d's corpus-scale benchmark (batching across texts is deliberately not in this API yet).
