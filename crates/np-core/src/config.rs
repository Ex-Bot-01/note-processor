//! Config file (`config.toml`), OS data dir, and the known-workspace registry.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf, MAIN_SEPARATOR_STR};

use rusqlite::{params, Connection};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::db::BUSY_TIMEOUT_MS;
use crate::loglevel::{resolve_level_from_env, LogLevel};

/// Name of the registry database inside the data dir.
///
/// Public because **F37**'s fallback has to ask a directory whether it holds our index, and a test that spells the name by
/// hand would pass even if the two stopped agreeing.
pub const REGISTRY_FILE: &str = "workspaces.sqlite";
/// The directory of per-workspace databases inside the data dir — the other half of the same question (**F37**): an
/// interrupted first run can leave one of these without a registry beside it, and that still counts as our index.
pub const WORKSPACES_DIR: &str = "workspaces";
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
///
/// **`deny_unknown_fields`, since F35.** This used to be the lenient half of the file — "rendering choices,
/// where an unrecognised key costs one wrong-looking line" — and the argument held for a key *inside* the
/// section. It never covered the case that started the round: `[catalogg]`, one letter off, is not a stray key
/// in this struct at all but the whole struct going unread, so `max_entries_per_file` stayed 600 under a config
/// claiming 200. One rule for every container (`docs/design-f35-unknown-keys.md` §2) is what makes the file's
/// behaviour describable in a sentence a user can rely on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
/// Both keys are `Option`, and **absent is still a valid state** — but since §12 rule 7's amendment of 2026-11-04 it no
/// longer means "quiet": with no `base_url` a fetch dials the origin compiled into [`crate::pins::DEFAULT_ORIGIN`], and `np
/// models fetch` says in words that it resolved to that default rather than doing it silently. With no `sha256` a fetch accepts
/// nothing that disagrees with [`crate::pins`].
///
/// **`base_url` is read as a destination** by `np models fetch`, through [`crate::fetch::resolve_origin`] — the one function
/// that answers "which origin did this run dial". It is not read by `install`, which never touches the network. An **empty**
/// value counts as unset (the rule `env_path` applies to paths), so `NP_MODELS_BASE_URL=` means "not configured" and now
/// carries a consequence it did not use to: see `an_unconfigured_fetch_says_it_is_using_the_compiled_in_origin` in
/// `crates/np-cli/tests/models_cli.rs`. §12 rule 7
/// promises one, and inventing a host would be exactly the compile-time secret that rule refuses to hold.
///
/// `deny_unknown_fields`, like [`EmbeddingsConfig`] and for the same reason (§7 there): a misspelled key here would
/// load as *no value at all*, in silence — a mistyped `baseurl` meaning "nowhere to fetch from", a mistyped
/// `sha256` meaning "the pin stands". §12 rule 7's closing sentence is that **silent fallback stays impossible**, so
/// the file is refused instead. That rule now covers the whole file, not just this section (F35,
/// `docs/design-f35-unknown-keys.md`): this struct was the second to get it, and the root was the missing one —
/// an invented section never reached a struct to be refused by.
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
    /// Whether a command that needs the pinned artifact may fetch it when it is absent (**F38**).
    ///
    /// **Default true**, which is a product decision rather than a serde convenience: the consumer of this program is
    /// increasingly another program, and "call `np embed`, get vectors" beats "call three commands and interpret three
    /// refusals". Default-on is defensible only because the surface is narrow — one id has pinned bytes
    /// ([`crate::pins`]), from one origin, verified by digest before anything is published — so what this key permits is
    /// never "download whatever config named". A machine that already holds its artifact pays nothing for the default:
    /// the trigger fires only where the answer to "is it there?" was no.
    ///
    /// Read it through [`ModelsConfig::auto_fetch_allowed`], which also consults `NP_NO_AUTO_FETCH`. Nothing here lets
    /// an *unpinned* id be fetched, whatever this says.
    #[serde(default = "auto_fetch_default")]
    pub auto_fetch: bool,
}

/// The default for [`ModelsConfig::auto_fetch`] as a function, so serde and [`Default`] state it once.
fn auto_fetch_default() -> bool {
    true
}

/// Hand-written so that both keys default to *unset* in one place — the state §12 rule 7 calls "the pin stands, and
/// there is nowhere to fetch from", which two independent `Option`s must not be able to disagree about.
impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            sha256: None,
            auto_fetch: auto_fetch_default(),
        }
    }
}

impl ModelsConfig {
    /// The digest this run was told to accept instead of the pin, lowered at load (see `resolve_models`, private
    /// hence a code span).
    ///
    /// **Using it is a deliberate act and has to look like one.** §12 rule 7 permits an unpinned digest only because
    /// a human named it (`[models] sha256`, or `NP_MODELS_SHA256`), and requires that accepting it **prints a line saying an
    /// unpinned digest was accepted** — the override is loud, or it is indistinguishable from a pin nobody checked.
    ///
    /// **It has no production caller today, and fetch is not the omission's victim.** A fetch compares the pin and nothing
    /// else, so an operator who sets `[models] sha256` expecting to accept a different build gets a refusal, not an override;
    /// the working door for that case remains `np models install --sha256`, where the human names the digest in argv. Anyone
    /// who wires this accessor into `fetch` owes rule 6's loud line **in the same change** — and owes correcting the sentence
    /// in `docs/design-m2-local-embeddings.md` §12 rule 7 that says the override is live.
    pub fn unpinned_digest_override(&self) -> Option<&str> {
        self.sha256.as_deref()
    }

    /// Whether this process may fetch a pinned artifact: the file's answer **and** nobody having forbidden it from
    /// outside with `NP_NO_AUTO_FETCH`.
    ///
    /// The environment outranks the file here, which breaks the precedence every other `[models]` key follows — and
    /// deliberately. `base_url` and `sha256` are *declarations*, so the operator's stated intent in the file wins and the
    /// environment only fills what the file left unset. This one is a **forbiddance**, and the people who need it are not
    /// the people who own that file: CI, an air-gapped host, a metered link on someone else's laptop. A switch a config
    /// file can veto is not a safety switch. It overrides in one direction only — setting `NP_NO_AUTO_FETCH=1` can turn
    /// fetching off, never on.
    ///
    /// Consulted per call rather than folded into the field at load, so a long-lived `Config` (the daemon this all feeds)
    /// can be silenced mid-life without a reload.
    ///
    /// Any value except empty or exactly `0` forbids — including `"0 "`, which reads as a mistake someone meant to forbid
    /// with. A switch that must be spelled correctly during a network outage is a switch that does not work.
    pub fn auto_fetch_allowed(&self) -> bool {
        self.auto_fetch && !Self::env_forbids_fetch()
    }

