//! Config file, data dir and the known-workspace registry.
//!
//! Nothing here reads or writes the real user config/data dir: every case that
//! could create files passes an explicit config path or a `data_dir` inside a
//! `tempfile::TempDir`. The four cases that exercise the OS fallbacks install their own
//! environment instead ([`TestEnv`]), because cargo gives a test process whatever `$HOME` the
//! developer's shell happens to hold — and a stripped CI shell holds nothing, which is a different
//! result from the one the product produces.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

// The same harness `crates/np-core/tests/db.rs` uses against a workspace database; see that file's
// header note on why the hold times and the handoff are shared.
mod common;

use common::{contended, hold_write_lock, Contention, HOLD_MS, HOLD_PAST_TIMEOUT_MS};
use np_core::config::{
    config_file_path, data_dir_for, declared_data_dir, legacy_data_dir_override, parse_diag_spec,
    peek_log_config, resolve_data_dir, resolve_diag, CatalogConfig, Config, ConfigError,
    DataDirPlatform, DiagSource, EmbeddingProvider, EmbeddingsConfig, Level, LogPeek, MarkerConfig,
    ModelsConfig, SearchConfig, ThreadCount, WorkspaceRegistry, DEFAULT_EMBEDDING_TOKENS,
    DEFAULT_LOG_MAX_BYTES, DIAG_LEVEL_NAMES, MAX_CONTINUATION_INDENT, MAX_EMBEDDING_TOKENS,
    REGISTRY_FILE, WINDOWS_DATA_SUBDIR, WORKSPACES_DIR,
};
use np_core::search::SearchMode;
use rusqlite::Connection;
use tempfile::TempDir;

/// `s` as a TOML basic string. Rust's `Debug` escapes for `\` and `"` match
/// TOML's, which matters for Windows temp-dir paths.
fn toml_str(s: &str) -> String {
    format!("{:?}", s)
}

/// Write `body` as a config file inside `dir` and return its path.
fn write_config(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("config.toml");
    fs::write(&path, body).expect("test setup: write config file");
    path
}

/// A config whose `data_dir` is `dir`, so nothing is created outside the tempdir.
fn config_with_data_dir(dir: &Path) -> Config {
    let body = format!("data_dir = {}\n", toml_str(&dir.display().to_string()));
    let path = write_config(dir, &body);
    Config::load(Some(&path)).expect("test setup: load config")
}

/// Every variable a case can be affected by: the five `config_file_path` and `default_data_dir` consult, on either
/// platform, plus the two `[models]` overrides of §12 rule 7.
const ENV_KEYS: [&str; 7] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "APPDATA",
    "LOCALAPPDATA",
    "NP_MODELS_BASE_URL",
    "NP_MODELS_SHA256",
];

/// The last two of [`ENV_KEYS`]: overrides rather than directories, so the isolated state is *unset* — there is no
/// temp-dir value they should point at. [`TestEnv::install`] clears them for exactly that reason; a case that wants
/// one asks through [`TestEnv::set_var`].
const OVERRIDE_ENV_KEYS: [&str; 2] = [ENV_KEYS[5], ENV_KEYS[6]];

/// Take [`ENV_LOCK`] without installing a temp environment.
///
/// Since `[models]` reads `NP_MODELS_BASE_URL` / `NP_MODELS_SHA256` on **every** `Config::load`, a case that loads a
/// config file is now affected by another thread's installed override even when it never touches the environment
/// itself — which is what a valueless `data_dir`-in-a-temp-dir fixture used to be immune to. Holding the lock is the
/// whole isolation here: nothing is set, nothing is restored, and the case still runs against its own explicit path.
fn hold_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Serialises the cases that rewrite the process environment.
///
/// Cargo runs the cases in this file on parallel threads while environment variables belong to the
/// whole process, so an installed set of directories has to stay installed for exactly one case.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// One case's worth of process environment, pointing at throwaway config and data directories.
///
/// A config file that does not name a `data_dir` means the OS one, and `config_file_path` reads the
/// same variables, so such a case cannot be hermetic without owning them: on a machine where `$HOME`
/// and the XDG pair are unset, `Config::load` refuses an empty data dir
/// (`ConfigError::MissingField("data_dir")`, which is the deliberate product behaviour), while a
/// developer's own `~/.config/note-processor/config.toml` would silently change what the assertions
/// see. Both failure modes are the environment leaking into the test, so both are removed here
/// rather than skipped around.
///
/// The same isolation the CLI integration tests get from `Command::env`
/// (`crates/np-cli/tests/cli.rs:38`) comes from `env::set_var` here, guarded by [`ENV_LOCK`]; the
/// previous values go back on drop. Most cases need none of this — each passes an explicit config path or a
/// `data_dir` inside its own `TempDir` — but every `[models]` case holds one, because those two keys are read out of
/// the environment by `Config::load` whether or not the fixture asked ([`OVERRIDE_ENV_KEYS`] clears them).
struct TestEnv {
    /// Held until the values are restored, so no second case installs over ours. A poisoned lock can
    /// only come from a case that already failed, so its contents are taken regardless.
    _guard: MutexGuard<'static, ()>,
    /// Kept alive so both directories outlive every read of them.
    _tmp: TempDir,
    /// What each variable held before, restored on drop.
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl TestEnv {
    /// Install temp config and data dirs under a fresh `TempDir` for the calling case.
    fn install() -> Self {
        let guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tmp = TempDir::new().expect("test setup: tempdir");
        let previous = ENV_KEYS
            .iter()
            .map(|key| (*key, env::var_os(key)))
            .collect();
        // All five spellings the two lookups accept, on every platform: the XDG pair for unix,
        // `APPDATA`/`LOCALAPPDATA` for Windows, and `HOME` as the fallback underneath them.
        env::set_var("HOME", tmp.path());
        env::set_var("XDG_CONFIG_HOME", tmp.path().join("config"));
        env::set_var("XDG_DATA_HOME", Self::data_dir_of(&tmp));
        env::set_var("APPDATA", tmp.path().join("config"));
        env::set_var("LOCALAPPDATA", Self::data_dir_of(&tmp));
        // A developer's own mirror or digest override is the same leak as a stray `$HOME`: it would decide what an
        // unrelated case loads, and — since `[models]` refuses bad values — could fail a case for a key it never set.
        for key in OVERRIDE_ENV_KEYS {
            env::remove_var(key);
        }
        Self {
            _guard: guard,
            _tmp: tmp,
            previous,
        }
    }

    /// Set or clear one of the variables this fixture snapshots, for the rest of the case.
    ///
    /// Restoring it is [`Drop`]'s job, which is why the key has to be one of [`ENV_KEYS`] — a variable outside that
    /// list would be set with nothing to put back, and would leak into every later case in this binary.
    fn set_var(&mut self, key: &'static str, value: Option<&str>) {
        assert!(
            ENV_KEYS.contains(&key),
            "test fixture: {key} is not snapshotted, so setting it here could not be undone"
        );
        match value {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }

    /// The data dir the installed environment names, on either platform.
    fn data_dir(&self) -> PathBuf {
        Self::data_dir_of(&self._tmp)
    }

    fn data_dir_of(tmp: &TempDir) -> PathBuf {
        // F37 nested the Windows data dir under the program folder, while this harness installs `%LOCALAPPDATA%` itself.
        // So on that shape the effective directory is one level deeper than the variable. Spelled out per platform rather
        // than taken from `default_data_dir()`: this value is the expectation the product is measured against, and a test
        // that computes its own expectation from the code under test cannot fail.
        #[cfg(windows)]
        return tmp.path().join("data").join(WINDOWS_DATA_SUBDIR);
        #[cfg(not(windows))]
        return tmp.path().join("data");
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // Runs while `_guard` is still held: field drops come after `Drop::drop`.
        for (key, value) in &self.previous {
            match value {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }
        }
    }
}

#[test]
fn defaults_match_spec() {
    let markers = MarkerConfig::default();
    for marker in ["✅", "- [x]", "erledigt"] {
        assert!(
            markers.done.iter().any(|done| done == marker),
            "done markers must contain {marker:?}, got {markers:?}"
        );
    }
    assert!(
        markers.paused.iter().any(|paused| paused == "pausiert"),
        "paused markers must contain \"pausiert\", got {markers:?}"
    );

    let catalog = CatalogConfig::default();
    assert_eq!(catalog.max_entries_per_file, 600);
    assert_eq!(catalog.continuation_indent, 2);
}

#[test]
fn default_marker_lists_are_the_global_constraints_list() {
    // Stored exactly as the spec lists them: not lowercased, nothing added or dropped.
    let markers = MarkerConfig::default();
    assert_eq!(
        markers.done,
        [
            "✅",
            "- [x]",
            "done",
            "finished",
            "erledigt",
            "fertig",
            "abgeschlossen"
        ]
    );
    assert_eq!(
        markers.paused,
        [
            "paused",
            "pausiert",
            "on hold",
            "gestoppt",
            "verschoben",
            "vertagt"
        ]
    );
}

#[test]
fn toml_overrides_and_keeps_defaults() {
    // The file names no `data_dir`, so this case reads the OS one and has to own it.
    let _env = TestEnv::install();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config(tmp.path(), "catalog.max_entries_per_file = 50\n");

    let cfg = Config::load(Some(&path)).expect("config with one key should load");

    assert_eq!(cfg.catalog.max_entries_per_file, 50);
    assert_eq!(cfg.catalog.continuation_indent, 2);
    assert_eq!(cfg.markers, MarkerConfig::default());
}

#[test]
fn empty_config_file_keeps_defaults() {
    // Same as above: an empty file names no `data_dir`, so the environment supplies it.
    let _env = TestEnv::install();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config(tmp.path(), "");

    let cfg = Config::load(Some(&path)).expect("an empty file is a valid config");

    assert_eq!(cfg.catalog, CatalogConfig::default());
    assert_eq!(cfg.markers, MarkerConfig::default());
}

#[test]
fn unparsable_config_file_is_a_parse_error() {
    let _env = hold_env();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config(tmp.path(), "catalog.max_entries_per_file = \"many\"\n");

    let err = Config::load(Some(&path)).expect_err("bad TOML must not load");

    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected Parse, got {err:?}"
    );
    // The variant carries the path precisely because this message is what a person acts on: bad
    // content in one of two possible files must say which.
    assert!(
        err.to_string().contains("config.toml"),
        "a parse refusal must name the file it refused: {err}"
    );
}

#[test]
fn explicit_missing_config_file_is_an_io_error() {
    let _env = hold_env();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let missing = tmp.path().join("typo.toml");

    let err = Config::load(Some(&missing)).expect_err("a named file must exist");

    assert!(
        matches!(err, ConfigError::Io(_)),
        "expected Io, got {err:?}"
    );
    assert!(
        err.to_string().contains("typo.toml"),
        "the error must name the file: {err}"
    );
}

#[test]
fn no_config_file_means_defaults() {
    // The environment belongs to this case, so "no config file there" is a premise it can assert
    // rather than a condition it prints a skip line for. A printed skip would prove nothing: libtest
    // captures stderr for a *passing* test, so CI could never tell this branch from a real run.
    let env = TestEnv::install();
    let path = config_file_path().expect("a supported runner names a config directory");
    assert!(
        !path.exists(),
        "the isolated config dir must hold no config file, found {}",
        path.display()
    );

    let cfg = Config::load(None).expect("a missing default config file is not an error");

    assert_eq!(cfg.catalog, CatalogConfig::default());
    assert_eq!(cfg.markers, MarkerConfig::default());
    assert_eq!(
        cfg.data_dir,
        env.data_dir(),
        "the data dir comes from the installed environment, never a real one"
    );
}

#[test]
fn config_file_path_points_at_note_processor_config() {
    // A supported runner always names a config directory (`HOME`/XDG on unix, `APPDATA` on Windows),
    // so a `None` here is the environment going missing — a broken fixture, not a platform to skip.
    let _env = TestEnv::install();
    let path = config_file_path().expect("a supported runner names a config directory");
    assert!(
        path.ends_with(Path::new("note-processor").join("config.toml")),
        "expected .../note-processor/config.toml, got {}",
        path.display()
    );
}

#[test]
fn workspace_db_is_stable_and_unique() {
    let _env = hold_env();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let cfg = config_with_data_dir(tmp.path());

    let first = cfg.workspace_db(Path::new("/a/notes"));
    let again = cfg.workspace_db(Path::new("/a/notes"));
    assert_eq!(first, again, "same path must map to the same file");
    assert_ne!(
        first,
        cfg.workspace_db(Path::new("/a/other")),
        "different paths must map to different files"
    );

    let name = first
        .file_name()
        .expect("test: db has a file name")
        .to_string_lossy();
    assert!(
        name.ends_with(".sqlite"),
        "file name must end .sqlite, got {name}"
    );
    assert_eq!(
        first.parent(),
        Some(tmp.path().join("workspaces").as_path())
    );
}

#[test]
fn workspace_db_name_is_lowercase_hex() {
    let _env = hold_env();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let cfg = config_with_data_dir(tmp.path());

    let stem = cfg
        .workspace_db(Path::new("/a/notes"))
        .file_stem()
        .expect("test: db has a stem")
        .to_string_lossy()
        .into_owned();

    assert_eq!(stem.len(), 16, "expected 16 hex chars, got {stem:?}");
    assert!(
        stem.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "expected lowercase hex, got {stem:?}"
    );
}

#[test]
fn relative_workspace_path_is_resolved_against_the_cwd() {
    let _env = hold_env();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let cfg = config_with_data_dir(tmp.path());
    let cwd = std::env::current_dir().expect("test: current dir");

    assert_eq!(
        cfg.workspace_db(Path::new("notes")),
        cfg.workspace_db(&cwd.join("notes"))
    );
}

#[test]
fn registry_records_and_bumps_last_seen() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let mut registry = WorkspaceRegistry::open(tmp.path()).expect("registry should open");

    registry
        .record(Path::new("/a/notes"), 100)
        .expect("record /a/notes");
    registry
        .record(Path::new("/b/more"), 200)
        .expect("record /b/more");
    registry
        .record(Path::new("/a/notes"), 300)
        .expect("re-record /a/notes");

    let recent = registry.recent().expect("recent");

    // A drive-less absolute path (`/a/notes`) is still absolute on Windows, where it resolves
    // against the current drive — so the stored key is `D:/a/notes`-shaped there and `/a/notes`
    // here. Compare by suffix; the ordering, count and timestamps stay exactly asserted.
    assert_eq!(recent.len(), 2, "re-recording must not add a second row");
    assert!(
        recent[0].0.ends_with("a/notes"),
        "most recently seen workspace first, got {:?}",
        recent[0].0
    );
    assert_eq!(recent[0].1, 300, "re-recording must bump last_seen");
    assert!(
        recent[1].0.ends_with("b/more"),
        "older workspace second, got {:?}",
        recent[1].0
    );
    assert_eq!(recent[1].1, 200);

    // `first_seen` belongs to the first sighting, not the latest one.
    let conn = Connection::open(tmp.path().join("workspaces.sqlite")).expect("test: open registry");
    let first_seen: i64 = conn
        .query_row(
            // Suffix match for the same reason as above: the stored key carries a drive letter
            // on Windows.
            "SELECT first_seen FROM workspaces WHERE workspace LIKE '%/a/notes'",
            [],
            |row| row.get(0),
        )
        .expect("test: read first_seen");
    assert_eq!(first_seen, 100);
}

