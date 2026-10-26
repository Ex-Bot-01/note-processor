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
use np_core::db::Db;
use np_core::parse::PARSE_VERSION;
use tempfile::TempDir;

/// Markdown notes in the committed corpus (everything but `scan.png`).
const CORPUS_NOTES: u64 = 16;
/// Tracked files of every kind: the notes plus one image.
const CORPUS_FILES: u64 = CORPUS_NOTES + 1;

/// A workspace copy whose data dir lives in the same tempdir.
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
        cmd.env("XDG_DATA_HOME", self.data_dir())
            .env("LOCALAPPDATA", self.data_dir())
            .env("HOME", self._tmp.path())
            .env("XDG_CONFIG_HOME", self.config_dir())
            .env("APPDATA", self.config_dir());
        cmd.args(args).arg(&self.root);
        cmd
    }

    /// The empty directory standing in for the OS config dir.
    fn config_dir(&self) -> PathBuf {
        self._tmp.path().join("config")
    }

    fn data_dir(&self) -> PathBuf {
        self._tmp.path().join("data")
    }

    fn index(&self) -> PathBuf {
        self.root.join("INDEX.md")
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

#[test]
fn missing_data_dir_is_a_configuration_error_not_a_panic() {
    let ws = workspace();
    // No data dir from the environment and no config file to supply one: every path the program
    // would derive would land next to the current directory, so it must refuse instead.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
    // Cleared rather than redirected: on Windows the data dir comes from `%LOCALAPPDATA%` and the config
    // dir from `%APPDATA%`, so an unset XDG pair alone would still find a developer's real directories.
    cmd.env("XDG_DATA_HOME", "")
        .env("LOCALAPPDATA", "")
        .env("HOME", "")
        .env("XDG_CONFIG_HOME", "")
        .env("APPDATA", "")
        .arg("scan")
        .arg(&ws.root);

    let failed = run(&mut cmd);

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
/// write sites made non-fatal.
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
    // The work is committed and still reported where it can be read.
    assert!(
        survived.stdout().contains("index      INDEX.md"),
        "the summary still reaches stdout:\n{}",
        survived.stdout()
    );
    assert!(ws.index().is_file(), "the catalog is still written");
}
