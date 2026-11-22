# F51 — A rerank stage: score the top hits with a cross-encoder, and say out loud when you did not

Status: **decided 2026-11-21 — one builder-check round folded in, and the two open rulings answered by the human (§7, §9).**
Written before the branch had a line of code in it, per `AGENTS.md`. Prior art: `search.rs` (read-only rule, ordering contract, `RankNote` as the vocabulary for "a
half was missing"), F35 (an unknown config value is refused, never ignored), F45 (hybrid answers and names the missing half;
only the mode that cannot answer refuses), `design-m2-models-install` and `pins.rs` (a pin says *what* bytes are acceptable;
the host is config), **F31** (`design-f31-quality-harness.md`: queries generated from the corpus they search are circular,
so that round shipped no label file — §9 has to live with that ruling rather than walk past it).

Three rulings came from the human and are recorded rather than invented here: **the stage lives inside `search::ranked`**
(not behind a command of its own — "a new command makes no sense for our use case"), **reusable as a library module is what
matters**, and **a llama.cpp re-pin is permitted only if the embedding model still works afterwards**. This note takes that
last one as a last resort rather than a convenience (§11).

The shape was restated by the human when this note went round: **retrieve first (keyword, semantic or hybrid), then rerank that
list** — the stage is a second pass over retrieval's output and never a third retriever.

## 1. What is missing, in one sentence

Embedding ranking goes flat on keywords and very short phrases — `np search "Rechnung"` returns fifteen notes whose cosine sits
inside a few thousandths of each other, so the order a reader sees is mostly the tie-break — and there is no second stage
anywhere in the program that can look at a (query, note) pair and say which one is actually about the question.

## 2. The stage

One optional pass between retrieval and the report:

```text
retrieval (keyword | semantic | hybrid, unchanged), run to depth max(--limit, N)
   → take the first N *note* hits            N = [search] rerank_top_n, default 5, capped (§7)
   → score each (query, note text) pair      cross-encoder, one forward pass per pair
   → reorder those N by score; everything else keeps its retrieval order
   → truncate the final list to --limit, exactly as today
```

Four rules that fall out of that shape and are easy to get wrong:

* **Depth is raised, output size is not.** `--limit` still decides how many hits a caller gets. Retrieval internally asks for
  `max(limit, N)` so that "rerank the top 25" means 25 *scored*, not 25 requested and 10 delivered — without this rule every
  N above the default limit silently scores `--limit` hits.
* **The stage needs no embedding model.** A cross-encoder reads two strings; it never touches a vector. So `[search] rerank =
  true` works on a vault with `[embeddings] provider = "none"`, and it works under `--mode keyword`. Rerank resolution is
  therefore its own door (§8) — piggy-backing on embed-provider resolution would quietly make the stage dead on every vault
  that has not consented to embeddings.