/// Take the write lock on the registry file from a second handle — the other `np` run that WAL and the
/// busy timeout exist for. It opens after the registry exists, so no schema write races with assertions.
fn hold_registry_lock(data_dir: &Path) -> Connection {
    let holder = Connection::open(data_dir.join("workspaces.sqlite"))
        .expect("test: open the registry file again");
    hold_write_lock(
        |sql| holder.execute_batch(sql),
        "INSERT INTO workspaces (workspace, first_seen, last_seen) VALUES ('/held/notes', 1, 1)",
    );
    holder
}

/// The payload this binary needs back from the writer thread: the registry itself, because the
/// assertions read its rows afterwards. `db.rs` returns a bare statement result, so only the shape of
/// the outcome stays local; the handoff and the timings come from [`common::contended`].
type Recorded = (Result<(), ConfigError>, WorkspaceRegistry);

/// Call `record` on `registry` while `holder` holds the lock, then commit the holder.
///
/// The waiting `record` cannot run on this thread — only this thread can release the lock it is blocked
/// on — so the registry travels to the writer and back inside [`Recorded`].
fn record_while_locked(
    holder: Connection,
    registry: WorkspaceRegistry,
    hold: Duration,
) -> Contention<Recorded> {
    contended(
        hold,
        move || {
            let mut registry = registry;
            let recorded = registry.record(Path::new("/waiting/notes"), 200);
            (recorded, registry)
        },
        || {
            holder
                .execute_batch("COMMIT")
                .expect("test: release the write lock");
        },
    )
}

/// The driver failure behind a [`ConfigError`], if there is one: the registry wraps SQLite errors in
/// `io::Error::other` (see `From<rusqlite::Error> for ConfigError`), so a typed match has to unwrap
/// that rather than compare the message text.
fn sqlite_error(err: &ConfigError) -> Option<&rusqlite::Error> {
    match err {
        ConfigError::Io(io_err) => io_err
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<rusqlite::Error>()),
        _ => None,
    }
}

#[test]
fn a_registry_write_waits_for_another_connection_instead_of_failing_busy() {
    // The registry sits in the shared data dir and is written by every command, so it is the file two
    // overlapping `np` runs are most likely to reach at the same moment. Same shape as the
    // workspace-database case in `crates/np-core/tests/db.rs`: one connection holds a write transaction,
    // another writes while it does, and the second must wait instead of reporting `SQLITE_BUSY`.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let registry = WorkspaceRegistry::open(tmp.path()).expect("registry should open");
    let holder = hold_registry_lock(tmp.path());

    let contention = record_while_locked(holder, registry, Duration::from_millis(HOLD_MS));
    let (recorded, registry) = contention.outcome;
    recorded.expect("the registry write has to wait for the lock, not fail with SQLITE_BUSY");
    // A diagnostic, not a discriminator, for the same reason as the workspace-database case beside it in
    // `tests/db.rs`: `common::contended` sleeps the whole `HOLD_MS` before releasing and `waited` starts
    // before that sleep, so a write that got the lock back is over half the hold by construction. The
    // registry's real discriminator is `the_registry_busy_timeout_is_the_same_short_one`; this figure is
    // printed because one near zero would mean the harness stopped measuring.
    assert!(
        contention.waited >= Duration::from_millis(HOLD_MS / 2),
        "record() returned after {:?}, so it never waited for the held lock",
        contention.waited
    );

    let recent = registry.recent().expect("recent");
    assert_eq!(recent.len(), 2, "both writes land: {recent:?}");
    assert!(
        recent[0].0.ends_with("waiting/notes"),
        "the write that waited is the most recent one: {recent:?}"
    );
}

#[test]
fn the_registry_busy_timeout_is_the_same_short_one() {
    // Why this case sits beside the one above: rusqlite installs its own 5000 ms timeout on every
    // connection it opens (`rusqlite-0.40.2/src/inner_connection.rs:118`), so patience alone would not
    // notice the registry pragma going missing. The hold sits between the two values —
    // `HOLD_PAST_TIMEOUT_MS` is 3000 ms, past our 1800 ms and well inside those five seconds — so the
    // case stays a discriminator rather than a tautology: `record` has to come back refusing while the
    // holder still has the transaction, which a driver-default connection would have waited out. That
    // gap is also what proves the value set in `WorkspaceRegistry::open` is the one in force. The
    // workspace-database equivalent is
    // `the_busy_timeout_in_force_is_this_crates_and_not_the_drivers_default` in
    // `crates/np-core/tests/db.rs`; both connections read the same constant.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let registry = WorkspaceRegistry::open(tmp.path()).expect("registry should open");
    let holder = hold_registry_lock(tmp.path());

    let contention = record_while_locked(
        holder,
        registry,
        Duration::from_millis(HOLD_PAST_TIMEOUT_MS),
    );

    let (recorded, _registry) = contention.outcome;
    let err =
        recorded.expect_err("a lock held past the configured timeout has to be refused eventually");
    // The typed code, not the message: `database is locked` is driver wording and could change under
    // us, and only the code says the refusal was "the lock is held" rather than some other failure.
    assert!(
        matches!(
            sqlite_error(&err),
            Some(rusqlite::Error::SqliteFailure(fault, _))
                if fault.code == rusqlite::ErrorCode::DatabaseBusy
        ),
        "expected SQLITE_BUSY, got {err:?}"
    );
    // Half our timeout below, the driver's default above: the give-up has to land near 1800 ms.
    assert!(
        contention.inside_call >= Duration::from_millis(900),
        "record() gave up after {:?}, which is not the ~1800 ms this crate asks for",
        contention.inside_call
    );
    assert!(
        contention.inside_call < Duration::from_millis(4500),
        "record() waited {:?}, i.e. something nearer rusqlite's own 5 s default than our value",
        contention.inside_call
    );
}

#[test]
fn registry_reopens_without_losing_rows() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let data_dir = tmp.path().join("nested").join("data");

    {
        let mut registry =
            WorkspaceRegistry::open(&data_dir).expect("open must create the data dir");
        registry
            .record(Path::new("/a/notes"), 100)
            .expect("record /a/notes");
    }
    assert!(
        data_dir.join("workspaces.sqlite").is_file(),
        "registry file is workspaces.sqlite"
    );

    let registry = WorkspaceRegistry::open(&data_dir).expect("reopening must not fail");
    let recent = registry.recent().expect("recent");

    // A drive-less absolute path (`/a/notes`) is still absolute on Windows, where it resolves
    // against the current drive, so the persisted key is `D:/a/notes`-shaped there. Match by
    // suffix rather than by the Unix spelling.
    assert_eq!(recent.len(), 1, "the row must survive reopening");
    assert!(
        recent[0].0.ends_with("a/notes"),
        "persisted workspace key, got {:?}",
        recent[0].0
    );
    assert_eq!(recent[0].1, 100);

    let conn = Connection::open(data_dir.join("workspaces.sqlite")).expect("test: open registry");
    let mode: String = conn
        .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
        .expect("test: read journal_mode");
    assert_eq!(mode.to_lowercase(), "wal");
}

/// A workspace path whose raw bytes are not valid UTF-8, so no `String` can hold it.
#[cfg(unix)]
fn non_utf8_workspace() -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    // `0xFF` is not a UTF-8 sequence: this is a real OS path whose lossy text carries `U+FFFD`.
    PathBuf::from(OsStr::from_bytes(b"/ws/we\xffrd"))
}

#[cfg(unix)]
#[test]
fn workspace_db_separates_a_non_utf8_path_from_its_lossy_spelling() {
    let _env = hold_env();
    // Lossy decoding maps `0xFF` and a literal replacement character to the same text, so hashing
    // the decoded string would hand two different workspaces one database — and each scan of one
    // would mark the other's notes stale.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let cfg = config_with_data_dir(tmp.path());
    let raw = non_utf8_workspace();
    let lossy_twin = PathBuf::from("/ws/we\u{fffd}rd");

    assert_eq!(
        raw.as_os_str().to_string_lossy(),
        lossy_twin.as_os_str().to_string_lossy(),
        "test premise: the two paths decode to one text"
    );
    assert_ne!(
        cfg.workspace_db(&raw),
        cfg.workspace_db(&lossy_twin),
        "the database key is the raw OS path, not its lossy text"
    );
}

/// Write `body` as a config file in `dir` that also names its own `data_dir`, and return the path.
///
/// The `data_dir` half is not decoration: `Config::load` refuses an unresolvable data dir **before** it looks at
/// anything else, so a fixture holding only `[catalog]` keys fails with `missing config value: data_dir` and every
/// message assertion about the catalog key goes red for an unrelated reason.
fn write_config_with_data_dir(dir: &Path, body: &str) -> PathBuf {
    let full = format!(
        "data_dir = {}\n{}",
        toml_str(&dir.display().to_string()),
        body
    );
    write_config(dir, &full)
}

#[test]
fn an_oversized_continuation_indent_is_refused_naming_key_value_and_ceiling() {
    let _env = hold_env();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[catalog]\ncontinuation_indent = 17\n");

    let err = match Config::load(Some(&path)) {
        Ok(cfg) => panic!(
            "an indent of 17 must be refused, got continuation_indent = {}",
            cfg.catalog.continuation_indent
        ),
        Err(err) => err,
    };
    let message = err.to_string();
    // The backlog's done-when is that a person can tell *which key* broke, so the text is the assertion — an
    // `is_err()` check would still pass if the refusal were reworded into "config io: InvalidData".
    assert!(
        message.contains("catalog.continuation_indent"),
        "the refusal must name the key: {message}"
    );
    assert!(message.contains("17"), "and the offending value: {message}");
    assert!(
        message.contains("max 16"),
        "and the ceiling to aim at, upper bound only — a negative literal is refused by serde before this check runs: \
         {message}"
    );
    // The refusal is data being rejected, not a file that could not be read: callers who care match the kind rather
    // than parsing prose.
    let ConfigError::Io(io_err) = &err else {
        panic!("an out-of-range value must travel as Io(InvalidData), got {err:?}");
    };
    assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn the_ceiling_itself_is_accepted_and_zero_means_no_indent() {
    let _env = hold_env();
    // The boundary, because a check that refuses 17 but also refuses 16 has off-by-one-drifted into breaking the
    // documented range; and 0, which is a legal layout choice (flush continuation lines) rather than an absurdity.
    for indent in [0usize, MAX_CONTINUATION_INDENT] {
        let tmp = TempDir::new().expect("test setup: tempdir");
        let path = write_config_with_data_dir(
            tmp.path(),
            &format!("[catalog]\ncontinuation_indent = {indent}\n"),
        );
        let cfg = Config::load(Some(&path))
            .unwrap_or_else(|err| panic!("indent {indent} is inside the documented range: {err}"));
        assert_eq!(cfg.catalog.continuation_indent, indent);
    }
}

#[test]
fn refusing_an_oversized_indent_leaves_the_default_alone() {
    let _env = hold_env();
    // The default path is what every golden byte was rendered with, so this item must not nudge it. Absent key ⇒ 2,
    // exactly as before the check existed.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[catalog]\nmax_entries_per_file = 400\n");
    let cfg = Config::load(Some(&path)).expect("a file naming no indent keeps the default");
    assert_eq!(cfg.catalog.continuation_indent, 2);
}

/// Write `body` as a config file and load it, expecting refusal; return the file and the error.
///
/// The path comes back because "names the file" has to be asserted against the error's **own** text: a
/// message built by concatenating the path would satisfy a `contains` check even if the refusal named no
/// file at all, which is the exact hole this helper must not open.
fn embeddings_refusal(body: &str) -> (PathBuf, ConfigError) {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), body);
    let err = match Config::load(Some(&path)) {
        Ok(cfg) => panic!(
            "the file must be refused, loaded provider = {:?}",
            cfg.embeddings.provider
        ),
        Err(err) => err,
    };
    (path, err)
}

#[test]
fn a_misspelled_embeddings_key_is_refused_naming_key_and_file() {
    // §7's scenario verbatim: `provide` instead of `provider`, with a perfectly valid value beside it.
    // Lenient deserialisation would load that as `provider = "none"` and the owner would believe their
    // corpus was embedding. The whole point of `deny_unknown_fields` on this one section is that the
    // file is refused instead — naming the key, so it can be fixed, and the file, because two config
    // files can be in play (F17's key+value+ceiling rule, same reasoning as
    // `explicit_missing_config_file_is_an_io_error`).
    let (path, err) = embeddings_refusal("[embeddings]\nprovide = \"openai-compatible\"\n");
    let message = err.to_string();

    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "an unknown key must travel as the typed parse refusal, got {err:?}"
    );
    // Quoted deliberately: toml names the bad key *and* lists the legal ones ("unknown field `provide`,
    // expected one of `provider`, `model`, `dim`, `max_tokens`"), so an assertion on the bare word would hold
    // by the suggestion list. "\`provide\`" is not a substring of "\`provider\`".
    assert!(
        message.contains("`provide`"),
        "the refusal must name the misspelled key as written: {message}"
    );
    assert!(
        message.contains(&path.display().to_string()),
        "and the file to go and fix, in its own words: {message}"
    );

    // Mutation this catches: dropping `deny_unknown_fields` from `EmbeddingsConfig`, which loads the
    // typo as `provider = None` — the test then fails on the `panic!` inside `embeddings_refusal`, with
    // the loaded provider printed.
}

#[test]
fn an_unrecognised_embeddings_key_is_refused_even_beside_a_valid_one() {
    // Half of "a typo must not mean none" that a single-key fixture cannot see: when *another* key in
    // the same section is correct, a struct that merely ignored unknowns would load `provider` fine and
    // the stray key would vanish. Refusal has to survive the company it keeps.
    let (_path, err) =
        embeddings_refusal("[embeddings]\nprovider = \"local-model\"\nmodell = \"gemma\"\n");
    let message = err.to_string();
    assert!(
        message.contains("`modell`"),
        "the stray key is named even though its neighbour is valid: {message}"
    );

    // Mutation this catches: replacing `deny_unknown_fields` with per-field defaults, i.e. the lenient
    // posture of `CatalogConfig`, which would load this file as provider = LocalModel and drop `modell`.
}

#[test]
fn omitting_the_embeddings_section_means_no_provider() {
    let _env = hold_env();
    // §2's shipping default, asserted on a file that says nothing about embeddings at all: no section,
    // no download, no contact. This is also the half of the story the refusal above cannot tell — a
    // reader who only saw the strict struct could not tell an omitted section from a refused one.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[catalog]\nmax_entries_per_file = 400\n");

    let cfg =
        Config::load(Some(&path)).expect("a file with no [embeddings] section is a valid config");

    assert_eq!(cfg.embeddings.provider, EmbeddingProvider::None);
    assert_eq!(cfg.embeddings.model, None);
    assert_eq!(cfg.embeddings.dim, None);
    assert_eq!(
        cfg.embeddings,
        EmbeddingsConfig::default(),
        "and that is the same state `Config::default()` describes for a fresh install"
    );

    // Mutation this catches: making `provider` mandatory (removing the container default), which turns
    // every existing config file into a `MissingField`-style parse error.
}

