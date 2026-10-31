//! The embedding provider seam: one vector type, one trait, and the states between them.
//!
//! What lives here is the *seam* and its two offline providers: [`NoEmbeddingProvider`] behind
//! `embeddings.provider = "none"`, the default (`docs/design-m2-embeddings.md` §2), and
//! [`LocalModelProvider`] behind `embeddings.provider = "local-model"` — the API of a local inference
//! runtime, without one inside it yet (`docs/design-m2-local-embeddings.md` §3, §9 item 1).
//! Deliberately absent, and absent in this round rather than in this design: no HTTP client, no model
//! loader, no weight file, no base URL, no key. Those arrive with the providers that need them, and
//! until one does, nothing reachable from `np_core::scan` can contact anything — which is the property
//! §7 asks for and §9 makes a hard rule ("No LLM, no network in the indexing path").
//!
//! Four names carry the whole contract:
//!
//! - [`Vector`] — a whole number of little-endian `f32` components, i.e. exactly the shape migration 7
//!   stores (`vector BLOB CHECK (typeof(vector) = 'blob' AND length(vector) = 4 * dim)`). Its two
//!   conversions are the Rust twins of that `CHECK`, and they refuse rather than truncate (§3).
//! - [`Embedder`] — what a scan calls: "which model are you", "how wide is your vector", "embed these
//!   texts". Nothing else, so adding a provider means implementing three methods and touching no caller.
//! - [`EmbedError`] — one variant per thing that can go wrong, each saying one true thing. Notably
//!   `Unavailable`'s payload is an opaque label rather than an endpoint, because this text ends up in
//!   test-failure output and CI logs (§9).
//! - [`LocalModelProvider`] — the type `local-model` resolves to, **feature or no feature**: the cargo feature builds a
//!   runtime, it does not change this provider's refusal, which stays unconditional until R6c wires storage. A [`ModelRow`], an
//!   artifact that exists, the row's document prefix, and a refusal where the forward pass goes. Nothing
//!   constructs it from a config yet; R6c wires `[embeddings]` to a provider (§9's split), so today it is
//!   reachable API only.
//! - `local_model::LocalEmbedder` (feature `local-model`, so not a link: this module documents both builds) —
//!   the same shape **with** the runtime inside: one loaded model, the row's two
//!   prefixes, a token budget, and real vectors from R6b2 onward. The two types stay separate because the seam's
//!   honest refusal is what a default build must keep answering, and a `Result` inside one provider would let
//!   "this build has no runtime" quietly become "this call happened to fail".
//!
//! The heavy dependency stays behind the `local-model` cargo feature (`crates/np-core/Cargo.toml`), while
//! everything in this module compiles by default: §9 item 1 wants CI and callers to exercise the seam
//! *while* the runtime is being chosen, and a refusal that only compiles under a feature flag is a
//! refusal nobody sees.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::db::ModelRow;
use crate::models;
use crate::walk;

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

    /// The components themselves, in order.
    ///
    /// The read side of [`Vector::to_bytes`]: a stored row is bytes, an in-memory one is numbers, and anything
    /// that compares two vectors (R6d's cosine ranking) needs this rather than a round trip through the storage
    /// form. Borrowed, not copied — a 768-component row is 3 KiB and a search reads thousands of them.
    pub fn components(&self) -> &[f32] {
        &self.0
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

/// Why one call to [`Embedder::embed`] — or one attempt to build a provider — did not produce vectors.
///
/// Every variant states a distinct fact, because the scan's report has to distinguish "nothing was
/// embedded because nobody chose a provider" (§2) from "something tried and failed" (§5) — those two
/// become different sentences in the catalog's durable-record line, and one of them is not an error at
/// all.
///
/// The taxonomy grew from four variants to nine under `docs/design-m2-local-embeddings.md` §3, which names
/// the failure classes a local runtime needs typed *before* it exists (parent §7's "four things and no
/// fifth" describes Round 3a's seam, not this one). Two rules bind every variant, old and new:
///
/// - **input-free**: none formats note text into its message. A vault is private and an error log outlives
///   the run (§3);
/// - **secret-free**: no endpoint, host, port or credential. An artifact path is neither — it names a file
///   this program wrote under the data dir, and naming it is what makes the refusal actionable, exactly as
///   [`crate::models::ModelError`] does for the same file.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// No provider was chosen (`embeddings.provider = "none"`), so nothing was embedded and nothing was
    /// contacted. This is the default state of a fresh install, reported as a count rather than as a
    /// fault (§2, §5).
    #[error("embedding disabled: embeddings.provider = \"none\"")]
    Disabled,
    /// A provider exists and could not serve this call — no artifact installed, no memory to load a model
    /// with. Not version debt: §5's P0 is that availability never becomes `EMBED_VERSION` debt, so the scan
    /// reports the count and keeps its chunks.
    ///
    /// The payload is an **opaque label** (a provider kind, a host category, anything a secret could not
    /// take): this enum's text lands in test-failure output and CI logs, so an error that formatted one
    /// would leak it on the first flake (§9). R6b1's loader and R6b2's forward pass raise it — `local-model`'s
    /// refusals are the specific ones below, which is what §10 of `docs/design-m2-local-embeddings.md` did to the
    /// class this variant was sketched for: there is no endpoint left that could be down.
    ///
    /// **Its home is §3's fifth class**, "runtime error with a bounded message": every label is one we chose, never
    /// the runtime's own diagnostic text — that text is exactly what this payload rule exists to keep out of logs,
    /// and [`EmbedError::ArtifactNotLoadable`] deliberately declines to be its carrier.
    ///
    /// **Every label `local-model` emits, verbatim** — twenty-two, spelled out in full because this list is the only
    /// place a reader can see what the runtime may answer, and a grep for any of them should find the string. It
    /// replaced an earlier version that listed six and described the rest as "planned, not yet reachable" for two
    /// rounds after the code could reach them.
    ///
    /// From `local_model::load` (ten):
    /// `"the model reported no output width"`, `"the model reported no input width"`,
    /// `"the model reported no training context"`, `"the model reported an empty vocabulary"` (one guard, four
    /// labels), `"the loaded model exposed no vocabulary"` (raised from two sites — reading the model's facts and
    /// resolving the tokenizer's vocabulary), `"the runtime refused to build a context for this model"`,
    /// `"this artifact cannot produce a sentence vector: the context pools with NONE"`,
    /// `"this artifact produces a score, not a sentence vector"` (RANK pooling: its row is `n_cls_out` wide, not
    /// the stored width — see `crate::local_model::Pooling`), and
    /// `"the runtime reported a pooling strategy this build does not know"`, and
    /// `"this context cannot hold a batch as wide as its own window"` — raised when the built context reports an
    /// `n_ubatch` narrower than the window we asked for, the fourth cheap refusal in `load`'s list (A1 review P1-1).
    /// It cannot fire on the pinned artifact, where llama.cpp preserves the three equal values; it exists so a re-pin
    /// that changes that arithmetic answers with a sentence instead of `src/llama-context.cpp:1631`.
    ///
    /// From `local_model::LoadedModel::embed_text` (twelve):
    /// `"the sequence produced no tokens for the model to read"`,
    /// `"the sequence is too long to hand to the tokenizer"`, `"the tokenizer refused to count this sequence"`,
    /// `"the tokenizer's count did not fit its own buffer"`, `"the tokenizer wrote into a buffer with no room"`,
    /// `"the sequence is longer than this context can hold"` (over the window; worded so the reader shortens a
    /// chunk instead of freeing RAM), `"the runtime reported a full KV window for this sequence"`,
    /// `"the runtime refused to run this sequence"`, `"the runtime produced no pooled embedding"`,
    /// `"the model produced a component that is not a number"`, `"the model produced a zero vector"`, and
    /// `"the model produced a vector too large to normalise"`.
    ///
    /// Four of those twelve — the tokenizer-path four (`too long to hand to the tokenizer`, `refused to count`,
    /// `did not fit its own buffer`, `wrote into a buffer with no room`) — are raised inside the shared
    /// tokenization helper, so they are equally reachable from `LoadedModel::count_tokens`. Listing them under
    /// `embed_text` alone is the shorter story, and R6b2's re-review asked which one it is (P2-9).
    ///
    /// Two honesty notes, because a list of what can happen invites reading it as a list of what has: **the KV
    /// window label cannot fire on this artifact** (it has no KV cache — `create_memory` returns null, so
    /// `llama_decode` diverts to the encoder path, which answers 0, `2` for an aborted run, or negative — never a
    /// `1`), and the last three normalisation
    /// labels have never been produced by any input here (§8's "untested" list in the round report says so too).
    /// They exist because storing such a row is how a search silently returns nothing.
    ///
    /// Not one of them is llama.cpp's diagnostic text, which is §3's fifth class in the abstract and this payload
    /// rule in practice. If a failure arrives that none of the twenty-two describes, the honest move is a
    /// twenty-third label named for that class — not a wider payload here.
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
    /// The `local-model` provider was asked to embed and cannot: this build contains no inference
    /// runtime. `model_id` is the row it was built from, so the refusal says which model stayed owed.
    ///
    /// This is the seam's placeholder for R6b1's forward pass, and it has to stay an error rather than an
    /// empty batch: `Ok(vec![])` would let a report say "0 chunks needed embedding" about a corpus that was
    /// never embedded — F21's false status claim with a different noun.
    #[error(
        "the local embedding runtime is not built yet: model `{model_id}` was asked to embed and produced nothing"
    )]
    RuntimeNotBuilt { model_id: String },
    /// The artifact a `local-model` row names is missing, or is not a file that can be read.
    ///
    /// One variant for both halves, because the source's own [`ErrorKind`](std::io::ErrorKind) already says
    /// which (`NotFound` is the missing case) and a second variant would draw a distinction no caller acts
    /// on.
    ///
    /// Not folded into [`crate::models::ModelError::Artifact`], which is the reuse §3's "existing
    /// `ModelError`" line invites: that variant's message says the artifact could not be read **to compare
    /// it with its row**, and this provider compares nothing *today* — see [`LocalModelProvider::new`] for the
    /// digest check that is R6b0/R6b1's to take, and which would make this the right variant to reuse. Claiming
    /// an operation that did not happen is the defect `models.rs` refuses on its own
    /// [`crate::models::ArtifactState::Mismatch`]. Naming the path is safe: a data-dir file, never note
    /// text and never a vault path.
    #[error("cannot read the model artifact {}: {source}", .path.display())]
    ArtifactUnreadable {
        /// The artifact the row names and this call could not read.
        path: PathBuf,
        /// Why not — `NotFound` means a live row with no file, which `np models list` calls
        /// [`crate::models::ArtifactState::Missing`].
        source: io::Error,
    },
    /// An artifact that **is** readable is not loadable as a model by any runtime this build has.
    ///
    /// Distinct from [`EmbedError::ArtifactUnreadable`]: there the bytes could not be reached, here they
    /// were reached and meant nothing. `local_model::load` raises it when llama.cpp returns null — bad magic, an unknown
    /// architecture, or a file whose tensor data runs past its end (measured: a 310 MB artifact truncated to
    /// 128 MiB comes back null rather than aborting) — and it is deliberately
    /// not the home for a runtime's own diagnostic text: §3 keeps "runtime error with a bounded message"
    /// as its own class, and giving that a payload no runtime has produced yet would be a variant written
    /// backwards.
    #[error("{} is not loadable as a model by this build", .path.display())]
    ArtifactNotLoadable { path: PathBuf },
    /// A sequence needs more tokens than `embeddings.max_tokens` allows, so it was refused instead of
    /// truncated.
    ///
    /// Truncation is the thing this variant exists to forbid (`docs/design-m2-local-embeddings.md` §5): a
    /// truncated chunk stamps `source_hash = <the whole chunk's digest>` onto a vector that read part of it
    /// — "a row claiming currency for bytes it never saw". The message carries both numbers and no text:
    /// the reader needs to know how far over, and must not need the note to say so.
    ///
    /// Raised by `local_model::LoadedModel::embed_text` (R6b2) over the count **llama.cpp's tokenizer
    /// returned**, never a byte length — `tokens` for a 23-byte sentence on this artifact is 9, and a
    /// byte-based check would refuse text that fits while accepting multibyte text that does not.
    #[error(
        "sequence needs {tokens} tokens, over the embeddings.max_tokens budget of {budget}; refused rather than truncated"
    )]
    TokenBudgetExceeded {
        /// What the prefixed sequence measures.
        tokens: usize,
        /// The `embeddings.max_tokens` it broke.
        budget: usize,
    },
    /// The `models` row a provider was built from declares a different width than the run needs.
    /// `configured` is what the caller needs vectors to be (`[embeddings] dim`, or the width already stored
    /// under this id); `provider` is what the row says it will produce.
    ///
    /// Refusing at construction is the point (§6: "Wrong-dim provider is refused before any write, naming
    /// both numbers — today a mismatch surfaces as a raw `DbError` from a CHECK constraint mid-run"). It is
    /// also not [`EmbedError::DimensionMismatch`], which is about a *buffer* whose component count
    /// disagrees: this is two declarations disagreeing, and once there is a buffer to compare, the write
    /// this variant forecloses has already been attempted.
    ///
    /// `provider` stays an `i64` because that is SQLite's `INTEGER`: a hand-edited row holding a negative
    /// or absurd width must be **reported**, not narrowed into a `usize` that would print something else —
    /// the reasoning [`ModelRow`] gives for its own column types.
    #[error(
        "the model declares {provider} dimensions but this run needs {configured}; refused before any write, because one model id cannot mean two widths"
    )]
    ProviderDimMismatch {
        /// The width the caller asked this provider to serve.
        configured: usize,
        /// The width the model itself answers with (`llama_model_n_embd_out`), or the installed row's `dim` when
        /// the refusal came from a provider built against a row. Not always "from the database": R6b1's loader
        /// raises this before any row is consulted, so the sentence below says "the model declares", not
        /// "the row declares" (R6b1 review P2-3).
        provider: i64,
    },
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

    /// The exact strings this provider would place in front of the model for `texts`, one per text, in order.
    ///
    /// **Why this is on the trait rather than spelled out by the caller.** A writer has to record what the model
    /// read, and only the provider knows how it dresses a chunk — `prefix_doc` today, whatever a future provider
    /// does. R6c2's P1 was that division going wrong: `embed_run` prepended `prefix_doc` itself while
    /// `local_model::LocalEmbedder` (behind the `local-model` feature) prepended it again, so documents reached llama.cpp with the
    /// instruction twice and the stored stamp covered one copy of a string the model saw twice. Both halves were
    /// defensible alone; nothing asked the pair the same question. This method is that question, and it is
    /// **required** rather than defaulted to identity so no provider can opt out by forgetting to implement it.
    ///
    /// The contract: [`Self::embed`] must read the model on exactly the bytes this returns, in the same order, with
    /// nothing added or removed in between. A provider whose dressing changes — a re-installed row with a new
    /// `prefix_doc`, say — therefore invalidates what it stored, which is §3's rule about prefixes turning into
    /// lies enforced by a digest instead of by hope.
    fn model_inputs(&self, texts: &[&str]) -> Vec<String>;

    /// Embed `texts`, returning one vector per text in the same order.
    ///
    /// The bytes the model reads are [`Self::model_inputs`] applied to `texts`: a provider that prefixes does it
    /// here as well, from the same rule, and a caller that pre-applies anything is double-applying it.
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
    /// Identity — and it never matters, because this provider refuses every batch
    /// ([`EmbedError::Disabled`]) so no bytes of its making reach a model. Implemented rather than defaulted
    /// because the writer stamps whatever this returns, and a placeholder claiming "the model would read exactly
    /// this" about a provider that embeds nothing is the sort of sentence §3 exists to stop.
    fn model_inputs(&self, texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_string()).collect()
    }

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

