//! Config file (`config.toml`), OS data dir, and the known-workspace registry.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::db::BUSY_TIMEOUT_MS;

/// Name of the registry database inside the data dir.
const REGISTRY_FILE: &str = "workspaces.sqlite";
/// Leading bytes of the workspace-path digest used as its database file name,
/// i.e. 16 lowercase hex characters.
const DB_HASH_BYTES: usize = 8;

/// Anything that can fail while reading configuration or the known-workspace registry.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A config file or database could not be read or created; the message names the path.
    #[error("config io: {0}")]
    Io(#[from] io::Error),
    /// The config file exists but is not valid TOML for [`Config`] — including a key it does not
    /// recognise under `[embeddings]` or `[models]`, both of which refuse one (`EmbeddingsConfig`'s and
    /// [`ModelsConfig`]'s `deny_unknown_fields`; `docs/design-m2-embeddings.md` §7 for the first, and the same
    /// argument one section later for the second).
    ///
    /// The path travels with the failure because a parse error that cannot say *which file* is broken
    /// is not actionable: this program reads one user file and takes another from the command line,
    /// and `toml::de::Error` knows its own line and column but never where it came from. The payload
    /// stays the driver's error rather than a formatted string so serde's line/caret diagnostic
    /// survives and callers can still match the kind — the same reason
    /// `tests/config.rs`'s `explicit_missing_config_file_is_an_io_error` insists that a refusal name
    /// its file.
    #[error("config parse error in {}: {source}", path.display())]
    Parse {
        /// The file whose contents were rejected.
        path: PathBuf,
        /// serde's own failure, with its line and column intact.
        #[source]
        source: toml::de::Error,
    },
    /// A value neither the config file nor the environment supplied. Every config
    /// struct is `#[serde(default)]`, so a partial file keeps defaults and this
    /// cannot fire from `Config::load` today; kept because the plan mandates it.
    #[error("missing config value: {0}")]
    MissingField(&'static str),
}

/// SQLite failures travel through [`ConfigError::Io`]: the mandated enum has no
/// dedicated variant, so the driver message is preserved as the payload.
impl From<rusqlite::Error> for ConfigError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Io(io::Error::other(err))
    }
}

/// Largest `continuation_indent` a config file may ask for.
///
/// A product bound, not one derived from [`crate::catalog`]'s column budget: the catalog wraps at 100 columns and any indent
/// below that still *renders*, so nothing here computes 16 out of 100. What is not a judgement is the need for **some** finite
/// ceiling — `wrap` adds the indent into its width budget (`catalog.rs`) and then builds `" ".repeat(indent)`, so an enormous
/// value either overflows that addition (a panic in a debug build, exit 101) or asks for gigabytes of spaces per entry. A
/// typo in a text file must not reach either, which is why the ceiling is enforced at load and not in the renderer
/// (`docs/design-f17-indent-clamp.md` §4).
pub const MAX_CONTINUATION_INDENT: usize = 16;

/// `[embeddings] max_tokens` when the key is absent (`docs/design-m2-local-embeddings.md` §5).
///
/// The default is a *budget*, not a target: an over-budget chunk is reported, never truncated (§5), so the
/// number has to sit above what the chunker normally produces while still being a fence. **1,024** is the human's
/// decision of 2026-10-30 (design §5, "decision B"), taken from R6b2's measured cost curve rather than from an
/// argument: at ~120–141 tokens/s a 1,024-token chunk is **~8 s** of CPU on this host — interpolated between the
/// two measured points (543 tok / 3.85 s and 2,163 tok / 18.0 s), so quote it as an interpolation, not a
/// measurement — while a whole corpus at ~500 tokens per note stays near the faster end of that curve. It also
/// leaves room for `prefix_doc` plus two special tokens on top of a `MAX_CHUNK_BYTES = 1800` chunk, which is what
/// the old 2048 was justified by; nothing about the chunker changed, so this is now slack rather than headroom.
///
/// Raising it is cheap to do and expensive to run: the number is a config key, so **nothing here blocks a later
/// increase** — what §5 asks for first is a re-measurement (R6d's throughput run with OpenMP decided), not a
/// constant edit.
pub const DEFAULT_EMBEDDING_TOKENS: usize = 1_024;

/// Largest `embeddings.max_tokens` a config file may ask for: **8,192 tokens**, which is the width of the context
/// this build asks llama.cpp for (`local_model::REQUESTED_CTX`), not a fact about any model card.
///
/// **Set by the human on 2026-10-30, superseding the same day's §5 option B (4,096) with option C.** What did not
/// change is the rule that made B worth having: **the ceiling equals the window this build creates**, so a config
/// value near the ceiling describes something the runtime can actually do. Raising the number alone would have
/// re-opened the exact defect B closed — `max_tokens = 8192` parsing happily and then being refused by a different
/// sentence, because the context was only 4,096 wide — so `REQUESTED_CTX` moved with it, and the compile-time
/// assertion under that constant is what keeps the pair from drifting apart again.
///
/// The price of 8,192, measured on this host the same afternoon (`tests/probes/decode_probe.c nctx=8192`, §14):
///
/// * **an 8,192-wide context costs nothing until something long runs through it** — with a 133-token sequence the peak
///   is 429,248 kB against a 429,528 kB model-open baseline, i.e. +0 MiB. The compute buffer is *touched* per forward
///   pass, not reserved per window;
/// * **a chunk near the ceiling costs about a gigabyte and about ninety seconds**: 7,999 tokens decoded rc 0 with peak
///   RSS **1,478 MiB (+1,058 MiB over baseline)** in 101 s — ~79 tok/s, continuing the falling rate (141 at 543,
///   120 at 2,163). That is a per-chunk commitment, not a per-note one: `DEFAULT_EMBEDDING_TOKENS` stays 1,024, and
///   `MAX_CHUNK_BYTES = 1800` keeps ordinary prose near 500 tokens;
/// * the window really is built as asked here — the probe reads back `n_ctx = 8192`, `n_ubatch = 8192` from the
///   context, which is the A1 review P1-1 pair this build now checks in Rust too.
///
/// Earlier versions of this constant cited Google's `embeddinggemma-2` card (8,192) and before that
/// `max_position_embeddings = 262144`, a RoPE/train-length property the design disowned (§5). Both were the wrong
/// *kind* of authority: what a model was trained on says nothing about what one embedding run can hold. The card's
/// number is now recorded in §5 as context for a long-note experiment, not as a bound we enforce.
///
/// The budget counts **what the model sees** — `prefix_doc` + chunk text + special tokens, because
/// `include_prompt = true` means the counted sequence is not just our bytes — so a caller near this ceiling has to
/// leave room for the prefix and specials too. [`crate::chunk`] caps a chunk at `MAX_CHUNK_BYTES = 1800`, which makes
/// a four-digit-token sequence unlikely for prose; **unlikely is the whole claim** (§5's holes are named there:
/// normalisation can expand bytes, `decode` emits U+FFFD runs, prefix and specials cost tokens from no chunk bytes).
/// R6d measures bytes-per-token on this corpus.
///
/// Three honest limits on the number itself. **(1) It is a global constant doing a per-model job**: 8,192 happens to
/// be *this* build's window, and a spec-named fallback (MiniLM, ~512 tokens) needs a smaller bound — so the follow-up
/// stays exactly as §5 wrote it, move the check beside the provider. **(2) It is now coupled to a build choice**, which
/// is why `local_model` carries a compile-time assertion that `MAX_EMBEDDING_TOKENS <= REQUESTED_CTX`: raising one
/// without the other either produces budgets that cannot run or a window nobody can reach, and neither should be
/// discovered by a user. **(3)** It is a load-time fence over *configured* numbers only; R6b2 enforces it over real
/// token counts, including prefix and specials. Posture unchanged from [`MAX_CONTINUATION_INDENT`] (F17 §4): enforce
/// at load, refuse rather than clamp.
pub const MAX_EMBEDDING_TOKENS: usize = 8_192;

