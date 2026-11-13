# CLI and report reference

What each command writes, which keys `--json` carries and why they are always present, what every line of the `np embed` report means, and what the exit codes promise. Quoted output is output a real run printed, not an illustration.

> *Moved out of `README.md` on 2026-12-09, when the README became a document you read before installing rather than one you read after something broke. The wording is the same as it was there — including the dates, the measured numbers and the claims about what has not been measured — because those are the parts that carry weight.*

```sh
# Global flags come before the subcommand: -v | -vv | -vvv, --log-spec SPEC, --log-file PATH
np setup <workspace> [--json] [--yes]     # index, fetch the pinned model if missing, embed — one command for a caller that wants vectors
np scan <workspace> [--json]              # index + regenerate the catalog
np embed <workspace> [--json]             # embed what the index says is owed
np search <workspace> <query> [--limit N] [--mode M] [--json]   # --limit defaults to 10, --mode defaults to config's `hybrid`
np models install|fetch|hash|list …
```

- **Order matters: scan, then embed.** Embedding works from the index. `scan` is idempotent and cheap on an unchanged tree.
- **What counts as a note.** Only files ending `.md` (any case) are parsed as notes; other files under the root are still tracked
  for changes, which is why a vault of five files can report `added 5` and `notes 3`. Directories whose name begins with `.` and
  `processed/` are not walked at all.
- **Incremental both ways.** A note whose bytes have not moved keeps its vector; a figure's vectors are keyed to its *content
  hash*, so the same image linked from six notes is encoded once, and deleting the last link to it prunes the row and its
  vectors on the next run.
- **`search` writes nothing.** It prepares only `SELECT`s — no vector, no stamp, no note row. The keyword half is the same
  database's FTS5 index, kept current by three triggers on `notes`, so a query costs a read and never a rebuild. (Opening a
  brand-new workspace's database still creates it, like every other command: "read-only" describes what search changes about
  the index.) Which kinds are indexed is never a refusal: a notes-only corpus searches fine, so does an images-only one, and an
  index with no vectors prints `nothing indexed for <id>` and exits **0**.
- **`np search` answers before any model exists.** That is what hybrid as the default bought (**F36**,
  `docs/design-m3-hybrid-search.md`): a workspace with no artifact and no vectors returns its keyword hits, prints one line on
  stderr naming the half that was missing, and exits **0**. Nothing dials out to fetch a model because a config key named one —
  the download stays an explicit `np models fetch`. Ask for `--mode semantic` in that same workspace and you get the F32 refusal
  at exit 1 instead, because a request for one list that cannot run is not answerable by the other.
- **A schema upgrade's record outlives the run that made it.** The scan whose open cleaned up orphan rows says so on stdout, once — a permanent
  one-line warning would train everyone to ignore it. The record itself lives in the workspace database and reaches every later report as
  `recorded_upgrade_orphans` and `recorded_merged_notes`, so "what did an upgrade delete here last month?" is a question `np scan --json` answers instead of
  one that needs a database shell (**F21**). A `null` means the file holds no such record; the all-zero string means the check ran and removed nothing, and
  those two states are never collapsed into an empty string.
- **Diagnostics never enter stdout.** `-v`, `NP_LOG` or `[log] level` puts human text on stderr; `--log-file` / `[log] file` writes
  JSONL; both are off unless asked, and every report and exit code above is byte-identical with them on or off. The levels, the ten
  targets, the twenty-one rule tokens and both caps are in [`docs/reference-diagnostics.md`](reference-diagnostics.md).
- **`--json` is the machine interface.** stdout carries exactly one JSON document (`scan`, `embed`, `search`, `models list`).
  Search's document is `{hits, mode, note, vectors}`: the list, the mode that ran, a sentence about a half that could not run
  (`null` when nothing was missing), and whether this workspace holds vectors at all — so "no semantic hits" and "no model
  installed" stay two different reports. Every hit carries the same **eleven** keys whatever the kind and whatever the mode —
  `score`, `cosine`, `bm25`, `semantic_rank`, `keyword_rank`, `kind`, `path`, `snippet`, `linked_from`, `source_note`,
  `linked_notes` — because an absent key and a null are two different claims. The five evidence keys are F36's, and they exist so
  a fused score can be argued with: in `keyword` mode `cosine` and `semantic_rank` are `null`, in `semantic` mode the reverse.
  The last three are F28's: **every** live note linking a figure, byte-ordered, where the other two were its head and its length.
  A note hit and an orphaned figure both get `[]`, never `null` — "this hit has no live linker" is one true statement about
  either, and telling them apart is what `kind` is for.

```sh
np search ~/Notes "vision projector" --limit 2 --json
```

A two-note vault with no model installed — the whole document one real run printed, `--mode keyword` so the numbers are the
keyword half's alone:

```sh
np search ./notes retro --mode keyword --json
```

```json
{
  "fetched": null,
  "hits": [
    {
      "bm25": -1.0620689655172414e-6,
      "cosine": null,
      "keyword_rank": 1,
      "kind": "note",
      "linked_from": 0,
      "linked_notes": [],
      "path": "retro.md",
      "score": 1.0620689655172414e-6,
      "semantic_rank": null,
      "snippet": "# Weekly\n\n[Retro]: the demo went well, migration postponed.\n",
      "source_note": null
    }
  ],
  "indexed": true,
  "keyword_used": true,
  "mode": "keyword",
  "note": null,
  "offline": false,
  "reason": null,
  "semantic_used": false,
  "vectors": false
}
```

