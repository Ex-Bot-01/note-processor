//! The diagnostic sinks, driven from outside: real `np` processes, real descriptors, real files.
//!
//! **Why every case here spawns the binary.** np-cli is a binary crate with no library target, so nothing in this file *can*
//! call `diag::install`. That limitation is the point rather than an obstacle: the claims under test are about descriptor 2 of a
//! process somebody else started, and a seam-level test that called into the sink layer would prove the layer works while leaving
//! open the question anyone actually asks — whether `np` ever hands it anything. The 2026-12-09 round learned that the hard way
//! when twenty-eight green rows coexisted with a feature unreachable from `np serve`. So: `-v` is typed, stderr is read from a
//! file, and the JSONL log is opened with `serde_json` rather than eyeballed.
//!
//! Three properties hold every case together:
//!
//! * **stdout is protocol.** A diagnostic never appears there, and no existing stdout byte changes.
//! * **stderr stays as silent as it was** unless someone asked (`-v`, `NP_LOG`, `[log] level`), and the sentences it already
//!   printed keep their exact bytes — several assertions below are copied from `tests/cli.rs`'s pinned literals rather than
//!   re-derived, so a reword here fails for the same reason it would fail there.
//! * **Diagnostics are never the reason a command fails.** An unopenable log path, a full budget and a deleted file all leave the
//!   exit code and the report untouched.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use np_core::config::Config;
use serde_json::Value;

use tempfile::TempDir;

/// The keys a JSONL record may carry — the frozen set from
/// `docs/superpowers/specs/2026-12-10-np-diagnostics-design.md` point 7 plus the three the same spec mandates: `elapsed_ms`
/// (point "Serve"/timing), and `bytes`/`action` inside the final `log full` record (point 8).
const RECORD_KEYS: [&str; 13] = [
    "ts",
    "level",
    "target",
    "msg",
    "request",
    "workspace",
    "rule",
    "what",
    "expected",
    "actual",
    "elapsed_ms",
    "bytes",
    "action",
];

/// A vault with its own data dir and config dir, so nothing here reads a developer's `config.toml` or writes into their
/// real index. The redirection is the same five variables `tests/cli.rs` sets, for the same reason: Windows takes its config
/// dir from `%APPDATA%` and ignores XDG entirely, so four is not enough.
struct Vault {
    _tmp: TempDir,
    root: PathBuf,
}

impl Vault {
    fn new() -> Self {
        let tmp = TempDir::new().expect("test setup: tempdir");
        let root = tmp.path().join("vault");
        fs::create_dir_all(&root).expect("test setup: vault dir");
        fs::create_dir_all(tmp.path().join("data")).expect("test setup: data dir");
        Self { _tmp: tmp, root }
    }

    /// `n` notes named `note-0.md` … so a case can talk about volume without talking about content.
    /// `np <args> <vault root>` run to completion, so a case reads as one line instead of three lines of temporaries.
    fn exec(&self, args: &[&str]) -> Run {
        let mut cmd = self.command(args);
        cmd.arg(&self.root);
        crate::run(&mut cmd)
    }

    fn write_notes(&self, n: usize) {
        for i in 0..n {
            fs::write(
                self.root.join(format!("note-{i:04}.md")),
                format!("# Note {i}\n\nword number {i}\n"),
            )
            .expect("test setup: note");
        }
    }

    /// `np` with every directory it reads configuration or data from pointed into this tempdir, and the two log variables
    /// **removed** so a developer's shell cannot turn a silent-by-default assertion green by accident. A case that wants one
    /// sets it on its own `Command`, which is also how the reader learns the case depends on it.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
        let config_dir = self._tmp.path().join("config");
        cmd.current_dir(&self.root)
            .env("XDG_DATA_HOME", self._tmp.path().join("data"))
            .env("LOCALAPPDATA", self._tmp.path().join("data"))
            .env("HOME", self._tmp.path())
            .env("XDG_CONFIG_HOME", &config_dir)
            .env("APPDATA", &config_dir)
            .env_remove("NP_MODELS_BASE_URL")
            .env_remove("NP_NO_AUTO_FETCH")
            .env_remove("NP_LOG")
            .env_remove("NP_LLAMA_LOG_LEVEL")
            .env_remove("RUST_LOG");
        cmd.args(args);
        cmd
    }

    /// A user-level `config.toml`, written where the binary actually looks (`note-processor/config.toml` under the OS config dir).
    fn write_user_config(&self, body: &str) {
        let dir = self._tmp.path().join("config").join("note-processor");
        fs::create_dir_all(&dir).expect("test setup: config package dir");
        fs::write(dir.join("config.toml"), body).expect("test setup: write user config");
    }

    /// A config that **refuses to load** for the most common reason an integrator hits: a section name one letter off (**F35**),
    /// which is not a stray key inside a known section but a whole section going unread. Review Focus 1 of the diagnostics plan
    /// is about exactly this file, because it is the failure that must still explain itself.
    fn write_refusing_config(&self) {
        self.write_user_config("[loggg]\nlevel = \"info\"\n");
    }

    /// The `[log]` section pointing at a fresh log file inside this tempdir, with everything else defaulted.
    fn log_config(&self, max_bytes: Option<u64>) -> PathBuf {
        let path = self.log_path();
        let mut body = String::from("[log]\n");
        body.push_str(&format!(
            "file = {}\n",
            toml_str(&path.display().to_string())
        ));
        if let Some(bytes) = max_bytes {
            body.push_str(&format!("max_bytes = {bytes}\n"));
        }
        self.write_user_config(&body);
        path
    }

    /// Where [`Vault::log_config`] would put the log, without writing any configuration — for a case that composes its
    /// own `[log]` section and still has to open the same file afterwards.
    fn log_path(&self) -> PathBuf {
        self._tmp.path().join("np.jsonl")
    }
}

/// One page: the smallest capacity `F_SETPIPE_SZ` accepts, and the size of the pipe
/// [`small_pipe`] hands to [`an_unread_stderr_pipe_never_stalls_the_request_loop`]. Stated as a constant because three
/// assertions read it and one `fcntl` call sets it; if they ever disagree, the case fails rather than quietly measures a
/// different pipe.
const PIPE_BYTES: usize = 4_096;

/// The bound on one diagnostic field, in bytes (`docs/reference-diagnostics.md`, "What may be in a diagnostic").
///
/// Stated here rather than imported because the code's constant is private to np-cli and this file's job is to notice if
/// the two disagree: raise it in `src/diag.rs` without raising this number and the cases below fail, which is the point.
const DIAG_FIELD_BOUND_BYTES: usize = 512;

/// `s` as a TOML basic string; Rust's `Debug` escapes match TOML's for `\` and `"`.
fn toml_str(s: &str) -> String {
    format!("{:?}", s)
}

/// A finished run with its argv in every failure message.
struct Run {
    argv: String,
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    fn lines(&self) -> impl Iterator<Item = String> + '_ {
        self.stderr
            .lines()
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
    }
}

/// Start `np` with **stdout and stderr redirected to files** rather than pipes.
///
/// Not a convenience: a client that never reads the child's stderr is one of the inputs this feature has to survive (Review
/// Focus 5), and a pipe is exactly where that deadlocks at 64 KiB. Files cannot fill, so a case can ask for a firehose without
/// its harness becoming the thing under test.
fn spawn_to_files(cmd: &mut Command, out: &Path, err: &Path) -> Child {
    let argv = format!("{cmd:?}");
    cmd.stdin(Stdio::piped())
        .stdout(fs::File::create(out).unwrap_or_else(|err| panic!("cannot create {out:?}: {err}")))
        .stderr(fs::File::create(err).unwrap_or_else(|err| panic!("cannot create {err:?}: {err}")));
    cmd.spawn()
        .unwrap_or_else(|err| panic!("cannot start `{argv}`: {err}"))
}

/// Run to completion and read both descriptors back as text.
fn run(cmd: &mut Command) -> Run {
    let argv = format!("{cmd:?}");
    let output: Output = cmd
        .output()
        .unwrap_or_else(|err| panic!("cannot start `{argv}`: {err}"));
    // `code` first, then the move: the struct literal's own field order is the borrow checker's evaluation order, and a
    // `panic!` in a later field cannot read `argv` after an earlier field has taken it.
    let code = output
        .status
        .code()
        .unwrap_or_else(|| panic!("`{argv}` was killed by a signal"));
    Run {
        argv,
        code,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Drive one `np serve` child to completion with `requests`, returning what it wrote to each descriptor.
///
/// The requests go in before the responses are read, and stdin closes afterwards, which is the shape of a client that fires a
/// batch and waits: nothing here reads a line at a time, so a server that blocks on either descriptor hangs the case rather than
/// passing it quietly.
fn serve_with(
    requests: &[String],
    mut configure: impl FnMut(&mut Command),
) -> (i32, String, String) {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let out = tmp.path().join("stdout.txt");
    let err = tmp.path().join("stderr.txt");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
    cmd.env_remove("NP_LOG").env_remove("RUST_LOG").arg("serve");
    configure(&mut cmd);
    let mut child = spawn_to_files(&mut cmd, &out, &err);
    {
        let stdin = child.stdin.take().expect("test setup: stdin pipe");
        let mut stdin = std::io::BufWriter::new(stdin);
        for line in requests {
            stdin
                .write_all(line.as_bytes())
                .expect("test setup: write request");
        }
        stdin.flush().expect("test setup: flush requests");
    }
    let status = child.wait().expect("the serve child must exit");
    let mut out_text = String::new();
    let mut err_text = String::new();
    fs::File::open(&out)
        .expect("test setup: read stdout file")
        .read_to_string(&mut out_text)
        .expect("test setup: read stdout");
    fs::File::open(&err)
        .expect("test setup: read stderr file")
        .read_to_string(&mut err_text)
        .expect("test setup: read stderr");
    (status.code().unwrap_or(-1), out_text, err_text)
}

/// One JSON-RPC request line for `method`, with the trailing newline included.
fn request(id: u32, method: &str, params: serde_json::Value) -> String {
    let mut doc = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
    if id != 0 {
        doc["id"] = serde_json::json!(id);
    }
    format!("{doc}\n")
}

/// Parse a JSONL file, reporting the offending line verbatim. A half-written record fails here, which is the whole claim of
/// `concurrent_processes_never_split_a_record`.
fn read_records(path: &Path) -> Vec<serde_json::Value> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read log file {path:?}: {err}"));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("log line is not one JSON object ({err}): {line:?}"))
        })
        .collect()
}

