//! The rerank **stage**, tested with a scripted scorer and no weights anywhere (`docs/design-f51-rerank-stage.md`).
//!
//! Every case here runs in keyword mode with `model = None, query_vector = None`, which is deliberate twice over: it keeps
//! the suite artifact-free (a 635 MB load has no business in a test of list arithmetic), and it proves the stage does not
//! lean on the embedder. The stage reads strings out of the index, so it must work where there are no vectors at all — a
//! stage that quietly required a provider would be dead code on exactly the machines whose retrieval most needs help.
//!
//! What is *not* covered here, and where it lives instead: what a real cross-encoder's scores mean (the artifact-gated
//! `tests/local_rerank.rs`), the arithmetic of pair building and reordering (`tests/rerank.rs`), and the sentence a human
//! sees when the stage declines (`np-cli`, Task 9).

use np_core::db::Db;
use np_core::rerank::{RerankError, Reranker};
use np_core::search::{
    self, RerankAsk, RerankInput, RerankNote, RerankSkip, RerankText, SearchMode,
};
use tempfile::TempDir;

/// A scorer that says exactly what it is told to, and remembers what it was asked.
///
/// `scores` are handed out in candidate order and cycled, so a test can plant any permutation it wants to see. `asked`
/// records every call — the strings, verbatim — because half of this stage's contract is *what the model is handed*, and a
/// claim about that cannot be made from the output order alone.
struct Scripted {
    scores: Vec<f64>,
    asked: Vec<(String, Vec<String>)>,
    decline: Option<RerankError>,
}

impl Scripted {
    fn new(scores: &[f64]) -> Self {
        Self {
            scores: scores.to_vec(),
            asked: Vec::new(),
            decline: None,
        }
    }

    fn refusing(error: RerankError) -> Self {
        Self {
            scores: Vec::new(),
            asked: Vec::new(),
            decline: Some(error),
        }
    }

    /// Every document string the stage ever sent, flattened.
    fn docs(&self) -> Vec<String> {
        self.asked
            .iter()
            .flat_map(|(_, docs)| docs.clone())
            .collect()
    }
}

impl Reranker for Scripted {
    fn score(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f64>, RerankError> {
        if let Some(error) = &self.decline {
            // The variant clones its own text, so the refusal is reproducible across calls.
            return Err(match error {
                RerankError::ArtifactAbsent => RerankError::ArtifactAbsent,
                RerankError::RuntimeNotBuilt => RerankError::RuntimeNotBuilt,
                RerankError::ArtifactUnverified(text) => {
                    RerankError::ArtifactUnverified(text.clone())
                }
                RerankError::CannotScore(text) => RerankError::CannotScore(text.clone()),
                other => panic!("the stage is not expected to see {other:?}"),
            });
        }
        self.asked.push((
            query.to_string(),
            docs.iter().map(|d| (*d).to_string()).collect(),
        ));
        Ok((0..docs.len())
            .map(|i| self.scores[i % self.scores.len()])
            .collect())
    }
}

struct Fixture {
    _tmp: TempDir,
    db: Db,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("test setup: tempdir");
        let db = Db::open(&tmp.path().join("index.sqlite")).expect("test setup: open db");
        // No model row and no vectors on purpose: see this file's header.
        Self { _tmp: tmp, db }
    }

    /// A live note whose title and body are exactly the given text (`None` title = the column is NULL).
    fn note(&self, rel: &str, title: Option<&str>, body: &str) {
        let db = &self.db;
        db.0.execute(
            "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
             VALUES (CAST(?1 AS BLOB), CAST(?1 AS BLOB), ?1, ?1, ?2, 'hh', 10, 1, 'ok', 1)",
            rusqlite::params![rel, format!("/ws/{rel}")],
        )
        .expect("test setup: file row");
        db.0.execute(
            "INSERT INTO notes (rel_bytes, rel, title, title_source, encoding, word_count, body)
             VALUES (?1, ?2, ?3, 'h1', 'utf-8', 3, ?4)",
            rusqlite::params![rel.as_bytes(), rel, title, body],
        )
        .expect("test setup: note row");
    }

    /// Keyword-mode search over this fixture, with or without a stage.
    fn run(
        &self,
        query: &str,
        limit: usize,
        input: Option<RerankInput<'_>>,
    ) -> Result<search::Answer, search::SearchError> {
        search::ranked(
            &self.db,
            SearchMode::Keyword,
            None,
            None,
            query,
            limit,
            search::Prefix::Marked,
            input,
        )
    }
}

