//! Opening a real GGUF through our own FFI — behind `local-model`, and nothing else.
//!
//! Two claims, each narrower than the milestone it belongs to.
//!
//! **R6b1:** llama.cpp v0.8.0 (the tree `third_party/llama-cpp.toml` pins) loads
//! `embeddinggemma-2-Q8_0.gguf` on this host through the hand-written declarations in [`crate::llama_ffi`],
//! and the numbers it answers with are the numbers in that file's header.
//!
//! **R6b2:** text goes in and one unit-length [`Vector`] comes out — [`LoadedModel::embed_text`] tokenizes,
//! runs a single forward pass, reads the pooled row and L2-normalises it, and [`LocalEmbedder`] is that path
//! behind [`Embedder`](crate::embed::Embedder). What is still **not** here: nothing constructs a
//! [`LocalEmbedder`] from a config, no caller embeds anything, and
//! [`LocalModelProvider`](crate::embed::LocalModelProvider) therefore still answers
//! [`EmbedError::RuntimeNotBuilt`] — R6c decides who holds a loaded model and when (§3's one load per
//! process), because 420 MB is not a thing to acquire as a side effect of resolving a provider.
//!
//! ## The window is three numbers, not one
//!
//! [`load`] sets `n_ctx = n_batch = n_ubatch = REQUESTED_CTX`. The last two are not decoration: this artifact
//! declares `attention.causal = 0`, and for non-causal attention v0.8.0 asserts
//! `n_ubatch >= n_tokens` (`src/llama-context.cpp:1920`) **and** `n_tokens <= n_batch` (`:1919`).
//! `GGML_ASSERT` maps to `GGML_ABORT` (`ggml/include/ggml.h:287-288`) and so survives `NDEBUG`: with
//! llama.cpp's default `n_ubatch = 512`, a 600-token sequence **aborted the process** (measured: exit 134, no
//! return code at all). Since `embeddings.max_tokens` accepts up to 8,192, that abort was reachable from an
//! ordinary ~1,800-byte note. One window, asked for once, is what makes the assert unreachable; see
//! [`REQUESTED_CTX`] for what it costs and [`LoadedModel::window`] for the refusal that guards it.
//!
//! What this module owns is therefore exactly the unsafe part: who holds the two C handles, in what order they
//! die, and which refusals happen before anything is allocated. [`load`] is the only place in the crate that
//! calls `llama_model_load_from_file`, and `LoadedModel`'s `Drop` the only place that frees the handles of a
//! **successful** load. A refusal frees too, in two other places — [`load`]’s numbered list names them: the model
//! on any error after it was loaded, and the context when pooling is refused. Reading `Drop` as the only free site
//! makes those two look like double frees; they are not (R6b1 review P2-2).
//!
//! ## Two facts about the artifact this was measured against
//!
//! `embeddinggemma-2-Q8_0.gguf` answers `n_embd_out = 768` and `n_embd_inp = 512`: the model runs at 512
//! inside and projects to 768, which is why [`load`] compares a declared width against **out**, and why the
//! deprecated `llama_n_embd` is not declared anywhere in this crate. It also answers `n_ctx_train = 262_144`
//! while `MAX_EMBEDDING_TOKENS` is 8,192 — see §5 of `docs/design-m2-local-embeddings.md`, which records that
//! conflict as an open question rather than a decision, and `ModelFacts::n_ctx_train` for what we do with the
//! difference.
//!
//! ## Portability
//!
//! This module converts paths with [`std::os::unix::ffi::OsStrExt`], i.e. it compiles on Unix only. That is
//! not an oversight to fix here: `build.rs` refuses to build `local-model` on any target but Linux (§14), so
//! there is no configuration in which a Windows or macOS build ever type-checks this file — and a lossy UTF-8
//! conversion would hand C a path that does not exist, which is a worse failure than the one it avoids.

use std::ffi::CString;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::NonNull;

use crate::db::ModelRow;
use crate::embed::{EmbedError, Embedder, Vector};
use crate::llama_ffi::{self, llama_context, llama_model};
use crate::models;
use crate::walk;

/// The context window [`load`] asks llama.cpp for, and the number it also gets for `n_batch` and `n_ubatch`:
/// `min(REQUESTED_CTX, n_ctx_train)`.
///
/// Not the model's trained window (§5 leaves 262,144 vs 8,192 open), and not `embeddings.max_tokens` either —
/// the budget decides what a caller may submit, this decides what the runtime can hold, and
/// [`LoadedModel::embed_text`] refuses between the two rather than letting llama.cpp decide with an assert.
///
/// **What it costs, measured** (peak RSS above a 419 MiB baseline, same process, wide window,
/// `GGML_OPENMP=OFF`): 133 tokens **+0 MiB**, 513 **+47**, 1,026 **+109**, 2,052 **+218**, 4,085 **+458**.
/// The compute buffer is reserved at context creation but its pages are touched per decode, so the price
/// tracks the **longest sequence actually run**, not the ceiling asked for — which is why 4,096 costs nothing
/// on ordinary chunks and why a smaller ceiling would not have bought much. Keeping it a constant rather than
/// a parameter is [`load`]'s choice, not the design's: legal input length comes from `embeddings.max_tokens`,
/// so when R6c wires config to a provider this becomes an argument, and until then a workspace configured
/// above 4,096 gets [`LoadedModel::window`]'s refusal instead of an abort.
const REQUESTED_CTX: usize = 4_096;

