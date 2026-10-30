//! `np_core::embed_run` against a fake provider: §6's currency rule, writer loop, exit arms and stamp rule.
//!
//! Every case here runs without the `local-model` feature and without the 310 MB artifact — that is the point of
//! the injected [`Embedder`] seam (`docs/design-m2-local-embeddings.md` §9), and it is what lets the arms be
//! tested at all: the real runtime's answer today is `RuntimeNotBuilt`, which cannot produce a vector to assert
//! on. Each test names the production mutation that would make it fail.
//!
//! The corpus is real: notes are written to a tempdir and indexed by [`np_core::scan`], so the chunk rows under
//! test are the ones `np embed` will meet in production, including their ids and their `state = 'ok'` parents.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

use np_core::config::{Config, EmbeddingProvider, EmbeddingsConfig};
use np_core::db::{self, Db};
use np_core::embed::{EmbedError, Embedder, Vector};
use np_core::embed_run::{embed, embed_with_inject, EmbedOutcome, EMBED_BATCH_CHUNKS};
use np_core::hash::sha256_bytes;
use np_core::{scan, ScanReport};
use tempfile::TempDir;

/// Width every fake provider here declares and produces. Small, and not a power of two that would let a
/// byte/component confusion hide (`to_bytes` of 3 components is 12 bytes).
const FAKE_DIM: usize = 3;

/// The model id installed by [`Setup::install_model`].
const MODEL_ID: &str = "fake-model@1";

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A scanned workspace, its config, and the tempdirs keeping both alive.
struct Setup {
    /// Where this workspace's database and artifact directory live; `data_dir` of [`Setup::cfg`].
    data_dir: PathBuf,
    /// Kept alive so both directories outlive every scan and embed run against them.
    _tmp: TempDir,
    root: PathBuf,
}

impl Setup {
    /// The configuration every case hands to `embed`: this tempdir's data dir, and a `[embeddings]` section
    /// naming [`MODEL_ID`] under `provider = "local-model"`. Default configuration is `provider = "none"`, which
    /// is §6's exit state (ii) and would answer every case below with a refusal instead of a run — the two cases
    /// that want that arm replace this section.
    fn cfg(&self) -> Config {
        Config {
            data_dir: self.data_dir.clone(),
            embeddings: EmbeddingsConfig {
                provider: EmbeddingProvider::LocalModel,
                model: Some(MODEL_ID.to_string()),
                dim: None,
                max_tokens: np_core::config::DEFAULT_EMBEDDING_TOKENS,
            },
            ..Config::default()
        }
    }

    fn db(&self) -> Db {
        Db::open(&self.cfg().workspace_db(&self.root)).expect("test setup: open workspace db")
    }

    /// The database path an `embed` of this workspace will open.
    fn db_path(&self) -> PathBuf {
        self.cfg().workspace_db(&self.root)
    }

    /// Write a `models` row the way `np models install` would, without its digest verification: the artifact is
    /// a few bytes here, and `install` would refuse it against any real declaration.
    fn install_model(&self, dim: i64, prefix_doc: &str) {
        let db = self.db();
        let wrote = db::insert_model(
            &db,
            MODEL_ID,
            dim,
            Some("00"),
            prefix_doc,
            "",
            1_730_000_000,
        )
        .expect("test setup: insert model row");
        assert!(wrote, "test setup: the row is not already there");
    }

    /// Create the artifact file the row names, so arm (iii) does not fire. A handful of bytes: nothing reads it,
    /// because no provider here loads a model.
    fn touch_artifact(&self) {
        let dir = self.cfg().workspace_models_dir(&self.root);
        fs::create_dir_all(&dir).expect("test setup: models dir");
        fs::write(dir.join(MODEL_ID), b"not a model").expect("test setup: artifact file");
    }

    /// Every stored `(chunk id, source_hash)` for [`MODEL_ID`].
    fn vectors(&self) -> Vec<(i64, String)> {
        db::vectors_for(&self.db(), MODEL_ID)
            .expect("test: reading vectors")
            .into_iter()
            .map(|stamp| (stamp.chunk_id, stamp.source_hash))
            .collect()
    }

