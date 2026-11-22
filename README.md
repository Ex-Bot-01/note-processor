# np — a local index for a folder of markdown notes

`np` walks a vault of `.md` files, keeps an incremental index in one SQLite database, resolves the links between them, rewrites a
readable `INDEX.md` inside the vault itself, and — if you ask — embeds notes and their figures with a small local model.
`np search` ranks words and meaning together, so it answers a query about `GGML_DEFAULT_N_THREADS` on a machine that has never
downloaded a model, and ranks prose by sense once one is installed.

No daemon, no port, no account: one binary, one database per workspace, all on disk. The indexing path opens no socket; the only
thing that dials out is a pinned model download you asked for, announced before byte one. An app can use np as its backend by
starting it on demand and talking to it over its pipes ([`np serve`](#use-np-from-your-app)).

**Start here:** [install](#install) · [try it](#try-it) · [commands](#the-six-commands) · [configure](#configure-it) ·
[integrate](#use-np-from-your-app) · [when something looks wrong](#when-something-looks-wrong) · [limits](#limits-stated-plainly)

## What it does

- **Indexes notes** — front matter, titles, headings, checklist items and links, under a workspace root. A rescan reports added /
  modified / moved / removed / unchanged and rewrites only what changed. Only `.md` becomes a note; everything else is tracked as an
  attachment — hashed, linkable, never searchable — and counted, so a vault full of `.txt` is not mistaken for an empty one.
- **Resolves links** — `[[Note]]`, `[[Note|alias]]`, `[text](relative.md)`, case-insensitive against the corpus. An ambiguous or
  missing target stays unresolved and is *named*, never guessed; a link inside code is an example, not an edge.
  [Rules in full](docs/reference-cli-and-reports.md).
- **Writes a catalog** — `INDEX.md` at the root, sharded past the entry budget. Generated files open with a header line, so a scan
  replaces only files it owns and leaves a hand-written `INDEX.md` alone.
- **Searches three ways** — `hybrid` (default) fuses FTS5 `bm25()` with the vector list by reciprocal rank fusion; `keyword` needs no
  model; `semantic` is vectors alone. Hits carry raw evidence (`cosine`, `bm25`, each list's rank): a fused rank sum is not a
  similarity and must never be read as one.
- **Embeds locally** — in-process, with llama.cpp built from sha256-pinned source; a linked `.png`/`.jpg`/`.jpeg` is content-hashed
  and de-duplicated across notes. And nothing outside the workspace root is ever read — not through an absolute path, not a symlink.

## Install

Get the binary for your platform from [the latest release](https://github.com/Ex-Bot-01/note-processor/releases/latest) and check it
against the `SHA256SUMS.txt` beside it: `sha256sum -c SHA256SUMS.txt` next to the archive, or on Windows compare
`(Get-FileHash .\np-x86_64-pc-windows-gnu-cross.zip).Hash.ToLower()` against that file.

| Platform | What you get | Worth knowing |
| --- | --- | --- |
| **Linux** `x86_64-unknown-linux-gnu` | one binary, inference runtime **in** it | reproducible between CI runs of one tag, not across machines ([why](docs/reference-releases.md)) |
| **Windows** `x86_64-pc-windows-gnu-cross.zip` | one `np.exe`, runtime **in** it | cross-built with mingw-w64: **unsigned, not an MSVC build** |
| **macOS** | *no asset exists* | needs an Apple SDK no cross-compiler here has → [build from source](docs/developing.md) |

Both archives carry `local-model` and `fetch`, so a downloaded binary can embed and auto-fetch rather than only index; the model is
never inside an archive. Building from source needs a stable Rust toolchain and nothing else for the default build — no C++, no cmake:

```sh
cargo install --git https://github.com/Ex-Bot-01/note-processor --tag v0.7.0 -p np-cli --locked
```

Per-version caveats, including stale assets and what is compiled out of them: [Release archive caveats](docs/reference-releases.md).

## Try it

Indexing needs no config and no model:

```sh
np scan ~/Notes
```

```text
added      5
modified   0
moved      0
removed    0
unchanged  0
notes      4
unresolved 1 — a target matched more than one file or none; spell its path to place it, e.g. [[notes/a.md]]
  gone
attachments 1 — tracked, never parsed: scratch.txt
skipped    0
index      INDEX.md
elapsed    84 ms
```

`~/Notes/INDEX.md` now lists every note with its size and link counts; the database lives under your OS data directory. Run it again
and nothing recomputes — `unchanged`, 2 ms instead of 84.

Add meaning with one command that indexes, fetches the pinned model if this workspace lacks it, and embeds:

```sh
np setup ~/Notes
np search ~/Notes "moved hash"
```

Before any model exists, `np search` still answers and names the half that was missing (exit **0**):

```text
the configured model is not installed in this workspace: ranked by keyword only — install it with `np models install <vault> --id <id> --path <file> --dim <width> --sha256 <digest>`, then `np embed`
0.0164 note   retro-notes.md  [bm25 -0.7941]   … # Retro notes  What the [moved] files taught us: detect them by [hash] …
```

With vectors installed the same command prints a relevance score per hit and ranks by sense as well as words; a measured run is in
[CLI and report reference](docs/reference-cli-and-reports.md). Everything quoted on this page is output real runs printed.

`np setup` is what an application should call — it indexes **first**, so keyword search works the moment indexing finishes, then gets as
far as it can on vectors and reports where it stopped. Four things worth knowing about that first run:

- **It may download ~295 MiB, and says so before byte one**, then verifies the digest before putting the file in place, so a partial or
  wrong download is never used. Refuse it with `--no-auto-fetch` (one run), `[models] auto_fetch = false`, or `NP_NO_AUTO_FETCH=1`
  (which outranks the file). Only ids this build **pins** are fetched — a name in a config file cannot make a download happen.
- **Offline exits 0 with `vectors: false`.** The vault is searchable and nothing needs fixing but the cable; any other stop on the
  vector half exits 1. [Models: fetch and install](docs/reference-models.md).
- **No model configured is a decision, not a fault.** np will not start inference just because a binary could; `np embeddings enable
  <id>` writes both config keys, quotes what a first `np embed` costs, and moves no bytes.
- **`np embed` reads the index, not the vault** — scan first, and it re-embeds only what moved. Reports go to stdout; llama.cpp's logs
  go to stderr at `warn`, where they cost a few hundred bytes rather than a tensor transcript.

## The six commands

| Command | What it does | What it writes |
| --- | --- | --- |
| `np scan <workspace>` | indexes notes, regenerates the catalog | index, `INDEX.md` (+ shards) |
| `np embed <workspace>` | encodes what the index says is owed | vectors for the configured model |
| `np search <workspace> <query>` | ranks notes and figures. `--limit N`, `--mode hybrid\|semantic\|keyword`, `--prefix` (match words at a word beginning), `--rerank` (optional cross-encoder, local weights only, never fetches) | nothing — it opens the database and only `SELECT`s |
| `np setup <workspace> [--yes]` | scan → fetch if missing → embed, one report | index, maybe the artifact, vectors |
| `np models install \| fetch \| hash \| list …` | puts model bytes in place, per workspace | `<data_dir>/workspaces/<key>.models/` |
| `np embeddings enable <id> \| disable` | turns the embedding model on or off in np's config. **Fetches nothing** | `config.toml`, `[embeddings]` |

Every command takes `--json`. Exit codes mean what they say: **0** success (including "nothing to do" and "nothing indexed"),
**1** anything the runtime refused, **2** an argv error. Every flag, report field and search-hit key:
[CLI and report reference](docs/reference-cli-and-reports.md).

## Before you script against it

- **The keyword half wants *every* word in *one* note.** Words are quoted and joined with `AND`, so `"lag broker"` finds the note
  holding both while `"retro kafka"` — one word in each of two notes — answers nothing at exit **0**, and a sentence-shaped query
  usually dies on one stray `how`. Ask for fewer words, or let `hybrid` rank by meaning once vectors exist. FTS5's own syntax stays
  out on purpose (`"`, `*`, `-`, `(` and `:` are ordinary characters, which keeps `GGML_DEFAULT_N_THREADS` out of a query parser) and
  a query past 32 words searches its first 32.
- **`--mode semantic` in a vault with no vectors refuses at exit 1**, where the default answers with keyword hits at exit 0: one list
  that cannot run is not answerable by the other.
- **Readers are never blocked by writers.** `search` and `models list` take no write lock, so they keep answering while a `scan` runs;
  two writers contend, and the second waits two seconds then reports `locked` as a state rather than a mystery.

## Configure it

Optional, read once per command from `$XDG_CONFIG_HOME/note-processor/config.toml` (Windows: `%APPDATA%\note-processor\config.toml`).
A partial file is fine — every key has a default, and an environment variable only *fills* a key the file leaves out.

```toml
[embeddings]
provider = "local-model"     # "none" (the default) indexes without vectors at all
model = "embeddinggemma-2"   # an id this workspace has been given, by fetch or install

# [search]
# mode = "hybrid"            # hybrid | semantic | keyword — `--mode` overrides per run
```

That is the whole minimum. The other keys — token budgets, llama.cpp threads and log volume, catalog sharding and indentation,
checklist markers, the fetch origin and its overrides — carry defaults and are listed in [Configuration reference](docs/reference-config.md).

## Use `np` from your app

`np serve` reads one JSON-RPC 2.0 request per line from stdin and writes one response per line to stdout until stdin closes, then
exits **0**. Same work as the commands, on a channel that does not fork and opens no port: whoever can write to the process's stdin
already owns the files it can read.

```jsonc
// one request per line in; one response per line out
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"workspace":"/vault","notifications":true}}
{"jsonrpc":"2.0","id":2,"method":"search","params":{"workspace":"/vault","query":"moved hash"}}
```

Ten methods — `initialize`, `search`, `scan`, `embed`, `models.list`, `links.list`, `embeddings.enable`, `embeddings.disable`,
`status.get`, `shutdown` — with the workspace named in every request, so one server answers for several vaults. Four rules decide most
client code:

- **Every `result` is exactly the document `np <verb> --json` prints** — one builder behind both channels, compared value for value by
  a test rather than trusted.
- **stdout is the protocol.** Every sentence goes to stderr, so a client never strips prose out of a parse.
- **States come back as data, not faults:** `locked`, `offline`, `reason` and `prefix_drift` are keys in the document, while a fault
  means the request was wrong (`-32700`, `-32601`, `-32602`) or np refused before working (`-32000`).
- **Progress is off unless you ask.** With `notifications` in the handshake, np narrates the slow parts — `locking`, `fetching` with
  byte checkpoints against the pinned size, `embedding`, `writing`, then a terminal `completed` or `refused` — and nothing arrives
  unsolicited, so older clients keep the responses they expect.

`links.list` answers what the terminal never asks — **who links this note, and where is every dead target written?** — in one call,
with both ends as display paths, the line each edge was written on, and unresolved targets named by the text a fix replaces.
`params.rel` scopes it to one note; it is a reader that creates nothing and answers while an embed runs. Diagnostics stay off the
protocol (`-v`, `NP_LOG`, `[log] file`), records carrying the id of the request that raised them. Methods, phases, fault codes, captured
transcripts and a Python walkthrough: [`np serve` reference](docs/reference-serve.md).

## Images, briefly

A figure is linked with ordinary markdown — `![alt](diagram.png)` — resolved like any other link, inside the workspace only. `np`
encodes **PNG and JPEG**, extension *and* content signature both; `.svg`, `.drawio`, `.excalidraw` and `.pdf` refuse as
`unsupported-format`, by decision (an SVG is a file that asks to fetch things). Figures are content-addressed, so bytes linked from six
notes encode once, and every refusal lands in one of six classes that each name an action. A refused figure costs nothing but the
count: the note around it stays searchable as text. Encoding needs `--features local-model,vision` plus the projector artifact —
published archives are text-only on the image side and say so per figure rather than silently skipping.
[Images in the index](docs/reference-images.md).

## When something looks wrong

| You saw | It means |
| --- | --- |
| `configuration disabled: embeddings.provider = "none"` from `np embed` | Nobody has said yes yet. `np embeddings enable <id>` writes both keys and quotes the download without starting it; an index-only build is a supported shape, not a failure |
| `no vectors in this workspace: ranked by keyword only …`, exit **0** | Not an error: keywords answered, the vector list had nothing to rank |
| `nothing matched "…"` in a vault you know holds the words | The keyword half needs every word in one note; try fewer words. A vault nobody indexed says `… this workspace has never been scanned` |
| `current 3 embedded 0` after you edited notes | Run `np scan` first — embedding is driven by the index |

Every other refusal sentence np prints, and the two llama.cpp warnings that are normal: [Troubleshooting](docs/reference-troubleshooting.md).

## Limits, stated plainly

- **Hosted CI grades Linux on every push to `main`; the Windows row and both C++ feature rows run nightly**, so between a merge and
  the next nightly `main` is graded on Linux only. Beside it stands a local gate list run by exit code, and each release's notes say
  which rows ran: [Developing](docs/developing.md).
- **The Windows write-lock behaviour has never been measured on Windows.** A second writer refusing cleanly is believed equivalent to
  Linux — SQLite uses `LockFileEx` there instead of POSIX record locks — and that belief is untested. MSVC parity is unmeasured too:
  `build.rs` refuses that target rather than pretending.
- **No macOS asset**, and feature builds on unsupported targets get a written refusal from `build.rs`.
- **Image encoding is off in the published archives** (`vision` is not a default feature), so figures land in `vision-unavailable`
  until you build with it.
- **No stemming**: `runner` does not match `runners`, because this vault holds identifiers and a typed word must appear whole. Ask for
  the shorter form instead — mark it (`"grand*"`) or pass `--prefix` — and the answer reports which words matched at a word beginning
  (`prefix_used`, `prefix_terms`) and which hits came only from a prefix (`matched_exactly`). Widening is opt-in because the rule that
  protects `runner` also protects your file names.
- **No OCR, no PDF or audio, no sync, no watch mode**, and no network transfer anywhere except the model-fetch path — the indexing
  half never opens a socket.
- Unmeasured and open: image throughput, resident-set growth with a projector open, cross-worker bit-identity, retrieval quality — in
  the [image-embeddings design](docs/design-m2-image-embeddings.md).

## Where to read next

Reference: [CLI and reports](docs/reference-cli-and-reports.md) · [configuration](docs/reference-config.md) ·
[`np serve`](docs/reference-serve.md) · [models](docs/reference-models.md) · [images](docs/reference-images.md) ·
[diagnostics](docs/reference-diagnostics.md) · [troubleshooting](docs/reference-troubleshooting.md) ·
[release caveats](docs/reference-releases.md) · [developing](docs/developing.md) · [licensing](docs/licensing.md).

[`docs/README.md`](docs/README.md) indexes the whole library, including the design behind each decision and the ADRs that locked it.
The code is two crates: `crates/np-core` (discovery, parsing, links, catalog, database, embeddings, media, search) and `crates/np-cli`
(argv, resolution, printers — no decisions of its own).

## License

MIT — [`LICENSE`](LICENSE), carried beside the binary in every archive. [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) holds the
verbatim MIT text for the statically linked llama.cpp / ggml code and exactly which archives are linked, because that obligation
attaches to the artefact you download rather than the repository it came from. Model weights are not redistributed: they are fetched
on your explicit action, digest-verified, and their own license governs them. [The long version](docs/licensing.md).