    /// The environment half of [`ModelsConfig::auto_fetch_allowed`], on its own because **argv needs to consult it
    /// without the file** (**F38**): `--auto-fetch` overrides `[models] auto_fetch = false`, and nothing overrides the
    /// kill switch — so a caller that has been explicitly asked to fetch still owes this question before it dials.
    ///
    /// Splitting it out is also what keeps the two halves from drifting: one reader of `NP_NO_AUTO_FETCH` answers for
    /// both the config-shaped decision and the argv-shaped one.
    pub fn env_forbids_fetch() -> bool {
        env_flag_set("NP_NO_AUTO_FETCH")
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
/// An **enum**, not a `String`, and that choice is the whole of §7's argument. A free-text provider field
/// would take a hand-typed `provide = "openai-compatible"` — the key misspelled, the value perfect — and load it
/// as *no provider at all*, in silence, on a machine whose owner believes they configured embeddings. In a
/// project whose F-series exists to stop exactly that shape of lie about status, the spelling has to be refused
/// rather than defaulted, which is what [`EmbeddingsConfig`]'s `deny_unknown_fields` does and what
/// `tests/config.rs` pins.
///
/// (This paragraph used to add "every other config struct here is `#[serde(default)]` with no unknown-key
/// refusal" as the contrast. F35 removed that contrast — every container refuses now — so the enum's argument
/// stands on its own: refusing a bad *key* was never enough if a bad *value* could still name a provider this
/// build cannot honour.)
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
    /// A model running inside this process (`local-model`). `np-cli`'s `embed_provider` is what turns this value into a
    /// provider: `local_model::LazyLocalEmbedder` (that module is behind `local-model`, so no link) when the binary was built with the runtime — loaded on the first
    /// batch rather than at resolution, so a run with nothing owed pays no load — and [`crate::embed::LocalModelProvider`]
    /// when it was not, which answers every batch it is handed with *runtime not built*. `embed_run` matches this same
    /// variant to decide which of §6's arms are reachable, so the claim that only validation reads it (true before R6c)
    /// stopped being true when the wiring landed. What loading one costs is §6's benchmark's question, measured in R6d.
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
/// **`deny_unknown_fields`, and since F35 that is no longer special** — every container in this file carries
/// it, so what §7 argued for this section is now the file's single rule (`docs/design-f35-unknown-keys.md`).
/// §7's reasoning still explains why this struct was the first: it guards a *status claim* — whether the corpus
/// is embedded at all — where an unrecognised key could become `provider = "none"` by omission. What F35 added
/// is the observation that a rendering section can lose just as much through a different door: `[catalogg]` is
/// not a stray key inside [`CatalogConfig`], it is that struct never being read at all.
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
    /// How loud llama.cpp may be on this program's stderr — **absent means unset, not `off`** (F32's sibling F33).
    ///
    /// Three things worth writing down, because each one was a review finding against the first draft:
    ///
    /// * The type is [`crate::loglevel::LogLevel`], which carries **no feature gate**. `[embeddings] log_level` is config vocabulary, and
    ///   np-cli builds np-core with `default-features = false`: gating the enum would make one key parse in a build with `local-model` and
    ///   refuse as unknown without it. Only installing a sink is gated, in `local_model`.
    /// * `Option`, because "the file said nothing" must stay distinguishable from "the file said `off`": the environment fills the first and
    ///   must not outvote the second ([`crate::loglevel::resolve_level`], §12 rule 7's precedence).
    /// * It is validated at load but **not** merged from the environment into this field, unlike `[models]`: the effective level is asked for
    ///   by name through [`crate::loglevel::resolve_level_from_env`], and a field that sometimes held the file's word and sometimes the
    ///   environment's would leave a reader unable to tell which they edited.
    pub log_level: Option<LogLevel>,
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
            // Unset rather than a level: see the field's note on why "nobody said" is not "said `off`".
            log_level: None,
        }
    }
}

/// Status markers (spec §8.7). Stored exactly as written here; later tasks match
/// them case-insensitively against the lowercased line.
///
/// `deny_unknown_fields` for the same reason and by the same rule as [`CatalogConfig`] (F35): a marker list that
/// silently failed to arrive does not look wrong on the file — it looks like a corpus with no done items.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
///
/// **`deny_unknown_fields`, since F35 — and this is the one that mattered.** A key inside a known section could
/// already be refused by that section; an *invented section* never reached any struct, so `[catalogg]` loaded
/// clean and left `[catalog]` at its defaults in silence. Refusing at the root is what closes the gap between
/// "a typo costs one setting" and "a typo costs a subsystem", and it gives up forward compatibility with
/// sections invented after this binary was built — a trade written down in `docs/design-f35-unknown-keys.md` §3
/// rather than discovered later.
/// How `np search` ranks (`docs/design-m3-hybrid-search.md`). Its own section because it changes an answer rather than what
/// is stored: nothing here affects a scan, an embed, or a byte in the database.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    /// Which lists a query may consult. Absent means [`SearchMode::Hybrid`](crate::search::SearchMode::Hybrid) — the default the human chose on 2026-11-07,
    /// with `semantic` and `keyword` one key away for anyone who wants one list alone. A value outside those three words is
    /// refused at parse time (F35's rule), naming what it expected rather than falling back.
    pub mode: crate::search::SearchMode,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            mode: crate::search::SearchMode::Hybrid,
        }
    }
}

