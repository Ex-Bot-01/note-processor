//! The `np` binary end to end: real process, real exit codes, real `INDEX.md`.
//!
//! These cases drive the compiled binary through `CARGO_BIN_EXE_np` rather than calling into
//! `np_core`, because what the CLI is responsible for is exactly the part a library test cannot
//! reach: argument parsing, the JSON shape it prints, and the exit code. The pipeline itself is
//! covered by `crates/np-core/tests/scan.rs`.
//!
//! Each case works on a **copy** of `tests/fixtures/corpus` in a tempdir with its own `data_dir`,
//! so no run writes a catalog into the committed corpus or a database into a real user data dir.
//!
//! The refresh receipt is asserted here too, because it is written to the child's own stderr after the
//! transaction commits: no in-process test can capture that descriptor, and no other test notices when it
//! stops being printed.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Stdio;
use std::process::{Command, Output};

use np_core::config::Config;
use np_core::db::{Db, FK_ORPHANS_KEY, NOTE_MERGE_KEY};
use np_core::parse::PARSE_VERSION;
use tempfile::TempDir;

/// Markdown notes in the committed corpus (everything but `scan.png`).
const CORPUS_NOTES: u64 = 16;
/// Tracked files of every kind: the notes plus one image.
const CORPUS_FILES: u64 = CORPUS_NOTES + 1;

/// A workspace copy whose data dir lives in the same tempdir.
/// Undoing **migration 11** where a fixture rewinds `schema_version` past it, or the next open replays that entry onto a
/// schema that still holds the keyword index and fails on `table notes_fts already exists`. Copies of this live in
/// `crates/np-core/tests/{db,scan}.rs` for the same reason; all three follow `db::MIGRATIONS` as the thing to keep up with.
const REWIND_MIGRATION_11: &str =
    "DROP TRIGGER notes_fts_ai; DROP TRIGGER notes_fts_ad; DROP TRIGGER notes_fts_au; \
                                   DROP TABLE notes_fts;";

struct Workspace {
    /// Kept alive so both directories outlive the runs against them.
    _tmp: TempDir,
    root: PathBuf,
}

impl Workspace {
    /// `np` pointed at this workspace, with every directory it reads configuration from set to this
    /// tempdir.
    ///
    /// Every directory the binary can read configuration from points into this tempdir, on every
    /// platform: `XDG_DATA_HOME` and `%LOCALAPPDATA%` for the data dir, `XDG_CONFIG_HOME`, `HOME` and
    /// `%APPDATA%` for the config file. Windows takes its config dir from `%APPDATA%` and ignores the XDG
    /// variables entirely (see `np_core::config::config_file_path`), so without redirecting it a
    /// developer's own `config.toml` would change what these assertions see.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = self.command_without_operand(args);
        cmd.arg(&self.root);
        cmd
    }

    /// The same hermetic `np` with **nothing appended**, for the cases that are about argv itself.
    ///
    /// A case asserting `np embed` with no operand fails would still be hermetic without these five variables —
    /// clap refuses before configuration is read — but it would then depend on that ordering staying true, and a
    /// developer's own `config.toml` becoming part of an argv test is exactly the coupling [`Workspace::command`]
    /// exists to remove.
    fn command_without_operand(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
        cmd.env("XDG_DATA_HOME", self.data_dir())
            .env("LOCALAPPDATA", self.data_dir())
            .env("HOME", self._tmp.path())
            .env("XDG_CONFIG_HOME", self.config_dir())
            .env("APPDATA", self.config_dir());
        cmd.args(args);
        cmd
    }

    /// The empty directory standing in for the OS config dir.
    fn config_dir(&self) -> PathBuf {
        self._tmp.path().join("config")
    }

    /// A user-level `config.toml`, written where the binary actually looks.
    ///
    /// `config_file_path` joins `note-processor/config.toml` under the OS config dir, and `Workspace::command`
    /// redirects that dir at this tempdir through `XDG_CONFIG_HOME` on Unix and `%APPDATA%` on Windows. Writing a file
    /// straight into `config_dir()` would leave it invisible to the process — the scan would succeed and the test would
    /// pass without having configured anything, which is the failure mode worth naming here.
    fn write_user_config(&self, body: &str) {
        let dir = self.config_dir().join("note-processor");
        fs::create_dir_all(&dir).expect("test setup: config package dir");
        fs::write(dir.join("config.toml"), body).expect("test setup: write user config");
    }

    fn data_dir(&self) -> PathBuf {
        self._tmp.path().join("data")
    }

    fn index(&self) -> PathBuf {
        self.root.join("INDEX.md")
    }

    /// This workspace's database, derived the way the binary derives it from the same absolute root
    /// [`Workspace::command`] passes — the same route [`record_parse_version`] takes.
    fn db_path(&self) -> PathBuf {
        Config {
            data_dir: self.data_dir(),
            ..Config::default()
        }
        .workspace_db(&self.root)
    }

    /// This workspace's artifact directory, **derived** the way the binary derives it rather than spelled out —
    /// the reason `crates/np-cli/tests/models_cli.rs` gives for the same rule, and what makes an assertion about
    /// the path `np embed` names mean something.
    fn models_dir(&self) -> PathBuf {
        Config {
            data_dir: self.data_dir(),
            ..Config::default()
        }
        .workspace_models_dir(&self.root)
    }
}

/// A fresh copy of the corpus in a tempdir.
fn workspace() -> Workspace {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("ws");
    let source = corpus();
    assert_no_generated_catalog(&source);
    copy_tree(&source, &root);
    fs::create_dir_all(tmp.path().join("config")).expect("test setup: config dir");
    Workspace { _tmp: tmp, root }
}

/// Fail loudly when the source corpus holds a generated catalog of its own.
///
/// [`copy_tree`] copies whatever the source directory holds, so an `INDEX.md` left behind by the brief's
/// hand-verification run (`np scan tests/fixtures/corpus`) arrives in every copy and turns `added 17`
/// into 18. `.gitignore` keeps such a file out of a commit; this keeps it out of a red herring.
fn assert_no_generated_catalog(corpus: &Path) {
    let leftovers: Vec<String> = fs::read_dir(corpus)
        .expect("test setup: read source corpus")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with("INDEX") && (name.ends_with(".md") || name.ends_with(".tmp"))
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "`{}` holds generated catalog files: {leftovers:?}",
        corpus.display()
    );
}

/// The committed fixture corpus, next to the repository's `Cargo.toml`.
fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("np-cli sits two levels under the workspace root")
        .join("tests")
        .join("fixtures")
        .join("corpus")
}

/// `from` copied to `to`, recursively, byte for byte.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("test setup: create copy dir");
    for entry in fs::read_dir(from).expect("test setup: read source dir") {
        let entry = entry.expect("test setup: readdir entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("test setup: file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("test setup: copy fixture file");
        }
    }
}

/// A finished run, with the command spelled out in every failure message.
struct Run {
    output: Output,
    argv: String,
}

impl Run {
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    fn code(&self) -> i32 {
        self.output
            .status
            .code()
            .unwrap_or_else(|| panic!("`{}` was killed by a signal", self.argv))
    }

    fn assert_success(&self) {
        assert_eq!(
            self.code(),
            0,
            "`{}` failed: stdout `{}`, stderr `{}`",
            self.argv,
            self.stdout(),
            self.stderr()
        );
    }
}

/// Run `np` with `args`, returning its output; the workspace root is appended last.
fn run(cmd: &mut Command) -> Run {
    let argv = format!("{cmd:?}");
    Run {
        output: cmd
            .output()
            .unwrap_or_else(|err| panic!("cannot start `{argv}`: {err}")),
        argv,
    }
}

#[test]
fn scan_writes_the_catalog_and_reports_it() {
    let ws = workspace();

    let first = run(&mut ws.command(&["scan"]));

    first.assert_success();
    assert!(ws.index().is_file(), "`np scan` must leave INDEX.md");
    let text = first.stdout();
    for expected in [
        "added      17",
        &format!("notes      {CORPUS_NOTES}"),
        "index      INDEX.md",
    ] {
        assert!(
            text.contains(expected),
            "`{expected}` missing from:\n{text}"
        );
    }
    // The corpus holds two `gamma.md` files, so at least one link cannot be placed; the summary
    // has to say what to do about it rather than print a bare number.
    assert!(
        text.contains("spell its path"),
        "unresolved links need the ambiguity hint:\n{text}"
    );
    // ... and name them. `Gamma` is the ambiguous basename the corpus is written to produce, so a
    // summary that prints only the count leaves the reader with nothing to fix.
    assert!(
        text.contains("  Gamma"),
        "the unresolved targets have to be listed by name:\n{text}"
    );
}

#[test]
fn json_output_matches_the_fixture_corpus() {
    let ws = workspace();

    let report: serde_json::Value =
        serde_json::from_str(&run(&mut ws.command(&["scan", "--json"])).stdout())
            .expect("`--json` must print one JSON object");

    assert_eq!(report["added"], CORPUS_FILES);
    assert_eq!(report["modified"], 0);
    assert_eq!(report["moved"], 0);
    assert_eq!(report["removed"], 0);
    assert_eq!(report["unchanged"], 0);
    assert_eq!(report["notes"], CORPUS_NOTES);
    assert_eq!(report["skipped"].as_array().map(Vec::len), Some(0));
    assert_eq!(report["index_files"][0], "INDEX.md");
    assert!(
        report["unresolved_links"].as_u64().unwrap_or(0) > 0,
        "the corpus is written to leave ambiguous targets: {report}"
    );
    // The names behind that count are part of the JSON contract too, not only of the printed summary:
    // `Gamma` is the ambiguous basename the corpus exists to produce, and an attachment target must not
    // appear. Pinned as a whole array, so renaming the field or changing its shape — a map, an object,
    // a nested list — fails here rather than in a downstream reader.
    let targets: Vec<&str> = report["unresolved_targets"]
        .as_array()
        .unwrap_or_else(|| panic!("`unresolved_targets` must serialise as an array: {report}"))
        .iter()
        .map(|target| {
            target
                .as_str()
                .unwrap_or_else(|| panic!("every unresolved target is a name: {report}"))
        })
        .collect();
    assert_eq!(
        targets,
        vec!["Gamma"],
        "the JSON has to name the note behind the count: {report}"
    );
    assert!(
        !targets.contains(&"scan.png"),
        "an attachment is not a missing note: {report}"
    );
    assert!(ws.index().is_file(), "`--json` still writes the catalog");
}

#[test]
fn rescan_is_a_no_op_and_exits_zero() {
    let ws = workspace();
    run(&mut ws.command(&["scan", "--json"])).assert_success();

    let again: serde_json::Value =
        serde_json::from_str(&run(&mut ws.command(&["scan", "--json"])).stdout())
            .expect("`--json` must print one JSON object");

    assert_eq!(again["added"], 0);
    assert_eq!(again["modified"], 0);
    assert_eq!(again["moved"], 0);
    assert_eq!(again["removed"], 0);
    assert_eq!(again["unchanged"], CORPUS_FILES);
    assert_eq!(again["notes"], CORPUS_NOTES);
}

/// `np scan <root>` with every place the binary can find a data dir or a config file **cleared**, so
/// configuration loading has nothing to resolve and the run is on the CLI's error path.
///
/// Cleared rather than redirected: on Windows the data dir comes from `%LOCALAPPDATA%` and the config dir
/// from `%APPDATA%`, so an unset XDG pair alone would still find a developer's real directories.
fn scan_without_a_data_dir(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
    cmd.env("XDG_DATA_HOME", "")
        .env("LOCALAPPDATA", "")
        .env("HOME", "")
        .env("XDG_CONFIG_HOME", "")
        .env("APPDATA", "")
        .arg("scan")
        .arg(root);
    cmd
}

#[test]
fn missing_data_dir_is_a_configuration_error_not_a_panic() {
    let ws = workspace();
    // No data dir from the environment and no config file to supply one: every path the program
    // would derive would land next to the current directory, so it must refuse instead.
    let failed = run(&mut scan_without_a_data_dir(&ws.root));

    assert_eq!(
        failed.code(),
        1,
        "an unusable data dir must exit 1, got:\n{}",
        failed.stdout()
    );
    let message = format!("{} {}", failed.stdout(), failed.stderr());
    assert!(
        message.contains("data_dir"),
        "the error must name the missing config value: {message}"
    );
    assert!(
        !message.contains("panicked") && !message.contains("workspaces/"),
        "a configuration refusal is not a panic and never a relative path: {message}"
    );
    assert!(
        !ws.root.join("workspaces").exists(),
        "a refused scan writes nothing into or beside the workspace"
    );
}

#[test]
fn version_reports_the_np_binary_name() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
    cmd.arg("--version");

    let version = run(&mut cmd);

    version.assert_success();
    assert!(
        version.stdout().starts_with("np "),
        "`np --version` must name the binary `np`: {}",
        version.stdout()
    );
}

// ---------------------------------------------------------------------------
// The refresh receipt: the only place a person learns that a parse-rule change reached an
// existing workspace, and the only thing this program writes to stderr on a successful scan.
//
// Both lines are printed *after* the transaction commits (`np_core::scan`), so no other test in
// this suite notices when they go: deleting both calls leaves every count green. And their sink is
// the child's own descriptor 2, which an in-process test can neither capture nor replace — that is
// exactly why `report_refresh` takes a writer parameter and its unit tests cannot pin the call site.
// So these cases drive the real binary and read what came back on stderr.
// ---------------------------------------------------------------------------

/// A stored rules version this build must treat as out of date, without assuming [`PARSE_VERSION`] is
/// nonzero — resetting that constant would underflow this into "up to date".
///
/// From a scan's point of view a lagging stored number is exactly what bumping the constant does: the
/// comparison at scan start is between that number and the constant, and nothing else.
fn outdated_parse_version() -> u32 {
    PARSE_VERSION.checked_sub(1).unwrap_or(PARSE_VERSION + 1)
}

/// Record `version` as the rules this workspace's rows were written under, through the same database path
/// the binary opens: [`Config::workspace_db`] keys on the absolute root, and [`Workspace::command`] passes
/// exactly that path.
fn record_parse_version(ws: &Workspace, version: u32) {
    let cfg = Config {
        data_dir: ws.data_dir(),
        ..Config::default()
    };
    let db =
        Db::open(&cfg.workspace_db(&ws.root)).expect("test setup: open the workspace database");
    let updated =
        db.0.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            (np_core::scan::PARSE_VERSION_KEY, version.to_string()),
        )
        .expect("test setup: record the lagging rules version");
    assert_eq!(updated, 1, "test setup: exactly one label row moved");
}

/// The `skipped` array of a `--json` report, for a premise that has to hold before an assertion about it
/// means anything.
#[cfg(unix)]
fn skipped(report: &serde_json::Value) -> Vec<String> {
    report["skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("`skipped` must serialise as an array: {report}"))
        .iter()
        .map(|path| {
            path.as_str()
                .unwrap_or_else(|| panic!("every skipped entry is a path: {report}"))
                .to_string()
        })
        .collect()
}

/// Write the chunker version this workspace recorded, the way an older build would have left it.
#[cfg(unix)] // the only caller is the unix-only gap case; ungated, these are dead code on Windows
fn record_chunk_version(ws: &Workspace, version: i64) {
    let cfg = Config {
        data_dir: ws.data_dir(),
        ..Config::default()
    };
    let db =
        Db::open(&cfg.workspace_db(&ws.root)).expect("test setup: open the workspace database");
    db.0.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        (np_core::scan::EMBED_VERSION_KEY, version.to_string()),
    )
    .expect("test setup: record the chunker version");
}

/// The recorded chunker version, or `None` when the workspace never recorded one.
#[cfg(unix)] // the only caller is the unix-only gap case; ungated, these are dead code on Windows
fn recorded_chunk_version(ws: &Workspace) -> Option<i64> {
    let cfg = Config {
        data_dir: ws.data_dir(),
        ..Config::default()
    };
    let db = Db::open(&cfg.workspace_db(&ws.root)).expect("test: open the workspace database");
    db.0.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        [np_core::scan::EMBED_VERSION_KEY],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|value| value.trim().parse().ok())
}

/// The note one case makes unreadable. Any corpus note will do; this one holds a heading, so it certainly
/// owns derived rows that a refresh owes.
#[cfg(unix)]
const BLOCKED_NOTE: &str = "sprint-review.md";

/// Strip every permission bit from `path` and put whatever it had back on the way out, so a failing
/// assertion cannot leave a tempdir note unreadable for the code that runs after it in the same process.
#[cfg(unix)]
fn deny_read(path: &Path) -> ModeRestore {
    let mode = fs::metadata(path)
        .expect("test setup: read the permission bits")
        .permissions()
        .mode()
        & 0o7777; // permission bits only; the file-type bits are not ours to set
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).expect("test setup: chmod 000");
    ModeRestore {
        path: path.to_path_buf(),
        mode,
    }
}