#[test]
fn every_provider_spelling_parses_and_a_partial_section_keeps_its_defaults() {
    let _env = hold_env();
    // The three kebab-case spellings §2 uses, pinned from a file rather than from the enum. R6a changed what
    // each spelling *earns*, not what it parses to: `none` and `local-model` load (`local-model` only beside
    // a model — §4 refuses a local provider with no identity), and `openai-compatible` is refused by **us**.
    // That still pins serde's renaming attributes, and this is how: had a rename drifted, the failure would be
    // `ConfigError::Parse` — serde unable to read the file — while §10's refusal is `Io(InvalidData)` on data
    // it read perfectly. So the loop asserts the variant, which is the half that survives the refusal.
    let section = |spelling: &str| {
        format!("[embeddings]\nprovider = \"{spelling}\"\nmodel = \"{LOCAL_ID}\"\n")
    };
    for spelling in ["none", "local-model", "openai-compatible"] {
        let tmp = TempDir::new().expect("test setup: tempdir");
        let path = write_config_with_data_dir(tmp.path(), &section(spelling));
        match Config::load(Some(&path)) {
            Ok(cfg) => assert_eq!(
                cfg.embeddings.model.as_deref(),
                Some(LOCAL_ID),
                "a key the file spelled out must arrive rather than be invented"
            ),
            Err(ConfigError::Parse { source, .. }) => panic!(
                "\"{spelling}\" is a legal spelling; serde was the one that refused it: {source}"
            ),
            Err(err) => assert!(
                matches!(err, ConfigError::Io(_)),
                "a refusal has to be ours, in our words, not the driver's: {err:?}"
            ),
        }
    }

    // Absent keys stay absent — the part of §2's shipping default a full section cannot show.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[embeddings]\nprovider = \"none\"\n");
    let cfg = Config::load(Some(&path))
        .unwrap_or_else(|err| panic!("\"none\" is the default provider: {err}"));
    assert_eq!(cfg.embeddings.provider, EmbeddingProvider::None);
    assert_eq!(cfg.embeddings.model, None, "absent model stays absent");
    assert_eq!(cfg.embeddings.dim, None, "absent dim stays absent");

    // The other two fields, and a section naming only one of them — which is what `EmbeddingsConfig`'s
    // own container default exists for.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(
        tmp.path(),
        "[embeddings]\nprovider = \"local-model\"\nmodel = \"embeddinggemma-2-text@1\"\ndim = 512\n",
    );
    let cfg = Config::load(Some(&path)).expect("a full [embeddings] section loads");
    assert_eq!(
        cfg.embeddings.model.as_deref(),
        Some("embeddinggemma-2-text@1")
    );
    assert_eq!(cfg.embeddings.dim, Some(512));

    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[embeddings]\nmodel = \"m@1\"\n");
    let cfg = Config::load(Some(&path)).expect("a partial [embeddings] section loads");
    assert_eq!(cfg.embeddings.provider, EmbeddingProvider::None);
    assert_eq!(cfg.embeddings.dim, None);

    // Mutations this catches, all three still fatal: dropping `#[serde(rename_all = "kebab-case")]`
    // ("local-model" then arrives as `ConfigError::Parse`, which the loop panics on); dropping the
    // hand-written `openai-compatible` rename, which leaves serde's own kebab-casing in charge and refuses §2's
    // spelling with "expected one of `none`, `local-model`, `open-ai-compatible`" — again a `Parse`; and
    // dropping the container `default`, which makes the partial section below error on its missing sibling.
}

/// Write `body` into a config file and load it expecting refusal; return the file and the error.
///
/// [`embeddings_refusal`] does the same for `[embeddings]`; this one exists so the panic on a wrong pass
/// describes what a *root-level* refusal would have meant, rather than printing an embedding provider nobody
/// was asking about.
fn root_refusal(body: &str) -> (PathBuf, ConfigError) {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), body);
    let err = match Config::load(Some(&path)) {
        Ok(cfg) => panic!(
            "the file must be refused; it loaded with catalog.max_entries_per_file = {}",
            cfg.catalog.max_entries_per_file
        ),
        Err(err) => err,
    };
    (path, err)
}

/// **F35, and a stance reversed rather than quietly deleted.** This case used to read
/// `an_unknown_key_outside_embeddings_still_loads` and its whole purpose was to prove that strictness stopped
/// at the `[embeddings]` boundary. The human ruled on 2026-11-06 that it does not (`docs/design-f35-unknown-keys.md`),
/// so the fixture survives — same TOML bytes, including both stray keys it was written to defend — and what it
/// demands of them is inverted. A file's history should show a decision reversed on purpose, not a test that
/// vanished.
#[test]
fn a_stray_key_at_the_top_level_is_refused_naming_key_and_file() {
    let _env = hold_env();
    // One unknown key per fixture, on purpose. Measured with two in one file (`stray_top_level` at the root and
    // `stray` inside `[catalog]`), toml reports the **nested** one — "unknown field `stray`, expected
    // `max_entries_per_file` …, at line 5" — because the descent into a table happens before the root's own
    // unknown-field check. Asserting on a name the driver may or may not have chosen is how a refusal test
    // starts passing for the wrong key, so each case here has exactly one candidate cause.
    let (path, err) = root_refusal("stray_top_level = 1\n[catalog]\nmax_entries_per_file = 400\n");
    let message = err.to_string();

    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "an unknown key must travel as the typed parse refusal, got {err:?}"
    );
    // Quoted for the same reason `[embeddings]`'s case quotes: toml appends "expected one of `data_dir`,
    // `catalog`, …", so an unquoted assertion would be satisfied by the suggestion list.
    assert!(
        message.contains("`stray_top_level`"),
        "the refusal must name the key as written: {message}"
    );
    assert!(
        message.contains(&path.display().to_string()),
        "and name the file to go and fix, in its own words: {message}"
    );

    // Mutation this catches: dropping `deny_unknown_fields` from `Config`, which loads this file and drops the
    // stray key — the panic inside `root_refusal` prints the value that would have been published instead.
}

/// Two unknowns in one file, and which one gets named. This is the diagnostic's own rule, measured rather than
/// assumed, and it belongs in the suite because a reader who expects the *first line* to be named would write
/// the assertion above wrong: with `stray_top_level` on line 2 and `stray` inside `[catalog]` on line 5, the
/// refusal points at line 5.
#[test]
fn an_unknown_key_inside_a_section_is_the_one_named_when_the_root_has_one_too() {
    let _env = hold_env();
    let (_path, err) =
        root_refusal("stray_top_level = 1\n[catalog]\nmax_entries_per_file = 400\nstray = \"x\"\n");
    let message = err.to_string();

    assert!(
        message.contains("`stray`") && message.contains("line 5"),
        "the nested key is the one named, at its own line: {message}"
    );

    // Mutation this catches: any change that makes the root report first — a hand-written visitor, or moving
    // `deny_unknown_fields` off `CatalogConfig` — flips which name appears here, and this case is where that
    // change would have to be re-decided rather than discovered by a red suite.
}

/// The card's own scenario: one letter wrong in a **section** name, which used to cost that subsystem's whole
/// settings in silence. `[catalogg]` is not read as `[catalog]`, so `max_entries_per_file` stayed at 600 while
/// its owner believed they had written 200 — a catalog rendered at defaults under a config that says otherwise.
#[test]
fn an_invented_section_is_refused_naming_the_section_and_its_line() {
    let _env = hold_env();
    let (path, err) = root_refusal("[catalogg]\nmax_entries_per_file = 200\n");
    let message = err.to_string();

    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "an invented section is a parse refusal, not a warning: {err:?}"
    );
    assert!(
        message.contains("`catalogg`"),
        "the invented section must be named as written: {message}"
    );
    // The line number is what makes this fixable in a 40-line file. `data_dir` sits on line 1 (written by the
    // helper), so the bad section is line 2 — asserted as a number rather than "some digits", because a
    // diagnostic that pointed at line 1 would still contain a line number.
    assert!(
        message.contains("line 2"),
        "the refusal must point at the line holding the section: {message}"
    );
    assert!(
        message.contains(&path.display().to_string()),
        "and name the file: {message}"
    );

    // Mutation this catches: a root that ignores unknown *tables* while refusing unknown scalars — serde's
    // `flatten`, or a hand-written visitor, both plausible ways to make the top level strict-looking while
    // `[catalogg]` keeps loading whole.
}

/// Every container refuses a key it has never heard of, asserted one row per struct so a section added later cannot rejoin
/// a lenient set by omission. F36 added the third row with the section: `[search]` decides how a query is ranked, and a
/// typo there (`modee = "keyword"`) would otherwise change an answer in silence — the exact failure F35 was written against.
#[test]
fn every_section_refuses_an_unknown_key() {
    let _env = hold_env();
    for (section, body, named) in [
        (
            "[catalog]",
            "[catalog]\nmax_entries_per_file = 400\nstray = \"x\"\n",
            "`stray`",
        ),
        (
            "[markers]",
            "[markers]\ndone = [\"done\"]\npausedd = [\"later\"]\n",
            "`pausedd`",
        ),
        ("[search]", "[search]\nmodee = \"keyword\"\n", "`modee`"),
        (
            "[models]",
            "[models]\nauto_fetche = true\n",
            "`auto_fetche`",
        ),
    ] {
        let (path, err) = root_refusal(body);
        let message = err.to_string();
        assert!(
            matches!(err, ConfigError::Parse { .. }),
            "{section}: expected a parse refusal, got {err:?}"
        );
        assert!(
            message.contains(named),
            "{section}: the stray key must be named ({named}) in: {message}"
        );
        assert!(
            message.contains(&path.display().to_string()),
            "{section}: and the file, in: {message}"
        );
    }

    // Mutations this catches: dropping `deny_unknown_fields` from any one of the three — each half fails only its own row,
    // which is why they are one test with three fixtures rather than one fixture, and why a fourth section has to be added
    // here as well as in `a_file_of_real_keys_still_loads_every_section`.
}

/// **The half a strictness change gets wrong.** Refusing everything also satisfies every assertion above, so
/// this case pins the other direction: a file whose keys are all real loads, in every section, with its values
/// arriving. It is the same TOML shape the refusal cases use, with the strays spelled correctly.
#[test]
fn a_file_of_real_keys_still_loads_every_section() {
    let _env = hold_env();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(
        tmp.path(),
        "[catalog]\nmax_entries_per_file = 400\ncontinuation_indent = 4\n\
         [markers]\ndone = [\"done\"]\npaused = [\"on hold\"]\n\
         [embeddings]\nprovider = \"none\"\nmax_tokens = 512\n\
         [models]\nbase_url = \"https://mirror.example\"\nauto_fetch = false\n\
         [search]\nmode = \"keyword\"\n",
    );

    let cfg = Config::load(Some(&path)).expect("a file with no unknown key must never be refused");
    assert_eq!(cfg.catalog.max_entries_per_file, 400);
    assert_eq!(cfg.catalog.continuation_indent, 4);
    assert_eq!(cfg.markers.paused, ["on hold".to_string()]);
    assert_eq!(cfg.embeddings.provider, EmbeddingProvider::None);
    assert_eq!(cfg.embeddings.max_tokens, 512);
    assert_eq!(
        cfg.models.base_url.as_deref(),
        Some("https://mirror.example")
    );
    assert_eq!(
        cfg.search.mode,
        SearchMode::Keyword,
        "the ranking choice arrives, not just the section"
    );
    assert!(
        !cfg.models.auto_fetch,
        "an explicit `auto_fetch = false` must arrive rather than default back to true"
    );

    // Mutation this catches: making the root strict by refusing *nested tables* wholesale (a visitor that only
    // accepts scalars), which fails here on `[catalog]` while every refusal case above still passes.
}

// ---------------------------------------------------------------------------
// R6a: `[embeddings] max_tokens`, and the two provider refusals
// (`docs/design-m2-local-embeddings.md` §5, §6, §10).
// ---------------------------------------------------------------------------

/// An id of the shape `docs/design-m2-local-embeddings.md` §4 mandates — quantisation inside the id, `@n` on
/// the end — so a fixture cannot drift into accepting something `np models install` would refuse.
const LOCAL_ID: &str = "embeddinggemma-2-text-q8_0@1";

/// The token ceiling is a **build fact, and the refusal must say so**: 8,192 is how wide a context this build creates
/// (`local_model::REQUESTED_CTX`), decided as design §5's option C on 2026-10-30 — the same day's option B put it at
/// 4,096 and moved back with the window. History is the reason for the wording: it was 32 768 (justified by
/// `max_position_embeddings = 262144`, a RoPE/train-length property since disowned), then 8,192 (the model card's
/// attention window) — and **no test noticed either time**, because every other case is written
/// *relative to* the constant. So the number is pinned through behaviour rather than a constant comparison (which clippy
/// correctly refuses as `assertions_on_constants`, and which would only restate the source): a budget one over the
/// ceiling must be refused, and the sentence must describe what the ceiling really is. A refusal that blames the model
/// for a build choice sends the reader to a different GGUF when the answer is a shorter chunk — which is why this test
/// asserts the message does *not* claim the model cannot attend to that many tokens.
#[test]
fn the_token_ceiling_carries_the_builds_window_into_its_refusal() {
    let _env = hold_env();
    let (_path, err) = embeddings_refusal(&format!(
        "[embeddings]\nmax_tokens = {}\n",
        MAX_EMBEDDING_TOKENS + 1
    ));
    let message = err.to_string();
    assert!(
        // The number is a literal on purpose. Reading it from `MAX_EMBEDDING_TOKENS` would make the assertion true by
        // construction and blind to exactly the edit this test exists to catch — someone retuning the ceiling (as
        // happened twice in 2026-10-30) without touching the decision record.
        message.contains("8192") && message.contains("in this build"),
        "the refusal has to teach the reader where the limit comes from, not just reject a number: {message}"
    );
    assert!(
        !message.contains("this model attends"),
        "the ceiling is our context width, not a claim about the artifact's training: {message}"
    );

    // And the ceiling itself is reachable: asking for exactly the window is legal, so the fence bounds the build rather than
    // refusing every real budget.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(
        tmp.path(),
        &format!("[embeddings]\nmax_tokens = {}\n", MAX_EMBEDDING_TOKENS),
    );
    let cfg = Config::load(Some(&path)).expect("the ceiling itself must be accepted");
    assert!(
        DEFAULT_EMBEDDING_TOKENS <= cfg.embeddings.max_tokens,
        "the default budget cannot exceed the bound it is validated against"
    );

    // Mutations this catches: editing the ceiling without the decision record (the message stops naming the build),
    // re-wording the refusal back to blaming the model, and lowering the ceiling below the default — the last of which
    // `local_model`'s compile-time `MAX_EMBEDDING_TOKENS <= REQUESTED_CTX` also refuses to build past.
}

#[test]
fn an_absent_max_tokens_takes_the_default_and_changes_nothing_else() {
    let _env = hold_env();
    // R6a adds a key to a section that used to *refuse* it by name, so the property that matters is what an
    // existing file still means: two fixtures written before this round — one silent about embeddings, one
    // naming provider/model/dim — must parse to exactly the values they parsed to then, plus the default budget. A
    // default that only appeared in `Config::default()` and not in the container default would fail here on the
    // second fixture while the first stayed green.
    //
    // 1,024 is design §5's decision B (2026-10-30), priced by R6b2's throughput curve; the number before it was 2048,
    // which was justified by arithmetic about chunk bytes rather than by a measured cost. Changing it again is a §5
    // change with a measurement attached, not an edit to this line.
    assert_eq!(
        DEFAULT_EMBEDDING_TOKENS, 1_024,
        "§5 fixes the budget at 1,024 tokens; changing it needs a re-measurement and a design note, not a constant edit"
    );

    for body in [
        "[catalog]\nmax_entries_per_file = 400\n",
        "[embeddings]\nprovider = \"local-model\"\nmodel = \"embeddinggemma-2-text@1\"\ndim = 512\n",
    ] {
        let tmp = TempDir::new().expect("test setup: tempdir");
        let path = write_config_with_data_dir(tmp.path(), body);
        let cfg = Config::load(Some(&path))
            .unwrap_or_else(|err| panic!("a file that never mentions max_tokens is still valid: {err}"));
        assert_eq!(
            cfg.embeddings.max_tokens, DEFAULT_EMBEDDING_TOKENS,
            "absent means the default, for {body:?}"
        );
    }

    // The values the pre-R6a file already meant, asserted rather than assumed.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(
        tmp.path(),
        "[embeddings]\nprovider = \"local-model\"\nmodel = \"embeddinggemma-2-text@1\"\ndim = 512\n",
    );
    let cfg = Config::load(Some(&path)).expect("test setup: load the pre-R6a file");
    assert_eq!(cfg.embeddings.provider, EmbeddingProvider::LocalModel);
    assert_eq!(
        cfg.embeddings.model.as_deref(),
        Some("embeddinggemma-2-text@1")
    );
    assert_eq!(cfg.embeddings.dim, Some(512));
    assert_eq!(
        cfg.catalog.max_entries_per_file, 600,
        "untouched sections stay default"
    );

    // Mutations this catches: `max_tokens: usize` with no manual `Default` (absent becomes 0, which the loader
    // then refuses — both fixtures fail to load), and editing the constant.
}

