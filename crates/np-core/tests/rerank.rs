//! The rerank recipe apart from any weights: how a (query, document) pair is assembled, what fits in the window, and
//! how scores become an order.
//!
//! These three are the parts of a rerank stage that can be wrong without any model being loaded, and they are the
//! parts a wrong answer on would look like a *quality* problem rather than a bug. A cross-encoder whose pair is
//! assembled wrongly still returns a confident float; a reorder that trusts `partial_cmp` still returns a permutation.
//! So the recipe is pinned here against the tree it mirrors, in a default build, with no artifact and no database.
//!
//! The shape of a pair is not our invention. It is llama.cpp v0.8.0's own fallback for building a reranking sequence
//! (`tools/server/server-common.cpp:2010-2032`), spelled once over here — including the part a model card leaves out,
//! which is that the three `add_*` vocabulary flags decide what gets emitted and the special-token ids decide whether
//! emission is even possible. Two publishers of one set of weights differ on `tokenizer.ggml.add_sep_token`, so a
//! recipe that assumed the flags would score one build with a token the other build's tokenizer never inserts.

use np_core::rerank::{
    fit_pair, pair_tokens, rank_order, Fit, NoReranker, RerankError, Reranker, Specials, TokenId,
    VocabFlags,
};

/// A vocabulary that has all three specials: `bos = 0`, and EOS and SEP sharing id `2` — which is what the pinned
/// reranker's own header says (`tokenizer.ggml.bos_token_id = 0`, `eos_token_id = seperator_token_id = 2`). Shared by
/// every case below so "the ids came apart" cannot be confused with "the recipe changed".
const ALL: Specials = Specials {
    bos: Some(0),
    eos: Some(2),
    sep: Some(2),
};

fn flags(bos: bool, eos: bool, sep: bool) -> VocabFlags {
    VocabFlags {
        add_bos: bos,
        add_eos: eos,
        add_sep: sep,
    }
}

/// The query and document are tiny and distinct so every emitted token is attributable to exactly one part.
const QUERY: &[TokenId] = &[10, 11];
const DOC: &[TokenId] = &[20];

#[test]
fn pair_tokens_follows_the_pinned_fallback_over_the_flag_table() {
    let all = pair_tokens(&ALL, &flags(true, true, true), QUERY, DOC);
    assert_eq!(
        all,
        vec![0, 10, 11, 2, 2, 20, 2],
        "[BOS] query [EOS] [SEP] doc [EOS] — the separator sits between the halves and one EOS closes each side"
    );

    let no_sep = pair_tokens(&ALL, &flags(true, true, false), QUERY, DOC);
    assert_eq!(
        no_sep,
        vec![0, 10, 11, 2, 20, 2],
        "without add_sep_token the halves run together and only one EOS is inserted between them: this is what a \
         publisher that omits the GGUF key gets, and it is a different sequence of tokens from the row above"
    );

    let sep_only = pair_tokens(&ALL, &flags(false, false, true), QUERY, DOC);
    assert_eq!(sep_only, vec![10, 11, 2, 20], "each flag stands alone");

    let bare = pair_tokens(&ALL, &flags(false, false, false), QUERY, DOC);
    assert_eq!(
        bare,
        vec![10, 11, 20],
        "no flags means the two texts concatenated"
    );
}

/// A flag says the tokenizer *would* insert a token; an id says this vocabulary has one. When they disagree, the id
/// wins, because writing `LLAMA_TOKEN_NULL` into a batch is not "no token" — it is an invalid token id that
/// [`llama_decode`] refuses (or worse, reads as a real row on a vocabulary wide enough to have index -1 nowhere).
#[test]
fn a_special_this_vocabulary_does_not_have_is_never_emitted() {
    let no_sep = Specials {
        bos: Some(0),
        eos: Some(2),
        sep: None,
    };
    assert_eq!(
        pair_tokens(&no_sep, &flags(true, true, true), QUERY, DOC),
        vec![0, 10, 11, 2, 20, 2],
        "add_sep is set and there is no separator to add"
    );

    let no_bos = Specials {
        bos: None,
        eos: Some(2),
        sep: Some(2),
    };
    assert_eq!(
        pair_tokens(&no_bos, &flags(true, true, true), QUERY, DOC),
        vec![10, 11, 2, 2, 20, 2]
    );

    let no_eos = Specials {
        bos: Some(0),
        eos: None,
        sep: Some(2),
    };
    assert_eq!(
        pair_tokens(&no_eos, &flags(true, true, true), QUERY, DOC),
        vec![0, 10, 11, 2, 20],
        "EOS closes both halves, so its absence costs two tokens"
    );
}

#[test]
fn fit_pair_keeps_the_query_and_cuts_the_note_tail() {
    let doc = vec![20; 20];
    let fit = fit_pair(&ALL, &flags(false, false, false), QUERY, &doc, 8);
    match fit {
        Fit::Truncated { tokens, dropped } => {
            assert_eq!(tokens.len(), 8, "the budget is a ceiling, not a suggestion");
            assert_eq!(dropped, 14, "twenty minus the six that fit beside the query");
            assert_eq!(&tokens[..2], QUERY, "the query is never what gets cut");
            assert_eq!(
                &tokens[2..],
                &[20; 6],
                "the cut falls at a token boundary, at the tail"
            );
        }
        other => panic!("expected a truncation, got {other:?}"),
    }
}

