//! Hybrid search: FTS5 keyword ranking beside the vectors (`docs/design-m3-hybrid-search.md`).
//!
//! Rows are written directly, as in `tests/search.rs`: what is under test is what a reader gets back and in which order,
//! not how the rows arrived. Two things make this file different from the semantic suite. It must pass **with no model row
//! at all** in several cases — that is the property the human asked for when they made hybrid the default: search works on a
//! machine that has never downloaded an artifact. And every keyword assertion here goes through the triggers migration 11
//! installs, because an index kept in sync by hand at each write site is an index waiting for the write site somebody forgets.

use np_core::db::{self, Db};
use np_core::embed::Vector;
use np_core::search::{self, HitKind, SearchMode};
use tempfile::TempDir;

const MODEL: &str = "fixture@1";

/// The stored form: every component little-endian, which is what `embeddings.vector` holds.
fn bytes(components: &[f32]) -> Vec<u8> {
    Vector::new(components.to_vec()).to_bytes()
}
const DIM: usize = 2;

/// 1/√2 — the diagonal of a unit square, normalised. Named because writing `0.7071` is clippy's `approx_constant` error
/// (it is right: an approximation of a known constant should say which constant), and because the exact value keeps this
/// fixture's cosines honest to the last decimal the fusion claims to rank by.
const HALF_DIAGONAL: f32 = std::f32::consts::FRAC_1_SQRT_2;

struct Fixture {
    _tmp: TempDir,
    db: Db,
}

impl Fixture {
    /// A workspace with notes and **no vectors**: the state a fresh machine is in.
    fn text_only() -> Self {
        let tmp = tempfile::tempdir().expect("test setup: tempdir");
        let db = Db::open(&tmp.path().join("index.sqlite")).expect("test setup: open db");
        Self { _tmp: tmp, db }
    }

    /// The same, with the model row every vector case needs.
    fn with_model() -> Self {
        let fx = Self::text_only();
        db::insert_model(&fx.db, MODEL, DIM as i64, None, "d:", "q:", 1_730_000_000)
            .expect("test setup: model row");
        fx
    }

    /// A live note holding `body`. No vectors — call [`Fixture::embed`] to add them.
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

    /// One chunk vector for `rel`, axis-aligned so a cosine is a number by eye.
    fn embed(&self, rel: &str, components: &[f32]) {
        let note_id: i64 = self
            .db
            .0
            .query_row("SELECT id FROM notes WHERE rel = ?1", [rel], |row| {
                row.get(0)
            })
            .expect("test setup: the note to embed");
        self.db
            .0
            .execute(
                "INSERT INTO chunks (note_id, ordinal, chunker, text, byte_start, byte_end, source_hash)
                 VALUES (?1, 0, 1, CAST('text' AS BLOB), 0, 4, 'sh')",
                [note_id],
            )
            .expect("test setup: chunk row");
        let chunk_id = self.db.0.last_insert_rowid();
        db::upsert_vector(
            &self.db,
            chunk_id,
            MODEL,
            DIM,
            &bytes(components),
            "sh",
            1_730_000_001,
        )
        .expect("test setup: vector row");
    }

    /// One figure with `vector`, linked from nothing: enough to be a semantic hit and never a keyword one.
    fn figure(&self, seed: u8, path: &str, components: &[f32]) {
        let hash = format!("{seed:0>64x}");
        let row =
            db::record_media(&self.db, &hash, 100, "image/png").expect("test setup: media row");
        db::link_media_path(
            &self.db,
            path.as_bytes(),
            std::path::Path::new(path),
            row.id,
        )
        .expect("test setup: media path");
        db::record_media_embedding(
            &self.db,
            row.id,
            MODEL,
            DIM as i64,
            &bytes(components),
            &hash,
            1_730_000_002,
        )
        .expect("test setup: media vector");
    }

    fn ranked(
        &self,
        mode: SearchMode,
        query_text: &str,
        query_vector: Option<&[f32]>,
    ) -> Vec<String> {
        let encoded = query_vector.map(bytes);
        let answer = search::ranked(
            &self.db,
            mode,
            Some(MODEL),
            encoded.as_deref(),
            query_text,
            10,
            search::Prefix::Marked,
        )
        .expect("the search answers");
        answer
            .paths()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    }
}

