//! `np serve` end to end: a real child process, requests written to its stdin, responses read from its stdout.
//!
//! These cases cannot be in-process. What `np serve` is responsible for is *framing and channels* — one object per line on stdout, nothing else on
//! stdout, states that stay states across a pipe — and every one of those properties is a property of a descriptor, which a function call cannot
//! exhibit. The dispatcher's own branches (fault codes, the notification rule) are unit-tested in `src/serve.rs`; what is here is the part that
//! only a pipe can show.
//!
//! The load-bearing case is [`a_search_response_is_the_same_document_np_search_json_prints`]: it runs the same question through both channels and
//! compares the two documents. Every other claim in the design ("results are the CLI's documents, not a second vocabulary") is a sentence in a
//! comment until that comparison exists, because nothing else notices when `search_workspace` grows a key that `serve` does not.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use np_core::config::Config;
use np_core::lock::WorkspaceLock;
use serde_json::{json, Value};
use tempfile::TempDir;

/// The protocol version `np serve` announces, spelled out here rather than imported so a bump inside the binary has to be noticed by somebody.
const PROTOCOL: u64 = 1;

/// Markdown notes in the committed corpus — the same figure `tests/cli.rs` pins, for the same reason: a new fixture note must arrive as a failing
/// count here too, not as a silently larger `added` line.
const CORPUS_NOTES: u64 = 16;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("np-cli sits two levels under the workspace root")
        .join("tests")
        .join("fixtures")
        .join("corpus")
}

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

/// One `np serve` child, hermetically configured the way `tests/cli.rs` configures every run.
///
/// The five directory variables are the whole reason a server test can be trusted on a developer's machine: without `APPDATA` a Windows user's own
/// `config.toml` decides which provider answers, and without `XDG_DATA_HOME` the child writes a real catalog into the developer's data dir.
/// `NP_MODELS_BASE_URL` and `NP_NO_AUTO_FETCH` are removed for the same reason **F38** made them load-bearing — a shell must not be able to change
/// what these assertions prove.
struct Server {
    _tmp: TempDir,
    root: PathBuf,
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// Drained on a thread, because a child whose stderr pipe fills stops writing to *stdout* too — and a test that hangs on a full 64 KB buffer
    /// reads like a deadlock in the server rather than a missing reader in the test.
    stderr: Arc<Mutex<Vec<u8>>>,
    id: i64,
    /// Every `np/progress` line this session has written, in order. A notification is not an answer to anything, so
    /// `read_line` sets it aside instead of returning it — which keeps every existing request/response assertion true for
    /// an opted-in session as well, and makes "what did the client see while it waited" a lookup rather than a race.
    progress: Vec<Value>,
}

impl Server {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("test setup: tempdir");
        let root = tmp.path().join("vault");
        copy_tree(&corpus(), &root);
        let data_dir = tmp.path().join("data");
        let config_dir = tmp.path().join("config");

        let mut child = Command::new(env!("CARGO_BIN_EXE_np"))
            .arg("serve")
            .env("XDG_DATA_HOME", &data_dir)
            .env("LOCALAPPDATA", &data_dir)
            .env("HOME", tmp.path())
            .env("XDG_CONFIG_HOME", &config_dir)
            .env("APPDATA", &config_dir)
            .env_remove("NP_MODELS_BASE_URL")
            .env_remove("NP_NO_AUTO_FETCH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("test setup: spawn np serve");

        let stdin = child.stdin.take().expect("test setup: stdin pipe");
        let stdout = BufReader::new(child.stdout.take().expect("test setup: stdout pipe"));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&stderr);
        let mut err_pipe = child.stderr.take().expect("test setup: stderr pipe");
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(read) = std::io::Read::read(&mut err_pipe, &mut buf) {
                if read == 0 {
                    break;
                }
                sink.lock()
                    .expect("stderr sink")
                    .extend_from_slice(&buf[..read]);
            }
        });

        Self {
            _tmp: tmp,
            root,
            child,
            stdin: Some(stdin),
            stdout,
            stderr,
            id: 0,
            progress: Vec::new(),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self._tmp.path().join("data")
    }

    /// Where this child reads `config.toml` from — the same package directory `np models fetch` tests write into. A case
    /// that configures `[models] base_url` needs it, and building the path here keeps the two harnesses honest about the
    /// one location the binary actually looks in.
    ///
    /// Gated with its only caller: a default-feature build has no transport to point at an origin, so an ungated helper
    /// here would be dead code, and `-D warnings` is where dead scaffolding gets caught.
    #[cfg(all(feature = "local-model", feature = "fetch"))]
    fn config_dir_for_test(&self) -> PathBuf {
        self._tmp.path().join("config").join("note-processor")
    }

    /// Send one line exactly as written. The point of the raw form is that a test can send something no builder would produce — a blank line, half
    /// an object, a notification — and see what the server does with it.
    fn send_raw(&mut self, line: &str) {
        let pipe = self
            .stdin
            .as_mut()
            .expect("stdin is open until a case closes it");
        writeln!(pipe, "{line}").expect("write request");
        pipe.flush().expect("flush request");
    }

    /// One request with an id, and the response line it produced. Panics with the child's stderr attached: when a server refuses to answer, the
    /// reason is nearly always on the channel that was not being read.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        self.send_raw(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        let response = self.read_line();
        assert_eq!(
            response["id"], id,
            "the answer to request {id} was {response}"
        );
        response
    }

    /// The `result` of a successful request, with the fault printed if there was one — which is how a test states what it expects without
    /// repeating the envelope four times.
    fn result(&mut self, method: &str, params: Value) -> Value {
        let response = self.request(method, params);
        assert!(
            response.get("error").is_none(),
            "expected a result, got a fault: {response}"
        );
        response["result"].clone()
    }

    fn read_line(&mut self) -> Value {
        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .expect("read a response line");
            assert!(read > 0, "stdout closed before answering");
            let value: Value = serde_json::from_str(line.trim_end()).unwrap_or_else(|source| {
                panic!("not one JSON object per line ({source}): {line:?}")
            });
            if !is_progress(&value) {
                return value;
            }
            self.progress.push(value);
        }
    }

    /// Everything the server wrote after stdin closed, parsed. For an opted-out session this must be empty — that is
    /// Review Focus 1. For an opted-in one it holds the terminal notification of the last request, which by design
    /// follows its response and so cannot have been read before it.
    fn finish(&mut self) -> Vec<Value> {
        self.hang_up();
        let mut rest = Vec::new();
        loop {
            let mut line = String::new();
            match self.stdout.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let value: Value =
                        serde_json::from_str(line.trim_end()).unwrap_or_else(|source| {
                            panic!("not one JSON object per line ({source}): {line:?}")
                        });
                    if is_progress(&value) {
                        self.progress.push(value.clone());
                    }
                    rest.push(value);
                }
            }
        }
        rest
    }

    /// Flush the stream without guessing at timing.
    ///
    /// A terminal notification follows its response, so it is still in the pipe the moment `read_line` returns the
    /// answer. There is no portable non-blocking read to check with, and a `sleep` here would be a test that passes on a
    /// fast machine and flakes on the operator's box. The server handles one request at a time, so sending a cheap second
    /// request and waiting for *its* response proves every earlier line has been written: ordering is a property of the
    /// protocol, not of scheduling.
    fn flush(&mut self) {
        self.result("initialize", json!({}));
    }

    /// The progress lines that belong to `id`.
    fn progress_for(&self, id: i64) -> Vec<Value> {
        self.progress
            .iter()
            .filter(|line| line["params"].get("request").is_some_and(|r| r == id))
            .cloned()
            .collect()
    }

    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().expect("stderr sink").clone()).into_owned()
    }

    /// Close the client's end of stdin. EOF is how every stdio client in existence says "I am done", so the child must treat it as a clean
    /// ending — and once it is closed, `wait` proves the process was still healthy up to that point, which no per-request check can show.
    fn hang_up(&mut self) {
        let pipe = self.stdin.take().expect("test setup: stdin held once");
        drop(pipe);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best effort: a child left running would outlive the tempdir it writes into.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `initialize` is the first request every client makes and the only one that must work with no workspace, no index and no network. It is also the
/// only place the build's compile-time capabilities are stated, which is what lets a UI decide at startup whether to draw an embed control at all
/// (**F40** refuses on exactly these facts).
#[test]
fn initialize_answers_before_anything_is_indexed() {
    let mut server = Server::new();
    let result = server.result("initialize", json!({}));
    assert_eq!(result["protocol"], PROTOCOL, "{result}");
    assert_eq!(result["np"], env!("CARGO_PKG_VERSION"), "{result}");
    for key in ["local_model", "vision", "fetch"] {
        assert!(
            result["features"][key].is_boolean(),
            "`{key}` must be a boolean, not absent: {result}"
        );
    }
    let methods: Vec<&str> = result["methods"]
        .as_array()
        .expect("an array of methods")
        .iter()
        .map(|m| m.as_str().expect("strings"))
        .collect();
    assert!(methods.contains(&"search"), "{methods:?}");
    assert!(methods.contains(&"models.list"), "{methods:?}");
    // A support screen reads this line to explain where an index lives; §12 rule 8 still applies, so it is a path and never a host.
    assert!(
        !result["data_dir"]
            .as_str()
            .unwrap_or_default()
            .contains("http"),
        "a data dir must never carry a URL: {result}"
    );
}

/// **The case the whole design rests on.** `np serve`'s `search` result and `np search --json`'s document are compared as values, key set included.
/// If someone adds a key to one channel and not the other, or the two ever disagree about what `vectors: false` means, this is where it surfaces —
/// before a frontend ships against the wrong one.
///
/// **What this case does not prove, measured rather than assumed.** Deleting `"offline"` from `search_document` leaves it *green*, because both
/// channels read that one builder and stay equal while losing the key together. Schema loss is pinned elsewhere on purpose:
/// `crates/np-cli/tests/cli.rs` asserts the whole seven-key set (`fetched, hits, mode, note, offline, reason, vectors`) for the CLI document. So
/// this test guards *agreement between channels* and that one guards *the shape itself*; a mutant proves each dies to its own test and survives the
/// other's. Anyone tempted to fold them together should run the mutant first.
#[test]
fn a_search_response_is_the_same_document_np_search_json_prints() {
    let mut server = Server::new();
    server.result(
        "scan",
        json!({"workspace": server.root.display().to_string()}),
    );

    let query = "meeting";
    let served = server.result(
        "search",
        json!({"workspace": server.root.display().to_string(), "query": query}),
    );

    let cli = Command::new(env!("CARGO_BIN_EXE_np"))
        .arg("search")
        .arg("--json")
        // Operand order is the CLI's: workspace first, then the query (`np search <WORKSPACE> <QUERY>`).
        .arg(&server.root)
        .arg(query)
        .env("XDG_DATA_HOME", server.data_dir())
        .env("LOCALAPPDATA", server.data_dir())
        .env("HOME", server._tmp.path())
        .env("XDG_CONFIG_HOME", server._tmp.path().join("config"))
        .env("APPDATA", server._tmp.path().join("config"))
        .output()
        .expect("run np search --json");
    assert!(cli.status.success(), "the CLI refused: {cli:?}");
    let printed: Value =
        serde_json::from_slice(&cli.stdout).expect("the CLI's stdout is one JSON document");

    // Non-emptiness first: two empty documents would compare equal and prove nothing.
    assert!(
        !served["hits"].as_array().expect("hits").is_empty(),
        "a search of the corpus found nothing to compare: {served}"
    );
    assert_eq!(served, printed, "the two channels answered differently");
}

/// Same claim as above for `models.list`, whose document is built by a third function (`listing_json`) with a key set that **R7a** pinned precisely
/// because adding `fetching` had to fail a test. A server that dropped one of those keys would break a support screen that reads it.
#[test]
fn a_models_list_response_is_the_same_document_np_models_list_json_prints() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    server.result("scan", json!({"workspace": workspace}));
    let served = server.result("models.list", json!({"workspace": workspace}));

    let cli = Command::new(env!("CARGO_BIN_EXE_np"))
        .args(["models", "list", "--json"])
        .arg(&server.root)
        .env("XDG_DATA_HOME", server.data_dir())
        .env("LOCALAPPDATA", server.data_dir())
        .env("HOME", server._tmp.path())
        .env("XDG_CONFIG_HOME", server._tmp.path().join("config"))
        .env("APPDATA", server._tmp.path().join("config"))
        .output()
        .expect("run np models list --json");
    assert!(cli.status.success(), "the CLI refused: {cli:?}");
    let printed: Value = serde_json::from_slice(&cli.stdout).expect("one JSON document");

    let served_keys: Vec<&str> = served
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    let cli_keys: Vec<&str> = printed
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(served_keys, cli_keys, "the key sets drifted");
    assert_eq!(served, printed, "the two listings differ");
}

