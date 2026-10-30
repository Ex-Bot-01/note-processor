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
    /// The unit tests in `src/llama_ffi.rs` compare our layout with the header; only this test can show
    /// that the compiled C++ agrees. If a field were missing or misordered, `n_gpu_layers` would not be
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
}

/// The loud half of "a feature-off run is not coverage". This is the only test in this file that exists
/// without `local-model`, and it says so instead of passing quietly.
#[cfg(not(feature = "local-model"))]
#[test]
fn local_model_feature_is_not_built() {
    println!("NOT COVERAGE: np-core was built without `--features local-model`, so no llama.cpp symbol was \
              linked or called. Re-run with: cargo test -p np-core --features local-model --test local_runtime");
}
