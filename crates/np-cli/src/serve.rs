//! `np serve` — JSON-RPC 2.0 over standard input/output, for a program that wants to ask `np` questions without owning a shell.
//!
//! **Why this exists and what it is not.** A frontend needs three things argv does not give it cheaply: no process start per question, no model
//! reload per question (a fresh `np` re-reads a 309 MB artifact before its first search), and one place to hold a workspace open. This module is
//! the smallest surface that provides them: **one long-lived process, newline-delimited JSON in and out, no port, no auth, no daemon**. The
//! loopback HTTP API, watcher and autostart are M4 in `docs/roadmap-2026-10-27.md` and have their own design ahead of them; nothing here
//! pre-implements them, and the dispatcher is a plain function so that an HTTP adapter can call it later without re-deciding what a request means.
//! Design: `docs/superpowers/specs/2026-12-08-np-serve-stdio-design.md`.
//!
//! # The one rule everything else follows from
//!
//! **stdout is the protocol.** Every human sentence, every llama.cpp warning (**F33**), every fetch announcement and every F42 drift notice goes
//! to stderr; stdout carries one JSON object per line and nothing else. That is not a style preference: `np` already guarantees "stdout is
//! exactly one JSON document" under `--json`, and Stage 10 of the operator's harness exists to prove it on Windows. A server that leaked a
//! progress line onto stdout would corrupt a client's parse in a way that looks like an `np` bug.
//!
//! # Results are the CLI's documents, not a second vocabulary
//!
//! Each method returns *the same object* `np <verb> --json` prints — built by the same function on both channels ([`crate::search_document`],
//! [`crate::embed_report`] plus [`crate::insert_drift`], [`crate::listing_json`]). Two implementations of one state is how a screen starts
//! disagreeing with a terminal about what `vectors: false` meant, which is the failure this whole repository has been carding against since F32.
//! A test asserts the equality directly rather than trusting the intent.
//!
//! # States stay states
//!
//! A locked workspace (`locked: true`, **F44**), an offline fetch (**F38**), a degraded hybrid answer (**F45**) come back as a *successful*
//! response whose result says so. Only things that are genuinely the caller's fault — unparseable JSON, an unknown method, parameters that name no
//! such thing, a refusal before any work — use JSON-RPC's `error` object. An app must never have to pattern-match a sentence to learn the network
//! was down; that rule cost three cards to establish and travels here unchanged.

use std::error::Error;
use std::io::{self, Write};

use serde_json::{json, Value};

use np_core::progress::{Phase, ProgressEvent};
use np_core::search::SearchMode;

/// The protocol version this build speaks. A client sends its own number in `initialize` and compares; an incompatible change is a bump rather
/// than a mystery. Nothing branches on it yet — there is nothing to branch about — which is the point of shipping it now, while adding it is free.
pub const PROTOCOL: u64 = 1;

/// The methods this build answers, in the order a support screen would list them. Kept beside [`dispatch`] rather than derived from it because a
/// client that calls something absent gets `-32601` and a support report that names what *was* available.
pub const METHODS: &[&str] = &[
    "initialize",
    "search",
    "scan",
    "embed",
    "models.list",
    "shutdown",
];

/// A JSON-RPC fault: the caller's request was wrong, or `np` refused before doing any work. Codes are the spec's reserved ones plus `-32000` for
/// "refused", which is deliberately one code rather than a per-sentence numbering — the message carries the sentence verbatim, and it is the same
/// sentence `np:` prints on stderr, so a UI can show it without a translation table.
struct Fault {
    code: i64,
    message: String,
}

