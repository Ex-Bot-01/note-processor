//! The fetch engine: §12 rules 4–8 and 12, tested with **no network at all**.
//!
//! Every case here runs against a [`Fake`] transport serving a few dozen bytes, and against a pin table the test
//! builds — because the production table pins a 309,855,456-byte artifact and this host has 2.8 GB free. The seam is
//! [`fetch::fetch_with_table`]; production reaches it only through [`fetch::fetch`], which passes the compiled-in
//! [`pins::PINNED`]. What that costs in realism is stated per case rather than glossed: nothing here proves the real
//! artifact's digest matches (that number is checked by `tests/pins.rs` against the file when a runner has it), and
//! nothing here proves a real HTTP stack behaves like [`Fake`] — only that *whatever* bytes arrive, these rules hold.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use np_core::db::{self, Db};
use np_core::fetch::MemberPin;
use np_core::fetch::{self, Body, Declaration, Expectation, FetchError, Transport};
use np_core::hash::sha256_bytes;
use np_core::models::{self, FetchState};
use tempfile::TempDir;

/// What a member's body should do when opened.
enum Serve {
    /// Deliver exactly these bytes, announcing their length.
    Full(Vec<u8>),
    /// Deliver a prefix of these bytes and then EOF, while announcing the full length.
    Truncated(Vec<u8>),
    /// Deliver these bytes with **no** announced length (§12 rule 12's unknown-size transfer).
    UnknownLen(Vec<u8>),
    /// Fail to open, with raw client-shaped text that names a host.
    Fail(String),
    /// Serve `bytes`, and on the first read check what the marker says at that moment (§12 rules 3–4 mid-flight).
    Inspect {
        bytes: Vec<u8>,
        models_dir: PathBuf,
        id: &'static str,
    },
}

/// A transport that records every request and can be told what each member does.
struct Fake {
    base: String,
    responses: RefCell<BTreeMap<String, Serve>>,
    opened: RefCell<Vec<String>>,
}

impl Fake {
    fn new() -> Self {
        Fake {
            // A real loopback origin with a port: the no-URL-leak cases assert on this exact string.
            base: "http://127.0.0.1:40313/models".to_string(),
            responses: RefCell::new(BTreeMap::new()),
            opened: RefCell::new(Vec::new()),
        }
    }

    fn serve(&self, member: &str, serve: Serve) {
        self.responses
            .borrow_mut()
            .insert(member.to_string(), serve);
    }

    fn opens(&self) -> usize {
        self.opened.borrow().len()
    }
}

impl Transport for Fake {
    fn open(&self, member: &dyn MemberPin) -> Result<Body, FetchError> {
        // The URL this transport *would* dial: built here so a leak into a message is this module's fault, not the
        // test's invention.
        let url = format!("{}/{}", self.base, member.filename());
        self.opened.borrow_mut().push(member.member().to_string());
        match self.responses.borrow_mut().remove(member.member()) {
            Some(Serve::Full(bytes)) => Ok(Body {
                reader: Box::new(Cursor::new(bytes)),
                bytes_total: Some(member.size_bytes()),
            }),
            Some(Serve::UnknownLen(bytes)) => Ok(Body {
                reader: Box::new(Cursor::new(bytes)),
                // §12 rule 12: a chunked transfer announces nothing.
                bytes_total: None,
            }),
            Some(Serve::Truncated(bytes)) => {
                let keep = bytes.len() / 2;
                Ok(Body {
                    reader: Box::new(Cursor::new(bytes[..keep].to_vec())),
                    bytes_total: Some(member.size_bytes()),
                })
            }
            Some(Serve::Inspect {
                bytes,
                models_dir,
                id,
            }) => {
                // **Mid-flight observation, from inside the transfer.** The marker has to exist *while* bytes move:
                // that is the whole answer to "is it downloading?" (§12 rule 3), and no post-hoc check can show it.
                match models::fetch_state(&models_dir, id) {
                    FetchState::Present(fields) => {
                        assert!(
                            !fields.nonce.is_empty(),
                            "the marker's nonce must be present and non-empty: {fields:?}"
                        );
                        assert_eq!(
                            fields.bytes_total,
                            Some(member.size_bytes()),
                            "a marker written before byte one still knows the expected size: {fields:?}"
                        );
                    }
                    other => panic!("the marker must exist during the transfer, got {other:?}"),
                }
                Ok(Body {
                    reader: Box::new(Cursor::new(bytes)),
                    bytes_total: Some(member.size_bytes()),
                })
            }
            // The transport's raw text names the host and URL; only a class may escape (§12 rule 8).
            Some(Serve::Fail(raw)) => Err(FetchError::Transport {
                id: "test-model".into(),
                member: member.member().into(),
                class: fetch::failure_class(&format!("{url}: {raw}")),
            }),
            // A member nobody told us about is a test bug, reported as one rather than served as empty bytes: an
            // empty body would hash to a real digest and turn a mistake into a passing case.
            None => panic!("no response configured for member `{}`", member.member()),
        }
    }
}

