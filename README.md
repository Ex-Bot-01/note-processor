# note-processor

A local-first index for a Markdown vault. `np` scans a folder of notes once and keeps a SQLite catalog that answers,
without re-reading the corpus: what is in this vault, which files changed since last time, where every link resolves,
which notes are ambiguous or unreadable — and *why* each row says what it says.

Nothing leaves the machine. There is no telemetry, no sync, and no network call in any code path that exists today.

## Status

Built and tested: the scan pipeline (content-hash change detection, FTS5 search index, link resolution with recorded
ambiguity), durable upgrade/refusal rules for damaged or unreadable notes, byte-exact cross-platform paths, and the
M2a ground for embeddings — deterministic chunking, `chunks`/`embeddings`/`models` storage, a model install command that
verifies what it is given.

**Not built yet: anything that embeds.** The tables are there, no vector is ever written, so semantic search does not
exist. That is the next round (see below).

## Commands

| Command | What it does |
| --- | --- |
| `np scan <workspace>` | Index a vault: detect what changed, re-parse and re-index it, report counts as text or `--json` |
| `np models install <workspace> --id … --path … --dim … --sha256 …` | Put a model artifact under the workspace's model directory, verifying the digest before and after |
| `np models hash <file>` | Print the SHA-256 of a file, in the format `install` expects |
| `np models list <workspace>` | Show each model row and whether its artifact is actually present and matches |
| `np models fetch <workspace> --id … [--member …] [--discard-marker]` | Download a **pinned** model's artifacts from the configured origin and record them — `fetch` is the only command that dials out, it accepts only bytes that hash to what this build pins, and `[models] base_url` (or `NP_MODELS_BASE_URL`) says where to ask. Offline? `install --path` still works |
| `np embed <workspace>` | Embed what is owed for the configured model and write it to the database — a `local-model` build first checks the artifact against its recorded digest; `--json` reports the same four labelled outcomes this command exits with |

Refusals are deliberate: an unreadable note is reported and left alone, never repaired; an install whose digest does not
match is refused before a byte is copied; an artifact that disagrees with its own row is reported, not overwritten.

## Where the thinking lives

- [`docs/plan-2026-10-28-state-and-roadmap.md`](docs/plan-2026-10-28-state-and-roadmap.md) — what exists today and the
  order the rest arrives in. Start here.
- [`docs/superpowers/specs/2026-10-23-note-processor-design.md`](docs/superpowers/specs/2026-10-23-note-processor-design.md)
  — the original design, including the refusal rules that still govern every later change.
- [`docs/design-m2-embeddings.md`](docs/design-m2-embeddings.md) and
  [`docs/design-m2-models-install.md`](docs/design-m2-models-install.md) — the M2a decisions, each one carrying the
  measurement or failure that produced it.
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