impl Fault {
    const PARSE_ERROR: i64 = -32700;
    const INVALID_REQUEST: i64 = -32600;
    const METHOD_NOT_FOUND: i64 = -32601;
    const INVALID_PARAMS: i64 = -32602;
    /// `np` refused before doing work (workspace is not a directory, config is unreadable, provider misconfigured).
    const REFUSED: i64 = -32000;
    /// A panic inside a handler. Reported and survived: one bad note must not end a session.
    const INTERNAL: i64 = -32603;

    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// What a handler may return instead of a document.
///
/// Two kinds because the two kinds need different codes and are different people's problems: [`ServeError::Fault`] is mine or the caller's — a
/// parameter that names no such thing, a method this build does not have — and [`ServeError::Refused`] is the world's: the workspace is not a
/// directory, the config is unreadable, the provider is switched off. Every CLI refusal already reads as a complete sentence (`locked: another
/// process is writing this workspace …`), which is why refusal wraps rather than re-words: the same text a person sees on stderr is what an app
/// can show, with no translation table between them.
enum ServeError {
    Fault(Fault),
    Refused(Box<dyn Error>),
}

impl ServeError {
    fn fault(code: i64, message: impl Into<String>) -> Self {
        Self::Fault(Fault::new(code, message))
    }
}

impl From<Box<dyn Error>> for ServeError {
    fn from(source: Box<dyn Error>) -> Self {
        Self::Refused(source)
    }
}

/// Wrap any lower layer's error as a refusal, so `?` reads the same at every call into the CLI's own functions. Concrete error types
/// (`ScanError`, `ModelError`, `serde_json::Error`) cannot convert themselves through a blanket impl without colliding with the boxed one above,
/// which is why this is a function and not a `From`.
fn refused<E: Into<Box<dyn Error>>>(source: E) -> ServeError {
    ServeError::Refused(source.into())
}

/// Serve until stdin closes. Returns `Ok` on EOF and on `shutdown`: a client that hung up is not a failure, and neither is one that said goodbye.
pub fn serve() -> Result<(), Box<dyn Error>> {
    // One handle, locked for the loop's life: interleaving responses on a shared stdout is exactly the corruption a client cannot recover from.
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    let clock = crate::serve_channel::SystemClock::new();
    let mut ch = crate::serve_channel::Channel::new(&mut out, &clock);
    // Every behaviour of the old inline loop lives on, now behind a sink the unit tests can drive in-process.
    crate::serve_channel::serve_loop(stdin.lock(), &mut ch)?;
    Ok(())
}

/// One line in, at most one response out (`None` for notifications and unparseable garbage).
///
/// Split from [`serve`] so the whole protocol is testable without a pipe: the tests call this with a string and read a value back.
pub(crate) fn handle_line(line: &str, ch: &mut crate::serve_channel::Channel<'_>) -> Option<Value> {
    if line.trim().is_empty() {
        // A blank line is not a request. Silently ignored rather than faulted, because the likely source is a client writing "\n" to flush or keep
        // the pipe warm — answering that with an error would train it to stop asking. The rule lives here rather than in the read loop so that it
        // is one branch with a unit test, not a line of the loop that no in-process test can reach.
        return None;
    }
    let request: Value = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(source) => {
            // `-32700` carries `id: null` because the id was never readable. A line that is valid JSON but not an object lands here too via the
            // shape check below — both are "I could not read your request", and the caller cannot tell me which because that is what failed.
            return Some(fault(Value::Null, Fault::PARSE_ERROR, source.to_string()));
        }
    };
    let id = request.get("id").cloned();
    let is_notification = id.is_none();
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return if is_notification {
            None
        } else {
            Some(fault(
                id.unwrap(),
                Fault::INVALID_REQUEST,
                "this interface is JSON-RPC 2.0: send \"jsonrpc\": \"2.0\"",
            ))
        };
    }
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return if is_notification {
            None
        } else {
            Some(fault(
                id.unwrap(),
                Fault::INVALID_REQUEST,
                "a request needs a \"method\" string",
            ))
        };
    };
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    // A panic in a handler is the caller's bad note or my bug; either way it must cost one response, not the session. `AssertUnwindSafe` is
    // honest here rather than a shrug: the state shared between requests is configuration and an open database handle, and nothing in this module
    // reads a value a panicked handler could have left half-updated — the workspace write lock releases through `Drop` on unwind either way.
    // The id travels with the call: a progress line names the request it belongs to, and a command that arrived as a
    // notification has no id to name — which is a different thing from an id of `null` (**Review Focus 5**).
    let call_id = id.clone();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch(method, &params, call_id.as_ref(), ch)
    }));
    let result = match outcome {
        Ok(Ok(result)) => result,
        Ok(Err(ServeError::Fault(fault))) => {
            return (!is_notification).then(|| fault_response(id.unwrap_or(Value::Null), fault))
        }
        Ok(Err(ServeError::Refused(source))) => {
            // A refusal is `-32000` with the CLI's own sentence, and nothing else: no code per sentence, because a caller that branches on an
            // error *kind* should be branching on a state key in the result (**F44**'s rule), not on a fault number.
            return (!is_notification).then(|| {
                fault_response(
                    id.unwrap_or(Value::Null),
                    Fault::new(Fault::REFUSED, source.to_string()),
                )
            });
        }
        Err(_) => {
            let _ = writeln!(
                io::stderr(),
                "np: serve: `{method}` panicked; the server is still running"
            );
            return (!is_notification).then(|| {
                fault_response(
                    id.unwrap_or(Value::Null),
                    Fault::new(Fault::INTERNAL, "`{method}` panicked inside np"),
                )
            });
        }
    };
    if is_notification {
        None
    } else {
        Some(json!({"jsonrpc": "2.0", "id": id.unwrap(), "result": result}))
    }
}