/// Eight notes matching one word, in a fixture where retrieval order is the path bytes (bm25 ties on identical bodies).
fn eight(db: &Fixture) {
    for letter in ["a", "b", "c", "d", "e", "f", "g", "h"] {
        db.note(&format!("{letter}.md"), Some(letter), "kafka broker");
    }
}

/// **A cancel lands between pairs.** With a `stop` hook the stage hands the scorer one pair per call and asks the hook before
/// each, so a client that stops waiting costs at most the pair already in flight — not the rest of the window.
#[test]
fn a_stop_hook_ends_the_stage_between_pairs_as_cancelled() {
    let db = Fixture::new();
    eight(&db);
    let mut scorer = Scripted::new(&[1.0]);
    let checks = std::cell::Cell::new(0);
    let stop = || {
        checks.set(checks.get() + 1);
        checks.get() > 1
    };
    let refused = db
        .run(
            "kafka",
            8,
            Some(RerankInput {
                reranker: &mut scorer,
                ask: RerankAsk::ByConfig,
                top_n: 5,
                text: RerankText::TitleBody,
                stop: Some(&stop),
            }),
        )
        .expect_err("a stage told to stop does not answer");
    assert!(
        matches!(refused, search::SearchError::Cancelled),
        "{refused:?}"
    );
    assert_eq!(scorer.asked.len(), 1, "one pair scored before the stop");
    assert_eq!(scorer.asked[0].1.len(), 1, "pairs go one per call");
}

/// **The stage reorders the first N and nothing else.** Planted scores `[1, 2, 3]` with `N = 3` put the third candidate
/// first inside the window; hits 4-8 keep their retrieval order behind it. A stage that resorted the whole list would make
/// "rerank the top few" and "rerank everything" the same operation, and the second one is not what anyone asked for.
#[test]
fn rerank_reorders_only_the_first_n_and_the_tail_keeps_retrieval_order() {
    let db = Fixture::new();
    eight(&db);

    // Ascending plants reverse the window: `rank_order` answers "which candidate sits at position i", so planting `[1, 2, 3]`
    // says the third candidate belongs first.
    let mut scorer = Scripted::new(&[1.0, 2.0, 3.0]);
    let answer = db
        .run(
            "kafka",
            8,
            Some(RerankInput {
                reranker: &mut scorer,
                ask: RerankAsk::ByConfig,
                top_n: 3,
                text: RerankText::TitleBody,
                stop: None,
            }),
        )
        .expect("keyword search with a stage answers");

    assert_eq!(
        answer.paths(),
        vec![
            "c.md", "b.md", "a.md", // the window, permuted by planted score
            "d.md", "e.md", "f.md", "g.md", "h.md", // the tail, untouched
        ],
        "{:?}",
        answer.paths()
    );
    assert!(answer.rerank.ran, "the stage ran");
    assert_eq!(answer.rerank.candidates, 3);
    assert_eq!(answer.rerank.skip, None);
    assert_eq!(answer.rerank.note, None);
    assert_eq!(
        scorer.asked.len(),
        1,
        "one call per query, not one per pair"
    );
    assert_eq!(
        scorer.asked[0].0, "kafka",
        "the query text is what it scores against"
    );
}

/// **Fewer hits than the window means the window shrinks**, and the report says how many were scored rather than how many
/// were asked for. `candidates` is the number a reader needs to cost the run, so it counts work done.
#[test]
fn the_stage_scores_what_it_is_given_and_reports_that_many() {
    let db = Fixture::new();
    db.note("a.md", Some("a"), "kafka broker");
    db.note("b.md", Some("b"), "kafka producer");

    let mut scorer = Scripted::new(&[1.0, 2.0]);
    let answer = db
        .run(
            "kafka",
            10,
            Some(RerankInput {
                reranker: &mut scorer,
                ask: RerankAsk::ByConfig,
                top_n: 5,
                text: RerankText::TitleBody,
                stop: None,
            }),
        )
        .expect("two hits answer");

    assert_eq!(answer.rerank.candidates, 2, "asked for 5, found 2 to score");
    assert_eq!(scorer.asked[0].1.len(), 2);
    assert_eq!(
        answer.paths(),
        vec!["b.md", "a.md"],
        "planted scores reorder both"
    );
}

