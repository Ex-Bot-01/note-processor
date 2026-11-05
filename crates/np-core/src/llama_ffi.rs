//! Hand-written `extern "C"` declarations for llama.cpp **v0.8.0**, behind `local-model`.
//!
//! No bindgen (§14): the public API is C, every handle is opaque to us, and the surface R6b1 needs is
//! a dozen functions. What bindgen would give us — the whole header, including the deprecated and
//! experimental halves of it — is smaller than what it costs: libclang on three OSes plus the
//! `bindgen`/`cc`/`cmake` build-dependency chain we are deliberately not taking.
//!
//! **This file mirrors one specific revision**: `third_party/llama-cpp.toml` pins v0.8.0, and
//! `crates/np-core/build.rs` refuses to build a tree whose declared version differs. The declarations
//! below were read out of that tree's `include/llama.h`, not from documentation or memory:
//!
//! * `llama_version` returns `const char *` here (a string like `0.8.0-dev`), **not** the integer
//!   build number older llama.cpp returned.
//! * There is no `llama_model_n_vocab`. Token count moved to the vocabulary object:
//!   [`llama_model_get_vocab`] + [`llama_vocab_n_tokens`].
//! * The deprecated spellings — `llama_load_model_from_file`, `llama_free_model`, `llama_n_ctx_train`,
//!   `llama_n_vocab`, `llama_new_context_with_model`, and the three R6b1 was tempted by (`llama_n_embd`,
//!   `llama_add_bos_token`, `llama_add_eos_token`) — are deliberately absent. Declaring them would make it
//!   possible to reach the wrong one without a compile error, since they still exist in v0.8.0. The last
//!   three matter more than a rename usually does: `llama_n_embd` answers with the model's **internal**
//!   width, so a loader that used it would read 512 for a model whose vectors are 768 (§5's width decision
//!   would be enforced against the wrong number and pass).
//! * `llama_backend_init` is **not** declared here and `load` never calls it, which deviates from the header's
//!   "call once at the start of the program" (`include/llama.h:489`). It is safe on this build for two reasons read out
//!   of the pinned tree rather than assumed: the f16 tables that call initializes come from any `ggml_init`
//!   (`src/llama.cpp:122-131`), and the load path performs several of those (`src/llama-graph.cpp:1436`,
//!   `src/llama-context.cpp:3107`); and backend registration needs no `ggml_backend_load_all()` when the CPU backend is
//!   compiled in, because the registry's own constructor registers it (`ggml/src/ggml-backend-reg.cpp:171-172` under
//!   `GGML_USE_CPU`). Measured corroboration: every artifact-gated load and decode here returns rc 0 with a finite
//!   pooled row. A future build that offloads to another device must revisit this (A1 review P2-4).
//! * The `llama_batch_ext_*` family is also absent, on purpose: choosing it over
//!   [`llama_batch_get_one`] plus the `logits` array of [`llama_batch`] is R6b1's design question (§14),
//!   and declaring both would decide it by accident.
//!
//! # Safety
//!
//! Everything here is `unsafe` in the C sense: raw pointers with an owner that lives in another
//! language. Each declaration says who owns what, because that is the only thing standing between
//! R6b1 and a use-after-free — and the [`llama_batch`] pair is the sharpest edge: it is passed **by
//! value**, so Rust cannot enforce "freed once", and [`llama_batch_get_one`] hands back a batch whose
//! arrays belong to *us*, which [`llama_batch_free`] would happily `free()`.

// The C spellings are kept deliberately (`llama_model`, not `LlamaModel`): this module mirrors one
// pinned header, and a reviewer checking it against `include/llama.h` greps for the C name. Renaming
// would leave every line here *almost* matching something, which is worse than a lint exception.
#![allow(non_camel_case_types)]

/// Opaque handles. These exist only as pointee types: llama.cpp never exposes their layout, so there
/// is nothing to construct, read through, or size — which is what an empty enum gives us (and why none
/// carries `repr(C)`: a zero-variant enum has no layout to declare).
pub enum llama_model {}
/// Context: one decode session over a model. Owned by [`llama_init_from_model`]'s caller.
pub enum llama_context {}
/// Vocabulary: owned by the model that produced it, never freed directly.
pub enum llama_vocab {}

/// `enum llama_pooling_type` (`include/llama.h:177-184`), mirrored as constants rather than a Rust enum.
///
/// It reaches us in two places only: as `llama_context_params.pooling_type`, which this file types `i32`
/// (the struct's own layout pins already vouch for its 4-byte width at offset 44), and as the return value
/// of [`llama_pooling_type`]. Constants rather than a `#[repr(C)]` enum because the field is mirrored as
/// `i32` today, and because C's `sizeof(enum)` is not something Rust can pin anyway — see `layout`'s "What
/// these pins cannot see".
///
/// **All six values are declared, including [`LLAMA_POOLING_TYPE_UNSPECIFIED`]**, because "-1 means ask the
/// model" is a fact R6b1/R6b2 must not re-guess: leaving it out would invite a zero-initialised field, and 0
/// is `NONE`, which silently disables pooling rather than asking. The mapping to a Rust type with only the
/// vector-producing cases lives in [`crate::local_model::Pooling`].
pub const LLAMA_POOLING_TYPE_UNSPECIFIED: i32 = -1;
/// No pooling: the context answers per token and never with one sentence vector.
pub const LLAMA_POOLING_TYPE_NONE: i32 = 0;
/// Mean over the real tokens of a sequence — what `gemma-embedding2` declares (`pooling_type = 1`).
pub const LLAMA_POOLING_TYPE_MEAN: i32 = 1;
/// Use the `[CLS]`-style first-token row.
pub const LLAMA_POOLING_TYPE_CLS: i32 = 2;
/// Use the last token's row.
pub const LLAMA_POOLING_TYPE_LAST: i32 = 3;
/// Reranking head; not a sentence vector.
pub const LLAMA_POOLING_TYPE_RANK: i32 = 4;

/// `typedef int32_t llama_token;` & friends (`include/llama.h`, "C interface"). Fixed-width on purpose:
/// C `int32_t` is 4 bytes on all three release targets, so these aliases cannot drift per platform.
pub type llama_token = i32;
/// Position of a token within a sequence.
pub type llama_pos = i32;
/// Sequence id; one batch can carry several sequences.
pub type llama_seq_id = i32;

/// `struct llama_batch` — the input to [`llama_decode`].
///
/// Field order and types copied from v0.8.0's header. The pointers do not own what they point at:
/// either they address arrays we allocated ([`llama_batch_init`]) or arrays the caller owns
/// ([`llama_batch_get_one`]). `logits` is how a batch asks for output per token — v0.8.0 has no
/// `llama_add_logits` C function; setting `logits[i] = 1` is what marks token *i* as an output, and `NULL` means
/// "only the last token is output **if embeddings are disabled**" (`include/llama.h:259-261`). The condition is
/// load-bearing for this crate: with `embeddings = true` — which is what [`crate::local_model::load`] sets — every
/// token becomes an output and a pooled row still arrives with `logits == NULL`. On the encoder path our artifact
/// actually takes that is `memory || cparams.embeddings` at `src/llama-context.cpp:1619`; on the decoder path it is
/// `output_all`, defined from `cparams.embeddings` at `:1866` and used at `:1909`. Drop the "if embeddings" clause
/// from that paraphrase and the next reader could conclude an embedding run needs a logits array.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct llama_batch {
    pub n_tokens: i32,
    pub token: *mut llama_token,
    pub embd: *mut f32,
    pub pos: *mut llama_pos,
    pub n_seq_id: *mut i32,
    pub seq_id: *mut *mut llama_seq_id,
    pub logits: *mut i8,
}

/// `struct llama_model_params` — passed **by value** to [`llama_model_load_from_file`].
///
/// Plain data with no ownership: the only pointer fields are either null (the AUTO case we want) or
/// arrays that must outlive the call. Get one from [`llama_model_default_params`] and set fields;
/// never build one field-by-field from a default-constructed struct, because the defaults are llama's
/// choice and have moved between revisions.
#[repr(C)]
pub struct llama_model_params {
    /// NULL-terminated device list; null means "all available devices" (AUTO for us).
    pub devices: *mut *mut std::ffi::c_void,
    /// NULL-terminated tensor/buffer-type overrides; null means none.
    pub tensor_buft_overrides: *const std::ffi::c_void,
    pub n_gpu_layers: i32,
    pub split_mode: i32,
    pub load_mode: i32,
    pub lazy_mode: i32,
    pub main_gpu: i32,
    pub tensor_split: *const f32,
    /// `bool (*)(float progress, void * user_data)` — null disables progress reporting.
    pub progress_callback: Option<extern "C" fn(f32, *mut std::ffi::c_void) -> bool>,
    pub progress_callback_user_data: *mut std::ffi::c_void,
    pub kv_overrides: *const std::ffi::c_void,
    pub vocab_only: bool,
    pub check_tensors: bool,
    pub use_extra_bufts: bool,
    pub no_host: bool,
    pub no_alloc: bool,
    pub load_mtp: bool,
}