#[test]
fn an_explicit_max_tokens_is_taken_verbatim_up_to_the_ceiling() {
    let _env = hold_env();
    // The boundaries of the accepted range: 1 is a legal if brutal budget, and `MAX_EMBEDDING_TOKENS` itself
    // must load — a check that refuses its own ceiling has off-by-one-drifted into breaking documented range,
    // which is the reason `the_ceiling_itself_is_accepted_and_zero_means_no_indent` exists for the indent.
    for tokens in [1_usize, 512, DEFAULT_EMBEDDING_TOKENS, MAX_EMBEDDING_TOKENS] {
        let tmp = TempDir::new().expect("test setup: tempdir");
        let path = write_config_with_data_dir(
            tmp.path(),
            &format!("[embeddings]\nprovider = \"none\"\nmax_tokens = {tokens}\n"),
        );
        let cfg = Config::load(Some(&path))
            .unwrap_or_else(|err| panic!("{tokens} tokens is inside the documented range: {err}"));
        assert_eq!(cfg.embeddings.max_tokens, tokens, "for {tokens}");
    }

    // Mutation this catches: `1..MAX_EMBEDDING_TOKENS` instead of `1..=`, which refuses the ceiling.
}

#[test]
fn a_max_tokens_outside_its_bounds_is_refused_naming_key_value_and_both_bounds() {
    // Zero is the refusal that matters most: a budget of nothing refuses every chunk, and a report that said
    // "0 chunks embedded" over it would read as a corpus already done. The ceiling case is the typo
    // (`999999999`) this bound exists to catch before R6b1's runtime allocates for it.
    for tokens in [0_usize, MAX_EMBEDDING_TOKENS + 1, 999_999_999] {
        let (_path, err) = embeddings_refusal(&format!(
            "[embeddings]\nprovider = \"none\"\nmax_tokens = {tokens}\n"
        ));
        let message = err.to_string();
        assert!(
            message.contains("embeddings.max_tokens"),
            "the refusal must name the key to fix: {message}"
        );
        assert!(
            message.contains(&tokens.to_string()),
            "and the offending value: {message}"
        );
        assert!(
            message.contains("1 and") && message.contains(&MAX_EMBEDDING_TOKENS.to_string()),
            "and the range to aim at, so it is actionable from the message alone like F17's indent: {message}"
        );
        let ConfigError::Io(io_err) = &err else {
            panic!("an out-of-range value must travel as Io(InvalidData), got {err:?}");
        };
        assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidData);

        // Mutation this catches: dropping the range check (both load silently), or clamping to the ceiling — a
        // clamp would publish bytes the config file does not describe, which is why F17 refuses instead.
    }
}

/// `np models install --dim 0` is refused by np-cli's type; a config file is the other door to the same value, and
/// R6a made it load-bearing (it becomes `LocalModelProvider::new`'s `configured` argument). Migration 7's
/// `CHECK (length(vector) = 4 * dim)` admits a zero-byte vector at `dim = 0`, so both doors refuse.
#[test]
fn a_zero_width_is_refused_because_it_names_no_vector() {
    let _env = hold_env();
    let (_path, err) = embeddings_refusal(
        "[embeddings]\nprovider = \"local-model\"\nmodel = \"gemma-2-text-q8_0@1\"\ndim = 0\n",
    );
    let message = err.to_string();
    assert!(
        message.contains("embeddings.dim") && message.contains("zero-width"),
        "the refusal names the key and the reason: {message}"
    );

    // The absent arm stays legal: leaving the width unset is a different statement from asserting zero, and this is
    // the shape every fresh install has.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[embeddings]\nprovider = \"none\"\n");
    let unset = Config::load(Some(&path)).expect("omitting dim stays valid");
    assert_eq!(unset.embeddings.dim, None, "unset is not zero");

    // Mutation this catches: deleting the bound — the first `embeddings_refusal` then panics inside itself, which is
    // how it reports a file that loaded when it should not have.
}

#[test]
fn local_model_without_a_model_is_refused_naming_the_key_it_needs() {
    // §6's exit state (ii) starts here: a chosen provider with no identity cannot write `embeddings.model`,
    // and loading it happily would make `np embed` report "nothing owed" for a corpus nothing named.
    let (_path, err) = embeddings_refusal("[embeddings]\nprovider = \"local-model\"\n");
    let message = err.to_string();
    assert!(
        message.contains("local-model") && message.contains("embeddings.model"),
        "the refusal has to name both the provider that needs it and the key that supplies it: {message}"
    );
    let ConfigError::Io(io_err) = &err else {
        panic!("valid TOML carrying an unservable choice must not be reported as a parse error: {err:?}");
    };
    assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidData);

    // Mutation this catches: defaulting the model to something (`\"none\"`, the first row in `models`) — the
    // file then loads, this test fails on the panic inside `embeddings_refusal`, and a corpus gets embedded
    // under a model nobody chose.
}

#[test]
fn local_model_with_an_uninstallable_id_is_refused_naming_the_id_and_the_rule() {
    let _env = hold_env();
    // "a `model` naming an installed row", the half that needs no database: whether an id *can* name a row is
    // `models::validate_id`'s rule, reused here rather than restated — and the id itself goes in the message,
    // because the reader has to know which of their keys to edit.
    for model in ["GEMMA-2", "has space", ".hidden", "trailing."] {
        let (_path, err) = embeddings_refusal(&format!(
            "[embeddings]\nprovider = \"local-model\"\nmodel = \"{model}\"\n"
        ));
        let message = err.to_string();
        assert!(
            message.contains(model),
            "the refusal must carry the id it refused: {message}"
        );
        assert!(
            message.contains("embeddings.model"),
            "and say which key holds it: {message}"
        );

        // Mutation this catches: dropping the `validate_id` call (this file loads, and the refusal moves to
        // embed time — or never arrives, because no row can ever be installed under these ids).
    }

    // And an id that *is* installable is accepted, even though no such row exists yet: whether one does is a
    // question for the caller holding the workspace database (`db::model_row`), not for a config file.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(
        tmp.path(),
        &format!("[embeddings]\nprovider = \"local-model\"\nmodel = \"{LOCAL_ID}\"\n"),
    );
    let cfg = Config::load(Some(&path))
        .unwrap_or_else(|err| panic!("`{LOCAL_ID}` is exactly what §4 says to install: {err}"));
    assert_eq!(cfg.embeddings.model.as_deref(), Some(LOCAL_ID));
}

#[test]
fn openai_compatible_still_parses_and_refuses_at_load_naming_what_changed() {
    // §10's decision, fenced: the variant stays so an existing file is not told it is malformed, and the
    // refusal is ours so it can say what went away and what to write instead. The F17 precedent is exactly this
    // shape — `ConfigError::Parse` passes serde's text through verbatim, which would accuse valid TOML.
    let (_path, err) = embeddings_refusal("[embeddings]\nprovider = \"openai-compatible\"\n");
    let message = err.to_string();
    assert!(
        message.contains("openai-compatible"),
        "the refusal must name the spelling it refused, or it cannot be matched to the file: {message}"
    );
    assert!(
        message.contains("local") && message.contains("local-model"),
        "what changed (embeddings are local) and what to write instead, both in one sentence: {message}"
    );
    let ConfigError::Io(io_err) = &err else {
        panic!(
            "a valid file must be refused by our rule, not by serde's unknown-variant text: {err:?}"
        );
    };
    assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidData);

    // Mutations this catches: deleting the variant (serde's `Parse` then answers a well-formed file and this
    // test fails on the `Io` match), and serving the spelling anyway — which the panic inside
    // `embeddings_refusal` reports, loaded provider and all.
}

#[test]
fn the_new_refusals_are_scoped_to_the_provider_that_needs_them() {
    // **Holds the environment lock, and did not used to.** This case loads a config file, and `Config::load` reads
    // `NP_MODELS_BASE_URL` whether or not the fixture asked — so while another case had that key installed as
    // `mirror.example`, this load refused on the origin instead of reaching the assertion below. It survived years of
    // green runs because the window is a thread race, and R8's three new cases moved the timing just enough to lose it:
    // measured failing (`Io(InvalidData, "invalid NP_MODELS_BASE_URL = …")`) on one run of four and passing on three.
    // `every_case_that_loads_a_config_holds_the_environment_lock` is the fence that stops this happening again.
    let _env = hold_env();
    // The other half of "refuse loudly", and the half a single fixture cannot see: an id that would be refused
    // beside `local-model` is ordinary text beside `none`, because nothing will embed. Refusing it there would
    // break config files that work today. (This used to cite `an_unknown_key_outside_embeddings_still_loads` as
    // the same argument one level up; F35 reversed *that* rule, and this one stands on its own: refusing an
    // unread `model` id beside a provider that will never embed is a different claim from refusing an unknown
    // key, and it is still scoped by design.)
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path =
        write_config_with_data_dir(tmp.path(), "[embeddings]\nmodel = \"GEMMA-2\"\ndim = 512\n");
    let cfg = Config::load(Some(&path))
        .expect("a default provider does not need its unread model key to be an installable id");
    assert_eq!(cfg.embeddings.provider, EmbeddingProvider::None);
    assert_eq!(cfg.embeddings.model.as_deref(), Some("GEMMA-2"));

    // Mutation this catches: running `validate_id` on `model` regardless of provider, which turns every
    // existing file holding a placeholder string into a refusal.
}

// ---------------------------------------------------------------------------
// R7b C1: `[models] base_url` and `[models] sha256`
// (`docs/design-m2-local-embeddings.md` §12 rule 7).
//
// The section's two keys are the only place this program can be told *where* artifacts come from and *which digest*
// to accept instead of the pin, so the cases below are mostly about refusals — a value that silently means "unset"
// is §12 rule 7's forbidden outcome ("Silent fallback stays impossible"). Precedence is asserted in
// `the_file_wins_and_the_environment_fills_only_an_unset_key`, and it mirrors `data_dir` in `Config::load`: the file
// wins, the environment fills a key the file left unset. `[embeddings]` has no env rule to mirror because it has
// none, so nothing here copies one from it.
// ---------------------------------------------------------------------------

/// A digest of the shape both doors accept, spelled in uppercase so the normalisation is visible: sha256 of the
/// empty input, which is published (`tests/hash.rs` asserts the lowercase spelling), so this fixture invents nothing.
const UPPER_DIGEST: &str = "A3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855";
const LOWER_DIGEST: &str = "a3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Write `body` as a config file, load it expecting refusal, and return the file with the error.
///
/// The `[models]` twin of [`embeddings_refusal`], including why the path comes back: "names the file" has to be
/// asserted against the error's **own** text. It also holds a [`TestEnv`] for the duration of the load, so the
/// refusal being examined cannot have been produced by an override sitting in the developer's shell.
fn models_refusal(body: &str) -> (PathBuf, ConfigError) {
    let _env = TestEnv::install();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), body);
    let err = match Config::load(Some(&path)) {
        Ok(cfg) => panic!(
            "the file must be refused, loaded base_url = {:?} sha256 = {:?}",
            cfg.models.base_url, cfg.models.sha256
        ),
        Err(err) => err,
    };
    (path, err)
}

#[test]
fn an_absent_models_section_names_neither_a_source_nor_an_override() {
    // The shipped state, and the one every existing config file is in: no fetch source, and the pin stands. Under
    // `TestEnv` because "absent" has to mean absent from the environment as well as from the file.
    let _env = TestEnv::install();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[catalog]\nmax_entries_per_file = 400\n");

    let cfg = Config::load(Some(&path)).expect("a file with no [models] section is a valid config");

    assert_eq!(cfg.models.base_url, None);
    assert_eq!(cfg.models.sha256, None);
    assert_eq!(
        cfg.models,
        ModelsConfig::default(),
        "and that is the same state `Config::default()` describes for a fresh install"
    );
    assert_eq!(
        cfg.models.unpinned_digest_override(),
        None,
        "no override means nothing unpinned can be accepted"
    );

    // Mutation this catches: defaulting `base_url` to some host — the file above would then carry an origin nobody
    // chose, which is the compile-time-secret shape §12 rule 7 refuses.
}

#[test]
fn a_models_section_arrives_apart_from_the_two_normalisations() {
    // Exactly two things may change a value on its way in: one trailing `/` off an origin, and case off a digest.
    // Everything else is the operator's spelling, kept.
    let _env = TestEnv::install();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(
        tmp.path(),
        &format!(
            "[models]\nbase_url = \"https://mirror.example/models/\"\nsha256 = \"{UPPER_DIGEST}\"\n"
        ),
    );

    let cfg = Config::load(Some(&path)).expect("a valid [models] section loads");

    assert_eq!(
        cfg.models.base_url.as_deref(),
        Some("https://mirror.example/models"),
        "one trailing slash is the same origin twice, and storing it would make every later join ambiguous"
    );
    assert_eq!(
        cfg.models.sha256.as_deref(),
        Some(LOWER_DIGEST),
        "a digest is compared against lowercase hex, so it is stored that way"
    );
    assert_eq!(
        cfg.models.unpinned_digest_override(),
        Some(LOWER_DIGEST),
        "the accessor reports the same lowered value the field holds, not the input spelling"
    );

    // The other normalisation edge: only **one** slash comes off. `…/base//` is not a typo this code can read minds
    // about, and a later join will show it rather than hide it.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(
        tmp.path(),
        "[models]\nbase_url = \"https://mirror.example/base//\"\n",
    );
    let cfg = Config::load(Some(&path)).expect("a doubled slash is odd, not invalid");
    assert_eq!(
        cfg.models.base_url.as_deref(),
        Some("https://mirror.example/base/")
    );

    // And a port survives untouched — the shape C2's loopback fixture will use.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(
        tmp.path(),
        "[models]\nbase_url = \"http://127.0.0.1:8099\"\n",
    );
    let cfg = Config::load(Some(&path)).expect("a loopback origin with a port is an origin");
    assert_eq!(
        cfg.models.base_url.as_deref(),
        Some("http://127.0.0.1:8099")
    );
}

#[test]
fn the_file_wins_and_the_environment_fills_only_an_unset_key() {
    // `data_dir`'s precedence, reused: a file that names a value is not overruled by the environment, and a file
    // that omits one takes the environment's. Both halves are asserted because a one-directional rule would pass a
    // test written for the other half — and only one of the two directions is what an operator debugging a mirror
    // expects.
    let mut env = TestEnv::install();
    env.set_var(
        "NP_MODELS_BASE_URL",
        Some("https://from-the-environment.example"),
    );
    env.set_var("NP_MODELS_SHA256", Some(LOWER_DIGEST));

    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(
        tmp.path(),
        "[models]\nbase_url = \"https://from-the-file.example\"\n",
    );
    let cfg = Config::load(Some(&path)).expect("both values are legal");
    assert_eq!(
        cfg.models.base_url.as_deref(),
        Some("https://from-the-file.example"),
        "an explicit file value is not overridden by the environment"
    );
    assert_eq!(
        cfg.models.sha256.as_deref(),
        Some(LOWER_DIGEST),
        "a key the file left unset comes from the environment"
    );

    // With neither key in the file, both come from the environment — and the env value is normalised and validated
    // on the way in like any other (see `an_env_supplied_value_is_validated_and_named_by_variable`).
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[catalog]\nmax_entries_per_file = 400\n");
    let cfg =
        Config::load(Some(&path)).expect("an environment-only [models] section is a valid config");
    assert_eq!(
        cfg.models.base_url.as_deref(),
        Some("https://from-the-environment.example")
    );
    assert_eq!(cfg.models.sha256.as_deref(), Some(LOWER_DIGEST));

    // Mutation this catches: inverting the precedence (the first assertion fails), or never reading the environment
    // (the second does).
}