`bm25` is SQLite's own number, unremapped (better is more negative, so `score` carries its negation to keep "higher first"
true across modes), and the text channel prints small magnitudes in scientific notation rather than `-0.0000`. With a hybrid
list whose semantic half could not run, `note` holds the sentence that stderr got and `cosine`/`semantic_rank` hold nulls.

**The keys beside `hits` say three different things, and #5 is why they are three.**

| Key | Question it answers |
|---|---|
| `mode` | what the caller **asked for** — the flag, or `[search] mode`. It is not a report of what ran: hybrid answers from keywords alone whenever no vector list exists (**F45**) and still calls itself `hybrid` here |
| `keyword_used`, `semantic_used` | which halves **actually ran**, so a client that shows "searched by keyword" instead of "hybrid" is showing what `np` did rather than what it was told to do |
| `vectors` | whether this workspace **holds vectors at all** — deliberately the truth about the index, and still `true` on a run that could not use them (F45's ruling: a reader that sees `false` goes and re-fetches 309 MB it does not need) |
| `indexed` | whether anyone has ever **scanned** this workspace. `false` with an empty `hits` means "this folder is not indexed", which is a different sentence from "nothing here matches" — and until #5 the two were indistinguishable, because every other key describes the answer rather than the vault |
| `reason`, `note` | why a half was missing: the token to branch on, and the prose for a human. One token, one fact per run, so it cannot also carry "not indexed" — that is what `indexed` is for |
| `fetched`, `offline` | what auto-fetch downloaded (**F38**), and whether it was tried and failed |

And with vectors present, `hybrid` fuses both lists. The same two-note vault after `np embed`, queried `"broker lag"` — one
real run, both channels, on a `--features local-model` build:

```text
0.0328 note   kafka.md  [cos 0.8143 bm25 -1.97e-6]   … # Kafka  Consumer [lag] and the migration plan for the [broker] fleet.
0.0161 note   retro.md  [cos 0.7090]
```

`kafka.md` is first because **both lists put it first** (fused `1/61 + 1/61`); `retro.md` appears on its cosine alone, with no
`bm25` and no keyword rank to show. That is the whole argument for fusion in two lines: a note that agrees in both lists beats a
note that leads one, and you can see why from the bracket rather than taking the score on faith — which matters because a fused
rank sum is not a similarity and must never be read as one.

A figure linked from two notes, from a `--features local-model,vision` build whose projector was installed offline
(`np models install --member mmproj`) — the whole output of one real run, including what the text channel makes of it:

```json
{
  "kind": "image",
  "linked_from": 2,
  "linked_notes": ["notes/a.md", "notes/b.md"],
  "path": "img/fig.png",
  "score": 0.5161576999208495,
  "source_note": "notes/a.md"
}
```

```text
0.8208 note   notes/a.md
0.7350 note   notes/b.md
0.5162 image  img/fig.png   (from notes/a.md +1 more)
```

Before F28 that image line was `(from notes/a.md)` and `notes/b.md` existed only as the number 2 in a JSON key nobody read.
The note printed is still `source_note`, because it is `linked_notes[0]` — one list, assembled once, and a test that fails
if head-or-count ever stops matching it.

- **Exit codes.** `0` success (including "nothing to do" and "nothing indexed"), `1` anything the runtime refused — disabled
  provider, missing artifact, a model mismatch — and `2` for argv errors, which is clap's doing. No command invents a code.

### A measured run of the vector half

One real `--features local-model` build against a three-note vault: the whole `np embed` report, then the search list it makes possible. Quoted because every line is a claim about what happened, and the table below says what each one means.

```text
model      embeddinggemma-2
outcome    nothing owed
seen       3
current    2
embedded   1
refused    0
busy-gap   0
errors     0
images     links 1 · embedded 0 · refused 1 (vision-unavailable 1)
verify     self-consistent, not verified
digest     not re-read: same size and mtime as the last check (295.5 MB); detects changed bytes, not a crafted mtime
threads    12 of 16 available (auto)
```

```text
0.7140 note   projects/timeline.md
0.6566 note   projects/brief.md
0.5703 note   index-notes.md
```

`np embed` prints one line per fact, in this order (measured output above):

| Line | Meaning |
| --- | --- |
| `model` | The id whose vectors this run read and wrote |
| `outcome` | What was left when the pass ended — `nothing owed`, or the arm that stopped it |
| `seen` / `current` / `embedded` | Notes considered, notes already vectorised for this model, notes encoded now |
| `refused` | Chunks over `max_tokens`: counted and named, never truncated |
| `busy-gap` | Work that appeared between the plan and the write; the next run picks it up |
| `errors` | Failures that did not stop the run — each is a finding, not noise |
| `images` | `links N · embedded N · refused N (class N …)` plus `unchecked` / `pruned` when they are non-zero. See below |
| `verify` | Whether this run re-hashed the artifact or took its stamp at face value |
| `digest` | The same claim in words, with the size it saw and what that check can and cannot detect |
| `threads` | Workers llama.cpp used, and whether `"auto"` or you chose it |
