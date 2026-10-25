# note-processor — design

Date: 2026-10-23
Status: for approval (review round answered)
Repo: https://github.com/Ex-Bot-01/note-processor

## 1. Intent and the why

I take notes at work in markdown, fast and out of order, mostly during meetings. I append to
files I already wrote instead of starting new ones. Weeks later two things fail: I cannot find
what is in the pile, and I cannot tell whether a topic is **finished or still live**. The note
that says "ask Ingrid for the vendor contract" is one line among hundreds; the sentence that
closes it is written three days later in a different file.

note-processor is a single-user, local-first workspace for that pile. It does two jobs:

1. **Make the pile navigable** — a deterministic catalog (`INDEX.md`) and a graph of notes,
   links, tags and clusters, built by code with no API and no network in the path.
2. **Make status visible** — open/done items tracked as a *timeline* (latest written text wins),
   and each new day opened with what is still unresolved from the last *n* days.

Success looks like: 87% of notes untagged → every note has tags/keywords and at least one
connection; "what is still open?" answered in under a second from `INDEX.md` or the UI; and no
word invented about my own notes.

## 2. Goals

- Point the app at a folder; everything inside it, recursively, is tracked — including the
  ~1,500 existing heterogeneous notes, with no migration step and no reformatting of them.
- Change detection by name + timestamp + size as trigger, content hash as identity: new file →
  processed and inserted into the existing graph; changed file → re-ingested and every entry
  that pointed at it updated; moved file → identity and history preserved; deleted file →
  entries marked stale, not destroyed.
- `INDEX.md`: one entry per note — title, path, size, tags, related words, links, cluster, status
  summary — alphabetized, machine-generated, safe to hand an LLM for an overview. One line by
  default, wrapping to indented continuation lines when the related words need the room, and the
  catalog shards into `INDEX-<range>.md` past a configurable size with `INDEX.md` as entry point
  (§9). No importance filter: nothing is silently hidden from the overview.
- A compressed, grounded copy of every note under `processed/` — the clean short version that
  feeds its graph node, while the messy original stays the ground truth (§8.7).
- Daily notes: opening a day creates `YYYY-MM-DD.md` if absent; a fenced TODO segment at the
  top is generated from the last *n* days (default 30); I write messy underneath; saving
  triggers processing of that file.
- Item timelines: an item records first written, last touched, current state (**open** by default,
  **done**, **paused**, or **superseded**) and `file:line` evidence for each transition. Open is
  the absence of a marker: anything I have not marked finished or paused is still open. State
  changes only on text I wrote (§8.7).
- Review queue with evidence: probable-same-project clusters, orphans with nearest neighbours,
  near-duplicates. Approving writes a real `[[wikilink]]`; after that the result is
  deterministic.
- Chat panel (closed by default) answering from index + graph, citing `file:line` for every
  claim, and when sources disagree naming the contradiction and showing **both** places and
  contents, unresolved.
- Runs on one machine, Linux and Windows both first-class. Boot-time autostart so today's file
  and TODO segment already exist before I open a window.

## 3. Non-goals (v1)

- Task-tracker product: due dates, assignments, reminders, a task table whose identity survives
  arbitrary edits, write-back into note bodies beyond the two cases in §7.
- Voice memos / transcription. Ingest keeps a `SourceAdapter` trait seam so a future adapter can
  emit a `.md` file and join the normal pipeline.
- Restructuring the originals. The messy file is never reflowed, retitled or summarised in place;
  compression happens only into a separate derived file under `processed/` (§8.7). A structural
  lint (missing heading, orphan list item, broken link target) reports in the UI.
- Multi-device sync, accounts, mobile, cloud storage of any kind.
- A plugin runtime. Only the trait seams (`SourceAdapter`, cluster/scoring functions) exist.
- Obsidian compatibility beyond plain `[[wikilink]]` and standard frontmatter being readable.

## 4. Anti-goals (failure even if it technically works)

- Confidently lying about status ("finished" inferred from vague prose).
- Silently rewriting or reflowing my note text.
- A graph edge, keyword or cluster with no evidence link back to `file:line`.
- Requiring the network — or the LLM endpoint — in order to write or index notes.
- Making me change how I write, rename old files, or add frontmatter by hand.
- Derived data drifting into the corpus it describes (generated text read back as my input).

## 5. Constraints

