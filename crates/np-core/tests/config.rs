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
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use np_core::config::{
    config_file_path, CatalogConfig, Config, ConfigError, MarkerConfig, WorkspaceRegistry,
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

/// Every variable `config_file_path` and `default_data_dir` consult, on either platform.
const ENV_KEYS: [&str; 5] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "APPDATA",
    "LOCALAPPDATA",
];

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
/// previous values go back on drop. No other case in this file reads the environment — each passes
/// an explicit config path or a `data_dir` inside its own `TempDir`.
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
        Self {
            _guard: guard,
            _tmp: tmp,
            previous,
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
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config(tmp.path(), "catalog.max_entries_per_file = \"many\"\n");

    let err = Config::load(Some(&path)).expect_err("bad TOML must not load");

    assert!(
        matches!(err, ConfigError::Parse(_)),
        "expected Parse, got {err:?}"
    );
}

#[test]
fn explicit_missing_config_file_is_an_io_error() {
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

/// How long the holding connection keeps its write transaction open, in milliseconds: inside this
/// crate's busy timeout (`BUSY_TIMEOUT_MS` is 250), and short enough that a lock nobody releases fails
/// the test rather than hanging it.
const HOLD_MS: u64 = 40;
/// A hold past that timeout and still far below the 5000 ms rusqlite sets on every connection of its own
/// (`rusqlite-0.40.2/src/inner_connection.rs:118`), so a test using it measures this crate's value.
const HOLD_PAST_TIMEOUT_MS: u64 = 700;

/// Take the write lock on the registry file from a second handle — the other `np` run that WAL and the
/// busy timeout exist for. It opens after the registry exists, so no schema write races with assertions.
fn hold_registry_lock(data_dir: &Path) -> Connection {
    let holder = Connection::open(data_dir.join("workspaces.sqlite"))
        .expect("test: open the registry file again");
    holder
        .execute_batch(
            "BEGIN IMMEDIATE;
             INSERT INTO workspaces (workspace, first_seen, last_seen) VALUES ('/held/notes', 1, 1);",
        )
        .expect("test: take the write lock");
    holder
}

/// Call `record` on `registry` while `holder` holds the lock, then commit the holder. Returns what the
/// call returned and how long it took, with the registry handed back.
///
/// The waiting `record` cannot run on this thread — only this thread can release the lock it is blocked
/// on — so the registry travels to the writer and back. The writer announces itself first, so the commit
/// lands while that call is already inside SQLite's busy loop rather than after it gave up.
fn record_while_locked(
    holder: Connection,
    registry: WorkspaceRegistry,
    hold: Duration,
) -> (Result<(), ConfigError>, Duration, WorkspaceRegistry) {
    let (tx, rx) = mpsc::channel();
    let writer = thread::spawn(move || {
        tx.send(()).expect("test: the writer is about to block");
        let mut registry = registry;
        let started = Instant::now();
        let recorded = registry.record(Path::new("/waiting/notes"), 200);
        (recorded, started.elapsed(), registry)
    });

    rx.recv().expect("test: the writer reached its statement");
    thread::sleep(hold);
    holder
        .execute_batch("COMMIT")
        .expect("test: release the write lock");
    writer.join().expect("test: writer thread")
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

    let (recorded, waited, registry) =
        record_while_locked(holder, registry, Duration::from_millis(HOLD_MS));
    recorded.expect("the registry write has to wait for the lock, not fail with SQLITE_BUSY");
    assert!(
        waited >= Duration::from_millis(HOLD_MS / 2),
        "record() returned after {waited:?}, so it never waited for the held lock"
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
    // notice the registry pragma going missing. Held past our 250 ms and well inside those five seconds,
    // `record` has to come back refusing — which is what proves the value set in
    // `WorkspaceRegistry::open` is the one in force. The workspace-database equivalent is
    // `the_busy_timeout_in_force_is_this_crates_and_not_the_drivers_default` in
    // `crates/np-core/tests/db.rs`; both connections read the same constant.
    let tmp = TempDir::new().expect("test setup: tempdir");
    let registry = WorkspaceRegistry::open(tmp.path()).expect("registry should open");
    let holder = hold_registry_lock(tmp.path());

    let (recorded, waited, _registry) = record_while_locked(
        holder,
        registry,
        Duration::from_millis(HOLD_PAST_TIMEOUT_MS),
    );

    let err =
        recorded.expect_err("a lock held past the configured timeout has to be refused eventually");
    assert!(
        err.to_string().contains("database is locked"),
        "expected the busy refusal, got {err:?}"
    );
    assert!(
        waited >= Duration::from_millis(100),
        "record() gave up after {waited:?}, which is not the ~250 ms this crate asks for"
    );
    assert!(
        waited < Duration::from_secs(1),
        "record() waited {waited:?}, i.e. something nearer rusqlite's own 5 s default than our value"
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