/// Restores a captured permission mode on drop.
#[cfg(unix)]
struct ModeRestore {
    path: PathBuf,
    mode: u32,
}

#[cfg(unix)]
impl Drop for ModeRestore {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode));
    }
}

/// (a) The receipt: a rules change reaching an existing workspace re-parses every unchanged note, and the
/// report itself still says `modified 0` because classification decided nothing. Without this line that
/// work is invisible — a scan that rewrote every derived row in the workspace prints an empty summary.
#[test]
fn a_parse_rule_change_reports_the_re_parse_on_stderr() {
    let ws = workspace();
    run(&mut ws.command(&["scan", "--json"])).assert_success();
    record_parse_version(&ws, outdated_parse_version());

    let refreshed = run(&mut ws.command(&["scan"]));

    refreshed.assert_success();
    // Pinned word for word: the text is the product here, not just the fact that something was printed.
    let expected = format!(
        "note-processor: re-parsed {CORPUS_NOTES} unchanged path(s) for parse rules v{PARSE_VERSION} (database held v{})",
        outdated_parse_version()
    );
    let stderr = refreshed.stderr();
    assert!(
        stderr.contains(&expected),
        "`{expected}` missing from:\n{stderr}"
    );
    // The counts stay the classification truth, and stdout stays what `--json` promises.
    assert!(
        refreshed.stdout().contains("modified   0"),
        "the receipt must not move the report's own counts:\n{}",
        refreshed.stdout()
    );
    assert!(
        !stderr.contains("not recorded"),
        "a refresh that covered the corpus must not also claim a gap:\n{stderr}"
    );
}

/// The chunker's own gap line, asserted the way its sibling above asserts the parse one. Recorded because
/// design-m2-models-install §8 claimed no capture seam exists for it — that was false: this file already captures a
/// child's stderr and matches its sibling's sentence verbatim.
#[cfg(unix)]
#[test]
fn an_incomplete_chunk_pass_reports_its_own_gap_on_stderr() {
    use np_core::chunk::CHUNKER_VERSION;

    let ws = workspace();
    run(&mut ws.command(&["scan", "--json"])).assert_success();
    // Parse current, chunker stamp not: a chunker-only bump is exactly the case where the parse receipt prints
    // nothing at all and the chunk gap would otherwise be silent.
    record_chunk_version(&ws, 0);
    let _restore = deny_read(&ws.root.join(BLOCKED_NOTE));

    let incomplete = run(&mut ws.command(&["scan", "--json"]));
    incomplete.assert_success();
    let expected = format!(
        "note-processor: chunker v{CHUNKER_VERSION} not recorded — 1 note(s) were not cut this scan; the next scan will retry"
    );
    let stderr = incomplete.stderr();
    assert!(
        stderr.contains(&expected),
        "`{expected}` missing from:\n{stderr}"
    );

    drop(_restore);
    let settled = run(&mut ws.command(&["scan", "--json"]));
    settled.assert_success();
    assert!(
        !settled.stderr().contains("chunker v"),
        "with the note readable again the version records and the line goes quiet:\n{}",
        settled.stderr()
    );
    assert_eq!(
        recorded_chunk_version(&ws),
        Some(CHUNKER_VERSION),
        "the workspace records this build's chunker once the pass completes"
    );
}

/// (b) The other half: a refresh that could not reach one note must say it did not record the version, or
/// a workspace with one unreadable subtree re-parses its whole corpus on every scan in silence.
#[cfg(unix)]
#[test]
fn an_incomplete_refresh_reports_the_gap_on_stderr() {
    let ws = workspace();
    run(&mut ws.command(&["scan", "--json"])).assert_success();
    record_parse_version(&ws, outdated_parse_version());
    let _restore = deny_read(&ws.root.join(BLOCKED_NOTE));

    let incomplete = run(&mut ws.command(&["scan", "--json"]));

    incomplete.assert_success();
    // Premise first: on a runner that reads a 0o000 file anyway (a privileged user, say) this fixture
    // proves nothing, and that belongs here rather than in a confusing red further down.
    let report: serde_json::Value =
        serde_json::from_str(&incomplete.stdout()).expect("`--json` must print one JSON object");
    assert!(
        skipped(&report).iter().any(|path| path == BLOCKED_NOTE),
        "the unreadable note has to be named in the report, or nothing was missed: {report}"
    );

    let expected = format!(
        "note-processor: parse rules v{PARSE_VERSION} not recorded — 1 note(s) were not re-parsed this scan; the next scan will retry"
    );
    let stderr = incomplete.stderr();
    assert!(
        stderr.contains(&expected),
        "`{expected}` missing from:\n{stderr}"
    );

    drop(_restore);
    // And it is one-shot: with the note readable again the refresh completes, records, and goes quiet.
    let settled = run(&mut ws.command(&["scan", "--json"]));
    settled.assert_success();
    assert!(
        !settled.stderr().contains("not recorded"),
        "a covering refresh must stop printing the gap:\n{}",
        settled.stderr()
    );
}

/// (c) The receipt is a courtesy, not a result. Both lines are written after the transaction has committed,
/// so a stderr that refuses writes — a detached terminal, a closed or unusable descriptor 2 — must not turn
/// a scan whose work is already on disk into a failure or a panic.
///
/// Two fixtures for "unusable stderr" were tried and rejected because **neither can fail**, which is the
/// hazard in testing a sink: `/dev/null` accepts every write, and so does a descriptor opened read-only and
/// handed over as descriptor 2 — Rust's stderr handle reports `Ok` for writes to it instead of the `EBADF`
/// the OS answers. `/dev/full` is what actually refuses, replying `ENOSPC` to every byte, and the premise is
/// asserted below so a platform that answered differently would say so here rather than pass vacuously.
/// Linux-only for that reason: nothing portable refuses a write on descriptor 2.
///
/// Both receipts are put in front of it at once — one unreadable note makes the refresh incomplete, so the
/// count line and the gap line are both due — which is what keeps this from passing with only one of the two
/// write sites made non-fatal. That "both are due" is itself a premise, so it is asserted below as well: on a
/// runner that reads a 0o000 file anyway (a privileged user) the refresh completes, only the count line is
/// ever written, and one non-fatal write site would be enough to pass.
///
/// This case deliberately does **not** go through [`run`]: that helper captures stdout *and* stderr so the
/// other cases can assert on them, and a test whose subject is descriptor 2 has to place the descriptor
/// itself and read back only the exit status.
#[cfg(target_os = "linux")]
#[test]
fn a_scan_survives_a_stderr_that_refuses_every_write() {
    let ws = workspace();
    run(&mut ws.command(&["scan", "--json"])).assert_success();
    record_parse_version(&ws, outdated_parse_version());
    let _restore = deny_read(&ws.root.join(BLOCKED_NOTE));

    // Premise first: the descriptor has to refuse the write, or there is nothing here to survive.
    if fs::write("/dev/full", "receipt").is_ok() {
        panic!("test premise: /dev/full accepted a write, so this case would prove nothing");
    }

    let mut cmd = ws.command(&["scan"]);
    let argv = format!("{cmd:?}");
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .expect("test setup: open /dev/full for writing"),
        ))
        .spawn()
        .unwrap_or_else(|err| panic!("cannot start `{argv}`: {err}"))
        .wait_with_output()
        .unwrap_or_else(|err| panic!("cannot wait for `{argv}`: {err}"));
    let survived = Run { argv, output };

    // What this pins is exit 101 — a panic whose own message cannot be printed either — and exit 1 would be
    // just as wrong: by the time the receipt is due the transaction has committed and the catalog is written.
    assert_eq!(
        survived.code(),
        0,
        "a scan must not fail because stderr refused its receipt: stdout `{}`, stderr `{}`",
        survived.stdout(),
        survived.stderr()
    );
    // The second premise, from the other end: the gap line was actually owed. The plain summary prints the
    // skipped paths on stdout, so a run that missed nothing — a privileged runner reading the 0o000 note —
    // fails here instead of passing with only the count line in front of the refusing descriptor.
    assert!(
        survived.stdout().contains(BLOCKED_NOTE),
        "the gap line was never due: nothing was missed:\n{}",
        survived.stdout()
    );
    // The work is committed and still reported where it can be read.
    assert!(
        survived.stdout().contains("index      INDEX.md"),
        "the summary still reaches stdout:\n{}",
        survived.stdout()
    );
    assert!(ws.index().is_file(), "the catalog is still written");
}

/// (d) The same refusing descriptor one layer up: the CLI's own error message. `np_core` may decline to
/// print anything, but a failure that reaches `main` still has to leave through exit code 1 — with
/// `eprintln!` it panicked instead, so the process died as 101 with no message about the configuration
/// error it had already worked out. Linux-only for the reason given on the receipt case above.
#[cfg(target_os = "linux")]
#[test]
fn an_error_is_reported_rather_than_panicked_when_stderr_refuses_it() {
    let ws = workspace();
    // Premise: the descriptor has to refuse, or there is nothing here to survive.
    if fs::write("/dev/full", "error").is_ok() {
        panic!("test premise: /dev/full accepted a write, so this case would prove nothing");
    }

    // Spawned directly rather than through [`run`]: `Command::output` replaces the inherited stdio with its
    // own pipes, which silently undoes the descriptor 2 this case is about and lets the test pass whatever
    // the binary does with it.
    let mut cmd = scan_without_a_data_dir(&ws.root);
    let argv = format!("{cmd:?}");
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .expect("test setup: open /dev/full for writing"),
        ))
        .spawn()
        .unwrap_or_else(|err| panic!("cannot start `{argv}`: {err}"))
        .wait_with_output()
        .unwrap_or_else(|err| panic!("cannot wait for `{argv}`: {err}"));
    let refused = Run { argv, output };

    // 101 is the answer this fixture produced before the fix — a panic inside the error handler, over a
    // refusal `missing_data_dir_is_a_configuration_error_not_a_panic` shows the CLI reports properly.
    assert_eq!(
        refused.code(),
        1,
        "a refusing stderr must cost the message, never the exit code: stdout `{}`, stderr `{}`",
        refused.stdout(),
        refused.stderr()
    );
}

// ---------------------------------------------------------------------------
// F21: an upgrade's deletions are announced from the durable receipt, not from a stream.
//
// Migration 3 wrote its orphan counts into `meta` and then called `report_orphans`, which `.expect`ed its
// way onto stderr — inside the migration's own transaction. A workspace holding orphans and opened with
// descriptor 2 refusing writes therefore had its whole upgrade rolled back, forever: exit 101, no rows
// removed, file still at version two. The receipt is what F21 makes the signal; these cases pin that it
// (a) survives a stderr that refuses every write, (b) reaches the scan that wrote it once and only once,
// on both channels and on every platform, and (c) says nothing when there was nothing to say.
//
// They need the real binary for (a) — `Db::open` runs before any in-process test could replace descriptor
// 2 — and a genuine version-two file for all three, because a freshly created database has no orphans to
// announce. So the fixture below is seeded by hand.
// ---------------------------------------------------------------------------

/// The schema as of version 2 — the first two `MIGRATIONS` entries, copied out verbatim.
///
/// The same file `crates/np-core/tests/db.rs` seeds its migration cases from: this binary drives the real
/// `np`, that one drives `Db::open`, and if the two spellings of "version 2" ever drift apart each suite
/// is upgrading a different database while both stay green. Shared as a **fixture file**, not as a library
/// helper — a function that writes old schemas would put a downgrade path in production code to serve a
/// test.
const VERSION_TWO: &str = include_str!("../../np-core/tests/fixtures/schema-v2.sql");

/// The receipt [`seed_version_two`] earns: exactly one row of each shape removed, and one destination
/// NULLed. Five rows changed, which is the total the summary line names.
const ORPHAN_RECEIPT: &str = "headings=1,tags=1,items=1,links_src=1,links_dst_null=1";

/// The receipt a cleanup that found nothing still writes — the record that the check ran, which is why it
/// is kept even when the scan report has nothing to announce.
const NOTHING_REMOVED: &str = "headings=0,tags=0,items=0,links_src=0,links_dst_null=0";

/// Write a version-two workspace database at `path` holding the five rows migration 3 repairs — a heading,
/// a tag, an item and an edge owned by note 77, plus one edge whose destination is note 88; neither id was
/// ever a note row.
///
/// Beside them sit the children of two live notes and of one note whose file went stale, so a cleanup that
/// deleted more than it should show up as a bigger receipt rather than passing unnoticed.
fn seed_version_two(path: &Path) {
    fs::create_dir_all(
        path.parent()
            .expect("test setup: the database has a parent"),
    )
    .expect("test setup: create the workspaces directory");
    let conn = rusqlite::Connection::open(path).expect("test setup: open the old file");
    conn.execute_batch(VERSION_TWO)
        .expect("test setup: version-two schema");
    conn.execute_batch(
        "INSERT INTO files (rel, case_key, abs, hash, size, mtime, state, last_seen) VALUES
            ('a.md', 'a.md', '/ws/a.md', 'ha', 1, 1, 'ok', 1),
            ('b.md', 'b.md', '/ws/b.md', 'hb', 1, 1, 'ok', 1),
            ('c.md', 'c.md', '/ws/c.md', 'hc', 1, 1, 'stale', 1);
         INSERT INTO notes (id, rel, title, title_source, encoding, word_count, body) VALUES
            (1, 'a.md', 'A', 'filename', 'utf-8', 3, 'a'),
            (2, 'b.md', 'B', 'filename', 'utf-8', 3, 'b'),
            (3, 'c.md', 'C', 'filename', 'utf-8', 3, 'c');
         INSERT INTO headings (note_id, level, text, line) VALUES
            (1, 1, 'A heading', 1), (77, 2, 'Gone', 4);
         INSERT INTO tags (note_id, tag) VALUES (1, 'keep'), (77, 'gone');
         INSERT INTO items (note_id, text, line, state, marker) VALUES
            (2, 'task', 2, 'open', '- [ ]'), (77, 'task', 5, 'open', NULL);
         INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin) VALUES
            (1, 2, 'md', 'b.md', 3, 'prose'),
            (77, NULL, 'wiki', 'Nobody', 6, 'prose'),
            (1, 88, 'wiki', 'AlsoGone', 7, 'prose');",
    )
    .expect("test setup: seed the old rows");
}

/// The single value a probe query returns, with the query in the failure message.
fn scalar<T: rusqlite::types::FromSql>(db: &Db, sql: &str) -> T {
    db.0.query_row(sql, [], |row| row.get::<_, T>(0))
        .unwrap_or_else(|err| panic!("query failed: {sql}\n{err}"))
}

/// The orphan receipt as migration 3 stored it, read back under the constant it writes under — so a
/// renamed key fails these tests instead of reading `NULL` and comparing equal to nothing.
fn receipt(db: &Db) -> String {
    scalar(
        db,
        &format!("SELECT value FROM meta WHERE key = '{FK_ORPHANS_KEY}'"),
    )
}

/// Every orphan [`seed_version_two`] planted, gone — and the legitimate rows beside it, kept. Asserted
/// after an upgrade whichever channel announced it.
fn assert_only_the_orphans_went(db: &Db) {
    for sql in [
        "SELECT COUNT(*) FROM headings WHERE note_id = 77",
        "SELECT COUNT(*) FROM tags WHERE note_id = 77",
        "SELECT COUNT(*) FROM items WHERE note_id = 77",
        "SELECT COUNT(*) FROM links WHERE src_note_id = 77",
        "SELECT COUNT(*) FROM links WHERE dst_note_id = 88",
    ] {
        assert_eq!(scalar::<i64>(db, sql), 0, "{sql}");
    }
    // A cleanup that deleted everything would satisfy the loop above; these are the rows the receipt says it
    // never touched — a live note's own tag, its resolved edge with its destination still attached, and the
    // dangling edge NULLed rather than dropped (migration 3's `ON DELETE SET NULL` shape).
    assert_eq!(
        scalar::<i64>(db, "SELECT COUNT(*) FROM tags WHERE note_id = 1"),
        1,
        "a live note's tag survived"
    );
    assert_eq!(
        scalar::<i64>(
            db,
            "SELECT COUNT(*) FROM links WHERE src_note_id = 1 AND target = 'b.md'
                AND dst_note_id IS NOT NULL"
        ),
        1,
        "the edge between two real notes kept its destination"
    );
    assert_eq!(
        scalar::<i64>(
            db,
            "SELECT COUNT(*) FROM links WHERE target = 'AlsoGone' AND dst_note_id IS NULL"
        ),
        1,
        "the dangling edge lost only its destination: it is somebody's prose"
    );
}

