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
    /// recognise under `[embeddings]`, which that section refuses (`EmbeddingsConfig`'s
    /// `deny_unknown_fields`, `docs/design-m2-embeddings.md` §7).
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
/// number has to sit above what the chunker normally produces while still being a fence. §5's own arithmetic
/// says 2048 leaves room for `prefix_doc` plus specials on top of a `MAX_CHUNK_BYTES = 1800` chunk, and R6d
/// measures real bytes-per-token before anyone moves it.
pub const DEFAULT_EMBEDDING_TOKENS: usize = 2048;

/// Largest `embeddings.max_tokens` a config file may ask for.
///
/// The largest sequence the configured model can attend to in one embedding: **8,192 tokens**, from Google's
/// `embeddinggemma-2` card (read 2026-10-29). This is a model fact, not a product judgement — and it replaced an
/// earlier ceiling of 32 768 that was justified by `max_position_embeddings = 262144`, a RoPE/train-length property
/// the design has since disowned (`docs/design-m2-local-embeddings.md` §5). A bound above what the model can read is
/// not generous, it is false: `validate_embeddings` would have gone on inviting `max_tokens = 16384` values that no
/// runtime can honour.
///
/// The budget counts **what the model sees** — `prefix_doc` + chunk text + special tokens, because
/// `include_prompt = true` means the counted sequence is not just our bytes — so a caller near this ceiling has to
/// leave room for the prefix and specials too. [`crate::chunk`] caps a chunk at `MAX_CHUNK_BYTES = 1800`, which makes
/// a four-digit-token sequence unlikely for prose; **unlikely is the whole claim** (§5's holes are named there:
/// normalisation can expand bytes, `decode` emits U+FFFD runs, prefix and specials cost tokens from no chunk bytes).
/// R6d measures bytes-per-token on this corpus.
///
/// Two honest limits on the number itself. It is **model-independent while the window is not**: 8,192 is the smallest
/// window any model configured *today* may have, and a spec-named fallback (MiniLM, ~512 tokens) would need a smaller
/// bound — so the follow-up is to move this check next to the provider rather than leave a global constant doing a
/// per-model job. And it is a load-time fence only: R6b2 enforces it over real token counts. Posture unchanged from
/// [`MAX_CONTINUATION_INDENT`] (F17 §4): enforce at load, refuse rather than clamp.
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
    /// will resolve to, owning the API and refusing until R6b1 builds the runtime. **Nothing instantiates it from
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir().unwrap_or_default(),
            catalog: CatalogConfig::default(),
            markers: MarkerConfig::default(),
            embeddings: EmbeddingsConfig::default(),
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
    /// Unknown keys are allowed everywhere except `[embeddings]`, which refuses them
    /// ([`EmbeddingsConfig`], §7 of the M2a design).
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
            "invalid embeddings.max_tokens = {}: this model attends to at most \
             {MAX_EMBEDDING_TOKENS} tokens per embedding, and the budget counts prefix + chunk + special tokens \
             (google/embeddinggemma-2 card, read 2026-10-29), so the value must be between 1 and \
             {MAX_EMBEDDING_TOKENS}",
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
