# The documents in this repository, sorted

Three groups: **reference** for using and building `np`, **designs and decisions** for why it is shaped this way, and
**history** for what already happened. If you came to change something, read the reference row that covers it, then the
design that decided it — a change that contradicts an ADR is a decision to reopen, not a detail to notice later.

## Reference: using `np`, and building it

| Document | What it answers |
| --- | --- |
| [`../README.md`](../README.md) | What `np` does, every command, and the first five minutes with a real folder |
| [`reference-cli-and-reports.md`](reference-cli-and-reports.md) | Every flag, every report field, and what each one means when it is empty |
| [`reference-config.md`](reference-config.md) | `config.toml`: every key, its default, and what refuses an unknown one |
| [`reference-serve.md`](reference-serve.md) | `np serve`: the JSON-RPC methods, progress notifications, and captured transcripts |
| [`reference-models.md`](reference-models.md) | Model artifacts: pinning, install, fetch, consent, and what a download costs |
| [`reference-images.md`](reference-images.md) | Image vectors: which formats, which limits, and why some are excluded |
| [`reference-diagnostics.md`](reference-diagnostics.md) | `-v`/`-vv`, `NP_LOG`, `[log] file`, and what a diagnostic never carries |
| [`reference-releases.md`](reference-releases.md) | Release assets, digests, cross-builds, and how to verify a download |
| [`reference-troubleshooting.md`](reference-troubleshooting.md) | A symptom first: what you saw, what it means, what to run |
| [`developing.md`](developing.md) | Local gates, what CI grades and when, and why the matrix is cut where it is |
| [`licensing.md`](licensing.md) | What this code is under, and what the vendored llama.cpp adds at build time |
| [`../AGENTS.md`](../AGENTS.md) | The conventions an agent follows opening this tree cold |

## Designs and decisions: why it is shaped this way

* **Decided** — [`adr/README.md`](adr/README.md) indexes one record per locked decision. An ADR settles; it does not argue
  for change.
* **Approved product design** — [`superpowers/specs/2026-10-23-note-processor-design.md`](superpowers/specs/2026-10-23-note-processor-design.md),
  with the later specs beside it (`np serve` over stdio, progress notifications, diagnostics). These are the designs the
  human approved; implementation plans that executed them are history.
* **Feature designs** — `design-*.md`, each naming the card it answered: `f*` for the hardening line (identity, paths,
  goldens, refusals, unknown keys, auto-fetch, consent), `m2-*`/`m3-*` for embeddings, image vectors and hybrid search.
  They keep their reasoning, including the parts that turned out wrong, because the wrong turn is often why the rule is
  worded that way.
* **Awaiting a ruling** — [`decision-memo-m3-or-indexer.md`](decision-memo-m3-or-indexer.md) (build M3, or stay an indexer)
  and [`design-i12-note-suffixes.md`](design-i12-note-suffixes.md) (should a `.txt` be a searchable note). Both are review
  gates: no code exists for either, and none should be written before the human answers.
* **Measured** — `measurement-*.md`: vector surface width, and the cost of diagnostics. A measurement names its method so
  someone can re-run it rather than trust it.
* **Deferred work** — [`backlog-m2-followups.md`](backlog-m2-followups.md) holds the open engineering cards and what each
  needs to close. The human's open decisions are in [`../todo.md`](../todo.md), not there.
* **Build operations** — [`ops-cross-build-windows-gnu.md`](ops-cross-build-windows-gnu.md) (how a Windows asset is built
  on Linux) and [`ops-distribution-and-ci-decision.md`](ops-distribution-and-ci-decision.md) (how releases get distributed,
  and what CI can afford).

## History: what already happened

* [`../release-notes/`](../release-notes/) — one file per release, plus
  `worklog-2026-10-30-to-2026-11-19.md`: the rulings as they came back and the rounds that shipped. Read it for the story;
  read the reference for the contract.
* [`archive/README.md`](archive/README.md) — the roadmap that was executed, the plan that sequenced it, and the
  implementation plans each round followed. Quoted, not linked: their paths are part of the record.

## Two conventions that keep this navigable

* **A document has one home.** A new design goes in `docs/`, a settled decision becomes an ADR, a shipped round goes to
  `release-notes/`, and `todo.md` stays short — it holds what is true now, what a human owes, and the next three actions.
* **Links are checked**, both spellings: `[text](target)` and a backticked path like `docs/reference-serve.md`. The fence is
  `.github/scripts/check-links.py`, run as a gate row, and it excludes exactly the shapes that were never promises — sample
  data, archived plans, syntax shown inside backticks.