* **Candidates are note hits only.** A figure has no prose to put beside the query — the same fact that keeps images out of the
  keyword list entirely. Inventing a stand-in (its linking note's text, its filename) would score bytes the reader never asked
  about. Figures keep their retrieval order and follow the reranked block; §11 lists that as a cost.
* **Notes outside the first N keep their retrieval order** and follow the reranked block, behind any figures. The stage may
  reorder what it scored and nothing else.

Off by default. `np search` with no flag and no config key produces **byte-identical output to today** — that is a test (§10),
not an aspiration. Four states, one of them in every document:

| State | `rerank.ran` | What the reader sees | Exit |
| --- | --- | --- | --- |
| stage off | `false`, `reason: "disabled"` | nothing extra on stderr | 0 |
| stage ran | `true`, `candidates: k` | `rerank_score` beside the old evidence, ranks before/after | 0 |
| stage declined | `false`, `reason: <cause>` | one line on stderr naming the cause and the fix | 0 or 1 (§8) |
| nothing to score (`k = 0`) | `false`, `reason: "no-candidates"` | the empty-answer sentence, unchanged | 0 |

## 3. Where the code goes, and what that buys

| Piece | Home | Gate | Reusable without |
| --- | --- | --- | --- |
| `trait Reranker { fn score(&mut self, query, docs) -> Result<Vec<f64>> }`, `NoReranker` | `np-core/src/rerank.rs` (new) | none | a runtime — same shape as `embed::Embedder` + `NoEmbeddingProvider`, which exist ungated for exactly this reason |
| `pair_text(flags, query, doc)` (the §4 recipe) and `rerank_top(hits, scores, n)` (the N-cap, the reorder, the before/after evidence) | `rerank.rs`, pure functions | none | a database, a model, or a vault — so the recipe and the order are covered by CI with zero weights present |
| `RerankModel::open(artifact)` + truncation + decode | `local_model.rs` (rank mode joins the loader it already has) | `local-model` | — |
| the stage call, the decline reasons, the report fields | `search::ranked` + `Answer`, printed by `np-cli`'s one `SearchRun` | none | anything |

The consequence the human asked about: **another Rust project can use only the reranker.** `RerankModel::open(path)` takes a
file, and `score(query, &[text])` takes strings — no `Db`, no config, no workspace type. `search::ranked` is consumer #1 of the
same functions.

No `np rerank` command, no new `np serve` method: the human ruled it out for their use case. If a non-Rust consumer ever needs
one, it is a JSON reader over this module and needs no core change.

## 4. Rank mode on the pinned runtime — measured in the tree and in these bytes

Everything below was read out of `.vendor/llama.cpp-v0.8.0` (the revision `third_party/llama-cpp.toml` pins) and out of the
artifact's own metadata, fetched with a ranged GET so the claim is cheap to re-check without moving 636 MB:

* The runtime knows the architecture (`src/llama-arch.cpp:23` → `LLAMA_ARCH_BERT = "bert"`) and the rerank head
  (`src/llama-graph.cpp:4153-4190`: RANK pools, applies `cls` + bias + `act_cls` — default TANH at `src/llama-hparams.h:411`,
  which is what a HuggingFace `classifier.dense` is — then `cls_out` + bias).
* The artifact carries that head: `cls.weight [1024,1024]`, `cls.bias [1024]`, `cls.output.weight [1024]`,
  `cls.output.bias [1]`. Its metadata also declares `bert.classifier.output_labels = ['LABEL_0']`, so
  `hparams.n_cls_out` is assigned from the file (`src/llama-model.cpp:1561-1565`, guarded by a non-empty labels array) rather
  than left at `llama-hparams.h:253`'s default of 1 — and one score per pair is read at `embd[0]`, exactly as the pinned server
  reads it (`tools/server/server-context.cpp:2623`). **The code still reads `n_cls_out` back through `llama_model_n_cls_out`
  and refuses anything but 1**, so the check tests the artifact in front of it instead of confirming a default.
* **The artifact does not declare pooling.** Its metadata has no `bert.pooling_type`, and `src/llama-hparams.h:398` defaults
  that field to `NONE`. That is why `llama-server` needs `--reranking`: the *caller* declares rank mode
  (`common/arg.cpp:3537`). So unlike the embedding path — which deliberately leaves `pooling_type` unspecified and takes it from
  the file — rank mode **sets `ctx_params.pooling_type = LLAMA_POOLING_TYPE_RANK`**, then reads back what the context built
  (`llama_pooling_type`) and refuses anything but RANK. Capability here is our claim, so it is our check.
* **Pair formatting follows the pinned tree's own fallback** for a model with no rerank template
  (`tools/server/server-common.cpp:2010-2032`): `[BOS if add_bos] · query · [EOS if add_eos] · [SEP if add_sep] · document ·
  [EOS if add_eos]`, each half tokenised with `add_special = false`. Every flag is **read at load**, not assumed, because other
  publishers' builds of the same weights differ here: this artifact carries
  `bos_token_id 0, eos_token_id 2, seperator_token_id 2, add_bos_token 1, add_eos_token 1, add_sep_token 1`, so its pairs are
  `<s> q </s> </s> p </s>` — the two-segment encoding a HuggingFace pair encoder produces for XLM-RoBERTa, which is what lets
  §9's reference check compare scores and not only order. An artifact that omitted `add_sep_token` would produce one EOS
  between the halves; that is llama.cpp's own answer for those bytes, and np does not second-guess it (§10 pins the recipe as a
  pure function over the flags, so both spellings are covered without weights).
* Five declarations join `llama_ffi.rs`, all present in the pinned header: `llama_model_n_cls_out` (`:614`),
  `llama_vocab_bos`/`eos`/`sep` (`:1217-1220`), `llama_vocab_get_add_sep` (`:1227`). **No pin bump**: the pinned version can
  score this model, which is the question the request asked to stop on if the answer was no.