/// Framing is the risk of newline-delimited JSON, and the value most likely to break it is one `np` does not control: **the snippet**, which is a
/// window onto note text and therefore contains raw line breaks whenever a match sits near one. (A first version of this case reached for a hit's
/// `title`; there is no such key in the search document — hits carry `path`, `snippet`, ranks and link counts — which is itself worth knowing
/// before a frontend draws a result list from a field that does not exist.)
///
/// So: a note whose matched text spans a line break, holds a tab, a double quote, a backslash and non-ASCII characters. Three requests follow, so
/// "exactly one line per response" is a count rather than luck, and the snippet must come back with its newline *intact as data* — proof that serde
/// escaped it rather than that the note happened to be short. Had a raw break reached stdout, `read_line` would have stopped mid-object and failed
/// on the parse.
#[test]
fn a_snippet_spanning_a_line_break_still_fits_on_one_response_line() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    fs::write(
        server.root.join("delimiters.md"),
        "quuxfizz first line\nsecond \"quoted\"\ttab and \\backslash\\ and 世界\n",
    )
    .expect("test setup: write the odd note");

    let scan = server.result("scan", json!({"workspace": workspace}));
    assert_eq!(
        scan["notes"].as_u64(),
        Some(CORPUS_NOTES + 1),
        "the odd note was not counted as a note: {scan}"
    );
    let search = server.result(
        "search",
        json!({"workspace": workspace, "query": "quuxfizz"}),
    );
    let initialize = server.result("initialize", json!({}));

    let hits = search["hits"].as_array().expect("hits");
    let hit = hits
        .iter()
        .find(|hit| hit["path"] == "delimiters.md")
        .unwrap_or_else(|| panic!("the odd note did not come back: {search}"));
    let snippet = hit["snippet"].as_str().expect("a snippet");
    assert!(
        snippet.contains('\n') && snippet.contains("世界") && snippet.contains('\"'),
        "the matched text lost its control characters instead of escaping them: {snippet:?}"
    );
    assert_eq!(initialize["protocol"], PROTOCOL);
}