/// Every key of every record must be one the reference documents. The layer passes fields through, so this is what keeps a new
/// field from reaching a published file format without being written down.
fn assert_keys_are_documented(records: &[serde_json::Value], log: &Path) {
    for record in records {
        let object = record
            .as_object()
            .unwrap_or_else(|| panic!("record is not an object: {record}"));
        for key in object.keys() {
            assert!(
                RECORD_KEYS.contains(&key.as_str()),
                "{log:?} carries `{key}`, which is not in the published record keys: {record}"
            );
        }
    }
}

/// **The default must stay exactly as silent as it is today** (spec point 1). Two halves, because "silent" alone would pass a
/// build that silenced pinned output too: nothing new appears, and what already appeared keeps its bytes.
#[test]
fn stderr_stays_silent_at_default_level() {
    let vault = Vault::new();
    vault.write_notes(3);

    let scan = vault.exec(&["scan"]);
    assert_eq!(scan.code, 0, "`{}`: {}", scan.argv, scan.stderr);
    // Captured from the binary at commit 367bdf1 (Task 1's HEAD): a first scan of a fresh vault writes **nothing** to stderr —
    // every summary line is stdout. If this ever stops being empty, the default level became a firehose.
    assert_eq!(
        scan.stderr, "",
        "a default-level scan writes no diagnostics\n{}",
        scan.stderr
    );
    assert!(
        !scan.stderr.contains("np_core::"),
        "no target prefix on stderr"
    );

    // And the sentence that does speak today — copied from `tests/cli.rs`'s `search_with_embedding_disabled_refuses_and_names_the_setting`,
    // which pins it against a literal so a reword fails there too — is still exactly one line, with nothing added before or after.
    // The workspace operand comes first because clap assigns positionals in order: `np search <workspace> <query>`.
    let mut refused = vault.command(&["search"]);
    refused
        .arg(&vault.root)
        .arg("--mode")
        .arg("semantic")
        .arg("alpha");
    let refused = run(&mut refused);
    assert_eq!(
        refused.code, 1,
        "a runtime refusal is still 1: {}",
        refused.stderr
    );
    assert_eq!(
        refused.stderr.trim_end(),
        concat!(
            r#"np: embedding disabled: embeddings.provider = "none" — set it to "local-model" and name an "#,
            "installed model with embeddings.model, then run np embed"
        ),
        "the refusal is this sentence, in full, and nothing else"
    );
    assert_eq!(
        refused.stderr.matches('\n').count(),
        1,
        "{:?}",
        refused.stderr
    );
    assert!(
        !refused.stderr.contains('{'),
        "a serve test forbids a `{{` at the start of any stderr line; keep it true everywhere: {:?}",
        refused.stderr
    );
}

/// **The one line that makes the environment visible** (spec point 2): `-v` says the effective level *and* where it came from,
/// because "I set `NP_LOG` and nothing happened" is otherwise indistinguishable from a binary that ignores it.
#[test]
fn verbose_prints_the_effective_level_and_its_source() {
    struct Case {
        name: &'static str,
        args: &'static [&'static str],
        env: Option<(&'static str, &'static str)>,
        /// A `[log] level` line for the config-file case.
        file_level: Option<&'static str>,
        expect: &'static str,
    }
    let cases = [
        Case {
            name: "flag",
            args: &["-v"],
            env: None,
            file_level: None,
            expect: "log level info from command line",
        },
        Case {
            name: "two -v",
            args: &["-vv"],
            env: None,
            file_level: None,
            expect: "log level debug from command line",
        },
        Case {
            name: "three -v",
            args: &["-vvv"],
            env: None,
            file_level: None,
            expect: "log level trace from command line",
        },
        Case {
            name: "environment",
            args: &[],
            env: Some(("NP_LOG", "debug")),
            file_level: None,
            expect: "log level debug from NP_LOG",
        },
        Case {
            name: "config file",
            args: &[],
            env: None,
            file_level: Some("warn"),
            expect: "log level warn from config.toml",
        },
        Case {
            // The house rule in its printed form: the file wins, and the line says which one won — so a reader who set `NP_LOG`
            // and wonders can see the answer without reading §12.
            name: "file beats environment",
            args: &[],
            env: Some(("NP_LOG", "trace")),
            file_level: Some("info"),
            expect: "log level info from config.toml",
        },
        Case {
            name: "a named spec",
            args: &["--log-spec", "np_core::search=trace"],
            env: None,
            file_level: None,
            expect: "log level warn from command line",
        },
    ];

    for case in cases {
        let vault = Vault::new();
        vault.write_notes(1);
        if let Some(level) = case.file_level {
            vault.write_user_config(&format!("[log]\nlevel = \"{level}\"\n"));
        }
        let mut cmd = vault.command(&["scan"]);
        for arg in case.args {
            cmd.arg(arg);
        }
        cmd.arg(&vault.root);
        if let Some((key, value)) = case.env {
            cmd.env(key, value);
        }
        let scanned = run(&mut cmd);
        assert_eq!(scanned.code, 0, "[{}] {}", case.name, scanned.stderr);
        let first = scanned.stderr.lines().next().unwrap_or_else(|| {
            panic!(
                "[{}] expected an announce line, stderr was empty",
                case.name
            )
        });
        assert_eq!(first, case.expect, "[{}]: {:?}", case.name, scanned.stderr);
        assert!(
            !scanned.stderr.contains('{'),
            "[{}] no JSON on stderr: {:?}",
            case.name,
            scanned.stderr
        );
    }

    // And when nobody asked, there is no announce line either — the silence of the case above was total.
    let vault = Vault::new();
    vault.write_notes(1);
    let plain = vault.exec(&["scan"]);
    assert_eq!(plain.code, 0, "{}", plain.stderr);
    assert!(!plain.stderr.contains("log level "), "{:?}", plain.stderr);
}

/// **The stderr cap** (spec point 8): 1 000 diagnostic lines per process, checked before formatting, then one suppression
/// notice naming what to do instead.
///
/// The firehose here is a `np serve` on a config that refuses (**Review Focus 1**'s input) with each request reporting it at
/// `error`: 1 200 requests, so 1 200 real diagnostics reach one process's descriptor 2. It is a per-request source rather than
/// the per-note one the plan sketched, because no per-note event exists until Task 3 instruments scan — recorded in the ledger
/// so the substitution is visible rather than silent.
#[test]
fn stderr_caps_at_one_thousand_lines() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).expect("test setup: vault dir");
    fs::write(root.join("a.md"), "# Alpha\n\nword\n").expect("test setup: note");
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(config_dir.join("note-processor")).expect("test setup: config dir");
    // `[loggg]` is F35's own failure shape: a section name one letter off, so the whole section goes unread and the file refuses.
    fs::write(
        config_dir.join("note-processor").join("config.toml"),
        "[loggg]\nlevel = \"info\"\n",
    )
    .expect("test setup: write refusing config");

    let asks = 1_200;
    let requests: Vec<String> = (1..=asks)
        .map(|i| request(i, "scan", serde_json::json!({"workspace": root})))
        .collect();
    let (code, out_text, err_text) = serve_with(&requests, |cmd| {
        cmd.env("XDG_DATA_HOME", tmp.path().join("data"))
            .env("LOCALAPPDATA", tmp.path().join("data"))
            .env("HOME", tmp.path())
            .env("XDG_CONFIG_HOME", &config_dir)
            .env("APPDATA", &config_dir)
            .env("NP_LOG", "error");
    });

    // The server answered every request, cap or no cap: suppression touches logs, never the protocol.
    assert_eq!(code, 0, "a serve run ends cleanly; stderr was {err_text:?}");
    let answers = out_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert_eq!(
        answers, asks as usize,
        "every request still gets its response"
    );
    for line in out_text.lines().filter(|l| !l.trim().is_empty()) {
        let doc: serde_json::Value =
            serde_json::from_str(line).expect("stdout stays one JSON object per line");
        assert!(
            doc.get("error").is_some(),
            "a refusing config is a refusal per request: {doc}"
        );
    }

    let diagnostics = err_text
        .lines()
        .filter(|line| line.starts_with("error np_core::config:"))
        .count();
    assert!(
        diagnostics <= 1_000,
        "stderr printed {diagnostics} diagnostics; the cap is 1 000 per process"
    );
    assert!(
        diagnostics > 900,
        "and it printed nearly all of them — a cap that fires at 3 lines is a different bug: {diagnostics}"
    );
    let notices = err_text
        .lines()
        .filter(|line| line.contains("diagnostics truncated after 1000 lines"))
        .count();
    assert_eq!(notices, 1, "exactly one suppression notice:\n{err_text}");
    let notice = err_text
        .lines()
        .find(|line| line.contains("diagnostics truncated after 1000 lines"))
        .expect("one notice");
    assert!(
        notice.contains("--log-spec") && notice.contains("[log] file"),
        "the notice has to name both ways out of the firehose: {notice}"
    );
    assert!(
        !err_text.contains('{'),
        "capped or not, stderr never turns into JSON"
    );
}

/// **The file sink and its byte budget** (spec point 7 and 8): one JSON object per line, keys only from the published set,
/// and past `max_bytes` it stops and says so in one final record rather than rotating behind the reader's back.
#[test]
fn the_file_holds_jsonl_and_stops_at_its_budget() {
    let vault = Vault::new();
    vault.write_notes(4);
    let log = vault.log_config(Some(40));

    let scanned = vault.exec(&["scan"]);
    assert_eq!(
        scanned.code, 0,
        "a full log never fails a command: {}",
        scanned.stderr
    );
    assert!(
        !scanned.stdout.contains('{'),
        "stdout is the report, not JSON"
    );
    assert!(
        !scanned.stderr.contains('{'),
        "stderr is human text: {:?}",
        scanned.stderr
    );

    let records = read_records(&log);
    assert!(
        !records.is_empty(),
        "a configured file must hold something, or `[log] file` is a no-op"
    );
    assert_keys_are_documented(&records, &log);

    let last = records.last().expect("at least one record");
    assert_eq!(
        last.get("msg").and_then(Value::as_str),
        Some("log full"),
        "the budget's last word is the honest stop: {last}"
    );
    assert!(
        last.get("bytes").is_some() && last.get("action").is_some(),
        "and it says how full and what to do: {last}"
    );
    let before_full = records.len() - 1;
    assert!(
        before_full >= 1,
        "something was written before the budget ran out"
    );
    // Every record before the stop is a real diagnostic with the three keys that make a line readable on their own.
    for record in &records[..before_full] {
        assert!(
            record.get("level").and_then(Value::as_str).is_some(),
            "{record}"
        );
        assert!(
            record.get("target").and_then(Value::as_str).is_some(),
            "{record}"
        );
        assert!(
            record.get("ts").and_then(Value::as_i64).is_some(),
            "{record}"
        );
    }

    // The budget latches: a second run against the same file writes nothing more, because "delete the file to continue" is the
    // documented remedy and appending past it would be a slow lie.
    let size_after_first = fs::metadata(&log).expect("test setup: stat log").len();
    let again = vault.exec(&["scan"]);
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert_eq!(
        fs::metadata(&log)
            .expect("test setup: stat log again")
            .len(),
        size_after_first,
        "a full log stays full until someone deletes it"
    );
}