Two refusals in `local_model.rs` stay exactly where they are: `load` still refuses `Pooling::Rank` (the P0-2 over-read — a RANK
buffer is `n_cls_out` floats wide, not `n_embd_out`), and the embedding path keeps its width check against `n_embd_out`. Rank
mode gets its own entry point (`load_rank`) over a shared private helper that owns "build a context with these numbers", so the
embed path loses nothing and gains no new branch. Splitting that helper out is behaviour-free and lands as its own commit before
any rank code.

**Window: 2048 tokens per pair**, requested as `n_ctx = n_batch = n_ubatch` (the existing three-numbers-one-value rule, which is
what keeps `src/llama-context.cpp`'s encoder assert unreachable), then bounded by the built `n_ubatch` read back. Truncation is
ours: tokenise the query, price the specials, cut the **note's tail** at a token boundary so the pair fits; if the query alone
does not fit, refuse rather than score a truncated question. Nothing is silently shortened on the query side.

## 5. The bytes: pin, fetch, and where they live

| Field | Value |
| --- | --- |
| id | `bge-reranker-v2-m3` |
| role | `rerank` (§6) — not an embedder, no identity row |
| member | `main` (one file; no projector) |
| published filename | `bge-reranker-v2-m3-Q8_0.gguf` |
| installed filename | `<models_dir>/bge-reranker-v2-m3`, per `models::member_artifact_name` — the primary member takes the id, so the *published* and *installed* names differ by design (`models.rs:64-77`) |
| size | 635,674,432 bytes |
| sha256 | `d32a877b7266f5275172458616824ce6728502416672052c2da795da3389201e` |
| default origin | `https://huggingface.co/smarttasks/bge-reranker-v2-m3-GGUF/resolve/main` |

Both numbers come from that repository's own `SHA256SUMS` and `MANIFEST.json` (the latter also lists the file at 635,674,432
bytes), and are re-measured on this host with `np models hash` before the row is committed — the same posture as the two
existing rows, which record where their numbers were read. Other publishers distribute the same weights under different
quantizer settings and therefore different sizes and digests (`gpustack/…`: 635,676,416; `pqnet/…`: 635,674,304); **the pin is
one repository's bytes**, and a mirror that serves other bytes must be pinned by its own digest through
`np models install --sha256`, not by editing this row. Q5_K_M (468,390,368 / `8d234ba4…`) and Q4_K_M (438,374,880 / `efee6434…`)
are measured for §9 and **not pinned**: one id, one accepted file.

**No `models` row.** That table's `dim INTEGER NOT NULL` is a claim about vectors (`migrations.rs:160-168`), a reranker has
none, and a row would make the id selectable as `[embeddings] model` — which is how a reranker ends up "configured" for a job it
cannot do. No migration either. What that costs, stated so nobody rediscovers it: three existing doors assume pin ⇒ identity ⇒
row, and all three become role-aware (§6).

**Verification.** A reranked search hashes the artifact against the pin **once per process** before loading it and reuses that
verdict for later loads in the same process while size and mtime are unchanged. There is deliberately no DB home for that
verdict: the existing verify-on-change stamp is columns on a `models` row (`local_model.rs:1079`, `db/models.rs:102`), which §5
just refused to write, so this round accepts **one hash per fresh process** and measures it (§9) rather than inventing an
eleventh table to avoid a number it has not read yet. If that measurement says the cost is unacceptable, the fix is a stamp
table as its own card with its own design — not a surprise migration smuggled in here. `np serve` never re-hashes within one
process; a one-shot `np` run can never score with bytes nobody checked.

**`np search` never fetches this artifact.** Not even in hybrid, not even with `[models] auto_fetch = true`: the request's rule
is that a search must not download it silently, and rerank is opt-in on top of that. Fetching stays `np models fetch --id …`,
and `NP_NO_AUTO_FETCH=1` keeps its veto over everything else that dials out.

## 6. Two amendments to `pins.rs` this needs

**(a) One origin per id, not one origin.** The table has held a single `DEFAULT_ORIGIN`, joined as `{base_url}/{filename}`, with
the model id deliberately absent from the path. That was fine while it pinned one repository's two files; with a second id
published elsewhere, fetching it would dial the embedding host and 404.

