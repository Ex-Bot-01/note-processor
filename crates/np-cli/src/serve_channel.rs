//! The one place `np serve` writes to stdout, and the loop that feeds it.
//!
//! Two reasons this is a module rather than a `writeln!` inside the read loop:
//!
//! **One owner.** A client parses stdout as NDJSON, so two writers interleaving is corruption it cannot recover from.
//! Everything the protocol says goes through [`Channel`], which holds the locked handle for the loop's life. Human
//! prose — every summary, notice and llama.cpp warning — stays on stderr, where it has always been.
//!
//! **A testable loop.** With the sink behind a trait the whole protocol can be driven in-process against a `Vec<u8>`:
//! framing, blank lines, unreadable requests and a client that stopped reading are all questions a pipe answers
//! slowly and a buffer answers exactly. The real-pipe suite in `tests/serve.rs` still runs; this is not a replacement
//! for it, it is the layer beneath where that suite cannot reach.

use std::cell::RefCell;
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use np_core::progress::{Phase, Progress, ProgressEvent};
use serde_json::{json, Value};

/// Where a line goes. Implementations must take whole lines: framing is this module's job, and a sink that received
/// half an object would produce exactly the unreadable stream the type exists to prevent.
pub trait LineSink {
    /// Deliver one complete line. A failure is the sink's to report; the loop's contract is that it never panics and
    /// never writes a partial line.
    fn line(&mut self, text: &str);
}

impl LineSink for io::StdoutLock<'static> {
    fn line(&mut self, text: &str) {
        // Errors are dropped exactly as the pre-Channel loop dropped them: a closed stdout means nobody is listening,
        // and there is no channel left to report it on. The loop keeps serving until stdin ends.
        let _ = writeln!(self, "{text}");
        let _ = self.flush();
    }
}

impl LineSink for Vec<u8> {
    fn line(&mut self, text: &str) {
        self.extend_from_slice(text.as_bytes());
        self.push(b'\n');
    }
}

/// How long something has been going on. Its only job is to keep the throttle testable: a test that waited a real
/// second per assertion would be a test people stop running.
pub trait Clock: Sync {
    /// Time since this clock started, monotonic by construction (**my call in the spec**: an `Instant`, not a wall
    /// clock, so an NTP correction cannot make `since_ms` go backwards).
    fn since_start(&self) -> Duration;
}