/// How a context turns per-token rows into one sentence vector.
///
/// The C enum has six values; this has four, and the two missing ones are not an omission to file.
/// `LLAMA_POOLING_TYPE_NONE` means the context fills per-token rows and never the sequence buffer, so it
/// cannot produce a sentence vector at all — [`load`] refuses it rather than modelling a state whose only
/// honest reading is "this artifact is useless to us". `LLAMA_POOLING_TYPE_UNSPECIFIED` cannot survive a
/// context constructor: `src/llama-context.cpp` resolves it to the model's own value, or to `NONE`. An
/// unrecognised number (a re-pin that adds a pooling type) is refused for the same reason as `NONE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// Mean over the sequence's real tokens — what `embeddinggemma-2` declares (`pooling_type = 1`), and the
    /// pooling §5 describes: mean, then L2-normalise.
    Mean,
    /// The first token's row.
    Cls,
    /// The last token's row.
    Last,
    /// The reranking head; produces a score, not an embedding (§5's stored width does not apply to it).
    Rank,
}

impl Pooling {
    /// Read one value out of the pinned enum mirror in [`crate::llama_ffi`].
    ///
    /// `None` means "refuse", and the caller reports the two reasons differently: `NONE` is a real answer about
    /// a real artifact that we cannot serve, while anything outside the declared range means the mirror no
    /// longer matches the header — a different sentence, because the cure is re-reading the pin.
    fn from_c(value: i32) -> Option<Self> {
        match value {
            llama_ffi::LLAMA_POOLING_TYPE_MEAN => Some(Self::Mean),
            llama_ffi::LLAMA_POOLING_TYPE_CLS => Some(Self::Cls),
            llama_ffi::LLAMA_POOLING_TYPE_LAST => Some(Self::Last),
            llama_ffi::LLAMA_POOLING_TYPE_RANK => Some(Self::Rank),
            _ => None,
        }
    }
}

/// What the loaded model and its context answered. Every field is a **count or a flag**, never a measurement
/// of this run's memory or time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFacts {
    /// Components in the vector this model answers with — the width one `embeddings` row must have, and the
    /// number [`load`] compares against a declared width. 768 for `embeddinggemma-2`; a stored row is `4 ×`
    /// this many **bytes**, since components are `f32`.
    pub n_embd_out: usize,
    /// Components in a token-embedding row entering the transformer (unit: components). 512 for
    /// `embeddinggemma-2`, where it is also the internal width. Never the candidate for a width check: input
    /// rows are this wide while the stored vectors are `n_embd_out` wide.
    pub n_embd_inp: usize,
    /// Tokens the model was trained with (unit: tokens) — a fact about the artifact, **not** our ceiling.
    /// `embeddings.max_tokens` inside `MAX_EMBEDDING_TOKENS` stays the bound a sequence is refused against (§5
    /// keeps the gap between the two as an open question), and it is not the window we ask llama.cpp for
    /// either, which is `REQUESTED_CTX`.
    pub n_ctx_train: usize,
    /// Entries in the vocabulary (unit: tokens). Valid token ids are `0..n_vocab`.
    pub n_vocab: usize,
    /// Whether a tokenisation request with `add_special = true` prepends BOS. A flag; when set it costs one
    /// token of the budget (`tokenizer.ggml.add_bos_token` is true for this artifact).
    pub add_bos: bool,
    /// Whether the same request appends EOS. Read separately from [`ModelFacts::add_bos`] because both are set
    /// here, and a token count that assumed one would be one short (§5's budget counts specials).
    pub add_eos: bool,
    /// How this context pools (unit: none — a strategy, not a number). Never `NONE`: see [`Pooling`].
    pub pooling: Pooling,
}

/// A loaded model plus one context over it, and the only owner of either.
///
/// **Neither `Clone` nor `Copy`, deliberately and structurally:** the two fields are C handles with exactly one
/// legitimate free each, so a second owner is a double free. Adding `Clone` would force a decision between
/// loading the artifact twice and reference-counting a handle llama.cpp does not count — which is why keeping
/// this type at two pointers plus [`ModelFacts`] is part of the design rather than an accident of it.
///
/// It is also neither `Send` nor `Sync`, because [`NonNull`] is neither and nothing here claims otherwise: no
/// comment in v0.8.0's header says a context may be driven from two threads, so the type does not assert that
/// it may. R6b2 shares one loaded model per process by **holding** it (§3's "one load per process, never per
/// note"), not by wrapping it in an `Arc`.
pub struct LoadedModel {
    /// From `llama_model_load_from_file`; freed second, because a context stores a raw pointer to its model
    /// rather than retaining it (`crate::llama_ffi`'s note on [`llama_ffi::llama_init_from_model`]).
    model: NonNull<llama_model>,
    /// From [`llama_ffi::llama_init_from_model`]; freed first, by the `Drop` implementation below.
    ctx: NonNull<llama_context>,
    facts: ModelFacts,
    /// Tokens this context holds in **one** decode — see [`LoadedModel::window`]. Equal to the `n_ctx`,
    /// `n_batch` and `n_ubatch` [`load`] asked for, kept beside the handles because a refusal has to quote the
    /// window that was actually built rather than the constant that was requested (a model trained on fewer
    /// tokens gets the smaller of the two).
    window: usize,
}

impl LoadedModel {
    /// The numbers the model and context answered. Copy them out freely — nothing here borrows C memory.
    pub fn facts(&self) -> ModelFacts {
        self.facts
    }

    /// The longest sequence this context can embed in one call, in tokens.
    ///
    /// A fact about **this run**, not about the artifact: it is `min(REQUESTED_CTX, n_ctx_train)`, and it is
    /// what [`LoadedModel::embed_text`] refuses against after the configured budget has passed. Reading it
    /// rather than re-deriving it is what lets a refusal name the real limit when the model's trained window
    /// was the smaller number.
    pub fn window(&self) -> usize {
        self.window
    }

