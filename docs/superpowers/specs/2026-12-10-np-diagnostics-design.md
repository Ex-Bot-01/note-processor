# Diagnostics: a debug window into `np` for people building on it

**Status: approved 2026-12-10** (operator; the settled points are recorded at the end and supersede anything above that conflicts). Purpose decided by the operator: *debugging while implementing the frontend* —
"when something fails and what fails." Everything below serves that; fleet observability is explicitly out.

## Why

`np` today explains itself in conclusions. A scan says `uncovered 3`; an embed says `embedded 40`; a search says
`vectors: false`. The reasons exist inside the process at the moment they are decided — which note was skipped and by
which rule, which chunk the model refused, why the provider was not built, how long each phase took — and are then summed
into one line and discarded. A person writing a GUI against `np serve` therefore debugs by guessing: they see a spinner
that lasted four seconds or a search that returned nothing, with no way to ask what happened inside.

The audience is the integrator and anyone following them: someone who can set an environment variable and read a file, but
who is not reading np's source and cannot attach a debugger. The deliverable is *ordering, cause, and cost*, at whatever
depth the moment needs.

## Goals

1. **Every failure names its cause and its place.** Not "refused" but which rule, on which input (path/id — never body),
   inside which operation, with the numbers that made it true (`309855456 bytes expected, 12582912 received`).
2. **Granularity the reader chooses**, per area and per depth, without recompiling: one knob for "tell me more about the
   search path only".
3. **Correlation to the request that caused it.** In `np serve`, every diagnostic line carries the JSON-RPC `id` (or says
   `none`) so a client can tie a log line to the response it is holding.
4. **Timing where timing is the question** — how long the walk, the fetch, the batch loop, and one request took.
5. **Safe by construction.** Note bodies never enter a diagnostic; no origin host or URL ever does (the existing rule).
   Paths, ids, rule tokens and counts are what may be logged, and tests enforce both halves.
6. **Free when off.** A disabled level costs a comparison, not a `format!`. Nothing in the chunk loop or the file walk
   allocates unless someone asked for trace.

## Non-goals

- Aggregation, shipping logs anywhere, OpenTelemetry exporters, metrics endpoints.
- Changing any existing output: CLI stdout/stderr bytes, `--json` documents, JSON-RPC results and fault codes all stay as
  pinned by the current suites. Diagnostics are additive and off by default.
- Making `np-core` a logging framework or exposing log levels in the wire protocol's *results*.
- Runtime reconfiguration: level is chosen when the process starts (a serve child is spawned by the client, so restarting
  it is cheap and honest).

## Anti-goals

- A mode that has to be on for normal use because summaries got worse.
- Diagnostics that leak a note's text into a bug report someone pastes publicly.
- A firehose at default level: `np scan ~/Notes` must print exactly what it prints today.
- Detailed logging as a substitute for fixing an unclear refusal — where a sentence is wrong, fix the sentence.

## Constraints from the existing design

- **stdout is protocol** in `np serve` and in every `--json` command. Diagnostics go to stderr or a file. Always.
- **Message hygiene already has teeth**: `EmbedError` and friends are input-free and secret-free by their own rule, and
  §12 rule 8 forbids naming an origin. Both hold for diagnostics; tests keep them honest rather than prose hoping.
- np-core's few existing diagnostics already take the writer as a parameter (`report_refresh(&mut io::stderr(), …)`), so
  there is a seam to migrate onto instead of inventing one.
- No new heavy dependency in `np-core`; anything heavyweight belongs to `np-cli`, which owns the process and its sinks.

## Approaches considered (operator delegated the choice; this is my call)

**A — `tracing` instrumentation in np-core, subscriber owned by np-cli.** Spans around scan / embed / search / fetch /
serve-request; events with typed fields at `trace`/`debug`/`info`; filtering by target and level from env or flag; human
text on stderr, optional JSON lines to a file. Chosen: it gives ordering, nesting and durations — which is exactly what
"when did it fail and what failed" needs — and integrators already know `RUST_LOG`, so the skill they learn here transfers.
Cost: `tracing` in np-core (no subscriber ⇒ near-zero cost), `tracing-subscriber` only in np-cli, plus field discipline.

**B — bespoke `Diag` sink inside np-core.** Zero dependencies, one privacy chokepoint, matches the testable-sink habit.
Rejected for this purpose: it buys control nobody's tools can read, and every viewer (durations, nesting, per-target
filters) is ours to write and theirs to learn.