/// How the catalog (`INDEX.md` and its shards) is laid out.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CatalogConfig {
    /// Entries one index file holds before the catalog splits into shards.
    pub max_entries_per_file: usize,
    /// Spaces added to a wrapped entry's continuation lines.
    pub continuation_indent: usize,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            max_entries_per_file: 600,
            continuation_indent: 2,
        }
    }
}

/// The `[models]` section: where artifacts may be fetched from, and the one door around the pin table
/// (`docs/design-m2-local-embeddings.md` §12 rule 7).
///
/// Both keys are `Option`, and **absent is the shipped state** — not a degraded one. With no `base_url` this build
/// has nowhere to fetch from, which is what keeps an unconfigured machine quiet; with no `sha256` it accepts nothing
/// that disagrees with [`crate::pins`].
///
/// **Nothing reads either key as a destination yet.** The fetch command is a later stage of R7b (design §12 rule 1:
/// the automatic trigger is M4's daemon, not a command), so this round makes the keys legal, defaulted, validated
/// and retrievable — and stops there. It is also why there is no documented default origin to name here: §12 rule 7
/// promises one, and inventing a host would be exactly the compile-time secret that rule refuses to hold.
///
/// `deny_unknown_fields`, like [`EmbeddingsConfig`] and for the same reason (§7 there): a misspelled key here would
/// load as *no value at all*, in silence — a mistyped `baseurl` meaning "nowhere to fetch from", a mistyped
/// `sha256` meaning "the pin stands". §12 rule 7's closing sentence is that **silent fallback stays impossible**, so
/// the file is refused instead. The strictness stops at this section's boundary, as it does there: unknown keys
/// elsewhere keep loading as they always have.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsConfig {
    /// Where to fetch from: an `http` or `https` origin, optionally with a path prefix — validated at load (see
    /// `resolve_models`, private hence a code span) and stored with one trailing `/` removed.
    ///
    /// A **path is allowed** because that is what a mirror usually looks like (`https://mirror.internal/mirror/` is
    /// §12 rule 7's use case: "so a mirror works"); what is refused is a value with no scheme or no host, which is
    /// not an origin at all, and credentials in the authority, because this file is not where a secret lives — the
    /// parent design's §9 protects the self-hosted server's URL and key, and `[models]` holds neither.
    ///
    /// The pin table decides *what* is accepted; nothing here changes that ([`crate::pins`] carries no host, on
    /// purpose). No default: see this section's header note.
    pub base_url: Option<String>,
    /// A digest the operator accepts **instead of the pin** — the escape hatch for pin rot (§12 rule 7: an upstream
    /// re-upload nobody noticed until first start refused), and no weaker than today, where any human may pass
    /// `install --sha256`.
    ///
    /// Input is case-insensitive and stored lowercase, exactly like `install --sha256`; anything that is not 64 hex
    /// characters is refused naming the key. Read it through [`ModelsConfig::unpinned_digest_override`], which
    /// carries the obligation that comes with using it.
    pub sha256: Option<String>,
}

/// Hand-written so that both keys default to *unset* in one place — the state §12 rule 7 calls "the pin stands, and
/// there is nowhere to fetch from", which two independent `Option`s must not be able to disagree about.
impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            sha256: None,
        }
    }
}

impl ModelsConfig {
    /// The digest this run was told to accept instead of the pin, lowered at load (see `resolve_models`, private
    /// hence a code span).
    ///
    /// **Using it is a deliberate act and has to look like one.** §12 rule 7 permits an unpinned digest only because
    /// a human named it (`[models] sha256`, or `NP_MODELS_SHA256`), and requires that accepting it **prints a line
    /// saying an unpinned digest was accepted** — the override is loud, or it is indistinguishable from a pin nobody
    /// checked. This round makes the fact retrievable and tested; nothing prints it yet, because no code path can
    /// fetch yet (see [`ModelsConfig`]'s header note), so a caller that starts using this accessor owes that line in
    /// the same change.
    pub fn unpinned_digest_override(&self) -> Option<&str> {
        self.sha256.as_deref()
    }
}

