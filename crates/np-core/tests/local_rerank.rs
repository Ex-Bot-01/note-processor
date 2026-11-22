//! Rank mode against a **real** cross-encoder: what the pinned artifact answers with, and whether the numbers mean
//! anything.
//!
//! Three of these cases are artifact-gated on `NP_TEST_RERANK_PATH` and print `NOT COVERAGE` when it is unset — F18's
//! rule that an absent 600 MB file must look like neither a failure nor coverage. Two need only the feature: a path
//! that does not exist, and an embedding model pointed at rank mode (gated on `NP_TEST_MODEL_PATH`, since that is the
//! other artifact this suite already knows how to find).
//!
//! ```text
//! NP_TEST_RERANK_PATH=/path/to/bge-reranker-v2-m3 \
//!   cargo test -p np-core --features local-model --test local_rerank -- --nocapture
//! ```
//!
//! **What the gated cases claim, and what they deliberately do not.** They claim the artifact loads as a rank head,
//! that its head is one number wide, that the tokenizer's specials are what its GGUF says they are, and that a relevant
//! pair outscores an irrelevant one in each of the two languages this round measures. They do **not** claim search got
//! better: eight hand-written pairs prove the wiring reads the right end of the model, not that retrieval improved on a
//! real vault — that question belongs to the fixture harness and a human reading the numbers
//! (`docs/design-f51-rerank-stage.md` §7).

/// The artifact-gated claims plus the refusals that need only the feature.
#[cfg(feature = "local-model")]
mod ranking {
    use std::path::PathBuf;
    use std::time::Instant;

    use np_core::embed::EmbedError;
    use np_core::local_model;
    use np_core::rerank::{RerankError, Reranker, VocabFlags};

    /// The reranker under test, or `None` when the runner has no artifact — which is the ordinary state on a CI
    /// runner, and every artifact-gated caller turns it into a printed `NOT COVERAGE`.
    fn artifact() -> Option<PathBuf> {
        std::env::var_os("NP_TEST_RERANK_PATH").map(PathBuf::from)
    }

    macro_rules! artifact_or_not_coverage {
        () => {
            match artifact() {
                Some(path) => path,
                None => {
                    println!(
                        "NOT COVERAGE: NP_TEST_RERANK_PATH is unset, so no reranker GGUF was loaded by this case. \
                         Re-run with it pointing at bge-reranker-v2-m3 (sha256 d32a877b…) to load one."
                    );
                    return;
                }
            }
        };
    }

    /// One (query, relevant, irrelevant) triple. Hand-written and short on purpose: they are here to catch a scorer
    /// that reads the wrong end of the model or assembles a pair without its separator, and a subtle benchmark would
    /// only make that failure harder to see.
    struct Triple {
        query: &'static str,
        relevant: &'static str,
        irrelevant: &'static str,
    }

