# F33 — llama.cpp's progress on stderr, at a level the user chose

Design for backlog **F33**. Written 2026-11-05 against `main` at `465f3fa`. Read with
`docs/design-m2-local-embeddings.md` (the FFI seam) and §12's rule that a refusal says what to change.

## 1. What was measured, here, before anything below was written

| # | Fact | How it was measured |
| --- | --- | --- |
| 1 | `np search` on a workspace with everything present writes **145,816 bytes over 1,553 lines** of llama.cpp output to stderr ahead of its one-line answer. Measured twice against the pre-F33 binary: identical both times. | 2026-11-05, `target/debug/np` built `--features local-model`, the install call removed and restored (`cmp` confirmed the source came back byte-identical). |
| 2 | **That number supersedes the card's.** The F33 card says “about 120 lines / 2,985 bytes”; nothing I can run reproduces it — today's same-command measurement is 49× larger. Its provenance is not in this tree, so it is recorded as superseded rather than reconciled, and the design argues from the number that was measured here. | Same two runs. |
| 2b | Of that transcript, **134,125 bytes are DEBUG**: `info` answers with 11,691 bytes / 181 lines, and warn — this design's default — with **424 bytes / 4 lines**, all four of them upstream warnings about the artifact's tokenizer (`src/llama-vocab.cpp:3072`, `LLAMA_LOG_WARN`). | Measured at each level through the hatch, 2026-11-05. |
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

“Fixed” means: by default a healthy run writes **no load transcript** — measured 424 bytes instead of 145,816 — while keeping the lines
upstream itself classes as warnings; a failing load still shows what llama.cpp said about why; and a reader who wants the whole transcript
has one documented way to ask for it.

One wording trap, recorded because the first draft fell into it: “fixed” is **not** “stderr is empty”. At `warn` this artifact still
produces four lines — `load: control-looking token: … was not control-type; this is probably a bug in the model` and two relatives — and
those are llama.cpp reporting something true about the GGUF. A design that promised silence would either have to hide a warning or lie in
its own table.

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
// np-core, always built — `crates/np-core/src/loglevel.rs`
pub enum LogLevel { Off, Error, Warn, Info, Debug }        // user-facing names, one permitted-value list
pub fn resolve_level(env: Option<&str>, file: Option<LogLevel>) -> Result<LogLevel, String>
impl LogLevel { fn forwards(&self, raw: i32) -> bool }     // pure gate over llama.cpp's numbers