/// How many worker threads the local embedder may ask llama.cpp for.
///
/// **Why a type rather than an `Option<usize>`:** llama.cpp's default worker count is `GGML_DEFAULT_N_THREADS`, a literal
/// `4` (`ggml/include/ggml.h:232`, annotated upstream `// TODO: better default`), so "unset" would not mean "no opinion"
/// — it would mean four, on a machine that may have thirty-two cores idle. Measured on the development host, using all of
/// them instead is +190 % on a realistic chunk and +260 % on a near-window one for +6.6 MiB of peak RSS; using *twice*
/// the machine's count runs at **worse than four** while burning 1,553 % CPU. Both halves of that are why this is a rule
/// with a floor and a ceiling rather than a number: `docs/design-m2-thread-policy.md` §1–§2 for the arithmetic, and
/// §14's D13 subsection in `docs/design-m2-local-embeddings.md` for the rows it comes from.
///
/// The deserializer refuses three things by name, because a silently-ignored worker count is the shape of lie this
/// project's F-series exists to prevent: `threads = 0` (no such request exists), a value above what C can be told
/// (`i32::MAX`; the first version of this seam *clamped* such a number and llama.cpp then aborted the process — the
/// refusal now lives in `local_model::load_with_options`, behind the `local-model` feature), any count above
/// `MAX_REQUESTED_WORKERS`, and anything that is neither `"auto"` nor an integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThreadCount {
    /// Leave it to the rule in [`ThreadCount::resolve`] — **the default**, so a config that says nothing gets a count
    /// derived from the machine rather than from a header file.
    #[default]
    Auto,
    /// An explicit count the operator chose. Honoured as given, including values above `available_parallelism()` — §5 of
    /// the policy design: an operator may know something this program cannot see. What is *not* honoured is a count past
    /// `MAX_REQUESTED_WORKERS`, and that refusal lives in the deserializer so it arrives as a key name rather than as a
    /// runtime failure or, as it did the first time, an uncatchable abort.
    Workers(std::num::NonZeroU32),
}

/// **Refused rather than clamped, and at a bound well below what C can be told.** The first version of this seam accepted
/// anything up to `i32::MAX` and clamped above it; measured on this pin, a request of 3,000,000,000 became `2147483647`,
/// the readback echoed that number back, and llama.cpp died in a C++ exception (`SIGABRT`) which nothing in Rust catches.
/// So the check has to happen before the value exists — but "before the value exists" is not the same as "the largest
/// number a field can hold", and the review of this change found that the first bound was exactly that: `threads =
/// 100000000` parsed, resolved verbatim, and reached `n_threads`. A host with a hundred million CPUs is not a thing this
/// program can see, let alone schedule across, so accepting it buys an operator nothing they could use.
///
/// 4,096 is therefore a **policy** bound, not a measured cliff: it is 128× the largest worker count in
/// `docs/design-m2-thread-policy.md` §2's table (32, the point past which this host got *slower*), and an operator who
/// genuinely wants more than four thousand workers on one process has a machine this program's evidence does not cover.
/// The `i32_t` ceiling stays as a second fence at load time (`local_model::load_with_options`, behind the feature), so a
/// value that somehow arrives by another route is still refused rather than aborting.
const MAX_REQUESTED_WORKERS: u64 = 4_096;

impl ThreadCount {
    /// The count to ask for on a machine where `available_parallelism()` answered `available`.
    ///
    /// ```text
    /// auto  →  min(available, max(min(available, 4), floor(0.8 × available)))
    /// n     →  n
    /// ```
    ///
    /// Each bound is a measurement rather than a preference (`docs/design-m2-thread-policy.md` §2):
    ///
    /// * **never more workers than CPUs** — 32 on a 16-CPU host measured 91.4 tok/s against the default four's 158.7;
    /// * **never fewer than four where four exist** — a bare 80 % of a two-core laptop is one worker at 49.1 tok/s, a
    ///   third of today, so the fraction must not bite on small hardware;
    /// * **`available = 0` cannot happen** (`available_parallelism()` returns a non-zero count), and is answered 1 anyway
    ///   rather than by handing zero to C.
    #[must_use]
    pub fn resolve(self, available: usize) -> usize {
        match self {
            // `Workers` is a `NonZeroU32` that the deserializer bounded by `MAX_REQUESTED_WORKERS`, so the conversion
            // cannot fail here; the fallback is the ceiling rather than `usize::MAX` so an unreachable path cannot be the
            // worst possible count.
            Self::Workers(n) => usize::try_from(n.get()).unwrap_or(MAX_REQUESTED_WORKERS as usize),
            Self::Auto => {
                if available == 0 {
                    return 1;
                }
                let floor = available.min(4);
                // `saturating` because `resolve` is public: a caller that invents `available = usize::MAX` gets
                // the ceiling behaviour of the rule, not a debug-build overflow panic.
                let fraction = available.saturating_mul(4) / 5;
                available.min(floor.max(fraction))
            }
        }
    }

    /// Whether the count came from the rule or from an operator. `np embed`'s report prints both either way; this keeps
    /// "12 because auto" from being reported as "12 because you said twelve".
    #[must_use]
    pub const fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }
}

