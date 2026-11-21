# F51 — A rerank stage: score the top hits with a cross-encoder, and say out loud when you did not

Status: **proposed 2026-11-21, awaiting review.** Written before the branch had a line of code in it, per `AGENTS.md`.
Prior art: `search.rs` (read-only rule, ordering contract, `RankNote` as the vocabulary for "a half was missing"), F35 (an
unknown config value is refused, never ignored), F45 (hybrid answers and names the missing half; only the mode that cannot
answer refuses), `design-m2-models-install` and `pins.rs` (a pin says *what* bytes are acceptable; the host is config), F31
(no labelled set existed — this round ships one, so §9 can finally state numbers instead of reasoning about recall).

Three rulings came from the human and are recorded rather than invented here: **the stage lives inside `search::ranked`**
(not behind a command of its own — "a new command makes no sense for our use case"), **reusable as a library module is what
matters**, and **a llama.cpp re-pin is permitted only if the embedding model still works afterwards**. This note takes that
last one as a last resort rather than a convenience (§11).

## 1. What is missing, in one sentence

Embedding ranking goes flat on keywords and very short phrases — `np search "Rechnung"` returns fifteen notes whose cosine
sits inside a few thousandths of each other, so the order a reader sees is mostly the tie-break — and there is no second
stage anywhere in the program that can look at a (query, note) pair and say which one is actually about the question.

## 2. The stage

One optional pass between retrieval and the report:

```text
retrieval (keyword | semantic | hybrid, unchanged)
   → take the first N *note* hits            N = [search] rerank_top_n, default 5, capped (§7)
   → score each (query, note text) pair      cross-encoder, one forward pass per pair
   → reorder those N by score; everything else keeps its retrieval order
```

Off by default. `np search` with no flag and no config key produces **byte-identical output to today** — that is a test (§10),
not an aspiration. Three states, one of them in every document:

| State | `rerank.ran` | What the reader sees | Exit |
| --- | --- | --- | --- |
| stage off | `false`, `reason: "disabled"` | nothing extra on stderr | 0 |
| stage ran | `true`, `candidates: k` | rerank score in the score column, evidence per hit | 0 |
| stage declined | `false`, `reason: <cause>` | one line on stderr naming the cause and the fix | 0 or 1 (§8) |

**Why only note hits are candidates.** A figure has no prose to put beside the query — the same fact that keeps images out of
the keyword list entirely. Inventing a stand-in (the text of its linking note, its filename) would score bytes the reader
never asked about. So figures keep their retrieval order and follow the reranked block; §11 lists that as a cost.

## 3. Where the code goes, and what that buys

| Piece | Home | Gate | Reusable without |
| --- | --- | --- | --- |
| `trait Reranker { fn score(&mut self, query, docs) -> Result<Vec<f64>> }`, `NoReranker` | `np-core/src/rerank.rs` (new) | none | a runtime — same shape as `embed::Embedder` + `NoEmbeddingProvider`, which exist ungated for exactly this reason |
| `rerank_top(hits, scores, n) -> Vec<RerankedHit>` — the N-cap, the reorder, the before/after evidence | `rerank.rs`, pure function over slices | none | a database, a model, or a vault |
| `RerankModel::open(artifact)` + pair formatting + truncation | `local_model.rs` (rank mode joins the loader it already has) | `local-model` | — |
| the stage call, the decline reasons, the report fields | `search::ranked` + `Answer`, printed by `np-cli`'s one `SearchRun` | none | anything |

The consequence the human asked about: **another Rust project can use only the reranker.** `RerankModel::open(path)` takes a
file, and `score(query, &[text])` takes strings — no `Db`, no config, no workspace type. `search::ranked` is consumer #1 of
the same two functions, which is also why the ordering rule is testable with zero model bytes (every model-backed suite in
this tree is artifact-gated, so an order that needed weights to check would never be checked in CI).

No `np rerank` command, no new `np serve` method: the human ruled it out for their use case. If a non-Rust consumer ever
needs one, it is a JSON reader over this module and needs no core change.

## 4. Rank mode on the pinned runtime — measured in the tree, not assumed

Everything below was read out of `.vendor/llama.cpp-v0.8.0` (the revision `third_party/llama-cpp.toml` pins) and out of the
artifact's own header, fetched with a ranged GET so the claim is cheap to re-check:

* The runtime knows the architecture (`src/llama-arch.cpp:23` → `LLAMA_ARCH_BERT = "bert"`) and the rerank head
  (`src/llama-graph.cpp:4153-4190`: RANK pools, applies `cls` + bias + `act_cls`, then `cls_out` + bias).
