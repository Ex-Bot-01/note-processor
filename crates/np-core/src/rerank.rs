//! The rerank stage's arithmetic, with no weights in sight.
//!
//! Three things live here: how a scored pair is assembled ([`pair_tokens`], [`fit_pair`]), how scores become an order
//! ([`rank_order`]), and the seam a scorer is plugged into ([`Reranker`]). All three are decided without asking a model
//! anything, which is why this module is **not** behind `local-model`: a binary built without llama.cpp still has to be
//! able to say "reranking is off" and "reranking is on but the artifact is missing", and both of those sentences name
//! something defined here.
//!
//! The split is deliberate. The half that touches FFI — loading an artifact in rank mode, running a batch, reading one
//! float out of a pooled row — is `crate::local_model::load_rank`, gated on the feature. What is here is everything a
//! wrong answer on would look like a *quality* problem instead of a bug: a pair assembled with the wrong specials still
//! yields a confident score, and an ordering that trusts [`f64::partial_cmp`] still yields a permutation. Those are not
//! things a real artifact can be asked to notice, so they are pinned here where no artifact is needed.
//!
//! One naming note for readers who arrive from the gated half: this module's `Specials` and `VocabFlags` are filled by
//! `local_model`, which reads them off a loaded artifact. They live on this side of the gate so the recipe can be read
//! — and its tests run — without llama.cpp in the build.

use thiserror::Error;

/// A token id in the spelling llama.cpp uses (`llama_token` is `int32_t`, `include/llama.h:36`).
///
/// Named here rather than reused from `crate::local_model` because this module compiles without the runtime, and a
/// recipe that could not be read without llama.cpp would have to be re-derived at every review.
pub type TokenId = i32;

/// Which special tokens this vocabulary's tokenizer inserts on request (`tokenizer.ggml.add_*_token`).
///
/// **A flag is not an id.** These three say "when asked with `add_special = true`, insert it"; [`Specials`] says
/// whether there is anything to insert. Both are read from the artifact rather than assumed, because publishers of one
/// set of weights disagree here: the pinned reranker's GGUF carries `add_sep_token`, and another publisher's
/// quantization of the same model omits the key — which changes the scored sequence, not merely its description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VocabFlags {
    /// `tokenizer.ggml.add_bos_token`.
    pub add_bos: bool,
    /// `tokenizer.ggml.add_eos_token`.
    pub add_eos: bool,
    /// `tokenizer.ggml.add_sep_token` — the one that differs between builds of one model.
    pub add_sep: bool,
}

/// The special-token ids this vocabulary actually has, `None` where it has none.
///
/// `None` is llama.cpp's own answer for an absent special (`LLAMA_TOKEN_NULL`, -1, declared in the `local-model`-gated
/// `crate::llama_ffi`) and is kept as `None` rather than as a sentinel id: `-1`
/// written into a batch is not "nothing", it is an invalid token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Specials {
    /// Beginning of sequence (`llama_vocab_bos`).
    pub bos: Option<TokenId>,
    /// End of sequence (`llama_vocab_eos`) — which closes *both* halves of a pair.
    pub eos: Option<TokenId>,
    /// Sentence separator (`llama_vocab_sep`). For a BERT-family artifact this is commonly the same id as `eos`; it is
    /// read separately anyway, because "these two numbers happen to be equal" is not a fact worth depending on across
    /// artifacts.
    pub sep: Option<TokenId>,
}

/// Assemble one scored pair: `[BOS?] · query · [EOS?] · [SEP?] · doc · [EOS?]`.
///
/// This is llama.cpp v0.8.0's fallback for building a reranking sequence (`tools/server/server-common.cpp:2010-2032`),
/// reproduced rather than improvised: a pair assembled differently from the way the runtime's own tool assembles one is
/// a pair the weights were not tuned on, and the score it returns is still a plausible float.
///
/// A token is emitted when its flag is set **and** this vocabulary has an id for it — see [`VocabFlags`] and
/// [`Specials`]. Note that EOS appears twice: it closes the query and closes the pair, so a vocabulary without one
/// loses two tokens rather than one.
///
/// `query` and `doc` are already tokenised by the caller with `add_special = false`; this function adds specials and
/// nothing else, so its output length is exactly `query.len() + doc.len()` plus the specials it emitted.
#[must_use]
pub fn pair_tokens(
    specials: &Specials,
    flags: &VocabFlags,
    query: &[TokenId],
    doc: &[TokenId],
) -> Vec<TokenId> {
    let mut out = Vec::with_capacity(query.len() + doc.len() + 4);
    if flags.add_bos {
        push(&mut out, specials.bos);
    }
    out.extend_from_slice(query);
    if flags.add_eos {
        push(&mut out, specials.eos);
    }
    if flags.add_sep {
        push(&mut out, specials.sep);
    }
    out.extend_from_slice(doc);
    if flags.add_eos {
        push(&mut out, specials.eos);
    }
    out
}