#[test]
fn an_empty_models_variable_counts_as_unset() {
    // `env_path`'s rule for a path — "an empty value counts as unset, as the XDG base directory spec requires" —
    // extended to these two keys, because `NP_MODELS_SHA256=` in a CI shell means "not set", not "the empty digest
    // is acceptable". The second reading would be a refusal at best and an override at worst.
    let mut env = TestEnv::install();
    env.set_var("NP_MODELS_BASE_URL", Some(""));
    env.set_var("NP_MODELS_SHA256", Some(""));

    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[catalog]\nmax_entries_per_file = 400\n");
    let cfg = Config::load(Some(&path)).expect("an empty variable is unset, and unset is valid");

    assert_eq!(cfg.models.base_url, None, "empty is not an origin");
    assert_eq!(cfg.models.sha256, None, "empty is not a digest");
}

#[test]
fn a_models_origin_that_is_not_http_or_https_is_refused_naming_key_and_value() {
    // Five shapes that are not a fetch source. Each refusal has to name the key and carry the value, because the
    // reader is looking at a file with two similar keys and needs to know which line to edit.
    for value in [
        "mirror.example/models",
        "ftp://mirror.example",
        "https://",
        "https://user@mirror.example",
        "https://mir ror.example",
    ] {
        let (_path, err) = models_refusal(&format!("[models]\nbase_url = \"{value}\"\n"));
        let message = err.to_string();
        assert!(
            message.contains("models.base_url"),
            "the refusal must name the key: {message}"
        );
        assert!(
            message.contains(value),
            "and carry the value it refused: {message}"
        );
        let ConfigError::Io(io_err) = &err else {
            panic!("valid TOML holding an unservable origin must not be reported as a parse error: {err:?}");
        };
        assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidData);
    }

    // Mutation this catches: accepting anything non-empty, which lets `ftp://` or a bare host reach a transport and
    // turn a configuration typo into a runtime refusal three hundred megabytes later.
}

#[test]
fn a_models_digest_that_is_not_64_hex_is_refused_naming_the_key() {
    // The same posture as `install --sha256`, which refuses the **format** before hashing anything: a truncated
    // paste and a different artifact are different problems, and only one of them is fixed by re-running
    // `np models hash`. The 8-character case is not arbitrary — it is the length of the prefix design §12 quotes,
    // which is exactly what an operator would paste from a chat message.
    for value in ["c4a8a526", &"0".repeat(63), &"z".repeat(64)] {
        let (_path, err) = models_refusal(&format!("[models]\nsha256 = \"{value}\"\n"));
        let message = err.to_string();
        assert!(
            message.contains("models.sha256"),
            "the refusal must name the key: {message}"
        );
        assert!(
            message.contains("64 hexadecimal characters"),
            "and the rule it broke, in the shape `install --sha256` already uses: {message}"
        );
        let ConfigError::Io(io_err) = &err else {
            panic!("a bad digest is data being rejected, not a broken file: {err:?}");
        };
        assert_eq!(io_err.kind(), std::io::ErrorKind::InvalidData);
    }

    // The value itself stays out of the message — 64 characters of noise in a sentence that has to stay readable,
    // and the operator can read their own file.
    let (_path, err) = models_refusal(&format!("[models]\nsha256 = \"{}\"\n", "z".repeat(64)));
    assert!(
        !err.to_string().contains(&"z".repeat(16)),
        "the refused digest must not be echoed: {err}"
    );
}

#[test]
fn an_env_supplied_value_is_validated_and_named_by_variable() {
    // A rule that watched only the file would let the environment route around it, which is why validation runs on
    // the merged value. The message then has to name the **variable**: telling someone to edit a `models.base_url`
    // line their file does not contain wastes the one thing a refusal is for.
    let mut env = TestEnv::install();
    env.set_var("NP_MODELS_BASE_URL", Some("mirror.example"));
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[catalog]\nmax_entries_per_file = 400\n");
    let err = match Config::load(Some(&path)) {
        Ok(cfg) => panic!(
            "an environment origin with no scheme must be refused, got {:?}",
            cfg.models.base_url
        ),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("NP_MODELS_BASE_URL"),
        "the refusal has to point at the variable: {err}"
    );

    // Same for the digest, and the same lowercase normalisation on the way in — otherwise the override would be
    // compared against a pin in a different case and refuse a byte-perfect download.
    env.set_var("NP_MODELS_BASE_URL", None);
    env.set_var("NP_MODELS_SHA256", Some("c4a8a526"));
    let err = match Config::load(Some(&path)) {
        Ok(cfg) => panic!(
            "an 8-character digest must be refused, got {:?}",
            cfg.models.sha256
        ),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("NP_MODELS_SHA256") && message.contains("64 hexadecimal characters"),
        "the variable and the rule, both in one sentence: {message}"
    );

    env.set_var("NP_MODELS_SHA256", Some(UPPER_DIGEST));
    let cfg =
        Config::load(Some(&path)).expect("a valid uppercase digest from the environment loads");
    assert_eq!(
        cfg.models.unpinned_digest_override(),
        Some(LOWER_DIGEST),
        "and is stored lowered, exactly like the file's"
    );
}

#[test]
fn an_unrecognised_models_key_is_refused_naming_key_and_file() {
    // §12 rule 7's "silent fallback stays impossible", applied to a typo: `baseurl` would otherwise load as *no
    // source configured*, and the operator would believe their mirror was set while every fetch refused for want of
    // an origin. Same reasoning, and same refusal shape, as `a_misspelled_embeddings_key_is_refused_naming_key_and_file`.
    let (_path, err) = models_refusal("[models]\nbaseurl = \"https://mirror.example\"\n");
    let message = err.to_string();

    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "an unknown key must travel as the typed parse refusal, got {err:?}"
    );
    // Quoted, for the reason that test gives: toml lists the legal fields too, so a bare word would match the
    // suggestion list. "`baseurl`" is not a substring of "`base_url`".
    assert!(
        message.contains("`baseurl`"),
        "the refusal must name the misspelled key as written: {message}"
    );

    // Mutation this catches: dropping `deny_unknown_fields` from `ModelsConfig`, which loads this file with
    // `base_url = None` — the panic inside `models_refusal` reports that, loaded value and all.
}

#[test]
fn strictness_no_longer_stops_at_a_section_boundary() {
    // Was `the_models_strictness_is_scoped_to_the_models_section`, whose entire claim was that a stray key
    // outside `[models]` kept loading. F35 removed the boundary (`docs/design-f35-unknown-keys.md`), so the
    // fixture stays — it is the exact file that used to load, `[models]` and all — and the demand inverts.
    // Keeping `[models] base_url` in it matters: the refusal has to arrive even when a *strict* section is
    // present and valid, because "one strict section" was the posture being replaced.
    let _env = TestEnv::install();
    // `[catalog]` lost its stray for the reason the other case gives: one unknown key per fixture, so the name
    // in the message is the one this test is about. What stays is the point — a valid, strict `[models]`
    // beside a root-level stray must not rescue the file.
    let (path, err) = root_refusal(
        "stray_top_level = 1\n[catalog]\nmax_entries_per_file = 400\n[models]\nbase_url = \"https://mirror.example\"\n",
    );
    let message = err.to_string();

    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "a valid `[models]` beside a stray key must not rescue the file: {err:?}"
    );
    assert!(
        message.contains("`stray_top_level`"),
        "the root's own stray must be named even beside a valid strict section: {message}"
    );
    assert!(
        message.contains(&path.display().to_string()),
        "and the file is named, in: {message}"
    );

    // Mutation this catches: reverting the root to lenient while leaving the sections strict — the pre-F35
    // posture, which loads this file and drops both strays.
}

/// A variable whose bytes are not valid UTF-8, so no `String` can hold it — the same construction
/// [`non_utf8_workspace`] uses for a path.
#[cfg(unix)]
fn non_utf8_value() -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    // `0xFF` is not a UTF-8 sequence.
    std::ffi::OsString::from_vec(vec![b'h', b't', b't', b'p', b':', 0xff])
}

#[cfg(unix)]
#[test]
fn a_non_text_models_variable_is_refused_rather_than_ignored() {
    // Both keys are ASCII by rule (an origin, a hex digest), so a value the OS cannot hand us as text is a mistake
    // in the environment someone could fix. Skipping it would report "unset" to a person who believes they set an
    // override — which is the silent fallback this section exists to forbid.
    //
    // Gated on unix because building non-UTF-8 bytes is platform-specific (F18: no claim about a platform this case
    // does not run on). Windows gets the same code path through `OsString`, untested here, and this file says so
    // rather than implying otherwise.
    // `TestEnv` captured this key at install, so its `Drop` puts the variable back — which is why the raw
    // `env::set_var` below (the typed helper takes `&str`, and the point here is a value that is not text) cannot
    // leak into another case. The binding itself is what holds [`ENV_LOCK`].
    let _env = TestEnv::install();
    env::set_var("NP_MODELS_BASE_URL", non_utf8_value());

    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[catalog]\nmax_entries_per_file = 400\n");
    let err = match Config::load(Some(&path)) {
        Ok(cfg) => panic!(
            "a non-text origin must be refused, got {:?}",
            cfg.models.base_url
        ),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("NP_MODELS_BASE_URL"),
        "the refusal has to name the variable nobody can read: {err}"
    );
}

// --- `[embeddings] threads` (`docs/design-m2-thread-policy.md`) ---------------------------------------------

/// The rule, as arithmetic rather than as prose: what count does `auto` choose on a machine of size N?
///
/// Pinned because every value here is a refusal of something simpler, and each refusal has a measurement behind it
/// (§14's D13 rows): the ceiling exists because 32 workers on 16 CPUs ran at *worse than four*; the floor exists because
/// a bare 80 % of a two-core laptop is one worker at a third of today's throughput. A change to either bound has to
/// change this table, which is the point — the numbers are not folklore about "most machines".
#[test]
fn auto_workers_follow_the_machine_within_measured_bounds() {
    let expected: [(usize, usize); 14] = [
        // Degenerate input first: `available_parallelism()` answers a non-zero count, so 0 cannot arrive from the
        // platform — and it still must not produce zero workers, because zero is not something C can be told.
        (0, 1),
        (1, 1),
        (2, 2),
        (3, 3),
        (4, 4),
        // The fraction only bites once there is something to leave behind: on 5 and 6 CPUs, "leave 20 %" would mean
        // leaving one whole CPU, so the floor of four holds.
        (5, 4),
        (6, 4),
        (7, 5),
        (8, 6),
        (12, 9),
        // The development host, whose row is measured rather than derived: 12 workers = 395.1 tok/s against the
        // default four's 158.7 and all sixteen's 459.6.
        (16, 12),
        (20, 16),
        (32, 25),
        (64, 51),
    ];
    for (available, wanted) in expected {
        assert_eq!(
            ThreadCount::Auto.resolve(available),
            wanted,
            "auto on {available} CPUs must choose {wanted} workers"
        );
        // **Never more workers than CPUs.** The property the whole ceiling exists for; asserted for every row rather
        // than stated once, because it is the one a future "just use nproc" simplification would break.
        assert!(
            ThreadCount::Auto.resolve(available) <= available.max(1),
            "auto oversubscribed a {available}-CPU machine"
        );
    }

    // An explicit count is honoured exactly, including the counts the rule would refuse. §5 of the policy: an operator
    // may know something this program cannot see; what is refused is only a value the runtime cannot be told, and that
    // refusal happens at load (`an_impossible_worker_count_is_refused_naming_the_key`).
    // An absurd denominator, because `resolve` is public and the multiplication inside it used to be a plain `*`: what
    // matters is that it returns rather than panicking in a debug build.
    let absurd = ThreadCount::Auto.resolve(usize::MAX);
    assert!(absurd > 0, "the rule must never answer zero workers");

    let three = std::num::NonZeroU32::new(3).expect("test setup: 3 is non-zero");
    let ninety = std::num::NonZeroU32::new(90).expect("test setup: 90 is non-zero");
    assert_eq!(ThreadCount::Workers(three).resolve(16), 3);
    assert_eq!(ThreadCount::Workers(ninety).resolve(16), 90);
    // The label the report prints distinguishes the two, so "12 because auto" is not reported as "12 because you said
    // twelve".
    assert!(ThreadCount::Auto.is_auto());
    assert!(!ThreadCount::Workers(three).is_auto());
}

/// `log_level` takes exactly the five names (F33), and absence means "the caller's default", not "off".
///
/// The field is `Option`: a config that says nothing must not become a decision, because np-core installs no log callback at all for
/// a library caller and the CLI's default is its own choice. What this case pins is that the *spelling* is validated here, at the
/// boundary, so a typo cannot reach a threshold as silence.
#[test]
fn log_level_takes_the_five_names_and_absence_means_unset() {
    let _env = hold_env();
    let tmp = tempfile::tempdir().expect("test setup: tempdir");
    let data = toml_str(&tmp.path().display().to_string());

    for (name, body) in [
        (None, ""),
        (
            Some(np_core::loglevel::LogLevel::Off),
            "log_level = \"off\"",
        ),
        (
            Some(np_core::loglevel::LogLevel::Error),
            "log_level = \"error\"",
        ),
        (
            Some(np_core::loglevel::LogLevel::Warn),
            "log_level = \"warn\"",
        ),
        (
            Some(np_core::loglevel::LogLevel::Info),
            "log_level = \"info\"",
        ),
        (
            Some(np_core::loglevel::LogLevel::Debug),
            "log_level = \"debug\"",
        ),
    ] {
        let path = write_config(
            tmp.path(),
            &format!("data_dir = {data}\n[embeddings]\n{body}\n"),
        );
        let cfg =
            Config::load(Some(&path)).unwrap_or_else(|err| panic!("`{body}` must load: {err}"));
        assert_eq!(cfg.embeddings.log_level, name, "`{body}`");
    }
}

