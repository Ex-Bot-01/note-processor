# Worker threads for the local embedder: the rule, its price, and what it does not claim

Status: **accepted and implemented (2026-11-02)** — the human chose option (c) after D13's measurement. Branch
`m23/threads-default`, stacked on `m22/thread-sweep` (the loader seam this needs). Every number here traces to a committed
log (`docs/design-m2-local-embeddings.md` §14's "D13: the worker count, measured").

As shipped: `[embeddings] threads` is `config::ThreadCount` with the rule in `ThreadCount::resolve`;
`LazyLocalEmbedder` resolves it once against `available_parallelism()` and hands the result to `load_with_options`;
`np embed` reports it as `threads 12 of 16 available (auto)` and `"workers": {"count", "available", "configured"}` in
JSON, `null` when no local model ran. The cases that hold it up are `auto_workers_follow_the_machine_within_measured_bounds`
(the §2 table, including `available = 0`), `an_impossible_worker_count_is_refused_naming_the_key`,
`the_worker_plan_is_the_config_resolved_against_the_machine_once`, and — artifact-gated —
`the_configured_count_is_the_count_the_context_runs_with`, which is the one that catches a report saying twelve while the
loader uses four.

## 1. Why there is a rule at all

`llama.cpp`'s default worker count is `GGML_DEFAULT_N_THREADS`, a literal `4` in `ggml/include/ggml.h:232` annotated
upstream with `// TODO: better default`. Nothing in the chain reads the machine. On the host this project is developed
on, using all sixteen workers instead of four measures **+190 %** on a realistic 375-token chunk and **+260 %** on a
near-window one, for **+6.6 MiB** of peak RSS and no measurable memory cost at the long length. Leaving that in a
literal is not conservatism; it is an unexamined default that every user pays.

Three options were on the table with those numbers attached:

| option | what it means | measured here | verdict |
| --- | --- | --- | --- |
| (a) | keep four, add a config key only | unchanged | safe, and leaves ~3× unused by everyone |
| (b) | use all of `available_parallelism()` | 459.6 tok/s (2.90×), 1,391 % CPU | fastest; takes the whole machine for the run |
| (c) | **a floored fraction** — accepted | 12 workers → 395.1 tok/s (**2.49×**), four CPUs left | chosen |

(c) buys 86 % of the available gain and leaves a quarter of the machine to whatever else the user is running. That is a
judgement about other people's machines, not a measurement — but the *shape* of it is measured, in both directions:

* **A fraction with no floor is a regression aimed at small hardware.** 80 % of a two-core laptop is one worker, and one
  worker measures **49.1 tok/s against today's 158.7** — a third of the current speed on exactly the machines that can
  least afford it. So the floor exists, and it is `min(available, 4)`: never more workers than exist, and never fewer
  than four where four exist.
* **A fraction with no ceiling is worse than the default.** Thirty-two workers on a sixteen-CPU host ran at **91.4 tok/s
  while burning 1,553 % CPU**, with a 24 % within-process spread. So `auto` never exceeds `available_parallelism()` —
  and an explicit count above it is allowed but no longer the program's idea of good sense (§5).

## 2. The rule

```text
auto     →  min(available, max(min(available, 4), floor(0.8 × available)))
fixed n  →  n                       (bounded only by the loader's refusal, §5)
```

Resolved values, so a reader can check their own machine without running anything:

| `available_parallelism()` | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 10 | 12 | 14 | **16** | 20 | 24 | 32 | 64 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| workers chosen | 1 | 2 | 3 | 4 | 4 | 4 | 5 | 6 | 8 | 9 | 11 | **12** | 16 | 19 | 25 | 51 |

Three properties worth naming, because each is a refusal of something simpler:

* **Small machines never lose.** For `available ≤ 4` the rule returns `available`, which is ≥ today's four-or-oversubscribed
  behaviour and never below it. A two-core laptop gets 2 (93.0 tok/s measured) instead of today's 4 threads on 2 CPUs.
* **The fraction only bites once there is something to leave behind.** At 5 and 6 CPUs the answer is still 4 — leaving
  "20 %" of six cores means one core, which is where headroom stops being headroom and becomes a rounding error.
* **`available_parallelism()` is the right denominator, not `nproc`.** Measured: it reports 16 unrestricted, **4** under
  `taskset -c 0-3`, **8** under `-c 0-7`, at the same throughput when four workers fit inside the cpuset
  (`thread-sweep-affinity-2026-11-02.log`). A container's CPU limit therefore changes the answer in the right direction,
  and a run inside a cpuset does not oversubscribe itself.

## 3. Configuration surface

`[embeddings] threads = "auto" | <n>`, **defaulting to `"auto"`** — so this change alters behaviour by default, which is
the point of choosing (c) over (a), and the escape hatch is one line in the same section as `provider`:

```toml
[embeddings]
provider = "local-model"
threads  = "auto"     # or an explicit count: threads = 2
```

* **The type carries the refusal.** `threads` is an untagged enum — the string `"auto"` or a positive integer. `0` is
  refused by name, because "zero workers" is not a thing to ask llama.cpp for and defaulting it would be exactly the
  silent fallback this project's F-series exists to eliminate; anything that is neither (`threads = "most"`) is refused
  by the same key-name message. `[embeddings]` already has `deny_unknown_fields`, so `thread = 8` (a typo) fails loudly
  rather than loading as `"auto"` in silence.
* **Why this section and not a new `[local_model]`.** A section named after one provider would sit silently irrelevant
  for the other two, and `[embeddings]` is where `provider`, `dim` and `max_tokens` already live — the knob belongs next
  to the thing it tunes. It is read only when `provider = "local-model"`, like `dim` and `model` before it; that this is
  not *new* looseness is true, and §7 lists what it implies.
* **No environment variable.** `NP_MODELS_BASE_URL` exists because a fetch origin is deployment data an operator may not
  control; a thread count is a property of the machine the config file already describes, and a second way to set one
  number is two numbers to test. The measurement harness passes counts directly to the loader instead.

## 4. Plumbing, and what `np embed` says about it

The resolved count travels with the provider: `LazyLocalEmbedder::new` takes the config value, resolves it against
`available_parallelism()` **once at construction**, and hands `LoadOptions { threads }` to the loader on first use
(§14's laziness is untouched — nothing loads a model just to ask this question).

`np embed` then reports it, in both output shapes:

```text
threads     12 of 16 available (auto)
```

```json
{"workers": {"count": 12, "available": 16, "configured": "auto"}, "...": "..."}
```

The field is `null` when no local model was involved — a run with `provider = "none"`, or §6's *no model configured*
arm — because the truthful default for "nothing decided about workers" is not a number. This exists for one reason: when
an embedding run is slow, "how many CPUs did it use" should be in the report rather than guessed from a rebuild with a
different constant. It follows `artifact_check`'s precedent (D12): set by the caller that knows, carried on the report so
the text summary and `--json` cannot disagree.

## 5. An explicit count is honoured, including a bad one

`threads = 32` on a 16-CPU machine means 32 workers. The program does not second-guess an operator who may know something
it cannot see — the next measurement's cpuset, a machine with more cores than this one, a benchmark that wants exactly
that cell. What it does instead:

* **`auto` never oversubscribes**, and the docs say why in measured terms (§1).
* **The loader refuses what C cannot be told.** A count above `i32::MAX` is an `EmbedError::Unavailable` naming the
  number, before a context exists. That is not hypothetical: the first version of this seam clamped such a value to
  `i32::MAX`, llama.cpp echoed it back, and the process died in a C++ exception (`SIGABRT`) — see `m22/thread-sweep`.
* **Config refuses what would overflow at load**, so the common case fails with a key name instead of a runtime error.

## 6. What this rule does not claim

Stated plainly because the numbers above are tempting to over-read:

* **Nothing above 16 workers has been measured anywhere.** The rule returns 51 on a 64-core machine; that is arithmetic,
  not evidence. Scaling was monotone up to exactly `available_parallelism()` on one 16-CPU host and catastrophic at
  double it — between 16 and 32 nothing was probed, and a bigger box may behave differently for reasons this artifact and
  this ggml build cannot show.
* **The whole command was not re-measured.** §14's corpus figures (16,161 chunks/hour, the evening-rule arithmetic) are a
  **four-worker** measurement. The forward pass is ~2.5× faster at 12 workers; what that does to a full `np embed` run —
  which also reads notes, chunks text, hashes digests and writes SQLite — is unmeasured until the corpus case is re-run
  under this default. Until then any "N× faster end to end" sentence is extrapolation and must be labelled as such.
* **cgroup quota handling is Rust's documented behaviour, not a measurement here.** Affinity was tested (§2); no
  container was available to test a CPU *quota*. A machine that limits CPUs by quota may therefore get a larger number
  than it should until someone measures it.
* **Bit-identical vectors across worker counts is an observation about this llama.cpp**, measured at two lengths on one
  artifact (`max |Δ| = 0` in both sweep logs). A re-pin re-runs `crates/np-core/tests/probes/thread_sweep.sh`, which now
  appends that table by itself, before anyone repeats the claim.
* **Being a good neighbour is not measurable from this side.** CPU utilisation is (1,391 % at sixteen workers); whether
  an interactive session suffers is a property of what else runs, and no number here settles it. That is the residual
  risk option (c) accepts in exchange for leaving a quarter of the machine free.

## 7. Tests this must arrive with

1. **The rule itself, pure and always compiled** (`config.rs`, so it runs on every gate row including the no-`HOME` one):
   resolved workers for `available` ∈ {0…4, 5, 6, 7, 8, 12, 16, 20, 32, 64}, pinning the table in §2 — including
   `available = 0`, which must not yield zero.
2. **Config parsing**: absent → `auto`; `"auto"`; an integer; `0` refused naming `embeddings.threads`; a non-integer
   string refused naming it; unknown key refused; file-wins-over-env ordering is *not* a thing here (§3, no env var).
3. **The provider carries the number**: `LazyLocalEmbedder::threads()` reports the resolved count and the denominator,
   artifact-free (construction refuses nothing new, so this runs on a normal gate), and `null` for a run with no local
   model is pinned at the CLI layer.
4. **The number reaches llama.cpp**, artifact-gated: load through the lazy provider at an explicit count and assert the
   context's readback equals it — the case that already exists for the raw loader (`asking_for_a_worker_count_...`), now
   reached through the config path so a plumbing break is caught rather than inferred.
5. **Report shape**: `np embed --json` carries `workers` with all three fields for a local-model run (feature + artifact
   gated, alongside the existing `real_model` cases) and `"workers": null` otherwise — because a key that silently goes
   missing is the failure mode this project keeps hitting.