/// `struct llama_context_params` — passed **by value** to [`llama_init_from_model`].
///
/// Same rule as [`llama_model_params`]: start from [`llama_context_default_params`]. Declared because
/// there is no way to reach [`llama_decode`] or [`llama_get_embeddings_seq`] without a context, and the
/// pooling/embedding switches R6b1 must set live in this struct (`pooling_type`, `embeddings`).
#[repr(C)]
pub struct llama_context_params {
    pub n_ctx: u32,
    pub n_batch: u32,
    pub n_ubatch: u32,
    pub n_seq_max: u32,
    pub n_rs_seq: u32,
    pub n_outputs_max: u32,
    pub n_outputs_max_per_seq: u32,
    pub n_threads: i32,
    pub n_threads_batch: i32,
    pub ctx_type: i32,
    pub rope_scaling_type: i32,
    /// `enum llama_pooling_type` — the value that makes [`llama_get_embeddings_seq`] return anything.
    pub pooling_type: i32,
    pub attention_type: i32,
    pub flash_attn_type: i32,
    pub rope_freq_base: f32,
    pub rope_freq_scale: f32,
    pub yarn_ext_factor: f32,
    pub yarn_attn_factor: f32,
    pub yarn_beta_fast: f32,
    pub yarn_beta_slow: f32,
    pub yarn_orig_ctx: u32,
    pub defrag_thold: f32,
    /// `ggml_backend_sched_eval_callback` takes **three** parameters (`ggml-backend.h:
    /// bool (*cb)(struct ggml_tensor * t, bool ask, void * user_data)`). Declaring it with two compiles,
    /// leaves every pinned offset unchanged (all function pointers are 8 bytes) and then reads `user_data`
    /// from the register that held `ask` — a wild pointer whose upper bits the psABI leaves unspecified.
    /// R6b0 review P1-1: this was the one declaration here that disagreed with the pinned header, and the
    /// layout pins cannot see callback-shape drift.
    pub cb_eval: Option<
        extern "C" fn(
            t: *mut std::ffi::c_void,
            ask: bool,
            user_data: *mut std::ffi::c_void,
        ) -> bool,
    >,
    pub cb_eval_user_data: *mut std::ffi::c_void,
    pub type_k: i32,
    pub type_v: i32,
    pub moe_cache_size: usize,
    pub abort_callback: Option<extern "C" fn(*mut std::ffi::c_void) -> bool>,
    pub abort_callback_data: *mut std::ffi::c_void,
    pub embeddings: bool,
    pub offload_kqv: bool,
    pub no_perf: bool,
    pub op_offload: bool,
    pub swa_full: bool,
    pub kv_unified: bool,
    pub samplers: *mut std::ffi::c_void,
    pub n_samplers: usize,
    pub ctx_other: *mut llama_context,
}

