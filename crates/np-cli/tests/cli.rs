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

    /// This workspace's database, derived the way the binary derives it from the same absolute root
    /// [`Workspace::command`] passes — the same route [`record_parse_version`] takes.
    fn db_path(&self) -> PathBuf {
        Config {
            data_dir: self.data_dir(),
            ..Config::default()
        }
        .workspace_db(&self.root)
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
    db.0.execute_batch(&format!(
        "DELETE FROM meta WHERE key = '{NOTE_MERGE_KEY}';
         DELETE FROM schema_version WHERE version = 5;"
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
    db.0.execute_batch(&format!(
        "DELETE FROM meta WHERE key = '{NOTE_MERGE_KEY}';
         DELETE FROM schema_version WHERE version = 5;"
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
    let expected = "unreadable=1;unkeyable=0;duplicates=0;names=lost";
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

    // The same counts on the machine-readable channel, as an object of four fields — including `names`, so a
    // reader gets the path and not only the number.
    let json = run(&mut ws.command(&["scan", "--json"]));
    json.assert_success();
    let report: serde_json::Value =
        serde_json::from_str(&json.stdout()).expect("`--json` must print one JSON object");
    assert_eq!(report["unindexable"]["unreadable"], 1, "{report}");
    assert_eq!(report["unindexable"]["unkeyable"], 0, "{report}");
    assert_eq!(report["unindexable"]["duplicates"], 0, "{report}");
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
