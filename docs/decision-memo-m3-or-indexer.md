# Decision memo — build M3, or keep `np` an indexer

Status: **for the human's ruling**, written 2026-11-20. No code was written for this memo and no existing file was
changed. Every claim names a file that was opened; anything not measured here or read in the repository is marked
*unmeasured*.

## 1. The question

`todo.md:19-26` describes a product with two jobs — make a messy pile navigable, and make status visible — where the second
job means daily files, open/done/paused item timelines whose latest written text wins, a fenced TODO segment generated each
day from the last *n* days by a self-hosted OpenAI-compatible endpoint, and a chat panel answering questions from the
index with `file:line` citations and both sides of any contradiction. What ships is the first job and part of its
neighbour: index, catalog, link graph, hybrid search, embeddings behind a consent door, and a JSON-RPC surface for an app
(`README.md:137-142`, `docs/reference-serve.md:71-80`). The gap has been treated as a backlog item, but it is not one: the
remaining features all require np to write inside someone's note folder, and that is a change in what the product *is*,
not another card to close.

One naming hazard, because it will cost an hour if it is not said out loud: `docs/design-m3-hybrid-search.md` is titled M3
and was "decided 2026-11-07, implemented in the same round" (`:1-4`). Hybrid search shipped. The M3 this memo is about is
the one in `todo.md` — daily files, timelines, generated TODO segment, chat panel — and none of it exists.

## 2. What is already paid for

Everything below is what an M3 would build on, and it is most of the hard part of the navigation job:

- **The index and its identity rules.** `crates/np-core/src/walk.rs` (byte-exact identity, display-folded keys),
  `scan.rs` (the write pass, the report, `notes` vs `attachments`), `hash.rs`, `path_keys.rs`. Re-running is cheap and
  idempotent because identity is bytes, not paths.
- **The catalog.** `catalog.rs` renders entries; `index_names.rs` decides shard names and ranges by the first character of
  the case-folded title, with split buckets (`#2`, `-2`) when one bucket exceeds `max_entries_per_file`. Deterministic, no
  API in that path — the property `todo.md:21-22` insists on.
- **The link graph.** `links.rs` resolves wiki and markdown targets and classifies them by extension (`:32`, `:373`);
  `serve` exposes `edges`, `unresolved` and `indexed` so a client can draw "what links here" without guessing whether an
  empty answer means "nothing" or "never scanned" (`docs/reference-serve.md:116-118`).
- **Chunking and vectors.** `chunk.rs` cuts on blank lines with no dependence on markdown structure; `embed_run.rs` does
  the owed work; consent has a door (`docs/design-f50-consent-has-a-door.md`) that writes two config keys and moves no
  bytes (`docs/reference-serve.md:88`).
- **Ranking.** `search.rs` fuses FTS5 `bm25()` with vectors by reciprocal rank fusion, keyword-only or vector-only on
  request, prefix matching included (`README.md:139`).
- **A protocol an app can drive**, with progress notifications that name their own phase and a terminal line per request
  (`docs/reference-serve.md:264-291`), plus diagnostics that stay off until asked for.

**What np writes today**: its index, `INDEX.md` and shards in the workspace, vectors and model artifacts under its data
dir, and two keys of its own `config.toml` (`README.md:137-142`, `:142`). It never writes a note file. The constraint that
governs any M3 work is recorded as a locked decision at `todo.md:361-363`:

> Only four things may ever be written into the workspace: create today's daily file, replace the fenced
> `<!-- np:todo -->` region, append approved `[[wikilink]]` under `## Related`, regenerate `INDEX.md` (+ shards) and
> `processed/`. Note bodies are never touched by an LLM.

Three of those four already happen. M3 is the moment the other two stop being hypothetical.

## 3. Three options

### Option A — stay a read-only indexer, and let a client do the writing

**Build:** nothing that writes a note. Finish the read side instead — make sure every answer a status view needs is
retrievable (np already parses status markers: `MarkerConfig` in `crates/np-core/src/config.rs`, with German and English
defaults per `todo.md:364-366`), version the `serve` protocol so a client can depend on it, and document what np will not
do in `docs/reference-*.md` rather than in a client's README.

**Cost: small.** Mostly API completeness and documentation. **Risk: low**, and the risk that does exist is a product
question, not an engineering one: "make status visible" then belongs to whoever renders it, and two clients can disagree.
**Unlocks:** a client owns its own undo. A daily file or a TODO segment written by the app can be undone by the app's
editor; the same bytes written by a background indexer cannot be, because the person's last save is not in that
transaction.

### Option B — build only the daily-file half

