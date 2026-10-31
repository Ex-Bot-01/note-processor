//! The provider seam: what [`np_core::embed`] promises while there is no model and no endpoint.
//!
//! Two jobs here, both aimed at the way this module could go wrong silently:
//!
//! - **The stored form.** Migration 7 keeps vectors as raw little-endian `f32` under
//!   `CHECK (length(vector) = 4 * dim)`, so a writer or reader that drifts produces rows the schema
//!   accepts and whose cosines are garbage. The byte-level assertions below pin the exact encoding, not
//!   just a round trip — a symmetric swap to big-endian survives `to_bytes`∘`from_bytes` and dies on the
//!   pinned bytes.
//! - **"None contacts nothing."** §7 wants that to be a property of the type rather than of a code path,
//!   so it is asserted as one: [`NoEmbeddingProvider`] has no field at all, which `size_of` can see.
//! - **The local seam, before there is a runtime.** `docs/design-m2-local-embeddings.md` §9 item 1 puts the
//!   API here while R6b0 and R6b1 are still ahead of it, so the parts that can already be wrong — where
//!   identity comes from, who applies the prefix, when a width disagreement is refused, what an absent
//!   artifact answers with — are fenced by [`LocalModelProvider`]'s cases at the bottom.
//!
//! Each test names the production mutation that would make it fail.

use std::fs;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use np_core::db::ModelRow;
use np_core::embed::{EmbedError, Embedder, LocalModelProvider, NoEmbeddingProvider, Vector};
use tempfile::TempDir;

/// Width the [`CountingEmbedder`] below claims and produces: small, and not a multiple that would let a
/// byte-count mistake hide (4 components would make `to_bytes`' length equal `dim * 4` for a buffer of
/// `dim` bytes too).
const COUNTING_DIM: usize = 3;

/// A provider that records every call it receives, for §7's staleness assertions.
///
/// Round 3b needs exactly one number from a provider — "was it called zero times?" — and a test that
/// only ever asserts zero would pass just as happily if the counter were wired to nothing. So this type
/// is itself under test below (`the_counting_provider_records_every_call_it_is_given`), which is what
/// turns the later zero into evidence rather than tautology. Reusable in shape: `Default`, a
/// `&self`-callable trait impl, and accessors that never need the borrow that made the call.
#[derive(Debug, Default)]
struct CountingEmbedder {
    /// `embed` calls so far, in the whole lifetime of this value.
    calls: AtomicUsize,
    /// Every text ever passed to `embed`, flattened in call order — the batch boundaries matter less
    /// than "which note text reached the provider at all".
    texts: Mutex<Vec<String>>,
}

impl CountingEmbedder {
    fn new() -> Self {
        Self::default()
    }