/// **A hit outside the window carries no rerank evidence.** All three fields are `None` there and all three are `Some` for a
/// candidate: half-values would let a report print "ranked 7th" about a note the model never read.
#[test]
fn a_hit_that_was_not_a_candidate_carries_null_rerank_evidence() {
    let db = Fixture::new();
    eight(&db);

    let mut scorer = Scripted::new(&[1.0, 2.0, 3.0]);
    let answer = db
        .run(
            "kafka",
            8,
            Some(RerankInput {
                reranker: &mut scorer,
                ask: RerankAsk::ByConfig,
                top_n: 3,
                text: RerankText::TitleBody,
                stop: None,
            }),
        )
        .expect("eight hits answer");

    for (index, hit) in answer.hits.iter().enumerate() {
        let scored = index < 3;
        assert_eq!(
            hit.rerank_score.is_some(),
            scored,
            "{} at {index}: evidence must follow candidacy",
            hit.path
        );
        assert_eq!(hit.rerank_rank_before.is_some(), scored);
        assert_eq!(hit.rerank_rank_after.is_some(), scored);
    }
    // `rank_before` is retrieval position and `rank_after` the final one, both 1-based: with a flat planted score the order
    // reverses inside the window, so those two must differ for the first candidate.
    let first = &answer.hits[0];
    assert_eq!(first.path, "c.md");
    assert_eq!(first.rerank_rank_before, Some(3));
    assert_eq!(first.rerank_rank_after, Some(1));
    assert_eq!(
        first.rerank_score,
        Some(3.0),
        "the hit carries its own score, not the slot's"
    );
}

/// **No provider, no vectors, still reranked.** The stage consumes the index's strings and nothing else, so a workspace with
/// `provider = "none"` gets the same second pass as a hybrid one. This is the case that catches a stage wired to the
/// semantic half by accident — it would be silently dead here and every other test would still pass.
#[test]
fn the_stage_needs_no_embeddings_provider() {
    let db = Fixture::new();
    eight(&db);

    let mut scorer = Scripted::new(&[1.0, 2.0, 3.0, 4.0]);
    let answer = db
        .run(
            "kafka",
            4,
            Some(RerankInput {
                reranker: &mut scorer,
                ask: RerankAsk::ByConfig,
                top_n: 4,
                text: RerankText::TitleBody,
                stop: None,
            }),
        )
        .expect("keyword-only search answers");

    assert!(!answer.semantic_used, "no vectors were read");
    assert!(answer.keyword_used);
    assert!(answer.rerank.ran, "and the stage ran anyway");
    assert_eq!(answer.rerank.candidates, 4);
    // A deeper window than `--limit` scores more candidates than the report prints, which is what "rerank top 25, show 5"
    // means; here limit and window are equal so the counts agree.
    assert_eq!(answer.hits.len(), 4);
}

/// **The switch decides what the model reads**, and the two settings really are different strings: `title-body` sends
/// `title + "\n" + body`, `body` sends the body with no title and no leading newline. For a note whose `title` column is
/// NULL the two must produce **the same** bytes — padding it with a blank first line would score the absence of a title as
/// if it were content, which is the difference between measuring titles and measuring whitespace.
#[test]
fn rerank_text_picks_what_the_model_is_handed() {
    let db = Fixture::new();
    db.note("titled.md", Some("Kaffeemühle"), "entkalken mit zitrone");
    // An **empty** title rather than a NULL one: FTS5's title snippet comes back NULL for a NULL title and keyword search
    // refuses that today (pre-existing, unrelated to rerank). The stage's rule is the same for both — add nothing, not even a
    // newline — and `Some("")` reaches it through the ordinary path.
    db.note("plain.md", Some(""), "zitrone auch hier");

    let ask = |text: RerankText| {
        let mut scorer = Scripted::new(&[1.0]);
        let answer = db
            .run(
                "zitrone",
                5,
                Some(RerankInput {
                    reranker: &mut scorer,
                    ask: RerankAsk::ByConfig,
                    top_n: 5,
                    text,
                    stop: None,
                }),
            )
            .expect("both notes match the query word");
        assert!(answer.rerank.ran);
        scorer.docs()
    };

    let title_body = ask(RerankText::TitleBody);
    let body_only = ask(RerankText::Body);

    assert_eq!(title_body.len(), 2);
    assert_eq!(body_only.len(), 2);
    // Which note is which follows from the content, not from a position: each is found by a word only it carries, since both
    // bodies hold the query word (that is why both are candidates).
    let titled = |docs: &[String]| {
        docs.iter()
            .find(|d| d.contains("entkalken"))
            .cloned()
            .expect("the titled note is a candidate")
    };
    assert_eq!(
        titled(&title_body),
        "Kaffeemühle\nentkalken mit zitrone",
        "`title-body` joins with one newline"
    );
    assert_eq!(
        titled(&body_only),
        "entkalken mit zitrone",
        "`body` carries no title and no padding"
    );
    assert_ne!(
        titled(&title_body),
        titled(&body_only),
        "the switch changes bytes"
    );

    let untitled = |docs: &[String]| {
        docs.iter()
            .find(|d| d.contains("auch"))
            .cloned()
            .expect("the titleless note is a candidate")
    };
    assert_eq!(untitled(&title_body), "zitrone auch hier");
    assert_eq!(
        untitled(&body_only),
        untitled(&title_body),
        "a NULL title must not become a blank first line"
    );
}