/// The provider behind `embeddings.provider = "local-model"`: one installed [`ModelRow`], the artifact it
/// names, and no inference.
///
/// **Why it exists before it can work:** every part of a local provider that is *not* the forward pass is a
/// decision R6b1 cannot cheaply undo — where identity comes from, who applies the prefix, which width
/// disagreement is refused and when — so those are built and tested now, while getting them wrong costs a
/// rename rather than a corpus of vectors no reference can re-derive
/// (`docs/design-m2-local-embeddings.md` §3, §9 item 1).
///
/// Three rules it carries, each forced by the shape of [`Embedder`]:
///
/// - **Identity comes from the row.** [`Embedder::model_id`] *is* `models.id`, so no second source of model
///   identity exists (§3, §4). That is why construction takes a row rather than a bare path: a provider built
///   from a path alone would have to invent an id or let every caller supply its own.
/// - **The prefix is the provider's job.** `embed(&[&str])` takes no prefix argument, so either the
///   constructor carries `prefix_doc` and [`Embedder::model_inputs`] applies it, or the prefix gets
///   applied nowhere — and "a provider that ignores prefixes is a bug" (parent §3).
/// - **It refuses.** Until R6b1 there is no runtime, so [`Embedder::embed`] answers
///   [`EmbedError::RuntimeNotBuilt`] naming the model id: never an empty batch, never a zero vector, never a
///   silent no-op.
///
/// What it does **not** do: load anything, hold weights, or read a note. Construction stats the artifact and
/// stops there — existence is checkable today, loading is R6b1's (§3: "Loading is explicit and bounded. One
/// load per process, never per note"). `Clone` is deliberately **not** derived: today the struct holds only
/// metadata, so a derive would be harmless, and in R6b1 it would either deep-copy weights or become a breaking
/// removal. Nothing clones a provider (asserted by this round compiling without the derive).
#[derive(Debug)]
pub struct LocalModelProvider {
    /// `models.id`, verbatim.
    model_id: String,
    /// The row's declared width, which [`LocalModelProvider::new`] refused to build anything over.
    dim: usize,
    /// `models.prefix_doc`: what every stored vector was prefixed with (parent §3 — "`prefix_doc` describes
    /// every row of `embeddings`"), and therefore what this provider must prepend.
    prefix_doc: String,
    /// The artifact file, as the caller resolved it from
    /// [`Config::workspace_models_dir`](crate::config::Config::workspace_models_dir).
    artifact: PathBuf,
}