/// **A refusal reaches the file as a record with a `rule`, not only as a sentence.** This is Goal 1 of the design in one
/// assertion: a frontend logs *why* without parsing prose. It also proves the file sink captures events at all — the budget case
/// above can be satisfied by the opening record alone, which a mutation that ignores `[log] file` when deciding what to capture
/// passed straight through until this case existed.
///
/// The refusing config carries its own `[log]` table on purpose: the sink is installed from a lenient peek at that table before
/// the file is validated (spec point 3), so the most common integrator failure is exactly the case that must still produce a
/// diagnostic — and it may not be the one failure with nowhere to put it.
#[test]
fn a_refusal_reaches_the_file_as_a_record_with_its_rule() {
    let vault = Vault::new();
    vault.write_notes(2);
    let log = vault._tmp.path().join("nested").join("np.jsonl");
    vault.write_user_config(&format!(
        "[loggg]\nlevel = \"info\"\n[log]\nfile = {}\n",
        toml_str(&log.display().to_string())
    ));

    let scanned = vault.exec(&["scan"]);
    assert_eq!(
        scanned.code, 1,
        "the config still refuses, and says so: {}",
        scanned.stderr
    );
    assert!(
        scanned.stderr.contains("loggg"),
        "the sentence is unchanged and names the key: {}",
        scanned.stderr
    );

    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    let refusal = records
        .iter()
        .find(|record| record.get("rule").is_some())
        .unwrap_or_else(|| panic!("no record carried a rule: {records:?}"));
    assert_eq!(
        refusal.get("rule").and_then(Value::as_str),
        Some("config-refused"),
        "the machine field, not the sentence: {refusal}"
    );
    assert_eq!(
        refusal.get("target").and_then(Value::as_str),
        Some("np_core::config")
    );
    assert_eq!(refusal.get("level").and_then(Value::as_str), Some("error"));
    let msg = refusal
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        msg.contains("config.toml") && msg.contains("loggg"),
        "the human sentence travels with the field: {msg}"
    );
    assert!(
        !serde_json::to_string(refusal).unwrap().contains("http"),
        "§12 rule 8: a diagnostic carries no origin, even in a path-shaped field: {refusal}"
    );
}

/// **An unopenable path is a warning, not a failure** (spec point 7): diagnostics are never the reason a command fails, the
/// same rule `scan`'s catalog write and `db`'s durable record already follow.
#[test]
fn an_unopenable_log_path_warns_once_and_the_command_succeeds() {
    let vault = Vault::new();
    vault.write_notes(2);
    // A directory where the log file should be: `create_dir_all(parent)` succeeds, and opening it as a file cannot.
    let blocked = vault._tmp.path().join("np.jsonl");
    fs::create_dir(&blocked).expect("test setup: occupy the log path with a directory");
    vault.write_user_config(&format!(
        "[log]\nfile = {}\n",
        toml_str(&blocked.display().to_string())
    ));

    let scanned = vault.exec(&["scan"]);
    assert_eq!(scanned.code, 0, "the scan still runs: {}", scanned.stderr);
    assert!(
        scanned.stdout.contains("added"),
        "and still prints its report: {}",
        scanned.stdout
    );

    let warnings = scanned
        .lines()
        .filter(|line| line.contains("could not be opened"))
        .collect::<Vec<_>>();
    assert_eq!(
        warnings.len(),
        1,
        "exactly one warning:\n{}",
        scanned.stderr
    );
    assert!(
        warnings[0].contains(&blocked.display().to_string()),
        "and it names the path that refused: {}",
        warnings[0]
    );
    assert_eq!(
        scanned.stderr.lines().count(),
        1,
        "one warning and nothing else on stderr:\n{}",
        scanned.stderr
    );
}

/// **A config that refuses must not be the one failure with no diagnostic** (spec point 3, Review Focus 1): `np serve` starts,
/// answers every request it can, reports the refusal per request, and keeps its stdout pure.
#[test]
fn a_bad_config_reports_but_still_serves() {
    let vault = Vault::new();
    vault.write_notes(2);
    vault.write_refusing_config();

    // Asked for nothing: today's bytes. The server starts, `initialize` answers, the request that needs configuration is a
    // refusal whose message names the broken file, and a later request still gets an answer.
    let requests = vec![
        request(1, "initialize", serde_json::json!({"protocol": 1})),
        request(2, "scan", serde_json::json!({"workspace": vault.root})),
        // `initialize` needs no configuration — it answers from the build's own facts — so a second one proves the session is
        // alive *and* well, not merely still replying with faults.
        request(3, "initialize", serde_json::json!({"protocol": 1})),
    ];
    let (code, out_text, err_text) = serve_with(&requests, |cmd| {
        cmd.env("XDG_DATA_HOME", vault._tmp.path().join("data"))
            .env("LOCALAPPDATA", vault._tmp.path().join("data"))
            .env("HOME", vault._tmp.path())
            .env("XDG_CONFIG_HOME", vault._tmp.path().join("config"))
            .env("APPDATA", vault._tmp.path().join("config"));
    });
    assert_eq!(
        code, 0,
        "a bad config does not end the session: {err_text:?}"
    );
    assert!(
        err_text.is_empty(),
        "unasked, it prints no diagnostics: {err_text:?}"
    );

    let answers: Vec<serde_json::Value> = out_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("stdout stays one object per line"))
        .collect();
    assert_eq!(answers.len(), 3, "one response per request: {out_text}");
    assert!(
        answers[0].get("result").is_some(),
        "`initialize` needs no configuration: {}",
        answers[0]
    );
    let refused = &answers[1];
    assert_eq!(
        refused["error"]["code"].as_i64(),
        Some(-32000),
        "a refusal is the code an app already handles: {refused}"
    );
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("config.toml") && message.contains("loggg"),
        "and it names the file and the key that broke: {message}"
    );
    assert!(
        answers[2].get("result").is_some(),
        "a refusal is one request's bad day, not the end of the session: {}",
        answers[2]
    );

    // Asked, in the same shape: the refusal also reaches the diagnostic channel, at `error`, with its rule and nothing else new.
    let (code, _out, err_text) = serve_with(&requests, |cmd| {
        cmd.env("XDG_DATA_HOME", vault._tmp.path().join("data"))
            .env("LOCALAPPDATA", vault._tmp.path().join("data"))
            .env("HOME", vault._tmp.path())
            .env("XDG_CONFIG_HOME", vault._tmp.path().join("config"))
            .env("APPDATA", vault._tmp.path().join("config"))
            .env("NP_LOG", "error");
    });
    assert_eq!(code, 0, "{err_text:?}");
    let reported = err_text
        .lines()
        .filter(|line| line.starts_with("error np_core::config:"))
        .count();
    assert_eq!(reported, 1, "one diagnostic for one refusal:\n{err_text}");
    assert!(
        err_text.contains("config-refused"),
        "the machine field is what a frontend logs instead of parsing prose:\n{err_text}"
    );
    assert!(
        !err_text.contains("loggg\n"),
        "a diagnostic names the key, and never echoes the file:\n{err_text}"
    );
}

/// **Two processes, one file** (Review Focus 2): a serve child plus a concurrent command. Interleaving is fine; a half-written
/// JSON line is not, because the reader's parser cannot tell a lost record from a corrupt log.
#[test]
fn concurrent_processes_never_split_a_record() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).expect("test setup: vault dir");
    for i in 0..30 {
        fs::write(root.join(format!("n{i}.md")), format!("# N{i}\n\nword\n"))
            .expect("test setup: note");
    }
    let log = tmp.path().join("shared.jsonl");
    // The reason both children have something to write: a config that refuses (**F35**), so every request reports it. And both
    // get the log path from `--log-file`, because a file that cannot load cannot configure its own sink — which is precisely
    // why the flag has to be enough on its own.
    fs::create_dir_all(tmp.path().join("config/note-processor")).expect("test setup: config dir");
    fs::write(
        tmp.path().join("config/note-processor/config.toml"),
        "[loggg]\nlevel = \"info\"\n",
    )
    .expect("test setup: write refusing config");
    let spawn_serve = |requests: Vec<String>| {
        let out = tmp.path().join(format!("serve-{}-out.txt", requests.len()));
        let err = tmp.path().join(format!("serve-{}-err.txt", requests.len()));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
        cmd.arg("serve")
            .env_remove("NP_LOG")
            .env("XDG_DATA_HOME", tmp.path().join("data"))
            .env("LOCALAPPDATA", tmp.path().join("data"))
            .env("HOME", tmp.path())
            .env("XDG_CONFIG_HOME", tmp.path().join("config"))
            .env("APPDATA", tmp.path().join("config"))
            .env("NP_LOG", "error")
            .arg("--log-file")
            .arg(&log);
        let mut child = spawn_to_files(&mut cmd, &out, &err);
        {
            let stdin = child.stdin.take().expect("test setup: stdin pipe");
            let mut stdin = std::io::BufWriter::new(stdin);
            for line in requests {
                stdin.write_all(line.as_bytes()).expect("test setup: write");
            }
            stdin.flush().expect("test setup: flush");
        }
        child
    };

    // One serve child on a refusing config writes one record per request; two of them, concurrently, is the interleaving.
    let mut first = spawn_serve(
        (1..=200)
            .map(|i| request(i, "scan", serde_json::json!({"workspace": root})))
            .collect(),
    );
    let mut second = spawn_serve(
        (201..=400)
            .map(|i| request(i, "scan", serde_json::json!({"workspace": root})))
            .collect(),
    );
    assert!(first.wait().expect("child 1 exits").success(), "child 1");
    assert!(second.wait().expect("child 2 exits").success(), "child 2");

    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    assert!(
        records.len() >= 400,
        "400 refusals across two processes should leave at least that many records, got {}",
        records.len()
    );
}