/// `[log]`: how loud np's diagnostics are, where they also go, and when the file stops growing.
///
/// Three keys because those are the three questions a person debugging actually asks — *how much*, *where*, *until when*. There
/// is deliberately no `[log] format`: JSON appears in a file and nowhere else (a `np serve` test refuses any stderr line that
/// starts with `{`), so a format switch would be a knob with one legal position.
///
/// **`deny_unknown_fields`, by F35's rule for every container in this file.**
/// Every field is an `Option`, so "the file said nothing" is one representation and derived `Default` is the honest one: an
/// absent `[log]` section leaves the level unresolved, the destination unopened and the budget unset (which the sink reads as
/// [`DEFAULT_LOG_MAX_BYTES`]) rather than inventing a value for any of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// How much to record — a bare level or the full spec grammar ([`parse_diag_spec`]), **absent means unset, not `off`**.
    ///
    /// Held as written rather than as a parsed [`DiagSpec`], for the same reason `[embeddings] log_level` is held as a
    /// [`LogLevel`]: the effective value is resolved *by name* through [`resolve_diag`], from three sources at once, and a field
    /// that sometimes held the file's word and sometimes the merged answer would leave a reader unable to tell which they
    /// edited. It is still validated at load (see `Config::load_with_platform`), so a typo cannot route around the rule.
    pub level: Option<String>,
    /// Where records also go, as JSONL, opened in append mode. Absent means stderr only.
    ///
    /// The file format is frozen and documented (`docs/reference-diagnostics.md`); the human format on stderr is not. Nothing
    /// rotates this file — see [`DEFAULT_LOG_MAX_BYTES`] for what happens at the end of it, and why deleting it is the remedy.
    pub file: Option<PathBuf>,
    /// The byte budget for `[log] file`; **absent means [`DEFAULT_LOG_MAX_BYTES`]**.
    ///
    /// It bounds *the file*, not the run: a log already at its budget when a process opens it gets nothing more appended, which
    /// is the only way to keep that promise without the rotation this design refuses.
    pub max_bytes: Option<u64>,
}

/// `[log] max_bytes` when the key is absent: **8 MiB**, roughly 40 000 records.
///
/// More than any single debugging session reads, and small enough that a forgotten `[log] file` cannot fill a disk unnoticed.
/// The alternative the design considered and refused is rotation: on Windows another `np` may still hold the handle, and a
/// rename that fails halfway is a worse story than a log that stops honestly (spec point 8).
pub const DEFAULT_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// OS data dir, from `$XDG_DATA_HOME` else `$HOME/.local/share`
    /// (Windows: `%LOCALAPPDATA%\note-processor`, nested since **F37**). Empty only when the environment names none.
    pub data_dir: PathBuf,
    /// **F37**: set when the OS default resolved to its nested location but this program's index was found at the pre-F37
    /// one, so `data_dir` points back at the old place for this release. A diagnostic for stderr, never part of a report
    /// (`#[serde(skip)]`): every path a document prints is already the effective one, and a second path in the same document
    /// would read as though two indexes existed.
    #[serde(skip)]
    pub legacy_data_dir: Option<PathBuf>,
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
    /// Ranking choice for `np search`. An absent `[search]` section means hybrid, which is the default rather than a
    /// degraded mode: with no vectors installed, the keyword half is all there is to rank by, and it still answers.
    pub search: SearchConfig,
    /// Diagnostics: level, destination file, and that file's budget. An absent `[log]` section means "nothing asked for":
    /// capture at [`DIAG_DEFAULT_LEVEL`], stderr exactly as silent as it was before this section existed, and no file.
    pub log: LogConfig,
}