* `PinnedModel` **still names no host** — that invariant is asserted in `tests/pins.rs` and is why the table is reviewable. The
  default stops being one constant: `DEFAULT_ORIGINS` maps id → origin, `default_origin(id)` replaces the constant, and each
  entry keeps the three shape rules the current one has (https, no credentials in the authority, no trailing `/`), asserted over
  every entry rather than over one string.
* `[models] base_url` / `NP_MODELS_BASE_URL` keeps its meaning — *one flat directory holding every pinned filename* — and now
  says so: filenames are unique across ids (`embeddinggemma-2-Q8_0.gguf` vs `bge-reranker-v2-m3-Q8_0.gguf`), so one mirror still
  serves both and `{base_url}/{filename}` still needs no id in it. Origin privacy is unchanged: no refusal, report line or
  diagnostic carries any of these hosts.

**(b) A role on every pinned member.** `role: Embedding | Rerank`, uniform within an id. This is what lets the refusal be true:
`[embeddings] model = "bge-reranker-v2-m3"` is refused as *"that id ranks, it does not embed"* rather than as "not installed".

Three existing doors assume pin ⇒ identity ⇒ `models` row, and each has to learn the role. **This is the largest work item in
the design**, and it is better said here than found in week one:

| Door | Today | After |
| --- | --- | --- |
| `tests/pins.rs::every_fetchable_id_declares_an_identity_and_every_pinned_id_has_one` (`:235-247`) | every pinned id must have a `PINNED_IDENTITIES` row with `dim > 0` | role-aware, and *asserting absence* for rerank ids: an `Embedding` id declares an identity with `dim > 0`; a `Rerank` id declares none — so nobody "fixes" the new row by adding `dim = 0`, which that test's own message explains is "a vector of nothing" |
| `fetch::fetch_with_progress` (`:481`) | refuses `NoIdentity` before a byte moves, builds a `Declaration` from the identity row | an id with no declared identity and role `Rerank` transfers and verifies bytes without a declaration; the `models` insert happens only where a declaration exists. A mistyped id still hits `NothingPinned` first, so that sentence keeps its precedence |
| `models::install` / `models::list` | install writes a row; list answers "installed" from rows | install takes a role branch and **refuses `--dim`, `--prefix-doc`, `--prefix-query` by name** for a rerank id (they declare nothing about bytes it cannot answer); list answers the reranker's state from pin + disk (`absent` / `present, verified at fetch` / `present, bytes moved`) and prints its role beside it |

## 7. Config, flags, depth, and the cap

| Knob | Default | Notes |
| --- | --- | --- |
| `[search] rerank` | `false` | the standing choice, like `[search] mode` |
| `[search] rerank_top_n` | `5` | hard-capped at **64**, refused above it (F35: refuse, name what was expected) |
| `np search --rerank` / `--no-rerank` | — | flag beats config in both directions, the `--auto-fetch`/`--no-auto-fetch` shape |
| `np serve search` param `"rerank": true \| false` | absent = config | same override, same document back |

**No `[search] limit`, and no new depth key.** Retrieval depth is raised internally to `max(--limit, N)` (§2), so the cap's job
is only to bound a cost that is linear in N. 64 rather than 8 because §9 has to reproduce its own stress row with settings a
real user can write: it scores 25 candidates as a config value and 50 as a stress case, and a cap of 8 would make the headline
number reachable only inside a test binary.

**Worker threads: llama.cpp's default this round, read back and printed beside every §9 figure.** `[embeddings] threads` belongs
to the embedder; pointing it at the reranker would make a consent-gated embeddings key silently govern a stage that runs with
embeddings switched off, and inventing `[search] rerank_threads` would ship a knob nobody measured. `LoadedModel::threads()`
already reports what the context was told, so the latency table states its thread count instead of implying one.

**Which bytes are scored — the human's ruling: `notes.title` + a newline + `notes.body`,** front matter excluded. "Yes
`notes.title + notes.body` is a good idea." That is the text the keyword half ranks (`notes_fts` over title and body), it is what
a reader recognises as the note, and for 1–3 word queries the title is likely decisive. The alternative — the whole decoded file
including front matter, which is what the chunker embedded (`chunk.rs:31-37`) — would have aligned rerank with the semantic half
and pre-empted **D2** ("is front matter part of the embedded text?"), which stays open in `todo.md`. A note with no title scores
its body alone, and that pair is not padded with a blank line: an empty title is a fact about the note, not a missing field.

