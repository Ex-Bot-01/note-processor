# note-processor — design

Date: 2026-10-23
Status: draft for approval
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
- `INDEX.md`: one line per note — title, path, size, tags, links, cluster, status summary —
  alphabetized, machine-generated, safe to hand to an LLM for an overview.
- Daily notes: opening a day creates `YYYY-MM-DD.md` if absent; a fenced TODO segment at the
  top is generated from the last *n* days (default 30); I write messy underneath; saving
  triggers processing of that file.
- Item timelines: an item records first written, last touched, current state (open / done /
  superseded) and `file:line` evidence for each transition. State changes only on text I wrote.
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
- Tidying prose by rewriting it (see §14 open question 1) — v1 lints structure, shows findings,
  does not silently restructure my notes.
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
- Embeddings: 512-dim, q8 storage ≈ 19 MB; brute-force cosine scan in RAM is fine — this is not
  a vector-search-infrastructure problem. Indexing budget: minutes of CPU for the whole corpus,
  milliseconds-to-seconds for one file.
- LLM: self-hosted OpenAI-compatible endpoint only (currently a Qwen3-Next-class model on my
  server). Model id, base URL and key are config; no vendor SDK in the tree.
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
| `INDEX.md` | workspace root | whole-file regenerate; excluded from tracking (§9) |

Everything else derived — tags, keywords, clusters, entities, statuses, contradiction edges —
lives in the database and appears in `INDEX.md`/the UI. Nothing is written into note bodies by an
LLM. No file is rewritten as a side effect of indexing; line endings and encoding are preserved
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
  tags, items) → chunk (~350 tokens ±overlap) → embed → cluster → index/render.
- **Storage**: SQLite (`rusqlite`) in the OS data dir, one DB per workspace. Tables: `files`
  (path, display path, hash, size, mtime, state), `notes`, `headings`, `chunks` (q8 vector blob),
  `items` (+`item_events` for first/touched/done/superseded with `file:line`), `links`
  (typed, `approved` flag), `tags`, `clusters` (+ membership), `contradictions`. FTS5 over note
  text for keyword search; vectors scanned brute-force in RAM.
- **Embeddings**: local int8 model via `candle` (candidate: `bge-small-en-v1.5` Matryoshka-truncated
  to 512 dims; fallback `all-MiniLM-L6-v2`). Model downloaded once, pinned by sha256 in config.
  Keyword/tag derivation is statistical, not model-based: existing tags from frontmatter/`#tags`,
  titles, link anchors for distinctiveness (vault-relative TF-IDF), no LLM.
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
a controlled re-embed rather than mixed-space comparisons.

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
windows-latest) running `cargo test` and `npm run build`. LLM behind a trait with a fake for
tests; one opt-in integration test against the real endpoint, skipped by default.

## 13. Build order

1. `np-core`: discover/hash/parse/registry + SQLite schema (fixtures, golden `INDEX.md`).
2. Embeddings + clustering + review-queue suggestions with evidence.
3. Item timelines: markers, provenance, tick extraction, staleness.
4. `npd`: API, WebSocket events, watcher integration, scheduler, autostart installer.
5. UI: index → note card → review queue → editor + TODO segment → chat panel.
6. `np-app` Tauri shell, folder picker, spawn-if-absent, installers for Linux and Windows.

## 14. Left to the builder (my call, listed so you can veto)

- Exact embedding model after a benchmark on my own notes; both candidates are local and pinned.
- Chunk size/overlap, cluster method (agglomerative over cosine with a link/tag prior), and the
  similarity thresholds that put something in the review queue instead of asserting it.
- "Stale since edit" definition: days since last edit with no state event, threshold configurable.
- `INDEX.md` line format: `- [[Title]](path) · size · tags · →2 ←1 · cluster · status`.
- DB schema details, error taxonomy, log rotation for `npd`, config file layout.

## 15. Open — needs my answer

1. Does the app ever write a **tidied copy** of a note, and where? (v1 as written: no.)
2. Which markers count as open/done in what I actually type — and are the notes German, English
   or mixed? (picks the embedding model)
3. Do I also edit these files in another editor while the app is open?
4. `INDEX.md`: one line for every note, or only "important" ones — and who decides important?