/// One member's expectation, held by the test rather than compiled in.
///
/// Production passes [`np_core::pins::PinnedModel`] (whose fields are `&'static`, because the table is a `const`);
/// a fixture's digest is computed at run time from its own bytes, so it needs an owned string. [`MemberPin`] is the
/// seam that lets both feed the **same** engine code — see its doc for why that is not an extension point.
struct Spec {
    member: &'static str,
    filename: &'static str,
    size_bytes: u64,
    sha256: String,
}

impl MemberPin for Spec {
    fn id(&self) -> &str {
        "test-model"
    }
    fn member(&self) -> &str {
        self.member
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

/// The two-member table every case uses, with digests computed over the fixtures themselves.
///
/// `size_bytes` travels with each fixture, so a case that truncates or extends does it **on purpose** and says which
/// rule it is exercising — an accidental length mismatch would otherwise read as `Interrupted` in a test about
/// something else.
fn table(main: &[u8], extra: &[u8]) -> Vec<Spec> {
    vec![
        Spec {
            member: "main",
            filename: "test-model.gguf",
            size_bytes: main.len() as u64,
            sha256: sha256_bytes(main),
        },
        Spec {
            member: "extra",
            filename: "test-model-mmproj.gguf",
            size_bytes: extra.len() as u64,
            sha256: sha256_bytes(extra),
        },
    ]
}

fn dir() -> TempDir {
    TempDir::new().expect("test setup: tempdir")
}

/// A workspace database, in memory — the row a successful fetch writes is part of what these cases verify, so the
/// engine cannot be tested without one.
fn db() -> Db {
    Db::in_memory().expect("test setup: in-memory database")
}

/// The identity every fixture records: a width nothing else in this file uses, and prefixes that are **not** both
/// empty, so an assertion that the row carries them cannot pass by defaulting.
fn declaration() -> Declaration {
    Declaration {
        dim: 4,
        prefix_doc: "doc: ".into(),
        prefix_query: "q: ".into(),
    }
}

const MAIN: &[u8] = b"the primary artifact, in words, not 310 MB of them";
const EXTRA: &[u8] = b"and its second member, also small on purpose";

#[test]
fn a_verified_pair_is_published_and_no_fetch_name_survives() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    fake.serve("main", Serve::Full(MAIN.to_vec()));
    fake.serve("extra", Serve::Full(EXTRA.to_vec()));

    let report = fetch::fetch_with_table(
        &db,
        &models,
        "test-model",
        &["main", "extra"],
        &fake,
        1_730_000_000,
        &Expectation {
            table: &table,
            declaration: &declaration(),
        },
    )
    .expect("both members verify");

    assert_eq!(report.members.len(), 2);
    assert_eq!(report.total_bytes, (MAIN.len() + EXTRA.len()) as u64);
    assert!(report.wrote_anything());
    assert_eq!(
        fs::read(models.join("test-model.gguf")).expect("main published"),
        MAIN
    );
    assert_eq!(
        fs::read(models.join("test-model-mmproj.gguf")).expect("second member published"),
        EXTRA
    );

    // Nothing recognisable, and nothing unrecognisable either: the marker went with the completion, and no `.part`
    // was left for `list` to report as a fetch that is not running.
    let left: Vec<String> = fs::read_dir(&models)
        .expect("models dir exists")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name != "test-model.gguf" && name != "test-model-mmproj.gguf")
        .collect();
    assert_eq!(
        left,
        Vec::<String>::new(),
        "stray names after success: {left:?}"
    );
    assert_eq!(
        models::fetch_state(&models, "test-model"),
        FetchState::Absent
    );
}

