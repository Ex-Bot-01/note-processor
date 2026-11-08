# Hybrid search: FTS5 beside the vectors, and which one you asked for

Status: **decided 2026-11-07, implemented in the same round.** Two rulings from the human are recorded here rather than
invented below: **hybrid is the default**, and **the user can choose** — search over embeddings, over FTS5, or both. The
first-start question was settled the same day in its recommended shape (§7): nothing dials out because a config value named a
model. Prior art: `search.rs`'s read-only rule and its ordering contract, F35 (an unknown config value is refused, never
ignored), F20/F13 (bytes are identity; a report prints display text but sorts by bytes), F31 (no labelled set — which is why
this note reasons about *recall* and refuses to claim precision).

## 1. What was missing, in one sentence

`np search` could only rank by meaning, so the first thing anyone reaches for in a vault — "the note that says
`GGML_DEFAULT_N_THREADS`" — had no answer: there is no FTS table anywhere in the schema (`grep -c fts5 crates/np-core/src/db.rs`
→ 0 before this change), and `build.rs` compiles SQLite with `-DSQLITE_ENABLE_FTS5` already, so the capability was sitting
unused in the bundled build.

## 2. The three modes, named once

| Mode | Ranks by | Needs | Returns image hits? |
| --- | --- | --- | --- |
| `semantic` | cosine over stored vectors | an installed model **and** embedded rows | yes |
| `keyword` | SQLite `bm25()` over title and body | nothing beyond a scan | **no — by construction**: a figure has no text to match, so there is no keyword list it can appear in |
| `hybrid` (default) | both lists fused (§4) | a scan; vectors only add the semantic half | yes, from the semantic side |

The headline property the default buys: **`np search` works on a machine with no model installed.** Before this, search was
unusable until ~310 MB of artifact and an embed pass existed. Keyword mode is also what makes the PoC demonstrable on Windows
and macOS, where feature builds refuse to compile inference (`build.rs`) — the search command still runs there, ranks by words,
and says so.

`np search <ws> <query> [--mode hybrid|semantic|keyword]`, and `[search] mode = "…"` in config, defaulting to `hybrid`. A CLI
flag beats config; an unrecognised value is refused at parse time with the three legal spellings named, in both doors (F35's
rule: refusing is a different act from ignoring).

## 3. Schema — migration 11, external content, three triggers

```sql
CREATE VIRTUAL TABLE notes_fts USING fts5(
    title, body,
    content='notes', content_rowid='id',
    tokenize = 'unicode61 remove_diacritics 2'
);
-- AFTER INSERT / AFTER DELETE / AFTER UPDATE OF title, body on notes:
--   insert (new…), or the FTS5 'delete' command with the OLD row, then the new one.
```

* **External content** (`content='notes'`) stores no second copy of the prose — the index holds terms pointing at `notes.id`,
  and the text is read back from `notes`. The cost of that choice is that the shadow table cannot notice a change on its own,
  which is what the three triggers are for; keeping them in sync by hand at every write site would be a drift waiting for the
  next author who forgets one.
* **Triggers, not a re-index pass.** A note row's `rel` changes on a move (`ON UPDATE CASCADE`) and `AFTER UPDATE OF title, body`
  deliberately does not fire for that, which is exactly right: a rename moves a note without changing a word of its text, and the
  FTS row keyed on `notes.id` survives it — the same reason `headings` and `links` need no work on a move.
* **No stemming.** `porter unicode61` was tried on paper first and rejected: it folds `runners`→`runner`, which is what you want
  in a prose corpus and what you do *not* want when the query is an identifier, a filename or a flag. The vault this project
  indexes is full of both. `remove_diacritics 2` stays, because `café`/`cafe` is a spelling difference, not a word difference —
  and this repository has enough latin-1 path fixtures to know which side that cuts on.
* **Deletion needs the OLD row**, so the delete trigger passes `old.title, old.body`. A trigger that reads the *new* values would
  leave ghost terms in the index after an edit — pinned by a case that edits a note until its old word is gone, then searches for
  the old word.

## 4. Fusion: rank-based, because bm25 and cosine do not share a scale

