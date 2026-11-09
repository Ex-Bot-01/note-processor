# Diagnostics: levels, targets, rule tokens

`np` separates its channels absolutely: **stdout is protocol** (`--json`, the JSON-RPC lines of `np serve`) and
**diagnostics go to stderr and to a file you asked for**. Nothing here changes a byte of protocol output, and nothing
here appears on stderr unless you ask.

Design: [`docs/superpowers/specs/2026-12-10-np-diagnostics-design.md`](superpowers/specs/2026-12-10-np-diagnostics-design.md).

## Turning it on

| Ask | What you get |
| --- | --- |
| `-v` / `-vv` / `-vvv` | `info` / `debug` / `trace` on stderr |
| `NP_LOG=debug`, `NP_LOG='fetch=trace,warn'` | same levels, with per-target overrides; the file beats the environment, a flag beats both |
| `--log-spec 'warn,np_core::embed=trace'` | the same grammar as `NP_LOG`, on the command line |
| `[log] level = "info"` in `config.toml` | the level for every run, no shell variable to remember |
| `--log-file PATH` or `[log] file = "PATH"` | a JSONL log: one object per line, every level that passed the filter |

A run always says once, on stderr, what it settled on and where that came from —
`np: diagnostics: debug from command line, stderr + /tmp/np.jsonl` — so a log read next week explains itself. A bad
level refuses at startup and names the variable it came from (`NP_LOG`, `--log-spec`, `log.level`).

stderr is capped at **1 000 diagnostic lines per process**. Past that the run continues, the file keeps everything, and
the last stderr line says so and names both ways out. The file has a budget of its own (`[log] max_bytes`, 8 MiB by
default); when it fills it gets one final record saying `log full` with the size and the remedy, and nothing more.

## What may be in a diagnostic

Paths, ids, rule tokens, counts and timings. Never a note's body, never an origin host or URL — the same rule
`docs/design-m2-model-registry.md` §12 already holds the transport to, extended to every event. Tests assert both halves
of that on both channels (`crates/np-core/tests/diagnostics.rs`, `crates/np-cli/tests/diag.rs`).

## Levels

| Level | Meaning |
| --- | --- |
| `error` | Something failed: work was asked for and did not happen. |
| `warn` | A degraded state somebody has to decide about — a refusal, an outage, a stale index. This is the default. |
| `info` | A state change worth reading later: what arrived, what a run decided, what a request cost. |
| `debug` | Per-item decisions that explain a summary: why each skipped file was skipped. |
| `trace` | Per-item routine: every file considered unchanged, every transport poll. Off unless named. |

## Targets

A target names the module whose failure this is, and is what a filter selects: `NP_LOG='fetch=trace,warn'` reads only
the transport. The ten published targets:

```diagnostic-targets
np_core::config
np_core::scan
np_core::embed
np_core::search
np_core::models
np_core::fetch
np_core::lock
np_cli::serve
np_cli::serve_channel
np_cli
```

`np_core::models` and `np_core::fetch` are filters that currently select nothing: their states are reported today under
the target of the command that met them (`np_core::embed`, `np_core::search`), because that is where the decision is
made. They stay in the list so a filter written against the manual does not break when the emitter moves.

## Rule tokens

Every refusal and degraded state carries a machine field, `rule`, beside its sentence — so a frontend can alert on
`token-budget` without parsing prose. The published vocabulary lives in code as
[`np_core::diagnostics::RULE_TOKENS`](../crates/np-core/src/diagnostics.rs); this list is checked against it in both
directions by `the_documented_token_list_matches_the_code` (np-core) and by the documentation test in
`crates/np-cli/tests/diag.rs` (np-cli), so a token can be neither undocumented nor invented.

```diagnostic-tokens
config-refused
provider-disabled
local-model-not-built
artifact-missing
offline
not-produced
token-budget
prefix-drift
locked
no-vectors
model-not-installed
fetch-disabled
artifact-fetched
not-markdown
unreadable
unrepresentable-path
unchanged-content
hidden-behind-unreadable
unresolved-ambiguity
identity-taken
duplicate-rows
```

### What each one says