#[test]
fn the_marker_exists_while_bytes_are_arriving() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    fs::create_dir_all(&models).expect("test setup: models dir");
    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    // The body itself observes the marker on first read (see [`Serve::Inspect`]).
    fake.serve(
        "main",
        Serve::Inspect {
            bytes: MAIN.to_vec(),
            models_dir: models.clone(),
            id: "test-model",
        },
    );

    fetch::fetch_with_table(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        1_730_000_001,
        &Expectation {
            table: &table,
            declaration: &declaration(),
        },
    )
    .expect("the observed fetch completes");
    assert_eq!(
        models::fetch_state(&models, "test-model"),
        FetchState::Absent
    );
}

#[test]
fn a_second_fetch_refuses_while_a_marker_exists_and_names_the_way_out() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    fs::create_dir_all(&models).expect("test setup: models dir");
    // A marker as some *other* process wrote it — the case §12 rule 4 is about, since liveness is consent.
    write_marker_raw(
        &models,
        "test-model",
        r#"{"nonce":"7777-3-9","started_at":1730000000,"bytes_done":12345,"bytes_total":99999}"#,
    );

    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    fake.serve("main", Serve::Full(MAIN.to_vec()));
    let err = fetch::fetch_with_table(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        1_730_000_002,
        &Expectation {
            table: &table,
            declaration: &declaration(),
        },
    )
    .expect_err("an existing marker is consent already given");

    assert!(matches!(err, FetchError::MarkerExists { .. }), "{err}");
    let text = err.to_string();
    for wanted in [
        ".fetching.test-model.json",
        "1730000000",
        "12345",
        "99999",
        "7777-3-9",
        "--discard-marker",
    ] {
        assert!(
            text.contains(wanted),
            "refusal must carry {wanted:?}: {text}"
        );
    }
    // Rule 4's other half: nothing was requested, so a refusal cannot have spent bandwidth.
    assert_eq!(fake.opens(), 0, "a refusal must not dial out first: {text}");
    assert!(
        !models.join("test-model.gguf").exists(),
        "no artifact on refusal"
    );
}

#[test]
fn an_unreadable_marker_is_reported_and_left_in_place() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    fs::create_dir_all(&models).expect("test setup: models dir");
    write_marker_raw(&models, "test-model", "{ truncated");

    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    fake.serve("main", Serve::Full(MAIN.to_vec()));
    let err = fetch::fetch_with_table(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        1_730_000_003,
        &Expectation {
            table: &table,
            declaration: &declaration(),
        },
    )
    .expect_err("a marker this build cannot read is not an absent marker");

    assert!(matches!(err, FetchError::MarkerUnreadable { .. }), "{err}");
    // **Reported, never deleted** (the F22/F23 posture R7a chose for the reader): the bytes are someone else's
    // evidence that a download was running.
    assert!(
        models::marker_path(&models, "test-model").exists(),
        "an unreadable marker must survive the refusal"
    );
    assert_eq!(fake.opens(), 0);
}

#[test]
fn a_short_stream_refuses_and_leaves_nothing_behind() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    fake.serve("main", Serve::Truncated(MAIN.to_vec()));

    let err = fetch::fetch_with_table(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        1_730_000_004,
        &Expectation {
            table: &table,
            declaration: &declaration(),
        },
    )
    .expect_err("a truncated transfer is not a verified artifact");

    match err {
        FetchError::Interrupted { got, expected, .. } => {
            assert_eq!(expected, MAIN.len() as u64);
            assert!(got < expected, "short means short: got {got} of {expected}");
        }
        other => panic!("expected Interrupted, got {other:?}"),
    }
    let text = err.to_string();
    // §12 rule 6: an interruption is not called corruption — nothing here knows why the stream stopped.
    assert!(!text.contains("corrupt"), "overstated refusal: {text}");
    assert_nothing_left(tmp.path(), &models);
}

