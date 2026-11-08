# note-processor

A local-first indexer for a folder of markdown notes. `np` walks a workspace, keeps an incremental index of every note in a
SQLite database, resolves the `[[wikilinks]]` and `[text](target)` links between them, regenerates a human-readable catalog
(`INDEX.md`) in the vault itself, and — when you opt in — embeds notes and the images they link with a small local model.
`np search` ranks by words and meaning together by default, so it answers a query about `GGML_DEFAULT_N_THREADS` on a machine
that has never downloaded a model, and ranks prose by sense once one is installed.

Nothing here is a service. No daemon, no server, no account: one binary, one database per workspace, all on disk. The
indexing path never opens a socket; the only command that can dial out is `np models fetch`, and it says so before it transfers
a byte.

**Everything measured in this file was measured on one host** — Linux x86-64, 16 CPUs, 7.4 GB RAM, 2026-11-05 — and says so
where a number is host-specific. Where something has *not* been measured, this file says that too rather than guessing.

## Contents

- [What it does, and does not do](#what-it-does-and-does-not-do)
- [Quick start](#quick-start)
- [Installing `np`](#installing-np)
- [Getting a model onto the machine](#getting-a-model-onto-the-machine)
- [Configuration](#configuration)
- [Using it day to day](#using-it-day-to-day)
- [Reading the report](#reading-the-report)
- [Images in the index](#images-in-the-index)
- [Troubleshooting](#troubleshooting)
- [Building from source, and developing](#building-from-source-and-developing)
- [Where the thinking lives](#where-the-thinking-lives)

## What it does, and does not do

| | |
| --- | --- |
| **Indexes notes** | Discovers files under a workspace root, parses front matter, titles, headings, checklist items and links, and stores them per workspace. A rescan reports added / modified / moved / removed / unchanged and rewrites only what changed |
| **Resolves links** | `[[Note]]`, `[[Note|alias]]` and `[text](relative.md)` are matched case-insensitively against the corpus. An ambiguous or missing target is left unresolved and *named* in the report — never guessed |
| **Writes a catalog** | `INDEX.md` at the workspace root, sharded into `INDEX-<a>-<z>.md` when it grows past the configured entry budget. Generated files carry a fingerprint header, so a scan only ever replaces files it owns |
| **Embeds locally** | With the `local-model` feature and an installed artifact, vectors are computed in-process by llama.cpp from the sha256-pinned source. Default builds contain no inference runtime at all |
| **Embeds images too** | A `.png`/`.jpg`/`.jpeg` linked from a note is content-hashed, de-duplicated across notes, and (in a build with image support) encoded to a 768-component unit vector. See [Images in the index](#images-in-the-index) |
| **Searches three ways** | `np search <workspace> <query> [--mode hybrid\|semantic\|keyword]`. `hybrid` (the default) fuses SQLite's FTS5 `bm25()` over titles and bodies with the vector list by reciprocal rank fusion; `keyword` needs no model at all and never returns a figure; `semantic` is vectors alone, the behaviour before FTS5. Hits are ordered deterministically and carry their raw evidence (`cosine`, `bm25`, each list's rank) so a fused score is never unexplained |
| **Does not** | Sync, serve, or watch. No OCR, no PDF/audio support, no vector-format rasterising ([F29](docs/backlog-m2-followups.md)), no stemming (`runner` does not match `runners`, because this vault holds identifiers), and no cloud calls outside `models fetch`. Image encoding needs a build made with `--features local-model,vision` plus an installed projector: the release binaries are text-only by default, and say so per figure rather than silently skipping |

## Quick start

```sh
cargo build --workspace --release
./target/release/np scan ~/Notes
```

```text
added      5
modified   0
moved      0
removed    0
unchanged  0
notes      3
unresolved 1 — a target matched more than one file or none; spell its path to place it, e.g. [[notes/a.md]]
  gone.md
skipped    0
index      INDEX.md
elapsed    163 ms
```

That is a working indexer: `~/Notes/INDEX.md` now lists every note with its size and link counts, and the database lives under
your OS data directory. Run it again and nothing recomputes (`unchanged 5`, 25 ms in the same test).

Embedding and searching need two more decisions of yours — a build with the runtime in it, and a model installed into that
workspace — because the default binary deliberately ships neither:

```sh
cargo build --release -p np-cli --features local-model   # also needs cmake; see Building from source
np models install ~/Notes --id embeddinggemma-2 --path ~/models/embeddinggemma-2-Q8_0.gguf \
  --dim 768 --sha256 2188ac1d…09135                        # the digest `np models hash` prints for that file
```

Then write `[embeddings] provider = "local-model"` + `model = "embeddinggemma-2"` into your config (see
[Configuration](#configuration)) and:

```sh
np scan ~/Notes && np embed ~/Notes && np search ~/Notes "how are moved files detected"
```

```text
model      embeddinggemma-2
outcome    nothing owed
seen       3
current    2
embedded   1
refused    0
busy-gap   0
errors     0
images     links 1 · embedded 0 · refused 1 (vision-unavailable 1)
verify     self-consistent, not verified
digest     not re-read: same size and mtime as the last check (295.5 MB); detects changed bytes, not a crafted mtime
threads    12 of 16 available (auto)
```

```text
0.7140 note   projects/timeline.md
0.6566 note   projects/brief.md
0.5703 note   index-notes.md
```

`np setup ~/Notes` does those three steps as one command, and is what an application should call: it indexes **first** (keyword
search works the moment indexing finishes), fetches the model if this workspace is missing it, embeds, and prints one report whose
`vectors` / `offline` / `fetched` fields say exactly where it got to. Offline it exits **0** with `vectors: false` — the vault is
searchable and nothing needs fixing but the cable — while any other stop on the vector half exits 1.

On the **release binaries**, which are built without the inference feature, the vector half cannot run at all: `setup` indexes and then
refuses at exit 1 — never with a download, because a binary that cannot read the model must not spend 310 MB fetching it. What the refusal
*says* depends on what it found first: an installed artifact gives `runtime not built`, naming `--features local-model` and keyword search
(F40's arm); a vault with nothing installed yet gives `configuration disabled`, which names a key you set correctly and no remedy — that
wording is a known defect (**F41**), not the behaviour to expect forever.

**The first run downloads a model, and says so before byte one.** Since F38, `np embed` and `np setup` fetch the pinned artifact when
it is missing: ~310 MB for the text encoder (`mmproj` adds ~555 MB in a `vision` build), announced with its id, size and the *kind* of
origin, then verified against the compiled-in digest before it is put in place — a partial or wrong file is never used. Turn it off
three ways: `--no-auto-fetch` for one run, `[models] auto_fetch = false` in the config file, or `NP_NO_AUTO_FETCH=1` in the
environment, which outranks the file precisely so a daemon can be silenced without editing it. Only ids this build **pins** are ever
fetched — a name in a config file cannot make a download happen — and `np search` never fetches at all, in any mode: reading a vault
must not cost bandwidth. With no connection, `np embed` exits 1 and says nothing was embedded; `np setup` still indexes; and
`np search ~/Notes "…"` answers from keywords while you wait for either.

Two things that trip people up, both by design:

- **`np embed` reads the index, not the vault.** Edit a note and `np embed` will call it current; run `np scan` first, then
  `np embed` re-embeds only what moved (`embedded 1` above).
- **The report goes to stdout; llama.cpp's log lines go to stderr, and by default only its warnings survive.** A model load used to write
  145,816 bytes over 1,553 lines here (`create_tensor:`, `llama_model_loader:` …) ahead of a one-line answer; the default level is `warn`,
  which leaves 424 bytes on a search and 1,670 on an embed of 15 chunks: what survives are llama.cpp's own warnings, and one of them repeats
  per batch, so the volume tracks work done rather than the size of the answer. Ask for the transcript back with
  `[embeddings] log_level = "info"`, or — when the file names no level at all, since a file that named one wins — `NP_LLAMA_LOG_LEVEL=debug`
  for a single run. See [the config table](#configuration).

## Installing `np`

### From source (the supported path)

You need a stable Rust toolchain — the repository pins none, and everything here was built with 1.99.1. Nothing else is needed
for the default build: no C++ compiler and no cmake. The only thing cargo reaches for is its own registry of crates.

```sh
git clone https://github.com/Ex-Bot-01/note-processor.git
cd note-processor
cargo build --workspace --release        # binary at target/release/np
cargo install --path crates/np-cli       # or install it as `np` on your PATH
np --help
```

```text
Usage: np <COMMAND>

Commands:
  scan    Index a note workspace and regenerate its catalog
  embed   Embed the notes a workspace owes, for the configured model
  search  Rank the indexed notes and images against a text query
  models  Install and inspect the model artifacts of one workspace
```

The default build links **no** inference runtime: `np scan` is fully functional, while `np embed` reports that nothing was
produced and `np search` refuses to turn your query into numbers — both at exit 1, with a message naming what to change. Add
`--features local-model` (and cmake) if you want the runtime in the binary; see
[Building from source](#building-from-source-and-developing).

### Pre-built binaries

GitHub Releases carry `np` archives named `np-<version>-<target>`. **`v0.1.0`'s assets are stale** — that tag points at the
2026-10-28 tree, whose CLI had exactly one subcommand (`scan`) — so use them for the indexer only, never for `embed`, `search` or
`models`.

From **`v0.2.0` onward, read the release notes before downloading**: `.github/workflows/release.yml` builds on hosted Windows,
Linux and macOS runners, and those runners are metered — when an account is out of included Actions minutes the workflow cannot
run at all (`docs/ops-actions-minutes.md`). A release produced by `.github/scripts/package-assets.sh` instead carries **the one
platform the packager was standing on**, and its notes say so. Source builds need no CI and no minutes:

```sh
cargo install --git https://github.com/Ex-Bot-01/note-processor --tag v0.2.3 -p np-cli --locked
```

**Current release: `v0.2.3`** — one fix on top of v0.2.2, and it is a Windows one. An upgraded machine whose index still sat at the
pre-F37 location was correctly *read* from there (nothing re-indexed, no duplicate), but printed none of the `reading your index from …` notice
that release promised: `Config::load` asked a merged struct whether the operator had declared a directory, and a resolved default always looks
like an instruction. Found by a field run on real Windows; graded on Linux through the same injectable-platform seam F37 already used.

**`v0.2.2` is where `np setup` and auto-fetch (F38) arrive**, along with the Windows data folder moving under
`%LOCALAPPDATA%\note-processor` (F37). Its assets are built locally, not by CI: Linux (`x86_64-unknown-linux-gnu`, by
`package-assets.sh`) and Windows (`x86_64-pc-windows-gnu`, cross-compiled with mingw-w64 — recipe in
[`docs/ops-cross-build-windows-gnu.md`](docs/ops-cross-build-windows-gnu.md)). Both are unsigned, and the Windows one is compiled **without**
the inference feature — which also means it cannot auto-fetch: F38's trigger is compiled out of a binary that could not read the bytes, so
`np embed` and `np setup` refuse there instead of spending 310 MB. `scan`, `search` in all three modes, `models list/install/fetch` and
everything about paths and identity do work. If you can build from source with `--features local-model`, do that instead — it is the only shape
with a runtime in it. No macOS asset exists yet: that needs an Apple SDK, which no cross-compiler here has.

## Getting a model onto the machine

Embedding is opt-in twice over: a feature at build time and an artifact at run time. One model identity is pinned in this
repository's build today — `embeddinggemma-2`, 768 components, empty document/query prefixes — and its two artifacts are a
text encoder (309,855,456 bytes) and a vision projector (554,821,056 bytes).

Artifacts live **per workspace**, next to that workspace's database:

```text
<data_dir>/workspaces/<key>.sqlite        the index for one workspace
<data_dir>/workspaces/<key>.models/       its installed artifacts, named by model id
<data_dir>/workspaces.sqlite              the registry of known workspaces
```

`<key>` is a digest of the workspace's absolute path as bytes, so two spellings of one directory share one index and two
directories never share it. `<data_dir>` is `$XDG_DATA_HOME` else `$HOME/.local/share`; on Windows it is
`%LOCALAPPDATA%\note-processor` — nested under a folder that says whose it is, like the config file always was (**F37**). Before that
change Windows wrote `workspaces/` and `workspaces.sqlite` straight into `%LOCALAPPDATA%`, which is where every application writes; if
that is where your index is, this release still reads it there and says so on stderr rather than reporting your notes as new.
An explicit `data_dir` in the config file is an instruction, not a location the program moved, and is never overridden.

### Option A — fetch it

```sh
np models fetch ~/Notes --id embeddinggemma-2
```

The command you run when you want to place the model by hand. (It used to be the only thing in the program that opened a socket;
since **F38** `np embed` and `np setup` reach the same origin themselves when the artifact is missing — see the first-run note above for
the three ways to refuse.) Both forms print where they are about to dial, in class terms and never as a URL:

```text
origin      this build's compiled-in default — no [models] base_url is configured (an empty NP_MODELS_BASE_URL counts as unset)
```

A text-only binary fetches only the text encoder; naming `--member mmproj` from such a build is refused, because transferring
554 MB that the binary cannot load is not a service. Interrupted downloads are guarded by a marker file that no clock may
expire — if you know no fetch is running, say so with `--discard-marker`. It will not overwrite an artifact that already
exists:

```text
np: refusing to overwrite the existing artifact <data_dir>/workspaces/<key>.models/embeddinggemma-2
```

### Option B — install a file you already have

```sh
np models hash ~/models/embeddinggemma-2-Q8_0.gguf     # prints "<sha256>  <path>"
np models install ~/Notes --id embeddinggemma-2 --path ~/models/embeddinggemma-2-Q8_0.gguf \
  --dim 768 --sha256 <that digest>
```

`install` reads the file, verifies it against the digest you declared, and copies it into place — it never moves or truncates
your original. One word on stdout: `installed`, or `already installed` when the id is recorded with the same tuple. Re-running
with a **different** dim or prefix for an existing id is refused rather than silently re-stamping history.

The projector of a model — the file that lets a vision build encode figures — installs the same way, from the same offline
file, with `--member`:

```sh
np models hash ~/models/mmproj-embeddinggemma-2-Q8_0.gguf
np models install ~/Notes --id embeddinggemma-2 --member mmproj \
  --path ~/models/mmproj-embeddinggemma-2-Q8_0.gguf --sha256 <that digest>
```

Three things about that command are deliberate. **`--dim` is refused, not ignored**: a projector carries no vector width, and
a flag that reads like a fact about the file would be a quieter failure than a refusal. **The model has to be installed
first** — a member row describes a file belonging to an installed identity, so `--member mmproj` on a workspace with no such
model refuses before reading anything, including the 554 MB you pointed it at. **The digest is still mandatory**: an
unverified projector would encode every figure in your vault with bytes nobody vouched for, and those vectors would then be
compared against pinned-model text vectors forever. `--json` on a member install prints `{"id", "member", "state"}` — the
extra key because `state` alone cannot say which artifact the run placed; the plain model install keeps its two keys.

`np models list ~/Notes` then says what the workspace believes:

```text
models     1
embeddinggemma-2  present  dim 768  sha256 2188ac1deca4  installed_at 1793893791  prefix_doc ""  prefix_query ""  files mmproj:present
unclaimed  0
```

`state` is `present`, or `missing` when the row exists and its file does not — a deleted artifact is reported rather than
forgotten. `files` names the model's **other** members (a projector, fetched or installed offline) and whether each file is
there; it appears only when there is one to name, and `--json` carries the same set as `members`
(`{"name", "file", "present"}`, empty for a model with a single file). It answers by stat, never by hashing — byte
verification happens at use time, where reading 554 MB is already being paid for — so it tells you *a file is there*, not
*those are the right bytes*. Before this field, confirming an offline projector install meant running `np embed` and watching
`projector-absent` go away: a diagnosis standing in for a report. Re-declaring an id with a different width is refused outright, because that would silently change what every stored
vector means; see [Troubleshooting](#troubleshooting) for the sentence it prints.

## Configuration

Optional, and read once per command from:

```text
$XDG_CONFIG_HOME/note-processor/config.toml     else ~/.config/note-processor/config.toml
                                                (Windows: %APPDATA%\note-processor\)
```

A partial file is fine — every key has a default. Two environment variables exist, and they **fill keys the file leaves out**;
a value in the file always wins, and an empty environment value counts as unset rather than as a choice.

| Key | Default | What it does |
| --- | --- | --- |
| `catalog.max_entries_per_file` | `600` | Entry budget for one catalog file before it shards |
| `catalog.continuation_indent` | `2` | Indent for wrapped continuation lines |
| `embeddings.provider` | `"none"` | `"none"`, or `"local-model"` to embed in-process. `"openai-compatible"` parses and is refused: nothing dials out for a vector |
| `embeddings.model` | unset — required by `local-model` | The id from `np models install`; there is no guessing, because an id nobody installed cannot own a vector |
| `embeddings.dim` | the installed row's width | A declared width that disagrees with the row is refused before any write |
| `embeddings.max_tokens` | `1024` | Token budget per sequence. Over-budget chunks are **reported**, never silently truncated |
| `embeddings.threads` | `"auto"` | Worker count for llama.cpp: `min(cpus, max(min(cpus, 4), 80%))`, so 16 CPUs → 12 and a 2-core laptop → 2. An integer pins it; `0`, negatives and non-integers are refused |
| `embeddings.log_level` | unset, which means `warn` | How loud llama.cpp may be on stderr: `off`, `error`, `warn`, `info`, `debug`. Anything else is refused naming the key and all five. `NP_LLAMA_LOG_LEVEL` fills it when the file leaves it unset — it does **not** override a file that named one, the same precedence every other environment value here follows |
| `models.base_url` | the compiled-in origin | Where `models fetch` may go. Mirrors are one line away; refusals never name a host |
| `models.sha256` | unset | Read into config (`NP_MODELS_SHA256` fills it when the file omits the key). Nothing in the fetch path consults it today, so it does **not** replace a pin — see `docs/design-m2-local-embeddings.md` §12 rule 7's note |
| `markers.done`, `markers.paused` | built-in lists | Strings that classify checklist items when the catalog renders them |
| `search.mode` | `hybrid` | Which lists `np search` ranks by: `hybrid`, `semantic` or `keyword`. `--mode` overrides it for one run; a fourth spelling is refused, naming the three (**F35**'s rule) |

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
```

## Using it day to day

```sh
np setup <workspace> [--json] [--yes]     # index, fetch the pinned model if missing, embed — one command for a caller that wants vectors
np scan <workspace> [--json]              # index + regenerate the catalog
np embed <workspace> [--json]             # embed what the index says is owed
np search <workspace> <query> [--limit N] [--mode M] [--json]   # --limit defaults to 10, --mode defaults to config's `hybrid`
np models install|fetch|hash|list …
```

- **Order matters: scan, then embed.** Embedding works from the index. `scan` is idempotent and cheap on an unchanged tree.
- **What counts as a note.** Only files ending `.md` (any case) are parsed as notes; other files under the root are still tracked
  for changes, which is why a vault of five files can report `added 5` and `notes 3`. Directories whose name begins with `.` and
  `processed/` are not walked at all.
- **Incremental both ways.** A note whose bytes have not moved keeps its vector; a figure's vectors are keyed to its *content
  hash*, so the same image linked from six notes is encoded once, and deleting the last link to it prunes the row and its
  vectors on the next run.
- **`search` writes nothing.** It prepares only `SELECT`s — no vector, no stamp, no note row. The keyword half is the same
  database's FTS5 index, kept current by three triggers on `notes`, so a query costs a read and never a rebuild. (Opening a
  brand-new workspace's database still creates it, like every other command: "read-only" describes what search changes about
  the index.) Which kinds are indexed is never a refusal: a notes-only corpus searches fine, so does an images-only one, and an
  index with no vectors prints `nothing indexed for <id>` and exits **0**.
- **`np search` answers before any model exists.** That is what hybrid as the default bought (**F36**,
  `docs/design-m3-hybrid-search.md`): a workspace with no artifact and no vectors returns its keyword hits, prints one line on
  stderr naming the half that was missing, and exits **0**. Nothing dials out to fetch a model because a config key named one —
  the download stays an explicit `np models fetch`. Ask for `--mode semantic` in that same workspace and you get the F32 refusal
  at exit 1 instead, because a request for one list that cannot run is not answerable by the other.
- **A schema upgrade's record outlives the run that made it.** The scan whose open cleaned up orphan rows says so on stdout, once — a permanent
  one-line warning would train everyone to ignore it. The record itself lives in the workspace database and reaches every later report as
  `recorded_upgrade_orphans` and `recorded_merged_notes`, so "what did an upgrade delete here last month?" is a question `np scan --json` answers instead of
  one that needs a database shell (**F21**). A `null` means the file holds no such record; the all-zero string means the check ran and removed nothing, and
  those two states are never collapsed into an empty string.
- **`--json` is the machine interface.** stdout carries exactly one JSON document (`scan`, `embed`, `search`, `models list`).
  Search's document is `{hits, mode, note, vectors}`: the list, the mode that ran, a sentence about a half that could not run
  (`null` when nothing was missing), and whether this workspace holds vectors at all — so "no semantic hits" and "no model
  installed" stay two different reports. Every hit carries the same **eleven** keys whatever the kind and whatever the mode —
  `score`, `cosine`, `bm25`, `semantic_rank`, `keyword_rank`, `kind`, `path`, `snippet`, `linked_from`, `source_note`,
  `linked_notes` — because an absent key and a null are two different claims. The five evidence keys are F36's, and they exist so
  a fused score can be argued with: in `keyword` mode `cosine` and `semantic_rank` are `null`, in `semantic` mode the reverse.
  The last three are F28's: **every** live note linking a figure, byte-ordered, where the other two were its head and its length.
  A note hit and an orphaned figure both get `[]`, never `null` — "this hit has no live linker" is one true statement about
  either, and telling them apart is what `kind` is for.

```sh
np search ~/Notes "vision projector" --limit 2 --json
```

A two-note vault with no model installed — the whole document one real run printed, `--mode keyword` so the numbers are the
keyword half's alone:

```sh
np search ./notes retro --mode keyword --json
```

```json
{
  "hits": [
    {
      "bm25": -1.1139240506329113e-6,
      "cosine": null,
      "keyword_rank": 1,
      "kind": "note",
      "linked_from": 0,
      "linked_notes": [],
      "path": "retro.md",
      "score": 1.1139240506329113e-6,
      "semantic_rank": null,
      "snippet": "# Weekly\n\n[Retro]: the demo went well, migration postponed.\n",
      "source_note": null
    }
  ],
  "mode": "keyword",
  "note": null,
  "vectors": false
}
```

`bm25` is SQLite's own number, unremapped (better is more negative, so `score` carries its negation to keep "higher first"
true across modes), and the text channel prints small magnitudes in scientific notation rather than `-0.0000`. With a hybrid
list whose semantic half could not run, `note` holds the sentence that stderr got and `cosine`/`semantic_rank` hold nulls.

And with vectors present, `hybrid` fuses both lists. The same two-note vault after `np embed`, queried `"broker lag"` — one
real run, both channels, on a `--features local-model` build:

```text
0.0328 note   kafka.md  [cos 0.8143 bm25 -1.97e-6]   … # Kafka  Consumer [lag] and the migration plan for the [broker] fleet.
0.0161 note   retro.md  [cos 0.7090]
```

`kafka.md` is first because **both lists put it first** (fused `1/61 + 1/61`); `retro.md` appears on its cosine alone, with no
`bm25` and no keyword rank to show. That is the whole argument for fusion in two lines: a note that agrees in both lists beats a
note that leads one, and you can see why from the bracket rather than taking the score on faith — which matters because a fused
rank sum is not a similarity and must never be read as one.

A figure linked from two notes, from a `--features local-model,vision` build whose projector was installed offline
(`np models install --member mmproj`) — the whole output of one real run, including what the text channel makes of it:

```json
{
  "kind": "image",
  "linked_from": 2,
  "linked_notes": ["notes/a.md", "notes/b.md"],
  "path": "img/fig.png",
  "score": 0.5161576999208495,
  "source_note": "notes/a.md"
}
```

```text
0.8208 note   notes/a.md
0.7350 note   notes/b.md
0.5162 image  img/fig.png   (from notes/a.md +1 more)
```

Before F28 that image line was `(from notes/a.md)` and `notes/b.md` existed only as the number 2 in a JSON key nobody read.
The note printed is still `source_note`, because it is `linked_notes[0]` — one list, assembled once, and a test that fails
if head-or-count ever stops matching it.

- **Exit codes.** `0` success (including "nothing to do" and "nothing indexed"), `1` anything the runtime refused — disabled
  provider, missing artifact, a model mismatch — and `2` for argv errors, which is clap's doing. No command invents a code.

## Reading the report

`np embed` prints one line per fact, in this order (measured output above):

| Line | Meaning |
| --- | --- |
| `model` | The id whose vectors this run read and wrote |
| `outcome` | What was left when the pass ended — `nothing owed`, or the arm that stopped it |
| `seen` / `current` / `embedded` | Notes considered, notes already vectorised for this model, notes encoded now |
| `refused` | Chunks over `max_tokens`: counted and named, never truncated |
| `busy-gap` | Work that appeared between the plan and the write; the next run picks it up |
| `errors` | Failures that did not stop the run — each is a finding, not noise |
| `images` | `links N · embedded N · refused N (class N …)` plus `unchecked` / `pruned` when they are non-zero. See below |
| `verify` | Whether this run re-hashed the artifact or took its stamp at face value |
| `digest` | The same claim in words, with the size it saw and what that check can and cannot detect |
| `threads` | Workers llama.cpp used, and whether `"auto"` or you chose it |

## Images in the index

A note links a figure with ordinary markdown image syntax — `![alt](diagram.png)` — and the path resolves exactly like a note
link: relative to the linking file, case-insensitively, inside the workspace only. Accepted extensions are `.png`, `.jpg` and
`.jpeg`, **and** the bytes must carry that format's signature: an accepted name wrapping a BMP or a GIF is refused as
`unsupported-format` rather than handed to a decoder that would happily read it (F26).

Figures are content-addressed, so identical bytes across notes encode once, and every refusal is counted under one of six
classes on the `images` line:

| Class | What it means, and what to do |
| --- | --- |
| `missing-target` | No readable file at that path: absent, a directory rather than a file, unopenable, or unreadable part-way through hashing. Fix the target or restore the file |
| `outside-workspace` | The target is absolute or escapes the workspace root (including through a symlink). Nothing outside the vault is read; move the figure in, or link it relatively |
| `unsupported-format` | Not `.png`/`.jpg`/`.jpeg`, or an accepted name whose bytes are not that raster — including every `.svg`, `.drawio`, `.excalidraw` and `.pdf`, which are out by decision, not oversight (see below). Re-export as PNG or JPEG |
| `undecodable` | Accepted name and signature, but the decode failed — a truncated or damaged file is the usual cause. Re-export the figure; nothing partial was stored |
| `over-budget` | The encode measured more image patches than the context window holds (8,192 tokens). **Never observed on this artifact pair**: one size was measured — a 4096×3072 PNG cost 1,064 tokens — and larger sizes have not been. If you see it, suspect a narrower window or a re-pinned artifact rather than a slightly big diagram |
| `vision-unavailable` | Two meanings, one label: this binary has no image code, or **a provider refused something this report has no class for**. Every release build of `np` is the first case (`vision` is off by default, like `local-model`), so every figure that *resolves and passes the content gate* lands here — and a build made with `--features local-model,vision` reports `projector-absent` instead until the projector is installed |
| `projector-absent` | This binary **does** have the image code and this workspace has no projector for it to load: the `mmproj` member is not recorded by this database, or is on disk but not the bytes that record vouches for. Fetch or install it. Split out of `vision-unavailable` by **F28**, because the two advise different actions — one file versus a rebuild — and the old folded label kept the count honest while sending someone to the wrong command |

**Vector and diagram source formats are out on purpose ([F29](docs/backlog-m2-followups.md)).** `.svg`, `.drawio`, `.excalidraw` and `.pdf` links
resolve like any other attachment and then refuse as `unsupported-format`. That is a product limit, not a gap waiting for an implementation note:
rasterising them means a decoder that must disable external references and DTDs — an SVG is a file that asks to fetch things — which is a dependency,
an attack surface, and a per-encode cost this project's rules say gets measured before it is added, all of it for a capability nobody has asked for yet.
So the answer to "what will `np` encode" is **PNG and JPEG**, extension *and* content signature both.

What a refused figure still costs you is nothing but the count: the note around it is indexed as text like any other, its prose searchable, and the link
itself recorded in `links` — so `np search` finds the note by what you wrote about the diagram. It will not find it by what the diagram depicts, which is
the half a rasteriser would buy.

The counts count what their nouns say: `links` is written targets de-duplicated per note (a refused link still counts),
`unchecked` means some note could not be read so the sweep was held off, and `pruned` reports vectors deleted along with the
figure they belonged to.

## Troubleshooting

| What you saw | What it means |
| --- | --- |
| `np: configuration disabled: embeddings.provider = "none"` from `np embed` | No provider chosen. Either write `[embeddings] provider = "local-model"` into the config, or accept that this build indexes only |
| `np: embedding disabled: embeddings.provider = "none" — set it to "local-model" and name an installed model with embeddings.model, then run np embed` from `np search` | Same state, seen from the command that needs a vector. The sentence ends with the two keys to edit (F32 made `search` say its own cause instead of borrowing this one). **Only `--mode semantic` reaches it now**: the default answers with keywords and caveats on stderr instead (**F36**) |
| `no vectors in this workspace: ranked by keyword only — np models fetch or np embed adds the semantic half` on stderr, exit **0** | Not an error. The keyword list answered; the vector list had nothing to rank against. Fetch a model and run `np embed` for the other half, or write `[search] mode = "keyword"` if you never want the sentence (builds without `fetch` word this line as install-then-embed, because they have no fetch command) |
| You want to know what an old schema upgrade removed — headings, tags, checklist items or links that predated the constraints | `np scan <workspace> --json`, and read `recorded_upgrade_orphans` / `recorded_merged_notes`. They carry the same strings the run that did the work printed once, out of the database rather than off a stream (**F21**). `null` means this file never recorded one; `headings=0,tags=0,…` means the sweep ran and removed nothing |
| `np: no model row for "embeddinggemma-2" in this workspace: fetch it with … or install bytes you already hold with …` | The config names a model this **workspace** has never been given — the usual state after editing `config.toml` by hand. The tail is the argv to run, and it lists only commands this build has (`np models fetch` appears in builds compiled with `fetch`) |
| `np: no embedding provider for this workspace: its model artifact is absent, so the query cannot be embedded: …` | The row exists but the file under `<key>.models/` does not. Same argv tail, same reason it is printed |
| `np: refusing to overwrite the existing artifact …` from `models fetch` | Rule 5 of §12: a fetch publishes into an absent target only. Remove the artifact deliberately, or install over a declared digest with `np models install` |
| `model embeddinggemma-2 is already installed with dim = 768; this command declares 512` (abridged: the real message wraps the values in backticks) | Installing a second meaning onto an existing id is refused and nothing is written. A different artifact or width means a new id, not an overwrite |
| `verify self-consistent, not verified` | This run trusted the stamp instead of re-hashing 300 MB — normal after the first check. `digest` says when it last looked |
| Vectors under another model id | `search` refuses rather than ranking your query against a different model's space; it names the configured id and never another model's path or URL |
| Thousands of `create_tensor:` lines | You asked for them: `log_level` is set to `info`/`debug` somewhere in your environment or config, and `debug` prints llama.cpp's entire transcript. Unset it and a healthy run leaves only warnings on stderr |
| `load: control-looking token: … was not control-type; this is probably a bug in the model`, and `init: embeddings required but some input tokens were not marked as outputs` | Real warnings from llama.cpp about the GGUF's tokenizer metadata and batching, kept by the default `warn` level. The second repeats once per batch, so an embed that does work prints more of them than a search does; `log_level = "error"` silences them — and with them a genuine load failure, which is why silence is not the default |
| `np: configuration error: config io: NP_LLAMA_LOG_LEVEL: "deubg" is not a log level — write one of off, error, warn, info, debug` | A typo in the variable. It refuses rather than silently defaulting, because a hatch you cannot trust is worse than no hatch — and note that a value in your **config file wins over it**, like every other key here |
| `np embed` says `current 3 embedded 0` after you edited notes | Run `np scan` first — embedding is driven by the index |
| `np: configuration disabled: embeddings.model = "…"` on a vault you just configured | Nothing is installed under that id **in this workspace** yet: run `np models fetch <ws> --id <id>`, or install bytes you already hold. The message names a config key you set correctly and no remedy — that wording is a known defect (**F41**), not a hint to edit your config |
| `offline: could not reach the pinned origin to fetch …` | Auto-fetch (**F38**) tried, and nothing answered. `np embed` exits 1 because it was asked for vectors and got none; `np setup` still indexes and exits 0 with `vectors:false, offline:true`; `np search --mode keyword` answers meanwhile. Re-run when you have a connection — no partial state was written |
| A Windows index you cannot find in `%LOCALAPPDATA\note-processor` but your notes are "new" | You likely ran an older build, which wrote loose in `%LOCALAPPDATA%`. This release keeps reading the old location for one release and says so on stderr (`reading your index from …`); if it did not say so and your notes re-added, that is a bug worth reporting (**F37**) |

## Building from source, and developing

```sh
cargo build --workspace                 # no C++, no runtime: the default everyone gets
cargo test --workspace                  # 645 tests pass here; none needs a model
```

The `local-model` feature builds llama.cpp from sha256-pinned source. It is **Linux-only by refusal** in `build.rs`, and it
needs two explicit steps, because `cargo build` never fetches anything:

```sh
CMAKE=/path/to/cmake scripts/vendor-llama-cpp.sh      # fetch + verify the pinned tarball into .vendor/
CMAKE=/path/to/cmake cargo build --release -p np-cli --features local-model
```

The vendor script downloads the tarball named in `third_party/llama-cpp.toml`, refuses on any digest mismatch (printing both),
and records provenance outside the tree; re-running it on an existing tree prints `already vendored: …` and fetches nothing.
On this host a cold feature build of `np-cli` took 57 s with `-j 4`.

**Watch your disk.** A target directory that has run every gate here reached **10.7 GiB**; `cargo clean` reclaims it, and the
pinned source in `.vendor/` (184 MB) survives so a rebuild needs no download.

Working on the code means running the whole gate list by exit code — subsetting it is how a green run stops meaning anything:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --locked -p np-core --features local-model --all-targets -- -D warnings
cargo clippy --locked -p np-cli --features local-model --all-targets -- -D warnings
cargo clippy --locked -p np-core --features local-model,vision --all-targets -- -D warnings
cargo clippy --locked -p np-cli --features local-model,vision --all-targets -- -D warnings   # the row whose absence was F28
cargo clippy --locked -p np-cli --no-default-features --all-targets -- -D warnings
cargo test --workspace
cargo test -p np-cli --no-default-features
env -u HOME -u XDG_CONFIG_HOME cargo test -p np-core --test config
RUSTDOCFLAGS="--deny warnings" cargo doc --locked --workspace --no-deps
```

Two rules that are not negotiable here: **never regenerate the catalog goldens** (any diff in `tests/golden*` is a stop-and-
report), and a test whose assertion cannot be broken by mutating the code it covers is a defect, not coverage.

The model-backed suites skip loudly instead of failing quietly. Set the two variables and they run for real:

```sh
NP_TEST_MODEL_PATH=~/models/embeddinggemma-2-Q8_0.gguf \
NP_TEST_MMPROJ_PATH=~/models/mmproj-embeddinggemma-2-Q8_0.gguf \
  cargo test -p np-core --features local-model,vision --test local_vision
```

Without them, nothing that needs weights runs. Counted on 2026-11-06 on this host with both variables unset — `cargo test --workspace` prints
**5** such lines (most artifact-gated suites are not even compiled without the runtime), `-p np-core -p np-cli --features local-model` prints
**36**, and the same pair with `--features local-model,vision` prints **45**. They mean "compiled, never ran", never "passed". The commands are
printed beside the numbers on purpose: an earlier pair recorded here (29 / 38) could not be reproduced by any command in this file once F28's
artifact-free cases landed, and a count that moves with every gated case is worth re-measuring rather than trusting. `$NP_TEST_VISION_LARGE` and
`$NP_THREAD_*` gate two further cells the same way. **GitHub Actions does run this repository.** That sentence used to read "cannot execute
jobs for this account today", which was true during the gap `todo.md` dates (no Windows steps from 2026-10-29 to 2026-10-31) and false ever since:
measured on 2026-11-06, `Test (ubuntu-latest)`, `Test (windows-latest)` and `Feature local-model (ubuntu-latest)` each executed and each passed on
every push to `main` at `2c65525`, `935b2cd` and `64a6b73` (runs `42171010611`, `42187057325`, `42188471272`) and on a branch push at `03cb04a`
(run `42192715021`). So a build or test claim about Linux or Windows has CI behind it as well as the local exit codes quoted above — and absent CI
output is still not evidence either way, which is the rule that round wrote down.

What CI never has is the model weights, so the artifact-gated rows print their `NOT COVERAGE` there by design and every *measured* figure in these
docs — bytes, milliseconds, vector counts — still comes from this host with those two variables set. macOS keeps its standing limit: the build fact
only (the unix `statvfs` arm compiles), its number never read here.

## Where the thinking lives

The code comments carry the local reasoning; these documents carry the decisions and the measurements behind them.

| Document | Holds |
| --- | --- |
| `docs/superpowers/specs/2026-10-23-note-processor-design.md` | The original product spec: what to index, how links resolve, what a catalog is |
| `docs/design-m2-embeddings.md` | The embedding design: chunking, budgets, the provider seam |
| `docs/design-m2-local-embeddings.md` | llama.cpp ownership (§14), the pin discipline and the §12 fetch rules, §5's context ceiling |
| `docs/design-m2-models-install.md` | What "installed" means: rows, digests, per-workspace artifacts |
| `docs/design-m2-image-embeddings.md` | R7c: media tables, discovery and sniffing, the vision seam, §8's refusal classes, §14's measured numbers |
| `docs/design-m2-thread-policy.md` | The `"auto"` worker rule and the measurements behind it |
| `docs/backlog-m2-followups.md` | What is owed: F27–F33 open, with what each one costs |
| `docs/roadmap-2026-10-27.md`, `todo.md` | Where this is going and what is next |

Repository layout: `crates/np-core` (discovery, parsing, links, catalog, database, embeddings, media, search) and
`crates/np-cli` (argv, resolution and printers — no decisions of its own), with `third_party/`, `scripts/` and the pinned
`.vendor/` tree for the C++ side.

**Known gaps, stated plainly.** The binaries CI publishes still cannot encode an image: `vision` is off by default, so every figure in a release
build's corpus lands in `vision-unavailable`. A build made with `--features local-model,vision` does encode them — the passthrough **F28** was is
in place, and the run hands over only a projector this workspace has recorded and whose digest (or stamp) matches. What that build needs from you is
the file itself, and either door is open now: `np models fetch --member mmproj`, or `np models install --member mmproj` from a
file you copied there yourself (**F28** step 3, offline, digest verified). And `np models list` now shows what landed:
`files mmproj:present` on the row, or `members` in `--json` (**F28**'s reporting gap, closed — it was a listing gap, not an
install one, and fetched projectors were invisible too). Image throughput, resident-set growth with a projector open, cross-worker bit-identity and
retrieval quality have **not** been measured — the list is in `docs/design-m2-image-embeddings.md` §14. Windows and macOS get a
written refusal from `build.rs` for feature builds and have never been executed here.

CI runs again, and reading it is what surfaced **F34**. Measured from the Actions API on 2026-11-05: hosted jobs ran continuously until
2026-10-29T13:11 UTC, created zero steps from 2026-10-29T14:39 through 2026-10-31T20:23, and have run on every push since
2026-11-01T20:08. The run just before `a816b17` was green on Windows, and the **next 43 CI runs all failed**
`Test (windows-latest)` on the same disk-preflight assertion — four days of merges that were green on Linux and red on Windows. F34's
fixes turned that around: green on the branch from `2ba7dc0`, and green on `main` at `b2840f9` (run `42090000404`, all three jobs). "Green" in this README still means the exit codes measured locally and listed
above; the Windows job is now a second reader, and it has already caught something local gates could not.

A fourth job joins on this branch — **`Feature local-model,vision (ubuntu-latest)`** — and it exists because of what the three above could not see:
every `--features local-model,vision` row ever run was graded on `-p np-core`, so a green CI said nothing about whether `np` itself could be built
with image support. It could not (**F28**), and no failure any of the other jobs produce would have said so. Its first grading is the run that adds it,
which is worth stating rather than backdating: this sentence was written before that job had ever gone green.

**Why the zero-step windows happen, now measured (`docs/ops-actions-minutes.md`).** This repository is private, so its hosted minutes are metered against
the account's monthly allowance — and both unexplained gaps in CI line up with that allowance running out. October spent **2,028 billed minutes** and went
zero-step on 2026-10-29T14:39; November had no C++ cache and spent **2,137 in six days**, crossing the cap at `2026-11-07T01:57:05Z`, seven seconds before
the first zero-step failure. A Windows job is 54.6 % of that spend, and both feature rows rebuild the vendored llama.cpp from scratch every run. So a red
main gets one check before anything else: do the jobs have **steps**? `steps=0` on all four means the account is out, not the tree — re-triggering until it
goes green would only teach everyone to ignore CI.

**What CI grades, as of 2026-11-07 (`ci.yml` reshaped for the allowance).** A **push to `main`** runs `Test (ubuntu-latest)` — fmt, clippy, rustdoc with
`--deny warnings`, and the whole workspace test suite including the release-only cases — and nothing else: about 4.6 billed minutes, landing within minutes of
the merge. The **Windows row and both C++ feature rows** now run on a **nightly schedule (03:17 UTC), on any pull request, and on a manual dispatch**, which is
where platform and feature-gated evidence comes from: `gh workflow run ci.yml --ref <sha>` buys the full matrix for one specific tree (~25.4 billed minutes) when
a claim needs it before anyone writes it down. **Pushes to other branches run no CI at all.** That is a trade, not a saving: between a merge and the next nightly,
`main` is graded on Linux only, so a Windows-only lint or a feature-gated compile break can sit there for up to a day. The 17-row local gate list — which includes
the `local-model` and `local-model,vision` clippy, rustdoc and test rows — is what stands in during that window, and it is what ran before every merge this week.

The arithmetic is in `docs/ops-actions-minutes.md`, including the correction it forced: at November's measured cadence (15 runs a day, 6.25 of them onto `main`)
the earlier recommendation to keep Windows and the feature rows on every merge would still have cost ~5,900 billed minutes a month against an allowance inferred at
~2,000. The shape above projects ~1,620.
