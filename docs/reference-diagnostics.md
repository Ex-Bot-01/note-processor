# Diagnostics: levels, targets, rule tokens

`np` separates its channels absolutely: **stdout is protocol** (`--json`, the JSON-RPC lines of `np serve`) and
**diagnostics go to stderr and to a file you asked for**. Nothing here changes a byte of protocol output, and nothing
appears on stderr unless you ask for it. Every transcript below was copied from the shipped binary — the method is at the
end.

Design: [`docs/superpowers/specs/2026-12-10-np-diagnostics-design.md`](superpowers/specs/2026-12-10-np-diagnostics-design.md).
Cost of having the machinery at all: [`docs/measurement-np-diagnostics-cost.md`](measurement-np-diagnostics-cost.md).

## Turning it on

| Ask                                        | What you get                                                        |
| ------------------------------------------ | ------------------------------------------------------------------- |
| `-v` / `-vv` / `-vvv`                      | `info` / `debug` / `trace` on stderr                                 |
| `NP_LOG=debug`, `NP_LOG='warn,np_core::fetch=trace'` | the level, plus per-target overrides; no shell flag to remember |
| `--log-spec 'warn,np_core::embed=trace'`   | the same grammar as `NP_LOG`, on this command line                   |
| `[log] level = "info"` in `config.toml`    | the level for every run, from the file                               |
| `--log-file PATH` or `[log] file = "PATH"` | a JSONL log: one object per line, every record that passed the filter |

Asking once turns on both halves of the channel you asked about: `-v`, `-vv`, `-vvv` and any level (`NP_LOG`,
`[log] level`, `--log-spec`) print human text on stderr; a path (`--log-file`, `[log] file`) writes JSONL. A run that
printed anything to stderr says once what it settled on and which source won, in this exact shape:

```text
log level debug from command line
```

That line is the run explaining itself, not a log record: no level prefix, no target, not counted against the cap, and
not written when nothing asked for diagnostics.

Two edges of that rule, both measured rather than reasoned about:

```console
$ np scan /tmp/diagdemo/vault 2>err; echo "nobody asked: $(wc -c < err) bytes"
nobody asked: 0 bytes
$ NP_LOG=off np scan /tmp/diagdemo/vault 2>err; cat err
log level off from NP_LOG
```

A run nobody asked for says nothing on stderr at all. A run where `NP_LOG` **said `off`** prints that one line and then
nothing: the variable was set, it did something, and "the environment did nothing" is otherwise indistinguishable from
"the environment was ignored" — which is the difference between a config question and a shell question. That line is not
a diagnostic, so the cap neither counts it nor suppresses it.