    /// How many times `embed` was called. Zero-after-an-unchanged-scan is §7's staleness assertion.
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The texts received, in order, across every call.
    fn texts(&self) -> Vec<String> {
        self.texts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Embedder for CountingEmbedder {
    /// Identity: this fake stands in for a provider with no prefix rule, and its `embed` adds nothing either, so
    /// the two agree — which is the only contract [`Embedder::model_inputs`] has.
    fn model_inputs(&self, texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_string()).collect()
    }

    fn model_id(&self) -> &str {
        "counting@test"
    }

    fn dim(&self) -> usize {
        COUNTING_DIM
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vector>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.texts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(texts.iter().map(|text| (*text).to_string()));
        Ok(texts
            .iter()
            .map(|_| Vector::new(vec![0.25; COUNTING_DIM]))
            .collect())
    }
}

#[test]
fn a_vector_survives_its_stored_bytes() {
    // Round trip through the form migration 7 stores, on values whose bit patterns are asymmetric
    // enough that a byte-order or stride slip cannot cancel itself out.
    let vector = Vector::new(vec![1.0, -2.5, 0.0, f32::MIN_POSITIVE, 4_000_000.5]);
    assert_eq!(vector.dim(), 5);

    let bytes = vector.to_bytes();
    assert_eq!(bytes.len(), 5 * 4, "four bytes per component, nothing else");

    let read_back = Vector::from_bytes(&bytes, 5).expect("our own bytes must read back");
    assert_eq!(read_back, vector, "the round trip must be exact");

    // Mutation this catches: `to_le_bytes` → `to_be_bytes` in `to_bytes` (or the same swap in
    // `from_bytes`) turns every component into a different number.
}

#[test]
fn the_stored_bytes_are_little_endian_f32_verbatim() {
    // The round trip above cannot catch an encoding that is wrong **both** ways, and migration 7's
    // `CHECK` cannot either — it counts bytes. So the exact stored bytes are pinned here, because
    // §3 made little-endian raw `f32` the schema's rule and a future re-read by another tool depends on
    // it: 1.0_f32 is `00 00 80 3F`, -2.5_f32 is `00 00 20 C0`.
    let bytes = Vector::new(vec![1.0, -2.5]).to_bytes();
    assert_eq!(bytes, vec![0x00, 0x00, 0x80, 0x3f, 0x00, 0x00, 0x20, 0xc0]);

    // Mutation this catches: flipping the byte order in both directions at once, or prefixing a
    // length/count field "for convenience" — either way these eight bytes stop being these eight.
}

#[test]
fn a_ragged_buffer_is_refused_as_malformed_naming_its_byte_length() {
    // Two ragged lengths on purpose. Six bytes (4·1+2) is the obvious one; ten bytes is the case that
    // must not be reported as a dimension mismatch, because at `dim = 2` it holds exactly two whole
    // components plus two stray bytes — any derived component count would print "expected 2, got 2" for
    // a buffer this function refuses.
    for (bytes, dim) in [
        (vec![0_u8, 0, 0x80, 0x3f, 0x00, 0x00], 2usize),
        (vec![0_u8; 10], 2),
    ] {
        let err = Vector::from_bytes(&bytes, dim).expect_err(
            "a length that is not a whole number of f32 components is no vector at all",
        );
        match err {
            EmbedError::Malformed { len } => assert_eq!(
                len,
                bytes.len(),
                "the refusal must carry the byte length it was given, not a derived count"
            ),
            other => panic!(
                "expected Malformed for {} bytes at dim {dim}, got {other:?}",
                bytes.len()
            ),
        }
    }

    // Mutation this catches: deleting the trailing-byte check. Then 10 bytes at `dim = 2` would decode
    // into a two-component vector that quietly swallowed two bytes (this test fails on `expect_err`),
    // and 6 bytes would fall through to `DimensionMismatch`, which this test fails on the match arm.
}

#[test]
fn from_bytes_refuses_a_component_count_that_is_not_the_declared_dim() {
    // A well-formed buffer of the wrong width: two components where the row declares three — the shape
    // of a model swap that forgot to update `models.dim`, which §3 refuses at read rather than letting
    // become a subtly wrong cosine.
    let bytes = Vector::new(vec![1.0, 2.0]).to_bytes();

    let err = Vector::from_bytes(&bytes, 3).expect_err("two components are not three");
    match err {
        EmbedError::DimensionMismatch { expected, got } => {
            assert_eq!(expected, 3, "the declared dim is what was expected");
            assert_eq!(got, 2, "and the count is in components, never bytes");
        }
        other => panic!("expected DimensionMismatch, got {other:?}"),
    }

    // Mutation this catches: dropping the `windows.len() != dim` comparison, which would return a
    // two-component vector for a row whose `dim` column says 3.
}

#[test]
fn the_none_provider_refuses_without_panicking() {
    // §7: an unconfigured provider is refused quietly. A panic here would poison CI's
    // `--ignored` release run (F16's lesson), so the error is asserted as a value rather than by
    // `#[should_panic]`, and on a non-empty batch — the texts must not be inspected to be refused.
    let err = NoEmbeddingProvider
        .embed(&["eine Notiz", "another note"])
        .expect_err("\"none\" never produces vectors");

    assert!(
        matches!(err, EmbedError::Disabled),
        "expected Disabled, got {err:?}"
    );
    // The refusal has to be actionable from the message alone, like F17's indent ceiling: a user who
    // sees it needs to know which key to set.
    assert!(
        err.to_string().contains("embeddings.provider"),
        "the refusal must name the config key that changes it: {err}"
    );

    // Mutation this catches: `Ok(vec![])` instead of an error — a scan would then report "0 chunks
    // needed embedding" for a corpus it never embedded.
}

#[test]
fn the_none_provider_holds_nothing_that_could_contact_anything() {
    // The load-bearing assertion of Round 3a, and deliberately about the **type**: §7 wants "`none`
    // constructs no HTTP client at all" to be provable without watching a network. A zero-sized type has
    // no room for a client, a base URL, an API key or a feature-gated field of any kind, so the property
    // cannot rot into a lie by someone adding "just one" field here — that addition has to make this
    // number non-zero and get argued for in review.
    assert_eq!(
        size_of::<NoEmbeddingProvider>(),
        0,
        "NoEmbeddingProvider must stay a unit struct: any client, URL or key field is the leak §9 forbids"
    );

    // Its self-description follows the same rule: there is no model and no width to report.
    assert_eq!(NoEmbeddingProvider.model_id(), "none");
    assert_eq!(NoEmbeddingProvider.dim(), 0);

    // Mutation this catches: `struct NoEmbeddingProvider { base_url: String }` (or any client field),
    // which is exactly the change §7's fence exists to catch.
}

#[test]
fn the_counting_provider_records_every_call_it_is_given() {
    // Why a test for a test helper: every staleness assertion in Round 3b is of the form "the provider
    // was called zero times", and that passes for free if the counter is not wired up. Asserting the
    // non-zero side — twice called, both batches' texts kept, right shape back — is what makes the later
    // zero mean something.
    let embedder = CountingEmbedder::new();
    assert_eq!(
        embedder.calls(),
        0,
        "a fresh provider has been called zero times"
    );

    let vectors = embedder
        .embed(&["erste Notiz", "zweite Notiz"])
        .expect("the counting provider always serves");
    embedder.embed(&["dritte Notiz"]).expect("and again");

    assert_eq!(embedder.calls(), 2, "one call per `embed`");
    assert_eq!(
        embedder.texts(),
        vec!["erste Notiz", "zweite Notiz", "dritte Notiz"],
        "texts are recorded in call order, across batches"
    );
    assert_eq!(vectors.len(), 2, "one vector per text");
    for vector in &vectors {
        assert_eq!(
            vector.dim(),
            embedder.dim(),
            "a provider keeps its declared width"
        );
    }

    // Mutation this catches: a counter that is never incremented (or a text list that is overwritten
    // instead of extended) — the helper would then certify "zero calls" for a provider that was called.
}

/// The other side of the pair. `from_bytes` compares with `!=`, and a test that only ever hands it a short
/// buffer cannot tell that from `<`: flip the comparison and every too-long vector is accepted, its extra
/// components read into a row whose stored `dim` now understates them — which is the same silent truncation
/// `Malformed` exists to prevent, arriving one line later. So the long case is asserted with both numbers in
/// the payload, not merely "it errored".
#[test]
fn a_buffer_longer_than_the_declared_dim_is_refused_by_name() {
    let bytes = Vector::new(vec![1.0, 2.0, 3.0]).to_bytes();
    match Vector::from_bytes(&bytes, 2) {
        Err(EmbedError::DimensionMismatch { expected: 2, got: 3 }) => {}
        other => panic!(
            "three components against a declared dim of 2 must refuse naming both numbers, got {other:?}"
        ),
    }
}

// ---------------------------------------------------------------------------
// The local-model seam: `LocalModelProvider`, which owns the API and has no runtime in it yet
// (`docs/design-m2-local-embeddings.md` §3, §4, §6, §9 item 1).
// ---------------------------------------------------------------------------

/// An identity of the shape §4 mandates — quantisation inside the id, `@n` on the end — so a fixture that
/// drifted back to `embeddinggemma-2-text@1` would stop exercising the thing that made ids carry quantisation.
const LOCAL_ID: &str = "embeddinggemma-2-text-q8_0@1";

/// The stored width as amended on 2026-10-29: **native 768, no MRL truncation** — the human's decision, recorded in
/// `todo.md` and `docs/design-m2-local-embeddings.md` §5. It used to be 512 via MRL, and this constant is where that
/// survived longest: a fixture that keeps asserting "the locked width is 512" after the amendment is code arguing with
/// its own design doc.
const LOCAL_DIM: usize = 768;

/// A prefix that is **not** empty and has no trailing whitespace of its own. Both halves matter: an empty
/// `prefix_doc` would let a provider that ignores prefixes pass every assertion below, and a prefix ending in
/// a space would hide behind a separator the provider invented for it.
const LOCAL_PREFIX: &str = "task: search_document|";

/// One installed row, with the columns this seam reads filled the way `np models install` fills them.
fn installed_row(id: &str, dim: i64, prefix_doc: &str) -> ModelRow {
    ModelRow {
        id: id.to_string(),
        dim,
        sha256: Some("0".repeat(64)),
        prefix_doc: prefix_doc.to_string(),
        prefix_query: "task: search_query|".to_string(),
        installed_at: 1_730_000_000,
    }
}

/// A real artifact file with a few bytes in it. The provider stats it and reads nothing else — writing the
/// 309.9 MB §1 measures would be a fixture claiming more than this round does.
fn installed_artifact(dir: &Path) -> PathBuf {
    let path = dir.join(LOCAL_ID);
    fs::write(&path, b"gguf").expect("test setup: write the artifact file");
    path
}

/// Recorded so a future check cannot be built on a comfortable lie. A row with `sha256 = NULL` is
/// `ArtifactState::NoArtifactExpected` — "a provider with no local weights" — and construction **does not look at
/// the digest at all**, so such a row builds a provider whenever some file happens to sit where the caller pointed.
/// Those are unattested bytes accepted because a path resolved: not a serviceable state (`provider = "none"` is the
/// spelling for a model with no weights), and explicitly *not* what R6b0's digest decision would catch either, since
/// there is no digest to compare against. If that round makes construction hash, this test is the one that says the
/// NULL arm still needs its own rule.
#[test]
fn a_row_with_no_digest_and_a_file_where_the_caller_pointed_builds_a_provider() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let artifact = installed_artifact(tmp.path());
    let mut row = installed_row(LOCAL_ID, LOCAL_DIM as i64, LOCAL_PREFIX);
    row.sha256 = None;