impl Default for Config {
    fn default() -> Self {
        // One resolution, both halves (`os_default_data_dir`), so the fallback cannot be applied in one entry point and
        // forgotten in another — which matters because `Config::default()` *is* the production path when no config file
        // exists yet: exactly the person upgrading from an older release whose index this has to find.
        let (data_dir, legacy_data_dir) = os_default_data_dir();
        Self {
            data_dir,
            legacy_data_dir,
            catalog: CatalogConfig::default(),
            markers: MarkerConfig::default(),
            embeddings: EmbeddingsConfig::default(),
            models: ModelsConfig::default(),
            search: SearchConfig::default(),
            log: LogConfig::default(),
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
    /// **Every key in the file must be one this build knows** (F35): all five containers — [`Config`] itself,
    /// [`CatalogConfig`], [`MarkerConfig`], [`EmbeddingsConfig`], [`ModelsConfig`] — carry
    /// `deny_unknown_fields`, so an unknown key or an invented section is [`ConfigError::Parse`] naming the file,
    /// the line and the key as written. Before F35 only `[embeddings]` and `[models]` refused anything, which
    /// meant a misspelled *section* was ignored whole; the decision and its cost are in
    /// `docs/design-f35-unknown-keys.md`.
    /// A `data_dir` neither the file nor the environment supplied is a failure, not a default:
    /// every path this program derives from it would go relative — a `workspaces/` directory next
    /// to whatever directory the command happened to run in. The CLI reports it and exits 1.
    pub fn load(override_: Option<&Path>) -> Result<Config, ConfigError> {
        Self::load_with_platform(override_, platform_of_default())
    }

    /// [`Config::load`] with the data-directory **shape** supplied rather than taken from `cfg` — the same seam
    /// [`resolve_data_dir`] exists for (**F37**), pushed up to the entry point that actually assembles a run.
    ///
    /// Production never passes anything but [`platform_of_default`]; the parameter exists because the question this answers
    /// ("did we fall back, and must we say so?") is decided by code Linux cannot reach through `load`, and a layout fix whose
    /// wiring is only exercised on the platform nobody here can run is how **F37**'s note shipped silent. One test drives the
    /// Windows shape end to end on this host; see `crates/np-core/tests/config.rs`.
    pub fn load_with_platform(
        override_: Option<&Path>,
        platform: DataDirPlatform,
    ) -> Result<Config, ConfigError> {
        // Which file the values came from, so the data-dir step below can ask that file a second question — *did you name a
        // `data_dir` yourself?* — instead of trusting the merged struct. Measured on Windows (**F37**): an upgraded machine
        // fell back to `%LOCALAPPDATA%` exactly as designed (its new workspaces landed there) and printed no note, because
        // [`Config::default`] had already resolved the fallback into `data_dir`, and "non-empty after merging" cannot tell an
        // instruction from a landing place. The directory was always right; only the announcement was missing.
        let mut source: Option<PathBuf> = None;
        let mut cfg = match override_ {
            Some(path) => {
                source = Some(path.to_path_buf());
                read_config(path)?
            }
            None => match config_file_path() {
                Some(path) => match read_config(&path) {
                    Err(ConfigError::Io(err)) if err.kind() == io::ErrorKind::NotFound => {
                        Config::default()
                    }
                    Ok(read) => {
                        source = Some(path);
                        read
                    }
                    other => other?,
                },
                None => Config::default(),
            },
        };
        {
            // One call answers both halves (**F37**): the file's `data_dir` wins when it named one — an instruction, not a
            // location we moved — and only the OS default is eligible for the pre-nesting fallback. Resolving through
            // [`resolve_data_dir`] rather than branching here is what lets a Linux test assert the explicit-directory rule in
            // the *Windows* shape, where the fallback could actually have applied.
            // Only the file's own key counts (**F37**). Reading `cfg.data_dir` here was the bug: on Windows it is non-empty by
            // default, so every run looked like an operator instruction, the fallback was reported as absent, and the stderr
            // note never printed on the one platform that has a fallback.
            let declared = source.as_deref().and_then(declared_data_dir);
            let explicit = declared.as_deref();
            let (dir, legacy) = os_data_dir_on(explicit, platform);
            if dir.as_os_str().is_empty() {
                // Neither the file nor the environment named a data dir. Every path derived from it would go relative — a
                // `workspaces/` directory beside whatever directory the command happened to run in — so this refuses.
                return Err(ConfigError::MissingField("data_dir"));
            }
            cfg.data_dir = dir;
            cfg.legacy_data_dir = legacy;
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
        // F33: the log level is *checked* here and stored nowhere. Two reasons, both stated because they are not obvious from the call:
        // the value can arrive from the environment, and a rule watching only the file would let an env typo route around it (the same
        // argument `resolve_models` gives for validating its merged values); and the effective level is then asked for by name through
        // [`crate::loglevel::resolve_level_from_env`] by the one caller that installs a sink, so merging it into `embeddings.log_level`
        // would make that field mean "the file's word" or "the environment's word" depending on load order.
        resolve_level_from_env(cfg.embeddings.log_level).map_err(invalid_data)?;
        // The same shape as F33's check one line above: **checked here, stored nowhere**, because the effective level arrives
        // from three sources and is resolved by name through [`resolve_diag`]. A `[log] level` that cannot be read refuses at
        // load rather than becoming a run that silently logged less than it was told to — and the refusal names the key, as
        // every other value in this file does.
        if let Some(raw) = cfg.log.level.as_deref() {
            parse_diag_spec(raw)
                .map_err(|err| invalid_data(format!("invalid log.level = {raw:?} — {err}")))?;
        }
        Ok(cfg)
    }

    /// This workspace's database: `data_dir/workspaces/<sha256(abs)[0..16]>.sqlite`.
    ///
    /// [`Config::workspace_models_dir`] places the same workspace's model artifacts beside it, from the same
    /// private `workspace_key`, because a row and the file it names belong to one workspace or to neither.
    ///
    /// The workspace need not exist: the canonical absolute spelling of `workspace`
    /// (current directory prepended when relative, separators that carry no meaning removed,
    /// no symlink resolution) is hashed, so the same directory always yields the same path
    /// whatever a caller typed — see `abs_path` for what "canonical" costs and what it refuses to do.
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
            .join(WORKSPACES_DIR)
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

/// Whether `key` is set to something that means "on": present, non-empty, and not exactly `0`.
///
/// Deliberately not [`env_text`]: that one refuses non-UTF-8 because both `[models]` string keys are ASCII by rule, and
/// refusing here would mean a machine whose environment is unparseable gets *no* answer about whether it may dial out —
/// the exact moment a safety switch must not shrug. A non-UTF-8 value is still not empty-or-`0`, so this reads it as "on".
fn env_flag_set(key: &str) -> bool {
    match env::var_os(key) {
        None => false,
        Some(value) => !(value.is_empty() || value == "0"),
    }
}

/// Non-empty environment variable as text; an empty value counts as unset, the rule [`env_path`] applies to paths., not skipped: both `[models]` keys are ASCII by rule (an origin,
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

/// The data directory the operator named **in the config file**, or `None` when they left it to this build.
///
/// A second, deliberate parse of the same text with a lenient shape (no `deny_unknown_fields`, no defaults): the question is
/// narrow — is there a top-level `data_dir` key? — and asking it through [`Config`] would answer with whatever
/// [`Config::default`] resolved, which is precisely the confusion **F37** shipped with. An unreadable or unparsable file yields
/// `None`, never an error: by the time [`Config::load`] asks this question it has already parsed the file strictly and refused
/// anything broken, so a `None` here can only mean "the key is not there".
///
/// Why the environment is not part of the answer: `$XDG_DATA_HOME` and `%LOCALAPPDATA%` are *inputs to the OS default*
/// ([`data_dir_for`]), not declarations. Treating them as declarations would make every run look operator-pinned and re-open
/// the same hole from the other side.
pub fn declared_data_dir(path: &Path) -> Option<PathBuf> {
    #[derive(serde::Deserialize)]
    struct DataDirProbe {
        data_dir: Option<PathBuf>,
    }
    let text = fs::read_to_string(path).ok()?;
    toml::from_str::<DataDirProbe>(&text).ok()?.data_dir
}

/// Absolute spelling of `path`, in the one spelling this program treats as an identity: the current
/// directory is prepended when it is relative, separators that name nothing are dropped, and symlinks
/// are not resolved.
///
/// Shared with [`crate::scan`], which walks and writes through this same spelling so the workspace
/// key in a database row and the name in the registry are the one absolute path — and shared with
/// [`WorkspaceRegistry::record`] for the same reason. That is why the normalisation lives *here*
/// rather than inside `workspace_key`: a client that spells one folder two ways (issue **#4**: a
/// Windows folder picker returns `D:\Notes\`, a saved "recent folders" file returns `D:\Notes`) must
/// not get two databases, two artifact directories — and therefore a second 309 MB download — *or*
/// two rows in the recent list disagreeing about whether it has seen this vault.
///
/// **What is dropped** ([`canonical_spelling`], which std's own [`Path::components`] iterator defines):
/// a trailing separator, a doubled separator, and a `.` component. All three say nothing about which
/// directory is meant, and on Windows the separator *style* disappears with them, because a component
/// is split on `/` and `\` alike — so `D:/Notes` and `D:\Notes` reach one key here rather than two.
///
/// **What is not resolved**, deliberately:
///
/// * **Symlinks.** [`std::fs::canonicalize`] would answer "which directory" exactly, and `workspace_key`'s
///   note records why it was ruled out: a key that moves when a link is re-pointed makes `np scan`
///   depend on the state of the filesystem above the vault, and every tool that spells the path through
///   the link would disagree with the one that spelled it directly.
/// * **`..`.** A parent component survives as itself. Collapsing `a/../b` lexically would be wrong wherever
///   `a` is a symlink — the same refusal as above, reached from the other side — and resolving it properly
///   is canonicalization. So `/v/a/../notes` is still a second spelling of one folder, and this is the
///   known residual: a client that sends `..` pays the re-index **#4** is about. Every GUI examined sends
///   a picker's path, which holds no `..`.
///
/// **What it costs.** A workspace first indexed under a spelling this function now rewrites — a trailing
/// separator, or a `.` (`np scan .`) — keys elsewhere after the change, so its index and its model cache
/// start empty: one `np scan` and, if vectors were wanted, one `np embed`. Nothing is lost (the old files
/// stay under the old key) and a clean spelling such as `/home/ana/notes` hashes exactly as it did before,
/// which `crates/np-core/tests/config.rs::a_clean_spelling_keys_where_it_did_before` pins by digest.
pub(crate) fn abs_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        // `workspace_db` is infallible by contract, so an unreadable current
        // directory leaves the spelling as given instead of failing.
        env::current_dir().unwrap_or_default().join(path)
    };
    canonical_spelling(&absolute)
}

/// The same directory with nothing left in its name but the directories themselves.
///
/// Rebuilt from [`Path::components`] rather than trimmed, because the iterator already knows the three
/// things worth knowing: `.` carries no meaning, a repeated separator carries no meaning, and Windows
/// splits on both separator styles. Rebuilding is also what keeps this total on a path made of bytes that
/// decode to nothing — components hand back the OS's own [`OsStr`] pieces, so no lossy text is introduced
/// between the path and its digest (`path_bytes` still decides the byte spelling, and F13's Windows
/// residual stays exactly where it was).
///
/// The separator between two pieces is inserted by this function and nowhere else, which is what keeps a
/// prefix honest: `D:` takes no separator before a *drive-relative* name (`D:notes` is not `D:\notes`) but
/// the root that follows a prefix supplies its own (`D:` + `\` = `D:\`). A plain `push` could not make
/// that distinction — on Windows it replaces the whole buffer when handed `\`, turning `D:\Notes` into the
/// UNC-shaped `\Notes`.
fn canonical_spelling(path: &Path) -> PathBuf {
    let mut text = OsString::new();
    // Whether the next piece has to be preceded by a separator. False at the start, after a root (the root
    // *is* a separator) and after a prefix, which either takes a root or takes its drive-relative name.
    let mut needs_separator = false;
    for component in path.components() {
        match component {
            // `.` names no directory: the one component with nothing to contribute.
            Component::CurDir => {}
            Component::RootDir => {
                text.push(OsStr::new(MAIN_SEPARATOR_STR));
                needs_separator = false;
            }
            Component::Prefix(prefix) => {
                text.push(prefix.as_os_str());
                needs_separator = false;
            }
            // `..` and an ordinary name both travel as the bytes the OS holds.
            piece @ (Component::ParentDir | Component::Normal(_)) => {
                if needs_separator {
                    text.push(OsStr::new(MAIN_SEPARATOR_STR));
                }
                text.push(piece.as_os_str());
                needs_separator = true;
            }
        }
    }
    PathBuf::from(text)
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

/// The folder this program nests its data files in on Windows (**F37**).
///
/// `config_dir()` has always appended a program folder (`%APPDATA%\note-processor`); the data side appended nothing, so a
/// Windows user shared `%LOCALAPPDATA%` — the directory every application shares — with two generically-named entries
/// (`workspaces`, `workspaces.sqlite`) and could not find their own index. The measurement and the ruling are F37 in
/// `docs/backlog-m2-followups.md`.
pub const WINDOWS_DATA_SUBDIR: &str = "note-processor";

/// Which data-directory shape to resolve.
///
/// **A value rather than a `cfg`, on purpose** (**F37**). Production picks the platform with `cfg` below and cannot do
/// otherwise; but CI is Linux, this development host cannot compile the Windows arm at all (`ring` and `libsqlite3-sys` need
/// MSVC `lib.exe`), and a layout fix whose Windows half never executes is a layout fix nobody verified — which is exactly how
/// the shape this card describes survived six CI rows. Passing the shape in lets `tests/config.rs` resolve both layouts on
/// one machine. It chooses *paths*, nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDirPlatform {
    /// `$XDG_DATA_HOME`, else `$HOME/.local/share`. Unchanged by F37: an unqualified `workspaces/` inside
    /// `~/.local/share` is far less exposed than in a shared Windows directory — which is why this is a Windows-only card.
    Unix,
    /// `%LOCALAPPDATA%\note-processor`: nested, as config already was.
    Windows,
}

/// Resolve the OS data dir from supplied environment values: pure, touching neither the real environment nor the disk.
///
/// `None` when the platform's variable is unset — [`Config::load`] refuses that rather than deriving
/// `workspaces/<hash>.sqlite` relative to the current directory.
pub fn data_dir_for(
    platform: DataDirPlatform,
    lookup: &dyn Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    match platform {
        // Nested since F37; `%LOCALAPPDATA%` is otherwise used as-is, with no `$HOME` fallback under it.
        DataDirPlatform::Windows => lookup("LOCALAPPDATA").map(|dir| dir.join(WINDOWS_DATA_SUBDIR)),
        DataDirPlatform::Unix => dir_from(
            lookup("XDG_DATA_HOME"),
            lookup("HOME"),
            Some(".local/share"),
        ),
    }
}

/// The one-release read-fallback (**F37**): when the nested location holds no index of ours and the pre-F37 location does,
/// this run keeps using the old directory.
///
/// What it prevents is a lie about the user's own files: an index is keyed by workspace path, so a moved data directory reads
/// as an empty workspace and a vault that has held months of notes answers `added 214`. That false report is the single worst
/// first experience this migration could produce, and it is the entire reason the fallback exists.
///
/// Conservative in three ways: the nested location wins as soon as it holds **our** registry file (so a fresh install after
/// upgrading is not dragged back to whatever unrelated `workspaces.sqlite` sits in `%LOCALAPPDATA%`); an empty or absent
/// legacy directory vetoes nothing; and either half counts as "ours" there — the registry, or the `workspaces/` directory
/// beside it, since an interrupted first run can leave one without the other. Falling back means reading *and writing* the
/// old place: splitting a registry into one directory and per-workspace databases into another is a worse bug than the layout
/// this card fixes. Removing the fallback after one release is its own change with its own relocation step, recorded as owed
/// on the card.
pub fn legacy_data_dir_override(nested: &Path, legacy: Option<&Path>) -> Option<PathBuf> {
    // `?` rather than a `let…else`: this function's whole answer is "no override unless every condition holds", so each
    // early exit is the same shape and spelling it with `?` keeps the three conditions readable as one list.
    let legacy = legacy?;
    if nested.join(REGISTRY_FILE).exists() {
        return None;
    }
    if legacy.join(REGISTRY_FILE).is_file() || legacy.join(WORKSPACES_DIR).is_dir() {
        Some(legacy.to_path_buf())
    } else {
        None
    }
}

/// Which [`DataDirPlatform`] this build's [`default_data_dir`] resolves — the one place `cfg` chooses it, so the fallback and
/// the paths can never disagree about which shape they are talking about.
#[cfg(windows)]
pub fn platform_of_default() -> DataDirPlatform {
    DataDirPlatform::Windows
}

/// Which [`DataDirPlatform`] this build's [`default_data_dir`] resolves. See the Windows arm.
#[cfg(not(windows))]
pub fn platform_of_default() -> DataDirPlatform {
    DataDirPlatform::Unix
}

/// Which directory a run uses, and whether it fell back: `(effective dir, legacy dir we are reading instead)`.
///
/// **The whole decision, in one pure function** — which is what makes the Windows shape testable on Linux (**F37**). Both
/// questions the layout asks are here: does the operator's own `data_dir` win (it always does; it is an instruction, not a
/// place we moved), and if we are using the OS default, is our index still sitting at the pre-nesting location? Passing the
/// platform and the environment in — rather than reading `cfg` and `env::var_os` inside — lets `tests/config.rs` drive the
/// Windows shape with a `%LOCALAPPDATA%` that does not exist, which is the only way this arm gets verified at all.
///
/// An empty first element means nothing was resolved: no explicit directory, and the environment named none.
pub fn resolve_data_dir(
    explicit_dir: Option<&Path>,
    platform: DataDirPlatform,
    lookup: &dyn Fn(&str) -> Option<PathBuf>,
) -> (PathBuf, Option<PathBuf>) {
    if let Some(dir) = explicit_dir {
        return (dir.to_path_buf(), None);
    }
    let Some(nested) = data_dir_for(platform, lookup) else {
        return (PathBuf::new(), None);
    };
    match legacy_data_dir_override(&nested, legacy_location(platform, lookup).as_deref()) {
        Some(legacy) => (legacy.clone(), Some(legacy)),
        None => (nested, None),
    }
}

/// Where this program's data sat **before** F37 nested it, or `None` on a shape that never had a separate location.
///
/// Takes the environment as a parameter so [`resolve_data_dir`] stays pure — which is what lets the Windows arm run on Linux CI
/// against a `%LOCALAPPDATA%` that is a tempdir rather than a registry this host cannot create. Empty counts as unset, the way
/// `env_path` already treats it.
fn legacy_location(
    platform: DataDirPlatform,
    lookup: &dyn Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    match platform {
        DataDirPlatform::Windows => {
            lookup("LOCALAPPDATA").filter(|value| !value.as_os_str().is_empty())
        }
        DataDirPlatform::Unix => None,
    }
}

/// The OS default, with **F37**'s one-release fallback applied: `(effective dir, legacy dir we fell back to)`. Both halves come
/// from one call so no entry point can apply the nesting and forget the fallback. An empty first element means the
/// environment named nothing, which [`Config::load`] refuses.
fn os_default_data_dir() -> (PathBuf, Option<PathBuf>) {
    resolve_data_dir(None, platform_of_default(), &|key| env_path(key))
}

/// The same resolution with the file's word in front of it, for whatever shape `load` was handed.
fn os_data_dir_on(
    explicit: Option<&Path>,
    platform: DataDirPlatform,
) -> (PathBuf, Option<PathBuf>) {
    resolve_data_dir(explicit, platform, &|key| env_path(key))
}

/// OS data dir: `$XDG_DATA_HOME` else `$HOME/.local/share`; on Windows `%LOCALAPPDATA%\note-processor` (**F37**).
/// `None` when the environment names no data directory at all — what [`Config::load`] refuses rather than deriving paths
/// relative to the current directory. Note this is the *nested* answer; use [`os_default_data_dir`] for the one an actual run
/// should use, which may be the pre-F37 location for one release.
#[cfg(windows)]
pub fn default_data_dir() -> Option<PathBuf> {
    data_dir_for(DataDirPlatform::Windows, &|key| env_path(key))
}

/// OS data dir: `$XDG_DATA_HOME` else `$HOME/.local/share`. See the Windows arm for what F37 changed there and why this shape
/// needed no change. `None` when the environment names nothing, which [`Config::load`] refuses.
#[cfg(not(windows))]
pub fn default_data_dir() -> Option<PathBuf> {
    data_dir_for(DataDirPlatform::Unix, &|key| env_path(key))
}

// ---------------------------------------------------------------------------------------------
// The diagnostic level vocabulary: what a level is spelled, how a spec of them is written, and which
// source won when file, environment and flag disagreed
// (`docs/superpowers/specs/2026-12-10-np-diagnostics-design.md`).
//
// This section decides; it never writes. The sinks that consume a resolved [`DiagSpec`] live in np-cli, which owns the process
// and its stderr — np-core's half is the vocabulary, so that `np`, a test harness, and a library caller embedding this crate
// all mean the same thing by `np_core::search=trace`.
// ---------------------------------------------------------------------------------------------

/// The permitted spellings of a diagnostic level, in the order a refusal lists them.
///
/// One constant for the reason [`crate::loglevel::LEVEL_NAMES`] is one: this list is what a refusal promises, and
/// `crates/np-core/tests/config.rs` parses every name in it, so dropping one from [`Level`]'s parser while leaving it here
/// turns a green suite red instead of shipping a word the documentation promised.
///
/// Six where F33 has five. `[embeddings] log_level` stops at `debug` because ggml has no word above it, and the two knobs stay
/// separate on purpose (spec point 6): asking to trace the search path must not also dump a model loader's hundred-and-twenty
/// line transcript, which is the only reason F33 exists.
pub const DIAG_LEVEL_NAMES: [&str; 6] = ["off", "error", "warn", "info", "debug", "trace"];

/// The level a run captures when neither the file, the environment nor a flag named one.
///
/// `Warn`, which is what keeps the promise the whole design hangs on (**spec point 1**): an ordinary `np scan` prints exactly
/// what it prints today, because at `warn` the refusals are the only things worth saying and every one of them already has its
/// pinned sentence. Deeper than this is something a person asks for.
pub const DIAG_DEFAULT_LEVEL: Level = Level::Warn;

/// The environment variable that supplies `[log] level` when the file leaves it unset.
///
/// **`RUST_LOG` is deliberately not honoured**, even though every reader of this line knows it. Two variables with opposite
/// precedence orders in one binary is a support story nobody wins: `RUST_LOG` follows the ecosystem's env-beats-file rule, and
/// this program's house rule (§12 rule 7) is file-beats-env, pinned for the other log knob at [`crate::loglevel::resolve_level`]
/// and by `crates/np-cli/tests/cli.rs`. One variable, one order, documented in `docs/reference-diagnostics.md`.
pub const DIAG_ENV_VAR: &str = "NP_LOG";

/// The config key this vocabulary is read from, spelled as a refusal spells it (**F35**'s habit: name the key, not the struct).
pub const DIAG_CONFIG_KEY: &str = "log.level";

/// The flag that outvotes both of them, spelled as a refusal spells it.
pub const DIAG_FLAG: &str = "--log-spec";

/// One depth of np's diagnostic channel.
///
/// Six names over the ten targets listed in `docs/reference-diagnostics.md`: `off` silences one target or everything, `trace`
/// is the per-note and per-chunk depth. The declaration order below is the *speaking* order (`off < error < … < trace`) rather
/// than a numeric protocol, and nothing in np-core compares these values — a resolved spec becomes filter directives in np-cli,
/// where the comparison happens once per event inside the subscriber's own filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Level {
    /// Record nothing. Legal for the whole run (`NP_LOG=off`) and for one target inside it
    /// (`np_core::scan=off`).
    Off,
    /// Failures only.
    Error,
    /// Refusals and degraded states — the default, and the level every existing user-facing sentence already lives at.
    #[default]
    Warn,
    /// Effects: what a command decided, with timing. What `-v` asks for.
    Info,
    /// Per-decision detail — which rule fired on which path. What `-vv` asks for.
    Debug,
    /// Per-item detail: one line per note, per chunk, per candidate. `-vvv`, and the level that makes the file sink worth having.
    Trace,
}

impl Level {
    /// The spelling this level parses from and prints as — always one of [`DIAG_LEVEL_NAMES`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A level name that is not one of [`DIAG_LEVEL_NAMES`].
///
/// Carries the finished sentence rather than the offending word because a spec can be wrong in three shapes — a bad bare
/// level, a bad override (`np_core::search=verbose`, which has to name the target), and an empty item — and each sentence is
/// written where it is discovered instead of being reassembled from parts here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDiagSpecError(String);

impl fmt::Display for ParseDiagSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseDiagSpecError {}

/// What to record, and where to stop: one default level plus the targets that want a different one.
///
/// `per_target` is a list rather than a map so the order a person wrote survives a round trip — the same order np-cli hands to
/// the subscriber, and the order a bug report quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagSpec {
    /// The level for every target [`DiagSpec::per_target`] does not name.
    pub default: Level,
    /// Per-target overrides, in the order written, each target appearing once (the last spelling wins).
    pub per_target: Vec<(String, Level)>,
}

impl Default for DiagSpec {
    fn default() -> Self {
        Self {
            default: DIAG_DEFAULT_LEVEL,
            per_target: Vec::new(),
        }
    }
}

/// Parse one spec: a bare level (`debug`), or a comma list of `target=level` with an optional bare level first
/// (`info,np_core::search=trace`).
///
/// Three rules worth stating because each one was someone's question:
///
/// * **Unknown targets are accepted.** `np_core::vision=trace` on a build without vision is not a typo in this spec — the name
///   may belong to a target another build emits, and refusing it would make the documented set unshippable in parts.
/// * **A bare level must come first**, because “the default is `warn`, unless you said otherwise” needs one place to say it.
/// * **Whitespace between items is a separator.** A spec is a list of tokens, and `info, x=trace` means what it looks like;
///   the strictness lives in the *names* (case-sensitive, exactly as [`crate::loglevel::LogLevel`] is), not in the commas.
pub fn parse_diag_spec(raw: &str) -> Result<DiagSpec, ParseDiagSpecError> {
    if raw.is_empty() {
        return Err(ParseDiagSpecError(format!(
            "an empty log spec names nothing — write one of {}",
            DIAG_LEVEL_NAMES.join(", ")
        )));
    }
    let mut spec = DiagSpec::default();
    // `seen_bare` is what makes a second bare level a refusal instead of a coin flip: only item 0 may omit `target=`.
    for (index, item) in raw.split(',').enumerate() {
        let item = item.trim();
        if item.is_empty() {
            return Err(ParseDiagSpecError(format!(
                "empty item {} of {raw:?} — write `target=level` or one of {}",
                index + 1,
                DIAG_LEVEL_NAMES.join(", ")
            )));
        }
        let (target, level_text) = match item.split_once('=') {
            // `=trace` alone has no target to attach the level to, and guessing one would invent a filter nobody wrote.
            Some(("", _)) => {
                return Err(ParseDiagSpecError(format!(
                    "{raw:?} has an item with no target before `=`"
                )))
            }
            Some((target, level)) => (Some(target), level),
            None if index == 0 => (None, item),
            None => {
                return Err(ParseDiagSpecError(format!(
                    "{item:?} is a bare level in position {}: only the first item may name the default \
                     (write `{item}=…` for a target)",
                    index + 1
                )));
            }
        };
        let level = parse_level(level_text).map_err(|err| match target {
            // Name the offending target: in a comma list, "which one was wrong" is the whole question.
            Some(target) => ParseDiagSpecError(format!("{target}={level_text}: {err}")),
            None => err,
        })?;
        match target {
            None => spec.default = level,
            Some(target) => replace_target(&mut spec.per_target, target, level),
        }
    }
    Ok(spec)
}

/// One level name, case-sensitively — the same strictness [`crate::loglevel::LogLevel`] shows, for the same reason: two
/// spellings for one setting in a file where every other value is exactly as written.
fn parse_level(text: &str) -> Result<Level, ParseDiagSpecError> {
    // Case-sensitive on purpose, as [`crate::loglevel::LogLevel`] is: `INFO` is a different string from `info`, and accepting
    // both would mean two spellings for one setting in a file where every other value is exactly as written.
    match text {
        "off" => Ok(Level::Off),
        "error" => Ok(Level::Error),
        "warn" => Ok(Level::Warn),
        "info" => Ok(Level::Info),
        "debug" => Ok(Level::Debug),
        "trace" => Ok(Level::Trace),
        other => Err(ParseDiagSpecError(format!(
            "{other:?} is not a log level — write one of {}",
            DIAG_LEVEL_NAMES.join(", ")
        ))),
    }
}

/// Set one target's level, replacing an earlier spelling so a target appears once and the last word wins.
fn replace_target(per_target: &mut Vec<(String, Level)>, target: &str, level: Level) {
    match per_target.iter_mut().find(|(name, _)| name == target) {
        Some(slot) => slot.1 = level,
        None => per_target.push((target.to_string(), level)),
    }
}

/// Which source supplied this run's levels — printed on stderr when someone asked for diagnostics, because “the environment did
/// nothing” is otherwise invisible (spec point 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagSource {
    /// Nobody named a level; [`DIAG_DEFAULT_LEVEL`] applies.
    Default,
    /// `[log] level` in the config file.
    Config,
    /// `NP_LOG`.
    Env,
    /// `-v` / `--log-spec` on this command line.
    Flag,
}