/// **A keyword query finds the exact string a vector would never match.** This is the case that made hybrid the default: an
/// identifier, a flag name, a filename. Nothing about `[1, 0]`-shaped similarity ranks it against prose, and there is no
/// model on this machine in any event — the fixture has no vectors at all.
#[test]
fn a_keyword_query_finds_an_exact_identifier_with_no_model_installed() {
    let fx = Fixture::text_only();
    fx.note(
        "runners.md",
        "Runner notes",
        "Set GGML_DEFAULT_N_THREADS to 12 before building.",
    );
    fx.note("other.md", "Other", "Nothing about thread counts here.");

    let hits = search::keyword(&fx.db, "GGML_DEFAULT_N_THREADS", 10, search::Prefix::Marked)
        .expect("keyword search answers");
    assert_eq!(
        hits.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(),
        vec!["runners.md"],
        "the identifier is in one note and only that note comes back"
    );
    assert!(
        hits[0].snippet.contains("GGML_DEFAULT_N_THREADS"),
        "a hit says why it matched: {:?}",
        hits[0].snippet
    );
    assert!(
        hits[0].bm25 < 0.0,
        "bm25 is SQLite's own number, unremapped"
    );

    // Hybrid reaches the same note with no query vector available — the semantic half simply contributes nothing.
    assert_eq!(
        fx.ranked(SearchMode::Hybrid, "GGML_DEFAULT_N_THREADS", None),
        vec!["runners.md"]
    );
    // And keyword mode is happy to answer alone.
    assert_eq!(
        fx.ranked(SearchMode::Keyword, "GGML_DEFAULT_N_THREADS", None),
        vec!["runners.md"]
    );

    // Mutation this catches: a migration that creates `notes_fts` but no triggers leaves every keyword list empty, and
    // an index built from the NEW row on delete would still hold the old terms.
}

/// **Diacritics fold; word endings do not.** `café` finds `cafe` because that is one spelling of one word, while `runner`
/// must not find `runners`: this vault holds identifiers and filenames as often as prose, and stemming would trade a
/// precise match for a fuzzy one on every code note in it. The tokenizer choice is the whole assertion.
#[test]
fn diacritics_fold_and_word_endings_do_not() {
    let fx = Fixture::text_only();
    fx.note(
        "cafe.md",
        "Café plan",
        "The team meets at the café on Friday.",
    );
    fx.note(
        "runners.md",
        "Runners",
        "Two runners were started by the sweep.",
    );

    assert_eq!(
        search::keyword(&fx.db, "cafe", 10, search::Prefix::Marked)
            .expect("search")
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        vec!["cafe.md"],
        "`café` and `cafe` are one spelling, not two words"
    );
    assert!(
        search::keyword(&fx.db, "runner", 10, search::Prefix::Marked)
            .expect("search")
            .is_empty(),
        "no stemming: a query for `runner` does not silently widen to `runners`"
    );
}

/// **The index follows the note through an edit and a move.** Editing a body until its old word is gone must remove that
/// word from the index — the delete half of the update trigger, which is the half that leaves ghost hits when it reads the
/// new row instead of the old. A rename then has to leave the keyword row alone: it moves on `notes.id`, exactly as
/// `headings` and `links` do, and `AFTER UPDATE OF title, body` deliberately does not fire for a path rewrite.
#[test]
fn the_keyword_index_follows_an_edit_and_survives_a_move() {
    let fx = Fixture::text_only();
    fx.note(
        "kafka.md",
        "Kafka",
        "The broker is kafka-3 with one partition.",
    );
    assert_eq!(
        search::keyword(&fx.db, "partition", 10, search::Prefix::Marked)
            .expect("search")
            .len(),
        1,
        "a body-only term is indexed"
    );
    assert_eq!(
        search::keyword(&fx.db, "kafka", 10, search::Prefix::Marked)
            .expect("search")
            .len(),
        1,
        "and so is a title-only one — `kafka` also appears in the body here, which is why the case below edits it out"
    );

    fx.db
        .0
        .execute(
            "UPDATE notes SET body = 'The broker is redis now.' WHERE rel = 'kafka.md'",
            [],
        )
        .expect("test setup: edit the note");
    assert!(
        search::keyword(&fx.db, "partition", 10, search::Prefix::Marked)
            .expect("search")
            .is_empty(),
        "the word is gone from the body, so it must be gone from the index"
    );
    assert_eq!(
        search::keyword(&fx.db, "redis", 10, search::Prefix::Marked)
            .expect("search")
            .len(),
        1,
        "and the new word is findable in the same row"
    );

    // The move a scan performs, statement for statement as `db::apply_move` does it: rewrite `files` (the foreign key
    // migration 4 left standing is on `rel_bytes`, so that cascade takes the note's bytes with it), then refresh
    // `notes.rel`. Nothing touches `notes_fts` — the keyword row is keyed on `notes.id`, which is the whole design.
    fx.db
        .0
        .execute(
            "UPDATE files SET rel_bytes = CAST('moved/kafka.md' AS BLOB), \
                     case_bytes = CAST('moved/kafka.md' AS BLOB), rel = 'moved/kafka.md', \
                     case_key = 'moved/kafka.md', abs = '/ws/moved/kafka.md' \
             WHERE rel_bytes = CAST('kafka.md' AS BLOB)",
            [],
        )
        .expect("test setup: move the file row");
    fx.db
        .0
        .execute(
            "UPDATE notes SET rel = 'moved/kafka.md' WHERE rel_bytes = CAST('moved/kafka.md' AS BLOB)",
            [],
        )
        .expect("test setup: refresh the note's display path");
    let hits = search::keyword(&fx.db, "redis", 10, search::Prefix::Marked).expect("search");
    assert_eq!(
        hits.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(),
        vec!["moved/kafka.md"],
        "the keyword row is still there and now names where the note lives"
    );

    // Deleting the note must clear its terms too, or a removed note keeps answering queries forever.
    fx.db
        .0
        .execute("DELETE FROM notes WHERE rel = 'moved/kafka.md'", [])
        .expect("test setup: delete the note row");
    assert!(
        search::keyword(&fx.db, "redis", 10, search::Prefix::Marked)
            .expect("search")
            .is_empty(),
        "a deleted note answers nothing"
    );
}