#[test]
fn right_length_wrong_digest_names_both_numbers_and_never_blames_the_human() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    let table = table(MAIN, EXTRA);
    // Same length as pinned, different bytes: the digest is the only thing that can catch this, which is the point
    // of pinning at all.
    let altered = b"the primary artifact, in words, not 310 MB of thay";
    assert_eq!(
        altered.len(),
        MAIN.len(),
        "test fixture must differ in kind, not length"
    );
    assert_ne!(sha256_bytes(altered), sha256_bytes(MAIN));
    let fake = Fake::new();
    fake.serve("main", Serve::Full(altered.to_vec()));

    let err = refusal(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        &table,
        1_730_000_005,
    );

    match err {
        FetchError::DigestMismatch {
            ref expected,
            ref found,
            size,
            ..
        } => {
            assert_eq!(expected, &sha256_bytes(MAIN));
            assert_eq!(found, &sha256_bytes(altered));
            assert_eq!(size, MAIN.len() as u64);
        }
        other => panic!("expected DigestMismatch, got {other:?}"),
    }
    let text = err.to_string();
    // §12 rule 7's wording: the *published file* disagrees with the build, not "your download is corrupted".
    assert!(text.contains("not the artifact this build pins"), "{text}");
    assert!(!text.contains("your download is corrupted"), "{text}");
    assert_nothing_left(tmp.path(), &models);
}

#[test]
fn an_announced_length_that_does_not_arrive_is_interruption_not_mismatch() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    // Pinned at 64 bytes, announced at 64, delivered 10: rule 6's two sentences stay distinct even when the transfer
    // lied about its size.
    let table = vec![Spec {
        member: "main",
        filename: "test-model.gguf",
        size_bytes: 64,
        sha256: sha256_bytes(b"short"),
    }];
    let fake = Fake::new();
    fake.serve("main", Serve::Full(b"short".to_vec()));

    let err = refusal(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        &table,
        1_730_000_006,
    );
    assert!(matches!(err, FetchError::Interrupted { .. }), "{err}");
    assert_nothing_left(tmp.path(), &models);
}

#[test]
fn an_unknown_transfer_size_still_verifies_against_the_pin() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    // No Content-Length: the marker says "size unknown" and the pin is still what decides (§12 rule 12).
    fake.serve("main", Serve::UnknownLen(MAIN.to_vec()));

    fetch_ok(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        &table,
        1_730_000_007,
    );
    assert_eq!(
        fs::read(models.join("test-model.gguf")).expect("published"),
        MAIN
    );
}

#[test]
fn an_existing_artifact_is_never_overwritten_and_is_not_even_requested() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    fs::create_dir_all(&models).expect("test setup: models dir");
    // Verified bytes that belong to this id already. Rule 5 publishes into an **absent** target; a fetch is not the
    // answer to "the artifact disagrees with its row" — `install`'s ArtifactDisagrees refusal is.
    // Same length by construction, different bytes: only a digest can tell these apart, which is the state rule 5
    // refuses to overwrite rather than argue about.
    let existing: Vec<u8> = vec![b'q'; MAIN.len()];
    fs::write(models.join("test-model.gguf"), &existing).expect("test setup: existing artifact");

    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    fake.serve("main", Serve::Full(MAIN.to_vec()));
    let err = refusal(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        &table,
        1_730_000_008,
    );

    assert!(matches!(err, FetchError::TargetExists { .. }), "{err}");
    assert_eq!(fake.opens(), 0, "the check runs before the transfer: {err}");
    assert_eq!(
        fs::read(models.join("test-model.gguf")).expect("still there"),
        existing,
        "an existing artifact's bytes must not change"
    );
}

#[test]
fn a_transport_failure_names_a_class_and_never_the_origin() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    // Raw client-shaped text carrying exactly what §12 rule 8 keeps out of a report.
    fake.serve(
        "main",
        Serve::Fail("Connection refused (os error 111) to http://127.0.0.1:40313".to_string()),
    );

    let err = refusal(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        &table,
        1_730_000_009,
    );
    assert!(matches!(err, FetchError::Transport { .. }), "{err}");
    assert_no_url_pieces(&err.to_string());
}