extern "C" {
    // ---- identity -------------------------------------------------------------

    /// The library's version string, e.g. `0.8.0-dev` (`src/llama.cpp`: `return LLAMA_VERSION;`).
    ///
    /// Memory: points at static storage owned by llama.cpp. Valid for the process lifetime, never to
    /// be freed, and not to be written through.
    ///
    /// This is also our proof that the binary linked *this* pinned tree rather than a `libllama` found
    /// somewhere else on the loader path — which is why `tests/local_runtime.rs` checks its prefix.
    pub fn llama_version() -> *const std::ffi::c_char;

    /// CPU/feature flags of the built library, e.g. `CPU : SSE3 = 1 | ... | OPENMP = 1`.
    ///
    /// Memory: v0.8.0 builds this into a function-local `static std::string` (`src/llama.cpp`,
    /// `llama_print_system_info`), so the pointer is owned by llama.cpp and **invalidated by the next
    /// call to this same function** — copy it out immediately, and do not call it from two threads.
    pub fn llama_print_system_info() -> *const std::ffi::c_char;

    // ---- parameters -----------------------------------------------------------

    /// Fresh model params with llama.cpp's own defaults. Returns a value type: nothing to free, and
    /// copying it is safe (see [`llama_model_params`] for what the pointer fields mean).
    pub fn llama_model_default_params() -> llama_model_params;

    /// Fresh context params with llama.cpp's own defaults. Same ownership as
    /// [`llama_model_default_params`].
    pub fn llama_context_default_params() -> llama_context_params;

    // ---- model ----------------------------------------------------------------

    /// Load a GGUF from disk. Returns null on failure (missing file, bad magic, unknown architecture).
    ///
    /// Memory: the returned model is heap-allocated by llama.cpp and **owned by us**; release it with
    /// [`llama_model_free`], and only after every context made from it is gone — a context keeps a
    /// pointer to its model rather than a reference count.
    ///
    /// `params` is taken by value, so the C++ side copies it; a local struct on our stack is fine.
    pub fn llama_model_load_from_file(
        path_model: *const std::ffi::c_char,
        params: llama_model_params,
    ) -> *mut llama_model;

    /// Free a model from [`llama_model_load_from_file`]. Idempotent only in the sense that llama.cpp
    /// checks for null; passing a twice-freed pointer is our bug, not its mercy.
    pub fn llama_model_free(model: *mut llama_model);

    /// The vocabulary inside a model.
    ///
    /// Memory: owned by `model`, valid as long as the model is, never freed on its own.
    ///
    /// **`model` may not be null.** v0.8.0's body is `return &model->vocab;` (`src/llama-model.cpp:3406-3408`) —
    /// no check — so a null in produces a *non-null pointer to nowhere*, not null: a caller that guards on
    /// "returns null" would pass its own guard and then dereference inside [`llama_vocab_n_tokens`]. We never
    /// reach that because the only handle here comes from a `NonNull` model ([`crate::local_model`] refuses a
    /// load that answered null). R6b0 review P1-1's lesson, applied to prose: this entry used to claim llama.cpp
    /// accepts null, and it does not.
    pub fn llama_model_get_vocab(model: *const llama_model) -> *const llama_vocab;

    /// Trained context window — a fact about the artifact (§5's token ceiling reads this rather than a
    /// constant). Reads metadata only, so it is valid on a `no_alloc` model.
    pub fn llama_model_n_ctx_train(model: *const llama_model) -> i32;

    /// Width of the vector this model **answers with** — for an embedding model, the width of one stored
    /// row. Distinct from [`llama_model_n_embd_inp`]: `gemma-embedding2` runs at 512 internally and projects
    /// to 768 (`src/llama-model.cpp:3430`: `return model->hparams.n_embd_out()`, which is
    /// `n_embd_out_impl` when the artifact declares `…embedding_length_out`, else `n_embd`).
    ///
    /// This is the number a declared width must be checked against — not [`llama_model_n_embd_inp`], and not
    /// the deprecated `llama_n_embd`, which returns the *internal* width and would let a 512-declared row
    /// pass while every stored vector was 768 wide.
    pub fn llama_model_n_embd_out(model: *const llama_model) -> i32;

    /// Width of a token-embedding row fed **into** the transformer (`src/llama-model.cpp:3426`:
    /// `hparams.n_embd_inp()`, i.e. `n_embd_inp_impl` or `n_embd`). A model can take input rows of one width
    /// and answer with another, so this is never the check for a stored vector.
    pub fn llama_model_n_embd_inp(model: *const llama_model) -> i32;

    /// Vocabulary size, i.e. the number of rows in the output projection.
    pub fn llama_vocab_n_tokens(vocab: *const llama_vocab) -> i32;

    /// Whether a tokenisation request with `add_special = true` prepends the BOS token
    /// (`tokenizer.ggml.add_bos_token`). A flag, not a count — but R6b2's token budget must still add one
    /// for it when it is set.
    ///
    /// Not the deprecated `llama_add_bos_token`, which v0.8.0 keeps alive with the same behaviour
    /// (`include/llama.h:1251`): declaring both would make the wrong spelling reachable without a compile
    /// error. **`vocab` may not be null:** `src/llama-vocab.cpp:4768-4774` is `return vocab->get_add_bos();` and
    /// the same for EOS, both unconditional dereferences — the header's silence is not a null contract. We never
    /// pass null: the only vocabulary handle in this crate comes from a live model (see
    /// [`crate::local_model`]’s `read_numbers`, which reads all three vocab facts while the model is alive).
    pub fn llama_vocab_get_add_bos(vocab: *const llama_vocab) -> bool;

    /// Whether a tokenisation request with `add_special = true` appends the EOS token. Same owner and same
    /// deprecation warning as [`llama_vocab_get_add_bos`]; both are set for `embeddinggemma-2`, which is why
    /// this round reads them separately instead of assuming one.
    pub fn llama_vocab_get_add_eos(vocab: *const llama_vocab) -> bool;

    // ---- context --------------------------------------------------------------

    /// Create a decode context over `model`. Returns null on failure.
    ///
    /// Memory: owned by us; free with [`llama_free`]. `model` must outlive the context — llama.cpp
    /// stores the pointer and does not retain it.
    pub fn llama_init_from_model(
        model: *mut llama_model,
        params: llama_context_params,
    ) -> *mut llama_context;

    /// Free a context from [`llama_init_from_model`] (and with it its KV cache and output buffers —
    /// every pointer handed out by [`llama_get_embeddings_seq`] dies here).
    pub fn llama_free(ctx: *mut llama_context);

    /// The pooling strategy this **context** will apply, which is not necessarily what the caller asked for:
    /// `src/llama-context.cpp` keeps `params.pooling_type` when it is not
    /// [`LLAMA_POOLING_TYPE_UNSPECIFIED`] and otherwise copies the model's own value — resolving an
    /// unspecified pair to [`LLAMA_POOLING_TYPE_NONE`].
    ///
    /// Read it after the context exists rather than trusting metadata: with `NONE` the decode path writes
    /// per-token rows and never the sequence map, so [`llama_get_embeddings_seq`] can only return null — an
    /// artifact that cannot produce a sentence vector (`src/llama-context.cpp`: `get_embeddings_seq` looks up
    /// `embd_seq`, and only the MEAN/CLS/LAST/RANK arms of the embedding extraction write it).
    pub fn llama_pooling_type(ctx: *const llama_context) -> i32;

    // ---- tokens ---------------------------------------------------------------

    /// Text to tokens. Writes at most `n_tokens_max` entries into `tokens` (caller-owned — and the caller can
    /// learn the exact size it needs by calling with `n_tokens_max = 0`, which writes nothing and answers with
    /// that number; `LoadedModel::tokenize` sizes every buffer that way).
    ///
    /// **Three outcomes, and the third is not a resize signal** (`include/llama.h:1271-1273`, implementation
    /// `src/llama-vocab.cpp:4632-4655`):
    ///
    /// * `>= 0` — tokens written, never more than `n_tokens_max`;
    /// * any other negative value — the buffer was too small and nothing was written; its magnitude is the number
    ///   of slots needed, **special tokens included**;
    /// * `i32::MIN` exactly — "INT32_MIN on overflow" (`include/llama.h:1274`), returned at
    ///   `src/llama-vocab.cpp:4640-4642` **before** the size check, for a result at or past `INT32_MAX` tokens.
    ///   Negating it overflows and allocating to its magnitude asks for ~8 GiB, so it is a refusal on its own
    ///   terms; [`crate::local_model`] separates all three with an enum rather than signs for exactly that reason.
    ///
    /// `text` need not be NUL-terminated; `text_len` decides. Thread-safe per the header.
    pub fn llama_tokenize(
        vocab: *const llama_vocab,
        text: *const std::ffi::c_char,
        text_len: i32,
        tokens: *mut llama_token,
        n_tokens_max: i32,
        add_special: bool,
        parse_special: bool,
    ) -> i32;

    // ---- batches --------------------------------------------------------------

    /// A batch view over `tokens` for a single sequence (id 0), positions tracked by [`llama_decode`].
    ///
    /// Memory: **borrowed**. The returned struct points at the caller's array and everything else is
    /// null, so it must never reach [`llama_batch_free`] — that function frees every non-null pointer
    /// it is given, which here would be our own buffer. Valid until `tokens` is freed or reused.
    pub fn llama_batch_get_one(tokens: *mut llama_token, n_tokens: i32) -> llama_batch;

    /// Allocate arrays for up to `n_tokens` entries (`embd == 0` means "token ids").
    ///
    /// **`n_tokens` is a capacity, not a count.** `src/llama-batch.cpp` builds the struct as
    /// `{ /*n_tokens =*/ 0, … }` and then `malloc`s each array for `n_tokens_alloc` entries, so the returned
    /// batch is empty: R6b1 must set `n_tokens` itself after filling `token`/`pos`/`seq_id`. (llama.cpp's own
    /// header comment says "all members are left uninitialized"; the source initialises `n_tokens` to 0, and
    /// the measured behaviour — asserted in `tests/local_runtime.rs` — is the source.)
    ///
    /// Memory: the returned struct owns `malloc`'d arrays. Exactly one [`llama_batch_free`] per call,
    /// and nothing may be read through the struct afterwards. The array *contents* are uninitialised — fill
    /// before use.
    pub fn llama_batch_init(n_tokens: i32, embd: i32, n_seq_max: i32) -> llama_batch;

    /// Free the arrays inside a batch from [`llama_batch_init`].
    ///
    /// Memory: takes the struct **by value** (v0.8.0's signature), so Rust cannot move-consume it:
    /// free once, then drop the local copy. Feeding it a [`llama_batch_get_one`] batch frees memory we
    /// did not allocate — the reason that function's doc says "borrowed" twice.
    pub fn llama_batch_free(batch: llama_batch);

    // ---- the built context's own numbers ---------------------------------------
    //
    // `include/llama.h:575-577` is explicit: "After creating a llama_context, it is recommended to query the actual
    // values using these functions / In some cases the requested values via llama_context_params may differ from the
    // actual values used by the context". Two are mirrored because our whole no-abort argument rests on one of them
    // (`n_ubatch`), and the other because a window whose width is guessed is not a window.

    /// The context's actual token capacity — what it will hold, not what was requested (`include/llama.h:578`).
    ///
    /// # Safety
    ///
    /// `ctx` must be a live context from [`llama_init_from_model`].
    pub fn llama_n_ctx(ctx: *const llama_context) -> u32;

    /// The context's actual micro-batch width (`include/llama.h:581`). On an encoder-only artifact this is the number
    /// `src/llama-context.cpp:1631`'s `GGML_ASSERT(n_ubatch >= n_tokens)` compares against, so it — not the requested
    /// `n_ctx` — is the ceiling above which llama.cpp aborts instead of returning anything.
    ///
    /// # Safety
    ///
    /// `ctx` must be a live context from [`llama_init_from_model`].
    pub fn llama_n_ubatch(ctx: *const llama_context) -> u32;

    /// The worker count this context will use for a single-token pass (`include/llama.h:1103`).
    ///
    /// Mirrored for one reason: `llama_context_default_params()` takes `n_threads` from `GGML_DEFAULT_N_THREADS`, a
    /// compile-time literal in `ggml/include/ggml.h:232` that upstream labels `// TODO: better default`, so "how many
    /// workers is this run using" is not answerable from our source.
    ///
    /// **Unlike [`llama_n_ubatch`], this one is an echo.** `include/llama.h:575-577` warns that *context* values may
    /// differ from what was requested, and `llama_n_ubatch` is the built answer; here llama.cpp copies the field
    /// (`src/llama-context.cpp:204-205`) and returns it unchanged (`:954-960`). So this proves what the context was
    /// told — useful for catching a caller that never applied its option, or a value that arrived truncated — while
    /// proof that workers actually ran is throughput.
    ///
    /// # Safety
    ///
    /// `ctx` must be a live context from [`llama_init_from_model`].
    pub fn llama_n_threads(ctx: *const llama_context) -> i32;

    /// The worker count this context will use for a batch/prompt pass (`include/llama.h:1106`).
    ///
    /// A separate number from [`llama_n_threads`] in llama.cpp's API, which is why both are read back: an embedding run
    /// takes the batch path, so a change that set only the generation count would look applied and do nothing.
    ///
    /// # Safety
    ///
    /// `ctx` must be a live context from [`llama_init_from_model`].
    pub fn llama_n_threads_batch(ctx: *const llama_context) -> i32;

    // ---- inference ------------------------------------------------------------

    /// Run the model over `batch`. The header's own outcome list (`include/llama.h:1001-1005`) has **five** answers and
    /// this mirror names all of them: `0` success, `1` no KV slot available, `2` aborted (processed ubatches stay in the
    /// context's memory), `-1` invalid input batch, `< -1` fatal error. Positive values are warnings rather than
    /// successes, and collapsing "non-zero" into one bucket loses the difference between "retry smaller" and "that run was
    /// cancelled". On error the memory state is restored (`:1000`).
    ///
    /// **The `1` needs a context that has a KV cache.** An architecture whose `create_memory` returns null — which
    /// includes `LLM_ARCH_GEMMA_EMBEDDING2`, `src/llama-model.cpp:2825` — has none: `llama_context::decode` logs
    /// "cannot decode batches with this context (calling encode() instead)" (`src/llama-context.cpp:1845-1847`) and
    /// the encoder path answers 0, `2` (aborted — `src/llama-context.cpp:1677`, and the header's outcome list at
    /// `include/llama.h:1003`) or negative, asserting on `n_ubatch >= n_tokens` (`:1631`) rather than reporting
    /// fullness. So callers on such an artifact get a refusal or an abort, never a `1`.
    ///
    /// Memory: allocates nothing for us to free. The results live in the context's own output buffer
    /// and stay valid until the next [`llama_decode`] or [`llama_free`] on that context.
    pub fn llama_decode(ctx: *mut llama_context, batch: llama_batch) -> i32;

    /// Pooled embedding for one sequence — **width depends on the pooling type** (`include/llama.h:1165-1169`):
    ///
    /// * `pooling_type == NONE` → null, always;
    /// * `pooling_type == RANK` → `float[n_cls_out]`, the sequence's rank(s), where `n_cls_out` is a header value
    ///   that defaults to **1** (`src/llama-hparams.h:253`) — **not** `n_embd`;
    /// * anything else (MEAN / CLS / LAST) → `float[n_embd]`, one-dimensional.
    ///
    /// Reading a RANK row at `n_embd` floats is an over-read of the context's buffer, which is why
    /// [`crate::local_model::load`] refuses RANK instead of modelling it as another sentence vector.
    ///
    /// Memory: owned by `ctx`, invalidated by the next [`llama_decode`] on the same context. Copy what
    /// you need; never store the pointer and never free it.
    pub fn llama_get_embeddings_seq(ctx: *mut llama_context, seq_id: llama_seq_id) -> *mut f32;

    /// Per-token embedding for output slot `i`: `n_embd` floats, or null for an invalid index.
    ///
    /// Memory: as [`llama_get_embeddings_seq`] — context-owned, short-lived, never freed here.
    ///
    /// **Declared and not called** (A1 review P2-3). Production code reads only the pooled row; this accessor is mirrored
    /// because §7's per-token verification is the next thing that will want it, and a half-mirrored API is how a later
    /// round invents a signature under time pressure. Read [`llama_get_embeddings_seq`]'s width rule first: what exists at
    /// all depends on the pooling type (`include/llama.h:1163-1169`), and this row belongs to the **token**, not the
    /// sequence — copying `n_embd_out` floats out of it is the same class of over-read that RANK pooling was refused for.
    pub fn llama_get_embeddings_ith(ctx: *mut llama_context, i: i32) -> *mut f32;
}

