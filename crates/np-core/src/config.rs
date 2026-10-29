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
    /// A model running inside this process (`local-model`). What that costs and how it is loaded is
    /// §6's benchmark's question; nothing in this crate reads the variant yet.
    LocalModel,
    /// An OpenAI-compatible HTTP endpoint. Its base URL and key arrive at runtime as
    /// `NP_EMBED_BASE_URL` / `NP_EMBED_API_KEY` and never enter a config file (§9), so this section
    /// holds no endpoint field to leak.
    ///
    /// Renamed by hand because `rename_all` cannot know it: serde kebab-cases `OpenAiCompatible` into
    /// `open-ai-compatible`, while §2 and the roadmap spell the config value `openai-compatible`. The
    /// explicit attribute is what keeps the documented spelling the only accepted one — and
    /// `tests/config.rs` parses every variant from a file precisely so this cannot drift.
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
}

/// The `[embeddings]` section: which provider to ask, and the two identity fields that provider will
/// need (`docs/design-m2-embeddings.md` §2, §7).
///
/// **`deny_unknown_fields`, unlike every other struct in this file.** That is a deliberate break with
/// the lenient posture of [`CatalogConfig`] and [`MarkerConfig`], and §7 says why: those structs guard
/// rendering choices, where an unrecognised key costs one wrong-looking line, while this one guards a
/// *status claim* — whether the corpus is embedded at all. A misspelled key there must not become
/// `provider = "none"` by omission, so the file is refused instead. The strictness stops at this
/// section's boundary: unknown keys elsewhere keep loading as they always have (`tests/config.rs`
/// asserts both halves, because a reader of one half cannot tell a scoped rule from a global one).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
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
    /// Also unread so far, for the same reason as [`EmbeddingsConfig::model`]. `u16` rather than
    /// `usize` because it is a *declared* width and the schema's `CHECK (length(vector) = 4 * dim)`
    /// has to hold: 65 535 components is 256 KiB per row, already past any model this project would
    /// run, so the type refuses an absurdity before a provider has to.
    pub dim: Option<u16>,
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