/// **A diagnostic on stderr is short enough for `poll`'s answer to mean what it says.** `POLLOUT` means "a write will be
/// accepted now" — one byte is enough for that — so a line longer than the smallest pipe a client can give us (one page, on
/// Linux) could still wait inside `write_all`, which is the exact stall the guard exists to remove. So stderr's copy of a
/// record is bounded at a page and marked with `…`, while the file keeps the whole thing: the channel that can block is the
/// one that gets the bound, and the reader who wants the entire TOML parse error has it in JSONL.
#[test]
fn a_stderr_line_is_short_enough_for_the_pipe_that_has_to_take_it() {
    let vault = Vault::new();
    vault.write_notes(2);
    // A key long enough that quoting it dwarfs a page: 6 000 characters of name, which the parse error reproduces in full.
    let giant = "k".repeat(6_000);
    let log = vault.log_path();
    vault.write_user_config(&format!(
        "[log]\nfile = {}\n\n[{giant}]\nx = 1\n",
        toml_str(&log.display().to_string())
    ));

    let scanned = vault.exec(&["-v", "scan"]);
    assert_eq!(scanned.code, 1, "a refused configuration still refuses");

    let err = &scanned.stderr;
    let start = err
        .find("error np_core::config:")
        .unwrap_or_else(|| panic!("the refusal should reach an asked-for stderr: {err}"));
    // The pinned refusal is main's own output, not a diagnostic, and keeps its bytes whatever this bound does.
    let end = err[start..]
        .find("np: configuration error:")
        .map(|offset| start + offset)
        .unwrap_or(err.len());
    let written = err[start..end].trim_end();
    assert!(
        written.ends_with('…'),
        "the stderr copy had to be cut, and a cut has to say so: {written}"
    );
    // The newline shares the page with the text, which is why the comparison is `<` and not `<=`.
    assert!(
        written.len() < PIPE_BYTES,
        "stderr was handed {} bytes plus a newline, more than the {PIPE_BYTES}-byte pipe a client may give us",
        written.len()
    );

    let records = read_records(&log);
    let refused = records
        .iter()
        .find(|record| record.get("rule").and_then(Value::as_str) == Some("config-refused"))
        .unwrap_or_else(|| panic!("the refusal belongs in the file too: {records:?}"));
    let msg = refused
        .get("msg")
        .and_then(Value::as_str)
        .expect("a refusal names itself");
    assert!(
        msg.len() > 8_000,
        "the file keeps the whole sentence, not a page of it: {} bytes",
        msg.len()
    );
    assert!(!msg.ends_with('…'), "and nothing was cut from it: {msg}");
    let kept = written["error np_core::config:".len()..]
        .trim_end()
        .trim_end_matches('…')
        .trim_start();
    assert!(
        msg.starts_with(kept),
        "the two channels disagree about the same record:\nstderr: {kept}\nfile: {}",
        &msg[..200.min(msg.len())]
    );
}

/// **The budget belongs to the file, not to the process** (spec point 8; `docs/reference-config.md` bounds *the file*). A sink
/// that counted only its own appends would let two `np` processes each write a whole budget and leave a log twice as long as
/// the setting promises. Each sink re-reads the file's size before it latches and every 16 appends between times, so a writer
/// stops near the size the *file* reached rather than the size it personally produced. The slack is fifteen records per
/// concurrent writer — the gap between another process's append and this one's `stat` — which is why the bound below is
/// budget + 25 % and not budget: a counter-only implementation leaves ~240 KiB here, because each of these two runs has well
/// over a budget's worth of records to write.
#[test]
fn two_processes_share_one_log_budget() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let config = tmp.path().join("config");
    fs::create_dir_all(config.join("note-processor")).expect("test setup: config dir");
    let log = tmp.path().join("shared.jsonl");
    const BUDGET: u64 = 65_536;
    // A configuration that *loads*, so `[log] max_bytes` reaches the sink through the ordinary path rather than through the
    // lenient peek a refusing file has to fall back on.
    fs::write(
        config.join("note-processor").join("config.toml"),
        format!(
            "[log]\nlevel = \"debug\"\nfile = {}\nmax_bytes = {BUDGET}\n",
            toml_str(&log.display().to_string())
        ),
    )
    .expect("test setup: write config");

    // Two vaults, because a workspace's writer lock is exactly one process deep: what is under test is two writers on the
    // *log*, not two writers on one catalog. Non-markdown files yield one deterministic debug record each and nothing else.
    let mut children = Vec::new();
    for letter in ['a', 'b'] {
        let root = tmp.path().join(format!("vault-{letter}"));
        fs::create_dir_all(&root).expect("test setup: vault dir");
        for i in 0..1_200 {
            fs::write(root.join(format!("p{i}.png")), "x\n").expect("test setup: file");
        }
        let out = tmp.path().join(format!("scan-{letter}-out.txt"));
        let err = tmp.path().join(format!("scan-{letter}-err.txt"));
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
        cmd.arg("scan")
            .arg(&root)
            .env_remove("NP_LOG")
            .env("XDG_DATA_HOME", tmp.path().join("data"))
            .env("LOCALAPPDATA", tmp.path().join("data"))
            .env("HOME", tmp.path())
            .env("XDG_CONFIG_HOME", &config)
            .env("APPDATA", &config);
        // Both streams to files, so no child can be slowed by a pipe nobody reads: the only limit that should bind is the one
        // under test.
        children.push(spawn_to_files(&mut cmd, &out, &err));
    }
    for (i, child) in children.iter_mut().enumerate() {
        let status = child.wait().expect("child exits");
        assert!(status.success(), "scan {i} exited {status}");
    }

    let size = fs::metadata(&log).expect("the log exists").len();
    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    assert!(
        size <= BUDGET + BUDGET / 4,
        "two writers left {size} bytes against a {BUDGET}-byte budget: the limit is the file's, not each process's",
    );
    assert!(
        records.len() >= 100,
        "the budget should have stopped the writers after real work, not at once: {} records",
        records.len()
    );
    assert!(
        records
            .iter()
            .any(|record| record.get("msg").and_then(Value::as_str) == Some("log full")),
        "a budget spent by two writers still ends with the record that says so: {:?}",
        records.last()
    );
}

/// **Deleting the log mid-run is documented as safe** (Review Focus 4): writes stop or continue, but the command does not fail,
/// panic, or lose its report — and the next run recreates the file, which is what makes "delete it" a real remedy.
///
/// Unix-only on purpose: unlinking a file another process still holds open is what POSIX allows and Windows does not — the
/// same asymmetry spec point 8 cites when it refuses rotation. The case would fail at setup there rather than test anything.
#[cfg(unix)]
#[test]
fn deleting_the_log_file_mid_run_does_not_fail_the_command() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).expect("test setup: vault dir");
    fs::write(root.join("a.md"), "# Alpha\n\nword\n").expect("test setup: note");
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(config_dir.join("note-processor")).expect("test setup: config dir");
    fs::write(
        config_dir.join("note-processor").join("config.toml"),
        "[loggg]\nlevel = \"info\"\n",
    )
    .expect("test setup: write refusing config");
    let log = tmp.path().join("np.jsonl");

    let out = tmp.path().join("stdout.txt");
    let err = tmp.path().join("stderr.txt");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
    cmd.arg("serve")
        .env_remove("NP_LOG")
        .env("XDG_DATA_HOME", tmp.path().join("data"))
        .env("LOCALAPPDATA", tmp.path().join("data"))
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("APPDATA", &config_dir)
        .env("NP_LOG", "error")
        .arg("--log-file")
        .arg(&log);
    let mut child = spawn_to_files(&mut cmd, &out, &err);
    {
        let mut stdin = child.stdin.take().expect("test setup: stdin pipe");
        stdin
            .write_all(request(1, "scan", serde_json::json!({"workspace": root})).as_bytes())
            .expect("test setup: write first request");
        stdin.flush().expect("test setup: flush");
        // Wait for the record to exist before pulling the file out from under the process, so this is a delete *during* a run
        // rather than before it. Polling with a deadline: a hung child must fail the case, not hang it.
        let started = std::time::Instant::now();
        while !log.exists() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(20),
                "the child never opened its log file"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        fs::remove_file(&log).expect("test setup: delete the log mid-run");
        for i in 2..=40 {
            stdin
                .write_all(request(i, "scan", serde_json::json!({"workspace": root})).as_bytes())
                .expect("test setup: write after the delete");
        }
        stdin.flush().expect("test setup: flush");
    }
    let status = child.wait().expect("the child must still exit");
    assert!(
        status.success(),
        "deleting the log must not fail the command"
    );
    let mut err_text = String::new();
    fs::File::open(&err)
        .expect("test setup: read stderr")
        .read_to_string(&mut err_text)
        .expect("test setup: read stderr 2");
    assert!(
        !err_text.contains("panicked"),
        "no panic on a vanished file:\n{err_text}"
    );

    // And the file comes back: the next run recreates it and writes into it. The refusing config is removed first because this
    // step is about the *file*, and a run that dies on configuration would prove nothing about it either way.
    fs::remove_file(config_dir.join("note-processor").join("config.toml"))
        .expect("test setup: remove refusing config");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
    cmd.env("XDG_DATA_HOME", tmp.path().join("data"))
        .env("LOCALAPPDATA", tmp.path().join("data"))
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("APPDATA", &config_dir)
        .env_remove("NP_LOG")
        .arg("--log-file")
        .arg(&log)
        .arg("scan")
        .arg(&root);
    let after = run(&mut cmd);
    assert_eq!(after.code, 0, "{}", after.stderr);
    let records = read_records(&log);
    assert!(
        !records.is_empty(),
        "a deleted log is recreated by the next run, not left missing"
    );
    assert_keys_are_documented(&records, &log);
}

// ---------------------------------------------------------------------------
// Task 3: the failure rules, on the channels a reader actually opens
// ---------------------------------------------------------------------------
//
// `crates/np-core/tests/diagnostics.rs` proves each arm *emits* its rule to whatever subscriber is installed. That is
// the seam; it is not the channel. The claims that matter to an integrator are about a file they configured and a
// process they started, so the two states whose rules were named in the plan — a refusal and a lock — are asserted here
// as JSONL records with real bytes in them, plus the privacy rule over a whole run at the loudest level `np` offers.

/// Plan Task 3 Step 1's case, on the file channel: an embed with embeddings switched off writes one record whose `rule`
/// is `provider-disabled`, at `warn`, on the embed target, naming the key to edit. The default level (`warn`) has to be
/// enough — a refusal that only appears when somebody thought to ask for verbosity is the failure this feature exists to
/// close, and every existing case above already proves the run itself stays byte-identical.
#[test]
fn an_embed_refusal_reaches_the_file_with_its_rule() {
    let vault = Vault::new();
    vault.write_notes(2);
    let log = vault.log_config(None);

    // Fetching is switched off so the run cannot depend on a network: with `provider = "none"` arm (ii) refuses before
    // any transport is reached, and this asserts that it does so *without* dialing out.
    let embed = {
        let mut cmd = vault.command(&["embed"]);
        cmd.env("NP_NO_AUTO_FETCH", "1").arg(&vault.root);
        run(&mut cmd)
    };
    assert_eq!(
        embed.code, 1,
        "vectors were asked for and none exist: {}",
        embed.stderr
    );

    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    let refusals: Vec<&Value> = records
        .iter()
        .filter(|record| record.get("rule").is_some())
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "one refusal, one record — a reader deduplicates nothing: {refusals:?}"
    );
    let refusal = refusals[0];
    assert_eq!(
        refusal.get("rule").and_then(Value::as_str),
        Some("provider-disabled"),
        "the token, not the sentence: {refusal}"
    );
    assert_eq!(
        refusal.get("target").and_then(Value::as_str),
        Some("np_core::embed")
    );
    assert_eq!(
        refusal.get("level").and_then(Value::as_str),
        Some("warn"),
        "a state to fix is not a fault (§4 of the design): {refusal}"
    );
    let what = refusal
        .get("what")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        what.contains("embeddings.provider"),
        "the same key the stderr sentence names: {refusal}"
    );
}