// ---- vision: the `mtmd` mirrors -------------------------------------------
//
// Everything in this section exists because one cargo feature links two more archives (`build.rs`'s `Shape`).
// Two headers are mirrored here, both from the tree named by `third_party/llama-cpp.toml`:
//
//   * `include/llama.h` — two functions the text path never needed. They stay in this module rather than in the
//     block above because nothing outside the image decode may flip a context's attention mask.
//   * `tools/mtmd/mtmd.h` and `tools/mtmd/mtmd-helper.h` — the projector, in a submodule of their own so a reader
//     can see at once which half of `np`'s FFI needs `libmtmd.a`.
//
// **One asymmetry worth knowing before a re-pin.** `build.rs` hashes `include/llama.h` and refuses a tree whose
// digest differs, so the text-path mirrors above are pinned to bytes. No digest covers `mtmd.h` — v0.8.0's own
// header calls its API "experimental and subject to many BREAKING CHANGES" (`mtmd.h:26-27`). These mirrors are
// therefore checked by hand on a re-pin: re-run the layout probe below against the new headers, because
// `mtmd_context_params` is passed **by value** and a field inserted in the middle would be C reading past what
// Rust wrote.

// `void llama_set_causal_attn(struct llama_context * ctx, bool causal_attn)` (`include/llama.h:1114`) and
// `bool llama_get_causal_attn(const struct llama_context * ctx)` (`:1117`).
//
// Mirrored for one reason, recorded in `crates/np-core/tests/probes/vision-spike-2026-11-02.md`:
// `mtmd_decode_use_non_causal` answers **true** for this architecture, so the image rows must be decoded with a
// non-causal mask. The get-half exists so the set is reversible: a seam that left the flag flipped would make
// every later *text* vector in the process quietly wrong while every image vector kept looking healthy — a
// failure no image-side assertion can see. `crate::vision` sets, decodes, and restores in one straight line with
// no early return between them, and `tests/local_vision.rs` pins that with bitwise text comparison.
#[cfg(feature = "vision")]
extern "C" {
    /// Set whether this context masks attention causally. Affects later decodes only.
    ///
    /// # Safety
    ///
    /// `ctx` must be a live context from [`llama_init_from_model`].
    pub fn llama_set_causal_attn(ctx: *mut llama_context, causal_attn: bool);

    /// What this context currently masks with — read it **before** changing it, so the change can be undone.
    ///
    /// # Safety
    ///
    /// `ctx` must be a live context from [`llama_init_from_model`].
    pub fn llama_get_causal_attn(ctx: *const llama_context) -> bool;
}

/// Mirrors of llama.cpp v0.8.0's `tools/mtmd/mtmd.h` and `tools/mtmd/mtmd-helper.h`, behind `vision`.
///
/// The C spellings are kept (`mtmd_context`, not `MtmdContext`) for the reason this file already gives: a reviewer
/// checking a mirror greps for the C name. Every declaration names the header line it was read from, and — as in
/// the text-path block — each one says who owns what, because that is the only thing between us and a
/// use-after-free across a language boundary.
///
/// What is **not** declared here is deliberate: audio (`mtmd_support_audio`, the stream and generation APIs — §3
/// of `docs/design-m2-image-embeddings.md` makes audio a non-goal, and the seam refuses an audio bitmap rather
/// than modelling it), video, batching (`mtmd_batch_*`: one image per call is what [`crate::vision`] promises),
/// chunk save/load/copy, and M-RoPE position helpers (`mtmd_decode_use_mrope` answers false for this projector and
/// the seam refuses a true answer rather than guessing at section-major positions). Declaring an API we never call
/// would only make the wrong one reachable without a compile error.
#[cfg(feature = "vision")]
pub mod mtmd {
    // The C spellings again, for the same reason as the parent module.
    #![allow(non_camel_case_types)]

    // `mtmd_init_from_file` takes the text model by pointer: the projector is built *against* a loaded model
    // rather than beside one (`mtmd.h:143-145`).
    use super::llama_model;

    /// Opaque handles (`mtmd.h:70-76`, `mtmd-helper.h:23`): pointee types with no layout we may read.
    pub enum mtmd_context {}
    /// One decoded image or audio clip (`mtmd.h:71`). Owned by whoever built it; freed with [`mtmd_bitmap_free`].
    pub enum mtmd_bitmap {}
    /// A video decoder a helper attached to a bitmap (`mtmd-helper.h:23`). Never ours to read, only to free.
    pub enum mtmd_helper_video {}
    /// One decode-ready piece of input — text or media (`mtmd.h:72`). Owned by the chunks list that holds it.
    pub enum mtmd_input_chunk {}
    /// A list of chunks, filled only by [`mtmd_tokenize`] (`mtmd.h:73`).
    pub enum mtmd_input_chunks {}