/// **F44 across a pipe.** Contended must not become a fault: an app's generic error handler would treat a retryable state as a failure, which is
/// the exact reasoning that made `locked` a document under `--json`. The server also has to keep answering afterwards — a refusal that wedges the
/// session is worse than the race it prevented.
#[test]
fn a_contended_workspace_is_a_result_and_the_server_keeps_answering() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    server.result("scan", json!({"workspace": workspace}));

    // Hold this workspace's write lock from the test process, the way a concurrent `np embed` would.
    let config = Config {
        data_dir: server.data_dir(),
        ..Config::default()
    };
    // Named exactly the way `np` names it — after this workspace's database file, in the directory that holds it — because a lock the test takes
    // under a different name excludes nothing, and the case would pass while proving nothing.
    let db_path = config.workspace_db(&server.root);
    let held = WorkspaceLock::acquire(
        db_path
            .parent()
            .expect("a workspace database sits in a directory"),
        db_path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("a lockable name"),
        std::time::Duration::from_millis(200),
    )
    .expect("test setup: take the workspace write lock");

    let contended = server.result("scan", json!({"workspace": workspace}));
    assert_eq!(contended["locked"], true, "{contended}");
    assert_eq!(contended["command"], "scan", "{contended}");
    assert!(
        contended["waited_ms"].is_number(),
        "a refusal must say how long it waited: {contended}"
    );

    // Readers are unaffected (**F44**'s design), so the same server answers a search immediately — and proves it never took its own lock hostage.
    let search = server.result(
        "search",
        json!({"workspace": workspace, "query": "meeting"}),
    );
    assert!(
        !search["hits"].as_array().expect("hits").is_empty(),
        "a reader was refused while a writer held the workspace: {search}"
    );

    drop(held);
    // Released, so the next write is ours again: this is the half that fails if `Drop` stops rolling the transaction back.
    let after = server.result("scan", json!({"workspace": workspace}));
    assert_eq!(
        after.get("locked"),
        None,
        "still contended after release: {after}"
    );
}

/// A refusal that happens *before* any work must not have touched the disk — §3 step 0, which `np scan` honours and which a server is more likely
/// to break, because it has an obvious reason to resolve paths once at startup.
#[test]
fn a_workspace_that_is_not_a_directory_refuses_without_creating_anything() {
    let mut server = Server::new();
    let missing = server._tmp.path().join("moved-away");
    let response = server.request("scan", json!({"workspace": missing.display().to_string()}));
    let error = &response["error"];
    assert_eq!(error["code"], -32000, "{response}");
    let message = error["message"].as_str().expect("a message");
    assert!(
        !message.contains("http"),
        "a refusal names no origin (**F32**): {message}"
    );
    assert!(
        !server.data_dir().join("workspaces").exists(),
        "the refusal created a data directory for a vault that does not exist"
    );
}

/// `-32601` lists the methods this build answers, because the likely cause is a typo or a client written against another version — and the fix is
/// visible in one round trip rather than in source.
#[test]
fn an_unknown_method_is_a_fault_that_names_the_methods() {
    let mut server = Server::new();
    let response = server.request("models_list", json!({}));
    assert_eq!(response["error"]["code"], -32601, "{response}");
    let message = response["error"]["message"].as_str().expect("a message");
    assert!(message.contains("models.list"), "{message}");
    // And the session is healthy: the next request answers normally.
    assert_eq!(server.result("initialize", json!({}))["protocol"], PROTOCOL);
}

/// A notification must be **executed and unanswered**. The silent half is easy to fake (drop the line on the floor); the executed half is easy to
/// fake too (answer it anyway). Both are asserted here: `INDEX.md` on disk proves the scan ran, and the single response line proves nothing was
/// printed for the notification.
#[test]
fn a_notification_runs_and_answers_nothing() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    assert!(
        !server.root.join("INDEX.md").exists(),
        "test setup: nothing indexed yet"
    );
    server.send_raw(
        &json!({"jsonrpc": "2.0", "method": "scan", "params": {"workspace": workspace}})
            .to_string(),
    );

    // The answer to the *next* request is the first line on stdout, so nothing was written for the notification.
    let initialize = server.request("initialize", json!({}));
    assert_eq!(initialize["result"]["protocol"], PROTOCOL);
    assert!(
        server.root.join("INDEX.md").exists(),
        "the notification was ignored rather than executed"
    );
}

/// Garbage gets a fault with `id: null` when it looked like a request, and nothing at all when it did not — and neither may end the session. A
/// client that writes half a buffer must be told; a client that writes noise must not be drowned.
#[test]
fn malformed_input_is_answered_when_it_names_an_id_and_ignored_when_it_does_not() {
    let mut server = Server::new();
    // A client that flushes a partial buffer writes a line that is not valid JSON. `writeln!` gives us that shape here: the newline arrives, so the
    // server sees one unparseable request and must answer it — with `id: null`, because reading the id is exactly what failed.
    server.send_raw(r#"{"jsonrpc":"2.0","id":99,"method":"scan"#);
    let response = server.read_line();
    assert_eq!(response["error"]["code"], -32700, "{response}");
    assert!(response["id"].is_null(), "{response}");
    // The leftover fragment of that buffer is its own line and therefore its own fault. Loud but harmless: the session survives either way, which
    // is what a client that retries needs.
    server.send_raw("}");
    let leftover = server.read_line();
    assert_eq!(leftover["error"]["code"], -32700, "{leftover}");

    // A blank line is a keepalive, not a request: no response, because there is nothing there to have failed.
    server.send_raw("");
    // Text that is not JSON at all *is* answered, and this is the one place the server cannot tell a request from a notification: reading the id is
    // exactly what failed. JSON-RPC 2.0 §5 resolves it the same way — a parse error gets a response with `id: null` — because silence here would
    // leave a client that mistyped a buffer waiting forever with no clue.
    server.send_raw("not json at all");
    let noise = server.read_line();
    assert_eq!(noise["error"]["code"], -32700, "{noise}");
    assert!(noise["id"].is_null(), "{noise}");
    let next = server.request("initialize", json!({}));
    assert_eq!(next["result"]["protocol"], PROTOCOL);
}

/// `np serve` must exit **0** when its client hangs up. Every stdio client in existence closes stdin to say it is done, and a non-zero exit would
/// be logged as a crash by whatever supervises the app.
#[test]
fn closing_stdin_ends_the_session_successfully() {
    let mut server = Server::new();
    assert_eq!(server.result("initialize", json!({}))["protocol"], PROTOCOL);
    server.hang_up();
    let status = server.child.wait().expect("wait for np serve");
    assert!(status.success(), "np serve exited {status:?} on EOF");
    // Killing an already-exited child is fine; `Drop` handles it.
    let _ = server.child.kill();
}

/// The two channels must never swap contents, and this is the case that reads both of them.
///
/// stdout is checked by construction (`read_line` fails on anything that is not a single JSON object with an envelope), so what needs asserting
/// explicitly is the other direction: **no protocol bytes may land on stderr**. A client that logs stderr verbatim into its support bundle — which
/// is what Stage 10 of the operator's Windows harness does for `--json` — would otherwise capture a response it cannot attribute, and a second copy
/// of a document on the wrong descriptor is exactly as confusing as prose on stdout.
#[test]
fn the_two_channels_never_swap_their_content() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    server.result("scan", json!({"workspace": workspace}));
    server.result(
        "search",
        json!({"workspace": workspace, "query": "meeting"}),
    );
    // A refusal, because its sentence is the most likely thing to be printed in the wrong place.
    let _ = server.request("scan", json!({"workspace": "/definitely/not/here"}));
    server.hang_up();
    let status = server.child.wait().expect("wait for np serve");
    assert!(status.success(), "np serve exited {status:?}");

    let stderr = server.stderr_text();
    for line in stderr.lines() {
        assert!(
            !line.trim_start().starts_with('{'),
            "a JSON object reached stderr, where a client expects prose: {line}"
        );
    }
    // And the human channel stays ours to describe: `np:` prefixed diagnostics only, never a bare stack or a panic report.
    for line in stderr.lines() {
        assert!(
            !line.contains("panicked at"),
            "a panic escaped to the client's log: {line}"
        );
    }
}