/// **A stage that cannot run is a note, not an error, when configuration asked for it**: the answer still comes back in
/// retrieval order, and the report says why in one token. Silent degradation is what makes people distrust a reordered
/// list; a wrong exit code is what makes a script treat "no reranker installed" as "search broken".
#[test]
fn a_declined_stage_degrades_and_says_which_cause() {
    let db = Fixture::new();
    eight(&db);

    for (error, token) in [
        (RerankError::ArtifactAbsent, "rerank-artifact-missing"),
        (RerankError::RuntimeNotBuilt, "rerank-runtime-not-built"),
        (
            RerankError::ArtifactUnverified("digest".into()),
            "rerank-artifact-unverified",
        ),
        (
            RerankError::CannotScore("pair too long".into()),
            "rerank-cannot-score",
        ),
    ] {
        let mut scorer = Scripted::refusing(error);
        let answer = db
            .run(
                "kafka",
                3,
                Some(RerankInput {
                    reranker: &mut scorer,
                    ask: RerankAsk::ByConfig,
                    top_n: 3,
                    text: RerankText::TitleBody,
                    stop: None,
                }),
            )
            .expect("a declined stage never costs the answer");

        assert!(!answer.rerank.ran);
        assert_eq!(answer.rerank.candidates, 0, "nothing was scored");
        let note = answer.rerank.note.clone().expect("the cause is named");
        assert_eq!(note.token(), token);
        assert!(!note.label().is_empty(), "and it has a sentence");
        assert_eq!(
            answer.paths(),
            vec!["a.md", "b.md", "c.md"],
            "retrieval order survives untouched"
        );
    }
}

/// **Asked for by name, the same cause refuses.** `np search --rerank` exits 1 on this error, because a human who named the
/// flag wants to know they did not get what they asked for; the config path above answers and warns instead. One cause, two
/// policies, decided by [`RerankAsk`] rather than by two code paths that could drift.
#[test]
fn an_explicit_ask_that_cannot_be_honoured_is_an_error() {
    let db = Fixture::new();
    eight(&db);

    let mut scorer = Scripted::refusing(RerankError::ArtifactAbsent);
    let problem = db
        .run(
            "kafka",
            3,
            Some(RerankInput {
                reranker: &mut scorer,
                ask: RerankAsk::Explicit,
                top_n: 3,
                text: RerankText::TitleBody,
                stop: None,
            }),
        )
        .expect_err("an explicit request that cannot be honoured refuses");

    match &problem {
        search::SearchError::RerankUnavailable { cause } => {
            assert_eq!(cause.token(), "rerank-artifact-missing");
        }
        other => panic!("expected a rerank refusal, got {other:?}"),
    }
    assert!(
        problem.to_string().contains("rerank"),
        "the sentence names the stage: {problem}"
    );
}