/// The number of wrapper tokens [`pair_tokens`] would emit — priced by [`fit_pair`] before it decides anything.
fn overhead(specials: &Specials, flags: &VocabFlags) -> usize {
    usize::from(flags.add_bos && specials.bos.is_some())
        + usize::from(flags.add_eos && specials.eos.is_some())
        // The separator between the halves. The closing EOS is the second use of `add_eos`, counted above only once,
        // so it is added here rather than doubling the line above.
        + usize::from(flags.add_sep && specials.sep.is_some())
        + usize::from(flags.add_eos && specials.eos.is_some())
}

fn push(out: &mut Vec<TokenId>, token: Option<TokenId>) {
    if let Some(token) = token {
        out.push(token);
    }
}

/// What [`fit_pair`] decided about one pair and one budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fit {
    /// Both halves fit as they are.
    Whole(Vec<TokenId>),
    /// The document's tail was cut to make room. `dropped` counts the document tokens left out — never query tokens,
    /// which are never cut ([`Fit::QueryTooLong`]).
    Truncated {
        /// The assembled pair, always within the budget.
        tokens: Vec<TokenId>,
        /// Document tokens that did not fit, counted at a token boundary from the tail.
        dropped: usize,
    },
    /// The query plus its wrapper exceeds the budget, so no document of any length could be scored beside it.
    QueryTooLong {
        /// Tokens the *cheapest* possible pair would need — the query plus its wrapper, with an empty document. This is
        /// the number worth naming: it is what the window would have to hold for this query at all.
        tokens: usize,
        /// The window it was measured against.
        budget: usize,
    },
}

/// Fit one (query, document) pair into `budget` tokens, cutting the document's tail if it has to.
///
/// **The wrapper is priced first.** A budget that covers both texts but not the tokens wrapped around them is not a fit,
/// and handing the caller back a list one token over budget would move the failure from here to inside `llama_decode`,
/// where it arrives as a runtime refusal with no mention of budgets.
///
/// **The query is never cut.** Truncating a note loses part of an answer, which a caller can report and a user can
/// shorten; truncating the query would answer a different question than the one asked, and return it as if it had been.
/// So a query that cannot fit beside nothing at all is [`Fit::QueryTooLong`], and the document side is the only thing
/// that shrinks.
#[must_use]
pub fn fit_pair(
    specials: &Specials,
    flags: &VocabFlags,
    query: &[TokenId],
    doc: &[TokenId],
    budget: usize,
) -> Fit {
    let overhead = overhead(specials, flags);
    if query.len() + overhead > budget {
        return Fit::QueryTooLong {
            tokens: query.len() + overhead,
            budget,
        };
    }
    let room = budget - overhead - query.len();
    if doc.len() <= room {
        return Fit::Whole(pair_tokens(specials, flags, query, doc));
    }
    let dropped = doc.len() - room;
    Fit::Truncated {
        tokens: pair_tokens(specials, flags, query, &doc[..room]),
        dropped,
    }
}

/// Order `scores` descending, returning a permutation (`out[i]` is the index into `scores` of the item that should
/// sit at position `i`).
///
/// Two rules, both load-bearing:
///
/// - **Ties keep retrieval order.** Equal scores are common — a head saturating near 1.0 produces runs of them — and a
///   tie broken by sort internals is a result that changes when the code is refactored. Keeping the retriever's order
///   means the stage only ever reorders what it disagrees with, which is the claim a rerank stage is allowed to make.
/// - **A NaN cannot decide anything.** [`f64::total_cmp`] is used rather than [`f64::partial_cmp`] because `partial_cmp`
///   returns `None` and leaves the comparator to invent an answer, and an inconsistent comparator is undefined behaviour
///   in the sort itself (`slice::sort_by` requires a total order). Under `total_cmp` NaN sorts *above* every number, so
///   it is moved last on purpose: a garbage score must not win a ranking.
#[must_use]
pub fn rank_order(scores: &[f64]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..scores.len()).collect();
    order.sort_by(|a, b| {
        let (x, y) = (scores[*a], scores[*b]);
        match (x.is_nan(), y.is_nan()) {
            // Both uninformative: neither beats the other, so retrieval order decides.
            (true, true) => a.cmp(b),
            // Descending means "worse sorts later", and nothing is worse than a value that means nothing.
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => y.total_cmp(&x).then(a.cmp(b)),
        }
    });
    order
}