    /// Five English and five German triples. The language pair is the point as much as the content is: this artifact is
    /// `v2-m3`, sold as multilingual, and a scorer that only works on ASCII text would pass an English-only suite.
    const TRIPLES: &[Triple] = &[
        Triple {
            query: "How do I back up my notes?",
            relevant: "Backups: copy the whole notes directory to an external disk every Friday, then verify one file opens.",
            irrelevant: "The café on Moltkestraße serves a very long espresso and better cake in the afternoon.",
        },
        Triple {
            query: "What should I water the ferns with?",
            relevant: "Ferns prefer rainwater or distilled water; tap water leaves mineral deposits on the fronds over time.",
            irrelevant: "The train from Hamburg to Berlin runs hourly and the seat reservation costs four euros.",
        },
        Triple {
            query: "Why does the build fail on a clean checkout?",
            relevant: "A clean checkout has no build directory, so the codegen step must run before linking or the generated sources are missing.",
            irrelevant: "This note collects recipes for bread with sourdough starter and a long cold proof in the fridge.",
        },
        Triple {
            query: "Wie sichere ich meine Notizen?",
            relevant: "Sicherung: Kopieren Sie das Notizverzeichnis jeden Freitag auf eine externe Festplatte und prüfen Sie, ob eine Datei sich öffnen lässt.",
            irrelevant: "Das Café in der Moltkestraße serviert einen sehr langen Espresso und nachmittags guten Kuchen.",
        },
        Triple {
            query: "Womit gieße ich die Farne?",
            relevant: "Farne mögen Regenwasser oder entmineralisiertes Wasser; Leitungswasser hinterlässt auf Dauer Kalkflecken auf den Blättern.",
            irrelevant: "Der Zug von Hamburg nach Berlin fährt stündlich und die Platzreservierung kostet vier Euro.",
        },
        Triple {
            query: "Warum schlägt der Build auf einem frischen Checkout fehl?",
            relevant: "Ein frischer Checkout hat kein Build-Verzeichnis, deshalb muss der Codegen-Schritt vor dem Binden laufen, sonst fehlen die erzeugten Quellen.",
            irrelevant: "Diese Notiz sammelt Rezepte für Brot mit Sauerteig-Ansatz und einer langen kalten Gare im Kühlschrank.",
        },
        Triple {
            query: "Which cable charges the keyboard?",
            relevant: "The keyboard takes USB-C; the old micro-USB cable from the drawer fits mechanically but will not negotiate the higher current.",
            irrelevant: "Roses want six hours of direct sun and a generous amount of compost worked into the bed each spring.",
        },
        Triple {
            query: "Wann muss ich die Rosen schneiden?",
            relevant: "Rosen schneidet man im späten Februar, wenn die stärkste Fröste vorbei sind; etwa ein Drittel der alten Triebe bleiben stehen.",
            irrelevant: "Der Drucker zieht Papier aus dem unteren Fach, wenn das obere leer ist, und meldet sonst einen Stau.",
        },
        Triple {
            query: "How long does the visa appointment take?",
            relevant: "The appointment itself is fifteen minutes; plan for an hour of waiting, and bring the printed confirmation and two biometric photos.",
            irrelevant: "This spreadsheet lists every podcast episode from 2019 with its duration and guest names.",
        },
        Triple {
            query: "Wie viel Speicher braucht ein Video-Schnitt?",
            relevant: "Rechne mit der zehnfachen Dateigröße des fertigen Videos für Zwischendateien und Cache, plus Reserve für Rohmaterial von der Kamera.",
            irrelevant: "Der Wecker im Flur piepst dreimal, bevor das Licht angeht, und lässt sich nicht dauerhaft leiser stellen.",
        },
    ];

    /// The pinned artifact loads, and what it answers with is measured rather than assumed.
    ///
    /// Every assertion here is a sentence from the design turned into bytes: the head is one number wide (so the read of
    /// one float is inside the buffer), the window is at most the pair ceiling we asked for and never wider than the
    /// artifact was trained on, and the tokenizer's specials are exactly what its GGUF declares — including the two that
    /// share an id, which is why the recipe reads ids instead of assuming a `[SEP]` sits between the halves.
    #[test]
    fn the_pinned_artifact_loads_in_rank_mode() {
        let path = artifact_or_not_coverage!();
        let started = Instant::now();
        let ranker = local_model::load_rank(&path).unwrap_or_else(|problem| {
            panic!("the pinned artifact refused to load as a rank head: {problem}")
        });
        println!(
            "loaded in {:?}: window {} tokens, head {} numbers",
            started.elapsed(),
            ranker.window(),
            ranker.n_cls_out()
        );

        assert_eq!(
            ranker.n_cls_out(),
            1,
            "the read is one float; a wider head is refused at load"
        );
        assert!(
            ranker.window() > 0 && ranker.window() <= 2_048,
            "the pair window came back as {}, which is outside (0, 2048]",
            ranker.window()
        );

        // The pinned file's own header: `bos_token_id = 0`, `eos_token_id = seperator_token_id = 2`, and all three
        // `add_*_token` flags present. A different publisher's quantization of the same weights can omit
        // `add_sep_token`, which changes the scored sequence — so it is asserted, not assumed.
        let flags = ranker.flags();
        assert_eq!(
            flags,
            VocabFlags {
                add_bos: true,
                add_eos: true,
                add_sep: true
            },
            "this is the flag table the pair recipe was written against"
        );
        let specials = ranker.specials();
        assert_eq!(specials.bos, Some(0));
        assert_eq!(specials.eos, Some(2));
        assert_eq!(
            specials.sep, specials.eos,
            "for this tokenizer the separator and the end-of-sentence token are one id; the recipe still asks for both \
             roles separately"
        );
    }

