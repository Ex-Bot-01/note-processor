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
        }
    }

    fn data_dir(&self) -> PathBuf {
        self._tmp.path().join("data")
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
        let mut line = String::new();
        let read = self
            .stdout
            .read_line(&mut line)
            .expect("read a response line");
        assert!(read > 0, "stdout closed before answering");
        let value: Value = serde_json::from_str(line.trim_end())
            .unwrap_or_else(|source| panic!("not one JSON object per line ({source}): {line:?}"));
        value
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