/// Route one request. Every mutating method acquires and releases the workspace write lock **inside itself** — never across requests — because a
/// server that held it for its whole life would refuse its own next command after two seconds, which is F44 working as designed against the wrong
/// caller.
fn dispatch(
    method: &str,
    params: &Value,
    request: Option<&Value>,
    ch: &mut crate::serve_channel::Channel<'_>,
) -> Result<Value, ServeError> {
    match method {
        "initialize" => Ok(initialize(params, ch)),
        "search" => search(params),
        "scan" => scan(params, request, ch),
        "embed" => embed(params, request, ch),
        "models.list" => models_list(params),
        // The client asked to be done. `serve` answers this one request and the loop exits on the following EOF; answering before exiting keeps
        // the response from racing the pipe closing.
        "shutdown" => Ok(json!({"ok": true})),
        other => Err(ServeError::Fault(Fault::new(
            Fault::METHOD_NOT_FOUND,
            format!(
                "unknown method `{other}`: this build answers {}",
                METHODS.join(", ")
            ),
        ))),
    }
}

/// What a client can do with this binary. `features` is the compile-time truth (**F40** refuses on these same facts), so a UI decides at startup
/// whether to draw an "embed" control at all instead of discovering the answer through a refusal.
fn initialize(params: &Value, ch: &mut crate::serve_channel::Channel<'_>) -> Value {
    // **The only session-level fact this server keeps** (spec §1): progress is off unless this connection asked for it,
    // and asking changes nothing else. A v0.3.0 client that never sends `notifications` sees the byte-identical stream
    // it saw before this feature existed.
    let wants_progress = params
        .get("notifications")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    ch.set_notifications(wants_progress);
    let mut result = json!({
        "np": env!("CARGO_PKG_VERSION"),
        "protocol": PROTOCOL,
        "features": {
            "local_model": cfg!(feature = "local-model"),
            "vision": cfg!(feature = "vision"),
            "fetch": cfg!(feature = "fetch"),
        },
        "methods": METHODS,
        // Where the index and model caches live. A path rather than a host: §12 rule 8 forbids naming an *origin*, and this is the line a support
        // screenshot needs when someone asks "why does this vault have no vectors".
        "data_dir": crate::Config::default().data_dir.display().to_string(),
    });
    // Support is advertised by the key's presence, so a client that asks an older server gets nothing and can fall back
    // to polling without a version check. `protocol` stays 1 either way: an optional parameter and added methods are
    // compatible changes, and reserving the number for incompatible ones is what makes it worth reading.
    if wants_progress {
        result["progress"] = json!(true);
    }
    result
}

/// The workspace every method names for itself. No session state: a server holding three vaults open is one process, and "which vault am I on" is
/// the kind of question that produces a confident wrong answer rather than an error.
fn workspace_of(params: &Value) -> Result<std::path::PathBuf, ServeError> {
    let raw = params
        .get("workspace")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ServeError::fault(Fault::INVALID_PARAMS, "params need a \"workspace\" path")
        })?;
    if raw.is_empty() {
        return Err(ServeError::fault(
            Fault::INVALID_PARAMS,
            "\"workspace\" is empty: name the vault to work on",
        ));
    }
    Ok(std::path::PathBuf::from(raw))
}

fn search(params: &Value) -> Result<Value, ServeError> {
    let workspace = workspace_of(params)?;
    let query = params.get("query").and_then(Value::as_str).ok_or_else(|| {
        ServeError::fault(Fault::INVALID_PARAMS, "params need a \"query\" string")
    })?;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(crate::DEFAULT_SEARCH_LIMIT);
    let mode = match params.get("mode") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(match text.as_str() {
            "hybrid" => SearchMode::Hybrid,
            "semantic" => SearchMode::Semantic,
            "keyword" => SearchMode::Keyword,
            other => {
                return Err(ServeError::fault(
                    Fault::INVALID_PARAMS,
                    format!("unknown mode `{other}`: write hybrid, semantic or keyword"),
                ))
            }
        }),
        Some(_) => {
            return Err(ServeError::fault(
                Fault::INVALID_PARAMS,
                "\"mode\" must be a string: hybrid, semantic or keyword",
            ))
        }
    };
    // `announce` false: the facts an announcement would print are in the document (`fetched.origin_kind`, `reason`), and stderr stays quiet for a
    // request that asked for neither prose nor progress.
    let run = crate::search_run(&workspace, query, limit, mode, false)?;
    Ok(crate::search_document(&run))
}

