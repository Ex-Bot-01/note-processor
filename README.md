# note-processor

A local-first index for a Markdown vault. `np` scans a folder of notes once and keeps a SQLite catalog that answers,
without re-reading the corpus: what is in this vault, which files changed since last time, where every link resolves,
which notes are ambiguous or unreadable — and *why* each row says what it says.

Nothing leaves the machine except when you ask it to: there is no telemetry, no sync, and exactly one command that can
make a network call — `np models fetch`, documented under [Installing a model](#installing-a-model). `np scan` and
`np embed` reach no network at all.

## Status

Built and tested: the scan pipeline (content-hash change detection, FTS5 search index, link resolution with recorded
ambiguity), durable upgrade/refusal rules for damaged or unreadable notes, byte-exact cross-platform paths, and the
M2a ground for embeddings — deterministic chunking, `chunks`/`embeddings`/`models` storage, a model install command that
verifies what it is given.

**Built since that sentence was written:** the forward pass (`np embed` writes vectors and reports what it refused), a pinned
artifact fetched over HTTPS or installed from disk and verified against its recorded digest, `np search` ranking notes and image
vectors that exist, and a thread policy for the decode.

**The boundary that matters when you read those claims.** Embedding needs the `local-model` feature: the default binary links no
inference runtime, so there `np embed` reports that nothing was produced and `np search` refuses to turn the query into numbers —
both at exit 1, with a message naming what to change. Image vectors need one more feature (`vision`) which **the CLI cannot ask
for yet**: `np-cli` has no `vision` passthrough, so every shipped binary records figures and declines to encode them. That gap is
filed as F28 in `docs/backlog-m2-followups.md` rather than left for a reader to discover.

## Commands

| Command | What it does |
| --- | --- |
| `np scan <workspace>` | Index a vault: detect what changed, re-parse and re-index it, report counts as text or `--json` |
| `np models install <workspace> --id … --path … --dim … --sha256 …` | Put a model artifact under the workspace's model directory, verifying the digest before and after |
| `np models hash <file>` | Print the SHA-256 of a file, in the format `install` expects |
| `np models list <workspace>` | Show each model row and whether its artifact is actually present and matches |
| `np models fetch <workspace> --id … [--member …] [--discard-marker]` | Download a **pinned** model's artifacts and record them — `fetch` is the only command that dials out, and it accepts only bytes that hash to what this build pins. Where it asks: `[models] base_url`, else `NP_MODELS_BASE_URL` when the file leaves that key out, else the one origin baked into the pin table. Which members: the ones this binary can use — a build without image support fetches the primary artifact only, and asking it for `mmproj` is refused as missing image support. Offline? `install --path` still works |
| `np search <workspace> <query> [--limit N] [--json]` | Rank what is already indexed against a text query — notes and image vectors in **one** ranked list, highest cosine first; `--limit` defaults to 10. **Changes nothing about the index**: no vector, no stamp and no note row is written (opening a database that does not exist yet still creates it and runs migrations, exactly as `scan` and `embed` do). An index with no vectors at all prints one line and exits 0; vectors stored under a **different** model id are a refusal naming the id this run asked for. `--json` prints `hits`, every object carrying the same five keys — `score`, `kind`, `path`, `linked_from`, `source_note` |
| `np embed <workspace>` | Embed what is owed for the configured model and write it to the database — a `local-model` build first checks the artifact against its recorded digest; `--json` reports the same four labelled outcomes this command exits with |

Refusals are deliberate: an unreadable note is reported and left alone, never repaired; an install whose digest does not
match is refused before a byte is copied; an artifact that disagrees with its own row is reported, not overwritten.

## Installing a model

```sh
# 1. place the `np` binary on PATH
np models fetch <workspace> --id embeddinggemma-2   # the one outbound request; a text-only build fetches one file
np scan  <workspace>                                # index the vault
np embed <workspace>                                # write the vectors — needs a `local-model` build
```

**Read that last line's condition.** The binary you get from `cargo install` links no inference runtime, so steps 1–2 work
and step 3 reports that nothing was produced (exit 1), as does `np search`: there is nothing to turn your query into numbers.
The fetch still succeeds, which means a default build will happily download 309 MB it cannot open — build with
`--features local-model` when this machine is meant to embed.

`fetch` is what pulls the model files, so installation needs no config file. It makes **one kind of outbound request**:
a plain GET for each member it plans, against `https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main`
— where this build's pinned digests were read from. Point `[models] base_url` (or `NP_MODELS_BASE_URL`) at a mirror to
move that request, and on an air-gapped host skip `fetch` and use `np models install --path … --sha256 …`, which needs
no network at all.

Two things this command will not do: it never accepts bytes that hash to something other than what the build pins, and
**its own output** — every transfer refusal, progress line and report — names a class (`not found`, `timed out`) and never a
host or URL. A *config* refusal is deliberately different: an invalid `[models] base_url` quotes the value you wrote, because
telling you to fix a key without showing the offending value wastes the one thing a refusal is for. A text-only binary also refuses to download the 554 MB projector it cannot load; `--member` still spells the
whole plan when you are fetching on behalf of a different build.

## Images in the index

`np embed` reads `![alt](target)` out of each note's own bytes and embeds the local file it names — resolved relative to
**that note's** directory, inside the workspace, percent-decoded once. Identity is the content hash, so one diagram written
to five paths or copied to three folders gets **one vector** and one search hit; renaming a file re-points a row and does not
re-embed it. A figure is re-encoded when its bytes change. What that sentence does not cover: the change check is file size
plus whole-second mtime, so an in-place rewrite that preserves both — same length, same timestamp — keeps its old vector
until something else makes the pass read the file (backlog **F27**).

**Accepted formats: `.png`, `.jpg`, `.jpeg` — and the name is only half the test.** A `.svg`, `.gif`, `.bmp` or `.pdf` link
refuses as `unsupported-format` on its extension; a file with an accepted extension then has to *be* that format, judged from
its first bytes (a PNG signature, or a JPEG start-of-image marker). A `.png` whose payload is really BMP bytes is therefore
refused as `unsupported-format`, not embedded: the pinned decoder would have taken them, and `np search` would have reported
an `image` beside a path whose contents are none of the three promised formats. Convert vector figures to PNG if you want
them searchable — an SVG rasteriser is owed, not hidden.

Images get their own line on the `np embed` report and never change its exit code; every refusal below is a **count**, and
`--json` carries an `image_refusals` array of `[name, class]` pairs — the linking note's path when the target never resolved,
the figure's own workspace path otherwise — because "refused 3" alone cannot tell a format decision from a missing file:

```text
images     links 7 · embedded 4 · refused 3 (missing-target 1, unsupported-format 2)
```

| Class | What it means, and what to do |
| --- | --- |
| `missing-target` | The link names a file this run could not read at that path — absent, unopenable, or unreadable while being hashed. Fix the target or restore the file |
| `outside-workspace` | The target is absolute or escapes the workspace root (including through a symlink). Nothing outside the vault is read; move the figure in, or link it relatively |
| `unsupported-format` | Not `.png`/`.jpg`/`.jpeg`, or an accepted name whose bytes are not that raster. Re-export as PNG or JPEG |
| `undecodable` | Accepted name and signature, but the decode failed — a truncated or damaged file is the usual cause. Re-export the figure; nothing partial was stored |
| `over-budget` | The encode measured more image patches than the context window holds (8,192 tokens). **Not reachable with one image on this build** — a 4096×3072 PNG costs 1,064 tokens because the projector clamps its own grid — so seeing it means an unusually large figure or a changed artifact: shrink the image and re-run |
| `vision-unavailable` | This binary has no image code, or the installed model declares no projector. In every `np` binary built today **every** figure lands here: `np-cli` has no `vision` passthrough to ask for it (**F28**), so a shipped build counts links and encodes none |

## Where the thinking lives

- [`docs/plan-2026-10-28-state-and-roadmap.md`](docs/plan-2026-10-28-state-and-roadmap.md) — what exists today and the
  order the rest arrives in. Start here.
- [`docs/superpowers/specs/2026-10-23-note-processor-design.md`](docs/superpowers/specs/2026-10-23-note-processor-design.md)
  — the original design, including the refusal rules that still govern every later change.
- [`docs/design-m2-embeddings.md`](docs/design-m2-embeddings.md) and
  [`docs/design-m2-models-install.md`](docs/design-m2-models-install.md) — the M2a decisions, each one carrying the
  measurement or failure that produced it.
- [`docs/design-m2-local-embeddings.md`](docs/design-m2-local-embeddings.md) — the runtime: feature gates, the pinned
  llama.cpp, artifact identity, and every throughput figure this repository quotes.
- [`docs/design-m2-image-embeddings.md`](docs/design-m2-image-embeddings.md) — R7c: image vectors, `np search`, the format
  and refusal rules above, and §14's list of what is **not** measured.
- [`docs/roadmap-2026-10-27.md`](docs/roadmap-2026-10-27.md) · [`docs/backlog-m2-followups.md`](docs/backlog-m2-followups.md)
  — what is next, and the debt found along the way (D1–D10 from the M2a whole-branch review).

## Building

```sh
cargo build --workspace
cargo test --workspace
```

Default builds contain no C++ and no inference runtime. The `local-model` feature on `np-core` (and a matching passthrough
on `np-cli`) builds llama.cpp from the sha256-pinned source under `third_party/`, is **Linux-only by refusal** in `build.rs`,
and is off in releases; it needs `cmake` (set `CMAKE=/path/to/cmake`) and nothing else new.

Embedding runs ask llama.cpp for worker threads by a rule rather than by its built-in default (which is a compile-time
`4`, whatever the machine has): `[embeddings] threads = "auto"` — the default — chooses `min(cpus, max(min(cpus, 4),
80 %))`, so a 16-CPU host gets 12 and a 2-core laptop gets 2 instead of being asked to run four. An explicit
`threads = 4` restores the old behaviour exactly. The measured basis, the ceiling (32 workers on 16 CPUs is *slower* than
four) and what has not been measured are in `docs/design-m2-thread-policy.md`; `np embed` prints the count it used
(`threads 12 of 16 available (auto)`, `"workers"` under `--json`).

`np embed` works on both sides of that line, and the two answers differ on purpose. A `local-model` build checks the
artifact against `models.sha256` before it reads a note — one `stat` when the file has not moved since the last check, a
full hash when its size or mtime changed or no check has run yet — then loads the artifact on its **first batch** (not
while it is deciding whether anything is owed) and writes real vectors. The report's `digest` line says which of those
happened; "not re-read" means the bytes were not hashed this run, which detects changed files but not a crafted
timestamp. A default build has nothing to load, so every batch is answered `runtime not built` and §6's fourth arm
reports that nothing was produced. Neither writes a placeholder: an absent runtime, an absent artifact and an over-budget chunk are each reported
as the specific thing they are.

CI is **configured** to build and test on Linux and Windows on every push, and **it does not run**. Since 2026-10-29
GitHub creates each run and then finishes it in about three seconds with **zero steps and no log** — `gh run view --log`
answers `log not found`, and `GET /repos/…/actions/permissions` answers **404** — so no runner ever picks a job up. The
account is reported to have no GitHub Actions budget; our token cannot tell that from Actions being switched off, and
nothing here claims which it is (the green runs this repo had before that date are why we know the workflows themselves
work). Read the workflows as configuration, not evidence. Every measured claim in this repo therefore comes
from one Linux host, through the gate list in [`docs/plan-2026-10-28-state-and-roadmap.md`](docs/plan-2026-10-28-state-and-roadmap.md):
`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`, the no-`$HOME` config suite,
rustdoc with `--deny warnings`, the two CFG fences, and the catalogue golden check. Release binaries are built for Linux, Windows and macOS; the
macOS artefact is compiled, not run, so treat its behaviour as unverified until someone runs it.