A bad level refuses before any work happens and names the source it came from (`NP_LOG`, `--log-spec`, `log.level`). A
*config file* whose `[log] level` is bad is a different shape of failure: the run cannot load its configuration, so the
refusal is reported as a diagnostic on the channel the file itself configured, and the command still refuses — see
[When the configuration is the thing being reported](#when-the-configuration-is-the-thing-being-reported).

### The grammar, exactly

* Comma-separated items. **Only the first item may be a bare level**, and it sets the default: `warn,np_core::scan=off`.
  A bare level anywhere else refuses, naming its position.
* Every other item is `target=level`, and the target is the **whole** target string from the list below — there are no
  prefixes and no wildcards, so `fetch=trace` filters a target called `fetch` (nothing emits on that) while
  `np_core::fetch=trace` filters the transport. This is exactness rather than oversight: two ways to spell one filter in
  a file where every other value is exactly as written.
* Level names are case-sensitive: `info`, not `INFO`.
* A repeated target keeps the **last** level written for it, so `NP_LOG='np_core::scan=trace,np_core::scan=off'` leaves
  scanning off.
* `off` is a level like any other and silences what it covers — including the routine per-phase lines, which is how you
  switch an expensive phase out of a log without switching the whole run off.

### Which source wins

**flag > file > environment > default (`warn`)**: `-v` / `--log-spec`, else `[log] level`, else `NP_LOG`, else warn.

The file beating the environment surprises people who reach for `NP_LOG` first, and it is deliberate: it is the same
shape as every other setting in `config.toml` (`docs/design-f35-unknown-keys.md`), and it means a checked-in config
produces the same diagnostics whether or not whoever ran the command exported something. Two consequences worth knowing:

* `NP_LOG=off` does **not** silence a run whose `[log] level` asked for something. Remove the key, or override it with
  `-v --log-spec off`, if the file is what is talking.
* The announce line names the winner, so this is never a guessing game: `log level error from NP_LOG` versus
  `log level error from config.toml`.

## What bounds stderr

Three limits, all of them designed so that a log channel cannot damage the work it is watching.

**1 000 diagnostic lines per process, counted before formatting.** Past the cap the run continues and drops lines; the
last thing on stderr names the cap and both ways out:

```text
diagnostics truncated after 1000 lines; 101 more suppressed (raise --log-spec or set [log] file)
```

**A full stderr pipe drops diagnostics, never requests.** A client that captures `np serve`'s stderr and never reads it
is normal, and an anonymous pipe holds about 64 KiB. The sink asks descriptor 2 whether it will take a write *now* and
drops the line when it will not, counting it as suppressed like any other dropped line. Measured on this box before that
was fixed: 1 200 requests sent to a server whose stderr nobody read, **1 057 answered**, the child stalled inside
`write(2)`. Now: 1 200 in, 1 200 out, and stderr stops at ~60 KiB.

Windows has no `poll`, so there a write to a full pipe still blocks as it always did; the cap bounds how many lines can
be waiting on it, and `[log] file` is the answer for a client that will not read stderr.

**The file has a budget** (`[log] max_bytes`, 8 MiB by default). When it fills, writing stops and one final record says
so — with the size and the remedy, in the file, where the person reading the log will see it:

```json
{"action":"delete the file to continue","bytes":466,"level":"warn","msg":"log full","target":"np_cli","ts":1794258005046}
```

A log path that cannot be opened warns once and the command runs on: a diagnostic sink that fails is not a reason for
work to stop. Deleting or truncating the file mid-run is supported — the next write reopens it, and the run does not
care.

## What may be in a diagnostic

Paths, ids, rule tokens, counts and timings. **Never a note's body, never an origin host or URL** — the rule
`docs/design-m2-model-registry.md` §12 already holds the transport to, extended to every event. Note *paths* are allowed
and useful (a skip event names the file it skipped); note *content* is not. `a_diagnostic_never_carries_note_text_or_an_origin`
in `crates/np-cli/tests/diag.rs` runs scan, embed, search and models at `trace` and asserts both halves on the file
channel; `no_diagnostic_carries_a_note_body` does the same in-process for np-core.

### How long a field may be

A path can be long — a deep vault whose directory names say something passes 512 bytes without anybody trying — so the two
fields that carry one, **`what` and `workspace`, are clamped to 512 bytes**, cut at a character boundary and marked with
`…`. The clamp keeps the **front** of the path, because paths diverge at the start rather than the end: what survives is
the vault and the directory, which is the part that identifies the file. A field is never redacted whole — "which file?"
answered by nothing is not a diagnostic — and a value inside the bound is written exactly as it stands, with no marker
attached.

`msg` is deliberately unclamped. Its length is program-authored, and the sentence worth reading in a config refusal is the
end of it (`unknown field \`loggg\``), so clipping would remove the remedy to keep the problem tidy. The bound lives in one
function at the place a field enters a record, so stderr and the file cannot disagree about how the same long path was
written; `a_long_path_is_truncated_and_still_names_the_file` asserts both fields on one record, and that a short path
arrives whole.

## Phases report what they cost

Five spans open while work happens, and each closes with an `info` record naming the phase and its wall-clock cost:

```diagnostic-spans
scan
embed
search
fetch
serve.request
```

| Span            | Target           | What it covers                                             |
| --------------- | ---------------- | ---------------------------------------------------------- |
| `scan`          | `np_core::scan`  | one workspace walk, from listing to committed rows          |
| `embed`         | `np_core::embed` | one embedding run, including the model load                |
| `search`        | `np_core::search`| one ranking, both halves                                   |
| `fetch`         | `np_core::fetch` | one model download attempt — never its origin              |
| `serve.request` | `np_cli::serve`  | one JSON-RPC request, from line read to response written   |

Two keys travel on those records and are inherited by events raised inside them: **`request`** (the JSON-RPC id of the
request that caused this) and **`workspace`**. Both are omitted when there is nothing to say — never `null`, never the
string `"none"` — so a key's absence carries information. `np_core::models` has no span of its own: listing models is
the sum of per-file verdicts, not a phase whose cost anyone would ask about.

## Levels

| Level   | Meaning                                                                              |
| ------- | ------------------------------------------------------------------------------------ |
| `error` | Something failed: work was asked for and did not happen.                              |
| `warn`  | A degraded state somebody has to decide about — a refusal, an outage, stale vectors. This is the default. |
| `info`  | A state change worth reading later: what arrived, what a run decided, what a phase cost. |
| `debug` | Per-item decisions that explain a summary: why each skipped file was skipped.         |
| `trace` | Per-item routine: every file considered unchanged. Off unless named.                   |

## Targets

A target names the module whose failure this is, and is exactly what a filter selects. The ten published targets:

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

`np_cli` carries the run itself: the announce line that reaches the file, a log path that would not open, a spent
budget. `np_cli::serve` is the request loop and its spans; `np_cli::serve_channel` is stdin/stdout framing (F25), which
reports its own failures because they are about the pipe, not about a request.

## Rule tokens

Every refusal names its cause with one of these, in the `rule` key and in the human line as `rule="…"`. The list is
published in code (`np_core::diagnostics::RULE_TOKENS`) and pinned to this document by tests in both crates, in both
directions: a token emitted and undocumented fails, and so does a token documented that nothing emits.

```diagnostic-tokens
artifact-fetched
artifact-missing
config-refused
duplicate-rows
fetch-disabled
hidden-behind-unreadable
identity-taken
local-model-not-built
locked
model-not-installed
no-vectors
not-markdown
not-produced
offline
prefix-drift
provider-disabled
token-budget
unreadable
unchanged-content
unrepresentable-path
unresolved-ambiguity
```

| Token                        | Level   | Target(s)                                        | `what` holds                                    |
| ---------------------------- | ------- | ------------------------------------------------ | ----------------------------------------------- |
| `artifact-fetched`           | info    | `np_core::fetch`                                 | the artifact path that landed                   |
| `artifact-missing`           | warn    | `np_core::embed`, `np_core::models`              | the file that is not there                      |
| `config-refused`             | error   | `np_core::config`                                | — (the message names the file and key)          |
| `duplicate-rows`             | debug   | `np_core::scan`                                  | the path whose two spellings both listed        |
| `fetch-disabled`             | warn    | `np_core::fetch`                                 | the artifact that was wanted                    |
| `hidden-behind-unreadable`   | debug   | `np_core::scan`                                  | a live row's stored note path                   |
| `identity-taken`             | debug   | `np_core::scan`                                  | the path whose identity belongs elsewhere       |
| `local-model-not-built`      | warn    | `np_core::embed`                                 | — (the message names the feature)               |
| `locked`                     | warn    | `np_core::lock`                                  | the workspace that stayed locked                |
| `model-not-installed`        | warn    | `np_core::embed`, `np_core::models`              | the model id that has no row                    |
| `no-vectors`                 | warn    | `np_core::search`                                | — (the message says what was searched)          |
| `not-markdown`               | debug   | `np_core::scan`                                  | the file that was not markdown                  |
| `not-produced`               | error   | `np_core::embed`                                 | the chunk the model gave up on                  |
| `offline`                    | warn    | `np_core::fetch`, `np_core::search`              | what could not be reached                       |
| `prefix-drift`               | warn    | `np_core::embed`                                 | — (the message names the pinned sentence)       |
| `provider-disabled`          | warn    | `np_core::embed`                                 | the key that is empty                           |
| `token-budget`               | warn    | `np_core::embed`                                 | the chunk id that was too big (`expected`/`actual` carry the counts) |
| `unreadable`                 | debug   | `np_core::scan`                                  | the path that could not be read                 |
| `unchanged-content`          | trace   | `np_core::scan`                                  | the file whose bytes matched the stored stamp   |
| `unrepresentable-path`       | debug   | `np_core::scan`                                  | the name no byte path could hold                |
| `unresolved-ambiguity`       | debug   | `np_core::scan`                                  | a lossy-keyed row no route accounted for (F23)  |

Two of these need a word of explanation:

* **`offline` versus `fetch-offline`.** Diagnostics say `offline`. The *result document* keeps its published
  `note: "fetch-offline"` (`docs/reference-serve.md`), because that string is frozen output some client already matches
  on. One state, two vocabularies, mapped in one place (`RankNote::diag_rule`).
* **`unchanged-content` is `trace`, not `debug`.** A warm vault produces it for nearly every file, and a level meant to
  explain a summary would spend itself on the healthy case. The scan summary still counts skipped files on stdout; this
  token is for the run where you need to know which ones and why.

## Reading the file

One JSON object per line, keys sorted so two runs diff cleanly. The published key set is stable — a new key is a change
to this section:

| Key          | Meaning                                                              |
| ------------ | -------------------------------------------------------------------- |
| `ts`         | Unix time in milliseconds, as written by the run                     |
| `level`      | `error`, `warn`, `info`, `debug`, `trace` — one of the five, always   |
| `target`     | one of the ten targets above                                          |
| `msg`        | the human sentence                                                    |
| `request`    | JSON-RPC id, present only inside a request span                       |
| `workspace`  | workspace path, when the record is about one                          |
| `rule`       | the refusal's cause, from the table above                             |
| `what`       | what the rule was applied to                                          |
| `expected`   | the limit a refusal hit                                               |
| `actual`     | what it measured                                                      |
| `elapsed_ms` | a phase span's wall-clock cost                                        |
| `bytes`      | log size, in the `log full` record                                    |
| `action`     | the remedy, in the `log full` record                                  |

Records are written with the newline in the same append as the line, so two `np` processes pointing at one file never
split a record each other's reader has to reject.

## Transcripts

Every block below is verbatim from `np` built `--release`; the vault is `/tmp/diagdemo/vault` with `a.md`,
`photo.png` and a `locked.md` whose mode denies reading. The method, including the config each run used, is at the end.

**A run nobody asked to talk says nothing.** No `-v`, no `NP_LOG`, no `[log]` in the file — stderr is empty, and stdout
carries its usual summary, byte for byte unchanged by any of this:

```console
$ np scan /tmp/diagdemo/vault > out 2> err; echo "exit=$? stderr_bytes=$(wc -c < err)"
exit=0 stderr_bytes=0
$ cat out
added      0
modified   0
moved      0
removed    0
unchanged  2
notes      1
unresolved 0
skipped    1
  locked.md
index      INDEX.md
elapsed    1 ms
```

The `skipped` list is stdout, not a diagnostic: it is existing pinned output (`crates/np-cli/tests/cli.rs`), and the
events below are what was always missing from it — *why* each file was skipped.

**A run that was asked explains itself, then reports per-file decisions:**

```console
$ np -vv scan /tmp/diagdemo/vault 2>&1 | head -4
log level debug from command line
debug np_core::scan: the file could not be read as the walk listed it; its rows were left alone rule="unreadable" what="locked.md" workspace="/tmp/diagdemo/vault"
debug np_core::scan: not markdown, so nothing was indexed from it rule="not-markdown" what="photo.png" workspace="/tmp/diagdemo/vault"
info np_core::scan: scan finished workspace="/tmp/diagdemo/vault" elapsed_ms=1
```

**The same events in the file**, one object per line — the skip decisions and the phase cost, with no `ts` collisions to
explain ordering to a reader:

```json
{"level":"debug","msg":"the file could not be read as the walk listed it; its rows were left alone","rule":"unreadable","target":"np_core::scan","ts":1794257472773,"what":"locked.md","workspace":"/tmp/diagdemo/vault"}
{"level":"debug","msg":"not markdown, so nothing was indexed from it","rule":"not-markdown","target":"np_core::scan","ts":1794257472773,"what":"photo.png","workspace":"/tmp/diagdemo/vault"}
{"elapsed_ms":15,"level":"info","msg":"scan finished","target":"np_core::scan","ts":1794257472774,"workspace":"/tmp/diagdemo/vault"}
```

**`np serve` tags every record with the request that caused it.** Two `scan` requests, the first with `"id": 9`, the
second a notification with no id at all, against a config that refuses:

```json
{"level":"error","msg":"log level error from config.toml","target":"np_cli","ts":1794257499980}
{"level":"error","msg":"config parse error in /tmp/diagdemo/config/note-processor/config.toml: TOML parse error at line 5, column 2\n  |\n5 | [loggg]\n  |  ^^^^^\nunknown field `loggg`, expected one of `data_dir`, `catalog`, `markers`, `embeddings`, `models`, `search`, `log`\n","request":9,"rule":"config-refused","target":"np_core::config","ts":1794257499980,"workspace":"/tmp/diagdemo/vault"}
{"level":"error","msg":"config parse error in /tmp/diagdemo/config/note-processor/config.toml: TOML parse error at line 5, column 2\n  |\n5 | [loggg]\n  |  ^^^^^\nunknown field `loggg`, expected one of `data_dir`, `catalog`, `markers`, `embeddings`, `models`, `search`, `log`\n","rule":"config-refused","target":"np_core::config","ts":1794257499980,"workspace":"/tmp/diagdemo/vault"}
```

The first refusal carries `"request":9`; the second has no `request` key, because a notification has no id — and nowhere
in this file is there a `"request": null` to guess about. The opening line is the sink's own announce record, which
belongs to the run rather than to any request.

### When the configuration is the thing being reported

A refused config is both the reporter and the report. `np` reads `[log]` leniently *before* validating the rest of the
file, so a run that cannot start still writes its refusal where you told it to write, at the level you asked for:

```console
$ np scan /tmp/diagdemo/vault; echo "exit=$?"
log level error from config.toml
error np_core::config: config parse error in /tmp/diagdemo/config/note-processor/config.toml: TOML parse error at line 4, column 2
```

The command still exits 1 with its pinned refusal on stdout/stderr — a diagnostic channel never turns a failure into a
success. And `np serve` stays alive: an unparseable config makes every request fail, which is a reason to answer each one
with an error, not a reason to stop answering.

## How these transcripts were produced

Deliberately plain, so they can be reproduced by hand:

```console
$ mkdir -p /tmp/diagdemo/{vault,config/note-processor,data}
$ printf '# Alpha\n\na note about indexing\n' > /tmp/diagdemo/vault/a.md
$ printf 'not markdown at all\n' > /tmp/diagdemo/vault/photo.png
$ printf '# Locked\n\ncannot read me\n' > /tmp/diagdemo/vault/locked.md && chmod 000 /tmp/diagdemo/vault/locked.md
$ export XDG_DATA_HOME=/tmp/diagdemo/data HOME=/tmp/diagdemo XDG_CONFIG_HOME=/tmp/diagdemo/config NP_NO_AUTO_FETCH=1
```

with `[log] file`, `[log] level` and `[log] max_bytes` in `$XDG_CONFIG_HOME/note-processor/config.toml` as each example
states, the cap transcript using a directory of 1 100 `.png` files to get past 1 000 lines. Timestamps are the real ones
from those runs; nothing was hand-composed, and the two tests named in
[What may be in a diagnostic](#what-may-be-in-a-diagnostic) keep the claims they make honest.
