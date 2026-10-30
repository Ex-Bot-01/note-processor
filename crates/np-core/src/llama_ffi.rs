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
//!   `llama_n_vocab` — are deliberately absent. Declaring them would make it possible to reach the
//!   wrong one without a compile error, since they still exist in v0.8.0.
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
/// `llama_add_logits` C function; setting `logits[i] = 1` is what marks token *i* as an output, and
/// `NULL` means "only the last token" (header: "if set to NULL ... only the last token is output").
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
    /// Memory: owned by `model`, valid as long as the model is, never freed on its own. Passing null is
    /// accepted by llama.cpp but returns null.
    pub fn llama_model_get_vocab(model: *const llama_model) -> *const llama_vocab;

    /// Trained context window — a fact about the artifact (§5's token ceiling reads this rather than a
    /// constant). Reads metadata only, so it is valid on a `no_alloc` model.
    pub fn llama_model_n_ctx_train(model: *const llama_model) -> i32;

    /// Vocabulary size, i.e. the number of rows in the output projection.
    pub fn llama_vocab_n_tokens(vocab: *const llama_vocab) -> i32;

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

    // ---- tokens ---------------------------------------------------------------

    /// Text to tokens. Writes at most `n_tokens_max` entries into `tokens` (caller-owned, and the
    /// caller must size it: v0.8.0 gives no upper bound other than its own return value).
    ///
    /// Returns the count written, or a negative value whose magnitude is how many would have been
    /// needed — so a negative result is a resize signal, not an error to report as-is. `text` need not
    /// be NUL-terminated; `text_len` decides. Thread-safe per the header.
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

    // ---- inference ------------------------------------------------------------

    /// Run the model over `batch`. Returns 0 on success, 1 when the KV cache is full, and negative on
    /// error (header: on error the memory state is restored).
    ///
    /// Memory: allocates nothing for us to free. The results live in the context's own output buffer
    /// and stay valid until the next [`llama_decode`] or [`llama_free`] on that context.
    pub fn llama_decode(ctx: *mut llama_context, batch: llama_batch) -> i32;

    /// Pooled embedding for one sequence: `n_embd` floats, or null when the context has
    /// `pooling_type == NONE`.
    ///
    /// Memory: owned by `ctx`, invalidated by the next [`llama_decode`] on the same context. Copy what
    /// you need; never store the pointer and never free it.
    pub fn llama_get_embeddings_seq(ctx: *mut llama_context, seq_id: llama_seq_id) -> *mut f32;

    /// Per-token embedding for output slot `i`: `n_embd` floats, or null for an invalid index.
    ///
    /// Memory: as [`llama_get_embeddings_seq`] — context-owned, short-lived, never freed here.
    pub fn llama_get_embeddings_ith(ctx: *mut llama_context, i: i32) -> *mut f32;
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
