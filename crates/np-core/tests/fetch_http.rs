//! The **real** transport, exercised end to end against a loopback server this file owns.
//!
//! Everything in `tests/fetch.rs` runs against a fake, which is what makes the rules cheap to test and also what
//! leaves one question open: does `HttpTransport` behave like the fake? This file closes that gap without egress —
//! bind `127.0.0.1:0`, answer with bytes written here, and hand the origin to the production client. What it still
//! does not prove is anything about a real CDN, TLS certificates, or 865 MB of transfer: those need the network this
//! suite deliberately does not use, and saying so is better than implying coverage that is not there.

// The transport itself only exists with the `fetch` feature (default-on); a `--no-default-features` build must still
// compile its tests, so this whole file goes away with the feature rather than failing to resolve `HttpTransport`.
#![cfg(feature = "fetch")]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::thread;

use np_core::db::Db;
use np_core::fetch::{self, Declaration, Expectation, FetchError, HttpTransport, MemberPin};
use np_core::hash::sha256_bytes;
use tempfile::TempDir;

const BODY: &[u8] = b"a pinned artifact over a socket, not 310 MB of them";

struct Spec {
    filename: &'static str,
    size_bytes: u64,
    sha256: String,
}

impl MemberPin for Spec {
    fn id(&self) -> &str {
        "test-model"
    }
    fn member(&self) -> &str {
        "main"
    }
    fn filename(&self) -> &str {
        self.filename
    }
    fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
    fn sha256(&self) -> &str {
        &self.sha256
    }
}

fn spec() -> Spec {
    Spec {
        filename: "test-model.gguf",
        size_bytes: BODY.len() as u64,
        sha256: sha256_bytes(BODY),
    }
}

fn declaration() -> Declaration {
    Declaration {
        dim: 4,
        prefix_doc: String::new(),
        prefix_query: String::new(),
    }
}

/// One accepted connection's scripted answer.
enum Reply {
    /// `200 OK` with `Content-Length`, then the bytes.
    Found(Vec<u8>),
    /// `200 OK` framed as chunked, with **no** `Content-Length` (§12 rule 12's unknown-size transfer).
    Chunked(Vec<u8>),
    /// A status and nothing else.
    Status(u16),
    /// Accept, then hang up without writing a response line.
    HangUp,
}

/// A one-shot HTTP/1.0 server on the loopback interface, returning the origin to dial.
///
/// One connection per origin is enough for these cases and keeps the fixture honest about it: `serve` hands back the
/// address and joins the thread at drop, so a test cannot pass because its server outlived the assertion.
fn serve(reply: Reply) -> (String, TempDir, thread::JoinHandle<()>) {
    let tmp = TempDir::new().expect("test setup: tempdir");
    let listener = TcpListener::bind("127.0.0.1:0").expect("test setup: bind a loopback port");
    let origin = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener
            .accept()
            .expect("test server: a connection arrives");
        // Read the request far enough to be a server rather than a pipe: a client that waits for the peer to stop
        // writing before reading would deadlock against a fixture that never reads.
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request);
        match reply {
            Reply::Found(bytes) => {
                write_all(
                    &mut stream,
                    format!(
                        "HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n",
                        bytes.len().as_u64()
                    )
                    .as_bytes(),
                );
                write_all(&mut stream, &bytes);
            }
            Reply::Chunked(bytes) => {
                // HTTP/1.1 on purpose: `Transfer-Encoding` is a 1.1 framing, and a 1.0 response carrying it is not a
                // transfer any client is obliged to decode — the point of the case is an origin that announced **no**
                // size, not a fixture that invented an invalid one.
                write_all(
                    &mut stream,
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
                );
                for chunk in bytes.chunks(7) {
                    write_all(&mut stream, format!("{:x}\r\n", chunk.len()).as_bytes());
                    write_all(&mut stream, chunk);
                    write_all(&mut stream, b"\r\n");
                }
                write_all(&mut stream, b"0\r\n\r\n");
            }
            Reply::Status(code) => {
                write_all(
                    &mut stream,
                    format!("HTTP/1.0 {code} No\r\nContent-Length: 0\r\n\r\n").as_bytes(),
                );
            }
            Reply::HangUp => {}
        }
        let _ = stream.flush();
    });
    (origin, tmp, worker)
}

/// `usize` → `u64` where a format string needs it, without an `as` cast sprinkled through the fixture.
trait Len {
    fn as_u64(&self) -> u64;
}
impl Len for usize {
    fn as_u64(&self) -> u64 {
        u64::try_from(*self).expect("a test body is not larger than u64")
    }
}

fn write_all(stream: &mut TcpStream, bytes: &[u8]) {
    stream
        .write_all(bytes)
        .expect("test server: the client is still reading");
}

fn fetch_from(origin: &str, models: &Path, db: &Db) -> Result<(), FetchError> {
    let table = vec![spec()];
    let plan = Expectation {
        table: &table,
        declaration: &declaration(),
    };
    let transport = HttpTransport::new(origin);
    fetch::fetch_with_table(
        db,
        models,
        "test-model",
        &["main"],
        &transport,
        1_730_000_100,
        &plan,
    )
    .map(|_| ())
}

