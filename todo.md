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
- **Gates on R6b0** (the local-inference round): width and provenance are settled (native 768; `np` fetches it — see the locked
  block above), so what remains is one check and one constraint: the pinned Rust binding's vendored llama.cpp revision must know
  `gemma-embedding2` (grep the vendored source, never trust a changelog), and R6b2's cross-quantisation check needs free RAM it may
  have to wait for. Assigned in R6b0 too: the four-step `--features local-model` CI job that design §2 promises and ci.yml does not
  yet run.