    let provider = LocalModelProvider::new(&row, &artifact, LOCAL_DIM)
        .expect("today: construction never inspects `sha256`, so the file alone satisfies it");
    assert_eq!(
        provider.model_id(),
        LOCAL_ID,
        "identity still comes from the row"
    );
    assert_eq!(
        provider.artifact(),
        artifact,
        "and the artifact is whatever path was handed in — attested or not"
    );
}

#[test]
fn the_local_provider_refuses_and_names_the_model_that_stayed_owed() {
    // §9 item 1's placeholder for R6b1's forward pass. The refusal is asserted as a *value* on a non-empty
    // batch, never by `should_panic`: CI runs `--ignored` in release (F16's lesson), and an authoring path
    // that panics poisons a green run just as surely as one that lies.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let artifact = installed_artifact(tmp.path());
    let provider = LocalModelProvider::new(
        &installed_row(LOCAL_ID, LOCAL_DIM as i64, LOCAL_PREFIX),
        &artifact,
        LOCAL_DIM,
    )
    .expect("a row, a width that agrees with it and an artifact on disk build a provider");

    let err = provider
        .embed(&["eine Notiz", "another note"])
        .expect_err("this build has no runtime, so nothing can be embedded");

    match &err {
        EmbedError::RuntimeNotBuilt { model_id } => assert_eq!(
            model_id, LOCAL_ID,
            "the refusal names the row it was built from, or a report cannot say which model stayed owed"
        ),
        other => panic!("expected RuntimeNotBuilt, got {other:?}"),
    }
    assert!(
        err.to_string().contains("not built yet"),
        "the message has to say the runtime is missing rather than the model: {err}"
    );

    // Mutations this catches: `Ok(vec![])` (a scan then reports 0 chunks owed for a corpus it never
    // embedded), and a refusal that dropped the id — the first fails on `expect_err`, the second on the match.
}

