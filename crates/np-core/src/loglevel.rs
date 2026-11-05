//! The log-level vocabulary behind `[embeddings] log_level` (F33).
//!
//! llama.cpp writes its load transcript to stderr through a process-global sink, and v0.8.0's own header says what happens when nobody
//! installs one: “If this is not called, or NULL is supplied, **everything** is output on stderr” (`include/llama.h:1672-1676`). On a
//! healthy run that is roughly 120 lines ahead of a one-line answer — `docs/design-f33-llama-log-level.md` §1 measures it for both
//! commands. This module decides *which* of those lines a user asked to see. It decides; it never writes anything and never installs a
//! callback.
//!
//! **Why this file carries no feature gate.** The five names are config vocabulary: `[embeddings] log_level` is a field of
//! [`crate::config::EmbeddingsConfig`], np-cli builds np-core with `default-features = false`, and a key that parses in one build and
//! refuses as unknown in the next is a defect rather than a design. Keeping the names, the gate and the precedence ungated also means every
//! CI row grades them — including rows that link no model at all. The FFI declaration, the callback and the installer live beside the
//! runtime they need, under `local-model`.
//!
//! Nothing here is load-bearing for a library caller: np-core installs no sink, so embedding np-core as a library leaves llama.cpp's own
//! stderr behaviour exactly as upstream wrote it. Only `np` opts in, because only `np` owns the process.

use std::fmt;
use std::str::FromStr;

/// The permitted spellings, in the order a refusal lists them.
///
/// One constant on purpose: the parser accepts this list and the error message promises it, so the mutant “drop `debug` from
/// [`LogLevel::from_str`] while leaving it here” cannot produce a refusal that names a value the parser would reject.
pub const LEVEL_NAMES: [&str; 5] = ["off", "error", "warn", "info", "debug"];

/// The level a run uses when neither the file nor the environment named one.
///
/// `Warn` rather than llama.cpp's “everything”: the transcript is what F33 exists to quiet, and a load that *fails* reports its reason at
/// error level (`src/llama.cpp:373-375` logs `error loading model: …`), so silence-by-default never means a failure goes unexplained.
pub const DEFAULT_LEVEL: LogLevel = LogLevel::Warn;

/// The environment variable that fills the key when the config file leaves it unset.
///
/// It *fills*, it does not override — see [`resolve_level`].
pub const LEVEL_ENV_VAR: &str = "NP_LLAMA_LOG_LEVEL";

/// llama.cpp's own `ggml_log_level` numbering, copied from the pinned tree (`ggml/include/ggml.h:653-660`) rather than invented.
mod ggml {
    pub const DEBUG: i32 = 1;
    pub const INFO: i32 = 2;
    pub const WARN: i32 = 3;
    pub const ERROR: i32 = 4;
    /// Not a level of its own: it extends the line already in progress, which is how load progress gets drawn.
    pub const CONT: i32 = 5;
}

/// How loud llama.cpp may be on this program's stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLevel {
    /// Forward nothing, including an error. This is *our* choice, not llama.cpp's `NONE`, and it sorts above every raw level so the gate
    /// in [`LogLevel::forwards`] can express it.
    Off,
    /// Error lines only.
    Error,
    /// Warnings and errors — the default, and enough to explain a failed load.
    Warn,
    /// Add llama.cpp's informational transcript: the model loader's metadata dump, tensor list and build-graph lines.
    Info,
    /// Everything upstream logs at `DEBUG` or above — the widest setting this gate offers. `NONE` (0) is *below* `DEBUG` in ggml's own
    /// numbering and stays silent here, along with any number this file does not recognise.
    Debug,
}