    /// The live chunks an embed would see, in its own order.
    fn live_chunks(&self) -> Vec<(i64, String)> {
        db::live_chunks(&self.db())
            .expect("test: reading chunks")
            .into_iter()
            .map(|chunk| {
                (
                    chunk.id,
                    String::from_utf8(chunk.text).expect("test setup: chunk text is UTF-8"),
                )
            })
            .collect()
    }
}

/// A workspace holding `notes` (file name → body), already scanned once.
///
/// The data dir sits inside the same tempdir, so no case touches a real user data dir and no two cases share one
/// workspace database.
fn workspace_at(notes: &[(&str, &str)]) -> Setup {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).expect("test setup: workspace dir");
    for (name, body) in notes {
        fs::write(root.join(name), body).expect("test setup: write note");
    }
    let setup = Setup {
        data_dir: tmp.path().join("data"),
        _tmp: tmp,
        root,
    };
    scan(&setup.root, &setup.cfg()).expect("test setup: first scan");
    setup
}

/// The same fixture from owned strings, which is what [`notes`] and [`two_notes`] produce.
fn workspace_owned(notes: &[(String, String)]) -> Setup {
    let borrowed: Vec<(&str, &str)> = notes
        .iter()
        .map(|(name, body)| (name.as_str(), body.as_str()))
        .collect();
    workspace_at(&borrowed)
}

/// The two-note corpus the prefix case needs: one sentence each, so one chunk each.
fn two_notes() -> Vec<(String, String)> {
    vec![
        ("a.md".to_string(), "alpha sentence\n".to_string()),
        ("b.md".to_string(), "beta sentence\n".to_string()),
    ]
}

/// `n` notes, each one paragraph and therefore one chunk, named so their chunk ids ascend with the name.
fn notes(n: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|index| {
            (
                format!("note-{index:03}.md"),
                format!("# Note {index}\n\nbody text {index} for the model to read\n"),
            )
        })
        .collect()
}

/// One quiet scan, for assertions that only need the report.
fn rescan(setup: &Setup) -> ScanReport {
    scan(&setup.root, &setup.cfg()).expect("test: rescan")
}

// ---------------------------------------------------------------------------
// fakes
// ---------------------------------------------------------------------------

/// A provider that records every batch it was handed and answers with a deterministic vector.
///
/// It is itself under test below (`the_fake_records_what_it_was_handed`), because most assertions here are
/// "the provider was handed exactly this" — a broken recorder would make each of them pass for nothing.
#[derive(Debug, Default)]
struct Fake {
    /// Width to declare and produce; [`FAKE_DIM`] in every case but the wrong-width one.
    dim: usize,
    /// The texts of every `embed` call, in call order, one entry per call.
    batches: Mutex<Vec<Vec<String>>>,
    /// When set, any call holding a text containing this substring refuses the **whole batch** with
    /// [`EmbedError::TokenBudgetExceeded`] — the posture `local_model::LocalEmbedder::embed` has, since it
    /// collects per-text results into one `Result`. That is what forces `embed_run` to retry the batch item by
    /// item to find the offender.
    over_budget: Option<String>,
    /// When non-zero, every call from this 1-based index on refuses with [`EmbedError::RuntimeNotBuilt`] —
    /// the interrupted run.
    fail_from: usize,
    calls: AtomicUsize,
}

impl Fake {
    fn new() -> Self {
        Self {
            dim: FAKE_DIM,
            ..Self::default()
        }
    }

    /// A fake that refuses any batch holding `marker`, as a real runtime refuses an over-budget text.
    fn over_budget(marker: &str) -> Self {
        Self {
            dim: FAKE_DIM,
            over_budget: Some(marker.to_string()),
            ..Self::default()
        }
    }