/// The production clock.
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    /// Start measuring now.
    pub fn new() -> Self {
        SystemClock {
            start: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn since_start(&self) -> Duration {
        self.start.elapsed()
    }
}

/// A clock a test sets by hand. Milliseconds are enough for every question asked here, and an atomic keeps it usable
/// through the `&dyn Clock` the channel holds.
#[cfg(test)]
#[derive(Default)]
pub struct FakeClock {
    ms: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl FakeClock {
    fn set_ms(&self, ms: u64) {
        self.ms.store(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn since_start(&self) -> Duration {
        Duration::from_millis(self.ms.load(std::sync::atomic::Ordering::SeqCst))
    }
}

/// The shortest gap between two progress lines **within one phase** (**§4**).
const THROTTLE: Duration = Duration::from_millis(1_000);

/// Every response and notification `np serve` writes.
pub struct Channel<'a> {
    sink: &'a mut dyn LineSink,
    clock: &'a dyn Clock,
    /// Set once by `initialize`, on this connection's own request (**§1**). It is the only session-level fact the
    /// server keeps; every request still names its own workspace.
    notifications: bool,
    /// The phase of the last line written, and when — the throttle's whole memory.
    emitted_phase: Option<Phase>,
    emitted_at: Duration,
    /// When the request now being handled began, on this clock. `since_ms` is measured from here (**§2**): a client
    /// that has had the pipe open for ten minutes wants "this fetch has run four seconds".
    request_at: Duration,
    /// The one slot of session memory: the last finished activity, for `status.get` (**§5**).
    last: Option<Value>,
    /// The terminal notification for the request being handled. It is *queued* rather than written because it has to go
    /// out **after** the response line, and `handle_line` builds the response without writing it — so the loop writes
    /// the answer first and flushes this after (the ordering rule the spec calls "one line, mirroring the response kind").
    pending_terminal: Option<Value>,
}

impl<'a> Channel<'a> {
    /// A channel that writes to `sink` and measures elapsed time with `clock`, with progress **off** until asked.
    pub fn new(sink: &'a mut dyn LineSink, clock: &'a dyn Clock) -> Self {
        Channel {
            sink,
            clock,
            notifications: false,
            emitted_phase: None,
            emitted_at: Duration::ZERO,
            request_at: clock.since_start(),
            last: None,
            pending_terminal: None,
        }
    }

    /// Mark the start of a new request. Called by the read loop before anything is parsed, so a request that turns out to
    /// be unreadable still has an honest elapsed time.
    pub fn begin_request(&mut self) {
        self.request_at = self.clock.since_start();
    }

    /// Time since the request now being handled began.
    pub fn since_request(&self) -> Duration {
        self.clock.since_start().saturating_sub(self.request_at)
    }

    /// Remember the last finished activity for `status.get`.
    pub fn set_last(&mut self, document: Value) {
        self.last = Some(document);
    }

    /// What finished last on this connection, for `status.get`. `None` until the first request ends.
    pub fn last(&self) -> Option<&Value> {
        self.last.as_ref()
    }

    /// Queue the terminal notification for the request being handled.
    pub fn queue_terminal(&mut self, params: Value) {
        self.pending_terminal = Some(params);
    }

    /// Write the response line, then the queued terminal line — the order a client reads them in.
    pub fn response_then_terminal(&mut self, value: &Value) {
        self.response(value);
        self.flush_terminal();
    }

    /// Flush any queued terminal notification. Never throttled: this is the last thing a request has to say, and a client
    /// that missed it would wait for an ending that already happened.
    pub fn flush_terminal(&mut self) {
        let Some(params) = self.pending_terminal.take() else {
            return;
        };
        if !self.notifications {
            return;
        }
        self.sink.line(
            &json!({"jsonrpc": "2.0", "method": "np/progress", "params": params}).to_string(),
        );
    }

    /// Turn the progress stream on for this connection. Only `initialize` calls it.
    pub fn set_notifications(&mut self, on: bool) {
        self.notifications = on;
    }

    /// Write one `np/progress` notification, subject to the throttle (**§4**): a phase change is written immediately,
    /// and within one phase at most one line per second. A first-run download is ~40 lines over its life; without this
    /// rule the status stream would out-volume the answer it is describing.
    pub fn notify(&mut self, phase: Phase, params: Value) {
        if !self.notifications {
            return;
        }
        let since = self.clock.since_start();
        if let Some(last) = self.emitted_phase {
            if last == phase && since.saturating_sub(self.emitted_at) < THROTTLE {
                return;
            }
        }
        self.emitted_phase = Some(phase);
        self.emitted_at = since;
        // A notification has no `id` member at all — absent, not null. `null` is the id of a request nobody could read,
        // which is a different document and one a client that dispatches on "is there an id" would answer.
        self.sink.line(
            &json!({"jsonrpc": "2.0", "method": "np/progress", "params": params}).to_string(),
        );
    }

    /// Write one response line. A document that will not serialise becomes the generic internal fault, because the
    /// alternative — a half-written line — breaks framing for every request after it.
    ///
    /// **One branch here has no unit test**, and saying so is cheaper than pretending: no `serde_json::Value` refuses
    /// to serialise (nesting a thousand arrays deep writes fine), and reaching the fallback needs a type of one's own
    /// with a failing `Serialize`, which would make `serde` a direct dependency of np-cli for the sake of one case. The
    /// fallback is therefore carried from the pre-`Channel` loop verbatim — untested then, untested now. What *is*
    /// proved below is the property a client depends on: whatever comes back, the sink gets whole lines and never half
    /// an object.
    pub fn response(&mut self, value: &Value) {
        let text = serde_json::to_string(value).unwrap_or_else(|source| {
            let _ = writeln!(
                io::stderr(),
                "np: serve: response not serialisable: {source}"
            );
            r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"response could not be serialised"},"id":null}"#
                .to_string()
        });
        self.sink.line(&text);
    }
}

/// The np-core reporter for one in-flight request: it knows the request id (or that there was none), the command name,
/// and how to write a line.
///
/// The `RefCell` is not concurrency, it is a reborrow. np-core's trait takes `&self` — a fetch loop must not be able to
/// hand the reporter to another thread by accident — while writing needs `&mut Channel`. Wrapping the mutable borrow
/// lets the adapter satisfy `&self` without `unsafe`, and nothing here outlives the request.
pub struct ServeProgress<'a, 'b> {
    ch: RefCell<&'a mut Channel<'b>>,
    /// The id to echo in `request`, or `None` when the command arrived as a notification. Absent from the params is
    /// what that means on the wire — never `"request": null` (**Review Focus 5**).
    request: Option<Value>,
    command: &'static str,
}

impl<'a, 'b> ServeProgress<'a, 'b> {
    /// An adapter for one request. `command` is `&'static str` because it is a method name from [`crate::serve::METHODS`],
    /// never text that came off the wire.
    pub fn new(ch: &'a mut Channel<'b>, request: Option<Value>, command: &'static str) -> Self {
        ServeProgress {
            ch: RefCell::new(ch),
            request,
            command,
        }
    }

    /// Throttle-check and write one event.
    fn publish(&self, event: &ProgressEvent<'_>) {
        let since = self.ch.borrow().since_request();
        let params = progress_params(
            self.request.as_ref(),
            self.command,
            event.phase,
            event,
            since,
        );
        self.ch.borrow_mut().notify(event.phase, params);
    }
}

impl Progress for ServeProgress<'_, '_> {
    fn report(&self, event: &ProgressEvent<'_>) {
        self.publish(event);
    }
}

/// Report one checkpoint for `reporter`, stamping `phase` onto the event.
///
/// The phase is an argument rather than only a field so that the code writing a status line says which phase it means:
/// a handler that fills in counters and forgets to set `phase` would otherwise report `Resolving` (the default) forever,
/// which is a lie a client cannot detect.
pub fn report(reporter: &ServeProgress<'_, '_>, phase: Phase, mut event: ProgressEvent<'_>) {
    event.phase = phase;
    reporter.publish(&event);
}

/// The `params` of one notification, in exactly the spec's key names (**§2**, **§3**).
///
/// Keys appear only when they mean something. `bytes_total` is absent when this build has no pinned size to offer, and
/// a chunk count never rides on a byte checkpoint: an absent key says "this phase has no such number", where a zero says
/// "nothing has been done", and the two render very differently in a status bar.
pub fn progress_params(
    request: Option<&Value>,
    command: &str,
    phase: Phase,
    counters: &ProgressEvent<'_>,
    since: Duration,
) -> Value {
    let mut params = serde_json::Map::new();
    if let Some(id) = request {
        params.insert("request".to_string(), id.clone());
    }
    params.insert("command".to_string(), json!(command));
    params.insert("phase".to_string(), json!(phase_name(phase)));
    params.insert("since_ms".to_string(), json!(since.as_millis() as u64));
    match phase {
        Phase::Fetching => {
            if let Some(member) = counters.member {
                params.insert("member".to_string(), json!(member));
            }
            params.insert("bytes_done".to_string(), json!(counters.bytes_done));
            if let Some(total) = counters.bytes_total {
                params.insert("bytes_total".to_string(), json!(total));
            }
            params.insert("members_done".to_string(), json!(counters.members_done));
            params.insert("members_total".to_string(), json!(counters.members_total));
        }
        Phase::Embedding | Phase::Writing => {
            params.insert("chunks_done".to_string(), json!(counters.chunks_done));
            params.insert("embedded".to_string(), json!(counters.embedded));
        }
        // `locking`, `resolving` and `indexing` carry no counters of their own; the bookend lines a scan writes add
        // `files_seen` / `notes_seen` by hand, because those numbers come from the report rather than from this seam.
        Phase::Locking | Phase::Resolving | Phase::Indexing => {}
    }
    Value::Object(params)
}

/// The wire spelling of a phase: lowercase, and the same words the spec's table uses. A rename here is a protocol
/// change, which is why it is one function with one test rather than a `format!` at each call site.
pub fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Resolving => "resolving",
        Phase::Locking => "locking",
        Phase::Indexing => "indexing",
        Phase::Fetching => "fetching",
        Phase::Embedding => "embedding",
        Phase::Writing => "writing",
    }
}