**Build:** create today's file if absent; regenerate the fenced `<!-- np:todo -->` segment from the last *n* days; the
endpoint plumbing to do it; a dry run that prints the exact bytes; and a report naming which files changed, in the shape
F50 already established for config writes (`changed` as "the keys np wrote", `docs/reference-serve.md:156`). Tests must
prove two things above all: an unchanged day rewrites nothing, and an edit a person makes *inside* the fence survives the
next run only as far as the fence's contract allows — which is to say it gets replaced, and that must be said in the
document and the report, not discovered.

**Cost: medium.** The generator is small; the write discipline is not. What has to be built honestly is idempotence under
a sync tool, a second editor with the file open, CRLF files, read-only directories, and a machine whose clock is wrong —
none of which np has had to care about for note files before, because it never opened one for writing. **Risk: medium, and
concentrated in one place.** The first corrupted note file costs more trust than the feature earns back, and the failure is
silent by construction: a person finds out weeks later that this morning's paragraph is gone. Mitigations are not optional —
write-and-rename, refuse rather than guess on any byte-level surprise, keep the previous segment, and never run at boot
without an explicit opt-in (`todo.md:346` currently assumes autostart creates today's file; that assumption deserves to be
re-examined before it is built). **Unlocks:** the second half of `todo.md:19-26` starts existing, and a client like Jott
gets "open today" for free instead of implementing it twice.

### Option C — build M3 as described

**Build:** B, plus item timelines (identity merging only on an anchor, fuzzy matches to a review queue as suggestions —
`todo.md:365-366`), plus the chat panel with citations and both sides of a contradiction.

**Cost: large.** The timeline work is a data model over notes that people edit out of order; the chat panel is a retrieval
product. **Risk: high, and one part of it is unmeasurable today.** `docs/design-m3-hybrid-search.md:5-6` already declines to
claim precision because F31's harness has no labelled query set — that argument applies with far more force to generated
prose that cites `file:line`: an answer that reads well and cites a real line is exactly the failure mode a citation cannot
catch. Marking this *unmeasured*: nothing in this repository can currently say whether a chat panel would be right often
enough to be worth its trust cost. **Unlocks:** the whole original vision, and the largest surface for getting it wrong.

## 4. The open sub-question: where daily files live

`todo.md:390-391` records this as needed "before M3's design, not before", so it is a decision to make with Option B or C,
not before either.

**Root `YYYY-MM-DD.md`.** Shortest path, and the convention several note apps use. Costs: a vault that already keeps date
files somewhere else now has two conventions for the same thing; a root file named `2026-11-20.md` collides with any
existing note of that name — np treats every `.md` as a note, so the collision is not refused, it becomes two entries whose
titles come from their first heading or filename (`parse.rs:115`); and 365 files starting with `2` land in one catalog
bucket, which shard splitting handles but which concentrates a year of writing into the same few shards.

**`daily/YYYY-MM-DD.md`.** Costs: one more path segment, and it is not what some apps expect. Gains: it never overwrites an
existing convention; it says what the file is to anyone browsing the folder in five years; a person who already keeps `daily/`
gets their own layout honoured rather than a second one; and a prefix gives np (and any client) something to filter, exclude
or treat specially later without guessing from filenames.

**My pick: `daily/`.** The reason is the same rule that made `attachments` a count instead of an inference — np should not be
the reason a folder grows two conventions for one kind of thing. **What would change my mind:** evidence about real folders.
If most target users keep date files at the root and have no `daily/` directory, then `daily/` is a preference imposed on
them, and the honest answer is to make it configurable — which is a third option this memo deliberately does not raise as a
default, because a config key that changes where new notes appear is the kind of choice a person should not have to
understand in order to use the product.

## 5. What I would need to know to decide

1. **Whose writes are these?** Should np ever be the last writer of a byte inside a note file, or is that a client's job for
   as long as np is a library other apps build on? (Ten minutes: read `todo.md:361-363` and answer yes or no.)
2. **What do the real folders look like?** Of the target vaults, how many already have a `daily/` directory, and how many
   hold date-named files at the root? (Ten minutes with Jott's own folder list — this decides §4 outright.)
3. **Is there any labelled query set?** Without one, "chat answers with citations" cannot be graded before it ships; F31's
   harness exists and is waiting for exactly that input. (Ten minutes: check whether any labelled set was ever assembled.)
4. **What is the autostart assumption now?** `todo.md:346` says today's file and TODO segment should exist before the app
   opens, on Linux and Windows. Does that still mean an unattended write at boot? (Ten minutes to rule.)
5. **What does Jott need first, in its own words?** A daily file it can open, or a status view it can render from what np
   already answers read-only? (Ten minutes asking whoever maintains it — this single answer probably chooses between A and B.)

## 6. What this memo is not

Not a design: no schema, no endpoint contract, no file format for a daily note. Not a schedule. And not a decision — the
plan that commissioned it says the direction call belongs to the human, and §5 lists what would sharpen it.
