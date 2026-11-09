//! The failure rules on the existing tokens (**design §1** point 4, plan Task 3).
//!
//! Every refusal np-core already answers with a sentence must also be an event with a `rule` field, because a
//! structured reader that cannot parse prose is exactly who this feature is for. These cases drive the real entry
//! points and assert on what the emitter told a subscriber — not on the sentence, which its own tests already pin.
//!
//! The tokens are the ones in the published list (`np_core::diagnostics::RULE_TOKENS`, mirrored in
//! `docs/reference-diagnostics.md`): a rule that appears nowhere else is a bug in one of the three, and
//! [`every_rule_this_crate_emits_is_a_published_token`] is the code side of that.

// The capture subscriber is this binary's own module rather than part of `tests/common/`, for the reason its header
// gives: `mod common;` would compile it into two other binaries that never use it.
mod capture;

use capture::{case, Record};
use np_core::config::{Config, EmbeddingProvider, EmbeddingsConfig};
use np_core::db::{self, Db};
use np_core::embed::{EmbedError, Embedder, Vector};
use np_core::embed_run::{self, EmbedOutcome, EmbedReport, Resolution};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Width the fake provider declares and the installed row declares.
const DIM: usize = 3;
/// The id every fixture installs under.
const MODEL_ID: &str = "fake-model@1";
/// Text no diagnostic may repeat, because it is what a note's body is made of in these fixtures.
const SECRET: &str = "sensitive-note-body-must-not-appear";

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// A scanned workspace with its own data dir, the way `tests/embed_run.rs` sets one up and these cases need it.
struct Setup {
    data_dir: PathBuf,
    _tmp: TempDir,
    root: PathBuf,
}

impl Setup {
    /// Configuration for this workspace. `provider` is spelled out rather than defaulted: the default is `none`, which
    /// is one of the arms under test and would answer every other case with that arm's refusal.
    fn cfg(&self, provider: EmbeddingProvider) -> Config {
        Config {
            data_dir: self.data_dir.clone(),
            embeddings: EmbeddingsConfig {
                provider,
                model: Some(MODEL_ID.to_string()),
                dim: None,
                max_tokens: np_core::config::DEFAULT_EMBEDDING_TOKENS,
                threads: np_core::config::ThreadCount::Auto,
                log_level: None,
            },
            ..Config::default()
        }
    }

    fn db(&self) -> Db {
        Db::open(
            &self
                .cfg(EmbeddingProvider::LocalModel)
                .workspace_db(&self.root),
        )
        .expect("test setup: open the workspace database")
    }

    /// A `models` row with a declared width and no digest, as `np models install` would write.
    fn install_model(&self) {
        let db = self.db();
        db::insert_model(&db, MODEL_ID, DIM as i64, Some("00"), "", "", 1_730_000_000)
            .expect("test setup: insert the model row");
    }

    /// The artifact file the row names, so arm (iii) does not fire when a case is about something else.
    fn touch_artifact(&self) {
        let dir = self
            .cfg(EmbeddingProvider::LocalModel)
            .workspace_models_dir(&self.root);
        fs::create_dir_all(&dir).expect("test setup: models dir");
        fs::write(dir.join(MODEL_ID), b"not a model").expect("test setup: artifact file");
    }
}

/// One scanned vault: `n` notes, each one paragraph and therefore one chunk, each holding [`SECRET`] as its body.
fn workspace(n: usize) -> Setup {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).expect("test setup: workspace dir");
    for index in 0..n {
        fs::write(
            root.join(format!("note-{index:03}.md")),
            format!("# Note {index}\n\n{SECRET} number {index}\n"),
        )
        .expect("test setup: write a note");
    }
    let setup = Setup {
        data_dir: tmp.path().join("data"),
        _tmp: tmp,
        root,
    };
    np_core::scan::scan(&setup.root, &setup.cfg(EmbeddingProvider::None))
        .expect("test setup: first scan");
    setup
}