#[test]
fn identity_width_and_artifact_come_from_the_row_handed_in() {
    // Two rows, one artifact file. A provider that hard-coded an id, a width or a path would answer one of
    // these pairs wrongly — which is §3's "no second source of identity exists", the rule its whole
    // constructor exists to enforce.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let artifact = installed_artifact(tmp.path());

    for (id, dim) in [(LOCAL_ID, LOCAL_DIM), ("mini-l12-v2@1", 3)] {
        let row = installed_row(id, dim as i64, LOCAL_PREFIX);
        let provider = LocalModelProvider::new(&row, &artifact, dim)
            .unwrap_or_else(|err| panic!("row `{id}` at width {dim} must build a provider: {err}"));
        assert_eq!(provider.model_id(), id, "model_id IS models.id, verbatim");
        assert_eq!(provider.dim(), dim, "dim IS models.dim");
        assert_eq!(
            provider.artifact(),
            artifact,
            "the artifact is the one handed in"
        );
    }

    // Mutations this catches: anything that stops answering *from the row* — `model_id()` returning the fixture
    // constant, `dim` hard-coded to `LOCAL_DIM`, `artifact` returning a path built from the id. The loop's second
    // pair (`mini-l12-v2@1`, width 3) is what kills each of those; a single-row version of this test would not.
    //
    // What it does NOT catch, recorded because an earlier draft claimed the opposite: storing `configured` instead
    // of `row.dim`. `new` refuses unless the two are equal, so no observable difference exists — that equivalence is
    // a property of the constructor, and only a constructor-level change (accepting unequal widths) would make it
    // testable here.
}