    /// How many tokens the model would see for `text` with its special tokens, without running a forward pass.
    ///
    /// The count is llama.cpp's own, so it includes whatever the artifact's vocabulary decides to add — 2 for
    /// an empty string on this artifact (BOS + EOS, both of which [`ModelFacts::add_bos`] and
    /// [`ModelFacts::add_eos`] say are enabled). A caller that wants to check a chunk against
    /// `embeddings.max_tokens` before paying for a decode uses this; [`LoadedModel::embed_text`] checks the
    /// same number itself, so this is a convenience and never a second authority.
    pub fn count_tokens(&self, text: &str) -> Result<usize, EmbedError> {
        Ok(self.tokenize(text)?.len())
    }

    /// Tokenize `prefix + text`, run **one** forward pass, and return the pooled row as a unit-length vector.
    ///
    /// `budget` is `embeddings.max_tokens` for this workspace — the caller's number, not this module's, because
    /// §5 makes the budget a configured fact and this function has no opinion about what it should be.
    /// `prefix` is what the model is asked to read the text under ([`TextKind`]'s side of that decision lives
    /// in [`LocalEmbedder`]; pass `""` for none).
    ///
    /// Four refusals before any vector exists, then two that mean the runtime failed:
    ///
    /// 1. [`EmbedError::TokenBudgetExceeded`] when the **token** count (not the byte count) exceeds `budget`
    ///    — the first production call site for that variant, and the reason §5's counting rule exists;
    /// 2. [`EmbedError::Unavailable`] *"the sequence is longer than this context can hold"* when it passes the
    ///    budget but not [`LoadedModel::window`]. Worded as it is on purpose: "context too small" would send a
    ///    user to free memory, when the fix is a shorter chunk or a re-tuned `REQUESTED_CTX`;
    /// 3. [`EmbedError::Unavailable`] with a fixed label for anything the runtime answers wrongly — a refused
    ///    forward pass, a null pooled row, a vector of zeros, a component that is not a number.
    ///
    /// The pooled row is **copied** out immediately: `llama_get_embeddings_seq` hands back a buffer the context
    /// owns and rewrites on the next decode ([`crate::llama_ffi`]'s note on it), so nothing here keeps the
    /// pointer past the `Vector` built from it.
    ///
    /// One text per call, therefore one decode per text: a caller wanting N vectors pays N decodes today.
    /// Multi-text batching is R6d's optimisation to take with throughput evidence, not something to assume in
    /// the shape of this API.
    pub fn embed_text(
        &self,
        prefix: &str,
        text: &str,
        budget: usize,
    ) -> Result<Vector, EmbedError> {
        let joined = format!("{prefix}{text}");
        let mut tokens = self.tokenize(&joined)?;
        let n = tokens.len();

        // The budget over the count llama.cpp returned, not over `joined.len()`: a byte comparison would accept
        // a 1,800-byte note that tokenizes to 2,400 and refuse a multibyte one that fits. Measured on this
        // artifact: 60 bytes of Japanese is 11 tokens with specials, 23 bytes of English is 9.
        if n > budget {
            return Err(EmbedError::TokenBudgetExceeded { tokens: n, budget });
        }
        // Then the window. Between the two sits the case `embeddings.max_tokens` above 4,096 creates: legal by
        // config, impossible for this context, and worth a sentence of its own rather than an assert.
        if n > self.window {
            return Err(EmbedError::Unavailable(
                "the sequence is longer than this context can hold".to_string(),
            ));
        }

        // SAFETY: `llama_batch_get_one` borrows the array — it stores the pointer, copies nothing, and the
        // returned batch must never reach `llama_batch_free` (see [`crate::llama_ffi`]). `tokens` outlives the
        // decode below, which is the whole obligation. `n` is positive (a tokenization always yields at least
        // the special tokens) and at most `self.window`, which is what keeps v0.8.0's `n_tokens <= n_batch` and
        // non-causal `n_ubatch >= n_tokens` asserts out of reach.
        let batch = unsafe { llama_ffi::llama_batch_get_one(tokens.as_mut_ptr(), n as i32) };

        // SAFETY: `ctx` is non-null by the type; `batch` borrows live memory as above. The return value is an
        // `int32_t` error code, read below rather than assumed.
        let rc = unsafe { llama_ffi::llama_decode(self.ctx.as_ptr(), batch) };
        match rc {
            0 => {}
            // 1 is documented in `include/llama.h` as "the KV cache ran out" — with the window check above it
            // should be unreachable, and is still reported separately because if it ever fires it is reporting
            // that our own window arithmetic is wrong.
            1 => {
                return Err(EmbedError::Unavailable(
                    "the context ran out of KV space for this sequence".to_string(),
                ))
            }
            _ => {
                return Err(EmbedError::Unavailable(
                    "the runtime refused to run this sequence".to_string(),
                ))
            }
        }

        // SAFETY: `ctx` as above. A null return means the context filled no sequence buffer (pooling `NONE`,
        // which [`load`] refuses, or an internal failure); either way it is handled before any read.
        let pooled = unsafe { llama_ffi::llama_get_embeddings_seq(self.ctx.as_ptr(), 0) };
        if pooled.is_null() {
            return Err(EmbedError::Unavailable(
                "the runtime produced no pooled embedding".to_string(),
            ));
        }

        // SAFETY: a non-null `pooled` points at `n_embd_out` floats owned by the context — the width [`load`]
        // read from the model and checked against the declared one. Copied out here, on the far side of the
        // only decode this call performs, so nothing can rewrite it before the copy.
        let row = unsafe { std::slice::from_raw_parts(pooled, self.facts.n_embd_out) };
        normalise(row)
    }