/// The all-zero case, which is every fresh workspace: migration 3 runs, removes nothing, and the scan says
/// nothing about it — while `meta` still records that the check happened.
///
/// Two workspaces because the field only exists on the process that did the migrating, and the two channels
/// are read differently: one run prints its summary, the other prints `--json`.
#[test]
fn an_upgrade_that_removed_nothing_is_announced_nowhere() {
    let json_ws = workspace();
    let report: serde_json::Value =
        serde_json::from_str(&run(&mut json_ws.command(&["scan", "--json"])).stdout())
            .expect("`--json` must print one JSON object");
    assert!(
        report.get("upgrade_orphans").is_some(),
        "the key is always present — one JSON shape for every scan, not one per outcome: {report}"
    );
    assert!(
        report["upgrade_orphans"].is_null(),
        "a cleanup that removed no rows has no loss to report: {report}"
    );
    // The durable record is a different question and gets a different answer: nothing was lost, and the file still
    // proves the sweep ran. `null` here would mean "this workspace never went through migration 3", which is not true of
    // a database this build created a moment ago (F21's second half).
    assert_eq!(
        report["recorded_upgrade_orphans"].as_str(),
        Some(NOTHING_REMOVED),
        "the zero receipt reaches anyone who asks, through `np scan --json` and not through `sqlite3`: {report}"
    );
    assert!(
        report["recorded_merged_notes"]
            .as_str()
            .is_some_and(|value| value.starts_with("merged=0")),
        "same for the merge check: {report}"
    );

    let plain = workspace();
    let first = run(&mut plain.command(&["scan"]));
    first.assert_success();
    assert!(
        !first.stdout().contains("orphan row(s)"),
        "`removed 0 orphan row(s)` would state a deletion that did not happen:\n{}",
        first.stdout()
    );
    // The other channel, because the durable receipt's echo goes to stderr (`db::report_orphans`) and "nowhere"
    // has to mean both streams this process writes to — stdout alone only rules out the summary line.
    assert!(
        !first.stderr().contains("orphan row(s)"),
        "a receipt echo at zero would state a deletion that did not happen:\n{}",
        first.stderr()
    );

    // The durable half is untouched by all of this: the receipt is written even at zero, because "looked
    // and found none" has to read differently from "this file never upgraded".
    let db = Db::open(&json_ws.db_path()).expect("test setup: open the upgraded database");
    assert_eq!(receipt(&db), NOTHING_REMOVED);
}

/// The stdout half of the announcement, asserted where every platform can run it: the scan whose own open
/// removed rows prints `removed N orphan row(s)` in its summary.
///
/// This is the positive assertion the `/dev/full` case below used to be the only home of. Gated on Linux,
/// Windows ran every other F21 case through `--json`, so nothing there would fail if the summary line
/// stopped being printed at all — and the summary is the channel a person actually reads.
#[test]
fn the_upgrading_scan_prints_the_orphans_it_removed() {
    let ws = workspace();
    seed_version_two(&ws.db_path());

    let first = run(&mut ws.command(&["scan"]));
    first.assert_success();
    // Total and stored counts on one line, spelled as `np-cli`'s `print_summary` writes them.
    assert!(
        first.stdout().contains("removed 5 orphan row(s)"),
        "the run that deleted five rows has to say so:\n{}",
        first.stdout()
    );
    assert!(
        first.stdout().contains(ORPHAN_RECEIPT),
        "and name the shapes as `meta` records them:\n{}",
        first.stdout()
    );
    // Once, on this channel. The stderr echo of the same sentence is deliberate (see
    // `np_core::db::report_orphans`) and lands on the other descriptor; a second copy *here* would mean the
    // summary grew a duplicate.
    let lines = first
        .stdout()
        .lines()
        .filter(|line| line.contains("orphan row(s)"))
        .count();
    assert_eq!(
        lines,
        1,
        "one announcement line per run:\n{}",
        first.stdout()
    );

    // And the line describes what actually happened: reopened through the normal path, the orphans are gone
    // and their receipt is in `meta`.
    let db = Db::open(&ws.db_path()).expect("test setup: open the upgraded database");
    assert_only_the_orphans_went(&db);
    assert_eq!(receipt(&db), ORPHAN_RECEIPT);
}

/// F21 §4.1, against a descriptor 2 that refuses every write while migration 3 runs: the upgrade still
/// completes.
///
/// The refusing descriptor here is `/dev/full`, so the premise is `ENOSPC` on an **open** descriptor — not a
/// closed one, which Rust's stdio handles report as a successful write (the premise check below is what
/// keeps that distinction honest). Linux is where `/dev/full` exists; the summary line this run also asserts
/// is pinned on every platform by `the_upgrading_scan_prints_the_orphans_it_removed`.
///
/// Before the fix this run exited 101 — `report_orphans` panicked inside the migration's transaction, which
/// rolled the deletes and the receipt back together, so the file stayed at version two and every later run
/// failed the same way. Exit 0 plus a removed-orphans line on stdout is the whole promise; the panic's own
/// message could not be printed either, so nothing else about the run was readable.
#[cfg(target_os = "linux")]
#[test]
fn a_scan_upgrades_when_stderr_refuses_writes() {
    let ws = workspace();
    seed_version_two(&ws.db_path());

    // Premise first, as `a_scan_survives_a_stderr_that_refuses_every_write` learned: `/dev/null` accepts
    // every write and so does a descriptor opened read-only, so only `/dev/full` actually refuses one. A
    // closed descriptor would refuse nothing at all, which is why this case is named for refusal.
    if fs::write("/dev/full", "receipt").is_ok() {
        panic!("test premise: /dev/full accepted a write, so this case would prove nothing");
    }

    // Not through [`run`]: that helper captures stderr, and this test's subject is descriptor 2.
    let mut cmd = ws.command(&["scan"]);
    let argv = format!("{cmd:?}");
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .expect("test setup: open /dev/full for writing"),
        ))
        .spawn()
        .unwrap_or_else(|err| panic!("cannot start `{argv}`: {err}"))
        .wait_with_output()
        .unwrap_or_else(|err| panic!("cannot wait for `{argv}`: {err}"));
    let survived = Run { argv, output };

    assert_eq!(
        survived.code(),
        0,
        "an upgrade must not be refused because stderr would not take its receipt: stdout `{}`, stderr `{}`",
        survived.stdout(),
        survived.stderr()
    );
    // The line still goes to stdout — a stream that is open here — naming the total beside the counts.
    assert!(
        survived.stdout().contains("removed 5 orphan row(s)"),
        "the run that deleted five rows has to say so:\n{}",
        survived.stdout()
    );

    // And the deletions are real, not a rolled-back attempt: reopened through the normal path — which now
    // finds nothing to migrate — the orphans are gone and their receipt is in `meta`.
    let db = Db::open(&ws.db_path()).expect("the upgraded file opens normally afterwards");
    assert_only_the_orphans_went(&db);
    assert_eq!(receipt(&db), ORPHAN_RECEIPT);
}

/// The echo is once per process, not a fixture on every report: the run whose own open removed the rows
/// carries the receipt on both channels, and no later run of the same workspace mentions it again — while
/// `meta` keeps it forever.
#[test]
fn only_the_scan_that_removed_the_orphans_announces_them() {
    let ws = workspace();
    seed_version_two(&ws.db_path());

    let upgrading = run(&mut ws.command(&["scan", "--json"]));
    upgrading.assert_success();
    // The same string `meta` holds, not a re-spelling of it: stdout and `--json` read one field, so they
    // cannot disagree about a fact F21 refuses to put on a stream alone.
    let report: serde_json::Value =
        serde_json::from_str(&upgrading.stdout()).expect("`--json` must print one JSON object");
    assert_eq!(
        report["upgrade_orphans"].as_str(),
        Some(ORPHAN_RECEIPT),
        "the upgrading scan carries the receipt it wrote: {}",
        upgrading.stdout()
    );

    let later = run(&mut ws.command(&["scan", "--json"]));
    later.assert_success();
    let again: serde_json::Value =
        serde_json::from_str(&later.stdout()).expect("`--json` must print one JSON object");
    assert!(
        again["upgrade_orphans"].is_null(),
        "a scan that migrated nothing has nothing to announce: {again}"
    );
    // **F21's Do-when, met through argv.** The announcement is once per process; the record is forever. Someone who
    // missed the run — a month late, on another machine, with the same file — asks the question of `np scan --json` and
    // gets the string `meta` holds, byte for byte, without opening a database shell.
    assert_eq!(
        again["recorded_upgrade_orphans"].as_str(),
        Some(ORPHAN_RECEIPT),
        "the receipt outlives the run that wrote it: {again}"
    );

    let printed = run(&mut ws.command(&["scan"]));
    printed.assert_success();
    assert!(
        !printed.stdout().contains("orphan row(s)"),
        "the receipt is history, not a queue; a line on every scan trains the reader to ignore it:\n{}",
        printed.stdout()
    );

    // Never cleared: the echo stays a copy of a record that outlives it. Read through the normal path,
    // which proves a later open neither rewrites nor repeats it.
    let db = Db::open(&ws.db_path()).expect("test setup: open the upgraded database");
    assert_only_the_orphans_went(&db);
    assert_eq!(receipt(&db), ORPHAN_RECEIPT);
}

// ---------------------------------------------------------------------------
// F19: the duplicate-note merge, on the channels only this binary has.
//
// `crates/np-core/tests/db.rs` pins the merge against the rows; nothing here pinned that a person running
// `np scan` is *told* about it. That is exactly the hole F21's review found for the orphan line: its only
// positive assertion about the summary sat behind `#[cfg(target_os = "linux")]`, so on Windows every case
// went through `--json` and no test would have failed had the stdout line stopped being printed at all. The
// case below is the same pin for `merge      merged=N;paths=…`, ungated, plus its `--json` half and the
// once-per-database rule.
// ---------------------------------------------------------------------------

/// The byte keys of the two planted paths, as hex literals for the `BLOB` columns (F13 §4: a TEXT bind fails
/// the `typeof(rel_bytes) = 'blob'` check rather than quietly matching).
const DUP_BYTES: &str = "X'6475702E6D64'";
/// The byte key of `third.md`, the note that links to both rows.
const THIRD_BYTES: &str = "X'74686972642E6D64'";
/// The receipt [`seed_duplicate_pair_at_version_four`] earns — and the exact string both channels must carry,
/// because `meta`, stdout and `--json` render one value rather than three computations of it (F21 §3).
const MERGE_RECEIPT: &str = "merged=1;paths=dup.md";

/// Add `dup.md` and `third.md` to a workspace copy, then leave its database holding **two note rows on
/// `dup.md`'s byte key** while claiming version four — the file migration 5 meets in the wild.
///
/// Derived rows hang off both rows and a third note links to each of them, so a merge that deleted the wrong
/// row, or forgot a shape, changes what this workspace looks like afterwards rather than passing quietly.
/// The seeding duplicates `crates/np-core/tests/db.rs`'s helper for the same reason [`VERSION_TWO`] and
/// [`seed_version_two`] are duplicated: no production API makes a pair any more, and a shared helper that
/// wrote old schemas would put a downgrade path in the library to serve a test.
///
/// Planted on the current schema rather than copied from a version-three script — migration 5 writes no DDL,
/// only row moves — so deleting its recorded version row and the receipt its step wrote while the file was
/// still empty reproduces a genuine version-four file without a third hand-copied schema to keep in step.
fn seed_duplicate_pair_at_version_four(ws: &Workspace) {
    fs::write(ws.root.join("dup.md"), "# Dup\n").expect("test setup: write dup.md");
    fs::write(ws.root.join("third.md"), "# Third\n").expect("test setup: write third.md");
    let db = Db::open(&ws.db_path()).expect("test setup: create the current schema");
    // The real absolute paths of the two files just written, so nothing downstream reads a row pointing at a
    // directory that does not exist. SQLite string literals do not treat `\` as an escape, so a Windows
    // tempdir path embeds unchanged.
    let abs = |name: &str| ws.root.join(name).to_string_lossy().into_owned();
    db.0.execute_batch(&format!(
        "INSERT INTO files (rel_bytes, case_bytes, rel, case_key, abs, hash, size, mtime, state, last_seen)
         VALUES ({DUP_BYTES}, {DUP_BYTES}, 'dup.md', 'dup.md', '{}', 'h1', 5, 1, 'ok', 9),
                ({THIRD_BYTES}, {THIRD_BYTES}, 'third.md', 'third.md', '{}', 'h2', 5, 1, 'ok', 9);
         INSERT INTO notes (id, rel_bytes, rel, title, title_source, encoding, word_count, body) VALUES
            (4,  {DUP_BYTES}, 'dup.md', 'Dup first',  'filename', 'utf-8', 1, 'first'),
            (9,  {DUP_BYTES}, 'dup.md', 'Dup second', 'filename', 'utf-8', 1, 'second'),
            (12, {THIRD_BYTES}, 'third.md', 'Third',  'filename', 'utf-8', 1, 'third');
         INSERT INTO headings (note_id, level, text, line) VALUES
            (4, 1, 'First heading', 1), (9, 2, 'Second heading', 3), (12, 1, 'Third heading', 1);
         INSERT INTO tags (note_id, tag) VALUES (4, 'first-tag'), (9, 'second-tag');
         INSERT INTO items (note_id, text, line, state, marker) VALUES
            (4, 'first task', 2, 'open', '- [ ]'), (9, 'second task', 4, 'done', '- [x]');
         INSERT INTO links (src_note_id, dst_note_id, kind, target, line, origin) VALUES
            (4, 12, 'wiki', 'Third', 3, 'prose'),
            (9, 12, 'wiki', 'Third', 5, 'prose'),
            (12, 4, 'wiki', 'Dup first', 2, 'prose'),
            (12, 9, 'wiki', 'Dup second', 3, 'prose');",
        abs("dup.md"),
        abs("third.md")
    ))
    .expect("test setup: plant two note rows on one key, their derived rows and the edges around them");
    // Undo what every entry above 4 created, migration 7's three tables included — a version-four file has no
    // chunk storage, and the next open re-applies those entries. Child table first, which matters once rows exist (with empty tables either order works) — with rows present an implicit DELETE
    // would be refused by `embeddings`' two foreign keys; these tables are still empty, so it is defence, not necessity.
    db.0.execute_batch(&format!(
        "DELETE FROM meta WHERE key = '{NOTE_MERGE_KEY}';
         DELETE FROM schema_version WHERE version > 4;
         ALTER TABLE files DROP COLUMN file_id;
         DROP TABLE embeddings;
         DROP TABLE chunks;
         DROP TABLE media_embeddings; DROP TABLE media_path; DROP TABLE media;
         DROP TABLE model_files;
         DROP TABLE models;
         {REWIND_MIGRATION_11}"
    ))
    .expect("test setup: rewind the file to version four");
    assert_eq!(
        scalar::<i64>(&db, "SELECT MAX(version) FROM schema_version"),
        4,
        "test setup: migration 5 has not run here"
    );
}

/// The [`NOTE_MERGE_KEY`] receipt as the file holds it, read under the constant it is written under so a
/// renamed key fails here rather than reading `NULL`.
fn merge_receipt(db: &Db) -> String {
    scalar(
        db,
        &format!("SELECT value FROM meta WHERE key = '{NOTE_MERGE_KEY}'"),
    )
}