/// Hand-written so the two accepted spellings produce a message that names `embeddings.threads`. Serde's derived
/// untagged-enum failure says "data did not match any variant", which is a sentence about the implementation; this file's
/// refusals have to be sentences a user can act on, and `[embeddings]` already refuses unknown keys for the same reason
/// (§7 of the M2a design).
impl<'de> serde::Deserialize<'de> for ThreadCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = ThreadCount;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("`\"auto\"` or a positive integer of worker threads")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value == 0 {
                    return Err(E::custom(
                        "embeddings.threads = 0 is not a worker count — write `\"auto\"`, or an integer of at \
                         least 1 (zero workers is not something the runtime can be asked for)",
                    ));
                }
                if value > MAX_REQUESTED_WORKERS {
                    return Err(E::custom(format!(
                        "embeddings.threads = {value} is more worker threads than this program will ask for \
                         (max {MAX_REQUESTED_WORKERS}); write `\"auto\"` or a count near your CPU count"
                    )));
                }
                // `value` is ≥ 1 and fits a `u32`; both were checked above, so neither fallback can run.
                Ok(ThreadCount::Workers(
                    std::num::NonZeroU32::new(value as u32)
                        .unwrap_or_else(|| unreachable!("checked non-zero above")),
                ))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let positive = u64::try_from(value).map_err(|_| {
                    E::custom(format!(
                        "embeddings.threads = {value} is negative; write `\"auto\"` or a positive count"
                    ))
                })?;
                self.visit_u64(positive)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "auto" => Ok(ThreadCount::Auto),
                    other => Err(E::custom(format!(
                        "embeddings.threads must be `\"auto\"` or a positive integer of worker threads, not \
                         {other:?}"
                    ))),
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Which embedding provider to ask for work from (`docs/design-m2-embeddings.md` §2).
///
/// An **enum**, not a `String`, and that choice is the whole of §7's argument. Every other config
/// struct here is `#[serde(default)]` with no unknown-key refusal, so a free-text provider field would
/// take a hand-typed `provide = "openai-compatible"` — the key misspelled, the value perfect — and
/// load it as *no provider at all*, in silence, on a machine whose owner believes they configured
/// embeddings. In a project whose F-series exists to stop exactly that shape of lie about status, the
/// spelling has to be refused rather than defaulted, which is what [`EmbeddingsConfig`]'s
/// `deny_unknown_fields` does and what `tests/config.rs` pins.
///
/// The accepted spellings are exactly the ones §2 writes — `"none"`, `"local-model"`,
/// `"openai-compatible"` — via `rename_all` plus one hand-written exception noted on
/// [`EmbeddingProvider::OpenAiCompatible`]. A config value with two accepted spellings would be two
/// values, so nothing here aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmbeddingProvider {
    /// No provider: nothing is embedded, downloaded or contacted. **The default**, and the shipping
    /// behaviour for anyone who has not chosen a model (§2) — not a stub awaiting a real one.
    #[default]
    None,
    /// A model running inside this process (`local-model`): [`crate::embed::LocalModelProvider`] is the type it
    /// will resolve to, owning the API. The runtime exists now — `local_model` opens a GGUF and embeds through it — but
    /// **nothing instantiates this provider from
    /// this value yet** — the only code that reads the variant is [`Config::load`]'s own validation; the wiring
    /// arrives with R6c. What loading one costs is §6's benchmark's question.
    ///
    /// Refused at [`Config::load`] unless [`EmbeddingsConfig::model`] names something `np models install`
    /// would accept — a local provider with no identity has nothing to write `embeddings.model` under, and
    /// guessing one would be a second source of model identity (§4).
    LocalModel,
    /// An OpenAI-compatible HTTP endpoint. **Kept parseable and refused at [`Config::load`]**, because
    /// embeddings are local now: `docs/design-m2-embeddings.md` §10 records that the self-hosted server
    /// exposes no embeddings route and nothing in this program dials out for a vector, so the spelling names
    /// something no code path can satisfy.
    ///
    /// Deleting it instead was decided against (`docs/design-m2-local-embeddings.md` §10): an existing
    /// `config.toml` would then fail with serde's verbatim "unknown variant" text — which
    /// [`ConfigError::Parse`] passes through — accusing a valid file of being broken and stopping `np scan`
    /// cold without naming a reason. F17's `continuation_indent` precedent is this exact shape: refuse at load
    /// in our own words, naming what changed and which key to set instead.
    ///
    /// Renamed by hand because `rename_all` cannot know it: serde kebab-cases `OpenAiCompatible` into
    /// `open-ai-compatible`, while §2 and the roadmap spell the config value `openai-compatible`. The explicit
    /// attribute is what keeps the documented spelling the only one that reaches our refusal, and
    /// `tests/config.rs` sends every variant through a file precisely so this cannot drift — a lost rename
    /// turns the refusal into serde's, which that test fails on.
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
}

/// The `[embeddings]` section: which provider to ask, and the two identity fields that provider will
/// need (`docs/design-m2-embeddings.md` §2, §7).
///
/// Every field has a default, and `max_tokens` is no exception: a config written before R6a keeps parsing to
/// exactly the values it parsed to then, plus [`DEFAULT_EMBEDDING_TOKENS`] — which is the property
/// `tests/config.rs` pins, because "adding a key must not change what an existing file means" is the half of
/// a strict-section story that a refusal test cannot see.
///
/// **`deny_unknown_fields`, unlike every other struct in this file.** That is a deliberate break with
/// the lenient posture of [`CatalogConfig`] and [`MarkerConfig`], and §7 says why: those structs guard
/// rendering choices, where an unrecognised key costs one wrong-looking line, while this one guards a
/// *status claim* — whether the corpus is embedded at all. A misspelled key there must not become
/// `provider = "none"` by omission, so the file is refused instead. The strictness stops at this
/// section's boundary: unknown keys elsewhere keep loading as they always have (`tests/config.rs`
/// asserts both halves, because a reader of one half cannot tell a scoped rule from a global one).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingsConfig {
    /// Which provider to ask. Absent means [`EmbeddingProvider::None`].
    pub provider: EmbeddingProvider,
    /// Model identity as the provider will name it (`embeddinggemma-2-text@1`-shaped), i.e. what
    /// `models.id` and `embeddings.model` will hold (§3).
    ///
    /// **Nothing reads this yet, and that is the state of Round 3a rather than an oversight:** the
    /// provider that would put a value here into a database row arrives with the scan wiring. It lives
    /// in config now so §2's "one config key apart" promise describes the whole surface a user touches,
    /// and so a chosen model is already expressible while §6's benchmark is still running.
    pub model: Option<String>,
    /// Vector width the provider must produce, in `f32` components — the number
    /// `embeddings.dim`/[`crate::embed::Vector::dim`] speaks.
    ///
    /// Still unread by any decision, and that is not this round's oversight: it becomes the `configured`
    /// argument of [`crate::embed::LocalModelProvider::new`] when R6c wires a config to a provider, and until
    /// then nothing can disagree with it. `u16` rather than `usize` because it is a *declared* width and the
    /// schema's `CHECK (length(vector) = 4 * dim)` has to hold: 65 535 components is 256 KiB per row, already
    /// past any model this project would run, so the type refuses an absurdity before a provider has to.
    pub dim: Option<u16>,
    /// Token ceiling for one sequence handed to the model, **default
    /// [`DEFAULT_EMBEDDING_TOKENS`]** — counted over `prefix_doc` + chunk text + special tokens, not over the
    /// chunk's bytes alone, because `include_prompt = true` means the model sees the prefix too
    /// (`docs/design-m2-local-embeddings.md` §5).
    ///
    /// An over-budget chunk is *reported*, never silently truncated: truncation would stamp the whole chunk's
    /// digest onto a vector that read part of it. Enforcing the budget needs a tokenizer and so belongs to
    /// R6b2; what R6a does is make the key legal, defaulted and bounded, since `deny_unknown_fields` means
    /// this section refused it by name before this round.
    pub max_tokens: usize,
    /// Worker threads the local embedder may use — **default [`ThreadCount::Auto`]**, the rule in
    /// [`ThreadCount::resolve`] rather than llama.cpp's compile-time four.
    ///
    /// Read only when `provider = "local-model"` (`dim` and `model` are likewise unread by the other providers, so this
    /// is not a new looseness — `docs/design-m2-thread-policy.md` §3 records the choice, including why there is no
    /// `[local_model]` section and no environment override). Accepted spellings and refusals: [`ThreadCount`].
    pub threads: ThreadCount,
}