| Token | Level(s) | Target | `what` names | Means |
| --- | --- | --- | --- | --- |
| `config-refused` | error | `np_core::config` | — | A `config.toml` was refused (**F35**). The command still runs on defaults, and the reason is in the message. |
| `provider-disabled` | warn | `np_core::embed`, `np_cli` | the key or model id | Embeddings are switched off (`embeddings.provider = "none"`), so nothing can turn text into vectors. |
| `model-not-installed` | warn | `np_core::embed`, `np_cli` | the configured id | `[embeddings] model` names an id this workspace has no row for. Distinct from `provider-disabled`: one edits a file, the other installs 309 MB. |
| `local-model-not-built` | warn | `np_core::embed` | the id that stays owed | This binary was built without an inference runtime (**F40**). Nothing on disk is wrong. |
| `artifact-missing` | warn | `np_core::embed`, `np_cli` | the artifact path | A row names a file the disk does not have. |
| `offline` | warn | `np_core::embed`, `np_cli` | the pinned id / workspace | A transfer was allowed, attempted, and met no connection (**F38**). |
| `not-produced` | error | `np_core::embed` | the model id | Work was asked for and a run ended without producing it — `np embed`'s arm (iv). |
| `token-budget` | warn | `np_core::embed` | the chunk id | A sequence went over `embeddings.max_tokens`; `expected` is the budget, `actual` what the chunk needed. Refused rather than truncated. |
| `prefix-drift` | warn | `np_core::embed` | the model id | An installed row's stored prefix is not what this build declares for that id, so its vectors mean something different by construction. |
| `locked` | warn | `np_core::lock` | the workspace path | Another writer holds this workspace and the wait expired (**F44**); `expected` is the wait allowed, `actual` how long it waited. |
| `no-vectors` | warn | `np_core::search` | the model id | The semantic half has nothing to rank under this id. |
| `fetch-disabled` | warn | `np_cli` | the model id | The artifact is missing and fetching was switched off (`[models] auto_fetch = false`, `NP_NO_AUTO_FETCH=1`). |
| `artifact-fetched` | info | `np_cli` | the model id | An artifact arrived during this command. Not a degradation — it says which run made the bytes appear. |
| `not-markdown` | debug | `np_core::scan` | the file path | A discovered file is not markdown: tracked and digested, never parsed into the catalog. |
| `unreadable` | debug | `np_core::scan` | the file or directory path | This scan could not read it (permission denied, vanished mid-scan). Existing rows are kept: unknown is not absent. |
| `unrepresentable-path` | debug | `np_core::scan` | the path as displayed | A name the byte key cannot hold losslessly (F18, Windows) — refused for good, so re-running changes nothing. |
| `unchanged-content` | trace | `np_core::scan` | the file path | Bytes match the stored digest, so nothing was re-parsed. The absence of this line for a file you edited is the diagnosis. |
| `hidden-behind-unreadable` | debug | `np_core::scan` | the stored note path | A live row sits under a directory this scan could not read: hidden from the catalog, **not** deleted. |
| `unresolved-ambiguity` | debug | `np_core::scan` | the stored note path | A row whose only spelling is lossy (F23) that no route accounted for this scan, so it stayed out of the catalog. |
| `identity-taken` | debug | `np_core::scan` | the file path | Another path already holds this note identity, so nothing about this file's note was rewritten. |
| `duplicate-rows` | debug | `np_core::scan` | the file path | More than one note row sits on this key (F13 §7), so none was re-parsed — guessing would rewrite one note with another's contents. |

Two spellings worth knowing:

* **`offline` here, `fetch-offline` in a result document.** `np search --json`'s `reason` names which *step of that
  command* declined; a diagnostic names the outage, so one alert matches `np embed`, `np models fetch` and `np search`.
  Result documents are frozen output and diagnostics publish their own vocabulary, so the mapping lives in
  `RankNote::diag_rule` rather than in either interface.
* **`skipped` is a list, `rule` is per cause.** `ScanReport::skipped` merges several of these states into one array of
  paths, which is right for a summary line and wrong for a diagnosis. The events are where the causes separate.

## The file's shape

One JSON object per line, keys from this frozen set and no others:
`ts`, `level`, `target`, `msg`, `request`, `workspace`, `rule`, `what`, `expected`, `actual`, `elapsed_ms`, `bytes`,
`action`. A record carries only what its event knows, so `rule` is absent on an announce line and `elapsed_ms` appears
only on a request that finished. The final `log full` record carries `bytes` (the size reached) and `action` (the
remedy). Every line is written in one append, so two `np` processes writing the same file never split a record.