/// Plan Task 3 Step 3's lock case: `rule: "locked"` with `what` naming **the workspace**, because a log reader has to be
/// able to tell which vault was busy. The lock is held from this test process for the reason `tests/cli.rs` gives — POSIX
/// record locks are per process, so an in-process second connection would not contend and the case would pass either way.
#[test]
fn a_locked_workspace_reports_the_rule_and_its_path() {
    let vault = Vault::new();
    vault.write_notes(2);
    let log = vault.log_config(None);
    assert_eq!(vault.exec(&["scan"]).code, 0, "the vault is indexed first");
    fs::remove_file(&log).expect("test setup: start the assertion from an empty log");

    let db = Config {
        data_dir: np_data_dir(&vault._tmp.path().join("data")),
        ..Config::default()
    }
    .workspace_db(&vault.root);
    let dir = db.parent().expect("test setup: the database has a parent");
    let stem = db
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("test setup: utf-8 stem");
    let _held =
        np_core::lock::WorkspaceLock::acquire(dir, stem, std::time::Duration::from_millis(300))
            .expect("test setup: take the lock the way a running `np` would");

    let refused = vault.exec(&["--log-file", log.to_str().expect("utf-8 path"), "scan"]);
    assert_eq!(
        refused.code, 1,
        "a held workspace refuses: {}",
        refused.stderr
    );

    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    let locked = records
        .iter()
        .find(|record| record.get("rule").and_then(Value::as_str) == Some("locked"))
        .unwrap_or_else(|| panic!("no `locked` record in {records:?}"));
    assert_eq!(
        locked.get("target").and_then(Value::as_str),
        Some("np_core::lock")
    );
    assert_eq!(locked.get("level").and_then(Value::as_str), Some("warn"));
    let what = locked
        .get("what")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        what.contains(
            vault
                .root
                .file_name()
                .and_then(|n| n.to_str())
                .expect("utf-8 name")
        ),
        "`what` must name the workspace a reader can act on: {locked}"
    );
    // Both numbers, so "busy" and "broken" stay distinguishable from the record alone: how long the run was prepared to
    // wait, and how long it did.
    let waited = locked.get("actual").and_then(Value::as_u64).unwrap_or(0);
    let willing = locked.get("expected").and_then(Value::as_u64).unwrap_or(0);
    assert!(
        waited > 0 && waited >= willing,
        "waited {waited} ms of a {willing} ms allowance: {locked}"
    );
}

/// Plan Task 3 Step 4's privacy rule, run at `trace` with every command that can produce a diagnostic. A note's body and
/// an origin host are the two things §12 forbids, and the vault below is built so both would be findable if either ever
/// travelled: the body is one unusual string in every chunk, and the only link in the vault names a host that exists
/// nowhere but in this file.
#[test]
fn a_diagnostic_never_carries_note_text_or_an_origin() {
    let vault = Vault::new();
    let body = "quixotic-vaultphrase-never-log-me";
    fs::write(
        vault.root.join("note-0.md"),
        format!("# Note\n\n{body} appears twice: {body}\n\n[seen elsewhere](https://origin.invalid/secret-path)\n"),
    )
    .expect("test setup: write a note holding both forbidden things");
    fs::write(
        vault.root.join("note-1.md"),
        "# Other\n\nnothing unusual here, only prose about indexing\n",
    )
    .expect("test setup: second note");
    let log = vault.log_config(None);
    vault.write_user_config(&format!(
        "[log]\nfile = {}\nlevel = \"trace\"\n",
        toml_str(&log.display().to_string())
    ));

    // Each command's own exit code, unchanged by the presence of a log: `embed` asked for vectors and got none (arm (ii),
    // exit 1) exactly as it does with diagnostics off, and the other three answered. A diagnostic is never the reason a
    // command fails, and never the reason it stops failing either.
    let root = vault.root.to_str().expect("utf-8 path");
    // Each argv is spelled in full, including the workspace, because `np search` takes its operand *before* the query and
    // `Vault::exec` appends one last; a case that reads as four commands run the same way is worth one helper less.
    for (argv, code) in [
        (vec!["scan", root], 0),
        (vec!["embed", "--no-auto-fetch", root], 1),
        (vec!["search", root, "indexing"], 0),
        (vec!["models", "list", root], 0),
    ] {
        let run_ = run(&mut vault.command(&argv));
        assert_eq!(
            run_.code,
            code,
            "`np {}` at trace exits as it does without diagnostics: {}",
            argv.join(" "),
            run_.stderr
        );
    }

    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    assert!(
        !records.is_empty(),
        "a trace-level run of four commands wrote nothing at all"
    );
    let text = fs::read_to_string(&log).expect("test setup: read the log");
    assert!(
        !text.contains(body),
        "a diagnostic quoted a note's body: {text}"
    );
    assert!(
        !text.contains("origin.invalid") && !text.contains("http"),
        "§12 rule 8: no origin host, and no URL that names one: {text}"
    );
}

/// Plan Task 4's skip rules on the file channel: a vault with one note, one attachment and one unreadable note, scanned
/// twice at `debug`, gives one record per reason with the path it is about — which is what makes `ScanReport::skipped`
/// readable from a log instead of only from a summary line. A third pass at `trace` then shows the routine rule the
/// `debug` level filtered out, so "absent at debug" means *filtered* rather than *never emitted*. First case here with
/// per-file traffic in a file, which is also what gives the privacy sweep above something to inspect.
#[cfg(unix)]
#[test]
fn each_skipped_file_reaches_the_file_with_its_own_rule() {
    use std::os::unix::fs::PermissionsExt;

    let vault = Vault::new();
    fs::write(vault.root.join("note.md"), "# Note\n\nindexed fine\n").expect("setup");
    fs::write(vault.root.join("diagram.png"), b"\x89PNG not really").expect("setup: attachment");
    let locked = vault.root.join("locked-down.md");
    fs::write(&locked, "# Locked\n\ncannot be read\n").expect("setup");
    let mode = fs::metadata(&locked)
        .expect("setup: stat")
        .permissions()
        .mode();
    fs::set_permissions(&locked, PermissionsExt::from_mode(0o000)).expect("setup: deny the note");

    let log = vault.log_path();
    vault.write_user_config(&format!(
        "[log]\nfile = {}\nlevel = \"debug\"\n",
        toml_str(&log.display().to_string())
    ));
    let first = vault.exec(&["scan"]);
    assert_eq!(
        first.code, 0,
        "a skip is not a scan failure: {}",
        first.stderr
    );

    // The second pass over the same vault is the one with something to explain — `note.md` is unchanged now rather than
    // added — and emptying the log between them keeps "exactly one record per rule" a claim about one scan.
    fs::remove_file(&log).expect("setup: empty the log");
    let second = vault.exec(&["scan"]);
    // Restore before any assertion: a failure must not leave a 0o000 file in a tempdir nobody will revisit.
    fs::set_permissions(&locked, PermissionsExt::from_mode(mode))
        .expect("teardown: restore the mode");
    assert_eq!(second.code, 0, "{}", second.stderr);

    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    // `unchanged-content` is deliberately absent at `debug`: it is a `trace` rule, and the level filter is what keeps a
    // warm scan's 400 unchanged files out of the log somebody opened to read one thing. The third pass below raises the
    // level and finds it, so "absent" here means filtered rather than never emitted.
    for (rule, path) in [
        ("not-markdown", "diagram.png"),
        ("unreadable", "locked-down.md"),
    ] {
        let found: Vec<&Value> = records
            .iter()
            .filter(|record| record.get("rule").and_then(Value::as_str) == Some(rule))
            .collect();
        assert_eq!(
            found.len(),
            1,
            "`{rule}` must appear once for the file it is about: {records:?}"
        );
        assert_eq!(
            found[0].get("what").and_then(Value::as_str),
            Some(path),
            "the path is what makes a rule actionable: {}",
            found[0]
        );
        assert_eq!(
            found[0].get("target").and_then(Value::as_str),
            Some("np_core::scan")
        );
    }

    assert!(
        !records
            .iter()
            .any(|record| record.get("rule").and_then(Value::as_str) == Some("unchanged-content")),
        "debug must not carry a trace rule: {records:?}"
    );

    // Same vault, `[log] level = "trace"`: the routine rule now appears, once per unchanged file, and nothing else about
    // the run changed.
    fs::remove_file(&log).expect("setup: empty the log");
    vault.write_user_config(&format!(
        "[log]\nfile = {}\nlevel = \"trace\"\n",
        toml_str(&log.display().to_string())
    ));
    let third = vault.exec(&["scan"]);
    assert_eq!(third.code, 0, "{}", third.stderr);
    let traced = read_records(&log);
    assert_keys_are_documented(&traced, &log);
    let unchanged: Vec<&Value> = traced
        .iter()
        .filter(|record| {
            record.get("rule").and_then(Value::as_str) == Some("unchanged-content")
                && record.get("what").and_then(Value::as_str) == Some("note.md")
        })
        .collect();
    assert_eq!(
        unchanged.len(),
        1,
        "one record for the note this scan left alone: {traced:?}"
    );
    assert_eq!(
        unchanged[0].get("what").and_then(Value::as_str),
        Some("note.md"),
        "{}",
        unchanged[0]
    );
    assert_eq!(
        unchanged[0].get("level").and_then(Value::as_str),
        Some("trace"),
        "§4: a per-item routine rule is `trace`, whatever level let it through"
    );
}

// ---------------------------------------------------------------------------
// Task 5: spans — every line knows the request that caused it, and what a phase cost
// ---------------------------------------------------------------------------