/// Hand-written rather than derived, so the default `max_tokens` is one number in one place — and so a later
/// field cannot quietly default to `0`, which this file refuses as a budget.
impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            provider: EmbeddingProvider::None,
            model: None,
            dim: None,
            max_tokens: DEFAULT_EMBEDDING_TOKENS,
            threads: ThreadCount::default(),
        }
    }
}

/// Status markers (spec §8.7). Stored exactly as written here; later tasks match
/// them case-insensitively against the lowercased line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MarkerConfig {
    /// Markers that make an item done.
    pub done: Vec<String>,
    /// Markers that make an item paused.
    pub paused: Vec<String>,
}

impl Default for MarkerConfig {
    fn default() -> Self {
        Self {
            done: marker_list(&[
                "✅",
                "- [x]",
                "done",
                "finished",
                "erledigt",
                "fertig",
                "abgeschlossen",
            ]),
            paused: marker_list(&[
                "paused",
                "pausiert",
                "on hold",
                "gestoppt",
                "verschoben",
                "vertagt",
            ]),
        }
    }
}

fn marker_list(markers: &[&str]) -> Vec<String> {
    markers.iter().map(|marker| (*marker).to_string()).collect()
}

/// Loaded configuration: where data lives, catalog layout, status markers, embedding provider.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// OS data dir, from `$XDG_DATA_HOME` else `$HOME/.local/share`
    /// (Windows: `%LOCALAPPDATA%`). Empty only when the environment names none.
    pub data_dir: PathBuf,
    /// Catalog layout.
    pub catalog: CatalogConfig,
    /// Status markers.
    pub markers: MarkerConfig,
    /// Embedding provider choice. An absent `[embeddings]` section is `provider = "none"`, which is
    /// the default and not a degraded state (§2).
    pub embeddings: EmbeddingsConfig,
    /// Fetch source and the digest override (§12 rule 7). An absent `[models]` section names neither, which is the
    /// shipped state: nothing to fetch from, and the pin stands.
    pub models: ModelsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir().unwrap_or_default(),
            catalog: CatalogConfig::default(),
            markers: MarkerConfig::default(),
            embeddings: EmbeddingsConfig::default(),
            models: ModelsConfig::default(),
        }
    }
}

impl Config {
    /// Read configuration from TOML.
    ///
    /// `override_` names the file to read, and a named file that is absent is an
    /// error — a typo'd path must not silently change behaviour. With `None`,
    /// [`config_file_path`] is used and a missing file means all defaults. A file
    /// that is present keeps every default its structs provide, so a partial file
    /// only changes the keys it spells out; unparsable content is
    /// [`ConfigError::Parse`], which names the file it refused.
    ///
    /// Unknown keys are allowed everywhere except `[embeddings]` and `[models]`, which refuse them
    /// ([`EmbeddingsConfig`], §7 of the M2a design; [`ModelsConfig`], §12 rule 7 of the fetch design).
    /// A `data_dir` neither the file nor the environment supplied is a failure, not a default:
    /// every path this program derives from it would go relative — a `workspaces/` directory next
    /// to whatever directory the command happened to run in. The CLI reports it and exits 1.
    pub fn load(override_: Option<&Path>) -> Result<Config, ConfigError> {
        let mut cfg = match override_ {
            Some(path) => read_config(path)?,
            None => match config_file_path() {
                Some(path) => match read_config(&path) {
                    Err(ConfigError::Io(err)) if err.kind() == io::ErrorKind::NotFound => {
                        Config::default()
                    }
                    other => other?,
                },
                None => Config::default(),
            },
        };
        if cfg.data_dir.as_os_str().is_empty() {
            // A config file that does not name a data dir means the OS one, which
            // `Config::default()` already resolved; reaching here empty means the environment
            // named nothing either.
            cfg.data_dir = default_data_dir().ok_or(ConfigError::MissingField("data_dir"))?;
        }
        // Refuse an indent the renderer cannot be asked to draw, from whatever source it arrived — this runs after the
        // file and environment merge, so a future override is covered too. It travels as `Io(InvalidData)` rather than a
        // fourth variant because the plan fixes `ConfigError` at three (`docs/.../2026-10-23-note-processor-m1-core-catalog.md`
        // step 3), following the precedent of the `From<rusqlite::Error>` impl above; `Parse` would have accused valid TOML
        // of being broken. Rejecting rather than clamping keeps published bytes honest with the config that produced them.
        if cfg.catalog.continuation_indent > MAX_CONTINUATION_INDENT {
            return Err(ConfigError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid catalog.continuation_indent = {} (max {MAX_CONTINUATION_INDENT})",
                    cfg.catalog.continuation_indent
                ),
            )));
        }
        validate_embeddings(&cfg)?;
        resolve_models(&mut cfg)?;
        Ok(cfg)
    }

    /// This workspace's database: `data_dir/workspaces/<sha256(abs)[0..16]>.sqlite`.
    ///
    /// [`Config::workspace_models_dir`] places the same workspace's model artifacts beside it, from the same
    /// private `workspace_key`, because a row and the file it names belong to one workspace or to neither.
    ///
    /// The workspace need not exist: the absolute spelling of `workspace` (current
    /// directory prepended when relative, no symlink resolution) is hashed as
    /// given, so the same input always yields the same path.
    ///
    /// The digest covers the raw OS bytes rather than a decoded string, because lossy decoding maps
    /// two different paths onto one text: a byte that is not valid UTF-8 and a literal replacement
    /// character both decode to `U+FFFD`. Two workspaces sharing one database would mark each
    /// other's notes stale on alternate scans. Windows has no byte-exact spelling of a path to reach
    /// for, so it keeps the lossy one.
    pub fn workspace_db(&self, workspace: &Path) -> PathBuf {
        self.workspaces_join(&workspace_key(workspace), "sqlite")
    }

    /// This workspace's model-artifact directory: `data_dir/workspaces/<db-hash>.models`.
    ///
    /// The name is `workspace_key` (private) — **the same key as [`Config::workspace_db`]**, which is the whole
    /// point of this function existing rather than the caller building a path (`docs/design-m2-models-install.md`
    /// §3 step 0). `models` rows live in the per-workspace database, so an artifact directory derived by any
    /// other rule would let an install in workspace A rename over the file workspace B's row names — the P0
    /// that design review raised, and the reason `install`/`list` take a workspace operand at all. Two
    /// spellings of "one workspace" must never be computed twice: `abs_path` is `pub(crate)` while
    /// `hex_prefix` and `DB_HASH_BYTES` are private, so a second derivation outside this file would have
    /// to re-implement the digest — and the two obvious shortcuts both diverge here. Over `path::Display`
    /// they diverge for a name UTF-8 cannot hold (the same collision `path_bytes` refuses inside a
    /// workspace), and through `canonicalize()` they diverge because neither function resolves symlinks.
    ///
    /// `.models` is a suffix on the key rather than a directory named by it so that `<hash>` is never both a
    /// file and a folder in one listing, and so `list` can tell an artifact directory from SQLite's own
    /// `<hash>.sqlite{-wal,-shm}` siblings at a glance.
    ///
    /// Like [`Config::workspace_db`], this derives a path for a workspace that does not exist — refusing
    /// such a workspace is the caller's job and is spelled
    /// [`crate::models::require_workspace_dir`].
    pub fn workspace_models_dir(&self, workspace: &Path) -> PathBuf {
        self.workspaces_join(&workspace_key(workspace), "models")
    }

    /// `data_dir/workspaces/<key>.<extension>` — the one place that assembles a per-workspace path, so the
    /// database and the artifact directory cannot be placed in different directories by two format strings.
    fn workspaces_join(&self, key: &str, extension: &str) -> PathBuf {
        self.data_dir
            .join("workspaces")
            .join(format!("{key}.{extension}"))
    }
}

