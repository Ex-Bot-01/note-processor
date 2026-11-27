use crate::diag;
use np_core::config::Config;
use np_core::db::Db;
use np_core::models;
use serde_json::{json, Value};
use std::error::Error;
use std::io;
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Which way a command or request failed, as the log's outcome record names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    /// SQLite reported the workspace database locked: another connection held its write lock past the busy timeout.
    DatabaseBusy,
    /// Anything else that ended the work without its result.
    Other,
}

impl Failure {
    /// Classify `err` by walking its source chain for SQLite's busy code. The message is checked as well, because several
    /// refusals reach this point as a sentence that quoted SQLite (`workspace database: sqlite: database is locked`) rather
    /// than as the typed error — and SQLite's own wording for the code is the one stable thing those sentences share.
    pub(crate) fn of(err: &(dyn Error + 'static)) -> Self {
        if np_core::db::is_database_busy(err) {
            return Self::DatabaseBusy;
        }
        Self::from_message(&err.to_string())
    }

    /// Classify a failure that only survived as its sentence, such as a JSON-RPC fault's `message`.
    pub(crate) fn from_message(message: &str) -> Self {
        if message.contains("database is locked") {
            Self::DatabaseBusy
        } else {
            Self::Other
        }
    }

    pub(crate) fn rule(self) -> &'static str {
        match self {
            Self::DatabaseBusy => np_core::diagnostics::rule::DATABASE_BUSY,
            Self::Other => np_core::diagnostics::rule::FAILED,
        }
    }
}

/// Write the outcome record for a CLI command that is about to exit non-zero: `error` level, on the `np_cli` target, with
/// the command in `what` and the sentence the person sees on stderr as the message — unless `explained`, meaning the
/// failing code already wrote a rule record of its own ("one refusal, one record"). `database-busy` is written either way,
/// because no other record ever names that failure.
pub(crate) fn log_command_failure(command: &str, err: &(dyn Error + 'static), explained: bool) {
    let failure = Failure::of(err);
    if explained && failure == Failure::Other {
        return;
    }
    tracing::error!(
        target: np_core::diagnostics::target::CLI,
        rule = failure.rule(),
        what = command,
        "`{command}` failed: {err}"
    );
}

/// Take this workspace's write lock, or report that somebody else holds it and stop.
///
/// Every command that changes state calls this before it touches anything, because the defect it closes is not two writers arguing inside
/// SQLite — that argument ends in `database is locked` in 5 ms, which is ugly but survivable. It is that `scan` renders `INDEX.md` *inside*
/// its write transaction: a loser can stop between "the database committed" and "the files were written", leaving an index that describes
/// notes the vault does not contain, and no rollback reaches a file. Exclusion has to happen before the first statement, which is what this
/// returns instead of a boolean: a guard held for the whole command, released by being dropped.
///
/// The refusal is a **state**, reported the way `offline` is: exit 1, one sentence on stderr naming what to do, and — under `--json`, which is
/// how an app asks — one machine-readable object on stdout. Before this, a losing `np scan --json` printed *nothing at all* and an app had to
/// infer contention from an empty document plus exit 1. Readers (`search`, `models list`) never call this function, so
/// a long `embed` cannot lock anybody out of reading: that asymmetry is the whole design, and `crates/np-cli/tests/cli.rs` pins both halves.
pub(crate) fn writer_lock(
    cfg: &Config,
    workspace: &Path,
    command: &str,
) -> Result<np_core::lock::WorkspaceLock, LockFailure> {
    // **The workspace-directory check comes first, and it belongs here rather than in each caller**: taking the
    // lock creates a file, and a typo'd workspace path
    // must not leave one behind. `Config::workspace_db` happily derives a name for a directory that does not exist, so a lock taken before
    // this check would create a data directory and a `<hash>.lock.db` for a vault nobody has — the exact failure
    // that check exists to prevent, re-opened through a new door. Every mutating command requires an existing
    // workspace directory anyway, so nothing loses coverage.
    models::require_workspace_dir(workspace).map_err(|source| LockFailure::Other(source.into()))?;
    let db_path = cfg.workspace_db(workspace);
    // The lock is named after the workspace database rather than recomputing an id, so there is no second derivation to drift from the first:
    // same directory, same stem, `.lock.db`. Copy a workspace's data somewhere else and its lock comes with it; two workspaces never contend.
    let dir = db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stem = db_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            LockFailure::Other(
                format!("cannot name the workspace lock for {}", db_path.display()).into(),
            )
        })?;
    match np_core::lock::WorkspaceLock::acquire(
        dir,
        stem,
        np_core::lock::WorkspaceLock::DEFAULT_WAIT,
    ) {
        Ok(held) => Ok(held),
        Err(np_core::lock::LockError::Held { waited }) => {
            // Contention as a diagnostic. `what` is the **workspace**, not the lock file:
            // `<hash>.lock.db` names a workspace to nobody reading a log, and the path is the identity every other
            // np diagnostic uses — a workspace path may be named where an origin's URL may not. The two numbers say
            // how long this process waited and how long it was willing to — the difference between a busy vault and
            // a broken one.
            let waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX);
            tracing::warn!(
                target: np_core::diagnostics::target::LOCK,
                rule = np_core::diagnostics::rule::LOCKED,
                what = %workspace.display(),
                expected = u64::try_from(np_core::lock::WorkspaceLock::DEFAULT_WAIT.as_millis()).unwrap_or(u64::MAX),
                actual = waited_ms,
                "another writer holds this workspace: `{command}` did not start"
            );
            Err(LockFailure::Locked {
                command: command.to_string(),
                waited_ms,
            })
        }
        Err(other) => Err(LockFailure::Other(other.into())),
    }
}