## 8. When it does not run — the two-sided rule

| Cause | rerank asked by config | rerank asked by `--rerank` |
| --- | --- | --- |
| artifact absent | answer un-reranked, one stderr line, exit 0 | refuse, exit 1, naming `np models fetch <vault> --id bge-reranker-v2-m3` |
| binary built without `local-model` | same as above | refuse, exit 1, naming the feature |
| digest or size disagrees with the pin | answer un-reranked + warn (a corrupt artifact must not silently rank) | refuse, exit 1 |
| runtime refuses to load it / context did not become RANK / `n_cls_out ≠ 1` | answer un-reranked + warn | refuse, exit 1 |
| no hits to score | `no-candidates`, exit 0 | same — there is nothing to refuse about |

Two non-causes worth naming, because both look like causes: **`[embeddings] provider = "none"` is not one** (§2: the stage needs
no embedder), and a workspace that was never scanned is not one either (it produces no hits, which is the row above).

Same posture as `--mode semantic` without vectors: a standing setting degrades and says so; an explicit request that cannot be
honoured is a refusal with the sentence for what to change. `np serve` reports refusals as the `-32000` fault carrying the same
sentence, and declines as a normal answer with `rerank.ran: false`. Each cause gets one diagnostics rule token and joins
`RULE_TOKENS` plus `docs/reference-diagnostics.md`, whose doc test checks the list in both directions.

## 9. What will be measured, and what will not

**Ranking quality — whose measurement, decided by the human: they run it.** "We first implement it and then I will test it by
myself on my pc." That settles the collision with F31 (`design-f31-quality-harness.md:40-48` rules that queries generated from
the corpus they search are "circular by construction" and shipped no label file for that reason; `todo.md` item 6 keeps F31 open
until ~20 queries are labelled from a real vault): **this round makes the measurement possible and runs the parts of it that need
no private data; it does not claim search got better for anybody.** Concretely:

* Ship `tests/fixtures/rerank-vault/` — ~100 small synthetic DE and EN notes (no private text) with hand-authored labels,
  1–3 word queries included on purpose because they are the case the stage exists for.
* On that fixture, report what is actually decidable there: does the cross-encoder put the planted answer first in both languages,
  and do the quants agree on the order. nDCG@10 goes beside it **labelled as a statement about the stage**, never about a real
  vault — the round's measurement document (docs/measurement-rerank.md, written by this round) prints that sentence beside the
  number, so the table cannot be quoted without it.
* Add nDCG@k and a rerank axis to `.github/scripts/f31_eval.py` rather than writing a second harness with a second label format:
  its metric arithmetic is self-tested (`--self-test`, a gate row), its TSV labels are the format F31 will close on, and it already
  takes any workspace plus any label file — which is the thing that lets the human re-run all of this on their own machine against
  their own notes. When their ~20 labels land, this round's numbers become comparable to them for free.

**Speed.** p50/p95 per query at N ∈ {5, 10, 15, 20, 25} and at 50 candidates. Two columns, because they answer different
questions and today neither is measurable the obvious way: *marginal* cost with the artifact loaded once, from an artifact-gated
harness in `np-core/tests/` that loads a rank model and scores M pairs (the existing `NP_TEST_MODEL_PATH` convention, plus
`NP_TEST_RERANK_PATH`); and *end-to-end* wall time of one-shot `np search --rerank`, which includes the §5 hash and the load.
**Not through `np serve`**: each request rebuilds its provider (`commands/search.rs:170`) and even reopens the database, so a
serve-based "marginal" figure would measure load-per-request under a different name — holding weights across requests is a
behaviour change to serve and is out of scope (§11). Peak RSS with and without the stage, from `/usr/bin/time -v`.

**Quants.** Q8_0 against Q5_K_M and Q4_K_M on identical pairs — top-1 agreement, top-10 overlap, latency, size — through the same
artifact-gated seam, **not** through a production escape hatch: nothing in the shipped binary may load bytes the pin does not
accept. Q8_0 stays pinned unless the report shows a smaller quant is equivalent on those numbers, and "equivalent" is decided
from them, not from the publisher's README.

**Reference check.** The same pairs through `sentence-transformers` `CrossEncoder`, if it installs within this host's disk budget
(no system pip; an isolated `uv` environment, ~2.3 GB of HF weights — approved to attempt, and if it does not fit that row is
reported as not measured). Agreement reported as score deltas *and* order agreement, with the formatting note from §4 beside it:
our recipe reproduces HuggingFace's two-segment encoding for these bytes, so a systematic offset would be a finding, not noise.