- Corpus: ~1,500 existing `.md` notes (~18k chunks), growing one daily file per working day.
- **Notes are mixed German and English**, so every embedding model and keyword heuristic has to be
  multilingual. Chosen model: `google/embeddinggemma-2` text tower (Apache-2.0), self-hosted —
  downloaded once by `np models install`, no API at index time (§9). Vectors: 512-dim via MRL
  truncation, q8 ≈ 19 MB for ~18k chunks; brute-force cosine scan in RAM, this is not a
  vector-search-infrastructure problem. Throughput is the one real cost of that model and is
  measured before we commit (§13 step 2).
- Weights are never committed to the repo; `np models install` fetches and verifies sha256.
- LLM: self-hosted OpenAI-compatible endpoint only (currently Qwen3-Next-class on my server).
  Model id, base URL and key are config; no vendor SDK in the tree. `embeddings.provider` may be
  pointed at that endpoint's `/v1/embeddings` instead of the local model — one config flag, and
  the trade is spelled out in §9.
- No LLM, no network in the indexing path. Catalog quality must not depend on the endpoint.
- Single machine, single user, but the API is loopback HTTP so `curl` and tests work.
- Linux **and** Windows: file watching, paths, line endings, autostart all differ (§11).
- Toolchain note: Rust is not installed in this environment; `rustup` + stable is a setup step.

## 6. Approaches considered

**A. Local daemon + browser UI.** One Rust binary serves API and built frontend on 127.0.0.1;
it owns watcher, registry, DB, embeddings. *For:* one process owns all state; core stays a plain
crate with no HTTP/UI so it is fully unit-testable; frontend is an ordinary web app; indexing
keeps running when the window closes; `curl`-testable everywhere. *Against:* no native folder
picker in a browser tab; something must be started at login.

**B. Tauri desktop app with the core inside it.** *For:* native window and picker, no local port.
*Against:* indexing only runs while the window is open — but both my workflows begin with a
filesystem event (a day rolling over, a file being saved), not a window event; plus webkit/gtk
build deps and a slow rebuild loop every day of development.

**C. CLI indexer + read-only generated dashboard.** *Ruled out by the requirements:* cannot
create the daily file, cannot generate the TODO segment on open, no chat panel — it delivers only
half of workflow 1 and none of its triggers.

**Chosen: A + B as a hybrid — the daemon owns everything, the window is dumb.** `npd` (daemon)
holds all state and does the morning work at boot; `np-app` (Tauri) contributes a native window
and one native feature, the folder picker, and spawns `npd` if it finds none running. This keeps
A's testability and always-on indexing and B's app feel, and costs two processes during
development — accepted because the shell is the last chunk of work and wraps a finished UI.

Sub-choices: embeddings run **locally in Rust** rather than via the endpoint's `/v1/embeddings`,
so indexing stays API-free and works with the server down (§9, my call). Frontend framework is
React + Vite + TypeScript (my call, best-supported markdown editor integration; §9).

## 7. What may ever be written into my files

| Write | Where | Rule |
|---|---|---|
| Create daily file `YYYY-MM-DD.md` | workspace | only if absent; body empty apart from the TODO segment |
| TODO segment between `<!-- np:todo:start -->` / `:end -->` markers | today's daily file | whole-region replace; nothing outside the markers is touched; ticks extracted before rewrite (§8.3) |
| Approved `[[wikilink]]` | the note I approved it in | appended under a `## Related` heading (created if absent), never inside existing lines |
| `INDEX.md` (+ `INDEX-<range>.md`) | workspace root | whole-file regenerate; excluded from tracking (§9) |
| Compressed copy `<relpath>` | `processed/` in workspace | whole-file regenerate from the current source hash only; derived, never an input (§8.7) |

Everything else derived — tags, keywords, clusters, entities, statuses, contradiction edges —
lives in the database and appears in `INDEX.md`/the UI. No LLM output ever lands inside a note
body: model text goes only into new files under `processed/`. No file is rewritten as a side effect of indexing; line endings and encoding are preserved
byte-for-byte outside the regions above.

## 8. Behaviour that was decided

### 8.1 Workflow 1 — everyday
Open app → today's file exists (created if missing) with the TODO segment at top, generated from
the last *n* days (default 30, configurable) → I write anything underneath, in any order,
appending as long as the file is open → something closes either in prose or by adding a tick/✅ to
the matching TODO line → save triggers processing of that file: parse items/links/tags, update
item timelines, re-cluster affected neighbourhood, regenerate `INDEX.md`.

The TODO prompt is fed **derived items, never raw prose**: open and recently-changed items with
their `file:line` provenance from the last *n* days, newest first, capped at a configurable token
budget (default ~6k) — 30 days of messy notes does not fit in a context window and reading it all
would be slower and worse. Fenced derived regions (`np:todo`) are stripped from that input, so
tomorrow's list is built from my items and not from yesterday's generated list.