* The artifact carries that head: `cls.weight [1024,1024]`, `cls.bias [1024]`, `cls.output.weight [1024]`,
  `cls.output.bias [1]`, and `bert.classifier.output_labels = ['LABEL_0']` → `n_cls_out = 1` (`src/llama-model.cpp:1564`).
  So one score per pair, at `embd[0]`, exactly as the pinned server reads it (`tools/server/server-context.cpp:2623`).
* **The artifact does not declare pooling.** Its metadata has no `bert.pooling_type`, and `src/llama-hparams.h:398` defaults
  that field to `NONE`. This is why `llama-server` needs `--reranking`: the *caller* declares rank mode
  (`common/arg.cpp:3537`). So unlike the embedding path — which deliberately leaves `pooling_type` unspecified and takes it
  from the file — rank mode **sets `ctx_params.pooling_type = LLAMA_POOLING_TYPE_RANK`** and then reads back what the context
  actually built (`llama_pooling_type`) and refuses anything but RANK. Capability here is our claim, so it is our check.
* Pair formatting follows the pinned tree's own fallback for a model with no rerank template
  (`tools/server/server-common.cpp:2010-2032`): `BOS · query · EOS · SEP · document · EOS`, each half tokenised with
  `add_special = false`. This artifact sets `add_bos`, `add_eos` **and** `add_sep` true with `eos == sep == 2`, so that spells
  `<s> q </s> </s> p </s>` — the two-segment encoding a HuggingFace pair encoder produces for XLM-RoBERTa. The reference
  cross-check in §9 therefore compares scores, not just order, and says how close they were.
* Five declarations join `llama_ffi.rs`, all present in the pinned header: `llama_model_n_cls_out` (`:614`),
  `llama_vocab_bos`/`eos`/`sep` (`:1217-1220`), `llama_vocab_get_add_sep` (`:1227`). **No pin bump**: the pinned version can
  score this model, which is the question the request asked to stop on if the answer was no.

Two refusals in `local_model.rs` stay exactly where they are: `load` still refuses `Pooling::Rank` (the P0-2 over-read — a
RANK buffer is `n_cls_out` floats wide, not `n_embd_out`), and the embedding path keeps its width check against
`n_embd_out`. Rank mode gets its own entry point (`load_rank`) over a shared private helper that owns "build a context with
these numbers", so the embed path loses nothing and gains no new branch. Splitting that helper out is a behaviour-free
refactor and lands as its own commit before any rank code.