// np-core, under `local-model` only
pub fn set_log_level(level: LogLevel)                      // installs the callback once
```

The split is deliberate and was a review finding against an earlier draft that put the enum behind `local-model`: `[embeddings] log_level`
is a field of `EmbeddingsConfig`, np-cli builds np-core with `default-features = false`, so a feature-gated type cannot be a config field —
and the vocabulary would then parse in one build and refuse as an unknown key in another. **The names, the gate and the precedence live in
always-built code, so every CI row grades them; only the FFI declaration, the callback and the installer are gated.**

* **`forwards` is pure and host-independent**, in the same house style as F34's `preflight`: it takes one of llama.cpp's
  `ggml_log_level` numbers and answers whether this threshold writes it. Every rule lives there, so every rule is testable without a model.
* **The encoding is spelled out because the obvious one is wrong.** Comparing a `LogLevel` against a `ggml_log_level` numerically would map
  `Off` to llama.cpp's `NONE = 0`, and `raw >= threshold` would then forward *everything* — the exact inverse of what `Off` means. The
  upstream numbers start at 0 and end at `CONT = 5`, so `Off` cannot be one of them: each name carries its own floor (`Debug` 1, `Info` 2,
  `Warn` 3, `Error` 4) and **`Off` is a floor above every raw level**, not a number borrowed from the header. A raw number outside
  `DEBUG..=ERROR` (including `NONE`) forwards nothing: an unrecognised level is a reason to write nothing, not to guess.
* **`CONT` is treated as `INFO`.** A continuation line carries no level of its own — llama.cpp uses it to extend the line in progress,
  which is how load progress gets drawn. Forwarding a fragment of a line whose start was filtered out would emit half a sentence, so
  `CONT` rides with `INFO`: visible when informational output is wanted, silent otherwise. This is a decision, not a fact from the
  header, and it is written down as one.
* **The callback's three binding rules**, in order. A panic unwinding out of an `extern "C"` body aborts the process — which on this
  command would cost both the exit code and our own `np:` line, so the body is wrapped in `catch_unwind(AssertUnwindSafe(..))`. Then:
  1. **Gate on the integer before dereferencing anything.** v0.8.0's callback signature carries no length (`ggml.h:2956`); ggml hands an
     already-`vsnprintf`'d NUL-terminated buffer, so [`std::ffi::CStr::from_ptr`] is the only way to read it and reading must happen after
     the gate decides the line is wanted at all.
  2. **Write the bytes as they arrived, or drop them.** `to_str()` then `write_all` on `Stderr::lock()`, ignoring the `Result`. Not
     `to_string_lossy`: it allocates to repair bytes that a progress line has no business costing, and a message this program cannot spell
     is not a message worth garbling. Dropping is safe here for finding 6's reason — nothing in this repo parses these lines.
  3. **Never call back into llama.cpp or ggml from the body.** The logger state is global and its own header says these functions are
     “NOT thread safe” (`llama.h:1674`); re-entering it from inside it is how a log line becomes a deadlock.
* **Level parsing** is a `FromStr` on the config type over one constant list of exactly five names: `off`, `error`, `warn`, `info`,
  `debug`. The parser and the permitted-value sentence read that same list, because a refusal that promises a name the parser rejects is
  worse than no message — the mutant “drop `debug` from `FromStr`, leave it in the message” must not survive. An unknown name refuses
  through the existing `[embeddings]` error path, naming the key and listing the five values, in the shape `threads = "0"` already uses.
* **Precedence: the file, then the environment fills what the file left unset, then `warn`.** That is this program's only existing
  config-versus-environment rule (`Config::load` for `data_dir`, shared by every `[models]` key), and an earlier draft of this document
  claimed the opposite while citing that very rule as its precedent. Review caught it: `config.rs` says a value the file spelled out wins
  and the environment “supplies a key the file left unset — **never** an override of a file that names one”, §12 rule 7 says the same about
  `NP_MODELS_BASE_URL`, and that rule was amended by the human on 2026-11-04, so inventing an exception to it is not this round's call.
* **What the hatch therefore is, honestly**: a way to name a level when the file names none — which is every config that exists today,
  since nothing reads this key yet — and *not* a way to outvote a file that already named one. A reader who wrote
  `log_level = "off"` gets silence, which is what they asked for, in the same deal `data_dir` offers. Writing “emergency override” here
  would have been the second time this round a hatch was described as stronger than the code makes it.
* **Validation runs on the env value even when the file wins**, like `[models]`: a name that is not one of the five refuses at startup
  naming the variable and all five permitted names, whether or not the file also set the key. A typo in a hatch should not silently mean
  “default”.
* **An empty value means unset**, because `env_text` already decided what an empty environment value is for this program —
  `NP_MODELS_BASE_URL=` means “not configured”, and §12 rule 7 depends on it. A first draft of this document called an empty level a typo;
  that would have made two variables in one program disagree about the same shape of input, so the existing rule wins and
  `NP_LLAMA_LOG_LEVEL= np embed` gets the default.

**Default: `warn`.** Forward `WARN` and `ERROR`, filter `DEBUG`/`INFO`/`CONT`. That keeps the failure explanations (llama.cpp reports a
bad file at error level) and drops the load transcript. It is a behaviour change on a healthy run — that is the point of F33 — and it is
reversible per command with one environment variable, no rebuild and no config edit.

## 5. What each surface looks like after

Every figure below is a measured stderr size, 2026-11-05, same vault and artifact; two runs each agreed to the byte.

| Situation | stderr | Note |
| --- | --- | --- |
| `np search`, pre-F33 binary | **145,816 B / 1,553 lines** | what F33 is fixing |
| `np search`, default (`warn`) | **424 B / 4 lines** | the four are upstream `LLAMA_LOG_WARN` tokenizer lines |
| `np embed` with three chunks owed, default | **602 B / 6 lines** | same class of warnings; the report on stdout is unchanged |
| `np embed` with nothing owed, default | **0 B** | nothing loads, so nothing logs — F32's arm table still holds |
| `[embeddings] log_level = "info"` | 11,691 B / 181 lines | the transcript, on purpose |
| `NP_LLAMA_LOG_LEVEL=debug np search …` | **145,816 B / 1,553 lines** | byte-identical to the pre-F33 binary for this artifact — the gate writes what it is handed and adds nothing. Stated as a measurement rather than a proof: `forwards` drops raw level 0 and unrecognised numbers, which upstream's default handler would print, and none appear in this transcript |
| A load that fails (truncated artifact) | error lines + our refusal | `src/llama.cpp:373-375` logs at error level, so `warn` keeps it; §6.5 pins it |
| `[embeddings] log_level = "warn"` in the file + `NP_LLAMA_LOG_LEVEL=info` | **424 B / 4 lines** | measured: the file outranks the environment, as §4 requires |
| A library using `np-core` directly | llama.cpp default sink | **unchanged** — the seam is not installed for you |

## 6. Tests, and what each pins

Written first, red against today's binary where that is reachable.

1. **`forwards` decision table** (np-core unit). One row per level × threshold, including `CONT` at every threshold and a threshold of
   `Off` with `ERROR`. Mutants: treating `CONT` as `ERROR`, and an off-by-one in the comparison (`>=` → `>`), each must fail a row.
2. **Config parsing** (np-core `config` suite, always built). All five spellings parse, each to its own variant, and absence means
   “unset” rather than a level; `"trace"`, `"INFO"` and `3` refuse naming `embeddings.log_level` and all five permitted names. Mutant:
   dropping any one name from the parser fails a row, and dropping one from the message fails the refusal.
3. **Precedence** (np-core, always built, against the pure resolver rather than a subprocess). File set → file wins even when the env
   names something else; file unset → env decides; neither → `Warn`. A mistyped env value refuses naming the variable and the five names,
   and an empty one is unset, exactly as `env_text` treats every other environment value here. This lives in np-core because np-cli cannot
   be *read* from a test: it is a
   bin-only crate, so `tests/cli.rs` sees only a child's stdout, stderr and exit code, and nothing prints the resolved level — a finding
   against this document's first draft, which asked for an assertion no harness in this repo can make. What np-cli *can* show is the
   refusal, and §6.6 does.
4. **The default actually silences** (np-cli, gated on `--features local-model` **and** `NP_TEST_MODEL_PATH`, printing `NOT COVERAGE`
   when either is missing — the feature is what decides vacuity here, since without it nothing loads and the case would pass while proving
   nothing). A real run's stderr contains no `llama_model_loader:` line; with `NP_LLAMA_LOG_LEVEL=info` it does. This is the only case that
   grades the installation itself; everything above grades the decision.
5. **A failure still explains itself** (np-cli, same gate). A truncated artifact at the default level must still fail *and* its stderr must
   contain `error loading model`, which the pinned tree guarantees rather than hopes for: the loader's throw is caught and logged at error
   level (`src/llama.cpp:373-375`). An earlier draft put this case in np-core with an escape clause for “if no error line appears” — both
   wrong. np-core cannot capture its own fd 2 (only a child's stderr is capturable, as `tests/cli.rs` already records), and since §3 has
   np-core installing nothing, such a case would grade llama.cpp's default sink and pass identically with F33 absent: unfalsifiable, and it
   would have poisoned every sibling test in its binary through the global sink.
6. **The hatch refuses a typo** (np-cli, every build). `NP_LLAMA_LOG_LEVEL=deubg np scan` exits 1 naming the variable, the value as written
   and all five names — and the same command with `info` succeeds, so the refusal is about spelling rather than about the variable existing.

Nothing here asserts a specific llama.cpp message beyond the `llama_model_loader:` prefix we have already observed twice (findings 1–2);
if upstream changes its wording, test 4 would need updating and that is the honest cost of pinning a third party's output at all.

## 7. Risks

| Risk | What stops it |
| --- | --- |
| A panic inside the callback aborts the process, costing the exit code and our own `np:` line. | The body gates on the integer first, allocates nothing, ignores I/O results, never re-enters llama.cpp, and runs inside `catch_unwind(AssertUnwindSafe(..))`. |
| Silencing hides why a load failed. | Default is `warn`, not `off`; §6 test 5 pins that a failing artifact still speaks. |
| A probe or benchmark loses data it parses. | Finding 6: none does. The escape hatch is for humans, and the card's stronger claim is corrected rather than repeated. |
| Two threads race on the global sink — its own header says these functions are “NOT thread safe” (`llama.h:1674`). | np-cli calls `set_log_level` once, in `embed_provider`, before anything can load; the seam keeps the level in a `OnceLock`, so **a second call is ignored rather than swapping mid-write** — first call wins, silently, which is stated here because “silently” is the part a caller would otherwise assume away. The callback reads that lock with `get().is_some_and(..)`, never `unwrap`. |
| An np-core test calls the seam and changes the sink for its whole binary. | No test does: §6 grades the pure decision in-process and the installation only through a child process, for exactly this reason. |
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