/// **A note found by both lists outranks one found by either alone** — the argument for fusing ranks instead of scores.
///
/// The fixture separates the lists on purpose, and the reason is worth the paragraph: an earlier version gave every note a
/// vector, and then `words.md` at keyword-rank 1 + semantic-rank 3 (1/61 + 1/63) *beat* `both.md` at 2 + 2 (1/62 + 1/62) by
/// eight millionths. That is not a bug in the fusion; it is what rank fusion means — agreement and position both count, and a
/// first place in one list is worth more than a second. So the case below puts `words.md` outside the semantic list entirely
/// (it is a note nobody embedded). `both.md` then sits **second in both lists** and still wins: 1/62 + 1/62 beats either
/// 1/61 alone, which is the property in one line — agreement outranks position. The two single-list hits tie exactly at 1/61
/// and fall to the ordering contract (path bytes ascending), so this case pins that tie-break inside hybrid too.
#[test]
fn a_hit_found_by_both_lists_outranks_one_found_by_either_alone() {
    let fx = Fixture::with_model();
    fx.note(
        "both.md",
        "Kafka migration",
        "kafka appears here once, and the vector is half-way.",
    );
    fx.embed("both.md", &[HALF_DIAGONAL, HALF_DIAGONAL]);
    // Deliberately **not** embedded: this note exists in the keyword list and nowhere else.
    fx.note(
        "words.md",
        "Kafka only",
        "kafka kafka kafka, nothing about vectors here.",
    );
    fx.note(
        "vector.md",
        "Unrelated prose",
        "Nothing about brokers at all.",
    );
    fx.embed("vector.md", &[1.0, 0.0]);

    let report = search::ranked(
        &fx.db,
        SearchMode::Hybrid,
        Some(MODEL),
        Some(&bytes(&[1.0, 0.0])),
        "kafka",
        10,
        search::Prefix::Marked,
    )
    .expect("hybrid search answers");
    let paths = report.paths();
    assert_eq!(paths[0], "both.md", "{paths:?}");
    assert_eq!(
        report.hits[0].semantic_rank,
        Some(2),
        "second on cosine, behind the [1,0] note"
    );
    assert_eq!(
        report.hits[0].keyword_rank,
        Some(2),
        "and second on bm25, behind the three-mention note"
    );
    assert!(
        report.hits[0].cosine.is_some() && report.hits[0].bm25.is_some(),
        "both halves vouch for this hit, so both numbers ride along"
    );

    // Both remaining hits were found by one list at rank 1, so both score 1/61 and the tie breaks on path bytes: `v` before `w`.
    assert_eq!(paths[1..], ["vector.md", "words.md"], "{paths:?}");
    for hit in &report.hits[1..] {
        let present = [hit.cosine, hit.bm25]
            .iter()
            .filter(|v| v.is_some())
            .count();
        assert_eq!(
            present, 1,
            "one list vouches for it, so exactly one raw number is set: {hit:?}"
        );
    }

    // Semantic mode alone must not see the keyword-only ranking at all, and `score` keeps meaning cosine there.
    let semantic = search::ranked(
        &fx.db,
        SearchMode::Semantic,
        Some(MODEL),
        Some(&bytes(&[1.0, 0.0])),
        "kafka",
        10,
        search::Prefix::Marked,
    )
    .expect("semantic search answers");
    // `words.md` is absent: it holds no vector, so the semantic list cannot see it at all. That absence is the same fact that
    // lets hybrid rank it from one list only.
    assert_eq!(semantic.paths(), ["vector.md", "both.md"]);
    assert!(semantic.hits[0].bm25.is_none(), "no keyword list ran");

    // Mutation this catches: summing raw bm25 with cosine puts `words.md` first (its bm25 magnitude dwarfs 1/61), which is
    // precisely the scale error §4 refuses to make.
}