**C — widen the progress seam into a general event stream (`np/log` notifications).** Cheapest and immediately useful over
the pipe. Rejected as the whole answer: it narrates commands, not internals — no durations, no module filters — though a
slice of it survives below as an opt-in convenience for GUI authors who cannot read the child's stderr.

## The how, decided

**Levels and targets.** Five levels (`trace`, `debug`, `info`, `warn`, `error`) over stable targets named after modules:
`np_core::scan`, `np_core::embed`, `np_core::search`, `np_core::fetch`, `np_core::db`, `np_cli`, `np_cli::serve`. Target
names are a documented set (a test asserts the doc lists every one) so "what can I even ask about" is answerable without
reading source.

**Choosing them.** Superseded below: precedence is default < config file < env < flag. Flag grammar
mirrors the common shape (`-v` = info, `-vv` = debug, `-vvv` = trace; `--log-spec 'np_core::search=trace,np_cli=info'` for
per-target). Config gains two keys: `[log] level` and `[log] file`; a path is opened append-only, created with the same
directory rules as the data dir.

**Formats.** stderr is human text, never machine-parsed, and unchanged in shape when nothing is enabled. A file (or
`--log-format jsonl`) writes one JSON object per line: `ts` (unix ms), `level`, `target`, `msg`, plus fields, plus `request`
when inside a serve request and `workspace` where known. Keys are stable and documented; the human format is allowed to
change, the file format is not.

**Failure rule — the part the operator asked for.** Every refusal, error arm and degraded state gets an event at `warn` or
`error` **with a machine field**: `rule` (a stable token — the same tokens already in results: `provider-disabled`,
`prefix-drift`, `token-budget`, `locked`, `offline`), `what` (path/id/member), `expected`, `actual`. The rule exists so a
frontend can log "why did this fail" without parsing prose, and so np-core's tests can assert the field rather than the
sentence. Where an existing result document already carries the state, the event is emitted at the same place the document
is built — one source of truth, no second story.

**Serve.** The child inherits its level from env/config like any other process; each request span carries the JSON-RPC `id`
(or `none`) and the workspace path. A response that is a fault or a degraded state emits the matching event, so "the GUI
showed nothing" resolves into "search ran in keyword mode because embeddings are switched off". Optional extra, opt-in in
`initialize.params`: mirror `warn`+ diagnostics as `np/diagnostic` notifications — for a client that has the pipe but not
the stderr. Off unless asked, same rule as progress.

**Cost.** Level check before any formatting; inside the chunk loop and the walk, per-item events are `trace` only and must
not build strings or vectors to be discarded. The gate list gets a row measuring the scan of a 4 000-note corpus at
default level against the same run with `NP_LOG=off`, so "free when off" is a number in a log, not an assertion.

**Migration.** The six direct stderr writes move onto the channel; existing user-facing summary lines keep their exact
bytes (they are pinned by tests) and are emitted through the new path at `info`. Nothing else changes output.

## What's left to the builder

- Whether spans live on functions via attribute or explicit guard, per site — whichever keeps the hot loops honest.
- Exact human-format layout (prefix width, colour when a tty), as long as existing pinned lines are untouched.
- Whether `np/diagnostic` ships in the first cut; it is separable and may be deferred without weakening anything above.
- File rotation: none initially; document that the file grows and deleting it is safe.

## Decisions settled at approval (2026-12-10)

A builder check surfaced eight under-specified points. Six were settled here as the author's calls; two went to the operator and are
recorded with their answers. Where this section conflicts with the text above, **this section wins**.

