# note-processor — handoff

**Read this first.** The design is approved by the human (2026-10-23); M1, the M2-hardening line and M2's local-embedding
round are shipped, through `v0.6.0`. Do not re-brainstorm an approved design: execute against the roadmap, or write a
design and get it reviewed before writing code for something new (`AGENTS.md`).

This file holds three things and nothing else: **what is true now**, **what a human owes**, and **the next three actions**.
Everything that was here and is not one of those has a home now, and no fact was dropped on the way:

| Where it went | What lives there |
| --- | --- |
| `release-notes/` | Each shipped release, plus `worklog-2026-10-30-to-2026-11-19.md`: the rulings as they came back, the rounds that merged, the measurements taken while a card was open |
| `docs/adr/` | Every locked decision, one per file, indexed in `docs/adr/README.md` — including ADR 0009, how work is done here |
| `docs/backlog-m2-followups.md` | Deferred engineering cards and their closure conditions |
| `docs/design-*.md`, `docs/measurement-*.md` | Designs as they were argued, measurements as they were taken |
| `AGENTS.md` | The conventions an agent follows opening this tree cold |
| `docs/README.md` | Every document, sorted into reference, designs and decisions, and history |

## What this product is (one paragraph)

A single-user, local-first markdown workspace for ~1,500 messy existing notes plus new **daily files**. The
human writes fast and out of order and cannot find things or tell what is still open. Two jobs: make the pile
navigable (deterministic `INDEX.md` catalog + graph of links/tags/clusters, no API in that path) and make
status visible (open/done/paused **item timelines**, latest written text wins; each new day opens with a fenced
TODO segment generated from the last *n* days by a self-hosted OpenAI-compatible endpoint). A chat panel answers
questions from the index/graph with `file:line` citations and shows **both** sides of any contradiction,
unresolved.

## Where things stand (2026-11-20)

* **Shipped through `v0.6.0`** (2026-11-19): scan and index with byte-exact identity, the deterministic catalog and its
  shards, the link graph, hybrid search, local embeddings behind a consent door, image vectors, `np serve` with progress
  notifications, diagnostics that stay off until asked for, and packaged releases. Notes: `release-notes/v0.4.0.md`,
  `v0.5.0.md`, `v0.5.1.md`, `v0.6.0.md`.
* **The second job has not started.** Everything above is the navigation half. The status half — daily files, item
  timelines, the generated TODO segment, the chat panel — needs np to write inside a note file for the first time, which
  is why it is a decision rather than a task: `docs/decision-memo-m3-or-indexer.md`.
* **Grading.** `.github/scripts/gates.sh` is the evidence a claim is made on; hosted CI runs Linux on `main` and the rest
  nightly, per pull request or on dispatch (`docs/developing.md`). Local green is not closure of a CI-graded claim.

## Open decisions — a human's call, not an agent's

1. **Build M3, or keep `np` an indexer.** Memo with three options, costs and risks:
   `docs/decision-memo-m3-or-indexer.md`. It answers the sub-question "`YYYY-MM-DD.md` at the root vs `daily/`" with
   reasons; the ruling is what makes it a plan.
2. **Should a `.txt` be a searchable note** (`[index] note_suffixes`, left open by issue #12)? Design:
   `docs/design-i12-note-suffixes.md`. No code exists and none should be written before the ruling.
3. **F25, filed and unadjudicated**: an over-wide token renders a physical line holding only `-`; re-measured 2026-11-06
   at `2c65525`, the shape still renders and R2 still refuses it. Two ways out, both costing a decision: move the bullet
   onto the over-wide token (golden-affecting), or keep the renderer and amend R2 plus a deliberate golden.
4. **CI's shape is money, so it stays a human decision.** Either raise the hosted-minute limit, or keep Windows and both
   C++ feature rows off every merge and cache the vendor build. Until then: count a job's `steps` before believing a red
   main (`docs/developing.md`).
5. **Is the comment-ratio target (0.35) still the goal?** Measured 2026-11-20 across `crates/*/src`: 13,396 comment lines to
   18,651 code lines (**0.72**), and hitting 0.35 means shedding **6,869 comment lines**. Narration is not where they are:
   whole-line history that can be deleted safely totals **two lines**, and the 1,327 lines carrying a ticket or `§` reference
   hold it *mid-sentence*, so removing the marker rewrites the sentence rather than shortening the file. The arithmetic lands on
   compressing genuine explanation — `db.rs` by 57%, `config.rs` by 62%, and `llama_ffi.rs` by **78%**, the file whose comments
   are where the pinned header's promises are written down (the reason `.github/scripts/check-llama-decls.py` can compare shapes
   but not intent). Either keep the ratio and accept that trade, or set the target to what narration plus real duplication can
   reach. Tranche 1 of the diet is merged (`Cargo.toml`, `ci.yml`) with a fence that proves prose-only diffs:
   `.github/scripts/check-comment-only.py`.
6. **F31 still owes ~20 labelled retrieval queries** from the real vault (`query<TAB>path` per line), or a recorded
   decline with the reason. Without them, recall can be measured and precision cannot — which is also the blocker on any
   chat-panel claim.

## Owed measurements — nothing may be claimed until one is run

* The **"chunks owed but unembedded" count** in the scan report — `ScanReport` carries no chunk counters at all today
  (`scan.rs:162-193`), so this starts with a short design, not a field.
* Backlog **D1–D10**, triaged against the tree on 2026-11-20 (`docs/backlog-m2-followups.md`): one closed (D9), seven open
  with a next action each, and two whose next move is a ruling rather than an hour of code — **D2** (is frontmatter part of the
  embedded text?) and **D5** (does the embed round delete stale vectors or compare them?). **D10** was restated: it names the
  wrong protection, and may be an orphaning write rather than a latent one. No test covers duplicate-healing next to chunks.
* **A4's A/B/A benchmark** and its whole-branch review, both skipped on instruction at merge time.
* **F18 residue:** Windows file identity (`VolumeSerialNumber` + `FileId`) needs real Windows hardware and a recorded run;
  CI proves only that the path is inert. No behaviour claim about a platform until it was run there.

## Next three actions

1. Rule on the two documents named above — they gate code, and one of them decides whether this project has a second job.
2. Pay the chunks-owed count in the scan report: it is the one number in `np embed`'s story that a person cannot ask for
   today. The definition is in the M2a backlog; the field, and what "owed" means once a chunker bump retires vectors, are not.
3. Settle F25 one way or the other, since both fixes touch goldens and neither should be chosen by whoever happens to be
   editing the renderer.

## Ask at the milestone that needs it

* `np` on a second machine: nothing here is machine-specific any more, but the release assets' visibility depends on the
  repository's, which is read from the host rather than remembered here.
* The front end (M5, ADR 0008) has never been started; its stack decision predates `np serve` (ruled 2026-11-09), so it
  should be re-read before any UI work, not assumed.