    fn with_dim(dim: usize) -> Self {
        Self {
            dim,
            ..Self::default()
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn batches(&self) -> Vec<Vec<String>> {
        self.batches
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn texts(&self) -> Vec<String> {
        self.batches().into_iter().flatten().collect()
    }
}

impl Embedder for Fake {
    fn model_id(&self) -> &str {
        MODEL_ID
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vector>, EmbedError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.batches
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(texts.iter().map(|text| (*text).to_string()).collect());
        let over_budget = self
            .over_budget
            .as_deref()
            .is_some_and(|marker| texts.iter().any(|text| text.contains(marker)));
        if over_budget {
            return Err(EmbedError::TokenBudgetExceeded {
                tokens: 4_242,
                budget: 1_024,
            });
        }
        if self.fail_from != 0 && call >= self.fail_from {
            return Err(EmbedError::RuntimeNotBuilt {
                model_id: MODEL_ID.to_string(),
            });
        }
        Ok((0..texts.len())
            .map(|_| Vector::new(vec![0.5; self.dim]))
            .collect())
    }
}

/// One-time guard that the fake's recorder works, so a later `calls() == 0` is evidence rather than tautology.
static RECORDER_CHECK: Once = Once::new();

fn assert_recorder_works() {
    RECORDER_CHECK.call_once(|| {
        let fake = Fake::new();
        fake.embed(&["one", "two"])
            .expect("test setup: fake answers");
        assert_eq!(fake.calls(), 1);
        assert_eq!(
            fake.texts(),
            vec!["one".to_string(), "two".to_string()],
            "the recorder must see every text of every call"
        );
    });
}

// ---------------------------------------------------------------------------
// the writer loop
// ---------------------------------------------------------------------------

#[test]
fn the_fake_records_what_it_was_handed() {
    assert_recorder_works();
}

/// Determinism and ordering: one batch per [`EMBED_BATCH_CHUNKS`] in chunk-id order, and a stored stamp equal
/// to the digest of exactly the text the provider was handed.
///
/// Mutation that fails it: batching without ordering (`HashMap` iteration), or stamping `chunk.text` rather
/// than the string actually passed to `embed`.
#[test]
fn chunks_reach_the_provider_in_chunk_order_and_are_stamped_with_what_it_read() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(3));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();
    let live = setup.live_chunks();
    assert_eq!(live.len(), 3, "three notes cut three chunks");

    let fake = Fake::new();
    let report = embed(&setup.root, &setup.cfg(), &fake).expect("run");
    assert!(report.succeeded(), "{:?}", report.outcome);
    assert_eq!(report.chunks_seen, 3);
    assert_eq!(report.embedded, 3);

    // One batch of three (below the bound), in the order `db::live_chunks` answers.
    let batches = fake.batches();
    assert_eq!(batches.len(), 1, "three chunks fit one bounded batch");
    let expected: Vec<String> = live.iter().map(|(_, text)| text.clone()).collect();
    assert_eq!(
        batches[0], expected,
        "the batch is the chunk texts in order"
    );

    // The stamp rule (§6, backlog D4): digest of the bytes handed over — here an empty prefix, so the text.
    let stored: HashMap<i64, String> = setup.vectors().into_iter().collect();
    assert_eq!(stored.len(), 3);
    for (id, text) in &live {
        assert_eq!(
            stored.get(id),
            Some(&sha256_bytes(text.as_bytes())),
            "chunk {id} must be stamped with the digest of what the model was given"
        );
    }
}

/// Per-item prefix pairing (Scope B's carried gap): with a non-empty `prefix_doc`, **each** text in a batch of
/// two must arrive with its own prefix, not another's and not a shared one.
///
/// Mutation that fails it: building the batch as `format!("{prefix}{}", inputs[0])`, or applying the prefix to
/// only the first element.
#[test]
fn every_text_in_a_batch_travels_with_its_own_prefix() {
    assert_recorder_works();
    let setup = workspace_owned(&two_notes());
    setup.install_model(FAKE_DIM as i64, "doc: ");
    setup.touch_artifact();

    let fake = Fake::new();
    let report = embed(&setup.root, &setup.cfg(), &fake).expect("run");
    assert!(report.succeeded(), "{:?}", report.outcome);

    let mut pairs: Vec<(String, String)> = setup
        .live_chunks()
        .into_iter()
        .map(|(_id, text)| {
            let stored = sha256_bytes(format!("doc: {text}").as_bytes());
            (text, stored)
        })
        .collect();
    pairs.sort();
    // The chunker trims a block's trailing newline, so the expected strings are the stored chunk texts with the
    // prefix in front — the pairing is the assertion, not the punctuation.
    let mut expected: Vec<String> = setup
        .live_chunks()
        .into_iter()
        .map(|(_, text)| format!("doc: {text}"))
        .collect();
    expected.sort();
    assert_eq!(expected.len(), 2, "two notes, two chunks");
    let mut received = fake.texts();
    received.sort();
    assert_eq!(
        received, expected,
        "each text arrives prefixed with the row's own prefix_doc, verbatim and in front of its own body"
    );

    // And the stamp is the digest of the *prefixed* string, so a prefix change makes it stale (tested next).
    let stored: HashMap<i64, String> = setup.vectors().into_iter().collect();
    assert_eq!(stored.len(), 2);
    for (_, digest) in &pairs {
        assert!(
            stored.values().any(|held| held == digest),
            "a stored stamp must be sha256(prefix + text)"
        );
    }
}

/// §7's "unchanged chunk → provider called zero times", the warm-path claim of the whole design.
#[test]
fn an_unchanged_corpus_never_reaches_the_provider() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(2));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();

    let first = Fake::new();
    embed(&setup.root, &setup.cfg(), &first).expect("first run");
    assert_eq!(first.calls(), 1);

    let second = Fake::new();
    let report = embed(&setup.root, &setup.cfg(), &second).expect("second run");
    assert_eq!(
        second.calls(),
        0,
        "a current corpus must not be re-embedded"
    );
    assert_eq!(report.chunks_seen, 2);
    assert_eq!(report.current, 2);
    assert_eq!(report.embedded, 0);
    assert_eq!(report.outcome, EmbedOutcome::Current);

    // A scan in between changes nothing about the chunk rows, so it must not re-owe them either.
    rescan(&setup);
    let third = Fake::new();
    embed(&setup.root, &setup.cfg(), &third).expect("third run");
    assert_eq!(
        third.calls(),
        0,
        "an unchanged note stays current across a scan"
    );
}