/// **Review Focus 1.** Progress is a favour the client asked for. A v0.3.0 client that sends the same three requests it
/// always sent must see three lines and nothing else — one unsolicited notification would break a parser that reads a
/// fixed number of lines, which is exactly how a compatible-looking addition becomes someone's crash.
#[test]
fn a_client_that_never_opts_in_sees_exactly_one_line_per_request() {
    let mut server = Server::new();
    let workspace = server.root.clone();

    let init = server.result("initialize", json!({}));
    assert_eq!(init["protocol"], PROTOCOL, "{init}");
    assert!(
        !init.as_object().expect("object").contains_key("progress"),
        "a client that did not ask must not be told progress exists: {init}"
    );

    let scan = server.result(
        "scan",
        json!({"workspace": workspace.display().to_string()}),
    );
    assert_eq!(
        scan["notes"], CORPUS_NOTES,
        "the fixture scanned the committed corpus: {scan}"
    );
    // `provider = "none"` is the configured default, so this is arm (ii): a degraded run reported as data, which is
    // also the shape that must not become a notification stream for a client that did not ask.
    let embed = server.result(
        "embed",
        json!({"workspace": workspace.display().to_string()}),
    );
    assert!(
        embed.get("outcome").is_some(),
        "the embed document still carries its own word for what happened: {embed}"
    );

    let rest = server.finish();
    assert!(
        rest.is_empty(),
        "an opted-out client must be sent nothing it did not ask for: {rest:?}"
    );
}

/// The other half of the same claim: opting in changes **what is streamed**, never **what is answered**. Every extra
/// line is a well-formed `np/progress` notification naming a request that was actually sent, and every response
/// document carries the same outcome and state keys as the one the quiet session got for the same work.
#[test]
fn opting_in_adds_notifications_without_changing_a_single_response() {
    let mut quiet = Server::new();
    let quiet_ws = quiet.root.display().to_string();
    quiet.result("initialize", json!({}));
    let scan_quiet = quiet.result("scan", json!({"workspace": quiet_ws}));
    let embed_quiet = quiet.result("embed", json!({"workspace": quiet_ws}));

    let mut talker = Server::new();
    let ws = talker.root.display().to_string();
    let init = talker.result("initialize", json!({"notifications": true}));
    assert_eq!(
        init["progress"], true,
        "support is advertised to the client that asked: {init}"
    );

    let mut results = serde_json::Map::new();
    for method in ["scan", "embed"] {
        results.insert(
            method.to_string(),
            talker.result(method, json!({"workspace": ws})),
        );
    }
    // The terminal notification of the last request follows its response, so it is read after hang-up rather than before.
    let rest = talker.finish();
    for line in &rest {
        assert!(
            is_progress(line),
            "an unexpected non-notification line: {line}"
        );
    }

    let extra = talker.progress.clone();
    for line in &extra {
        assert_eq!(
            line["method"], "np/progress",
            "an extra line must be a progress notification, never a second response: {line}"
        );
        assert!(
            !line.as_object().expect("object").contains_key("id"),
            "a notification carries no id member: {line}"
        );
        let params = line["params"].as_object().expect("params object");
        for key in ["command", "phase", "since_ms"] {
            assert!(
                params.contains_key(key),
                "always-present key `{key}` is missing: {line}"
            );
        }
        if let Some(request) = params.get("request") {
            assert!(
                request.is_number(),
                "`request` echoes a real id rather than a placeholder: {line}"
            );
        }
        assert!(
            !line.to_string().contains("http"),
            "**§12 rule 8**: a progress line names no origin: {line}"
        );
    }

    // Nothing about the answer depends on whether somebody is watching. The two sessions index different vaults, so
    // paths differ by construction; what must agree is every key that describes *what happened*.
    let scan_talker = &results["scan"];
    for key in ["notes", "added", "updated", "removed", "unchanged"] {
        assert_eq!(
            scan_quiet.get(key),
            scan_talker.get(key),
            "`{key}` changed because a client asked for progress: {} / {}",
            scan_quiet,
            scan_talker
        );
    }
    let embed_talker = &results["embed"];
    for key in [
        "outcome", "embedded", "owed", "offline", "fetched", "locked",
    ] {
        assert_eq!(
            embed_quiet.get(key),
            embed_talker.get(key),
            "`{key}` changed because a client asked for progress: {} / {}",
            embed_quiet,
            embed_talker
        );
    }

    let quiet_rest = quiet.finish();
    assert!(
        quiet_rest.is_empty(),
        "the quiet session received unsolicited lines: {quiet_rest:?}"
    );
}

/// A `np/progress` notification, as opposed to a response.
fn is_progress(value: &Value) -> bool {
    value.get("method").and_then(Value::as_str) == Some("np/progress")
}

/// The last progress line of a request, which is the one that must mirror the response kind (**spec §2's terminal rule**).
fn last_progress(lines: &[Value]) -> Value {
    lines
        .last()
        .unwrap_or_else(|| panic!("no progress line was written for this request at all"))
        .clone()
}

/// A degraded success is **`completed` carrying the state**, not `refused`. The distinction is the whole reason a GUI can
/// tell "someone else is writing" from "np broke": both are interruptions, and only one of them is an error object on the
/// wire. Contention is used here because it is reachable without any feature flags — the same lock a concurrent `np embed`
/// takes is held by this test process.
#[test]
fn a_contended_embed_ends_completed_with_locked_true() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    server.result("initialize", json!({"notifications": true}));
    server.result("scan", json!({"workspace": workspace}));

    let config = Config {
        data_dir: server.data_dir(),
        ..Config::default()
    };
    let db_path = config.workspace_db(&server.root);
    let held = WorkspaceLock::acquire(
        db_path
            .parent()
            .expect("a workspace database sits in a directory"),
        db_path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("a lockable name"),
        std::time::Duration::from_millis(200),
    )
    .expect("test setup: take the workspace write lock");

    server.id += 1;
    let id = server.id;
    server.send_raw(
        &json!({"jsonrpc": "2.0", "id": id, "method": "embed", "params": {"workspace": workspace}})
            .to_string(),
    );
    let response = server.read_line();

    assert_eq!(response["result"]["locked"], true, "{response}");
    assert_eq!(response["result"]["command"], "embed", "{response}");
    assert!(
        response.get("error").is_none(),
        "contention is a result, so its terminal line cannot be a refusal: {response}"
    );

    server.flush();
    let last = last_progress(&server.progress_for(id));
    assert_eq!(last["params"]["phase"], "completed", "{last}");
    assert_eq!(last["params"]["command"], "embed", "{last}");
    assert_eq!(
        last["params"]["locked"], true,
        "the state the client received travels on the terminal line: {last}"
    );
    for line in &server.progress_for(id) {
        assert!(
            !line.to_string().contains("http"),
            "**§12 rule 8**: no origin in a progress line: {line}"
        );
    }

    // Readers are unaffected, and the server is still itself afterwards.
    let search = server.result(
        "search",
        json!({"workspace": workspace, "query": "meeting"}),
    );
    assert!(
        !search["hits"].as_array().expect("hits").is_empty(),
        "a reader was refused while a writer held the workspace: {search}"
    );
    drop(held);
    server.finish();
}

/// The state keys a terminal line carries are the ones its own result carried — copied through, never invented. A search
/// over a vault with no vectors is the reachable degraded success in a default build: `vectors: false` is data, so the
/// phase says `completed`.
#[test]
fn a_search_without_vectors_ends_completed_with_its_own_state() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    server.result("initialize", json!({"notifications": true}));
    server.result("scan", json!({"workspace": workspace}));

    server.id += 1;
    let id = server.id;
    server.send_raw(
        &json!({"jsonrpc": "2.0", "id": id, "method": "search", "params": {"workspace": workspace, "query": "meeting"}})
            .to_string(),
    );
    let response = server.read_line();
    assert!(response.get("error").is_none(), "{response}");

    server.flush();
    let last = last_progress(&server.progress_for(id));
    assert_eq!(last["params"]["phase"], "completed", "{last}");
    assert_eq!(last["params"]["command"], "search", "{last}");
    for key in ["offline", "vectors", "reason"] {
        if let Some(value) = response["result"].get(key) {
            assert_eq!(
                last["params"].get(key),
                Some(value),
                "`{key}` was in the result and must be on the terminal line: {} / {last}",
                response["result"]
            );
        }
    }
    server.finish();
}

