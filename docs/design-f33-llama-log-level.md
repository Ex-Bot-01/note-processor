# F33 — llama.cpp's progress on stderr, at a level the user chose

Design for backlog **F33**. Written 2026-11-05 against `main` at `465f3fa`. Read with
`docs/design-m2-local-embeddings.md` (the FFI seam) and §12's rule that a refusal says what to change.

## 1. What was measured, here, before anything below was written

| # | Fact | How it was measured |
| --- | --- | --- |
| 1 | `np embed` on a workspace with everything present writes **~120 lines / 2,985 bytes** of llama.cpp progress to stderr before its own report. | The F33 card's measurement, re-run in this round's history. |
| 2 | `np search` does it too: the first stderr line in a local-model build is `llama_model_loader: loaded meta data with 46 key-value pairs and 413 tensors …`, ahead of a one-line answer. | Observed while measuring F32's five arms across three builds, 2026-11-05; recorded in the F32 round. |
| 3 | llama.cpp v0.8.0 declares `void llama_log_set(ggml_log_callback, void * user_data)` at `include/llama.h:1676`, and its comment says what happens if you never call it: “If this is not called, or NULL is supplied, **everything** is output on stderr.” | Read from the pinned tree. |
| 4 | The symbol is in the archive we link, not just the header: `nm --defined-only …/out/llama-cpp-llama/src/libllama.a` lists `T llama_log_set`. | Measured 2026-11-05. |
| 5 | The callback signature and levels: `void (*)(enum ggml_log_level, const char * text, void * user_data)`, with `NONE=0 DEBUG=1 INFO=2 WARN=3 ERROR=4 CONT=5` (`ggml/include/ggml.h:2956`, `:653-660`). | Read from the pinned tree. |
| 6 | **Nothing in this repo reads those lines.** The committed probe logs contain zero llama.cpp loader lines — `grep -c "llama_model_loader\|create_tensor\|print_info"` is **0** for both `probes/thread-sweep-short-2026-11-02.log` (190 lines) and `probes/corpus-auto-2026-11-02.log` (82 lines). The sweep script sends each cell's combined output to a per-cell file that is never committed, and parses only its own `CELL`/`SUMMARY`/`LOAD` tags. | Measured 2026-11-05. |
| 7 | `[embeddings]` carries `deny_unknown_fields`, so a new key there must be declared deliberately — an invented one refuses by name. | Read from `config.rs`. |

Finding 6 **corrects the backlog card**, which says “`tests/probes/thread_sweep.sh` and the R6d benchmark work read some of these
lines”. They do not: they capture everything to a scratch file and parse their own tags. So the escape hatch below is not there to
keep a measurement working — it is there for a human diagnosing a load failure. That is a weaker justification than the card claimed,
and the design states the weaker one rather than keeping the stronger.

## 2. What is wrong, and what “fixed” means

`llama_log_set` is never called anywhere in this tree, so llama.cpp uses its default sink and writes every level to stderr. On a
healthy run that is pure noise ahead of a one-line answer, and it is the same on both commands (findings 1–2). The card's own framing
is right: the useful content is the *last* thing on stderr, under our own `np:` prefix.

“Fixed” means: by default, a healthy `np embed` or `np search` writes **no** llama.cpp progress lines; a failing load still shows
what llama.cpp said about why; and a reader who wants the full transcript has one documented way to ask for it.

## 3. Where the decision lives — and why not in np-core

`llama_log_set` sets a **process-global** sink, and `ggml`'s own comment says it applies to “all future logging events”. A library
that changes it changes the host program's stderr. The card already names this as the trap: “a library changing llama.cpp's
process-global stderr is exactly what a downstream user would not want.”

So:

* **np-core** gets the *seam*: an FFI declaration for `llama_log_set`, a callback that filters by level, and one public
  `local_model::set_log_level(Level)` under `local-model`. It installs nothing on its own.
* **np-cli** owns the *policy*: it resolves the level (env → config → default) and calls the seam once, before any command can load
  a model. A library user of `np-core` keeps llama.cpp's default behaviour untouched — which is what “not our process to quiet” means
  in practice.

## 4. The shape

```rust
// np-core, under `local-model`
pub enum LogLevel { Off, Error, Warn, Info, Debug }        // user-facing names
fn forwards(level: i32, threshold: i32) -> bool            // pure, unit-tested
pub fn set_log_level(level: LogLevel)                      // installs the callback once
```

* **`forwards` is pure and host-independent**, in the same house style as F34's `preflight`: it compares a `ggml_log_level` number
  against the threshold and returns whether to write. Every rule lives there, so every rule is testable without a model.
* **`CONT` is treated as `INFO`.** A continuation line carries no level of its own — llama.cpp uses it to extend the line in progress,
  which is how load progress gets drawn. Forwarding a fragment of a line whose start was filtered out would emit half a sentence, so
  `CONT` rides with `INFO`: visible when informational output is wanted, silent otherwise. This is a decision, not a fact from the
  header, and it is written down as one.
* **The callback must never unwind into C.** It runs on whatever thread llama.cpp logs from, and a panic crossing an `extern "C"`
  boundary is undefined behaviour rather than an abort. The body does: gate on `forwards`, then write the bytes to stderr with
  `std::io::Write` on `Stderr::lock()`, ignoring the `Result`. No allocation before the gate, no `unwrap`, and the whole body wrapped in
  `catch_unwind` — a dropped message is a worse outcome than a lost line only if it were load-bearing, and finding 6 says nothing here
  parses these.