    /// Tokenize with `add_special = true`, sizing the buffer from llama.cpp's own answer rather than a guess.
    ///
    /// The rule is [`src/llama-vocab.cpp:4645-4647`]: when `n_tokens_max` is too small the call writes nothing
    /// and returns **minus the exact count needed, specials included** (measured: a 2-slot buffer for a
    /// 17-token sentence returned `-17`). So one undersized guess costs one retry, and no assumption about
    /// bytes-per-token is load-bearing — the first allocation is only an optimisation.
    fn tokenize(&self, joined: &str) -> Result<Vec<llama_ffi::llama_token>, EmbedError> {
        let bytes = joined.as_bytes();
        // `text_len` is an `int32_t`; a larger input cannot be named to C at all. Unreachable through the
        // budget (8,192 tokens cannot come from 2 GiB of text), but `as i32` would otherwise wrap silently.
        if bytes.len() > i32::MAX as usize {
            return Err(EmbedError::Unavailable(
                "the sequence is too long to hand to the tokenizer".to_string(),
            ));
        }
        let vocab = self.vocab()?;

        // One token per byte plus the specials is an upper bound for any byte-level tokenizer (every piece
        // consumes at least one byte), so this is a first guess that is nearly always right — measured, 23
        // bytes of English needs 9 slots and 60 bytes of Japanese needs 11.
        let mut tokens = vec![0 as llama_ffi::llama_token; bytes.len().saturating_add(2).max(4)];
        let written = self.tokenize_into(vocab, bytes, &mut tokens)?;
        if written >= 0 {
            tokens.truncate(written as usize);
            return Ok(tokens);
        }

        // The buffer was short and llama.cpp named the exact size it needed. Allocate that and try once: a
        // second shortfall would mean its own count was wrong, which is a runtime refusal rather than a size
        // to keep chasing.
        let needed = written.unsigned_abs() as usize;
        let mut sized = vec![0 as llama_ffi::llama_token; needed];
        let again = self.tokenize_into(vocab, bytes, &mut sized)?;
        if again < 0 {
            return Err(EmbedError::Unavailable(
                "the tokenizer's count did not fit its own buffer".to_string(),
            ));
        }
        sized.truncate(again as usize);
        Ok(sized)
    }

    /// One `llama_tokenize` call: `Ok(n)` with `n >= 0` is the number of tokens written into `tokens`, and
    /// `Ok(n)` with `n < 0` is llama.cpp's own "you needed `-n` slots" answer (widened to `i64` so the caller
    /// can negate it without the `i32::MIN` overflow). Writes nothing in the negative case.
    fn tokenize_into(
        &self,
        vocab: *const llama_ffi::llama_vocab,
        bytes: &[u8],
        tokens: &mut [llama_ffi::llama_token],
    ) -> Result<i64, EmbedError> {
        // SAFETY: `vocab` is non-null (checked by [`Self::vocab`]); `bytes` is a Rust slice whose length fits
        // `i32` (checked by the caller) and is read, not freed; `tokens` has exactly `tokens.len()` writable
        // slots, which is what `n_tokens_max` promises. `add_special = true` because §5's budget counts what the
        // model sees, and `parse_special = false` because a note's text is data — treating it as markup would
        // let a file that contains a control token spell vocabulary entries the tokenizer then obeys.
        let n = unsafe {
            llama_ffi::llama_tokenize(
                vocab,
                bytes.as_ptr() as *const std::ffi::c_char,
                bytes.len() as i32,
                tokens.as_mut_ptr(),
                tokens.len() as i32,
                true,
                false,
            )
        };
        if n >= 0 {
            return Ok(i64::from(n));
        }
        if n == i32::MIN {
            // `llama_vocab::tokenize` answers this for results at or past `INT32_MAX` tokens
            // (`src/llama-vocab.cpp:4640-4642`) — negating it would overflow, and no real note is that long.
            return Err(EmbedError::Unavailable(
                "the tokenizer refused to count this sequence".to_string(),
            ));
        }
        Ok(-i64::from(n))
    }

    /// The vocabulary handle the tokenizer calls need.
    fn vocab(&self) -> Result<*const llama_ffi::llama_vocab, EmbedError> {
        // SAFETY: `model` is non-null by the type. `llama_model_get_vocab` returns `&model->vocab` — a plain
        // member address that cannot be null for a live model (`src/llama-model.cpp:3406-3408`), which is why
        // [`crate::llama_ffi`] documents it as never returning null rather than as a checked call. The check
        // stays because a re-pin could change it and a null here must be a refusal, not a dereference.
        let vocab = unsafe { llama_ffi::llama_model_get_vocab(self.model.as_ptr()) };
        if vocab.is_null() {
            return Err(EmbedError::Unavailable(
                "the loaded model exposed no vocabulary".to_string(),
            ));
        }
        Ok(vocab)
    }
}

/// Scale a pooled row to unit L2 length, refusing anything that cannot be scaled.
///
/// §5's recipe is mean-then-L2 and llama.cpp has already done the mean (`pooling_type = MEAN`, refused
/// otherwise by [`load`]), so this is the whole of our arithmetic. Measured on this artifact the raw pooled row
/// has ‖v‖ ≈ 184–220 depending on sequence length, so the scaling is load-bearing rather than cosmetic: stored
/// as-is, every cosine in §5's search would be a dot product weighted by how long the note was.
///
/// A row that is all zeros, or contains a component that is not a number, is a **refusal**: a zero vector
/// survives every width check, stores fine, and then scores 0.0 against everything — which is how a silent
/// runtime failure becomes a search that quietly returns nothing. NaN survives `f32` round-tripping for the
/// same reason and would poison a cosine the same way.
fn normalise(row: &[f32]) -> Result<Vector, EmbedError> {
    if !row.iter().all(|x| x.is_finite()) {
        return Err(EmbedError::Unavailable(
            "the model produced a component that is not a number".to_string(),
        ));
    }
    // Summed in f64: 768 squares of values around 10 add up to ~3.4e4, which f32 handles, but a row that
    // legitimately reached 1e20 would overflow f32 into +inf and then read as "not a number" instead of as the
    // enormous-but-finite vector it is.
    let sum: f64 = row.iter().map(|x| f64::from(*x) * f64::from(*x)).sum();
    if sum == 0.0 || !sum.is_finite() {
        return Err(EmbedError::Unavailable(
            "the model produced a zero vector".to_string(),
        ));
    }
    let scale = (1.0 / sum.sqrt()) as f32;
    Ok(Vector::new(row.iter().map(|x| x * scale).collect()))
}