/// Refuse an `[embeddings]` section that cannot be served, in our own words rather than serde's.
///
/// Three refusals, all travelling as `Io(InvalidData)` for the reason F17 established and
/// [`ConfigError::Parse`]'s doc states: the TOML is *valid*, so a variant whose payload is a driver error
/// would accuse a well-formed file of being broken — and only our own sentence can name what changed and which
/// key to set. `Parse` stays reserved for bytes serde cannot read.
///
/// The checks are ordered value-first: a number outside its bounds is wrong whatever the provider says, and it
/// is checked before the provider-specific rules so an absurd budget cannot be hidden behind a refusal about a
/// model name.
fn validate_embeddings(cfg: &Config) -> Result<(), ConfigError> {
    if !(1..=MAX_EMBEDDING_TOKENS).contains(&cfg.embeddings.max_tokens) {
        return Err(invalid_data(format!(
            "invalid embeddings.max_tokens = {}: one embedding holds at most \
             {MAX_EMBEDDING_TOKENS} tokens in this build — that is how wide a context it creates, not the largest \
             the model was trained on — and the budget counts prefix + chunk + special tokens, so the value must be \
             between 1 and {MAX_EMBEDDING_TOKENS}. Long notes are chunked; raise no key to embed one whole file",
            cfg.embeddings.max_tokens
        )));
    }
    if cfg.embeddings.provider == EmbeddingProvider::OpenAiCompatible {
        // §10's decision: the variant stays, the capability does not. Naming both halves — what went away and
        // what to write instead — is what makes this a refusal a user can act on rather than a regression.
        return Err(invalid_data(
            "embeddings.provider = \"openai-compatible\" names an HTTP endpoint provider, and embeddings are \
             local now: nothing in np dials out for a vector. Set embeddings.provider = \"local-model\" and \
             embeddings.model to an installed model id"
                .to_string(),
        ));
    }
    // A zero width is refused here as well as in `np models install --dim`, and for the same stated reason:
    // migration 7's `CHECK (length(vector) = 4 * dim)` admits a zero-byte vector when `dim` is 0. R6a made this
    // value load-bearing — it becomes [`crate::embed::LocalModelProvider::new`]'s `configured` argument — so the
    // bound cannot live only in np-cli's `NonZeroU16`, which a config file routes around without ever parsing an
    // argv. Refuse, do not clamp: F17's posture again.
    if cfg.embeddings.dim == Some(0) {
        return Err(invalid_data(
            "invalid embeddings.dim = 0 (a zero-width model has no vector to compare; omit the key to leave the \
             width unset)"
                .to_string(),
        ));
    }
    if cfg.embeddings.provider == EmbeddingProvider::LocalModel {
        let Some(model) = cfg.embeddings.model.as_deref() else {
            return Err(invalid_data(
                "embeddings.provider = \"local-model\" needs embeddings.model to name an installed model; \
                 none is set"
                    .to_string(),
            ));
        };
        // The id rule is `models::validate_id`'s, not a second one written here: whether an id can name an
        // installed row at all is decided by what `np models install` would accept (§2 of that design), and a
        // config naming something uninstallable is refused now instead of at embed time. Whether such a row
        // *is* installed needs the workspace database, so that half stays with the caller that holds one —
        // `db::model_row` returning `None`, and no provider constructible without a row (`embed::LocalModelProvider`).
        if let Err(err) = crate::models::validate_id(model) {
            let rule = match err {
                crate::models::ModelError::InvalidId { rule, .. } => rule,
                // `validate_id` has no other failure today; naming the value beats a claim about a rule that
                // was not broken if that ever changes.
                other => return Err(invalid_data(other.to_string())),
            };
            return Err(invalid_data(format!(
                "embeddings.model `{model}` cannot name an installed model: {rule}"
            )));
        }
    }
    Ok(())
}

