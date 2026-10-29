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
    MarkerConfig, WorkspaceRegistry, MAX_CONTINUATION_INDENT,
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
    // expected one of `provider`, `model`, `dim`"), so an assertion on the bare word would be satisfied
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
    // The three kebab-case spellings §2 uses, pinned from the file rather than from the enum: dropping
    // `rename_all` would silently start accepting `LocalModel` and refuse `local-model`.
    for (spelling, expected) in [
        ("none", EmbeddingProvider::None),
        ("local-model", EmbeddingProvider::LocalModel),
        ("openai-compatible", EmbeddingProvider::OpenAiCompatible),
    ] {
        let tmp = TempDir::new().expect("test setup: tempdir");
        let path = write_config_with_data_dir(
            tmp.path(),
            &format!("[embeddings]\nprovider = \"{spelling}\"\n"),
        );
        let cfg = Config::load(Some(&path))
            .unwrap_or_else(|err| panic!("\"{spelling}\" is a legal provider: {err}"));
        assert_eq!(cfg.embeddings.provider, expected, "for {spelling}");
        // Unread by production code in Round 3a, but they must arrive rather than be invented.
        assert_eq!(cfg.embeddings.model, None, "absent model stays absent");
        assert_eq!(cfg.embeddings.dim, None, "absent dim stays absent");
    }

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

    // Mutations this catches, all three observed: dropping `#[serde(rename_all = "kebab-case")]`
    // ("local-model" then fails to parse); dropping the hand-written `openai-compatible` rename, which
    // leaves serde's own kebab-casing in charge and refuses §2's spelling with "expected one of `none`,
    // `local-model`, `open-ai-compatible`"; and dropping the container `default`, which makes a partial
    // section error on its missing sibling key.
}

#[test]
fn an_unknown_key_outside_embeddings_still_loads() {
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