impl DiagSource {
    /// The word a person sees for this source, and the only place those four words are written.
    ///
    /// Pinned by `crates/np-cli/tests/diag.rs` through the announce line (`log level info from config.toml`), so the enum and
    /// the sentence cannot drift apart.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Config => "config.toml",
            Self::Env => "NP_LOG",
            Self::Flag => "command line",
        }
    }
}

/// Resolve the levels from every source: **flag > file > environment > [`DIAG_DEFAULT_LEVEL`]**, and say which one won.
///
/// Two rules travel with the order, both inherited rather than invented here:
///
/// * **The file outranks the environment** (§12 rule 7, amended by the human on 2026-11-04 and already pinned for
///   `[embeddings] log_level` at [`crate::loglevel::resolve_level`] / `crates/np-cli/tests/cli.rs`). What [`DIAG_ENV_VAR`]
///   buys is a level without editing a file, not a way to outvote a file that named one.
/// * **Every present value is parsed, even a losing one.** A bad `NP_LOG` refuses even when the file would have won, because a
///   typo in the hatch must be visible rather than silently meaning “whatever the file said” — [`crate::loglevel::resolve_level`]
///   says the same thing about its own hatch.
///
/// An *empty* value counts as unset, which is this file's rule for environment text everywhere (`env_text`) rather than a second
/// opinion: `NP_LOG= np scan` gets the file's level or the default, not a refusal.
///
/// The winner supplies the **whole** spec, overrides included — one source, one story. Merging `[log] level`'s default with
/// `NP_LOG`'s overrides would make the printed line (“from NP_LOG”) describe only half of what was applied, which is exactly
/// the invisibility this feature exists to remove.
///
/// The failure text names the variable or key it came from, as [`crate::loglevel::resolve_level`] does: telling someone to edit
/// a file that does not hold the value wastes the one thing a refusal is for.
pub fn resolve_diag(
    cli: Option<&str>,
    env: Option<&str>,
    config: Option<&str>,
) -> Result<(DiagSpec, DiagSource), String> {
    // Parsed in *reading* order so every bad value is seen; chosen in *precedence* order.
    let from_flag = present(cli)
        .map(|value| parse_named(value, DIAG_FLAG))
        .transpose()?;
    let from_env = present(env)
        .map(|value| parse_named(value, DIAG_ENV_VAR))
        .transpose()?;
    let from_config = present(config)
        .map(|value| parse_named(value, DIAG_CONFIG_KEY))
        .transpose()?;

    // Chosen in precedence order: the flag is this run's word, the file outranks the environment (§12 rule 7), and the
    // environment speaks only where both above it are silent.
    Ok(from_flag
        .map(|spec| (spec, DiagSource::Flag))
        .or_else(|| from_config.map(|spec| (spec, DiagSource::Config)))
        .or_else(|| from_env.map(|spec| (spec, DiagSource::Env)))
        .unwrap_or_else(|| (DiagSpec::default(), DiagSource::Default)))
}