#[test]
fn failure_class_summarises_without_echoing() {
    // The rule made executable at its source: whatever arrives, the output is one of a fixed set and holds none of
    // the input's addresses.
    let inputs = [
        "https://mirror.example/models/main.gguf: status code 404",
        "http://127.0.0.1:40313/x: connection reset by peer",
        "status code 403 forbidden for user bob@example.com",
        "error:60:0000:ssl routines::certificate verify failed at https://secret.host/artifact",
        "GET https://upstream.invalid/m -> timed out after 30s",
        "redirected to https://other-host.example/file.gguf",
        "status code 503 upstream unavailable",
        "some text naming /home/agent/models and port 8099",
    ];
    let allowed = [
        "not found",
        "refused by the server (authentication or permissions)",
        "server error",
        "redirect refused",
        "could not resolve the configured host",
        "timed out",
        "TLS failure",
        "transport failure",
    ];
    for raw in inputs {
        let class = fetch::failure_class(raw);
        assert!(allowed.contains(&class.as_str()), "{raw:?} -> {class:?}");
        assert_no_url_pieces(&class);
    }
    // And the classes are not all the same string, or rule 8 would be satisfied by returning "transport failure"
    // forever while losing the only actionable distinction a human has.
    let distinct: std::collections::BTreeSet<String> =
        inputs.iter().map(|raw| fetch::failure_class(raw)).collect();
    assert!(distinct.len() >= 5, "classes collapsed to {distinct:?}");
}

#[test]
fn an_unknown_id_refuses_before_any_request() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    fake.serve("main", Serve::Full(MAIN.to_vec()));

    let err = refusal(
        &db,
        &models,
        "other-model",
        &["main"],
        &fake,
        &table,
        1_730_000_010,
    );
    // `other-model` is a *legal id* with nothing pinned — the distinction tests/pins.rs keeps honest: an id this
    // build has no expectation for cannot be verified, so it must not be downloaded to find out.
    assert!(matches!(err, FetchError::NothingPinned { .. }), "{err}");
    assert_eq!(fake.opens(), 0);
    assert!(
        !models.exists(),
        "a refusal creates no workspace state: {err}"
    );
}

#[test]
fn a_member_with_no_recorded_digest_is_refused_by_name_and_the_pair_stays_unwritten() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    // Only `main` is pinned — exactly the production state of the mmproj half (§12 rule 7's second digest).
    let table = vec![table(MAIN, EXTRA).into_iter().next().expect("main row")];
    let fake = Fake::new();
    fake.serve("main", Serve::Full(MAIN.to_vec()));

    let err = refusal(
        &db,
        &models,
        "test-model",
        &["main", "extra"],
        &fake,
        &table,
        1_730_000_011,
    );
    match err {
        FetchError::MemberUnpinned { member, .. } => assert_eq!(member, "extra"),
        other => panic!("expected MemberUnpinned, got {other:?}"),
    }
    // Half a pair is worse than none: the refusal resolves every name before byte one.
    assert_eq!(fake.opens(), 0);
    assert!(!models.join("test-model.gguf").exists());
}

#[test]
fn an_illegal_id_refuses_as_an_id_problem_not_a_file_problem() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    let err = refusal(
        &db,
        &models,
        ".hidden",
        &["main"],
        &fake,
        &table,
        1_730_000_012,
    );
    // `.hidden` breaks `validate_id`'s first-character rule (§12 rule 2's forgery defence), so this is the arm that
    // keeps a fetch from writing a name the listing cannot attribute to any id.
    assert!(matches!(err, FetchError::BadId { .. }), "{err}");
    assert_eq!(fake.opens(), 0);
}

#[test]
fn preflight_refuses_before_byte_one_when_the_disk_cannot_hold_the_pair() {
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    // A pinned size no host could hold: `saturating_add` keeps the arithmetic honest instead of wrapping past zero,
    // which is the failure mode a naive `+ margin` has here.
    let table = vec![Spec {
        member: "main",
        filename: "test-model.gguf",
        size_bytes: u64::MAX / 2,
        sha256: sha256_bytes(MAIN),
    }];
    let fake = Fake::new();
    fake.serve("main", Serve::Full(MAIN.to_vec()));

    let err = refusal(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        &table,
        1_730_000_013,
    );
    match err {
        FetchError::DiskShortfall {
            needed, available, ..
        } => {
            assert!(needed > available, "{needed} must exceed {available}");
        }
        other => panic!("expected DiskShortfall, got {other:?}"),
    }
    assert_eq!(
        fake.opens(),
        0,
        "an admission check that dials out is not one: {err}"
    );
    assert!(!models.exists());
}