/// A config *value* refused as data, the way F17 refuses an oversized indent — see [`validate_embeddings`].
fn invalid_data(message: String) -> ConfigError {
    ConfigError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

/// Take any `[models]` value the file left unset from the environment, normalise both spellings, and refuse what
/// cannot be honoured ([`ModelsConfig`], `docs/design-m2-local-embeddings.md` §12 rule 7).
///
/// **Precedence follows `data_dir` in [`Config::load`]**, this file's only existing config-versus-environment rule:
/// a value the file spelled out wins, and the environment (`NP_MODELS_BASE_URL`, `NP_MODELS_SHA256`) supplies a key
/// the file left unset — never an override of a file that names one. `[embeddings]` had no env rule to copy because
/// it has none at all, so borrowing its shape would have meant inventing one.
///
/// Validation runs on the merged value for the reason F17's ceiling gives at the indent check above: a key can
/// arrive from either place, and a rule watching only the file would let the environment route around it. Each
/// refusal names both the config key and the variable the value came from, because telling someone to edit a file
/// that does not hold the value wastes the one thing a refusal is for.
fn resolve_models(cfg: &mut Config) -> Result<(), ConfigError> {
    // Where each present value came from, so the message can point at the place to edit. The file's own spelling
    // needs no marker beyond the key itself; only the environment arm changes it.
    let mut base_origin = "models.base_url";
    if cfg.models.base_url.is_none() {
        if let Some(value) = env_text("NP_MODELS_BASE_URL")? {
            cfg.models.base_url = Some(value);
            base_origin = "NP_MODELS_BASE_URL";
        }
    }
    let mut digest_origin = "models.sha256";
    if cfg.models.sha256.is_none() {
        if let Some(value) = env_text("NP_MODELS_SHA256")? {
            cfg.models.sha256 = Some(value);
            digest_origin = "NP_MODELS_SHA256";
        }
    }

    // One trailing `/` is the same origin spelled twice, and leaving it in means every caller has to know which of
    // `origin` + `/name` to write. Exactly one comes off: `…/base//` is not a typo this function can read minds
    // about, and a later join will show it rather than hide it.
    if let Some(url) = cfg.models.base_url.clone() {
        let trimmed = url.strip_suffix('/').unwrap_or(&url).to_string();
        validate_base_url(&trimmed, base_origin)?;
        cfg.models.base_url = Some(trimmed);
    }
    if let Some(digest) = cfg.models.sha256.clone() {
        let lowered = validate_digest(&digest, digest_origin)?;
        cfg.models.sha256 = Some(lowered);
    }
    Ok(())
}

/// Refuse a `[models] base_url` that names no origin, and say which of the two places held it.
///
/// The rule is what this file can check without an HTTP dependency (C1 adds none): a scheme of `http` or `https`,
/// then a non-empty authority with no credentials and no whitespace in it. A path after the authority is legal and
/// untouched — [`ModelsConfig::base_url`] says why a mirror usually has one.
fn validate_base_url(url: &str, origin: &str) -> Result<(), ConfigError> {
    let refuse = |reason: String| {
        Err(invalid_data(format!(
            "invalid {origin} = {url:?}: {reason} — expected an http(s) origin such as \
             `https://mirror.example/models` (no credentials, no spaces)"
        )))
    };
    let Some((scheme, rest)) = url.split_once("://") else {
        return refuse("missing `://`, so there is no scheme to fetch over".to_string());
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        // Only these two: a fetch carries 310 MB, and anything else would do it without transport security while
        // the pin table's refusal is the only check left.
        return refuse(format!(
            "scheme `{}` cannot carry a fetch; use http or https",
            scheme.to_ascii_lowercase()
        ));
    }
    // The authority runs to the first path/query/fragment separator, whatever the rest of the URL holds.
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    if authority.is_empty() {
        return refuse("no host follows the scheme".to_string());
    }
    if authority.contains('@') {
        // Credentials here would be a secret in an ordinary config file, which is not §9's class — and a base URL
        // that needs them is a mirror configured in the wrong place.
        return refuse(
            "carries credentials; a fetch source authenticates some other way".to_string(),
        );
    }
    if authority.chars().any(char::is_whitespace) {
        return refuse("the host contains whitespace".to_string());
    }
    Ok(())
}

/// Validate a `[models] sha256` and return it lowered, in the shape `install --sha256` already refuses bad input —
/// [`crate::models::ModelError::InvalidDigest`] is the sentence this one copies: the key, the rule, how many
/// characters arrived, and never the value itself, which is 64 characters of noise in a message that has to stay
/// readable.
fn validate_digest(digest: &str, origin: &str) -> Result<String, ConfigError> {
    // The *rule* is `models`' own digest normalisation (crate-private, hence a code span), reused for the same reason
    // [`validate_embeddings`] reuses `models::validate_id`: 64 hex characters is one fact about one kind of value, and
    // a second implementation of it is a second place where "what a digest looks like" could drift. Only the message
    // is ours, because only we know which key or variable the reader has to go and edit.
    match crate::models::normalize_digest(digest) {
        Ok(lowered) => Ok(lowered),
        Err(crate::models::ModelError::InvalidDigest { chars }) => Err(invalid_data(format!(
            "invalid {origin}: a sha256 must be exactly {} hexadecimal characters; this value holds {chars}",
            crate::models::DIGEST_HEX_CHARS
        ))),
        // `normalize_digest` has one failure mode today; naming what came back beats asserting about a rule that was
        // not broken, which is the same arm [`validate_embeddings`] keeps around `validate_id`.
        Err(other) => Err(invalid_data(other.to_string())),
    }
}

/// Non-empty environment variable as text; an empty value counts as unset, the rule [`env_path`] applies to paths.
///
/// A value that is not valid UTF-8 is **refused**, not skipped: both `[models]` keys are ASCII by rule (an origin,
/// a hex digest), so such a value is a mistake in the environment someone can fix — and ignoring it would report
/// "unset" to a person who believes they set an override.
fn env_text(key: &str) -> Result<Option<String>, ConfigError> {
    let Some(raw) = env::var_os(key) else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    raw.into_string()
        .map(Some)
        .map_err(|_| invalid_data(format!("invalid {key}: the value is not valid text")))
}

/// The per-workspace name: the leading `DB_HASH_BYTES` bytes of the workspace path digest, as lowercase
/// hex. Shared by [`Config::workspace_db`] and [`Config::workspace_models_dir`].
///
/// Everything that decides *which workspace a row and an artifact belong to* goes through this one function,
/// which is why it hashes [`path_bytes`] of [`abs_path`] rather than anything a caller could spell
/// differently: see [`Config::workspace_db`] for the two divergences this forecloses.
fn workspace_key(workspace: &Path) -> String {
    let digest = Sha256::digest(path_bytes(&abs_path(workspace)));
    hex_prefix(&digest[..DB_HASH_BYTES])
}

/// Location of the config file: `$XDG_CONFIG_HOME/note-processor/config.toml`,
/// else `~/.config/note-processor/config.toml` (Windows: `%APPDATA%\note-processor\`).
/// `None` when the environment names no config directory at all.
pub fn config_file_path() -> Option<PathBuf> {
    let dir = config_dir()?;
    Some(dir.join("note-processor").join("config.toml"))
}

fn read_config(path: &Path) -> Result<Config, ConfigError> {
    let text = fs::read_to_string(path).map_err(|err| {
        ConfigError::Io(io::Error::new(
            err.kind(),
            format!("{err}: {}", path.display()),
        ))
    })?;
    // `Parse` carries the path because `toml::de::Error` cannot know it: `from_str` has no file to
    // name, and this function is the only place that does. The driver error goes with it unchanged so
    // its line/column caret survives.
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// Absolute spelling of `path`: the current directory is prepended when it is
/// relative, symlinks are not resolved.
///
/// Shared with [`crate::scan`], which walks and writes through this same spelling so the workspace
/// key in a database row and the name in the registry are the one absolute path.
pub(crate) fn abs_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        // `workspace_db` is infallible by contract, so an unreadable current
        // directory leaves the spelling as given instead of failing.
        env::current_dir().unwrap_or_default().join(path)
    }
}