/// Plan Task 5 Step 1: two requests down one pipe, one with `id: 9` and one notification-shaped (no id at all), against
/// a config that refuses so each request produces a diagnostic of its own. The tagged half is the correlation an
/// integrator asked for; the untagged half is the harder half and the reason this is a test rather than a `null`: a
/// frontend that sees `"request": null` cannot tell "this line belongs to no request" from "this request had no id",
/// which is the same ambiguity `serve_channel.rs` refuses on the wire by omitting the key.
#[test]
fn a_serve_request_tags_every_line_it_causes() {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).expect("test setup: vault dir");
    fs::write(root.join("a.md"), "# Alpha\n\nword\n").expect("test setup: note");
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(config_dir.join("note-processor")).expect("test setup: config dir");
    let log = tmp.path().join("np.jsonl");
    // A refusing file that still names its own log: F35's failure shape, and the one combination where a per-request
    // diagnostic exists without asking a working command to produce one.
    fs::write(
        config_dir.join("note-processor").join("config.toml"),
        format!(
            "[log]\nfile = {}\nlevel = \"error\"\n[loggg]\nlevel = \"info\"\n",
            toml_str(&log.display().to_string())
        ),
    )
    .expect("test setup: write a refusing config with a working [log]");

    let requests = vec![
        request(9, "scan", serde_json::json!({"workspace": root})),
        // `request(0, …)` emits no `id` key at all — the notification shape.
        request(0, "scan", serde_json::json!({"workspace": root})),
    ];
    let (code, _, _) = serve_with(&requests, |cmd| {
        cmd.env("XDG_DATA_HOME", tmp.path().join("data"))
            .env("LOCALAPPDATA", tmp.path().join("data"))
            .env("HOME", tmp.path())
            .env("XDG_CONFIG_HOME", &config_dir)
            .env("APPDATA", &config_dir);
    });
    assert_eq!(code, 0, "the server ran both requests: {code}");

    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    let refusals: Vec<&Value> = records
        .iter()
        .filter(|record| record.get("rule").and_then(Value::as_str) == Some("config-refused"))
        .collect();
    assert_eq!(
        refusals.len(),
        2,
        "one refusal per request, and the file is in append order: {records:?}"
    );
    assert_eq!(
        refusals[0].get("request").and_then(Value::as_u64),
        Some(9),
        "the first request's line carries its id: {}",
        refusals[0]
    );
    assert!(
        refusals[1].get("request").is_none(),
        "a request with no id gets the key **omitted**, never `null` and never \"none\": {}",
        refusals[1]
    );
    for record in &records {
        assert_ne!(
            record.get("request"),
            Some(&Value::Null),
            "`request` is either an id or absent: {record}"
        );
    }
    // The workspace travels the same way, because a server can serve several vaults from one process and a line that
    // cannot say which one it is about is a line nobody can act on.
    assert!(
        refusals[0]
            .get("workspace")
            .and_then(Value::as_str)
            .is_some_and(|w| w.ends_with("vault")),
        "every record inside a request span names its workspace: {}",
        refusals[0]
    );
}

/// Plan Task 5 Step 3: a phase reports what it cost, and search says which half it actually ranked.
///
/// The scan is over 400 notes rather than the two-note vault most cases here use for one reason: `elapsed_ms` of `0`
/// is a legitimate measurement of a fast phase, and asserting `> 0` against a corpus that could finish inside a
/// millisecond would make the case depend on how busy the machine running it happens to be.
#[test]
fn a_slow_phase_reports_its_cost() {
    let vault = Vault::new();
    vault.write_notes(400);
    let log = vault.log_config(None);
    vault.write_user_config(&format!(
        "[log]\nfile = {}\nlevel = \"info\"\n",
        toml_str(&log.display().to_string())
    ));

    let scan = vault.exec(&["scan"]);
    assert_eq!(scan.code, 0, "{}", scan.stderr);
    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    let timed: Vec<&Value> = records
        .iter()
        .filter(|record| record.get("elapsed_ms").is_some())
        .collect();
    assert!(
        !timed.is_empty(),
        "a scan at info reports what its phase cost: {records:?}"
    );
    let scan_span = timed
        .iter()
        .find(|record| record.get("target").and_then(Value::as_str) == Some("np_core::scan"))
        .unwrap_or_else(|| panic!("no timing record on the scan target: {timed:?}"));
    assert_eq!(
        scan_span.get("level").and_then(Value::as_str),
        Some("info"),
        "timing is a state change worth reading later (§4): {scan_span}"
    );
    assert!(
        scan_span
            .get("elapsed_ms")
            .and_then(Value::as_u64)
            .is_some_and(|ms| ms > 0),
        "400 notes is more than a millisecond of work: {scan_span}"
    );

    // The search half: at `debug`, the run says which lists it ranked. This is the line that answers "why did I get
    // keyword results" when nothing failed — no rule, because there is no refusal; just the mode it was asked for and
    // the halves it used.
    fs::remove_file(&log).expect("setup: empty the log");
    vault.write_user_config(&format!(
        "[log]\nfile = {}\nlevel = \"debug\"\n",
        toml_str(&log.display().to_string())
    ));
    let root = vault.root.to_str().expect("utf-8 path");
    let search = run(&mut vault.command(&["search", root, "word"]));
    assert_eq!(search.code, 0, "{}", search.stderr);
    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    let ranked = records
        .iter()
        .filter(|record| record.get("target").and_then(Value::as_str) == Some("np_core::search"))
        .filter(|record| record.get("level").and_then(Value::as_str) == Some("debug"))
        .collect::<Vec<_>>();
    assert_eq!(
        ranked.len(),
        1,
        "one search, one account of what it ranked: {records:?}"
    );
    let msg = ranked[0]
        .get("msg")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        msg.contains("keyword"),
        "the sentence names the half that ran: {msg}"
    );
    assert!(
        ranked[0].get("rule").is_none(),
        "this is not a refusal, so it carries no rule: {}",
        ranked[0]
    );
}

// ---------------------------------------------------------------------------
// Review Focus 5: a client that never reads stderr
// ---------------------------------------------------------------------------

/// Plan Review Focus 5, and the one case in this file that exists because of a measurement rather than an idea.
///
/// A GUI spawns `np serve`, captures stderr to see whether anything went wrong, and **never reads the pipe** — the most
/// normal possible thing for a frontend to do with a stream it does not parse. An anonymous pipe holds about 64 KiB; a
/// refusing config emits a ~350-byte diagnostic per request, so the pipe fills after a couple of hundred requests and
/// every later stderr write waits for a reader that will never come. Measured on this box before the fix: 1 200 requests
/// in, **1 057 answered**, the child stalled inside `write(2)`. That is a log channel stopping the thing it watches, so
/// the sink now asks descriptor 2 whether it will take a write and drops the line when it will not.
///
/// The assertions are the two halves of that claim: every request is answered (nothing blocks), and stderr really did
/// fill (the case would pass vacuously if the diagnostics never reached the pipe's capacity).
/// A pipe this case owns, sized as small as the kernel allows: `(read half, write half)`.
///
/// `Stdio::piped()` yields a pipe of the system default (~64 KiB on this box), and a process that drops diagnostics keeps
/// such a pipe *nearly* full — and nearly is not full. A 90-byte closing notice can slip into whatever bytes are left, so a
/// test hoping to catch "the notice waited for a reader that never came" would pass by luck rather than by construction.
/// `F_SETPIPE_SZ` moves the capacity to its floor of one page: after a handful of refusals the pipe is genuinely full, every
/// later write is refused by `poll` instead of by chance, and the leftover space is smaller than any line we print.
#[cfg(unix)]
fn small_pipe() -> (std::fs::File, std::fs::File) {
    use std::os::fd::FromRawFd;
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is two ints and `pipe2` fills both halves. `O_CLOEXEC` keeps the read half out of any other process
    // this test tree may spawn.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        panic!("test setup: pipe2 failed");
    }
    // SAFETY: `fds[1]` is a descriptor the call above just created; `F_SETPIPE_SZ` takes an int and one page is its floor.
    if unsafe { libc::fcntl(fds[1], libc::F_SETPIPE_SZ, 4_096) } < 0 {
        panic!("test setup: F_SETPIPE_SZ failed");
    }
    // SAFETY: each half is a fresh descriptor and this is the only place its number is adopted into an owner.
    unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    }
}

