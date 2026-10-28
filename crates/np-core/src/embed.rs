//! The embedding provider seam: one vector type, one trait, and the states between them.
//!
//! What lives here is the *seam* and exactly one provider — [`NoEmbeddingProvider`], the implementation
//! behind `embeddings.provider = "none"`, which is the default (`docs/design-m2-embeddings.md` §2).
//! Deliberately absent, and absent in this round rather than in this design: no HTTP client, no model
//! loader, no weight file, no base URL, no key. Those arrive with the providers that need them, and
//! until one does, nothing reachable from `np_core::scan` can contact anything — which is the property
//! §7 asks for and §9 makes a hard rule ("No LLM, no network in the indexing path").
//!
//! Three names carry the whole contract:
//!
//! - [`Vector`] — a whole number of little-endian `f32` components, i.e. exactly the shape migration 7
//!   stores (`vector BLOB CHECK (typeof(vector) = 'blob' AND length(vector) = 4 * dim)`). Its two
//!   conversions are the Rust twins of that `CHECK`, and they refuse rather than truncate (§3).
//! - [`Embedder`] — what a scan calls: "which model are you", "how wide is your vector", "embed these
//!   texts". Nothing else, so adding a provider means implementing three methods and touching no caller.
//! - [`EmbedError`] — four states, each saying one true thing. Notably `Unavailable`'s payload is an
//!   opaque label rather than an endpoint, because this text ends up in test-failure output and CI logs
//!   (§9).

/// Bytes per component: the `4` in migration 7's `CHECK (length(vector) = 4 * dim)`.
const COMPONENT_BYTES: usize = 4;

/// One embedding, held as `dim` `f32` components and stored as those components little-endian.
///
/// The dimension is the length of the buffer rather than a second field, so there is no pair to keep in
/// sync — which is precisely the coupling [`Vector::from_bytes`] has to verify against a *declared* dim
/// coming from elsewhere (the `models.dim` column, or [`Embedder::dim`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Vector(Vec<f32>);

impl Vector {
    /// The vector made of `components`, whose length is its dimension.
    pub fn new(components: Vec<f32>) -> Self {
        Self(components)
    }

    /// Number of components — the value `embeddings.dim` must hold for this vector, and the value
    /// [`Embedder::dim`] promises.
    pub fn dim(&self) -> usize {
        self.0.len()
    }

    /// The bytes to store: every component little-endian, so exactly `4 * dim()` of them.
    ///
    /// Little-endian is not a preference but the schema's rule (§3): the stored form is raw `f32`, never
    /// hex or JSON, and a writer that picked the other order would produce vectors whose cosines are
    /// garbage while every length check still passed.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.0.len() * COMPONENT_BYTES);
        for component in &self.0 {
            bytes.extend_from_slice(&component.to_le_bytes());
        }
        bytes
    }

    /// Read stored bytes back as a [`Vector`] of the declared `dim`, refusing anything
    /// [`Vector::to_bytes`] could not have written.
    ///
    /// Two refusals, in this order, and the order is the point:
    ///
    /// 1. trailing bytes past the last whole component mean the buffer cannot describe *any* vector, so
    ///    it is [`EmbedError::Malformed`] — never dressed up as a wrong dimension;
    /// 2. a well-formed buffer holding a different component count than `dim` is
    ///    [`EmbedError::DimensionMismatch`].
    ///
    /// Ruling out the ragged case first is what keeps it from falling through into case 2 and, worse,
    /// into silently succeeding: reading 10 bytes as components drops the trailing pair, so a `dim = 2`
    /// row would hand back a two-component vector and never mention the two bytes it swallowed.
    pub fn from_bytes(bytes: &[u8], dim: usize) -> Result<Self, EmbedError> {
        let (windows, trailing) = bytes.as_chunks::<COMPONENT_BYTES>();
        if !trailing.is_empty() {
            return Err(EmbedError::Malformed { len: bytes.len() });
        }
        if windows.len() != dim {
            return Err(EmbedError::DimensionMismatch {
                expected: dim,
                got: windows.len(),
            });
        }
        Ok(Self(
            windows
                .iter()
                .map(|window| f32::from_le_bytes(*window))
                .collect(),
        ))
    }
}