    /// `enum mtmd_input_chunk_type` (`mtmd.h:53-58`) as constants, following the pooling-type precedent in the
    /// parent module: the field crossing the boundary is a 4-byte C enum, mirrored as `i32`, and every value the
    /// header declares is spelled out so "unrecognised" stays distinguishable from "text".
    pub const MTMD_INPUT_CHUNK_TYPE_TEXT: i32 = 0;
    /// An image chunk: [`mtmd_encode_chunk`] turns it into embedding rows.
    pub const MTMD_INPUT_CHUNK_TYPE_IMAGE: i32 = 1;
    /// An audio chunk — refused by [`crate::vision`], never encoded (§3's non-goal).
    pub const MTMD_INPUT_CHUNK_TYPE_AUDIO: i32 = 2;
    /// Validation sentinel, not a real chunk type.
    pub const MTMD_INPUT_CHUNK_TYPE_COUNT: i32 = 3;

    /// `struct mtmd_context_params` (`mtmd.h:107-128`) — passed **by value** to [`mtmd_init_from_file`].
    ///
    /// Same rule as [`crate::llama_ffi::llama_context_params`]: start from [`mtmd_context_params_default`] and set
    /// named fields; never construct one field-by-field, because the defaults are mtmd's choice (`mtmd.cpp:533-552`
    /// asks for GPU, timings and a warmup pass — three things this crate turns off, and the only place that gets to
    /// decide what "off" means alongside them is the caller that reads them back).
    #[repr(C)]
    pub struct mtmd_context_params {
        /// Upstream default is `true`; this crate asks for CPU, matching how the text model is loaded.
        pub use_gpu: bool,
        /// `ggml_backend_dev_t` (`ggml-backend.h`) — a device pointer; null means "choose".
        pub device: *mut std::ffi::c_void,
        /// Per-encode timing lines on stderr. Off: our output is one line per image, not llama.cpp's.
        pub print_timings: bool,
        /// Worker count for the vision encoder's own ggml graph. Left at mtmd's default (4) — R7c's worker policy
        /// governs the text context, and §11 says an image encode runs on the same llama context.
        pub n_threads: i32,
        /// Deprecated in favour of [`mtmd_context_params::media_marker`] (`mtmd.h:113`), but still a field: leaving
        /// it out would shift every offset after it.
        pub image_marker: *const std::ffi::c_char,
        /// The marker string that splits the prompt around the media. Null means mtmd's own default.
        pub media_marker: *const std::ffi::c_char,
        /// `enum llama_flash_attn_type` — mirrored as `i32`, as in the parent module.
        pub flash_attn_type: i32,
        /// Whether to run a warmup encode after init. Off: nothing here needs a warmed encoder, and it costs an
        /// image-sized pass at open time.
        pub warmup: bool,
        /// Soft-token floor; -1 means "read from the artifact" (the card's 70–1120 lever, unused this round — the
        /// spike note measured that it *is* honoured, and §8 measures cost per encode instead of choosing a budget).
        pub image_min_tokens: i32,
        /// Soft-token ceiling; -1 means "read from the artifact".
        pub image_max_tokens: i32,
        /// `ggml_backend_sched_eval_callback` — **three** parameters (`ggml-backend.h`:
        /// `bool (*cb)(struct ggml_tensor * t, bool ask, void * user_data)`), the same trap R6b0 review P1-1 caught
        /// in the text-path mirror: two compiles, keeps every offset, and reads `user_data` from a garbage register.
        pub cb_eval: Option<
            extern "C" fn(
                t: *mut std::ffi::c_void,
                ask: bool,
                user_data: *mut std::ffi::c_void,
            ) -> bool,
        >,
        pub cb_eval_user_data: *mut std::ffi::c_void,
        /// Max tokens per encode batch; the first image is added even if it exceeds this. Only `mtmd_batch_*` reads
        /// it, and those are not declared — but the field is part of the layout we pass by value.
        pub batch_max_tokens: i32,
        /// `mtmd_progress_callback` (`mtmd.h:105`): `bool (*)(float progress, void * user_data)`.
        pub progress_callback: Option<extern "C" fn(f32, *mut std::ffi::c_void) -> bool>,
        pub progress_callback_user_data: *mut std::ffi::c_void,
    }

    /// `struct mtmd_input_text` (`mtmd.h:77-82`) — passed by **pointer** to [`mtmd_tokenize`].
    ///
    /// `text` need not be NUL-terminated: `mtmd.cpp:1330` does `input_text.assign(text->text, text->text_len)`,
    /// so the length is what decides. The pointer must outlive that call.
    #[repr(C)]
    pub struct mtmd_input_text {
        pub text: *const std::ffi::c_char,
        /// Bytes at [`mtmd_input_text::text`], not characters.
        pub text_len: usize,
        pub add_special: bool,
        pub parse_special: bool,
    }

    /// `struct mtmd_helper_video_init_params` (`mtmd-helper.h:26-33`) — mirrored only because it is the *whole*
    /// body of [`mtmd_helper_init_opt`], which crosses the boundary by value. Nothing reads a field.
    #[repr(C)]
    pub struct mtmd_helper_video_init_params {
        pub fps_target: f32,
        pub ffmpeg_bin_dir: *const std::ffi::c_char,
        pub timestamp_interval_ms: i64,
        pub no_soundtrack: bool,
    }

    /// `struct mtmd_helper_init_opt` (`mtmd-helper.h:37-39`, typedef'd at `:40`) — by value to
    /// [`mtmd_helper_bitmap_init_from_buf`]. Take it from [`mtmd_helper_init_opt_default`]; the pinned code marks
    /// the whole option `GGML_UNUSED` on the image path (`mtmd-helper.cpp:379`) and video is not something this
    /// crate decodes.
    #[repr(C)]
    pub struct mtmd_helper_init_opt {
        pub video_params: mtmd_helper_video_init_params,
    }

    /// `struct mtmd_helper_bitmap_wrapper` (`mtmd-helper.h:49-52`) — returned **by value**.
    ///
    /// Two handles, and both are ours if non-null. On this build the image path leaves `video_ctx` null
    /// (`mtmd-helper.cpp:387`, `:406`, `:414`), but `MTMD_VIDEO` is ON in our cmake cache (`LLAMA_SUBPROCESS`
    /// defaults ON, so the force-off at `tools/mtmd/CMakeLists.txt:9-12` does not fire) and a buffer that reaches
    /// the video path would fill it, so [`crate::vision`] frees both.
    ///
    /// **Do not read this mirror as the thing that keeps non-raster formats out.** What bounds the corpus is §7's
    /// extension gate in `crate::media`, not this decoder: measured on 2026-11-03, a BMP payload reached
    /// [`mtmd_helper_bitmap_init_from_buf`] through a `.png` filename and came back a live bitmap and then a vector
    /// (80 tokens). `stb_image` inside `libmtmd` decodes bmp/tga/gif/psd/pnm/qoi as well as png and jpeg, and the webp
    /// and video arms need ffmpeg (`mtmd-helper.cpp:417-427`), which is not installed on this host — so the promise
    /// "png/jpg/jpeg only" is enforced where files are discovered, and a mislabelled file is a scope leak rather than
    /// a decode failure.
    #[repr(C)]
    pub struct mtmd_helper_bitmap_wrapper {
        pub bitmap: *mut mtmd_bitmap,
        pub video_ctx: *mut mtmd_helper_video,
    }