/// Why a pair could not be scored.
///
/// Two rules bind every variant, borrowed whole from [`crate::embed::EmbedError`] because they are the reason that
/// taxonomy is safe to log:
///
/// - **input-free**: no note text, ever. A vault is private and an error log outlives the run;
/// - **secret-free, runtime-text-free**: the `String` payloads are sentences this crate wrote, never a diagnostic from
///   llama.cpp, whose wording is neither stable nor ours to publish.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RerankError {
    /// This binary was built without `local-model`, so there is no runtime to score with. Distinct from
    /// [`RerankError::ArtifactAbsent`] because the fix differs: one is a different build of `np`, the other is a
    /// download. Mirrors [`crate::embed::EmbedError::RuntimeNotBuilt`].
    #[error("this build has no inference runtime, so it cannot rerank")]
    RuntimeNotBuilt,
    /// The artifact is not on disk. Nothing here fetches it: a search that silently starts a 600 MB download would be
    /// a search whose cost the user did not choose.
    #[error("the rerank model is not installed")]
    ArtifactAbsent,
    /// The file is there and is not the file we pinned. The payload is our own sentence about which check failed —
    /// sizes and digests, never a path with a host in it (the same posture as [`crate::pins`]'s refusals).
    #[error("rerank model failed verification: {0}")]
    ArtifactUnverified(String),
    /// The artifact loaded but is not something that can score a pair — an embedding model pointed at rank mode, say.
    /// The payload names the property that was missing, in our words.
    #[error("cannot rerank with this model: {0}")]
    CannotScore(String),
    /// One pair did not fit the window even after the document's tail was cut, because the query alone was too long.
    /// The numbers are the cheapest possible pair and the window it was measured against; no text accompanies them.
    #[error("a {tokens}-token query does not fit the {budget}-token scoring window")]
    PairTooLong {
        /// Tokens the query plus its wrapper needs, with an empty document.
        tokens: usize,
        /// The scoring window this run was built with.
        budget: usize,
    },
    /// The runtime refused or produced nothing usable. Every label is one we chose — see [`RerankError`]'s rules — and
    /// the list is short on purpose: `"the runtime refused to score this pair"`, `"the runtime produced no score for
    /// this pair"`, `"the runtime produced a score that is not a number"`.
    #[error("the inference runtime could not score a pair: {0}")]
    Runtime(String),
}

/// Something that scores documents against a query.
///
/// The seam exists so the search stage can be written once and run in every build: with a loaded cross-encoder behind
/// it, and with [`NoReranker`] behind it when reranking is off, not built, or not installed. `&mut self` because a
/// scorer owns a llama.cpp context, and one context has no documented thread-safety
/// (`include/llama.h:560-563` describes one context per thread).
pub trait Reranker {
    /// Score each document against `query`, returning one score per document in the order given.
    ///
    /// Higher means more relevant. The scale is whatever the head emits (a logit, usually: negatives included), so
    /// only the *order* of the returned vector is meaningful — see [`rank_order`].
    fn score(&mut self, query: &str, docs: &[&str]) -> Result<Vec<f64>, RerankError>;
}

/// A [`Reranker`] that refuses, keeping the reason it should refuse for.
///
/// The stage asks one question of its scorer — "score these" — and every unavailable door answers it with a variant
/// rather than with a branch at the call site. That is what keeps `np search --rerank` from having four code paths,
/// and what lets the CLI turn [`RerankError::ArtifactAbsent`] into "install it with `np models fetch`" while
/// [`RerankError::RuntimeNotBuilt`] becomes "this binary was built without a runtime".
#[derive(Debug, Clone)]
pub struct NoReranker(pub RerankError);

impl Reranker for NoReranker {
    fn score(&mut self, _query: &str, _docs: &[&str]) -> Result<Vec<f64>, RerankError> {
        Err(self.0.clone())
    }
}