/// A query too long to score against anything is a different refusal from a note too long to fit beside it: cutting
/// the note loses part of an answer, and cutting the query would silently answer a different question. So the query
/// side is never truncated, and the caller is told the cheapest pair this query could ever have formed.
#[test]
fn fit_pair_refuses_a_query_that_does_not_fit_on_its_own() {
    let long = vec![10; 6];
    let fit = fit_pair(&ALL, &flags(false, false, false), &long, DOC, 4);
    assert_eq!(
        fit,
        Fit::QueryTooLong {
            tokens: 6,
            budget: 4
        }
    );

    // With specials the cheapest pair is longer than the query, and that is the number worth reporting: it is what
    // the window would have to hold for *any* document, including an empty one.
    let fit = fit_pair(&ALL, &flags(true, true, true), &long, &[], 9);
    assert_eq!(
        fit,
        Fit::QueryTooLong {
            tokens: 10,
            budget: 9
        },
        "the four specials are priced in"
    );
}

/// The specials are part of the bill. A budget that covers both texts but not the tokens wrapped around them must
/// still come back under budget — an over-budget token list is the bug this case exists for, because it survives
/// `fit_pair` and dies inside `llama_decode`.
#[test]
fn fit_pair_counts_the_specials_it_will_add() {
    let doc = vec![20; 4];
    // query 2 + doc 4 = 6, plus four specials = 10 asked against a budget of 8.
    let fit = fit_pair(&ALL, &flags(true, true, true), QUERY, &doc, 8);
    match fit {
        Fit::Truncated { tokens, dropped } => {
            assert_eq!(tokens.len(), 8);
            assert_eq!(dropped, 2);
            assert_eq!(
                tokens.first(),
                Some(&0),
                "the wrapper is intact at the front"
            );
            assert_eq!(tokens.last(), Some(&2), "and at the back");
        }
        other => panic!("expected a truncation, got {other:?}"),
    }

    // The same texts with no specials fit whole, which is the point: the budget question is about the assembled
    // pair, never about the two halves separately.
    assert!(matches!(
        fit_pair(&ALL, &flags(false, false, false), QUERY, &doc, 8),
        Fit::Whole(_)
    ));

    // An empty document still pays its wrapper: two query tokens plus four specials fill a budget of six exactly,
    // and one token less is already nothing to score.
    assert!(matches!(
        fit_pair(&ALL, &flags(true, true, true), QUERY, &[], 6),
        Fit::Whole(_)
    ));
    let fit = fit_pair(&ALL, &flags(true, true, true), QUERY, &[], 5);
    assert_eq!(
        fit,
        Fit::QueryTooLong {
            tokens: 6,
            budget: 5
        },
        "the empty document is the cheapest pair there is, and it does not fit either"
    );
}

#[test]
fn rank_order_is_a_total_order_that_breaks_ties_by_retrieval_position() {
    let scores = [0.1, 0.9, 0.9, f64::NAN];
    assert_eq!(
        rank_order(&scores),
        vec![1, 2, 0, 3],
        "descending, equal scores keep the earlier candidate first, and a NaN cannot decide anything so it lands last"
    );

    assert_eq!(rank_order(&[]), Vec::<usize>::new());
    assert_eq!(rank_order(&[0.5]), vec![0]);
    assert_eq!(
        rank_order(&[-0.5, -0.1, -0.9]),
        vec![1, 0, 2],
        "a cross-encoder's scores are logits: negatives are the ordinary case"
    );

    // A permutation, in both directions: every index once.
    let scores = [3.0, 1.0, 4.0, 1.0, 5.0];
    let order = rank_order(&scores);
    assert_eq!(order.len(), scores.len());
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![0, 1, 2, 3, 4]);
}

/// The disabled and not-built doors both need something that implements the trait and refuses, so the stage has one
/// code path. What matters is that the refusal keeps its cause: "off by configuration" and "this binary was built
/// without a runtime" become different sentences at the CLI, and the same variant on the way out.
#[test]
fn no_reranker_refuses_with_its_cause() {
    let cases = [
        RerankError::RuntimeNotBuilt,
        RerankError::ArtifactAbsent,
        RerankError::ArtifactUnverified("digest mismatch".to_string()),
        RerankError::CannotScore("the artifact is not a rank head".to_string()),
        RerankError::PairTooLong {
            tokens: 3000,
            budget: 2048,
        },
        RerankError::Runtime("the runtime refused to run this pair".to_string()),
    ];
    for case in cases {
        let expected = case.to_string();
        let mut reranker = NoReranker(case);
        let problem = reranker
            .score("q", &["d"])
            .expect_err("a reranker that is not there cannot score");
        assert_eq!(problem.to_string(), expected);
    }
}