### 8.2 Workflow 2 — search
A chat panel, closed by default, opens over the current view. It retrieves from index + graph,
answers with a `file:line` citation per claim, and states contradictions explicitly: *"these two
conflict"* followed by both paths, line ranges and quoted contents. The system never picks a
winner; newest-first is display order, not a verdict.

### 8.3 Status is a timeline, not a field
An item's state comes only from text present in files, resolved in write order — daily file date
first, then append/line order within a file. A tick landed on a generated TODO line closes the
**source** item that line points at (`(2026-10-21.md:L43)` provenance), not a phantom. Because
that evidence sits inside a region we regenerate, processing runs *before* regeneration: extract
ticks from the current segment into the referenced items' timelines, then rewrite the segment.

### 8.4 Item identity across files (my call — veto if wrong)
Two lines in different files are the *same* item only when an anchor says so, in this order:
(1) a tick on a generated TODO line, via that line's `(file:Lnn)` provenance; (2) an explicit
`[[wikilink]]` or repeated exact heading naming the same topic thread; (3) otherwise it is *not*
merged — the tick becomes an "unmatched completion" entry in the review queue with its nearest
candidates and their similarity, and my approval creates the link. Fuzzy matching proposes,
it never asserts, so a wrong guess costs me one click instead of silently closing an item.

### 8.5 Contradictions are edges
`contradicts(note_a_span, note_b_span, evidence_a, evidence_b, detected_by, detected_at)` stored
as data. The LLM may propose pairs; the deterministic layer checks cheap structural conflicts
(same topic thread, differing status marker or differing number/date). Rendering both sides is a
property of the data, not a favour from the model.

### 8.6 Endpoint failure is a normal state
Every LLM call has a timeout and retry policy. No endpoint means: today's file still created,
TODO segment filled with unresolved items carried verbatim with their source lines — never an
empty block, never an apology paragraph, never a fabricated list. Chat says it cannot answer
rather than answering from memory.

### 8.7 Compressed node summaries — grounded, gated, reversible
Every graph node carries a compressed version of its source note; the day file stays messy and
its compressed twin is what I read or hand to an LLM. Rules:

- **Bounded**: the summary may only restate what is in the source. It may reorder, merge duplicate
  mentions, and tighten wording; it may not add a fact, name, number, date or conclusion.
- **Nothing dropped silently**: every open/done/paused item and every `[[link]]`/tag/entity in the
  source must appear in the summary (items keep their provenance `(file:Lnn)`), otherwise the
  difference is listed under a `## Dropped` heading for me to see.
- **Gate before it is stored**: a deterministic check rejects any summary containing a capitalised
  entity, number or date absent from the source, and any item present in the source but neither
  summarised nor listed as dropped. One retry with a stricter prompt; if it still fails, keep the
  previous summary, mark the node `ungrounded` in the UI, and never overwrite good text with bad.
- **Target**: ≤ 35 % of source tokens (configurable), items exempt from compression pressure.
- The original is always the ground truth: the note card shows the compressed copy above the raw
  text with a one-click *open original at this line* on every summarised section.

**Status markers** (my default list, case-insensitive, extendable in config without a rebuild):
done — `✅`, `- [x]`, "done", "finished", "erledigt", "fertig", "abgeschlossen"; paused —
"paused", "pausiert", "on hold", "gestoppt", "verschoben", "vertagt"; open — everything else,
including explicit `- [ ]`. A marker line attaches to the item it names (§8.4).

## 9. Architecture (my call unless noted)

```
note-processor/
  crates/
    np-core/   # library: parse, hash, embed, cluster, items, registry, INDEX render
    npd/       # daemon binary: axum API + WebSocket events, notify watcher, scheduler
    np-app/    # Tauri thin client: webview + folder picker; spawns npd if not running
  ui/          # React + Vite + TypeScript, CodeMirror 6 editor
```

- **np-core**: no HTTP, no UI, no async task spawning. Everything testable with `cargo test` on
  fixture folders. Stages: discover → hash/dedupe → parse (frontmatter, headings, lists, links,
  tags, items) → chunk (~350 tokens ±overlap) → embed → cluster → index/render. A separate
  **enrich** queue (LLM compression, §8.7) runs after the deterministic pass, resumable and
  rate-limited: it can be down or offline and every deterministic surface stays complete.
