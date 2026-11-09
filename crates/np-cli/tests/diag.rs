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
        let path = self._tmp.path().join("np.jsonl");
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
}

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

/// **Deleting the log mid-run is documented as safe** (Review Focus 4): writes stop or continue, but the command does not fail,
/// panic, or lose its report — and the next run recreates the file, which is what makes "delete it" a real remedy.
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
