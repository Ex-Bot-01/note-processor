# F51 rerank stage — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an optional cross-encoder pass that reorders the first N hits of any `np search` / `np serve search` answer, off by default, behind the existing `local-model` feature.

**Architecture:** Retrieval stays exactly as it is; a new `np_core::rerank` module owns the pair recipe, the token fitting and the reorder as pure functions plus a `Reranker` trait, and `local_model` supplies one implementation of that trait in llama.cpp **rank mode**. `search::ranked` is the single consumer, so the CLI and the server cannot disagree. The artifact is pinned by id with a new **role** on the pin table and gets no `models` row, because it declares no vectors.

**Tech Stack:** Rust 2021 workspace (`np-core`, `np-cli`), hand-written llama.cpp FFI (pinned v0.8.0, no bindgen), rusqlite, serde/serde_json, clap, tracing; Python 3 fence/harness scripts under `.github/scripts/`.

**Spec:** `docs/design-f51-rerank-stage.md` — read it before any task; it holds the rulings this plan argues from (rank mode is our claim, note text = `title + "\n" + body`, no `models` row, per-id origins, verify-once-per-process, measurement belongs to the human).

## Global Constraints

- Default build unchanged: **no new dependency**, no C++/cmake requirement outside `local-model`. `cargo build --workspace` must still work on a machine with no cmake.
- **No migration.** Nothing in this plan adds a table, a column or an entry to `db::MIGRATIONS`.
- **No llama.cpp re-pin.** `third_party/llama-cpp.toml` and its two digests do not move. If the pinned tree cannot score the artifact, stop and report.
- Only one quant is pinned: Q8_0. Q5_K_M and Q4_K_M are measured through an artifact-gated seam and never accepted by production code.
- `np search` **never fetches** the reranker — not in hybrid, not with `[models] auto_fetch = true`.
- Every `unsafe` site needs a `// SAFETY:` line: both crates `deny(unsafe_op_unsafe_fn, clippy::undocumented_unsafe_blocks)` at crate level and `.github/scripts/check-unsafe-lints.py` fails a file that allows either lint away.
- Tracked files carry **no host paths, no account names, no CI run numbers, no "measured on <date>" narration** (`AGENTS.md`). Dates come from `git log` or `date -u`.
- Comments say why the code is the way it is now — no ticket history, no round numbers.
- **Never regenerate the catalog goldens.** Any diff under `tests/golden*` is a stop-and-report.
- One task per commit group, one change per commit; a refactor that changes no behaviour lands separately from the commit that changes behaviour.
- Gate list: `.github/scripts/gates.sh`. The `local-model` rows need `CMAKE` pointing at a real cmake binary — with it unset they print **SKIPPED**, which is not a pass. Artifact-gated suites print `NOT COVERAGE` when their env var is absent; count them, never claim them.

## Review Focus

The five things no single task's tests will make obvious, each of which a real user meets:

- **Bytes that are not the pinned bytes.** A reranker file that exists with the right name and the wrong digest must never reach a forward pass — decline (config) or refuse (flag), and say which. → Task 9 pins it.
- **A stage that is silently dead on an unconsented vault.** `[embeddings] provider = "none"` is the default, and `--mode keyword` needs no embedder; rerank must still run there, because a cross-encoder reads two strings. → Tasks 8 and 9 pin it.
- **`--limit` smaller than N.** "Rerank the top 25" with the default `--limit 10` has to score 25 and print 10; getting this wrong makes every N above 10 quietly mean 10. → Task 8 pins it.
- **Reading a RANK row at the wrong width** is a memory-safety bug, not a wrong number: the pooled buffer is `n_cls_out` floats, not `n_embd_out`. Rank mode reads `llama_model_n_cls_out` back and refuses anything but 1, and refuses a context that did not become RANK. → Task 4 pins it.
- **Frozen output.** With no flag and no config key, stdout, stderr and `--json` are byte-identical to today, including the key set of every hit. → Tasks 8 and 9 pin it with a golden document.

---

### Task 1: Mirror the five header entries rank mode needs

**Files:**
- Modify: `crates/np-core/src/llama_ffi.rs` (inside the `extern "C"` block, next to the other vocabulary accessors)
- Test: `.github/scripts/check-llama-decls.py` (existing fence row `llama-decls`)

**Interfaces:**
- Consumes: nothing.
- Produces: `llama_model_n_cls_out`, `llama_vocab_bos`, `llama_vocab_eos`, `llama_vocab_sep`, `llama_vocab_get_add_sep` — used by Task 4 only.

- [ ] **Step 1: Add the five declarations** with the C spellings kept (this module mirrors a header; a reviewer greps the C name):

```rust
pub fn llama_model_n_cls_out(model: *const llama_model) -> u32;
pub fn llama_vocab_bos(vocab: *const llama_vocab) -> llama_token;
pub fn llama_vocab_eos(vocab: *const llama_vocab) -> llama_token;
pub fn llama_vocab_sep(vocab: *const llama_vocab) -> llama_token;
pub fn llama_vocab_get_add_sep(vocab: *const llama_vocab) -> bool;
```

  Each doc comment names the header line it was read from (`include/llama.h:614`, `:1217-1220`, `:1227`), says who owns what (every one of these borrows: the vocabulary belongs to the model, never freed directly), and for `llama_vocab_get_add_sep` says what a false answer costs — no separator between the two halves of a pair.