#[cfg(unix)]
#[test]
fn an_unread_stderr_pipe_never_stalls_the_request_loop() {
    use std::io::Write;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    let tmp = TempDir::new().expect("test setup: tempdir");
    let root = tmp.path().join("vault");
    fs::create_dir_all(&root).expect("test setup: vault dir");
    fs::write(root.join("a.md"), "# Alpha\n\nword\n").expect("test setup: note");
    let config_dir = tmp.path().join("config");
    fs::create_dir_all(config_dir.join("note-processor")).expect("test setup: config dir");
    // The same table both refuses and configures: `[loggg]` makes every request a refusal (one diagnostic each), while
    // `[log] file` gives the run somewhere that always accepts writes. That second half is what turns "stderr stayed short"
    // from an impression into a measurement — the file holds every diagnostic this run produced, so the bytes that never
    // reached the pipe can be counted rather than asserted.
    let log = tmp.path().join("np.jsonl");
    fs::write(
        config_dir.join("note-processor").join("config.toml"),
        format!(
            "[log]\nfile = {}\n\n[loggg]\nlevel = \"info\"\n",
            toml_str(&log.display().to_string())
        ),
    )
    .expect("test setup: write refusing config");

    let asks: usize = 1_200;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_np"));
    cmd.arg("serve")
        .current_dir(&root)
        .env("XDG_DATA_HOME", tmp.path().join("data"))
        .env("LOCALAPPDATA", tmp.path().join("data"))
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("APPDATA", &config_dir)
        .env("NP_LOG", "error")
        .env_remove("RUST_LOG");
    // **Held, never read.** The write half goes to the child, the read half stays here undrained: that is what a frontend
    // does with a stream it does not parse, and reading it would remove the condition under examination.
    let (mut unread_stderr, stderr_write) = small_pipe();
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_write));
    let mut child = cmd.spawn().expect("test setup: start `np serve`");
    let mut stdin = child.stdin.take().expect("test setup: stdin");
    let mut stdout = child.stdout.take().expect("test setup: stdout");

    // Feeding happens off the main thread because a stalled child stops draining *stdin* too, and a harness that blocks on
    // its own write cannot report what the child did.
    let (fed_tx, fed_rx) = mpsc::channel::<usize>();
    let feeder = thread::spawn(move || {
        let mut written = 0usize;
        for i in 1..=asks {
            let line = request(i as u32, "scan", serde_json::json!({"workspace": root}));
            if stdin.write_all(line.as_bytes()).is_err() {
                break;
            }
            written += 1;
            // Every 50 rather than every line: flushing exists here so a stalled child is noticed early, and a per-line
            // flush would measure the harness's syscalls instead of the server's behaviour.
            if written.is_multiple_of(50) {
                let _ = stdin.flush();
            }
        }
        let _ = stdin.flush();
        let _ = fed_tx.send(written);
        // Closing stdin is what lets the server reach its own clean exit.
        drop(stdin);
    });

    let (answered_tx, answered_rx) = mpsc::channel::<usize>();
    thread::spawn(move || {
        let mut answered = 0usize;
        let mut buf = std::io::BufReader::new(&mut stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match std::io::BufRead::read_line(&mut buf, &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if !line.trim().is_empty() {
                        answered += 1;
                    }
                }
            }
        }
        let _ = answered_tx.send(answered);
    });

    let answered = answered_rx.recv_timeout(Duration::from_secs(90)).expect(
        "the request loop answers within 90 s; a stall here is the defect this case exists for",
    );
    let fed = fed_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_default();
    let _ = feeder.join();

    assert_eq!(
        answered, asks,
        "every request gets its response even though nobody reads stderr (fed {fed} of {asks})"
    );

    // **Exit first, drain second.** The closing notice is owed here — 1 200 diagnostics against a 1 000-line cap into a pipe
    // that stopped accepting writes long ago — and it used to be written with a bare `writeln!`, which waits for a reader
    // that will never come: the server answered every request and then never exited. Draining stderr before waiting would
    // unblock exactly that write, so the order *is* the assertion, and the deadline is here because a hung child must fail
    // this case rather than hang the suite behind it.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("test setup: poll the child") {
            Some(status) => break status,
            None if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                panic!(
                    "`np serve` was still running 30 s after stdin closed with nobody reading stderr, so something on the \
                     way out blocked on it"
                );
            }
            None => thread::sleep(Duration::from_millis(25)),
        }
    };
    assert!(status.success(), "and the run ends cleanly: {status}");

    // Now, with the process gone: did stderr stay short because diagnostics were dropped, or because none were written? The
    // file answers that exactly, because a regular file never refuses a write.
    // Drained without waiting for EOF. `Stdio::from(file)` hands the child a descriptor and keeps its own copy until the
    // `Command` is dropped, so this process may hold the pipe's write end open — which means there is no last writer to
    // close, and `read_to_end` would sit there forever after a perfectly good run. The child has exited, so every byte it
    // wrote is already in the pipe: read until it says "nothing more right now".
    use std::os::fd::AsRawFd;
    // SAFETY: `unread_stderr` is a live descriptor for the duration, and reading its flags to add one bit changes nothing
    // else about it. This is unix-only code inside a `#[cfg(unix)]` case.
    let flags = unsafe { libc::fcntl(unread_stderr.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0, "test setup: F_GETFL");
    assert!(
        unsafe {
            libc::fcntl(
                unread_stderr.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        } >= 0,
        "test setup: O_NONBLOCK"
    );
    let mut arrived = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        match std::io::Read::read(&mut unread_stderr, &mut chunk) {
            Ok(0) => break,
            Ok(got) => arrived.extend_from_slice(&chunk[..got]),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => panic!("test setup: reading the undrained pipe: {err}"),
        }
    }
    let records = read_records(&log);
    let refused: Vec<&Value> = records
        .iter()
        .filter(|record| record.get("rule").and_then(Value::as_str) == Some("config-refused"))
        .collect();
    assert_eq!(
        refused.len(),
        asks,
        "the file sink saw every refusal, one per request and none dropped: {records:?}"
    );
    let written: usize = refused
        .iter()
        .map(|record| record["msg"].as_str().unwrap_or_default().len())
        .sum();
    // How much arrives is the kernel's business: a pipe with no reader reports itself writable while it has a page free, so
    // a one-page pipe takes roughly one line and then stops answering `poll` at all. What the case needs is that something
    // reached the pipe (so nothing was proven by accident), that far less than the run produced arrived (so diagnostics were
    // genuinely dropped), and that it never exceeded the capacity (so the writer respected "not now" instead of queueing).
    assert!(
        !arrived.is_empty(),
        "the pipe had to receive something for this to prove anything: {} bytes arrived",
        arrived.len()
    );
    assert!(
        arrived.len() <= PIPE_BYTES,
        "stderr cannot hold more than the pipe: {} bytes into a {PIPE_BYTES}-byte pipe",
        arrived.len()
    );
    assert!(
        arrived.len() * 4 < written,
        "stderr held {} B while the run produced {written} B of refusal sentences: most of them should have been dropped, \
         so a stderr anywhere near the file's size would mean nothing was being skipped",
        arrived.len()
    );
    // And if the closing notice did make it onto the pipe, it owes the count it claims.
    let text = String::from_utf8_lossy(&arrived);
    assert!(
        !text.contains("diagnostics truncated") || text.contains("suppressed"),
        "a truncation notice on stderr has to name what it suppressed: {:?}",
        text.lines().next_back()
    );
}

// ---------------------------------------------------------------------------
// Task 6: the manual and the code are one list
// ---------------------------------------------------------------------------

/// Plan Task 6 Step 2, run from the binary's own test tree. `crates/np-core/tests/diagnostics.rs` checks the same two
/// fences against `np_core::diagnostics`, and that is the right place for the *vocabulary*; this case is the one that
/// answers the question an integrator actually asks — whether the document shipped next to **this artifact** lists every
/// token this artifact can emit. It therefore reads the repository's manual rather than a copy, in both directions: a
/// token emitted and undocumented fails here, and so does a token documented that nothing emits (the failure mode a
/// hand-written list drifts into).
#[test]
fn the_diagnostics_reference_documents_every_target_and_token() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/reference-diagnostics.md"
    );
    let manual = fs::read_to_string(path).unwrap_or_else(|err| panic!("cannot read {path}: {err}"));
    for (fence, coded, what) in [
        (
            "diagnostic-tokens",
            np_core::diagnostics::RULE_TOKENS.as_slice(),
            "rule token",
        ),
        (
            "diagnostic-targets",
            np_core::diagnostics::TARGETS.as_slice(),
            "target",
        ),
    ] {
        let listed = fenced_block(&manual, fence)
            .unwrap_or_else(|| panic!("`docs/reference-diagnostics.md` has no ```{fence} block"));
        let documented: Vec<&str> = listed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        let missing: Vec<&&str> = coded.iter().filter(|t| !documented.contains(t)).collect();
        let invented: Vec<&&str> = documented.iter().filter(|t| !coded.contains(t)).collect();
        assert!(
            missing.is_empty() && invented.is_empty(),
            "{what}s drift between code and the manual.\n  undocumented: {missing:?}\n  invented: {invented:?}"
        );
    }
    // The pairing, not merely the two lists: `RULE_TARGETS` is what an integrator writes alert filters from, and a row that
    // sets a real token beside a target that never emits it is a monitoring gap no set comparison can see. Checked here too,
    // from the binary's own test tree, so the document shipped beside this artifact cannot disagree with the code about
    // where a rule appears.
    let pairs = documented_rule_targets(&manual);
    let mut coded: Vec<(String, Vec<String>)> = np_core::diagnostics::RULE_TARGETS
        .iter()
        .map(|(rule, list)| {
            let mut targets: Vec<String> =
                list.iter().map(|target| (*target).to_string()).collect();
            targets.sort();
            ((*rule).to_string(), targets)
        })
        .collect();
    coded.sort();
    let mut documented = pairs;
    for (_, targets) in &mut documented {
        targets.sort();
    }
    documented.sort();
    assert_eq!(
        documented, coded,
        "`docs/reference-diagnostics.md` and `np_core::diagnostics::RULE_TARGETS` disagree about which target emits each rule"
    );

    // The five phase names are a doc-side claim only — there is no code list to compare them with, and inventing one for
    // a test would be a constant nobody reads at runtime. What this does catch is the cheap accident: renaming the
    // `scan` span or dropping `fetch` from the manual while the other still names it.
    for phase in ["scan", "embed", "search", "fetch", "serve.request"] {
        let named = fenced_block(&manual, "diagnostic-spans")
            .unwrap_or_else(|| panic!("the manual has no ```diagnostic-spans block"))
            .lines()
            .any(|line| line.trim() == phase);
        assert!(
            named,
            "`{phase}` is a phase that reports its cost; the manual's span list must say so"
        );
    }
}

/// The manual's token table as `(token, targets)` rows. Rows are recognised by their level cell holding a level name, which
/// skips every other table in the file; a `—` in the targets cell contributes nothing.
fn documented_rule_targets(manual: &str) -> Vec<(String, Vec<String>)> {
    let mut rows = Vec::new();
    for line in manual.lines() {
        let Some(rest) = line.trim().strip_prefix("| `") else {
            continue;
        };
        let cells: Vec<&str> = rest.split('|').map(str::trim).collect();
        if cells.len() < 3 {
            continue;
        }
        let token = cells[0].split('`').next().unwrap_or_default().to_string();
        if !["error", "warn", "info", "debug", "trace"].contains(&cells[1]) {
            continue;
        }
        let targets = cells[2]
            .split(',')
            .map(|cell| cell.trim().trim_matches('`').to_string())
            .filter(|cell| !cell.is_empty() && cell != "\u{2014}")
            .collect();
        rows.push((token, targets));
    }
    rows
}

/// The body of the first fenced block whose info string is `info`.
fn fenced_block(manual: &str, info: &str) -> Option<String> {
    let opener = format!("```{info}");
    let rest = &manual[manual.find(&opener)? + opener.len()..];
    Some(rest[..rest.find("\n```")?].to_string())
}

// ---------------------------------------------------------------------------
// Review Focus 3: a path that outgrows a log line
// ---------------------------------------------------------------------------