/// The stdout half of F19's announcement, asserted where every platform runs it — the pin F21's review asked
/// for and this file never grew for the merge line.
///
/// Four things, in order: the upgrading run prints the line naming the merged path; `--json` carries the
/// identical string under `merged_notes`; no later scan of that workspace announces it on **either** channel
/// while `meta` keeps the receipt forever; and an upgrade with nothing to fold stays silent on stderr, which is
/// the CLI half of the zero guard in `db::report_merges`.
#[test]
fn the_upgrading_scan_prints_the_merged_pair_on_stdout() {
    // Two workspaces because the announcement belongs to the one run whose own `Db::open` did the merging: a
    // database can only be upgraded once, so whichever channel is asked first has to be answered by a file
    // that has not been asked yet.
    let ws = workspace();
    seed_duplicate_pair_at_version_four(&ws);

    let first = run(&mut ws.command(&["scan"]));
    first.assert_success();
    assert!(
        first.stdout().contains(MERGE_RECEIPT),
        "the run whose open folded a pair onto its oldest row has to name the merged path:\n{}",
        first.stdout()
    );
    // The whole line, once: label and stored value spelled as `print_summary` writes them. Asserting the
    // vector rather than counting one substring means neither a missing line nor a second copy of it passes.
    let summary = first.stdout();
    let lines: Vec<&str> = summary
        .lines()
        .filter(|line| line.contains("merged="))
        .collect();
    assert_eq!(
        lines,
        ["merge      merged=1;paths=dup.md"],
        "one announcement line per run, on the channel a person reads:\n{summary}"
    );

    // And the line describes what actually happened to this workspace: one row for the path, receipt kept.
    let db = Db::open(&ws.db_path()).expect("test setup: open the merged database");
    assert_eq!(
        scalar::<i64>(
            &db,
            &format!("SELECT COUNT(*) FROM notes WHERE rel_bytes = {DUP_BYTES}")
        ),
        1,
        "the pair is one row once the merge has run"
    );
    assert_eq!(merge_receipt(&db), MERGE_RECEIPT);

    // The other channel, on a file that still needs merging: the same string `meta` holds, not a re-spelling.
    let json_ws = workspace();
    seed_duplicate_pair_at_version_four(&json_ws);
    let upgrading = run(&mut json_ws.command(&["scan", "--json"]));
    upgrading.assert_success();
    let report: serde_json::Value =
        serde_json::from_str(&upgrading.stdout()).expect("`--json` must print one JSON object");
    assert_eq!(
        report["merged_notes"].as_str(),
        Some(MERGE_RECEIPT),
        "the upgrading scan carries the receipt it wrote: {}",
        upgrading.stdout()
    );

    // Once per database, not a fixture on every report (F21's rule, and the reason a permanent line here
    // would be a warning nobody can act on). The key stays present and null: one JSON shape for every scan.
    let later = run(&mut json_ws.command(&["scan", "--json"]));
    later.assert_success();
    let again: serde_json::Value =
        serde_json::from_str(&later.stdout()).expect("`--json` must print one JSON object");
    assert!(
        again.get("merged_notes").is_some(),
        "the key is always present, whatever the outcome: {again}"
    );
    assert!(
        again["merged_notes"].is_null(),
        "a scan that merged nothing has nothing to announce: {again}"
    );

    let printed = run(&mut json_ws.command(&["scan"]));
    printed.assert_success();
    assert!(
        !printed.stdout().contains("merged="),
        "the receipt is history, not a queue; a line on every scan trains the reader to ignore it:\n{}",
        printed.stdout()
    );
    // The other channel of the same promise: `report_merges` writes to stderr, so "never again" has to hold
    // there too — stdout alone only rules out the summary line. F21's orphan case does this for `orphan row(s)`.
    assert!(
        !printed.stderr().contains("merged="),
        "a later scan has nothing to announce on stderr either:\n{}",
        printed.stderr()
    );

    // Never cleared: the echo is a copy of a record that outlives it.
    let db = Db::open(&json_ws.db_path()).expect("test setup: open the merged database");
    assert_eq!(
        merge_receipt(&db),
        MERGE_RECEIPT,
        "the durable receipt survives every later scan"
    );

    // The zero guard, on the one run that can reach it. `report_merges` is called only by the open that actually
    // ran migration 5, so a later scan cannot catch a missing guard however often it runs: this workspace
    // upgrades now, finds no pair, writes `merged=0;paths=` to `meta`, and must print nothing. Delete the guard
    // in `report_merges` and this assertion fails beside `a_merge_that_folded_nothing_writes_no_line`.
    let quiet_ws = workspace();
    rewind_to_version_four_without_a_pair(&quiet_ws);
    let quiet = run(&mut quiet_ws.command(&["scan"]));
    quiet.assert_success();
    assert!(
        !quiet.stderr().contains("merged="),
        "`merged=0` states no work done — its record is `meta`, not stderr:\n{}",
        quiet.stderr()
    );
    let db = Db::open(&quiet_ws.db_path()).expect("test setup: open the upgraded database");
    assert_eq!(
        merge_receipt(&db),
        "merged=0;paths=",
        "silent, but still recorded: \"looked and found none\" is not \"never looked\""
    );
}

/// Leave a workspace's database claiming version four with **nothing to merge**, so the next `np scan` really
/// runs migration 5 and finds no pair — the run that has to stay quiet on stderr.
///
/// The file is unindexed, which is what makes it pair-free: `notes` holds no rows at all, so `duplicate_groups`
/// cannot answer with a group whatever the corpus on disk looks like.
fn rewind_to_version_four_without_a_pair(ws: &Workspace) {
    let db = Db::open(&ws.db_path()).expect("test setup: create the current schema");
    // Migration 7's tables go with the version number, child first — see `seed_duplicate_pair_at_version_four`.
    db.0.execute_batch(&format!(
        "DELETE FROM meta WHERE key = '{NOTE_MERGE_KEY}';
         DELETE FROM schema_version WHERE version > 4;
         ALTER TABLE files DROP COLUMN file_id;
         DROP TABLE embeddings;
         DROP TABLE chunks;
         DROP TABLE media_embeddings; DROP TABLE media_path; DROP TABLE media;
         DROP TABLE model_files;
         DROP TABLE models;
         {REWIND_MIGRATION_11}"
    ))
    .expect("test setup: rewind the file to version four");
    assert_eq!(
        scalar::<i64>(&db, "SELECT MAX(version) FROM schema_version"),
        4,
        "test setup: migration 5 has not run here"
    );
    assert_eq!(
        scalar::<i64>(&db, "SELECT COUNT(*) FROM notes"),
        0,
        "test setup: nothing for migration 5 to merge"
    );
}

/// The record line as `np`'s summary writes it: the label, then the value `meta` holds verbatim — one string
/// rendered by two channels, not a second spelling of the four fields.
#[cfg(unix)]
fn record_line(record: &str) -> String {
    format!("unindexable {record}")
}

/// F22 §5's stdout half: an unreadable directory prints **one** line naming the loss by kind, and `--json`
/// carries the same counts as an object.
///
/// This is the channel a person actually reads, so it is asserted here rather than in `np-core`: no
/// in-process test reaches the child's stdout. The line is pinned whole, not as "contains something about
/// losses", because the counts by kind are the entire product — `skipped 1` was already printed before F22
/// and said nothing a permission problem could act on.
#[cfg(unix)]
#[test]
fn an_unreadable_directory_prints_one_record_line() {
    let ws = workspace();
    // A directory, not a bare note: `unreadable` counts subtrees the walk did not enter, which is the shape
    // whose report line could not say whether it was a permission problem or an unkeyable name.
    let blocked = ws.root.join("lost");
    fs::create_dir_all(&blocked).expect("test setup: create the directory to block");
    fs::write(blocked.join("note.md"), "# Blocked Heading\n").expect("test setup: write the note");
    run(&mut ws.command(&["scan"])).assert_success();

    let _restore = deny_read(&blocked);
    // Premise off the disk, not off the report: a privileged runner reads a 0o000 directory anyway, and then
    // nothing was lost and every assertion below would be about a clean workspace.
    if fs::read_dir(&blocked).is_ok() {
        eprintln!(
            "NOT COVERAGE: 0o000 did not deny access to {} on this runner (a privileged user?), so this \
             scan lost nothing and had no line to print",
            blocked.display()
        );
        return;
    }

    let losing = run(&mut ws.command(&["scan"]));
    losing.assert_success();
    let expected = "unreadable=1;unkeyable=0;duplicates=0;ambiguous=0;names=lost";
    assert!(
        losing.stdout().contains(&record_line(expected)),
        "`{}` missing from:\n{}",
        record_line(expected),
        losing.stdout()
    );
    // Once. A second copy would mean the summary grew a duplicate of its own line, and F21's lesson is that a
    // repeated warning is a warning the reader stops seeing.
    let lines = losing
        .stdout()
        .lines()
        .filter(|line| line.starts_with("unindexable"))
        .count();
    assert_eq!(lines, 1, "one record line per run:\n{}", losing.stdout());

    // The same counts on the machine-readable channel, as an object of five fields — including `names`, so a
    // reader gets the path and not only the number. `ambiguous` is F23's: notes whose registry row could not be
    // matched to a byte spelling this scan, the same population that holds the parse version back.
    let json = run(&mut ws.command(&["scan", "--json"]));
    json.assert_success();
    let report: serde_json::Value =
        serde_json::from_str(&json.stdout()).expect("`--json` must print one JSON object");
    assert_eq!(report["unindexable"]["unreadable"], 1, "{report}");
    assert_eq!(report["unindexable"]["unkeyable"], 0, "{report}");
    assert_eq!(report["unindexable"]["duplicates"], 0, "{report}");
    assert_eq!(report["unindexable"]["ambiguous"], 0, "{report}");
    assert_eq!(
        report["unindexable"]["names"].as_array().map(Vec::len),
        Some(1),
        "the name has to survive into JSON: {report}"
    );
    assert_eq!(report["unindexable"]["names"][0], "lost", "{report}");

    // And the durable copy agrees with what was printed, which is the point of writing it at all.
    let db = Db::open(&ws.db_path()).expect("test setup: open the workspace database");
    let stored: String = scalar(
        &db,
        &format!(
            "SELECT value FROM meta WHERE key = '{}'",
            np_core::scan::UNINDEXABLE_KEY
        ),
    );
    assert_eq!(stored, expected, "one value, two channels");
}

/// The other half of one JSON shape: a scan that lost nothing prints `"unindexable": null` — the **key
/// present**, exactly like `upgrade_orphans` — and its summary prints no line.
///
/// Suppressing the key at zero would cost every reader a missing-key branch to answer a question `null`
/// already answers, and would make "nothing was lost" unreadable apart from "this build stopped reporting
/// it". The record-side half of this promise — the all-zero value overwriting an older one — is pinned in
/// `crates/np-core/tests/scan.rs`, where a test can reach `meta` directly.
#[test]
fn a_scan_that_loses_nothing_reports_the_key_as_null() {
    let ws = workspace();

    let report: serde_json::Value =
        serde_json::from_str(&run(&mut ws.command(&["scan", "--json"])).stdout())
            .expect("`--json` must print one JSON object");
    assert!(
        report.get("unindexable").is_some(),
        "the key is there on every scan whatever the outcome: {report}"
    );
    assert!(
        report["unindexable"].is_null(),
        "a workspace that lost nothing has nothing to name: {report}"
    );

    let plain = run(&mut ws.command(&["scan"]));
    plain.assert_success();
    assert!(
        !plain.stdout().contains("unindexable"),
        "a zero record would state a loss this scan did not have:\n{}",
        plain.stdout()
    );
}

/// F17's "done when": an absurd `continuation_indent` fails the command with a message naming the key, instead of
/// re-rendering the catalog one token per line — or, past a point, trying to allocate the indent. The library case in
/// `crates/np-core/tests/config.rs` proves the refusal exists; this one proves the channel a person reads carries it, which
/// is F21's rule for anything whose whole purpose is to be noticed.
#[test]
fn an_oversized_continuation_indent_fails_the_scan_and_names_the_key() {
    let ws = workspace();
    ws.write_user_config("[catalog]\ncontinuation_indent = 999\n");

    let mut cmd = ws.command(&["scan"]);
    let failed = run(&mut cmd);

    assert_eq!(
        failed.code(),
        1,
        "a config the renderer cannot honour must exit 1, got:\n{}",
        failed.stdout()
    );
    let message = format!("{} {}", failed.stdout(), failed.stderr());
    assert!(
        message.contains("catalog.continuation_indent") && message.contains("999"),
        "the refusal has to name the key and the value that broke it: {message}"
    );
    assert!(
        !message.contains("panicked"),
        "a config refusal is a message, not the arithmetic overflow an unclamped indent can reach: {message}"
    );
    assert!(
        !ws.index().exists(),
        "and nothing is written: a refused scan does not get to reflow the catalog first"
    );
}

#[test]
fn an_indent_at_the_ceiling_still_renders_the_catalog() {
    // The other side of the boundary at the process level: 16 is inside the documented range, so the same fixture
    // shape has to scan cleanly. Without this pair the refusal test would pass under a check that refused everything.
    let ws = workspace();
    ws.write_user_config("[catalog]\ncontinuation_indent = 16\n");
    // An entry long enough to wrap. The corpus alone does not give one: scanned at indent 2 and at indent 16, this
    // workspace's `INDEX.md` holds **no** continuation line at all (the M1 golden set `tests/golden/` contains exactly
    // one, in a shard), so an assertion about indented lines would be asserting over an empty set. Planting the note is what makes
    // the claim below able to fail.
    fs::write(
        ws.root.join("long-note-for-wrapping.md"),
        "# A note title long enough to push the rendered entry line past the hundred-column budget the catalog promises\n",
    )
    .expect("test setup: plant a note whose entry wraps");

    let mut cmd = ws.command(&["scan"]);
    let ok = run(&mut cmd);

    assert_eq!(
        ok.code(),
        0,
        "indent 16 is legal; refusing it would refuse the documented range:\n{}",
        ok.stderr()
    );
    let index = fs::read_to_string(ws.index()).expect("the catalog renders at the ceiling");
    // Some line has to start with sixteen spaces. The first draft asked whether every indented line was longer than
    // its trimmed self — true of any line beginning with a space, so it held even with nothing wrapped; the review
    // caught that, and this is the assertion it should have been. With the long note planted above it fails when `wrap`
    // stops indenting (verified by mutation) and passes at the ceiling.
    assert!(
        index.lines().any(|line| line.starts_with(&" ".repeat(16))),
        "indent 16 has to reach the rendered bytes, not merely exit 0:\n{index}"
    );
}

// ---------------------------------------------------------------------------
// `np embed` — §6's four arms, driven through argv.
//
// R6c1's defect D-2 is why this section exists. All nineteen embed cases lived in
// `crates/np-core/tests/embed_run.rs`, and one of them (`an_absent_artifact_names_a_command_that_exists`) drove
// `embed_run` directly — so np-cli answering a missing artifact file with a placeholder provider whose declared
// width is 0, and the run then refusing with §6's wrong-dim sentence about a row that declares 768, survived a
// green suite. That placeholder is built in `np-cli` and the false sentence is assembled there, so only argv can
// see it. What is pinned below is what this file alone can pin: the exit codes (1 for every runtime refusal, clap's
// 2 for argv), §6's two-channel rule for one fact, the `--json` key set, and that both channels carry the same
// numbers.
// ---------------------------------------------------------------------------

/// The model id every case here configures. Legal under `models::validate_id`, which
/// [`np_core::config::Config::load`] applies to `embeddings.model` before a run starts.
const EMBED_MODEL: &str = "tiny-model@1";

/// The width `install_model` declares and the row therefore holds. Configured nowhere else: `[embeddings] dim`
/// stays unset so the row's own declaration is the width this run needs, which leaves a missing artifact file as
/// the only thing left to refuse.
const EMBED_DIM: &str = "768";

/// `[embeddings]` naming `model` under `provider = "local-model"`.
fn local_model_config(model: &str) -> String {
    format!("[embeddings]\nprovider = \"local-model\"\nmodel = \"{model}\"\n")
}

/// Install [`EMBED_MODEL`] through the real command, so the row and the artifact are what `np models install`
/// writes rather than a fixture shaped to look like one. Returns the artifact path it placed.
///
/// The source bytes are not a model: `install` verifies the declared digest against the file and stores it, and
/// `embed` never turns them into numbers — in a default build because there is no runtime to ask, in a
/// `local-model` build because llama.cpp refuses bytes with the wrong magic. Both refusals are arm (iv), and
/// [`embed_with_everything_present_reports_the_runtime_that_is_not_built`] pins that each build says its own.
fn install_model(ws: &Workspace) -> PathBuf {
    let source = ws._tmp.path().join("artifact.bin");
    fs::write(&source, b"the bytes one model stands for\n")
        .expect("test setup: write the artifact source");
    // The digest comes from `np_core::hash::sha256_file` — the same function `np models hash` calls, and the only
    // SHA-256 in the program (`crates/np-cli/tests/models_cli.rs` pins that equality).
    let digest =
        np_core::hash::sha256_file(&source).expect("test setup: digest the artifact source");

    let installed = run(&mut ws.command(&[
        "models",
        "install",
        "--id",
        EMBED_MODEL,
        "--path",
        source.to_str().expect("test setup: utf-8 tempdir path"),
        "--dim",
        EMBED_DIM,
        "--sha256",
        &digest,
    ]));
    installed.assert_success();
    ws.models_dir().join(EMBED_MODEL)
}

/// One aligned field of a text summary — the value after the label, trimmed.
///
/// Panics when the label is absent, because a missing line is exactly what `assert_eq!` against `None` would catch
/// while a `.contains("seen")` on the whole stdout would not: the report's own text mentions nothing else, but a
/// helper that cannot fail loudly is not evidence.
fn summary_field(text: &str, label: &str) -> String {
    text.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(char::is_whitespace)?;
            (name == label).then(|| value.trim().to_string())
        })
        .unwrap_or_else(|| panic!("`{label}` has to carry a value in:\n{text}"))
}

/// One count of a text summary, by its aligned label.
fn summary_count(text: &str, label: &str) -> u64 {
    let value = summary_field(text, label);
    value
        .parse()
        .unwrap_or_else(|err| panic!("`{label} = {value}` is not a count: {err}"))
}

