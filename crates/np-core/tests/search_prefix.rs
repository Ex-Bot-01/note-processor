//! Prefix matching (**issue #6**, `docs/design-f49-word-beginnings.md`): what a query can ask for when it stops one
//! character short of a word, and what an answer is allowed to claim about how it matched.
//!
//! Rows are written directly, as in `tests/search_modes.rs` — what is under test is the reading of an answer, not how the
//! rows arrived. Every case here is a pair: the widened result *and* the fact that says it was widened. A feature that made
//! `grand` reach `grandma` without also saying so would have fixed one bug (#6's) and opened the one §4 of this repository's
//! rules closes — putting words on screen that the bytes did not support.

use np_core::db::Db;
use np_core::embed::Vector;
use np_core::search::{self, Prefix, SearchMode};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    db: Db,
}

impl Fixture {
    fn text_only() -> Self {
        let tmp = tempfile::tempdir().expect("test setup: tempdir");
        let db = Db::open(&tmp.path().join("index.sqlite")).expect("test setup: open db");
        Self { _tmp: tmp, db }
    }

    /// A live note holding `body`, through the triggers migration 11 installs — an index kept in sync by hand at each write
    /// site is an index waiting for the write site somebody forgets.
    fn note(&self, rel: &str, title: &str, body: &str) {
        self.db
            .0
            .execute(
                "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
                 VALUES (CAST(?1 AS BLOB), CAST(?1 AS BLOB), ?1, ?1, ?2, 'hh', 10, 1, 'ok', 1)",
                rusqlite::params![rel, format!("/ws/{rel}")],
            )
            .expect("test setup: file row");
        self.db
            .0
            .execute(
                "INSERT INTO notes (rel_bytes, rel, title, title_source, encoding, word_count, body)
                 VALUES (CAST(?1 AS BLOB), ?1, ?2, 'heading', 'utf-8', 3, ?3)",
                rusqlite::params![rel, title, body],
            )
            .expect("test setup: note row");
    }

    /// Paths the keyword list returns for `query`, in order. The default policy — a word matches as a prefix only if its
    /// author marked it — because most of what this file pins is what happens *without* asking for more.
    fn keyword(&self, query: &str) -> Vec<String> {
        search::keyword(&self.db, query, 10, Prefix::Marked)
            .expect("a keyword query always answers")
            .into_iter()
            .map(|hit| hit.path)
            .collect()
    }
}

/// The vault issue #6 measured against, in the reporter's own words: two notes a half-typed word should reach and one it
/// should not.
fn family() -> Fixture {
    let fx = Fixture::text_only();
    fx.note("cake.md", "Family", "My grandma made a cake.");
    fx.note("visit.md", "Family", "My grandmother visited too.");
    fx.note("lake.md", "Outdoors", "The kestrel flew over the lake.");
    fx
}

#[test]
fn an_unmarked_word_still_matches_only_whole_words() {
    // **The default is the old behaviour, and this is the test that keeps it from drifting.** `runner` does not match
    // `runners` because a vault holds identifiers (README, "no stemming"); widening that for everyone to help the half-remembered
    // proper noun would trade a documented rule for an undocumented one. Issue #6 asked for a door, not for the wall to move.
    let fx = family();
    assert_eq!(fx.keyword("grand"), Vec::<String>::new());
    assert_eq!(fx.keyword("grandm"), Vec::<String>::new());
    assert_eq!(fx.keyword("kestr"), Vec::<String>::new());
    // …and a whole word still lands, case notwithstanding.
    assert_eq!(fx.keyword("grandma"), vec!["cake.md"]);
    assert_eq!(fx.keyword("KESTREL"), vec!["lake.md"]);

    // Mutation this catches: applying `Prefix::EveryTerm` unconditionally, which turns every assertion above into a hit and
    // makes the README's sentence false without changing a single line of it.
}