#[test]
fn a_row_of_a_different_width_is_refused_naming_both_numbers() {
    // §6: "Wrong-dim provider is refused before any write, naming both numbers". The 768 case is the one this
    // project nearly shipped — native width 768 against the locked stored 512 (§5) — and the negative row is a
    // hand-edited database, which `ModelRow`'s own note says must be *reported* rather than narrowed.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let artifact = installed_artifact(tmp.path());

    // None of these can equal `configured`: 768 became the *accepted* width when §5 was amended, so the rejected
    // arms are now the old locked 512, a small legal width, and a negative one a hand-edited row could hold.
    for stored in [512_i64, 256, -3] {
        let err = LocalModelProvider::new(
            &installed_row(LOCAL_ID, stored, LOCAL_PREFIX),
            &artifact,
            LOCAL_DIM,
        )
        .expect_err("a row whose width is not the configured one builds nothing");
        match &err {
            EmbedError::ProviderDimMismatch {
                configured: LOCAL_DIM,
                provider,
            } => assert_eq!(
                *provider, stored,
                "the refusal carries the row's own number"
            ),
            other => {
                panic!("expected ProviderDimMismatch for a row saying {stored}, got {other:?}")
            }
        }
        let message = err.to_string();
        assert!(
            message.contains(&LOCAL_DIM.to_string()) && message.contains(&stored.to_string()),
            "both numbers, in the message a human reads: {message}"
        );

        // Mutations this catches: relaxing the guard (`Ok(dim) => dim`, which builds a provider over a row that
        // disagrees with the config), or reporting `configured` where the payload takes `row.dim` — the second prints
        // 512 twice, and the assertion above on `-3` is what dies.
        //
        // What it does NOT catch: narrowing only the *comparison* (`row.dim as usize == configured`). The refusal
        // still happens and the payload is still `i64`, so both numbers still print. Verified by mutation rather than
        // argued: that mutant compiles and stays green. Keeping the payload `i64` is what makes the message honest;
        // a future change that narrows the payload as well must add its own test.
    }
}

