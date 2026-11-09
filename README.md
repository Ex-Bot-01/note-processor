# np — a local index for a folder of markdown notes

`np` walks a vault of `.md` files, keeps an incremental index in a SQLite database, resolves the `[[wikilinks]]` and
`[text](target)` links between notes, rewrites a readable `INDEX.md` in the vault itself, and — if you ask for it — embeds notes
and the images they link with a small local model. `np search` ranks words and meaning together, so it answers a query about
`GGML_DEFAULT_N_THREADS` on a machine that has never downloaded a model, and ranks prose by sense once one is installed.

No daemon, no port, no account: one binary, one database per workspace, all on disk. The indexing path never opens a socket; the
only thing that dials out is a pinned model download, and it says so before byte one. An app that wants `np` as its backend starts
one on demand and talks to it over its pipes ([`np serve`](#use-np-from-your-app)).

Everything quoted below was measured on one host — Linux x86-64, 16 CPUs, 7.4 GB RAM — and where something has *not* been measured,
this file says so instead of guessing.

**Start here:** [install](#install) · [first run](#first-run-two-commands) · [commands](#the-five-commands) ·
[config](#configure-it) · [app integration](#use-np-from-your-app) · [troubleshooting](#when-something-looks-wrong) ·
[limits](#limits-stated-plainly)

## What it does

- **Indexes notes.** Discovers files under a workspace root, parses front matter, titles, headings, checklist items and links. A rescan
  reports added / modified / moved / removed / unchanged and rewrites only what changed.
- **Resolves links.** `[[Note]]`, `[[Note|alias]]` and `[text](relative.md)` are matched case-insensitively against the corpus. An
  ambiguous or missing target is left unresolved and *named* in the report — never guessed.
- **Writes a catalog.** `INDEX.md` at the workspace root, sharded into `INDEX-<a>-<z>.md` once it passes the configured entry budget.
  Generated files carry a fingerprint header, so a scan only ever replaces files it owns.
- **Searches three ways.** `hybrid` (the default) fuses SQLite's FTS5 `bm25()` over titles and bodies with the vector list by reciprocal
  rank fusion; `keyword` needs no model at all; `semantic` is vectors alone. Hits carry their raw evidence (`cosine`, `bm25`, each
  list's rank), because a fused rank sum is not a similarity and must never be read as one.
- **Embeds text and figures locally.** Vectors are computed in-process by llama.cpp built from sha256-pinned source. A
  `.png`/`.jpg`/`.jpeg` linked from a note is content-hashed, de-duplicated across notes, and encoded in a build with image support.
- **Stays inside your vault.** Nothing outside the workspace root is read — not through an absolute path, not through a symlink. The
  indexing path opens no socket; the only transfer is a pinned model download you asked for.

## Install

Grab the binary for your platform from [the latest release](https://github.com/Ex-Bot-01/note-processor/releases/latest) and check
it against the `SHA256SUMS.txt` published beside it:

```sh
sha256sum -c SHA256SUMS.txt        # Linux/macOS, run next to the archive
```

```powershell
(Get-FileHash .\np-0.3.0-x86_64-pc-windows-gnu-cross.zip).Hash.ToLower()   # Windows: compare against SHA256SUMS.txt
```

| Platform | What you get | Worth knowing |
| --- | --- | --- |
| **Linux** `x86_64-unknown-linux-gnu` | one binary, inference runtime **in** it | built by `.github/scripts/package-assets.sh`; byte-reproducible |
| **Windows** `x86_64-pc-windows-gnu-cross.zip` | one `np.exe`, runtime **in** it | cross-built here with mingw-w64 — **unsigned, not an MSVC build**; import table checked at packaging so it stays a single file |
| **macOS** | *no asset exists* | needs an Apple SDK no cross-compiler here has → [build from source](docs/developing.md) |

Both archives carry the `local-model` and `fetch` features, so a downloaded binary can embed and auto-fetch rather than only index.
The model itself is never inside an archive: ~309 MB arrives only when you run `np setup` or `np embed` (a vision projector adds
~555 MB more), and `NP_NO_AUTO_FETCH=1` switches that off. Every archive contains `np`, `LICENSE` and `THIRD_PARTY_NOTICES.md`.

Building from source needs nothing but a stable Rust toolchain for the default build — no C++, no cmake:

```sh
cargo install --git https://github.com/Ex-Bot-01/note-processor --tag v0.3.0 -p np-cli --locked
```

Per-version caveats (which old assets are stale, what is compiled out of them): [Release archive caveats](docs/reference-releases.md).

## First run, two commands

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
notes      3
unresolved 1 — a target matched more than one file or none; spell its path to place it, e.g. [[notes/a.md]]
  gone.md
skipped    0
index      INDEX.md
elapsed    163 ms
```

`~/Notes/INDEX.md` now lists every note with its size and link counts, and the database lives under your OS data directory. Run it
again and nothing recomputes (`unchanged 5`, 25 ms in the same test).

Now add meaning — one command that indexes, fetches the pinned model if this workspace lacks it, and embeds:

```sh
np setup ~/Notes
np search ~/Notes "how are moved files detected"
```

```text
0.7140 note   projects/timeline.md
0.6566 note   projects/brief.md
0.5703 note   index-notes.md
```

`np setup` is what an application should call: it indexes **first** (keyword search works the moment indexing finishes), then gets
as far as it can on vectors and reports exactly where it stopped. Offline, it exits **0** with `vectors: false` — the vault is
searchable and nothing needs fixing but the cable. Any other stop on the vector half exits 1.

**The first run may download ~310 MB, and says so before byte one.** Since F38, `np embed`, `np setup` and a default-mode
`np search` fetch the pinned artifact when it is missing, announcing id, size and the *kind* of origin, then verifying the digest
before putting it in place — a partial or wrong file is never used. Three ways to refuse: `--no-auto-fetch` for one run,
`[models] auto_fetch = false` in the config, or `NP_NO_AUTO_FETCH=1` in the environment (which outranks the file, so a daemon can be
silenced without editing it). Only ids this build **pins** are ever fetched — a name in a config file cannot make a download happen.

Two habits that pay off:

- **`np embed` reads the index, not the vault.** Edit a note and `np embed` calls it current; run `np scan` first, then
  `np embed` re-embeds only what moved.
- **The report goes to stdout; llama.cpp's log lines go to stderr**, where the default level (`warn`) leaves a few hundred bytes
  instead of the 145,816-byte tensor transcript it used to print ahead of a one-line answer.

## The five commands

| Command | What it does | What it writes |
| --- | --- | --- |
| `np scan <workspace>` | indexes notes and regenerates the catalog; reports added / modified / moved / removed / unchanged | index, `INDEX.md` (+ shards) |
| `np embed <workspace>` | encodes what the index says is owed | vectors for the configured model |
| `np search <workspace> <query> [--limit N] [--mode hybrid\|semantic\|keyword]` | ranks notes and figures; default `hybrid` fuses FTS5 `bm25()` with vectors by reciprocal rank fusion | nothing (it opens the database, then only `SELECT`s) |
| `np setup <workspace> [--yes]` | scan → fetch if missing → embed, one report | index, maybe the artifact, vectors |
| `np models install \| fetch \| hash \| list …` | puts model bytes in place, per workspace | `<data_dir>/workspaces/<key>.models/` |

Every command takes `--json` for a machine-readable document, and exit codes mean what they say: **0** success (including "nothing
to do" and "nothing indexed"), **1** anything the runtime refused, **2** argv errors. Full field-by-field reference, including why
every search hit always carries the same eleven keys: [CLI and report reference](docs/reference-cli-and-reports.md).

Two behaviours worth knowing before you script against it:

- **`np search` answers before any model exists.** A workspace with no artifact and no vectors returns its keyword hits, prints one
  line on stderr naming the half that was missing, and exits **0**. Ask for `--mode semantic` in the same vault and you get a refusal
  at exit 1 instead — a request for one list that cannot run is not answerable by the other.
- **Readers are never blocked by writers.** `search` and `models list` take no write lock, so they keep answering while a `scan` or
  `embed` runs. Two writers contend: the second waits two seconds and then reports `locked`, as a state rather than a mystery
  (`np: locked: another process is writing this workspace (2000 ms) …`).

## Configure it

Optional, read once per command from `$XDG_CONFIG_HOME/note-processor/config.toml` (Windows:
`%APPDATA%\note-processor\config.toml`). A partial file is fine — every key has a default, and an environment variable only *fills*
a key the file leaves out.

```toml
[embeddings]
provider = "local-model"     # "none" (the default) indexes without vectors at all
model = "embeddinggemma-2"   # an id this workspace has been given, by fetch or install

# [search]
# mode = "hybrid"            # hybrid | semantic | keyword — `--mode` overrides per run
```

That is the whole minimum. Fifteen keys exist in total — counted from the `deny_unknown_fields` structs that define them, because a key
not in those structs cannot be written at all — and those three are the ones most people need. The other twelve (token budgets,
llama.cpp thread count and log volume, catalog sharding and indentation, checklist markers, the fetch origin and its two overrides)
are listed with defaults and refusal behaviour in [Configuration reference](docs/reference-config.md).

## Use `np` from your app

`np serve` reads one JSON-RPC 2.0 request per line from stdin and writes one response per line to stdout until stdin closes — same
work as the commands, on a channel that does not fork:

```console
$ np serve
{"jsonrpc":"2.0","id":1,"method":"initialize"}
{"jsonrpc":"2.0","id":1,"result":{"np":"0.3.0","protocol":1,"features":{"local_model":false,"vision":false,"fetch":true},"methods":["initialize","search","scan","embed","models.list","shutdown"],"data_dir":"/home/you/.local/share/note-processor"}}
{"jsonrpc":"2.0","id":2,"method":"search","params":{"workspace":"/home/you/Notes","query":"timeline"}}
{"jsonrpc":"2.0","id":2,"result":{"hits":[…],"mode":"hybrid","vectors":false,"note":"embeddings are switched off (…)","reason":"provider-disabled","fetched":null,"offline":false}}
```

Six methods — `initialize`, `search`, `scan`, `embed`, `models.list`, `shutdown` — and three rules:

- **Every `result` is exactly the document `np <verb> --json` prints**, because both channels call one builder; a test compares them
  value for value rather than trusting intent.
- **stdout is the protocol.** Every sentence goes to stderr, so a client never strips prose out of a parse.
- **States come back as data, not faults.** `locked`, `offline`, `reason` and `prefix_drift` are keys in the document; a fault means
  the *request* was wrong (`-32700`, `-32601`, `-32602`) or `np` refused before doing work (`-32000`).

A workspace is named in every request rather than remembered at startup, so one server can answer for several vaults. It exits **0**
when stdin closes. Nothing listens on a port: whoever can write to the process's stdin already owns the files it can read. Design and
as-built notes: [`np serve` over stdio](docs/superpowers/specs/2026-12-08-np-serve-stdio-design.md).

## Images, briefly

A note links a figure with ordinary markdown — `![alt](diagram.png)` — resolved like any other link, inside the workspace only.
`np` encodes **PNG and JPEG**, extension *and* content signature both; `.svg`, `.drawio`, `.excalidraw` and `.pdf` resolve and then
refuse as `unsupported-format`, by decision rather than oversight (an SVG is a file that asks to fetch things). Figures are
content-addressed, so identical bytes linked from six notes encode once. Every refusal is counted under one of six classes that each
name an action — [Images in the index](docs/reference-images.md). A refused figure still costs you nothing but the count: the note
around it stays searchable as text.

Image *encoding* needs a build made with `--features local-model,vision` plus the projector artifact; the release binaries are
text-only on the image side and say so per figure rather than silently skipping.

## When something looks wrong

| You saw | It means |
| --- | --- |
| `configuration disabled: embeddings.provider = "none"` from `np embed` | No provider chosen yet — write `[embeddings] provider = "local-model"`, or accept an index-only build |
| `no vectors in this workspace: ranked by keyword only …` on stderr, exit **0** | Not an error. Keywords answered; the vector list had nothing to rank |
| `np embed` reports `current 3 embedded 0` after you edited notes | Run `np scan` first — embedding is driven by the index |
| `locked: another process is writing this workspace (2000 ms)` | Another `np` holds this vault's write lock (**F44**). Retry; readers were never blocked. A killed process cannot leave a lock behind |
| `offline: could not reach the pinned origin to fetch …` | Auto-fetch tried and nothing answered. `setup` still indexed and exited 0; keyword search works meanwhile; no partial state was written |

The full table — including every refusal sentence, the two llama.cpp warnings that are normal, and what to read when an old schema
upgrade removed something: [Troubleshooting](docs/reference-troubleshooting.md).

## Limits, stated plainly

- **No hosted CI has graded this tree since 2026-11-07** (the account's included minutes ran out). What stands in its place is a
  20-row local gate list run by exit code, and each release's notes say which rows ran: [developing](docs/developing.md).
- **The Windows write-lock behaviour has never been measured on Windows.** `np scan` / `np embed` refusing cleanly when a second
  writer holds a vault is believed equivalent to Linux — SQLite uses `LockFileEx` there instead of POSIX record locks — and that
  belief is untested. MSVC parity is unmeasured too: `build.rs` refuses that target rather than pretending.
- **No macOS asset**, and feature builds on Windows/macOS get a written refusal from `build.rs`.
- **Image encoding is off in the release binaries** (`vision` is not a default feature), so every figure lands in
  `vision-unavailable` until you build with it.
- **No stemming**: `runner` does not match `runners`, because this vault holds identifiers. No OCR, no PDF or audio, no sync, no
  watch mode, no cloud calls outside `models fetch`.
- Unmeasured and open: image throughput, resident-set growth with a projector open, cross-worker bit-identity, retrieval quality —
  the list is in [`design-m2-image-embeddings.md` §14](docs/design-m2-image-embeddings.md).

## Documentation

| Read | For |
| --- | --- |
| [Models: fetch and install](docs/reference-models.md) | where artifacts live per workspace, what `install` verifies, what it refuses |
| [Configuration reference](docs/reference-config.md) | every key, default, and refusal |
| [CLI and report reference](docs/reference-cli-and-reports.md) | `--json` fields, report lines, exit codes, measured output |
| [Images in the index](docs/reference-images.md) | sniffing, the six refusal classes, why vector formats are out |
| [Troubleshooting](docs/reference-troubleshooting.md) | every sentence `np` prints that makes people pause |
| [Release archive caveats](docs/reference-releases.md) | what each version's assets do and cannot do |
| [Developing](docs/developing.md) | feature builds, vendored llama.cpp, the gate list, what CI grades |
| [Licensing in full](docs/licensing.md) | the MIT ruling, what the notice obligation attaches to |

The decisions behind those, each with its measurements:

| Document | Holds |
| --- | --- |
| [`docs/superpowers/specs/2026-10-23-note-processor-design.md`](docs/superpowers/specs/2026-10-23-note-processor-design.md) | The original product spec: what to index, how links resolve, what a catalog is |
| [`docs/design-m2-embeddings.md`](docs/design-m2-embeddings.md) | The embedding design: chunking, budgets, the provider seam |
| [`docs/design-m2-local-embeddings.md`](docs/design-m2-local-embeddings.md) | llama.cpp ownership (§14), the pin discipline and the §12 fetch rules, §5's context ceiling |
| [`docs/design-m2-models-install.md`](docs/design-m2-models-install.md) | What "installed" means: rows, digests, per-workspace artifacts |
| [`docs/design-m2-image-embeddings.md`](docs/design-m2-image-embeddings.md) | R7c: media tables, discovery and sniffing, the vision seam, §8's refusal classes, §14's measured numbers |
| [`docs/design-m2-thread-policy.md`](docs/design-m2-thread-policy.md) | The `"auto"` worker rule and the measurements behind it |
| [Hybrid search (F36)](docs/design-m3-hybrid-search.md) · [auto-fetch (F38)](docs/design-f38-auto-fetch.md) · [`np serve`](docs/superpowers/specs/2026-12-08-np-serve-stdio-design.md) | The three most recent behaviour changes, as designed and as built |
| [Backlog](docs/backlog-m2-followups.md) · [roadmap](docs/roadmap-2026-10-27.md) · [todo](todo.md) | What is owed, with what each item costs, and where this is going |

Code comments carry the local reasoning; those files carry the decisions and the measurements.

Repository layout: `crates/np-core` (discovery, parsing, links, catalog, database, embeddings, media, search) and `crates/np-cli`
(argv, resolution, printers — no decisions of its own), with `third_party/`, `scripts/` and the pinned `.vendor/` tree for C++.

## License

MIT — [`LICENSE`](LICENSE), ruled on 2026-12-08 (closing F47). Both release archives carry it next to the binary, and
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md) carries the verbatim MIT text for the llama.cpp / ggml code statically linked in,
plus exactly which archives are linked — because that obligation attaches to the artefact you download, not to the repository it came
from. Model weights are a separate matter and are not redistributed: `embeddinggemma-2` is fetched on your explicit action,
digest-verified, and its own license governs it. [The long version, including the copyright line's status](docs/licensing.md).