#[test]
fn a_marked_word_reaches_every_word_that_starts_with_it() {
    let fx = family();
    let hits =
        search::keyword(&fx.db, "grand*", 10, Prefix::Marked).expect("a marked word answers");
    assert_eq!(
        hits.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(),
        vec!["visit.md", "cake.md"],
        "`grand*` reaches both `grandma` and `grandmother`, and nothing else — in `bm25()` order, which is why the shorter \
         note that mentions the word once outranks the other here and path would only break a tie"
    );
    // The plan is where the widened words are named, so a caller never re-parses the query to find out.
    let plan = search::plan("grand*", Prefix::Marked).expect("one word is a plan");
    assert_eq!(plan.prefix_terms, vec!["grand".to_string()]);
    assert!(
        plan.prefixes(),
        "a marked term means this answer matched beginnings"
    );

    // Mutation this catches: emitting `"grand*"` (quoted) instead of `grand*`. FTS5 then looks for a token whose literal text
    // ends in an asterisk and answers with nothing — the exact bug #6 describes, reintroduced one layer down.
}

#[test]
fn every_term_mode_widens_words_nobody_marked() {
    // `--prefix` / `"prefix": true`: the shape of a front end's "match word beginnings" toggle, which is off or on for the
    // whole query and cannot mark each word as the person types it.
    let fx = family();
    let widened =
        search::keyword(&fx.db, "grand", 10, Prefix::EveryTerm).expect("every-term mode answers");
    assert_eq!(
        widened
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        vec!["visit.md", "cake.md"],
        "the same two notes `grand*` reaches, from a query with no star in it"
    );
    assert_eq!(
        search::keyword(&fx.db, "kestr", 10, Prefix::EveryTerm)
            .expect("every-term mode answers")
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        vec!["lake.md"],
        "and the fragment the reporter typed reaches its note at last"
    );
    let plan = search::plan("grand kestr", Prefix::EveryTerm).expect("two words are a plan");
    assert_eq!(
        plan.prefix_terms,
        vec!["grand".to_string(), "kestr".to_string()],
        "every word of the query is named as widened, in the order it was asked"
    );

    // Mutation this catches: treating `EveryTerm` as "strip the stars" — the queries above then answer empty and the toggle
    // appears to do the opposite of what its name says.
}

#[test]
fn a_hit_found_only_by_a_prefix_says_so_and_one_that_contains_the_word_says_it_does_not() {
    // The per-hit fact #6 asked for, on two notes that differ in exactly one respect: one *contains* `grand`, the other only
    // begins with it. A GUI labelling "matched as a word beginning" labels these differently, and nothing else in the document
    // tells them apart.
    let fx = Fixture::text_only();
    fx.note("size.md", "Totals", "A grand total of nine.");
    fx.note("cake.md", "Family", "My grandma made a cake.");

    let hits =
        search::keyword(&fx.db, "grand*", 10, Prefix::Marked).expect("a marked word answers");
    let exact: Vec<(&str, bool)> = hits
        .iter()
        .map(|hit| (hit.path.as_str(), hit.matched_exactly))
        .collect();
    assert_eq!(
        exact,
        vec![("cake.md", false), ("size.md", true)],
        "`cake.md` was reached through a prefix; `size.md` holds the whole word"
    );

    // …and under an unmarked query nothing is ever prefix-only, so the key cannot quietly start meaning "unchecked".
    let plain = search::keyword(&fx.db, "grand", 10, Prefix::Marked).expect("a whole word answers");
    assert_eq!(
        plain.iter().map(|hit| hit.matched_exactly).collect::<Vec<_>>(),
        vec![true],
        "an unmarked query's every hit matched exactly — `false` would be a verdict on a test that never ran"
    );

    // Mutation this catches: dropping the correlated `EXISTS` subquery and filling the field with `true`. Everything above
    // fails, and so does the claim a client builds on it.
}

