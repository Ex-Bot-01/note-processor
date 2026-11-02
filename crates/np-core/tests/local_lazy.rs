//! The **lazy** local provider: the one `np embed` resolves to when the runtime is compiled in
//! (`local_model::LazyLocalEmbedder`, R6c2).
//!
//! Nothing here needs the 310 MB artifact. That is the point of this file: every claim is about what happens
//! *before* or *instead of* a load, and each one runs on any machine in the default `--features local-model` test
//! row — no `NOT COVERAGE`, no skip, because there is nothing to be missing. The artifact-gated half of R6c2 (a
//! real vector reaching `embeddings`) lives in `np-cli`'s argv suite, where the command under test is the one that
//! does the wiring.
//!
//! Three rules the cases are built around:
//!
//! * **Nothing is resident until text arrives.** §14's argument for laziness is that an up-to-date workspace must
//!   not pay ~420 MiB for a run that embeds nothing, so `loaded()` is asserted rather than narrated.
//! * **The two refusals that need no model are taken eagerly**, in the same order as
//!   [`np_core::embed::LocalModelProvider::new`] takes them, because §6's *artifact absent* arm has to exist
//!   before anyone asks whether a chunk is owed (R6c1 D-2).
//! * **A failed load stays typed.** The provider mirrors its refusal so it can hand the same one out twice; the
//!   second call must still be `ArtifactNotLoadable`, not a string.

/// Everything in this file needs the runtime compiled in — including the cases that never load anything, because
/// the type under test does not exist otherwise.
#[cfg(feature = "local-model")]
mod lazy {
    use std::fs;
    use std::io::Write;

    use np_core::db::ModelRow;
    use np_core::embed::{EmbedError, Embedder};
    use np_core::local_model::LazyLocalEmbedder;

    /// The width this file's rows declare and the runs ask for. No artifact is read, so the number only has to be
    /// self-consistent — 768 because that is what the real row looks like.
    const DIM: usize = 768;

    fn row(dim: i64) -> ModelRow {
        ModelRow {
            id: "embeddinggemma-2".to_string(),
            dim,
            sha256: None,
            prefix_doc: "task: search result | query: ".to_string(),
            prefix_query: "query: ".to_string(),
            installed_at: 1_730_000_000,
            verified_size: None,
            verified_mtime: None,
        }
    }

    /// A file that exists and is a regular file, so the artifact check passes and only a load could object.
    fn impostor_artifact(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("not-a-model.gguf");
        let mut file = fs::File::create(&path).expect("test setup: a writable artifact path");
        file.write_all(b"this is not a gguf file at all, just some bytes\n")
            .expect("test setup: the impostor's bytes");
        path
    }