/// Which side of a search a piece of text is on.
///
/// Not a cosmetic distinction: §5 and the `models` table store `prefix_doc` and `prefix_query` separately
/// because a query and a document reach the model under different instructions, and a vector written under one
/// prefix is not comparable with a vector written under the other. Both kinds are implemented now — `np search`
/// needs the query side in R6d — while **only [`TextKind::Document`] has a caller today**, which is what
/// [`LocalEmbedder`]'s doc says rather than what this one pretends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    /// A note's chunk, embedded under `models.prefix_doc` for storage.
    Document,
    /// A search string, embedded under `models.prefix_query` for comparison.
    Query,
}

/// An [`Embedder`] backed by one loaded model, the two prefixes of a `models` row, and a token budget.
///
/// This is what `[embeddings] provider = "local-model"` will resolve to, and it is deliberately **not**
/// [`LocalModelProvider`](crate::embed::LocalModelProvider): that type is the seam's default half, reachable
/// without the feature and answering [`EmbedError::RuntimeNotBuilt`], and R6c decides how a config becomes this.
/// Constructing it is also the last place the three widths can disagree cheaply — see [`LocalEmbedder::new`].
///
/// Owns one [`LoadedModel`], so it owns ~420 MB of resident weights (§14) and is neither `Send` nor `Sync`
/// because the handles inside it are neither. One per process, held for the run's lifetime: §3's "one load per
/// process, never per note".
#[derive(Debug)]
pub struct LocalEmbedder {
    model: LoadedModel,
    /// `models.id`, verbatim — [`Embedder::model_id`] is what a write stamps into `embeddings.model`.
    model_id: String,
    prefix_doc: String,
    prefix_query: String,
    /// `embeddings.max_tokens`, checked by [`LoadedModel::embed_text`] over the token count.
    max_tokens: usize,
}

impl LocalEmbedder {
    /// Wrap a loaded model with the row's prefixes and the workspace's token budget.
    ///
    /// Refuses [`EmbedError::ProviderDimMismatch`] when `row.dim` disagrees with what the artifact answers:
    /// [`load`] compared the caller's *configured* width, and this is the check that the row being embedded
    /// under is **that** row. Without it, a provider built from one model's row around another model's handles
    /// would write 768-wide vectors stamped with a different id — the exact §6 defect the schema's `CHECK`
    /// catches only after something has been computed.
    pub fn new(model: LoadedModel, row: &ModelRow, max_tokens: usize) -> Result<Self, EmbedError> {
        let declared = usize::try_from(row.dim);
        match declared {
            Ok(dim) if dim == model.facts().n_embd_out => Ok(Self {
                model,
                model_id: row.id.clone(),
                prefix_doc: row.prefix_doc.clone(),
                prefix_query: row.prefix_query.clone(),
                max_tokens,
            }),
            // A negative `dim` cannot be a width, and neither can one that simply differs; both are reported
            // as the mismatch they are, naming the number the row holds.
            _ => Err(EmbedError::ProviderDimMismatch {
                configured: model.facts().n_embd_out,
                provider: row.dim,
            }),
        }
    }

    /// The loaded model behind this embedder — for facts, and so a caller can read [`LoadedModel::window`].
    pub fn model(&self) -> &LoadedModel {
        &self.model
    }

    /// Embed one text under the prefix [`kind`] selects.
    pub fn embed_as(&self, text: &str, kind: TextKind) -> Result<Vector, EmbedError> {
        let prefix = match kind {
            TextKind::Document => &self.prefix_doc,
            TextKind::Query => &self.prefix_query,
        };
        self.model.embed_text(prefix, text, self.max_tokens)
    }
}

impl Embedder for LocalEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        self.model.facts().n_embd_out
    }

    /// The document path: every text embedded under `prefix_doc`, one decode each, in the order given.
    ///
    /// The [`Embedder`] contract is what a scan calls and what lands in `embeddings`, so it can only ever be
    /// [`TextKind::Document`]. A query goes through [`LocalEmbedder::embed_as`] with [`TextKind::Query`]
    /// explicitly — there is no hidden mode switch in the trait method.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vector>, EmbedError> {
        texts
            .iter()
            .map(|text| self.embed_as(text, TextKind::Document))
            .collect()
    }
}

impl fmt::Debug for LoadedModel {
    /// Prints the facts and the two addresses. Addresses rather than nothing, because "is this the same loaded
    /// model?" is the first question a double-load bug asks — and printing weights would be meaningless, since
    /// we never read them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedModel")
            .field("facts", &self.facts)
            .field("model", &self.model)
            .field("ctx", &self.ctx)
            .finish()
    }
}

impl Drop for LoadedModel {
    fn drop(&mut self) {
        // SAFETY: both pointers came from llama.cpp's constructors in [`load`] and were non-null there. This
        // type is not `Clone`/`Copy` and owns both, so `drop` runs once per pair — and the order is not
        // interchangeable: a context holds a raw pointer to its model without retaining it, so freeing the
        // model first would leave `~llama_context` reading freed memory.
        unsafe {
            llama_ffi::llama_free(self.ctx.as_ptr());
            llama_ffi::llama_model_free(self.model.as_ptr());
        }
    }
}