/// The sorted key names of a JSON object, the way `crates/np-cli/tests/models_cli.rs` pins key sets.
fn json_keys(value: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object: {value}"))
        .keys()
        .cloned()
        .collect();
    names.sort();
    names
}

/// Arm (ii), first half: no `[embeddings]` section means `provider = "none"`, which §2 documents as the default
/// state of a fresh install — so it leaves as exit 1 with *configuration disabled* on **both** channels, and reads
/// nothing. The scan above the run is what makes `seen 0` evidence rather than an empty workspace.
#[test]
fn embed_without_an_embeddings_section_is_configuration_disabled_on_both_channels() {
    let ws = workspace();
    // A config file that exists and parses and holds no `[embeddings]` section at all.
    ws.write_user_config("# deliberately no [embeddings] section\n");
    run(&mut ws.command(&["scan"])).assert_success();

    let failed = run(&mut ws.command(&["embed"]));

    assert_eq!(
        failed.code(),
        1,
        "arm (ii) is exit 1 and adds no new code:\n{}",
        failed.stdout()
    );
    for channel in [failed.stdout(), failed.stderr()] {
        assert!(
            channel.contains("configuration disabled")
                && channel.contains("embeddings.provider = \"none\""),
            "both channels have to name the arm and the key: {channel}"
        );
    }
    let stdout = failed.stdout();
    assert_eq!(summary_count(&stdout, "seen"), 0, "nothing was read");
    assert_eq!(summary_count(&stdout, "embedded"), 0);
    assert_eq!(
        summary_field(&stdout, "model"),
        "-",
        "no model id was named"
    );
}

/// Arm (ii), second half: `provider = "local-model"` with a model id that names no installed row. The refusal has
/// to name the id — "configuration disabled" alone would not say what to install.
#[test]
fn embed_naming_a_model_that_is_not_installed_names_the_id() {
    let ws = workspace();
    // No `np models install` run: the row does not exist.
    ws.write_user_config(&local_model_config(EMBED_MODEL));
    run(&mut ws.command(&["scan"])).assert_success();

    let failed = run(&mut ws.command(&["embed"]));

    assert_eq!(failed.code(), 1, "arm (ii) is exit 1:\n{}", failed.stdout());
    for channel in [failed.stdout(), failed.stderr()] {
        assert!(
            channel.contains("configuration disabled")
                && channel.contains(&format!("embeddings.model = \"{EMBED_MODEL}\"")),
            "the refusal has to name the key and the id it could not find: {channel}"
        );
    }
    assert_eq!(summary_count(&failed.stdout(), "seen"), 0);
}

/// Arm (iii), reached from argv: the row is installed, its artifact file is not there, and the run owes work. This
/// is D-2's regression test — the case `crates/np-core/tests/embed_run.rs` could not see because it injected a
/// provider instead of resolving one.
#[test]
fn embed_with_the_artifact_file_missing_names_installing_it() {
    let ws = workspace();
    let artifact = install_model(&ws);
    ws.write_user_config(&local_model_config(EMBED_MODEL));
    run(&mut ws.command(&["scan"])).assert_success();
    fs::remove_file(&artifact).expect("test setup: remove the installed artifact file");

    let failed = run(&mut ws.command(&["embed"]));

    assert_eq!(
        failed.code(),
        1,
        "arm (iii) is exit 1 (§6):\n{}",
        failed.stdout()
    );
    for channel in [failed.stdout(), failed.stderr()] {
        assert!(channel.contains("artifact absent"), "{channel}");
        // §6's remedy, naming only commands this build actually has. `fetch` is a cargo feature, so the assertion follows it
        // rather than asserting one build's shape in every row — the hint used to promise "fetching a model is not
        // implemented", which stopped being true when R7b shipped the command.
        assert!(channel.contains("np models install"), "{channel}");
        if cfg!(feature = "fetch") {
            assert!(
                channel.contains("np models fetch"),
                "this binary can fetch, and the remedy should say so: {channel}"
            );
        } else {
            assert!(
                !channel.contains("np models fetch"),
                "a build without `fetch` must not name it: {channel}"
            );
        }
    }
    // The defect itself. The row declares 768 and the truth is a missing file, so no width may appear in the
    // answer; this fails the moment np-cli answers an unreadable artifact with a placeholder provider again,
    // because that placeholder's `dim()` is 0 and §6's wrong-dim refusal quotes it. (Verified by mutation: see the
    // round report's r6c1-fix section.)
    assert!(
        !failed.stdout().contains("dimensions") && !failed.stderr().contains("dimensions"),
        "a missing file is not a width disagreement:\n{}\n{}",
        failed.stdout(),
        failed.stderr()
    );
    // The path arm (iii) looked at, spelled where the artifact would live — derived, not guessed.
    assert!(
        failed.stdout().contains(&artifact.display().to_string()),
        "the report has to name the file a person can install: {}",
        failed.stdout()
    );
    // The corpus was read (so `seen` is real) and nothing was written before the arm fired.
    let stdout = failed.stdout();
    assert!(summary_count(&stdout, "seen") > 0, "{stdout}");
    assert_eq!(summary_count(&stdout, "embedded"), 0);
    assert_eq!(
        summary_field(&stdout, "verify"),
        "self-consistent, not verified"
    );
}

/// Arm (iv) as each build answers it: configured, installed, artifact present, work owed — and nothing behind the
/// batch that can turn text into numbers. **What "nothing" means is what R6c2 split.** Without `local-model` the
/// provider type refuses every batch unanswered (`RuntimeNotBuilt`); with it the provider *tries to load* the
/// installed bytes and llama.cpp returns null for them, because what [`install_model`] put there is a sentence
/// rather than a GGUF. Both are arm (iv), both exit 1, both write nothing — so everything below holds in either
/// build, and each build's own sentence is pinned by its own arm of one `cfg!`, which is honest about being a
/// different answer rather than pretending there is one.
#[test]
fn embed_with_everything_present_reports_the_runtime_that_is_not_built() {
    let ws = workspace();
    install_model(&ws); // the artifact stays where `install` put it
    ws.write_user_config(&local_model_config(EMBED_MODEL));
    run(&mut ws.command(&["scan"])).assert_success();

    let failed = run(&mut ws.command(&["embed"]));

    assert_eq!(failed.code(), 1, "arm (iv) is exit 1:\n{}", failed.stdout());
    for channel in [failed.stdout(), failed.stderr()] {
        assert!(channel.contains("owed and not produced"), "{channel}");
        let expected = if cfg!(feature = "local-model") {
            "is not loadable as a model"
        } else {
            "the local embedding runtime is not built yet"
        };
        assert!(
            channel.contains(expected),
            "arm (iv) has to name why nothing was produced ({expected:?} in a build with              local-model = {}), not that the corpus is empty: {channel}",
            cfg!(feature = "local-model"),
        );
        assert!(
            channel.contains(EMBED_MODEL),
            "and name what stayed owed: {channel}"
        );
    }
    let stdout = failed.stdout();
    assert!(summary_count(&stdout, "seen") > 0, "{stdout}");
    assert_eq!(summary_count(&stdout, "embedded"), 0);
    assert_eq!(summary_count(&stdout, "errors"), 1, "one batch refused");
}

/// §6 adds no exit code: a missing operand stays clap's problem, and clap's 2 has to survive the trip from
/// `main`'s own `ExitCode::SUCCESS`/`FAILURE` handling.
#[test]
fn embed_without_a_workspace_operand_stays_claps_exit_two() {
    let ws = workspace();
    ws.write_user_config(&local_model_config(EMBED_MODEL));

    let failed = run(&mut ws.command_without_operand(&["embed"]));

    assert_eq!(
        failed.code(),
        2,
        "argv is clap's to refuse, with clap's code:\n{}",
        failed.stderr()
    );
    assert!(
        failed.stdout().is_empty(),
        "a refused argv prints no report: {}",
        failed.stdout()
    );
    let stderr = failed.stderr();
    assert!(
        stderr.contains("WORKSPACE"),
        "clap has to name the operand that is missing: {stderr}"
    );
}

/// One `--json` case, on arm (iii): the outcome object's shape pinned as key sets, and every count equal to the
/// number the text summary printed for the same state. The label spellings differ on purpose (`seen` /
/// `chunks_seen`, `refused` / `refused_budget`, `busy-gap` / `busy_gap`), so that mapping is part of what this pins.
#[test]
fn embed_json_reports_the_same_outcome_and_counts_as_the_summary() {
    let ws = workspace();
    let artifact = install_model(&ws);
    ws.write_user_config(&local_model_config(EMBED_MODEL));
    run(&mut ws.command(&["scan"])).assert_success();
    fs::remove_file(&artifact).expect("test setup: remove the installed artifact file");

    let text = run(&mut ws.command(&["embed"]));
    let json = run(&mut ws.command(&["embed", "--json"]));

    assert_eq!(text.code(), 1);
    assert_eq!(
        json.code(),
        text.code(),
        "one fact, one exit code, whatever the channel:\n{}",
        json.stderr()
    );
    let report: serde_json::Value =
        serde_json::from_str(&json.stdout()).expect("`--json` must print one JSON object");

    // The whole top-level key set, so renaming `verification` or giving one arm an extra field fails here rather
    // than in a reader written against this shape.
    assert_eq!(
        json_keys(&report),
        vec![
            "artifact_check",
            "busy_gap",
            "chunks_seen",
            "current",
            "embedded",
            "errors",
            // The four image fields (§6's order puts them after the chunk counts): a run that saw no figure still
            // carries the keys, with `image_refusals` an empty array, because a reader must be able to tell "zero"
            // from "this build did not look".
            "image_refusals",
            "images_embedded",
            // Media rows this run deleted (§7 rule 4's cleanup, made visible): present and zero when nothing went stale.
            "images_pruned",
            "images_refused",
            "images_seen",
            // Did the stale-figure sweep finish? `false` on an arm that never opened the database — which is why the summary
            // says nothing about it here, and why a reader must not infer this from `images_unchecked` being zero.
            "images_sweep_ran",
            // Notes this run could not read (§6's `images_unchecked`): present and zero even on an arm that opened no
            // database, because a reader must be able to tell "nothing was skipped" from "this build said nothing".
            "images_unchecked",
            "model",
            "outcome",
            "refused_budget",
            "verification",
            "workers"
        ]
    );
    // The outcome object is externally tagged: exactly one arm key holding exactly the three fields §6 names.
    assert_eq!(json_keys(&report["outcome"]), vec!["artifact-absent"]);
    let absent = &report["outcome"]["artifact-absent"];
    assert_eq!(json_keys(absent), vec!["artifact", "hint", "model"]);
    assert_eq!(absent["model"], EMBED_MODEL);
    assert_eq!(report["model"], EMBED_MODEL);
    assert_eq!(absent["artifact"], artifact.display().to_string());
    let hint = absent["hint"].as_str().unwrap_or_default();
    let fetch_expected = cfg!(feature = "fetch");
    assert!(
        hint.contains("np models install") && hint.contains("np models fetch") == fetch_expected,
        "the JSON remedy is the text remedy, feature included: {hint}"
    );

    for (label, field) in [
        ("seen", "chunks_seen"),
        ("current", "current"),
        ("embedded", "embedded"),
        ("refused", "refused_budget"),
        ("busy-gap", "busy_gap"),
        ("errors", "errors"),
    ] {
        assert_eq!(
            report[field].as_u64(),
            Some(summary_count(&text.stdout(), label)),
            "`{field}` has to be the number the summary printed for `{label}`:\n{}",
            text.stdout()
        );
    }
    // Neither image line appears here: this run saw no figure and read every note, so both counts are zero and the lines
    // are suppressed (§6's text shape stays exactly what it was before images existed).
    let summary_text = text.stdout();
    for line in ["images", "unchecked"] {
        assert!(
            !summary_text
                .lines()
                .any(|got| got.split_whitespace().next() == Some(line)),
            "`{line}` must not appear in a run that saw no figure:\n{summary_text}"
        );
    }

    assert_eq!(
        report["verification"],
        summary_field(&text.stdout(), "verify")
    );
    // D12's field, on the arm that read no artifact: **present and null**, not missing. A reader written against this
    // shape has to be able to ask "were the model's bytes checked" and get "no artifact was involved" rather than
    // inferring it from a key that appears only sometimes. The text summary says the same thing by silence.
    assert!(
        report["artifact_check"].is_null(),
        "arm (iii) verified nothing, so JSON must not claim otherwise: {}",
        report["artifact_check"]
    );
    assert!(
        !text.stdout().contains("\ndigest "),
        "and the summary prints no digest line when there was no check:\n{}",
        text.stdout()
    );
    // The worker count follows the same rule as `digest`: **no local model, nothing to report**. Present and null in
    // JSON (a reader must be able to ask the question), absent from the summary (a line reading `threads 4 of 16` for a
    // run that embedded nothing would be a number about nothing).
    assert!(
        report["workers"].is_null(),
        "arm (iii) built no local provider, so JSON must not state a worker count: {}",
        report["workers"]
    );
    assert!(
        !text.stdout().contains("\nthreads "),
        "and the summary prints no threads line when no model was involved:\n{}",
        text.stdout()
    );
    // The arm's sentence travels on stderr under `--json` too — two channels, one fact.
    assert!(
        json.stderr().contains("artifact absent"),
        "{}",
        json.stderr()
    );
}

// ---------------------------------------------------------------------------
// `np embed` against a real GGUF — the one thing R6c2 exists to prove.
//
// Everything above this line runs in a build with no runtime, which is exactly why it could never show a vector
// arriving: the provider there answers `runtime not built` for every batch, and `crates/np-core/tests/embed_run.rs`
// proves the writer loop against a fake. What is below drives the **feature build's** binary at the 310 MB artifact
// and asserts the three things no fake can stand in for — that real float-32 vectors of the declared width land in
// `embeddings`, that a second run calls them current instead of recomputing them, and that `prefix_doc` reaches
// the model (different prefix, different bytes). §7's batch-invariance note asked for exactly this pairing.
//
//   NP_TEST_MODEL_PATH=/path/to/embeddinggemma-2-Q8_0.gguf \
//     cargo test -p np-cli --features local-model --test cli -- --nocapture
//
// With the variable unset each case prints `NOT COVERAGE` and passes, for the reason `local_embed.rs` gives: an
// absent 310 MB file must not look like a failure, and must not look like coverage either. The artifact is
// **symlinked**, never copied — `models install` would spend 310 MB of disk per case in a suite that already runs
// in parallel, and following the workspace's own path spelling is part of what is being tested.
// ---------------------------------------------------------------------------

/// `np search` through the real binary: argv stays clap's to refuse, and every runtime refusal keeps exit 1 with a message
/// that says what to change (§8) — none of them invents a code, and none writes to stdout.
#[test]
fn search_without_a_query_is_claps_to_refuse() {
    let ws = workspace();
    ws.write_user_config(&local_model_config(EMBED_MODEL));

    // `<workspace>` then `<query>`: the operand order every other subcommand uses, so `Workspace::command` cannot build it
    // (it appends the workspace last, which would put the query where clap expects the root).
    let mut cmd = ws.command_without_operand(&["search"]);
    let failed = run(&mut cmd);

    assert_eq!(failed.code(), 2, "argv is clap's:\n{}", failed.stderr());
    assert!(
        failed.stdout().is_empty(),
        "a refused argv prints no report"
    );
    assert!(
        failed.stderr().contains("QUERY"),
        "clap has to name the operand it needs: {}",
        failed.stderr()
    );
}

#[test]
fn search_with_embedding_disabled_refuses_and_names_the_setting() {
    let ws = workspace();
    // No `[embeddings]` section at all: `provider = "none"`, so there is no way to turn the query into a vector.
    fs::write(ws.root.join("a.md"), "# Alpha\n\nnothing indexed here\n").expect("test setup: note");
    run(&mut ws.command(&["scan"])).assert_success();

    let mut cmd = ws.command_without_operand(&["search"]);
    cmd.arg(&ws.root).arg("--mode").arg("semantic").arg("alpha");
    let failed = run(&mut cmd);

    assert_eq!(
        failed.code(),
        1,
        "a runtime refusal is 1:\n{}",
        failed.stderr()
    );
    // The exact setting, not "some word that appears near the subject". The first version accepted `contains("provider")`,
    // which the generic trait refusal satisfied while naming nothing a reader could change (Task 6 review P1). F32 pins the
    // whole line against a literal so a reword that drops the remedy fails too — and against a literal rather than
    // `NoProvider::sentence()`, because an expectation assembled from the code under test cannot catch a reword.
    let stderr = failed.stderr();
    assert_eq!(
        stderr.trim_end(),
        concat!(
            r#"np: embedding disabled: embeddings.provider = "none" — set it to "local-model" and name an "#,
            "installed model with embeddings.model, then run np embed"
        ),
        "the refusal is this sentence, in full"
    );
    assert_eq!(stderr.matches('\n').count(), 1, "one line: {stderr:?}");
    assert!(failed.stdout().is_empty(), "a refusal prints no hit list");
}