/// The bytes of a path as the OS holds them, which is what makes two paths that decode to the same
/// lossy text hash apart.
#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

/// The bytes of a path as the OS holds them. Windows paths are UTF-16 and reach this program as
/// text, so its spelling stays lossy — see [`Config::workspace_db`]. This arm covers *any* non-Unix target, not
/// Windows alone: wherever it applies, two paths differing only in units Rust cannot represent share one
/// database. F18 §3 refuses such a name *inside* a workspace and never the root's own name, which is relative to
/// nothing, so that collision is the one level the refusal does not reach.
#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

/// Lowercase hex for the leading bytes of a digest.
fn hex_prefix(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Non-empty environment variable as a path; an empty value counts as unset, as
/// the XDG base directory spec requires.
fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `primary` when set, else the home directory, optionally with `home_suffix`
/// appended. The Windows rules pass no suffix, the Unix ones do; taking both
/// spellings as arguments keeps one implementation that every platform tests.
fn dir_from(
    primary: Option<PathBuf>,
    home: Option<PathBuf>,
    home_suffix: Option<&str>,
) -> Option<PathBuf> {
    primary.or_else(|| match (home, home_suffix) {
        (Some(home), Some(suffix)) => Some(home.join(suffix)),
        (Some(home), None) => Some(home),
        (None, _) => None,
    })
}

#[cfg(windows)]
fn config_dir() -> Option<PathBuf> {
    dir_from(env_path("APPDATA"), None, None)
}

#[cfg(not(windows))]
fn config_dir() -> Option<PathBuf> {
    dir_from(
        env_path("XDG_CONFIG_HOME"),
        env_path("HOME"),
        Some(".config"),
    )
}

/// OS data dir: `$XDG_DATA_HOME` else `$HOME/.local/share` (Windows: `%LOCALAPPDATA%`).
/// `None` when the environment names no data directory at all — what [`Config::load`] refuses
/// rather than deriving paths relative to the current directory.
#[cfg(windows)]
pub fn default_data_dir() -> Option<PathBuf> {
    dir_from(env_path("LOCALAPPDATA"), None, None)
}

/// OS data dir: `$XDG_DATA_HOME` else `$HOME/.local/share` (Windows: `%LOCALAPPDATA%`).
/// `None` when the environment names no data directory at all — what [`Config::load`] refuses
/// rather than deriving paths relative to the current directory.
#[cfg(not(windows))]
pub fn default_data_dir() -> Option<PathBuf> {
    dir_from(
        env_path("XDG_DATA_HOME"),
        env_path("HOME"),
        Some(".local/share"),
    )
}

/// The workspaces this program has seen, in `<data_dir>/workspaces.sqlite` —
/// separate from each workspace's own database. Rows are never deleted.
#[derive(Debug)]
pub struct WorkspaceRegistry {
    conn: Connection,
}

impl WorkspaceRegistry {
    /// Open the registry in `data_dir`, creating the directory and the table on
    /// first use.
    ///
    /// The registry is written by every command and shared by every run of this program, so it gets the
    /// two connection pragmas a workspace database sets that apply here — WAL, because a scan and an
    /// `np` run overlap on one data dir, and `BUSY_TIMEOUT_MS` (a code span, because the constant is
    /// `pub(crate)`), so the later one waits rather than failing with `SQLITE_BUSY`. It declares no
    /// foreign keys, so it asks for nothing else. The timeout comes first in the batch so it also covers
    /// the `CREATE TABLE` below.
    pub fn open(data_dir: &Path) -> Result<WorkspaceRegistry, ConfigError> {
        fs::create_dir_all(data_dir)?;
        let conn = Connection::open(data_dir.join(REGISTRY_FILE))?;
        conn.execute_batch(&format!(
            "PRAGMA busy_timeout={BUSY_TIMEOUT_MS};
             PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS workspaces (
                 workspace  TEXT    PRIMARY KEY,
                 first_seen INTEGER NOT NULL,
                 last_seen  INTEGER NOT NULL
             );"
        ))?;
        Ok(Self { conn })
    }

    /// Add `workspace`, or bump its `last_seen`; `first_seen` keeps the earliest value.
    pub fn record(&mut self, workspace: &Path, now: i64) -> Result<(), ConfigError> {
        let workspace = abs_path(workspace).to_string_lossy().into_owned();
        self.conn.execute(
            "INSERT INTO workspaces (workspace, first_seen, last_seen) VALUES (?1, ?2, ?2)
             ON CONFLICT(workspace) DO UPDATE SET last_seen = excluded.last_seen",
            params![workspace, now],
        )?;
        Ok(())
    }

    /// `(path, last_seen)` for every known workspace, most recent first. The path
    /// breaks ties so the order is deterministic.
    pub fn recent(&self) -> Result<Vec<(String, i64)>, ConfigError> {
        let mut stmt = self.conn.prepare(
            "SELECT workspace, last_seen FROM workspaces ORDER BY last_seen DESC, workspace ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut recent = Vec::new();
        for row in rows {
            recent.push(row?);
        }
        Ok(recent)
    }
}

#[cfg(test)]
mod tests {
    use super::dir_from;
    use std::path::PathBuf;

    #[test]
    fn dir_from_prefers_primary_then_joins_home_suffix() {
        let primary = PathBuf::from("/xdg/data");
        let home = PathBuf::from("/home/ana");

        assert_eq!(
            dir_from(
                Some(primary.clone()),
                Some(home.clone()),
                Some(".local/share")
            ),
            Some(primary)
        );
        assert_eq!(
            dir_from(None, Some(home.clone()), Some(".local/share")),
            Some(PathBuf::from("/home/ana/.local/share"))
        );
        assert_eq!(dir_from(None, None, Some(".config")), None);
    }

    #[test]
    fn dir_from_without_home_suffix_is_the_windows_spelling() {
        // `%APPDATA%` / `%LOCALAPPDATA%` are used as-is and there is no fallback.
        assert_eq!(
            dir_from(
                Some(PathBuf::from("C:\\Users\\ana\\AppData\\Local")),
                None,
                None
            ),
            Some(PathBuf::from("C:\\Users\\ana\\AppData\\Local"))
        );
        assert_eq!(
            dir_from(None, Some(PathBuf::from("/home/ana")), None),
            Some(PathBuf::from("/home/ana"))
        );
    }
}