    /// Every relevant document outscores its irrelevant partner, in all ten triples.
    ///
    /// This is the smallest claim that would catch a real bug: reading `n_embd_out` floats and returning the first, or
    /// assembling `[query][doc]` without the separator, both still produce numbers — they just stop producing this
    /// ordering. Scores are printed because an ordering with no magnitudes cannot be eyeballed for confidence.
    #[test]
    fn a_relevant_pair_outranks_an_irrelevant_one_in_both_languages() {
        let path = artifact_or_not_coverage!();
        let mut ranker = local_model::load_rank(&path).expect("the pinned artifact loads");

        let started = Instant::now();
        let mut failures = Vec::new();
        for (index, triple) in TRIPLES.iter().enumerate() {
            let scores = ranker
                .score(triple.query, &[triple.relevant, triple.irrelevant])
                .expect("both documents are short enough to score");
            let (relevant, irrelevant) = (scores[0], scores[1]);
            println!(
                "[{:>2}] {:>7.4} vs {:>7.4}  {:?}",
                index, relevant, irrelevant, triple.query
            );
            if relevant <= irrelevant {
                failures.push((triple.query, relevant, irrelevant));
            }
        }
        println!(
            "{} triples scored in {:?} ({:?} each)",
            TRIPLES.len(),
            started.elapsed(),
            started.elapsed() / (2 * TRIPLES.len() as u32)
        );
        assert!(
            failures.is_empty(),
            "{} of {} pairs came back the wrong way round: {:?}",
            failures.len(),
            TRIPLES.len(),
            failures
        );
    }

    /// The head emits real logits, not a constant.
    ///
    /// An ordering test alone would pass on a scorer that returned `0.0` for everything — it wouldn't pass the assertion
    /// above, but it would pass one written against a single pair, and this is the cheap check that keeps any future
    /// version of that test honest. Both signs appear because a cross-encoder's scale is a logit: relevant text is
    /// usually positive and irrelevant usually negative, and a head pinned to one side of zero would say so here.
    #[test]
    fn scores_are_finite_and_span_both_signs() {
        let path = artifact_or_not_coverage!();
        let mut ranker = local_model::load_rank(&path).expect("the pinned artifact loads");

        let mut all = Vec::new();
        for triple in TRIPLES {
            all.extend(
                ranker
                    .score(triple.query, &[triple.relevant, triple.irrelevant])
                    .expect("both documents are short enough to score"),
            );
        }
        assert_eq!(all.len(), 2 * TRIPLES.len());
        assert!(all.iter().all(|s| s.is_finite()), "every score is a number");
        assert!(
            all.iter().any(|s| *s > 0.0),
            "nothing scored above zero: {all:?} — a head returning only negatives is not ranking these pairs"
        );
        assert!(
            all.iter().any(|s| *s < 0.0),
            "nothing scored below zero: {all:?}"
        );
    }