/// An invented level refuses naming the key, the value as written and all five permitted names.
///
/// The rows are not one kind of mistake and they do not fail alike, in the same split `threads` documents at
/// [`an_impossible_worker_count_is_refused_naming_the_key`]: a wrong *spelling* reaches our visitor and produces our sentence naming
/// `embeddings.log_level`, while a value of the wrong **type** is refused by serde before the visitor sees it — that message names the file
/// and lists what was expected, but not the key path. Pinning the difference is the point: smoothing it over would let a future change to
/// the visitor drop one half unnoticed.
#[test]
fn an_impossible_log_level_is_refused_naming_key_value_and_the_five_names() {
    let _env = hold_env();
    let tmp = tempfile::tempdir().expect("test setup: tempdir");
    let data = toml_str(&tmp.path().display().to_string());

    for (body, names_key) in [
        (r#"log_level = "trace""#, true),
        (r#"log_level = "INFO""#, true),
        (r#"log_level = """#, true),
        ("log_level = 3", false),
        ("log_level = true", false),
    ] {
        let path = write_config(
            tmp.path(),
            &format!("data_dir = {data}\n[embeddings]\n{body}\n"),
        );
        let err = Config::load(Some(&path)).expect_err("`{body}` must be refused");
        let text = err.to_string();
        let expectation = if names_key { "must" } else { "must not" };
        assert_eq!(
            text.contains("embeddings.log_level"),
            names_key,
            "`{body}` {expectation} name embeddings.log_level: {text}"
        );
        // Every refusal lists all five spellings, whichever layer produced it — that list is one constant in `loglevel.rs`, and this
        // assertion is what stops the parser and the promise from drifting apart.
        for name in ["off", "error", "warn", "info", "debug"] {
            assert!(
                text.contains(name),
                "`{body}` refusal must list the permitted name `{name}`: {text}"
            );
        }
        assert!(
            text.contains(&tmp.path().display().to_string()),
            "`{body}` must name the config file: {text}"
        );
    }

    // And a wrong *spelling* quotes what was written, so the reader knows which word to fix.
    let path = write_config(
        tmp.path(),
        &format!("data_dir = {data}\n[embeddings]\nlog_level = \"trace\"\n"),
    );
    let text = Config::load(Some(&path))
        .expect_err("must refuse")
        .to_string();
    assert!(text.contains("\"trace\""), "must quote the value: {text}");
}

/// Both accepted spellings load, and absence means `auto` rather than "unset, so four".
#[test]
fn threads_defaults_to_auto_and_takes_both_spellings() {
    let _env = hold_env();
    let tmp = tempfile::tempdir().expect("test setup: tempdir");
    let data = toml_str(&tmp.path().display().to_string());

    for (body, wanted) in [
        ("", ThreadCount::Auto),
        ("threads = \"auto\"", ThreadCount::Auto),
        (
            "threads = 3",
            ThreadCount::Workers(std::num::NonZeroU32::new(3).unwrap()),
        ),
    ] {
        let path = write_config(
            tmp.path(),
            &format!("data_dir = {data}\n[embeddings]\n{body}\n"),
        );
        let cfg =
            Config::load(Some(&path)).unwrap_or_else(|err| panic!("`{body}` must load: {err}"));
        assert_eq!(cfg.embeddings.threads, wanted, "`{body}`");
    }
}

/// The refusals, each naming the key so a person edits the right line.
///
/// These are deserializer failures rather than post-load validation on purpose: the message arrives with the file, line
/// and column serde already knows, and a value that cannot exist must not reach a struct where some later caller has to
/// remember to re-check it. The 3,000,000,000 case is not hypothetical — an earlier version of this seam *clamped* such a
/// count and llama.cpp then aborted the process (`SIGABRT`, uncatchable in Rust), which is why any ceiling is enforced at
/// the boundary rather than at load.
///
/// The rows are not all one kind of mistake, and they do not fail alike: `-1` and `0` are counts that do not exist,
/// `4097`/`100000000` are integers the field could hold and this program still will not ask for, `"most"`/`"AUTO"` are
/// strings outside the two accepted spellings — all four produce our own sentence naming `embeddings.threads`. A float, a
/// bool and an integer wider than `u64` are refused by serde before our visitor sees them, and name the key through the
/// rendered source line instead; that difference is pinned in its own loop below rather than smoothed over. The ceiling
/// rows are here because the first version of this bound was `i32::MAX`, which review found to be a sentence about the C
/// field rather than about the machine — `threads = 100000000` parsed, resolved verbatim and reached `n_threads`
/// (`config.rs`'s `MAX_REQUESTED_WORKERS` for the reasoning).
#[test]
fn an_impossible_worker_count_is_refused_naming_the_key() {
    let _env = hold_env();
    let tmp = tempfile::tempdir().expect("test setup: tempdir");
    let data = toml_str(&tmp.path().display().to_string());

    for (body, fragment) in [
        ("threads = 0", "embeddings.threads"),
        ("threads = -1", "embeddings.threads"),
        ("threads = 3000000000", "embeddings.threads"),
        ("threads = \"most\"", "embeddings.threads"),
        ("threads = \"AUTO\"", "embeddings.threads"),
        // Integers past the policy ceiling, including one that `i32_t` could hold and that used to load.
        ("threads = 4097", "embeddings.threads"),
        ("threads = 100000000", "embeddings.threads"),
    ] {
        let path = write_config(
            tmp.path(),
            &format!("data_dir = {data}\n[embeddings]\n{body}\n"),
        );
        let err = Config::load(Some(&path)).expect_err("`{body}` must be refused");
        let text = err.to_string();
        assert!(
            text.contains(fragment),
            "`{body}` must name {fragment:?}: {text}"
        );
        // And it has to say which file, like every other parse failure in this file.
        assert!(
            text.contains(&tmp.path().display().to_string()),
            "`{body}` must name the config file: {text}"
        );
    }

    // **Wrong types are a second class of refusal, and the message differs.** Measured rather than assumed: a float, a
    // bool and an integer too large for `u64` (toml hands that to serde as an `i128`, which this visitor does not
    // implement) never reach our code, so they come back as serde's `invalid type … expected "\"auto\"" or a positive
    // integer of worker threads`. They still name the key — through the rendered source line and column, e.g.
    // `2 | threads = 4.0` with a caret under the value — but not through the dotted path our own messages print, because
    // "which key" is toml's sentence to say and "what I wanted" is ours. Asserting `embeddings.threads` here would be a
    // test written against the prose rather than the output.
    for body in [
        "threads = 4.0",
        "threads = true",
        "threads = 99999999999999999999",
    ] {
        let path = write_config(
            tmp.path(),
            &format!("data_dir = {data}\n[embeddings]\n{body}\n"),
        );
        let text = Config::load(Some(&path))
            .expect_err("`{body}` must be refused")
            .to_string();
        assert!(
            text.contains("expected `\"auto\"` or a positive integer of worker threads"),
            "`{body}` must say what was expected: {text}"
        );
        assert!(
            text.contains("threads"),
            "`{body}` must point at the key it refused: {text}"
        );
        assert!(
            text.contains(&tmp.path().display().to_string()),
            "`{body}` must name the config file: {text}"
        );
    }

    // The typo case, refused by the section's existing strictness rather than by anything added here: `thread = 8` must
    // not load as "auto" in silence.
    let path = write_config(
        tmp.path(),
        &format!("data_dir = {data}\n[embeddings]\nthread = 8\n"),
    );
    let err = Config::load(Some(&path)).expect_err("a misspelled key must not be ignored");
    assert!(
        err.to_string().contains("thread"),
        "the refusal must name the misspelled key: {err}"
    );
}

/// **A boundary, not a clamp.** 4,096 loads as 4,096; 4,097 is refused. The distinction matters because the failure mode
/// this whole seam has been revised around twice is a value being quietly *changed* on the way in — first by clamping at
/// load (which produced an uncatchable `SIGABRT`), and before that by a ceiling so high it accepted anything.
#[test]
fn the_worker_ceiling_is_a_boundary_and_never_a_clamp() {
    let _env = hold_env();
    let tmp = tempfile::tempdir().expect("test setup: tempdir");
    let data = toml_str(&tmp.path().display().to_string());

    let path = write_config(
        tmp.path(),
        &format!("data_dir = {data}\n[embeddings]\nthreads = 4096\n"),
    );
    let cfg = Config::load(Some(&path)).expect("the largest accepted count must load");
    assert_eq!(
        cfg.embeddings.threads,
        ThreadCount::Workers(std::num::NonZeroU32::new(4096).unwrap()),
        "and load as itself, not as a smaller number"
    );

    let path = write_config(
        tmp.path(),
        &format!("data_dir = {data}\n[embeddings]\nthreads = 4097\n"),
    );
    assert!(
        Config::load(Some(&path)).is_err(),
        "one past the ceiling must be refused, not rounded down"
    );
}

/// **Every case in this file that calls `Config::load` holds [`ENV_LOCK`].**
///
/// The rule is stated at [`hold_env`] and the reason is not obvious: `[models]` reads `NP_MODELS_BASE_URL` /
/// `NP_MODELS_SHA256` on **every** load, so a case that never touches the environment is still reading another thread's
/// installed override. Cargo runs these cases on parallel threads; env vars belong to the process. That makes the failure
/// a race rather than a bug — it shows up as one run in four refusing with `invalid NP_MODELS_BASE_URL = "mirror.example"`
/// in a case about something else entirely, which is exactly what `the_new_refusals_are_scoped_to_the_provider_that_needs_them`
/// did (unprotected since `ef8a382`, exposed 2026-11-02 when R8 added three cases and shifted the timing).
///
/// The fence reads this file's own text at run time rather than trusting a comment, because the class of defect is a
/// *later* case forgetting the guard — the mistake is invisible to any check that only looks at what is there now. A test
/// that mutates the environment must take it through [`TestEnv`] (which owns and restores the values) or hold the lock by
/// hand; a test that merely loads a config must hold the lock too. Scope: this file only. Other test binaries are separate
/// processes, and `crates/np-cli/tests/cli.rs` gives each command an explicit environment.
#[test]
fn every_case_that_loads_a_config_holds_the_environment_lock() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/config.rs"),
    )
    .expect("the test file is readable from its own manifest dir");

    let mut offenders: Vec<&str> = Vec::new();
    for (name, body) in annotated_test_bodies(&source) {
        // This fence is not itself a config loader, and it must not have to hold the lock it exists to require.
        if name == "every_case_that_loads_a_config_holds_the_environment_lock" {
            continue;
        }
        if body.contains("Config::load")
            && !body.contains("hold_env()")
            && !body.contains("TestEnv::install")
        {
            offenders.push(name);
        }
    }
    assert!(
        offenders.is_empty(),
        "these cases call `Config::load` while another thread may have `NP_MODELS_*` installed: {offenders:?}\n\
         add `let _env = hold_env();` at the top of each (see [`hold_env`])."
    );
}
/// Split a Rust source file into `(fn name, body)` for every function annotated `#[test]`.
///
/// The attribute is part of the match on purpose: an earlier draft scanned every top-level function and reported
/// `config_with_data_dir` and `embeddings_refusal`, which are helpers that load a config *inside* a case that already holds
/// the lock. A fence with two false positives is a fence someone silences, so it asks the same question Rust does: is this
/// an annotated test? Brace counting rather than line matching, because a body containing `}` inside a string or a nested
/// closure would end a naive scan early and report the wrong span — which in a fence means a false pass.
fn annotated_test_bodies(source: &str) -> Vec<(&str, &str)> {
    const MARKER: &str = "#[test]";
    let mut out = Vec::new();
    let mut rest = source;
    while let Some(found) = rest.find(MARKER) {
        let after_attr = &rest[found + MARKER.len()..];
        // Attribute and signature are separated by whitespace only; anything else between them is not this pattern.
        let gap = after_attr.len() - after_attr.trim_start().len();
        let Some(head) = after_attr[gap..].strip_prefix("fn ") else {
            rest = after_attr;
            continue;
        };
        let Some(paren) = head.find('(') else { break };
        let name = &head[..paren];
        let Some(open) = head.find('{') else { break };
        let mut depth = 0usize;
        let mut end = None;
        for (index, ch) in head.char_indices() {
            if index < open {
                continue;
            }
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(index + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        out.push((name, &head[..end]));
        rest = &head[end..];
    }
    out
}

// ---------------------------------------------------------------------------
// F36: `[search] mode` (`docs/design-m3-hybrid-search.md`). Two cases, because a new section has two ways to be wrong: it
// can refuse what it should accept, and it can accept what it should refuse.
// ---------------------------------------------------------------------------

/// The default is the answer for the reader who never opens a config file, so it is pinned rather than left to a `Default`
/// impl nobody reads: absent section means hybrid, and an empty file loads.
#[test]
fn an_absent_search_section_means_hybrid() {
    let _env = hold_env();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "");

    let cfg = Config::load(Some(&path)).expect("an empty file is a valid file");
    assert_eq!(
        cfg.search.mode,
        SearchMode::Hybrid,
        "the shipped default is the ranking both lists produce"
    );
    assert_eq!(
        SearchConfig::default().mode,
        cfg.search.mode,
        "and `Default` agrees with the parser, so no path through the code can rank differently"
    );
}

/// A mode outside the three names is refused **by config**, not quietly downgraded to the default: a reader who writes
/// `semantic` as `semantics` has to be told, or they search a hybrid list believing it holds only vectors. The refusal names
/// what it expected, which is what distinguishes it from serde's bare "unknown variant".
#[test]
fn an_invented_search_mode_is_refused_and_names_the_three() {
    let _env = hold_env();
    let (path, err) = root_refusal("[search]\nmode = \"fuzzy\"\n");
    let message = err.to_string();
    assert!(
        matches!(err, ConfigError::Parse { .. }),
        "expected a parse refusal, got {err:?}"
    );
    assert!(
        message.contains("fuzzy"),
        "the value the file holds must be named in: {message}"
    );
    for name in ["hybrid", "semantic", "keyword"] {
        assert!(
            message.contains(name),
            "and each real mode has to be listed (`{name}` missing from: {message})"
        );
    }
    assert!(
        message.contains(&path.display().to_string()),
        "and the file, in: {message}"
    );

    // Mutations this catches: adding a serde `other` arm that swallows the value into the default, and renaming a mode in
    // one door only — `SearchMode` is the single definition behind config, `--mode` and `--json`, so a fourth spelling has
    // nowhere to enter. The argv door is pinned by `an_invented_mode_refuses_with_claps_exit_two` in `np-cli`.
}

// ---------------------------------------------------------------------------
// F38: `[models] auto_fetch` — the pinned model may be fetched without being asked twice,
// and there is a switch off that no config file can veto. `docs/design-f38-auto-fetch.md`.
// ---------------------------------------------------------------------------

/// **The default is the product decision.** An application calling `np embed` wants vectors to simply exist, so a
/// machine with nothing installed fetches rather than refuses — which is only safe because the surface is one pinned
/// id from one origin (§12 rule 7), not "the internet". A default flipped here flips first-run behaviour on every
/// machine, so it is asserted in both directions: absent means on, and `allowed()` agrees when nothing objects.
#[test]
fn an_absent_auto_fetch_leaves_the_pinned_model_fetchable() {
    let _env = hold_env();
    env::remove_var("NP_NO_AUTO_FETCH");
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[catalog]\nmax_entries_per_file = 300\n");

    let cfg = Config::load(Some(&path)).expect("a file with no [models] section must load");
    assert!(
        cfg.models.auto_fetch,
        "F38: fetching the pinned artifact is on unless someone says otherwise"
    );
    assert!(cfg.models.auto_fetch_allowed());
}

/// Off in the file means off, and a non-boolean is refused by name rather than read as truthy. TOML has a real boolean
/// type; accepting `"no"` would be the moment this key started lying about what it was set to.
#[test]
fn auto_fetch_can_be_switched_off_in_the_file_and_a_string_is_refused() {
    let _env = hold_env();
    env::remove_var("NP_NO_AUTO_FETCH");
    let tmp = TempDir::new().expect("test setup: tempdir");

    let off = write_config_with_data_dir(tmp.path(), "[models]\nauto_fetch = false\n");
    let cfg = Config::load(Some(&off)).expect("[models] auto_fetch = false is a real key");
    assert!(!cfg.models.auto_fetch);
    assert!(
        !cfg.models.auto_fetch_allowed(),
        "the file said off, and nothing else may talk it back on"
    );

    let text = write_config_with_data_dir(tmp.path(), "[models]\nauto_fetch = \"no\"\n");
    let err = Config::load(Some(&text)).expect_err("a string is not a boolean");
    assert!(
        err.to_string().contains("auto_fetch"),
        "the refusal must name the key: {err}"
    );
}

/// **The kill switch has to outrank the file, or it is not one.** `NP_MODELS_BASE_URL` *fills* a key the file left
/// unset (`base_url` wins), and that ordering is right for an origin — the file is the operator's stated intent. It
/// would be wrong here: CI, an air-gapped machine and a metered link all need to forbid a dial-out from outside, on a
/// box whose config says fetching is fine. So `NP_NO_AUTO_FETCH` overrides in one direction only, and any value except
/// empty or `0` counts — a switch you have to spell correctly under a network outage is a switch that does not work.
#[test]
fn the_kill_switch_works_without_a_file_to_veto_it() {
    let _env = hold_env();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(tmp.path(), "[models]\nauto_fetch = true\n");
    let cfg = Config::load(Some(&path)).expect("the file says fetching is allowed");
    assert!(
        cfg.models.auto_fetch_allowed(),
        "unset env leaves the file in charge"
    );

    for on in ["1", "true", "yes", "0 "] {
        env::set_var("NP_NO_AUTO_FETCH", on);
        assert!(
            !cfg.models.auto_fetch_allowed(),
            "NP_NO_AUTO_FETCH={on:?} must forbid fetching even though the file allows it"
        );
    }

    // Empty and `0` mean "not switched off": same unset rule as every other np environment key, so a script that
    // exports an empty variable from a template does not silently disable the feature.
    for off in ["", "0"] {
        env::set_var("NP_NO_AUTO_FETCH", off);
        assert!(
            cfg.models.auto_fetch_allowed(),
            "NP_NO_AUTO_FETCH={off:?} means unset, not off"
        );
    }
    env::remove_var("NP_NO_AUTO_FETCH");

    // Mutation this catches: reading the env var at load time and folding it into the field. That would make
    // `auto_fetch` mean two different things depending on when `Config::load` ran, and a caller holding a long-lived
    // Config (a daemon) could never turn fetching off mid-life.
}

// ---------------------------------------------------------------------------
// F37 — where the data directory lands, and what happens to an index that predates the nesting.
//
// The Windows arms of this layout cannot be compiled on CI's Linux runners or on this development host (`ring` and
// `libsqlite3-sys` need MSVC `lib.exe`), which is exactly why resolution takes a `DataDirPlatform` value instead of reading
// `cfg` itself: the shape is chosen by production with `cfg`, and every shape is *checked* here. A layout fix whose Windows
// half never runs would be a layout fix nobody verified — the bug this card was filed about survived six CI rows for precisely
// that reason.
// ---------------------------------------------------------------------------

/// `%LOCALAPPDATA%` is shared by every application; config has always nested under `note-processor` and data did not, so one
/// program had two answers to "where do you keep things" on one platform (F37). The fix nests data the same way.
#[test]
fn the_windows_data_dir_nests_the_program_folder_like_config_already_did() {
    let base = PathBuf::from(r"C:\Users\ana\AppData\Local");
    let got = data_dir_for(DataDirPlatform::Windows, &|key| match key {
        "LOCALAPPDATA" => Some(base.clone()),
        _ => None,
    });
    assert_eq!(
        got,
        Some(base.join(WINDOWS_DATA_SUBDIR)),
        "the index has to sit under a folder that says whose it is"
    );
    // The suffix is spelled by the library, not by this case, so README and code cannot drift apart quietly.
    assert_eq!(WINDOWS_DATA_SUBDIR, "note-processor");
}

/// A Windows shell with no `%LOCALAPPDATA%` gets a refusal, not a path built beside the current directory — the same rule
/// `Config::load` enforces on every platform.
#[test]
fn a_windows_environment_naming_nothing_still_refuses_rather_than_guessing() {
    assert_eq!(data_dir_for(DataDirPlatform::Windows, &|_key| None), None);
    // An empty variable is "unset" here, the way it already is for `XDG_DATA_HOME`: `env_path` filters it.
    // This case pins the *shape's* behaviour with a lookup that returns nothing; the empty-string filter is
    // `env_path`'s own contract, unchanged by F37.
}

/// Unix is untouched: `$XDG_DATA_HOME` as-is, else `$HOME/.local/share`, else nothing. Asserting this on the same seam keeps
/// "we fixed Windows" from becoming "and accidentally moved everybody else's index".
#[test]
fn the_unix_shape_is_unchanged_by_the_seam() {
    let xdg = PathBuf::from("/srv/notes-data");
    let home = PathBuf::from("/home/ana");
    let both = |key: &str| match key {
        "XDG_DATA_HOME" => Some(xdg.clone()),
        "HOME" => Some(home.clone()),
        _ => None,
    };
    assert_eq!(
        data_dir_for(DataDirPlatform::Unix, &both),
        Some(xdg.clone())
    );

    let home_only = |key: &str| (key == "HOME").then(|| home.clone());
    assert_eq!(
        data_dir_for(DataDirPlatform::Unix, &home_only),
        Some(home.join(".local/share"))
    );
    assert_eq!(data_dir_for(DataDirPlatform::Unix, &|_key| None), None);
}

/// **The fallback that keeps a vault from lying about itself.** An index is keyed by workspace path, so a data directory that
/// moved reads as an empty workspace — and a vault holding months of notes answering `added 214` is the worst first
/// experience this migration could produce. For one release, an old index is read where it stands.
#[test]
fn an_index_left_where_it_was_before_f37_is_still_read_from_there() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let nested = tmp.path().join("Local").join(WINDOWS_DATA_SUBDIR);
    let legacy = tmp.path().join("Local");
    fs::create_dir_all(&nested).expect("test setup: nested dir");
    fs::create_dir_all(legacy.join(WORKSPACES_DIR)).expect("test setup: legacy workspaces dir");
    fs::write(
        legacy.join(REGISTRY_FILE),
        b"not a database, and this case never opens it",
    )
    .expect("test setup: legacy registry");

    assert_eq!(
        legacy_data_dir_override(&nested, Some(&legacy)),
        Some(legacy.clone()),
        "the old location holds our index, so this run reads there"
    );
}

/// The nested location wins as soon as it holds **our** registry: a user who installed fresh after upgrading is not dragged
/// back to whatever unrelated `workspaces.sqlite` may sit in `%LOCALAPPDATA%`.
#[test]
fn a_nested_directory_holding_our_registry_wins_over_the_old_one() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let nested = tmp.path().join("Local").join(WINDOWS_DATA_SUBDIR);
    let legacy = tmp.path().join("Local");
    fs::create_dir_all(&nested).expect("test setup: nested dir");
    fs::create_dir_all(legacy.join(WORKSPACES_DIR)).expect("test setup: legacy workspaces dir");
    fs::write(nested.join(REGISTRY_FILE), b"ours").expect("test setup: nested registry");
    fs::write(legacy.join(REGISTRY_FILE), b"the old one").expect("test setup: legacy registry");

    assert_eq!(legacy_data_dir_override(&nested, Some(&legacy)), None);
}

