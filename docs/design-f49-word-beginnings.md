# F49 — a query can ask for word beginnings, and an answer says which words it widened

**Status: built.** `np_core::search::{Prefix, Term, terms, plan}`, the `prefix` parameter of `search` over the pipe, and
`np search --prefix`, shipped in the issue-#6 round. Numbered here rather than in
[`backlog-m2-followups.md`](backlog-m2-followups.md) because the decision arrived with the implementation; F48 was the last
number taken, so this took the next one.

## The gap

Keyword search quoted every word of a query as an FTS5 **phrase**, which makes prefix matching impossible by construction:
there is no spelling of `grand` that reaches a note containing only `grandma`. The reporter measured the release binary
(`np-0.4.0-x86_64-unknown-linux-gnu.tar.gz`, digest `3d808d96…dd62b`) against a vault holding `grandma`, `grandmother` and
`kestrel`:

| query | hits before F49 |
|---|---|
| `grandma`, `grandmother` | 1 each |
| `grand`, `grandm`, `grandmo`, `kestr` | **0** |

Case was not the problem — the tokenizer case-folds — the **word boundary** was. And there was no way to ask: no flag, no
parameter, and a client could not work around it by rewriting the query, because np builds the FTS5 expression itself and
quotes everything the caller types.

## Why #1's answer did not cover this

Issue #1 closed with documentation, on the argument that a query being an `AND` of its words is deliberate and load-bearing
for precision: changing the parser there would have traded a documentation problem for a retrieval-quality regression. That
argument was about **how many words must match**. This one is about **what counts as a word**, and it is a different rule —
the README's "no stemming: `runner` does not match `runners`, because this vault holds identifiers".

Both rules protect the same person, and neither wanted to move. A vault holds file names, commands and environment variables
as well as prose; a search that quietly widened `GGML_DEFAULT_N_THREADS` into whatever begins with it would report notes the
reader never asked about, which is precisely the failure mode this repository's §12 rule 12 spends its length on — putting
words on screen that the bytes do not support. So F49 did not move the wall. **It added a door, and made the answer admit
when it was used.** That second half is not decoration: a widened list that looks identical to an exact one is how a note
about a grandmother ends up quoted as saying *grand*.

## What a query can contain now

One character of syntax, at the end of a word:

- **`"grand*"`** — that word matches at a word beginning; every other word in the same query still has to appear whole. No
  flag, so a person typing into an ordinary search box can use it today, and a client that does nothing at all gets it.
- **`--prefix` / `"prefix": true`** — *every* word matches at a word beginning. This is the shape of a front end's visible
  "match word beginnings" toggle: one switch, stated in the interface rather than inferred from what the person typed.
- Everything else is unchanged. Word characters are alphanumerics and `_`; every other character separates, including `*` in
  any position that is not immediately after a word — so `a*b`, `**` and a leading `*` split exactly as they always did.

The cap still counts **32 words** (`MAX_QUERY_TERMS`), applied to the terms before planning, so a pasted page cannot turn one
search into a thousand prefix lookups either.

## The decisions

**A marked term is emitted unquoted with a trailing `*`; every other term stays a quoted phrase.** A prefix term needs no
quoting because `terms` keeps word characters only — there is no metacharacter left to reach the FTS5 parser. Handing the raw
query to `MATCH`, or "unescaping" it, would make `"`, `-`, `(` and `:` a supported input surface whose malformed forms return
SQLite parse errors from inside a library call. The existing test that `-foo OR (bar): "baz*"` reads as four **words** rather
than as grammar still passes, which is the check that this stayed a bag of words.

**The default is `Prefix::Marked`, not `EveryTerm`.** An unmarked query answers exactly what it answered before, and
`an_unmarked_word_still_matches_only_whole_words` exists to fail the day somebody decides widening should be free.

**`prefix_used` describes the answer, not the request.** It is read off the plan — *did the keyword list run, and did that
plan contain a prefix term* — so `np search --prefix '!!!'` reports `keyword_used: false`, `prefix_used: false`,
`prefix_terms: []`. A key that contradicts `hits: []` is a pair a caller has to reconcile, and reconciliation is where bugs
live. The same reasoning made `vectors` the truth about the index rather than about the run (**F45**).