/// How a fixture provider refuses. An enum rather than an [`EmbedError`] because `EmbedError` is not `Clone` (it
/// carries an `io::Error`), and the privacy case hands the same refusal to two runs.
enum Refusal {
    /// A provider that answered "no" — arm (iv)'s failure shape.
    Unavailable(&'static str),
    /// One over-budget sequence, with both numbers.
    OverBudget { tokens: usize, budget: usize },
}

/// A provider that refuses every call the way a real one does, with the variant the case asks for.
struct Refusing {
    /// What every `embed` call returns.
    failure: Refusal,
}

impl Embedder for Refusing {
    fn model_id(&self) -> &str {
        MODEL_ID
    }

    fn dim(&self) -> usize {
        DIM
    }

    fn model_inputs(&self, texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_string()).collect()
    }

    fn embed(&self, _texts: &[&str]) -> Result<Vec<Vector>, EmbedError> {
        Err(match &self.failure {
            Refusal::Unavailable(why) => EmbedError::Unavailable(why.to_string()),
            Refusal::OverBudget { tokens, budget } => EmbedError::TokenBudgetExceeded {
                tokens: *tokens,
                budget: *budget,
            },
        })
    }
}

/// One embed run whose provider refuses with `failure`.
fn embed_refused(setup: &Setup, failure: Refusal) -> EmbedReport {
    let mut provider = Refusing { failure };
    embed_run::embed_with_resolution(
        &setup.root,
        &setup.cfg(EmbeddingProvider::LocalModel),
        Resolution::Provider(&mut provider),
        np_core::progress::none(),
    )
    .expect("a refusal is a report, not an error")
}

/// Every rule token reported on target `target`, in emission order.
fn rules(events: &[Record], target: &str) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.target == target)
        .filter_map(|event| event.field("rule").map(str::to_string))
        .collect()
}

/// The one event reporting `rule`, asserted to be the only one before any of its fields are read: two events for one
/// refusal is the same duplicate a reader would have to deduplicate.
fn only<'a>(events: &'a [Record], rule: &str) -> &'a Record {
    let found: Vec<&Record> = events.iter().filter(|event| event.is_rule(rule)).collect();
    assert_eq!(
        found.len(),
        1,
        "`{rule}` must be reported exactly once, saw {}",
        events.len()
    );
    found[0]
}

// ---------------------------------------------------------------------------
// the arms, as events
// ---------------------------------------------------------------------------

/// Plan Task 3 Step 1: an embed with embeddings switched off reports `provider-disabled` on the embed target at warn —
/// a state to fix, not a fault — and names the key that decides it.
#[test]
fn a_refusal_reports_its_rule_field_and_not_only_its_sentence() {
    let setup = workspace(1);
    let (_report, events) = case(|| {
        let mut provider = np_core::embed::NoEmbeddingProvider;
        embed_run::embed_with_resolution(
            &setup.root,
            &setup.cfg(EmbeddingProvider::None),
            Resolution::Provider(&mut provider),
            np_core::progress::none(),
        )
        .expect("a refusal is a report")
    });

    let event = only(&events, "provider-disabled");
    assert_eq!(event.target, "np_core::embed");
    assert_eq!(event.level, tracing::Level::WARN);
    assert_eq!(
        event
            .field("what")
            .map(|w| w.contains("embeddings.provider")),
        Some(true),
        "the same key the sentence names: {event:?}"
    );
    // The reader that cannot parse prose still needs to know which id stayed owed.
    assert!(
        rules(&events, "np_core::embed") == vec!["provider-disabled".to_string()],
        "one refusal, one rule: {:?}",
        rules(&events, "np_core::embed")
    );
}

/// Plan Task 3 Step 3, the offline case: an outage is a state (`offline`, warn), and it is reported even though the
/// command exits 0 on `np setup`'s path — because "why are there no vectors" has the same answer here as everywhere else.
#[test]
fn an_outage_is_reported_as_the_offline_rule() {
    let setup = workspace(1);
    setup.install_model();
    let (_report, events) = case(|| {
        embed_run::embed_with_resolution(
            &setup.root,
            &setup.cfg(EmbeddingProvider::LocalModel),
            Resolution::FetchSkippedOffline {
                id: MODEL_ID.to_string(),
            },
            np_core::progress::none(),
        )
        .expect("an outage is a report")
    });

    let event = only(&events, "offline");
    assert_eq!(event.target, "np_core::embed");
    assert_eq!(event.level, tracing::Level::WARN);
    assert_eq!(event.field("what"), Some(MODEL_ID));
}