/// **Keyword mode never invents an image hit.** A figure has no prose to match, so there is no keyword list it can appear
/// in; a hybrid run still returns it, from the semantic half, with its linker set intact. Saying this out loud keeps
/// "search finds images" from becoming true in one mode and false in another without anyone noticing.
#[test]
fn keyword_mode_returns_no_images_and_hybrid_still_does() {
    let fx = Fixture::with_model();
    fx.note("a.md", "Alpha", "See the figure for kafka.");
    fx.embed("a.md", &[1.0, 0.0]);
    fx.figure(0xaa, "img/fig.png", &[0.0, 1.0]);

    let keyword = search::ranked(
        &fx.db,
        SearchMode::Keyword,
        Some(MODEL),
        None,
        "kafka",
        10,
        search::Prefix::Marked,
    )
    .expect("keyword answers");
    assert_eq!(keyword.paths(), ["a.md"]);
    assert!(
        keyword.hits.iter().all(|hit| hit.kind == HitKind::Note),
        "no figure can match a word it does not contain"
    );

    let hybrid = search::ranked(
        &fx.db,
        SearchMode::Hybrid,
        Some(MODEL),
        Some(&bytes(&[0.0, 1.0])),
        "kafka",
        10,
        search::Prefix::Marked,
    )
    .expect("hybrid answers");
    // The note is vouched for twice (keyword rank 1, semantic rank 2) and the figure once — at cosine 1.0. Rank fusion ranks
    // by agreement, not by similarity, so the doubly-claimed note leads. Semantic mode shows the opposite order, which is the
    // half of this pair that keeps "hybrid changed the ranking" from being an abstraction.
    assert_eq!(hybrid.paths(), ["a.md", "img/fig.png"], "{:?}", hybrid.hits);
    let semantic = search::ranked(
        &fx.db,
        SearchMode::Semantic,
        Some(MODEL),
        Some(&bytes(&[0.0, 1.0])),
        "kafka",
        10,
        search::Prefix::Marked,
    )
    .expect("semantic answers");
    assert_eq!(semantic.paths(), ["img/fig.png", "a.md"]);
}

/// **A workspace with no vectors is a state, not an error** — and it says which half answered. This is the machine that has
/// not downloaded 310 MB yet: hybrid answers from keywords, reports `vectors = false`, and names why in one line. A query
/// vector supplied anyway (the CLI embedding eagerly) must not turn the absence into a failure either.
#[test]
fn no_vectors_is_a_state_and_the_report_says_which_half_answered() {
    let fx = Fixture::text_only();
    fx.note("a.md", "Alpha", "kafka runs on port 9092.");

    let report = search::ranked(
        &fx.db,
        SearchMode::Hybrid,
        Some(MODEL),
        Some(&bytes(&[1.0, 0.0])),
        "kafka",
        10,
        search::Prefix::Marked,
    )
    .expect("hybrid answers with no vectors at all");
    assert_eq!(report.paths(), ["a.md"]);
    assert!(!report.semantic_used && report.keyword_used);
    assert_eq!(
        report.note,
        Some(search::RankNote::NoVectors),
        "the caller prints one honest line rather than nothing"
    );

    // Pure semantic mode in the same workspace still answers empty: nothing was refused, there is simply no vector to rank.
    let semantic = search::ranked(
        &fx.db,
        SearchMode::Semantic,
        Some(MODEL),
        Some(&bytes(&[1.0, 0.0])),
        "kafka",
        10,
        search::Prefix::Marked,
    )
    .expect("semantic answers empty");
    assert!(semantic.paths().is_empty());
}