`bm25()` returns a negative-magnitude score whose range depends on corpus statistics; cosine lives in `[-1, 1]`. Any weighted sum
of the raw numbers would be a constant nobody can justify, and tuning it against no labelled set (F31) is fitting an unknown. So
hybrid fuses **ranks**:

```
fused(hit) = Σ over the lists that contain it:  1 / (60 + rank_in_that_list)      -- rank is 1-based
```

Reciprocal rank fusion, `k = 60` unmodified — not because 60 was measured here but because it is the value the technique is
defined with, and inventing a number this repository cannot tune would be the worse invention. A note found by both lists outranks
one found by either alone, which is the behaviour worth having; ties break on the existing contract (path bytes ascending, then
kind, then stored bytes) so hybrid ordering is as deterministic as semantic ordering already is.

**What the report says.** The number that ordered the list stays `score` — in hybrid that is the fused value, and it is *not* a
cosine, which is why every hit also carries its raw evidence: `cosine` when the vector list held it, `bm25` when the keyword list
did, plus `semantic_rank` / `keyword_rank`. A reader looking at `0.0325` in hybrid mode should not be able to mistake it for a
similarity, and a machine reader should not have to guess which list produced it. Text output prints both raw numbers and the
matching snippet from FTS5 (`snippet()` over the body), which is the "why did this match" column §3B of the roadmap asked for.

## 5. Degradation, named rather than silent

* hybrid with **no vectors** in the workspace → keyword results plus one stderr line saying so. Not an error: the index is
  usable, and refusing would punish a machine that has not downloaded 310 MB yet. (Vectors under *another* model id remain a
  refusal, as today — that is a mistake, not a missing download.)
* hybrid where FTS returns nothing and the semantic half answers → the semantic list alone, no warning: that is just a query
  with no literal match, and it happens in normal use.
* `keyword` mode asked for on a workspace whose scan has not run → empty list, exit 0 (nothing indexed is a fact about the
  workspace, same as today's semantic answer with no rows).

## 6. Cost, measured on 2026-11-08

FTS5's term index over prose was guessed to land "in the same order as the text itself". Measured instead, on one host (Linux x86-64,
16 CPUs), with `SQLITE_ENABLE_DBSTAT_VTAB` — which **is** compiled into the bundled SQLite, so `docs/measurement-f30-vector-surface.md`'s
claim that per-table bytes needed `dbstat` "which this rusqlite build does not enable" was wrong when it was made and is corrected there.

Two corpora, both created by `np scan` rather than by hand, so every row arrived through the production path. Before/after is one and the
same file: measure, then drop `notes_fts`, its triggers and `schema_version` row 11, `VACUUM`, measure, then re-open so the ladder replays
migration 11 and `VACUUM` again — which is also a live test of the upgrade path a real user takes.