/// **Nothing to score is a reason, not a fault.** A query whose hits are all figures (or that has no hits at all) leaves the
/// stage with no candidates: `ran = false`, `skip = NoCandidates`, no note. `Disabled` is what a run without a stage reports,
/// so `--json` can always answer "was this list reranked?" without inferring it from an empty field.
#[test]
fn no_candidates_and_no_stage_are_both_reported_by_name() {
    let db = Fixture::new();

    let mut scorer = Scripted::new(&[1.0]);
    let answer = db
        .run(
            "kafka",
            3,
            Some(RerankInput {
                reranker: &mut scorer,
                ask: RerankAsk::ByConfig,
                top_n: 3,
                text: RerankText::TitleBody,
                stop: None,
            }),
        )
        .expect("an empty answer is still an answer");
    assert!(!answer.rerank.ran);
    assert_eq!(answer.rerank.skip, Some(RerankSkip::NoCandidates));
    assert_eq!(answer.rerank.note, None);
    assert!(scorer.asked.is_empty(), "nothing to score means no call");

    let plain = db.run("kafka", 3, None).expect("no stage answers");
    assert!(!plain.rerank.ran);
    assert_eq!(plain.rerank.skip, Some(RerankSkip::Disabled));
    assert_eq!(plain.rerank.candidates, 0);
}

/// The two non-fault reasons are **result-document words, not diagnostic rule tokens**: nothing was degraded, so there is
/// nothing for an operator to alert on. Pinned here so a future edit cannot start emitting `rerank-disabled` as a rule.
#[test]
fn skip_reasons_are_tokens_for_the_document_only() {
    assert_eq!(RerankSkip::Disabled.token(), "disabled");
    assert_eq!(RerankSkip::NoCandidates.token(), "no-candidates");
    // And the four causes have both halves: a document token and a diagnostic rule.
    for (note, doc, diag) in [
        (
            RerankNote::ArtifactAbsent,
            "rerank-artifact-missing",
            np_core::diagnostics::rule::RERANK_ARTIFACT_MISSING,
        ),
        (
            RerankNote::RuntimeNotBuilt,
            "rerank-runtime-not-built",
            np_core::diagnostics::rule::RERANK_RUNTIME_NOT_BUILT,
        ),
        (
            RerankNote::ArtifactUnverified("x".into()),
            "rerank-artifact-unverified",
            np_core::diagnostics::rule::RERANK_ARTIFACT_UNVERIFIED,
        ),
        (
            RerankNote::CannotScore("y".into()),
            "rerank-cannot-score",
            np_core::diagnostics::rule::RERANK_CANNOT_SCORE,
        ),
    ] {
        assert_eq!(note.token(), doc);
        assert_eq!(note.diag_rule(), diag);
        assert!(
            np_core::diagnostics::RULE_TOKENS.contains(&diag),
            "{diag} must be published"
        );
    }
}

/// **The remedy names only doors this build can open** — the rule every [`np_core::search::RankNote`] sentence already follows,
/// and the reason it exists twice in `search.rs` rather than once. A `fetch` build has two ways to place these bytes and names
/// both: `np models fetch` transfers them, and `np models install` is the offline door whose digest comes from the pin. A build
/// without `fetch` must not name a subcommand that was compiled out of it.
#[test]
fn absent_artifact_names_the_door_this_build_can_open() {
    let sentence = RerankNote::ArtifactAbsent.label();
    assert!(
        sentence.contains(np_core::pins::RERANKER_ID) && sentence.contains("np models install"),
        "the id and the door that exists in every build: {sentence}"
    );
    assert_eq!(
        sentence.contains("np models fetch"),
        cfg!(feature = "fetch"),
        "the wording a binary prints is the wording that binary can honour: {sentence}"
    );
}

/// The stage's own arithmetic must not be reachable by accident from the default path: with no input the answer is byte-for-byte
/// what it was before this feature, and `rerank` says `disabled`. `tests/search.rs`'s read-only fence runs the same path.
#[test]
fn without_a_stage_nothing_moves_and_nothing_is_scored() {
    let db = Fixture::new();
    eight(&db);

    let answer = db
        .run("kafka", 8, None)
        .expect("plain keyword search answers");
    assert_eq!(
        answer.paths(),
        vec!["a.md", "b.md", "c.md", "d.md", "e.md", "f.md", "g.md", "h.md"]
    );
    assert!(answer.hits.iter().all(|hit| hit.rerank_score.is_none()
        && hit.rerank_rank_before.is_none()
        && hit.rerank_rank_after.is_none()));
}
