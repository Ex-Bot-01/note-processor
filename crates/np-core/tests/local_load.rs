//! R6b1's claim about a **real** GGUF: llama.cpp v0.8.0 loads `embeddinggemma-2-Q8_0.gguf` through our own
//! FFI, and the numbers it answers with are the numbers in that file's header.
//!
//! Four of these tests need nothing but the feature and run on every `local-model` CI row: two refusals that
//! never reach a model file (an absent path, a directory), one that reaches llama.cpp with bytes that mean
//! nothing, and the wrong-declared-width case once an artifact is present. The rest are **artifact-gated** on
//! `NP_TEST_MODEL_PATH`, and when that variable is unset they print `NOT COVERAGE` and pass — F18's rule: an
//! absent 310 MB file must not look like a failure, and must not look like coverage either. There is no
//! `#[ignore]` here, because an ignored test is exactly the silent skip that rule exists to forbid.
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
    use std::io::Write;
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
            "`gemma-embedding2.context_length` is 262 144 — above `MAX_EMBEDDING_TOKENS`, which §5 keeps open"
        );
        assert!(
            facts.n_vocab > 200_000,
            "the pinned artifact declares a 262 144-entry vocabulary, got {}",
            facts.n_vocab
        );
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
    /// a context is built, so the refusal costs one load rather than a load plus a KV cache.
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

    /// Load and release the real artifact three times in one process, asserting the same facts each round.
    ///
    /// This is the crash test for ownership: a double free or a use-after-free in `LoadedModel`'s `Drop`, or a
    /// model freed while its context still points at it, shows up here as SIGABRT or a segfault rather than as
    /// a theory about drop order. Repeated loads also answer the practical question §3's "one load per process"
    /// rule is judged against — a leak would make the second or third round fail with an allocation refusal.
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
