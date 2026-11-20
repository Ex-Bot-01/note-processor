# Issue #12, part two — should `.txt` become a searchable note?

**Status: design written 2026-11-20 for the human's ruling. No code has been written against this document, and none
should be until it is ruled on.** Issue #12 (opened 2026-11-16, closed 2026-11-19) asked for one of two things; one of
them shipped, and the other is the question left open.

## 1. The question

A person points `np` at a folder of notes that is not one file format — `note.md`, `plain.txt`, `todo.txt`,
`scratch.txt` — runs a scan, and gets back a count of files. Only the `.md` files are searchable. The rule itself is
deliberate; what was missing was any way to see it from outside, which is why a front end had to explain np's rule in its
own words and own the risk of it changing.

## 2. What np does now

The decision is one line: `crates/np-core/src/walk.rs:28` declares `const MARKDOWN_SUFFIX: &str = ".md";` and
`walk.rs:446` sets `is_markdown = case_key.ends_with(MARKDOWN_SUFFIX)` — on the display-folded key, so `.MD` counts and
`.txt` does not.

On the other side of that flag an attachment is *tracked*: hashed, stored, listable and linkable, never parsed. That is
`scan.rs:765` ("an attachment is hashed, tracked and linkable but never parsed (spec §10)"), and `owes_a_reparse` at
`scan.rs:788-789` is what keeps a scan from ever re-reading one: it returns false unless the file is markdown.

**Issue #12's second ask already shipped, in v0.6.0 (tag dated 2026-11-19, commit `e435985`).** The scan report carries
`attachments` (`scan.rs:204`) — live tracked files that are not notes — and up to five of their names in
`attachment_examples` (`scan.rs:209`, cap `ATTACHMENT_EXAMPLE_LIMIT = 5` at `scan.rs:156`), documented in
`docs/reference-cli-and-reports.md:22-24` with the identity `notes + attachments` = every live tracked file. The rule has
a diagnostic name too: `rule::NOT_MARKDOWN` (`crates/np-core/src/diagnostics.rs:107`). A front end can now print np's own
numbers instead of inferring them, which is what the issue asked for on that side.

What did **not** ship, and what this document is about, is the first ask: making a `.txt` a note at all. Three facts
decide how expensive that is.

- **Parsing does not need markdown.** `parse_markdown` (`crates/np-core/src/parse.rs:115`) reads optional frontmatter and
  `#` headings and falls back to the file name for a title; a plain-text file yields no headings and a filename title,
  which is honest rather than broken. Chunking does not look at markdown either: `crates/np-core/src/chunk.rs:112-127`
  splits on blank lines and merges blocks up to the byte bound, so paragraphs of prose chunk exactly as they would in a
  `.md`.
- **The suffix test is used in more than one place.** Link classification repeats it: `crates/np-core/src/links.rs:32`
  (`const MARKDOWN_EXTENSION: &str = "md";`) and `links.rs:373` decide whether a link's target is a note or an attachment.
  A suffix list that changed `walk.rs` alone would make a `.txt` searchable but still not a link *destination* of the same
  kind, which is a contradiction a person would report as a bug.
- **Config has no `[index]` section, and every container refuses unknown keys.** `Config` holds `catalog`, `markers`,
  `embeddings`, `models`, `search` and `log`; there is no `[index]`. Since F35 an invented key or section is refused, not
  defaulted (`crates/np-core/src/config.rs:143-150`, `docs/design-f35-unknown-keys.md`), so a new knob arrives with a
  refusal path and a migration note whether it lands in a new section or an existing one.

## 3. Options

**A — Close the issue; ship nothing more.** The visibility half is done, so the front end's sentence becomes quotable:
"np tracks N files, of which M are notes; here are the ones that are not." Cost: documentation only — `docs/reference-config.md`
and `docs/reference-cli-and-reports.md` would state the rule once, in the present tense, where someone configuring np will
find it. What it locks in: `.txt` stays invisible to search forever unless a later round reopens this, and every front end
keeps repeating np's rule in its own words.

