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

use serde_json::Value;
use std::io::{self, BufRead, Write};

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

/// Every response and notification `np serve` writes.
pub struct Channel<'a> {
    sink: &'a mut dyn LineSink,
}

impl<'a> Channel<'a> {
    /// A channel that writes to `sink`.
    pub fn new(sink: &'a mut dyn LineSink) -> Self {
        Channel { sink }
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

/// Read requests until end of input, writing at most one response line per request.
///
/// Behaviour is the loop `serve` has always had: a blank line is ignored without an answer, an unreadable request is
/// answered with `-32700`, a notification gets nothing, and end of input is a clean exit rather than an error — a
/// client closing the pipe is how it says goodbye.
pub(crate) fn serve_loop(input: impl BufRead, ch: &mut Channel<'_>) -> io::Result<()> {
    for line in input.lines() {
        let line = match line {
            Ok(line) => line,
            Err(source) => {
                // "stdin unreadable" has no useful response to send (we cannot even know a request id) and the loop cannot continue.
                let _ = writeln!(io::stderr(), "np: serve: stdin: {source}");
                break;
            }
        };
        match crate::serve::handle_line(&line, ch) {
            Some(response) => ch.response(&response),
            // A notification gets no response — including for a malformed line, per JSON-RPC 2.0 §5: nothing is known about the sender's intent,
            // so there is nothing to answer and an error would be a guess.
            None => {
                if !line.trim().starts_with('{') {
                    let _ = writeln!(
                        io::stderr(),
                        "np: serve: ignored a line that is not a JSON object"
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{serve_loop, Channel, LineSink};
    use std::io::Cursor;

    /// Two values in, exactly two newline-terminated parseable objects out — framing, which is the whole contract a
    /// client has to trust. A writer that buffered without flushing, or wrote an object without its newline, shows up
    /// here as one line or an unparseable one.
    #[test]
    fn a_buffer_sink_receives_whole_lines() {
        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut ch = Channel::new(&mut buffer);
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
            let mut ch = Channel::new(&mut buffer);
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
        let mut buffer: Vec<u8> = Vec::new();
        let input = Cursor::new(b"\n{\"jsonrpc\":\"2.0\",\"method\":\"scan\"}\n{not json\n");
        {
            let mut ch = Channel::new(&mut buffer);
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

        let mut sink = Refusing { attempts: 0 };
        let input = Cursor::new(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        );
        {
            let mut ch = Channel::new(&mut sink);
            serve_loop(input, &mut ch).expect("a vanished client is not a fault in np");
        }
        assert_eq!(sink.attempts, 1, "the request was still answered once");
    }
}
