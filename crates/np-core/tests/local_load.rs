//! R6b1's claim about a **real** GGUF: llama.cpp v0.8.0 loads `embeddinggemma-2-Q8_0.gguf` through our own
//! FFI, and the numbers it answers with are the numbers in that file's header.
//!
//! **Three** cases need nothing but the feature and run on every `local-model` CI row: two refusals that never
//! reach a model file (an absent path, a directory), and one that reaches llama.cpp with bytes that mean nothing.
//! Everything else is **artifact-gated** on `NP_TEST_MODEL_PATH` — four cases: the real-file facts, the
//! wrong-declared-width refusal, the three-round load/drop cycle, and the truncated-artifact refusal — and when
//! the variable is unset each prints `NOT COVERAGE` and passes. F18's rule: an absent 310 MB file must not look
//! like a failure, and must not look like coverage either. There is no `#[ignore]` here, because an ignored test
//! is exactly the silent skip that rule exists to forbid.
//!
//! Counted plainly because the CI row is where this distinction matters: on GitHub there is no artifact, so the
//! width refusal and every real number are measured locally, not there.
//!
//! ```text
//! NP_TEST_MODEL_PATH=/path/to/embeddinggemma-2-Q8_0.gguf \
//!   cargo test -p np-core --features local-model --test local_load -- --nocapture
//! ```
//!
//! Two things these tests deliberately do **not** do. They assert no bound on memory: peak RSS is printed, not
//! compared, because a threshold on a page-cache-dependent number is a flake factory and the deliverable is
//! the measurement (§14 wants the RAM cost of opening the model, which is a figure a human reads). And they
//! stop at the facts — no tokenizing, no decode, no pooling arithmetic, no vectors. That is R6b2's claim, and
//! a test that quietly started decoding here would make this round's narrower claim unverifiable.