    /// A query that cannot fit the window beside an empty document is refused, and nothing is decoded.
    ///
    /// The refusal happens before the forward pass, so this case costs a tokenization rather than ten seconds — which is
    /// also the point: a long query must not become twenty-nine scores of a truncated question.
    #[test]
    fn a_query_wider_than_the_window_is_refused_rather_than_truncated() {
        let path = artifact_or_not_coverage!();
        let mut ranker = local_model::load_rank(&path).expect("the pinned artifact loads");

        // Far past any window this build can build: the pair ceiling is 2,048 tokens and this is ~12,000 words.
        let query = "water ".repeat(12_000);
        let problem = ranker
            .score(&query, &["a short document"])
            .expect_err("a twelve-thousand-word query cannot be scored beside anything");
        match problem {
            RerankError::PairTooLong { tokens, budget } => {
                assert!(
                    tokens > budget,
                    "{tokens} was reported against a budget of {budget}"
                );
                assert!(
                    budget <= 2_048 && budget > 0,
                    "the budget named is this ranker's own window, got {budget}"
                );
                assert_eq!(budget, ranker.window());
            }
            other => panic!("expected a too-long refusal, got {other:?}"),
        }
    }

    /// A path that does not exist is refused with the artifact sentence, before llama.cpp is asked for anything.
    #[test]
    fn an_absent_artifact_is_refused_by_name() {
        let missing = PathBuf::from("/definitely/not/here/bge-reranker-v2-m3");
        let problem =
            local_model::load_rank(&missing).expect_err("a path that does not exist cannot load");
        assert!(
            matches!(problem, EmbedError::ArtifactNotLoadable { .. }),
            "expected a named-artifact refusal, got {problem:?}"
        );
    }

    /// An **embedding** model is not a reranker, and rank mode says so rather than scoring garbage.
    ///
    /// This is the pair of refusals that makes "capability is measured, not declared" true in both directions: `load`
    /// refuses a RANK artifact ("this artifact produces a score, not a sentence vector"), and this refuses an MEAN one.
    /// Which arm fires depends on what llama.cpp resolves for that file's pooling, so the assertion is about the class
    /// of answer rather than one sentence.
    #[test]
    fn an_embedding_model_is_not_a_reranker() {
        let Some(model) = std::env::var_os("NP_TEST_MODEL_PATH").map(PathBuf::from) else {
            println!(
                "NOT COVERAGE: NP_TEST_MODEL_PATH is unset, so no embedding artifact was pushed through rank \
                 mode. Re-run with it set to exercise the cross-refusal."
            );
            return;
        };
        let problem = local_model::load_rank(&model)
            .expect_err("an embedding model must not load as a rank head");
        println!("rank mode answered about an embedder: {problem}");
        assert!(
            matches!(problem, EmbedError::Unavailable(_)),
            "expected a refusal that names what the artifact turned out to be, got {problem:?}"
        );
    }

    /// Both purposes refuse each other's artifacts, and neither refusal is a panic.
    ///
    /// Kept next to the case above because it is the same claim read from the other side: an embedding loader pointed at
    /// the reranker must not return a model that produces vectors.
    #[test]
    fn an_embedding_loader_refuses_a_reranker() {
        let path = artifact_or_not_coverage!();
        let problem =
            local_model::load(&path, 1024).expect_err("a reranker is not a 1024-wide embedder");
        println!("the embedding loader answered about a reranker: {problem}");
        assert!(
            matches!(
                problem,
                EmbedError::ProviderDimMismatch { .. } | EmbedError::Unavailable(_)
            ),
            "expected a width or pooling refusal, got {problem:?}"
        );
    }
}

/// The loud half of "a feature-off run is not coverage" — the same rule [`local_load`](crate) states for the link
/// proof. Without `local-model` there is no rank loader to call.
#[cfg(not(feature = "local-model"))]
#[test]
fn local_model_feature_is_not_built() {
    println!("NOT COVERAGE: np-core was built without `--features local-model`, so no reranker GGUF was loaded and no \
              rank head was read. Re-run with: cargo test -p np-core --features local-model --test local_rerank");
}