    #[test]
    fn a_missing_artifact_is_refused_at_construction_not_at_the_first_batch() {
        let dir = tempfile::tempdir().expect("a temp dir to look for an artifact in");
        let absent = dir.path().join("models").join("embeddinggemma-2");

        let problem = LazyLocalEmbedder::new(
            &row(DIM as i64),
            &absent,
            DIM,
            1_024,
            np_core::config::ThreadCount::Auto,
        )
        .expect_err("a row whose bytes are not there cannot produce a provider");

        // This variant is what `np-cli` turns into §6's arm (iii). Were it raised later — inside the first
        // `embed`, as every other load failure now is — a workspace with no artifact would report a *failure*
        // instead of the actionable "install it" sentence, which is the D-2 defect in a new place.
        match &problem {
            EmbedError::ArtifactUnreadable { path, .. } => {
                assert_eq!(path, &absent, "the refusal names the file it looked for");
            }
            other => panic!("expected the artifact-absent refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_row_of_another_width_is_refused_before_the_file_is_touched() {
        let dir = tempfile::tempdir().expect("a temp dir with no artifact in it");
        // Deliberately absent: the width refusal must arrive first, so the missing file is never statted. The
        // same ordering argument `LocalModelProvider::new` documents — do not spend an operation to learn what
        // the row already said.
        let absent = dir.path().join("embeddinggemma-2");

        let problem = LazyLocalEmbedder::new(
            &row(512),
            &absent,
            DIM,
            1_024,
            np_core::config::ThreadCount::Auto,
        )
        .expect_err("a row half the width of this run is a mismatch");
        match problem {
            EmbedError::ProviderDimMismatch {
                configured,
                provider,
            } => {
                assert_eq!(configured, DIM, "the width this run stores at");
                assert_eq!(provider, 512, "the width the row declares, verbatim");
            }
            other => panic!("expected the width refusal, got {other:?}"),
        }
    }

    #[test]
    fn identity_answers_from_the_row_while_nothing_is_resident() {
        let dir = tempfile::tempdir().expect("a temp dir to hold the impostor");
        let artifact = impostor_artifact(dir.path());
        let embedder = LazyLocalEmbedder::new(
            &row(DIM as i64),
            &artifact,
            DIM,
            1_024,
            np_core::config::ThreadCount::Auto,
        )
        .expect("the two eager checks pass: right width, real file");

        // What `embed_run` needs in order to work out what is owed. Both answers come from the row and the
        // config, so a workspace that owes nothing never asks llama.cpp for anything — §14's whole argument, as
        // an assertion instead of a sentence.
        assert_eq!(embedder.model_id(), "embeddinggemma-2");
        assert_eq!(embedder.dim(), DIM);
        assert!(
            !embedder.loaded(),
            "answering identity questions must not have loaded 310 MB of weights"
        );
    }

    #[test]
    fn a_failed_load_is_remembered_and_stays_typed() {
        let dir = tempfile::tempdir().expect("a temp dir to hold the impostor");
        let artifact = impostor_artifact(dir.path());
        let embedder = LazyLocalEmbedder::new(
            &row(DIM as i64),
            &artifact,
            DIM,
            1_024,
            np_core::config::ThreadCount::Auto,
        )
        .expect("the eager checks pass; only llama.cpp gets to object now");

        let first = embedder
            .embed(&["anything at all"])
            .expect_err("bytes with the wrong magic cannot be a model");
        match &first {
            EmbedError::ArtifactNotLoadable { path } => {
                assert_eq!(path, &artifact, "the refusal names the impostor");
            }
            other => panic!("expected the not-loadable refusal, got {other:?}"),
        }
        assert!(
            !embedder.loaded(),
            "a failed load leaves nothing resident — §14's peak is for models that loaded"
        );

        // The reason `LoadFailure` exists at all: a run with several batches must not read 310 MB per batch on
        // its way to arm (iv), and the second refusal has to keep its variant, because arm (iv)'s sentence is
        // built from the typed reason.
        let second = embedder
            .embed(&["and again"])
            .expect_err("the remembered failure repeats");
        match second {
            EmbedError::ArtifactNotLoadable { path } => assert_eq!(path, artifact),
            other => panic!("the mirrored refusal lost its type: {other:?}"),
        }
    }
    /// The worker plan a lazy provider reports: the config value resolved against this process's CPU allowance, and
    /// nothing loaded to work it out.
    ///
    /// This is the fence behind `np embed`'s `threads` line (`docs/design-m2-thread-policy.md` §4): the number a user
    /// reads has to be the number that provider will hand llama.cpp, decided once, from one reading of the machine. It
    /// runs without the artifact on purpose — resolving a count must not cost 310 MB of weights, and a case that needed
    /// the model would not be able to say so.
    #[test]
    fn the_worker_plan_is_the_config_resolved_against_the_machine_once() {
        let dir = tempfile::tempdir().expect("a temp dir to hold the impostor");
        let artifact = impostor_artifact(dir.path());
        let available = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);

        let auto = LazyLocalEmbedder::new(
            &row(DIM as i64),
            &artifact,
            DIM,
            1_024,
            np_core::config::ThreadCount::Auto,
        )
        .expect("the eager checks pass");
        let plan = auto.worker_plan();
        // The expected count is the rule written out **here** rather than read back from `resolve`, so this case fails
        // when the rule moves. Comparing the provider against the same function it calls could only ever catch a field
        // being wired to the wrong source — useful, but not what the message claims.
        let expected = if available == 0 {
            1
        } else {
            available.min(available.min(4).max(available * 4 / 5))
        };
        assert_eq!(
            plan.count, expected,
            "the report must state the rule's answer for this machine ({available} CPUs), not a number              remembered from elsewhere"
        );
        assert_eq!(
            plan.available, available,
            "and the denominator it was derived from"
        );
        assert_eq!(plan.configured, "auto");
        assert!(
            plan.count >= 1 && plan.count <= available.max(1),
            "a plan that oversubscribes or starves this machine: {plan:?}"
        );
        assert!(
            !auto.loaded(),
            "working out a worker count must not have loaded the model"
        );

        // An explicit count survives the trip unchanged, and says it came from the operator rather than the rule —
        // including counts the rule would never choose, which is §5's point.
        let three = std::num::NonZeroU32::new(3).expect("test setup: 3 is non-zero");
        let explicit = LazyLocalEmbedder::new(
            &row(DIM as i64),
            &artifact,
            DIM,
            1_024,
            np_core::config::ThreadCount::Workers(three),
        )
        .expect("the eager checks pass");
        let plan = explicit.worker_plan();
        assert_eq!(plan.count, 3, "an explicit count is honoured exactly");
        assert_eq!(plan.configured, "explicit");

        // **Nothing is resident yet, so there is no answer to give.** The accessor reports the loaded model's reality,
        // not the plan — a `Some(3)` here would be the report quoting itself.
        assert_eq!(
            explicit.loaded_worker_count(),
            None,
            "a provider that has not loaded cannot report what the context answered"
        );
    }

    /// The artifact a real load can read, or `None` when the environment says it is absent.
    ///
    /// The standing gate (F18): a case that needs 310 MB of weights prints `NOT COVERAGE` and passes rather than failing
    /// a machine that legitimately does not have them — "skipped" and "measured nothing" are the same sentence here, and
    /// CI counts these prints (`docs/CI.md`, and the feature row's floor).
    fn artifact_path() -> Option<std::path::PathBuf> {
        std::env::var_os("NP_TEST_MODEL_PATH").map(std::path::PathBuf::from)
    }

    /// The configured count reaches llama.cpp **through this provider**, not just through the raw loader.
    ///
    /// The plan and the load read the same field, so a break between them is invisible to anything that never loads: the
    /// report would say twelve while the context ran four, and every number in `np embed`'s output would still look
    /// plausible. Artifact-gated for the standing reason (F18) — this needs real weights to have a context to ask.
    // The whole file is `#![cfg(feature = "local-model")]`, so no per-case gate is needed on top of it.
    #[test]
    fn the_configured_count_is_the_count_the_context_runs_with() {
        let Some(path) = artifact_path() else {
            println!("NOT COVERAGE: NP_TEST_MODEL_PATH is unset, so nothing was loaded and the count went unverified.");
            return;
        };
        let two = std::num::NonZeroU32::new(2).expect("test setup: 2 is non-zero");
        let embedder = LazyLocalEmbedder::new(
            &row(DIM as i64),
            &path,
            DIM,
            1_024,
            np_core::config::ThreadCount::Workers(two),
        )
        .expect("a real artifact and a matching width pass the eager checks");
        assert_eq!(embedder.loaded_worker_count(), None, "nothing loaded yet");

        embedder
            .embed(&["a sentence the model can read"])
            .expect("the artifact loads and answers");
        assert_eq!(
            embedder.loaded_worker_count(),
            Some(2),
            "the provider was told two workers and the context answered otherwise"
        );
    }
}