/// Changing only `prefix_doc` makes every stored vector stale, because the stamp is of the prefixed bytes.
///
/// Mutation that fails it: stamping `chunks.source_hash` or `files.hash`, which no prefix change moves.
#[test]
fn a_changed_prefix_makes_a_stored_vector_stale() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(1));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();
    embed(&setup.root, &setup.cfg(), &Fake::new()).expect("first run");

    // Re-install the same id with a different document prefix. `insert_model` is idempotent, so the row is
    // replaced by hand — the point is the row's `prefix_doc`, not the install path.
    {
        let db = setup.db();
        db.0.execute(
            "UPDATE models SET prefix_doc = 'query-side: ' WHERE id = ?1",
            rusqlite::params![MODEL_ID],
        )
        .expect("test: move the prefix");
    }

    let fake = Fake::new();
    let report = embed(&setup.root, &setup.cfg(), &fake).expect("second run");
    assert_eq!(
        fake.calls(),
        1,
        "the same text under a new prefix is owed again"
    );
    assert_eq!(report.current, 0);
    assert_eq!(report.embedded, 1);
}

/// Budget refusal: an over-budget chunk is counted and writes nothing, while its batchmates still land.
#[test]
fn an_over_budget_chunk_is_reported_and_stamps_nothing() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(3));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();
    let live = setup.live_chunks();

    // The batch refuses because one member is over budget; the per-item retry then answers two and refuses one.
    let fake = Fake::over_budget("body text 1 ");
    let report = embed(&setup.root, &setup.cfg(), &fake).expect("run");

    assert_eq!(
        report.embedded, 2,
        "the two chunks that fit are still embedded"
    );
    assert_eq!(report.current, 0);
    // The offender is refused rather than truncated, so it is owed and unproduced: arm (iv), exit non-zero.
    assert!(!report.succeeded(), "{:?}", report.outcome);
    let reason = match &report.outcome {
        EmbedOutcome::NotProduced { reason } => reason.clone(),
        other => panic!("expected the owed-and-not-produced arm, got {other:?}"),
    };
    assert!(
        reason.contains("over the token budget")
            && reason.contains("4242")
            && reason.contains("1024"),
        "the report must say it refused rather than truncated, with both numbers: {reason}"
    );

    // Exactly two rows, and the missing one is a chunk this run refused — not an arbitrary pair.
    let stored = setup.vectors();
    assert_eq!(stored.len(), 2);
    let embedded_ids: Vec<i64> = stored.iter().map(|(id, _)| *id).collect();
    let owed_ids: Vec<i64> = live
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| !embedded_ids.contains(id))
        .collect();
    assert_eq!(owed_ids.len(), 1, "one chunk of three stays unstamped");
    let over = live
        .iter()
        .find(|(id, _)| id == &owed_ids[0])
        .map(|(_, text)| text)
        .expect("test: the unstamped id is a live chunk");
    assert!(
        over.contains("body text 1 "),
        "the unstamped chunk must be the one the model refused, not any one: {over}"
    );

    // No row claims a digest for text the model never read in full: every stored stamp is a real chunk digest.
    for (id, hash) in &stored {
        let text = live
            .iter()
            .find(|(chunk_id, _)| chunk_id == id)
            .map(|(_, text)| text)
            .expect("test: the row names a live chunk");
        assert_eq!(hash, &sha256_bytes(text.as_bytes()));
    }
}

