# note-processor — handoff / todo

**Read this first, then read the two documents below in order. Do not re-brainstorm: the design is
approved by the human (2026-10-23) and the M1 plan is written and self-reviewed.**

Repo: `/home/agent/note-processor` · remote `https://github.com/Ex-Bot-01/note-processor` (private,
push works — `gh` is authenticated as `Ex-Bot-01`, git identity is set repo-local)

1. **Design (approved):** `docs/superpowers/specs/2026-10-23-note-processor-design.md`
2. **Plan to execute now:** `docs/superpowers/plans/2026-10-23-note-processor-m1-core-catalog.md`

## What this product is (one paragraph)

A single-user, local-first markdown workspace for ~1,500 messy existing notes plus new **daily
files**. The human writes fast and out of order and cannot find things or tell what is still open.
Two jobs: make the pile navigable (deterministic `INDEX.md` catalog + graph of links/tags/clusters,
no API in that path) and make status visible (open/done/paused **item timelines**, latest written
text wins; each new day opens with a fenced TODO segment generated from the last *n* days by a
self-hosted OpenAI-compatible endpoint). A chat panel answers questions from the index/graph with
`file:line` citations and shows **both** sides of any contradiction, unresolved.

## Do now — Milestone 1

Use **superpowers:subagent-driven-development** on the M1 plan (the human chose it): a fresh
subagent implements each task, a fresh reviewer gates it before the next starts, whole-branch
review at the end. Read both documents before dispatching anything; hand each subagent the spec path
**and** the exact task text — executors see only their own task.

M1 deliverable: `np-core` + `np scan <workspace>` producing a byte-stable catalog. Nothing else.

- [ ] Task 1 — Cargo workspace, `np` binary, ubuntu+windows CI (**Step 0 installs Rust: it is not installed in this environment**)
- [ ] Task 2 — config file (XDG/APPDATA), data dir, known-workspace registry
- [ ] Task 3 — deterministic recursive walker, exclusions, Windows long-path helper
- [ ] Task 4 — content hash + change classification (Added/Modified/Moved/Removed)
- [ ] Task 5 — SQLite schema, move keeps identity, delete marks `stale`
- [ ] Task 6 — markdown parse: frontmatter subset, title precedence, headings, tags
- [ ] Task 7 — link extraction + ambiguity-safe resolution
- [ ] Task 8 — item candidates + configurable open/done/paused markers
- [ ] Task 9 — byte-stable `INDEX.md` render, wrapping at 100 cols, shards at 600 entries
- [ ] Task 10 — `np scan` end-to-end, idempotency property test, fixture corpus

Branch: create `m1/core-catalog`, push as you go, and when M1 is green ask the human whether to
squash-merge into `main` or open a PR (they have not chosen yet — do not decide for them).

**Definition of done for M1:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test --workspace` and `cargo test --release -- --ignored` all pass on Linux **and** Windows
CI, and `cargo run -p np-cli --bin np -- scan tests/fixtures/corpus --json` prints a report whose
second run reports zero changes with byte-identical `INDEX.md`.

## Locked decisions — do not revisit

- Shape: **daemon owns everything, Tauri window is a dumb client** (`np-core` / `npd` / `np-app`).
  Login autostart so today's file and TODO segment exist before the app opens. Linux **and** Windows.
- Single machine, single user. Loopback HTTP + token in config (port 8765) — no cloud, no accounts.
- Embeddings: self-hosted `google/embeddinggemma-2` **text tower only** (Apache-2.0), 512-d via MRL
  truncation, mean-pool + L2-normalise, task prefixes per the model card; downloaded once by
  `np models install`, sha256-pinned, recorded on every vector. Fallbacks in spec §9 (MiniLM
  multilingual / GGUF Q8 / endpoint embeddings) are one config key each. M2 starts with a throughput
  benchmark before committing.
- Only four things may ever be written into the workspace: create today's daily file, replace the
  fenced `<!-- np:todo -->` region, append approved `[[wikilink]]` under `## Related`, regenerate
  `INDEX.md` (+ shards) and `processed/`. Note bodies are never touched by an LLM.
- Status: **open is the default** (no marker), a tick means done; markers are config with German and
  English defaults (spec §8.7). Item identity across files merges only on an anchor; fuzzy matches
  go to the review queue as suggestions, never as assertions.
- Derived data lives in the DB and `INDEX.md`, one catalog entry per note, **no importance filter**.
- Frontend: React + Vite + TypeScript + CodeMirror 6 (M5).

## Environment facts

- Rust: **not installed** — run Task 1 Step 0 (`rustup` minimal stable + `clippy`, `rustfmt`).
- Node v22.22.1 + npm 9 present (needed from M5). `git` 2.53, `gh` 2.103 authenticated.
- This harness: the `subagent` tool exists in a fresh session (verified 2026-10-23) — use it.
- Never put the GitHub token in a command line or a file; `$GH_TOKEN` is already wired for `gh`.

## Open items to ask the human about, at the milestone that needs them

- **M2/M4:** base URL + model id of their OpenAI-compatible endpoint (currently Qwen3-Next-class on
  their server) and how they want the key stored. Nothing in M1 calls it.
- **M3:** where daily files live inside the workspace (`YYYY-MM-DD.md` at root vs `daily/`).
- **M4:** autostart specifics once you know the actual Linux distro (systemd user unit) and Windows
  edition (logon Scheduled Task).
- Later milestones get their own plan written from spec §13 — one plan per milestone, same cadence.

## Working notes the human gave along the way

They want to be asked rather than assumed when a choice changes what gets built; they like plain,
concrete answers; their notes are mixed German/English; the original idea of a task tracker and voice
memos stays out of v1 (kept as `SourceAdapter` trait seam only).