**B — `[index] note_suffixes`, defaulting to `["md"]`.** An explicit list of suffixes treated as notes; everything else
stays an attachment exactly as today. Touched: a new `[index]` container in `config.rs` (with `deny_unknown_fields`, like
every other container); the one test in `walk.rs:446`; the same rule in `links.rs:373` so link kinds agree with search;
`owes_a_reparse` (`scan.rs:788`) so a file that *became* a note is re-read on the next scan after a config change; and
`attachments`/`attachment_examples`, which must count from the same list or the report starts contradicting search.

**C — Any file that decodes as UTF-8 text is a note.** No config, no list, no new section. It also silently turns
`server.log`, `package-lock.json`, vendored source and someone's `.csv` export into notes with generated titles, in the
catalog and in search results. Given that np indexes folders it does not own, that is a rule whose failures are invisible
until a person sees their build log in a search answer; listed here for completeness and recommended against.

**D — A parser per suffix (`.org`, `.rst`, `.adoc`).** Issue #12 says plainly it is not asking for a converter and nothing
here should rewrite anyone's files. Per-suffix parsers are a format-support commitment that outlives this question, and B
does not require one: an `.org` file under B reads as plain text, which is worse-formatted but searchable and honest.

## 4. What each option costs, and what it locks in

| | A nothing more | B suffix list | C any text |
|---|---|---|---|
| engineering | small: two doc paragraphs | medium: config container + two call sites + reclassification + report semantics | medium to code, large to live with |
| behaviour change on an existing vault | none | only for someone who opts in | everything at once |
| catalog and goldens | none | unchanged by default: `tests/fixtures/corpus` holds 16 `.md` and one `.png`, so no golden moves until a fixture adds a `.txt` — which an implementation should, to pin the behaviour | goldens rewritten on first run over any real folder |
| database | none | a file that becomes a note needs its old attachment row reclassified and its text reparsed; `owes_a_reparse` currently declines every non-markdown file, so this is new ground rather than a reused path | same, plus rows for files nobody meant as notes |
| embeddings | none | newly promoted notes are chunks owed but unembedded, which is the count task 13 of the improvement plan asks for independently — opt-in and consent stay as they are | same, at whatever scale the folder happens to hold |
| a `.txt` that is not text | n/a | np does not refuse undecodable bytes: `scan.rs:887-891` decodes with replacements and records `encoding = "lossy"` on the note row (`scan.rs:64-68`), while `unindexable` counts unreadable, unkeyable and duplicated *paths* rather than encodings. A UTF-16 `.txt` therefore becomes a note full of U+FFFD and says so only in a database column — a decision to make deliberately, not to discover | same, with no list to blame |
| trust | the rule stays np's, stated in np's words | the rule stays np's and becomes the person's choice | np decides what a note is, alone |

Two things B must not do. It must not accept an empty list or a suffix with a leading dot or a duplicate — the F35 stance
is to refuse and name the problem, not to default quietly. And it must not report a file both ways: if `.txt` is listed,
that file is a note and stops appearing in `attachments`, or the two halves of the scan report stop adding up to the
number of tracked files that `docs/reference-cli-and-reports.md:24` promises.

## 5. Recommendation

**B**, with `.md` as the only default, plain text read as text (no per-suffix parser), and one line added to the scan
report or diagnostics naming which suffixes np treated as notes — so a front end can quote the list rather than infer it
from a config file it does not own.

The reason is the same one that made issue #12 worth filing: np's rules should be np's, stated in its output, and a person
with a decade of `todo.txt` files should not need a fork or a converter to search them. A stays defensible if the answer
is "np indexes markdown, full stop" — that is a coherent product boundary, and it deserves to be written down in
`docs/reference-config.md` rather than left as an accident of one constant in `walk.rs`.

**Smallest next step:** the ruling itself. Nothing else in this document needs deciding before code — but no code until
it is ruled on, per the plan's rule that a new feature needs a reviewed design first.

## 6. Explicitly not decided here

Whether `.org`, `.rst` or `.adoc` ever get structure-aware treatment (they stay plain text under B). Whether daily files
are markdown and where they live — that is the open M3 question, recorded in `todo.md` under "Open items to ask the human about". Whether a note's
suffix belongs in the catalog entry (nothing here suggests it does). And whether `attachments` should grow a per-suffix
breakdown: if B ships, that count becomes a list of what the person chose to exclude, which is a different sentence and
should be written as one.