/// Wrong width: refused before any write, naming both numbers, with the provider never asked.
#[test]
fn a_wrong_width_provider_is_refused_before_any_write() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(2));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();

    let fake = Fake::with_dim(FAKE_DIM + 1);
    let error = embed(&setup.root, &setup.cfg(), &fake).expect_err("a mismatch must refuse");
    match &error {
        np_core::embed_run::EmbedRunError::ProviderDimMismatch {
            configured,
            provider,
        } => {
            assert_eq!(*configured, FAKE_DIM);
            assert_eq!(*provider, (FAKE_DIM + 1) as i64);
        }
        other => panic!("expected the width refusal, got {other:?}"),
    }
    let text = error.to_string();
    assert!(
        text.contains(&FAKE_DIM.to_string()) && text.contains(&(FAKE_DIM + 1).to_string()),
        "both numbers belong in the message: {text}"
    );
    assert_eq!(
        fake.calls(),
        0,
        "the refusal comes before the provider runs"
    );
    assert!(setup.vectors().is_empty(), "and before anything is written");
}

/// Resume after an interrupted run: the second run writes only what is left.
#[test]
fn an_interrupted_run_resumes_with_only_what_is_left() {
    assert_recorder_works();
    let total = EMBED_BATCH_CHUNKS + 1;
    let setup = workspace_owned(&notes(total));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();

    // The first batch commits, the second refuses: `fail_from` counts calls, so call 2 is batch two.
    let interrupted = Fake {
        fail_from: 2,
        ..Fake::new()
    };
    let report = embed(&setup.root, &setup.cfg(), &interrupted).expect("first run");
    assert_eq!(report.embedded, EMBED_BATCH_CHUNKS);
    assert_eq!(report.errors, 1);
    assert!(!report.succeeded());
    assert_eq!(setup.vectors().len(), EMBED_BATCH_CHUNKS);

    // The resume sees the committed batch as current and owes exactly one chunk.
    let resumed = Fake::new();
    let report = embed(&setup.root, &setup.cfg(), &resumed).expect("second run");
    assert!(report.succeeded(), "{:?}", report.outcome);
    assert_eq!(report.current, EMBED_BATCH_CHUNKS);
    assert_eq!(report.embedded, 1);
    assert_eq!(
        resumed.calls(),
        1,
        "only the remaining chunk reaches the model"
    );
    assert_eq!(resumed.texts().len(), 1);
    assert_eq!(setup.vectors().len(), total);
}