/// An empty or absent legacy directory vetoes nothing — the ordinary fresh install. And a half-written legacy location (the
/// `workspaces/` directory with no registry beside it, which is what an interrupted first run leaves) still counts as ours:
/// reading the wrong directory is the failure this function exists to prevent, in both directions.
#[test]
fn an_empty_legacy_location_vetoes_nothing_and_a_half_written_one_still_counts() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let nested = tmp.path().join("Local").join(WINDOWS_DATA_SUBDIR);
    let legacy = tmp.path().join("Local");
    fs::create_dir_all(&nested).expect("test setup: nested dir");
    fs::create_dir_all(&legacy).expect("test setup: legacy dir, empty");

    assert_eq!(legacy_data_dir_override(&nested, Some(&legacy)), None);
    // No legacy directory at all (a machine that never ran an older build): same answer, no filesystem error.
    assert_eq!(
        legacy_data_dir_override(&nested, Some(&legacy.join("nope"))),
        None
    );

    fs::create_dir_all(legacy.join(WORKSPACES_DIR)).expect("test setup: legacy workspaces only");
    assert_eq!(
        legacy_data_dir_override(&nested, Some(&legacy)),
        Some(legacy),
        "a `workspaces/` directory with no registry is still our index"
    );
}

/// There was never a pre-F37 Windows-shaped location on Unix, so there is nothing to fall back to — asserted through the one
/// function that decides, with an environment deliberately holding both variables: on the Unix shape `%LOCALAPPDATA%` is
/// ignored outright, and a rule that moved somebody's data dir based on a variable they set for another platform would be
/// caught here.
#[test]
fn the_unix_shape_has_no_legacy_location_at_all() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let local = tmp.path().join("Local");
    fs::create_dir_all(local.join(WORKSPACES_DIR)).expect("test setup: a legacy-looking index");
    fs::write(local.join(REGISTRY_FILE), b"unrelated to unix").expect("test setup: registry");
    let xdg = tmp.path().join("xdg-data");

    let lookup = |key: &str| match key {
        "LOCALAPPDATA" => Some(local.clone()),
        "XDG_DATA_HOME" => Some(xdg.clone()),
        _ => None,
    };
    let (dir, legacy) = resolve_data_dir(None, DataDirPlatform::Unix, &lookup);
    assert_eq!(dir, xdg, "the Unix shape resolves to XDG and nothing else");
    assert_eq!(
        legacy, None,
        "and there is no older Unix location to fall back to"
    );
}

/// **The bug the Windows field run found (F37, 2026-11-08).** An upgraded machine fell back to `%LOCALAPPDATA%` exactly as designed —
/// its new workspace databases landed there — and printed no note. The reason was not in the fallback, which was right; it was in how
/// `Config::load` decided whether the operator had *declared* a directory: it asked the merged struct, and [`Config::default`] had
/// already resolved the fallback into `data_dir`. So every Windows run looked like an instruction, the fallback reported itself absent,
/// and the one platform with a fallback never heard about it.
///
/// This case pins the distinction the fix rests on: a file that never mentions `data_dir` does not declare one, **even though the
/// merged default's `data_dir` is non-empty**. Restore the merged-struct question and this test fails on Linux with the same shape the
/// field run hit — which matters, because the note itself can only ever print on Windows.
#[test]
fn a_file_that_never_names_data_dir_does_not_get_to_claim_one() {
    // Installed environment, not the one cargo happened to hand us. The `test-config-nohome` gate row runs this binary with `$HOME` and
    // `$XDG_CONFIG_HOME` unset — which is its whole point — and an assertion that "the default resolves *some* directory" means nothing in
    // an environment that names none: it failed there on the night it was written, for exactly the reason that row exists.
    let _env = TestEnv::install();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let file = tmp.path().join("config.toml");

    // A real config that says everything except where the data lives.
    fs::write(
        &file,
        "[embeddings]\nprovider = \"local-model\"\nmodel = \"tiny-model\"\n",
    )
    .expect("write config");
    assert_eq!(
        declared_data_dir(&file),
        None,
        "a file that never mentions data_dir cannot be the thing that pinned it"
    );
    // The trap: the merged default DOES have a directory in it. Asking the struct instead of the file is what erased F37's note.
    let merged = Config::default();
    assert!(
        !merged.data_dir.as_os_str().is_empty(),
        "the default resolves an OS directory, which is exactly why reading it back as a declaration was wrong"
    );

    // And the honest case still works: name the key, get it back verbatim.
    let pinned = tmp.path().join("chosen");
    fs::write(
        &file,
        format!("data_dir = \"{}\"\n", pinned.display()).as_bytes(),
    )
    .expect("write config");
    assert_eq!(
        declared_data_dir(&file),
        Some(pinned),
        "an instruction is an instruction, and this one survives"
    );

    // Unreadable or absent names nothing rather than erroring: `load` has already parsed the file strictly by now.
    assert_eq!(declared_data_dir(&tmp.path().join("absent.toml")), None);
}

/// **The end-to-end version, through `Config::load` itself.** The case above checks the two halves; this one checks that they are joined.
/// It drives the **Windows** shape on Linux (`load_with_platform`, the seam F37's own "a value, not a `cfg`" argument implies all the way
/// up to the entry point) with `%LOCALAPPDATA%` holding the pre-F37 layout and a config file that never mentions `data_dir`.
///
/// Why it can fail on Linux at all: under the shipped bug, `explicit` came from the merged struct, which on this host resolves through the
/// *unix* shape to `$XDG_DATA_HOME` — a directory with no legacy index in it. So the buggy wiring answers "the operator pinned XDG, no
/// fallback", while the fixed one answers "we are reading `%LOCALAPPDATA%` and must say so". Restore the merged-struct question and this
/// case goes red with the exact field-run shape: an upgraded Windows vault read from a place nobody announced.
#[test]
fn loading_a_windows_upgrade_reaches_the_fallback_and_flags_it() {
    let mut env = TestEnv::install();
    // A `%LOCALAPPDATA%` of its own, holding yesterday's layout — deliberately NOT the data dir TestEnv installs, so a run that ignored
    // the fallback lands somewhere visibly different instead of coincidentally right.
    let local = TempDir::new().expect("test setup: %LOCALAPPDATA%");
    fs::create_dir(local.path().join(WORKSPACES_DIR)).expect("legacy workspaces dir");
    let legacy_local = local.path().to_path_buf();
    let spelled = legacy_local
        .to_str()
        .expect("tempdir path is utf-8")
        .to_owned();
    env.set_var("LOCALAPPDATA", Some(&spelled));

    let cfgdir = TempDir::new().expect("test setup: config dir");
    let file = cfgdir.path().join("config.toml");
    fs::write(&file, "[search]\nmode = \"keyword\"\n").expect("write config");

    let cfg = Config::load_with_platform(Some(&file), DataDirPlatform::Windows).expect("load");
    assert_eq!(
        cfg.data_dir, legacy_local,
        "the run has to read AND write the old directory — splitting the registry from its databases is worse than the layout F37 fixes"
    );
    assert_eq!(
        cfg.legacy_data_dir.as_deref(),
        Some(legacy_local.as_path()),
        "and it has to know it fell back: `None` here is what silenced the stderr note on every upgraded Windows machine, measured 2026-11-08"
    );

    // Once the nested folder holds our registry, the same load stops falling back — and stops having anything to announce.
    let nested = legacy_local.join(WINDOWS_DATA_SUBDIR);
    fs::create_dir(&nested).expect("nested dir");
    fs::write(nested.join(REGISTRY_FILE), b"").expect("nested registry");
    let cfg = Config::load_with_platform(Some(&file), DataDirPlatform::Windows).expect("load");
    assert_eq!(
        cfg.data_dir, nested,
        "the new layout wins as soon as it holds our index"
    );
    assert_eq!(
        cfg.legacy_data_dir, None,
        "nothing is being read from the old place, so nothing may be announced"
    );

    // And an operator who really did write `data_dir` still outranks both halves, with a legacy index sitting right there.
    let pinned = cfgdir.path().join("chosen");
    fs::write(
        &file,
        format!("data_dir = \"{}\"\n", pinned.display()).as_bytes(),
    )
    .expect("write config");
    let cfg = Config::load_with_platform(Some(&file), DataDirPlatform::Windows).expect("load");
    assert_eq!(cfg.data_dir, pinned, "an instruction is an instruction");
    assert_eq!(
        cfg.legacy_data_dir, None,
        "and it is never treated as a location we moved"
    );
}