/// Whether a source spoke: present and non-empty.
fn present(value: Option<&str>) -> Option<&str> {
    value.filter(|text| !text.is_empty())
}

/// Parse one source's value, prefixing any complaint with the spelling of that source.
fn parse_named(value: &str, source: &str) -> Result<DiagSpec, String> {
    parse_diag_spec(value).map_err(|err| format!("{source}: {err}"))
}

/// What `[log]` says, read **leniently**: without validating the rest of the file, and without refusing anything.
///
/// This exists for one reason, and it is a real one: the sink has to be installed before `Config::load` runs (spec point 3 —
/// "a config that refuses must not become the one failure with no diagnostic"), and the level announced at install time has to
/// be the level that will actually run. Since `[log] level` outranks `NP_LOG`, installing from flag-and-env alone would announce
/// a value the config file then quietly overrode. So np-cli asks this question first, gets a truthful answer even from a file
/// that is about to refuse, and asks the strict question (`[LogConfig]`) once the file has loaded.
///
/// **Lenient means lenient**: no TOML at all, no `log` table, or a value of the wrong type all read as "nothing was said", never
/// as an error. The strict path still refuses what this one ignores — a typo in `[log] level` fails the run at load with its
/// key named — so nothing is *accepted* here that the file would not accept; only reported early.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogPeek {
    /// `[log] level` as written, if it is a string.
    pub level: Option<String>,
    /// `[log] file`, if it is a string.
    pub file: Option<PathBuf>,
    /// `[log] max_bytes`, if it is a non-negative integer.
    pub max_bytes: Option<u64>,
}

/// [`LogPeek`] for the file this run would read ([`config_file_path`]), or nothing when there is no such file to read.
#[must_use]
pub fn peek_log_config() -> LogPeek {
    let Some(path) = config_file_path() else {
        return LogPeek::default();
    };
    let Ok(text) = fs::read_to_string(path) else {
        return LogPeek::default();
    };
    // Deserialise the **document** into a table. `text.parse::<toml::Value>()` would be wrong and quietly so: `FromStr` for
    // `toml::Value` parses a TOML *value*, not a file, so a real config — which always opens with a key or a `[table]` header —
    // fails to parse at column 8 and the peek reports silence about every file in the world.
    let Ok(document) = toml::from_str::<toml::Table>(&text) else {
        return LogPeek::default();
    };
    let Some(table) = document.get("log").and_then(toml::Value::as_table) else {
        return LogPeek::default();
    };
    LogPeek {
        level: table
            .get("level")
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        file: table
            .get("file")
            .and_then(toml::Value::as_str)
            .map(PathBuf::from),
        max_bytes: table
            .get("max_bytes")
            .and_then(toml::Value::as_integer)
            .and_then(|bytes| u64::try_from(bytes).ok()),
    }
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
