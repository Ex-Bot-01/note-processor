# F48 — the link graph becomes a query, and what a client may assume about link identity

**Status: built.** `np_core::links::listing` and the `links.list` method of `np serve`, shipped in the issue-#2 round.
Numbered here rather than in [`backlog-m2-followups.md`](backlog-m2-followups.md) because the decision arrived with the
implementation the reporter offered to write; F47 was the last number taken, so this took the next one.

## The gap

`np` computes the whole link graph on every scan — `extract_links` reads every prose line, `resolve` places each target or
refuses to, `store_links` writes an edge per link with its kind, its raw target and its line — and then kept all of it.
Seven methods answered over the pipe and none of them read `links`. The reporter (an editor being built on `np serve`)
named three features that want the same edge list: a **backlinks pane**, **fix-at-source** for an unresolved `[[target]]`
marked while it is still being typed, and **rename/move rewriting**.

Two fallbacks exist and both are bad in ways this repository has spent forty-odd cards arguing about.

* **Re-parse every note in the client.** Now there are two wikilink grammars, and they diverge quietly: np skips a link
  inside a fence, strips `|alias`, `#heading` and `^block`, refuses a destination with a scheme before its first `:`, and
  counts `[[Gamma]]` as unresolved when two files claim that name. None of that is visible to a client-side parser until
  a pane shows a backlink that np does not believe exists, which is a bug report nobody can reproduce.
* **Select from `<data_dir>/workspaces/<key>.sqlite`.** This works today — readers are never blocked by writers — and it
  builds a shipping application on `links.src_note_id`, a column whose meaning migrations have already reshaped twice
  (migration 3 made `dst_note_id` a foreign key that NULLs itself, migration 4 moved identity from `rel` to `rel_bytes`).

The nearest thing to an answer was `Hit::linked_notes`, which carries the linker set for **image** hits only, and
`np scan --json`'s `unresolved_targets`, which names dead targets but not where they were written — a count where
fix-at-source needs `(src, line)`.

## What a client may assume about link identity

This is the part that was worth writing down before the code, because every choice below outlives the method.

1. **A note is named by its display path, and never by a row id.** Every entry carries `src` and (when resolved) `dst` as
   the same spelling `np search` prints in a hit. `notes.id` is an implementation detail of this database: it survives a
   rename precisely so that links do not have to be rewritten (design-a4), which also means it means nothing outside the
   file. A client that stored ids would break on the first migration; a client that stores paths breaks only when the user
   moves a note, which is the breakage it can actually handle.
2. **A target is data about the text, not an address.** `target` is what the author typed — before case-folding, before
   basename resolution, before any decision np made about it. It is in every entry because it is the string a fix
   replaces, and it is *never* something to open: `[[Beta]]` means `deep/nested/Beta.md` only because np's resolver said
   so for this corpus on this scan.
3. **Scoping matches the byte key.** `params.rel` is compared against `notes.rel_bytes`, the identity F13 §4 put in place
   of display text. A client holding a path that came off a report holds those bytes already; one that typed or received a
   spelling goes through `RelPath::from_display`, which inherits F13's residual unchanged — two live rows whose raw names
   differ where UTF-8 cannot round-trip are addressed by their display spelling, not apart. That is a documented limit of
   text-addressed callers, not a new one introduced here.
4. **`line` is 1-based and ending-independent.** Same rule `crate::links::RawLink` has always held: the number is identical
   for an LF and a CRLF copy of the note, because the shared line model trims a trailing `\r` for content only. A marker
   drawn in an editor stays on the right line after one re-save.
5. **An edge's existence is a claim about the last scan, not about now.** Nothing here re-reads the vault. `np scan` owns
   freshness, and `indexed` says whether that has ever happened at all.

## The decisions

**`rel` names a subject, not a direction.** An edge appears when the named note is **either end** of it. The three
features need two different filters — a backlinks pane wants edges *arriving* at a note, fix-at-source wants dead links
*written in* it — and one parameter silently meaning "incoming" for `edges` and "outgoing" for `unresolved` is how a pane
ends up listing a note as its own backlink. Every entry names both of its ends, so the client states which one it wanted by
comparing `dst` to the path it asked about, and no fourth key had to be invented to say so. An unknown note answers two
empty lists; an empty or non-string `rel` is a fault, because that is a client that meant something and sent the wrong
thing (**F35**'s rule for configuration values, applied to protocol parameters).

**Prose origins only.** Every other reader of `links` reads prose — an image hit's linker set, a scan's unresolved list —
because an `approved` edge is a claim a human made about a note that may no longer exist. Nothing writes `approved` rows
yet; when **F13**'s approval path does, this read has to be revisited *as a decision*, not widened by one line.

**The source note has to be live.** `files.state = 'stale'` is history the database keeps on purpose, and a scan's
unresolved count still counts it ("their rows are history the report does not rewrite"). A pane a person clicks may not
name a file they cannot open, so this predicate is search's linker-set one rather than the author-facing count's. **The two
numbers can therefore disagree on a corpus with churn in it, and that is the correct disagreement**: one counts what was
written, this lists what can be opened. A test pins the stale-source case so the divergence is deliberate rather than
latent.

**A stale destination is unresolved**, by `db::LIVE_EDGE` — the same predicate behind `ScanReport`'s `unresolved_links`
count, so a count and the list printed under it cannot disagree about which edges are intact. Such an edge appears with its
target and no `dst`: the destination is the thing in question, and a path beside a dead target looks like somewhere to
click.

**Attachments appear in neither list, and that is a stated gap.** A figure's `links.target` is relative to the *linking
note's directory* and arrives in TEXT already lossy-decoded (§6 of the image design), so this table cannot say whether the
figure exists or where it moved. "Which notes embed this figure" is answered by `search`'s `linked_notes`, which re-runs
`media::link_target_rel` — the one rule that can. A rename tool that needs figure edges must go through that rule, or this
read grows a third bucket built on it; what it must not do is guess from `links.target`. The test asserts no attachment row
leaks into either list, so widening this is a visible change rather than an accident.

**`indexed` travels with the answer.** An empty pair of lists means two different things in a folder nobody has ever
scanned, and **issue #5** established that a report must say which: *"nothing links here"* and *"this folder is not
indexed"* are different sentences, and a rename-rewriting pass that reads the first into the second silently orphans every
note in the vault. So the document carries `indexed`, read from `scan::indexed` — the scan stamp plus rows, never file
existence, for the reason #5's own comment gives.

**A reader, in the strong sense.** No write lock, and **no database creation**: `status.get`'s rule (test for the file,
then open) rather than `models list`'s, because a pane that polls would mint an index for every folder it hovered over and
add a writer to an embed already in flight. A workspace that is not a directory still refuses before anything is claimed
about it.

## Deliberately not done

* **No terminal twin.** `links.list` answers a question a person at a shell does not ask, so it has no CLI text channel —
  the second method after `status.get` to be built that way. If someone wants `np links`, the document builder is already
  shared and the core read takes a scope argument; what must not happen is a second implementation of either.
* **No limits, no pagination.** Unscoped means the whole graph, which is what the rename pass asked for. Scoping by `rel`
  is the cheap path and is what a pane should call; nothing here pretends a 10⁵-edge corpus will not be a large response.
* **No approved edges, no figure edges, no per-note counts** — see above; each is a decision with its own trade-off, not a
  line to delete.