/// Plan Review Focus 3: **a vault path containing note-like text**, truncated at a bound and never redacted.
///
/// Two failure modes live here and they pull in opposite directions. A diagnostic's `what` is a path, and a path can be
/// long — a deep vault under a name that says something (`…/the-quick-brown-fox-jumps-over-the-lazy-dog-while-…/x.png`)
/// — so an unbounded field lets one file's name dictate the size of every record written about it, and a terminal full of
/// 4 KB lines is a log nobody reads. The tempting fix is worse: redact the whole field when it gets silly, which throws
/// away exactly the information that makes a rule actionable ("which file?", answered by nothing). So the bound clamps
/// and keeps the front: paths diverge at the start, not the end, so the head of a long path identifies the vault and the
/// directory, and a marker says the rest exists.
///
/// `workspace` is clamped by the same rule and asserted here because length arrives differently for the two fields —
/// `what` is workspace-relative, while `workspace` is absolute, so a deep root blows past the bound on its own.
#[test]
fn a_long_path_is_truncated_and_still_names_the_file() {
    let vault = Vault::new();
    // Sentence-like component names on purpose: the point is that text which *looks* like content still travels as a
    // path, because it is one, and the privacy rule is about note bodies rather than about file names.
    let prose = "the-quick-brown-fox-jumps-over-the-lazy-dog-while-the-index-stays-exactly-as-long";
    // Nine components **above** the vault, so `workspace` — which is absolute — is itself past the bound, and seven
    // inside it, so `what` — which is workspace-relative, and therefore shorter by exactly the part a reader cannot see
    // in the record — passes too. Both fields on one record, both clamped: that is what the two channels must agree on.
    let mut root = vault.root.clone();
    for _ in 0..9 {
        root = root.join(prose);
    }
    let mut deep = root.clone();
    for _ in 0..6 {
        deep = deep.join(prose);
    }
    fs::create_dir_all(&deep).expect("test setup: deep directories");
    let file = deep.join(format!("{prose}.png"));
    fs::write(&file, b"\x89PNG not really").expect("test setup: attachment");
    let (absolute, relative) = (
        root.display().to_string(),
        file.strip_prefix(&root)
            .expect("test setup: inside the vault")
            .display()
            .to_string(),
    );
    assert!(
        absolute.len() > DIAG_FIELD_BOUND_BYTES && relative.len() > DIAG_FIELD_BOUND_BYTES,
        "test setup: this case is about two paths past the bound — workspace {} B, what {} B",
        absolute.len(),
        relative.len()
    );

    let log = vault.log_path();
    vault.write_user_config(&format!(
        "[log]\nfile = {}\nlevel = \"debug\"\n",
        toml_str(&log.display().to_string())
    ));
    // `command` rather than `exec`: the workspace being scanned is the deep one, not the tempdir's own root.
    let mut cmd = vault.command(&["scan"]);
    cmd.arg(&root);
    let scanned = crate::run(&mut cmd);
    assert_eq!(
        scanned.code, 0,
        "a long name is not a scan failure: {}",
        scanned.stderr
    );

    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    let found: Vec<&Value> = records
        .iter()
        .filter(|record| record.get("rule").and_then(Value::as_str) == Some("not-markdown"))
        .collect();
    assert_eq!(found.len(), 1, "one record for the one file: {records:?}");

    let what = found[0]
        .get("what")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!(
                "a skipped file without a `what` is a redaction, not a truncation: {}",
                found[0]
            )
        });
    assert!(
        what.starts_with(prose),
        "the front of the path is what identifies it, so that is what survives: {what}"
    );
    assert!(what.ends_with('…'), "a clamped field says so: {what}");
    assert!(
        what.len() <= DIAG_FIELD_BOUND_BYTES,
        "`what` must stay inside the bound ({} bytes): {what}",
        what.len()
    );
    // The same record's `workspace` is absolute, which is where a vault root adds length. It is clamped the same way:
    // present, prefixed by the real path, marked, bounded.
    let workspace = found[0]
        .get("workspace")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!(
                "the scan record must still name the workspace: {}",
                found[0]
            )
        });
    assert!(
        workspace.starts_with(&vault.root.display().to_string()),
        "`workspace` keeps its front too: {workspace}"
    );
    assert!(
        workspace.ends_with('…'),
        "and says it was clamped: {workspace}"
    );
    assert!(
        workspace.len() <= DIAG_FIELD_BOUND_BYTES,
        "`workspace` must stay inside the bound ({} bytes): {workspace}",
        workspace.len()
    );

    // And a short path is untouched by any of this: no marker, exact name. Without this half the case would only prove
    // that everything gets clamped, which would be its own kind of lie.
    vault.write_notes(1);
    fs::write(vault.root.join("plain.png"), b"\x89PNG also not")
        .expect("setup: short-named attachment");
    fs::remove_file(&log).expect("setup: empty the log");
    let second = vault.exec(&["scan"]);
    assert_eq!(second.code, 0, "{}", second.stderr);
    let records = read_records(&log);
    let plain = records
        .iter()
        .find(|record| record.get("what").and_then(Value::as_str) == Some("plain.png"))
        .expect("a short path must arrive whole: no marker, no ellipsis");
    assert!(
        !plain
            .get("what")
            .and_then(Value::as_str)
            .unwrap_or("")
            .ends_with('…'),
        "the bound clamps long values, it does not decorate every value: {plain}"
    );
}

// ---------------------------------------------------------------------------
// The flag has to reach the records, not only the announce line
// ---------------------------------------------------------------------------

/// `-v` is the knob an operator reaches for first, and the announce line is the one place a bug in it can hide: phase one
/// prints `log level debug from command line` before configuration is read, so a case that stops at that sentence passes
/// while the run resolves to something else entirely the moment `Config::load` re-resolves the levels.
///
/// Both channels are asserted because they are configured by different keys: `-vv` alone asks for stderr, and `-vv
/// --log-file` asks for both, and the second phase reopens the file with whatever it just resolved.
#[test]
fn a_verbose_flag_reaches_the_records_and_not_only_the_announce() {
    let vault = Vault::new();
    vault.write_notes(1);
    fs::write(vault.root.join("photo.png"), b"\x89PNG not markdown").expect("setup: attachment");
    // No `[log]` in the file and no `NP_LOG`: the flag is the only thing that asked, which is the case that matters.

    let plain = vault.exec(&["-vv", "scan"]);
    assert_eq!(plain.code, 0, "{}", plain.stderr);
    assert!(
        plain
            .stderr
            .lines()
            .any(|line| line.starts_with("debug np_core::scan:")
                && line.contains("rule=\"not-markdown\"")),
        "`-vv` must put the per-file decisions on stderr, not merely announce that it will: {:?}",
        plain.stderr
    );

    let log = vault.log_path();
    let to_file = vault.exec(&["-vv", "--log-file", &log.display().to_string(), "scan"]);
    assert_eq!(to_file.code, 0, "{}", to_file.stderr);
    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    assert!(
        records.iter().any(|record| {
            record.get("rule").and_then(Value::as_str) == Some("not-markdown")
                && record.get("level").and_then(Value::as_str) == Some("debug")
        }),
        "a debug-level record must reach the file the same flag enabled: {records:?}"
    );
}

/// The precedence chain is `flag > file > environment > default`, and the manual tells a reader stuck behind a file level to
/// override it with a flag. That advice is only true if the flag survives the second resolution — so this case pins the one
/// combination where a wrong answer is invisible on stderr's first line: a file that asked for `warn`, a command line that
/// asked for `trace`.
#[test]
fn a_flag_outranks_the_file_that_asked_first() {
    let vault = Vault::new();
    vault.write_notes(1);
    fs::write(vault.root.join("photo.png"), b"\x89PNG not markdown").expect("setup: attachment");
    vault.write_user_config("[log]\nlevel = \"warn\"\n");

    let run = vault.exec(&["-vvv", "scan"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        run.stderr.starts_with("log level trace from command line"),
        "and it must say so on its first line: {:?}",
        run.stderr
    );
    assert!(
        run.stderr
            .lines()
            .any(|line| line.starts_with("debug np_core::scan:")),
        "`-vvv` over `[log] level = \"warn\"` runs at trace; a file that outranked the flag would print only its announce \
         line and this case is the difference: {:?}",
        run.stderr
    );
}

/// Review Focus 1's second input — a **bad `[log] level`** — and the reason it is a separate case from an unknown key: this
/// typo is in the diagnostics section itself, so the thing that fails to parse is the setting whose whole job is reporting
/// failures. Refusing it before the sinks exist means `np serve` exits 1 without answering one request and writes nothing to
/// the `[log] file` the same table configured: a log-level typo taking down an integrator's server, which is the one thing
/// this feature promises never to happen.
///
/// The behavior asserted instead: run at the levels that *are* understood (flag, `NP_LOG`, default), open the file the table
/// named, say once on stderr what was ignored, and let `Config::load`'s strict refusal arrive where it always has — as a
/// fault on each request and a `config-refused` record in that file. A bad `--log-spec` or `NP_LOG` still refuses the run
/// outright: spec point 2 wants a mistyped command line to stop there, and only there can it be fixed before work starts.
#[test]
fn a_bad_log_level_in_the_file_still_serves() {
    let vault = Vault::new();
    vault.write_notes(1);
    let log = vault.log_path();
    vault.write_user_config(&format!(
        "[log]\nlevel = \"verbose\"\nfile = {}\n",
        toml_str(&log.display().to_string())
    ));
    let requests = vec![
        request(1, "initialize", serde_json::json!({"protocol": 1})),
        request(2, "scan", serde_json::json!({"workspace": vault.root})),
        request(3, "initialize", serde_json::json!({"protocol": 1})),
    ];
    let (code, out_text, err_text) = serve_with(&requests, |cmd| {
        cmd.env("XDG_DATA_HOME", vault._tmp.path().join("data"))
            .env("LOCALAPPDATA", vault._tmp.path().join("data"))
            .env("HOME", vault._tmp.path())
            .env("XDG_CONFIG_HOME", vault._tmp.path().join("config"))
            .env("APPDATA", vault._tmp.path().join("config"));
    });
    assert_eq!(
        code, 0,
        "a diagnostics typo cannot end the session before it answers a request: {err_text:?}"
    );
    let answers: Vec<serde_json::Value> = out_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("stdout stays one object per line"))
        .collect();
    assert_eq!(answers.len(), 3, "one response per request: {out_text}");
    assert!(answers[0].get("result").is_some(), "{}", answers[0]);
    assert_eq!(
        answers[1]["error"]["code"].as_i64(),
        Some(-32000),
        "the config is still refused, per request: {}",
        answers[1]
    );
    let message = answers[1]["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("log.level") || message.contains("level"),
        "and the fault names the key that broke: {message}"
    );
    assert!(answers[2].get("result").is_some(), "{}", answers[2]);

    // The file the ignored table configured is still open, and it holds the refusal.
    let records = read_records(&log);
    assert_keys_are_documented(&records, &log);
    assert!(
        records
            .iter()
            .any(|record| record.get("rule").and_then(Value::as_str) == Some("config-refused")),
        "the refusal belongs in the log the same section asked for: {records:?}"
    );
    // And stderr says, once and plainly, that the value was ignored — a typo nobody mentions is a typo nobody finds.
    assert!(
        err_text.contains("log.level"),
        "the ignored level has to be named on stderr: {err_text:?}"
    );
}

/// Where np puts its files, given a directory handed to it as `$XDG_DATA_HOME` / `%LOCALAPPDATA%`. **Windows nests** under
/// `note-processor` (**F37**), so an assertion that spells the raw variable names a directory np never writes there. The
/// measured version of that sentence is run 43616394486, and the helper beside it in `tests/cli.rs` names the five cases.
fn np_data_dir(raw: &std::path::Path) -> std::path::PathBuf {
    if cfg!(windows) {
        raw.join(np_core::config::WINDOWS_DATA_SUBDIR)
    } else {
        raw.to_path_buf()
    }
}
