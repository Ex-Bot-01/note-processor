//! The link proof for `local-model`: llama.cpp is statically linked into this test binary, its symbols
//! resolve, and the code that answers is the pinned tree — not a `libllama` borrowed from somewhere on
//! the loader path.
//!
//! Two entry points are enough for that claim: [`llama_version`] returns a string compiled into the
//! library (`src/llama.cpp`: `return LLAMA_VERSION;`) and [`llama_print_system_info`] walks the
//! registered backends to describe CPU features. Both need no model file, no tokenizer, no weights —
//! which is the point: this round's whole claim is that a pinned 10 MB archive set links and answers,
//! and nothing here pretends to be inference (§14).
//!
//! With `local-model` **off**, the one test in the second half of this file prints `NOT COVERAGE` and
//! passes: an absent feature must not look like a failure, but it must not look like coverage either
//! (F18's rule). With the feature **on** there is no way to skip these tests — no env var, no
//! `#[ignore]`, no artifact check. If they are not running, something is broken, and that is the
//! signal this file exists to produce.

/// Everything below is the claim itself: compiled only when the runtime is compiled in.
#[cfg(feature = "local-model")]
mod link_proof {
    use std::ffi::CStr;
    use std::os::raw::c_char;

    use np_core::llama_ffi;

    /// Read the pinned tag out of `third_party/llama-cpp.toml` rather than repeating it here: the pin
    /// file is the one place that version is written (§14), and a test that hardcoded `0.8` would keep
    /// passing after a re-pin that forgot to update it.
    fn pinned_version_prefix() -> String {
        let pin = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../third_party/llama-cpp.toml"
        );
        let text = std::fs::read_to_string(pin).expect("the pin file is part of the repository");
        // Same subset build.rs parses: a quoted string on one line.
        let tag = text
            .lines()
            .filter_map(|line| line.trim().strip_prefix("tag = \""))
            .find_map(|rest| rest.strip_suffix('"'))
            .expect("the pin file declares `tag`");
        let numbers = tag.trim_start_matches('v');
        let mut parts = numbers.split('.');
        let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
            panic!("pin tag {tag} is not X.Y.Z");
        };
        format!("{major}.{minor}.")
    }

    /// C string to Rust String, with the two failures that matter stated separately: a null pointer
    /// (the function declined to answer) and non-UTF-8 bytes (we would be misreading the library).
    fn owned_c_string(pointer: *const c_char, what: &str) -> String {
        assert!(
            !pointer.is_null(),
            "llama.cpp returned a null pointer for {what}"
        );
        // SAFETY: llama.cpp's own two static-string accessors (`src/llama.cpp`) hand back NUL-terminated
        // string literals / a `static std::string` that lives past this call. Neither is freed by us.
        let text = unsafe { CStr::from_ptr(pointer) };
        text.to_str()
            .unwrap_or_else(|e| panic!("{what} is not valid UTF-8: {e}"))
            .to_string()
    }

    /// The version string is the pinned one. In v0.8.0 `llama_version()` returns `const char *` (older
    /// releases returned an integer build number), so "non-zero" here means a real pointer whose string
    /// carries the pinned major.minor — a link to any other llama.cpp fails this.
    #[test]
    fn version_is_the_pinned_llama_cpp() {
        // SAFETY: no arguments, returns a pointer to static storage owned by llama.cpp (§ module docs).
        let version = owned_c_string(unsafe { llama_ffi::llama_version() }, "llama_version()");
        let prefix = pinned_version_prefix();
        assert!(
            !version.is_empty(),
            "llama_version() answered with an empty string"
        );
        assert!(
            version.starts_with(&prefix),
            "linked llama_version() = {version:?}, which does not start with the pinned prefix {prefix:?} \
             (third_party/llama-cpp.toml) — so this binary is not linked against the tree we pinned"
        );
    }

    /// The system-info entry point resolves and answers in UTF-8. It also proves the ggml backend
    /// registry is inside the link, since the string is built by walking it.
    #[test]
    fn system_info_is_a_readable_string() {
        // SAFETY: returns a pointer into a function-local `static std::string` in llama.cpp, valid until
        // the next call to this same function — and this test calls it once, then copies.
        let info = owned_c_string(
            unsafe { llama_ffi::llama_print_system_info() },
            "llama_print_system_info()",
        );
        assert!(
            !info.trim().is_empty(),
            "llama_print_system_info() answered with whitespace"
        );
        // Measured on this host from the pinned tree: `CPU : SSE3 = 1 | SSSE3 = 1 | LLAMAFILE = 1 |
        // OPENMP = 1 | REPACK = 1`. The tree is sha256-pinned, so naming content here cannot drift.
        assert!(
            info.contains("CPU"),
            "system info did not describe a CPU backend: {info:?}"
        );
    }

    /// A struct returned **by value** from C++ lands where our `#[repr(C)]` mirror says it does.
    ///
    /// The unit tests in `src/llama_ffi.rs` compare our layout with **constants measured from** the header
    /// (the probe is published there); only this test can show that the compiled C++ agrees. If a field were missing or misordered, `n_gpu_layers` would not be
    /// -1 at the offset we read it from (`src/llama-model.cpp`: `/*.n_gpu_layers =*/ -1`, and
    /// `use_extra_bufts = true`). No model, no file, no allocation — a plain return value.
    #[test]
    fn model_params_cross_the_boundary_in_our_layout() {
        // SAFETY: returns a value-type struct of plain data; nothing is allocated and nothing is freed.
        let params = unsafe { llama_ffi::llama_model_default_params() };
        assert!(
            params.devices.is_null(),
            "default devices should be AUTO (null)"
        );
        assert_eq!(
            params.n_gpu_layers, -1,
            "llama.cpp's default n_gpu_layers; a wrong read means our struct layout is wrong"
        );
        assert!(params.use_extra_bufts);
        assert!(!params.check_tensors);
        assert!(!params.vocab_only);
        assert!(!params.no_alloc);
        assert!(params.progress_callback.is_none());
        assert!(params.kv_overrides.is_null());
    }

    /// The second struct returned by value, and the one R6b1 actually configures. `llama_model_default_params`
    /// alone left two of the three mirrored structs with no runtime evidence at all (R6b0 review P2-6); this is
    /// the cheapest thing that changes that, and it still needs no model, no file and no inference.
    ///
    /// What it proves is a field-order claim: `n_ctx` and `n_batch` are read at offsets 0 and 4, `n_threads`
    /// where our mirror says 28, `embeddings` at 153-ish among the tail bools. A shifted struct would show up
    /// as a zero context size or a garbage thread count, not as a subtle difference.
    #[test]
    fn context_params_cross_the_boundary_in_our_layout() {
        // SAFETY: a value-type struct of plain data; nothing is allocated, nothing is freed.
        let params = unsafe { llama_ffi::llama_context_default_params() };
        assert!(
            params.n_ctx > 0,
            "llama.cpp defaults n_ctx to a non-zero size; reading 0 here means we are looking at the wrong \
             four bytes"
        );
        assert!(
            params.n_batch > 0,
            "n_batch is at offset 4 in the pinned header and must be non-zero by default"
        );
        assert!(
            params.n_threads > 0,
            "n_threads (offset 28) defaults to a thread count, not zero or noise"
        );
        assert!(
            params.cb_eval.is_none(),
            "the default context installs no eval callback — and reading this field is what the three-parameter \
             `cb_eval` declaration is for"
        );
        assert!(
            params.abort_callback.is_none(),
            "the default context installs no abort callback"
        );
        assert!(
            !params.embeddings,
            "embeddings are opt-in; R6b1 sets this, the default is false"
        );
    }

    /// The third struct, and the only one of the three that owns memory on the C side. It also carries the
    /// fact R6b1 is most likely to get wrong, discovered while writing this test: `llama_batch_init`'s first
    /// argument is a **capacity**, and `src/llama-batch.cpp` returns `n_tokens = 0`. An assertion that "init(4)
    /// gives n_tokens 4" was written here first and failed with `left: 0` — the test earns its keep by pinning
    /// the real behaviour rather than the guess.
    ///
    /// Writing through the returned pointers and reading the bytes back is the layout proof: if `token` were
    /// our version of `embd`, or the offset were wrong, we would be writing outside llama.cpp's allocation
    /// (and under a sanitiser, that is a crash; here, the value still has to survive the round trip through the
    /// pointer C handed us). Freed with `llama_batch_free`, which v0.8.0 takes **by value** — the only place
    /// the suite exercises that ownership rule end to end. No model, no inference.
    #[test]
    fn batch_init_hands_back_the_layout_we_declare() {
        // SAFETY: an allocation owned by the returned struct, freed at the end of this function by llama.cpp.
        let batch = unsafe { llama_ffi::llama_batch_init(4, 0, 1) };
        assert_eq!(
            batch.n_tokens, 0,
            "a freshly initialised batch is empty — the argument was a capacity"
        );
        assert!(
            !batch.token.is_null(),
            "embd=0 means token-driven: the token array must be allocated"
        );
        assert!(
            batch.embd.is_null(),
            "with embd=0 no embedding array is allocated — a non-null here means we read the wrong pointer \
             field"
        );
        assert!(
            !batch.pos.is_null() && !batch.n_seq_id.is_null() && !batch.seq_id.is_null(),
            "the remaining arrays are allocated for the requested capacity"
        );
        // SAFETY: `llama_batch_init(4, 0, 1)` mallocs 4 tokens, 4 positions, 4 counts and 4 sequence arrays of
        // 1 entry each; every write below is inside index 0 of an allocation we own until the free.
        unsafe {
            *batch.token.add(0) = 4242;
            *batch.n_seq_id.add(0) = 1;
            // `seq_id` is `llama_seq_id **`: an array of per-token arrays. Both levels get exercised.
            let first_token_sequences = *batch.seq_id.add(0);
            assert!(
                !first_token_sequences.is_null(),
                "llama.cpp allocates one sequence array per token slot"
            );
            *first_token_sequences.add(0) = 3;
            assert_eq!(
                *batch.token, 4242,
                "the token pointer in our mirror points at llama.cpp's own array"
            );
            assert_eq!(*batch.n_seq_id.add(0), 1);
            assert_eq!(
                *first_token_sequences, 3,
                "reading back through both pointer levels proves `seq_id` is where the header says"
            );
        }
        // SAFETY: this batch came from `llama_batch_init`, so freeing it is correct (never a
        // `llama_batch_get_one` batch, which borrows the caller's arrays).
        unsafe { llama_ffi::llama_batch_free(batch) };
    }
}

/// The loud half of "a feature-off run is not coverage". This is the only test in this file that exists
/// without `local-model`, and it says so instead of passing quietly.
#[cfg(not(feature = "local-model"))]
#[test]
fn local_model_feature_is_not_built() {
    println!("NOT COVERAGE: np-core was built without `--features local-model`, so no llama.cpp symbol was \
              linked or called. Re-run with: cargo test -p np-core --features local-model --test local_runtime");
}