/// A real error object is **`refused`**, with the code that went out. Two kinds of ending, one rule: the terminal line
/// mirrors the response kind.
#[test]
fn an_unknown_method_ends_refused_with_the_code() {
    let mut server = Server::new();
    server.result("initialize", json!({"notifications": true}));

    server.id += 1;
    let id = server.id;
    server.send_raw(&json!({"jsonrpc": "2.0", "id": id, "method": "nope"}).to_string());
    let response = server.read_line();
    assert_eq!(response["error"]["code"], -32601, "{response}");

    server.flush();
    let last = last_progress(&server.progress_for(id));
    assert_eq!(last["params"]["phase"], "refused", "{last}");
    assert_eq!(last["params"]["command"], "nope", "{last}");
    assert_eq!(last["params"]["code"], -32601, "{last}");
    server.finish();
}

/// **Review Focus 5.** A command sent as a notification still narrates itself — with no `request` key anywhere. Inventing
/// an id would let a client believe two unrelated commands were one, and answering the command with a response line would
/// break the rule that notifications are silent.
#[test]
fn a_notification_command_produces_progress_without_a_request_key() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    server.result("initialize", json!({"notifications": true}));

    server.send_raw(
        &json!({"jsonrpc": "2.0", "method": "scan", "params": {"workspace": workspace}})
            .to_string(),
    );
    let rest = server.finish();
    // After hang-up the remaining lines are the id-less scan's own, plus `initialize`'s terminal line — which *does*
    // carry a request key, because that request had an id. So the claim is about the scan's lines: they exist, and none
    // of them names a request.
    let scan_lines: Vec<&Value> = rest
        .iter()
        .filter(|line| line["params"]["command"] == "scan")
        .collect();
    assert!(
        !scan_lines.is_empty(),
        "a command that runs still narrates itself, even unanswered: {rest:?}"
    );
    for line in &rest {
        assert!(
            is_progress(line),
            "a notification earned a response line: {line}"
        );
        assert!(
            !line.as_object().expect("object").contains_key("id"),
            "a notification carries no id member: {line}"
        );
    }
    for line in &scan_lines {
        let params = line["params"].as_object().expect("params object");
        assert!(
            !params.contains_key("request"),
            "no id was sent, so no request key may appear: {line}"
        );
        for key in ["command", "phase", "since_ms"] {
            assert!(
                params.contains_key(key),
                "always-present key `{key}`: {line}"
            );
        }
    }
}

/// `status.get` is the one question a client can ask *while nothing of its own is running* — the push-only design means
/// the server answers "what just finished, and what is on disk" rather than "what is happening now". Three keys, always
/// present, so a client reads `result["running"]` without a contains-check: `last`, `fetch_markers`, `running`.
#[test]
fn status_get_answers_three_keys_before_anything_is_indexed() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();

    let status = server.result("status.get", json!({"workspace": workspace}));
    assert!(
        status["last"].is_null(),
        "nothing has finished on this connection yet: {status}"
    );
    assert_eq!(
        status["fetch_markers"],
        Value::Array(Vec::new()),
        "a workspace nobody has fetched into has no markers: {status}"
    );
    assert!(
        status["running"].is_null(),
        "`running` ships as null and documented so, because a server that handles one request at a time cannot answer \
         what is in flight while it is answering this one: {status}"
    );
}

/// A marker another process wrote is reported **verbatim** — including an unknown size, which stays unknown (§12 rule 12)
/// rather than becoming zero. Reading it must not touch it: §12 rule 4 gives no read command the power to end a fetch.
#[test]
fn status_get_reports_a_marker_another_process_left() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    let config = Config {
        data_dir: server.data_dir(),
        ..Config::default()
    };
    let models_dir = config.workspace_models_dir(&server.root);
    fs::create_dir_all(&models_dir).expect("test setup: models dir");
    let marker = np_core::models::marker_path(&models_dir, "embeddinggemma-2");
    fs::write(
        &marker,
        r#"{"nonce":"1730000000-1-0","started_at":1730000000,"bytes_done":67108864,"bytes_total":null}"#,
    )
    .expect("test setup: marker");

    let status = server.result("status.get", json!({"workspace": workspace}));
    let markers = status["fetch_markers"].as_array().expect("array");
    assert_eq!(markers.len(), 1, "{status}");
    assert_eq!(markers[0]["id"], "embeddinggemma-2", "{status}");
    assert_eq!(markers[0]["state"], "running", "{status}");
    assert_eq!(markers[0]["bytes_done"], 67_108_864, "{status}");
    assert!(
        markers[0].get("bytes_total").is_none(),
        "an unannounced size is absent, not zero: {status}"
    );
    assert_eq!(
        fs::read_to_string(&marker).expect("marker still readable"),
        r#"{"nonce":"1730000000-1-0","started_at":1730000000,"bytes_done":67108864,"bytes_total":null}"#,
        "**§12 rule 4**: a status read leaves the marker exactly as it found it"
    );

    // The last finished activity is this connection's own history: `initialize` ran before this request, so `last` names
    // it rather than the status call still in flight.
    assert_eq!(
        status["last"],
        Value::Null,
        "first request on the connection: {status}"
    );
    server.result("scan", json!({"workspace": workspace}));
    let after = server.result("status.get", json!({"workspace": workspace}));
    assert_eq!(after["last"]["command"], "scan", "{after}");
    assert_eq!(after["last"]["phase"], "completed", "{after}");
    assert!(
        after["last"]["elapsed_ms"].is_number(),
        "a status line without its elapsed time cannot be compared with anything: {after}"
    );
    assert!(
        !after.to_string().contains("http"),
        "**§12 rule 8**: no origin in a status document: {after}"
    );
}

/// **Is this index stale? Answered without doing work.** The question a GUI asks most often is whether what it is showing has
/// gone old, and until now answering it meant running a scan — work, not a reading. `last_scan` is stamped inside the scan's own
/// transaction, so it cannot claim a scan that never committed, and it is per workspace, because staleness is a property of one
/// vault and never of the machine.
#[test]
fn status_get_says_when_this_workspace_was_last_scanned() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();

    let before = server.result("status.get", json!({"workspace": workspace}));
    assert_eq!(
        before["last_scan"],
        Value::Null,
        "a vault nobody has indexed has no scan time — not the same sentence as 0: {before}"
    );

    let floor = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock")
        .as_secs();
    server.result("scan", json!({"workspace": workspace}));
    let after = server.result("status.get", json!({"workspace": workspace}));
    let stamp = after["last_scan"]
        .as_u64()
        .expect("a scan time after scanning");
    assert!(
        (floor..floor + 120).contains(&stamp),
        "the stamp has to be this scan, not a leftover or a date in the future: {after}"
    );

    // A different vault answers for itself. Without this half, a stamp written into a shared place would pass every other
    // assertion here while telling vault B that vault A is fresh.
    let mut other = Server::new();
    let other_path = other.root.display().to_string();
    let untouched = other.result("status.get", json!({"workspace": other_path}));
    assert_eq!(
        untouched["last_scan"],
        Value::Null,
        "scanning vault A cannot make vault B look fresh: {untouched}"
    );
    server.finish();
    other.finish();
}

/// **Review Focus 3.** A status question must not create the thing it asks about. `scan` and `embed` open (and so
/// create) a workspace database; if `status.get` shared that path, then a GUI polling for status would litter a
/// nonexistent vault with `.sqlite` files — and each one reads as "an indexed workspace" to every other tool.
#[test]
fn status_get_never_creates_a_workspace_database() {
    let mut server = Server::new();
    let data_dir = server.data_dir();
    // A path that is not a directory and does not exist: the shape a typo or a deleted vault produces.
    let missing = server
        ._tmp
        .path()
        .join("no-such-vault")
        .display()
        .to_string();

    let before = listing_of(&data_dir);
    let status = server.result("status.get", json!({"workspace": missing}));
    let after = listing_of(&data_dir);

    assert_eq!(
        before, after,
        "a status read wrote something into the data dir"
    );
    assert!(
        !after.iter().any(|name| name.ends_with(".sqlite")),
        "a status read created a workspace database: {after:?}"
    );
    assert!(
        status["fetch_markers"]
            .as_array()
            .expect("array")
            .is_empty(),
        "a vault that is not there has no markers, and says so as data: {status}"
    );
}