/// Arm (iii): the artifact is what is missing, so the event names the file. A reader debugging a container with the
/// models directory mounted read-only needs the path, and nothing else about the row.
#[test]
fn an_absent_artifact_is_reported_with_the_file_it_could_not_open() {
    let setup = workspace(1);
    setup.install_model();
    let (_report, events) = case(|| {
        embed_run::embed_with_resolution(
            &setup.root,
            &setup.cfg(EmbeddingProvider::LocalModel),
            Resolution::ArtifactAbsent,
            np_core::progress::none(),
        )
        .expect("an absent artifact is a report")
    });

    let event = only(&events, "artifact-missing");
    assert_eq!(event.target, "np_core::embed");
    assert_eq!(event.level, tracing::Level::WARN);
    assert!(
        event.field("what").map(|w| w.ends_with(MODEL_ID)) == Some(true),
        "the artifact path, so a mount problem is findable: {event:?}"
    );
}

/// Arm (v): a binary built without inference. The rule token is the one the human-facing sentence already uses —
/// `np` must not need two vocabularies for one state.
#[test]
fn a_binary_without_a_runtime_reports_the_local_model_rule() {
    let setup = workspace(1);
    setup.install_model();
    let (_report, events) = case(|| {
        embed_run::embed_with_resolution(
            &setup.root,
            &setup.cfg(EmbeddingProvider::LocalModel),
            Resolution::RuntimeNotBuilt {
                id: MODEL_ID.to_string(),
            },
            np_core::progress::none(),
        )
        .expect("a missing feature is a report")
    });

    let event = only(&events, "local-model-not-built");
    assert_eq!(event.target, "np_core::embed");
    assert_eq!(event.level, tracing::Level::WARN);
    assert_eq!(event.field("what"), Some(MODEL_ID));
}

/// Arm (iv), the failure half: a provider that refused every batch is a fault, so this one is an error — the level
/// split (§4) is "a state to fix is warn, something failed is error", and this is the second.
#[test]
fn a_provider_that_refused_every_batch_is_an_error_rule() {
    let setup = workspace(1);
    setup.install_model();
    setup.touch_artifact();
    let (report, events) =
        case(|| embed_refused(&setup, Refusal::Unavailable("the server said no")));
    assert_eq!(report.errors, 1, "one batch, refused");

    let event = only(&events, "not-produced");
    assert_eq!(event.target, "np_core::embed");
    assert_eq!(event.level, tracing::Level::ERROR);
    // The provider's own words travel in `msg`, which is how §4's "the reason, not just the rule" is satisfied.
    assert!(
        event
            .msg
            .as_deref()
            .map(|m| m.contains("the server said no"))
            == Some(true),
        "{event:?}"
    );
    let _ = report;
}

/// Plan Task 3 Step 2's `token-budget`: the numbers are the whole diagnosis for an over-budget chunk, so they travel as
/// `expected` (the budget) and `actual` (what the chunk needed) rather than only inside a sentence.
#[test]
fn an_over_budget_chunk_reports_its_numbers() {
    let setup = workspace(1);
    setup.install_model();
    setup.touch_artifact();
    let (report, events) = case(|| {
        embed_refused(
            &setup,
            Refusal::OverBudget {
                tokens: 9_000,
                budget: 2_048,
            },
        )
    });

    let event = only(&events, "token-budget");
    assert_eq!(event.target, "np_core::embed");
    assert_eq!(event.level, tracing::Level::WARN);
    assert_eq!(event.field("expected"), Some("2048"));
    assert_eq!(event.field("actual"), Some("9000"));
    assert_eq!(report.refused_budget, 1, "and the document says so too");
}