#[test]
fn a_missing_artifact_is_refused_at_construction_as_a_missing_file() {
    // A live row whose file was deleted is `np models list`'s `missing` state (§3 of the install design), and
    // the provider must refuse it rather than build something that fails later, deeper, and with a runtime's
    // own words in the message.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let gone = installed_artifact(tmp.path());
    fs::remove_file(&gone).expect("test setup: delete the artifact again");

    let err = LocalModelProvider::new(
        &installed_row(LOCAL_ID, LOCAL_DIM as i64, LOCAL_PREFIX),
        &gone,
        LOCAL_DIM,
    )
    .expect_err("a row with no artifact behind it builds nothing");

    match &err {
        EmbedError::ArtifactUnreadable { path, source } => {
            assert_eq!(
                path, &gone,
                "the refusal names the artifact that is not there"
            );
            // The kind, not just the variant: this file's doc claims `NotFound` distinguishes *missing* from
            // *unreadable*, and a claim no test reads would rot the first time someone wrapped the error.
            assert_eq!(
                source.kind(),
                std::io::ErrorKind::NotFound,
                "a deleted artifact must still say so through its own kind"
            );
        }
        other => panic!("expected ArtifactUnreadable, got {other:?}"),
    }
    assert!(
        err.to_string().contains(LOCAL_ID),
        "the message has to name the file a human can go and re-install: {err}"
    );

    // Mutation this catches: deleting the `fs::metadata` check (construction succeeds, and the failure moves
    // into R6b1's loader), or swallowing the error into `ArtifactNotLoadable`, which claims bytes were read.
}

#[test]
fn a_directory_is_not_an_unreadable_artifact_it_is_a_directory() {
    // The lesson `models::install` learned in Windows CI: a directory is not a zero-length artifact, and the
    // two platforms disagree about what reading one even means. So the provider names the kind rather than
    // reporting a size it never measured.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let dir_as_artifact = tmp.path().join(LOCAL_ID);
    fs::create_dir(&dir_as_artifact).expect("test setup: make a directory named like the id");

    let err = LocalModelProvider::new(
        &installed_row(LOCAL_ID, LOCAL_DIM as i64, LOCAL_PREFIX),
        &dir_as_artifact,
        LOCAL_DIM,
    )
    .expect_err("a directory is not a model file");

    match &err {
        EmbedError::ArtifactUnreadable { source, .. } => assert_eq!(
            source.to_string(),
            "not a regular file",
            "the same wording `models::install` uses, so one kind of \"not a file\" keeps one name"
        ),
        other => panic!("expected ArtifactUnreadable for a directory, got {other:?}"),
    }

    // Mutation this catches: dropping the `is_file` arm, which lets a directory through to be "loaded".
}