/// Read requests until end of input, writing at most one response line per request.
///
/// Behaviour is the loop `serve` has always had: a blank line is ignored without an answer, an unreadable request is
/// answered with `-32700`, a notification gets nothing, and end of input is a clean exit rather than an error — a
/// client closing the pipe is how it says goodbye.
pub(crate) fn serve_loop(input: impl BufRead, ch: &mut Channel<'_>) -> io::Result<()> {
    for line in input.lines() {
        // One clock reading per request, taken before anything is parsed.
        ch.begin_request();
        let line = match line {
            Ok(line) => line,
            Err(source) => {
                // "stdin unreadable" has no useful response to send (we cannot even know a request id) and the loop cannot continue.
                let _ = writeln!(io::stderr(), "np: serve: stdin: {source}");
                break;
            }
        };
        match crate::serve::handle_line(&line, ch) {
            Some(response) => ch.response_then_terminal(&response),
            // A notification gets no response — including for a malformed line, per JSON-RPC 2.0 §5: nothing is known about the sender's intent,
            // so there is nothing to answer and an error would be a guess.
            None => {
                if !line.trim().starts_with('{') {
                    let _ = writeln!(
                        io::stderr(),
                        "np: serve: ignored a line that is not a JSON object"
                    );
                }
                // A notification-shaped command still ends, and its ending still gets a line — with no `request` key,
                // because none was given (**Review Focus 5**).
                ch.flush_terminal();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{report, serve_loop, Channel, FakeClock, LineSink, ServeProgress, SystemClock};
    use np_core::progress::{Phase, ProgressEvent};
    use std::io::Cursor;

    /// Two values in, exactly two newline-terminated parseable objects out — framing, which is the whole contract a
    /// client has to trust. A writer that buffered without flushing, or wrote an object without its newline, shows up
    /// here as one line or an unparseable one.
    #[test]
    fn a_buffer_sink_receives_whole_lines() {
        let clock = SystemClock::new();
        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut ch = Channel::new(&mut buffer, &clock);
            ch.response(&serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}}));
            ch.response(&serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"ok":false}}));
        }
        let text = String::from_utf8(buffer).expect("every line is UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per response: {text:?}");
        assert!(text.ends_with('\n'), "the last line is terminated too");
        for (index, line) in lines.iter().enumerate() {
            let parsed: serde_json::Value =
                serde_json::from_str(line).expect("each line parses on its own");
            assert_eq!(parsed["id"], index + 1, "line {index}: {line}");
        }
    }

    /// The loop, in-process: one request in, exactly one response out, and the response is the real `initialize`
    /// document rather than a stub.
    #[test]
    fn feed_initialize_through_the_loop_writes_exactly_one_line() {
        let mut buffer: Vec<u8> = Vec::new();
        let input = Cursor::new(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        );
        {
            let clock = SystemClock::new();
            let mut ch = Channel::new(&mut buffer, &clock);
            serve_loop(input, &mut ch).expect("the loop ends cleanly at end of input");
        }
        let text = String::from_utf8(buffer).expect("output is UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1, "one request earned one line: {text:?}");
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).expect("parses");
        assert_eq!(parsed["id"], 1, "the line was {}", lines[0]);
        assert_eq!(parsed["result"]["protocol"], 1, "the line was {}", lines[0]);
    }

    /// A blank line and a notification are answered with silence, and unreadable text is answered — through the loop,
    /// not only through `handle_line`, because the loop is what a client meets.
    #[test]
    fn the_loop_answers_only_requests() {
        let clock = SystemClock::new();
        let mut buffer: Vec<u8> = Vec::new();
        let input = Cursor::new(b"\n{\"jsonrpc\":\"2.0\",\"method\":\"scan\"}\n{not json\n");
        {
            let mut ch = Channel::new(&mut buffer, &clock);
            serve_loop(input, &mut ch).expect("end of input is a clean exit");
        }
        let text = String::from_utf8_lossy(&buffer).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "the blank line and the notification are silent; the unreadable line is answered: {text:?}"
        );
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).expect("parses");
        assert_eq!(parsed["error"]["code"], -32700, "{parsed}");
        assert!(parsed["id"].is_null(), "no id was readable: {parsed}");
    }

    /// A sink that refuses every write (**Review Focus 4**): no panic, no partial line, and the loop still treats end
    /// of input as a normal ending. This is the client that vanished mid-run — `np` must not die noisy, and must not
    /// leave half an object in a pipe somebody else will read.
    #[test]
    fn a_sink_that_refuses_writes_nothing_and_panics_nothing() {
        struct Refusing {
            attempts: usize,
        }

        impl LineSink for Refusing {
            fn line(&mut self, _text: &str) {
                self.attempts += 1;
                // Deliberately writes nothing: a closed pipe accepts no bytes and reports an error.
            }
        }

        let clock = SystemClock::new();
        let mut sink = Refusing { attempts: 0 };
        let input = Cursor::new(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        );
        {
            let mut ch = Channel::new(&mut sink, &clock);
            serve_loop(input, &mut ch).expect("a vanished client is not a fault in np");
        }
        assert_eq!(sink.attempts, 1, "the request was still answered once");
    }

    /// **Review Focus 1.** A client that never asked gets silence: the flag is what turns lines on, not the existence
    /// of a reporter. An opted-out session must produce *zero* bytes on stdout beyond its responses, or every v0.3.0
    /// client would start seeing notifications it cannot parse.
    #[test]
    fn an_opted_out_channel_writes_no_notification() {
        let clock = FakeClock::default();
        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut ch = Channel::new(&mut buffer, &clock);
            ch.notify(Phase::Fetching, serde_json::json!({"command": "embed"}));
        }
        assert!(buffer.is_empty(), "no opt-in, no line: {buffer:?}");
    }

    /// The envelope: a JSON-RPC notification has **no `id` member at all**. `null` would be a different document — a
    /// request whose id is unknown — and a client that answers requests by looking for `id` would answer this one.
    #[test]
    fn a_notification_carries_no_id_and_names_its_method() {
        let clock = FakeClock::default();
        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut ch = Channel::new(&mut buffer, &clock);
            ch.set_notifications(true);
            ch.notify(
                Phase::Fetching,
                serde_json::json!({"request": 7, "command": "embed", "phase": "fetching", "since_ms": 0}),
            );
        }
        let line = String::from_utf8(buffer).expect("UTF-8");
        let parsed: serde_json::Value = serde_json::from_str(line.trim_end()).expect("one object");
        assert!(
            !parsed.as_object().expect("object").contains_key("id"),
            "a notification has no id member, not an id of null: {parsed}"
        );
        assert_eq!(parsed["method"], "np/progress");
        assert_eq!(parsed["params"]["request"], 7);
        assert_eq!(parsed["params"]["since_ms"], 0);
    }

    /// The throttle (**§4**): a phase change is worth a line immediately, and within one phase at most one line per
    /// second. Five byte checkpoints inside a second are one line; the move to `embedding` is another, 50 ms later.
    #[test]
    fn throttle_allows_one_line_per_phase_change_and_one_per_second() {
        // A counting sink rather than a buffer, so assertions can run *between* writes: the channel holds the buffer
        // mutably for the whole block, and copying it out mid-test would hide how many lines exist at the moment being
        // judged.
        // The count travels through a shared cell: the channel borrows the sink for its whole life, so an assertion
        // that read `sink.lines` between writes would not compile.
        #[derive(Clone)]
        struct Counted {
            lines: std::rc::Rc<std::cell::Cell<usize>>,
        }

        impl LineSink for Counted {
            fn line(&mut self, _text: &str) {
                self.lines.set(self.lines.get() + 1);
            }
        }

        let clock = FakeClock::default();
        let lines = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let mut sink = Counted {
            lines: std::rc::Rc::clone(&lines),
        };
        {
            let mut ch = Channel::new(&mut sink, &clock);
            ch.set_notifications(true);
            let params = |phase: &str| serde_json::json!({"command": "embed", "phase": phase});
            // One phase, five checkpoints, one second of fake time: four of them are dropped.
            for step in 0..5 {
                clock.set_ms(step * 180);
                ch.notify(Phase::Fetching, params("fetching"));
            }
            assert_eq!(
                lines.get(),
                1,
                "five byte checkpoints inside one second are one line"
            );
            // A phase change is not throttled: the client learns about it the moment it happens.
            clock.set_ms(220);
            ch.notify(Phase::Embedding, params("embedding"));
            assert_eq!(lines.get(), 2, "a phase change earns a line at once");
            // Still inside the same phase, and still under a second since that line: silent.
            clock.set_ms(900);
            ch.notify(Phase::Embedding, params("embedding"));
            assert_eq!(lines.get(), 2, "under a second inside a phase stays quiet");
            // Past the second, the same phase speaks again.
            clock.set_ms(1_500);
            ch.notify(Phase::Embedding, params("embedding"));
            assert_eq!(lines.get(), 3, "a long phase still narrates itself");
        }
    }

    /// **Review Focus 5.** A command sent as a notification has no id to echo. Its progress lines omit `request`
    /// entirely — inventing a sequence number would make two clients' streams indistinguishable from one client's, and
    /// `null` would read as "an id that was unknown" rather than "no id was given".
    #[test]
    fn an_id_less_request_omits_the_request_key_rather_than_inventing_one() {
        let clock = FakeClock::default();
        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut ch = Channel::new(&mut buffer, &clock);
            ch.set_notifications(true);
            let reporter = ServeProgress::new(&mut ch, None, "scan");
            report(&reporter, Phase::Indexing, ProgressEvent::default());
        }
        let line = String::from_utf8(buffer).expect("UTF-8");
        let parsed: serde_json::Value = serde_json::from_str(line.trim_end()).expect("parses");
        let params = parsed["params"].as_object().expect("params object");
        assert!(
            !params.contains_key("request"),
            "no id was given, so no request key: {parsed}"
        );
        assert_eq!(params["command"], "scan");
        assert_eq!(params["phase"], "indexing");
        assert!(
            params.contains_key("since_ms"),
            "the always-present keys are all there: {parsed}"
        );
    }

    /// `since_ms` measures the **request**, not the connection (**§2**): a client that has been open for ten minutes and
    /// starts an embed wants "this fetch has run four seconds", not "this process started 600 seconds ago". A shared
    /// clock reading is the bug this pins.
    #[test]
    fn since_ms_measures_the_request_not_the_connection() {
        let clock = FakeClock::default();
        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut ch = Channel::new(&mut buffer, &clock);
            ch.set_notifications(true);
            // The connection is ten minutes old when this request begins.
            clock.set_ms(600_000);
            ch.begin_request();
            let reporter = ServeProgress::new(&mut ch, Some(serde_json::json!(3)), "embed");
            report(&reporter, Phase::Fetching, ProgressEvent::default());
            clock.set_ms(604_120);
            report(
                &reporter,
                Phase::Embedding,
                ProgressEvent {
                    phase: Phase::Embedding,
                    ..Default::default()
                },
            );
        }
        let lines: Vec<serde_json::Value> = String::from_utf8(buffer)
            .expect("UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("parses"))
            .collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[0]["params"]["since_ms"], 0,
            "the request starts at zero"
        );
        assert_eq!(
            lines[1]["params"]["since_ms"], 4_120,
            "four seconds into the request, not ten minutes into the connection"
        );
    }

    /// The counter keys a phase is allowed to carry, and only those: `fetching` brings member and byte counts,
    /// `embedding` brings chunk counts, and neither leaks the other's keys as zeros. An absent key means "this phase has
    /// no such number"; a zero would mean "nothing done", and a GUI that renders 0 % for a phase with no bytes to count
    /// is a bug a user sees.
    #[test]
    fn each_phase_carries_its_own_counters_and_no_others() {
        let clock = FakeClock::default();
        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut ch = Channel::new(&mut buffer, &clock);
            ch.set_notifications(true);
            let reporter = ServeProgress::new(&mut ch, Some(serde_json::json!(7)), "embed");
            report(
                &reporter,
                Phase::Fetching,
                ProgressEvent {
                    phase: Phase::Fetching,
                    member: Some("model"),
                    bytes_done: 67_108_864,
                    bytes_total: Some(309_855_456),
                    members_done: 0,
                    members_total: 2,
                    ..Default::default()
                },
            );
            clock.set_ms(2_000);
            report(
                &reporter,
                Phase::Embedding,
                ProgressEvent {
                    phase: Phase::Embedding,
                    chunks_done: 118,
                    embedded: 3,
                    ..Default::default()
                },
            );
        }
        let lines: Vec<serde_json::Value> = String::from_utf8(buffer)
            .expect("UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("one object per line"))
            .collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        let fetch = &lines[0]["params"];
        assert_eq!(fetch["request"], 7);
        assert_eq!(fetch["phase"], "fetching");
        assert_eq!(fetch["member"], "model");
        assert_eq!(fetch["bytes_done"], 67_108_864);
        assert_eq!(fetch["bytes_total"], 309_855_456);
        assert_eq!(fetch["members_done"], 0);
        assert_eq!(fetch["members_total"], 2);
        let fetch_keys = fetch.as_object().expect("object");
        assert!(
            !fetch_keys.contains_key("chunks_done") && !fetch_keys.contains_key("embedded"),
            "a byte checkpoint says nothing about chunks: {}",
            lines[0]
        );
        let embed = &lines[1]["params"];
        assert_eq!(embed["phase"], "embedding");
        assert_eq!(embed["chunks_done"], 118);
        assert_eq!(embed["embedded"], 3);
        let embed_keys = embed.as_object().expect("object");
        for foreign in ["bytes_done", "bytes_total", "member"] {
            assert!(
                !embed_keys.contains_key(foreign),
                "`embedding` carries no byte keys ({foreign}): {}",
                lines[1]
            );
        }
        assert!(
            !lines.iter().any(|line| line.to_string().contains("http")),
            "**§12 rule 8**: a progress line names no origin: {lines:?}"
        );
    }
}