/// Every entry name under `root`, sorted, with directories walked. Cheap enough for a test data dir and exact about
/// "nothing appeared".
fn listing_of(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        } {
            let entry = entry.expect("test: readdir entry");
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            found.push(path.display().to_string());
        }
    }
    found.sort();
    found
}

/// **§9's bookends.** A scan says it started and says it ended, with counts on the closing line — and nothing in
/// between. The measurement behind the design (a solo 4 000-note scan at 2.6 s) is why a per-file callback was refused:
/// threading a reporter through np-core's hottest loop to narrate two seconds costs more than it tells.
///
/// The closing line is written **unthrottled**, and this test is the reason: on a corpus this size both lines land in the
/// same second, so a one-per-second rule applied to the pair would drop the ending and leave a client believing the walk
/// was still going.
#[test]
fn a_scan_is_bookended_by_two_indexing_lines_and_nothing_between() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    server.result("initialize", json!({"notifications": true}));

    server.id += 1;
    let id = server.id;
    server.send_raw(
        &json!({"jsonrpc": "2.0", "id": id, "method": "scan", "params": {"workspace": workspace}})
            .to_string(),
    );
    let response = server.read_line();
    assert!(response.get("error").is_none(), "{response}");
    server.flush();

    let lines = server.progress_for(id);
    let indexing: Vec<&Value> = lines
        .iter()
        .filter(|line| line["params"]["phase"] == "indexing")
        .collect();
    assert_eq!(
        indexing.len(),
        2,
        "one line at entry, one at exit, nothing between: {lines:?}"
    );
    let opened = indexing[0]["params"].as_object().expect("params");
    assert!(
        !opened.contains_key("files_seen") && !opened.contains_key("notes_seen"),
        "the opening line cannot know counts it has not counted: {}",
        indexing[0]
    );
    let closed = indexing[1]["params"].as_object().expect("params");
    assert_eq!(
        closed.get("notes_seen").and_then(Value::as_i64),
        Some(CORPUS_NOTES as i64),
        "the closing line carries the report's own count: {}",
        indexing[1]
    );
    assert!(
        closed
            .get("files_seen")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            >= CORPUS_NOTES,
        "every fixture note is a file this pass looked at: {}",
        indexing[1]
    );
    // The terminal line still comes last, after both bookends.
    assert_eq!(
        lines.last().expect("a terminal line")["params"]["phase"],
        "completed",
        "{lines:?}"
    );
    server.finish();
}

/// **O2's stall, narrated.** Hybrid search may resolve a provider and dial out for 309 MB before it writes a byte of an
/// answer, so the phase that covers that stretch has to appear on the wire — otherwise a GUI shows a spinner over a
/// request that is doing something entirely reasonable. Keyword mode never resolves anything, so `resolving` is absent
/// there rather than reported as instant: absence carries information here.
#[test]
fn a_search_narrates_the_resolution_it_may_have_to_wait_for() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    server.result("initialize", json!({"notifications": true}));
    server.result("scan", json!({"workspace": workspace}));

    server.id += 1;
    let id = server.id;
    server.send_raw(
        &json!({"jsonrpc": "2.0", "id": id, "method": "search", "params": {"workspace": workspace, "query": "meeting"}})
            .to_string(),
    );
    let response = server.read_line();
    assert!(response.get("error").is_none(), "{response}");
    server.flush();

    let lines = server.progress_for(id);
    assert!(
        lines
            .iter()
            .any(|line| line["params"]["phase"] == "resolving"),
        "the default mode can fetch, so it says it is deciding first: {lines:?}"
    );

    // The other half of the claim: a mode that never resolves a provider never claims to be doing so.
    let mut quiet = Server::new();
    let other = quiet.root.display().to_string();
    quiet.result("initialize", json!({"notifications": true}));
    quiet.result("scan", json!({"workspace": other}));
    quiet.id += 1;
    let keyword_id = quiet.id;
    quiet.send_raw(
        &json!({"jsonrpc": "2.0", "id": keyword_id, "method": "search", "params": {"workspace": other, "query": "meeting", "mode": "keyword"}})
            .to_string(),
    );
    let keyword = quiet.read_line();
    assert!(keyword.get("error").is_none(), "{keyword}");
    quiet.flush();
    assert!(
        !quiet
            .progress_for(keyword_id)
            .iter()
            .any(|line| line["params"]["phase"] == "resolving"),
        "`--mode keyword` resolves nothing and must not claim it is: {:?}",
        quiet.progress_for(keyword_id)
    );
    server.finish();
    quiet.finish();
}

/// A fetch that really reaches its transport, narrated through the pipe (**O2's** justification for putting search in
/// scope at all).
///
/// **What is served, and why.** The origin answers with the pinned `Content-Length` and then hangs up mid-body after a few
/// MiB, so the run ends in `fetch`'s own *interrupted* refusal. That is deliberate: it exercises the byte loop over a real
/// socket with real bytes and real digests-in-progress without transferring 309 MB inside `cargo test`, and it proves the
/// ending that matters most — a transport interruption is reported as a **refusal**, while an outage is reported as a
/// state. What this case cannot show is a verified download; that stays with the artifact-gated rows in
/// `.github/scripts/gates.sh`.
///
/// **Why the server pauses.** The throttle allows one line per second inside a phase, so a loopback transfer that finishes
/// in 40 ms correctly produces a single `fetching` line. Asserting "several lines" against a fast server would be asserting
/// that the throttle is broken, so the origin dribbles: each chunk arrives more than a second after the last, and two or
/// more checkpoints become observable rather than throttled away.
#[cfg(all(feature = "local-model", feature = "fetch"))]
#[test]
fn a_fetch_that_reaches_its_transport_narrates_bytes_and_ends_refused() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    let pinned = np_core::pins::PINNED
        .first()
        .expect("a build with `fetch` pins something");
    const CHUNK: usize = 4 * 1024 * 1024;
    const CHUNKS: usize = 3;

    let listener = TcpListener::bind("127.0.0.1:0").expect("test setup: bind a loopback origin");
    let port = listener
        .local_addr()
        .expect("test setup: origin addr")
        .port();
    let declared = pinned.size_bytes;
    // The origin thread owns the listener and loops until the process exits; there is no shutdown to wait for, and a
    // `join` would hang on an `accept` nothing is coming to. Every request in this case is answered before it ends.
    let _worker = thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut probe = [0_u8; 2048];
            let _ = stream.read(&mut probe).unwrap_or(0);
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\n\r\n");
            if stream.write_all(head.as_bytes()).is_err() {
                continue;
            }
            let body = vec![0_u8; CHUNK];
            for _ in 0..CHUNKS {
                if stream.write_all(&body).is_err() {
                    break;
                }
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(1_200));
            }
            // EOF mid-body. `fetch` calls this interrupted, not offline — and the progress lines before it are the point.
            drop(stream);
        }
    });

    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    // Written after spawn and before the request: `load_config` runs per command, so a serve session picks up a config
    // that appeared while it was already running. The URL goes only into the child's config — never into an assertion
    // message, which is how §12 rule 8 stays true by construction rather than by care.
    let config_dir = server.config_dir_for_test();
    fs::create_dir_all(&config_dir).expect("test setup: config package dir");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[embeddings]\nprovider = \"local-model\"\nmodel = \"{}\"\n[models]\nbase_url = \"http://127.0.0.1:{port}\"\n",
            pinned.id
        ),
    )
    .expect("test setup: write user config");

    server.result("initialize", json!({"notifications": true}));
    let response = server.request("embed", json!({"workspace": workspace}));
    server.flush();
    let lines = {
        let mut all = server.progress.clone();
        all.extend(server.finish());
        all
    };

    let fetching: Vec<&Value> = lines
        .iter()
        .filter(|line| line["params"]["phase"] == "fetching")
        .collect();
    assert!(
        fetching.len() >= 2,
        "a dribbled transfer earns more than one byte checkpoint: {lines:?}"
    );
    let mut previous = None;
    for line in &fetching {
        let params = &line["params"];
        assert_eq!(params["member"], pinned.member, "{line}");
        assert_eq!(
            params["bytes_total"], pinned.size_bytes,
            "the denominator is the pinned size, never what the transport announced: {line}"
        );
        let done = params["bytes_done"].as_u64().expect("a byte count");
        if let Some(before) = previous {
            assert!(done >= before, "bytes move forward within a member: {line}");
        }
        previous = Some(done);
    }

    // The ending mirrors the response kind: an interrupted transfer is a refusal, and it carries the code that went out.
    assert!(
        response.get("error").is_some(),
        "a transfer that stopped early must not be reported as a success: {response}"
    );
    let last = lines
        .iter()
        .rev()
        .find(|line| {
            line["params"]
                .get("request")
                .is_some_and(|r| *r == response["id"])
        })
        .expect("a terminal line for the request");
    assert_eq!(last["params"]["phase"], "refused", "{last}");
    assert_eq!(last["params"]["code"], -32000, "{last}");

    // §12 rule 8, on both channels: the origin's URL appears nowhere in what the client was told.
    let transcript = lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !transcript.contains("http"),
        "a progress line named an origin: {transcript}"
    );
    assert!(
        !server.stderr_text().contains(&format!("127.0.0.1:{port}")),
        "stderr named the origin and port"
    );
}

