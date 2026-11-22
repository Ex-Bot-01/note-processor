# Design: make README.md a router, not an encyclopedia

Date: 2026-11-22 (`date -u`). Approver: the human, who chose approach B below.
Scope: `README.md` only, plus whatever a moved paragraph needs in an existing `docs/` file. No code changes.

## Intent and the why

The human opened `README.md` and could not tell what `np` does, how to use it, or how to integrate it, and called it
"a wall of text". The cause is not length alone: 334 lines / ~28 KB carry facts buried inside parentheticals and
mid-sentence caveats — one line runs 581 characters — so a reader cannot skim for the sentence they came for. Two of
the fourteen sections are documentation index tables that duplicate `docs/README.md`, and the `np serve` section
reproduces an 11-line JSON transcript whose verbatim twin is already in `docs/reference-serve.md`.

Success looks like: a first-time reader reaches a working `np scan` without scrolling past prose, and an integrating
developer finds the protocol contract in one hop. Failure would look like a short README that got short by dropping a
claim the old one made.

## Goals

- **Scannable.** Every section answers one question, every line under ~200 characters, no fact inside a parenthetical
  that a reader needs to act on.
- **Ordered by reader intent:** what it is → try it → commands → configure → integrate → when something looks wrong →
  limits → where to read next.
- **One home per detail.** Anything longer than a line goes to the `docs/reference-*.md` that already covers it, and the
  README keeps a pointer with the answer's shape ("every key and its default").
- **Convention-clean.** Drop the `**#7**`, `**issue #14**`, `**F51**` markers and the "measured on one host" narration,
  both of which `AGENTS.md` already forbids in tracked files.

## Non-goals

- No new documents, no restructured `docs/`, no changes to code, tests or gates.
- No new claims: nothing enters the README that is not true of the current tree and sourced to a file in it.
- Not a rewrite of `docs/README.md`, which stays the document index.

## Anti-goals

- **Silent loss.** A cut paragraph must land somewhere reachable, or stay.
- **Invented output.** Every transcript stays verbatim capture from an existing reference file, cited as such — no
  hand-typed "illustrative" JSON. The `np serve` excerpt moves to the `v0.6.0` capture in
  `docs/reference-serve.md`, which lists all ten methods, rather than the older `0.3.x` one whose handshake lists seven.
- **Hedging as a style.** Where something is unmeasured it stays said plainly, once.

## Constraints

- `.github/scripts/check-links.py` fails on any unresolved markdown link *or* backticked path. Baseline before the
  change: 81 files, 651 links, 0 broken; both that row and `check-dates-ahead.py` must pass after it.
- `docs/README.md` promises `../README.md` answers "what `np` does, every command, and the first five minutes with a
  real folder". The rewrite keeps all three, so that line needs no edit.
- Units: the pinned embedding artifact is 309,855,456 bytes — 295 MiB, which is what `docs/reference-models.md` quotes.
  The old README said "~309 MB" and "~310 MB" in two places; one spelling wins.

## Approaches considered

| Approach | Trade-off | Verdict |
| --- | --- | --- |
| **A — restructure in place, keep every fact** | Safer on content, but a scannable page cannot hold 28 KB: the wall of text survives as a wall of bullets | Rejected — does not fix what was reported |
| **B — router README (~9–11 KB), detail to existing docs** | Fixes density; needs care that nothing is orphaned | **Chosen by the human** — every cut already has a documented home |
| **C — B plus a separate quick-start document under `docs/`** | Cleaner README, but splits the first-five-minutes story across two files and adds a document to maintain | Rejected — the README's job *is* the first five minutes |

## Decided shape (the human's approach, my layout)

Headings, in order: title + 3-line pitch · **What it does** (5 bullets) · **Try it** (`np scan`, then `np setup` +
`np search`, with the measured output) · **The six commands** (table) · three scripting facts as short bullets ·
**Configure it** (the 3-key TOML, pointer for the other twelve) · **Use `np` from your app** (framing, the rules that
matter to a client, a two-line captured handshake, pointers) · **Images, briefly** · **When something looks wrong**
(4-row table + pointer) · **Limits, stated plainly** (kept intact — this is the README's most useful section and it is
already short) · **Where to read next** (one compact list replacing two tables) · **License**.

## Left to me as builder

Exact wording, line lengths, which of the three scripting facts becomes a bullet versus a table row, and where inside
`docs/reference-serve.md` or `docs/reference-cli-and-reports.md` a moved paragraph lands — provided no claim disappears
and both fences stay green.
