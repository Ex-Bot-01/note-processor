# Configuration reference

Every key, its default, and what it refuses. The file is optional and read once per command; a partial file is fine.

> *Moved out of `README.md` on 2026-12-09, when the README became a document you read before installing rather than one you read after something broke. The wording is the same as it was there — including the dates, the measured numbers and the claims about what has not been measured — because those are the parts that carry weight.*


Optional, and read once per command from:

```text
$XDG_CONFIG_HOME/note-processor/config.toml     else ~/.config/note-processor/config.toml
                                                (Windows: %APPDATA%\note-processor\)
```

A partial file is fine — every key has a default. Two environment variables exist, and they **fill keys the file leaves out**;
a value in the file always wins, and an empty environment value counts as unset rather than as a choice.

The table below was checked against the structs on 2026-12-09 rather than copied from the README: every container carries
`deny_unknown_fields` (**F35**), so `Config`, `CatalogConfig`, `MarkerConfig`, `EmbeddingsConfig`, `ModelsConfig`, `SearchConfig` and `LogConfig` in
`crates/np-core/src/config.rs` *are* the key list — a key not in them cannot be written at all. That audit added two rows this table had
lost: top-level `data_dir`, and `models.auto_fetch`, which is one of the three kill switches the README names while omitting the key that
holds it. Three more arrived on 2026-12-10 with `[log]`, the diagnostics section. There are **18** keys.

| Key | Default | What it does |
| --- | --- | --- |
| `data_dir` (top level) | the OS data directory | Where indexes and artifacts live: `$XDG_DATA_HOME` else `$HOME/.local/share`; Windows `%LOCALAPPDATA%\note-processor`. An explicit value is an instruction, not a location the program moved, so F37's read-fallback never second-guesses it. `legacy_data_dir` sits beside it in the struct but is `#[serde(skip)]`: a diagnostic for stderr, never a key you write |
| `catalog.max_entries_per_file` | `600` | Entry budget for one catalog file before it shards |
| `catalog.continuation_indent` | `2` | Indent for wrapped continuation lines |
| `embeddings.provider` | `"none"` | `"none"`, or `"local-model"` to embed in-process. `"openai-compatible"` parses and is refused: nothing dials out for a vector |
| `embeddings.model` | unset — required by `local-model` | The id from `np models install`; there is no guessing, because an id nobody installed cannot own a vector |
| `embeddings.dim` | the installed row's width | A declared width that disagrees with the row is refused before any write |
| `embeddings.max_tokens` | `1024` | Token budget per sequence. Over-budget chunks are **reported**, never silently truncated |
| `embeddings.threads` | `"auto"` | Worker count for llama.cpp: `min(cpus, max(min(cpus, 4), 80%))`, so 16 CPUs → 12 and a 2-core laptop → 2. An integer pins it; `0`, negatives and non-integers are refused |
| `embeddings.log_level` | unset, which means `warn` | How loud llama.cpp may be on stderr: `off`, `error`, `warn`, `info`, `debug`. Anything else is refused naming the key and all five. `NP_LLAMA_LOG_LEVEL` fills it when the file leaves it unset — it does **not** override a file that named one, the same precedence every other environment value here follows |
| `models.base_url` | the compiled-in origin | Where `models fetch` may go. Mirrors are one line away; refusals never name a host |
| `models.auto_fetch` | `true` | Whether a command that needs the pinned artifact may fetch it when it is absent (**F38**). `--no-auto-fetch` refuses for one run and `NP_NO_AUTO_FETCH=1` outranks this file; only ids this build **pins** are ever fetchable, whatever the key says. Default-on is a product decision, not a serde convenience: the consumer of `np` is increasingly another program, and "call one command, get vectors" beats "call three and interpret three refusals" |
| `models.sha256` | unset | Read into config (`NP_MODELS_SHA256` fills it when the file omits the key). Nothing in the fetch path consults it today, so it does **not** replace a pin — see `docs/design-m2-local-embeddings.md` §12 rule 7's note |
| `markers.done`, `markers.paused` | built-in lists | Strings that classify checklist items when the catalog renders them |
| `search.mode` | `hybrid` | Which lists `np search` ranks by: `hybrid`, `semantic` or `keyword`. `--mode` overrides it for one run; a fourth spelling is refused, naming the three (**F35**'s rule) |
| `log.level` | unset, which means `warn` — and nothing on stderr | How loud diagnostics may be: `off`, `error`, `warn`, `info`, `debug`, `trace`, or the same comma grammar `NP_LOG` takes (`warn,np_core::scan=off`) with whole target names as filters. A bad level is refused naming the key. **A flag outranks this key and this key outranks `NP_LOG`**, which is the precedence every environment value here follows; whatever won says so once on stderr (`log level info from config.toml`). Levels, targets and every rule token: [`docs/reference-diagnostics.md`](reference-diagnostics.md) |
| `log.file` | unset, so no file | Path for the JSONL log — one object per line, machine-readable, every record that passed the filter. Created if absent, appended if present, and its parent directory is created the way the data dir is. An unopenable path warns once and the command runs on: a sink that fails is not a reason for work to stop |
| `log.max_bytes` | `8388608` (8 MiB) | The budget for **the file**, not the run, counted from the bytes already on the path — so a log that filled in an earlier run stays full instead of gaining one record per invocation. Spending it writes one final `log full` record with the size and the remedy (`delete the file to continue`) and then stops; there is no rotation, because a half-finished rename is a worse story than a log that stops honestly. A non-integer is refused at load; `0` creates the file and writes nothing to it |

```toml
[embeddings]
provider = "local-model"
model = "embeddinggemma-2"
# max_tokens = 1024        # per-sequence budget; over-budget is reported, not truncated
# threads = "auto"         # or an integer
# log_level = "warn"       # off | error | warn | info | debug — llama.cpp's stderr volume

[catalog]
max_entries_per_file = 600

# [search]
# mode = "hybrid"          # hybrid | semantic | keyword — `np search --mode` overrides it per run

# [log]
# level = "warn"           # off|error|warn|info|debug|trace, or 'warn,np_core::scan=off'
# file = "/var/log/np.jsonl"   # JSONL; unset means no file. No `~` expansion: an absolute path is a path
# max_bytes = 8388608      # budget for that file; when spent, one `log full` line and stop
```