impl LockFailure {
    /// The machine-readable form of a refusal, built once for every channel.
    ///
    /// `locked` sits beside `offline` and `fetched` as the third thing that can stop a mutating command without being anybody's bug. It is a
    /// function rather than a literal at a print site because `np serve` has to put *this same object* in a JSON-RPC result: two places building
    /// it is two chances for a screen and a terminal to disagree about what contention means.
    fn document(&self) -> Option<Value> {
        match self {
            Self::Locked { command, waited_ms } => Some(json!({
                "command": command,
                "locked": true,
                "waited_ms": waited_ms,
            })),
            Self::Other(_) => None,
        }
    }

    /// The CLI's shape of a refusal: under `--json` the document goes to stdout first, then the sentence travels the error channel and decides the
    /// exit code — two channels, one fact. Other channels render [`Self::document`] their own way, which is why this takes the channel as an
    /// argument instead of the lock helper taking a boolean.
    pub(crate) fn report_cli(self, json: bool) -> Box<dyn Error> {
        // Printed before `self` is consumed below: a JSON caller that gets only a sentence on stderr is exactly
        // the defect this type exists to prevent (before it, a losing `np scan --json` printed nothing at all).
        if json {
            if let Some(doc) = self.document() {
                emit_json(&doc);
            }
        }
        match self {
            Self::Locked { waited_ms, .. } => format!(
                "locked: another process is writing this workspace ({waited_ms} ms) — try again once it finishes; \
                 `np search` and `np models list` answer while a writer runs"
            )
            .into(),
            Self::Other(source) => source,
        }
    }
}

/// Whether the server may work, as three outcomes rather than two.
///
/// **Contention is a state, and a state must survive every channel.** A JSON-RPC *fault* would fire an app's generic "the backend refused"
/// handler on something it is meant to retry, so `np serve` turns contention back into the very document `np scan --json` prints under contention
/// ([`LockFailure::document`]) and lets the caller branch on `locked`.
pub(crate) enum LockGrant {
    /// Ours to work under; the guard drops once the request's document is built.
    Held(np_core::lock::WorkspaceLock),
    /// Somebody else is writing, and this is the document that says so.
    Contended(Value),
}

pub(crate) fn server_writer_lock(
    cfg: &Config,
    workspace: &Path,
    command: &str,
) -> Result<LockGrant, Box<dyn Error>> {
    match writer_lock(cfg, workspace, command) {
        Ok(guard) => Ok(LockGrant::Held(guard)),
        Err(LockFailure::Locked { waited_ms, command }) => Ok(LockGrant::Contended(
            LockFailure::Locked { command, waited_ms }
                .document()
                .expect("a locked refusal always has a document"),
        )),
        Err(LockFailure::Other(source)) => Err(source),
    }
}

/// Why a mutating command could not take the workspace, as data rather than as a printed sentence.
///
/// The distinction that matters is between *contention* — a state, retry it, and readers keep answering — and *everything else*, which is a
/// refusal that needs somebody to change something. Collapsing both into `Box<dyn Error>` was fine while the CLI was the only channel: there was
/// exactly one way to render either. `np serve` needs the first one's document, so the two kinds are separate arms now.
pub(crate) enum LockFailure {
    /// Another process holds the workspace. The two fields are what [`Self::document`] carries, so a caller never parses a sentence to learn this.
    Locked { command: String, waited_ms: u64 },
    /// Anything else that stopped us before work: no workspace directory, an unreadable data dir, a name we cannot derive from the database path.
    Other(Box<dyn Error>),
}