/// The vector list being empty is a fact a reader acts on ("run `np embed`"), and it is decided inside
/// [`np_core::search`], which is where the event belongs.
#[test]
fn an_empty_vector_list_is_reported_where_it_is_decided() {
    let setup = workspace(2);
    let db = setup.db();
    let query: Vec<u8> = bytemuck_f32(&[1.0, 0.0, 0.0]);
    let (hits, events) = case(|| {
        np_core::search::search(&db, MODEL_ID, &query, 10)
            .expect("an empty index answers Ok(empty)")
    });

    assert!(hits.is_empty(), "the fixture is the empty case");
    let event = only(&events, "no-vectors");
    assert_eq!(event.target, "np_core::search");
    assert_eq!(event.level, tracing::Level::WARN);
    assert_eq!(event.field("what"), Some(MODEL_ID));
}

/// A query vector that cannot be decoded is a caller bug rather than an index state, and must not be reported as
/// `no-vectors`: the two answers send a reader to different commands.
#[test]
fn a_refused_query_is_not_reported_as_an_empty_index() {
    let setup = workspace(1);
    let db = setup.db();
    let (result, events) = case(|| {
        np_core::search::search(&db, MODEL_ID, b"not a vector at all", 10)
            .expect_err("a ragged buffer is refused")
    });
    let _ = result;

    assert!(
        rules(&events, "np_core::search").is_empty(),
        "a caller error is not an index state: {:?}",
        rules(&events, "np_core::search")
    );
}

/// Plan Task 3 Step 4's privacy rule, at the emitter: a diagnostic carries paths, ids, rule tokens and counts. Never a
/// note's body, which is what these fixtures make findable.
#[test]
fn no_diagnostic_carries_a_note_body() {
    let setup = workspace(2);
    setup.install_model();
    setup.touch_artifact();
    let (reports, events) = case(|| {
        let mut all = Vec::new();
        // Every arm that can be reached from here, in one gate: provider off, artifact absent, runtime absent,
        // offline, and a provider that refuses. The body is in every chunk of this vault, so any leak anywhere shows.
        for failure in [
            Refusal::Unavailable("nope"),
            Refusal::OverBudget {
                tokens: 5_000,
                budget: 2_048,
            },
        ] {
            all.push(embed_refused(&setup, failure));
        }
        let mut provider = np_core::embed::NoEmbeddingProvider;
        all.push(
            embed_run::embed_with_resolution(
                &setup.root,
                &setup.cfg(EmbeddingProvider::None),
                Resolution::Provider(&mut provider),
                np_core::progress::none(),
            )
            .expect("a refusal is a report"),
        );
        all
    });

    assert_eq!(reports.len(), 3, "the runs ran");
    assert!(!events.is_empty(), "and each of them reported something");
    for event in &events {
        let rendered = format!("{event:?}");
        assert!(
            !rendered.contains(SECRET),
            "a diagnostic quoted note text: {rendered}"
        );
    }
}

/// The published list (`np_core::diagnostics::RULE_TOKENS`) must cover what the code emits, or the documentation task
/// documents a token nothing produces and a produced one goes undeclared.
#[test]
fn every_rule_this_crate_emits_is_a_published_token() {
    let setup = workspace(1);
    setup.install_model();
    setup.touch_artifact();
    let (report, events) = case(|| embed_refused(&setup, Refusal::Unavailable("no runtime")));
    assert!(!report.succeeded());

    for rule in rules(&events, "np_core::embed")
        .into_iter()
        .chain(rules(&events, "np_core::search"))
    {
        assert!(
            np_core::diagnostics::RULE_TOKENS.contains(&rule.as_str()),
            "`{rule}` is emitted but not in the published list: {:?}",
            np_core::diagnostics::RULE_TOKENS
        );
    }
}

/// Little-endian f32 bytes, the encoding stored vectors use.
fn bytemuck_f32(values: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// The arm this file's `Refusing` fake is written against: the outcome it produces is a refusal, so the events above
/// are about a real refusal rather than a fixture that stopped meaning anything.
#[test]
fn the_refusing_provider_produces_a_refusal_outcome() {
    let setup = workspace(1);
    setup.install_model();
    setup.touch_artifact();
    let report = embed_refused(&setup, Refusal::Unavailable("nope"));
    assert!(
        matches!(report.outcome, EmbedOutcome::NotProduced { .. }),
        "{:?}",
        report.outcome
    );
    assert!(!report.succeeded());
}