/// Idempotence through `upsert_vector`'s `ON CONFLICT`: re-writing a batch that is already there updates the row
/// rather than failing on the primary key. Reached by stamping a chunk with a digest nothing agrees with.
#[test]
fn a_repeated_batch_updates_instead_of_failing() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(1));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();
    let text = setup.live_chunks()[0].1.clone();

    // A row stamped with a digest the run cannot match: the chunk is owed, and the write hits an existing
    // `(chunk_id, model)` key. Without `ON CONFLICT DO UPDATE` this is `SQLITE_CONSTRAINT_PRIMARYKEY`.
    {
        let db = setup.db();
        let live = db::live_chunks(&db).expect("test: read chunks");
        db::upsert_vector(
            &db,
            live[0].id,
            MODEL_ID,
            FAKE_DIM,
            &[0u8; FAKE_DIM * 4],
            "stale-digest",
            1_730_000_000,
        )
        .expect("test: seed a stale vector");
    }

    let report = embed(&setup.root, &setup.cfg(), &Fake::new()).expect("run");
    assert!(report.succeeded(), "{:?}", report.outcome);
    assert_eq!(report.current, 0, "the old stamp is not currency");
    assert_eq!(report.embedded, 1);
    let stored = setup.vectors();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].1,
        sha256_bytes(text.as_bytes()),
        "the row now says what it holds"
    );
}

// ---------------------------------------------------------------------------
// the liveness fence (backlog D3)
// ---------------------------------------------------------------------------

/// §6's stale-note fixture: one note whose `files.state` is `stale` must never be owed.
#[test]
fn a_stale_note_s_chunks_are_never_owed() {
    assert_recorder_works();
    let setup = workspace_at(&[
        ("live.md", "kept note text\n"),
        ("gone.md", "deleted note text\n"),
    ]);
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();
    assert_eq!(
        setup.live_chunks().len(),
        2,
        "both notes are live to start with"
    );

    fs::remove_file(setup.root.join("gone.md")).expect("test: delete one note");
    let report = rescan(&setup);
    assert_eq!(
        report.removed, 1,
        "the scan marked it stale rather than forgetting it"
    );

    let fake = Fake::new();
    let embed_report = embed(&setup.root, &setup.cfg(), &fake).expect("run");
    assert!(embed_report.succeeded(), "{:?}", embed_report.outcome);
    assert_eq!(
        embed_report.chunks_seen, 1,
        "the fence admits only the live note's chunk"
    );
    let texts = fake.texts();
    assert_eq!(texts.len(), 1);
    assert!(
        !texts.iter().any(|text| text.contains("deleted note text")),
        "a stale note's text must never reach a model: {texts:?}"
    );

    // And its chunk row is still there — the fence is what hides it, not a delete.
    let all: i64 = setup
        .db()
        .0
        .query_row("SELECT COUNT(*) FROM chunks", [], |row| row.get(0))
        .expect("test: count chunks");
    assert_eq!(all, 2, "the stale note keeps its chunk row and its vectors");
}

// ---------------------------------------------------------------------------
// the four arms
// ---------------------------------------------------------------------------

/// Arm (ii), first half: `provider = "none"` refuses before anything is read or written.
#[test]
fn provider_none_is_configuration_disabled_and_reads_nothing() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(2));
    // No row installed and no artifact created on purpose: arm (ii) must fire before either is consulted.
    let cfg = Config {
        embeddings: EmbeddingsConfig {
            provider: EmbeddingProvider::None,
            model: None,
            dim: None,
            max_tokens: np_core::config::DEFAULT_EMBEDDING_TOKENS,
        },
        ..setup.cfg()
    };

    let fake = Fake::new();
    let report = embed(&setup.root, &cfg, &fake).expect("the arm is a report, not an error");
    assert_eq!(
        report.outcome,
        EmbedOutcome::ConfigurationDisabled {
            key: "embeddings.provider",
            id: Some("none".to_string())
        }
    );
    assert_eq!(report.outcome.label(), "configuration disabled");
    assert_eq!(report.chunks_seen, 0, "nothing was read");
    assert_eq!(fake.calls(), 0, "nothing was asked");
    assert!(!report.succeeded(), "arm (ii) is exit 1 (§6)");
}