**`prefix_terms` carries the words, not the query.** `grand*` reports `["grand"]`: what np searched for, with the syntax
removed. That is why the text channel can print the list verbatim above the hits —

```text
word beginnings matched: grand
0.0164 note   size.md  [bm25 -1.05e-6]   … # Totals  A [grand] total of nine. 
0.0161 note   cake.md  [bm25 -9.51e-7]   … # Family  My [grandma] made a cake on Sunday. 
```

— and why a client labelling hits never has to parse the user's typing to know what was widened.

**`matched_exactly` is computed by SQLite, in the statement that found the hit.** Three values, because there are three
facts: `true` (every word appears in that note as a whole word), `false` (the note was reached only because a widened word
*begins* with one of them) and `null` (this hit did not come from the keyword list at all — a vector or image hit was never in
a whole-word test, and `null` is not `false`).

Getting that per-row truth took two measured refusals. `notes_fts MATCH ?3` written in the select list is rejected outright —
`unable to use function MATCH in the requested context`. The obvious rewrite, aliasing the table (`FROM notes_fts AS whole
WHERE whole MATCH ?3`), is rejected too: `no such column: whole`. What works is a correlated subquery that repeats the real
table name and ties itself to the outer row by rowid:

```sql
EXISTS (
    SELECT 1 FROM notes_fts
     WHERE notes_fts MATCH ?3 AND notes_fts.rowid = notes.id
)
```

The alternative — run `exact_expression` as a second statement and join paths in Rust — reads a database that a concurrent
`np scan` may have moved between the two statements, and then `matched_exactly` would be a claim about a row set that produced
no hit at all. One statement cannot disagree with itself.

**A prefix hit is ranked by the same `bm25()` as an exact one.** Widening changes which documents match, not how well they
match; re-scoring prefix hits would have invented a second ranking function with no measurement behind it. In the capture
above the whole-word note still comes first, on its own bm25.

**`"prefix": false` is not a sanitizer.** It means *do not widen every word*; a `*` the person typed still marks its word,
identically to the CLI at rest. A caller that read `false` as "treat the query as literal text" would show a user a widened
list it believed narrow, so the parameter's documentation and a test both say otherwise.

**A `prefix` that is not a boolean is refused** (`-32602`, naming `true or false`) rather than mapped onto a default: `"yes"`
from a GUI would otherwise become a search that person never asked for, and the answer would look like np's own idea. This is
the same rule `links.list` draws for a `rel` that cannot name a note.

## What this does not do

- **No stemming, no synonyms, no fuzzy matching.** `grandm` reaches `grandma`; `grandmas` does not reach `grandma`, and
  `colour` does not reach `color`. Stemming is the thing #6 declined to become.
- **A trailing asterisk cannot be searched for as text**, and there is no escape spelling — a note holding a literal `grand*`
  is found by searching the words around it. This is the entire cost of choosing one character of syntax over an escape
  grammar, and it is stated in the CLI's own `--help`, because that is where a reader meets the rule.
- **The semantic half is untouched.** Vectors embed whole text; there is no prefix concept to widen, so a hybrid answer's
  vector hits carry `matched_exactly: null` whatever the query says.
- **No per-term provenance.** One boolean per hit, not a list of which of five words arrived by prefix — a client that needs
  more than "this hit needed widening" is asking for a different feature, and it should be measured before it is built.

## What is measured, and where

`crates/np-core/tests/search_prefix.rs` holds the core rules (default unchanged; marked term widens; `EveryTerm` widens
unmarked words; exact versus prefix-only on two notes that differ in exactly that respect; `*` is syntax only at a word's
end; widening cannot invent a keyword half; a vector-only hit answers `null`). The surfaces are covered where they live:
`a_query_can_ask_for_word_beginnings_and_the_answer_says_which_words_it_widened` in `crates/np-cli/tests/cli.rs` pins both doors, the
text-channel line and the empty-answer sentence — which now admits it tried word beginnings, because
`nothing matched "qqzz"` after `--prefix` would report a question this run did not ask — and two cases in
`crates/np-cli/tests/serve.rs` pin the parameter's three states and its refusal. The transcript in
[`reference-serve.md`](reference-serve.md) was re-captured, so the document keys a reader copies out of it are the ones this
build sends.