    extern "C" {
        // ---- context ---------------------------------------------------------

        /// Fresh params with mtmd's own defaults (`mtmd.h:139`; body `mtmd.cpp:533`). Value type, nothing to free.
        pub fn mtmd_context_params_default() -> mtmd_context_params;

        /// Load a projector against an **already loaded** text model. Null on failure (`mtmd.h:143-145`).
        ///
        /// Memory: the returned context is owned by us and freed with [`mtmd_free`], before the model dies — it
        /// keeps a raw pointer to `text_model` (member at `mtmd.cpp:567`, initialised at `:615`). Unlike llama.cpp's own
        /// loader this entry point catches `std::exception` and answers null (`mtmd.cpp:1256-1260`), so a bad
        /// projector is a refusal rather than a foreign exception crossing into Rust.
        pub fn mtmd_init_from_file(
            mmproj_fname: *const std::ffi::c_char,
            text_model: *const llama_model,
            ctx_params: mtmd_context_params,
        ) -> *mut mtmd_context;

        /// Free a context from [`mtmd_init_from_file`] (`mtmd.h:147`). Idempotent only in that `delete` tolerates
        /// null; passing a twice-freed pointer is our bug.
        pub fn mtmd_free(ctx: *mut mtmd_context);

        // ---- capability and shape facts --------------------------------------

        /// Whether the loaded projector has a vision graph (`mtmd.h:157`).
        ///
        /// True for the pinned `mmproj` (measured in the spike note). The seam refuses a false answer as §8's
        /// `vision-unavailable` rather than decoding into an encoder that is not there.
        ///
        /// # Safety
        ///
        /// `ctx` must be live.
        pub fn mtmd_support_vision(ctx: *const mtmd_context) -> bool;

        /// Whether this model needs a non-causal mask for `chunk` (`mtmd.h:151`); a null chunk asks about the
        /// default image case. **True for this architecture** — the reason [`crate::llama_ffi::llama_set_causal_attn`]
        /// is mirrored.
        ///
        /// # Safety
        ///
        /// `ctx` live; `chunk` either null or owned by a live chunks list.
        pub fn mtmd_decode_use_non_causal(
            ctx: *const mtmd_context,
            chunk: *const mtmd_input_chunk,
        ) -> bool;

        /// Whether the model uses M-RoPE positions (`mtmd.h:154`). False for this projector; [`crate::vision`]
        /// refuses a true answer instead of writing 1-D positions into a batch that expects four sections.
        ///
        /// # Safety
        ///
        /// `ctx` must be live (from [`mtmd_init_from_file`]).
        pub fn mtmd_decode_use_mrope(ctx: *const mtmd_context) -> bool;

        /// The marker string this context splits prompts on (`mtmd.h:181`; body returns a `std::string`'s
        /// `c_str()`, so it is NUL-terminated and owned by `ctx`). Valid while `ctx` is.
        ///
        /// # Safety
        ///
        /// `ctx` must be live (from [`mtmd_init_from_file`]).
        pub fn mtmd_get_marker(ctx: *const mtmd_context) -> *const std::ffi::c_char;

        // ---- bitmap ----------------------------------------------------------

        /// Whether a decoded bitmap holds audio (`mtmd.h:205`). The seam's guard for §3's audio non-goal: this
        /// projector answers `mtmd_support_audio == true`, so without this check WAV bytes would reach the encoder.
        ///
        /// # Safety
        ///
        /// `bitmap` must be live.
        pub fn mtmd_bitmap_is_audio(bitmap: *const mtmd_bitmap) -> bool;

        /// Free a bitmap from [`crate::llama_ffi::mtmd::mtmd_helper_bitmap_init_from_buf`] (`mtmd.h:206`).
        ///
        /// # Safety
        ///
        /// First and only free of this handle.
        pub fn mtmd_bitmap_free(bitmap: *mut mtmd_bitmap);

        // ---- helper (decode + video) -----------------------------------------

        /// Fresh helper options (`mtmd-helper.h:42`). Value type, nothing to free.
        pub fn mtmd_helper_init_opt_default() -> mtmd_helper_init_opt;

        /// Decode an image or audio buffer into a bitmap (`mtmd-helper.h:75-79`) — PNG and JPEG through the
        /// `stb_image` already inside `libmtmd` (§8's reason no new dependency enters this build).
        ///
        /// Ownership: on success **both** wrapper fields are ours if non-null. The bitmap copies the buffer, so
        /// `buf` may be dropped as soon as this returns; the id it stamps is a sha256 of these bytes
        /// (`mtmd-helper.cpp:381-383`), which is why no caller needs to name the file. A null bitmap means "these
        /// bytes are not media this build can decode" — mtmd's own error text goes to stderr and never into our
        /// refusal (§12 rule 8). Unlike [`mtmd_init_from_file`] this function has no `try`/`catch`: the image path
        /// is C (stb) plus one hash, so nothing throws short of an allocation failure, which aborts either way.
        ///
        /// `placeholder == false`, always, for us: a placeholder bitmap is a token-counting stand-in that
        /// [`mtmd_encode_chunk`] refuses by contract (`mtmd.h:190-195`).
        pub fn mtmd_helper_bitmap_init_from_buf(
            ctx: *const mtmd_context,
            buf: *const std::ffi::c_uchar,
            len: usize,
            placeholder: bool,
            opt: mtmd_helper_init_opt,
        ) -> mtmd_helper_bitmap_wrapper;

        /// Free a video decoder handed back by [`mtmd_helper_bitmap_init_from_buf`] (`mtmd-helper.h:176`).
        /// Declared so the wrapper's second handle is always releasable — `MTMD_VIDEO` is ON in our configure, so
        /// treating it as impossible would be a claim about this build rather than a fact of it.
        ///
        /// # Safety
        ///
        /// First and only free of this handle.
        pub fn mtmd_helper_video_free(ctx: *mut mtmd_helper_video);

        // ---- chunks ----------------------------------------------------------

        /// An empty chunk list, filled only by [`mtmd_tokenize`] (`mtmd.h:251`).
        pub fn mtmd_input_chunks_init() -> *mut mtmd_input_chunks;

        /// How many chunks the list holds (`mtmd.h:252`).
        ///
        /// # Safety
        ///
        /// `chunks` must be live.
        pub fn mtmd_input_chunks_size(chunks: *const mtmd_input_chunks) -> usize;

        /// Chunk `idx`, borrowed from the list and freed with it (`mtmd.h:253`). Out-of-range is not checked here,
        /// so callers bound the index by [`mtmd_input_chunks_size`].
        ///
        /// # Safety
        ///
        /// `chunks` live and `idx` below its size.
        pub fn mtmd_input_chunks_get(
            chunks: *const mtmd_input_chunks,
            idx: usize,
        ) -> *const mtmd_input_chunk;

        /// Free the list and every chunk inside it (`mtmd.h:254`) — which invalidates every pointer from
        /// [`mtmd_input_chunks_get`] and every row [`mtmd_get_output_embd`] handed back for those chunks.
        pub fn mtmd_input_chunks_free(chunks: *mut mtmd_input_chunks);

        /// What kind of chunk this is (`mtmd.h:260`), one of the `MTMD_INPUT_CHUNK_TYPE_*` constants.
        ///
        /// # Safety
        ///
        /// `chunk` must be live.
        pub fn mtmd_input_chunk_get_type(chunk: *const mtmd_input_chunk) -> i32;

        /// Tokens this chunk occupies in the decoder (`mtmd.h:263`) — for an image chunk, its patch count, which
        /// is the number §8's budget rule and [`crate::vision::VisionModel::last_token_cost`] report.
        ///
        /// # Safety
        ///
        /// `chunk` must be live.
        pub fn mtmd_input_chunk_get_n_tokens(chunk: *const mtmd_input_chunk) -> usize;

        // ---- the pipeline ----------------------------------------------------

        /// Split a prompt around the media marker and pair each marker with a bitmap (`mtmd.h:330-334`).
        ///
        /// Returns 0, 1 for "marker count does not match the bitmaps", 2 for a preprocessing error — and 2 is also
        /// what an internal `std::exception` becomes (`mtmd.cpp:2067-2076`), so no C++ exception escapes here.
        /// `output` is filled on success only; either way the caller frees it.
        ///
        /// # Safety
        ///
        /// `ctx` live; `output` from [`mtmd_input_chunks_init`]; `text` a valid pointer whose `text_len` bytes are
        /// readable and outlive the call; `bitmaps` an array of `n_bitmaps` live bitmap pointers. The bitmaps keep
        /// their owners — mtmd copies what it needs into the chunks.
        pub fn mtmd_tokenize(
            ctx: *const mtmd_context,
            output: *mut mtmd_input_chunks,
            text: *const mtmd_input_text,
            bitmaps: *const *const mtmd_bitmap,
            n_bitmaps: usize,
        ) -> i32;

        /// Run the vision encoder over one media chunk (`mtmd.h:354-355`); 0 on success, 1 on a generic error, and
        /// an internal exception is caught into that same 1 (`mtmd.cpp:2207-2213`). Text chunks are ignored
        /// silently, which is why the caller picks them out by type first.
        ///
        /// # Safety
        ///
        /// `ctx` live; `chunk` a live chunk belonging to a list this context tokenized.
        pub fn mtmd_encode_chunk(ctx: *mut mtmd_context, chunk: *const mtmd_input_chunk) -> i32;

        /// The rows the last [`mtmd_encode_chunk`] produced (`mtmd.h:360`): reading size is
        /// `llama_model_n_embd_inp(model) * n_tokens * 4` bytes — **input** width, not the stored 768.
        ///
        /// Memory: owned by `ctx`, valid until the next encode on it. Never freed here, and null means the last
        /// pass produced nothing.
        ///
        /// # Safety
        ///
        /// `ctx` must be live.
        pub fn mtmd_get_output_embd(ctx: *mut mtmd_context) -> *mut f32;
    }