* **Level parsing** is a `FromStr` on the config type with exactly five names: `off`, `error`, `warn`, `info`, `debug`. An unknown name
  refuses through the existing `[embeddings]` error path, naming the key and listing the five values — the same shape as
  `threads = "0"` refusing with “permitted values: auto, integer >= 1”.
* **Precedence**: `NP_LLAMA_LOG_LEVEL` (env) → `[embeddings] log_level` (file) → default. Env wins because it is the emergency hatch,
  and this matches how `NP_MODELS_BASE_URL` overrides its config counterpart. An env value that is not one of the five names refuses at
  startup with the same message as the config form — a typo in an escape hatch should not silently mean “default”.

**Default: `warn`.** Forward `WARN` and `ERROR`, filter `DEBUG`/`INFO`/`CONT`. That keeps the failure explanations (llama.cpp reports a
bad file at error level) and drops the load transcript. It is a behaviour change on a healthy run — that is the point of F33 — and it is
reversible per command with one environment variable, no rebuild and no config edit.

## 5. What each surface looks like after

| Situation | Before | After |
| --- | --- | --- |
| `np search` with everything present | ~120 progress lines, then the answer | no llama.cpp lines |
| `np embed`, healthy | ~2,985 bytes of progress, then the report | no llama.cpp lines, same report |
| A load that fails (truncated or corrupt artifact) | progress + error lines + our refusal | error lines + our refusal |
| `[embeddings] log_level = "info"` | key refused as unknown | full transcript, as today |
| `NP_LLAMA_LOG_LEVEL=debug np embed …` | n/a | full transcript for one run |
| A library using `np-core` directly | llama.cpp default sink | **unchanged** — the seam is not installed for you |

## 6. Tests, and what each pins

Written first, red against today's binary where that is reachable.

1. **`forwards` decision table** (np-core unit). One row per level × threshold, including `CONT` at every threshold and a threshold of
   `Off` with `ERROR`. Mutants: treating `CONT` as `ERROR`, and an off-by-one in the comparison (`>=` → `>`), each must fail a row.
2. **Config parsing** (np-core `config` suite). `"info"` parses to `Info`; `"trace"` refuses naming `embeddings.log_level`, the offending
   value and all five permitted names. Mutant: dropping a name from the list fails the refusal assertion.
3. **Env precedence** (np-cli). With `[embeddings] log_level = "off"` set in the file and `NP_LLAMA_LOG_LEVEL=info` in the environment,
   the resolved level is `Info`; with the env var unset it is `Off`. Both directions, because a hatch that cannot win is not a hatch.
4. **The default actually silences** (np-cli, needs both artifact variables). A real local-model run's stderr contains no
   `llama_model_loader:` line; with `NP_LLAMA_LOG_LEVEL=info` it does. Without both variables the case prints `NOT COVERAGE` and passes,
   the house rule — so a default-feature CI row cannot read anything into it. This is the only case that grades the installation itself;
   everything above grades the decision.
5. **A failure still explains itself** (np-core, artifact-gated). The existing truncated-artifact case must keep failing *and*, with the
   level left at its default, its stderr must still contain a line from llama.cpp's error path. If the truncated file produces no error
   line at all on this host, the case says so rather than asserting an absence.

Nothing here asserts a specific llama.cpp message beyond the `llama_model_loader:` prefix we have already observed twice (findings 1–2);
if upstream changes its wording, test 4 would need updating and that is the honest cost of pinning a third party's output at all.

## 7. Risks

| Risk | What stops it |
| --- | --- |
| A panic inside the callback unwinds into C. | The body allocates nothing before the gate, ignores I/O results, and is wrapped in `catch_unwind`. |
| Silencing hides why a load failed. | Default is `warn`, not `off`; §6 test 5 pins that a failing artifact still speaks. |
| A probe or benchmark loses data it parses. | Finding 6: none does. The escape hatch is for humans, and the card's stronger claim is corrected rather than repeated. |
| Two threads race on the global sink. | np-cli calls `set_log_level` once at startup, before any model load; the seam itself uses a `OnceLock` so a second call cannot swap the callback while llama.cpp is mid-write. |
| A downstream library user loses their logs. | np-core installs nothing (§3); only np-cli opts in. |
| `NP_LLAMA_LOG_LEVEL=deubg` silently means default. | An unrecognised env value refuses at startup, naming the five names. |
| Windows/macOS behaviour of the callback differs. | The filtering is pure Rust on numbers; the FFI call is one symbol present in the archive we link (finding 4). No platform-specific path exists, so none can drift — and macOS remains unmeasured here, stated as such. |

## 8. Out of scope

* Any structured logging of our own, or a `--verbose` flag on other subcommands.
* `ggml_log_set` / `ggml_log_get`: one seam is enough while `llama_log_set` routes everything (`llama.h:1676`).
* Changing what llama.cpp prints at error level, or reformatting its lines into ours.
* F28 (whether np-cli gains a `vision` feature) and F35 (unknown top-level config keys), both waiting on product decisions.

## 9. Done when

1. §6's five groups exist, were red first where reachable, and kill the named mutants.
2. A default-features build is unaffected: no new symbols are referenced, and `cargo test -p np-cli --no-default-features` stays green.
3. The full gate list passes in every feature shape it covers, goldens byte-identical, and the branch's CI run is green on **both**
   ubuntu and windows before asking to merge.
4. README's configuration table gains the key, its troubleshooting table gains one row pointing at `NP_LLAMA_LOG_LEVEL`, and the F33
   card carries finding 6 as a correction rather than a repetition of the stronger claim.
5. Every sentence in this document is true of the shipped code — re-measured against the new binary, not against this file.