/// Load `path` and open one context over it, refusing before allocating anything that cannot be described.
///
/// `configured_dim` is the width this run needs vectors to be — `[embeddings] dim`, or the width already stored
/// under this model id — and it is compared against what the **artifact answers** (`llama_model_n_embd_out`),
/// which is the pair §6 requires to agree. This is the check a declared row cannot make for itself:
/// `models.dim` is a number someone typed, and until now nothing compared it with the file.
///
/// Four refusals, cheapest first, none of them a panic:
///
/// 1. [`EmbedError::ArtifactUnreadable`] when the path cannot be named to C (an interior NUL) or cannot be
///    statted — including a directory, which is the same rule and the same variant as
///    [`crate::embed::LocalModelProvider::new`], so "that is not a file" keeps one name;
/// 2. [`EmbedError::ArtifactNotLoadable`] when llama.cpp returns null for a readable file: the bytes were
///    reached and meant nothing (bad magic, unknown architecture, a truncated tensor). Its doc comment has
///    waited for this call site;
/// 3. [`EmbedError::ProviderDimMismatch`] when `n_embd_out != configured_dim`, naming both numbers — before any
///    context exists, because the context is the expensive part;
/// 4. [`EmbedError::Unavailable`] with a fixed label when the runtime answers nonsense instead: no vocabulary,
///    a non-positive width or window, a null context, or pooling that cannot yield a sentence vector. This is
///    §3's "runtime error with a bounded message" class, and the labels are this file's own strings — never
///    llama.cpp's diagnostic text, which is exactly what that variant's payload rule keeps out of logs.
///
/// No new [`EmbedError`] variant: R6a's nine already cover every thing that can go wrong here, including the
/// runtime-refusal class it reserved for this round.
///
/// Every error path frees what it allocated. The model is freed in exactly one place — the `Err` arm below —
/// and the context in exactly two: this function's pooling refusal, and `LoadedModel`'s `Drop`.
pub fn load(path: &Path, configured_dim: usize) -> Result<LoadedModel, EmbedError> {
    // The same spelling `models` and `LocalModelProvider::new` use for this file: absolute, no symlink
    // resolution, prefixed when long — so the path C receives and the path an error names differ only by a
    // prefix a platform needs.
    let open_path = walk::to_open_path(&models::access(path));

    // A NUL inside a path cannot reach C at all. Checked before the stat because it costs no syscall, which is
    // the ordering argument `LocalModelProvider::new` uses for its own cheapest refusal.
    let c_path = CString::new(open_path.as_os_str().as_bytes()).map_err(|_| {
        EmbedError::ArtifactUnreadable {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"),
        }
    })?;

    let meta = fs::metadata(&open_path).map_err(|source| EmbedError::ArtifactUnreadable {
        path: path.to_path_buf(),
        source,
    })?;
    if !meta.is_file() {
        return Err(EmbedError::ArtifactUnreadable {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
        });
    }

    // SAFETY: `llama_model_default_params` returns a value-type struct of plain data and allocates nothing.
    // Taking llama.cpp's own defaults rather than constructing one is what keeps a field added in a future
    // revision from arriving zeroed instead of defaulted.
    let model_params = unsafe { llama_ffi::llama_model_default_params() };

    // SAFETY: `c_path` is a valid NUL-terminated string that outlives this call, and `model_params` was built
    // by llama.cpp and is copied by value. A null return is llama.cpp's documented failure, handled below; it
    // leaves nothing of ours allocated.
    let model = unsafe { llama_ffi::llama_model_load_from_file(c_path.as_ptr(), model_params) };
    let Some(model) = NonNull::new(model) else {
        return Err(EmbedError::ArtifactNotLoadable {
            path: path.to_path_buf(),
        });
    };

    // From here the model is ours, and every failure below must hand it back. One arm does that, so a new
    // refusal inside `open_context` cannot leak it — which is why the fallible work lives in a function whose
    // only success value is "the context is now owned too".
    match open_context(model, configured_dim) {
        Ok((ctx, facts, window)) => Ok(LoadedModel {
            model,
            ctx,
            facts,
            window,
        }),
        Err(problem) => {
            // SAFETY: `model` came from `llama_model_load_from_file` and no context outlived `open_context`'s
            // failure paths (each frees the one it made), so this is the first and only free of this handle.
            unsafe { llama_ffi::llama_model_free(model.as_ptr()) };
            Err(problem)
        }
    }
}