**Window: 2048 tokens per pair**, requested as `n_ctx = n_batch = n_ubatch` (the existing three-numbers-one-value rule, which
is what keeps `src/llama-context.cpp`'s encoder assert unreachable), then bounded by the built `n_ubatch` read back. Truncation
is ours: tokenise the query, price the specials, and cut the **note's tail** at a token boundary so the pair fits; if the query
alone does not fit, refuse rather than score a truncated question. Nothing is silently shortened on the query side.

## 5. The bytes: pin, fetch, and where they live

| Field | Value |
| --- | --- |
| id | `bge-reranker-v2-m3` |
| member | `main` (one file; no projector) |
| filename at origin | `bge-reranker-v2-m3-Q8_0.gguf` |
| size | 635,674,432 bytes |
| sha256 | `d32a877b7266f5275172458616824ce6728502416672052c2da795da3389201e` |
| origin | the published GGUF repository's `resolve/main`, as a **per-id** default (§6) |

Both numbers come from the repository's own `SHA256SUMS`/`MANIFEST.json` and are re-measured on this host before the row is
committed, in the same way `pins.rs` records how the embedding pair was read. Q5_K_M (468,390,368 / `8d234ba4…`) and Q4_K_M
(438,374,880 / `efee6434…`) are measured for §9 and **not pinned**: one id, one accepted file.

**No `models` row.** That table's `dim INTEGER NOT NULL` is a claim about vectors (`migrations.rs:160-168`), a reranker has
none, and a row would make the id selectable as `[embeddings] model` — which is how a reranker ends up "configured" for a job
it cannot do. So the pin table is the whole authority for this id, `np models fetch/install` publish the file into the same
models directory under the same naming rule (`models::member_artifact_name`), and `PinnedModel` gains a **role** so the refusal
can be true: `[embeddings] model = "bge-reranker-v2-m3"` is refused as *"that id ranks, it does not embed"* rather than as
"not installed". No migration.

**Verification.** A reranked search hashes the artifact against the pin **once per process** before loading it, and reuses
that verdict for later loads in the same process while size and mtime are unchanged — `np serve` must not hash 636 MB per
request, and a one-shot `np` run must not be able to score with bytes nobody checked. §9 measures what that costs; there is no
unverified load path to fall back to.

**`np search` never fetches this artifact.** Not even in hybrid, not even with `[models] auto_fetch = true`: the request's rule
is that a search must not download it silently, and rerank is opt-in on top of that. Fetching stays `np models fetch --id …` /
`np setup`-shaped work, and `NP_NO_AUTO_FETCH=1` keeps its veto over everything else that does dial out.

## 6. The pins.rs amendment this needs

`pins.rs` has held one origin for every id — `DEFAULT_ORIGIN`, joined as `{base_url}/{filename}`, with the model id
deliberately absent from the path. That was fine while the table pinned one repository's two files. It is not fine with a
second id published somewhere else: fetching it would dial the embedding host and 404.

* `PinnedModel` **still names no host** — that invariant is asserted in `tests/pins.rs` and it is the reason the table is
  reviewable. What changes is that the default stops being a single constant: `DEFAULT_ORIGINS` maps id → origin, and
  `default_origin(id)` replaces the constant. Each entry keeps the three shape rules the current one has (https, no
  credentials in the authority, no trailing `/`), asserted over every entry rather than over one string.
* `[models] base_url` / `NP_MODELS_BASE_URL` keeps its meaning — *one flat directory holding every pinned filename* — and
  now says so: filenames are unique across ids (`embeddinggemma-2-Q8_0.gguf` vs `bge-reranker-v2-m3-Q8_0.gguf`), so a single
  mirror still serves both, and `{base_url}/{filename}` does not need the id in it. Origin privacy is unchanged: no refusal,
  report line or diagnostic carries any of these hosts.
* `fetchable_ids()`, `total_size()` and `fetch_plan()` keep working per id; `np models list` grows a row for the reranker with
  its role beside it, so "which of these can I use as an embedder" is answerable from one command.

## 7. Config, flags, and the cap

| Knob | Default | Notes |
| --- | --- | --- |
| `[search] rerank` | `false` | the standing choice, like `[search] mode` |
| `[search] rerank_top_n` | `5` | hard-capped at **64**, refused above it (F35: refuse, name what was expected) |
| `np search --rerank` / `--no-rerank` | — | flag beats config in both directions, the `--auto-fetch`/`--no-auto-fetch` shape |
| `np serve search` param `"rerank": true \| false` | absent = config | same override, same document back |

64 rather than 5-or-8 because the cap's job is to bound a cost that is linear in N, and §9 has to be able to reproduce its own
latency figure with `np search --rerank-top-n`-shaped settings through config alone: the measurement asks for 25 candidates as
a user setting and 50 as a stress case, and a cap of 8 would make the headline number unreachable outside a test binary.

## 8. When it does not run — the two-sided rule

| Cause | rerank asked by config | rerank asked by `--rerank` |
| --- | --- | --- |
| artifact absent | answer un-reranked, one stderr line, exit 0 | refuse, exit 1, naming `np models fetch <vault> --id bge-reranker-v2-m3` |
| binary built without `local-model` | same as above | refuse, exit 1, naming the feature |
| digest or size disagrees with the pin | answer un-reranked + warn (a corrupt artifact must not silently rank) | refuse, exit 1 |
| runtime refuses to load it / no RANK pooling | answer un-reranked + warn | refuse, exit 1 |

Same posture as `--mode semantic` without vectors: a standing setting degrades and says so; an explicit request that cannot be
honoured is a refusal with the sentence for what to change. `np serve` reports refusals as the `-32000` fault carrying the same
sentence, and declines as a normal answer with `rerank.ran: false`. Each cause gets one diagnostics rule token (§10 names them)
and joins `RULE_TOKENS` plus `docs/reference-diagnostics.md`, which a doc test checks in both directions.

## 9. What will be measured, and what will not

A committed synthetic vault — `tests/fixtures/rerank-vault/`, ~100 small DE and EN notes (no private text), with
`tests/fixtures/rerank-labels.json` holding `{query, language, short, relevant[]}`, 1–3 word queries included on purpose
because they are the case this stage exists for. `scripts/measure-rerank.py --vault <dir> --labels <file>` drives `np scan` and
`np search --json` over **any** vault and label file, so the same numbers can be re-run on a real corpus.

* **Ranking quality**: nDCG@10 with binary relevance, rerank off vs on, over hybrid retrieval; short queries (≤ 3 words) as
  their own subset, because an aggregate over long queries can hide the only case that improves.
* **Speed**: p50/p95 per query at N ∈ {5, 10, 15, 20, 25} and at 50 candidates, measured twice — marginal cost inside one
  process (`np serve`, model loaded once) and end-to-end wall time of a one-shot `np search` (load + verify included). Both are
  honest and they answer different questions.
* **Memory**: peak RSS of the search process with and without the stage, from `/usr/bin/time -v`.
* **Quants**: Q8_0 against Q5_K_M and Q4_K_M on identical pairs — top-1 agreement, top-10 overlap, latency, size. Run through
  the artifact-gated test seam (`NP_TEST_RERANK_PATH`, beside the existing `NP_TEST_MODEL_PATH`), **not** through a production
  escape hatch: nothing in the shipped binary may load bytes the pin does not accept. Q8_0 stays pinned unless the report shows
  a smaller quant is equivalent, and "equivalent" is decided from those numbers, not from the publisher's README.
* **Reference check**: the same pairs through `sentence-transformers` `CrossEncoder`, if it installs on this host within its
  disk budget; agreement reported as score deltas *and* order agreement, with the one known formatting difference stated.

Not measured, and listed in `docs/measurement-rerank.md` as such: figures/images under rerank (never candidates), chunk-level
reranking (the stage scores the note body, not its best chunk), nDCG on a real private vault, behaviour with more than one
model id installed, Windows and macOS numbers (feature builds refuse to compile inference there — build fact only), the effect
of the 2048-token cut on long notes, and whether rerank helps keyword-only retrieval.

## 10. Tests, and what each one can lose

| Test | It fails if |
| --- | --- |
| `disabled_by_default_output_is_unchanged` (CLI stdout+stderr and `--json`, golden document) | any key, order or byte moves when nothing was asked for |
| ordering: a scored pair set reorders; ties break on path bytes then kind then raw evidence | the reorder leaks arrival order, or a tie becomes host-dependent |
| N cap: exactly min(N, hits) pairs are scored; `rerank_top_n = 65` is refused | the cap moved into the scorer, or nonsense config was accepted |
| notes beyond N keep retrieval order and follow the reranked block | the tail got re-sorted by score it never received |
| images are never candidates and keep their relative order | a figure was scored against text it does not have |
| truncation: a note longer than the window is cut at a token boundary, query kept whole, over-long query refused | bytes were cut instead of tokens, or the question was truncated |
| missing artifact × {config, flag} → warn+exit 0 / refuse+exit 1, both naming the fix | the two cases collapsed into one sentence |
| runtime-not-built × {config, flag}, same pair of outcomes | a default build promised to rerank |
| digest mismatch → declines, never scores | unverified bytes reached a forward pass |
| `--json` / `np serve search` parity on the same index and query | one channel grew a key the other does not have |
| rank-mode load against the pinned artifact (artifact-gated) | RANK was assumed instead of read back, or the score row was read at the wrong width |
| five DE + five EN relevant/irrelevant pairs beat their irrelevant partner (artifact-gated) | the pair recipe drifted from §4 |
| `[embeddings] model` naming a reranker id is refused as a role error | "ranks, does not embed" became "not installed" |
| `RULE_TOKENS` ↔ `docs/reference-diagnostics.md` both ways (existing test) | a token shipped undocumented |

## 11. Deliberately not in this round

No `np rerank` command and no new serve method (the human's call, §3). No crate split for reuse — one module boundary is
enough until a second consumer exists to justify moving the pin table and the C++ build wiring. No images as candidates. No
chunk-level scoring. No score threshold that drops hits (this stage may reorder, it may not decide what counts as relevant).
No sigmoid squashing of the score into `[0,1]` — a monotone transform changes no order and would be presentation dressed as
evidence. No re-pin: rank mode works on the pinned tree (§4), and a bump would need byte-identical vectors from an existing
index before anything else was asked of it, which is its own project, not a step inside this one. No migration. No second
pinned quant.

## 12. Left to the implementer

The exact private shape of the shared context helper in `local_model.rs`; where the decline reasons live (a `RerankNote` enum
beside `RankNote` is the obvious guess, and it should follow that type's `label`/`token`/`report` triple); how the measurement
script lays out its output; the fixture notes' wording (synthetic, DE and EN, plausibly vault-shaped rather than lorem ipsum);
whether `np models install --id <reranker>` keeps `--dim` optional or refuses it by name — either is fine as long as it cannot
write a `models` row.