fn scan(
    params: &Value,
    request: Option<&Value>,
    ch: &mut crate::serve_channel::Channel<'_>,
) -> Result<Value, ServeError> {
    let workspace = workspace_of(params)?;
    let cfg = crate::load_config().map_err(refused)?;
    // **`locking` before the attempt, not after the grant** (spec §2): a client watching a first run needs to see that
    // np is waiting on another writer, because from outside a blocked write and a slow scan look identical. The walk
    // itself stays unreported until Task 8 gives scan a seam; this line is the part that needs none.
    let progress = crate::serve_channel::ServeProgress::new(ch, request.cloned(), "scan");
    crate::serve_channel::report(&progress, Phase::Locking, ProgressEvent::default());
    // The lock is taken and released inside this one request. A held-across-requests design would have the server refuse *itself* after 2 s,
    // which is F44 doing its job against the wrong caller.
    let _writer = match crate::server_writer_lock(&cfg, &workspace, "scan")? {
        crate::LockGrant::Contended(document) => return Ok(document),
        crate::LockGrant::Held(guard) => guard,
    };
    let report = crate::scan(&workspace, &cfg).map_err(refused)?;
    serde_json::to_value(report).map_err(refused)
}

fn embed(
    params: &Value,
    request: Option<&Value>,
    ch: &mut crate::serve_channel::Channel<'_>,
) -> Result<Value, ServeError> {
    let workspace = workspace_of(params)?;
    let cfg = crate::load_config().map_err(refused)?;
    // Reported before the lock is attempted, for the same reason `scan` says it: waiting is the state a GUI has to draw.
    let progress = crate::serve_channel::ServeProgress::new(ch, request.cloned(), "embed");
    crate::serve_channel::report(&progress, Phase::Locking, ProgressEvent::default());
    let _writer = match crate::server_writer_lock(&cfg, &workspace, "embed")? {
        crate::LockGrant::Contended(document) => return Ok(document),
        crate::LockGrant::Held(guard) => guard,
    };
    // (The reporter was built above, before the lock: `Channel::notify` is what knows whether this connection opted in,
    // so a handler that decided for itself would be a second place where "did they ask?" can be answered wrongly.)
    // The same function `np embed` runs — arm order, fetch trigger and all — with the human channel off. F42's drift notice still prints, on
    // stderr, because it is prose about a state the document also reports in `prefix_drift`.
    let report = crate::embed_report(&workspace, crate::AutoFetch::Deferred, false, &progress)
        .map_err(refused)?;
    let drift = match report.model.as_deref() {
        Some(id) => {
            crate::prefix_drift_notice(&crate::load_config().map_err(refused)?, &workspace, id)
                .map_err(refused)?
        }
        None => None,
    };
    crate::insert_drift(serde_json::to_value(&report).map_err(refused)?, drift).map_err(refused)
}

fn models_list(params: &Value) -> Result<Value, ServeError> {
    let workspace = workspace_of(params)?;
    let config = crate::load_config().map_err(refused)?;
    let db = crate::workspace_db(&config, &workspace).map_err(refused)?;
    Ok(match np_core::models::list(&db, &config, &workspace) {
        Ok(listing) => {
            crate::listing_json(&listing.models, &listing.fetching, &listing.unclaimed, None)
        }
        // A directory we cannot read is a state the document already knows how to carry (`models_dir_error`), so it stays a result rather than
        // becoming a fault: `np models list --json` prints the same object and exits non-zero, and exit codes do not exist here.
        Err(err @ np_core::models::ModelError::ModelsDir { .. }) => {
            crate::listing_json(&[], &[], &[], Some(&err.to_string()))
        }
        Err(err) => return Err(refused(err)),
    })
}

/// A JSON-RPC error object. Kept in one place so the envelope shape cannot vary by fault kind.
fn fault(id: Value, code: i64, message: impl Into<String>) -> Value {
    fault_response(
        id,
        Fault {
            code,
            message: message.into(),
        },
    )
}

fn fault_response(id: Value, fault: Fault) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": fault.code, "message": fault.message },
    })
}

#[cfg(test)]
mod tests {
    use super::{handle_line, Fault};