| Corpus | Prose in `notes` (title + body) | File without migration 11 | With it | Delta |
| --- | --- | --- | --- | --- |
| 20 notes (`docs/measurement-f30-vector-surface.md`'s shape) | 6,333 B | 172,032 B | 204,800 B | **+32,768 B** (+19.0% of the file) |
| 500 notes, 556,210 B of prose | 556,210 B | 1,777,664 B | 1,994,752 B | **+217,088 B** (+12.2% of the file) |

Read the second row, not the first: on 20 notes the delta is eight pages and says nothing about shape, while on 500 the index costs
**39% of the prose it indexes**. Per object, `notes_fts_data` holds 192,512 B of that and the other three shadow tables the remaining
~24 KB. For orientation against F30's numbers: the same 500-note vault spends 688,128 B on `notes` and another 688,128 B on `chunks`, so
the keyword index is the third-largest consumer in the file and **smaller than the chunk table it sits beside** — the storage argument for
leaving it out was never going to be the interesting one.

Write-side cost, same database, alternating phases on one corpus (5 runs each, `np scan`'s own `elapsed`, every run touching all 500 files
so nothing skips parsing): **431 ms without the triggers, 515 ms with them — +84 ms, +19.5%, about 0.17 ms per note**. The counterfactual is
the honest one (triggers dropped, index left stale), and the phase order was no-triggers → replay → with-triggers, so the second number also
proves the replay works: after it, `np search --mode keyword` found `notes/280-kafka.md` again at exit 0.

That is a *full re-parse* cost. An ordinary scan re-parses only what changed, so a vault where two notes moved costs ~0.35 ms and reports
what it always reported.

## 7. First-start fetch, as ruled

Nothing in this program dials out except `np models fetch`, which says what it will transfer before it transfers it. A command
that *needs* vectors and finds none **refuses with the two doors named** (`fetch` online, `install --path` offline) rather than
starting a download or silently falling back — and this change makes "silently falling back" unnecessary, because keyword mode is
a legitimate answer to type instead of an excuse. When a daemon exists (M4), the consent line and a `--no-fetch` belong there, not
here.

**Amended 2026-11-08 by F38 — the ruling reversed for the two commands that write vectors, and this paragraph is where that lives so
the two documents cannot tell different stories.** `np embed` and `np setup` now fetch a *pinned* artifact when the workspace is
missing one, default on, because the consumer of this program is increasingly another program that wants vectors to exist rather than
a person willing to sequence three commands. Three parts of the ruling above survive untouched:

+ **`np search` still never fetches**, in any mode and whatever the config says — a read command that downloads would make "your query
  took four minutes because it fetched" possible, and this card's own contribution is that keywords already answer.
+ **"Silently falling back" is still not what happens.** Offline, `np embed` exits 1 saying nothing was embedded; `np setup` indexes,
  exits 0, and reports `vectors: false, offline: true`. Keyword search remains a legitimate answer rather than a consolation.
+ **Consent still exists**, just not as a daemon-only feature: `[models] auto_fetch`, `--no-auto-fetch` / `--yes`, and
  `NP_NO_AUTO_FETCH=1` (which outranks the file), plus an announcement with id, member list, byte count and origin *kind* before byte
  one, and a digest check before the bytes are put in place.

What changed underneath is what makes default-on defensible: the target is **one pinned id from one compiled-in origin**, verified by
digest — not "whatever a config value named". An unpinned id refuses before any transport is chosen, and a build without inference or
without the HTTP client cannot reach the trigger at all (`docs/design-f38-auto-fetch.md`).

## 8. Deliberately not in this round

* **Phrase/boolean syntax** (`"exact phrase"`, `NEAR`, column filters) exposed as its own mode: FTS5 supports it and users will
  find the syntax by accident; what it needs first is a decision about whether raw FTS5 query syntax is a *supported input*, since
  a malformed match expression is a parse error from SQLite that would surface as a program failure. That is its own card.
* **Image text**: no OCR, so figures stay out of the keyword half (F29's line holds).
* **Re-ranking** (semantic re-score of a keyword candidate set): needs F31's labels to know whether it helps.
* **Coverage visibility** ("which chunks are embedded?") — separate debt, unchanged by this.

## 9. What implementation taught the note (2026-11-08)

Three things this note did not know when it was written, each now stated where the next reader will hit it.

**F32's refusals had to be re-requested, not deleted.** §5 promised keyword search would answer where a vector could not, and that
quietly invalidated four pinned CLI cases whose whole claim was *exit 1* in exactly those workspaces. They now pass `--mode semantic`,
which is the request their refusal belongs to; the sentences they check are unchanged. A default change that turns four refusals into
four successes is not a test-suite nuisance — it is the card's real content, and the honest record is that the pins moved with it.

**Rewinding a version number gained a fifth object set.** Six fixtures stamp `schema_version` back to 4 and let the next open replay the
ladder. Each had to learn migration 11's objects (`notes_fts` plus three triggers on `notes`) or fail on `table notes_fts already exists`.
The SQL is one const per test file — three copies, because integration tests cannot share a private helper — and each copy names the other
two. That duplication is accepted debt with a stated trigger for revisiting it: **the next migration edits the same six sites**, and if a
third file joins, the copies should become a dev-support module behind a feature instead.

**A small number is not a missing one.** The text channel first printed SQLite's `bm25()` as `-0.0000` on a two-note vault, because four
decimals cannot hold a millionth — and a reader deciding whether to trust a ranking read that as "no score". Small magnitudes now print in
scientific notation (`np-cli::number`), with an assertion that no hit line begins `0.0000`. The lesson generalises: any column that shows a
number from someone else's function has to be checked against that function's real output range on the smallest corpus it can produce, not
against the range assumed here.