#[test]
fn a_star_is_syntax_only_at_the_end_of_a_word() {
    // The one piece of syntax F49 adds has an edge, and the edge is documented: a leading or doubled `*` stays a separator,
    // exactly as every other punctuation character does. Without this rule the query surface would gain FTS5's grammar by
    // degrees, which is why `match_expression` quoted everything in the first place.
    let fx = family();
    assert_eq!(
        fx.keyword("*grand"),
        Vec::<String>::new(),
        "a leading star marks nothing: it separates, and `grand` still has to be a whole word"
    );
    assert_eq!(
        fx.keyword("grand**").len(),
        2,
        "a second star is only a separator; the first one already marked the word"
    );
    assert_eq!(
        fx.keyword("grandmother*"),
        vec!["visit.md"],
        "marking a whole word still matches it — a prefix that happens to be complete is not a different thing"
    );

    // The cost, stated as a test because it is the one thing a user can lose: a trailing asterisk cannot be searched for.
    fx.note("glob.md", "Globs", "The pattern grand* matches many names.");
    assert_eq!(
        fx.keyword("grand*").len(),
        3,
        "`grand*` asks for word beginnings even against a note that holds the literal text — there is no escape spelling, \
         and F49 says so rather than discovering it later"
    );
}

#[test]
fn widening_cannot_invent_a_keyword_half() {
    // A query with no word in it runs no keyword list under either policy: `keyword_used` stays false and `prefix_used` stays
    // false, because there was nothing to widen. The pair is what a client reads; a `true` beside an empty list would be a
    // claim about work that did not happen.
    let fx = family();
    for policy in [Prefix::Marked, Prefix::EveryTerm] {
        let answer = search::ranked(
            &fx.db,
            SearchMode::Keyword,
            None,
            None,
            "!!! ???",
            10,
            policy,
        )
        .expect("a punctuation query answers");
        assert!(answer.hits.is_empty(), "{policy:?}: nothing to match");
        assert!(
            !answer.keyword_used,
            "{policy:?}: no word, so no keyword list ran"
        );
        assert!(
            !answer.prefix_used && answer.prefix_terms.is_empty(),
            "{policy:?}: a query that ran no keyword list matched no prefixes"
        );
    }

    // Mutation this catches: computing `prefix_used` from the *policy* rather than from the plan, which makes
    // `--prefix np search '!!!'` advertise widened words that no hit was found by.
}

#[test]
fn a_hit_the_keyword_list_never_held_has_no_whole_word_answer() {
    // `None` is not `false`. A vector-only hit did not fail a whole-word test; it was never in one. This is the difference
    // between telling a client "not exact" and telling it "the keyword half has no opinion", and the reason the field is an
    // `Option` rather than a bit.
    let fx = Fixture::text_only();
    fx.note("cake.md", "Family", "My grandma made a cake.");
    np_core::db::insert_model(&fx.db, "fixture@1", 2, None, "d:", "q:", 1_730_000_000)
        .expect("test setup: model row");
    let note_id: i64 = fx
        .db
        .0
        .query_row("SELECT id FROM notes", [], |row| row.get(0))
        .unwrap();
    fx.db
        .0
        .execute(
            "INSERT INTO chunks (note_id, ordinal, chunker, text, byte_start, byte_end, source_hash)
             VALUES (?1, 0, 1, CAST('text' AS BLOB), 0, 4, 'sh')",
            [note_id],
        )
        .expect("test setup: chunk row");
    let chunk_id = fx.db.0.last_insert_rowid();
    np_core::db::upsert_vector(
        &fx.db,
        chunk_id,
        "fixture@1",
        2,
        &Vector::new(vec![1.0, 0.0]).to_bytes(),
        "sh",
        1_730_000_001,
    )
    .expect("test setup: vector row");

    let answer = search::ranked(
        &fx.db,
        SearchMode::Semantic,
        Some("fixture@1"),
        Some(&Vector::new(vec![1.0, 0.0]).to_bytes()),
        "grand",
        10,
        Prefix::Marked,
    )
    .expect("semantic search answers");
    assert_eq!(answer.hits.len(), 1, "the vector list held the note");
    assert!(!answer.keyword_used, "and the keyword list never ran");
    assert_eq!(
        answer.hits[0].matched_exactly, None,
        "so there is no whole-word answer to give — not `false`"
    );
}
