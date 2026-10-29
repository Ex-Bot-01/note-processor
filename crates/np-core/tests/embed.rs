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
//!
//! Each test names the production mutation that would make it fail.

use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use np_core::embed::{EmbedError, Embedder, NoEmbeddingProvider, Vector};

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