/// **Mode names are one list, spelled once.** `--mode`, the config value and `--json` all read these strings, and a fourth
/// spelling appearing in only one of them is how F28's split-refusal lesson repeats: parse it, refuse it by name, and let the
/// label round-trip.
#[test]
fn mode_names_parse_refuse_and_round_trip() {
    for (text, mode) in [
        ("hybrid", SearchMode::Hybrid),
        ("semantic", SearchMode::Semantic),
        ("keyword", SearchMode::Keyword),
    ] {
        let parsed: SearchMode = text.parse().expect("legal spelling parses");
        assert_eq!(parsed, mode);
        assert_eq!(parsed.label(), text, "the label is the spelling");
    }
    for bad in ["Hybrid", "vector", "", "full-text"] {
        assert!(
            bad.parse::<SearchMode>().is_err(),
            "`{bad}` is not a mode and must not parse"
        );
    }
}

/// **FTS5's operators are words here, not grammar.** A query is user text, and the characters that mean something to the
/// FTS5 parser — `"`, `*`, `-`, `(`, `:`, a bare `OR` — arrive in note searches as often as they arrive in queries: pasted
/// stack traces, command lines, Markdown. Passing text through untouched would make `np search '"kafka'` answer with a
/// SQLite parse error from inside a library call and turn every such query into a bug report about our parser.
///
/// So each word is quoted and the words are ANDed, which this case pins three ways: an unbalanced quote still finds its
/// word, a sentence of operators still reads as a conjunction of its words, and a query with no words at all answers empty
/// instead of refusing.
#[test]
fn fts5_operators_are_words_not_grammar() {
    let fx = Fixture::text_only();
    fx.note(
        "kafka.md",
        "Kafka",
        "The kafka consumer lagged behind the producer.",
    );
    fx.note(
        "flags.md",
        "Flags",
        r#"The line was: -foo OR (bar): "baz*""#,
    );
    fx.note("plain.md", "Plain", "Nothing to do with any of it.");

    // An unbalanced quote is a separator, not a syntax error.
    assert_eq!(
        search::keyword(&fx.db, "\"kafka", 10, search::Prefix::Marked)
            .expect("a stray quote must never reach the parser as grammar")
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        vec!["kafka.md"],
        "the word survives its own punctuation"
    );

    // Operators read as words, and every word has to be present: `flags.md` holds `-`, `foo`, `OR`, `bar`, `baz` and the
    // rest of that line, so it is the only note a conjunction of them can describe.
    let hits = search::keyword(
        &fx.db,
        "-foo OR (bar): \"baz*\"",
        10,
        search::Prefix::Marked,
    )
    .expect("a query of pure operators still has to answer");
    assert_eq!(
        hits.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(),
        vec!["flags.md"],
        "AND over words, not FTS5's own boolean grammar"
    );

    // A query with no word in it has nothing to ask for. Empty and successful — refusing would be a verdict on input that is
    // merely uninformative, and `np search '!!!'` finding nothing is the truth about the vault.
    let empty = search::keyword(&fx.db, "\":()*-", 10, search::Prefix::Marked)
        .expect("no tokens is not an error");
    assert!(
        empty.is_empty(),
        "a punctuation-only query matches nothing, and got: {empty:?}"
    );

    // Mutation this catches: handing the raw query to `MATCH` (the first case then errors, the second reads as FTS5 syntax),
    // and dropping the quote-escaping in `match_expression`, which turns a token containing `"` into broken grammar.
}

/// **The term cap is a cost ceiling, and it bites in a stated direction.** Thirty-two words is far past any query a person
/// types; beyond it, `np search` stops adding constraints rather than refusing or searching nothing — so a pasted page still
/// returns results, ranked by its first thirty-two words. Pinned because the choice is invisible in the source comment and
/// the alternative (drop the *leading* terms, or error) would look equally plausible in a diff.
#[test]
fn a_query_beyond_the_term_cap_searches_its_first_terms() {
    let fx = Fixture::text_only();
    // `terms.md` holds words 01..=32 and not word 33; `tail.md` holds only word 33.
    let first_terms: Vec<String> = (1..=32).map(|n| format!("wzn{n:02}")).collect();
    fx.note(
        "terms.md",
        "Thirty-two words",
        &format!("{} and nothing else.", first_terms.join(" ")),
    );
    fx.note("tail.md", "The thirty-third", "wzn33 appears nowhere else.");

    let long_query = format!("{} wzn33", first_terms.join(" "));
    assert_eq!(
        long_query.split_whitespace().count(),
        33,
        "test setup: one term past the cap"
    );

    let hits = search::keyword(&fx.db, &long_query, 10, search::Prefix::Marked)
        .expect("a long query still answers");
    assert_eq!(
        hits.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(),
        vec!["terms.md"],
        "the first thirty-two terms constrain the search; the thirty-third is dropped rather than dropping the query"
    );

    // Mutation this catches: off-by-one at the cap (`> MAX_QUERY_TERMS` keeps 33 terms and finds nothing), and a cap that
    // truncates from the front, which would return `tail.md` instead.
}