/// Arm (ii), second half: `embeddings.model` naming no installed row names the key and the id.
#[test]
fn a_model_naming_no_row_is_configuration_disabled_naming_the_id() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(2));
    setup.touch_artifact();

    let fake = Fake::new();
    let report = embed(&setup.root, &setup.cfg(), &fake).expect("the arm is a report");
    assert_eq!(
        report.outcome,
        EmbedOutcome::ConfigurationDisabled {
            key: "embeddings.model",
            id: Some(MODEL_ID.to_string())
        },
        "the refusal must name the key and the id it could not find"
    );
    assert_eq!(
        report.outcome.detail(),
        Some(format!("embeddings.model = \"{MODEL_ID}\""))
    );
    assert_eq!(fake.calls(), 0);
    assert!(!report.succeeded());
}

/// Arm (iii): chunks are owed, the row exists, and there is no artifact — so the advice must name a command
/// that exists. `np models fetch` does not (§6's wording predates R7b), so the hint names `install`.
#[test]
fn an_absent_artifact_names_a_command_that_exists() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(2));
    setup.install_model(FAKE_DIM as i64, "");

    let fake = Fake::new();
    let report = embed(&setup.root, &setup.cfg(), &fake).expect("the arm is a report");
    assert_eq!(report.outcome.label(), "artifact absent");
    let detail = report
        .outcome
        .detail()
        .expect("arm (iii) carries its advice");
    assert!(detail.contains("np models install"), "{detail}");
    assert!(!detail.contains("np models fetch"), "{detail}");
    assert!(detail.contains("not implemented"), "{detail}");
    assert_eq!(
        report.chunks_seen, 2,
        "the corpus was read before the arm fired"
    );
    assert_eq!(report.embedded, 0);
    assert_eq!(
        fake.calls(),
        0,
        "no provider call is attempted without an artifact"
    );
    assert!(!report.succeeded());
}

/// Arm (i) on a corpus that owes nothing, with no artifact and no provider call.
#[test]
fn nothing_owed_exits_zero_even_with_no_artifact() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(1));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();
    embed(&setup.root, &setup.cfg(), &Fake::new()).expect("first run");

    fs::remove_file(setup.cfg().workspace_models_dir(&setup.root).join(MODEL_ID))
        .expect("test: remove the artifact");
    let fake = Fake::new();
    let report = embed(&setup.root, &setup.cfg(), &fake).expect("run");
    assert_eq!(report.outcome, EmbedOutcome::Current);
    assert!(
        report.succeeded(),
        "§6's exit state (i) is 0 whatever else is true"
    );
    assert_eq!(fake.calls(), 0);
}

/// Arm (iv): owed and not produced — the only arm that means "gave up". This is what `np embed` does today on a
/// real `local-model` install, because the runtime still refuses; R6c2 changes the provider, not this arm.
#[test]
fn owed_and_not_produced_is_the_gave_up_arm() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(1));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();

    let provider = np_core::embed::LocalModelProvider::new(
        &db::model_row(&setup.db(), MODEL_ID)
            .expect("test: read the row")
            .expect("test: the row is installed"),
        &setup.cfg().workspace_models_dir(&setup.root).join(MODEL_ID),
        FAKE_DIM,
    )
    .expect("the provider builds against the touched artifact");

    let report = embed(&setup.root, &setup.cfg(), &provider).expect("run");
    assert_eq!(report.outcome.label(), "owed and not produced");
    let reason = report.outcome.detail().expect("arm (iv) says why");
    assert!(reason.contains(MODEL_ID), "{reason}");
    assert_eq!(report.embedded, 0);
    assert!(setup.vectors().is_empty());
    assert!(!report.succeeded());
}

// ---------------------------------------------------------------------------
// SQLITE_BUSY: a reported gap, never a silent skip
// ---------------------------------------------------------------------------