/// A number for the text channel: four decimals when that says something, scientific notation when it does not.
///
/// The fixed `{:.4}` this function replaced was right about cosine, which lives in `[-1, 1]`, and wrong about everything
/// else — SQLite's `bm25()` on a two-note vault answers `-0.000001`-sized values, so a keyword hit led with `0.0000` and
/// read as "no score computed" rather than "a real, tiny, higher-is-better number". The threshold is not tuned: it is the
/// point below which four decimals stop carrying a digit. Full precision has always travelled in `--json`.
pub(crate) fn number(value: f64) -> String {
    if value.abs() < 5e-4 {
        format!("{value:.2e}")
    } else {
        format!("{value:.4}")
    }
}

/// The configuration every command needs, reported as the missing value it is: [`Config::load`] refuses rather
/// than deriving `workspaces/<hash>.sqlite` next to the current directory, and a model command that guessed a
/// data dir would install artifacts nobody can find.
pub(crate) fn load_config() -> Result<Config, Box<dyn Error>> {
    let cfg = match Config::load(None) {
        Ok(cfg) => cfg,
        Err(err) => {
            // **The refusal is itself a diagnostic**: a broken config must not be the one failure that reports
            // nothing, and this is the most common one. `error` level, on the config target, with the rule as a field rather
            // than as prose — which is what lets a frontend log *why* without matching a sentence. It reaches stderr only if
            // someone asked for diagnostics, so the pinned single-line refusal below is still exactly one line.
            tracing::error!(
                target: np_core::diagnostics::target::CONFIG,
                rule = np_core::diagnostics::rule::CONFIG_REFUSED,
                "{err}"
            );
            return Err(format!("configuration error: {err}").into());
        }
    };
    // **Phase two**: `[log] level` and `[log] file` now that there is a file to read them from. Idempotent, because several
    // commands load configuration more than once per run.
    diag::apply_config(&cfg);
    // **The one-release fallback note, on stderr.** Stdout stays exactly one document when `--json` is asked for — the fact this
    // line carries is about *where* the run is reading from, not about the run's result, and a reader who needs it is a
    // human standing at the machine. Nothing here is actionable: that is the point of the fallback, and saying "moved"
    // would imply a move happened, which this release deliberately does not do.
    if let Some(legacy) = &cfg.legacy_data_dir {
        let _ = writeln!(io::stderr(), "np: {}", legacy_data_dir_note(legacy));
    }
    Ok(cfg)
}

/// The sentence [`load_config`] prints when its fallback found an index at the pre-nesting Windows location.
///
/// A function rather than an inline literal for one reason: the fallback itself cannot happen on this platform or on CI
/// (Unix has no pre-nesting shape to fall back from), so the *words* are the only part of this path that Linux can check at
/// all. What they must do is name the directory, say no action is needed, and stay out of the report — asserted in this
/// module's tests, while the resolution that decides whether they print is asserted in
/// `crates/np-core/tests/config.rs` through [`np_core::config::DataDirPlatform`].
fn legacy_data_dir_note(legacy: &Path) -> String {
    format!(
        "reading your index from {} — where it was written before this release nested its data folder. \
         nothing to do: it stays there until a later release moves it",
        legacy.display()
    )
}

/// This workspace's database, opened in the order that stops a bad path from creating one.
///
/// [`models::require_workspace_dir`] comes **first** because `Config::workspace_db` is infallible by contract
/// and `Db::open` creates directories: check after opening would have already minted an empty database for a
/// typo'd name, which the check can no longer un-mint.
pub(crate) fn workspace_db(config: &Config, workspace: &Path) -> Result<Db, Box<dyn Error>> {
    models::require_workspace_dir(workspace)?;
    Ok(Db::open(&config.workspace_db(workspace))?)
}

/// One line on stdout, with the result dropped for `print_summary`'s reason: a descriptor that refuses writes
/// (a closed pipe, `/dev/full`) must not turn finished work into the panic `println!` would raise.
pub(crate) fn emit(line: &str) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{line}");
}

/// One JSON document on stdout, pretty-printed like `np scan --json`, with the same dropped-write rule as
/// [`emit`].
pub(crate) fn emit_json(value: &Value) {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    let mut out = io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Whole seconds since the Unix epoch for `models.installed_at`, mirroring `np_core::scan`'s private
/// `now_secs`: a clock that reads before the epoch contributes `0`, which is that function's answer to the
/// same failure and keeps a working install from failing because the machine's clock is broken.
pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(all(test, feature = "fetch"))]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_note_names_the_index_and_asks_for_nothing() {
        let note = legacy_data_dir_note(std::path::Path::new(r"C:\Users\ana\AppData\Local"));
        assert!(
            note.contains(r"C:\Users\ana\AppData\Local"),
            "it has to name the directory it is reading: {note}"
        );
        assert!(
            note.contains("nothing to do"),
            "a person reading this must not go looking for a fix: {note}"
        );
        assert!(
            !note.contains("moved"),
            "this release reads where the index is; it does not claim to have moved anything: {note}"
        );
    }
}