/// The up-front fact about an artifact file: it is there, and it is a regular file.
///
/// Both local provider shapes ask it before any model work — `LocalModelProvider::new` because that is all it
/// does, and `local_model::LazyLocalEmbedder::new` (behind the `local-model` feature, so named in prose rather
/// than linked: this item has to resolve in a default build too) because §6's *artifact absent* arm must be settled
/// **before** [`crate::embed_run`] is asked whether the workspace owes anything. One function is what keeps that
/// arm one sentence in both builds, which is R6c1 D-2's rule: a missing file is a fact about the filesystem, never
/// a configuration complaint.
///
/// The spelling rules are `models`' own — absolute, no symlink resolution, prefixed when long — so an artifact
/// under a workspace that crosses the Windows threshold is still statable here instead of refused with a raw OS
/// error. A directory is not a small unreadable artifact, and the platforms disagree about what reading one even
/// means ([`crate::models::install`] learned this), so it keeps that wording and this module keeps one name for
/// "that is not a file".
pub fn artifact_is_readable(artifact: &Path) -> Result<(), EmbedError> {
    let statable = walk::to_open_path(&models::access(artifact));
    let meta = fs::metadata(&statable).map_err(|source| EmbedError::ArtifactUnreadable {
        path: artifact.to_path_buf(),
        source,
    })?;
    if !meta.is_file() {
        return Err(EmbedError::ArtifactUnreadable {
            path: artifact.to_path_buf(),
            source: io::Error::new(io::ErrorKind::NotADirectory, "not a regular file"),
        });
    }
    Ok(())
}