/// Why one call to [`Embedder::embed`] did not produce vectors.
///
/// Every variant states a distinct fact, because the scan's report has to distinguish "nothing was
/// embedded because nobody chose a provider" (§2) from "something tried and failed" (§5) — those two
/// become different sentences in the catalog's durable-record line, and one of them is not an error at
/// all.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// No provider was chosen (`embeddings.provider = "none"`), so nothing was embedded and nothing was
    /// contacted. This is the default state of a fresh install, reported as a count rather than as a
    /// fault (§2, §5).
    #[error("embedding disabled: embeddings.provider = \"none\"")]
    Disabled,
    /// A provider exists and could not serve this call — model not installed, endpoint down, out of
    /// memory. Not version debt: §5's P0 is that availability never becomes `EMBED_VERSION` debt, so the
    /// scan reports the count and keeps its chunks.
    ///
    /// The payload is an **opaque label** ("openai-compatible", a host category, anything but the URL):
    /// this enum's text lands in test-failure output and CI logs, so an error that formatted the
    /// endpoint would leak it on the first flake (§9).
    #[error("embedding provider unavailable [{0}]")]
    Unavailable(String),
    /// A vector whose component count is not the declared one. `expected` is what was declared
    /// ([`Embedder::dim`], or `models.dim` for a stored row), `got` what the buffer actually holds; both
    /// count **components**, never bytes.
    #[error("vector dimension mismatch: expected {expected} components, got {got}")]
    DimensionMismatch { expected: usize, got: usize },
    /// A buffer that is not a whole number of 4-byte components, which no vector has ever been — `len`
    /// is its byte length.
    ///
    /// It gets its own variant instead of folding into [`EmbedError::DimensionMismatch`] because there is
    /// no honest component count to report for a ragged buffer: rounding 10 bytes down says "got 2" next
    /// to "expected 2" for a `dim = 2` row, i.e. a refusal that reads like nothing happened, and rounding
    /// up collides the same way just below. A message that under-reads as a no-op is the exact defect
    /// class this project's F-series exists to remove, so the truth costs one variant instead of a
    /// permanent footnote.
    #[error("malformed vector buffer: {len} bytes is not a whole number of f32 components")]
    Malformed { len: usize },
}

/// Anything that can turn texts into vectors — in-process model, HTTP endpoint, or a test's stand-in.
///
/// The interface is three methods and no async, no builder, no config type: a scan needs to know which
/// model it is writing rows under ([`Embedder::model_id`]), how wide the result is
/// ([`Embedder::dim`], checked against [`Vector::dim`] at write time by the schema's `CHECK`), and the
/// vectors themselves ([`Embedder::embed`]). Providers are `&self`-callable so a scan can hold one and
/// batch through it (§5: vector writes commit per batch, outside the scan's transaction).
pub trait Embedder {
    /// Model identity exactly as it belongs in `models.id` / `embeddings.model`: id plus version,
    /// verbatim, one fact in one place (§3, §4).
    fn model_id(&self) -> &str;

    /// Component count every returned [`Vector`] must have. A provider that returns anything else has a
    /// bug, and the mismatch is [`EmbedError::DimensionMismatch`] rather than a truncated row.
    fn dim(&self) -> usize;

    /// Embed `texts`, returning one vector per text in the same order.
    ///
    /// Implementations that cannot serve return an [`EmbedError`]; they never panic on absence of a
    /// model or an endpoint (§7: CI runs `--ignored`, so a panicking path poisons a green run).
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vector>, EmbedError>;
}

/// The provider behind `embeddings.provider = "none"` (§2's default): it refuses, and could not do
/// otherwise.
///
/// **This type holds no client, no base URL, no key, and no field of any kind.** That is the
/// load-bearing property rather than an accident of emptiness: §7 wants "`none` contacts nothing" to be
/// a fact about the *type* — where a later reader cannot add a connection pool, a `reqwest::Client`, or
/// a `String` of URL by oversight — and not about one code path that happens not to dial out. A unit
/// struct has nowhere to put those, so "constructs no HTTP client at all" needs no test that watches the
/// network; `tests/embed.rs` pins the emptiness itself with `size_of::<NoEmbeddingProvider>() == 0`, so
/// the first field anyone adds here is a decision that has to be argued for in review.
#[derive(Debug)]
pub struct NoEmbeddingProvider;

impl Embedder for NoEmbeddingProvider {
    /// `"none"` — the same spelling as the config value, and usable only as a report label: this
    /// provider never produces a row, so no `models.id` is derived from it (§3).
    fn model_id(&self) -> &str {
        "none"
    }

    /// 0: there is no vector width to declare. A caller that reads this before calling
    /// [`Embedder::embed`] still gets [`EmbedError::Disabled`] rather than a zero-length row.
    fn dim(&self) -> usize {
        0
    }

    /// Always [`EmbedError::Disabled`], with no argument inspection and nothing to contact: the refusal
    /// is the whole behaviour (§2).
    fn embed(&self, _texts: &[&str]) -> Result<Vec<Vector>, EmbedError> {
        Err(EmbedError::Disabled)
    }
}