impl LogLevel {
    /// The spelling this level parses from and prints as — always one of [`LEVEL_NAMES`].
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }

    /// The lowest raw level this threshold forwards.
    const fn floor(self) -> i32 {
        match self {
            Self::Debug => ggml::DEBUG,
            Self::Info => ggml::INFO,
            Self::Warn => ggml::WARN,
            Self::Error => ggml::ERROR,
            // Not `ggml::NONE`: 0 is *below* every raw level, so a floor of 0 would forward the whole transcript and mean the exact
            // inverse of "off". Upstream's numbers have no room for this choice, which is why it borrows none of them.
            Self::Off => i32::MAX,
        }
    }

    /// Whether a message at llama.cpp's raw `level` reaches stderr under this threshold.
    ///
    /// Two rules live here, and both are pinned by `tests/loglevel.rs`:
    ///
    /// * **A continuation rides with [`LogLevel::Info`].** `GGML_LOG_LEVEL_CONT` extends the line in progress — np-core takes
    ///   `llama_model_default_params()`, so llama.cpp installs its own progress callback and that callback's entire output is `.` at
    ///   `CONT` (`src/llama.cpp:413-423`). Forwarding a fragment of a line whose opening was filtered out would print half a sentence, so a
    ///   continuation is treated as informational: visible when informational output is wanted, silent otherwise. This is a decision
    ///   (design §4), not a fact from the header.
    /// * **An unrecognised number forwards nothing.** A level this file has never seen is a reason to write nothing rather than to guess;
    ///   inventing a line on someone's stderr is not a safe default, and if upstream renumbers, silence in an unexpected row is what makes
    ///   `tests/loglevel.rs` fail loudly instead of quietly leaking the transcript.
    #[must_use]
    pub const fn forwards(self, level: i32) -> bool {
        let effective = if level == ggml::CONT {
            ggml::INFO
        } else if level == ggml::DEBUG
            || level == ggml::INFO
            || level == ggml::WARN
            || level == ggml::ERROR
        {
            level
        } else {
            return false;
        };
        effective >= self.floor()
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A level name that is not one of [`LEVEL_NAMES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseLogLevelError(String);

impl fmt::Display for ParseLogLevelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a log level — write one of {}",
            self.0,
            LEVEL_NAMES.join(", ")
        )
    }
}

impl std::error::Error for ParseLogLevelError {}

impl FromStr for LogLevel {
    type Err = ParseLogLevelError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        // Case-sensitive on purpose: `INFO` is a different string from `info`, and accepting both would mean two spellings for one
        // setting in a file where every other value is exactly as written. `[embeddings] threads = "AUTO"` refuses for the same reason.
        match text {
            "off" => Ok(Self::Off),
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            other => Err(ParseLogLevelError(other.to_owned())),
        }
    }
}

/// Resolve the level from the file and the environment: **the file wins, the environment fills what the file left unset**, and
/// [`DEFAULT_LEVEL`] applies only when both are silent.
///
/// This is this program's one existing config-versus-environment rule — `Config::load` states it for `data_dir`, every `[models]` key
/// follows it, and §12 rule 7 of the M2 design says the environment “supplies a key the file left unset — never an override of a file that
/// names one”. That rule was amended by the human on 2026-11-04, so a diagnostic hatch does not get to invent an exception to it: what
/// [`LEVEL_ENV_VAR`] buys is a level without editing a file, not a way to outvote a file that named one. An earlier draft of
/// `docs/design-f33-llama-log-level.md` said the opposite and cited this very rule as its precedent; review caught the contradiction.
///
/// A value that is not one of [`LEVEL_NAMES`] refuses **even when the file would have won**, for the reason `[models]` gives: a mistyped
/// hatch should be visible rather than silently meaning “whatever the file said”. An *empty* value counts as unset, which is also
/// `config.rs`'s existing rule for environment text (`env_text`) rather than a second opinion — so `NP_LLAMA_LOG_LEVEL= np embed` gets the
/// default rather than a refusal, exactly as `NP_MODELS_BASE_URL=` gets “not configured”. The refusal names the variable, because telling
/// someone to edit a file that does not hold the value wastes the one thing a refusal is for.
pub fn resolve_level(env: Option<&str>, file: Option<LogLevel>) -> Result<LogLevel, String> {
    let Some(raw) = env.filter(|value| !value.is_empty()) else {
        return Ok(file.unwrap_or(DEFAULT_LEVEL));
    };
    let parsed = LogLevel::from_str(raw).map_err(|err| format!("{LEVEL_ENV_VAR}: {err}"))?;
    // When the file named a level it wins, and the value parsed above is dropped on purpose: checking it first is what makes a typo in the
    // hatch visible instead of turning it into "the file's value, quietly".
    Ok(file.unwrap_or(parsed))
}

/// The level for this run, reading [`LEVEL_ENV_VAR`] the way this program reads every other environment value.
///
/// Lives here rather than in np-cli so the two rules that belong to this key — “empty means unset” and “not valid text is a refusal, not a
/// shrug” — are stated once, in the file that owns the vocabulary, instead of being re-derived by each caller.
pub fn resolve_level_from_env(file: Option<LogLevel>) -> Result<LogLevel, String> {
    let raw = match std::env::var_os(LEVEL_ENV_VAR) {
        None => None,
        Some(value) if value.is_empty() => None,
        Some(value) => Some(
            value
                .into_string()
                .map_err(|_| format!("{LEVEL_ENV_VAR}: the value is not valid text"))?,
        ),
    };
    resolve_level(raw.as_deref(), file)
}

/// Hand-written for the same reason [`ThreadCount`](crate::config::ThreadCount)'s is: the derived failure for a string enum says “unknown
/// variant”, which is a sentence about the implementation rather than one a user can act on. This names the key and lists the five
/// spellings from [`LEVEL_NAMES`], so a refusal cannot promise a name the parser would reject — the mutant “drop `debug` from
/// [`FromStr`] while leaving it in the message” has to fail, and does (`tests/loglevel.rs`, `tests/config.rs`).
impl<'de> serde::Deserialize<'de> for LogLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = LogLevel;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "one of {}", LEVEL_NAMES.join(", "))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                LogLevel::from_str(value).map_err(|_| {
                    E::custom(format!(
                        "embeddings.log_level = {value:?} is not a log level — write one of {}",
                        LEVEL_NAMES.join(", ")
                    ))
                })
            }
        }

        // `deserialize_str` rather than `deserialize_string`: the value is borrowed from the document, and a level has no reason to
        // allocate on its way into an error message that will be dropped. A non-string (a number, a bool) fails here with serde's own
        // type complaint, which names the key through the rendered source line — the same split `threads` documents.
        deserializer.deserialize_str(Visitor)
    }
}