#[test]
fn the_rows_prefix_is_prepended_by_the_provider_and_by_nobody_else() {
    // §3's rule with teeth: `embed(&[&str])` takes no prefix argument, so if the provider does not apply the
    // row's `prefix_doc`, either nothing applies it or every caller re-implements it. The witness is the
    // recording [`CountingEmbedder`] — a stand-in for R6b1's runtime — and what it is *asked* is the assertion.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let artifact = installed_artifact(tmp.path());
    let row = installed_row(LOCAL_ID, LOCAL_DIM as i64, LOCAL_PREFIX);
    let provider = LocalModelProvider::new(&row, &artifact, LOCAL_DIM)
        .expect("test setup: a provider over the whole row");

    // The caller's own batch carries no prefix. That is the point of asserting it here: a test that handed in
    // already-prefixed text could not tell who added the second copy.
    let notes = ["eine Notiz", "another note"];
    let inputs = provider.model_inputs(&notes);
    assert_eq!(
        inputs,
        vec![
            format!("{LOCAL_PREFIX}eine Notiz"),
            format!("{LOCAL_PREFIX}another note")
        ],
        "the stored prefix sits in front of the chunk's own text, with nothing added between them"
    );

    let runtime = CountingEmbedder::new();
    let refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
    runtime
        .embed(&refs)
        .expect("the recording fake always serves");
    // Harness self-check, not production coverage: `CountingEmbedder` records verbatim what it is handed and
    // `refs` borrows from `inputs`, so only an edit to *this file* can fail the assertion below. The production
    // weight of the test lives entirely in the `inputs` assertion above. It stays because it documents the intent —
    // what reaches the model is exactly what the provider prefixed — and because R6c's write boundary is where the
    // fence this cannot be gets added: asserting `text.matches(PREFIX).count() == 1` over raw chunk text at the call
    // site, which is the only shape that catches a caller prepending a second time.
    assert_eq!(
        runtime.texts(),
        inputs,
        "harness self-check: the fake was handed exactly the strings the provider built"
    );

    // Identity travels with the row, not with whatever implements the trait: the fake's own id is a different
    // string, and `np embed` writes rows under the provider's.
    assert_eq!(provider.model_id(), LOCAL_ID, "model_id IS the row's id");
    assert_ne!(
        runtime.model_id(),
        provider.model_id(),
        "the fake proves nothing about identity by agreeing with it"
    );

    // Mutations this catches: returning `text.to_string()` (prefix dropped — the bug §3 names), appending
    // instead of prepending, and `format!("{prefix} {text}")` (a separator nobody declared); each fails the
    // `inputs` assertion, and the third also fails on a prefix chosen so that a space would be visible.
}

#[test]
fn the_runtime_side_refusals_carry_their_numbers_and_no_input() {
    // The two variants R6b2 and R6b1 will raise have no production raiser yet, so what is fenced here is their
    // **payload contract** (§3: "typed, secret-free, input-free"): a budget refusal must be actionable from its
    // two numbers alone, and a not-loadable refusal must name the file without claiming anything was read.
    // Rename either payload field or drop a number from the text and this dies here rather than in R6b2's report.
    let budget = EmbedError::TokenBudgetExceeded {
        tokens: 2600,
        budget: 2048,
    };
    let message = budget.to_string();
    assert!(
        message.contains("2600") && message.contains("2048"),
        "both numbers or the reader cannot tell how far over it was: {message}"
    );
    assert!(
        message.contains("max_tokens"),
        "and which key to go and raise: {message}"
    );
    assert!(
        message.contains("refused rather than truncated"),
        "§5's posture belongs in the message, since a silent truncation is the defect: {message}"
    );

    let artifact = Path::new("/data/workspaces/0123456789abcdef.model");
    let unloadable = EmbedError::ArtifactNotLoadable {
        path: artifact.to_path_buf(),
    };
    assert!(
        unloadable
            .to_string()
            .contains(&artifact.display().to_string()),
        "a refusal that cannot say which artifact is not a refusal anyone can act on"
    );

    // Mutations this catches: dropping `{budget}` from the budget text (a reader sees "needs 2600 tokens" and
    // no ceiling), or folding `ArtifactNotLoadable` into `ArtifactUnreadable`, which adds a `{source}` this
    // class does not have — that one fails to compile, which is why the variants are separate.
}
