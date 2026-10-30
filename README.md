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

Default builds contain no C++ and no inference runtime. The `local-model` feature on `np-core` (and a matching passthrough
on `np-cli`) builds llama.cpp from the sha256-pinned source under `third_party/`, is **Linux-only by refusal** in `build.rs`,
and is off in releases; it needs `cmake` (set `CMAKE=/path/to/cmake`) and nothing else new. Nothing embeds yet: no command
writes a vector, which is what the next round is for.```

CI is **configured** to build and test on Linux and Windows on every push. Read that as configuration rather than
evidence: no job has started since 2026-10-29 (an account-side Actions problem), so today every measured claim in this
repo comes from one Linux host, and anything else is a written refusal rather than a tested platform. Release binaries are built for Linux, Windows and macOS; the
macOS artefact is compiled, not run, so treat its behaviour as unverified until someone runs it.