#[test]
fn the_real_client_streams_verifies_and_publishes() {
    let (origin, _tmp, worker) = serve(Reply::Found(BODY.to_vec()));
    let tmp = TempDir::new().expect("test setup: tempdir");
    let models = tmp.path().join("models");
    let db = Db::in_memory().expect("test setup: database");

    fetch_from(&origin, &models, &db).expect("a good transfer completes");
    worker.join().expect("test server: the exchange finished");

    assert_eq!(
        fs::read(models.join("test-model.gguf")).expect("published"),
        BODY
    );
    let row = np_core::db::model_row(&db, "test-model")
        .expect("query")
        .expect("the fetch created the row");
    assert_eq!(row.sha256.as_deref(), Some(sha256_bytes(BODY).as_str()));
    assert!(
        !models::marker_exists(&models),
        "the marker goes with completion"
    );
}

#[test]
fn a_url_names_the_file_and_never_the_model_id() {
    // The URL shape is a rule, not an accident (§12 rule 7): the pin already names each member's file, so assembling
    // a path from an id would be one more route from argv into a request line.
    let transport =
        HttpTransport::new("https://mirror.example/ggml-org/embeddinggemma-2-GGUF/resolve/main");
    let pinned = np_core::pins::pinned_member("embeddinggemma-2", "main").expect("the shipped pin");
    assert_eq!(
        transport.url_for(pinned),
        "https://mirror.example/ggml-org/embeddinggemma-2-GGUF/resolve/main/embeddinggemma-2-Q8_0.gguf"
    );
    let mmproj =
        np_core::pins::pinned_member("embeddinggemma-2", "mmproj").expect("the second pin");
    assert_eq!(
        transport.url_for(mmproj),
        "https://mirror.example/ggml-org/embeddinggemma-2-GGUF/resolve/main/mmproj-embeddinggemma-2-Q8_0.gguf"
    );
}

#[test]
fn a_missing_file_refuses_by_class_and_names_no_origin() {
    let (origin, _tmp, worker) = serve(Reply::Status(404));
    let tmp = TempDir::new().expect("test setup: tempdir");
    let models = tmp.path().join("models");
    let db = Db::in_memory().expect("test setup: database");

    let err = fetch_from(&origin, &models, &db).expect_err("404 is a refusal");
    worker.join().expect("test server: the exchange finished");

    match &err {
        FetchError::Transport { class, .. } => assert_eq!(class, "not found"),
        other => panic!("expected a transport refusal, got {other:?}"),
    }
    let text = err.to_string();
    for forbidden in ["127.0.0.1", "http", "://", origin.as_str()] {
        assert!(
            !text.contains(forbidden),
            "§12 rule 8 broken, {forbidden:?} in: {text}"
        );
    }
    assert_nothing_written(&models);
}

#[test]
fn a_server_that_hangs_up_is_a_class_rather_than_a_hang() {
    let (origin, _tmp, worker) = serve(Reply::HangUp);
    let tmp = TempDir::new().expect("test setup: tempdir");
    let models = tmp.path().join("models");
    let db = Db::in_memory().expect("test setup: database");

    let err = fetch_from(&origin, &models, &db).expect_err("an empty response is not an artifact");
    worker.join().expect("test server: the exchange finished");

    assert!(matches!(err, FetchError::Transport { .. }), "{err}");
    assert!(!err.to_string().contains("127.0.0.1"), "{err}");
    assert_nothing_written(&models);
}

#[test]
fn a_chunked_transfer_fetches_with_the_size_left_unknown() {
    // §12 rule 12 end to end: no `Content-Length` means the marker and any report say "size unknown", while the pin
    // stays the only thing that decides acceptance.
    let (origin, _tmp, worker) = serve(Reply::Chunked(BODY.to_vec()));
    let tmp = TempDir::new().expect("test setup: tempdir");
    let models = tmp.path().join("models");
    let db = Db::in_memory().expect("test setup: database");

    fetch_from(&origin, &models, &db).expect("a chunked transfer still verifies");
    worker.join().expect("test server: the exchange finished");
    assert_eq!(
        fs::read(models.join("test-model.gguf")).expect("published"),
        BODY
    );
}

/// A refusal may have had to create the directory (the preflight asks about the filesystem it will write to), but it
/// must leave **no names** behind: no `.part`, no marker, no artifact. That is the guarantee §12 cares about, and an
/// empty directory is not workspace state a listing reports.
fn assert_nothing_written(models: &Path) {
    if let Ok(entries) = fs::read_dir(models) {
        let names: Vec<String> = entries
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert!(names.is_empty(), "a refusal left {names:?} behind");
    }
    // A missing directory is the cleanest outcome of all: nothing was ever created.
}

/// Does this workspace's models directory hold a fetch marker? (The listing's own reader needs a `Config`, which
/// these transport cases have no reason to build.)
mod models {
    use std::path::Path;

    pub fn marker_exists(models_dir: &Path) -> bool {
        fs_read_dir_dot_fetching(models_dir)
    }

    fn fs_read_dir_dot_fetching(models_dir: &Path) -> bool {
        if let Ok(entries) = std::fs::read_dir(models_dir) {
            return entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".fetching.")
            });
        }
        false
    }
}
