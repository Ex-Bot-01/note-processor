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
    config_file_path, CatalogConfig, Config, ConfigError, EmbeddingProvider, EmbeddingsConfig,
    MarkerConfig, ModelsConfig, ThreadCount, WorkspaceRegistry, DEFAULT_EMBEDDING_TOKENS,
    MAX_CONTINUATION_INDENT, MAX_EMBEDDING_TOKENS,
};
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
        tmp.path().join("data")
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

#[test]
fn an_unknown_key_outside_embeddings_still_loads() {
    let _env = hold_env();
    // The strictness is scoped, and this is the half that proves it: `[catalog]` and the top level keep
    // the lenient posture every existing config file relies on. Without this case, a reader of the test
    // above could not tell "one section refuses typos" from "the whole file does" — and flipping the
    // file strict would be a silent behaviour change for every installed config.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(
        tmp.path(),
        "stray_top_level = 1\n[catalog]\nmax_entries_per_file = 400\nstray = \"x\"\n",
    );

    let cfg = Config::load(Some(&path)).expect("unknown keys outside [embeddings] stay accepted");

    assert_eq!(cfg.catalog.max_entries_per_file, 400);
    assert_eq!(cfg.embeddings.provider, EmbeddingProvider::None);

    // Mutation this catches: adding `deny_unknown_fields` to `Config` or `CatalogConfig`, which would
    // refuse config files that work today.
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
    // The other half of "refuse loudly", and the half a single fixture cannot see: an id that would be refused
    // beside `local-model` is ordinary text beside `none`, because nothing will embed. Refusing it there would
    // break config files that work today — the same scoping argument as
    // `an_unknown_key_outside_embeddings_still_loads`, one level down.
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
fn the_models_strictness_is_scoped_to_the_models_section() {
    // The other half of the rule, and the half one refusal test cannot show: strictness stops at the section's
    // boundary, so an existing config file with a stray key beside it keeps loading (`an_unknown_key_outside_embeddings_still_loads`
    // is the same argument for `[embeddings]`).
    let _env = TestEnv::install();
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config_with_data_dir(
        tmp.path(),
        "stray_top_level = 1\n[catalog]\nmax_entries_per_file = 400\nstray = \"x\"\n[models]\nbase_url = \"https://mirror.example\"\n",
    );

    let cfg = Config::load(Some(&path)).expect("unknown keys outside [models] stay accepted");
    assert_eq!(cfg.catalog.max_entries_per_file, 400);
    assert_eq!(
        cfg.models.base_url.as_deref(),
        Some("https://mirror.example")
    );

    // Mutation this catches: adding `deny_unknown_fields` to `Config` or `CatalogConfig`, which would refuse config
    // files that work today.
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
    let three = std::num::NonZeroU32::new(3).expect("test setup: 3 is non-zero");
    let ninety = std::num::NonZeroU32::new(90).expect("test setup: 90 is non-zero");
    assert_eq!(ThreadCount::Workers(three).resolve(16), 3);
    assert_eq!(ThreadCount::Workers(ninety).resolve(16), 90);
    // The label the report prints distinguishes the two, so "12 because auto" is not reported as "12 because you said
    // twelve".
    assert!(ThreadCount::Auto.is_auto());
    assert!(!ThreadCount::Workers(three).is_auto());
}

/// Both accepted spellings load, and absence means `auto` rather than "unset, so four".
#[test]
fn threads_defaults_to_auto_and_takes_both_spellings() {
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
/// remember to re-check it. The 3,000,000,000 case is not hypothetical — an earlier version of this seam *clamped* such
/// a count and llama.cpp then aborted the process (`SIGABRT`, uncatchable in Rust), which is why the ceiling here is
/// `i32::MAX` and why it is enforced at the boundary.
#[test]
fn an_impossible_worker_count_is_refused_naming_the_key() {
    let tmp = tempfile::tempdir().expect("test setup: tempdir");
    let data = toml_str(&tmp.path().display().to_string());

    for (body, fragment) in [
        ("threads = 0", "embeddings.threads"),
        ("threads = -1", "embeddings.threads"),
        ("threads = 3000000000", "embeddings.threads"),
        ("threads = \"most\"", "embeddings.threads"),
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