#[test]
fn every_refusal_message_is_free_of_the_origin() {
    // One case per refusal, all checked against the same string: the loopback origin this test's transport would use.
    // §12 rule 8 is about *every* message, so the fence is written as a sweep rather than attached to one arm.
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    fs::create_dir_all(&models).expect("test setup: models dir");

    let fake = Fake::new();
    fake.serve("main", Serve::Fail("status code 404".to_string()));
    let table = table(MAIN, EXTRA);
    let refusals = vec![
        refusal(
            &db,
            &models,
            "test-model",
            &["main"],
            &fake,
            &table,
            1_730_000_014,
        ),
        // A marker written by someone else, whose own text this sweep also covers.
        {
            write_marker_raw(
                &models,
                "test-model",
                r#"{"nonce":"1-1-1","started_at":5,"bytes_done":6,"bytes_total":null}"#,
            );
            let err = refusal(
                &db,
                &models,
                "test-model",
                &["main"],
                &fake,
                &table,
                1_730_000_015,
            );
            fs::remove_file(models::marker_path(&models, "test-model"))
                .expect("clean up for the next refusal");
            err
        },
    ];
    for err in refusals {
        let text = err.to_string();
        assert_no_url_pieces(&text);
    }
}

// --- helpers ---------------------------------------------------------------------------------------------

/// Fetch and expect success.
fn fetch_ok<T: MemberPin>(
    db: &Db,
    models: &Path,
    id: &str,
    requested: &[&str],
    transport: &dyn Transport,
    table: &[T],
    now: i64,
) {
    let plan = Expectation {
        table,
        declaration: &declaration(),
    };
    fetch::fetch_with_table(db, models, id, requested, transport, now, &plan)
        .unwrap_or_else(|err| panic!("the fetch of `{id}` should succeed: {err}"));
}

/// Fetch and keep the error, panicking on success with what was published.
fn refusal<T: MemberPin>(
    db: &Db,
    models: &Path,
    id: &str,
    requested: &[&str],
    transport: &dyn Transport,
    table: &[T],
    now: i64,
) -> FetchError {
    let plan = Expectation {
        table,
        declaration: &declaration(),
    };
    match fetch::fetch_with_table(db, models, id, requested, transport, now, &plan) {
        Ok(report) => panic!("expected a refusal for `{id}`, got {report:?}"),
        Err(err) => err,
    }
}

/// Assert nothing recognisable or stray is left in the models directory after a refusal.
fn assert_nothing_left(_tmp: &Path, models: &Path) {
    // **No directory at all is the cleanest outcome** (nothing was ever created), so an unreadable path passes here
    // rather than failing: the cases that care about *stray names* have a directory by construction.
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
}

/// Write a marker's bytes directly, as another process would.
fn write_marker_raw(models: &Path, id: &str, body: &str) {
    let path = models::marker_path(models, id);
    fs::create_dir_all(models).expect("test setup: models dir");
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .expect("test setup: marker")
        .write_all(body.as_bytes())
        .expect("test setup: marker bytes");
}

/// §12 rule 8, spelled as an assertion: no part of an origin appears in a report.
fn assert_no_url_pieces(text: &str) {
    for forbidden in [
        "http://",
        "https://",
        "127.0.0.1",
        "40313",
        "://",
        "mirror.example",
        "secret.host",
        "upstream.invalid",
        "other-host.example",
        "bob@example.com",
    ] {
        assert!(
            !text.contains(forbidden),
            "a refusal named {forbidden:?} — §12 rule 8 says a class, never a URL: {text}"
        );
    }
}

#[test]
fn a_successful_fetch_records_the_row_and_every_member_it_published() {
    // The half of §12 rule 10 that only shows up when the bytes arrive: `models.sha256` carries the **primary**
    // member's digest (so every existing reader keeps its meaning) while `model_files` names both halves. An id whose
    // pair is recorded under one digest would later refuse its own projector.
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    fake.serve("main", Serve::Full(MAIN.to_vec()));
    fake.serve("extra", Serve::Full(EXTRA.to_vec()));

    fetch_ok(
        &db,
        &models,
        "test-model",
        &["main", "extra"],
        &fake,
        &table,
        1_730_000_020,
    );

    let row = db::model_row(&db, "test-model")
        .expect("the row exists")
        .expect("fetch created it");
    assert_eq!(row.dim, 4);
    assert_eq!(row.sha256.as_deref(), Some(sha256_bytes(MAIN).as_str()));
    assert_eq!(row.prefix_doc, "doc: ");
    assert_eq!(row.prefix_query, "q: ");
    assert_eq!(row.installed_at, 1_730_000_020);

    let files = db::model_files(&db, "test-model").expect("members recorded");
    assert_eq!(
        files
            .iter()
            .map(|file| (file.name.as_str(), file.size))
            .collect::<Vec<_>>(),
        vec![("extra", EXTRA.len() as i64), ("main", MAIN.len() as i64)],
        "both members, by name, in name order"
    );
}