Not measured, and listed in this round's measurement document as such: figures/images under rerank (never candidates), chunk-level
reranking (the stage scores whole-note text, not the best chunk), nDCG on a real private vault (until F31's labels exist),
whether rerank improves keyword-only retrieval (it *runs* there; that is not a claim it helps), the effect of the 2048-token cut
on long notes, front matter under rerank (§7), rerank worker-thread scaling (one thread count, stated), and any Windows or macOS
figure — **no number**, though a `windows-gnu` feature build is a real target (`build.rs:288`) and this round has to keep it
compiling; macOS refuses inference outright.

## 10. Tests, and what each one can lose

| Test | It fails if |
| --- | --- |
| `disabled_by_default_output_is_unchanged` (CLI stdout+stderr and `--json`, golden document) | any key, order or byte moves when nothing was asked for |
| `pair_text` over the flag table (add_bos × add_eos × add_sep), no weights | the recipe drifted from §4, or a flag was assumed instead of read |
| ordering: a scored pair set reorders; ties break on path bytes then kind then raw evidence | the reorder leaks arrival order, or a tie became host-dependent |
| N cap: exactly min(N, hits) pairs scored; `rerank_top_n = 65` refused; `--limit 3` with N=25 scores 25 and returns 3 | the cap moved into the scorer, nonsense config was accepted, or depth stopped being raised |
| notes beyond N keep retrieval order and follow the reranked block | the tail got re-sorted by a score it never received |
| images are never candidates and keep their relative order | a figure was scored against text it does not have |
| truncation: over-long note cut at a token boundary, query kept whole, over-long query refused | bytes were cut instead of tokens, or the question was truncated |
| missing artifact × {config, flag} → warn+exit 0 / refuse+exit 1, both naming the fix | the two cases collapsed into one sentence |
| runtime-not-built × {config, flag}, same pair of outcomes | a default build promised to rerank |
| digest mismatch → declines, never scores | unverified bytes reached a forward pass |
| rerank runs with `[embeddings] provider = "none"` and under `--mode keyword` | the stage got wired to embed-provider resolution after all |
| `[embeddings] model` naming a reranker id refused as a role error; pins test asserts a rerank id has **no** identity row | "ranks, does not embed" became "not installed", or someone gave it `dim = 0` |
| `--json` / `np serve search` parity on the same index and query | one channel grew a key the other does not have |
| rank-mode load against the pinned artifact (artifact-gated) | RANK was assumed instead of read back, or `n_cls_out` was assumed instead of checked |
| five DE + five EN relevant/irrelevant pairs beat their irrelevant partner (artifact-gated) | the pair recipe drifted from §4 |
| `RULE_TOKENS` ↔ `docs/reference-diagnostics.md` both ways (existing test) | a token shipped undocumented |

## 11. Deliberately not in this round

No `np rerank` command and no new serve method (the human's call, §3). No crate split for reuse — one module boundary is enough
until a second consumer exists to justify moving the pin table and the C++ build wiring. No images as candidates. No chunk-level
scoring. No score threshold that drops hits (this stage may reorder; it may not decide what counts as relevant). No sigmoid
squashing of the score into `[0,1]` — a monotone transform changes no order and would be presentation dressed as evidence. No
holding model weights across `np serve` requests (a behaviour change to a long-running command, with its own lifetime questions;
§9 measures marginal cost without it). No rerank worker-thread key (§7). No re-pin: rank mode works on the pinned tree (§4), and a
bump would need byte-identical vectors from an existing index before anything else was asked of it — its own project, not a step
inside this one. No migration. No second pinned quant.

## 12. Left to the implementer

The exact private shape of the shared context helper in `local_model.rs`; where the decline reasons live (a `RerankNote` beside
`RankNote` is the obvious guess, and it should follow that type's `label`/`token`/`report` triple); how the measurement harness
and `f31_eval.py` share their output layout; the fixture notes' wording (synthetic, DE and EN, plausibly vault-shaped rather than
lorem ipsum); whether the verify-once-per-process verdict lives in a `OnceLock` keyed by path or in the `RerankModel` type — as
long as there is no path from a fresh process to an unverified load.