/// A batch that meets the write lock stamps nothing, is counted, and exits non-zero.
///
/// The lock is held by a second connection on this thread (`BEGIN IMMEDIATE`), so the wait is deterministic:
/// `BUSY_TIMEOUT_MS` is 1800 ms and nobody is going to release it. Costs ~2 s, which is the price of testing the
/// real timeout instead of pretending.
#[test]
fn a_busy_batch_stamps_nothing_and_is_reported() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(2));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();

    let holder = Db::open(&setup.db_path()).expect("test setup: second connection");
    holder
        .0
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("test setup: take the write lock");

    let fake = Fake::new();
    let report =
        embed(&setup.root, &setup.cfg(), &fake).expect("a busy batch is reported, not raised");
    assert_eq!(report.busy_gap, 1, "one batch met the lock");
    assert_eq!(report.embedded, 0);
    assert_eq!(report.errors, 0, "a lock is a gap, not an error count");
    assert!(
        setup.vectors().is_empty(),
        "nothing is stamped for that batch"
    );
    let reason = match &report.outcome {
        EmbedOutcome::NotProduced { reason } => reason.clone(),
        other => panic!("expected arm (iv), got {other:?}"),
    };
    assert!(reason.contains("SQLITE_BUSY"), "{reason}");

    // Release, and the same run finishes: the gap was a gap rather than damage.
    holder.0.execute_batch("ROLLBACK;").expect("test: release");
    let after = embed(&setup.root, &setup.cfg(), &Fake::new()).expect("run after the lock");
    assert!(after.succeeded(), "{:?}", after.outcome);
    assert_eq!(after.embedded, 2);
}

/// §6's contention rule: a `np scan` must succeed while an embed sits **between** batches.
///
/// The hook runs between batches, outside every transaction. Mutation that fails it: one transaction for the
/// whole corpus — the scan would then wait out `busy_timeout` and fail, which is exactly what §6 opens this
/// command to avoid.
#[test]
fn a_scan_can_take_the_write_lock_between_batches() {
    assert_recorder_works();
    let total = EMBED_BATCH_CHUNKS + 1;
    let setup = workspace_owned(&notes(total));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();

    // The scan runs on the same thread inside the hook, so it needs its own borrow of the fixture.
    let root = setup.root.clone();
    let cfg = setup.cfg();
    let mut scans = 0usize;
    let fake = Fake::new();
    let report = embed_with_inject(&setup.root, &setup.cfg(), &fake, &mut |db_path| {
        assert!(
            db_path.exists(),
            "the hook is handed the database an embed is writing"
        );
        // Count batches by how often we are called: more than once means the loop really is batched.
        scans += 1;
        scan(&root, &cfg).expect("a scan must succeed while an embed sits between batches");
    })
    .expect("run");

    assert!(report.succeeded(), "{:?}", report.outcome);
    assert_eq!(
        fake.batches().len(),
        2,
        "two bounded batches for {total} chunks"
    );
    assert_eq!(scans, 2, "one hook call per committed batch");
    assert_eq!(setup.vectors().len(), total);
}

// ---------------------------------------------------------------------------
// the report's shape
// ---------------------------------------------------------------------------

/// §7's caveat line travels on every report, including the arms that did no work: "a caveat nobody reads at
/// 2 a.m. is not a caveat".
#[test]
fn every_report_carries_the_caveat_line() {
    assert_recorder_works();
    let setup = workspace_owned(&notes(1));
    setup.install_model(FAKE_DIM as i64, "");
    setup.touch_artifact();
    let report = embed(&setup.root, &setup.cfg(), &Fake::new()).expect("run");
    assert_eq!(report.verification, "self-consistent, not verified");

    let cfg = Config {
        embeddings: EmbeddingsConfig {
            provider: EmbeddingProvider::None,
            model: None,
            dim: None,
            max_tokens: np_core::config::DEFAULT_EMBEDDING_TOKENS,
        },
        ..setup.cfg()
    };
    let disabled = embed(&setup.root, &cfg, &Fake::new()).expect("arm (ii)");
    assert_eq!(disabled.verification, "self-consistent, not verified");
}

/// An empty `data_dir` is refused rather than turned into a database beside the current directory — the same
/// fence [`np_core::scan`] raises.
#[test]
fn an_empty_data_dir_is_refused() {
    let setup = workspace_owned(&notes(1));
    let cfg = Config {
        data_dir: PathBuf::new(),
        ..setup.cfg()
    };
    let error = embed(&setup.root, &cfg, &Fake::new()).expect_err("must refuse");
    assert!(
        error.to_string().contains("data_dir"),
        "the refusal names the missing value: {error}"
    );
}