/// The same claim, stated as a gap when the build cannot make it: a `fetch`-less binary has no transport to narrate, and
/// printing nothing here would let a default-feature run look like coverage it did not have.
#[cfg(not(all(feature = "local-model", feature = "fetch")))]
#[test]
fn a_fetch_that_reaches_its_transport_narrates_bytes_and_ends_refused() {
    eprintln!("NOT COVERAGE: fetch narration needs `--features local-model,fetch`");
}

/// **The document is part of the interface.** A method that exists but is not written down is a method nobody can use, and a
/// doc that describes only the methods someone remembered to mention reads like a complete one. So the list comes from the
/// binary's own `METHODS`, and the file is read from the repository — if they drift, this fails rather than the reader finding
/// out.
#[test]
fn the_reference_documents_every_method() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join("docs/reference-serve.md");
    let doc = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} must exist: {err}", path.display()));

    // The list comes off the wire rather than out of the source, which is the stronger claim: it is what a client is told it
    // may call, including anything a future task adds to the handshake.
    let mut server = Server::new();
    let handshake = server.request(
        "initialize",
        json!({"workspace": server.root.display().to_string()}),
    );
    let methods: Vec<String> = handshake["result"]["methods"]
        .as_array()
        .expect("initialize reports its methods")
        .iter()
        .map(|value| value.as_str().expect("a method name").to_string())
        .collect();
    assert!(
        methods.len() >= 7,
        "the handshake lost methods: {methods:?}"
    );
    server.finish();

    for method in &methods {
        assert!(
            doc.contains(&format!("`{method}`")),
            "`{method}` is offered over the pipe but appears nowhere in docs/reference-serve.md"
        );
    }
    // The two keys a client must find to use progress at all, and the state key that is easiest to forget documenting.
    for needle in [
        "`np/progress`",
        "`notifications`",
        "`prefix_drift`",
        "`running`",
    ] {
        assert!(
            doc.contains(needle),
            "the reference never mentions {needle}, so a client cannot learn it exists"
        );
    }

    // And the transcripts are transcripts: every line inside their fences parses as one JSON document, because a block with a
    // hand-typed ellipsis in it teaches a client to expect lines that no server ever sent.
    // README carries its own captured block, and the same rule applies to it: a composite assembled by hand from two sessions is
    // how a document ends teaching clients a sequence no server ever sent (`since_ms` that runs backwards, a `fetching` line
    // followed by an offline result). So both files are parsed here rather than just the reference.
    let readme = fs::read_to_string(root.join("README.md")).expect("README must exist");
    let mut checked = 0;
    let mut inside = false;
    for line in doc.lines().chain(readme.lines()) {
        if line.trim_start().starts_with("```json") {
            inside = true;
            continue;
        }
        if inside && line.trim_start() == "```" {
            inside = false;
            continue;
        }
        if inside {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed: Result<Value, _> = serde_json::from_str(trimmed);
            assert!(
                parsed.is_ok(),
                "a documented transcript line is not one JSON document: {trimmed}"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 25,
        "the reference should carry real transcripts, not prose about them (checked {checked} lines)"
    );
}

/// **The throttle is per request, not per connection.** Two searches sent back to back on one pipe are two requests, and
/// each owes its client the line that says what it is about to wait for. Carrying throttle memory across the boundary
/// swallows the second `resolving` — a client driving a GUI's search box would see the first keystroke narrate and every
/// later one go silent, which is precisely the "why is it not telling me anything" report this feature exists to end.
#[test]
fn two_requests_on_one_connection_each_open_their_own_phase() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    server.result("initialize", json!({"notifications": true}));

    let mut ids = Vec::new();
    for _ in 0..2 {
        server.id += 1;
        let id = server.id;
        ids.push(id);
        server.send_raw(
            &json!({"jsonrpc": "2.0", "id": id, "method": "search", "params": {"workspace": workspace, "query": "meeting"}})
                .to_string(),
        );
        let response = server.read_line();
        assert!(response.get("error").is_none(), "{response}");
    }
    // Both requests were answered before anything was drained, so both sets of lines are in the same session and the
    // second one starts inside a second of the first — the exact timing the leak used to eat.
    server.flush();

    for id in ids {
        let lines = server.progress_for(id);
        assert!(
            lines
                .iter()
                .any(|line| line["params"]["phase"] == "resolving"),
            "request {id} on a busy connection lost its opening phase: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line["params"]["phase"] == "completed"),
            "request {id} never got its ending: {lines:?}"
        );
    }
    server.finish();
}