/// **F45's vocabulary, graded without a filesystem, a model, or a network.**
///
/// `RankNote` grew from one variant to seven in one change, and every one of them is a state a caller has to be able to tell
/// apart — so the properties that make the enum worth its name get asserted over *all* of them rather than per-variant:
///
/// * **Every note names the half you did get.** Each sentence says "ranked by keyword only", because a note that explains what
///   is missing without saying an answer still arrived is how a degraded read starts looking like a failure.
/// * **No two notes share a sentence.** Two states printing identical prose is one state with two names, which is how F40 spent
///   an afternoon: `outcome owed and not produced` meant three different things at once.
/// * **Tokens are unique, kebab-case, and stable.** `--json`'s `reason` key branches on these; the sentences above are prose and
///   may be reworded. A token that collides with another silently merges two caller paths.
/// * **A build never advertises a door it cannot open** (F32's rule, applied to notes): without `fetch`, no sentence may name
///   `np models fetch`. The opposite row asserts that with `fetch` at least one does — otherwise the pair of consts for each state
///   could collapse to the fetch-less wording and this suite would not notice.
/// * **No host, ever** (§12 rule 8): an origin *kind* at most, so `http` must appear nowhere in a note.
#[test]
fn every_rank_note_says_what_arrived_and_never_names_a_host() {
    let all = [
        search::RankNote::NoVectors,
        search::RankNote::ProviderDisabled,
        search::RankNote::ModelNotInstalled,
        search::RankNote::ArtifactMissing,
        search::RankNote::FetchOffline,
        search::RankNote::FetchDisabled,
        search::RankNote::ArtifactFetched,
    ];

    let mut tokens = std::collections::BTreeSet::new();
    let mut sentences = std::collections::BTreeSet::new();
    let mut names_fetch = 0usize;

    for note in all {
        let token = note.token();
        let sentence = note.label();

        assert!(
            !token.is_empty()
                && token
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "`reason` tokens are kebab-case, like every other token this program prints: {token:?}"
        );
        assert!(
            tokens.insert(token),
            "two notes share the token {token:?}, so a caller branching on `reason` cannot tell them apart"
        );
        assert!(
            sentences.insert(sentence),
            "two notes print the identical sentence ({sentence:?}); that is one state wearing two names"
        );
        assert!(
            sentence.contains("keyword"),
            "{token} has to say which half DID answer, not only which one is missing: {sentence}"
        );
        assert!(
            !sentence.contains("http") && !sentence.contains('@'),
            "a note may name an origin kind and never a host or URL (§12 rule 8): {sentence}"
        );
        if sentence.contains("np models fetch") {
            names_fetch += 1;
        }
    }

    #[cfg(not(feature = "fetch"))]
    assert_eq!(
        names_fetch, 0,
        "this build compiled the HTTP client out, so no note may send a reader to `np models fetch`"
    );
    #[cfg(feature = "fetch")]
    assert!(
        names_fetch > 0,
        "a build that CAN fetch has to say so somewhere in this vocabulary, or the fetch-less wording \
         is being printed by a binary that could have done better"
    );
}

/// The two halves of F45's contract, kept apart on purpose: **`reason` is a token and `note` is prose.** A mutant that makes
/// `token()` return the sentence (the obvious shortcut when a JSON key already exists next to it) passes every assertion above
/// and fails this one, because then the machine-readable channel changes whenever a sentence is reworded — which is precisely the
/// coupling that made F41's wording change cost fourteen test edits.
#[test]
fn a_note_token_is_not_its_sentence() {
    for note in [
        search::RankNote::ProviderDisabled,
        search::RankNote::ArtifactFetched,
    ] {
        assert_ne!(
            note.token(),
            note.label(),
            "`reason` must stay a token even though it sits beside `note` in the same document"
        );
        assert!(
            !note.token().contains(' '),
            "a token with a space in it is a sentence wearing a key's name: {:?}",
            note.token()
        );
    }
}