/// The artifact-gated claims, plus the refusals that need only the feature.
#[cfg(feature = "local-model")]
mod loading {
    use std::error::Error;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};
    use std::time::Instant;

    use np_core::embed::EmbedError;
    use np_core::local_model::{self, ModelFacts, Pooling};

    /// The width `embeddinggemma-2-Q8_0.gguf` answers with (`gemma-embedding2.embedding_length_out = 768`,
    /// measured from the file's own header and re-read here through `llama_model_n_embd_out`). One spelling of
    /// it in this file, shared by the load that must succeed and the declaration that must be refused.
    const DIM: usize = 768;

    /// The artifact under test, or `None` when the runner has no model file — which is the ordinary state on a
    /// CI runner, and every artifact-gated caller turns it into a printed `NOT COVERAGE`.
    fn artifact() -> Option<PathBuf> {
        std::env::var_os("NP_TEST_MODEL_PATH").map(PathBuf::from)
    }

    macro_rules! artifact_or_not_coverage {
        () => {
            match artifact() {
                Some(path) => path,
                None => {
                    println!(
                        "NOT COVERAGE: NP_TEST_MODEL_PATH is unset, so no real GGUF was loaded by this case. \
                         Re-run with it pointing at embeddinggemma-2-Q8_0.gguf (sha256 2188ac1d…) to load one."
                    );
                    return;
                }
            }
        };
    }

    /// One artifact load at a time. libtest runs tests on threads, `VmHWM` is process-wide, and two loads in
    /// flight would let each case report the other's peak — which is worse than reporting nothing, because the
    /// number in §14 is supposed to answer "what does opening this model cost".
    ///
    /// A poisoned lock is taken anyway: the poison came from another test's assertion failure, and refusing to
    /// run here would turn one red case into a cascade of missing coverage.
    fn artifact_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One `kB` field of `/proc/self/status` — `"VmHWM"` is the peak resident set (what "what did opening the
    /// model cost" means when the memory is handed back afterwards) and `"VmRSS"` is what is resident right now.
    ///
    /// `None` if `/proc` has no such line. Nothing here asserts a magnitude against either — see this file's
    /// header: the deliverable is the figure, not a bound.
    fn status_kb(field: &str) -> Option<u64> {
        let status = fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line
                .strip_prefix(field)
                .and_then(|rest| rest.strip_prefix(':'))
            {
                return rest.trim().trim_end_matches(" kB").trim().parse().ok();
            }
        }
        None
    }

    /// The measured header facts, read back through [`local_model::load`] — i.e. through `llama.h`'s own
    /// accessors in the tree we pinned, not by re-parsing the file. Every number below came from the artifact's
    /// KV section on 2026-10-30; a mismatch means the file at `NP_TEST_MODEL_PATH` is **not** that artifact,
    /// which is what the messages say rather than something subtler.
    #[test]
    fn real_artifact_answers_the_numbers_in_its_own_header() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();

        let before = status_kb("VmRSS");
        let started = Instant::now();
        let model = local_model::load(&path, DIM).unwrap_or_else(|problem| {
            panic!("loading {} should succeed: {problem}", path.display())
        });
        let seconds = started.elapsed().as_secs_f64();
        let facts = model.facts();
        let resident = status_kb("VmRSS");
        let peak = status_kb("VmHWM").expect(
            "/proc/self/status is part of the Linux promise `build.rs` makes for this feature",
        );

        println!(
            "MODEL LOAD: {} in {:.3} s, RSS {:?} kB before -> {:?} kB after, peak {} kB",
            path.display(),
            seconds,
            before,
            resident,
            peak
        );
        println!("MODEL FACTS: {facts:?}");

        assert_eq!(
            facts.n_embd_out, 768,
            "`gemma-embedding2.embedding_length_out` is 768 in the pinned artifact and §5 stores that width"
        );
        assert_eq!(
            facts.n_embd_inp, 512,
            "the model runs at 512 internally; reading the *out* accessor's number here would mean the two \
             declarations are swapped in our mirror"
        );
        assert_eq!(
            facts.n_ctx_train, 262_144,
            "`gemma-embedding2.context_length` is 262 144 — above `MAX_EMBEDDING_TOKENS`, whose gap §5 closed as decision B"
        );
        assert!(
            facts.n_vocab == 262_144,
            "the pinned artifact declares a 262 144-entry vocabulary, got {}",
            facts.n_vocab
        );
        // Pinned exactly rather than as "> 200 000", and with this note attached on purpose: this artifact answers
        // **262 144 for both `n_ctx_train` and `n_vocab`**, so no assertion on either value can catch the two
        // accessors being swapped — even an exact one. Catching that needs a second artifact whose two numbers
        // differ, which is a reason to keep `crates/np-core/tests/probes/model_load_probe.c` around: it prints both
        // from C, so a Rust-side swap shows up as a disagreement between the two programs rather than as a pass.
        assert_eq!(
            facts.pooling,
            Pooling::Mean,
            "the artifact declares `pooling_type = 1` (MEAN) and we must not override it — anything else here \
             means our context params overrode the file"
        );
        assert!(
            facts.add_bos && facts.add_eos,
            "both specials are added (`add_bos_token`/`add_eos_token` are true), so R6b2's token count must \
             budget two, not one; got add_bos={} add_eos={}",
            facts.add_bos,
            facts.add_eos
        );
    }

    /// The standing debt this closes: a declared width that disagrees with the artifact was previously refused
    /// only against the *row* (`LocalModelProvider::new`), never against the file. Declaring 512 against a
    /// model that answers 768 must be a named, typed refusal carrying both numbers — and it must happen before
    /// a context is built, so the refusal costs one load rather than a load plus a context — and this artifact has
    /// no KV cache at all (`local_model`'s module header), which is why "context" is the honest noun here.
    #[test]
    fn declaring_the_wrong_width_is_refused_naming_both_numbers() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();

        let problem = local_model::load(&path, 512)
            .expect_err("the artifact answers 768, so a run configured for 512 cannot be served");
        let message = problem.to_string();
        println!("WRONG WIDTH REFUSAL: {message}");

        match problem {
            EmbedError::ProviderDimMismatch {
                configured,
                provider,
            } => {
                assert_eq!(configured, 512, "what the caller declared");
                assert_eq!(provider, 768, "what the artifact answered, verbatim");
            }
            other => panic!("expected the declared-vs-actual variant, got {other:?}"),
        }
        assert!(
            message.contains("512") && message.contains("768"),
            "the refusal has to name both numbers or a reader cannot act on it: {message}"
        );
    }

    /// A path that is absent is a typed refusal naming that path — not a panic, and not the zero-vector batch
    /// §5 forbids. Nothing here reaches llama.cpp: the stat refuses first, which is why this case runs on every
    /// `local-model` CI row with no artifact in sight.
    #[test]
    fn an_absent_artifact_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("a temp dir to hold nothing");
        let path = dir.path().join("never-installed.gguf");

        let problem = local_model::load(&path, DIM)
            .expect_err("nothing exists at that path, so loading it must fail");
        let message = problem.to_string();

        match problem {
            EmbedError::ArtifactUnreadable { path: named, .. } => {
                assert_eq!(
                    named, path,
                    "the refusal names the file the caller asked for"
                );
            }
            other => panic!("expected an unreadable-artifact refusal, got {other:?}"),
        }
        assert!(
            message.contains("never-installed.gguf"),
            "the message has to carry the path: {message}"
        );
    }

    /// A directory is not a small unreadable artifact — the rule `models::install` and
    /// `LocalModelProvider::new` already hold, kept one variant here too. The point of the case is that the OS
    /// opens directories happily (my C probe measured `gguf_init_from_reader: failed to read magic` for exactly
    /// this path), so "it opened" must never become "it loaded".
    #[test]
    fn a_directory_is_not_an_artifact() {
        let dir = tempfile::tempdir().expect("a temp dir to be mistaken for a model");

        let problem =
            local_model::load(dir.path(), DIM).expect_err("a directory is not a loadable artifact");
        let message = problem.to_string();

        match problem {
            EmbedError::ArtifactUnreadable { .. } => {}
            other => panic!("expected an unreadable-artifact refusal, got {other:?}"),
        }
        assert!(
            message.contains("not a regular file"),
            "the refusal should say what is wrong with the path, got: {message}"
        );
    }

    /// Bytes that exist and mean nothing are the **other** refusal: llama.cpp returns null, and that becomes
    /// [`EmbedError::ArtifactNotLoadable`] naming the file — never a panic inside C++, and never a model object
    /// whose facts are zeros. This is the only case in the file that reaches `llama_model_load_from_file`
    /// without an artifact, so CI's feature row exercises refusal #2 even with no model on the runner.
    ///
    /// llama.cpp writes its own diagnostics to stderr for this attempt (`gguf_init_from_reader: invalid magic
    /// characters`); that is its logger, not a panic, and this round does not install a log callback.
    #[test]
    fn bytes_that_are_not_a_gguf_are_refused_by_name() {
        let dir = tempfile::tempdir().expect("a temp dir to hold the impostor");
        let path = dir.path().join("not-a-model.gguf");
        let mut file = fs::File::create(&path).expect("a writable temp file");
        file.write_all(b"this is not a gguf file at all, just some bytes\n")
            .expect("the impostor's bytes");
        drop(file);

        let problem = local_model::load(&path, DIM)
            .expect_err("bytes with the wrong magic cannot be a model");
        let message = problem.to_string();

        match &problem {
            EmbedError::ArtifactNotLoadable { path: named } => {
                assert_eq!(named, &path, "the refusal names the file that was refused");
            }
            other => panic!("expected a not-loadable refusal, got {other:?}"),
        }
        assert!(
            message.contains("not-a-model.gguf"),
            "the message has to carry the path: {message}"
        );
        // The source of a load refusal is llama.cpp's null return, which has no `io::Error` behind it — if that
        // ever changes, this line is where the two refusals drifted.
        assert!(
            problem.source().is_none(),
            "the runtime's refusal carries our words, not a chained OS error"
        );
    }

    /// An interrupted download or a moved `.part` file produces exactly this shape: valid GGUF metadata, tensor
    /// data that runs past the end of the file. Measured on this host (2026-10-30) because `load`'s doc promises the
    /// refusal and nothing tested it: llama.cpp logs `tensor 'token_embd.weight' data is not within the file bounds`
    /// and **returns null** — it does not `GGML_ABORT`, so the process survives and the caller gets a typed refusal.
    ///
    /// Costs 128 MiB of temp space for the duration of the case, on the same filesystem as the artifact's own
    /// directory when that is writable, else in the temp dir. Deleted before returning.
    #[test]
    fn an_incomplete_artifact_is_refused_rather_than_aborting() {
        let source = artifact_or_not_coverage!();
        let _serial = artifact_lock();

        let dir = tempfile::tempdir().expect("a temp dir for the truncated copy");
        let truncated = dir.path().join("truncated.gguf");
        const KEEP: u64 = 128 * 1024 * 1024;

        let mut from = fs::File::open(&source).expect("the artifact opens for reading");
        let mut to = fs::File::create(&truncated).expect("a writable temp file");
        // `&mut File` is itself a `Read`, so `take` needs no `by_ref` — whose name is ambiguous here because
        // both `Read` and `Write` are in scope for `File`.
        let mut head = (&mut from).take(KEEP);
        io::copy(&mut head, &mut to).expect("128 MiB copies");
        drop(to);

        let problem = local_model::load(&truncated, DIM)
            .expect_err("a file whose tensors run past its end must be refused, not loaded");
        match &problem {
            // The point of the case: refusal #2 (reached and meant nothing), not a panic and not a hang.
            EmbedError::ArtifactNotLoadable { path } => {
                assert_eq!(path, &truncated, "the refusal names the incomplete file");
            }
            other => {
                panic!("expected a not-loadable refusal for a truncated artifact, got {other:?}")
            }
        }

        fs::remove_file(&truncated).expect("the truncated copy is deleted");
    }

    /// Load and release the real artifact three times in one process, asserting the same facts each round.
    ///
    /// This is the crash test for ownership: a double free or a use-after-free in `LoadedModel`'s `Drop`, or a
    /// model freed while its context still points at it, shows up here as SIGABRT or a segfault rather than as
    /// a theory about drop order.
    ///
    /// **What it cannot do is detect a leak.** A `Drop` that freed nothing would print three identical fact sets,
    /// grow the `ROUND` lines, and pass: three leaked loads cost ~1.2 GB against ~6 GB available, and most of what
    /// a load touches is mapped weights, i.e. reclaimable. §14's rule against memory thresholds (a bound on a
    /// page-cache-dependent number is a flake factory) means the growth is printed for a human to read, not asserted.
    /// Crash detection and idempotent facts are the coverage; say so rather than implying more (R6b1 review P2-1).
    #[test]
    fn loading_and_dropping_three_times_costs_the_same_each_time() {
        let path = artifact_or_not_coverage!();
        let _serial = artifact_lock();

        let mut rounds: Vec<ModelFacts> = Vec::new();
        for round in 1..=3 {
            let started = Instant::now();
            let model = local_model::load(&path, DIM).unwrap_or_else(|problem| {
                panic!("round {round}: loading should succeed: {problem}")
            });
            println!(
                "ROUND {round}: loaded in {:.3} s, RSS {:?} kB with the model open, peak so far {:?} kB",
                started.elapsed().as_secs_f64(),
                status_kb("VmRSS"),
                status_kb("VmHWM")
            );
            rounds.push(model.facts());
            // Explicit, not end-of-scope: the free is what is under test, so it has to happen here.
            drop(model);
        }

        for (index, facts) in rounds.iter().enumerate() {
            assert_eq!(
                facts,
                &rounds[0],
                "round {} answered {:?}, which differs from round 1 — a handle reused after free",
                index + 1,
                facts
            );
        }
    }
}

/// The loud half of "a feature-off run is not coverage" — the same rule `tests/local_runtime.rs` states for
/// the link proof. Without `local-model` there is no loader to call and no archive to call it with, so this
/// prints instead of pretending the artifact was exercised.
#[cfg(not(feature = "local-model"))]
#[test]
fn local_model_feature_is_not_built() {
    println!("NOT COVERAGE: np-core was built without `--features local-model`, so no GGUF was loaded and no \
              llama.cpp symbol was called. Re-run with: cargo test -p np-core --features local-model \
              --test local_load");
}