/// **`shutdown` is the request it looks like.** The client asked to be done; the answer goes out and the process stops,
/// without the client also having to close stdin. Reading EOF here while our own write half is still open is the proof:
/// only the server can close that pipe, and it closed it because it was told to.
#[test]
fn shutdown_ends_the_session_without_waiting_for_eof() {
    let mut server = Server::new();
    let handshake = server.request(
        "initialize",
        json!({"workspace": server.root.display().to_string()}),
    );
    assert_eq!(handshake["result"]["protocol"], 1, "{handshake}");

    server.id += 1;
    let id = server.id;
    server.send_raw(&json!({"jsonrpc": "2.0", "id": id, "method": "shutdown"}).to_string());
    let response = server.read_line();
    assert_eq!(response["id"], id, "{response}");
    assert_eq!(
        response["result"]["protocol"], 1,
        "the answer names the protocol it stopped speaking: {response}"
    );

    // Stdin is deliberately left open. If `shutdown` were merely noted, the next read would block forever instead of
    // returning EOF.
    assert!(
        server.stdin.is_some(),
        "this case never closed its write half"
    );
    // **Wait for the exit, with a deadline.** Polling `try_wait` rather than blocking on stdout means a regression — a
    // server that notes `shutdown` and keeps reading — fails this case in ten seconds instead of hanging the suite, which is
    // how the first version of it behaved. Stdin stays open the whole time: only the server can close *its* write half, so
    // an exit is the proof that it stopped because it was told to.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut exited = None;
    while std::time::Instant::now() < deadline {
        if let Some(status) = server.child.try_wait().expect("test setup: poll the child") {
            exited = Some(status);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let status = exited.expect("ten seconds after `shutdown` the process was still running");
    assert_eq!(
        status.code(),
        Some(0),
        "a shutdown that ends in an error is a bug report"
    );

    let mut leftover = String::new();
    let read = server
        .stdout
        .read_line(&mut leftover)
        .expect("test setup: read after exit");
    assert_eq!(
        read, 0,
        "a process that exited cannot have more to say; the harness is reading a stale pipe: {leftover}"
    );
}

/// **An unreadable marker is reported, not repaired** — over the pipe, not just one layer down. A marker written by a
/// killed process can be truncated, and `status.get` has to say "something is here and I cannot read it" instead of
/// either deleting the evidence or pretending the directory is empty.
#[test]
fn an_unreadable_marker_is_reported_and_left_alone() {
    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    let config = Config {
        data_dir: server.data_dir(),
        ..Config::default()
    };
    let models_dir = config.workspace_models_dir(&server.root);
    fs::create_dir_all(&models_dir).expect("test setup: models dir");
    let marker = np_core::models::marker_path(&models_dir, "embeddinggemma-2");
    // Half a JSON object: what a SIGKILL at the wrong moment leaves behind.
    fs::write(&marker, r#"{"nonce":"1730000000-1-","#).expect("test setup: torn marker");

    let status = server.result("status.get", json!({"workspace": workspace}));
    let markers = status["fetch_markers"].as_array().expect("array");
    assert_eq!(markers.len(), 1, "{status}");
    assert_eq!(markers[0]["state"], "unreadable", "{status}");
    assert_eq!(markers[0]["id"], "embeddinggemma-2", "{status}");
    assert!(
        markers[0]["reason"].is_string(),
        "the reason is the only thing a reader can act on: {status}"
    );
    assert!(
        !status.to_string().contains("http"),
        "**§12 rule 8**: {status}"
    );
    assert_eq!(
        fs::read_to_string(&marker).expect("marker still there"),
        r#"{"nonce":"1730000000-1-","#,
        "**§12 rule 4**: a read command does not clean up after a crashed writer"
    );
    server.finish();
}

/// **The phase the whole feature was requested for.** A GUI asked to be able to say "embedding 118 of 412"; that sentence
/// needs `embedding` lines on the wire, and nothing else in this suite would notice if they stopped arriving — np-core's
/// seam tests call `embed_with_inject` directly, so they pass whatever `np serve` hands the chunk loop. This is the case
/// that closes that gap, and it runs against a real model because an embed that writes no vectors emits no such line.
///
/// Gated on `local-model` **and** the artifact: without either, this prints `NOT COVERAGE` rather than passing on a path
/// it never reached.
#[cfg(all(feature = "local-model", feature = "fetch"))]
#[test]
fn an_embed_that_works_narrates_the_chunks_it_encodes() {
    let source = match std::env::var_os("NP_TEST_MODEL_PATH").map(PathBuf::from) {
        Some(path) => path,
        None => {
            eprintln!(
                "NOT COVERAGE: NP_TEST_MODEL_PATH is unset, so no chunk was encoded and no `embedding` line was read."
            );
            return;
        }
    };

    let mut server = Server::new();
    let workspace = server.root.display().to_string();
    // Install through the real command: a row written by `np models install` is what makes this an embed of an artifact
    // rather than a fetch, and keeps the digest check on the path.
    let pinned = np_core::pins::PINNED
        .iter()
        .find(|pin| pin.member == "main")
        .expect("test setup: the main member is pinned");
    let installer = Command::new(env!("CARGO_BIN_EXE_np"))
        .args([
            "models",
            "install",
            &workspace,
            "--id",
            pinned.id,
            "--path",
            &source.display().to_string(),
            "--dim",
            "768",
            "--sha256",
            pinned.sha256,
        ])
        .env("XDG_DATA_HOME", server.data_dir())
        .env("LOCALAPPDATA", server.data_dir())
        .env("HOME", server._tmp.path())
        .env("XDG_CONFIG_HOME", server._tmp.path().join("config"))
        .env("APPDATA", server._tmp.path().join("config"))
        .env_remove("NP_NO_AUTO_FETCH")
        .output()
        .expect("test setup: run np models install");
    assert!(
        installer.status.success(),
        "install refused: {}",
        String::from_utf8_lossy(&installer.stderr)
    );

    let config_dir = server.config_dir_for_test();
    fs::create_dir_all(&config_dir).expect("test setup: config dir");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[embeddings]\nprovider = \"local-model\"\nmodel = \"{}\"\n",
            pinned.id
        ),
    )
    .expect("test setup: write user config");

    server.result("initialize", json!({"notifications": true}));
    // Index first. `embed` on a vault that was never scanned has nothing pending, takes the *current* arm, and writes no
    // chunk lines — which is correct behaviour and would make this case pass by proving nothing at all.
    let indexed = server.request("scan", json!({"workspace": workspace}));
    assert!(indexed.get("error").is_none(), "{indexed}");
    let response = server.request("embed", json!({"workspace": workspace}));
    server.flush();
    assert!(response.get("error").is_none(), "{response}");
    let lines = {
        let mut all = server.progress.clone();
        all.extend(server.finish());
        all
    };

    let embedding: Vec<&Value> = lines
        .iter()
        .filter(|line| line["params"]["phase"] == "embedding")
        .collect();
    assert!(
        !embedding.is_empty(),
        "an embed that encoded chunks wrote no `embedding` line: {lines:?}"
    );
    let first = &embedding[0]["params"];
    assert_eq!(
        first["chunks_done"].as_u64().expect("a count"),
        0,
        "the opening line says nothing is done yet: {}",
        embedding[0]
    );
    assert!(
        first.get("embedded").is_some(),
        "`embedded` is the number a progress bar reads: {}",
        embedding[0]
    );
    // **The denominator ships.** The run holds its owed list before the writer loop, so a total is free rather than another
    // query, and every GUI was drawing an indeterminate bar for want of it. It is the *plan* — chunks this run decided it
    // owed — never what remains, or the fraction would creep to 1/1 with work still queued.
    assert_eq!(
        first["chunks_total"].as_u64(),
        Some(1),
        "a one-note vault owes one chunk and says so from the opening line: {}",
        embedding[0]
    );
    // A one-note vault produces one chunk, so `embedding` legitimately appears exactly once and the *next* phase carries
    // the finished count — that is what the writing line below asserts. What this loop checks instead is the shape of the
    // counters: monotonic within the phase, no invented denominator, and every line naming the request it belongs to.
    let mut previous = 0;
    for line in &embedding {
        let done = line["params"]["chunks_done"].as_u64().expect("a count");
        assert!(done >= previous, "chunks move forward: {line}");
        previous = done;
    }
    // **`writing` only when something was written** (as-built #10): this run embedded a chunk, so the phase has to appear.
    assert!(
        lines
            .iter()
            .any(|line| line["params"]["phase"] == "writing"),
        "a run that wrote vectors never said `writing`: {lines:?}"
    );
    // The response and the line agree about how much was written.
    let embedded = response["result"]["embedded"]
        .as_u64()
        .expect("the report counts what it embedded");
    assert!(embedded > 0, "nothing was embedded: {}", response["result"]);
    let writing = lines
        .iter()
        .rev()
        .find(|line| line["params"]["phase"] == "writing")
        .expect("a writing line");
    assert_eq!(
        writing["params"]["embedded"].as_u64(),
        Some(embedded),
        "a progress line that disagrees with its own response is worse than no line: {writing}"
    );
    assert!(
        writing["params"]["chunks_done"].as_u64().unwrap_or(0) > 0,
        "the commit phase has to show what was committed: {writing}"
    );
}

/// The same claim, stated as a gap when the build cannot make it.
#[cfg(not(all(feature = "local-model", feature = "fetch")))]
#[test]
fn an_embed_that_works_narrates_the_chunks_it_encodes() {
    eprintln!(
        "NOT COVERAGE: chunk narration needs `--features local-model,fetch` and NP_TEST_MODEL_PATH"
    );
}
