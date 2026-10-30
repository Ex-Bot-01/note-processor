//! Opening a real GGUF through our own FFI — behind `local-model`, and nothing else.
//!
//! **One claim, narrowly:** llama.cpp v0.8.0 (the tree `third_party/llama-cpp.toml` pins) can load
//! `embeddinggemma-2-Q8_0.gguf` on this host through the hand-written declarations in [`crate::llama_ffi`],
//! and the numbers it answers with are the numbers in that file's header. Nothing here tokenizes, decodes,
//! pools or produces a vector — that is R6b2, and until it lands
//! [`LocalModelProvider`](crate::embed::LocalModelProvider) still answers
//! [`EmbedError::RuntimeNotBuilt`]. This loader sits **beside**
//! that provider on purpose: `embed` is a contract about vectors, and bolting a half-built forward pass onto
//! it would either lie about the batch or turn the seam's one honest refusal into a partial one.
//!
//! What this module owns is therefore exactly the unsafe part: who holds the two C handles, in what order they
//! die, and which refusals happen before anything is allocated. [`load`] is the only place in the crate that
//! calls `llama_model_load_from_file`, and `LoadedModel`'s `Drop` the only place that frees either handle.
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

use crate::embed::EmbedError;
use crate::llama_ffi::{self, llama_context, llama_model};
use crate::models;
use crate::walk;

/// The context window [`load`] asks llama.cpp for: `min(REQUESTED_CTX, n_ctx_train)`.
///
/// Not the model's trained window (§5 leaves 262,144 vs 8,192 open), and not `embeddings.max_tokens` either —
/// this round never submits a token, so the only thing a window costs is memory, and 4,096 rows of KV cache is
/// what a prototype laptop can afford while R6b2 decides what the real ceiling has to be. Keeping it a
/// constant rather than a parameter is the same choice [`load`]'s signature makes: one number, written once,
/// until a measurement argues for moving it.
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
}

impl LoadedModel {
    /// The numbers the model and context answered. Copy them out freely — nothing here borrows C memory.
    pub fn facts(&self) -> ModelFacts {
        self.facts
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
        Ok((ctx, facts)) => Ok(LoadedModel { model, ctx, facts }),
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
fn open_context(
    model: NonNull<llama_model>,
    configured_dim: usize,
) -> Result<(NonNull<llama_context>, ModelFacts), EmbedError> {
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
    // better than the file" defect. `n_ctx` is the only sizing decision this round makes, capped by
    // `REQUESTED_CTX`.
    let mut ctx_params = unsafe { llama_ffi::llama_context_default_params() };
    ctx_params.embeddings = true;
    ctx_params.n_ctx = requested_ctx(numbers.n_ctx_train);

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
    #[test]
    fn a_path_that_cannot_be_named_to_c_is_refused() {
        let path = PathBuf::from("inner\0.gguf");
        let problem = load(&path, 768).expect_err("a NUL inside a path cannot name a file");
        match problem {
            EmbedError::ArtifactUnreadable { path: named, .. } => {
                assert_eq!(named, path, "the refusal names the path the caller gave");
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