**1. Two knobs, not one — stderr stays exactly as silent as today (operator's answer: as recommended).** The *capture* level defaults
to `warn` and governs what is recorded to a configured file; stderr prints diagnostics only when asked, by `-v`/`-vv`/`-vvv`,
`NP_LOG`, or `[log] level`. Consequence: refusal and degraded-state sentences keep their pinned bytes (`crates/np-cli/tests/cli.rs:2338,
2634, 2868`), no existing case changes, and an offline `np search` prints what it prints now. The migration of the six direct stderr
writes therefore keeps them unconditional at their current bytes — they move onto the channel as `info` *emissions* whose stderr printing
is not gated, so nothing is silenced; the sixth write referred to in review is the llama.cpp gate (`local_model.rs:1544`), which stays on
its own path (point 6).

**2. Precedence follows the house rule (operator's answer: as recommended).** Default < config file < env < flag, matching §12 rule 7 and
the pinned precedent at `loglevel.rs:160` / `cli.rs:3726`; a diagnostic hatch invents no exception. `NP_LOG` is the only variable —
`RUST_LOG` is deliberately *not* honoured, so one binary never carries two variables with opposite orders. Because "the environment did
nothing" is otherwise invisible, `-v` prints one first line naming the effective level **and its source**
(`log level info from config.toml`). A bad value refuses, naming the variable, as `NP_LLAMA_LOG_LEVEL` already does (`cli.rs:2481`).

**3. Bootstrap (author's call).** Two-phase: resolve flag/env before any config read so an early failure has a sink; after the first
`load_config()`, apply `[log] level`/`file` per the order above. A config that refuses (F35 unknown key, unhonourable level) is printed
at `error` to the already-resolved stderr sink — the most common integrator failure must not become the one failure with no diagnostic.
For `np serve`, a bad config still starts the server and answers requests; each request reports the config error as it does today.

**4. The `rule` vocabulary is diagnostics-owned (author's call).** Result documents keep their keys untouched; diagnostics publish one
list of tokens — reusing existing spellings where they exist (`provider-disabled`, `prefix-drift`, `token-budget`, `locked`, `offline`)
and adding the ones scan needs so Goal 1's first sentence is true: skipped notes and unrepresentable directories currently collapse into
counters, so scan gains internal plumbing to know *which* rule fired at each site. A doc-completeness test (precedent:
`crates/np-cli/tests/serve.rs:1404`) asserts every token in code appears in the doc and vice versa.

**5. Targets close at ten names (author's call).** `np_core::scan` (includes walk), `np_core::embed` (includes chunk/change),
`np_core::search`, `np_core::fetch`, `np_core::db` (includes lock, orphans, merges), `np_core::models`, `np_core::config`,
`np_cli`, `np_cli::serve`, `np_cli::serve_channel`. Documented as the whole set and pinned by the same completeness test: a target name
becomes as sticky as a wire key once published.

**6. llama.cpp keeps its own knob (author's call).** `[embeddings] log_level` / `NP_LLAMA_LOG_LEVEL` (F33) stays separate and is
documented as deliberately unrelated to `[log] level`, with the reason: F33 exists so that asking for search tracing does not dump a
hundred-and-twenty lines of model-loader transcript nobody asked for.

**7. File-sink semantics, frozen now because the format is immutable (author's call).** `request` is *omitted* when there is no id —
never `"none"`, never `null` — matching the pipe rule in `serve_channel.rs`. One append handle per process, one JSON object per write.
An unopenable path warns once on stderr and continues: diagnostics are never the reason a command fails, as with every other sink here
(`scan.rs:2166`, `db.rs:912`). JSON goes only to a file; `--log-format jsonl` requires `[log] file` or `--log-file`, because a serve test
already forbids `{` at the start of any stderr line.

**8. Caps (operator's question, answered here).** Yes on stderr, no on file, and the numbers differ because their jobs do.
- **stderr: 1 000 diagnostic lines per process run**, then one final line — `diagnostics truncated after 1000 lines; N more suppressed
  (raise --log-spec or set [log] file)`. Not too big: a normal `-v` run reaches tens of lines, and the runs that blow past 1 000 are
  trace-level loops over thousands of notes, which is precisely when stderr is the wrong place and the file is the right one. The cap is
  checked *before* formatting, so suppressed lines cost a counter, not a `format!`. Pinned user-facing output is never counted or
  suppressed — it is output, not logs.
- **file: no line cap; a byte budget with no rotation.** `[log] max_bytes` defaults to 8 MiB; once exceeded, writing stops and one final
  JSONL record says `{"msg":"log full","bytes":…,"action":"delete the file to continue"}`. No rename-and-continue: on Windows another np
  may hold the handle, and a rotation that fails halfway is a worse story than a log that stops honestly. Eight mebibytes is roughly
  40 000 records — more than any single debugging session reads, and deleting the file stays safe because the next run recreates it.

**9. Cost evidence is a measurement, not a gate row (author's call).** A percentage fence would go red on this memory-tight box for
reasons that are not the code, and the only thousands-of-notes harness (`local_corpus.rs`) is artifact-gated, so as a gate row it would
print `NOT COVERAGE` in every default log. Instead: `docs/measurement-np-diagnostics-cost.md` records a default-level run against
`NP_LOG=off` on the existing corpus, and the *behavioural* half — that nothing is formatted when disabled — is asserted by tests that
count allocations-free paths rather than by wall clock.