/// The one spelling of "the bytes the model is handed": `prefix` then `text`, verbatim, nothing between.
///
/// R6c1 review D-4: this rule used to be written twice — here and as a local `format!` in
/// [`crate::embed_run`] — which is how a stamp rule quietly stops describing what the model read. Both callers go
/// through this function now, so a change to the prefix rule is one edit and both the digest and the batch move
/// together. Mutating the concatenation here fails `every_text_in_a_batch_travels_with_its_own_prefix` and
/// `a_changed_prefix_makes_a_stored_vector_stale` (measured: 17 passed / 2 failed).
pub fn prefixed(prefix: &str, text: &str) -> String {
    format!("{prefix}{text}")
}

impl LocalModelProvider {
    /// Build the provider for an installed row.
    ///
    /// `configured` is the width this run needs vectors to be — `[embeddings] dim`, or the width already
    /// stored under this id — and it is an argument rather than something read off the row because the row
    /// and it are the pair §6 requires to agree. `artifact` is where that row's bytes live.
    ///
    /// Two refusals, in this order, both before anything is loaded or written:
    ///
    /// 1. [`EmbedError::ProviderDimMismatch`] when the row's `dim` is not `configured`; a negative or
    ///    non-numeric width fails here too, because it cannot *be* `configured`;
    /// 2. [`EmbedError::ArtifactUnreadable`] when `artifact` is missing or is not a regular file.
    ///
    /// The dim check goes first because it needs no filesystem: statting the artifact to report a width
    /// disagreement would spend an operation learning what the row already said — the same ordering argument
    /// that makes [`crate::models::install`] validate the digest *format* before hashing anything.
    ///
    /// **Digest verification is an open, costed decision and this constructor does not make it.** Nothing here
    /// compares the file against `models.sha256`: construction stats one path. The case for hashing is strong —
    /// `docs/design-m2-models-install.md` states as a safety property that "nothing embeds against a mismatching
    /// row", and no code path currently enforces that — and the cost is smaller than it looks *if* §3's "one load
    /// per process, never per note" holds, because then it is one streaming SHA-256 pass over ~310 MB per `np embed`
    /// run, which [`crate::hash::sha256_file`] already does for `np models list`. It stays unmade in R6a because the
    /// install design assigned it ("re-verifying on every embed is 4b's cost decision") and because whether an
    /// mmap-first runtime even reads the whole file is a measurement (R6d), not an assumption. **R6b0/R6b1 owns it**,
    /// and when it lands it refuses with both digests in [`crate::models::ModelError::ArtifactDisagrees`]'s shape —
    /// never repairing, and never treating the check as the only thing standing between a row and a write.
    ///
    /// Related reality, recorded rather than papered over: a row whose `sha256` is `NULL`
    /// ([`crate::models::ArtifactState::NoArtifactExpected`], "a provider with no local weights") has **no digest to
    /// verify against ever**, so it can never be attested by that check either. Construction today does not look at
    /// `sha256` at all, so such a row builds a provider whenever some file happens to sit where the caller pointed —
    /// unattested bytes, accepted because a path resolved. That is a caller error, not a serviceable state: a model
    /// with no weights is spelled `provider = "none"`, and the NULL-sha fixture in `tests/embed.rs` pins the
    /// behaviour so the future digest check cannot quietly become the only fence.
    pub fn new(row: &ModelRow, artifact: &Path, configured: usize) -> Result<Self, EmbedError> {
        let dim = match usize::try_from(row.dim) {
            Ok(dim) if dim == configured => dim,
            _ => {
                return Err(EmbedError::ProviderDimMismatch {
                    configured,
                    provider: row.dim,
                })
            }
        };

        artifact_is_readable(artifact)?;

        Ok(Self {
            model_id: row.id.clone(),
            dim,
            prefix_doc: row.prefix_doc.clone(),
            artifact: artifact.to_path_buf(),
        })
    }