    mod layout {
        //! Layout pins for the three structs that cross by value or by pointer — same discipline, and the same
        //! measured provenance, as the `layout` module of [`crate::llama_ffi`]: a wrong field order here is not a compile error, it
        //! is C reading past what Rust wrote.
        //!
        //! ```text
        //! $ cc -O1 -std=c11 -I .vendor/llama.cpp-v0.8.0/include -I .vendor/llama.cpp-v0.8.0/ggml/include \
        //!       -I .vendor/llama.cpp-v0.8.0/tools/mtmd -o /tmp/mtmd_layout /tmp/mtmd_layout.c && /tmp/mtmd_layout
        //! sizeof(struct mtmd_context_params)=96 align=8
        //!   use_gpu=0 device=8 print_timings=16 n_threads=20 image_marker=24 media_marker=32
        //!   flash_attn_type=40 warmup=44 image_min_tokens=48 image_max_tokens=52 cb_eval=56
        //!   cb_eval_user_data=64 batch_max_tokens=72 progress_callback=80 progress_callback_user_data=88
        //! sizeof(mtmd_input_text)=24 align=8
        //!   text=0 text_len=8 add_special=16 parse_special=17
        //! sizeof(struct mtmd_helper_video_init_params)=32 align=8
        //!   fps_target=0 ffmpeg_bin_dir=8 timestamp_interval_ms=16 no_soundtrack=24
        //! sizeof(mtmd_helper_init_opt)=32 align=8  video_params=0
        //! sizeof(struct mtmd_helper_bitmap_wrapper)=16 align=8  bitmap=0 video_ctx=8
        //! sizeof(enum mtmd_input_chunk_type)=4 sizeof(enum llama_flash_attn_type)=4
        //! ```
        //!
        //! Measured on this host on 2026-11-03 against the tree `third_party/llama-cpp.toml` pins. The header itself
        //! is not digest-pinned (see this module's note), which is exactly why these numbers exist: they turn a
        //! silent layout disagreement into a build failure on the first feature-on compile after a re-pin.

        use super::*;
        use std::mem::{align_of, offset_of, size_of};

        const _: () = assert!(size_of::<*mut u8>() == 8, "this is the 64-bit layout");

        const _: () = {
            assert!(size_of::<mtmd_context_params>() == 96);
            assert!(align_of::<mtmd_context_params>() == 8);
            // `bool` then a pointer forces 7 bytes of padding at 1..8; four 4-byte scalars follow the two bools.
            assert!(offset_of!(mtmd_context_params, use_gpu) == 0);
            assert!(offset_of!(mtmd_context_params, device) == 8);
            assert!(offset_of!(mtmd_context_params, print_timings) == 16);
            assert!(offset_of!(mtmd_context_params, n_threads) == 20);
            assert!(offset_of!(mtmd_context_params, image_marker) == 24);
            assert!(offset_of!(mtmd_context_params, media_marker) == 32);
            assert!(offset_of!(mtmd_context_params, flash_attn_type) == 40);
            assert!(offset_of!(mtmd_context_params, warmup) == 44);
            assert!(offset_of!(mtmd_context_params, image_min_tokens) == 48);
            assert!(offset_of!(mtmd_context_params, image_max_tokens) == 52);
            assert!(offset_of!(mtmd_context_params, cb_eval) == 56);
            assert!(offset_of!(mtmd_context_params, cb_eval_user_data) == 64);
            assert!(offset_of!(mtmd_context_params, batch_max_tokens) == 72);
            assert!(offset_of!(mtmd_context_params, progress_callback) == 80);
            assert!(offset_of!(mtmd_context_params, progress_callback_user_data) == 88);
        };

        const _: () = {
            assert!(size_of::<mtmd_input_text>() == 24);
            assert!(align_of::<mtmd_input_text>() == 8);
            assert!(offset_of!(mtmd_input_text, text) == 0);
            assert!(offset_of!(mtmd_input_text, text_len) == 8);
            assert!(offset_of!(mtmd_input_text, add_special) == 16);
            assert!(offset_of!(mtmd_input_text, parse_special) == 17);
        };

        const _: () = {
            // The float at 0 leaves 4 bytes before the pointer; the bool at 24 pads the struct to 32.
            assert!(size_of::<mtmd_helper_video_init_params>() == 32);
            assert!(offset_of!(mtmd_helper_video_init_params, fps_target) == 0);
            assert!(offset_of!(mtmd_helper_video_init_params, ffmpeg_bin_dir) == 8);
            assert!(offset_of!(mtmd_helper_video_init_params, timestamp_interval_ms) == 16);
            assert!(offset_of!(mtmd_helper_video_init_params, no_soundtrack) == 24);
            // The wrapper is two pointers, and the option struct is exactly its one member — both cross by value.
            assert!(size_of::<mtmd_helper_init_opt>() == 32);
            assert!(offset_of!(mtmd_helper_init_opt, video_params) == 0);
            assert!(size_of::<mtmd_helper_bitmap_wrapper>() == 16);
            assert!(offset_of!(mtmd_helper_bitmap_wrapper, bitmap) == 0);
            assert!(offset_of!(mtmd_helper_bitmap_wrapper, video_ctx) == 8);
        };

        /// The chunk-type mirror is `i32`, which is what the probe measured C's enum to be on this ABI. The C-side
        /// width is still the part no Rust assertion can pin — see the parent module's layout notes.
        const _: () = {
            assert!(MTMD_INPUT_CHUNK_TYPE_TEXT == 0);
            assert!(MTMD_INPUT_CHUNK_TYPE_IMAGE == 1);
            assert!(MTMD_INPUT_CHUNK_TYPE_AUDIO == 2);
            assert!(MTMD_INPUT_CHUNK_TYPE_COUNT == 3);
        };

        /// Both by-value structs must be `Copy` in Rust for the call to pass them the way C does; a field that ever
        /// gained a `Drop` type would make this fail at compile time instead of at the boundary.
        #[cfg(test)]
        #[test]
        fn by_value_structs_are_plain_copies() {
            assert!(!std::mem::needs_drop::<mtmd_context_params>());
            assert!(!std::mem::needs_drop::<mtmd_helper_init_opt>());
            assert!(!std::mem::needs_drop::<mtmd_helper_bitmap_wrapper>());
        }
    }
}