/// Read the model's facts, check the declared width, then build the context that answers the rest.
///
/// On `Err` nothing is left allocated except `model`, which the caller frees; on `Ok` the returned context is
/// non-null and becomes a `LoadedModel` with no further fallible step.
/// Returns the context, its facts, and the window it was built with — three values because the window is a
/// fact about the constructor call, not about the model, and [`LoadedModel::embed_text`] has to refuse against
/// the number actually built.
fn open_context(
    model: NonNull<llama_model>,
    configured_dim: usize,
) -> Result<(NonNull<llama_context>, ModelFacts, usize), EmbedError> {
    let numbers = read_numbers(model)?;

    // The width check compares our `usize` against the runtime's own `i32`, so a refusal prints the number
    // llama.cpp gave rather than a converted approximation of it (`ProviderDimMismatch::provider` is `i64`
    // because the database column is, and `i64::from(i32)` is exact).
    if numbers.n_embd_out != configured_dim {
        return Err(EmbedError::ProviderDimMismatch {
            configured: configured_dim,
            provider: i64::from(numbers.raw_out),
        });
    }

    // SAFETY: value-type struct of plain data, as above. `embeddings` is the switch that makes a forward pass
    // produce embeddings at all; `pooling_type` stays `LLAMA_POOLING_TYPE_UNSPECIFIED` on purpose — §5 takes
    // the pooling strategy from the artifact, and overriding it here would be the design's exact "we know
    // better than the file" defect.
    //
    // One window, written three times. `n_ctx` is the ceiling this round chose; `n_batch`/`n_ubatch` are llama.cpp's
    // defaults of 2,048 and 512 unless set, and for this **non-causal** artifact 512 is an abort threshold
    // rather than a tuning knob (module header: "The window is three numbers"). Setting them equal to `n_ctx`
    // is what makes `llama_decode` answer with a return code instead of `GGML_ABORT`, and it is why
    // [`LoadedModel::embed_text`] refuses above the same number instead of discovering the limit downstream.
    let mut ctx_params = unsafe { llama_ffi::llama_context_default_params() };
    ctx_params.embeddings = true;
    ctx_params.n_ctx = requested_ctx(numbers.n_ctx_train);
    ctx_params.n_batch = ctx_params.n_ctx;
    ctx_params.n_ubatch = ctx_params.n_ctx;
    let window = ctx_params.n_ctx as usize;

    // SAFETY: `model` is non-null and outlives this call; `ctx_params` is copied by value.
    let ctx = unsafe { llama_ffi::llama_init_from_model(model.as_ptr(), ctx_params) };
    let Some(ctx) = NonNull::new(ctx) else {
        return Err(EmbedError::Unavailable(
            "the runtime refused to build a context for this model".to_string(),
        ));
    };

    // SAFETY: `ctx` is non-null (checked above) and owned by us; the call reads one field of its parameters.
    let pooling = unsafe { llama_ffi::llama_pooling_type(ctx.as_ptr()) };
    match Pooling::from_c(pooling) {
        Some(kind) => Ok((
            ctx,
            ModelFacts {
                n_embd_out: numbers.n_embd_out,
                n_embd_inp: numbers.n_embd_inp,
                n_ctx_train: numbers.n_ctx_train,
                n_vocab: numbers.n_vocab,
                add_bos: numbers.add_bos,
                add_eos: numbers.add_eos,
                pooling: kind,
            },
            window,
        )),
        None => {
            // Refusing is also the only place a context dies outside `LoadedModel`'s `Drop`, so it has to die
            // here: leaking it would keep hundreds of MB reachable behind an error nobody can retry past.
            // SAFETY: this context came from `llama_init_from_model` above and is not owned by any
            // `LoadedModel` yet; nothing reads through it after this call.
            unsafe { llama_ffi::llama_free(ctx.as_ptr()) };
            Err(EmbedError::Unavailable(
                if pooling == llama_ffi::LLAMA_POOLING_TYPE_NONE {
                    "this artifact cannot produce a sentence vector: the context pools with NONE"
                        .to_string()
                } else {
                    // Not `NONE` and not any value the pinned header declares: the mirror no longer matches the
                    // tree it mirrors, which is a re-pin problem rather than an artifact problem.
                    "the runtime reported a pooling strategy this build does not know".to_string()
                },
            ))
        }
    }
}

/// The widths, counts and flags that are facts about the **model** — everything in [`ModelFacts`] except what
/// only a context can answer (`pooling`).
struct Numbers {
    n_embd_out: usize,
    n_embd_inp: usize,
    n_ctx_train: usize,
    n_vocab: usize,
    add_bos: bool,
    add_eos: bool,
    /// [`llama_ffi::llama_model_n_embd_out`]'s raw answer, kept so a width refusal can name the runtime's own
    /// number instead of our conversion of it.
    raw_out: i32,
}

/// Read every model-level fact through the FFI.
///
/// Split out because all six reads share one refusal rule (a non-positive count is not a count) and none of
/// them can fail in a way that leaves memory behind.
fn read_numbers(model: NonNull<llama_model>) -> Result<Numbers, EmbedError> {
    // SAFETY: `model` is non-null by the type, and every function below reads one integer out of
    // already-loaded metadata without allocating.
    let (raw_out, raw_inp, raw_ctx, vocab) = unsafe {
        (
            llama_ffi::llama_model_n_embd_out(model.as_ptr()),
            llama_ffi::llama_model_n_embd_inp(model.as_ptr()),
            llama_ffi::llama_model_n_ctx_train(model.as_ptr()),
            llama_ffi::llama_model_get_vocab(model.as_ptr()),
        )
    };
    let n_embd_out = positive(raw_out, "the model reported no output width")?;
    let n_embd_inp = positive(raw_inp, "the model reported no input width")?;
    // Zero here would be worse than useless: `llama_context_params.n_ctx == 0` means "take it from the model"
    // (`include/llama.h`), so a min() that produced 0 would silently hand us the artifact's full trained window
    // instead of the ceiling we asked for.
    let n_ctx_train = positive(raw_ctx, "the model reported no training context")?;

    if vocab.is_null() {
        return Err(EmbedError::Unavailable(
            "the loaded model exposed no vocabulary".to_string(),
        ));
    }
    // SAFETY: `vocab` is non-null (checked just above) and owned by the model, which outlives this read.
    let n_vocab = unsafe { llama_ffi::llama_vocab_n_tokens(vocab) };
    let n_vocab = positive(n_vocab, "the model reported an empty vocabulary")?;

    // SAFETY: `vocab` as above; both calls read one bool out of the vocabulary object.
    let (add_bos, add_eos) = unsafe {
        (
            llama_ffi::llama_vocab_get_add_bos(vocab),
            llama_ffi::llama_vocab_get_add_eos(vocab),
        )
    };

    Ok(Numbers {
        n_embd_out,
        n_embd_inp,
        n_ctx_train,
        n_vocab,
        add_bos,
        add_eos,
        raw_out,
    })
}