- [ ] **Step 2: Run the fence** — `python3 .github/scripts/check-llama-decls.py` — expect every new name reported as matched, exit 0. Then `python3 .github/scripts/check-llama-decls.py --self-test`.
- [ ] **Step 3: Compile the feature** — `CMAKE=<path-to-cmake> cargo clippy -p np-core --features local-model --all-targets -- -D warnings`. Expect: clean. (Declarations nothing calls yet must not trip `dead_code`: they are `pub` in a `pub mod`, which is why no `#[allow]` is needed — if one does fire, the visibility is wrong, not the lint.)
- [ ] **Step 4: Commit** `git commit -am "feat(ffi): mirror n_cls_out and the BOS/EOS/SEP vocabulary accessors"`

---

### Task 2: Give the context builder a purpose (refactor, no behaviour change)

**Files:**
- Modify: `crates/np-core/src/local_model.rs` (`open_context`, `load_with_options`, plus a `Purpose` type beside `Pooling`)
- Test: existing suites — `cargo test -p np-core --features local-model --test local_load --test local_window --test local_runtime --test local_threads`

**Interfaces:**
- Consumes: `load`, `load_with_options`, `read_numbers`, `requested_ctx` as they are.
- Produces: `enum Purpose { Embed { configured_dim: usize }, Rank { ctx_tokens: usize } }` and `fn open_context(model: NonNull<llama_model>, purpose: Purpose, options: LoadOptions) -> Result<(NonNull<llama_context>, ModelFacts, usize), EmbedError>` — Task 4 calls it with `Purpose::Rank`.

- [ ] **Step 1: Write the failing shape test** in `crates/np-core/tests/local_load.rs` (artifact-gated file; it must compile with no artifact present):

```rust
#[test]
fn a_rank_context_is_built_at_the_requested_window() {
    // Purpose::Rank sets n_ctx = n_batch = n_ubatch = ctx_tokens and pooling RANK explicitly;
    // Purpose::Embed keeps today's behaviour byte for byte, including the width check and the
    // refusal of a context that pools with RANK.
    assert_eq!(Purpose::Rank { ctx_tokens: 2048 }.ctx_tokens(), 2048);
    assert!(matches!(Purpose::Embed { configured_dim: 768 }, Purpose::Embed { configured_dim: 768 }));
}
```