mod layout {
    //! Layout pins for the three structs that cross the FFI by value — see below for what they can and
    //! cannot catch.
    //!
    //! A wrong field order or missing padding is not a compile error: it is C reading past what Rust wrote,
    //! into whichever field happens to sit there. So every expected size and offset here was measured against
    //! the pinned header with a C probe, not transcribed by eye:
    //!
    //! ```text
    //! $ gcc -I .vendor/llama.cpp-v0.8.0/include -I .vendor/llama.cpp-v0.8.0/ggml/include \
    //!       -std=c11 -o /tmp/layout_probe /tmp/layout_probe.c && /tmp/layout_probe
    //! sizeof(llama_batch)=56
    //!   n_tokens=0 token=8 embd=16 pos=24 n_seq_id=32 seq_id=40 logits=48
    //! sizeof(llama_model_params)=80
    //!   devices=0 tensor_buft_overrides=8 n_gpu_layers=16 split_mode=20 load_mode=24 lazy_mode=28
    //!   main_gpu=32 tensor_split=40 progress_callback=48 progress_callback_user_data=56 kv_overrides=64
    //!   vocab_only=72 check_tensors=73 use_extra_bufts=74 no_host=75 no_alloc=76 load_mtp=77
    //! sizeof(llama_context_params)=168
    //!   n_ctx=0 n_batch=4 n_threads=28 n_threads_batch=32 pooling_type=44 flash_attn_type=52
    //!   rope_freq_base=56 yarn_orig_ctx=80 defrag_thold=84 cb_eval=88 cb_eval_user_data=96 type_k=104
    //!   type_v=108 moe_cache_size=112 abort_callback=120 embeddings=136 kv_unified=141 samplers=144
    //!   n_samplers=152 ctx_other=160
    //! sizeof(enum llama_pooling_type)=4 sizeof(enum ggml_type)=4 sizeof(llama_token)=4
    //! sizeof(llama_pos)=4 sizeof(llama_seq_id)=4
    //! ```
    //!
    //! Run on 2026-10-29 against `include/llama.h` whose sha256 is `c0854771…` — the value pinned in
    //! `third_party/llama-cpp.toml` and re-checked by `build.rs` on every feature-on build, so these numbers
    //! belong to a named tree rather than to "some 0.8.x". The same run produced the field-name list checked
    //! below; a re-pin re-runs the probe and updates the constants deliberately.
    //!
    //! **Names and order too, not just offsets.** A second check compares the three header structs against
    //! this file field by field: `llama_batch` 7 fields, `llama_model_params` 17, `llama_context_params` 38 —
    //! identical names in identical order as measured on 2026-10-29. That matters because Rust does not care
    //! what a field is called: a mirror that renames `n_seq_id` to `pos` compiles, keeps every offset, and
    //! hands R6b1 a plausible-looking wrong field.
    //!
    //! ## What these pins cannot see
    //!
    //!   * **A callback with the wrong shape.** Every function pointer is 8 bytes, so `cb_eval` declared with
    //!     two parameters instead of the header's three left every number below unchanged and the suite green;
    //!     it was caught by reading `ggml-backend.h` (R6b0 review P1-1). Signature review against the header
    //!     is a human step, and this file is where its absence would show up.
    //!   * **A field added into existing tail padding** (`llama_context_params` pads at 142–143): size and
    //!     offsets unchanged, C and Rust disagreeing about what lives there. Benign in practice because R6b1
    //!     starts every struct from llama.cpp's own `*_default_params()` and changes named fields — it never
    //!     constructs one field-by-field-default and hands it over. If that ever changes, this sentence stops
    //!     being the defence and the header digest becomes load-bearing instead.
    //!   * **C's `sizeof(enum)`.** The probe measured 4 for every enum here on this ABI; nothing in Rust can
    //!     detect llama.cpp being built with `-fshort-enums`. Recorded rather than covered.
    //!
    //! The numbers are the SysV x86-64 / AArch64 LP64 layout and also MSVC's for these structs: 8-byte
    //! pointers, 4-byte `int`/`enum`, 1-byte `bool`, no bitfields — which is why the pointer-width guard below
    //! fails loudly on a 32-bit host instead of silently disagreeing with C.

    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    // These pins are **not** behind `#[cfg(test)]`, on purpose: a wrong mirror then fails any
    // `cargo build`/`check` with `--features local-model`, not only a test run. Measured — deleting
    // `llama_batch::n_seq_id` survived plain `cargo check` while these pins sat in `#[cfg(test)]`, and failed
    // the build once they left it; widening `n_ctx` to `i64` fails with "assertion failed:
    // size_of::<llama_context_params>() == 168". (R6b0 review P2-6 asked for const assertions; this is the
    // placement that actually bites.) The cost is honest: on a 32-bit host this module stops compiling at all,
    // which is the loud failure the pointer-width guard below was always trying to produce.

    const _: () = assert!(
        size_of::<*mut u8>() == 8,
        "these constants are the 64-bit layout"
    );

    const _: () = {
        assert!(size_of::<llama_batch>() == 56);
        assert!(align_of::<llama_batch>() == 8);
        // `n_tokens` is an i32 followed by pointers, so 4 bytes of padding must appear at offset 4.
        assert!(offset_of!(llama_batch, n_tokens) == 0);
        assert!(offset_of!(llama_batch, token) == 8);
        assert!(offset_of!(llama_batch, embd) == 16);
        assert!(offset_of!(llama_batch, pos) == 24);
        assert!(offset_of!(llama_batch, n_seq_id) == 32);
        assert!(offset_of!(llama_batch, seq_id) == 40);
        assert!(offset_of!(llama_batch, logits) == 48);
    };

    const _: () = {
        assert!(size_of::<llama_model_params>() == 80);
        assert!(align_of::<llama_model_params>() == 8);
        assert!(offset_of!(llama_model_params, devices) == 0);
        assert!(offset_of!(llama_model_params, tensor_buft_overrides) == 8);
        // Four enums packed between the pointers with no padding: 16, 20, 24, 28, then main_gpu at 32.
        assert!(offset_of!(llama_model_params, n_gpu_layers) == 16);
        assert!(offset_of!(llama_model_params, split_mode) == 20);
        assert!(offset_of!(llama_model_params, load_mode) == 24);
        assert!(offset_of!(llama_model_params, lazy_mode) == 28);
        assert!(offset_of!(llama_model_params, main_gpu) == 32);
        assert!(offset_of!(llama_model_params, tensor_split) == 40);
        assert!(offset_of!(llama_model_params, progress_callback) == 48);
        assert!(offset_of!(llama_model_params, progress_callback_user_data) == 56);
        assert!(offset_of!(llama_model_params, kv_overrides) == 64);
        // Six bools packed at the tail: 72..77, and the struct rounds up to 80.
        assert!(offset_of!(llama_model_params, vocab_only) == 72);
        assert!(offset_of!(llama_model_params, check_tensors) == 73);
        assert!(offset_of!(llama_model_params, use_extra_bufts) == 74);
        assert!(offset_of!(llama_model_params, no_host) == 75);
        assert!(offset_of!(llama_model_params, no_alloc) == 76);
        assert!(offset_of!(llama_model_params, load_mtp) == 77);
    };

    const _: () = {
        assert!(size_of::<llama_context_params>() == 168);
        assert!(align_of::<llama_context_params>() == 8);
        assert!(offset_of!(llama_context_params, n_ctx) == 0);
        assert!(offset_of!(llama_context_params, n_batch) == 4);
        assert!(offset_of!(llama_context_params, n_threads) == 28);
        assert!(offset_of!(llama_context_params, n_threads_batch) == 32);
        // The two fields R6b1 must set: pooling strategy, and `embeddings` itself.
        assert!(offset_of!(llama_context_params, pooling_type) == 44);
        assert!(offset_of!(llama_context_params, flash_attn_type) == 52);
        assert!(offset_of!(llama_context_params, yarn_orig_ctx) == 80);
        assert!(offset_of!(llama_context_params, cb_eval) == 88);
        assert!(offset_of!(llama_context_params, type_k) == 104);
        assert!(offset_of!(llama_context_params, moe_cache_size) == 112);
        assert!(offset_of!(llama_context_params, embeddings) == 136);
        assert!(offset_of!(llama_context_params, kv_unified) == 141);
        assert!(offset_of!(llama_context_params, samplers) == 144);
        assert!(offset_of!(llama_context_params, ctx_other) == 160);
    };

    /// Our side of the enum mirror: `llama_token`, `llama_pos` and `llama_seq_id` are `i32`, which is what the
    /// probe measured C to be. Asserted at compile time — as a `#[test]` this could not fail (the aliases *are*
    /// `i32`), which is precisely why the old runtime version of it was dropped (R6b0 review P2-6). The C-side
    /// width is the part no Rust assertion can pin; see "What these pins cannot see".
    const _: () = {
        assert!(size_of::<llama_token>() == 4);
        assert!(size_of::<llama_pos>() == 4);
        assert!(size_of::<llama_seq_id>() == 4);
    };

    /// The compile-time pins above are the guarantee; these two are the same claims restated where
    /// `cargo test` can show them. Cheap, and CI's `--lib` line keeps reporting that layout was checked.
    #[cfg(test)]
    mod runtime_checks {
        use super::*;

        /// The same three sizes, checked in the running binary. Cheap, and it keeps a visible line in
        /// `cargo test --features local-model --lib` for anyone reading CI output rather than this file.
        #[test]
        fn layout_constants_hold_in_the_running_binary() {
            assert_eq!(size_of::<llama_batch>(), 56);
            assert_eq!(size_of::<llama_model_params>(), 80);
            assert_eq!(size_of::<llama_context_params>(), 168);
        }

        /// `llama_batch` is passed by value in two signatures; if Rust ever stopped laying it out like the
        /// C struct that is copy-by-value, every one of those calls would be a silent memory bug.
        #[test]
        fn batch_is_passed_by_value_as_a_plain_copied_struct() {
            assert!(
                !std::mem::needs_drop::<llama_batch>(),
                "a by-value FFI struct cannot own anything"
            );
            let copied: llama_batch = llama_batch {
                n_tokens: 1,
                token: std::ptr::null_mut(),
                embd: std::ptr::null_mut(),
                pos: std::ptr::null_mut(),
                n_seq_id: std::ptr::null_mut(),
                seq_id: std::ptr::null_mut(),
                logits: std::ptr::null_mut(),
            };
            let also: llama_batch = copied;
            assert_eq!(copied.n_tokens, also.n_tokens);
        }
    }
}