#[test]
fn search_with_no_row_names_the_missing_row_and_says_what_to_run() {
    // **Hybrid changed what this test has to ask for (2026-11-08).** `docs/design-m3-hybrid-search.md` made keyword search a
    // legitimate answer, so a workspace with no vectors now exits 0 with a hit list instead of refusing — and F32's sentence
    // would never print. `--mode semantic` is the request this refusal belongs to: *only* the vector list, which genuinely
    // cannot run. The sentences themselves are unchanged, and `search_without_a_model_answers_from_keywords_and_says_the_half`
    // pins the new default alongside them.
    // F32's state: the config asks for a local model this workspace has no row for — the reader who edited
    // `config.toml` before installing anything. Today this prints a sentence about `provider = "none"`, which is
    // not the configuration they have, and tells them to change a setting that is already right.
    let ws = workspace();
    fs::write(ws.root.join("a.md"), "# Alpha\n\nabout alpha\n").expect("test setup: note");
    run(&mut ws.command(&["scan"])).assert_success();
    // Deliberately no `install_model`: the row is what is missing.
    ws.write_user_config(&local_model_config(EMBED_MODEL));

    let mut cmd = ws.command_without_operand(&["search"]);
    cmd.arg(&ws.root).arg("--mode").arg("semantic").arg("alpha");
    let failed = run(&mut cmd);

    assert_eq!(
        failed.code(),
        1,
        "a refusal is 1, and clap keeps argv at 2:\n{}",
        failed.stderr()
    );
    let stderr = failed.stderr();
    // A literal prefix, not `NoProvider::sentence()` — building the expectation from the code under test would make this
    // case pass whatever the sentence became.
    assert!(
        stderr.starts_with(&format!(
            "np: no model row for \"{EMBED_MODEL}\" in this workspace:"
        )),
        "the refusal has to name the missing row and the id the config asked for: {stderr}"
    );
    assert!(
        !stderr.contains(r#"embeddings.provider = "none""#),
        "this config says `local-model`; blaming a setting the reader does not have is the defect: {stderr}"
    );
    assert!(
        !stderr.contains("   "),
        "the message wrapped mid-sentence and printed a run of spaces: [{stderr}]"
    );
    // One line. `Db::open` can write orphan receipts to stderr, so "exactly one" is the honest claim rather than
    // whole-buffer equality — and it is what catches a refusal that starts printing advice on a second line.
    assert_eq!(
        stderr.matches('\n').count(),
        1,
        "a refusal is one line, got {stderr:?}"
    );
    // The remedy names a command this build has. `fetch` is a cargo feature, so the assertion follows the build rather
    // than promising a subcommand that `--no-default-features` compiles out of the help table (the same shape as
    // `embed_remedy_names_only_commands_this_build_has`).
    assert!(
        stderr.contains("np models install"),
        "install is in every build, so it has to be named: {stderr}"
    );
    if cfg!(feature = "fetch") {
        assert!(
            stderr.contains("np models fetch"),
            "this binary can fetch, and the remedy should say so: {stderr}"
        );
    } else {
        assert!(
            !stderr.contains("np models fetch"),
            "a build without `fetch` must not name it: {stderr}"
        );
    }
    assert!(failed.stdout().is_empty(), "a refusal prints no hit list");

    // The same refusal under `--json`: a machine reader gets the sentence on stderr and nothing on stdout, which is what
    // today's behaviour is and nothing in the tree pinned for any search refusal before F32.
    let mut json_cmd = ws.command_without_operand(&["search"]);
    json_cmd
        .arg(&ws.root)
        .arg("--mode")
        .arg("semantic")
        .arg("alpha")
        .arg("--json");
    let json_failed = run(&mut json_cmd);
    assert_eq!(json_failed.code(), 1, "{}", json_failed.stderr());
    assert!(
        json_failed.stdout().is_empty(),
        "a refusal writes no JSON: {}",
        json_failed.stdout()
    );
    assert_eq!(
        json_failed.stderr(),
        stderr,
        "the same state must refuse the same way in both channels"
    );
}

#[cfg(not(feature = "local-model"))]
#[test]
fn searching_without_the_runtime_names_the_model_that_stayed_owed() {
    // **Hybrid changed what this test has to ask for (2026-11-08).** `docs/design-m3-hybrid-search.md` made keyword search a
    // legitimate answer, so a workspace with no vectors now exits 0 with a hit list instead of refusing — and F32's sentence
    // would never print. `--mode semantic` is the request this refusal belongs to: *only* the vector list, which genuinely
    // cannot run. The sentences themselves are unchanged, and `search_without_a_model_answers_from_keywords_and_says_the_half`
    // pins the new default alongside them.
    // §6 arm (iv)'s class, from the search side: the row says installed and the file is there, but **this binary** links no
    // inference runtime. It has to come back as the runtime-not-built refusal naming the model id — not as "this provider
    // cannot embed a search query", which reads as a capability the seam lacks rather than one build flag (review P1).
    let ws = workspace();
    fs::write(ws.root.join("a.md"), "# Alpha\n\nabout alpha\n").expect("test setup: note");
    run(&mut ws.command(&["scan"])).assert_success();
    install_model(&ws);
    ws.write_user_config(&local_model_config(EMBED_MODEL));

    let mut cmd = ws.command_without_operand(&["search"]);
    cmd.arg(&ws.root).arg("--mode").arg("semantic").arg("alpha");
    let failed = run(&mut cmd);

    assert_eq!(failed.code(), 1, "\n{}", failed.stderr());
    let stderr = failed.stderr();
    assert!(
        stderr.contains("runtime is not built"),
        "the refusal has to say the runtime is missing: {stderr}"
    );
    assert!(
        stderr.contains(EMBED_MODEL),
        "and name the model that produced nothing: {stderr}"
    );
    assert!(failed.stdout().is_empty());
}

#[test]
fn an_invented_log_level_refuses_and_a_valid_one_does_not() {
    // F33's escape hatch has to be usable, which starts with a typo in it being visible. The refusal belongs to any command
    // that reads configuration — `scan` is the cheapest such command, and pinning it there rather than on `search` also pins
    // that the value is checked while resolving config rather than deep inside a model load.
    let ws = workspace();
    fs::write(ws.root.join("a.md"), "# Alpha\n\nabout alpha\n").expect("test setup: note");

    let mut cmd = ws.command(&["scan"]);
    cmd.env("NP_LLAMA_LOG_LEVEL", "deubg");
    let failed = run(&mut cmd);

    assert_eq!(
        failed.code(),
        1,
        "a runtime refusal is 1:\n{}",
        failed.stderr()
    );
    let stderr = failed.stderr();
    assert!(
        stderr.contains("NP_LLAMA_LOG_LEVEL"),
        "the refusal must name the variable that was mistyped: {stderr}"
    );
    assert!(
        stderr.contains("deubg"),
        "and the value as written, so the reader knows which word is wrong: {stderr}"
    );
    for name in ["off", "error", "warn", "info", "debug"] {
        assert!(
            stderr.contains(name),
            "the refusal omits `{name}`: {stderr}"
        );
    }

    // The same command with a real level runs: the refusal is about the spelling, not about the variable existing.
    let mut ok = ws.command(&["scan"]);
    ok.env("NP_LLAMA_LOG_LEVEL", "info");
    let passed = run(&mut ok);
    assert_eq!(passed.code(), 0, "{}", passed.stderr());
}

#[test]
fn search_with_the_artifact_file_missing_says_the_query_cannot_be_embedded() {
    // **Hybrid changed what this test has to ask for (2026-11-08).** `docs/design-m3-hybrid-search.md` made keyword search a
    // legitimate answer, so a workspace with no vectors now exits 0 with a hit list instead of refusing — and F32's sentence
    // would never print. `--mode semantic` is the request this refusal belongs to: *only* the vector list, which genuinely
    // cannot run. The sentences themselves are unchanged, and `search_without_a_model_answers_from_keywords_and_says_the_half`
    // pins the new default alongside them.
    let ws = workspace();
    fs::write(ws.root.join("a.md"), "# Alpha\n\nnothing indexed here\n").expect("test setup: note");
    run(&mut ws.command(&["scan"])).assert_success();
    install_model(&ws);
    ws.write_user_config(&local_model_config(EMBED_MODEL));
    // The row says installed; the file is not there — arm (iii)'s shape, seen from the search side.
    fs::remove_file(ws.models_dir().join(EMBED_MODEL)).ok();

    let mut cmd = ws.command_without_operand(&["search"]);
    cmd.arg(&ws.root).arg("--mode").arg("semantic").arg("alpha");
    let failed = run(&mut cmd);

    assert_eq!(failed.code(), 1, "\n{}", failed.stderr());
    let stderr = failed.stderr();
    assert!(
        stderr.contains("artifact") && stderr.contains("absent"),
        "a reader needs to know the artifact is what is missing: {stderr}"
    );
    // F32 appended the same pointer embed's arm (iii) gives. Without this assertion it could be deleted silently.
    assert!(
        stderr.contains("np models install"),
        "and what to run about it: {stderr}"
    );
    assert!(
        !stderr.contains("   "),
        "the message wrapped mid-sentence and printed a run of spaces: [{stderr}]"
    );
    assert_eq!(stderr.matches('\n').count(), 1, "one line: {stderr:?}");
    assert!(failed.stdout().is_empty());
}

// ---------------------------------------------------------------------------
// Hybrid search (F36, `docs/design-m3-hybrid-search.md`). These four cases are the default-build proof that keyword
// search is a real answer rather than an error path: no artifact, no runtime, exit 0. The F32 refusals above survive by
// asking for `--mode semantic`, which is the one request that genuinely cannot run without a model.
// ---------------------------------------------------------------------------

/// The sentence every build owes when the vector half was not there to answer. Pinned as a literal in both feature
/// shapes, because a reword that drops the remedy would otherwise pass by matching a substring of itself.
#[cfg(feature = "fetch")]
const NO_VECTORS_SENTENCE: &str = concat!(
    "no vectors in this workspace: ranked by keyword only \u{2014} ",
    "`np models fetch` or `np embed` adds the semantic half"
);

#[cfg(not(feature = "fetch"))]
const NO_VECTORS_SENTENCE: &str = concat!(
    "no vectors in this workspace: ranked by keyword only \u{2014} ",
    "`np embed` adds the semantic half once a model is installed"
);

#[test]
fn search_without_a_model_answers_from_keywords_and_says_the_half_that_ran() {
    // The property hybrid exists for: a workspace on a machine that has never downloaded 310 MB still answers a word
    // query. Nothing here installs a model, sets `NP_TEST_MODEL_PATH`, or builds a runtime — if this test ever needs an
    // artifact to pass, the default has stopped meaning anything.
    let ws = workspace();
    run(&mut ws.command(&["scan"])).assert_success();

    let ok = {
        let mut cmd = ws.command_without_operand(&["search"]);
        cmd.arg(&ws.root).arg("demo");
        run(&mut cmd)
    };

    assert_eq!(ok.code(), 0, "keyword search answers:\n{}", ok.stderr());
    let text = ok.stdout();
    assert!(
        text.contains("sprint-review.md"),
        "`demo` appears in one note, and that note has to come back:\n{text}"
    );
    assert!(
        !text.contains("bom-note.md"),
        "a note with no such word must not appear:\n{text}"
    );
    // Which half produced a hit is printed as evidence beside the score. `kind` labels *what* the hit is — a note or a
    // figure, F28's distinction — not which list found it, and conflating the two would cost the text channel that.
    assert!(
        text.contains("bm25"),
        "the line has to show the keyword half produced this hit:\n{text}"
    );
    // **A tiny number is not a missing one.** SQLite's `bm25()` on a corpus this small answers in millionths, and the first
    // version of this column printed `-0.0000` — which reads as "no score was computed" on the one screen where a reader is
    // deciding whether the ranking is trustworthy. Small magnitudes print in scientific notation instead (`number`).
    let first_line = text.lines().next().expect("one hit means one line");
    assert!(
        !first_line.starts_with("0.0000") && !first_line.contains("bm25 -0.0000"),
        "a real, small score must not print as zero: {first_line}"
    );
    // The missing half is said out loud, on stderr, once, in this build's own words. Stderr so stdout stays a parseable
    // hit list; one line because the F32 refusals set that shape and a two-line caveat breaks the same readers.
    assert_eq!(
        ok.stderr().trim_end(),
        NO_VECTORS_SENTENCE,
        "the caveat is this sentence, in full"
    );
}

/// **An empty result says which half ran, and never prints a model name that does not exist.** The first interactive Windows
/// run (2026-11-07) hit `nothing indexed for  by hybrid` on the most likely first invocation of all — no `[embeddings]` section,
/// no vectors, a query nothing matches. `{model}` interpolated the empty string: a double space where a name belongs, and a claim
/// about an index the keyword half had just finished consulting. The keyword arm is pinned beside it so the two sentences cannot
/// quietly merge back into one.
#[test]
fn an_empty_result_names_the_query_and_never_an_absent_model() {
    let ws = workspace();
    run(&mut ws.command(&["scan"])).assert_success();

    // Hybrid, no vectors: the sentence names the query and says the vectors are missing.
    let mut hybrid = ws.command_without_operand(&["search"]);
    hybrid.arg(&ws.root).arg("zzz nothing in this vault");
    let ok = run(&mut hybrid);
    assert_eq!(
        ok.code(),
        0,
        "an empty result is a fact, not an error:\n{}",
        ok.stderr()
    );
    let line = ok.stdout().trim_end().to_string();
    assert_eq!(
        line,
        r#"nothing matched "zzz nothing in this vault" by hybrid: this workspace holds no vectors"#,
        "this is the sentence a reader gets"
    );

    // Keyword mode keeps its own shorter sentence: it never consulted a vector list, so saying one is missing would be noise.
    let mut keyword = ws.command_without_operand(&["search"]);
    keyword
        .arg(&ws.root)
        .arg("--mode")
        .arg("keyword")
        .arg("zzz");
    let ok = run(&mut keyword);
    assert_eq!(ok.code(), 0, "\n{}", ok.stderr());
    assert_eq!(
        ok.stdout().trim_end(),
        r#"nothing matched "zzz""#,
        "keyword mode names the query and nothing else"
    );

    // The regression itself, asserted as an absence: no double space anywhere, and no `for  by` with a hole in it.
    for text in [line, ok.stdout()] {
        assert!(
            !text.contains("  ") && !text.contains("for  by"),
            "a sentence with a hole where a model name should be: {text:?}"
        );
    }

    // Mutation this catches: interpolating `{model}` unconditionally (both assertions above fail on the first run), and
    // collapsing the two arms into one, which loses the keyword sentence. The third arm — a real model id with nothing embedded
    // yet — needs a provider that can embed, so it is pinned in `mod real_model` below.
}

#[test]
fn search_json_names_the_mode_that_ran_and_the_half_that_was_missing() {
    // `--json` gains three keys beside `hits`, and they describe the **list**, not one hit: which mode ran, whether this
    // workspace holds vectors at all, and why a half is missing. A machine reader that used to see `{hits}` alone cannot
    // tell "no semantic hits" from "no model installed"; these three keys are that difference.
    let ws = workspace();
    run(&mut ws.command(&["scan"])).assert_success();

    let mut cmd = ws.command_without_operand(&["search"]);
    cmd.arg(&ws.root).arg("demo").arg("--json");
    let ok = run(&mut cmd);

    assert_eq!(ok.code(), 0, "\n{}", ok.stderr());
    let value: serde_json::Value =
        serde_json::from_str(&ok.stdout()).expect("`--json` prints exactly one JSON object");
    assert_eq!(
        json_keys(&value),
        vec!["hits", "mode", "note", "vectors"],
        "the hit list plus the three keys that say how it was made"
    );
    assert_eq!(
        value["mode"], "hybrid",
        "the default mode reports its own name"
    );
    assert_eq!(
        value["vectors"], false,
        "this workspace has no vectors, and pretending otherwise would be the F32 mistake again"
    );
    assert_eq!(
        value["note"], NO_VECTORS_SENTENCE,
        "the same sentence stderr carries, so the two channels cannot tell different stories"
    );
    let hits = value["hits"].as_array().expect("hits is an array");
    assert!(!hits.is_empty(), "the keyword list answers");
    assert_eq!(
        hits[0]["kind"], "note",
        "keyword search finds notes and says so"
    );
    assert_eq!(
        json_keys(&hits[0]),
        vec![
            "bm25",
            "cosine",
            "keyword_rank",
            "kind",
            "linked_from",
            "linked_notes",
            "path",
            "score",
            "semantic_rank",
            "snippet",
            "source_note",
        ],
        "every hit carries the same eleven keys, with null where a half did not run"
    );
    assert!(
        hits[0]["cosine"].is_null() && hits[0]["semantic_rank"].is_null(),
        "no vector ran, so no cosine and no semantic rank: {}",
        ok.stdout()
    );
    assert!(
        hits[0]["snippet"].is_string() && hits[0]["bm25"].is_number(),
        "the keyword half has to show its evidence: {}",
        ok.stdout()
    );
}

#[test]
fn config_can_owe_the_semantic_half_alone_and_says_so() {
    // Proves `[search] mode` is read rather than parsed: with `semantic` in the file and no model installed, the F32
    // refusal returns — the same exit 1 the flag form pins above, reached through config instead of argv.
    let ws = workspace();
    // The mode has to be *in the file*, or this test proves nothing about config being read: with no `[search]` section the
    // default is hybrid, and hybrid answers from keywords instead of refusing.
    ws.write_user_config(&format!(
        "{}\n[search]\nmode = \"semantic\"\n",
        local_model_config(EMBED_MODEL)
    ));
    run(&mut ws.command(&["scan"])).assert_success();

    let failed = {
        let mut cmd = ws.command_without_operand(&["search"]);
        cmd.arg(&ws.root).arg("demo");
        run(&mut cmd)
    };
    assert_eq!(
        failed.code(),
        1,
        "a mode that needs a row this workspace lacks refuses:\n{}",
        failed.stderr()
    );
    assert!(failed.stdout().is_empty(), "a refusal prints no hit list");
    assert!(
        failed.stderr().contains("no model row"),
        "F32's sentence about the missing row, not the keyword caveat: {}",
        failed.stderr()
    );
    assert!(
        !failed.stderr().contains(NO_VECTORS_SENTENCE),
        "`semantic` asked for one list and did not get a partial answer instead: {}",
        failed.stderr()
    );
}

#[test]
fn keyword_mode_answers_where_the_semantic_half_would_refuse() {
    // The other half of the same workspace: identical config, `mode = "keyword"` in the file, and the query answers with
    // exit 0 even though a model is configured and absent. That is the escape hatch the human asked for — search on a
    // machine that cannot get the artifact — and it also proves the mode reaches the code path that decides whether to
    // load a provider at all.
    let ws = workspace();
    ws.write_user_config(&format!(
        "{}\n[search]\nmode = \"keyword\"\n",
        local_model_config(EMBED_MODEL)
    ));
    run(&mut ws.command(&["scan"])).assert_success();

    let ok = {
        let mut cmd = ws.command_without_operand(&["search"]);
        cmd.arg(&ws.root).arg("demo");
        run(&mut cmd)
    };
    assert_eq!(
        ok.code(),
        0,
        "keyword mode ignores the absent model:\n{}",
        ok.stderr()
    );
    assert!(ok.stdout().contains("sprint-review.md"), "{}", ok.stdout());
    assert!(
        ok.stderr().is_empty(),
        "keyword mode asked for one list and got it: nothing to caveat: {}",
        ok.stderr()
    );
}

#[test]
fn an_invented_mode_refuses_with_claps_exit_two() {
    // A fourth spelling has to be refused by both doors. This is argv's; config's is `tests/config.rs`.
    let ws = workspace();
    run(&mut ws.command(&["scan"])).assert_success();

    let mut cmd = ws.command_without_operand(&["search"]);
    cmd.arg(&ws.root).arg("--mode").arg("fuzzy").arg("demo");
    let failed = run(&mut cmd);

    assert_eq!(
        failed.code(),
        2,
        "clap's usage error is exit 2:\n{}",
        failed.stderr()
    );
    for name in ["hybrid", "semantic", "keyword"] {
        assert!(
            failed.stderr().contains(name),
            "the refusal has to list the three real names, and `{name}` is missing from:\n{}",
            failed.stderr()
        );
    }
}

#[cfg(feature = "local-model")]
mod real_model {
    use super::*;

    /// The width `embeddinggemma-2-Q8_0.gguf` answers with — the same 768 `np-core`'s artifact suites pin from
    /// the file's own header. If this ever disagrees with the artifact, the run refuses rather than writing
    /// garbage, which is the behaviour under test either way.
    const REAL_DIM: i64 = 768;

    fn artifact() -> Option<PathBuf> {
        std::env::var_os("NP_TEST_MODEL_PATH").map(PathBuf::from)
    }

    macro_rules! artifact_or_not_coverage {
        () => {
            match artifact() {
                Some(path) => path,
                None => {
                    println!(
                        "NOT COVERAGE: NP_TEST_MODEL_PATH is unset, so no real vector was written by this case. \
                         Re-run with it pointing at embeddinggemma-2-Q8_0.gguf (sha256 2188ac1d…)."
                    );
                    return;
                }
            }
        };
    }

    /// A two-note workspace, small on purpose: this case pays a real load plus one decode per chunk, and the
    /// corpus's seventeen files would multiply that for no additional claim.
    fn tiny_workspace() -> Workspace {
        let tmp = TempDir::new().expect("test setup: tempdir");
        let root = tmp.path().join("ws");
        fs::create_dir_all(&root).expect("test setup: workspace root");
        fs::create_dir_all(tmp.path().join("config")).expect("test setup: config dir");
        fs::write(
            root.join("a.md"),
            "# Alpha\n\nThe cat sits on the warm mat while the rain falls outside.\n",
        )
        .expect("test setup: first note");
        fs::write(
            root.join("b.md"),
            "# Beta\n\nRevenue for the quarter rose because the pipeline stopped leaking.\n",
        )
        .expect("test setup: second note");
        Workspace { _tmp: tmp, root }
    }

    /// `np search` against vectors a real model wrote. The default-build cases above can only prove the refusals; this is
    /// the one that proves the ranking path end to end — argv order, the four-decimal text line, and `--json` carrying
    /// the hit list beside `mode`, `note` and `vectors` — with the query embedded by the same artifact that made the stored
    /// rows, which is what makes `note: null` and `vectors: true` worth pinning here rather than in a default-build case.
    #[test]
    fn searching_a_real_index_answers_in_both_channels() {
        let artifact = artifact_or_not_coverage!();
        let ws = tiny_workspace();
        run(&mut ws.command(&["scan"])).assert_success();
        link_artifact(&ws, &artifact, "task: search result | query: ");
        ws.write_user_config(&local_model_config(EMBED_MODEL));
        run(&mut ws.command(&["embed"])).assert_success();

        let search = |json: bool| {
            let mut cmd = ws.command_without_operand(&["search"]);
            cmd.arg(&ws.root).arg("the cat sits on the warm mat");
            if json {
                cmd.arg("--json");
            }
            run(&mut cmd)
        };

        let text = search(false);
        assert_eq!(text.code(), 0, "\n{}", text.stderr());
        let text_out = text.stdout();
        let first_line = text_out
            .lines()
            .next()
            .expect("a real index answers with at least one hit");
        let score = first_line
            .split_whitespace()
            .next()
            .expect("the line starts with the score");
        assert_eq!(
            score.split('.').nth(1).map(str::len),
            Some(4),
            "the text channel prints four decimals and says so in its own shape: {first_line}"
        );
        assert!(
            first_line.contains("note") && first_line.contains("a.md"),
            "the cat note has to lead a query about the cat: {}",
            text_out
        );

        let json = search(true);
        assert_eq!(json.code(), 0, "\n{}", json.stderr());
        let value: serde_json::Value =
            serde_json::from_str(&json.stdout()).expect("`--json` prints exactly one JSON object");
        assert_eq!(
            json_keys(&value),
            vec!["hits", "mode", "note", "vectors"],
            "the hit list plus F36's three keys that say how it was made — and nothing else on stdout"
        );
        // **A healthy run says so by silence.** `note` is null and `vectors` is true exactly when both halves answered, so
        // the caveat key cannot be read as a routine warning; this is the pair to the default-build case
        // `search_json_names_the_mode_that_ran_and_the_half_that_was_missing`, which pins the other side.
        assert_eq!(
            value["mode"].as_str(),
            Some("hybrid"),
            "the default mode names itself"
        );
        assert_eq!(
            value["vectors"].as_bool(),
            Some(true),
            "this workspace holds vectors a real model wrote"
        );
        assert!(
            value["note"].is_null(),
            "nothing was missing, so nothing is caveated: {}",
            json.stdout()
        );
        let hits = value["hits"].as_array().expect("hits is an array");
        assert!(
            !hits.is_empty(),
            "a query about the cat matches the cat note"
        );
        assert_eq!(
            json_keys(&hits[0]),
            vec![
                "bm25",
                "cosine",
                "keyword_rank",
                "kind",
                "linked_from",
                "linked_notes",
                "path",
                "score",
                "semantic_rank",
                "snippet",
                "source_note",
            ],
            "§12's hit fields, F28's linker list and F36's five evidence keys, no extras"
        );
        // Both halves ran here, so neither number is null — the claim that would be invisible in a default build, where
        // only the keyword side can answer.
        assert!(
            hits[0]["cosine"].is_number() && hits[0]["semantic_rank"].is_number(),
            "the vector half ran, so it shows its numbers: {}",
            json.stdout()
        );
        assert_eq!(hits[0]["path"].as_str(), Some("a.md"));
        assert_eq!(hits[0]["kind"].as_str(), Some("note"));
        // A note hit carries the linker list **empty**, not absent and not null: "no live linker" is one true statement
        // about both a note and an orphaned figure, and `kind` is what says which (F28's schema rule).
        assert_eq!(
            hits[0]["linked_notes"].as_array().map(Vec::len),
            Some(0),
            "a note hit belongs to no note: {}",
            json.stdout()
        );
        let scores: Vec<f64> = hits
            .iter()
            .map(|hit| hit["score"].as_f64().expect("score is a number"))
            .collect();
        let mut ordered = scores.clone();
        ordered.sort_by(|a, b| b.partial_cmp(a).expect("scores are comparable"));
        assert_eq!(
            scores,
            ordered,
            "the array arrives ranked:\n{}",
            json.stdout()
        );
    }

    /// The third empty-result arm: a workspace whose model **is** configured and installed, with nothing embedded yet, asks for
    /// the semantic half and gets an honest `nothing indexed for <id> by semantic` — the sentence that predates F36, kept because
    /// here a real id exists to name. Gated on `local-model` + artifact because a default build cannot reach it: with no runtime
    /// compiled in, embedding the query is itself the refusal.
    #[test]
    fn an_unembedded_workspace_names_its_model_in_the_empty_result() {
        let artifact = artifact_or_not_coverage!();
        let ws = tiny_workspace();
        run(&mut ws.command(&["scan"])).assert_success();
        link_artifact(&ws, &artifact, "query for note search: ");
        ws.write_user_config(&local_model_config(EMBED_MODEL));
        // Deliberately no `np embed`: the row exists, the vectors do not.

        let mut cmd = ws.command_without_operand(&["search"]);
        cmd.arg(&ws.root)
            .arg("--mode")
            .arg("semantic")
            .arg("anything");
        let ok = run(&mut cmd);
        assert_eq!(
            ok.code(),
            0,
            "no rows yet is a fact, not a refusal:\n{}",
            ok.stderr()
        );
        assert_eq!(
            ok.stdout().trim_end(),
            format!("nothing indexed for {EMBED_MODEL} by semantic"),
            "with a real id in hand, the sentence names it"
        );
    }

    /// F33's claim about a healthy run, measured rather than asserted in prose: the load transcript is gone from stderr by
    /// default, and one environment variable brings it back.
    ///
    /// Gated on `local-model` **and** `NP_TEST_MODEL_PATH`, because the feature is what decides vacuity here: without it nothing loads, so
    /// a default-features row would run this case, see no transcript, and conclude F33 worked when it had merely compiled the loader out.
    /// That is why the guard prints `NOT COVERAGE` rather than skipping quietly.
    #[test]
    fn a_healthy_run_writes_no_transcript_and_the_hatch_brings_it_back() {
        let artifact = artifact_or_not_coverage!();
        let ws = tiny_workspace();
        run(&mut ws.command(&["scan"])).assert_success();
        link_artifact(&ws, &artifact, "task: search result | query: ");
        ws.write_user_config(&local_model_config(EMBED_MODEL));
        run(&mut ws.command(&["embed"])).assert_success();

        let search = |env: &[(&str, &str)]| {
            let mut cmd = ws.command_without_operand(&["search"]);
            cmd.arg(&ws.root).arg("the cat sits on the warm mat");
            for (key, value) in env {
                cmd.env(key, value);
            }
            run(&mut cmd)
        };

        let quiet = search(&[]);
        assert_eq!(quiet.code(), 0, "\n{}", quiet.stderr());
        let transcript = quiet.stderr();
        assert!(
            !transcript.contains("llama_model_loader:"),
            "the default level must not print the load transcript, got {transcript}"
        );
        assert!(
            !transcript.contains("Dumping metadata"),
            "nor the metadata dump that comes with it: {transcript}"
        );
        // Something has to survive, or the loop below grades nothing: a `DEFAULT_LEVEL` of `Off` prints an empty stderr and would pass
        // every absence-assertion in this case. The bound is the measured 424 bytes with room for upstream to grow its warnings (§5).
        assert!(
            transcript.contains("load: ") || transcript.contains("init: "),
            "the default level keeps llama.cpp's warnings, and this artifact emits them: {transcript:?}"
        );
        assert!(
            transcript.len() < 4096,
            "a healthy run's stderr measured 424 bytes; {} means the transcript leaked: {transcript}",
            transcript.len()
        );
        // What *does* survive is warnings only — the difference between a `warn` default and an `off` one. Both prefixes here were read in
        // the pinned tree rather than guessed at: `load:` is `LLAMA_LOG_WARN` from the vocabulary load (`src/llama-vocab.cpp:3072`,
        // `:3403`) and `init:` is `LLAMA_LOG_WARN` from `src/llama-batch.cpp:212`. Asserting a shape rather than a line count means a fifth
        // tokenizer quirk upstream will not trip this case, while a leaked progress fragment (a run of dots at `CONT`) still will.
        for line in transcript.lines() {
            assert!(
                line.starts_with("load: ") || line.starts_with("init: ") || line.is_empty(),
                "the default level keeps llama.cpp's warnings, and nothing else: {transcript}"
            );
        }

        // The hatch, one variable, no rebuild and no config edit.
        let loud = search(&[("NP_LLAMA_LOG_LEVEL", "info")]);
        assert_eq!(loud.code(), 0, "\n{}", loud.stderr());
        let noisy = loud.stderr();
        assert!(
            noisy.contains("llama_model_loader:"),
            "`NP_LLAMA_LOG_LEVEL=info` has to bring the transcript back: {noisy}"
        );

        // And it changes only stderr. A verbosity switch that moved the answer would be a different bug with the same name, so
        // the two runs' stdout is compared rather than each being checked against a guess.
        assert_eq!(
            quiet.stdout(),
            loud.stdout(),
            "the hit list must not depend on how loud the loader was"
        );

        // The file outranks the environment (§4, §12 rule 7): `warn` in `config.toml` beats `info` in the shell. This is the
        // direction the house rule points, so it is pinned rather than left to a code reader's interpretation.
        ws.write_user_config(&format!(
            "{}\nlog_level = \"warn\"\n",
            local_model_config(EMBED_MODEL).trim_end()
        ));
        let outranked = search(&[("NP_LLAMA_LOG_LEVEL", "info")]);
        assert_eq!(outranked.code(), 0, "\n{}", outranked.stderr());
        assert!(
            !outranked.stderr().contains("llama_model_loader:"),
            "a file that named `warn` must not be outvoted by the environment: {}",
            outranked.stderr()
        );
    }

    /// A load that **fails** has to stay explainable at the default level — the risk F33's own silence creates.
    ///
    /// Not unfalsifiable, unlike the version of this case that first lived in np-core: `src/llama.cpp:373-375` catches the loader's throw
    /// and logs `error loading model: …` at error level, so a `warn` default is required to keep that line, and an absent transcript is a
    /// failure rather than an escape hatch. It runs against the real binary because llama.cpp writes to the process's own fd 2, which no
    /// in-process test can capture.
    #[test]
    fn a_failing_load_still_explains_itself_at_the_default_level() {
        let artifact = artifact_or_not_coverage!();
        let ws = tiny_workspace();
        run(&mut ws.command(&["scan"])).assert_success();
        // The config is what makes this a load at all: without it `embed` refuses in configuration and never opens a file, which is how the
        // first run of this case failed — with a correct refusal for a different problem.
        ws.write_user_config(&local_model_config(EMBED_MODEL));

        // A prefix of the real file, recorded under **its own** digest. That is deliberate: D12's verify-on-change would otherwise refuse
        // the swapped bytes before a model was ever opened, and this case is about llama.cpp refusing a GGUF whose tensors run past its
        // end — not about our digest check, which has cases of its own.
        use std::io::{copy, Read as _};
        const KEEP: u64 = 128 * 1024 * 1024;
        let path = ws.models_dir().join(EMBED_MODEL);
        fs::create_dir_all(ws.models_dir()).expect("test setup: models dir");
        // Not `mut`: `Read::take` takes the reader by value, so the binding is moved rather than borrowed.
        let from = fs::File::open(&artifact).expect("test setup: open the artifact");
        let mut to = fs::File::create(&path).expect("test setup: write the prefix");
        copy(&mut from.take(KEEP), &mut to).expect("test setup: copy 128 MiB");
        drop(to);

        let digest = np_core::hash::sha256_file(&path).expect("test setup: digest the prefix");
        let conn = rusqlite::Connection::open(ws.db_path()).expect("test setup: open the index");
        conn.execute(
            "INSERT INTO models (id, dim, sha256, prefix_doc, prefix_query, installed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                EMBED_MODEL,
                REAL_DIM,
                digest,
                "task: search result | query: ",
                "query: ",
                1_730_000_000_i64
            ],
        )
        .expect("test setup: install the row");

        let failed = run(&mut ws.command(&["embed"]));
        assert_eq!(
            failed.code(),
            1,
            "an artifact that cannot be loaded is a runtime refusal:\n{}",
            failed.stderr()
        );
        let stderr = failed.stderr();
        assert!(
            stderr.contains("error loading model"),
            "the default level must keep llama.cpp's own explanation of the failure: {stderr}"
        );
        assert!(
            stderr.contains("np:"),
            "and our refusal has to be on stderr beside it: {stderr}"
        );
    }

    /// Record [`EMBED_MODEL`] as installed, with its bytes **linked** rather than copied into place.
    ///
    /// Written by hand because `np models install` copies, and the point of the link is that `embed` resolves the
    /// workspace's own path to a file it did not write — which is also what a user who placed an artifact there
    /// themselves gets. The digest is the real one because `np embed` now checks it: D12's verify-on-change hashes the
    /// artifact on the first run after install and refuses if it disagrees, so a row that lied here would fail the very
    /// case it belongs to.
    fn link_artifact(ws: &Workspace, artifact: &Path, prefix_doc: &str) {
        fs::create_dir_all(ws.models_dir()).expect("test setup: models dir");
        let link = ws.models_dir().join(EMBED_MODEL);
        std::os::unix::fs::symlink(artifact, &link).expect("test setup: link the artifact");

        let digest = np_core::hash::sha256_file(artifact).expect("test setup: digest the artifact");
        let conn = rusqlite::Connection::open(ws.db_path()).expect("test setup: open the index");
        conn.execute(
            "INSERT INTO models (id, dim, sha256, prefix_doc, prefix_query, installed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                EMBED_MODEL,
                REAL_DIM,
                digest,
                prefix_doc,
                "query: ",
                1_730_000_000_i64
            ],
        )
        .expect("test setup: install the row");
    }

    /// Every vector this model has stored, in chunk order, with the width each row claims.
    fn stored_vectors(db: &Path) -> Vec<(i64, Vec<u8>)> {
        let conn = rusqlite::Connection::open(db).expect("test: open the index");
        let mut statement = conn
            .prepare("SELECT chunk_id, vector FROM embeddings WHERE model = ?1 ORDER BY chunk_id")
            .expect("test: read the vectors");
        statement
            .query_map(rusqlite::params![EMBED_MODEL], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .expect("test: query the vectors")
            .collect::<Result<Vec<_>, _>>()
            .expect("test: collect the vectors")
    }

    /// L2 norm of a little-endian float-32 vector — recomputed here rather than trusted from `np-core`, because
    /// "what is in the database" is the claim and the reader must not share the writer's assumptions.
    fn norm(bytes: &[u8]) -> f64 {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| {
                let value = f32::from_le_bytes([word[0], word[1], word[2], word[3]]) as f64;
                value * value
            })
            .sum::<f64>()
            .sqrt()
    }

    /// D12 through the real binary: an artifact that does not hash to its row is refused **before** anything is
    /// embedded, nothing is written, and no stamp is left behind.
    ///
    /// This case needs no model load — the check runs where the row and the path are both at hand (`embed_provider`),
    /// and the lazy provider means llama.cpp is never reached — so it costs one hash of a few dozen bytes while
    /// pinning the whole point of D12: a same-id swap cannot quietly stamp vectors as made by a model that never ran.
    #[test]
    fn a_swapped_artifact_is_refused_before_anything_is_embedded() {
        let artifact = artifact_or_not_coverage!();
        let ws = tiny_workspace();
        run(&mut ws.command(&["scan"])).assert_success();
        link_artifact(&ws, &artifact, "task: search result | query: ");
        ws.write_user_config(&local_model_config(EMBED_MODEL));

        // Replace the artifact with something else. Different size **and** different bytes, which is the case the
        // stamp notices; the equal-size-same-second swap it cannot see is pinned in np-core's `artifact_verify`.
        let link = ws.models_dir().join(EMBED_MODEL);
        fs::remove_file(&link).expect("test setup: drop the symlink");
        fs::write(&link, b"an artifact of somebody else's").expect("test setup: write an impostor");

        let refused = run(&mut ws.command(&["embed"]));
        assert_ne!(
            refused.code(),
            0,
            "a swapped artifact is not a successful embed"
        );
        let stderr = refused.stderr();
        assert!(
            stderr.contains("hashes to") && stderr.contains("its row says"),
            "the refusal has to name both digests so the reader knows which side moved:\n{stderr}"
        );
        assert!(
            !stderr.contains("task: search result"),
            "and it must not quote model text or note content into an error:\n{stderr}"
        );

        // Nothing was written: no vectors, and no stamp for the bytes that failed. The second half matters — a stamp
        // left by a refused run would be a reason for the next run to skip the check that just caught something.
        let conn = rusqlite::Connection::open(ws.db_path()).expect("test: open the index");
        let vectors: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .expect("read the vector count");
        assert_eq!(vectors, 0, "a refused run writes no vectors");
        let stamped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM models WHERE id = ?1 AND verified_size IS NOT NULL",
                rusqlite::params![EMBED_MODEL],
                |row| row.get(0),
            )
            .expect("read the stamp");
        assert_eq!(stamped, 0, "a refused verification leaves no stamp behind");
    }

    #[test]
    fn a_real_model_writes_vectors_that_the_next_run_calls_current() {
        let artifact = artifact_or_not_coverage!();
        let ws = tiny_workspace();
        run(&mut ws.command(&["scan"])).assert_success();
        link_artifact(&ws, &artifact, "task: search result | query: ");
        ws.write_user_config(&local_model_config(EMBED_MODEL));

        let first = run(&mut ws.command(&["embed"]));
        first.assert_success();
        let embedded = summary_count(&first.stdout(), "embedded");
        assert!(
            embedded >= 2,
            "two notes owe at least two chunks, got {} in:\n{}",
            embedded,
            first.stdout()
        );
        assert_eq!(summary_count(&first.stdout(), "errors"), 0);
        assert_eq!(summary_count(&first.stdout(), "refused"), 0);

        // D12, first run: the row was written straight into the database by this test (no stamp anywhere), so the
        // artifact had to be hashed before a single note was read. This is the ~11 s §14 prices, paid once per change.
        let digest_line = summary_field(&first.stdout(), "digest");
        assert!(
            digest_line.contains("re-checked against models.sha256: ok"),
            "the first embed of an unstamped artifact says it verified the bytes, not that it trusted them:\n{}\n{}",
            digest_line,
            first.stdout()
        );

        let stored = stored_vectors(&ws.db_path());
        assert_eq!(
            stored.len(),
            embedded as usize,
            "one row per embedded chunk"
        );
        for (chunk_id, bytes) in &stored {
            assert_eq!(
                bytes.len(),
                REAL_DIM as usize * 4,
                "chunk {chunk_id} has to hold {REAL_DIM} float-32 components"
            );
            let length = norm(bytes);
            assert!(
                (length - 1.0).abs() < 1e-3,
                "chunk {chunk_id} was stored with ‖v‖ = {length}; §7 normalises, so 1.0 is the claim"
            );
        }

        // The second run must not recompute: `current` is the currency query answering from the stamps, and this
        // is the first time that claim has been made against a real model rather than a fake's digest arithmetic.
        let second = run(&mut ws.command(&["embed"]));
        second.assert_success();
        assert_eq!(
            summary_count(&second.stdout(), "embedded"),
            0,
            "a workspace that owes nothing must not load the model to re-derive vectors it already has"
        );
        assert_eq!(summary_count(&second.stdout(), "current"), embedded);

        // ...and the second run says so without re-reading 295.5 MiB: same size, same mtime as the stamp the first
        // run wrote. The wording is deliberately weaker than "verified" — see `ArtifactCheck`'s note on mtimes.
        let second_digest = summary_field(&second.stdout(), "digest");
        assert!(
            second_digest.contains("not re-read") && second_digest.contains("same size and mtime"),
            "a run that skipped the hash has to admit it skipped:\n{second_digest}"
        );

        // The same fact in `--json`, as a **populated** object. Every other case in this file can only show the key
        // null or absent, which serde reports identically through `Value::is_null`; here the tag and its field are
        // pinned against the artifact's own size, so a renamed arm or a dropped `bytes` fails somewhere real.
        let third = run(&mut ws.command(&["embed", "--json"]));
        third.assert_success();
        let report: serde_json::Value =
            serde_json::from_str(&third.stdout()).expect("`--json` must print one JSON object");
        let on_disk = fs::metadata(&artifact)
            .expect("the artifact is still there")
            .len();
        assert_eq!(
            report["artifact_check"]["unchanged-since-stamp"]["bytes"].as_u64(),
            Some(on_disk),
            "`--json` has to carry the skip with the file's size:\n{}",
            third.stdout()
        );

        // **The worker count this run used**, reported rather than left guessable. `docs/design-m2-thread-policy.md` §4's
        // reason for the field: llama.cpp's own default is a compile-time literal, so once the count is derived from the
        // machine, "how many CPUs did that run get" cannot be read off the source. Pinned in JSON (the shape a reader
        // consumes) and cross-checked against the text summary, because those two channels disagreeing is this project's
        // recurring defect rather than a hypothetical one.
        let workers = &report["workers"];
        let count = workers["count"]
            .as_u64()
            .expect("a local run states its worker count");
        let available = workers["available"]
            .as_u64()
            .expect("and the CPU allowance it derived from");
        // Not `count >= 1`: the type makes zero unrepresentable and the rule cannot return it, so that check could never
        // fire. This one can — it recomputes the rule from the reported denominator, so a report built from a stale or
        // differently-sourced count fails.
        assert_eq!(
            count as usize,
            np_core::config::ThreadCount::Auto.resolve(available as usize),
            "the run configured `auto`, so its worker count must be the rule applied to the machine it reported"
        );
        assert!(
            count <= available,
            "the auto rule must not oversubscribe the machine ({count} workers on {available} CPUs)"
        );
        assert_eq!(
            workers["configured"].as_str(),
            Some("auto"),
            "this run configured no thread count, so the report must not imply it did: {workers}"
        );
        let plain = run(&mut ws.command(&["embed"]));
        // `summary_field` answers the value after the label, so the label is this case's own formatting.
        let expected_value = format!("{count} of {available} available (auto)");
        assert_eq!(
            summary_field(&plain.stdout(), "threads"),
            expected_value,
            "the summary and `--json` must agree about the worker count:\n{}",
            plain.stdout()
        );

        // §3's rule about prefixes turning into lies, measured on real numbers — and the measurement is what found
        // R6c2's P1, so read this block for what it *does not* prove as much as what it does.
        //
        // An earlier draft compared raw bytes before and after a prefix change and **survived** a provider that
        // ignores `prefix_doc` entirely: two processes over the same weights differ in low mantissa bits, so byte
        // inequality is noise. The fix was to measure the floor — wipe the table, re-embed identical input under the
        // unchanged prefix, take that cosine as "as close as the same computation gets" — and then assert something
        // relative: a changed instruction moves the numbers further than recomputing does. Measured here:
        // cos(same) = 1.000000 (to six places; the bytes still differ), cos(changed) = 0.867559.
        //
        // What it does NOT prove is that `LocalEmbedder` applies the prefix, because re-running the same mutant now
        // still passes: cos(changed) = 0.895576 **with `TextKind::Document` ignoring its prefix**. The numbers move
        // anyway, because `embed_run` prepends `prefix_doc` itself before stamping and calling `embed`, and
        // `LoadedModel::embed_text` prepends it a second time. That double application is R6c2's P1 — recorded in
        // `r6c2-findings.md`, with the seam decision it needs — and until it is fixed this assertion pins "the
        // stored numbers depend on `prefix_doc`", which is true, rather than the stronger thing it was written to
        // pin.
        let first_vector = stored.first().expect("test: one stored vector").1.clone();
        clear_embeddings(&ws.db_path());
        run(&mut ws.command(&["embed"])).assert_success();
        let repeat = stored_vectors(&ws.db_path());
        let noise = cosine(
            &first_vector,
            &repeat.first().expect("test: repeated vector").1,
        );

        set_prefix(&ws.db_path(), "describe the note for retrieval | ");
        let third = run(&mut ws.command(&["embed"]));
        third.assert_success();
        assert_eq!(
            summary_count(&third.stdout(), "embedded"),
            embedded,
            "a changed prefix makes every stored vector stale"
        );
        let after = stored_vectors(&ws.db_path());
        assert_eq!(after.len(), stored.len(), "the same chunks, re-embedded");

        let moved = cosine(
            &first_vector,
            &after.first().expect("test: re-prefixed vector").1,
        );
        println!(
            "prefix sensitivity: cos(same prefix, re-run) = {noise:.6}, \
             cos(changed prefix) = {moved:.6} (n = {REAL_DIM})"
        );
        assert!(
            moved < noise - 1e-3,
            "changing `prefix_doc` has to move the numbers further than recomputing them does — \
             same-prefix {noise:.6}, changed-prefix {moved:.6}. A provider that ignores the prefix makes these \
             equal, which is §3's bug and this assertion's whole purpose"
        );

        // And the ordering that says the vectors mean something: a note is closer to itself under a new
        // instruction than to a different note. An ordering, never a score, for the reason `local_embed.rs` gives.
        let (a_id, a_new) = &after[0];
        let b_new = &after
            .iter()
            .find(|(id, _)| id != a_id)
            .expect("test: a second chunk")
            .1;
        let self_similarity = cosine(&first_vector, a_new);
        let other_similarity = cosine(&first_vector, b_new);
        assert!(
            self_similarity > other_similarity,
            "the same note under a new instruction ({self_similarity:.4}) has to stay closer to itself than a \
             different note is ({other_similarity:.4})"
        );
    }

    /// Forget every stored vector, so the next `np embed` recomputes from scratch.
    ///
    /// This is a test reaching into the database, which no command does — §6 keeps writing to `embeddings` the one
    /// route with a currency check. It exists to produce a second measurement of an *identical* computation, which
    /// is what turns "the bytes changed" into a claim about noise.
    fn clear_embeddings(db: &Path) {
        let conn = rusqlite::Connection::open(db).expect("test: open the index");
        conn.execute("DELETE FROM embeddings", [])
            .expect("test: drop the stored vectors");
    }

    /// Point this model's rows at a new document instruction, as someone editing `models` by hand would.
    fn set_prefix(db: &Path, prefix_doc: &str) {
        let conn = rusqlite::Connection::open(db).expect("test: open the index");
        conn.execute(
            "UPDATE models SET prefix_doc = ?1 WHERE id = ?2",
            rusqlite::params![prefix_doc, EMBED_MODEL],
        )
        .expect("test: change the prefix");
    }

    /// Cosine similarity of two little-endian float-32 vectors.
    fn cosine(a: &[u8], b: &[u8]) -> f64 {
        let (a_words, _) = a.as_chunks::<4>();
        let (b_words, _) = b.as_chunks::<4>();
        assert_eq!(a_words.len(), b_words.len(), "two vectors of one width");
        let dot: f64 = a_words
            .iter()
            .zip(b_words)
            .map(|(x, y)| f32::from_le_bytes(*x) as f64 * f32::from_le_bytes(*y) as f64)
            .sum();
        // Both sides are unit-normalised on the way in (asserted above), so this is the cosine itself; dividing
        // anyway keeps the helper honest if a future model stops normalising.
        let norm = |v: &[u8]| -> f64 {
            v.as_chunks::<4>()
                .0
                .iter()
                .map(|w| {
                    let value = f32::from_le_bytes(*w) as f64;
                    value * value
                })
                .sum::<f64>()
                .sqrt()
        };
        dot / (norm(a) * norm(b))
    }
}