- **Storage**: SQLite (`rusqlite`) in the OS data dir, one DB per workspace. Tables: `files`
  (path, display path, hash, size, mtime, state), `notes`, `headings`, `chunks` (q8 vector blob),
  `items` (+`item_events` for first/touched/done/superseded with `file:line`), `links`
  (typed, `approved` flag), `tags`, `clusters` (+ membership), `contradictions`, `summaries`
  (note_id, model, source_hash, text, gate_result, created_at). FTS5 over note **and** summary
  text for keyword search; vectors scanned brute-force in RAM.
- **Embeddings — self-hosted `google/embeddinggemma-2`, text tower only.** Verified against the
  hub: 740M params total but 270M in the text tower (`language_model.*`, 413 tensors), 768-d native
  with Matryoshka truncation to 512/256/128 (we use **512**, re-normalised after slicing), mean
  pooling over token embeddings then L2-normalise, 8K-token context, Apache-2.0.
  - `np models install` downloads it once, slices out the `language_model.*` tensors plus tokenizer
    into the data dir (~540 MB bf16; the vision/audio towers we do not use stay in the download
    cache), pins sha256, and records model id + version on every vector.
  - Runtime: candle (`candle-transformers` 0.11 ships `gemma4` text layers). Its `TextModel`
    returns last-token logits for generation, so np-core carries a **text-only encoder wrapper** —
    all positions out of the final norm, mean-pool with the attention mask, normalise, slice to 512.
    The layer code is private in candle, so this wrapper is adapted from candle's Apache-2.0
    `gemma4` module and lives in np-core with the licence noted.
  - Fallbacks, in order, if that stalls: (a) GGUF Q8_0 (`ggml-org/embeddinggemma-2-GGUF`, 310 MB,
    text-only) through llama.cpp bindings — costs a C++ build on Windows, so it is not the default;
    (b) `paraphrase-multilingual-MiniLM-L12-v2` (118M, 384-d, ~7 MB total, runs on candle's
    well-trodden Bert path) if the benchmark shows EmbeddingGemma is too slow on my CPU;
    (c) `embeddings.provider = "openai-compatible"` for endpoint embeddings — cheapest CPU, but note
    text crosses the network and indexing needs the server up. Every fallback is one config key and
    a re-embed, never a code change.
  - **Task prefixes are part of correctness**: the model is trained with instruction prefixes,
    retrieval being asymmetric (query prefix on my questions, document prefix on stored chunks,
    `none` when there is no title) and clustering/similarity symmetric (same prefix both sides).
    Prefix text lives in config; queries and corpus must always share the same dimension.
  - Raw chunks are embedded for citation-grade retrieval; each node also gets one vector from its
    compressed summary, used for clustering and nearest-neighbour work (shorter text, less noise,
    and far fewer tokens than embedding summaries alone would need).
  - Keyword/tag derivation stays statistical, never model-based: existing tags from
    frontmatter/`#tags`, titles, link anchors, distinctiveness by vault-relative TF-IDF so keywords
    are what makes a note different, not what makes it sound like a note.
- **API** (`127.0.0.1:8765`, token in config file, `Origin` checked): `GET /api/index`,
  `GET /api/notes/{id}`, `POST /api/notes/{id}/links` (approve placement), `GET|PUT /api/file`,
  `POST /api/daily/today`, `POST /api/search` (chat/RAG, streams), `GET /api/graph`,
  `GET /api/queue`, `POST /api/browse` (server-side directory listing for browser mode),
  `GET /api/events` (WebSocket: file changes, ingest progress).
- **UI**: four surfaces — Index table (filter by tag/cluster/status/staleness), Note card
  (outline, lead, tags, entities, typed in/out links, stale-since badge), Review queue
  (suggestion + evidence + approve), Editor (CodeMirror 6; TODO segment read-only-fenced, prose
  free) with the chat panel docked and closed by default. TanStack Query for state, WebSocket for
  live updates, autosave debounced ~1.5 s with atomic write (temp + rename; retry-on-lock on
  Windows).
- **Scheduler**: on daemon start and at each date rollover — create today's file, extract ticks,
  regenerate the TODO segment, run ingest queue, regenerate `INDEX.md`.

## 10. Determinism and idempotency

Re-running the whole pipeline over unchanged files is a no-op: same hashes → no re-embed; render
is byte-stable given the DB (alphabetical order, fixed formatting, no timestamps in generated
lines except explicit provenance). Approved links are inputs, so a suggestion once approved never
reappears as a suggestion. Non-markdown files inside the workspace (images, PDFs) get a `files`
row — path, hash, size, mtime — and can be linked to and shown as attachments, but are not parsed
or embedded. One active workspace at a time; recently used workspaces listed for one-click switch,
each with its own DB. Day rollover uses local midnight. If a file changes on disk while open in
the editor the app does **not** overwrite: it shows a conflict banner with *reload*, *keep mine
(write anyway)* and *save as copy*. Model id + version recorded on every vector so a model change triggers
a controlled re-embed rather than mixed-space comparisons. A summary is regenerated only when its
source hash changes, so `processed/` is stable across re-runs and an endpoint outage never blanks
an existing compressed copy.