/// The `n_ctx` to hand the context constructor: never more than `REQUESTED_CTX`, never more than the model was
/// trained with, and never 0 — which llama.cpp reads as "no ceiling" (see [`read_numbers`]).
fn requested_ctx(n_ctx_train: usize) -> u32 {
    // `n_ctx_train` is positive by the time this is called, so the minimum is too, and it cannot exceed
    // `u32::MAX` because `REQUESTED_CTX` does.
    REQUESTED_CTX.min(n_ctx_train) as u32
}

/// Turn one `int32_t` answer into a count, refusing anything that is not one.
///
/// `as usize` would turn `-1` into 18446744073709551615: the width check would still refuse, but it would
/// report an arithmetic artefact instead of what happened, and a zero would pass as a legitimate width.
/// `label` is always a literal from this file — [`EmbedError::Unavailable`] carries our own bounded words,
/// never runtime text (§3's payload rule).
fn positive(value: i32, label: &'static str) -> Result<usize, EmbedError> {
    match usize::try_from(value) {
        Ok(count) if count > 0 => Ok(count),
        _ => Err(EmbedError::Unavailable(label.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Every value the pinned `enum llama_pooling_type` declares, mapped or refused — the table this module's
    /// whole pooling posture rests on. None of it can be reached from an artifact: the only file we have
    /// answers MEAN, so `NONE`, `UNSPECIFIED` and an out-of-range number would otherwise never be exercised.
    #[test]
    fn pooling_maps_the_pinned_enum_and_refuses_the_rest() {
        assert_eq!(
            Pooling::from_c(llama_ffi::LLAMA_POOLING_TYPE_MEAN),
            Some(Pooling::Mean)
        );
        assert_eq!(
            Pooling::from_c(llama_ffi::LLAMA_POOLING_TYPE_CLS),
            Some(Pooling::Cls)
        );
        assert_eq!(
            Pooling::from_c(llama_ffi::LLAMA_POOLING_TYPE_LAST),
            Some(Pooling::Last)
        );
        assert_eq!(
            Pooling::from_c(llama_ffi::LLAMA_POOLING_TYPE_RANK),
            Some(Pooling::Rank)
        );
        // The two that must never become a `Pooling`: `NONE` cannot produce a sentence vector, and
        // `UNSPECIFIED` means the mirror was read before the context resolved it.
        assert_eq!(Pooling::from_c(llama_ffi::LLAMA_POOLING_TYPE_NONE), None);
        assert_eq!(
            Pooling::from_c(llama_ffi::LLAMA_POOLING_TYPE_UNSPECIFIED),
            None
        );
        // A value outside the header's range: a re-pin that adds a strategy must fail here, not quietly become
        // `Mean`.
        assert_eq!(Pooling::from_c(99), None);
    }

    /// The window ceiling: 4,096 for a model with a bigger trained window, the model's own number when it is
    /// smaller, and never 0 — because llama.cpp reads 0 as "no ceiling".
    #[test]
    fn context_size_is_capped_and_never_zero() {
        assert_eq!(requested_ctx(262_144), 4_096);
        assert_eq!(requested_ctx(REQUESTED_CTX), 4_096);
        assert_eq!(requested_ctx(2_048), 2_048);
        assert_eq!(requested_ctx(1), 1);
    }

    /// A width or count that is not positive is refused as a runtime refusal, and the label survives: no `as
    /// usize` wrapping `-1` into an absurd number, and no zero sneaking through as a legal width.
    #[test]
    fn non_positive_counts_are_refused() {
        for value in [-1, i32::MIN, 0] {
            let problem = positive(value, "the model reported no output width")
                .expect_err("no non-positive value is a count");
            match problem {
                EmbedError::Unavailable(label) => {
                    assert_eq!(label, "the model reported no output width")
                }
                other => panic!("expected the runtime-refusal variant, got {other:?}"),
            }
        }
        assert_eq!(positive(768, "width").expect("768 is a count"), 768);
    }

    /// A path with an interior NUL cannot be named to C. Refused as unreadable, before any syscall and long
    /// before any panic — this is the input most likely to arrive from a hand-edited config or a corrupt row.
    ///
    /// The assertion that matters is *which* refusal: `fs::metadata` on such a path also yields
    /// `ArtifactUnreadable`, so deleting the `CString` guard would keep a variant-only test green. Pinning our own
    /// message text (`"path contains a NUL byte"`, `InvalidInput`) is what distinguishes our ordering claim from a
    /// std error we never intended to produce (R6b1 review P2-5).
    #[test]
    fn a_path_that_cannot_be_named_to_c_is_refused() {
        let path = PathBuf::from("inner\0.gguf");
        let problem = load(&path, 768).expect_err("a NUL inside a path cannot name a file");
        match problem {
            EmbedError::ArtifactUnreadable {
                path: named,
                source,
            } => {
                assert_eq!(named, path, "the refusal names the path the caller gave");
                assert_eq!(
                    source.kind(),
                    io::ErrorKind::InvalidInput,
                    "our own guard fires, not a syscall that happened to fail: {source}"
                );
                assert_eq!(
                    source.to_string(),
                    "path contains a NUL byte",
                    "the message is ours — std's nul-byte error would read \"data provided contains a nul byte\""
                );
            }
            other => panic!("expected an unreadable-artifact refusal, got {other:?}"),
        }
    }

    /// `load`'s promise on a path that does not exist is a typed refusal, asserted as a variant rather than as
    /// message text. The directory case lives in `tests/local_load.rs`, next to the artifact-gated cases.
    #[test]
    fn an_absent_file_never_reaches_the_runtime() {
        let path = PathBuf::from("/definitely/not/here/np-r6b1.gguf");
        let problem = load(&path, 768).expect_err("nothing is installed at that path");
        assert!(
            matches!(problem, EmbedError::ArtifactUnreadable { .. }),
            "expected an unreadable-artifact refusal, got {problem:?}"
        );
    }
}