    /// The artifact this provider would be asked to load: the file [`EmbedError::ArtifactUnreadable`] and
    /// [`EmbedError::ArtifactNotLoadable`] name, and the one `np models list` reports a state for.
    pub fn artifact(&self) -> &Path {
        &self.artifact
    }
}

impl Embedder for LocalModelProvider {
    /// Each text with this row's `prefix_doc` in front, verbatim, with nothing added between the two: the
    /// [`Embedder::model_inputs`] contract, applied by the provider that owns the row.
    ///
    /// **This is the sequence [`crate::config::EmbeddingsConfig::max_tokens`] counts** (§5): prefix + text +
    /// special tokens, not the chunk's bytes alone. R6b2 enforces the budget and measures it over what this
    /// returns — bounding the un-prefixed text would leave every existing test green while limiting something else.
    ///
    /// The prefix is applied in exactly one place per provider, which is what makes parent §3's "a provider that
    /// ignores prefixes is a bug" checkable: `tests/embed.rs` hands the result to a recording fake and asserts the
    /// stored prefix sits in front of text the caller never touched. In this build [`Self::embed`] refuses before
    /// reaching the model, so the method's production callers are the writer's stamp and, behind the feature flag,
    /// `local_model::LocalEmbedder` (behind the `local-model` feature).
    fn model_inputs(&self, texts: &[&str]) -> Vec<String> {
        texts
            .iter()
            .map(|text| prefixed(&self.prefix_doc, text))
            .collect()
    }

    /// The installed row's `id`, verbatim: the provider adds and strips nothing, because `embeddings.model`
    /// is a foreign key to `models.id` and one spelling of an identity is the rule §4 exists to keep.
    fn model_id(&self) -> &str {
        &self.model_id
    }

    /// The row's declared width. A provider that returned anything other than what its rows say would be the
    /// §6 mismatch this type refuses at construction.
    fn dim(&self) -> usize {
        self.dim
    }

    /// Always [`EmbedError::RuntimeNotBuilt`], naming the model, with the batch untouched: this type is the seam's
    /// default half, and a build that links no C++ has no way to turn text into numbers (§9 item 1).
    ///
    /// Not a statement about the program: in a `local-model` build `np-cli` resolves
    /// `local_model::LazyLocalEmbedder` instead and vectors are computed, so this refusal is what a **default**
    /// binary answers rather than something no binary can do.
    fn embed(&self, _texts: &[&str]) -> Result<Vec<Vector>, EmbedError> {
        Err(EmbedError::RuntimeNotBuilt {
            model_id: self.model_id.clone(),
        })
    }
}