## 11. Cross-platform

`notify` for watching (inotify / ReadDirectoryChangesW). Paths normalised internally to `/`,
original spelling kept for display; case-insensitive comparison on Windows so `Notes/x.md` and
`notes/x.md` are one node; CRLF tolerated on read, never rewritten; `\\?\` long-path prefix above
260 chars. Config: `$XDG_CONFIG_HOME/note-processor/config.toml`, `%APPDATA%\note-processor\`.
Data dir: `$XDG_DATA_HOME` / `%LOCALAPPDATA%`. Auth by token (no Unix sockets). Autostart:
`note-processor install-autostart` writes a systemd **user** unit on Linux, a logon Scheduled Task
on Windows; `uninstall-autostart` reverses it.

## 12. Testing

Fixture vaults in `tests/fixtures/` (messy notes, appended text, ticks in prose and on TODO lines,
renames, deletes, CRLF files, unicode + umlaut names, a file >260-char path). Golden-file tests:
`INDEX.md` render, item timeline resolution, tick-extraction-before-regeneration order. Property
test: pipeline twice = byte-identical output. Cross-platform CI matrix (ubuntu-latest +
windows-latest) running `cargo test` and `npm run build`, with embeddings behind a trait and a
**deterministic stub embedder** in tests — CI never downloads 1.5 GB of weights or calls my LLM
endpoint. Real-model quality and throughput are measured by an offline bench binary on my machine,
not in CI. LLM behind a trait with a fake for
tests; one opt-in integration test against the real endpoint, skipped by default.

## 13. Build order

1. `np-core`: discover/hash/parse/registry + SQLite schema (fixtures, golden `INDEX.md`).
2. Embeddings: `np models install` + the text-only encoder wrapper + **a throughput and quality
   benchmark on my own notes** (chunks/hour on this CPU, retrieval spot-checks in German and
   English). Decision rule: keep EmbeddingGemma 2 if a full first pass fits in one evening,
   otherwise fall back per §9 and record the choice. Then clustering + review-queue suggestions
   with evidence.
2b. Enrich queue: grounded compression into `processed/` with the gate and `## Dropped` list.
3. Item timelines: markers, provenance, tick extraction, staleness.
4. `npd`: API, WebSocket events, watcher integration, scheduler, autostart installer.
5. UI: index → note card → review queue → editor + TODO segment → chat panel.
6. `np-app` Tauri shell, folder picker, spawn-if-absent, installers for Linux and Windows.

## 14. Left to the builder (my call, listed so you can veto)

- Which of the two multilingual models survives the benchmark in step 2; both are local, pinned by
  hash, and switchable by config.
- Default status-marker vocabulary in §8.7, stored as config so I can add my own wording later.
- Catalog shard threshold (default 600 entries per file) and the continuation-line indent rule.
- Compression target 35 % and how many gate retries (one).
- Chunk size/overlap, cluster method (agglomerative over cosine with a link/tag prior), and the
  similarity thresholds that put something in the review queue instead of asserting it.
- "Stale since edit" definition: days since last edit with no state event, threshold configurable.
- `INDEX.md` line format: `- [[Title]](path) · size · tags · →2 ←1 · cluster · status`. `→n` and `←n`
  count edges that land on a **live** note, not stored rows: an edge into a stale or deleted destination
  is not advertised as intact and surfaces in the unresolved figure instead.
- DB schema details, error taxonomy, log rotation for `npd`, config file layout.

## 15. Resolved in review

1. **Compressed copy: yes**, into a separate `processed/` tree, strictly bounded to the messy
   original, which stays the ground truth (§8.7). The day file itself stays messy; I read the
   compressed twin, the TODO segment, or ask chat.
2. **Language: mixed German/English** → self-hosted `google/embeddinggemma-2` text tower at 512-d
   MRL, local by default; endpoint embeddings available as a config flag (§9).
   **Status: open is the default** — anything not marked finished or paused; a tick means done
   (§8.7).
3. **No other editors.** note-processor is the only writer by design, so external-change handling
   stays a safety net (hash check + banner) rather than a core flow.
4. **One entry per note, no filter** — wrapping and sharding handle size, related words go on the
   entry (§2).