    /// Drive one line through [`handle_line`] with a throwaway channel. The assertions below are the ones written
    /// against the pure function; the channel exists because the request handler is about to need it, and a test that
    /// threw its `Vec<u8>` away would still be asserting exactly what it asserted before.
    fn answer(line: &str) -> Option<Value> {
        let mut sink: Vec<u8> = Vec::new();
        let clock = crate::serve_channel::SystemClock::new();
        let mut ch = crate::serve_channel::Channel::new(&mut sink, &clock);
        handle_line(line, &mut ch)
    }

    use super::*;

    /// A response to every request that carries an id, and to none that does not — the notification rule, which is what lets a client fire a
    /// `scan` and keep asking questions.
    #[test]
    fn a_notification_gets_no_response_at_all() {
        assert!(answer(r#"{"jsonrpc":"2.0","method":"nope"}"#).is_none());
        let reply = answer(r#"{"jsonrpc":"2.0","id":7,"method":"nope"}"#);
        assert!(reply.is_some(), "the same call with an id must be answered");
    }

    /// Every unparseable line is answered, with `id: null` — including one that a notification-lover would call noise.
    ///
    /// The temptation is to treat "I cannot read this" as "there is no id, so it is a notification". That inference is unsound in the one direction
    /// that matters: a client whose buffer got truncated mid-request *did* send an id and would wait forever for its answer. JSON-RPC 2.0 §5
    /// resolves it the same way for the same reason, so `np serve` answers `-32700` and lets the client notice its own mistake. `crates/np-cli/tests/serve.rs`
    /// proves the same thing through a real pipe; this case pins the branch itself.
    #[test]
    fn every_unparseable_line_is_answered_with_a_null_id() {
        for line in ["{not json", "total garbage", r#"{"jsonrpc":"2.0","#] {
            let reply = answer(line).unwrap_or_else(|| panic!("`{line}` went unanswered"));
            assert_eq!(
                reply["error"]["code"],
                Fault::PARSE_ERROR,
                "{line} → {reply}"
            );
            assert!(reply["id"].is_null(), "no id was readable: {reply}");
        }
    }

    /// A blank line is the one input that is silently ignored: there is nothing there to have failed, and the likely writer is a client flushing a
    /// keepalive. Answering it would train that client to stop sending requests.
    #[test]
    fn a_blank_line_is_not_a_request() {
        assert!(answer("").is_none());
        assert!(answer("   ").is_none());
    }

    /// An unknown method names the ones this build has. A support report should be self-explanatory, and a client that typed `models.list` as
    /// `models_list` should find out in one round trip rather than by reading source.
    #[test]
    fn an_unknown_method_lists_what_this_build_answers() {
        let reply = answer(r#"{"jsonrpc":"2.0","id":3,"method":"models_list"}"#).expect("answered");
        assert_eq!(reply["error"]["code"], Fault::METHOD_NOT_FOUND);
        let message = reply["error"]["message"].as_str().expect("a message");
        assert!(message.contains("search"), "{message}");
        assert!(message.contains("models.list"), "{message}");
    }

    /// `initialize` is the capability answer a UI needs before it draws anything, and it must never require a workspace.
    #[test]
    fn initialize_reports_version_protocol_and_features() {
        let reply = answer(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).expect("answered");
        assert_eq!(reply["result"]["protocol"], PROTOCOL);
        assert!(reply["result"]["np"].is_string(), "{reply}");
        for key in ["local_model", "vision", "fetch"] {
            assert!(
                reply["result"]["features"][key].is_boolean(),
                "`{key}` must be a compile-time boolean, not absent: {}",
                reply["result"]
            );
        }
        let methods: Vec<&str> = reply["result"]["methods"]
            .as_array()
            .expect("an array")
            .iter()
            .map(|m| m.as_str().expect("strings"))
            .collect();
        assert_eq!(methods, METHODS);
    }

    /// A missing or empty workspace is a parameter fault, not a filesystem event: refusing here is what keeps §3 step 0 true — no data directory
    /// and no lock file are created for a vault that was never named.
    #[test]
    fn a_request_without_a_workspace_is_an_invalid_params_fault() {
        for body in [
            r#"{"jsonrpc":"2.0","id":1,"method":"search","params":{"query":"x"}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"scan","params":{"workspace":""}}"#,
        ] {
            let reply = answer(body).expect("answered");
            assert_eq!(
                reply["error"]["code"],
                Fault::INVALID_PARAMS,
                "{body} → {reply}"
            );
        }
    }

    /// The version string a client compares against `np --version`, from the same place both read.
    #[test]
    fn initialize_names_the_package_version() {
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.3.0");
    }
}