#[test]
fn a_row_that_declares_something_else_refuses_before_byte_one() {
    // The workspace said 1,024 dimensions; this build pins 4. That is resolvable only by the human who wrote the
    // first number, so nothing transfers and both sides are named — `install`'s shape for the same problem.
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    np_core::db::insert_model(
        &db,
        "test-model",
        1024,
        Some(&sha256_bytes(MAIN)),
        "",
        "",
        7,
    )
    .expect("row");

    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    fake.serve("main", Serve::Full(MAIN.to_vec()));
    let err = refusal(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        &table,
        1_730_000_021,
    );

    assert!(matches!(err, FetchError::IdentityDisagrees { .. }), "{err}");
    let text = err.to_string();
    assert!(
        text.contains("1024") && text.contains("4"),
        "both widths named: {text}"
    );
    assert_eq!(
        fake.opens(),
        0,
        "a declared fact needs no transfer to check: {text}"
    );
    assert!(!models.join("test-model.gguf").exists());
    // The row is untouched — a refusal that rewrote the declaration would have decided the disagreement.
    assert_eq!(
        db::model_row(&db, "test-model")
            .expect("row")
            .expect("still there")
            .dim,
        1024
    );
}

#[test]
fn a_row_with_no_digest_of_its_own_does_not_disagree_about_one() {
    // `models.sha256` is NULL for a provider with no weights (§2 of the embeddings design). "Nothing recorded" is not
    // evidence of a mismatch, and treating it as such would make a fetch refuse the one workspace shape that has no
    // opinion to contradict.
    let tmp = dir();
    let db = db();
    let models = tmp.path().join("models");
    np_core::db::insert_model(&db, "test-model", 4, None, "doc: ", "q: ", 7).expect("row");

    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    fake.serve("main", Serve::Full(MAIN.to_vec()));
    fetch_ok(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        &table,
        1_730_000_022,
    );

    // The row keeps its NULL digest: recording the fetched bytes there is `install`'s declaration contract, not a
    // fetch's quiet side effect. What a fetch does write is `model_files`, where the digest does belong.
    let row = db::model_row(&db, "test-model")
        .expect("row")
        .expect("still there");
    assert_eq!(row.sha256, None);
    let files = db::model_files(&db, "test-model").expect("members");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].sha256, sha256_bytes(MAIN));
}

#[test]
fn a_database_that_refuses_the_record_reports_it_rather_than_a_success() {
    // The durable-record gap `install` documents between rename and insert, reached on the fetch side: the bytes are
    // verified and published, and then `model_files` refuses them. What must **not** happen is a success report — the
    // caller has to learn that this workspace cannot yet use what it just downloaded.
    let tmp = dir();
    let models = tmp.path().join("models");
    let path = tmp.path().join("ws.sqlite");

    // Open once through `Db` so the schema is the current one, close it, then remove *one* of migration 9's tables
    // out from under it. Reopening cannot re-create the table (the version already says migration 9 ran), which is
    // exactly the half-applied schema a real interruption can leave.
    {
        let opened = Db::open(&path).expect("test setup: current schema");
        drop(opened);
    }
    {
        let raw = rusqlite::Connection::open(&path).expect("test setup: open raw");
        raw.execute_batch("DROP TABLE model_files")
            .expect("test setup: drop the member table");
    }
    let db = Db::open(&path).expect("reopening a version-9 file must not fail");

    let table = table(MAIN, EXTRA);
    let fake = Fake::new();
    fake.serve("main", Serve::Full(MAIN.to_vec()));
    let err = refusal(
        &db,
        &models,
        "test-model",
        &["main"],
        &fake,
        &table,
        1_730_000_030,
    );

    assert!(matches!(err, FetchError::Db { .. }), "{err}");
    // The artifact itself is on disk — published before the record failed. That is `list`'s "an artifact with no row"
    // shape (or here, a row with no members), reported rather than quietly rolled back: deleting bytes a human may
    // want to keep is not this command's call.
    assert_eq!(
        fs::read(models.join("test-model.gguf")).expect("published"),
        MAIN
    );
}