- [ ] **Step 2: Run it, expect a compile failure** naming `Purpose`.
- [ ] **Step 3: Implement the extraction.** `open_context` matches on `purpose` for exactly three things — whether the `n_embd_out == configured_dim` check runs, what the window is (`requested_ctx(n_ctx_train)` versus the caller's `ctx_tokens`, each still bounded by the trained window), and whether `pooling_type` stays `UNSPECIFIED` (embedding: capability comes from the file) or is written as `LLAMA_POOLING_TYPE_RANK` (rank: **the artifact declares no pooling and `llama-hparams.h` defaults it to NONE, so this is our claim and Task 4 reads it back**). Everything else — the three-numbers-one-window rule, the worker-count refusal, both readbacks — stays shared. `load_with_options` calls `open_context(model, Purpose::Embed { configured_dim }, options)`.
- [ ] **Step 4: Run the four suites above plus `cargo test --workspace`.** Expect: every existing assertion passes unchanged. Any test that needed editing is a behaviour change — stop and split this commit.
- [ ] **Step 5: Commit** `git commit -am "refactor(local-model): one context builder with a stated purpose"`

---

### Task 3: `rerank.rs` — the recipe, the fitting and the reorder, with no weights involved

**Files:**
- Create: `crates/np-core/src/rerank.rs`
- Modify: `crates/np-core/src/lib.rs` (`pub mod rerank;`, ungated, with the one-line reason the embedder seam already gives)
- Test: `crates/np-core/tests/rerank.rs` (new; runs in the default build, no artifact, no database)

**Interfaces:**
- Consumes: nothing outside std.
- Produces:

```rust
pub type TokenId = i32;
pub struct VocabFlags { pub add_bos: bool, pub add_eos: bool, pub add_sep: bool }
pub struct Specials { pub bos: Option<TokenId>, pub eos: Option<TokenId>, pub sep: Option<TokenId> }
pub fn pair_tokens(specials: &Specials, flags: &VocabFlags, query: &[TokenId], doc: &[TokenId]) -> Vec<TokenId>;
pub enum Fit { Whole(Vec<TokenId>), Truncated { tokens: Vec<TokenId>, dropped: usize }, QueryTooLong { tokens: usize, budget: usize } }
pub fn fit_pair(specials: &Specials, flags: &VocabFlags, query: &[TokenId], doc: &[TokenId], budget: usize) -> Fit;
pub fn rank_order(scores: &[f64]) -> Vec<usize>;
pub trait Reranker { fn score(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f64>, RerankError>; }
pub enum RerankError { RuntimeNotBuilt, ArtifactAbsent, ArtifactUnverified(String), CannotScore(String), PairTooLong { tokens: usize, budget: usize }, Runtime(String) }
pub struct NoReranker(pub RerankError);   // implements Reranker by refusing
```

- [ ] **Step 1: Write the failing tests** in `crates/np-core/tests/rerank.rs`. Named, with the assertions spelled:
  - `pair_tokens_follows_the_pinned_fallback_over_the_flag_table` — over all eight flag combinations with `bos = Some(0)`, `eos = sep = Some(2)`, query `[10, 11]`, doc `[20]`: `{bos,eos,sep}` → `[0,10,11,2,2,20,2]`; `{bos,eos,!sep}` → `[0,10,11,2,20,2]`; `add_sep` alone adds one more `2`; a `None` special is skipped even when its flag is set (a vocabulary that has no SEP cannot emit one).
  - `fit_pair_keeps_the_query_and_cuts_the_note_tail` — budget 8, query 3 tokens, doc 20 → `Truncated { tokens: 8, dropped: 15 }`, and the first three tokens of the result are still the query's.
  - `fit_pair_refuses_a_query_that_does_not_fit_on_its_own` — budget 4, query 6 tokens → `QueryTooLong { tokens: 6, budget: 4 }`; nothing is truncated on the query side, ever.
  - `fit_pair_counts_the_specials_it_will_add` — an empty doc still pays BOS/EOS/SEP/EOS; a budget that fits the halves but not the specials returns `Truncated`, never a token list over budget.
  - `rank_order_is_a_total_order_that_breaks_ties_by_retrieval_position` — `[0.1, 0.9, 0.9, f64::NAN]` → `[1, 2, 0, 3]`: `total_cmp` descending, equal scores keep the earlier candidate first, and a NaN cannot decide anything (`rank_order` never trusts `partial_cmp`).
  - `no_reranker_refuses_with_its_cause` — `NoReranker(RerankError::RuntimeNotBuilt).score("q", &["d"])` returns that variant.

- [ ] **Step 2: Run, expect failures** naming the missing module: `cargo test -p np-core --test rerank`.
- [ ] **Step 3: Implement.** `pair_tokens` is the pinned tree's fallback spelled once (`tools/server/server-common.cpp:2010-2032`) — `[BOS?] · query · [EOS?] · [SEP?] · doc · [EOS?]`, flags read from the caller rather than assumed, because other publishers of these weights omit `add_sep_token` and get one EOS. `fit_pair` prices the specials **before** deciding, then cuts the doc's tail at a token boundary and reports how many it dropped. `rank_order` returns a permutation (`out[i]` = index into `scores`).
- [ ] **Step 4: Run, expect pass.** Then `cargo clippy -p np-core --all-targets -- -D warnings` (default features: this module must compile with no runtime).
- [ ] **Step 5: Commit** `git commit -am "feat(rerank): the pair recipe, token fitting and reorder as pure functions"`

---

### Task 4: Rank mode in `local_model` — load it, check what it became, score a pair

**Files:**
- Modify: `crates/np-core/src/local_model.rs` (`Purpose::Rank` arm, `RankModel`, `impl Reranker for RankModel`)
- Test: `crates/np-core/tests/local_rerank.rs` (new, artifact-gated on **`NP_TEST_RERANK_PATH`**, printing `NOT COVERAGE` and returning when unset — copy the pattern in `tests/local_load.rs`)
- Modify: `docs/developing.md` (the gated-suite paragraph gains the third variable and its command line)

**Interfaces:**
- Consumes: `Purpose::Rank { ctx_tokens }` (Task 2), `pair_tokens` / `fit_pair` / `Reranker` / `RerankError` (Task 3), the five FFI entries (Task 1).
- Produces:

```rust
pub const RERANK_CTX_TOKENS: usize = 2048;
pub struct RankFacts { pub window: usize, pub n_cls_out: usize, pub flags: VocabFlags, pub specials: Specials }
pub struct RankModel(/* model + ctx handles, facts */);
impl RankModel { pub fn open(path: &Path) -> Result<RankModel, EmbedError>; pub fn facts(&self) -> RankFacts; }
// impl crate::rerank::Reranker for RankModel
```

- [ ] **Step 1: Write the failing artifact-gated tests** — each one returns early with a printed `NOT COVERAGE` line when `NP_TEST_RERANK_PATH` is unset:
  - `the_pinned_artifact_loads_in_rank_mode`: `RankModel::open` succeeds; `facts().n_cls_out == 1`; `facts().window <= RERANK_CTX_TOKENS`; `facts().flags == VocabFlags { add_bos: true, add_eos: true, add_sep: true }` and `specials.eos == specials.sep` — this is the assertion that pins §4 of the design to bytes rather than to prose.
  - `a_relevant_pair_outranks_an_irrelevant_one_in_both_languages`: five DE and five EN (query, relevant, irrelevant) triples from the Task 10 fixture; every relevant score beats its irrelevant partner.
  - `scores_fall_in_the_cross_encoder_range_and_are_finite`: all scores finite; at least one positive and one negative across the set (a head that silently returned zeros would pass a pure ordering test).
- [ ] **Step 2: Run, expect compile failure** naming `RankModel`.
- [ ] **Step 3: Implement `open`.** Load like `load_with_options` does (same `walk::to_open_path` + `models::access` spelling, same four refusals) through `Purpose::Rank { ctx_tokens: RERANK_CTX_TOKENS }`, then **read back and refuse**: `llama_pooling_type(ctx)` must be `LLAMA_POOLING_TYPE_RANK`; `llama_model_n_cls_out(model)` must be `1` — *"the runtime answered a rank head of N outputs"* on anything else, because reading a wider row than the buffer holds is an over-read, and accepting 1 from `llama-hparams.h:253`'s default would test a default rather than the artifact. Read `llama_vocab_bos/eos/sep` + the three `add_*` flags into `VocabFlags`/`Specials`, treating `LLAMA_TOKEN_NULL` as `None`.
- [ ] **Step 4: Implement `Reranker for RankModel`.** Per doc: tokenise query and doc separately with `add_special = false` (reuse the existing size-then-allocate `tokenize`), `fit_pair`, one `llama_batch_get_one` + `llama_decode`, then copy `n_cls_out` floats from `llama_get_embeddings_seq(ctx, 0)` — **not** `n_embd_out`. Each SAFETY line says which buffer it vouches for and why the width is the checked one. A `Fit::QueryTooLong` becomes `RerankError::PairTooLong`; a non-zero decode rc becomes `RerankError::Runtime("…")` with our own words, never llama.cpp's.
- [ ] **Step 5: Run** `cargo test -p np-core --features local-model --test local_rerank -- --nocapture` twice — once without the env var (expect the `NOT COVERAGE` line and a pass), once with `NP_TEST_RERANK_PATH=<the pinned Q8_0 file>` (expect real passes). Then the feature clippy rows for `local-model` and `local-model,vision`.
- [ ] **Step 6: Commit** `git commit -am "feat(local-model): rank-mode loading and pair scoring through a checked head"`

---

### Task 5: A role on the pin table, and one origin per id

**Files:**
- Modify: `crates/np-core/src/pins.rs` (`Role`, `PinnedModel.role`, `DEFAULT_ORIGINS`, `default_origin`, `role`, the reranker's `RERANKER` row)
- Modify: `crates/np-core/src/fetch.rs` (`resolve_origin(id, configured)`, `Expectation.declaration: Option<&Declaration>`, skip the row write when there is no declaration)
- Test: `crates/np-core/tests/pins.rs`, `crates/np-core/tests/fetch.rs`

**Interfaces:**
- Consumes: existing `PINNED`, `pinned`, `pinned_member`, `fetch_plan`, `total_size`.
- Produces:

```rust
pub enum Role { Embedding, Rerank }           // Copy + Eq + label() -> "embedding" | "rerank"
impl PinnedModel { pub role: Role }            // new field, set on every row
pub fn role(id: &str) -> Option<Role>;         // uniform within an id
pub fn default_origin(id: &str) -> Option<&'static str>;
pub const RERANKER_ID: &str = "bge-reranker-v2-m3";
```

  The new row, with provenance in `read_on` exactly as the other two rows carry theirs: id `bge-reranker-v2-m3`, member `MAIN_FILE`, filename `bge-reranker-v2-m3-Q8_0.gguf`, size `635_674_432`, sha256 `d32a877b7266f5275172458616824ce6728502416672052c2da795da3389201e`, role `Rerank`. **Re-measure both numbers on the build host with `np models hash` before committing, and put where they were read in the commit message, not in the table.**

- [ ] **Step 1: Amend the invariant test first** so the new row cannot be made to pass by lying. In `crates/np-core/tests/pins.rs`, replace `every_fetchable_id_declares_an_identity_and_every_pinned_id_has_one` with two cases and keep their old messages:
  - `an_embedding_id_declares_an_identity_with_a_width` — every pinned member whose role is `Embedding` has a `PINNED_IDENTITIES` row with `dim > 0` ("a zero width admits a vector of nothing").
  - `a_rerank_id_declares_no_identity_at_all` — for every pinned member whose role is `Rerank`, `identity(id).is_none()`. This asserts **absence**, so nobody later "fixes" the reranker's refusal by adding `dim = 0`; the comment says a reranker has no vectors to be the width *of*.
- [ ] **Step 2: Add the origin-table test** — `every_default_origin_has_the_shape_a_transport_joins_against`: https scheme, no `@` in the authority, no trailing `/`, asserted over **every** entry of `DEFAULT_ORIGINS` (this replaces the single-string assertions and keeps their reasons), plus `the_pin_table_still_names_no_host_in_any_row` unchanged.
- [ ] **Step 3: Run both — expect failures** (`Role` missing).
- [ ] **Step 4: Implement `pins.rs`.** Replace `DEFAULT_ORIGIN` with the table plus `default_origin(id)`; keep every existing doc argument (a row says *what*, never *where*; the default exists because an install has no config file yet) and add the new one: two ids, two publishers, so "one origin for everything" became a claim that cannot fetch both. `[models] base_url` keeps its meaning as **one flat directory holding every pinned filename** — filenames are unique across ids, so `{base_url}/{filename}` still needs no id in it; say that where `DEFAULT_ORIGIN`'s text said the old thing.
- [ ] **Step 5: Make fetch role-aware.** `resolve_origin(id, configured)` (callers: `fetch::http`, `commands/models.rs`) falls back to `default_origin(id)`, and a caller holding an id with no default origin refuses rather than dialing a stranger. `Expectation.declaration` becomes `Option<&Declaration>`; where it is `None` the transfer still verifies size and digest and **writes no `models` row**. Keep refusal precedence: `NothingPinned` for an unknown id still answers before `NoIdentity`, and `NoIdentity` now means "this build declares no identity for a pinned id that is not a reranker".
- [ ] **Step 6: Add the fake-transport tests** in `crates/np-core/tests/fetch.rs`: fetching `RERANKER_ID` through the existing test transport writes the artifact file at `<models_dir>/bge-reranker-v2-m3` and leaves `models` empty; a digest mismatch leaves nothing published (the `.part.` name is cleaned); the origin used was the reranker's, not the embedding model's.
- [ ] **Step 7: Run** `cargo test -p np-core --test pins --test fetch --test models`, then `cargo test --workspace`. Expect pass.
- [ ] **Step 8: Commit** `git commit -am "feat(pins): a role per pinned member, and one default origin per id"`

---

### Task 6: Install and list a reranker that has no row

**Files:**
- Modify: `crates/np-core/src/models.rs` (`install`, `install_projector` neighbours, `list`, `Listing`, `ListedModel`)
- Modify: `crates/np-cli/src/commands/models.rs` (argument surface for a rerank id)
- Test: `crates/np-core/tests/models.rs`, `crates/np-cli/tests/models_cli.rs`

**Interfaces:**
- Consumes: `pins::role`, `pins::RERANKER_ID`, `member_artifact_name`.
- Produces: `models::install` refuses a rerank id's embedding-only arguments; `Listing` gains `reranker: Option<ListedReranker>` with `state: RerankerState` = `Absent | PresentVerified | PresentBytesMoved`, answered from the pin plus the file (never from a row).

- [ ] **Step 1: Write the failing tests.** np-core: `installing_a_reranker_verifies_the_bytes_and_writes_no_row` (row count unchanged, artifact present at `<id>`), `a_reranker_id_refuses_the_embedding_declaration_by_name` (`--dim`, `--prefix-doc`, `--prefix-query` each named in the refusal — a reranker answers nothing about vector width), `listing_reports_the_reranker_state_from_disk_and_pin` (all three states, including "present but its size no longer matches the pin"). np-cli: `models_list_json_carries_the_reranker_with_its_role` and `models_install_for_a_reranker_refuses_dim`.
- [ ] **Step 2: Run, expect failures.**
- [ ] **Step 3: Implement.** One role branch in `install`, reusing every existing step (digest format check first, empty-source check, one hash of the source, publish-by-rename). `list` answers the reranker's state without a row and prints its role beside the embedding model's; `np models list --json` grows one key, present-and-null when the build pins no reranker.
- [ ] **Step 4: Run** `cargo test -p np-core --test models && cargo test -p np-cli --test models_cli`, then update `docs/reference-models.md` in the same commit (the reference documents both commands' surfaces, and a stale reference is a wrong reference).
- [ ] **Step 5: Commit** `git commit -am "feat(models): install and list a reranker without giving it a models row"`

---

### Task 7: `[search] rerank` and `rerank_top_n`, refused above the cap

**Files:**
- Modify: `crates/np-core/src/config.rs` (`SearchConfig`, `Default`, one new const)
- Test: `crates/np-core/tests/config.rs`
- Modify: `docs/reference-config.md` (the `[search]` block in both the annotated sample and the key table)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub const MAX_RERANK_CANDIDATES: usize = 64;`, `SearchConfig { mode, rerank: bool, rerank_top_n: usize }` with defaults `Hybrid / false / 5`.

- [ ] **Step 1: Write the failing tests** — `search_section_defaults_to_no_rerank`; `rerank_top_n_above_the_cap_is_refused_naming_it` (`rerank_top_n = 65` refuses, and the sentence names `search.rerank_top_n` and `64`, because F35's rule is that nonsense is refused rather than clamped); `an_unknown_search_key_is_still_refused` (the new keys must not loosen `deny_unknown_fields`); `rerank_keys_read_from_the_file_as_written` (`rerank = true`, `rerank_top_n = 25`).
- [ ] **Step 2: Run, expect failures.**
- [ ] **Step 3: Implement**, hand-writing the error like `[embeddings] threads` does (its comment explains why a derived message cannot name the key). No environment override — every other per-run lever for search is a flag.
- [ ] **Step 4: Run** `cargo test -p np-core --test config` and the no-HOME variant `env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config`.
- [ ] **Step 5: Commit** `git commit -am "feat(config): [search] rerank and rerank_top_n, capped"`

---

### Task 8: The stage inside `search::ranked`, with weight-free tests

**Files:**
- Modify: `crates/np-core/src/search.rs` (`Hit` fields, `Answer`, `RerankInput`, `RerankNote`, the stage in `ranked`, note-text reads)
- Modify: `crates/np-core/src/diagnostics.rs` (four rule tokens + `RULE_TOKENS`)
- Test: `crates/np-core/tests/rerank_stage.rs` (new; a scripted fake `Reranker`, no weights), `crates/np-core/tests/search.rs` (the read-only fence must still pass)

**Interfaces:**
- Consumes: `rerank::{Reranker, rank_order}` (Task 3), `config::MAX_RERANK_CANDIDATES` (Task 7).
- Produces:

  The reorder is `rerank::rank_order` — the function `docs/design-f51-rerank-stage.md` §3 calls `rerank_top`. It takes the scores and returns a permutation so it needs no `Hit` type, which is what lets Task 3 test it with slices.

```rust
pub enum RerankAsk { ByConfig, Explicit }                       // decides warn-vs-refuse
pub struct RerankInput<'a> { pub reranker: &'a mut dyn Reranker, pub ask: RerankAsk, pub top_n: usize }
pub enum RerankNote { ArtifactAbsent, RuntimeNotBuilt, ArtifactUnverified(String), CannotScore(String) }
impl RerankNote { pub fn label(&self) -> String; pub fn token(&self) -> &'static str; pub fn diag_rule(&self) -> &'static str; pub fn report(&self); }
pub enum RerankSkip { Disabled, NoCandidates }                  // the two non-fault reasons
pub struct RerankReport { pub ran: bool, pub skip: Option<RerankSkip>, pub note: Option<RerankNote>, pub candidates: usize }
// Answer gains: pub rerank: RerankReport
// SearchError gains: RerankUnavailable { cause: RerankNote }   // what makes `np search --rerank` exit 1
// Hit gains:    pub rerank_score: Option<f64>, pub rerank_rank_before: Option<usize>, pub rerank_rank_after: Option<usize>
pub fn ranked(conn, mode, model, query_vector, query_text, limit, prefix, rerank: Option<RerankInput>) -> Result<Answer, SearchError>;
```

  Tokens: `rerank-artifact-missing`, `rerank-runtime-not-built`, `rerank-artifact-unverified`, `rerank-cannot-score`. Add all four to `RULE_TOKENS` and to `docs/reference-diagnostics.md` in the same commit — the existing doc test checks both directions. `disabled` and `no-candidates` are **result-document reasons, not rule tokens**: nothing degraded, so there is nothing for an operator to alert on (the same split `RankNote::token` documents for result keys versus diagnostic rules).

- [ ] **Step 1: Write the failing tests** with a scripted `FakeReranker` (returns planted scores, records what it was asked):
  - `rerank_reorders_only_the_first_n_and_the_tail_keeps_retrieval_order` — 8 hits, N=3: the first three are permuted by planted scores; hits 4-8 keep their order and follow.
  - `the_stage_asks_for_min_top_n_hits_and_reports_how_many_it_scored` — 2 hits with N=5 → `candidates: 2`.
  - `a_hit_that_was_not_a_candidate_carries_null_rerank_evidence` — every non-candidate hit has all three new fields `None`, and a candidate has all three `Some` (`rerank_rank_before` is its retrieval position, `rerank_rank_after` its final one).
  - `figures_are_never_candidates_and_keep_their_relative_order` — an image hit at retrieval rank 2 with N=3 stays behind the reranked notes and ahead of the tail's figures.
  - `a_tie_in_rerank_score_breaks_on_path_bytes_then_kind` — extend `search.rs`'s existing total-order argument rather than inventing a new one.
  - `the_stage_needs_no_embeddings_provider` — `ranked(..., model = None, query_vector = None, ...)` in keyword mode still reranks; this is the Review Focus line about a silently dead stage.
  - `note_text_is_title_then_body_and_a_note_without_a_title_loses_no_line` — the fake records the strings it was handed.
  - `a_declined_stage_reports_once_with_its_token` — capture diagnostics with the existing `tests/capture.rs` helper; exactly one event, correct rule token, and (config ask) no error raised.
  - `an_explicit_ask_that_cannot_be_honoured_is_an_error` — same cause, `RerankAsk::Explicit` → `Err(SearchError::RerankUnavailable { .. })`, which is what makes the CLI exit 1.
- [ ] **Step 2: Run, expect failures** (`ranked`'s new arity).
- [ ] **Step 3: Implement.** Order of operations inside `ranked`: fuse and truncate as today at depth `max(limit, top_n)` when a stage is present (the fused list is built to that depth, then truncated to `limit` at the end — one line in the function's doc says why output size does not move), select the first N note hits, read their text with **one read-only `SELECT title, body`** (`fn note_texts(conn, &[(kind, path)])`; bytes are identity, so match on `rel_bytes`, and a hit whose row vanished keeps its position with `None` evidence rather than scoring empty), score, reorder with `rank_order`, splice. Every statement stays a `SELECT` — `searching_writes_nothing` runs this path now, so the read-only fence covers the stage for free.
- [ ] **Step 4: Update every in-tree caller** (`np-cli`'s `search_run` passes `None` for now) and run `cargo test --workspace`; expect the existing search suites green without edits (a changed expectation there means the default path moved — stop).
- [ ] **Step 5: Commit** `git commit -am "feat(search): an optional rerank stage between retrieval and the report"`

---

### Task 9: The CLI and serve doors, including the exit codes

**Files:**
- Modify: `crates/np-cli/src/main.rs` (two flags), `crates/np-cli/src/commands/search.rs` (`search_run`, `SearchRun`, `search_document`, text line), `crates/np-cli/src/serve.rs` (`"rerank"` param), `crates/np-cli/src/commands/rerank.rs` (new: resolve + verify-once)
- Test: `crates/np-cli/tests/cli.rs`, `crates/np-cli/tests/serve.rs`, `crates/np-cli/tests/rerank_cli.rs` (new)

**Interfaces:**
- Consumes: `search::RerankInput` / `RerankAsk` / `RerankNote` (Task 8), `local_model::RankModel` behind `local-model`, `pins::RERANKER_ID` + `pins::pinned_member` for size/digest, `Config::workspace_models_dir`.
- Produces: `--rerank` / `--no-rerank`; serve `"rerank": true|false|null`; the document keys

```jsonc
// per hit (always present):  "rerank_score": null, "rerank_rank_before": null, "rerank_rank_after": null
// beside hits (always present):
"rerank": { "ran": false, "reason": "disabled", "candidates": 0, "model": null }
```

- [ ] **Step 1: Write the failing CLI tests** — `search_json_carries_the_rerank_keys_when_nothing_was_asked` (a full golden document for a default run: this is the Review Focus's frozen-output line, and it must fail if any key appears, disappears or reorders), `rerank_by_config_with_no_artifact_warns_and_exits_zero_naming_the_fetch`, `explicit_rerank_without_the_artifact_refuses_at_exit_one`, `rerank_top_n_from_config_scores_more_than_the_limit_and_prints_the_limit` (the Review Focus depth line: 12 notes, `--limit 3`, `rerank_top_n = 8` → 8 scored, 3 printed), `rerank_runs_with_embeddings_switched_off`, `serve_search_matches_cli_json_for_the_same_query` (parity, including the decline case), `serve_rerank_param_must_be_a_boolean_or_null`, and — cfg-gated per feature shape like the existing pairs — `a_build_without_the_runtime_declines_by_config_and_refuses_by_flag`.
- [ ] **Step 2: Run, expect failures.**
- [ ] **Step 3: Implement the resolution door** in `commands/rerank.rs`: config first (`[search] rerank`), flag over it in both directions; resolve `<models_dir>/bge-reranker-v2-m3`; **verify against the pin once per process** — a `OnceLock` verdict keyed by path + size + mtime, hashing when there is no verdict or the stamp moved, refusing on any mismatch; then `RankModel::open`. In a build without `local-model`, the same function returns `NoReranker(RerankError::RuntimeNotBuilt)`, which is what makes one code path serve both feature shapes. `np search` never fetches: no transport is constructed on this path at all, and a test asserts it by running with `[models] auto_fetch = true` and an absent artifact.
- [ ] **Step 4: Implement the document and the text line.** Text channel: when the stage ran, print the rerank score in the score column (it is what ordered the list) and keep `cos … bm25 …` in brackets beside it; when it declined by config, one line on stderr naming the cause and the fix, exit 0. JSON keeps `score` as the retrieval number and carries `rerank_score` beside it, so a reader can tell which pass produced the order. Serve: same document, refusals as the `-32000` fault carrying the CLI's sentence verbatim.
- [ ] **Step 5: Run** `cargo test -p np-cli --test cli --test serve --test rerank_cli`, then `cargo test -p np-cli --no-default-features` (the no-runtime shapes), then the `local-model` and `local-model,fetch` CLI rows with `CMAKE` set.
- [ ] **Step 6: Commit** `git commit -am "feat(cli): --rerank/--no-rerank, the serve option, and the two exit codes"`

---

### Task 10: A DE/EN fixture vault, labels, and nDCG in the existing harness

**Files:**
- Create: `tests/fixtures/rerank-vault/` (~100 small notes, ~half German), `tests/fixtures/rerank-labels.tsv`
- Modify: `.github/scripts/f31_eval.py` (nDCG@k, a rerank axis, `--self-test` additions)
- Test: the fence's own `--self-test`, plus `crates/np-core/tests/rerank_fixture.rs`

**Interfaces:**
- Consumes: `f31_eval.py`'s existing TSV label format (`query<TAB>path[<TAB>kind]`) and `search()` runner.
- Produces: `ndcg_at(results, k)`, and `--rerank` which passes `"rerank": true` per query; the fixture as data.

- [ ] **Step 1: Write the metric's own test first** — extend `--self-test` with a hand-computed nDCG@10 case (planted gains, one known answer at rank 4 → a number computed on paper and written into the script) plus the mutant that flatters itself (nDCG over answered queries only must **not** rise when recall falls). Run `python3 .github/scripts/f31_eval.py --self-test` and watch it fail before implementing.
- [ ] **Step 2: Implement `ndcg_at` and the rerank axis.** Binary relevance, as F31 does everywhere else; print nDCG@k beside recall@k and MRR per mode, and print the run's shape above the table (binary, modes, k, rerank on/off, label path) so a 0.02 difference over eight queries cannot be read as a finding.
- [ ] **Step 3: Write the fixture.** ~100 notes of two to eight lines each, DE and EN, plausibly vault-shaped (a meeting note, a recipe, a project log, an invoice reminder) with deliberately flat vocabulary overlap between neighbours so retrieval genuinely misses; include 1-3 word queries in both languages and label every one. No private text, no real names, no host paths.
- [ ] **Step 4: Write `crates/np-core/tests/rerank_fixture.rs`** — `every_labelled_answer_is_an_indexed_path` (scan the fixture into a temp DB once; an unresolvable label is a broken fixture, not a bad search) and `the_fixture_vault_scans_clean` (no skipped files). These run with no model, so a rotting fixture fails a gate row rather than someone's measurement.
- [ ] **Step 5: Run** `python3 .github/scripts/f31_eval.py --self-test`, `cargo test -p np-core --test rerank_fixture`, `python3 .github/scripts/check-links.py`.
- [ ] **Step 6: Commit** `git commit -am "test(fixtures): a DE/EN vault and labels the rerank stage can be measured against"`

---

### Task 11: Measure what this host can measure, and write it down

**Files:**
- Modify: `crates/np-core/tests/local_rerank.rs` (timing rows), `.github/scripts/f31_eval.py` (nothing further)
- Create: `docs/measurement-rerank.md`
- Modify: `README.md`, `docs/reference-cli-and-reports.md`, `docs/reference-config.md`, `docs/reference-serve.md`, `docs/reference-models.md`, `THIRD_PARTY_NOTICES.md`, `docs/design-f51-rerank-stage.md` (status line), `todo.md`

**Interfaces:**
- Consumes: everything above.
- Produces: the numbers, and the documents that carry them.

- [ ] **Step 1: Run the stage over the fixture both ways** with a `local-model` release build and both artifacts installed: `python3 .github/scripts/f31_eval.py tests/fixtures/rerank-vault --labels tests/fixtures/rerank-labels.tsv --modes keyword,semantic,hybrid --k 10 [--rerank]`. Record recall@10, MRR and nDCG@10 per mode, rerank off then on.
- [ ] **Step 2: Measure the stage's cost** from the artifact-gated harness in `local_rerank.rs`: p50/p95 over ≥30 scored queries at N ∈ {5, 10, 15, 20, 25} and at 50 candidates, with `RankModel::facts().window` and the context's read-back thread counts printed beside the table; peak RSS of one-shot `np search --rerank` versus without it (`/usr/bin/time -v`); the end-to-end wall time of a fresh process including the pin hash.
- [ ] **Step 3: Compare the quants** through the same seam with `NP_TEST_RERANK_PATH` pointed at Q5_K_M and Q4_K_M: top-1 agreement, top-10 overlap against Q8_0's order, latency, size. Pin nothing new; if a smaller quant is equivalent on those numbers, that is a follow-up card with its own decision, not an edit here.
- [ ] **Step 4: Attempt the reference cross-check** (an isolated `uv` environment, `sentence-transformers`, the HF weights). If it installs and runs, report score deltas **and** order agreement against np's scores, with §4 of the design beside it. If it does not fit, write that row as not measured — an attempted-and-declined row is a fact; a silent gap is not.
- [ ] **Step 5: Write `docs/measurement-rerank.md`** in the style of `docs/measurement-f30-vector-surface.md`: what was run, the exact commands, every figure, and a **"not measured"** list carrying at least — real-vault quality (the human runs it), figures/images under rerank, chunk-level scoring, keyword-only improvement, front matter under rerank, long-note truncation effects, worker-thread scaling, and any Windows or macOS figure. State next to the fixture numbers that they describe the stage, not anyone's vault.
- [ ] **Step 6: Update the live documents** — README (search section: what the stage is, that it is off by default, what it costs), `reference-cli-and-reports.md` (flags, the new hit keys, the `rerank` object, both exit codes), `reference-config.md` (both keys), `reference-serve.md` (`"rerank"` param + `METHODS` note if any changed), `reference-models.md` (Task 6's surface), `THIRD_PARTY_NOTICES.md` (the reranker: Apache-2.0 as the upstream BAAI model and the publishing repository both state, with the same "check the card yourself" posture the existing notices keep). Set the design doc's status to implemented and give `todo.md` its honest next actions.
- [ ] **Step 7: Run the whole gate list** — `.github/scripts/gates.sh` with `CMAKE` set so the `local-model` rows execute rather than print SKIPPED, plus the artifact-gated rows with `NP_TEST_MODEL_PATH`, `NP_TEST_MMPROJ_PATH` and `NP_TEST_RERANK_PATH` set. Then `python3 .github/scripts/verify_catalog_goldens.py` must print `GOLDEN CHECK: pass` with no golden touched. Record in the final report **which rows ran, which skipped and why, and how many `NOT COVERAGE` lines printed** — a green run with 40 vacuous suites is not evidence.
- [ ] **Step 8: Commit** `git commit -am "docs(rerank): what the stage measures on this host, and what it does not"`
