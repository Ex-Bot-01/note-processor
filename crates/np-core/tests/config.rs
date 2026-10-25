//! Config file, data dir and the known-workspace registry.
//!
//! Nothing here reads or writes the real user config/data dir: every case that
//! could create files passes an explicit config path or a `data_dir` inside a
//! `tempfile::TempDir`, and no test sets process environment variables.

use std::fs;
use std::path::{Path, PathBuf};

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
    let tmp = TempDir::new().expect("test setup: tempdir");
    let path = write_config(tmp.path(), "catalog.max_entries_per_file = 50\n");

    let cfg = Config::load(Some(&path)).expect("config with one key should load");

    assert_eq!(cfg.catalog.max_entries_per_file, 50);
    assert_eq!(cfg.catalog.continuation_indent, 2);
    assert_eq!(cfg.markers, MarkerConfig::default());
}

#[test]
fn empty_config_file_keeps_defaults() {
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
    // `load(None)` reads this machine's config path; only assert when nothing is
    // there, so the test never depends on (or touches) a real user config.
    let Some(path) = config_file_path() else {
        eprintln!("skipping: this environment names no config directory");
        return;
    };
    if path.exists() {
        eprintln!("skipping: real config file at {}", path.display());
        return;
    }

    let cfg = Config::load(None).expect("a missing default config file is not an error");

    assert_eq!(cfg.catalog, CatalogConfig::default());
    assert_eq!(cfg.markers, MarkerConfig::default());
}

#[test]
fn config_file_path_points_at_note_processor_config() {
    let Some(path) = config_file_path() else {
        eprintln!(
            "skipping: this environment names no config directory, so there is no path to check"
        );
        return;
    };
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
