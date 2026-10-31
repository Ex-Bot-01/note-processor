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

        let problem = LazyLocalEmbedder::new(&row(DIM as i64), &absent, DIM, 1_024)
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

        let problem = LazyLocalEmbedder::new(&row(512), &absent, DIM, 1_024)
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
        let embedder = LazyLocalEmbedder::new(&row(DIM as i64), &artifact, DIM, 1_024)
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
        let embedder = LazyLocalEmbedder::new(&row(DIM as i64), &artifact, DIM, 1_024)
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
}