/// A `data_dir` written in a config file is an instruction, not a place we moved. The fallback applies to the OS default and
/// nowhere else — otherwise the program would second-guess a path chosen on purpose, which is a worse bug than the layout.
#[test]
fn an_explicit_data_dir_in_the_file_is_never_second_guessed() {
    let env = TestEnv::install();
    let explicit = env._tmp.path().join("chosen-place");
    fs::create_dir_all(&explicit).expect("test setup: explicit data dir");
    let config_dir = env._tmp.path().join("config");
    fs::create_dir_all(&config_dir).expect("test setup: config dir");
    let cfg_path = write_config(
        &config_dir,
        &format!("data_dir = {}\n", toml_str(&explicit.display().to_string())),
    );

    let cfg = Config::load(Some(&cfg_path)).expect("the file is valid");
    assert_eq!(cfg.data_dir, explicit);
    assert_eq!(
        cfg.legacy_data_dir, None,
        "an operator-chosen directory cannot be a location we moved"
    );
}

/// **The explicit-directory rule, asserted in the shape where it could actually have been broken.** The Unix-only version of
/// this claim above is weak by construction: there is no pre-F37 location on this platform, so a mutant that applied the
/// fallback to an operator's chosen directory would survive it. Here the same rule is driven through the Windows shape with a
/// `%LOCALAPPDATA%` that exists as a tempdir and genuinely holds an index — and the file's answer still wins.
#[test]
fn an_operator_chosen_directory_wins_even_when_the_legacy_index_is_sitting_there() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let local = tmp.path().join("Local");
    let chosen = tmp.path().join("chosen-by-hand");
    fs::create_dir_all(local.join(WORKSPACES_DIR)).expect("test setup: legacy index");
    fs::write(local.join(REGISTRY_FILE), b"ours, from before the nesting")
        .expect("test setup: registry");
    fs::create_dir_all(&chosen).expect("test setup: chosen dir");

    let lookup = |key: &str| (key == "LOCALAPPDATA").then(|| local.clone());

    // (a) The fallback really would fire here — asserted first, so (b) cannot pass because nothing was ever pending.
    let (dir, legacy) = resolve_data_dir(None, DataDirPlatform::Windows, &lookup);
    assert_eq!(
        dir,
        local
            .join(WINDOWS_DATA_SUBDIR)
            .parent()
            .expect("test setup: parent")
    );
    assert_eq!(
        legacy,
        Some(local.clone()),
        "the fallback is armed in this shape"
    );

    // (b) With the file's word in front of it, nothing moves and nothing is reported.
    let (dir, legacy) = resolve_data_dir(Some(&chosen), DataDirPlatform::Windows, &lookup);
    assert_eq!(dir, chosen, "an instruction is not a location we moved");
    assert_eq!(
        legacy, None,
        "and there is no fallback to report about a path nobody relocated"
    );

    // (c) And on the Unix shape the whole question does not arise.
    let lookup_unix = |key: &str| (key == "XDG_DATA_HOME").then(|| chosen.clone());
    let (dir, legacy) = resolve_data_dir(None, DataDirPlatform::Unix, &lookup_unix);
    assert_eq!(dir, chosen);
    assert_eq!(legacy, None);
}

// ---------------------------------------------------------------------------------------------
// `[log] level` / `NP_LOG` / `--log-spec`: the diagnostic level vocabulary (docs/superpowers/specs/2026-12-10-np-diagnostics-design.md).
//
// These cases are about *resolution*, not sinks: nothing here opens a file or writes a line. What they pin is the two things an
// integrator can otherwise only guess at — which words mean which depth, and which source won when three of them disagreed.
// ---------------------------------------------------------------------------------------------

/// The grammar: a bare level, or `target=level` items with an optional bare level first. Unknown targets are **accepted** (a
/// target this build does not compile still names something another build emits), and a spec of overrides alone keeps the
/// documented default rather than inventing one.
#[test]
fn diag_spec_parses_levels_and_target_overrides() {
    // Every name the published list promises parses, and prints back as itself. That is what makes `DIAG_LEVEL_NAMES` more
    // than a comment: drop a name from the parser and this row fails instead of shipping a word the documentation promised.
    for name in DIAG_LEVEL_NAMES {
        let spec = parse_diag_spec(name)
            .unwrap_or_else(|err| panic!("`{name}` is a documented level: {err}"));
        assert_eq!(
            spec.default.as_str(),
            name,
            "`{name}` prints back as itself"
        );
    }

    let spec = parse_diag_spec("info,np_core::search=trace")
        .expect("a bare level followed by one target override is the documented shape");
    assert_eq!(spec.default, Level::Info, "the bare word sets the default");
    assert_eq!(
        spec.per_target,
        vec![("np_core::search".to_string(), Level::Trace)],
        "and `target=level` items are overrides, in the order written"
    );

    let bare = parse_diag_spec("debug").expect("a bare level alone is a whole spec");
    assert_eq!(bare.default, Level::Debug);
    assert!(bare.per_target.is_empty(), "no override was written");

    let overrides_only = parse_diag_spec("np_core::db=off").expect("overrides alone are legal");
    assert_eq!(
        overrides_only.default,
        Level::Warn,
        "with no bare word the default is the documented one, not `off` and not `trace`"
    );

    let spaced = parse_diag_spec("info, np_core::fetch=off")
        .expect("a space after a comma separates items, it is not part of a name");
    assert_eq!(
        spaced.per_target,
        vec![("np_core::fetch".to_string(), Level::Off)],
        "so the target arrives without the leading space"
    );

    let repeated = parse_diag_spec("np_core::db=off,np_core::db=trace")
        .expect("a target written twice is a correction, not a contradiction");
    assert_eq!(
        repeated.per_target,
        vec![("np_core::db".to_string(), Level::Trace)],
        "the last spelling of one target wins, and it appears once"
    );

    let unknown_target = parse_diag_spec("np_other::thing=trace")
        .expect("an unknown target is not a typo in this one");
    assert_eq!(
        unknown_target.per_target,
        vec![("np_other::thing".to_string(), Level::Trace)],
        "a target another build emits must survive to that build"
    );
}

/// A value nobody can read **refuses, naming where it came from** — the `NP_LLAMA_LOG_LEVEL` rule (`loglevel.rs`), because a
/// refusal that says "edit the file" while the environment holds the bad word wastes the one thing a refusal is for.
#[test]
fn diag_spec_refuses_naming_its_source() {
    let from_env = resolve_diag(None, Some("verbose"), None)
        .expect_err("a level nobody spelled is a refusal, not a shrug to the default");
    assert!(
        from_env.contains("NP_LOG"),
        "the environment's bad word must name the environment: {from_env}"
    );
    assert!(
        !from_env.contains("--log-spec"),
        "and must not send the reader to a flag they did not type: {from_env}"
    );

    let from_flag = resolve_diag(Some("verbose"), None, None)
        .expect_err("the same text from a flag refuses too");
    assert!(
        from_flag.contains("--log-spec"),
        "a bad flag value names the flag: {from_flag}"
    );
    assert!(
        !from_flag.contains("NP_LOG"),
        "not the variable: {from_flag}"
    );

    let from_file =
        resolve_diag(None, None, Some("verbose")).expect_err("and so does a bad file value");
    assert!(
        from_file.contains("log.level"),
        "a bad file value names the key: {from_file}"
    );

    // The refusal is also the only place the six words are listed, so it has to list them.
    for name in ["off", "error", "warn", "info", "debug", "trace"] {
        assert!(
            from_env.contains(name),
            "the refusal must list {name:?} or a reader cannot fix the value: {from_env}"
        );
    }

    // An empty spec names nothing, so it is a refusal from the parser rather than a silent trip to the default.
    let empty = parse_diag_spec("").expect_err("a spec of nothing is not a spec");
    assert!(
        empty.to_string().contains("empty"),
        "and it says so: {empty}"
    );

    // A bare level belongs first and once: two of them is a contradiction the parser will not guess a winner for.
    let two_bare = resolve_diag(Some("info,debug"), None, None).expect_err("two defaults refuse");
    assert!(
        two_bare.contains("debug"),
        "naming the second one is how it gets fixed: {two_bare}"
    );

    // A bad *override* level is as refused as a bad default, and says which item was wrong.
    let bad_override = resolve_diag(Some("info,np_core::search=verbose"), None, None)
        .expect_err("an override with an unreadable level refuses too");
    assert!(
        bad_override.contains("np_core::search"),
        "naming the offending target is how a comma list gets fixed: {bad_override}"
    );
}

/// **The house rule** (§12 rule 7, pinned for llama.cpp's transcript at `loglevel.rs:160` / `cli.rs:3726`): the file wins, the
/// environment fills what the file left unset. A diagnostic hatch invents no exception, so `NP_LOG` cannot outvote a file that
/// named a level — and the caller is told which of them won, because "the environment did nothing" is otherwise invisible.
#[test]
fn diag_resolution_prefers_the_file_over_the_environment() {
    let (spec, source) =
        resolve_diag(None, Some("trace"), Some("warn")).expect("both values are readable");
    assert_eq!(
        spec.default,
        Level::Warn,
        "the file outranks the environment"
    );
    assert_eq!(source, DiagSource::Config, "and says so");

    // The environment answers when the file is silent…
    let (spec, source) = resolve_diag(None, Some("trace"), None).expect("one source is enough");
    assert_eq!(spec.default, Level::Trace);
    assert_eq!(source, DiagSource::Env);

    // …an empty environment value counts as unset rather than as a refusal (`env_text`'s rule)…
    let (spec, source) =
        resolve_diag(None, Some(""), Some("warn")).expect("an empty NP_LOG is silence");
    assert_eq!(spec.default, Level::Warn);
    assert_eq!(source, DiagSource::Config, "so the file still answers");

    // …and when nobody said anything at all the default is `warn`, which is what keeps stderr as quiet as it is today.
    let (spec, source) = resolve_diag(None, None, None).expect("no source is not an error");
    assert_eq!(spec.default, Level::Warn);
    assert_eq!(source, DiagSource::Default);

    // A bad environment value refuses **even though the file would have won**: a typo in the hatch must be visible instead of
    // silently meaning "whatever the file said" — the same reasoning as `resolve_level`.
    resolve_diag(None, Some("verbose"), Some("warn")).expect_err("a refused hatch stays refused");
}

/// The flag is the loudest voice: it outvotes both file and environment, because it is the one spelling typed for this run.
#[test]
fn diag_flag_beats_everything() {
    let (spec, source) = resolve_diag(
        Some("debug,np_core::embed=trace"),
        Some("info"),
        Some("error"),
    )
    .expect("the flag is readable");
    assert_eq!(spec.default, Level::Debug);
    assert_eq!(
        spec.per_target,
        vec![("np_core::embed".to_string(), Level::Trace)],
        "the winning source supplies the whole spec, overrides included"
    );
    assert_eq!(source, DiagSource::Flag);

    // The four words a caller prints are one per source, and `as_str` is where they live.
    assert_eq!(DiagSource::Default.as_str(), "default");
    assert_eq!(DiagSource::Config.as_str(), "config.toml");
    assert_eq!(DiagSource::Env.as_str(), "NP_LOG");
    assert_eq!(DiagSource::Flag.as_str(), "command line");
}

/// `[log]` as config vocabulary: defaulted, strict about its own keys, and with a budget whose default is a documented number
/// rather than a magic value in the sink.
#[test]
fn the_log_section_defaults_and_refuses_an_unknown_key() {
    // `[models]` reads its two environment overrides on every load, so any case that loads a config file is exposed to another
    // thread's installed values — which is what `every_case_that_loads_a_config_holds_the_environment_lock` exists to catch.
    let _env = hold_env();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let data = toml_str(&tmp.path().display().to_string());

    // Absent means "nothing asked for": no level, no file, and the budget left unset so the sink's default is one place.
    let quiet = config_with_data_dir(tmp.path());
    assert_eq!(quiet.log.level, None);
    assert_eq!(quiet.log.file, None);
    assert_eq!(quiet.log.max_bytes, None);
    assert_eq!(
        DEFAULT_LOG_MAX_BYTES,
        8 * 1024 * 1024,
        "8 MiB, as documented"
    );

    let path = write_config(
        tmp.path(),
        &format!(
            "data_dir = {data}\n[log]\nlevel = \"info\"\nfile = \"/tmp/np.jsonl\"\nmax_bytes = 4096\n"
        ),
    );
    let loud = Config::load(Some(&path)).expect("the three keys are legal");
    assert_eq!(loud.log.level.as_deref(), Some("info"));
    assert_eq!(loud.log.file, Some(PathBuf::from("/tmp/np.jsonl")));
    assert_eq!(loud.log.max_bytes, Some(4_096));

    // **F35's rule applies to this section like every other**: an invented key inside it is a refusal naming the key, not a
    // setting that silently did not arrive — and `[log]` is the section a reader is most likely to invent a key for, because
    // everybody knows what logging tools call their knobs.
    let stray = write_config(
        tmp.path(),
        &format!("data_dir = {data}\n[log]\nrotate = true\n"),
    );
    let err = Config::load(Some(&stray)).expect_err("an unknown [log] key refuses");
    let text = err.to_string();
    assert!(text.contains("rotate"), "the refusal names the key: {text}");

    // And a level that is not one of the six fails at load, naming the key — it must not become a run that logged less than it
    // was told to. `resolve_diag` refuses the same text from the environment; this is the file's half of that rule.
    let bad = write_config(
        tmp.path(),
        &format!("data_dir = {data}\n[log]\nlevel = \"verbose\"\n"),
    );
    let err = Config::load(Some(&bad)).expect_err("a level nobody spelled is not a default");
    let text = err.to_string();
    assert!(
        text.contains("log.level") && text.contains("verbose"),
        "the refusal names the key and the value: {text}"
    );
}

/// **The lenient read, and why it exists.** The sink is installed before `Config::load` runs, so a file that is about to refuse
/// still has to answer two questions — how loud, and where — or the most common integrator failure becomes the one failure with
/// no diagnostic (spec point 3). It is also what lets `-v` announce the level that will *actually* run: `[log] level` outranks
/// `NP_LOG`, so announcing from flag-and-env alone would promise one thing and do another.
#[test]
fn the_log_table_is_readable_from_a_file_that_refuses_for_another_reason() {
    // `TestEnv` because this reads the environment's config path, like every other case here that does: cargo gives a test
    // process whatever `$HOME` the developer's shell holds, and a real `~/.config` would decide what an unrelated case proves.
    let env_guard = TestEnv::install();
    fs::create_dir_all(env_guard._tmp.path().join("config").join("note-processor"))
        .expect("test setup: config dir");
    let path =
        config_file_path().expect("test setup: the installed environment names a config path");

    // A file with F35's own defect — a section name one letter off — plus a correct `[log]` table.
    fs::write(
        &path,
        "[loggg]\nlevel = \"info\"\n[log]\nlevel = \"debug\"\nfile = \"/tmp/np-diag.jsonl\"\nmax_bytes = 1024\n",
    )
    .expect("test setup: write config");
    Config::load(None).expect_err("the file still refuses, and loading is not the peek's job");

    let peek = peek_log_config();
    assert_eq!(
        peek.level.as_deref(),
        Some("debug"),
        "what it said is what it meant"
    );
    assert_eq!(
        peek.file,
        Some(PathBuf::from("/tmp/np-diag.jsonl")),
        "and the sink it asked for"
    );
    assert_eq!(peek.max_bytes, Some(1_024));

    // A file that is not TOML at all peeks as silence: this read reports, it never refuses — the strict path keeps that job.
    fs::write(&path, "this is not toml = = =").expect("test setup: write junk");
    assert_eq!(
        peek_log_config(),
        LogPeek::default(),
        "unreadable means unsaid"
    );

    // A value of the wrong type is likewise unsaid rather than a crash or a guess.
    fs::write(&path, "[log]\nmax_bytes = \"lots\"\n").expect("test setup: write typed junk");
    let peek = peek_log_config();
    assert_eq!(peek.max_bytes, None, "a non-integer budget is no budget");
    drop(env_guard);
}
